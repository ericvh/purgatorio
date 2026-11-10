//! sed 's/re/new/[g]' [file...]: a small substitute command. The regular
//! expression has the syntax of regexp(6); in `new`, `&` is the match,
//! `\1` to `\9` are subexpressions and `\n` is a newline.

use std::io::{BufRead, Write};

use purgatorio::{Context, Error};
use regexp::Regex;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let script = args
        .get(1)
        .ok_or_else(|| Error::from("usage: sed script [file...]"))?;
    let (old, new, global) = parse(script)?;
    let re = Regex::new(&old).map_err(|e| Error(format!("{old}: {e}")))?;
    let files = &args[2..];
    let run = |input: &mut dyn BufRead| -> Result<(), Error> {
        let mut out = std::io::stdout().lock();
        let mut line = String::new();
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 {
                break;
            }
            let nl = line.ends_with('\n');
            let body = if nl {
                &line[..line.len() - 1]
            } else {
                line.as_str()
            };
            let replaced = subst(body, &re, &new, global);
            out.write_all(replaced.as_bytes())?;
            if nl {
                out.write_all(b"\n")?;
            }
        }
        Ok(())
    };
    if files.is_empty() {
        run(&mut std::io::stdin().lock())
    } else {
        for f in files {
            let file = std::fs::File::open(f).map_err(|_| Error::last())?;
            run(&mut std::io::BufReader::new(file))?;
        }
        Ok(())
    }
}

/// Splits `s<d>re<d>new<d>flags`; `\\<d>` stands for a literal delimiter.
fn parse(script: &str) -> Result<(String, String, bool), Error> {
    let mut c = script.chars();
    if c.next() != Some('s') {
        return Err(Error::from("only s/// scripts"));
    }
    let delim = c.next().ok_or_else(|| Error::from("only s/// scripts"))?;
    let mut parts = vec![String::new()];
    while let Some(ch) = c.next() {
        let more = parts.len() < 3;
        let cur = parts.last_mut().expect("parts is never empty");
        if ch == '\\' {
            match c.next() {
                Some(n) if n == delim => cur.push(n),
                Some(n) => {
                    cur.push('\\');
                    cur.push(n);
                }
                None => cur.push('\\'),
            }
        } else if ch == delim && more {
            parts.push(String::new());
        } else {
            cur.push(ch);
        }
    }
    if parts.len() < 2 {
        return Err(Error(format!("unterminated s command: {script}")));
    }
    let flags = parts.get(2).map_or("", String::as_str);
    Ok((parts[0].clone(), parts[1].clone(), flags.contains('g')))
}

/// Replaces the first match of `re` in `line`, or every match if `global`.
fn subst(line: &str, re: &Regex, new: &str, global: bool) -> String {
    let mut out = String::new();
    let mut at = 0;
    while at <= line.len() {
        let Some(caps) = re.find_at(line, at) else {
            break;
        };
        let (s, e) = caps[0].expect("a match has a whole-match range");
        out.push_str(&line[at..s]);
        expand(&mut out, line, &caps, new);
        at = e;
        if !global {
            break;
        }
        if s == e {
            // An empty match: copy one character so the scan moves on.
            match line[e..].chars().next() {
                Some(ch) => {
                    out.push(ch);
                    at += ch.len_utf8();
                }
                None => {
                    at += 1;
                    break;
                }
            }
        }
    }
    if at <= line.len() {
        out.push_str(&line[at..]);
    }
    out
}

/// Appends the replacement text `new` for one match.
fn expand(out: &mut String, line: &str, caps: &regexp::Captures, new: &str) {
    let mut c = new.chars();
    while let Some(ch) = c.next() {
        match ch {
            '&' => {
                if let Some((a, b)) = caps[0] {
                    out.push_str(&line[a..b]);
                }
            }
            '\\' => match c.next() {
                Some(d @ '1'..='9') => {
                    let n = d as usize - '0' as usize;
                    if let Some(Some((a, b))) = caps.get(n) {
                        out.push_str(&line[*a..*b]);
                    }
                }
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            ch => out.push(ch),
        }
    }
}
