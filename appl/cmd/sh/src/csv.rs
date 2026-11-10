//! `load csv`: comma-separated values, after sh-csv(1). An item holding a
//! comma, a newline or a double quote is quoted with double quotes, and a
//! double quote inside one is doubled.

use crate::input::Input;

/// One item, quoted if it needs it.
fn quote(s: &str) -> String {
    if !s.contains([',', '\n', '"']) {
        return String::from(s);
    }
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `${csv list}`: the items as one comma-separated line.
pub fn join(items: &[String]) -> String {
    let q: Vec<String> = items.iter().map(|s| quote(s)).collect();
    q.join(",")
}

/// Reads one item. Returns the item, or `None` at end of file before any
/// byte, and whether the input ended. The `,` or newline after the item
/// is left unread.
fn item(inp: &mut Input) -> (Option<Vec<u8>>, bool) {
    let mut s = Vec::new();
    let mut quoted = false;
    match inp.get() {
        None => return (None, true),
        Some(b'"') => quoted = true,
        Some(b',' | b'\n') => {
            inp.unget();
            return (Some(s), false);
        }
        Some(c) => s.push(c),
    }
    loop {
        let Some(c) = inp.get() else {
            return (Some(s), true);
        };
        if quoted {
            if c != b'"' {
                s.push(c);
                continue;
            }
            match inp.get() {
                None => return (Some(s), true),
                Some(b'"') => s.push(b'"'),
                Some(b',' | b'\n') => {
                    inp.unget();
                    return (Some(s), false);
                }
                // Text after a closing quote: take it as more of the item.
                Some(_) => {
                    inp.unget();
                    quoted = false;
                }
            }
        } else {
            match c {
                b',' | b'\n' => {
                    inp.unget();
                    return (Some(s), false);
                }
                b'"' => quoted = true,
                _ => s.push(c),
            }
        }
    }
}

/// Reads one line of items, or `None` at end of file. An empty line is
/// one empty item.
pub fn line(inp: &mut Input) -> Option<Vec<String>> {
    let mut items = Vec::new();
    loop {
        let (s, eof) = item(inp);
        if items.is_empty() && s.is_none() && eof {
            return None;
        }
        items.push(String::from_utf8_lossy(&s.unwrap_or_default()).into_owned());
        if eof || inp.get() != Some(b',') {
            return Some(items);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<Vec<String>> {
        let mut inp = Input::bytes(s.as_bytes());
        std::iter::from_fn(|| line(&mut inp)).collect()
    }

    #[test]
    fn parse_and_join() {
        assert_eq!(
            lines("a,b\n\"c,d\",\"e\"\"f\"\n\n,x"),
            vec![vec!["a", "b"], vec!["c,d", "e\"f"], vec![""], vec!["", "x"]]
        );
        assert_eq!(lines("\"a\nb\",c\n"), vec![vec!["a\nb", "c"]]);
        assert_eq!(lines("a,"), vec![vec!["a", ""]]);
        let items: Vec<String> = ["a", "b,c", "say \"hi\"", ""]
            .iter()
            .map(|s| String::from(*s))
            .collect();
        assert_eq!(join(&items), "a,\"b,c\",\"say \"\"hi\"\"\",");
        assert_eq!(lines(&join(&items)), vec![items]);
    }
}
