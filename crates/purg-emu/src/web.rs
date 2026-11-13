//! The emu's screen in a web browser (`emu -w`): a [`Screen`] served
//! over HTTP and a WebSocket to a page with a `<canvas>`.
//!
//! The emu listens on one TCP address (loopback unless the user names
//! another) and prints a URL with a random token. `GET /?t=<token>`
//! returns the page (one HTML file with its script inline);
//! `GET /ws?t=<token>` upgrades to a WebSocket. A request without the
//! token gets `403 Forbidden`, and a WebSocket request whose `Origin` is
//! not the page's own is refused the same way, so other local users and
//! other web pages cannot watch the screen or type into it.
//!
//! **Server to page.** A text message `{"width":W,"height":H,"input":B}`
//! when the page connects, and `{"input":B}` when it gains or loses the
//! input (below). Each damaged rectangle is a binary message: the byte
//! 1, then x, y, width and height as little-endian `u32`, then the
//! rectangle's pixels as RGBA rows, compressed with zlib (RFC 1950); the
//! page inflates them with `DecompressionStream` and puts them with
//! `putImageData`. A new page first gets the whole screen.
//!
//! **Page to server.** Binary messages: `m` with x, y (`i32`) and the
//! buttons (`u32`, pointer(3)'s bits), and `k` with a byte of modifiers
//! (bit 0: control) and the key's name as the DOM's `KeyboardEvent.key`
//! gives it, which becomes a character or a keyboard(6) code.
//!
//! **Several pages.** Any number may watch. Only one sends input: the
//! page that has been connected longest; when it goes, the next oldest
//! takes over. The others' pointer and key messages are dropped.
//!
//! **Threads.** One thread accepts connections; each connection has a
//! reader (the HTTP request, then the page's messages) and a writer. A
//! flush only copies the damaged pixels into the emu's copy of the screen
//! and adds the rectangle to each page's pending damage, so a slow page
//! never holds up a prog: its writer takes the union of everything
//! damaged since it last sent and sends that, read from the screen as it
//! is then. [`WebScreen::shutdown`] sends every page a close frame when
//! the emu exits.
//!
//! The WebSocket (RFC 6455) and the little HTTP it needs are written
//! here: one page, one upgrade, unfragmented frames out, and fragments
//! assembled on the way in. SHA-1 for the handshake comes from `ring`
//! and zlib from `miniz_oxide`, both already in the emu's graph.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use purg_platform::screen::{Input, Pointer, Rect, Screen};

use crate::host;

/// The page: HTML, CSS and script in one file, nothing fetched from
/// anywhere else.
const PAGE: &str = include_str!("web.html");

/// The most bytes of an HTTP request head the server reads.
const MAX_HEAD: usize = 8192;
/// The longest message a page may send. Its messages are a few bytes.
const MAX_MESSAGE: usize = 4096;
/// How long a new connection may take to send its request.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// zlib's level for the pixels. miniz_oxide's level 1 gives twice the
/// bytes of level 2 on the demo's screen for half the time; above 2 the
/// gain is small (docs/graphics.md, Section 8.2).
const LEVEL: u8 = 2;
/// How long [`WebScreen::shutdown`] waits for the pages' writers.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

/// The address `-w` listens on when none is given: loopback, any port.
pub const DEFAULT_ADDR: &str = "127.0.0.1:0";

/// The magic string of RFC 6455's handshake.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub struct WebScreen {
    width: u32,
    height: u32,
    /// The screen as last flushed, `0x00RRGGBB` per pixel.
    frame: Mutex<Vec<u32>>,
    clients: Mutex<Vec<Arc<Client>>>,
    /// Signalled when a client goes, for [`WebScreen::shutdown`].
    gone: Condvar,
    input: Input,
    token: String,
    start: Instant,
    next_id: AtomicU64,
}

/// One connected page.
struct Client {
    id: u64,
    stream: TcpStream,
    state: Mutex<Pending>,
    wake: Condvar,
}

/// What a page's writer has still to send.
#[derive(Default)]
struct Pending {
    damage: Option<Rect>,
    texts: VecDeque<String>,
    pong: Option<Vec<u8>>,
    closing: bool,
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

/// A random token of 128 bits, in hex.
fn new_token() -> String {
    let mut b = [0u8; 16];
    host::entropy(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl WebScreen {
    /// Listens on `addr` and starts accepting pages. Returns the screen
    /// and the URL to open.
    pub fn listen(addr: &str, width: u32, height: u32) -> io::Result<(Arc<WebScreen>, String)> {
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?;
        let screen = Arc::new(WebScreen {
            width,
            height,
            frame: Mutex::new(vec![0x00FF_FFFF; width as usize * height as usize]),
            clients: Mutex::new(Vec::new()),
            gone: Condvar::new(),
            input: Input::new(),
            token: new_token(),
            start: Instant::now(),
            next_id: AtomicU64::new(0),
        });
        let url = format!("http://{}/?t={}", url_host(local), screen.token);
        let s = screen.clone();
        std::thread::Builder::new()
            .name(String::from("web accept"))
            .spawn(move || s.accept(listener))?;
        Ok((screen, url))
    }

    fn accept(self: Arc<WebScreen>, listener: TcpListener) {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let s = self.clone();
            let _ = std::thread::Builder::new()
                .name(String::from("web conn"))
                .spawn(move || s.serve(stream));
        }
    }

    /// Sends every page a close frame and waits a little for them to go.
    pub fn shutdown(&self) {
        let deadline = Instant::now() + SHUTDOWN_WAIT;
        let mut clients = self.clients.lock().unwrap();
        for c in clients.iter() {
            c.state.lock().unwrap().closing = true;
            c.wake.notify_one();
        }
        while !clients.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            clients = self.gone.wait_timeout(clients, deadline - now).unwrap().0;
        }
    }

    /// The id of the page that sends input: the oldest.
    fn controller(&self) -> Option<u64> {
        self.clients.lock().unwrap().first().map(|c| c.id)
    }

    fn token_ok(&self, query: &str) -> bool {
        let given = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("t="))
            .unwrap_or("");
        // Compared without stopping at the first difference.
        given.len() == self.token.len()
            && given
                .bytes()
                .zip(self.token.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }

    /// Serves one connection: the page, a WebSocket, or an error.
    fn serve(self: Arc<WebScreen>, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(HEAD_TIMEOUT));
        let _ = stream.set_nodelay(true);
        let Ok(req) = read_head(&mut stream) else {
            return;
        };
        let reply = |stream: &mut TcpStream, status: &str| {
            let body = format!("{status}\n");
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        };
        if req.method != "GET" {
            return reply(&mut stream, "405 Method Not Allowed");
        }
        let (path, query) = req.target.split_once('?').unwrap_or((&req.target, ""));
        if path != "/" && path != "/ws" {
            return reply(&mut stream, "404 Not Found");
        }
        if !self.token_ok(query) {
            return reply(&mut stream, "403 Forbidden");
        }
        if path == "/" {
            // The page may connect only to its own server; older browsers
            // need the ws: origin spelled out. A Host with anything but
            // the characters of a host and port is left out.
            let host = req.header("host").unwrap_or("");
            let ws = if host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
            {
                format!(" ws://{host}")
            } else {
                String::new()
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'{ws}; frame-ancestors 'none'\r\nConnection: close\r\n\r\n{PAGE}",
                PAGE.len()
            );
            return;
        }
        // A browser always sends Origin with a WebSocket request; it must
        // be the page's own. A program that sends none needs the token
        // all the same.
        if let Some(origin) = req.header("origin") {
            let host = req.header("host").unwrap_or("");
            if origin != format!("http://{host}") {
                return reply(&mut stream, "403 Forbidden");
            }
        }
        let upgrade = req
            .header("upgrade")
            .is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
        let connection = req.header("connection").is_some_and(|c| {
            c.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
        });
        let (true, true, Some("13"), Some(key)) = (
            upgrade,
            connection,
            req.header("sec-websocket-version"),
            req.header("sec-websocket-key"),
        ) else {
            return reply(&mut stream, "400 Bad Request");
        };
        if write!(
            stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            accept_key(key)
        )
        .is_err()
        {
            return;
        }
        let _ = stream.set_read_timeout(None);
        self.session(stream);
    }

    /// Runs a WebSocket session: registers the page, starts its writer,
    /// and reads its messages until it goes.
    fn session(self: &Arc<WebScreen>, stream: TcpStream) {
        let Ok(out) = stream.try_clone() else { return };
        let client = Arc::new(Client {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            stream,
            state: Mutex::new(Pending {
                damage: Some([0, 0, self.width as i32, self.height as i32]),
                ..Pending::default()
            }),
            wake: Condvar::new(),
        });
        {
            let mut clients = self.clients.lock().unwrap();
            clients.push(client.clone());
            let input = clients[0].id == client.id;
            client.state.lock().unwrap().texts.push_back(format!(
                "{{\"width\":{},\"height\":{},\"input\":{input}}}",
                self.width, self.height
            ));
        }
        let s = self.clone();
        let c = client.clone();
        let writer = std::thread::Builder::new()
            .name(String::from("web write"))
            .spawn(move || s.write_loop(&c, out));
        if writer.is_ok() {
            self.read_loop(&client);
        }
        self.remove(&client);
    }

    /// Takes the page out of the list, ends its writer, and tells the
    /// page that now sends input, if that changed.
    fn remove(&self, client: &Client) {
        let mut clients = self.clients.lock().unwrap();
        let Some(i) = clients.iter().position(|c| c.id == client.id) else {
            return;
        };
        clients.remove(i);
        if i == 0
            && let Some(next) = clients.first()
        {
            next.state
                .lock()
                .unwrap()
                .texts
                .push_back(String::from("{\"input\":true}"));
            next.wake.notify_one();
        }
        drop(clients);
        client.state.lock().unwrap().closing = true;
        client.wake.notify_one();
        let _ = client.stream.shutdown(Shutdown::Read);
        self.gone.notify_all();
    }

    fn read_loop(&self, client: &Client) {
        let mut r = &client.stream;
        let mut message = Vec::new();
        loop {
            let Ok(f) = read_frame(&mut r, MAX_MESSAGE) else {
                return;
            };
            match f.opcode {
                OP_CLOSE => return,
                OP_PING => {
                    client.state.lock().unwrap().pong = Some(f.payload);
                    client.wake.notify_one();
                }
                OP_PONG => {}
                OP_TEXT | OP_BINARY | OP_CONTINUATION => {
                    if f.opcode != OP_CONTINUATION {
                        message.clear();
                    }
                    if message.len() + f.payload.len() > MAX_MESSAGE {
                        return;
                    }
                    message.extend_from_slice(&f.payload);
                    if f.fin {
                        if self.controller() == Some(client.id) {
                            self.event(&message);
                        }
                        message.clear();
                    }
                }
                _ => return,
            }
        }
    }

    /// Turns a page's message into input.
    fn event(&self, m: &[u8]) {
        let msec = self.start.elapsed().as_millis() as u32;
        match m {
            [b'm', rest @ ..] if rest.len() == 12 => {
                let word = |i: usize| [rest[i], rest[i + 1], rest[i + 2], rest[i + 3]];
                let x = i32::from_le_bytes(word(0));
                let y = i32::from_le_bytes(word(4));
                let buttons = u32::from_le_bytes(word(8)) & 0x1F;
                let x = x.clamp(0, self.width as i32 - 1);
                let y = y.clamp(0, self.height as i32 - 1);
                self.input.pointer(Pointer {
                    x,
                    y,
                    buttons,
                    msec,
                });
            }
            [b'k', mods, name @ ..] => {
                if let Ok(name) = std::str::from_utf8(name)
                    && let Some(c) = key(name, mods & 1 != 0)
                {
                    self.input.key(c);
                }
            }
            _ => {}
        }
    }

    fn write_loop(&self, client: &Client, mut out: TcpStream) {
        loop {
            let mut st = client.state.lock().unwrap();
            while st.damage.is_none() && st.texts.is_empty() && st.pong.is_none() && !st.closing {
                st = client.wake.wait(st).unwrap();
            }
            let texts = std::mem::take(&mut st.texts);
            let pong = st.pong.take();
            let damage = st.damage.take();
            let closing = st.closing;
            drop(st);
            if closing {
                // What the emu drew last is shown before the page is
                // told it has gone (code 1001, "going away").
                if let Some(r) = damage {
                    let _ = write_frame(&mut out, OP_BINARY, &self.encode(r), None);
                }
                let _ = write_frame(&mut out, OP_CLOSE, &[0x03, 0xE9], None);
                let _ = out.shutdown(Shutdown::Write);
                return;
            }
            let mut ok = true;
            if let Some(p) = pong {
                ok &= write_frame(&mut out, OP_PONG, &p, None).is_ok();
            }
            for t in texts {
                ok &= write_frame(&mut out, OP_TEXT, t.as_bytes(), None).is_ok();
            }
            if let Some(r) = damage {
                ok &= write_frame(&mut out, OP_BINARY, &self.encode(r), None).is_ok();
            }
            if !ok {
                let _ = out.shutdown(Shutdown::Both);
                return;
            }
        }
    }

    /// The message for rectangle `r`: its place and its zlib-compressed
    /// RGBA pixels.
    pub fn encode(&self, r: Rect) -> Vec<u8> {
        let w = self.width as usize;
        let (dx, dy) = ((r[2] - r[0]) as usize, (r[3] - r[1]) as usize);
        let mut rgba = Vec::with_capacity(dx * dy * 4);
        {
            let f = self.frame.lock().unwrap();
            for y in r[1] as usize..r[3] as usize {
                for &p in &f[y * w + r[0] as usize..][..dx] {
                    let [b, g, r, _] = p.to_le_bytes();
                    rgba.extend_from_slice(&[r, g, b, 0xFF]);
                }
            }
        }
        let mut m = Vec::with_capacity(17 + rgba.len() / 8);
        m.push(1);
        for v in [r[0], r[1], dx as i32, dy as i32] {
            m.extend_from_slice(&(v as u32).to_le_bytes());
        }
        m.extend_from_slice(&miniz_oxide::deflate::compress_to_vec_zlib(&rgba, LEVEL));
        m
    }
}

impl Screen for WebScreen {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn flush(&self, r: Rect, pixels: &[u8], stride: usize) {
        let r = [
            r[0].max(0),
            r[1].max(0),
            r[2].min(self.width as i32),
            r[3].min(self.height as i32),
        ];
        if r[0] >= r[2] || r[1] >= r[3] {
            return;
        }
        let w = self.width as usize;
        {
            let mut f = self.frame.lock().unwrap();
            for (i, y) in (r[1]..r[3]).enumerate() {
                let row = &pixels[i * stride..];
                let out = &mut f[y as usize * w + r[0] as usize..][..(r[2] - r[0]) as usize];
                for (o, px) in out.iter_mut().zip(row.as_chunks::<4>().0) {
                    *o = u32::from_le_bytes(*px) & 0x00FF_FFFF;
                }
            }
        }
        for c in self.clients.lock().unwrap().iter() {
            let mut st = c.state.lock().unwrap();
            st.damage = Some(union(st.damage, r));
            drop(st);
            c.wake.notify_one();
        }
    }

    fn input(&self) -> &Input {
        &self.input
    }
}

/// The host part of a URL for `addr`. An unspecified address is shown as
/// loopback, which reaches it from this machine.
fn url_host(addr: SocketAddr) -> String {
    match addr {
        SocketAddr::V4(a) if a.ip().is_unspecified() => format!("127.0.0.1:{}", a.port()),
        SocketAddr::V6(a) if a.ip().is_unspecified() => format!("[::1]:{}", a.port()),
        a => a.to_string(),
    }
}

/// A character for a DOM key name, with control held or not: a
/// character key gives itself (or its control code), a named key its
/// keyboard(6) code, and a modifier key nothing.
pub fn key(name: &str, control: bool) -> Option<char> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if control {
            return (c.is_ascii_alphabetic() || "@[\\]^_".contains(c))
                .then(|| char::from(c.to_ascii_uppercase() as u8 & 0x1F));
        }
        return Some(c);
    }
    const KF: u32 = 0xF000;
    let code = match name {
        "Enter" => u32::from(b'\n'),
        "Tab" => u32::from(b'\t'),
        "Backspace" => 0x08,
        "Escape" => 0x1B,
        "Delete" => 0x7F,
        "Home" => KF | 0x0D,
        "ArrowUp" => KF | 0x0E,
        "PageUp" => KF | 0x0F,
        "PrintScreen" => KF | 0x10,
        "ArrowLeft" => KF | 0x11,
        "ArrowRight" => KF | 0x12,
        "PageDown" => KF | 0x13,
        "Insert" => KF | 0x14,
        "End" => KF | 0x18,
        "ArrowDown" => 0xF800,
        _ => {
            let n: u32 = name.strip_prefix('F')?.parse().ok()?;
            if !(1..=12).contains(&n) {
                return None;
            }
            KF | n
        }
    };
    char::from_u32(code)
}

/// The `Sec-WebSocket-Accept` value for a `Sec-WebSocket-Key`.
pub fn accept_key(key: &str) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(key.trim().as_bytes());
    ctx.update(WS_GUID.as_bytes());
    base64(ctx.finish().as_ref())
}

/// Standard base64 with padding (RFC 4648).
fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = u32::from(c[0]) << 16
            | u32::from(*c.get(1).unwrap_or(&0)) << 8
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                s.push(char::from(A[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// An HTTP request's line and headers.
struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Reads a request head, byte by byte so that nothing after it is
/// consumed.
fn read_head(r: &mut impl Read) -> io::Result<Head> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "bad request");
    let mut buf = Vec::new();
    let mut b = [0u8];
    while !buf.ends_with(b"\r\n\r\n") {
        if buf.len() == MAX_HEAD || r.read(&mut b)? == 0 {
            return Err(bad());
        }
        buf.push(b[0]);
    }
    let text = std::str::from_utf8(&buf).map_err(|_| bad())?;
    let mut lines = text.split("\r\n");
    let mut line = lines.next().ok_or_else(bad)?.split(' ');
    let (Some(method), Some(target), Some(version)) = (line.next(), line.next(), line.next())
    else {
        return Err(bad());
    };
    if !version.starts_with("HTTP/1.") {
        return Err(bad());
    }
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok(Head {
        method: method.to_string(),
        target: target.to_string(),
        headers,
    })
}

pub const OP_CONTINUATION: u8 = 0;
pub const OP_TEXT: u8 = 1;
pub const OP_BINARY: u8 = 2;
pub const OP_CLOSE: u8 = 8;
pub const OP_PING: u8 = 9;
pub const OP_PONG: u8 = 10;

/// One WebSocket frame, unmasked.
pub struct Frame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// Reads a frame of at most `max` payload bytes. Masked frames (a
/// client's) are unmasked; unmasked ones (a server's) are taken as they
/// are.
pub fn read_frame(r: &mut impl Read, max: usize) -> io::Result<Frame> {
    let mut h = [0u8; 2];
    r.read_exact(&mut h)?;
    let fin = h[0] & 0x80 != 0;
    let opcode = h[0] & 0x0F;
    let masked = h[1] & 0x80 != 0;
    let len = match h[1] & 0x7F {
        126 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            u64::from(u16::from_be_bytes(b))
        }
        127 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            u64::from_be_bytes(b)
        }
        n => u64::from(n),
    };
    if len > max as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too long"));
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok(Frame {
        fin,
        opcode,
        payload,
    })
}

/// Writes one final frame; a client passes the mask RFC 6455 requires of
/// it, a server none.
pub fn write_frame(
    w: &mut impl Write,
    opcode: u8,
    payload: &[u8],
    mask: Option<[u8; 4]>,
) -> io::Result<()> {
    let mut m = Vec::with_capacity(payload.len() + 14);
    m.push(0x80 | opcode);
    let bit = if mask.is_some() { 0x80 } else { 0 };
    match payload.len() {
        n if n < 126 => m.push(bit | n as u8),
        n if n <= 0xFFFF => {
            m.push(bit | 126);
            m.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            m.push(bit | 127);
            m.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    match mask {
        Some(k) => {
            m.extend_from_slice(&k);
            m.extend(payload.iter().enumerate().map(|(i, b)| b ^ k[i % 4]));
        }
        None => m.extend_from_slice(payload),
    }
    w.write_all(&m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_rfc6455() {
        // The example in RFC 6455, Section 1.3.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn frames_round_trip() {
        for len in [0, 5, 125, 126, 65535, 65536] {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            for mask in [None, Some([1, 2, 3, 4])] {
                let mut wire = Vec::new();
                write_frame(&mut wire, OP_BINARY, &data, mask).unwrap();
                let f = read_frame(&mut wire.as_slice(), 1 << 20).unwrap();
                assert!(f.fin);
                assert_eq!(f.opcode, OP_BINARY);
                assert_eq!(f.payload, data);
            }
        }
        let mut wire = Vec::new();
        write_frame(&mut wire, OP_BINARY, &[0; 200], None).unwrap();
        assert!(read_frame(&mut wire.as_slice(), 100).is_err());
    }

    #[test]
    fn keys() {
        assert_eq!(key("a", false), Some('a'));
        assert_eq!(key("é", false), Some('é'));
        assert_eq!(key("a", true), Some('\x01'));
        assert_eq!(key("Enter", false), Some('\n'));
        assert_eq!(key("ArrowDown", false), Some('\u{F800}'));
        assert_eq!(key("ArrowUp", false), Some('\u{F00E}'));
        assert_eq!(key("F12", false), Some('\u{F00C}'));
        assert_eq!(key("F13", false), None);
        assert_eq!(key("Shift", false), None);
    }

    #[test]
    fn heads() {
        let req = b"GET /ws?t=x HTTP/1.1\r\nHost: a:1\r\nUpgrade: websocket\r\n\r\nrest";
        let mut r = &req[..];
        let h = read_head(&mut r).unwrap();
        assert_eq!(h.method, "GET");
        assert_eq!(h.target, "/ws?t=x");
        assert_eq!(h.header("UPGRADE"), Some("websocket"));
        assert_eq!(r, b"rest");
    }
}
