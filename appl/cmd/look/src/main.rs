//! look [-dfnix] [-r endkey] [-tc] [string] [file]: print the lines of a
//! sorted file whose key begins with `string`. The key is the line up to a
//! tab, or up to `c` with `-tc`, or the whole line with a bare `-t`.
//!
//! `-x` wants the whole key to match; `-r endkey` prints every line from
//! `string` up to and including `endkey`; `-i` takes the strings from
//! standard input, one per line. Comparisons follow `-d` (only letters,
//! digits, blanks and tabs count), `-f` (fold case) and `-n` (compare
//! leading numbers). With no file, `/lib/words` is searched with `-df`.
//! The exit status is `not found` or `no dictionary` on failure.

use std::cmp::Ordering;
use std::io::{self, BufRead, Write};

use purgatorio::{Context, Error};

struct Look {
    dict: bool,
    fold: bool,
    numeric: bool,
    exact: bool,
    term: Option<char>,
}

/// A leading number: optional minus, digits, optional fraction.
fn num(s: &str) -> f64 {
    let s = s.trim_start();
    let end = s
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && c == '-')))
        .map_or(s.len(), |(i, _)| i);
    s[..end].parse().unwrap_or(0.0)
}

impl Look {
    fn canon(&self, s: &str) -> String {
        s.chars()
            .filter(|c| !self.dict || c.is_alphanumeric() || *c == ' ' || *c == '\t')
            .map(|c| {
                if self.fold {
                    c.to_lowercase().next().unwrap_or(c)
                } else {
                    c
                }
            })
            .collect()
    }

    fn key<'a>(&self, line: &'a str) -> &'a str {
        match self.term {
            Some(t) => line.split(t).next().unwrap_or(line),
            None => line,
        }
    }

    /// How a line's key compares with `want`: Equal when it matches (as a
    /// prefix unless `-x`).
    fn cmp(&self, key: &str, want: &str) -> Ordering {
        if self.numeric {
            return num(key).partial_cmp(&num(want)).unwrap_or(Ordering::Equal);
        }
        let k = self.canon(key);
        let w = self.canon(want);
        if !self.exact && k.starts_with(&w) {
            return Ordering::Equal;
        }
        k.cmp(&w)
    }
}

fn usage() -> Error {
    Error::from("usage: look -[dfinx] [-r endkey] [-t[c]] [string] [file]")
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut l = Look {
        dict: false,
        fold: false,
        numeric: false,
        exact: false,
        term: Some('\t'),
    };
    let (mut interactive, mut end, mut opts) = (false, None::<String>, false);
    let mut i = 1;
    while i < args.len() && args[i].starts_with('-') && args[i] != "-" {
        let w = args[i].clone();
        i += 1;
        let mut chars = w.chars().skip(1);
        while let Some(c) = chars.next() {
            opts = true;
            match c {
                'd' => l.dict = true,
                'f' => l.fold = true,
                'n' => l.numeric = true,
                'i' => interactive = true,
                'x' => l.exact = true,
                't' => {
                    l.term = chars.next();
                    break;
                }
                'r' => {
                    let rest: String = chars.by_ref().collect();
                    if rest.is_empty() {
                        end = Some(args.get(i).cloned().ok_or_else(usage)?);
                        i += 1;
                    } else {
                        end = Some(rest);
                    }
                }
                _ => return Err(usage()),
            }
        }
    }
    let mut rest = args[i..].to_vec();
    let mut strings: Vec<String> = Vec::new();
    if !interactive {
        if rest.is_empty() {
            return Err(usage());
        }
        strings.push(rest.remove(0));
    }
    let file = match rest.as_slice() {
        [] => {
            if !opts {
                l.dict = true;
                l.fold = true;
            }
            String::from("/lib/words")
        }
        [f] => f.clone(),
        _ => return Err(usage()),
    };
    let Ok(data) = std::fs::read(&file) else {
        eprintln!("look: can't open {file}");
        purgatorio::exits("fail:no dictionary");
    };
    let text = String::from_utf8_lossy(&data);
    if interactive {
        for line in io::stdin().lock().lines() {
            strings.push(line?);
        }
    }
    let mut out = io::BufWriter::new(io::stdout().lock());
    let mut found = false;
    for want in &strings {
        for line in text.lines() {
            let key = l.key(line);
            let hit = match &end {
                None => l.cmp(key, want) == Ordering::Equal,
                Some(e) => l.cmp(key, want) != Ordering::Less && l.cmp(key, e) != Ordering::Greater,
            };
            if hit {
                writeln!(out, "{line}")?;
                found = true;
            }
        }
    }
    out.flush()?;
    if !found {
        purgatorio::exits("fail:not found");
    }
    Ok(())
}
