//! Frames. Everything on an rstyx connection is a frame: one byte of
//! kind, a u32 little-endian length and that many bytes.
//!
//! ```text
//! C -> S  HELLO   "pgrstyx1 <user>"
//! S -> C  CHAL    challenge[32], or empty when no proof is wanted
//! C -> S  PROOF   a mount proof (docs/auth.md) for attach name "rstyx",
//!                 or empty
//! S -> C  OK | ERR <reason>
//! C -> S  EXEC    flags u8, then the command line for `sh -c`
//! then, in both directions and in any order:
//!         STYX    bytes of the 9P conversation in which the client
//!                 serves its namespace and the server mounts it
//!         IN      (C -> S) standard input; an empty one is end of file
//!         OUT ERR (S -> C) standard output and standard error
//!         WIN     stream u8, n u32: n more bytes of that stream may be sent
//! S -> C  STATUS  the command's exit status; the last frame
//! ```
//!
//! IN, OUT and ERR are flow-controlled: a sender starts with [`WINDOW`]
//! bytes of credit for each stream and gets more with WIN, once the
//! receiver has passed the bytes on. A stalled reader of one stream then
//! never holds up the 9P conversation that shares the connection.

/// The protocol name and version, the first word of HELLO.
pub const MAGIC: &str = "pgrstyx1";
/// The attach name the proof is made for, so that a proof for a mount is
/// not a proof for a command.
pub const ANAME: &str = "rstyx";
/// The service a bare host name is dialled at.
pub const SERVICE: &str = "rstyx";
/// The bytes before a frame's data.
pub const HDR: usize = 5;
/// The longest frame data accepted.
pub const MAX: usize = 64 * 1024;
/// The most 9P bytes put in one STYX frame.
pub const STYX_CHUNK: usize = 16 * 1024;
/// The credit a stream starts with. It is half of a pipe's queue, so the
/// bytes a peer may send always fit in the pipe they go to.
pub const WINDOW: usize = 16 * 1024;
/// The most bytes put in one IN, OUT or ERR frame.
pub const CHUNK: usize = 8 * 1024;

pub const EPROTO: &str = "rstyx protocol botch";
pub const EHUNGUP: &str = "connection closed";

pub mod kind {
    pub const HELLO: u8 = 1;
    pub const CHAL: u8 = 2;
    pub const PROOF: u8 = 3;
    pub const OK: u8 = 4;
    pub const ERR: u8 = 5;
    pub const EXEC: u8 = 6;
    pub const STYX: u8 = 7;
    pub const IN: u8 = 8;
    pub const OUT: u8 = 9;
    pub const ERRS: u8 = 10;
    pub const WIN: u8 = 11;
    pub const STATUS: u8 = 12;
}

/// EXEC flag: the client serves its namespace without authentication, so
/// the server mounts it with `MNOAUTH`.
pub const NOAUTH: u8 = 1;

/// One frame, ready to write in a single write.
pub fn frame(kind: u8, data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(HDR + data.len());
    f.push(kind);
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(data);
    f
}

/// A WIN frame.
pub fn window(stream: u8, n: usize) -> Vec<u8> {
    let mut d = vec![stream];
    d.extend_from_slice(&(n as u32).to_le_bytes());
    frame(kind::WIN, &d)
}

/// The stream and count of a WIN frame's data.
pub fn parse_window(d: &[u8]) -> Option<(u8, usize)> {
    let n: [u8; 4] = d.get(1..5)?.try_into().ok()?;
    (d.len() == 5).then(|| (d[0], u32::from_le_bytes(n) as usize))
}

/// Cuts frames out of a byte stream that arrives in pieces.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole frame, `Ok(None)` when more bytes are needed.
    pub fn take(&mut self) -> Result<Option<(u8, Vec<u8>)>, &'static str> {
        if self.buf.len() < HDR {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > MAX {
            return Err(EPROTO);
        }
        if self.buf.len() < HDR + len {
            return Ok(None);
        }
        let kind = self.buf[0];
        let data = self.buf[HDR..HDR + len].to_vec();
        self.buf.drain(..HDR + len);
        Ok(Some((kind, data)))
    }
}

/// Reads exactly one frame, and no byte past it.
pub fn read_frame(r: &mut impl std::io::Read) -> Result<(u8, Vec<u8>), String> {
    let mut hdr = [0u8; HDR];
    r.read_exact(&mut hdr).map_err(|_| String::from(EHUNGUP))?;
    let len = u32::from_le_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    if len > MAX {
        return Err(String::from(EPROTO));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data).map_err(|_| String::from(EHUNGUP))?;
    Ok((hdr[0], data))
}

/// The dial string for `host`: `host` is `tcp!host!rstyx`, `net!host` is
/// `net!host!rstyx`, and a full address is itself.
pub fn mkaddr(host: &str) -> String {
    match host.split('!').count() {
        1 => format!("tcp!{host}!{SERVICE}"),
        2 => format!("{host}!{SERVICE}"),
        _ => String::from(host),
    }
}

/// One word as `sh` reads it back: quoted when it holds anything but
/// plain characters.
pub fn quote(word: &str) -> String {
    let plain = |c: char| c.is_alphanumeric() || "_-+./:,%@!".contains(c);
    if !word.is_empty() && word.chars().all(plain) {
        return String::from(word);
    }
    format!("'{}'", word.replace('\'', "''"))
}

/// The command line for `args`. A single argument is a command line
/// already and goes as it is; several are the words of one command, each
/// quoted so that it arrives as one word.
pub fn command(args: &[String]) -> String {
    match args {
        [] => String::from("sh"),
        [line] => line.clone(),
        words => words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames() {
        let mut wire = frame(kind::OUT, b"hello");
        wire.extend(window(kind::IN, 70000));
        wire.extend(frame(kind::STATUS, b""));
        let mut d = Decoder::default();
        // A byte at a time: a frame appears only when it is whole.
        let mut got = Vec::new();
        for b in &wire {
            d.push(&[*b]);
            while let Some(f) = d.take().unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], (kind::OUT, b"hello".to_vec()));
        assert_eq!(parse_window(&got[1].1), Some((kind::IN, 70000)));
        assert_eq!(got[2], (kind::STATUS, Vec::new()));
        let mut r = &wire[..];
        assert_eq!(read_frame(&mut r).unwrap(), (kind::OUT, b"hello".to_vec()));
        assert_eq!(r.len(), wire.len() - HDR - 5);
    }

    #[test]
    fn bad_frames() {
        let mut d = Decoder::default();
        d.push(&[kind::OUT, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(d.take(), Err(EPROTO));
        let mut short = &[kind::OUT, 4, 0, 0, 0, 1][..];
        assert_eq!(read_frame(&mut short).unwrap_err(), EHUNGUP);
        assert_eq!(parse_window(&[1, 2, 3]), None);
    }

    #[test]
    fn addresses_and_commands() {
        assert_eq!(mkaddr("box"), "tcp!box!rstyx");
        assert_eq!(mkaddr("net!box"), "net!box!rstyx");
        assert_eq!(mkaddr("tcp!box!99"), "tcp!box!99");
        let v = |s: &[&str]| s.iter().map(|s| String::from(*s)).collect::<Vec<_>>();
        assert_eq!(command(&[]), "sh");
        assert_eq!(command(&v(&["ls | wc"])), "ls | wc");
        assert_eq!(
            command(&v(&["echo", "a b", "it's", "", "x/y.z"])),
            "echo 'a b' 'it''s' '' x/y.z"
        );
    }
}
