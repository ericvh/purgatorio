//! The prog's environment group, through the files in `/env` (`#e`). Unlike
//! `std::env`, which is a snapshot taken when the prog started, these see
//! changes made by other progs sharing the group.
//!
//! sh keeps a list in a variable's file as its words separated by spaces,
//! quoted where a word needs it, as Inferno's sh does; [`quote_list`] and
//! [`unquote_list`] write and read that form, so other commands (mk) see
//! the same lists sh does.

use std::io::{Read, Write};

use crate::abi::mode;
use crate::{Error, create, open, remove};

fn path(name: &str) -> Result<String, Error> {
    if name.is_empty() || name.contains(['/', '\0']) || name == "." || name == ".." {
        return Err(Error::from(crate::abi::error::EBADARG));
    }
    Ok(format!("/env/{name}"))
}

/// The value of `name`, or `None` if it is unset.
pub fn get(name: &str) -> Option<String> {
    let fd = open(&path(name).ok()?, mode::OREAD).ok()?;
    let mut s = Vec::new();
    std::fs::File::from(fd).read_to_end(&mut s).ok()?;
    Some(String::from_utf8_lossy(&s).into_owned())
}

/// Sets `name` to `value`.
pub fn set(name: &str, value: &str) -> Result<(), Error> {
    let fd = create(&path(name)?, mode::OWRITE, 0o666)?;
    std::fs::File::from(fd).write_all(value.as_bytes())?;
    Ok(())
}

/// Removes `name`; it is not an error if it was unset.
pub fn unset(name: &str) -> Result<(), Error> {
    match remove(&path(name)?) {
        Err(e) if e.0 == crate::abi::error::ENONEXIST => Ok(()),
        r => r,
    }
}

/// Every variable, sorted by name.
pub fn list() -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/env")? {
        let name = e?.file_name().to_string_lossy().into_owned();
        if let Some(v) = get(&name) {
            out.push((name, v));
        }
    }
    out.sort();
    Ok(out)
}

/// The blanks that separate the words of a list: space, tab, CR and NL.
fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// A list as its `/env` file holds it: its words separated by spaces, each
/// in `'quotes'` (with `''` for a quote) when it is empty or holds a blank
/// or a quote, so [`unquote_list`] gives the list back. Other words are
/// written as they are, so a program that reads a plain value such as
/// `/env/home` sees no quotes.
pub fn quote_list(words: &[String]) -> String {
    let needs = |w: &str| w.is_empty() || w.contains(|c| is_blank(c) || c == '\'');
    let words: Vec<String> = words
        .iter()
        .map(|w| match needs(w) {
            true => format!("'{}'", w.replace('\'', "''")),
            false => w.clone(),
        })
        .collect();
    words.join(" ")
}

/// Splits a variable's value into its words at blanks, honouring `'quotes'`
/// as [`quote_list`] writes them (and as sh's `${unquote}` reads them).
pub fn unquote_list(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut c = s.chars().peekable();
    loop {
        while c.next_if(|&c| is_blank(c)).is_some() {}
        if c.peek().is_none() {
            return words;
        }
        let mut w = String::new();
        while let Some(ch) = c.next_if(|&c| !is_blank(c)) {
            if ch != '\'' {
                w.push(ch);
                continue;
            }
            loop {
                match c.next() {
                    None => break,
                    Some('\'') if c.next_if_eq(&'\'').is_some() => w.push('\''),
                    Some('\'') => break,
                    Some(ch) => w.push(ch),
                }
            }
        }
        words.push(w);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| String::from(*s)).collect()
    }

    #[test]
    fn lists_round_trip() {
        for l in [
            v(&["a"]),
            v(&["a", "b c"]),
            v(&["a b"]),
            v(&[""]),
            v(&["", ""]),
            v(&["a", ""]),
            v(&["it's", "x\ny", "$x", "{echo}"]),
        ] {
            assert_eq!(unquote_list(&quote_list(&l)), l, "{l:?}");
        }
        assert_eq!(quote_list(&v(&["/usr/inferno"])), "/usr/inferno");
        assert_eq!(quote_list(&v(&["a", "b c"])), "a 'b c'");
        assert_eq!(unquote_list("hello"), ["hello"]);
        assert!(unquote_list("").is_empty());
        assert_eq!(
            unquote_list("a 'b c' '' 'it''s'x"),
            ["a", "b c", "", "it'sx"]
        );
        assert_eq!(unquote_list("a\u{a0}b\tc"), ["a\u{a0}b", "c"]);
    }
}
