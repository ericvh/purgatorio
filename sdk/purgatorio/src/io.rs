//! Standard input, output and error, and the print macros.
//!
//! In a wasm32 guest these are `std::io`'s and `std`'s macros. A wasm64
//! guest has no WASI, and its `std` discards what is printed; there they
//! are this module's own, over fds 0, 1 and 2 (`docs/wasm64.md`). Code
//! that uses [`stdout`](fn@stdout) and [`println!`](crate::println) from
//! here builds for both widths.
//!
//! As with `std`, standard output is buffered by line and standard error
//! is not buffered.

#[cfg(not(target_arch = "wasm64"))]
pub use std::io::{Stderr, Stdin, StdinLock, Stdout, stderr, stdin, stdout};

#[cfg(target_arch = "wasm64")]
pub use wide::{
    Stderr, Stdin, StdinLock, Stdout, last_error, read_fd, stderr, stdin, stdout, write_fd,
};

/// `std::print!` for both widths.
#[cfg(not(target_arch = "wasm64"))]
#[macro_export]
macro_rules! print {
    ($($t:tt)*) => { ::std::print!($($t)*) };
}

/// `std::println!` for both widths.
#[cfg(not(target_arch = "wasm64"))]
#[macro_export]
macro_rules! println {
    ($($t:tt)*) => { ::std::println!($($t)*) };
}

/// `std::eprint!` for both widths.
#[cfg(not(target_arch = "wasm64"))]
#[macro_export]
macro_rules! eprint {
    ($($t:tt)*) => { ::std::eprint!($($t)*) };
}

/// `std::eprintln!` for both widths.
#[cfg(not(target_arch = "wasm64"))]
#[macro_export]
macro_rules! eprintln {
    ($($t:tt)*) => { ::std::eprintln!($($t)*) };
}

/// `std::print!` for both widths.
#[cfg(target_arch = "wasm64")]
#[macro_export]
macro_rules! print {
    ($($t:tt)*) => { $crate::io::_print(::core::format_args!($($t)*)) };
}

/// `std::println!` for both widths.
#[cfg(target_arch = "wasm64")]
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($t:tt)*) => { $crate::io::_print(::core::format_args!("{}\n", ::core::format_args!($($t)*))) };
}

/// `std::eprint!` for both widths.
#[cfg(target_arch = "wasm64")]
#[macro_export]
macro_rules! eprint {
    ($($t:tt)*) => { $crate::io::_eprint(::core::format_args!($($t)*)) };
}

/// `std::eprintln!` for both widths.
#[cfg(target_arch = "wasm64")]
#[macro_export]
macro_rules! eprintln {
    () => { $crate::eprint!("\n") };
    ($($t:tt)*) => { $crate::io::_eprint(::core::format_args!("{}\n", ::core::format_args!($($t)*))) };
}

#[cfg(target_arch = "wasm64")]
#[doc(hidden)]
pub use wide::{_eprint, _print};

#[cfg(target_arch = "wasm64")]
mod wide {
    use std::cell::{RefCell, RefMut};
    use std::fmt;
    use std::io::{self, BufRead, BufReader, LineWriter, Read, Write};

    use crate::sys;

    /// The prog's error string as an `io::Error`.
    pub fn last_error() -> io::Error {
        io::Error::other(crate::errstr())
    }

    /// `sys->read`.
    pub fn read_fd(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) };
        if n < 0 {
            Err(last_error())
        } else {
            Ok(n as usize)
        }
    }

    /// `sys->write`.
    pub fn write_fd(fd: i32, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe { sys::write(fd, buf.as_ptr(), buf.len()) };
        if n < 0 {
            Err(last_error())
        } else {
            Ok(n as usize)
        }
    }

    /// An fd as a reader and writer that does not own it.
    struct Raw(i32);

    impl Read for Raw {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            read_fd(self.0, buf)
        }
    }

    impl Write for Raw {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            write_fd(self.0, buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // The buffers are leaked so that a lock can hold a borrow of one for
    // as long as it likes. A wasm64 guest has one thread.
    thread_local! {
        static IN: &'static RefCell<BufReader<Raw>> =
            Box::leak(Box::new(RefCell::new(BufReader::new(Raw(0)))));
        static OUT: &'static RefCell<LineWriter<Raw>> =
            Box::leak(Box::new(RefCell::new(LineWriter::new(Raw(1)))));
    }

    /// Standard input, fd 0, buffered.
    #[derive(Debug, Clone, Copy)]
    pub struct Stdin;

    /// Standard input held for a run of reads: what [`Stdin::lock`] gives.
    /// While it lives, other reads of standard input panic.
    pub struct StdinLock(RefMut<'static, BufReader<Raw>>);

    /// Standard output, fd 1, buffered by line.
    #[derive(Debug, Clone, Copy)]
    pub struct Stdout;

    /// Standard error, fd 2, unbuffered.
    #[derive(Debug, Clone, Copy)]
    pub struct Stderr;

    pub fn stdin() -> Stdin {
        Stdin
    }

    pub fn stdout() -> Stdout {
        Stdout
    }

    pub fn stderr() -> Stderr {
        Stderr
    }

    impl Stdin {
        pub fn lock(&self) -> StdinLock {
            StdinLock(IN.with(|i| i.borrow_mut()))
        }

        /// Reads a line, with its newline, onto the end of `buf`.
        pub fn read_line(&self, buf: &mut String) -> io::Result<usize> {
            self.lock().read_line(buf)
        }

        /// The lines of standard input, without their newlines.
        pub fn lines(self) -> io::Lines<StdinLock> {
            self.lock().lines()
        }
    }

    impl Read for Stdin {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.lock().read(buf)
        }
    }

    impl Read for StdinLock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl BufRead for StdinLock {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            self.0.fill_buf()
        }

        fn consume(&mut self, n: usize) {
            self.0.consume(n);
        }
    }

    impl Stdout {
        /// As `std::io::Stdout::lock`: this is already the only user.
        pub fn lock(&self) -> Stdout {
            Stdout
        }
    }

    impl Write for Stdout {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            OUT.with(|o| o.borrow_mut().write(buf))
        }

        fn flush(&mut self) -> io::Result<()> {
            OUT.with(|o| o.borrow_mut().flush())
        }
    }

    impl Stderr {
        /// As `std::io::Stderr::lock`: this is already the only user.
        pub fn lock(&self) -> Stderr {
            Stderr
        }
    }

    impl Write for Stderr {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            write_fd(2, buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// What `print!` does: a failed write ends the prog quietly, as the
    /// panic hook does for `std`'s macros in a wasm32 guest (see `rt`).
    pub fn _print(args: fmt::Arguments<'_>) {
        if Stdout.write_fmt(args).is_err() {
            unsafe { sys::exits("".as_ptr(), 0) }
        }
    }

    /// What `eprint!` does. A failed write to standard error is ignored.
    pub fn _eprint(args: fmt::Arguments<'_>) {
        let _ = Stderr.write_fmt(args);
    }
}
