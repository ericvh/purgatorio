//! acme driven from outside, on both VMs. The emu serves its screen with
//! `-w` and this test is the browser (as in `web.rs`): it sends pointer
//! and key events over the WebSocket. acme opens a file, the test clicks
//! in the body and types, types `Put` in the window's tag and clicks it
//! with button 2, runs `ls` the same way, undoes the typing, searches
//! with button 3, and ends acme with `Exit` twice (the first warns).
//! Then it checks the file on the host, the `+Errors` output on the
//! screen, and the screen `test/draw -d` dumps against a golden image.
//!
//! The golden, `tests/golden/acme.ppm.z`, is the screen as a PPM
//! compressed with zlib. `PURG_GOLDEN_UPDATE=1` rewrites it; on a
//! mismatch, or with `PURG_GOLDEN_SHOW=1`, the screen is written to the
//! target's test directory as `acme-<vm>.ppm`.
//! Requires `cargo xtask build-root` first.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use purg_emu::web::{OP_BINARY, OP_CLOSE, accept_key, read_frame, write_frame};

mod common;

/// Wide enough for the window's tag to stay one line; short, to keep
/// the golden small.
const SIZE: (i32, i32) = (800, 300);
const WAIT: Duration = Duration::from_secs(240);
/// The scroll bar's last column, which the thumb leaves dark.
const SCROLL_EDGE: i32 = 11;
/// How long the screen must stay unchanged for acme to count as idle.
const QUIET: Duration = Duration::from_millis(700);

/// A root of its own: links to `root/`'s entries but a fresh `tmp`.
struct Root(PathBuf);

impl Root {
    fn new(vm: &str) -> Root {
        static N: AtomicUsize = AtomicUsize::new(0);
        let real =
            std::fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root"))
                .unwrap();
        assert!(
            real.join("bin/acme.wasm").exists()
                && real.join("bin/test/draw.wasm").exists()
                && real.join("bin/test/acmefs.wasm").exists()
                && real.join("bin/win.wasm").exists(),
            "guest binaries missing; run `cargo xtask build-root` first"
        );
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("acme")
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
        std::fs::create_dir_all(path.join("tmp/acme")).unwrap();
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

/// Counts the frames the emu sends, and when the last came.
#[derive(Clone)]
struct Frames(Arc<Mutex<(usize, Instant, bool)>>);

impl Frames {
    fn start(mut s: TcpStream) -> Frames {
        let f = Frames(Arc::new(Mutex::new((0, Instant::now(), false))));
        let shared = f.0.clone();
        std::thread::spawn(move || {
            loop {
                match read_frame(&mut s, 1 << 26) {
                    Ok(fr) if fr.opcode == OP_BINARY => {
                        let mut g = shared.lock().unwrap();
                        g.0 += 1;
                        g.1 = Instant::now();
                    }
                    Ok(fr) if fr.opcode == OP_CLOSE => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            shared.lock().unwrap().2 = true;
        });
        f
    }

    fn count(&self) -> usize {
        self.0.lock().unwrap().0
    }

    /// Waits for a frame after the `before`th, then for the screen to
    /// stay unchanged for [`QUIET`].
    fn settle(&self, before: usize, what: &str) {
        let start = Instant::now();
        loop {
            let (n, last, closed) = *self.0.lock().unwrap();
            if n > before && last.elapsed() >= QUIET {
                return;
            }
            assert!(!closed, "{what}: the emu closed the page");
            assert!(start.elapsed() < WAIT, "{what}: the screen did not change");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

struct Page {
    s: TcpStream,
    frames: Frames,
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

    /// Moves to `(x, y)`, then presses and releases `button`, and waits
    /// for acme to draw what it did.
    fn click(&mut self, x: i32, y: i32, button: u32, what: &str) {
        let n = self.frames.count();
        self.pointer(x, y, 0);
        std::thread::sleep(Duration::from_millis(100));
        self.pointer(x, y, button);
        self.pointer(x, y, 0);
        self.frames.settle(n, what);
    }

    /// Types `s` at the pointer, a key at a time as a browser sends them.
    fn typed(&mut self, s: &str, what: &str) {
        let n = self.frames.count();
        for c in s.chars() {
            let mut m = vec![b'k', 0];
            match c {
                '\n' => m.extend_from_slice(b"Enter"),
                '\x08' => m.extend_from_slice(b"Backspace"),
                '\x7F' => m.extend_from_slice(b"Delete"),
                c => m.extend_from_slice(c.to_string().as_bytes()),
            }
            self.send(&m);
        }
        self.frames.settle(n, what);
    }

    fn hover(&mut self, x: i32, y: i32) {
        self.pointer(x, y, 0);
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn run(vm: &str) {
    let root = Root::new(vm);
    let file = root.0.join("tmp/acme/notes");
    std::fs::write(&file, "hello\n").unwrap();
    let mut w = common::Watched::spawn(
        Command::new(env!("CARGO_BIN_EXE_emu"))
            .arg("-r")
            .arg(&root.0)
            .arg(format!("--vm={vm}"))
            .args(["-s", &format!("{}x{}", SIZE.0, SIZE.1), "-w"])
            .args([
                "sh",
                "-c",
                "acme /tmp/acme/notes; echo acme: $status; test/draw -d",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped()),
    );
    let out = common::Collected::new(w.child.stdout.take().unwrap());
    let (host, token) = url(&w);
    let s = connect(&host, &token);
    let frames = Frames::start(s.try_clone().unwrap());
    let mut p = Page { s, frames };
    p.frames.settle(0, "acme's first screen");

    // The default font's lines are 19 pixels: the row's tag is at the
    // top, a column's tag under it after a 1-pixel line, then the
    // window's tag, then its body. The left column is the left half.
    const FH: i32 = 19;
    let tag_end = (SIZE.0 / 2 - 6, 2 * (FH + 1) + FH / 2);
    // Click in the body below the text: the cursor goes to the end.
    p.click(100, 200, 1, "click in the body");
    p.typed("from acme\n", "typing in the body");
    // Type Put at the end of the window's tag and click it.
    p.hover(tag_end.0, tag_end.1);
    p.typed("Put", "typing Put");
    p.click(tag_end.0, tag_end.1, 2, "Put");
    let want = "hello\nfrom acme\n";
    assert_eq!(std::fs::read_to_string(&file).unwrap(), want, "{vm}: Put");
    // A command for sh: its output goes to /tmp/acme/+Errors.
    p.hover(tag_end.0, tag_end.1);
    p.typed("\x08\x08\x08ls", "typing ls");
    p.click(tag_end.0, tag_end.1, 2, "ls");
    // Undo takes back the typing, which leaves the file modified.
    p.hover(tag_end.0, tag_end.1);
    p.typed("\x08\x08Undo", "typing Undo");
    p.click(tag_end.0, tag_end.1, 2, "Undo");
    // Button 3 on a word in the body searches for it and selects it.
    p.click(30, 3 * (FH + 1) + FH / 2, 4, "button 3");
    // The first Exit warns about the modified file; the second exits.
    p.hover(tag_end.0, tag_end.1);
    p.typed("\x08\x08\x08\x08Exit", "typing Exit");
    p.click(tag_end.0, tag_end.1, 2, "the first Exit");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), want, "{vm}: Undo");
    // acme exits, so the screen may not change; the emu ends instead.
    p.pointer(tag_end.0, tag_end.1, 2);
    p.pointer(tag_end.0, tag_end.1, 0);

    let (status, err) = w.finish(WAIT);
    assert!(status.success(), "{vm}: {}", common::failure(status, &err));
    let out = out.bytes(WAIT);
    let nl = out.iter().position(|&b| b == b'\n').unwrap();
    assert_eq!(&out[..nl], b"acme: ", "{vm}: acme failed");
    let rest = &out[nl + 1..];
    let nl = rest.iter().position(|&b| b == b'\n').unwrap();
    assert_eq!(
        String::from_utf8_lossy(&rest[..nl]),
        format!("{} {}", SIZE.0, SIZE.1)
    );
    let px = &rest[nl + 1..];
    assert_eq!(px.len(), (SIZE.0 * SIZE.1 * 4) as usize);
    let at = |x: i32, y: i32| {
        let i = ((y * SIZE.0 + x) * 4) as usize;
        u32::from_le_bytes(px[i..i + 4].try_into().unwrap()) & 0xFF_FFFF
    };
    // The row's tag and a body in acme's colours, and the scroll bar's
    // thumb covering all of a short file.
    assert_eq!(at(SIZE.0 - 3, 3), 0xEAFFFF, "{vm}: row tag");
    assert_eq!(at(100, SIZE.1 - 3), 0xFFFFEA, "{vm}: body");
    assert_eq!(at(3, 70), 0xFFFFEA, "{vm}: scroll bar");
    assert_eq!(at(SCROLL_EDGE, 70), 0x99994C, "{vm}: scroll bar's edge");

    // The golden is the screen as a PPM, compressed.
    let mut ppm = format!("P6 {} {} 255\n", SIZE.0, SIZE.1).into_bytes();
    ppm.extend(px.chunks(4).flat_map(|p| [p[2], p[1], p[0]]));
    let golden = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/acme.ppm.z");
    if std::env::var_os("PURG_GOLDEN_UPDATE").is_some() {
        std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
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
    let keep = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("acme-{vm}.ppm"));
    if std::env::var_os("PURG_GOLDEN_SHOW").is_some() {
        std::fs::write(&keep, &ppm).unwrap();
        eprintln!("{vm}: the screen is in {}", keep.display());
    }
    if ppm != want {
        std::fs::write(&keep, &ppm).unwrap();
        panic!(
            "{vm}: the screen differs from {}; it is in {}",
            golden.display(),
            keep.display()
        );
    }
}

#[test]
fn edit_put_run_exit_wasmtime() {
    run("wasmtime");
}

#[test]
fn edit_put_run_exit_wasmi() {
    run("wasmi");
}

/// Runs `test/acmefs` (by a button 2 click at `at`) in `mode` until
/// `done` holds for what it reports, and returns that; fails with `what`
/// once [`WAIT`] has passed.
fn until(
    p: &mut Page,
    tmp: &Path,
    out: &common::Collected,
    at: (i32, i32),
    mode: &str,
    what: &str,
    done: impl Fn(&str) -> bool,
) -> String {
    std::fs::write(tmp.join("mode"), mode).unwrap();
    let start = Instant::now();
    loop {
        p.click(at.0, at.1, 2, what);
        let got = results(tmp, what, out);
        if done(&got) {
            return got;
        }
        assert!(
            start.elapsed() < WAIT,
            "{what}: not so after {WAIT:?}:\n{got}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Where to click with button 2 to execute the last word of a tag holding
/// `text`, drawn from `x0` in `width` pixels with lines `fh` high from
/// `y0`: right of the end of its last line. The tag wraps as acme's
/// frames do, by the default font's widths.
fn tag_end_at(text: &str, x0: i32, width: i32, y0: i32, fh: i32) -> (i32, i32) {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root/fonts/go/regular.13/0000");
    let sub = purg_memdraw::Subfont::read(&std::fs::read(path).unwrap()).unwrap();
    let (mut x, mut line) = (0, 0);
    for c in text.chars() {
        let i = c as usize;
        assert!(i < sub.n(), "{c:?} is not in the font's first subfont");
        let w = i32::from(sub.info[i].width);
        if x + w > width && x > 0 {
            line += 1;
            x = 0;
        }
        x += w;
    }
    ((x0 + x + 4).min(x0 + width - 2), y0 + line * fh + fh / 2)
}

/// Waits for `test/acmefs` to write its results, and returns them.
fn results(tmp: &Path, what: &str, con: &common::Collected) -> String {
    let out = tmp.join("out");
    let start = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(&out)
            && s.ends_with("end\n")
        {
            let _ = std::fs::remove_file(&out);
            return s;
        }
        assert!(
            start.elapsed() < WAIT,
            "{what}: no results from test/acmefs; the console:\n{}",
            common::tail(&con.bytes(Duration::ZERO))
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// What `test/acmefs` reports of acme(4) (see `appl/test/acmefs`).
const FS_WANT: &str = "\
ctl: 8 fields, body 0 dir 0 dirty 0 width>0 true font /fonts/default.font tab>0 true
body: \"one\\ntwo\\nthree\\n\"
addr 2: 4 8
addr #3,#5: 3 5
addr /thr/: 8 11
addr 0: 0 0
addr $: 14 14
addr 3-1: 4 8
addr 2,3: 4 14
addr /o/: 0 1
addr ?o?: 6 7
addr /(/: bad address syntax
addr 9: address out of range
xdata #3,#5: \"\\nt\"
dot=addr: ok
addr=dot: 8 11
data write at 2: addr 8 8
data from 0: \"one\\nTWO\\nthree\\n\"
addr after: 14 14
dirty: 1
clean: 0
bogus: ill-formed control message
name: ok
index: /tmp/acme/fsw Del Snarf Undo | Look
tag: /tmp/acme/fsw Del Snarf Undo | Look Extra
cleartag: /tmp/acme/fsw Del Snarf Undo |
event: MX 0 flag 3
event: \"MX0 3 0 3 New\"
write back: ok
windows after New: 2
body write: [\"EI8 13 0 5 more\\n\"]
after a killed reader: [\"EI0 1 0 1 x\"]
index at end: \"\"
deleted: deleted window
end
";

/// acme(4) and `win` from inside acme. The test types `test/acmefs` in
/// the row's tag and runs it with button 2, a mode in
/// `/tmp/acme/mode` saying what to check: first acme(4) itself (with a
/// button 2 click on `New` in the window it made, for the events), then,
/// after `win` is run from the right column's tag and given `cat`, an
/// interrupt and `echo hello`, the win window's text, and after `Del` in
/// its tag, the progs left.
fn fs_and_win(vm: &str) {
    let root = Root::new(vm);
    let tmp = root.0.join("tmp/acme");
    let mut w = common::Watched::spawn(
        Command::new(env!("CARGO_BIN_EXE_emu"))
            .arg("-r")
            .arg(&root.0)
            .arg(format!("--vm={vm}"))
            .args(["-s", &format!("{}x{}", SIZE.0, SIZE.1), "-w"])
            .args(["sh", "-c", "acme; echo acme: $status"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped()),
    );
    let out = common::Collected::new(w.child.stdout.take().unwrap());
    let (host, token) = url(&w);
    let s = connect(&host, &token);
    let frames = Frames::start(s.try_clone().unwrap());
    let mut p = Page { s, frames };
    p.frames.settle(0, "acme's first screen");

    const FH: i32 = 19;
    // The end of the row's tag, and the right column: its tag (with
    // `win` 69 to 88 pixels into it in the default font) and the first
    // line of the body of the first window in it.
    let row_end = (SIZE.0 - 6, FH / 2);
    let col_x = SIZE.0 / 2 + 16;
    let win_cmd = (col_x + 75, FH + 1 + FH / 2);
    let body1 = (col_x + 4, 3 * (FH + 1) + FH / 2);

    // acme(4).
    std::fs::write(tmp.join("mode"), "fs").unwrap();
    p.hover(row_end.0, row_end.1);
    p.typed("test/acmefs", "typing test/acmefs");
    p.click(row_end.0, row_end.1, 2, "test/acmefs");
    let start = Instant::now();
    while !tmp.join("ready").exists() {
        if let Ok(o) = std::fs::read_to_string(tmp.join("out")) {
            panic!("{vm}: test/acmefs ended early:\n{o}");
        }
        assert!(
            start.elapsed() < WAIT,
            "{vm}: test/acmefs did not get ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    p.click(body1.0, body1.1, 2, "button 2 on New");
    let got = results(&tmp, "fs", &out);
    assert_eq!(got, FS_WANT, "{vm}: acme(4)");

    // win, from the right column's tag: the shell's prompt comes, `cat`
    // waits for input until Delete interrupts it, and echo answers. Each
    // step waits until the guest sees it done, as the progs take their
    // time on a loaded host.
    let body_is = |want: &'static str| {
        move |got: &str| got.lines().any(|l| l.strip_prefix("body: ") == Some(want))
    };
    p.click(win_cmd.0, win_cmd.1, 2, "win");
    until(
        &mut p,
        &tmp,
        &out,
        row_end,
        "body",
        "win's prompt",
        body_is(r#""; ""#),
    );
    p.hover(SIZE.0 - 100, SIZE.1 - 20);
    p.typed("cat\n", "typing cat");
    until(&mut p, &tmp, &out, row_end, "ps", "cat running", |g| {
        g.contains(" cat")
    });
    p.hover(SIZE.0 - 100, SIZE.1 - 20);
    p.typed("\x7F", "interrupting cat");
    until(
        &mut p,
        &tmp,
        &out,
        row_end,
        "body",
        "the prompt after cat",
        body_is(r#""; cat\n; ""#),
    );
    p.hover(SIZE.0 - 100, SIZE.1 - 20);
    p.typed("echo hello\n", "typing echo hello");
    let got = until(
        &mut p,
        &tmp,
        &out,
        row_end,
        "body",
        "echo's output",
        body_is(r#""; cat\n; echo hello\nhello\n; ""#),
    );
    assert!(got.contains("/-"), "{vm}: win's name\n{got}");

    // Del typed at the end of win's tag and clicked kills the shell and
    // deletes the window. The tag holds the host's name, so where its end
    // is depends on the host: a long name wraps it.
    let tag = got
        .lines()
        .find_map(|l| l.strip_prefix("tag: "))
        .unwrap_or_else(|| panic!("{vm}: no tag for win:\n{got}"))
        .to_string();
    let del = tag_end_at(
        &format!("{tag}Del"),
        col_x,
        SIZE.0 - col_x,
        2 * (FH + 1),
        FH,
    );
    p.hover(del.0, del.1);
    p.typed("Del", "typing Del");
    p.click(del.0, del.1, 2, "Del");
    // win kills the group and acme deletes the window: wait until no win
    // and no shell of its is left.
    until(
        &mut p,
        &tmp,
        &out,
        row_end,
        "ps",
        "the progs after Del",
        |g| {
            let progs = g.lines().next().unwrap_or("");
            !progs.contains("win") && progs.matches(" sh").count() == 2
        },
    );
    std::fs::write(tmp.join("mode"), "body").unwrap();
    p.click(row_end.0, row_end.1, 2, "test/acmefs body");
    assert_eq!(
        results(&tmp, "after Del", &out),
        "end\n",
        "{vm}: the window stayed"
    );

    // acme still works: Exit ends it.
    p.hover(row_end.0, row_end.1);
    p.typed(
        "\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08Exit",
        "typing Exit",
    );
    p.pointer(row_end.0, row_end.1, 2);
    p.pointer(row_end.0, row_end.1, 0);
    let (status, err) = w.finish(WAIT);
    assert!(status.success(), "{vm}: {}", common::failure(status, &err));
    let out = String::from_utf8_lossy(&out.bytes(WAIT)).into_owned();
    assert_eq!(out, "acme: \n", "{vm}: acme failed");
}

#[test]
fn fs_and_win_wasmtime() {
    fs_and_win("wasmtime");
}

#[test]
fn fs_and_win_wasmi() {
    fs_and_win("wasmi");
}
