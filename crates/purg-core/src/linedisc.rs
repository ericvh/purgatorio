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
//!   sequence, and echo `\b \b` for each column it took. Terminals send
//!   DEL for the Backspace key, so DEL erases.
//! - ^U kills the line, erasing each character.
//! - ^D ends the input: at the start of a line a read returns 0; after
//!   some text the read returns that text with no newline. ^D is not
//!   echoed.
//! - ^C is the interrupt key. It throws away what was typed and not yet
//!   read, typeahead included, echoes `^C` and a newline, and calls the
//!   reader's interrupt hook ([`ConsReader::set_interrupt`]), which posts
//!   the `interrupt` note to the console's foreground process group
//!   (`docs/abi.md`, "Interrupts").
//! - Every other byte is ordinary: it is echoed and delivered.
//!
//! Inferno's interrupt key is DEL. Here DEL erases, because the terminals
//! the kernel's UART sits behind (QEMU's stdio on Linux and macOS) send
//! DEL for Backspace; ^C is the key those terminals' users expect.
//!
//! In raw mode (`rawon` written to `consctl`) nothing is echoed or edited,
//! ^C is an ordinary byte, and bytes are delivered as they arrive, CR as
//! CR. Switching to raw mode delivers a line still being typed as it
//! stands.
//!
//! Erasing knows how many columns each character took: the discipline
//! follows the terminal's column through the echo and through what is
//! written to the console ([`ConsReader::output`]), so a tab erases back
//! to where it started and a wide character erases both its columns.
//!
//! Where the input is processed depends on the console. One whose read
//! blocks until input arrives ([`Console::interrupt_driven`], the
//! kernel's UART) is read by a console kproc ([`ConsReader::pump`]), so
//! input is echoed as it is typed, typeahead included, and the interrupt
//! key acts when it arrives. Otherwise the discipline runs in the reader:
//! a byte is echoed when a read of `cons` takes it from the console. The
//! echo of each batch of input is one [`Console::write`], so it does not
//! mix with another writer's buffer.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use purg_platform::Console;
use purg_platform::sync::{Mutex, Rendez};

use crate::error::{Error, Result};

const BS: u8 = 0x08;
const DEL: u8 = 0x7f;
const KILL: u8 = 0x15;
const EOF: u8 = 0x04;
/// The interrupt key, ^C.
pub const INTR: u8 = 0x03;

/// The longest line, in bytes. What is typed past it is dropped, and not
/// echoed.
pub const MAX_LINE: usize = 4096;

/// What erases one column on the terminal.
const ERASE: &[u8] = b"\x08 \x08";

/// What the interrupt key echoes.
const INTR_ECHO: &[u8] = b"^C\n";

/// The columns `c` takes on a terminal: 0 for combining marks and other
/// zero-width characters, 2 for East Asian wide and full-width ones and
/// emoji, 1 for the rest. A short table, not all of Unicode's.
pub fn width(c: char) -> usize {
    const ZERO: &[(u32, u32)] = &[
        (0x0300, 0x036f),
        (0x0483, 0x0489),
        (0x0591, 0x05bd),
        (0x0610, 0x061a),
        (0x064b, 0x065f),
        (0x200b, 0x200f),
        (0x2028, 0x202e),
        (0x2060, 0x2064),
        (0x20d0, 0x20ff),
        (0xfe00, 0xfe0f),
        (0xfe20, 0xfe2f),
        (0xfeff, 0xfeff),
        (0xe0100, 0xe01ef),
    ];
    const WIDE: &[(u32, u32)] = &[
        (0x1100, 0x115f),
        (0x231a, 0x231b),
        (0x2329, 0x232a),
        (0x23e9, 0x23ec),
        (0x25fd, 0x25fe),
        (0x2614, 0x2615),
        (0x2648, 0x2653),
        (0x26a1, 0x26a1),
        (0x26aa, 0x26ab),
        (0x26bd, 0x26be),
        (0x26c4, 0x26c5),
        (0x26f5, 0x26f5),
        (0x26fa, 0x26fa),
        (0x2705, 0x2705),
        (0x270a, 0x270b),
        (0x274c, 0x274c),
        (0x2753, 0x2755),
        (0x2795, 0x2797),
        (0x2b1b, 0x2b1c),
        (0x2b50, 0x2b50),
        (0x2e80, 0x303e),
        (0x3041, 0x33ff),
        (0x3400, 0x4dbf),
        (0x4e00, 0x9fff),
        (0xa000, 0xa4cf),
        (0xa960, 0xa97f),
        (0xac00, 0xd7a3),
        (0xf900, 0xfaff),
        (0xfe10, 0xfe19),
        (0xfe30, 0xfe6f),
        (0xff00, 0xff60),
        (0xffe0, 0xffe6),
        (0x1f300, 0x1f64f),
        (0x1f680, 0x1f6ff),
        (0x1f900, 0x1f9ff),
        (0x1fa70, 0x1faff),
        (0x20000, 0x2fffd),
        (0x30000, 0x3fffd),
    ];
    let c = c as u32;
    let within = |t: &[(u32, u32)]| {
        t.binary_search_by(|&(lo, hi)| {
            if hi < c {
                core::cmp::Ordering::Less
            } else if lo > c {
                core::cmp::Ordering::Greater
            } else {
                core::cmp::Ordering::Equal
            }
        })
        .is_ok()
    };
    if within(ZERO) {
        0
    } else if within(WIDE) {
        2
    } else {
        1
    }
}

/// The terminal's cursor column, followed through the bytes sent to it.
#[derive(Default)]
struct Column {
    col: usize,
    /// A UTF-8 sequence part way through: its bytes so far.
    seq: [u8; 4],
    have: usize,
    want: usize,
}

impl Column {
    /// Whether `b` continues the UTF-8 sequence part way through.
    fn continues(&self, b: u8) -> bool {
        self.want > 0 && b & 0xc0 == 0x80
    }

    /// Settles a sequence that `b` cuts short: it took a column. Returns
    /// whether `b` begins a character, so the column before it is where
    /// that character starts.
    fn begins(&mut self, b: u8) -> bool {
        if self.continues(b) {
            return false;
        }
        if self.want > 0 {
            self.want = 0;
            self.col += 1;
        }
        true
    }

    fn advance(&mut self, b: u8) {
        if self.continues(b) {
            self.seq[self.have] = b;
            self.have += 1;
            if self.have == self.want {
                self.want = 0;
                self.col += core::str::from_utf8(&self.seq[..self.have])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .map_or(1, width);
            }
            return;
        }
        self.begins(b);
        match b {
            b'\n' | b'\r' => self.col = 0,
            b'\t' => self.col = (self.col / 8 + 1) * 8,
            BS => self.col = self.col.saturating_sub(1),
            0..0x20 | DEL => {}
            0x20..0x80 => self.col += 1,
            _ => {
                let want = match b {
                    0xc0..0xe0 => 2,
                    0xe0..0xf0 => 3,
                    0xf0..0xf8 => 4,
                    _ => 0,
                };
                if want == 0 {
                    self.col += 1;
                } else {
                    self.seq[0] = b;
                    self.have = 1;
                    self.want = want;
                }
            }
        }
    }
}

/// The state machine: bytes in, echo and readable chunks out. It does no
/// I/O, so it is tested on its own.
#[derive(Default)]
pub struct LineDisc {
    /// The line being typed.
    line: Vec<u8>,
    /// Where each character of `line` begins: its offset in `line`, and
    /// the column it began at. A byte that continues no UTF-8 sequence is
    /// a character of its own.
    starts: Vec<(usize, usize)>,
    /// What reads return, one chunk each at most. An empty chunk is an
    /// end of input.
    ready: VecDeque<Vec<u8>>,
    /// The last cooked byte was CR, so a following LF is dropped.
    after_cr: bool,
    /// The last chunk of `ready` is raw bytes, and more may join it.
    raw_tail: bool,
    /// The terminal's column.
    col: Column,
    /// The interrupt key was typed since [`LineDisc::take_interrupt`].
    interrupted: bool,
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
                self.echo(b"\n", echo);
                self.flush_line();
            }
            BS | DEL => self.erase(echo),
            KILL => {
                while !self.line.is_empty() {
                    self.erase(echo);
                }
            }
            EOF => {
                if self.line.is_empty() {
                    self.ready.push_back(Vec::new());
                } else {
                    self.flush_line();
                }
            }
            INTR => {
                self.line.clear();
                self.starts.clear();
                self.ready.clear();
                self.after_cr = false;
                self.echo(INTR_ECHO, echo);
                self.interrupted = true;
            }
            // One byte is kept for the newline.
            _ if self.line.len() + 1 < MAX_LINE => {
                if self.col.begins(b) {
                    self.starts.push((self.line.len(), self.col.col));
                }
                self.line.push(b);
                self.echo(&[b], echo);
            }
            _ => {}
        }
    }

    /// Whether the interrupt key was typed since the last call.
    pub fn take_interrupt(&mut self) -> bool {
        core::mem::take(&mut self.interrupted)
    }

    /// Something else was written to the terminal: the column moves.
    pub fn output(&mut self, buf: &[u8]) {
        for &b in buf {
            self.col.advance(b);
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

    fn echo(&mut self, bytes: &[u8], echo: &mut Vec<u8>) {
        self.output(bytes);
        echo.extend_from_slice(bytes);
    }

    /// Makes the line being typed, if any, readable.
    fn flush_line(&mut self) {
        self.starts.clear();
        if !self.line.is_empty() {
            self.ready.push_back(core::mem::take(&mut self.line));
            self.raw_tail = false;
        }
    }

    /// Removes the last character of the line, a whole UTF-8 sequence,
    /// and wipes the columns it took.
    fn erase(&mut self, echo: &mut Vec<u8>) {
        let Some((at, col)) = self.starts.pop() else {
            self.line.clear();
            return;
        };
        self.line.truncate(at);
        for _ in 0..self.col.col.saturating_sub(col) {
            echo.extend_from_slice(ERASE);
        }
        self.col.col = self.col.col.min(col);
        self.col.want = 0;
    }
}

/// What a console kproc and the readers share: the discipline, and a
/// count of the batches of input it took, which readers sleep on.
#[derive(Default)]
struct Shared {
    disc: Mutex<LineDisc>,
    batches: AtomicUsize,
    arrived: Rendez,
    /// The console ended; reads past the input return 0.
    ended: AtomicBool,
}

/// Console input through a [`LineDisc`]: what `#c/cons` reads when the
/// console wants the discipline.
#[derive(Default)]
pub struct ConsReader {
    shared: Shared,
    raw: AtomicBool,
    /// A console kproc runs the discipline ([`ConsReader::pump`]).
    pumped: AtomicBool,
    /// Called when the interrupt key is typed.
    on_interrupt: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
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

    /// Sets what the interrupt key does.
    pub fn set_interrupt(&self, f: Box<dyn Fn() + Send + Sync>) {
        *self.on_interrupt.lock() = Some(f);
    }

    /// `buf` was written to the console: the terminal's column moves.
    /// Followed only under a console kproc: a reader that runs the
    /// discipline holds it while it waits for input.
    pub fn output(&self, buf: &[u8]) {
        if self.pumped.load(Ordering::Acquire) {
            self.shared.disc.lock().output(buf);
        }
    }

    fn interrupt(&self) {
        if let Some(f) = &*self.on_interrupt.lock() {
            f();
        }
    }

    /// Takes one batch of input through the discipline and echoes it.
    /// Returns whether the interrupt key was in it, and how the echo went.
    fn feed(&self, cons: &dyn Console, bytes: &[u8], disc: &mut LineDisc) -> (bool, Result<()>) {
        // The mode when the bytes are taken, not when a read began:
        // `rawon` may have been written meanwhile.
        let raw = self.is_raw();
        let mut echo = Vec::new();
        for &b in bytes {
            disc.input(b, raw, &mut echo);
        }
        let wrote = if echo.is_empty() {
            Ok(())
        } else {
            cons.write(&echo).map(|_| ()).map_err(Error::from)
        };
        (disc.take_interrupt(), wrote)
    }

    /// Says whether a console kproc runs [`ConsReader::pump`]: while one
    /// does, [`ConsReader::read`] takes what it leaves and never reads the
    /// console itself. Set before starting the kproc, so that no reader
    /// reads the console meanwhile.
    pub fn set_pumped(&self, on: bool) {
        self.pumped.store(on, Ordering::Release);
    }

    /// The console kproc: reads `cons` as input arrives, running the
    /// discipline over it, until the console ends.
    pub fn pump(&self, cons: &dyn Console) {
        self.set_pumped(true);
        let s = &self.shared;
        let mut chunk = [0u8; 256];
        loop {
            let n = cons.read(&mut chunk).unwrap_or(0);
            let interrupted = {
                let mut disc = s.disc.lock();
                if n == 0 {
                    disc.end();
                    s.ended.store(true, Ordering::Release);
                    false
                } else {
                    // An echo that cannot be written is lost; the input
                    // is not.
                    self.feed(cons, &chunk[..n], &mut disc).0
                }
            };
            s.batches.fetch_add(1, Ordering::AcqRel);
            s.arrived.wakeup();
            if interrupted {
                self.interrupt();
            }
            if n == 0 {
                return;
            }
        }
    }

    /// Reads a line (cooked) or the bytes that have arrived (raw) from
    /// `cons`, echoing to it in cooked mode. Returns 0 at end of input.
    pub fn read(&self, cons: &dyn Console, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let s = &self.shared;
        if self.pumped.load(Ordering::Acquire) {
            loop {
                let seen = s.batches.load(Ordering::Acquire);
                {
                    let mut disc = s.disc.lock();
                    if let Some(n) = disc.take(buf) {
                        return Ok(n);
                    }
                    if s.ended.load(Ordering::Acquire) {
                        return Ok(0);
                    }
                }
                crate::block::park(&s.arrived, || s.batches.load(Ordering::Acquire) != seen)?;
            }
        }
        // One reader at a time: the lock is held while waiting for input.
        let mut disc = s.disc.lock();
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
            let (interrupted, wrote) = self.feed(cons, &chunk[..n], &mut disc);
            if interrupted {
                self.interrupt();
            }
            wrote?;
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
        // The emoji is two columns wide.
        assert_eq!(cooked(&mut d, b"\x7f"), [ERASE, ERASE].concat());
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
    fn erase_takes_stray_bytes_one_at_a_time() {
        let mut d = LineDisc::new();
        // Stray continuation bytes, and a sequence cut short by another
        // character, are characters of one byte each.
        cooked(&mut d, b"a\x80\x80\xc3b");
        assert_eq!(cooked(&mut d, b"\x7f"), ERASE);
        assert_eq!(d.line, b"a\x80\x80\xc3");
        assert_eq!(cooked(&mut d, b"\x7f"), ERASE);
        assert_eq!(d.line, b"a\x80\x80");
        assert_eq!(cooked(&mut d, b"\x7f"), ERASE);
        assert_eq!(d.line, b"a\x80");
        assert_eq!(cooked(&mut d, b"\x7f\x7f"), [ERASE, ERASE].concat());
        assert_eq!(d.line, b"");
    }

    #[test]
    fn erase_wipes_the_columns_a_character_took() {
        let mut d = LineDisc::new();
        // A tab from column 2 (after a prompt) reaches column 8.
        d.output(b"; ");
        cooked(&mut d, b"a\tb");
        assert_eq!(cooked(&mut d, b"\x7f"), ERASE, "b");
        assert_eq!(cooked(&mut d, b"\x7f"), [ERASE; 5].concat(), "the tab");
        assert_eq!(cooked(&mut d, b"\x7f"), ERASE, "a");
        // Wide characters take two columns, combining marks none.
        cooked(&mut d, "日e\u{301}".as_bytes());
        assert_eq!(cooked(&mut d, b"\x7f"), b"", "the combining accent");
        assert_eq!(cooked(&mut d, b"\x7f"), ERASE);
        assert_eq!(cooked(&mut d, b"\x7f"), [ERASE, ERASE].concat());
        assert!(d.line.is_empty());
        // ^U wipes a line of tabs back to where it began.
        cooked(&mut d, b"\t\tx");
        assert_eq!(cooked(&mut d, b"\x15"), [ERASE; 15].concat());
        // A line typed after output that ended mid-line starts there.
        d.output(b"\nlong prompt> ");
        cooked(&mut d, b"\t");
        assert_eq!(cooked(&mut d, b"\x7f"), [ERASE; 3].concat());
    }

    #[test]
    fn widths() {
        assert_eq!(width('a'), 1);
        assert_eq!(width('é'), 1);
        assert_eq!(width('\u{301}'), 0);
        assert_eq!(width('日'), 2);
        assert_eq!(width('한'), 2);
        assert_eq!(width('Ａ'), 2);
        assert_eq!(width('😀'), 2);
        assert_eq!(width('€'), 1);
    }

    #[test]
    fn interrupt_drops_unread_input() {
        let mut d = LineDisc::new();
        cooked(&mut d, b"typeahead\n");
        assert_eq!(cooked(&mut d, b"half\x03"), b"half^C\n");
        assert!(d.take_interrupt());
        assert!(!d.take_interrupt(), "taken once");
        assert_eq!(take(&mut d), None, "the line and the typeahead went");
        assert_eq!(cooked(&mut d, b"\x7fok\n"), b"ok\n", "nothing to erase");
        assert_eq!(take(&mut d).unwrap(), b"ok\n");
        // Raw, ^C is a byte like any other.
        let mut echo = Vec::new();
        d.input(INTR, true, &mut echo);
        assert!(!d.take_interrupt());
        assert_eq!(take(&mut d).unwrap(), b"\x03");
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
    fn other_control_bytes_are_ordinary() {
        let mut d = LineDisc::new();
        assert_eq!(cooked(&mut d, b"a\x01b\n"), b"a\x01b\n");
        assert_eq!(take(&mut d).unwrap(), b"a\x01b\n");
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

    #[test]
    fn pump_echoes_as_input_arrives_and_interrupts() {
        use alloc::sync::Arc;
        use core::sync::atomic::AtomicUsize;

        let cons = Arc::new(Script::new(&[
            b"one\n",
            b"typeahead\n",
            b"lost\x03",
            b"two\x7fo\n",
        ]));
        let r = Arc::new(ConsReader::new());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        r.set_interrupt(Box::new(move || {
            h.fetch_add(1, Ordering::SeqCst);
        }));
        // The kproc runs to the console's end before anything reads.
        let (pr, pc) = (r.clone(), cons.clone());
        std::thread::spawn(move || pr.pump(&*pc)).join().unwrap();
        assert_eq!(
            cons.output(),
            [
                &b"one\n"[..],
                b"typeahead\n",
                b"lost^C\n",
                b"two\x08 \x08o\n"
            ],
            "echoed as it arrived"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(read(&r, &cons), b"two\n", "^C dropped what came before");
        assert_eq!(read(&r, &cons), b"", "the console's end");
        assert_eq!(read(&r, &cons), b"", "and it stays ended");
    }

    #[test]
    fn a_reader_waits_for_the_pump() {
        use alloc::sync::Arc;

        let cons = Arc::new(Script::new(&[b"later\n"]));
        let r = Arc::new(ConsReader::new());
        r.pumped.store(true, Ordering::Release);
        let rr = r.clone();
        let rc = cons.clone();
        let reader = std::thread::spawn(move || read(&rr, &rc));
        std::thread::sleep(std::time::Duration::from_millis(20));
        r.pump(&*cons);
        assert_eq!(reader.join().unwrap(), b"later\n");
    }
}
