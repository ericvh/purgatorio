//! The display the draw device (`#i`) shows its screen image on, and the
//! pointer and keyboard input that comes back from it.
//!
//! The device owns the screen's pixels: it draws into an image in memory
//! and, on a flush, hands the damaged rectangle's pixels to
//! [`Screen::flush`]. A backend only has to show them. The emu's window
//! copies them into a winit/softbuffer surface; a headless screen drops
//! them; a browser backend could send them to a canvas; a kernel's ramfb
//! or virtio-gpu could copy or transfer them. Input goes the other way,
//! into the [`Input`] the backend was given, which `#m` and `#c/keyboard`
//! read.
//!
//! A backend whose size changes (a window the user resizes) makes
//! [`Screen::size`] return the new size and then calls [`Input::resized`].
//! The draw device makes a new screen image of that size before its next
//! request, keeping what fits of the old one, and tells its clients
//! through their `refresh` files (draw(3)).

use alloc::collections::VecDeque;

use crate::sync::{Mutex, Rendez};

/// image(6)'s `x8r8g8b8`: what [`Screen::chan`] gives unless a backend
/// says otherwise. A pixel is a little-endian `u32` `0x00RRGGBB`.
pub const XRGB32: u32 = 0x6808_1828;

/// A rectangle, `[min.x, min.y, max.x, max.y)`, in screen pixels.
pub type Rect = [i32; 4];

/// What the draw device asks of a display.
pub trait Screen: Send + Sync {
    /// The screen's width and height in pixels. The screen image is
    /// `(0, 0)-(width, height)`. It may change; see [`Input::resized`].
    fn size(&self) -> (u32, u32);
    /// The channel descriptor of the screen image and of the pixels
    /// [`Screen::flush`] receives.
    fn chan(&self) -> u32 {
        XRGB32
    }
    /// Shows rectangle `r` of the screen image. `pixels` holds its rows,
    /// top to bottom, each `stride` bytes, starting with the byte that
    /// holds pixel `r[0]`, as image(6) lays out rows. Just after a resize
    /// `r` may still be in the old size: a backend clips it to its own.
    fn flush(&self, r: Rect, pixels: &[u8], stride: usize);
    /// Where the backend delivers pointer and keyboard events.
    fn input(&self) -> &Input;
    /// Moves the pointer, when a program writes `/dev/pointer`. A backend
    /// that cannot move the host's pointer does nothing.
    fn set_pointer(&self, _x: i32, _y: i32) {}
}

/// One pointer state, as `/dev/pointer` reports it (pointer(3)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pointer {
    pub x: i32,
    pub y: i32,
    /// Button 1 is bit 0, button 2 bit 1, button 3 bit 2; a wheel step
    /// up is bit 3, down bit 4.
    pub buttons: u32,
    /// Milliseconds, from any fixed point.
    pub msec: u32,
}

/// Pointer events and keyboard bytes not yet read.
#[derive(Default)]
struct Queues {
    pointer: VecDeque<Pointer>,
    last: Pointer,
    keys: VecDeque<u8>,
    /// How many times the screen has changed size.
    resizes: u64,
}

/// The most pointer events kept unread; older ones are dropped.
const MAX_POINTER: usize = 64;
/// The most keyboard bytes kept unread.
const MAX_KEYS: usize = 4096;

/// Input from a display, waiting to be read. Pointer motion with the
/// buttons unchanged replaces an unread motion, so a slow reader sees the
/// latest position without losing a press or a release.
pub struct Input {
    q: Mutex<Queues>,
    /// Woken whenever an event arrives.
    pub rendez: Rendez,
    /// Rendezvous of progs waiting in `purgatorio.wait` for input, woken
    /// with [`Input::rendez`].
    watchers: Mutex<alloc::vec::Vec<Rendez>>,
}

impl Default for Input {
    fn default() -> Input {
        Input::new()
    }
}

impl Input {
    pub fn new() -> Input {
        Input {
            q: Mutex::new(Queues::default()),
            rendez: Rendez::new(),
            watchers: Mutex::new(alloc::vec::Vec::new()),
        }
    }

    /// Wakes the readers and the watchers.
    fn wakeup(&self) {
        self.rendez.wakeup();
        for w in self.watchers.lock().iter() {
            w.wakeup();
        }
    }

    /// Wakes `w` too whenever an event arrives, until [`Input::unwatch`].
    pub fn watch(&self, w: &Rendez) {
        self.watchers.lock().push(w.clone());
    }

    pub fn unwatch(&self, w: &Rendez) {
        self.watchers.lock().retain(|x| !x.same(w));
    }

    /// Records a pointer state.
    pub fn pointer(&self, p: Pointer) {
        {
            let mut q = self.q.lock();
            let replace = q.pointer.back().is_some_and(|b| b.buttons == p.buttons);
            if replace {
                q.pointer.pop_back();
            }
            if q.pointer.len() == MAX_POINTER {
                q.pointer.pop_front();
            }
            q.pointer.push_back(p);
            q.last = p;
        }
        self.wakeup();
    }

    /// The last pointer state recorded.
    pub fn last_pointer(&self) -> Pointer {
        self.q.lock().last
    }

    /// Takes the oldest unread pointer state.
    pub fn take_pointer(&self) -> Option<Pointer> {
        self.q.lock().pointer.pop_front()
    }

    pub fn has_pointer(&self) -> bool {
        !self.q.lock().pointer.is_empty()
    }

    /// Records a typed character (keyboard(6) gives the codes of keys
    /// that are not characters).
    pub fn key(&self, c: char) {
        {
            let mut q = self.q.lock();
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            if q.keys.len() + s.len() <= MAX_KEYS {
                q.keys.extend(s.bytes());
            }
        }
        self.wakeup();
    }

    /// Says that the screen has changed size: [`Screen::size`] already
    /// returns the new one.
    pub fn resized(&self) {
        self.q.lock().resizes += 1;
        self.wakeup();
    }

    /// How many times the screen has changed size, so that a reader can
    /// tell whether it has since it last looked.
    pub fn resizes(&self) -> u64 {
        self.q.lock().resizes
    }

    pub fn has_keys(&self) -> bool {
        !self.q.lock().keys.is_empty()
    }

    /// Takes up to `buf.len()` bytes of typed characters, never splitting
    /// one, unless `buf` is too short for even the first.
    pub fn take_keys(&self, buf: &mut [u8]) -> usize {
        let mut q = self.q.lock();
        let mut n = 0;
        while let Some(&b) = q.keys.front() {
            let len = match b {
                0xF0..=0xF7 => 4,
                0xE0..=0xEF => 3,
                0xC0..=0xDF => 2,
                _ => 1,
            };
            if n + len > buf.len() && n > 0 {
                break;
            }
            for _ in 0..len {
                let Some(b) = q.keys.pop_front() else {
                    break;
                };
                if n < buf.len() {
                    buf[n] = b;
                    n += 1;
                }
            }
        }
        n
    }
}

/// A screen with nowhere to show its pixels: the draw device's default
/// when there is no window, so that programs and tests can draw and read
/// back what they drew.
pub struct Headless {
    size: Mutex<(u32, u32)>,
    input: Input,
}

impl Headless {
    pub fn new(width: u32, height: u32) -> Headless {
        Headless {
            size: Mutex::new((width, height)),
            input: Input::new(),
        }
    }

    /// Changes the size, as a window the user resized would.
    pub fn resize(&self, width: u32, height: u32) {
        *self.size.lock() = (width, height);
        self.input.resized();
    }
}

impl Screen for Headless {
    fn size(&self) -> (u32, u32) {
        *self.size.lock()
    }

    fn flush(&self, _r: Rect, _pixels: &[u8], _stride: usize) {}

    fn input(&self) -> &Input {
        &self.input
    }
}
