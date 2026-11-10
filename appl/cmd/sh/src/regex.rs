//! `load regex`: the `match` command and `${re}` substitution, after
//! sh-regex(1), with regexp(6) syntax from the shared `regexp` crate.

use purgatorio::Error;
use regexp::{Captures, Regex};

const RE_USAGE: &str = "usage: re (g|v|s|sg|m|mg|M) arg...";

fn compile(pat: &str) -> Result<Regex, Error> {
    Regex::new(pat).map_err(|e| Error(format!("bad regex \"{pat}\": {e}")))
}

/// `match regex [arg ...]`: fails with `no match` unless every `arg`
/// matches.
pub fn match_cmd(argv: &[String]) -> Result<String, Error> {
    let Some(pat) = argv.get(1) else {
        return Err(Error::from("usage: match regexp [arg...]"));
    };
    let re = compile(pat)?;
    Ok(if argv[2..].iter().all(|a| re.is_match(a)) {
        String::new()
    } else {
        String::from("no match")
    })
}

fn text(s: &str, range: Option<(usize, usize)>) -> &str {
    range.map_or("", |(a, b)| &s[a..b])
}

/// Appends the substitution `subs` for one match: `\d` is subexpression
/// `d` (`\0` the whole match), and `\` quotes any other character.
fn expand(out: &mut String, s: &str, caps: &Captures, subs: &str) {
    let mut c = subs.chars();
    while let Some(ch) = c.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match c.next() {
            Some(d @ '0'..='9') => {
                let n = d as usize - '0' as usize;
                out.push_str(text(s, caps.get(n).copied().flatten()));
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
}

/// Replaces the first match of `re` in `s`, or all of them if `global`.
/// After a null match nothing more is replaced.
fn replace(re: &Regex, s: &str, subs: &str, global: bool) -> String {
    let mut out = String::new();
    let mut at = 0;
    while let Some(caps) = re.find_at(s, at) {
        let (a, b) = caps[0].expect("a match has a whole-match range");
        out.push_str(&s[at..a]);
        expand(&mut out, s, &caps, subs);
        at = b;
        if !global || a == b {
            break;
        }
    }
    out.push_str(&s[at..]);
    out
}

/// `${re op regex ...}`. `argv` starts with `re`.
pub fn re(argv: &[String]) -> Result<Vec<String>, Error> {
    let Some(op) = argv.get(1) else {
        return Err(Error::from(RE_USAGE));
    };
    let op = op.as_str();
    let form = match op {
        "g" | "v" => "regex [arg...]",
        "s" | "sg" => "regex subs [arg...]",
        "m" | "M" | "mg" => "regex arg",
        _ => return Err(Error::from(RE_USAGE)),
    };
    let usage = || Error(format!("usage: re {op} {form}"));
    let rest = &argv[2..];
    let Some(pat) = rest.first() else {
        return Err(usage());
    };
    let args = &rest[1..];
    if matches!(op, "m" | "M" | "mg") && args.len() != 1 {
        return Err(usage());
    }
    if matches!(op, "s" | "sg") && args.is_empty() {
        return Err(usage());
    }
    let re = compile(pat)?;
    Ok(match op {
        "g" | "v" => args
            .iter()
            .filter(|a| re.is_match(a) == (op == "g"))
            .cloned()
            .collect(),
        "s" | "sg" => args[1..]
            .iter()
            .map(|a| replace(&re, a, &args[0], op == "sg"))
            .collect(),
        "m" | "M" => {
            let s = &args[0];
            match re.find(s) {
                None => Vec::new(),
                Some(caps) if op == "m" => vec![String::from(text(s, caps[0]))],
                Some(caps) => caps.iter().map(|&c| String::from(text(s, c))).collect(),
            }
        }
        _ => {
            // mg
            let s = &args[0];
            let mut out = Vec::new();
            let mut at = 0;
            while let Some(caps) = re.find_at(s, at) {
                let (a, b) = caps[0].expect("a match has a whole-match range");
                out.push(String::from(&s[a..b]));
                if a == b {
                    break;
                }
                at = b;
            }
            out
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Vec<String>, Error> {
        let argv: Vec<String> = args.iter().map(|s| String::from(*s)).collect();
        re(&argv)
    }

    #[test]
    fn ops() {
        assert_eq!(
            run(&["re", "g", "^a", "ab", "ba", "ac"]).unwrap(),
            ["ab", "ac"]
        );
        assert_eq!(run(&["re", "v", "^a", "ab", "ba"]).unwrap(), ["ba"]);
        assert_eq!(run(&["re", "m", "b+", "abbbc"]).unwrap(), ["bbb"]);
        assert!(run(&["re", "m", "x", "abc"]).unwrap().is_empty());
        assert_eq!(
            run(&["re", "M", "(a)(x)?(c)", "zac"]).unwrap(),
            ["ac", "a", "", "c"]
        );
        assert_eq!(
            run(&["re", "mg", "[0-9]+", "a1b22c333"]).unwrap(),
            ["1", "22", "333"]
        );
        assert_eq!(run(&["re", "mg", "x*", "ab"]).unwrap(), [""]);
        assert_eq!(
            run(&["re", "s", "(o+)", "<\\1>", "foo", "boo", "x"]).unwrap(),
            ["f<oo>", "b<oo>", "x"]
        );
        assert_eq!(run(&["re", "sg", "o", "0", "foo"]).unwrap(), ["f00"]);
        assert_eq!(run(&["re", "sg", "x*", "-", "ab"]).unwrap(), ["-ab"]);
        assert_eq!(
            run(&["re", "s", "b", "[\\0\\\\]", "abc"]).unwrap(),
            ["a[b\\]c"]
        );
    }

    #[test]
    fn usage_errors() {
        assert_eq!(run(&["re"]).unwrap_err().0, RE_USAGE);
        assert_eq!(run(&["re", "q", "a"]).unwrap_err().0, RE_USAGE);
        assert_eq!(
            run(&["re", "m", "a"]).unwrap_err().0,
            "usage: re m regex arg"
        );
        assert_eq!(
            run(&["re", "s", "a"]).unwrap_err().0,
            "usage: re s regex subs [arg...]"
        );
        assert_eq!(
            run(&["re", "g", "("]).unwrap_err().0,
            "bad regex \"(\": unmatched left paren"
        );
    }
}
