//! The network database: attrdb(6) text, as `/lib/ndb/local` holds it.
//!
//! An entry starts on a line that begins with a non-blank character and
//! goes on over the lines that begin with blanks. Each line holds
//! `attr=value` pairs (a bare `attr` has an empty value); `#` starts a
//! comment, and a value may be quoted with `'` or `"` (a doubled quote is
//! one quote). A database whose first file has a `database=` entry is the
//! files that entry lists with `file=`, searched in order.

pub mod daemon;
pub mod dns;

/// One `attr=value` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub attr: String,
    pub val: String,
}

/// An entry: its lines, each a list of pairs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub lines: Vec<Vec<Pair>>,
}

impl Entry {
    /// Every pair, in order.
    pub fn pairs(&self) -> impl Iterator<Item = &Pair> {
        self.lines.iter().flatten()
    }

    /// Whether the entry has `attr`, with value `val` if one is given.
    pub fn has(&self, attr: &str, val: Option<&str>) -> bool {
        self.pairs()
            .any(|p| p.attr == attr && val.is_none_or(|v| p.val == v))
    }

    /// Whether the entry has every `attr=val` pair of `query`, where a
    /// value of `*` matches any value.
    pub fn matches(&self, query: &[(String, String)]) -> bool {
        query
            .iter()
            .all(|(a, v)| self.has(a, if v == "*" { None } else { Some(v) }))
    }

    /// The first value of `attr`.
    pub fn get(&self, attr: &str) -> Option<&str> {
        self.pairs()
            .find(|p| p.attr == attr)
            .map(|p| p.val.as_str())
    }

    /// Every value of `attr`.
    pub fn all(&self, attr: &str) -> Vec<&str> {
        self.pairs()
            .filter(|p| p.attr == attr)
            .map(|p| p.val.as_str())
            .collect()
    }

    /// The values of `rattr` for a match on `attr=val`: those on the line
    /// of the match when it has any, otherwise those of the whole entry.
    pub fn near(&self, attr: &str, val: Option<&str>, rattr: &str) -> Vec<&str> {
        let line = self.lines.iter().find(|l| {
            l.iter()
                .any(|p| p.attr == attr && val.is_none_or(|v| p.val == v))
        });
        let near: Vec<&str> = line
            .into_iter()
            .flatten()
            .filter(|p| p.attr == rattr)
            .map(|p| p.val.as_str())
            .collect();
        if near.is_empty() {
            self.all(rattr)
        } else {
            near
        }
    }

    /// The entry on one line, as `attr=value` pairs.
    pub fn line(&self) -> String {
        let pairs: Vec<String> = self.pairs().map(format_pair).collect();
        pairs.join(" ")
    }

    /// The entry in attrdb format: its first line, then the others
    /// indented by a tab.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, l) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\t');
            }
            let pairs: Vec<String> = l.iter().map(format_pair).collect();
            out.push_str(&pairs.join(" "));
            out.push('\n');
        }
        out
    }
}

fn format_pair(p: &Pair) -> String {
    format!("{}={}", quote(&p.attr), quote(&p.val))
}

/// Opens `file` with `read`, following a `database` entry's `file=` list.
/// `read` returns a file's text or the error to report.
pub fn open(
    file: &str,
    read: &dyn Fn(&str) -> Result<String, String>,
) -> Result<Vec<Entry>, String> {
    let first = parse(&read(file)?, file)?;
    let files = files(&first);
    if files.is_empty() {
        return Ok(first);
    }
    let mut db = Vec::new();
    for f in &files {
        db.extend(parse(&read(f)?, f)?);
    }
    Ok(db)
}

/// The files a `database` entry names.
pub fn files(db: &[Entry]) -> Vec<String> {
    db.iter()
        .find(|e| e.has("database", None))
        .map(|e| e.all("file").into_iter().map(String::from).collect())
        .unwrap_or_default()
}

/// Reads a file with std, reporting a failure the way Inferno's attrdb
/// does (`can't open file: err`); `err` is the system's error string.
pub fn read_std(file: &str, errstr: &dyn Fn() -> String) -> Result<String, String> {
    std::fs::read_to_string(file).map_err(|_| format!("can't open {file}: {}", errstr()))
}

/// Parses attrdb text. `name` names the file in errors (`file:line: err`).
pub fn parse(text: &str, name: &str) -> Result<Vec<Entry>, String> {
    let mut db: Vec<Entry> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let cont = raw.starts_with([' ', '\t']);
        let pairs = parse_line(raw).map_err(|e| format!("{name}:{}: {e}", n + 1))?;
        if pairs.is_empty() {
            continue;
        }
        match db.last_mut() {
            Some(e) if cont => e.lines.push(pairs),
            _ => db.push(Entry { lines: vec![pairs] }),
        }
    }
    Ok(db)
}

/// Splits one line into pairs, stopping at `#`.
pub fn parse_line(line: &str) -> Result<Vec<Pair>, &'static str> {
    let mut out = Vec::new();
    let mut c = line.chars().peekable();
    loop {
        while c.next_if(|ch| ch.is_whitespace()).is_some() {}
        match c.peek() {
            None | Some('#') => return Ok(out),
            _ => {}
        }
        let attr = word(&mut c)?;
        let val = if c.next_if_eq(&'=').is_some() {
            word(&mut c)?
        } else {
            String::new()
        };
        out.push(Pair { attr, val });
    }
}

/// A plain or quoted word, ending at white space, `=` or `#`.
fn word(c: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<String, &'static str> {
    let mut s = String::new();
    while let Some(&ch) = c.peek() {
        match ch {
            '\'' | '"' => {
                c.next();
                loop {
                    match c.next() {
                        None => return Err("missing quote"),
                        Some(q) if q == ch => {
                            if c.next_if_eq(&ch).is_some() {
                                s.push(ch);
                            } else {
                                break;
                            }
                        }
                        Some(o) => s.push(o),
                    }
                }
            }
            '=' | '#' => break,
            _ if ch.is_whitespace() => break,
            _ => {
                s.push(ch);
                c.next();
            }
        }
    }
    Ok(s)
}

/// Inferno's `%q`: quoted when empty or holding white space or a quote.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c == '\'') {
        return String::from(s);
    }
    format!("'{}'", s.replace('\'', "''"))
}

/// Splits a query or a registry line into words, honouring `'` quoting
/// as [`quote`] writes it.
pub fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut c = s.chars().peekable();
    loop {
        while c.next_if(|ch| ch.is_whitespace()).is_some() {}
        if c.peek().is_none() {
            return out;
        }
        let mut w = String::new();
        while let Some(&ch) = c.peek() {
            if ch.is_whitespace() {
                break;
            }
            c.next();
            if ch == '\'' {
                loop {
                    match c.next() {
                        None => break,
                        Some('\'') if c.next_if_eq(&'\'').is_some() => w.push('\''),
                        Some('\'') => break,
                        Some(o) => w.push(o),
                    }
                }
            } else {
                w.push(ch);
            }
        }
        out.push(w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DB: &str = "sys=fs dom=fs.example.org\n\tip=10.0.0.2 # c\n\ttcp=styx port=6666\n\
                      sys='a host' ip=10.0.0.9\n";

    #[test]
    fn entries_lines_and_lookups() {
        let db = parse(DB, "db").unwrap();
        assert_eq!(db.len(), 2);
        assert_eq!(db[0].lines.len(), 3);
        assert_eq!(db[0].get("ip"), Some("10.0.0.2"));
        assert!(db[0].has("tcp", Some("styx")));
        assert!(!db[0].has("tcp", Some("*")));
        let q = [(String::from("tcp"), String::from("*"))];
        assert!(db[0].matches(&q) && !db[1].matches(&q));
        assert_eq!(db[0].near("tcp", Some("styx"), "port"), vec!["6666"]);
        assert_eq!(db[1].get("sys"), Some("a host"));
        assert_eq!(db[1].line(), "sys='a host' ip=10.0.0.9");
        assert_eq!(
            db[0].text(),
            "sys=fs dom=fs.example.org\n\tip=10.0.0.2\n\ttcp=styx port=6666\n"
        );
    }

    #[test]
    fn errors_and_words() {
        assert_eq!(parse("a='b\n", "f").unwrap_err(), "f:1: missing quote");
        assert_eq!(words("a 'b c' 'd''e'"), vec!["a", "b c", "d'e"]);
        assert_eq!(quote(""), "''");
    }
}
