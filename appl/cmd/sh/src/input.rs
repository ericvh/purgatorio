//! A byte reader with one byte of push-back, over an fd or a string, for
//! `getcsv` and the S-expression parser.

use std::fs::File;
use std::io::Read;
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;

pub struct Input {
    fd: Option<i32>,
    buf: Vec<u8>,
    pos: usize,
    /// Bytes consumed before `buf[0]`.
    base: usize,
}

impl Input {
    /// Reads from `fd`, which stays open.
    pub fn fd(fd: i32) -> Input {
        Input {
            fd: Some(fd),
            buf: Vec::new(),
            pos: 0,
            base: 0,
        }
    }

    pub fn bytes(b: &[u8]) -> Input {
        Input {
            fd: None,
            buf: b.to_vec(),
            pos: 0,
            base: 0,
        }
    }

    pub fn get(&mut self) -> Option<u8> {
        if self.pos == self.buf.len() {
            let fd = self.fd?;
            // Keep one byte for unget.
            let keep = self.buf.len().min(1);
            self.base += self.buf.len() - keep;
            self.buf.drain(..self.buf.len() - keep);
            self.pos = keep;
            let mut chunk = [0u8; 4096];
            // SAFETY: the fd belongs to the caller, and ManuallyDrop keeps
            // the File from closing it.
            let mut f = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
            match f.read(&mut chunk) {
                Ok(n) if n > 0 => self.buf.extend_from_slice(&chunk[..n]),
                _ => {
                    self.fd = None;
                    return None;
                }
            }
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Some(b)
    }

    /// Pushes back the byte last read.
    pub fn unget(&mut self) {
        self.pos = self.pos.saturating_sub(1);
    }

    /// How many bytes have been consumed.
    pub fn offset(&self) -> usize {
        self.base + self.pos
    }

    /// The bytes not yet read, for a reader over a string.
    pub fn rest(&self) -> &[u8] {
        &self.buf[self.pos..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_back() {
        let mut i = Input::bytes(b"ab");
        assert_eq!(i.get(), Some(b'a'));
        i.unget();
        assert_eq!(i.get(), Some(b'a'));
        assert_eq!(i.get(), Some(b'b'));
        assert_eq!(i.offset(), 2);
        assert_eq!(i.get(), None);
        assert!(i.rest().is_empty());
    }
}
