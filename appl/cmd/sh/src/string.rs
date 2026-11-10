//! `load string`: string(2) operations as builtins, after sh-string(1).
//! `prefix` and `in` are commands; the rest are `${}` substitutions that
//! yield a two-element list where string(2) returns a pair. Every builtin
//! takes exactly the arguments its usage shows, or fails with `usage`.

use purgatorio::Error;

/// The `${}` builtins this module adds.
pub fn is_subst(name: &str) -> bool {
    matches!(
        name,
        "splitl"
            | "splitr"
            | "drop"
            | "take"
            | "splitstrl"
            | "splitstrr"
            | "tolower"
            | "toupper"
            | "len"
            | "alen"
            | "slice"
            | "fields"
            | "padl"
            | "padr"
    )
}

/// The commands this module adds.
pub fn is_cmd(name: &str) -> bool {
    matches!(name, "prefix" | "in")
}

fn usage(form: &str) -> Error {
    Error(format!("usage: {form}"))
}

/// The arguments after the name, if there are exactly `n`.
fn args<'a>(argv: &'a [String], n: usize, form: &str) -> Result<&'a [String], Error> {
    if argv.len() == n + 1 {
        Ok(&argv[1..])
    } else {
        Err(usage(form))
    }
}

/// Runs `prefix` or `in`, returning the status.
pub fn cmd(argv: &[String]) -> Result<String, Error> {
    let yes = match argv[0].as_str() {
        "prefix" => {
            let a = args(argv, 2, "prefix pre s")?;
            a[1].starts_with(a[0].as_str())
        }
        _ => {
            let a = args(argv, 2, "in c cl")?;
            a[0].chars().next().is_some_and(|c| in_class(c, &a[1]))
        }
    };
    Ok(if yes {
        String::new()
    } else {
        String::from("false")
    })
}

/// string(2)'s `in`: whether `c` is in class `cl`. A class lists
/// characters and `a-z` ranges; a leading `^` negates it.
pub fn in_class(c: char, cl: &str) -> bool {
    let mut cs: Vec<char> = cl.chars().collect();
    let negate = cs.len() > 1 && cs[0] == '^';
    if negate {
        cs.remove(0);
    }
    let mut found = false;
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '-' && i > 0 && i + 1 < cs.len() {
            if cs[i - 1] <= c && c <= cs[i + 1] {
                found = true;
                break;
            }
            i += 2;
            continue;
        }
        if cs[i] == c {
            found = true;
            break;
        }
        i += 1;
    }
    found != negate
}

/// Byte offset of the first character of `s` in class `cl`.
fn first_in(s: &str, cl: &str) -> Option<usize> {
    s.char_indices()
        .find(|&(_, c)| in_class(c, cl))
        .map(|t| t.0)
}

/// Byte offset of the first character of `s` not in class `cl`.
fn first_out(s: &str, cl: &str) -> usize {
    s.char_indices()
        .find(|&(_, c)| !in_class(c, cl))
        .map_or(s.len(), |t| t.0)
}

fn pair(a: &str, b: &str) -> Vec<String> {
    vec![String::from(a), String::from(b)]
}

/// A non-negative decimal number, or `None`.
fn number(s: &str) -> Option<usize> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(s.parse().unwrap_or(usize::MAX))
}

/// Runs one `${}` builtin.
pub fn subst(argv: &[String]) -> Result<Vec<String>, Error> {
    let name = argv[0].as_str();
    Ok(match name {
        "splitl" => {
            let a = args(argv, 2, "splitl s cl")?;
            let i = first_in(&a[0], &a[1]).unwrap_or(a[0].len());
            pair(&a[0][..i], &a[0][i..])
        }
        "splitr" => {
            let a = args(argv, 2, "splitr s cl")?;
            let i = a[0]
                .char_indices()
                .rev()
                .find(|&(_, c)| in_class(c, &a[1]))
                .map_or(0, |(i, c)| i + c.len_utf8());
            pair(&a[0][..i], &a[0][i..])
        }
        "drop" => {
            let a = args(argv, 2, "drop s cl")?;
            vec![String::from(&a[0][first_out(&a[0], &a[1])..])]
        }
        "take" => {
            let a = args(argv, 2, "take s cl")?;
            vec![String::from(&a[0][..first_out(&a[0], &a[1])])]
        }
        "splitstrl" => {
            let a = args(argv, 2, "splitstrl s t")?;
            let i = a[0].find(a[1].as_str()).unwrap_or(a[0].len());
            pair(&a[0][..i], &a[0][i..])
        }
        "splitstrr" => {
            let a = args(argv, 2, "splitstrr s t")?;
            let i = a[0].rfind(a[1].as_str()).map_or(0, |i| i + a[1].len());
            pair(&a[0][..i], &a[0][i..])
        }
        "tolower" => vec![args(argv, 1, "tolower s")?[0].to_lowercase()],
        "toupper" => vec![args(argv, 1, "toupper s")?[0].to_uppercase()],
        "len" => vec![args(argv, 1, "len s")?[0].chars().count().to_string()],
        "alen" => vec![args(argv, 1, "alen s")?[0].len().to_string()],
        "slice" => {
            const FORM: &str = "slice start end s";
            let a = args(argv, 3, FORM)?;
            let chars: Vec<char> = a[2].chars().collect();
            let start = number(&a[0]).ok_or_else(|| usage(FORM))?;
            let end = if a[1] == "end" {
                chars.len()
            } else {
                number(&a[1]).ok_or_else(|| usage(FORM))?
            };
            let end = end.min(chars.len());
            let start = start.min(end);
            vec![chars[start..end].iter().collect()]
        }
        "fields" => {
            let a = args(argv, 2, "fields cl s")?;
            a[1].split(|c| in_class(c, &a[0]))
                .map(String::from)
                .collect()
        }
        _ => {
            // padl and padr.
            let form = format!("{name} n [s ...]");
            let n = argv
                .get(1)
                .and_then(|n| number(n))
                .ok_or_else(|| usage(&form))?;
            let s = argv[2..].join(" ");
            let fill = " ".repeat(n.saturating_sub(s.chars().count()));
            // As sh-string(1) has it, padl puts the spaces on the right
            // and padr on the left.
            if name == "padl" {
                vec![s + &fill]
            } else {
                vec![fill + &s]
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Vec<String>, Error> {
        let argv: Vec<String> = args.iter().map(|s| String::from(*s)).collect();
        subst(&argv)
    }

    #[test]
    fn classes() {
        assert!(in_class('c', "a-z"));
        assert!(!in_class('C', "a-z"));
        assert!(in_class('C', "^a-z"));
        assert!(in_class('-', "a-"));
        assert!(in_class('^', "^"));
        assert!(!in_class('x', ""));
    }

    #[test]
    fn splits() {
        assert_eq!(run(&["splitl", "a.b.c", "."]).unwrap(), ["a", ".b.c"]);
        assert_eq!(run(&["splitr", "a.b.c", "."]).unwrap(), ["a.b.", "c"]);
        assert_eq!(run(&["splitr", "abc", "."]).unwrap(), ["", "abc"]);
        assert_eq!(
            run(&["splitstrl", "a::b::c", "::"]).unwrap(),
            ["a", "::b::c"]
        );
        assert_eq!(
            run(&["splitstrr", "a::b::c", "::"]).unwrap(),
            ["a::b::", "c"]
        );
        assert_eq!(run(&["fields", ":", "a::b"]).unwrap(), ["a", "", "b"]);
    }

    #[test]
    fn slices_and_pads() {
        assert_eq!(run(&["slice", "1", "3", "héllo"]).unwrap(), ["él"]);
        assert_eq!(run(&["slice", "3", "end", "hello"]).unwrap(), ["lo"]);
        assert_eq!(run(&["slice", "4", "2", "hello"]).unwrap(), [""]);
        assert_eq!(run(&["slice", "1", "99", "hey"]).unwrap(), ["ey"]);
        assert!(run(&["slice", "-1", "2", "hey"]).is_err());
        assert_eq!(run(&["padl", "4", "a"]).unwrap(), ["a   "]);
        assert_eq!(run(&["padr", "4", "a", "b"]).unwrap(), [" a b"]);
        assert_eq!(run(&["padr", "1", "abc"]).unwrap(), ["abc"]);
        assert!(run(&["len"]).is_err());
        assert!(run(&["len", "a", "b"]).is_err());
    }
}
