//! sendmail [-s server] [recipient ...]: send the mail message on
//! standard input by SMTP, as Inferno's sendmail(1) with smtp(2).
//!
//! The message's header (the lines before the first empty one) is
//! scanned for `From:`, `To:` and `Cc:`, in any case. The recipients are
//! the arguments, or those listed on the `To:` line, but not both. Without
//! a `From:` line the sender is the user (`/dev/user`); a sender with no
//! `@domain` gets `$DOMAIN`, else the contents of
//! `/usr/user/mail/domain`. The `Cc:` recipients get the message too.
//!
//! The message goes to the mail server `$smtp`, the `smtp` attribute
//! cs(8) finds in the network database (on the SMTP port), or with `-s`
//! (Purgatorio's addition, for tests and for hosts with no such entry)
//! to `server`, a dial address whose network and port default to `tcp`
//! and 25. sendmail says `RSET`, `HELO` with the sender's domain, `MAIL
//! FROM`, `RCPT TO` for each recipient, then sends the message with
//! leading dots doubled and lines ended by CR LF, and `QUIT`s. A
//! failure is reported as smtp(2) does (`send failed : RCPT receive
//! failed ( no such user)`) and gives an error status.
//!
//! Unlike Inferno's, sendmail does not put a `From ` mailbox line before
//! the message (a server takes it as the end of the header), and it looks
//! for the header lines in the header only.

use std::fs::File;
use std::io::{Read, Write};

use purgatorio::{Context, Error, dial};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut args: Vec<String> = args.into_iter().skip(1).collect();
    let mut server: Option<String> = None;
    if args.first().map(String::as_str) == Some("-s") {
        args.remove(0);
        if args.is_empty() {
            return Err(Error::from("usage: sendmail [-s server] [recipient ...]"));
        }
        server = Some(args.remove(0));
    } else if args.first().map(String::as_str) == Some("--") {
        args.remove(0);
    }
    let mut msg = Vec::new();
    std::io::stdin().read_to_end(&mut msg)?;
    let msg = String::from_utf8_lossy(&msg).into_owned();
    let h = Header::scan(&msg);
    if !h.to.is_empty() && !args.is_empty() {
        fail("recipients specified on To: line and as args - aborted");
    }
    let from = h.from.clone().unwrap_or_else(|| {
        std::fs::read_to_string("/dev/user")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    });
    let from = adddom(&from);
    let to = if h.to.is_empty() { args } else { h.to.clone() };
    let mut smtp = match Smtp::open(server.as_deref()) {
        Ok(s) => s,
        Err(e) => fail(&format!("smtp open failed: {e}")),
    };
    let sent = smtp.sendmail(&from, &to, &h.cc, &msg);
    smtp.close();
    if let Err(e) = sent {
        fail(&format!("send failed : {e}"));
    }
    Ok(())
}

fn fail(msg: &str) -> ! {
    eprintln!("sendmail: {msg}");
    purgatorio::exits("fail:error");
}

/// The addresses on the `From:`, `To:` and `Cc:` lines of a header.
#[derive(Debug, Default, PartialEq)]
struct Header {
    from: Option<String>,
    to: Vec<String>,
    cc: Vec<String>,
}

impl Header {
    fn scan(msg: &str) -> Header {
        let mut h = Header::default();
        for line in msg.lines() {
            if line.trim_end_matches('\r').is_empty() {
                break;
            }
            if h.from.is_none() {
                h.from = field(line, "from").map(|v| extract(&v));
            }
            if h.to.is_empty() {
                h.to = list(line, "to");
            }
            if h.cc.is_empty() {
                h.cc = list(line, "cc");
            }
        }
        h
    }
}

/// The value of a header line `name:` (or `name :`), the name in any
/// case, without the blanks around it.
fn field(line: &str, name: &str) -> Option<String> {
    let head = line.get(..name.len())?;
    if !head.eq_ignore_ascii_case(name) {
        return None;
    }
    let rest = &line[name.len()..];
    let rest = rest.strip_prefix(':').or_else(|| rest.strip_prefix(" :"))?;
    let v = rest.trim_matches([' ', '\t', '\r', '\n']);
    (!v.is_empty()).then(|| v.to_string())
}

/// The addresses of a `To:` or `Cc:` line: for each comma-separated
/// item, the address in its `<...>`, else its words.
fn list(line: &str, name: &str) -> Vec<String> {
    let Some(v) = field(line, name) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for part in v.split(',') {
        if part.contains('<') {
            out.push(extract(part));
        } else {
            out.extend(
                part.split([' ', '\t'])
                    .filter(|w| !w.is_empty())
                    .map(String::from),
            );
        }
    }
    out
}

/// The address in `Name <address>`, or the whole of it.
fn extract(s: &str) -> String {
    match s.find('<') {
        Some(i) => {
            let rest = &s[i + 1..];
            rest[..rest.find('>').unwrap_or(rest.len())].to_string()
        }
        None => s.to_string(),
    }
}

/// `user@` the domain, for a sender with no domain.
fn adddom(s: &str) -> String {
    if s.is_empty() || s.contains('@') {
        return s.to_string();
    }
    let dom = purgatorio::env::get("DOMAIN")
        .filter(|d| !d.trim_end().is_empty())
        .or_else(|| std::fs::read_to_string(format!("/usr/{s}/mail/domain")).ok())
        .map(|d| d.trim_end_matches('\n').to_string())
        .filter(|d| !d.is_empty());
    match dom {
        Some(d) => format!("{s}@{d}"),
        None => s.to_string(),
    }
}

/// The address to dial: `$smtp` by default, else `server` with `tcp`
/// and port 25 filled in.
fn server_addr(server: Option<&str>) -> String {
    match server {
        None => String::from("tcp!$smtp!smtp"),
        Some(s) => match s.split('!').count() {
            1 => format!("tcp!{s}!25"),
            2 => format!("{s}!25"),
            _ => s.to_string(),
        },
    }
}

/// An SMTP conversation (smtp(2)).
struct Smtp {
    f: File,
    /// What has been read but not yet taken as a line.
    buf: Vec<u8>,
}

impl Smtp {
    fn open(server: Option<&str>) -> Result<Smtp, String> {
        let mut c = dial::dial(&server_addr(server)).map_err(|_| String::from("dialup failed"))?;
        let f = File::from(c.data.take().ok_or("dialup failed")?);
        let mut s = Smtp { f, buf: Vec::new() };
        s.reply()?;
        Ok(s)
    }

    /// The server's reply: Ok for 1xx, 2xx and 3xx, else the text after
    /// the code. A reply of several lines (`250-`) is read to its end.
    fn reply(&mut self) -> Result<(), String> {
        loop {
            let Some(line) = self.line() else {
                return Err(String::from("could not read response from server"));
            };
            let l = line.trim_end_matches('\n').trim_end_matches('\r');
            if l.len() < 3 {
                return Err(String::from("short response from server"));
            }
            if l.as_bytes().get(3) == Some(&b'-') {
                continue;
            }
            if l.starts_with(['1', '2', '3']) {
                return Ok(());
            }
            return Err(l[3..].to_string());
        }
    }

    /// The next line from the server, or `None` at its end.
    fn line(&mut self) -> Option<String> {
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let l: Vec<u8> = self.buf.drain(..=i).collect();
                return Some(String::from_utf8_lossy(&l).into_owned());
            }
            let mut b = [0u8; 512];
            match self.f.read(&mut b) {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.buf.extend_from_slice(&b[..n]),
            }
        }
    }

    fn write(&mut self, s: &str) -> bool {
        self.f.write_all(format!("{s}\r\n").as_bytes()).is_ok()
    }

    /// A command and its reply; a failure names the command.
    fn cmd(&mut self, s: &str) -> Result<(), String> {
        let name = &s[..s.find([' ', ':']).unwrap_or(s.len())];
        if !self.write(s) {
            return Err(format!("{name} send failed"));
        }
        self.reply()
            .map_err(|r| format!("{name} receive failed ({r})"))
    }

    fn sendmail(
        &mut self,
        from: &str,
        to: &[String],
        cc: &[String],
        msg: &str,
    ) -> Result<(), String> {
        self.cmd("RSET")?;
        let (user, dom) = from.split_once('@').unwrap_or((from, ""));
        if from.is_empty() || user.is_empty() {
            return Err(String::from("no 'from' name"));
        }
        if to.is_empty() {
            return Err(String::from("no 'to' name"));
        }
        if dom.is_empty() {
            return Err(String::from("no domain name"));
        }
        self.cmd(&format!("HELO {dom}"))?;
        self.cmd(&format!("MAIL FROM:<{from}>"))?;
        let mut errs = String::new();
        for r in to.iter().chain(cc) {
            if let Err(e) = self.cmd(&format!("RCPT TO:<{r}>")) {
                errs.push(' ');
                errs.push_str(&e);
            }
        }
        if !errs.is_empty() {
            return Err(errs);
        }
        self.cmd("DATA")?;
        let mut body = String::new();
        for line in msg.split_inclusive('\n') {
            let line = line.trim_end_matches('\n').trim_end_matches('\r');
            if line.starts_with('.') {
                body.push('.');
            }
            body.push_str(line);
            body.push_str("\r\n");
        }
        if self.f.write_all(body.as_bytes()).is_err() {
            return Err(format!("write to server failed: {}", purgatorio::errstr()));
        }
        self.cmd(".")
    }

    fn close(&mut self) {
        self.write("QUIT");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header() {
        let h = Header::scan(
            "From: Bob <bob@example.org>\nTO: a@x.org, Carl <c@y.org>\ncc :d@z\nSubject: hi\n\nTo: body@not.org\n",
        );
        assert_eq!(h.from.as_deref(), Some("bob@example.org"));
        assert_eq!(h.to, ["a@x.org", "c@y.org"]);
        assert_eq!(h.cc, ["d@z"]);
        assert_eq!(Header::scan("\nTo: x@y\n"), Header::default());
        assert_eq!(field("Fromage: x", "from"), None);
    }

    #[test]
    fn addresses() {
        assert_eq!(server_addr(None), "tcp!$smtp!smtp");
        assert_eq!(server_addr(Some("mail")), "tcp!mail!25");
        assert_eq!(server_addr(Some("tcp!mail")), "tcp!mail!25");
        assert_eq!(
            server_addr(Some("tcp!127.0.0.1!2525")),
            "tcp!127.0.0.1!2525"
        );
    }
}
