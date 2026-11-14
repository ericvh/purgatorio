//! rio driven from outside, on both VMs. The emu serves its screen with
//! `-w` and this test is the browser (as in `acme.rs`): it sends pointer
//! and key events over the WebSocket, and keeps a copy of the screen
//! from the frames the emu sends, so it can wait for what it expects to
//! see.
//!
//! It sweeps a window with the menu's `New`, types `echo hello` into it
//! and reads the window's text back through `/dev/text`, runs
//! `draw/demo` in it and checks that the demo's pixels land inside the
//! window, makes a second window, resizes and moves the first (the demo
//! draws again each time), deletes it and checks with `ps`, typed in the
//! second, that its progs are gone, and ends rio with `Exit`.
//!
//! A second run (`editing_*`) selects text with button 1 (a double click
//! and a sweep), snarfs it with button 2's menu and checks `/dev/snarf`,
//! sends and pastes it into another window, resizes a window by dragging
//! its border, reads `/dev/window` and `/dev/screen` as images, hides
//! and shows a window, checks that a program that draws once and exits
//! leaves its picture, and resizes the window a second rio runs in and
//! checks that rio's window is scaled to stay on its screen.
//!
//! Each step waits for something it can see (a pixel, a file a command
//! wrote) with a bound, never for a fixed time. Keys and pointer events
//! reach rio on separate queues, so before the test types into a window
//! it waits to see that window current. Nothing depends on the host's
//! name. Requires `cargo xtask build-root` first.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use purg_emu::web::{OP_BINARY, OP_CLOSE, accept_key, read_frame, write_frame};

mod common;

const SIZE: (i32, i32) = (800, 600);
const WAIT: Duration = Duration::from_secs(240);
/// The default font's height, and rio's menu items: that and 2 pixels
/// above and below.
const FH: i32 = 19;
const IH: i32 = FH + 4;
/// rio's menu.
const NEW: usize = 0;
const RESIZE: usize = 1;
const MOVE: usize = 2;
const DELETE: usize = 3;
const HIDE: usize = 4;
const EXIT: usize = 5;
/// The first hidden window's label in the menu.
const HIDDEN: usize = 6;
/// Button 2's menu in a text window, with no plumber running.
const SNARF: usize = 2;
const PASTE: usize = 1;
const SEND: usize = 3;
/// Where the menu is called up: on the background throughout.
const MENU_AT: (i32, i32) = (620, 450);
/// rio's colours: the background, and the border of the current window.
const BACKGROUND: u32 = 0x777777;
const FOCUS: u32 = 0x2E6E6E;
/// acme's tags and bodies.
const ACME_TAG: u32 = 0xEAFFFF;
const ACME_BODY: u32 = 0xFFFFEA;
/// The colour of `draw/demo`'s first bar.
const DEMO_RED: u32 = 0xDD2222;

type Rect = (i32, i32, i32, i32);

/// A root of its own: links to `root/`'s entries but a fresh `tmp`.
struct Root(PathBuf);

impl Root {
    fn new(vm: &str) -> Root {
        static N: AtomicUsize = AtomicUsize::new(0);
        let real =
            std::fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root"))
                .unwrap();
        assert!(
            real.join("bin/rio.wasm").exists() && real.join("bin/draw/demo.wasm").exists(),
            "guest binaries missing; run `cargo xtask build-root` first"
        );
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("rio")
            .join(format!(
                "{}-{vm}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        for e in std::fs::read_dir(&real).unwrap() {
            let name = e.unwrap().file_name();
            if name != "tmp" {
                std::os::unix::fs::symlink(real.join(&name), path.join(&name)).unwrap();
            }
        }
        std::fs::create_dir_all(path.join("tmp/rio")).unwrap();
        Root(path)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The URL `emu -w` prints.
fn url(w: &common::Watched) -> (String, String) {
    let start = Instant::now();
    loop {
        let err = String::from_utf8_lossy(&w.stderr_so_far()).into_owned();
        if let Some(i) = err.find("emu: screen at ") {
            let rest = &err[i + "emu: screen at ".len()..];
            if let Some(end) = rest.find('\n') {
                let u = rest[..end].strip_prefix("http://").unwrap();
                let (host, token) = u.split_once("/?t=").unwrap();
                return (host.to_string(), token.to_string());
            }
        }
        assert!(start.elapsed() < WAIT, "no URL from emu -w:\n{err}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The screen as the page would show it, `0xRRGGBB` per pixel.
#[derive(Clone)]
struct Mirror(Arc<Mutex<Vec<u32>>>);

impl Mirror {
    /// Keeps the copy from the frames read from `s`.
    fn start(mut s: TcpStream) -> Mirror {
        let m = Mirror(Arc::new(Mutex::new(vec![0; (SIZE.0 * SIZE.1) as usize])));
        let px = m.0.clone();
        std::thread::spawn(move || {
            while let Ok(f) = read_frame(&mut s, 1 << 26) {
                if f.opcode == OP_CLOSE {
                    break;
                }
                if f.opcode != OP_BINARY || f.payload.first() != Some(&1) {
                    continue;
                }
                let v = |i: usize| u32::from_le_bytes(f.payload[i..i + 4].try_into().unwrap());
                let (x, y, w, h) = (v(1), v(5), v(9), v(13));
                let rgba = miniz_oxide::inflate::decompress_to_vec_zlib(&f.payload[17..]).unwrap();
                let mut g = px.lock().unwrap();
                for row in 0..h {
                    for col in 0..w {
                        let i = ((row * w + col) * 4) as usize;
                        let o = ((y + row) * SIZE.0 as u32 + x + col) as usize;
                        g[o] = u32::from(rgba[i]) << 16
                            | u32::from(rgba[i + 1]) << 8
                            | u32::from(rgba[i + 2]);
                    }
                }
            }
        });
        m
    }

    fn at(&self, x: i32, y: i32) -> u32 {
        self.0.lock().unwrap()[(y * SIZE.0 + x) as usize]
    }

    /// Whether any pixel in `r` is black: text drawn there.
    fn ink(&self, r: Rect) -> bool {
        let g = self.0.lock().unwrap();
        (r.1..r.3).any(|y| (r.0..r.2).any(|x| g[(y * SIZE.0 + x) as usize] == 0))
    }

    /// Whether every pixel in `draw/demo`'s red is inside window `r`.
    fn red_only_inside(&self, r: Rect) -> bool {
        let g = self.0.lock().unwrap();
        (0..SIZE.1).all(|y| {
            (0..SIZE.0).all(|x| {
                g[(y * SIZE.0 + x) as usize] != DEMO_RED
                    || (x >= r.0 + 4 && x < r.2 - 4 && y >= r.1 + 4 && y < r.3 - 4)
            })
        })
    }
}

/// Opens the page's WebSocket.
fn connect(host: &str, token: &str) -> TcpStream {
    use std::io::{Read, Write};
    const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
    let mut s = TcpStream::connect(host).unwrap();
    write!(
        s,
        "GET /ws?t={token} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {KEY}\r\nOrigin: http://{host}\r\n\r\n"
    )
    .unwrap();
    let mut head = Vec::new();
    let mut b = [0u8];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(s.read(&mut b).unwrap(), 1);
        head.push(b[0]);
    }
    let head = String::from_utf8(head).unwrap();
    assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
    assert!(head.contains(&accept_key(KEY)), "{head}");
    s
}

struct Page {
    s: TcpStream,
    screen: Mirror,
    tmp: PathBuf,
    /// The menu item rio puts under the pointer: the last chosen.
    lasthit: usize,
    /// The same for button 2's menu.
    lasthit2: usize,
    vm: String,
    /// Commands run, for file names.
    n: usize,
}

impl Page {
    fn send(&mut self, m: &[u8]) {
        write_frame(&mut self.s, OP_BINARY, m, Some([1, 2, 3, 4])).unwrap();
    }

    fn pointer(&mut self, x: i32, y: i32, buttons: u32) {
        let mut m = vec![b'm'];
        m.extend_from_slice(&x.to_le_bytes());
        m.extend_from_slice(&y.to_le_bytes());
        m.extend_from_slice(&buttons.to_le_bytes());
        self.send(&m);
    }

    fn click(&mut self, x: i32, y: i32, button: u32) {
        self.pointer(x, y, 0);
        self.pointer(x, y, button);
        self.pointer(x, y, 0);
    }

    /// A drag with `button` held from `a` to `b`.
    fn drag(&mut self, a: (i32, i32), b: (i32, i32), button: u32) {
        self.pointer(a.0, a.1, 0);
        self.pointer(a.0, a.1, button);
        self.pointer((a.0 + b.0) / 2, (a.1 + b.1) / 2, button);
        self.pointer(b.0, b.1, button);
        self.pointer(b.0, b.1, 0);
    }

    /// Chooses menu item `i`: button 3 down on the background, up on the
    /// item. rio puts the last item chosen under the pointer.
    fn menu(&mut self, i: usize) {
        let (x, y) = MENU_AT;
        let iy = y + (i as i32 - self.lasthit as i32) * IH;
        self.pointer(x, y, 0);
        self.pointer(x, y, 4);
        self.pointer(x, iy, 4);
        self.pointer(x, iy, 0);
        self.lasthit = i;
    }

    /// Chooses item `i` of button 2's menu in the window at `at`.
    fn menu2(&mut self, at: (i32, i32), i: usize) {
        let (x, y) = at;
        let iy = y + (i as i32 - self.lasthit2 as i32) * IH;
        self.pointer(x, y, 0);
        self.pointer(x, y, 2);
        self.pointer(x, iy, 2);
        self.pointer(x, iy, 0);
        self.lasthit2 = i;
    }

    fn typed(&mut self, s: &str) {
        for c in s.chars() {
            let mut m = vec![b'k', 0];
            match c {
                '\n' => m.extend_from_slice(b"Enter"),
                c => m.extend_from_slice(c.to_string().as_bytes()),
            }
            self.send(&m);
        }
    }

    /// Waits until `done` holds for the screen.
    fn see(&self, what: &str, done: impl Fn(&Mirror) -> bool) {
        let start = Instant::now();
        while !done(&self.screen) {
            if start.elapsed() > WAIT {
                let keep =
                    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("rio-{}.ppm", self.vm));
                let mut ppm = format!("P6 {} {} 255\n", SIZE.0, SIZE.1).into_bytes();
                for &p in self.screen.0.lock().unwrap().iter() {
                    ppm.extend([(p >> 16) as u8, (p >> 8) as u8, p as u8]);
                }
                std::fs::write(&keep, ppm).unwrap();
                panic!("{}: {what}; the screen is in {}", self.vm, keep.display());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Clicks button 1 in window `r` and waits for it to be current.
    fn focus(&mut self, r: Rect) {
        self.click((r.0 + r.2) / 2, (r.1 + r.3) / 2, 1);
        self.see("the window made current", |s| {
            s.at(r.0 + 1, r.1 + 20) == FOCUS
        });
    }

    /// Types `cmd` into the current window with its output going to a
    /// new file, and returns what it wrote once it has finished.
    fn run(&mut self, cmd: &str) -> String {
        String::from_utf8_lossy(&self.run_bytes(cmd)).into_owned()
    }

    /// Waits for `/tmp/rio/<name>` to exist.
    fn file(&self, name: &str) {
        let start = Instant::now();
        while !self.tmp.join(name).exists() {
            assert!(start.elapsed() < WAIT, "{}: no {name}", self.vm);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn run_bytes(&mut self, cmd: &str) -> Vec<u8> {
        self.n += 1;
        let out = format!("out{}", self.n);
        self.typed(&format!(
            "{cmd} > /tmp/rio/{out}; echo > /tmp/rio/{out}.done\n"
        ));
        let start = Instant::now();
        while !self.tmp.join(format!("{out}.done")).exists() {
            assert!(start.elapsed() < WAIT, "{}: {cmd} did not finish", self.vm);
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::read(self.tmp.join(&out)).unwrap()
    }

    /// Runs `cmd` until `done` holds for its output.
    fn until(&mut self, cmd: &str, what: &str, done: impl Fn(&str) -> bool) -> String {
        let start = Instant::now();
        loop {
            let got = self.run(cmd);
            if done(&got) {
                return got;
            }
            assert!(
                start.elapsed() < WAIT,
                "{}: {what}: not so after {WAIT:?}:\n{got}",
                self.vm
            );
        }
    }
}

/// The width of `text` in the default font, from its first subfont.
fn width(text: &str) -> i32 {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root/fonts/go/regular.13/0000");
    let sub = purg_memdraw::Subfont::read(&std::fs::read(path).unwrap()).unwrap();
    text.chars()
        .map(|c| i32::from(sub.info[c as usize].width))
        .sum()
}

/// The middle of `draw/demo`'s first bar in window `r`: inside the
/// border, 5 to 18 percent across and 4 to 14 percent down.
fn red_bar(r: Rect) -> (i32, i32) {
    let (x0, y0, x1, y1) = (r.0 + 4, r.1 + 4, r.2 - 4, r.3 - 4);
    let (w, h) = (x1 - x0, y1 - y0);
    (
        x0 + (5 * w / 100 + 18 * w / 100) / 2,
        y0 + (4 * h / 100 + 14 * h / 100) / 2,
    )
}

/// Waits for the demo's first bar in window `r`, with none of its red
/// outside (the screen may show the window's old place for a moment).
fn demo_in(p: &Page, r: Rect, what: &str) {
    let (x, y) = red_bar(r);
    p.see(what, |s| s.at(x, y) == DEMO_RED && s.red_only_inside(r));
}

/// An emu running rio, and the page on its screen. The page goes first,
/// then the emu, then its root.
struct Rio {
    p: Page,
    out: common::Collected,
    w: common::Watched,
    _root: Root,
}

fn start(vm: &str) -> Rio {
    let root = Root::new(vm);
    let mut w = common::Watched::spawn(
        Command::new(env!("CARGO_BIN_EXE_emu"))
            .arg("-r")
            .arg(&root.0)
            .arg(format!("--vm={vm}"))
            .args(["-s", &format!("{}x{}", SIZE.0, SIZE.1), "-w"])
            .args(["sh", "-c", "rio; echo rio: $status"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped()),
    );
    let out = common::Collected::new(w.child.stdout.take().unwrap());
    let (host, token) = url(&w);
    let s = connect(&host, &token);
    let screen = Mirror::start(s.try_clone().unwrap());
    let p = Page {
        s,
        screen,
        tmp: root.0.join("tmp/rio"),
        lasthit: 0,
        lasthit2: 0,
        vm: vm.to_string(),
        n: 0,
    };
    p.see("rio's background", |s| s.at(400, 300) == BACKGROUND);
    Rio {
        p,
        out,
        w,
        _root: root,
    }
}

/// Ends rio with `Exit` and checks that it and the emu end well.
fn exit(r: Rio) {
    let Rio {
        mut p,
        out,
        mut w,
        _root,
    } = r;
    let vm = p.vm.clone();
    p.menu(EXIT);
    let (status, err) = w.finish(WAIT);
    assert!(status.success(), "{vm}: {}", common::failure(status, &err));
    let out = String::from_utf8_lossy(&out.bytes(WAIT)).into_owned();
    assert_eq!(out, "rio: \n", "{vm}: rio failed");
}

fn run(vm: &str) {
    let mut r = start(vm);
    let p = &mut r.p;

    // A window, swept with button 3 after New. It is current: the shell
    // in it reads what is typed.
    let w1 = (20, 20, 380, 280);
    p.menu(NEW);
    p.drag((w1.0, w1.1), (w1.2, w1.3), 4);
    p.see("the new window", |s| s.at(w1.0 + 1, w1.1 + 20) == FOCUS);
    p.typed("echo hello\n");
    // On a slow host the first `cat` can be typed before echo prints, and
    // its line then shows above `hello`, as rio shows a line typed ahead:
    // look for the output line, not for it right after the command.
    let text = p.until("cat /dev/text", "echo's output", |t| {
        t.contains("; echo hello\n") && t.lines().any(|l| l == "hello")
    });
    assert!(text.contains("; cat /dev/text"), "{vm}: {text:?}");
    assert_eq!(p.run("cat /dev/winid").trim(), "1", "{vm}");
    let wctl = p.run("cat /dev/wctl");
    let want = format!(
        "{:11} {:11} {:11} {:11} visible current ",
        w1.0, w1.1, w1.2, w1.3
    );
    assert_eq!(wctl, want, "{vm}: wctl");

    // The demo draws in the window, not on the screen.
    p.typed("draw/demo\n");
    demo_in(p, w1, "the demo in the window");
    assert_eq!(p.screen.at(400, 300), BACKGROUND, "{vm}: background");

    // A second window, for the commands that look.
    let w2 = (420, 20, 780, 280);
    p.menu(NEW);
    p.drag((w2.0, w2.1), (w2.2, w2.3), 4);
    p.see("the second window current, the first not", |s| {
        s.at(w2.0 + 1, w2.1 + 20) == FOCUS && s.at(w1.0 + 1, w1.1 + 20) != FOCUS
    });
    assert!(p.run("ls /dev/wsys").contains("/dev/wsys/2"), "{vm}: wsys");

    // Resize: choose the window, then sweep its new rectangle. The demo
    // hears of it and draws again in the new window.
    let old = w1;
    let w1 = (20, 300, 500, 590);
    p.menu(RESIZE);
    p.click(200, 150, 4);
    p.drag((w1.0, w1.1), (w1.2, w1.3), 4);
    demo_in(p, w1, "the demo after the resize");
    p.see("where the window was", |s| {
        s.at(red_bar(old).0, red_bar(old).1) == BACKGROUND
    });

    // Move: drag the window by 50 right and 20 up.
    p.menu(MOVE);
    p.drag((200, 400), (250, 380), 4);
    let w1 = (w1.0 + 50, w1.1 - 20, w1.2 + 50, w1.3 - 20);
    demo_in(p, w1, "the demo after the move");
    p.see("the moved window's old place", |s| {
        s.at(30, 400) == BACKGROUND
    });

    // Delete: the window goes, and with it the demo, its shell and its
    // rio.
    p.menu(DELETE);
    p.click(300, 450, 4);
    p.see("the deleted window gone", |s| s.at(300, 450) == BACKGROUND);
    p.focus(w2);
    let ps = p.until("ps", "the progs after Delete", |g| !g.contains(" demo"));
    let count = |name: &str| ps.lines().filter(|l| l.ends_with(name)).count();
    // rio and the second window's rio; the emu's sh -c and the second
    // window's shell.
    assert_eq!((count(" rio"), count(" sh")), (2, 2), "{vm}:\n{ps}");

    // acme runs in the second window, in its colours inside the window
    // only, and Exit clicked with button 2 in its row's tag ends it; the
    // window's text comes back and its shell carries on.
    p.typed("acme\n");
    let inner = (w2.0 + 4, w2.1 + 4, w2.2 - 4, w2.3 - 4);
    p.see("acme in the window", |s| {
        s.at(inner.2 - 3, inner.1 + 3) == ACME_TAG && s.at(inner.0 + 30, inner.3 - 3) == ACME_BODY
    });
    let outside = {
        let g = p.screen.0.lock().unwrap();
        (0..SIZE.1).any(|y| {
            (0..SIZE.0).any(|x| {
                let c = g[(y * SIZE.0 + x) as usize];
                (c == ACME_TAG || c == ACME_BODY)
                    && !(x >= inner.0 && x < inner.2 && y >= inner.1 && y < inner.3)
            })
        })
    };
    assert!(!outside, "{vm}: acme drew outside its window");
    let x = inner.0 + 16 + width("Newcol Putall ") + width("Exit") / 2;
    p.click(x, inner.1 + FH / 2, 2);
    p.see("acme gone", |s| s.at(inner.2 - 3, inner.1 + 3) == 0xFFFFFF);
    // Keys typed as acme ended may have gone to it.
    p.until("echo back", "the shell after acme", |g| g == "back\n");

    // Exit ends rio, and its windows' progs.
    exit(r);
}

/// The top left of the text in window `r`: inside the border and rio's
/// margin.
fn text_at(r: Rect) -> (i32, i32) {
    (r.0 + 8, r.1 + 6)
}

/// An image(6) file's header and pixels: its rectangle, and the pixel at
/// `(x, y)` as `0xRRGGBB`.
fn image_pixel(vm: &str, img: &[u8], x: i32, y: i32) -> (Rect, u32) {
    let head = String::from_utf8_lossy(&img[..60]).into_owned();
    let f: Vec<&str> = head.split_whitespace().collect();
    assert_eq!(f[0], "x8r8g8b8", "{vm}: {head:?}");
    let v: Vec<i32> = f[1..5].iter().map(|v| v.parse().unwrap()).collect();
    let r = (v[0], v[1], v[2], v[3]);
    let dx = (r.2 - r.0) as usize;
    assert_eq!(img.len(), 60 + 4 * dx * (r.3 - r.1) as usize, "{vm}: size");
    let o = 60 + 4 * ((y - r.1) as usize * dx + (x - r.0) as usize);
    let px = u32::from(img[o + 2]) << 16 | u32::from(img[o + 1]) << 8 | u32::from(img[o]);
    (r, px)
}

/// rio's rule for a window when its screen changes size: the same place,
/// scaled.
fn scaled(r: Rect, from: Rect, to: Rect) -> Rect {
    let x = |v: i32| to.0 + (v - from.0) * (to.2 - to.0) / (from.2 - from.0);
    let y = |v: i32| to.1 + (v - from.1) * (to.3 - to.1) / (from.3 - from.1);
    (x(r.0), y(r.1), x(r.2), y(r.3))
}

fn editing(vm: &str) {
    let mut r = start(vm);
    let p = &mut r.p;

    // A tall window, and in it commands whose output is commands.
    let w1 = (20, 20, 380, 560);
    let (ax, ay) = text_at(w1);
    p.menu(NEW);
    p.drag((w1.0, w1.1), (w1.2, w1.3), 4);
    p.see("the first window", |s| s.at(w1.0 + 1, w1.1 + 20) == FOCUS);
    p.typed("echo touch /tmp/rio/sent1\necho touch /tmp/rio/sent2\n");
    // The prompt after the second command's output, before typing more:
    // a line typed sooner would be read with the second.
    p.see("the prompt on the fifth line", |s| {
        s.ink((ax, ay + 4 * FH, ax + width("; "), ay + 5 * FH))
    });
    p.until("cat /dev/text", "echo's output", |t| {
        t.starts_with(
            "; echo touch /tmp/rio/sent1\ntouch /tmp/rio/sent1\n\
             ; echo touch /tmp/rio/sent2\ntouch /tmp/rio/sent2\n",
        )
    });

    // A double click at the start of the second line selects the line;
    // button 2's snarf copies it to /dev/snarf.
    let line = |i: i32| ay + i * FH + FH / 2;
    p.click(ax, line(1), 1);
    p.click(ax, line(1), 1);
    p.menu2((200, 400), SNARF);

    // A second window (made after the snarf: pointer events are taken in
    // order), where send runs the line.
    let w2 = (420, 20, 780, 280);
    p.menu(NEW);
    p.drag((w2.0, w2.1), (w2.2, w2.3), 4);
    p.see("the second window", |s| s.at(w2.0 + 1, w2.1 + 20) == FOCUS);
    p.menu2((600, 150), SEND);
    p.file("sent1");
    assert_eq!(
        p.run("cat /dev/snarf"),
        "touch /tmp/rio/sent1\n",
        "{vm}: /dev/snarf"
    );

    // Sweep the fourth line and its newline with button 1, snarf it, and
    // paste it into the second window, where it runs.
    p.focus(w1);
    p.drag((ax, line(3)), (ax, line(4)), 1);
    p.menu2((200, 400), SNARF);
    p.focus(w2);
    p.menu2((600, 150), PASTE);
    p.file("sent2");

    // Button 1 on the first window's bottom right corner drags it.
    let old = w1;
    let w1 = (20, 20, 400, 500);
    p.drag((old.2 - 2, old.3 - 2), (w1.2 - 2, w1.3 - 2), 1);
    p.see("the resized window", |s| {
        s.at(w1.2 - 2, 300) == FOCUS && s.at(old.0 + 30, old.3 - 20) == BACKGROUND
    });
    let want = format!(
        "{:11} {:11} {:11} {:11} visible current ",
        w1.0, w1.1, w1.2, w1.3
    );
    assert_eq!(p.run("cat /dev/wctl"), want, "{vm}: wctl after the drag");

    // /dev/window and /dev/screen are images.
    let img = p.run_bytes("cat /dev/window");
    let (ir, px) = image_pixel(vm, &img, w1.0 + 1, w1.1 + 20);
    assert_eq!((ir, px), (w1, FOCUS), "{vm}: /dev/window");
    let img = p.run_bytes("cat /dev/screen");
    let (ir, px) = image_pixel(vm, &img, 700, 500);
    assert_eq!(
        (ir, px),
        ((0, 0, SIZE.0, SIZE.1), BACKGROUND),
        "{vm}: /dev/screen"
    );
    assert_eq!(image_pixel(vm, &img, w1.0 + 1, w1.1 + 20).1, FOCUS, "{vm}");

    // Hide takes the window off the screen; its label shows it again.
    p.menu(HIDE);
    p.click(100, 300, 4);
    p.see("the hidden window gone", |s| {
        s.at(100, 300) == BACKGROUND && s.at(w2.0 + 1, w2.1 + 20) == FOCUS
    });
    assert!(
        p.run("cat /dev/wsys/1/wctl").contains(" hidden "),
        "{vm}: wctl of a hidden window"
    );
    p.menu(HIDDEN);
    p.see("the window shown again", |s| {
        s.at(w1.0 + 1, w1.1 + 20) == FOCUS
    });
    // That item has gone from the menu, and rio puts its last item under
    // the pointer instead.
    p.lasthit = EXIT;

    // A program that draws once and exits leaves its picture.
    let w3 = (440, 300, 760, 580);
    p.typed(&format!(
        "echo new -r {} {} {} {} 'test/draw -c dd22dd; echo > /tmp/rio/drawn; cat' > /dev/wctl\n",
        w3.0, w3.1, w3.2, w3.3
    ));
    p.file("drawn");
    p.see("the program's picture", |s| s.at(600, 440) == 0xDD22DD);
    p.focus(w1);
    assert_eq!(p.screen.at(600, 440), 0xDD22DD, "{vm}: the picture went");

    // rio in the second window, with a window of its own; resizing the
    // second window scales the inner rio's window to its new screen.
    p.focus(w2);
    p.typed("rio -i cat\n");
    let screen = (w2.0 + 4, w2.1 + 4, w2.2 - 4, w2.3 - 4);
    let (sw, sh) = (screen.2 - screen.0, screen.3 - screen.1);
    let (iw, ih) = ((sw * 2 / 3).max(50), (sh * 2 / 3).max(50));
    let inner = (
        screen.0 + sw / 6,
        screen.1 + sh / 6,
        screen.0 + sw / 6 + iw,
        screen.1 + sh / 6 + ih,
    );
    p.see("the inner rio's window", |s| {
        s.at(inner.0 + 1, inner.1 + 20) == FOCUS && s.at(screen.0 + 2, screen.1 + 2) == BACKGROUND
    });
    let old2 = w2;
    let w2 = (420, 20, 620, 200);
    p.menu(RESIZE);
    p.click(old2.0 + 10, old2.3 - 10, 4);
    p.drag((w2.0, w2.1), (w2.2, w2.3), 4);
    let screen2 = (w2.0 + 4, w2.1 + 4, w2.2 - 4, w2.3 - 4);
    let inner2 = scaled(inner, screen, screen2);
    p.see("the inner window scaled", |s| {
        s.at(inner2.0 + 1, inner2.1 + 20) == FOCUS
            && s.at(inner2.2 - 2, inner2.3 - 20) == FOCUS
            && s.at(old2.2 - 10, old2.3 - 10) == BACKGROUND
    });

    // Delete the second window, and with it the inner rio.
    p.menu(DELETE);
    p.click(w2.0 + 10, w2.3 - 10, 4);
    p.see("the second window gone", |s| {
        s.at(w2.0 + 10, w2.3 - 10) == BACKGROUND
    });
    exit(r);
}

#[test]
fn editing_wasmtime() {
    editing("wasmtime");
}

#[test]
fn editing_wasmi() {
    editing("wasmi");
}

#[test]
fn windows_wasmtime() {
    run("wasmtime");
}

#[test]
fn windows_wasmi() {
    run("wasmi");
}
