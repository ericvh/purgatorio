//! The emu's window (`emu -g`): a [`Screen`] shown with winit and
//! softbuffer.
//!
//! The main thread runs winit's event loop, as macOS requires, and does
//! the same on Linux so there is one way of doing it. Prog threads draw
//! into the draw device's screen image; a flush copies the damaged
//! rectangle into [`Frame`] here and wakes the loop through an
//! [`EventLoopProxy`]. The loop copies only what changed into the
//! window's buffer and presents that damage. Pointer and keyboard events
//! go into the screen's [`Input`], which `#m` and `#c/keyboard` read.
//!
//! The window has the screen's size in physical pixels; it is not scaled
//! on a high-density display.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use purg_platform::screen::{Input, Pointer, Rect, Screen};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

/// What prog threads tell the event loop.
#[derive(Debug)]
pub enum Event {
    /// The frame has damage to show.
    Redraw,
    /// A program moved the pointer.
    Warp(i32, i32),
    /// The first prog ended with this exit status.
    Exit(u8),
}

/// The screen as last flushed, and what changed since it was shown.
struct Frame {
    pixels: Vec<u32>,
    damage: Option<Rect>,
}

pub struct WindowScreen {
    width: u32,
    height: u32,
    frame: Mutex<Frame>,
    proxy: Mutex<EventLoopProxy<Event>>,
    input: Input,
}

impl WindowScreen {
    pub fn new(width: u32, height: u32, proxy: EventLoopProxy<Event>) -> WindowScreen {
        WindowScreen {
            width,
            height,
            frame: Mutex::new(Frame {
                pixels: vec![0x00FF_FFFF; width as usize * height as usize],
                damage: Some([0, 0, width as i32, height as i32]),
            }),
            proxy: Mutex::new(proxy),
            input: Input::new(),
        }
    }

    fn send(&self, e: Event) {
        // The loop has gone only when the emu is exiting.
        let _ = self.proxy.lock().unwrap().send_event(e);
    }
}

fn union(a: Option<Rect>, b: Rect) -> Rect {
    match a {
        None => b,
        Some(a) => [
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ],
    }
}

impl Screen for WindowScreen {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn flush(&self, r: Rect, pixels: &[u8], stride: usize) {
        let w = self.width as usize;
        let mut f = self.frame.lock().unwrap();
        for (i, y) in (r[1]..r[3]).enumerate() {
            let row = &pixels[i * stride..];
            let out = &mut f.pixels[y as usize * w + r[0] as usize..][..(r[2] - r[0]) as usize];
            for (o, px) in out.iter_mut().zip(row.as_chunks::<4>().0) {
                *o = u32::from_le_bytes(*px) & 0x00FF_FFFF;
            }
        }
        f.damage = Some(union(f.damage, r));
        drop(f);
        self.send(Event::Redraw);
    }

    fn input(&self) -> &Input {
        &self.input
    }

    fn set_pointer(&self, x: i32, y: i32) {
        self.send(Event::Warp(x, y));
    }
}

/// Makes the event loop. Fails when there is no display to open.
pub fn event_loop() -> Result<EventLoop<Event>, String> {
    EventLoop::<Event>::with_user_event()
        .build()
        .map_err(|e| format!("cannot open display: {e}"))
}

/// Runs the window until the first prog exits ([`Event::Exit`]) or the
/// window is closed; returns the emu's exit status.
pub fn run(event_loop: EventLoop<Event>, screen: Arc<WindowScreen>) -> u8 {
    let mut app = App {
        screen,
        window: None,
        start: Instant::now(),
        buttons: 0,
        at: PhysicalPosition::new(0, 0),
        modifiers: ModifiersState::empty(),
        status: 0,
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("emu: window: {e}");
        return 1;
    }
    app.status
}

struct Shown {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    /// The damage of the previous present, for a buffer two frames old.
    previous: Option<Rect>,
}

struct App {
    screen: Arc<WindowScreen>,
    window: Option<Shown>,
    start: Instant,
    buttons: u32,
    at: PhysicalPosition<i32>,
    modifiers: ModifiersState,
    status: u8,
}

impl App {
    fn pointer(&self) {
        self.screen.input.pointer(Pointer {
            x: self.at.x,
            y: self.at.y,
            buttons: self.buttons,
            msec: self.start.elapsed().as_millis() as u32,
        });
    }

    fn open(&mut self, el: &ActiveEventLoop) -> Result<(), String> {
        let (w, h) = (self.screen.width, self.screen.height);
        let attrs = Window::default_attributes()
            .with_title("Purgatorio")
            .with_inner_size(PhysicalSize::new(w, h))
            .with_resizable(false);
        let window = Rc::new(el.create_window(attrs).map_err(|e| e.to_string())?);
        let context = softbuffer::Context::new(window.clone()).map_err(|e| e.to_string())?;
        let surface =
            softbuffer::Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
        self.window = Some(Shown {
            window,
            surface,
            previous: None,
        });
        Ok(())
    }

    /// Copies what changed into the window's buffer and presents it.
    fn present(&mut self) -> Result<(), String> {
        let Some(s) = self.window.as_mut() else {
            return Ok(());
        };
        let size = s.window.inner_size();
        let (Some(bw), Some(bh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return Ok(());
        };
        s.surface.resize(bw, bh).map_err(|e| e.to_string())?;
        let mut f = self.screen.frame.lock().unwrap();
        let damage = f.damage.take();
        let mut buf = s.surface.buffer_mut().map_err(|e| e.to_string())?;
        let (fw, fh) = (self.screen.width as i32, self.screen.height as i32);
        let (w, h) = (bw.get() as i32, bh.get() as i32);
        let full = [0, 0, w, h];
        // A new buffer needs everything; one shown two frames ago also
        // needs what changed in between.
        let copy = match (buf.age(), damage) {
            (1, Some(d)) => d,
            (1, None) => return Ok(()),
            (2, Some(d)) => union(s.previous, d),
            _ => full,
        };
        s.previous = damage;
        let r = [
            copy[0].max(0),
            copy[1].max(0),
            copy[2].min(w),
            copy[3].min(h),
        ];
        for y in r[1]..r[3] {
            for x in r[0]..r[2] {
                let o = (y * w + x) as usize;
                buf[o] = if x < fw && y < fh {
                    f.pixels[(y * fw + x) as usize]
                } else {
                    0
                };
            }
        }
        drop(f);
        let (Some(dw), Some(dh)) = (
            NonZeroU32::new((r[2] - r[0]).max(0) as u32),
            NonZeroU32::new((r[3] - r[1]).max(0) as u32),
        ) else {
            return buf.present().map_err(|e| e.to_string());
        };
        buf.present_with_damage(&[softbuffer::Rect {
            x: r[0] as u32,
            y: r[1] as u32,
            width: dw,
            height: dh,
        }])
        .map_err(|e| e.to_string())
    }

    fn key(&self, e: &KeyEvent) {
        if e.state != ElementState::Pressed {
            return;
        }
        let input = &self.screen.input;
        if let Key::Named(k) = &e.logical_key
            && let Some(c) = named_key(*k)
        {
            input.key(c);
            return;
        }
        if let Key::Character(s) = &e.logical_key
            && self.modifiers.control_key()
        {
            for c in s.chars() {
                if c.is_ascii_alphabetic() || "@[\\]^_".contains(c) {
                    input.key(char::from(c.to_ascii_uppercase() as u8 & 0x1F));
                }
            }
            return;
        }
        if let Some(t) = &e.text {
            for c in t.chars() {
                input.key(if c == '\r' { '\n' } else { c });
            }
        }
    }
}

/// keyboard(6)'s codes for keys that are not characters.
fn named_key(k: NamedKey) -> Option<char> {
    const KF: u32 = 0xF000;
    let code = match k {
        NamedKey::Enter => u32::from(b'\n'),
        NamedKey::Tab => u32::from(b'\t'),
        NamedKey::Backspace => 0x08,
        NamedKey::Escape => 0x1B,
        NamedKey::Delete => 0x7F,
        NamedKey::Space => u32::from(b' '),
        NamedKey::Home => KF | 0x0D,
        NamedKey::ArrowUp => KF | 0x0E,
        NamedKey::PageUp => KF | 0x0F,
        NamedKey::PrintScreen => KF | 0x10,
        NamedKey::ArrowLeft => KF | 0x11,
        NamedKey::ArrowRight => KF | 0x12,
        NamedKey::PageDown => KF | 0x13,
        NamedKey::Insert => KF | 0x14,
        NamedKey::End => KF | 0x18,
        NamedKey::ArrowDown => 0xF800,
        NamedKey::F1 => KF | 1,
        NamedKey::F2 => KF | 2,
        NamedKey::F3 => KF | 3,
        NamedKey::F4 => KF | 4,
        NamedKey::F5 => KF | 5,
        NamedKey::F6 => KF | 6,
        NamedKey::F7 => KF | 7,
        NamedKey::F8 => KF | 8,
        NamedKey::F9 => KF | 9,
        NamedKey::F10 => KF | 10,
        NamedKey::F11 => KF | 11,
        NamedKey::F12 => KF | 12,
        _ => return None,
    };
    char::from_u32(code)
}

impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.open(el) {
            eprintln!("emu: window: {e}");
            self.status = 1;
            el.exit();
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: Event) {
        match event {
            Event::Redraw => {
                if let Some(s) = &self.window {
                    s.window.request_redraw();
                }
            }
            Event::Warp(x, y) => {
                self.at = PhysicalPosition::new(x, y);
                if let Some(s) = &self.window {
                    let _ = s.window.set_cursor_position(PhysicalPosition::new(x, y));
                }
            }
            Event::Exit(status) => {
                self.status = status;
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested | WindowEvent::Resized(_) => {
                if matches!(event, WindowEvent::Resized(_)) {
                    let mut f = self.screen.frame.lock().unwrap();
                    f.damage = Some([0, 0, i32::MAX, i32::MAX]);
                }
                if let Err(e) = self.present() {
                    eprintln!("emu: window: {e}");
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.at = PhysicalPosition::new(position.x as i32, position.y as i32);
                self.pointer();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let bit = match button {
                    MouseButton::Left => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Right => 4,
                    _ => return,
                };
                if state == ElementState::Pressed {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                self.pointer();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };
                // A wheel step is a press and release of button 4 (up) or
                // 5 (down).
                let bit = if dy > 0.0 {
                    8
                } else if dy < 0.0 {
                    16
                } else {
                    return;
                };
                self.buttons |= bit;
                self.pointer();
                self.buttons &= !bit;
                self.pointer();
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.key(&event),
            _ => {}
        }
    }
}
