//! The FTP client side: replies, data connections and `LIST` lines.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};

use purgatorio::dial;

/// A reply: its code and its text, lines joined.
pub struct Reply {
    pub code: u32,
    pub text: String,
}

pub struct Ftp {
    /// The control connection, read through a buffer and written
    /// directly.
    input: BufReader<File>,
    /// The control connection's directory in /net, for active mode.
    pub dir: String,
    pub active: bool,
    pub debug: bool,
}

impl Ftp {
    pub fn new(conn: File, dir: String, active: bool, debug: bool) -> Ftp {
        Ftp {
            input: BufReader::new(conn),
            dir,
            active,
            debug,
        }
    }

    /// The control connection, for another prog to go on with.
    pub fn into_file(self) -> File {
        self.input.into_inner()
    }

    /// Reads one reply, of one line or several (`123-` ... `123 `).
    pub fn reply(&mut self) -> Result<Reply, String> {
        let mut text = String::new();
        let mut first: Option<String> = None;
        loop {
            let mut line = String::new();
            if self.input.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                return Err(String::from("ftp: connection closed"));
            }
            let line = line.trim_end_matches(['\r', '\n']).to_string();
            if self.debug {
                eprintln!("<- {line}");
            }
            let code = line
                .get(..3)
                .filter(|c| c.bytes().all(|b| b.is_ascii_digit()));
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(line.get(4..).unwrap_or(""));
            match (&first, code) {
                (None, Some(c)) if line.as_bytes().get(3) == Some(&b'-') => {
                    first = Some(String::from(c));
                }
                (None, Some(c)) => {
                    return Ok(Reply {
                        code: c.parse().unwrap_or(0),
                        text,
                    });
                }
                (Some(f), Some(c)) if f == c && line.as_bytes().get(3) == Some(&b' ') => {
                    return Ok(Reply {
                        code: c.parse().unwrap_or(0),
                        text,
                    });
                }
                (None, None) => return Err(format!("ftp: bad reply {line}")),
                _ => {}
            }
        }
    }

    pub fn send(&mut self, cmd: &str) -> Result<(), String> {
        if self.debug {
            let shown = if cmd.starts_with("PASS ") {
                "PASS ****"
            } else {
                cmd
            };
            eprintln!("-> {shown}");
        }
        self.input
            .get_mut()
            .write_all(format!("{cmd}\r\n").as_bytes())
            .map_err(|e| e.to_string())
    }

    pub fn cmd(&mut self, cmd: &str) -> Result<Reply, String> {
        self.send(cmd)?;
        self.reply()
    }

    /// Runs `cmd` over a data connection, sending `upload` or returning
    /// what comes back.
    pub fn transfer(&mut self, cmd: &str, upload: Option<&[u8]>) -> Result<Vec<u8>, Reply> {
        let fail = |text: String| Reply { code: 0, text };
        let (mut data, listener) = if self.active {
            let local = dial::local(&self.dir).map_err(|e| fail(e.0))?;
            let host = local
                .rsplit_once('!')
                .map(|(h, _)| h)
                .unwrap_or("127.0.0.1");
            let ann = dial::announce(&format!("tcp!{host}!0")).map_err(|e| fail(e.0))?;
            let here = dial::local(&ann.dir).map_err(|e| fail(e.0))?;
            let port: u32 = here
                .rsplit('!')
                .next()
                .and_then(|p| p.parse().ok())
                .unwrap_or(0);
            let r = self
                .cmd(&format!(
                    "PORT {},{},{}",
                    host.replace('.', ","),
                    port >> 8,
                    port & 255
                ))
                .map_err(fail)?;
            if r.code / 100 != 2 {
                return Err(r);
            }
            (None, Some(ann))
        } else {
            let r = self.cmd("PASV").map_err(fail)?;
            if r.code != 227 {
                return Err(r);
            }
            let addr = pasv_addr(&r.text)
                .ok_or_else(|| fail(format!("ftp: bad PASV reply {}", r.text)))?;
            let c = dial::dial(&addr).map_err(|e| fail(e.0))?;
            (c.data.map(File::from), None)
        };
        let r = self.cmd(cmd).map_err(fail)?;
        if r.code / 100 != 1 {
            return Err(r);
        }
        if let Some(ann) = listener {
            let c = dial::listen(&ann.dir).map_err(|e| fail(e.0))?;
            data = c.data.map(File::from);
        }
        let mut data = data.ok_or_else(|| fail(String::from("ftp: no data connection")))?;
        let mut got = Vec::new();
        let res = match upload {
            Some(bytes) => data.write_all(bytes),
            None => data.read_to_end(&mut got).map(drop),
        };
        drop(data);
        let r = self.reply().map_err(fail)?;
        res.map_err(|e| fail(e.to_string()))?;
        if r.code / 100 != 2 {
            return Err(r);
        }
        Ok(got)
    }
}

/// `tcp!h!p` from a 227 reply's `(h1,h2,h3,h4,p1,p2)`.
pub fn pasv_addr(text: &str) -> Option<String> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let n: Vec<u32> = text[start..]
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .take(6)
        .filter_map(|s| s.parse().ok())
        .collect();
    if n.len() != 6 || n.iter().any(|&x| x > 255) {
        return None;
    }
    Some(format!(
        "tcp!{}.{}.{}.{}!{}",
        n[0],
        n[1],
        n[2],
        n[3],
        n[4] * 256 + n[5]
    ))
}

/// A file a listing describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub perm: u32,
    pub uid: String,
    pub gid: String,
    pub size: u64,
    pub mtime: u32,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn year_of(t: i64) -> i64 {
    // The year holding day `t / 86400`, closely enough: search near.
    let guess = 1970 + t.div_euclid(86400 * 365);
    (guess - 2..=guess + 1)
        .rev()
        .find(|&y| days_from_civil(y, 1, 1) * 86400 <= t)
        .unwrap_or(guess)
}

/// The time `Mon dd hh:mm` or `Mon dd yyyy` gives, as seen at `now`: a
/// time without a year is in the last twelve months.
fn ls_time(mon: &str, day: &str, when: &str, now: i64) -> u32 {
    let Some(m) = MONTHS.iter().position(|&x| x.eq_ignore_ascii_case(mon)) else {
        return 0;
    };
    let m = m as i64 + 1;
    let Ok(d) = day.parse::<i64>() else {
        return 0;
    };
    let t = if let Some((h, mi)) = when.split_once(':') {
        let (Ok(h), Ok(mi)) = (h.parse::<i64>(), mi.parse::<i64>()) else {
            return 0;
        };
        let mut y = year_of(now);
        let at = |y: i64| days_from_civil(y, m, d) * 86400 + h * 3600 + mi * 60;
        if at(y) > now + 86400 {
            y -= 1;
        }
        at(y)
    } else {
        let Ok(y) = when.parse::<i64>() else {
            return 0;
        };
        days_from_civil(y, m, d) * 86400
    };
    u32::try_from(t).unwrap_or(0)
}

fn perm_of(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut p = 0;
    for (i, bit) in [0o400, 0o200, 0o100, 0o40, 0o20, 0o10, 4, 2, 1]
        .iter()
        .enumerate()
    {
        if b.get(i + 1).is_some_and(|&c| c != b'-') {
            p |= bit;
        }
    }
    p
}

/// One line of a `LIST` reply, in Unix `ls -l` form or the DOS form
/// (`03-01-24  10:20AM  <DIR>  name`); `None` for anything else (a
/// `total` line).
pub fn parse_list(line: &str, now: i64) -> Option<Entry> {
    let line = line.trim_end_matches(['\r', '\n']);
    let fields: Vec<&str> = line.split_whitespace().collect();
    let first = *fields.first()?;
    if first.len() >= 10 && "-dlcbps".contains(&first[..1]) && fields.len() >= 9 {
        // The name is what follows the eighth field, spaces and all.
        let mut rest = line;
        for _ in 0..8 {
            rest = rest.trim_start();
            rest = &rest[rest.find(char::is_whitespace)?..];
        }
        let name = rest.trim_start();
        let name = match first.as_bytes()[0] {
            b'l' => name.split(" -> ").next().unwrap_or(name),
            _ => name,
        };
        return Some(Entry {
            name: String::from(name),
            dir: first.starts_with('d'),
            perm: perm_of(first),
            uid: String::from(fields[2]),
            gid: String::from(fields[3]),
            size: fields[4].parse().unwrap_or(0),
            mtime: ls_time(fields[5], fields[6], fields[7], now),
        });
    }
    // DOS: date, time, size or <DIR>, name.
    if fields.len() >= 4 && first.len() >= 8 && first.as_bytes()[2] == b'-' {
        let mut rest = line;
        for _ in 0..3 {
            rest = rest.trim_start();
            rest = &rest[rest.find(char::is_whitespace)?..];
        }
        let dir = fields[2].eq_ignore_ascii_case("<DIR>");
        return Some(Entry {
            name: String::from(rest.trim_start()),
            dir,
            perm: if dir { 0o777 } else { 0o666 },
            uid: String::from("ftp"),
            gid: String::from("ftp"),
            size: if dir {
                0
            } else {
                fields[2].parse().unwrap_or(0)
            },
            mtime: 0,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_709_288_430; // 2024-03-01 10:20:30

    #[test]
    fn lists() {
        let e = parse_list(
            "-rw-r--r-- 1 bob staff 1234 Feb 28 09:15 a file.txt\r\n",
            NOW,
        )
        .unwrap();
        assert_eq!(e.name, "a file.txt");
        assert_eq!((e.dir, e.perm, e.size), (false, 0o644, 1234));
        assert_eq!(e.uid, "bob");
        assert_eq!(e.mtime, 1_709_111_700);
        let e = parse_list("drwxr-x---   2 root  root   4096 Dec 25  2023 sub", NOW).unwrap();
        assert!(e.dir);
        assert_eq!(e.perm, 0o750);
        assert_eq!(e.mtime, 1_703_462_400);
        // A time in what would be the future is last year's.
        let e = parse_list("-rw-r--r-- 1 u g 1 Dec 25 10:00 old", NOW).unwrap();
        assert_eq!(e.mtime, 1_703_498_400);
        let e = parse_list("lrwxrwxrwx 1 u g 3 Mar  1 10:00 ln -> target", NOW).unwrap();
        assert_eq!(e.name, "ln");
        assert!(parse_list("total 12", NOW).is_none());
        let e = parse_list("03-01-24  10:20AM       <DIR>          Program Files", NOW).unwrap();
        assert!(e.dir);
        assert_eq!(e.name, "Program Files");
        let e = parse_list("03-01-24  10:20AM                 42 x.txt", NOW).unwrap();
        assert_eq!(e.size, 42);
    }

    #[test]
    fn pasv() {
        assert_eq!(
            pasv_addr("Entering Passive Mode (127,0,0,1,195,80).").unwrap(),
            "tcp!127.0.0.1!50000"
        );
        assert!(pasv_addr("nothing").is_none());
        assert!(pasv_addr("(1,2,3,4,5)").is_none());
    }
}
