//! win: a shell in an acme window, after the `win` of Inferno's and
//! Plan 9's acme.
//!
//! `win` opens a new window through acme(4) (`/mnt/acme/new/ctl`), names
//! it `dir/-sysname`, and starts `sh -i` in a process group of its own
//! with its standard input, output and error on pipes. What the shell
//! writes is put into the body at the output point, which then moves
//! past it. Text typed after the output point is the shell's input: when
//! a newline is typed, everything after the output point up to the last
//! newline is sent to the shell. Button 2 on text that is not one of
//! acme's commands sends that text to the shell as a line; acme's own
//! commands and button 3 are handed back to acme. The Delete key or ^C
//! typed in the body sends `interrupt` to the shell's process group.
//! `Del` (or deleting the window any other way) kills the group, and the
//! window goes when the shell exits.
//!
//! acme(4)'s `event` file cannot say when it is ready, so a second copy
//! of win (`win -E`) copies it into a pipe; win waits on that pipe and
//! the shell's output with [`purgatorio::ready`].

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use purgatorio::abi::pctl;
use purgatorio::{Context, Error, Ready};

const ACME: &str = "/mnt/acme";

/// One acme(4) event.
#[derive(Debug, PartialEq, Eq)]
struct Event {
    origin: char,
    kind: char,
    q0: usize,
    q1: usize,
    flag: u32,
    text: String,
}

/// The first whole event in `s` and the bytes it takes, or `None` when
/// `s` does not hold one yet.
fn parse_one(s: &str) -> Option<(Event, usize)> {
    let mut cs = s.chars();
    let origin = cs.next()?;
    let kind = cs.next()?;
    let head = origin.len_utf8() + kind.len_utf8();
    let rest = &s[head..];
    let mut nums = [0usize; 4];
    let mut pos = 0;
    for n in &mut nums {
        let sp = rest[pos..].find(' ')?;
        *n = rest[pos..pos + sp].parse().ok()?;
        pos += sp + 1;
    }
    let mut end = pos;
    let mut text = rest[pos..].chars();
    for _ in 0..nums[3] {
        end += text.next()?.len_utf8();
    }
    if !rest[end..].starts_with('\n') {
        return None;
    }
    let e = Event {
        origin,
        kind,
        q0: nums[0],
        q1: nums[1],
        flag: u32::try_from(nums[2]).ok()?,
        text: rest[pos..end].to_string(),
    };
    Some((e, head + end + 1))
}

/// The whole events at the front of `buf`, which keeps the rest.
fn parse(buf: &mut Vec<u8>) -> Vec<Event> {
    let valid = match std::str::from_utf8(buf) {
        Ok(s) => s.len(),
        Err(e) => e.valid_up_to(),
    };
    let s = String::from_utf8_lossy(&buf[..valid]).into_owned();
    let mut out = Vec::new();
    let mut used = 0;
    while let Some((e, n)) = parse_one(&s[used..]) {
        out.push(e);
        used += n;
    }
    buf.drain(..used);
    out
}

/// Where the output point moves when acme changes `q0..q1` (an insert
/// when `insert`) and the change was not win's own: an insert before it
/// pushes it on, typing at or after it leaves it, and a delete before or
/// across it pulls it back.
fn track(q: usize, insert: bool, q0: usize, q1: usize) -> usize {
    if insert {
        if q0 < q { q + (q1 - q0) } else { q }
    } else if q1 <= q {
        q - (q1 - q0)
    } else if q0 < q {
        q0
    } else {
        q
    }
}

fn rw(path: &str) -> Result<File, Error> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| Error(format!("{path}: {}", Error::from(e))))
}

/// Writes `msg` to `/prog/<pid>/ctl`.
fn progctl(pid: u32, msg: &str) {
    if let Ok(mut f) = OpenOptions::new()
        .write(true)
        .open(format!("/prog/{pid}/ctl"))
    {
        let _ = f.write_all(msg.as_bytes());
    }
}

struct Win {
    ctl: File,
    addr: File,
    data: File,
    xdata: File,
    event: File,
    /// The output point: shell output goes here, and what is typed
    /// after it is the shell's input.
    q: usize,
    sh: u32,
    copier: u32,
    to_sh: File,
    from_sh: File,
    events: File,
    evbuf: Vec<u8>,
    carry: Vec<u8>,
    /// The flag of a button 2 or 3 event whose expansion comes next.
    expanding: Option<u32>,
}

impl Win {
    fn start() -> Result<Win, Error> {
        let mut ctl = rw(&format!("{ACME}/new/ctl"))?;
        let mut buf = [0u8; 256];
        let n = ctl.read(&mut buf)?;
        let id = String::from_utf8_lossy(&buf[..n])
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        let dir = format!("{ACME}/{id}");
        let event = rw(&format!("{dir}/event"))?;
        let addr = rw(&format!("{dir}/addr"))?;
        let data = rw(&format!("{dir}/data"))?;
        let xdata = rw(&format!("{dir}/xdata"))?;
        let cwd = purgatorio::dot().unwrap_or_else(|_| String::from("/"));
        let sys = std::fs::read_to_string("/dev/sysname")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && !s.contains(char::is_whitespace))
            .unwrap_or_else(|| String::from("win"));
        let name = if cwd == "/" {
            format!("/-{sys}")
        } else {
            format!("{}/-{sys}", cwd.trim_end_matches('/'))
        };
        ctl.write_all(format!("name {name}\ndumpdir {cwd}\ndump win\n").as_bytes())?;

        let [in_r, in_w] = purgatorio::pipe()?;
        let [out_r, out_w] = purgatorio::pipe()?;
        let sh = purgatorio::spawn(
            "/bin/sh.wasm",
            &[String::from("sh"), String::from("-i")],
            pctl::NEWFD | pctl::FORKNS | pctl::NEWPGRP,
            &[
                (in_r.as_raw_fd(), 0),
                (out_w.as_raw_fd(), 1),
                (out_w.as_raw_fd(), 2),
            ],
        )?;
        drop((in_r, out_w));
        let [ev_r, ev_w] = purgatorio::pipe()?;
        let copier = purgatorio::spawn(
            "/bin/win.wasm",
            &[String::from("win"), String::from("-E")],
            pctl::NEWFD,
            &[(event.as_raw_fd(), 0), (ev_w.as_raw_fd(), 1)],
        );
        drop(ev_w);
        let copier = match copier {
            Ok(p) => p,
            Err(e) => {
                progctl(sh, "killgrp");
                let _ = ctl.write_all(b"delete\n");
                return Err(e);
            }
        };
        Ok(Win {
            ctl,
            addr,
            data,
            xdata,
            event,
            q: 0,
            sh,
            copier,
            to_sh: File::from(in_w),
            from_sh: File::from(out_r),
            events: File::from(ev_r),
            evbuf: Vec::new(),
            carry: Vec::new(),
            expanding: None,
        })
    }

    fn run(&mut self) -> Result<(), Error> {
        loop {
            let fds = [
                Ready::Read(self.events.as_raw_fd()),
                Ready::Read(self.from_sh.as_raw_fd()),
            ];
            let mut buf = [0u8; 8192];
            match purgatorio::ready(&fds, -1)? {
                Some(0) => {
                    let n = self.events.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        // The window has gone.
                        self.finish(false);
                        return Ok(());
                    }
                    self.evbuf.extend_from_slice(&buf[..n]);
                    for e in parse(&mut self.evbuf) {
                        if !self.event(e)? {
                            self.finish(true);
                            return Ok(());
                        }
                    }
                }
                Some(1) => {
                    let n = self.from_sh.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        // The shell has exited.
                        self.finish(true);
                        return Ok(());
                    }
                    self.carry.extend_from_slice(&buf[..n]);
                    let valid = match std::str::from_utf8(&self.carry) {
                        Ok(s) => s.len(),
                        Err(e) if e.error_len().is_some() => self.carry.len(),
                        Err(e) => e.valid_up_to(),
                    };
                    let s = String::from_utf8_lossy(&self.carry[..valid]).into_owned();
                    self.carry.drain(..valid);
                    self.output(&s)?;
                }
                _ => {}
            }
        }
    }

    /// Kills the shell's process group and the event copier and, with
    /// `delete`, the window.
    fn finish(&mut self, delete: bool) {
        progctl(self.sh, "killgrp");
        if delete {
            let _ = self.ctl.write_all(b"delete\n");
        }
        progctl(self.copier, "kill");
    }

    fn set_addr(&mut self, a: &str) -> Result<(), Error> {
        self.addr.write_all(a.as_bytes())?;
        Ok(())
    }

    /// Puts `s` into the body at the output point, which moves past it.
    fn output(&mut self, s: &str) -> Result<(), Error> {
        if s.is_empty() {
            return Ok(());
        }
        self.set_addr(&format!("#{}", self.q))?;
        self.data.write_all(s.as_bytes())?;
        self.q += s.chars().count();
        self.ctl.write_all(b"clean\n")?;
        Ok(())
    }

    /// Sends what was typed after the output point, up to the last
    /// newline, to the shell.
    fn send_typed(&mut self) -> Result<(), Error> {
        self.set_addr(&format!("#{},$", self.q))?;
        let mut text = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            // `xdata` stops at the end of the address.
            let n = self.xdata.read(&mut buf)?;
            if n == 0 {
                break;
            }
            text.extend_from_slice(&buf[..n]);
        }
        let text = String::from_utf8_lossy(&text).into_owned();
        let Some(nl) = text.rfind('\n') else {
            return Ok(());
        };
        let line = &text[..=nl];
        self.q += line.chars().count();
        self.to_sh.write_all(line.as_bytes())?;
        self.ctl.write_all(b"clean\n")?;
        Ok(())
    }

    /// Acts on one event. False when win should end.
    fn event(&mut self, e: Event) -> Result<bool, Error> {
        match e.kind {
            'I' | 'D' => {
                // win's own writes come back as E and F events; it has
                // moved the output point for them already.
                if e.origin == 'E' || e.origin == 'F' {
                    return Ok(true);
                }
                let insert = e.kind == 'I';
                let typed = insert && e.q0 >= self.q;
                self.q = track(self.q, insert, e.q0, e.q1);
                if !typed {
                    return Ok(true);
                }
                if e.text.contains(['\x7F', '\x03']) {
                    self.interrupt(&e)?;
                    return Ok(true);
                }
                if e.text.contains('\n') || (e.text.is_empty() && e.q1 > e.q0) {
                    self.send_typed()?;
                }
            }
            'X' | 'x' | 'L' | 'l' => {
                if e.flag & 2 != 0 && self.expanding.is_none() {
                    // The expansion follows.
                    self.expanding = Some(e.flag);
                    return Ok(true);
                }
                let flag = self.expanding.take().unwrap_or(e.flag);
                let exec = e.kind.eq_ignore_ascii_case(&'x');
                let cmd = e.text.trim();
                if exec && flag & 1 != 0 && (cmd == "Del" || cmd == "Delete") {
                    return Ok(false);
                }
                if exec && flag & 1 == 0 && !cmd.is_empty() {
                    // Not one of acme's: a line for the shell, shown as
                    // if typed.
                    let line = format!("{cmd}\n");
                    self.output(&line)?;
                    self.to_sh.write_all(line.as_bytes())?;
                    return Ok(true);
                }
                // Back to acme, which does what it would have done.
                let back = format!("{}{}{} {} 0 0 \n", e.origin, e.kind, e.q0, e.q1);
                self.event.write_all(back.as_bytes())?;
            }
            _ => {}
        }
        Ok(true)
    }

    /// The Delete key or ^C was typed: takes the keys out of the body
    /// and sends `interrupt` to the shell's group.
    fn interrupt(&mut self, e: &Event) -> Result<(), Error> {
        let keys: Vec<usize> = e
            .text
            .chars()
            .enumerate()
            .filter(|(_, c)| *c == '\x7F' || *c == '\x03')
            .map(|(i, _)| e.q0 + i)
            .collect();
        for &k in keys.iter().rev() {
            self.set_addr(&format!("#{},#{}", k, k + 1))?;
            // An empty write deletes the address (write_all would send
            // nothing).
            let _ = self.data.write(b"")?;
            self.q = track(self.q, false, k, k + 1);
        }
        progctl(self.sh, "interruptgrp");
        Ok(())
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("-E") {
        // The copier: the event file on fd 0 into the pipe on fd 1.
        let (mut ev, mut out) = (std::io::stdin(), std::io::stdout());
        let mut buf = [0u8; 8192];
        while let Ok(n) = ev.read(&mut buf) {
            if n == 0 || out.write_all(&buf[..n]).and_then(|()| out.flush()).is_err() {
                break;
            }
        }
        return Ok(());
    }
    if args.len() > 1 {
        return Err(Error::from("usage: win"));
    }
    let mut w = Win::start()?;
    w.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events() {
        let mut b = b"KI3 4 0 1 \n\nMX1 1 3 0 \nMX0 3 0 3 Del\nKI9".to_vec();
        let es = parse(&mut b);
        assert_eq!(
            es,
            [
                Event {
                    origin: 'K',
                    kind: 'I',
                    q0: 3,
                    q1: 4,
                    flag: 0,
                    text: String::from("\n"),
                },
                Event {
                    origin: 'M',
                    kind: 'X',
                    q0: 1,
                    q1: 1,
                    flag: 3,
                    text: String::new(),
                },
                Event {
                    origin: 'M',
                    kind: 'X',
                    q0: 0,
                    q1: 3,
                    flag: 0,
                    text: String::from("Del"),
                },
            ]
        );
        assert_eq!(b, b"KI9");
        let mut b = "KI0 2 0 2 é \n".as_bytes().to_vec();
        assert_eq!(parse(&mut b)[0].text, "é ");
        assert!(b.is_empty());
    }

    #[test]
    fn output_point() {
        assert_eq!(track(10, true, 3, 5), 12);
        assert_eq!(track(10, true, 10, 12), 10);
        assert_eq!(track(10, true, 11, 12), 10);
        assert_eq!(track(10, false, 2, 4), 8);
        assert_eq!(track(10, false, 8, 12), 8);
        assert_eq!(track(10, false, 10, 12), 10);
    }
}
