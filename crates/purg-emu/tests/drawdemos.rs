//! The draw demos (`draw/clock`, `draw/coffee`, `draw/faces`, `draw/sh`)
//! driven from outside, on both VMs. The emu serves its screen with `-w`
//! and this test is the browser (as in `web.rs` and `acme.rs`): it keeps
//! a copy of the screen from the frames the emu sends, and sends pointer
//! and key events. The emu runs `sh -i` with its standard input and
//! output on pipes, so the test starts each program in the background,
//! kills it, and lists the progs with `ps`.
//!
//! Each step waits for a condition (a pixel, the screen staying the same
//! for a moment, `ps` showing what it should), bounded by [`WAIT`]. The
//! clock runs at a fixed time (`-t`) and `draw/sh`'s text is fixed, so
//! their screens are compared with goldens in `tests/golden/` (PPM
//! compressed with zlib); `PURG_GOLDEN_UPDATE=1` rewrites them, and on a
//! mismatch, or with `PURG_GOLDEN_SHOW=1`, the screen is written to the
//! target's test directory. Requires `cargo xtask build-root` first.

use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use purg_emu::web::{OP_BINARY, OP_CLOSE, accept_key, read_frame, write_frame};

mod common;

const WAIT: Duration = Duration::from_secs(240);
/// How long the screen must stay unchanged to count as drawn.
const QUIET: Duration = Duration::from_millis(700);
/// 2026-11-13 12:34:56 GMT.
const FIXED: &str = "1794573296";
const MARK: &str = "@@end@@";

fn root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root");
    for b in ["clock", "coffee", "faces", "sh"] {
        assert!(
            root.join(format!("bin/draw/{b}.wasm")).exists(),
            "guest binaries missing; run `cargo xtask build-root` first"
        );
    }
    root
}

/// The screen as the page sees it, `0xRRGGBB` per pixel, and the frames
/// so far.
struct Shown {
    w: usize,
    px: Vec<u32>,
    frames: usize,
    last: Instant,
    closed: bool,
}

#[derive(Clone)]
struct Screen(Arc<Mutex<Shown>>);

impl Screen {
    fn start(mut s: TcpStream, w: usize, h: usize) -> Screen {
        let sc = Screen(Arc::new(Mutex::new(Shown {
            w,
            px: vec![0; w * h],
            frames: 0,
            last: Instant::now(),
            closed: false,
        })));
        let shared = sc.0.clone();
        std::thread::spawn(move || {
            loop {
                match read_frame(&mut s, 1 << 26) {
                    Ok(f) if f.opcode == OP_BINARY && f.payload.first() == Some(&1) => {
                        let word = |i: usize| {
                            u32::from_le_bytes(f.payload[i..i + 4].try_into().unwrap()) as usize
                        };
                        let (x, y, dx, dy) = (word(1), word(5), word(9), word(13));
                        let rgba =
                            miniz_oxide::inflate::decompress_to_vec_zlib(&f.payload[17..]).unwrap();
                        let mut g = shared.lock().unwrap();
                        let w = g.w;
                        for j in 0..dy {
                            for i in 0..dx {
                                let p = &rgba[(j * dx + i) * 4..][..3];
                                g.px[(y + j) * w + x + i] =
                                    u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2]);
                            }
                        }
                        g.frames += 1;
                        g.last = Instant::now();
                    }
                    Ok(f) if f.opcode == OP_CLOSE => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            shared.lock().unwrap().closed = true;
        });
        sc
    }

    fn at(&self, x: usize, y: usize) -> u32 {
        let g = self.0.lock().unwrap();
        g.px[y * g.w + x]
    }

    fn frames(&self) -> usize {
        self.0.lock().unwrap().frames
    }

    fn pixels(&self) -> Vec<u32> {
        self.0.lock().unwrap().px.clone()
    }

    /// Waits until `cond` holds of the screen.
    fn until(&self, what: &str, cond: impl Fn(&Screen) -> bool) {
        let start = Instant::now();
        while !cond(self) {
            assert!(!self.0.lock().unwrap().closed, "{what}: the page closed");
            assert!(start.elapsed() < WAIT, "{what}: not so after {WAIT:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for a frame after the `before`th and then for the screen to
    /// stay unchanged for [`QUIET`].
    fn settle(&self, before: usize, what: &str) {
        self.until(what, |s| {
            let g = s.0.lock().unwrap();
            g.frames > before && g.last.elapsed() >= QUIET
        });
    }
}

/// An emu running `sh -i`, with its screen on a page.
struct Emu {
    w: common::Watched,
    stdin: Option<ChildStdin>,
    out: common::Collected,
    seen: usize,
    s: TcpStream,
    screen: Screen,
    vm: String,
}

impl Emu {
    fn start(vm: &str, size: (usize, usize)) -> Emu {
        let mut w = common::Watched::spawn(
            Command::new(env!("CARGO_BIN_EXE_emu"))
                .arg("-r")
                .arg(root())
                .arg(format!("--vm={vm}"))
                .args(["-s", &format!("{}x{}", size.0, size.1), "-w"])
                .args(["sh", "-i"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped()),
        );
        let stdin = w.child.stdin.take();
        let out = common::Collected::new(w.child.stdout.take().unwrap());
        let (host, token) = url(&w);
        let s = connect(&host, &token);
        let screen = Screen::start(s.try_clone().unwrap(), size.0, size.1);
        Emu {
            w,
            stdin,
            out,
            seen: 0,
            s,
            screen,
            vm: vm.to_string(),
        }
    }

    /// Runs `line` in the emu's shell and returns what it printed.
    fn cmd(&mut self, line: &str) -> String {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}; echo {MARK}").unwrap();
        stdin.flush().unwrap();
        let start = Instant::now();
        loop {
            let all = self.out.bytes(Duration::ZERO);
            let new = String::from_utf8_lossy(&all[self.seen..]).into_owned();
            if let Some(i) = new.find(&format!("{MARK}\n")) {
                self.seen += i + MARK.len() + 1;
                return new[..i].to_string();
            }
            assert!(
                start.elapsed() < WAIT,
                "{}: no answer to {line:?}; so far:\n{new}",
                self.vm
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Starts `line` in the background and returns its pid.
    fn background(&mut self, line: &str) -> u32 {
        let out = self.cmd(&format!("{line} & echo pid $apid"));
        out.lines()
            .find_map(|l| l.trim_start_matches("; ").strip_prefix("pid "))
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or_else(|| panic!("{}: no pid for {line:?} in {out:?}", self.vm))
    }

    /// The names of the progs `ps` lists, but for the shell and ps.
    fn progs(&mut self) -> Vec<String> {
        let out = self.cmd("ps");
        let mut names: Vec<String> = out
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (f.len() >= 6 && f[0].parse::<u32>().is_ok()).then(|| f[5..].join(" "))
            })
            .collect();
        // The emu's sh -i and ps itself.
        for n in ["sh", "ps"] {
            if let Some(i) = names.iter().position(|x| x == n) {
                names.remove(i);
            }
        }
        names
    }

    /// Waits until `ps` shows `cond` holding of the other progs.
    fn progs_until(&mut self, what: &str, cond: impl Fn(&[String]) -> bool) {
        let start = Instant::now();
        loop {
            let p = self.progs();
            if cond(&p) {
                return;
            }
            assert!(
                start.elapsed() < WAIT,
                "{}: {what}: progs are {p:?}",
                self.vm
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Kills `pid` and waits for no prog but the shell to be left.
    fn kill_and_check(&mut self, pid: u32, what: &str) {
        self.cmd(&format!("kill {pid}"));
        self.progs_until(&format!("after killing {what}"), <[String]>::is_empty);
    }

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

    /// Types `s` a key at a time, as a browser sends them: `\n` is Enter,
    /// `\x08` Backspace, `\x7F` Delete, and `^` before a letter holds
    /// control.
    fn typed(&mut self, s: &str) {
        let mut ctl = false;
        for c in s.chars() {
            if c == '^' {
                ctl = true;
                continue;
            }
            let mut m = vec![b'k', u8::from(ctl)];
            ctl = false;
            match c {
                '\n' => m.extend_from_slice(b"Enter"),
                '\x08' => m.extend_from_slice(b"Backspace"),
                '\x7F' => m.extend_from_slice(b"Delete"),
                c => m.extend_from_slice(c.to_string().as_bytes()),
            }
            self.send(&m);
        }
    }

    /// Ends the emu's shell and the emu.
    fn finish(mut self) {
        drop(self.stdin.take());
        let (status, err) = self.w.finish(WAIT);
        assert!(
            status.success(),
            "{}: {}",
            self.vm,
            common::failure(status, &err)
        );
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

/// Opens the page's WebSocket.
fn connect(host: &str, token: &str) -> TcpStream {
    use std::io::Read;
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

/// Compares the screen with golden `name`.
fn golden(e: &Emu, size: (usize, usize), name: &str) {
    let mut ppm = format!("P6 {} {} 255\n", size.0, size.1).into_bytes();
    ppm.extend(
        e.screen
            .pixels()
            .iter()
            .flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]),
    );
    let golden =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/golden/{name}.ppm.z"));
    if std::env::var_os("PURG_GOLDEN_UPDATE").is_some() {
        std::fs::write(
            &golden,
            miniz_oxide::deflate::compress_to_vec_zlib(&ppm, 10),
        )
        .unwrap();
    }
    let want = std::fs::read(&golden)
        .ok()
        .and_then(|z| miniz_oxide::inflate::decompress_to_vec_zlib(&z).ok())
        .unwrap_or_default();
    let keep = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}.ppm", e.vm));
    if std::env::var_os("PURG_GOLDEN_SHOW").is_some() {
        std::fs::write(&keep, &ppm).unwrap();
        eprintln!("{}: the screen is in {}", e.vm, keep.display());
    }
    if ppm != want {
        std::fs::write(&keep, &ppm).unwrap();
        panic!(
            "{}: the screen differs from {}; it is in {}",
            e.vm,
            golden.display(),
            keep.display()
        );
    }
}

fn clock(vm: &str) {
    const SIZE: (usize, usize) = (320, 240);
    let mut e = Emu::start(vm, SIZE);
    // The analog face at a fixed time: the hub's red at the centre, the
    // wall's colour in a corner.
    let n = e.screen.frames();
    let pid = e.background(&format!("draw/clock -t {FIXED}"));
    e.screen.until("the clock's face", |s| {
        s.at(160, 120) == 0xCC2A1E && s.at(1, 1) == 0xE8E4D8
    });
    e.screen.settle(n, "the clock");
    golden(&e, SIZE, "clock");
    e.kill_and_check(pid, "draw/clock");
    // The seven-segment clock.
    let n = e.screen.frames();
    let pid = e.background(&format!("draw/clock -d -t {FIXED}"));
    e.screen
        .until("the digital clock", |s| s.at(1, 1) == 0x141814);
    e.screen.settle(n, "the digital clock");
    golden(&e, SIZE, "clock-d");
    e.kill_and_check(pid, "draw/clock -d");
    e.finish();
}

#[test]
fn clock_wasmtime() {
    clock("wasmtime");
}

#[test]
fn clock_wasmi() {
    clock("wasmi");
}

fn coffee(vm: &str) {
    const SIZE: (usize, usize) = (320, 240);
    let mut e = Emu::start(vm, SIZE);
    let pid = e.background("draw/coffee");
    // Where draw/coffee puts things on a 320x240 screen: a unit is 2.4
    // pixels, the table's top is at 187, the rim at 106, the cup's
    // centre at x 160 and its red band from 128 to 140.
    e.screen.until("the cup", |s| {
        s.at(155, 155) == 0xFBFAF6
            && s.at(155, 134) == 0xC0432C
            && s.at(2, SIZE.1 - 2) == 0x8A5530
            && s.at(170, 107) == 0x4A2A14
    });
    // The steam moves: frames keep coming.
    let n = e.screen.frames();
    e.screen.until("the steam moving", |s| s.frames() >= n + 10);
    e.kill_and_check(pid, "draw/coffee");
    e.finish();
}

#[test]
fn coffee_wasmtime() {
    coffee("wasmtime");
}

#[test]
fn coffee_wasmi() {
    coffee("wasmi");
}

fn faces(vm: &str) {
    const SIZE: (usize, usize) = (400, 300);
    const BACK: u32 = 0xF2F0E6;
    // draw/faces's grid with the default font, 19 pixels high: tiles
    // 76x81 from (10, 35), 10 apart.
    let tile = |i: usize| (10 + i * 86, 35);
    let count = |s: &Screen| {
        (0..4)
            .take_while(|&i| {
                let (x, y) = tile(i);
                s.at(x + 1, y + 1) != BACK
            })
            .count()
    };
    let mut e = Emu::start(vm, SIZE);
    let pid = e.background("draw/faces -i 0.2");
    // sh and faces.
    e.screen
        .until("two faces", |s| s.at(1, 1) == BACK && count(s) == 2);
    // A new command gets a highlighted tile, last in name order.
    let sleep = e.background("sleep 1000");
    e.screen.until("a face for sleep", |s| {
        let (x, y) = tile(2);
        count(s) == 3 && s.at(x + 1, y + 1) == 0xE0A800
    });
    // Button 1 on it shows its progs on the bottom line.
    let (x, y) = tile(2);
    e.pointer(x as i32 + 30, y as i32 + 30, 1);
    e.pointer(x as i32 + 30, y as i32 + 30, 0);
    e.screen
        .until("the details", |s| s.at(1, SIZE.1 - 2) == 0xE2DFD0);
    e.cmd(&format!("kill {sleep}"));
    e.screen.until("sleep's face gone", |s| count(s) == 2);
    e.kill_and_check(pid, "draw/faces");
    e.finish();
}

#[test]
fn faces_wasmtime() {
    faces("wasmtime");
}

#[test]
fn faces_wasmi() {
    faces("wasmi");
}

fn shell(vm: &str) {
    const SIZE: (usize, usize) = (480, 200);
    // The default font's lines are 19 pixels, from 4 down; text starts
    // 16 pixels in.
    const FH: usize = 19;
    let ink_on = |s: &Screen, line: usize| {
        (4 + line * FH..4 + (line + 1) * FH).any(|y| (16..60).any(|x| s.at(x, y) == 0))
    };
    let mut e = Emu::start(vm, SIZE);
    let n = e.screen.frames();
    e.background("draw/sh");
    e.screen
        .until("the prompt", |s| s.at(2, 2) == 0xE6E6E6 && ink_on(s, 0));
    e.screen.settle(n, "the prompt");
    // Typing, Backspace and ^U; then a command and its output.
    e.typed("junk^uecho hellp\x08o\n");
    e.screen.until("echo's output", |s| ink_on(s, 2));
    let n = e.screen.frames();
    e.screen.settle(n.saturating_sub(1), "echo's output");
    golden(&e, SIZE, "drawsh");
    // Delete interrupts cat, and the shell prompts again.
    e.typed("cat\n");
    e.progs_until("cat running", |p| p.iter().any(|n| n == "cat"));
    e.typed("\x7F");
    e.progs_until("cat interrupted", |p| !p.iter().any(|n| n == "cat"));
    e.screen.until("the prompt after cat", |s| ink_on(s, 3));
    // exit ends the shell and the window, leaving nothing.
    e.typed("exit\n");
    e.progs_until("after exit", <[String]>::is_empty);
    // Killing the window kills the shell's group, even with a command
    // running in it.
    let n = e.screen.frames();
    let pid = e.background("draw/sh");
    e.screen.settle(n, "the second window");
    e.typed("cat\n");
    e.progs_until("the second cat", |p| p.iter().any(|n| n == "cat"));
    e.kill_and_check(pid, "draw/sh");
    e.finish();
}

#[test]
fn shell_wasmtime() {
    shell("wasmtime");
}

#[test]
fn shell_wasmi() {
    shell("wasmi");
}
