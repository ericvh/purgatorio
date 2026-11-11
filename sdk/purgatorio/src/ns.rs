//! The namespace as `ns` prints it.
//!
//! A prog's `/prog/n/ns` file is the kernel's raw view, one line per bind
//! or mount and a last `cd` line, as in Inferno:
//!
//! ```text
//! bind  /tmp/a /tmp/b
//! bind -bc /tmp/c /tmp/b
//! mount -c #|/data1 /n/x spec
//! cd /tmp
//! ```
//!
//! The flags field is empty for a replacing bind, so such a line has two
//! spaces, and nothing is quoted. [`tidy`] turns that into what Inferno's
//! `ns` prints: words quoted for the shell, `#/` and `#U` dropped from the
//! front of a source, a mount's attach name left out, and the source of a
//! mount on a network connection (`/net/tcp/3/data`) shown as the
//! connection's address (`tcp!10.0.0.1!564`).

/// Characters that make [`quoted`] quote a word, as in Inferno's `ns`.
const SHELLMETA: &[char] = &['\'', ' ', '\t', '\\', '$', '#'];

/// `raw`, the contents of a `/prog/n/ns` file, as `ns` prints it. With
/// `net` set, a mount on a `/net` conversation shows the address read
/// from the conversation's `remote` file (`ns` without `-r`).
pub fn tidy(raw: &str, net: bool) -> String {
    let mut out = String::new();
    for line in raw.lines() {
        if let Some(l) = tidy_line(line, net) {
            out.push_str(&l);
            out.push('\n');
        }
    }
    out
}

fn tidy_line(line: &str, net: bool) -> Option<String> {
    let mut words = line.split([' ', '\n']).filter(|w| !w.is_empty());
    let cmd = words.next()?;
    let mut rest: Vec<&str> = words.collect();
    if rest.is_empty() {
        return None;
    }
    if cmd == "cd" {
        return Some(format!("cd {}", quoted(rest[0])));
    }
    let mut flag = String::new();
    if rest[0].starts_with('-') {
        flag = format!("{} ", rest.remove(0));
    }
    if rest.len() < 2 {
        return None;
    }
    let mut src = String::from(rest[0]);
    if src.len() >= 3 && (src.starts_with("#/") || src.starts_with("#U")) {
        src.drain(..2);
    }
    let dest = match rest[1] {
        "#M" => "/",
        d => d,
    };
    if cmd == "mount" && net {
        src = netaddr(src);
    }
    Some(format!("{cmd} {flag}{} {}", quoted(&src), quoted(dest)))
}

/// `proto!address` for `/net/proto/n/data` (or under `/net.alt`), from
/// the conversation's `remote` file; `f` itself otherwise.
fn netaddr(f: String) -> String {
    if !f.starts_with('/') {
        return f;
    }
    let flds: Vec<&str> = f.split('/').filter(|e| !e.is_empty()).collect();
    if flds.len() < 4 || !matches!(flds[0], "net" | "net.alt") || flds[3] != "data" {
        return f;
    }
    let remote = format!("/{}/{}/{}/remote", flds[0], flds[1], flds[2]);
    let addr = match std::fs::read(&remote) {
        Ok(b) if !b.is_empty() => b,
        _ => return f,
    };
    let addr = String::from_utf8_lossy(&addr[..addr.len().min(256)]);
    let addr = addr.strip_suffix('\n').unwrap_or(&addr);
    if flds[0] == "net" {
        format!("{}!{addr}", flds[1])
    } else {
        format!("/{}/{}!{addr}", flds[0], flds[1])
    }
}

/// `s`, in single quotes with quotes doubled when it holds a character
/// the shell would take specially.
pub fn quoted(s: &str) -> String {
    if !s.contains(SHELLMETA) {
        return String::from(s);
    }
    let mut r = String::from("'");
    for c in s.chars() {
        if c == '\'' {
            r.push('\'');
        }
        r.push(c);
    }
    r.push('\'');
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidies_as_inferno_ns() {
        let raw = "bind  /tmp/a /tmp/b\n\
                   bind -ac #U /\n\
                   bind -b #U/dev /dev\n\
                   bind  #/ /n/root\n\
                   mount -c #|/data1 /n/x spec\n\
                   mount  /srv/fs /n/y \n\
                   bind  /a #M\n\
                   mount -b /net/tcp/0/data /n/z \n\
                   cd /tmp/it's\n";
        let want = "bind /tmp/a /tmp/b\n\
                    bind -ac '#U' /\n\
                    bind -b /dev /dev\n\
                    bind '#/' /n/root\n\
                    mount -c '#|/data1' /n/x\n\
                    mount /srv/fs /n/y\n\
                    bind /a /\n\
                    mount -b /net/tcp/0/data /n/z\n\
                    cd '/tmp/it''s'\n";
        assert_eq!(tidy(raw, false), want);
    }
}
