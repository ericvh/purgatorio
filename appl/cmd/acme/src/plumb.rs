//! The plumber's `edit` port. A helper prog (`acme --plumb`) tells the
//! plumber acme reads the port, then copies each message it reads there
//! to a pipe that acme's loop waits on with the mouse and keyboard. acme
//! opens the file a message names, at the line `name:line` or an `addr`
//! attribute gives. Without a plumber the helper ends at once and acme
//! works on without it.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use plumbmsg::Msg;
use purgatorio::abi::{mode, pctl};

/// The argument that runs acme as the helper.
pub const HELPER: &str = "--plumb";
const PORT: &str = "edit";

/// The pipe from the helper.
pub struct Port {
    pipe: File,
    pid: u32,
    buf: Vec<u8>,
}

/// Starts the helper, or says nothing and gives `None` when it cannot
/// be started.
pub fn start() -> Option<Port> {
    let [ours, theirs] = purgatorio::pipe().ok()?;
    let pid = purgatorio::spawn(
        "/bin/acme.wasm",
        &[String::from("acme"), String::from(HELPER)],
        pctl::NEWFD,
        &[(theirs.as_raw_fd(), 1)],
    )
    .ok()?;
    drop(theirs);
    Some(Port {
        pipe: File::from(ours),
        pid,
        buf: Vec::new(),
    })
}

impl Port {
    pub fn fd(&self) -> i32 {
        self.pipe.as_raw_fd()
    }

    /// The whole messages read so far, or `None` when the helper has
    /// gone.
    pub fn read(&mut self) -> Option<Vec<Msg>> {
        let mut b = [0u8; 8192];
        let n = self.pipe.read(&mut b).ok().filter(|&n| n > 0)?;
        self.buf.extend_from_slice(&b[..n]);
        let mut out = Vec::new();
        while let Some(len) = msg_len(&self.buf) {
            let m: Vec<u8> = self.buf.drain(..len).collect();
            if let Some(m) = Msg::unpack(&m) {
                out.push(m);
            }
        }
        Some(out)
    }

    /// Kills the helper.
    pub fn stop(self) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/prog/{}/ctl", self.pid))
        {
            let _ = f.write_all(b"kill");
        }
    }
}

/// The length of the first message in `b` if it is all there: six
/// lines, the last the length of the data, then the data.
fn msg_len(b: &[u8]) -> Option<usize> {
    let mut at = 0;
    let mut last = 0;
    for _ in 0..6 {
        let nl = b[at..].iter().position(|&c| c == b'\n')?;
        last = at;
        at += nl + 1;
    }
    let n: usize = String::from_utf8_lossy(&b[last..at - 1])
        .trim()
        .parse()
        .ok()?;
    (b.len() >= at + n).then_some(at + n)
}

/// The file a message names and the line to show: `name:line`, or the
/// `addr` attribute when it is a line number.
pub fn target(m: &Msg) -> (String, Option<usize>) {
    let data = String::from_utf8_lossy(&m.data);
    let data = data.trim();
    let (name, line) = crate::split_addr(data);
    let attrs = plumbmsg::string2attrs(&m.attr);
    let line = line.or_else(|| plumbmsg::lookup(&attrs, "addr").and_then(|a| a.parse().ok()));
    (String::from(name), line)
}

/// `acme --plumb`: says acme reads the port and copies its messages to
/// standard output, until the plumber or acme goes.
pub fn helper() -> Result<(), purgatorio::Error> {
    let start = Msg {
        src: String::from(PORT),
        dst: String::from("plumb"),
        kind: String::from("text"),
        data: b"start".to_vec(),
        ..Msg::default()
    };
    let Ok(fd) = purgatorio::open(plumbmsg::INPUT, mode::OWRITE) else {
        return Ok(());
    };
    if File::from(fd).write_all(&start.pack()).is_err() {
        return Ok(());
    }
    let Ok(fd) = purgatorio::open(&plumbmsg::port_file(PORT), mode::OREAD) else {
        return Ok(());
    };
    let mut port = File::from(fd);
    let mut out = std::io::stdout();
    let mut b = vec![0u8; 64 * 1024];
    loop {
        let n = match port.read(&mut b) {
            Ok(0) | Err(_) => return Ok(()),
            Ok(n) => n,
        };
        if out.write_all(&b[..n]).and_then(|()| out.flush()).is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages() {
        let m = Msg {
            src: String::from("B"),
            dst: String::from("edit"),
            dir: String::from("/usr/x"),
            kind: String::from("text"),
            attr: String::new(),
            data: b"/usr/x/f.rs:12".to_vec(),
        };
        let mut two = m.pack();
        two.extend_from_slice(&m.pack());
        let one = m.pack().len();
        assert_eq!(msg_len(&two), Some(one));
        assert_eq!(msg_len(&two[..one - 1]), None);
        assert_eq!(target(&m), (String::from("/usr/x/f.rs"), Some(12)));
        let m = Msg {
            attr: String::from("addr=7"),
            data: b"f.rs".to_vec(),
            ..m
        };
        assert_eq!(target(&m), (String::from("f.rs"), Some(7)));
    }
}
