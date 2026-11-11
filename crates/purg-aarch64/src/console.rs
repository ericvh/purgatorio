//! The console: the PL011 as a [`purg_platform::Console`].
//!
//! Receive is interrupt-driven. The UART interrupt drains the receive FIFO
//! into a ring buffer. An [`Input::Blocking`] console's
//! [`Pl011Console::read`] sleeps on a [`Rendez`] until the ring has bytes,
//! as a terminal does (the kernel); an [`Input::Polled`] one returns what
//! has arrived, and 0 (end of input) when nothing has, for an image whose
//! programs take no console input (the probe). The ring is shared with the interrupt
//! handler, so it is a plain spinlock taken with IRQs masked, never a
//! `purg_platform::sync` lock. Readers also drain the FIFO themselves, so
//! input still arrives (once a second, at the sleep safety period) if the DTB names no interrupt.
//!
//! Input is raw: the bytes the UART received, CR for Enter and no end of
//! file. The console asks for the line discipline
//! ([`Console::line_discipline`]), so `#c/cons` echoes, edits lines, turns
//! CR into LF and reads ^D as end of input (`purg_core::linedisc`), and
//! `eia0` on the same UART stays a plain byte stream.
//!
//! Writes are serialised with a lock, so concurrent writers' buffers do not
//! interleave.

use core::fmt::{self, Write};

use purg_platform::Console;
use purg_platform::sync::{Mutex, Rendez};
use spin::Once;

use crate::board::board;
use crate::dtb::BootInfo;
use crate::exception::IrqGuard;
use crate::irq::{self, After};
use crate::mmu;
use crate::pl011::Pl011;

const RING: usize = 4096;

struct Ring {
    buf: [u8; RING],
    head: usize,
    len: usize,
    /// Bytes dropped because the ring was full.
    dropped: u64,
}

impl Ring {
    const fn new() -> Ring {
        Ring {
            buf: [0; RING],
            head: 0,
            len: 0,
            dropped: 0,
        }
    }

    fn push(&mut self, b: u8) {
        if self.len == RING {
            self.dropped += 1;
            return;
        }
        self.buf[(self.head + self.len) % RING] = b;
        self.len += 1;
    }

    fn peek(&self) -> Option<u8> {
        (self.len > 0).then(|| self.buf[self.head])
    }

    fn pop(&mut self) {
        self.head = (self.head + 1) % RING;
        self.len -= 1;
    }
}

/// Taken only with IRQs masked.
static RX: spin::Mutex<Ring> = spin::Mutex::new(Ring::new());

/// What [`Pl011Console::read`] does when no input has arrived.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    /// Sleep until some does.
    Blocking,
    /// Return 0, end of input.
    Polled,
}

pub struct Pl011Console {
    uart: Pl011,
    input: Input,
    rx: Rendez,
    wlock: Mutex<()>,
}

static CONSOLE: Once<Pl011Console> = Once::new();

/// The PL011 through the high-half device window.
fn uart() -> Pl011 {
    // SAFETY: the high half maps the device window, which holds the PL011,
    // as Device memory.
    unsafe { Pl011::new(mmu::phys_to_virt(board().uart)) }
}

/// Moves whatever the receive FIFO holds into the ring. IRQs must be
/// masked.
fn drain(uart: &Pl011) {
    let mut ring = RX.lock();
    let mut tmp = [0u8; 32];
    loop {
        let n = uart.read(&mut tmp);
        for &b in &tmp[..n] {
            ring.push(b);
        }
        if n < tmp.len() {
            break;
        }
    }
}

fn uart_irq() -> After {
    let uart = uart();
    drain(&uart);
    uart.clear_rx_interrupt();
    if let Some(c) = CONSOLE.get() {
        c.rx.wakeup();
    }
    After::Return
}

/// Sets up the console and, if the DTB gives the UART's interrupt, routes
/// it. Call once, after [`irq::init`]. The DTB's PL011 must be the
/// [`board`]'s.
pub fn init(info: &BootInfo<'_>, input: Input) -> &'static Pl011Console {
    let c = CONSOLE.call_once(|| Pl011Console {
        uart: uart(),
        input,
        rx: Rendez::new(),
        wlock: Mutex::new(()),
    });
    if let Some(intid) = info.uart_intid {
        irq::register(intid, uart_irq);
        c.uart.enable_rx_interrupt();
    }
    c
}

impl Pl011Console {
    fn ready(&self) -> bool {
        let _g = IrqGuard::new();
        drain(&self.uart);
        RX.lock().len > 0
    }

    /// Bytes dropped because nobody read them in time.
    pub fn dropped(&self) -> u64 {
        let _g = IrqGuard::new();
        RX.lock().dropped
    }
}

impl Console for Pl011Console {
    fn read(&self, buf: &mut [u8]) -> purg_platform::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match self.input {
            Input::Blocking => self.rx.sleep(|| self.ready()),
            Input::Polled => {
                self.ready();
            }
        }
        let _g = IrqGuard::new();
        let mut ring = RX.lock();
        let mut n = 0;
        while n < buf.len() {
            let Some(b) = ring.peek() else { break };
            ring.pop();
            buf[n] = b;
            n += 1;
        }
        Ok(n)
    }

    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        let _g = self.wlock.lock();
        self.uart.write(buf);
        Ok(buf.len())
    }

    fn line_discipline(&self) -> bool {
        true
    }
}

/// The unlocked console, for early boot and for `fail`, which may run in
/// an exception handler or with the console lock held.
pub struct Raw;

impl Write for Raw {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        uart().write(s.as_bytes());
        Ok(())
    }
}

/// Prints through the console lock once there is a console.
pub fn print(args: fmt::Arguments<'_>) {
    match CONSOLE.get() {
        Some(c) => {
            let _g = c.wlock.lock();
            let _ = Raw.write_fmt(args);
        }
        None => {
            let _ = Raw.write_fmt(args);
        }
    }
}
