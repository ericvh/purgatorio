//! Plan 9 attribute lists, as factotum's `ctl` and `rpc` use them:
//! `proto=ed25519 user=alice !seed=...`. Words are separated by spaces and
//! tabs; a value may be quoted with `'`, and `''` inside quotes is a quote.

use alloc::string::String;
use alloc::vec::Vec;

/// Splits `s` into words, removing quotes. `None` for an unbalanced quote.
pub fn tokenize(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = s.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    cur.push('\'');
                } else {
                    quoted = false;
                }
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if have {
                    out.push(core::mem::take(&mut cur));
                    have = false;
                }
            }
            '\'' => {
                quoted = true;
                have = true;
            }
            _ => {
                cur.push(c);
                have = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if have {
        out.push(cur);
    }
    Some(out)
}

/// Quotes `s` when it is empty or holds a space, tab, newline or quote.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && !s.contains([' ', '\t', '\n', '\r', '\'']) {
        return String::from(s);
    }
    let mut q = String::from("'");
    for c in s.chars() {
        if c == '\'' {
            q.push('\'');
        }
        q.push(c);
    }
    q.push('\'');
    q
}

/// An attribute list: `(name, value)` pairs in order. A word with no `=`
/// is a name with an empty value. A leading `!` stays part of the name.
pub fn parse(s: &str) -> Option<Vec<(String, String)>> {
    Some(
        tokenize(s)?
            .into_iter()
            .map(|w| match w.split_once('=') {
                Some((k, v)) => (String::from(k), String::from(v)),
                None => (w, String::new()),
            })
            .collect(),
    )
}

/// The value of `name` in `attrs`.
pub fn get<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_quotes() {
        assert_eq!(
            tokenize("a  'b c' 'it''s' ''").unwrap(),
            ["a", "b c", "it's", ""]
        );
        assert_eq!(tokenize("x='a b'").unwrap(), ["x=a b"]);
        assert!(tokenize("'open").is_none());
        for s in ["", "a b", "it's", "plain"] {
            assert_eq!(tokenize(&quote(s)).unwrap(), [s]);
        }
        assert_eq!(quote("plain"), "plain");
    }

    #[test]
    fn attributes() {
        let a = parse("proto=pass user=bob !password='a b' flag").unwrap();
        assert_eq!(get(&a, "proto"), Some("pass"));
        assert_eq!(get(&a, "!password"), Some("a b"));
        assert_eq!(get(&a, "flag"), Some(""));
        assert_eq!(get(&a, "server"), None);
    }
}
