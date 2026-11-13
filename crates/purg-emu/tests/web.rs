//! `emu -w`: the screen served to a browser (docs/graphics.md,
//! Section 8.2), with this test as the browser. It connects a WebSocket,
//! checks the first frame against the screen of an emu without a window,
//! sends pointer events and reads them back from `/dev/pointer`, and
//! checks that a request without the token, or from another origin, is
//! refused. Requires `cargo xtask build-root` first.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use purg_emu::web::{OP_BINARY, OP_CLOSE, OP_TEXT, accept_key, read_frame, write_frame};

mod common;

/// The screen's size in both emus.
const SIZE: (usize, usize) = (800, 600);
/// How long to wait for the emu to start, draw or answer.
const WAIT: Duration = Duration::from_secs(120);

fn root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root");
    assert!(
        root.join("bin/draw/demo.wasm").exists() && root.join("bin/test/draw.wasm").exists(),
        "guest binaries missing; run `cargo xtask build-root` first"
    );
    root
}

fn emu(vm: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_emu"));
    c.arg("-r")
        .arg(root())
        .arg(format!("--vm={vm}"))
        .arg("-s")
        .arg(format!("{}x{}", SIZE.0, SIZE.1));
    c
}

/// The URL `emu -w` prints on its standard error.
fn url(w: &common::Watched) -> String {
    let start = Instant::now();
    loop {
        let err = String::from_utf8_lossy(&w.stderr_so_far()).into_owned();
        if let Some(i) = err.find("emu: screen at ") {
            let rest = &err[i + "emu: screen at ".len()..];
            if let Some(end) = rest.find('\n') {
                return rest[..end].to_string();
            }
        }
        assert!(start.elapsed() < WAIT, "no URL from emu -w:\n{err}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `host:port` and the token of a URL `http://host:port/?t=token`.
fn split(url: &str) -> (String, String) {
    let rest = url.strip_prefix("http://").unwrap();
    let (host, query) = rest.split_once("/?t=").unwrap();
    (host.to_string(), query.to_string())
}

/// Sends a GET and returns the status line and what follows the head
/// (all of it for a plain reply; nothing yet for an upgrade).
fn get(host: &str, target: &str, extra: &str) -> (String, TcpStream) {
    let mut s = TcpStream::connect(host).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    write!(s, "GET {target} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n").unwrap();
    let mut head = Vec::new();
    let mut b = [0u8];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(s.read(&mut b).unwrap(), 1, "short reply: {head:?}");
        head.push(b[0]);
    }
    (String::from_utf8(head).unwrap(), s)
}

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

fn upgrade_headers(origin: Option<&str>) -> String {
    let mut h = format!(
        "Upgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {KEY}\r\n"
    );
    if let Some(o) = origin {
        h.push_str(&format!("Origin: {o}\r\n"));
    }
    h
}

/// Opens a WebSocket as the page does.
fn connect(host: &str, token: &str) -> TcpStream {
    let origin = format!("http://{host}");
    let (head, s) = get(
        host,
        &format!("/ws?t={token}"),
        &upgrade_headers(Some(&origin)),
    );
    assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
    assert!(
        head.contains(&format!("Sec-WebSocket-Accept: {}\r\n", accept_key(KEY))),
        "{head}"
    );
    s
}

/// The next text message.
fn text(s: &mut TcpStream) -> String {
    loop {
        let f = read_frame(s, 1 << 26).unwrap();
        if f.opcode == OP_TEXT {
            return String::from_utf8(f.payload).unwrap();
        }
    }
}

/// A pointer message.
fn pointer(x: i32, y: i32, buttons: u32) -> Vec<u8> {
    let mut m = vec![b'm'];
    m.extend_from_slice(&x.to_le_bytes());
    m.extend_from_slice(&y.to_le_bytes());
    m.extend_from_slice(&buttons.to_le_bytes());
    m
}

/// The screen of an emu without a window after `draw/demo -n`, as RGBA.
fn headless(vm: &str) -> Vec<u8> {
    let o = common::output(
        emu(vm).args(["sh", "-c", "draw/demo -n; test/draw -d"]),
        b"",
    );
    assert!(
        o.status.success(),
        "{}",
        common::failure(o.status, &o.stderr)
    );
    let nl = o.stdout.iter().position(|&b| b == b'\n').unwrap();
    let head = String::from_utf8_lossy(&o.stdout[..nl]);
    assert_eq!(head, format!("{} {}", SIZE.0, SIZE.1));
    let px = &o.stdout[nl + 1..];
    assert_eq!(px.len(), SIZE.0 * SIZE.1 * 4);
    px.chunks(4)
        .flat_map(|p| [p[2], p[1], p[0], 0xFF])
        .collect()
}

fn screen_and_pointer(vm: &str) {
    let want = headless(vm);
    let mut w = common::Watched::spawn(
        emu(vm)
            .args([
                "-w",
                "sh",
                "-c",
                "draw/demo -n; echo drawn; read 49 < /dev/pointer; echo; read 4 < /dev/keyboard",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped()),
    );
    let out = common::Collected::new(w.child.stdout.take().unwrap());
    let (host, token) = split(&url(&w));
    let start = Instant::now();
    while !out.bytes(Duration::ZERO).starts_with(b"drawn\n") {
        assert!(start.elapsed() < WAIT, "the demo did not draw");
        std::thread::sleep(Duration::from_millis(10));
    }

    // The first page gets the input and the whole screen.
    let mut a = connect(&host, &token);
    assert_eq!(
        text(&mut a),
        format!(
            "{{\"width\":{},\"height\":{},\"input\":true}}",
            SIZE.0, SIZE.1
        )
    );
    let f = read_frame(&mut a, 1 << 26).unwrap();
    assert_eq!(f.opcode, OP_BINARY);
    let word = |i: usize| u32::from_le_bytes(f.payload[i..i + 4].try_into().unwrap()) as usize;
    assert_eq!(f.payload[0], 1);
    assert_eq!(
        (word(1), word(5), word(9), word(13)),
        (0, 0, SIZE.0, SIZE.1)
    );
    let got = miniz_oxide::inflate::decompress_to_vec_zlib(&f.payload[17..]).unwrap();
    eprintln!(
        "{vm}: first frame {} bytes for {} bytes of RGBA",
        f.payload.len(),
        got.len()
    );
    assert!(
        got == want,
        "{vm}: the browser's screen differs from the headless one"
    );

    // A second page watches; its input is ignored.
    let mut b = connect(&host, &token);
    assert_eq!(
        text(&mut b),
        "{\"width\":800,\"height\":600,\"input\":false}"
    );
    write_frame(&mut b, OP_BINARY, &pointer(1, 2, 4), Some([9, 8, 7, 6])).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    write_frame(&mut a, OP_BINARY, &pointer(10, 20, 1), Some([1, 2, 3, 4])).unwrap();
    // Keys from the watcher are dropped too; the arrow is keyboard(6)'s.
    write_frame(&mut b, OP_BINARY, b"k\0x", Some([5, 5, 5, 5])).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    write_frame(&mut a, OP_BINARY, b"k\0ArrowUp", Some([4, 3, 2, 1])).unwrap();

    let (status, err) = w.finish(WAIT);
    let out = out.bytes(WAIT);
    let text_out = String::from_utf8_lossy(&out);
    assert!(status.success(), "{vm}: {}", common::failure(status, &err));
    let fields: Vec<&str> = text_out["drawn\n".len()..].split_whitespace().collect();
    assert_eq!(&fields[..4], ["m", "10", "20", "1"], "{vm}: {text_out:?}");
    assert!(text_out.ends_with("\n\u{F00E}"), "{vm}: {text_out:?}");

    // The emu's exit closes both pages.
    for s in [&mut a, &mut b] {
        let end = loop {
            match read_frame(s, 1 << 26) {
                Ok(f) if f.opcode == OP_CLOSE => break true,
                Ok(_) => continue,
                Err(_) => break false,
            }
        };
        assert!(end, "{vm}: no close frame");
    }
}

#[test]
fn screen_and_pointer_wasmtime() {
    screen_and_pointer("wasmtime");
}

#[test]
fn screen_and_pointer_wasmi() {
    screen_and_pointer("wasmi");
}

/// Without the token, or from another page, the emu refuses.
#[test]
fn token_and_origin_required() {
    let w = common::Watched::spawn(
        emu("wasmtime")
            .args(["-w", "sleep", "600"])
            .stdin(Stdio::null())
            .stdout(Stdio::null()),
    );
    let u = url(&w);
    let (host, token) = split(&u);
    assert!(host.starts_with("127.0.0.1:"), "{u}");
    assert_eq!(token.len(), 32, "{u}");
    let bad = "0".repeat(32);
    for (target, origin) in [
        (String::from("/ws"), Some(format!("http://{host}"))),
        (format!("/ws?t={bad}"), Some(format!("http://{host}"))),
        (
            format!("/ws?t={token}"),
            Some(String::from("http://evil.example")),
        ),
        (format!("/ws?t={token}"), Some(format!("https://{host}"))),
    ] {
        let (head, _) = get(&host, &target, &upgrade_headers(origin.as_deref()));
        assert!(
            head.starts_with("HTTP/1.1 403 "),
            "{target} {origin:?}: {head}"
        );
    }
    let (head, _) = get(&host, "/", "");
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    // With the token, the page, then its socket.
    let (head, mut s) = get(&host, &format!("/?t={token}"), "");
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    let mut page = String::new();
    s.read_to_string(&mut page).unwrap();
    assert!(page.contains("<canvas"), "{page}");
    let mut c = connect(&host, &token);
    assert!(text(&mut c).contains("\"input\":true"));
}
