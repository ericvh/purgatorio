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
const EXIT: usize = 5;
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
        String::from_utf8_lossy(&std::fs::read(self.tmp.join(&out)).unwrap()).into_owned()
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

fn run(vm: &str) {
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
    let mut p = Page {
        s,
        screen,
        tmp: root.0.join("tmp/rio"),
        lasthit: 0,
        vm: vm.to_string(),
        n: 0,
    };
    p.see("rio's background", |s| s.at(400, 300) == BACKGROUND);

    // A window, swept with button 3 after New. It is current: the shell
    // in it reads what is typed.
    let w1 = (20, 20, 380, 280);
    p.menu(NEW);
    p.drag((w1.0, w1.1), (w1.2, w1.3), 4);
    p.see("the new window", |s| s.at(w1.0 + 1, w1.1 + 20) == FOCUS);
    p.typed("echo hello\n");
    let text = p.until("cat /dev/text", "echo's output", |t| {
        t.contains("echo hello\nhello\n")
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
    demo_in(&p, w1, "the demo in the window");
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
    demo_in(&p, w1, "the demo after the resize");
    p.see("where the window was", |s| {
        s.at(red_bar(old).0, red_bar(old).1) == BACKGROUND
    });

    // Move: drag the window by 50 right and 20 up.
    p.menu(MOVE);
    p.drag((200, 400), (250, 380), 4);
    let w1 = (w1.0 + 50, w1.1 - 20, w1.2 + 50, w1.3 - 20);
    demo_in(&p, w1, "the demo after the move");
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
    p.menu(EXIT);
    let (status, err) = w.finish(WAIT);
    assert!(status.success(), "{vm}: {}", common::failure(status, &err));
    let out = String::from_utf8_lossy(&out.bytes(WAIT)).into_owned();
    assert_eq!(out, "rio: \n", "{vm}: rio failed");
}

#[test]
fn windows_wasmtime() {
    run("wasmtime");
}

#[test]
fn windows_wasmi() {
    run("wasmi");
}
