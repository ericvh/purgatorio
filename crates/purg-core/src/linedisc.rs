//! The console line discipline: echo and line editing for a console that
//! is a bare byte stream (a UART), as Inferno's cons does when no terminal
//! is in front of it.
//!
//! A [`Console`] opts in with [`Console::line_discipline`]. The kernel's
//! and the probe's PL011 do. The hosted emu's does not: the host terminal
//! already echoes and edits, and a second echo would double every
//! character.
//!
//! In cooked mode (the default) each typed character is echoed and
//! collected into a line, and a read returns at most one line:
//!
//! - Enter ends the line. CR and CR LF both read as LF, and LF is echoed.
//! - Backspace (^H) and DEL erase the last character, a whole UTF-8
//!   sequence, and echo `\b \b`. Terminals send DEL for the Backspace
//!   key, so DEL erases; it is not an interrupt key.
//! - ^U kills the line, echoing `\b \b` for each character.
//! - ^D ends the input: at the start of a line a read returns 0; after
//!   some text the read returns that text with no newline. ^D is not
//!   echoed.
//! - Every other byte, ^C included, is ordinary: it is echoed and
//!   delivered. There is no interrupt key.
//!
//! In raw mode (`rawon` written to `consctl`) nothing is echoed or edited
//! and bytes are delivered as they arrive, CR as CR. Switching to raw mode
//! delivers a line still being typed as it stands.
//!
//! The discipline runs in the reader: a byte is echoed when a read of
//! `cons` takes it from the console, so what is typed while nothing reads
//! is echoed at the next read. The echo of each batch of input is one
//! [`Console::write`], so it does not mix with another writer's buffer.
//!
//! Erasing assumes each character took one column. A tab or a wide
//! character leaves the cursor short of where the text began.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use purg_platform::Console;
use purg_platform::sync::Mutex;

use crate::error::{Error, Result};

const BS: u8 = 0x08;
const DEL: u8 = 0x7f;
const KILL: u8 = 0x15;
const EOF: u8 = 0x04;

/// The longest line, in bytes. What is typed past it is dropped, and not
/// echoed.
pub const MAX_LINE: usize = 4096;

/// What erases one character on the terminal.
const ERASE: &[u8] = b"\x08 \x08";

/// The state machine: bytes in, echo and readable chunks out. It does no
/// I/O, so it is tested on its own.
#[derive(Default)]
pub struct LineDisc {
    /// The line being typed.
    line: Vec<u8>,
    /// What reads return, one chunk each at most. An empty chunk is an
    /// end of input.
    ready: VecDeque<Vec<u8>>,
    /// The last cooked byte was CR, so a following LF is dropped.
    after_cr: bool,
    /// The last chunk of `ready` is raw bytes, and more may join it.
    raw_tail: bool,
}

impl LineDisc {
    pub fn new() -> LineDisc {
        LineDisc::default()
    }

    /// Takes one byte from the console, in raw or cooked mode, and
    /// appends what the terminal should show for it to `echo`.
    pub fn input(&mut self, b: u8, raw: bool, echo: &mut Vec<u8>) {
        if raw {
            self.after_cr = false;
            self.flush_line();
            if !self.raw_tail {
                self.ready.push_back(Vec::new());
                self.raw_tail = true;
            }
            if let Some(tail) = self.ready.back_mut() {
                tail.push(b);
            }
            return;
        }
        self.raw_tail = false;
        let after_cr = core::mem::replace(&mut self.after_cr, b == b'\r');
        match b {
            b'\n' if after_cr => {}
            b'\r' | b'\n' => {
                self.line.push(b'\n');
                echo.push(b'\n');
                self.flush_line();
            }
            BS | DEL => {
                if self.erase() {
                    echo.extend_from_slice(ERASE);
                }
            }
            KILL => {
                while self.erase() {
                    echo.extend_from_slice(ERASE);
                }
            }
            EOF => {
                if self.line.is_empty() {
                    self.ready.push_back(Vec::new());
                } else {
                    self.flush_line();
                }
            }
            // One byte is kept for the newline.
            _ if self.line.len() + 1 < MAX_LINE => {
                self.line.push(b);
                echo.push(b);
            }
            _ => {}
        }
    }

    /// The console itself reported end of input: what was typed is
    /// delivered, then a read returns 0.
    pub fn end(&mut self) {
        self.flush_line();
        self.raw_tail = false;
        self.ready.push_back(Vec::new());
    }

    /// Fills `buf` from the next chunk. `None` means nothing is ready and
    /// more input is needed; `Some(0)` is an end of input.
    pub fn take(&mut self, buf: &mut [u8]) -> Option<usize> {
        let chunk = self.ready.front_mut()?;
        let n = buf.len().min(chunk.len());
        buf[..n].copy_from_slice(&chunk[..n]);
        chunk.drain(..n);
        // An end of input is taken whole; a chunk partly read keeps its
        // rest for the next read.
        if chunk.is_empty() {
            self.ready.pop_front();
            if self.ready.is_empty() {
                self.raw_tail = false;
            }
        }
        Some(n)
    }

    /// Makes the line being typed, if any, readable.
    fn flush_line(&mut self) {
        if !self.line.is_empty() {
            self.ready.push_back(core::mem::take(&mut self.line));
            self.raw_tail = false;
        }
    }

    /// Removes the last character of the line: a byte and the UTF-8
    /// continuation bytes after it. Returns false if the line was empty.
    fn erase(&mut self) -> bool {
        let mut removed = false;
        for _ in 0..4 {
            match self.line.pop() {
                None => break,
                Some(b) => {
                    removed = true;
                    if b & 0xc0 != 0x80 {
                        break;
                    }
                }
            }
        }
        removed
    }
}

/// Console input through a [`LineDisc`]: what `#c/cons` reads when the
/// console wants the discipline.
#[derive(Default)]
pub struct ConsReader {
    disc: Mutex<LineDisc>,
    raw: AtomicBool,
}

impl ConsReader {
    pub fn new() -> ConsReader {
        ConsReader::default()
    }

    /// Switches raw mode on or off, for the bytes read from the console
    /// from now on.
    pub fn set_raw(&self, on: bool) {
        self.raw.store(on, Ordering::Release);
    }

    pub fn is_raw(&self) -> bool {
        self.raw.load(Ordering::Acquire)
    }

    /// Reads a line (cooked) or the bytes that have arrived (raw) from
    /// `cons`, echoing to it in cooked mode. Returns 0 at end of input.
    pub fn read(&self, cons: &dyn Console, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // One reader at a time: the lock is held while waiting for input.
        let mut disc = self.disc.lock();
        loop {
            if let Some(n) = disc.take(buf) {
                return Ok(n);
            }
            let mut chunk = [0u8; 256];
            let n = cons.read(&mut chunk).map_err(Error::from)?;
            if n == 0 {
                disc.end();
                continue;
            }
            // The mode when the bytes are taken, not when the read began:
            // `rawon` may have been written while this slept.
            let raw = self.is_raw();
            let mut echo = Vec::new();
            for &b in &chunk[..n] {
                disc.input(b, raw, &mut echo);
            }
            if !echo.is_empty() {
                cons.write(&echo).map_err(Error::from)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec;
    use alloc::vec::Vec;
    use std::sync::Mutex as StdMutex;

    use super::*;

    /// Feeds `input` cooked; returns the echo.
    fn cooked(d: &mut LineDisc, input: &[u8]) -> Vec<u8> {
        let mut echo = Vec::new();
        for &b in input {
            d.input(b, false, &mut echo);
        }
        echo
    }

    fn take(d: &mut LineDisc) -> Option<Vec<u8>> {
        let mut buf = [0u8; 128];
        d.take(&mut buf).map(|n| buf[..n].to_vec())
    }

    #[test]
    fn echoes_and_delivers_a_line_at_a_time() {
        let mut d = LineDisc::new();
        assert_eq!(cooked(&mut d, b"echo"), b"echo");
        assert_eq!(take(&mut d), None, "no newline yet");
        assert_eq!(cooked(&mut d, b" hi\nls\n"), b" hi\nls\n");
        assert_eq!(take(&mut d).unwrap(), b"echo hi\n");
        assert_eq!(take(&mut d).unwrap(), b"ls\n");
        assert_eq!(take(&mut d), None);
    }

    #[test]
    fn cr_and_crlf_read_as_one_newline() {
        let mut d = LineDisc::new();
        assert_eq!(cooked(&mut d, b"a\rb\r\nc\n\n"), b"a\nb\nc\n\n");
        for want in [&b"a\n"[..], b"b\n", b"c\n", b"\n"] {
            assert_eq!(take(&mut d).unwrap(), want);
        }
        assert_eq!(take(&mut d), None);
    }

    #[test]
    fn backspace_and_del_erase_one_character() {
        let mut d = LineDisc::new();
        let echo = cooked(&mut d, b"helo\x08lp\x7fo\n");
        assert_eq!(echo, b"helo\x08 \x08lp\x08 \x08o\n");
        assert_eq!(take(&mut d).unwrap(), b"hello\n");
    }

    #[test]
    fn erasing_an_empty_line_echoes_nothing() {
        let mut d = LineDisc::new();
        assert_eq!(cooked(&mut d, b"\x08\x7f\x15"), b"");
        assert_eq!(cooked(&mut d, b"a\x08\x08\x08b\n"), b"a\x08 \x08b\n");
        assert_eq!(take(&mut d).unwrap(), b"b\n");
    }

    #[test]
    fn kill_erases_the_whole_line() {
        let mut d = LineDisc::new();
        let echo = cooked(&mut d, b"junk\x15ok\n");
        assert_eq!(echo, b"junk\x08 \x08\x08 \x08\x08 \x08\x08 \x08ok\n");
        assert_eq!(take(&mut d).unwrap(), b"ok\n");
    }

    #[test]
    fn erase_removes_a_whole_utf8_character() {
        let mut d = LineDisc::new();
        // Two, three and four byte characters, each erased by one key.
        let echo = cooked(&mut d, "aé€😀".as_bytes());
        assert_eq!(echo, "aé€😀".as_bytes());
        assert_eq!(cooked(&mut d, b"\x7f"), ERASE);
        assert_eq!(d.line, "aé€".as_bytes());
        assert_eq!(cooked(&mut d, b"\x08"), ERASE);
        assert_eq!(d.line, "aé".as_bytes());
        assert_eq!(cooked(&mut d, b"\x7f"), ERASE);
        assert_eq!(d.line, b"a");
        // ^U echoes one erase for each character, not each byte.
        let echo = cooked(&mut d, "éé\x15".as_bytes());
        assert_eq!(echo, ["éé".as_bytes(), ERASE, ERASE, ERASE].concat());
        assert_eq!(cooked(&mut d, b"x\n"), b"x\n");
        assert_eq!(take(&mut d).unwrap(), b"x\n");
    }

    #[test]
    fn erase_stops_at_malformed_utf8() {
        let mut d = LineDisc::new();
        // Five stray continuation bytes: at most four go with one key.
        cooked(&mut d, b"a\x80\x80\x80\x80\x80");
        assert_eq!(cooked(&mut d, b"\x7f"), ERASE);
        assert_eq!(d.line, b"a\x80");
        assert_eq!(cooked(&mut d, b"\x7f"), ERASE);
        assert_eq!(d.line, b"");
    }

    #[test]
    fn eof_at_line_start_reads_as_zero_bytes() {
        let mut d = LineDisc::new();
        assert_eq!(cooked(&mut d, b"one\n\x04two\n"), b"one\ntwo\n");
        assert_eq!(take(&mut d).unwrap(), b"one\n");
        assert_eq!(take(&mut d).unwrap(), b"", "the ^D");
        assert_eq!(take(&mut d).unwrap(), b"two\n", "input goes on after it");
        assert_eq!(take(&mut d), None);
    }

    #[test]
    fn eof_mid_line_delivers_the_partial_line() {
        let mut d = LineDisc::new();
        assert_eq!(cooked(&mut d, b"part\x04"), b"part", "^D is not echoed");
        assert_eq!(take(&mut d).unwrap(), b"part");
        assert_eq!(take(&mut d), None, "that ^D was not an end of input");
        cooked(&mut d, b"\x04");
        assert_eq!(take(&mut d).unwrap(), b"");
        // The delivered text can no longer be erased.
        assert_eq!(cooked(&mut d, b"ab\x04\x7f\x7fc\n"), b"abc\n");
        assert_eq!(take(&mut d).unwrap(), b"ab");
        assert_eq!(take(&mut d).unwrap(), b"c\n");
    }

    #[test]
    fn ctrl_c_is_an_ordinary_byte() {
        let mut d = LineDisc::new();
        assert_eq!(cooked(&mut d, b"a\x03b\n"), b"a\x03b\n");
        assert_eq!(take(&mut d).unwrap(), b"a\x03b\n");
    }

    #[test]
    fn a_short_read_keeps_the_rest_of_the_line() {
        let mut d = LineDisc::new();
        cooked(&mut d, b"abcdef\nxy\n");
        let mut buf = [0u8; 4];
        assert_eq!(d.take(&mut buf), Some(4));
        assert_eq!(&buf, b"abcd");
        assert_eq!(d.take(&mut buf), Some(3));
        assert_eq!(&buf[..3], b"ef\n");
        assert_eq!(d.take(&mut buf), Some(3));
        assert_eq!(&buf[..3], b"xy\n");
        assert_eq!(d.take(&mut buf), None);
    }

    #[test]
    fn a_line_is_capped() {
        let mut d = LineDisc::new();
        let echo = cooked(&mut d, &vec![b'x'; MAX_LINE + 100]);
        assert_eq!(echo.len(), MAX_LINE - 1, "the rest is not echoed");
        assert_eq!(cooked(&mut d, b"\n"), b"\n");
        let mut buf = vec![0u8; 2 * MAX_LINE];
        assert_eq!(d.take(&mut buf), Some(MAX_LINE));
        assert_eq!(buf[MAX_LINE - 1], b'\n');
    }

    #[test]
    fn raw_mode_passes_bytes_through_unechoed() {
        let mut d = LineDisc::new();
        let mut echo = Vec::new();
        for &b in b"a\x7f\x15\r\n\x04\x08" {
            d.input(b, true, &mut echo);
        }
        assert_eq!(echo, b"");
        assert_eq!(take(&mut d).unwrap(), b"a\x7f\x15\r\n\x04\x08");
        assert_eq!(take(&mut d), None);
        // More raw bytes arriving before a read join the same chunk.
        d.input(b'x', true, &mut echo);
        d.input(b'y', true, &mut echo);
        assert_eq!(take(&mut d).unwrap(), b"xy");
    }

    #[test]
    fn switching_modes_keeps_what_was_typed() {
        let mut d = LineDisc::new();
        let mut echo = Vec::new();
        // rawon delivers the unfinished line as it stands.
        cooked(&mut d, b"half");
        d.input(b'q', true, &mut echo);
        assert_eq!(take(&mut d).unwrap(), b"half");
        assert_eq!(take(&mut d).unwrap(), b"q");
        // rawoff: raw bytes not yet read stay apart from the next line,
        // and cannot be erased.
        d.input(b'r', true, &mut echo);
        assert_eq!(cooked(&mut d, b"\x7fline\n"), b"line\n");
        assert_eq!(take(&mut d).unwrap(), b"r");
        assert_eq!(take(&mut d).unwrap(), b"line\n");
        // A CR typed raw does not swallow a cooked LF after it.
        d.input(b'\r', true, &mut echo);
        assert_eq!(cooked(&mut d, b"\n"), b"\n");
        assert_eq!(take(&mut d).unwrap(), b"\r");
        assert_eq!(take(&mut d).unwrap(), b"\n");
    }

    #[test]
    fn end_of_the_console_delivers_what_was_typed_first() {
        let mut d = LineDisc::new();
        cooked(&mut d, b"tail");
        d.end();
        assert_eq!(take(&mut d).unwrap(), b"tail");
        assert_eq!(take(&mut d).unwrap(), b"");
        assert_eq!(take(&mut d), None);
    }

    /// A console that hands out its input in the given pieces, then ends.
    struct Script {
        input: StdMutex<VecDeque<Vec<u8>>>,
        output: StdMutex<Vec<Vec<u8>>>,
    }

    impl Script {
        fn new(pieces: &[&[u8]]) -> Script {
            Script {
                input: StdMutex::new(pieces.iter().map(|p| p.to_vec()).collect()),
                output: StdMutex::new(Vec::new()),
            }
        }

        fn output(&self) -> Vec<Vec<u8>> {
            self.output.lock().unwrap().clone()
        }
    }

    impl Console for Script {
        fn read(&self, buf: &mut [u8]) -> purg_platform::Result<usize> {
            let Some(p) = self.input.lock().unwrap().pop_front() else {
                return Ok(0);
            };
            buf[..p.len()].copy_from_slice(&p);
            Ok(p.len())
        }

        fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
            self.output.lock().unwrap().push(buf.to_vec());
            Ok(buf.len())
        }

        fn line_discipline(&self) -> bool {
            true
        }
    }

    fn read(r: &ConsReader, cons: &Script) -> Vec<u8> {
        let mut buf = [0u8; 64];
        let n = r.read(cons, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    #[test]
    fn reader_echoes_each_batch_in_one_write() {
        let cons = Script::new(&[b"ech", b"p\x7fo hi\r", b"\x04"]);
        let r = ConsReader::new();
        assert_eq!(read(&r, &cons), b"echo hi\n");
        assert_eq!(
            cons.output(),
            [&b"ech"[..], b"p\x08 \x08o hi\n"],
            "one write for each batch read"
        );
        assert_eq!(read(&r, &cons), b"", "^D");
        assert_eq!(read(&r, &cons), b"", "the console's own end");
        assert_eq!(cons.output().len(), 2, "nothing more was echoed");
    }

    #[test]
    fn reader_in_raw_mode_returns_bytes_as_they_arrive() {
        let cons = Script::new(&[b"a", b"\x7f\r", b"line\n", b"x"]);
        let r = ConsReader::new();
        r.set_raw(true);
        assert!(r.is_raw());
        assert_eq!(read(&r, &cons), b"a");
        assert_eq!(read(&r, &cons), b"\x7f\r");
        assert!(cons.output().is_empty(), "raw mode echoes nothing");
        r.set_raw(false);
        assert_eq!(read(&r, &cons), b"line\n");
        assert_eq!(cons.output(), [b"line\n"]);
        assert_eq!(read(&r, &cons), b"x", "the partial line at the end");
        assert_eq!(read(&r, &cons), b"");
    }

    #[test]
    fn reader_delivers_several_lines_from_one_batch_one_per_read() {
        let cons = Script::new(&[b"a\nb\nc"]);
        let r = ConsReader::new();
        assert_eq!(read(&r, &cons), b"a\n");
        assert_eq!(read(&r, &cons), b"b\n");
        assert_eq!(read(&r, &cons), b"c");
        assert_eq!(cons.output(), [b"a\nb\nc"]);
    }
}
