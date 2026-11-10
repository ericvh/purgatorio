//! `load sexprs`: S-expressions as sexprs(6) defines them, after
//! sh-sexprs(1). The reader takes canonical and advanced forms and any
//! mixture; [`Sexp::text`] writes the advanced form on one line.
//!
//! Atoms are bytes. Inferno's atoms are Limbo strings, so non-ASCII text
//! comes out as `\x` escapes of its bytes here, which read back the same.

use crate::exec::quote as shquote;
use crate::input::Input;
use crate::num::{b64_dec, b64_enc};
use crate::shmods::Fail;

/// The longest byte count a raw or quoted string may claim.
const MAX_TOKEN: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum Sexp {
    Atom {
        data: Vec<u8>,
        hint: Option<Vec<u8>>,
        /// Written in hex or base64 rather than as a token or string.
        binary: bool,
    },
    List(Vec<Sexp>),
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n')
}

fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"-./_:*+=".contains(&c)
}

fn is_token(s: &[u8]) -> bool {
    !s.is_empty() && !s[0].is_ascii_digit() && s.iter().all(|&c| is_token_char(c))
}

/// Printable ASCII and white space only.
fn is_textual(s: &[u8]) -> bool {
    s.iter().all(|&c| (b' '..0x7f).contains(&c) || is_space(c))
}

/// An atom, binary unless its bytes are textual.
fn atom(data: Vec<u8>, hint: Option<Vec<u8>>) -> Sexp {
    let binary = !is_textual(&data);
    Sexp::Atom { data, hint, binary }
}

/// A string atom, as `${mktext}` makes one.
pub fn text_atom(s: &str) -> Sexp {
    Sexp::Atom {
        data: s.as_bytes().to_vec(),
        hint: None,
        binary: false,
    }
}

/// A token as is, anything else as a quoted string with escapes.
fn quote(s: &[u8]) -> String {
    if is_token(s) {
        return String::from_utf8_lossy(s).into_owned();
    }
    let mut out = String::from("\"");
    for &c in s {
        match c {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            8 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            0x0b => out.push_str("\\v"),
            c if !(b' '..0x7f).contains(&c) => out.push_str(&format!("\\x{c:02x}")),
            c => out.push(c as char),
        }
    }
    out.push('"');
    out
}

fn hex_enc(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02X}")).collect()
}

fn hex_dec(s: &[u8]) -> Vec<u8> {
    let digits: Vec<u8> = s
        .iter()
        .filter_map(|&c| (c as char).to_digit(16).map(|d| d as u8))
        .collect();
    digits
        .chunks(2)
        .map(|p| p[0] << 4 | p.get(1).unwrap_or(&0))
        .collect()
}

impl Sexp {
    /// The advanced form, on one line.
    pub fn text(&self) -> String {
        match self {
            Sexp::Atom { data, hint, binary } => {
                let h = hint
                    .as_ref()
                    .map_or(String::new(), |h| format!("[{}]", quote(h)));
                if !binary {
                    format!("{h}{}", quote(data))
                } else if data.len() <= 4 {
                    format!("{h}#{}#", hex_enc(data))
                } else {
                    format!("{h}|{}|", b64_enc(data))
                }
            }
            Sexp::List(l) => {
                let els: Vec<String> = l.iter().map(Sexp::text).collect();
                format!("({})", els.join(" "))
            }
        }
    }

    /// The canonical form.
    pub fn pack(&self, out: &mut Vec<u8>) {
        match self {
            Sexp::Atom { data, hint, .. } => {
                if let Some(h) = hint {
                    out.extend_from_slice(format!("[{}:", h.len()).as_bytes());
                    out.extend_from_slice(h);
                    out.push(b']');
                }
                out.extend_from_slice(format!("{}:", data.len()).as_bytes());
                out.extend_from_slice(data);
            }
            Sexp::List(l) => {
                out.push(b'(');
                for e in l {
                    e.pack(out);
                }
                out.push(b')');
            }
        }
    }

    /// An atom's value as text; empty for a list.
    pub fn astext(&self) -> String {
        match self {
            Sexp::Atom { data, .. } => String::from_utf8_lossy(data).into_owned(),
            Sexp::List(_) => String::new(),
        }
    }

    pub fn els(&self) -> &[Sexp] {
        match self {
            Sexp::List(l) => l,
            Sexp::Atom { .. } => &[],
        }
    }
}

/// A syntax error and the offset it was found at.
type Syntax = (String, usize);

fn syntax<T>(msg: &str, at: usize) -> Result<T, Syntax> {
    Err((String::from(msg), at))
}

/// Skips white space and returns the next byte.
fn ws(inp: &mut Input) -> Option<u8> {
    loop {
        match inp.get() {
            Some(c) if is_space(c) => {}
            c => return c,
        }
    }
}

/// The bytes up to `end`, which is consumed.
fn to_closing(inp: &mut Input, end: u8) -> Result<Vec<u8>, Syntax> {
    let p0 = inp.offset();
    let mut s = Vec::new();
    loop {
        match inp.get() {
            None => return syntax(&format!("missing closing '{}'", end as char), p0),
            Some(c) if c == end => return Ok(s),
            Some(c) => s.push(c),
        }
    }
}

/// The body of a quoted string, after the opening `"`.
fn unquote(inp: &mut Input) -> Result<Vec<u8>, Syntax> {
    let p0 = inp.offset();
    let mut s = Vec::new();
    loop {
        let Some(mut c) = inp.get() else {
            return syntax("unclosed quoted string", p0);
        };
        if c == b'"' {
            return Ok(s);
        }
        if c == b'\\' {
            let e0 = inp.offset();
            let Some(e) = inp.get() else {
                return syntax("unclosed quoted string", p0);
            };
            c = match e {
                b'\r' | b'\n' => {
                    // A line continuation, of either order of CR and LF.
                    let other = if e == b'\r' { b'\n' } else { b'\r' };
                    match inp.get() {
                        Some(n) if n == other => {}
                        Some(_) => inp.unget(),
                        None => {}
                    }
                    continue;
                }
                b'b' => 8,
                b'f' => 0x0c,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'v' => 0x0b,
                b'0'..=b'7' => {
                    let mut v = (e - b'0') as u32;
                    for _ in 0..2 {
                        match inp.get() {
                            Some(d @ b'0'..=b'7') => v = v << 3 | (d - b'0') as u32,
                            _ => return syntax("illegal octal escape", e0),
                        }
                    }
                    (v & 0xff) as u8
                }
                b'x' => {
                    let hi = inp.get().and_then(|c| (c as char).to_digit(16));
                    let lo = inp.get().and_then(|c| (c as char).to_digit(16));
                    match (hi, lo) {
                        (Some(h), Some(l)) => (h << 4 | l) as u8,
                        _ => return syntax("illegal hex escape", e0),
                    }
                }
                other => other,
            };
        }
        s.push(c);
    }
}

/// A simple string starting with byte `c`.
fn simple(inp: &mut Input, c: u8, hint: Option<Vec<u8>>) -> Result<Sexp, Syntax> {
    let mut c = Some(c);
    let mut count: Option<usize> = None;
    while let Some(d @ b'0'..=b'9') = c {
        let n = count
            .unwrap_or(0)
            .saturating_mul(10)
            .saturating_add((d - b'0') as usize);
        if n > MAX_TOKEN {
            return syntax("implausible token length", inp.offset());
        }
        count = Some(n);
        c = inp.get();
    }
    match c {
        Some(b'"') => {
            let s = unquote(inp)?;
            Ok(Sexp::Atom {
                data: s,
                hint,
                binary: false,
            })
        }
        Some(b'|') => Ok(atom(
            b64_dec(&String::from_utf8_lossy(&to_closing(inp, b'|')?)),
            hint,
        )),
        Some(b'#') => Ok(atom(hex_dec(&to_closing(inp, b'#')?), hint)),
        Some(b':') if count.is_some() => {
            let mut a = Vec::new();
            for _ in 0..count.unwrap_or(0) {
                match inp.get() {
                    Some(b) => a.push(b),
                    None => return syntax("missing bytes in raw token", inp.offset()),
                }
            }
            Ok(atom(a, hint))
        }
        _ if count.is_some() => syntax("token can't start with a digit", inp.offset()),
        _ => {
            let mut s = Vec::new();
            while let Some(ch) = c.filter(|&ch| is_token_char(ch)) {
                s.push(ch);
                c = inp.get();
            }
            if s.is_empty() {
                return syntax("missing token", inp.offset());
            }
            if c.is_some() {
                inp.unget();
            }
            Ok(Sexp::Atom {
                data: s,
                hint,
                binary: false,
            })
        }
    }
}

/// One expression, or `None` at end of input.
fn item(inp: &mut Input) -> Result<Option<Sexp>, Syntax> {
    let p0 = inp.offset();
    let Some(c) = ws(inp) else {
        return Ok(None);
    };
    match c {
        b'{' => {
            let enc = to_closing(inp, b'}')?;
            let mut inner = Input::bytes(&b64_dec(&String::from_utf8_lossy(&enc)));
            item(&mut inner)
        }
        b'(' => {
            let mut l = Vec::new();
            loop {
                match ws(inp) {
                    None => return syntax("unclosed '('", p0),
                    Some(b')') => return Ok(Some(Sexp::List(l))),
                    Some(_) => inp.unget(),
                }
                match item(inp)? {
                    Some(e) => l.push(e),
                    None => return syntax("unclosed '('", p0),
                }
            }
        }
        b'[' => {
            let Some(h) = ws(inp) else {
                return syntax("missing ] in display hint", p0);
            };
            let hint = simple(inp, h, None)?;
            match ws(inp) {
                Some(b']') => {}
                Some(_) => {
                    inp.unget();
                    return syntax("missing ] in display hint", p0);
                }
                None => return syntax("missing ] in display hint", p0),
            }
            let Sexp::Atom {
                data,
                binary: false,
                ..
            } = hint
            else {
                return syntax("illegal display hint", inp.offset());
            };
            let Some(c) = ws(inp) else {
                return syntax("missing token", inp.offset());
            };
            simple(inp, c, Some(data)).map(Some)
        }
        c => simple(inp, c, None).map(Some),
    }
}

/// Reads one expression from `inp`: `Ok(None)` at end of input.
pub fn read(inp: &mut Input) -> Result<Option<Sexp>, String> {
    item(inp).map_err(|(msg, at)| format!("{msg} at offset {at}"))
}

/// Parses `s`, which must hold one expression and nothing but white space
/// after it. An empty string is `None`.
pub fn parse(s: &str) -> Result<Option<Sexp>, Fail> {
    let mut inp = Input::bytes(s.as_bytes());
    let e = read(&mut inp).map_err(|e| Fail::raise("bad sexp", &e))?;
    if !inp.rest().iter().all(|&c| is_space(c)) {
        return Err(Fail::raise(
            "bad sexp",
            &format!("extra text found at end of s-expression {}", shquote(s)),
        ));
    }
    Ok(e)
}

fn one<'a>(argv: &'a [String], form: &str) -> Result<&'a str, Fail> {
    match argv {
        [_, s] => Ok(s),
        _ => Err(Fail::raise("usage", &format!("usage: {form}"))),
    }
}

/// `islist sexp`.
pub fn islist(argv: &[String]) -> Result<String, Fail> {
    let s = one(argv, "islist sexp")?;
    if s.starts_with('(') || matches!(parse(s)?, Some(Sexp::List(_))) {
        Ok(String::new())
    } else {
        Ok(String::from("not a list"))
    }
}

/// The `${}` builtins.
pub fn subst(argv: &[String]) -> Result<Vec<String>, Fail> {
    let name = argv[0].as_str();
    Ok(match name {
        "els" | "textels" => {
            let e = parse(one(argv, &format!("{name} sexp"))?)?;
            let els = e.as_ref().map_or(&[][..], Sexp::els);
            if name == "els" {
                els.iter().map(Sexp::text).collect()
            } else {
                els.iter().map(Sexp::astext).collect()
            }
        }
        "text" => vec![parse(one(argv, "text sexp")?)?.map_or(String::new(), |e| e.astext())],
        "b64" => {
            let e = parse(one(argv, "b64 sexp")?)?;
            let mut packed = Vec::new();
            if let Some(e) = e {
                e.pack(&mut packed);
            }
            vec![format!("{{{}}}", b64_enc(&packed))]
        }
        "mktext" => vec![text_atom(one(argv, "mktext sexp")?).text()],
        "mklist" => vec![format!("({})", argv[1..].join(" "))],
        _ => {
            // mktextlist
            let els: Vec<String> = argv[1..].iter().map(|s| text_atom(s).text()).collect();
            vec![format!("({})", els.join(" "))]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(s: &str) -> String {
        parse(s).unwrap().unwrap().text()
    }

    #[test]
    fn forms() {
        assert_eq!(
            rt("(snicker \"abc\" (#03# |YWJj|))"),
            "(snicker abc (#03# abc))"
        );
        assert_eq!(
            rt("(12:hello world!(5:inner0:))"),
            "(\"hello world!\" (inner \"\"))"
        );
        assert_eq!(rt("[mime]3:abc"), "[mime]abc");
        assert_eq!(rt("\"a\\x01\\n\\101\""), "\"a\\x01\\nA\"");
        assert_eq!(rt("{KDE6YSk=}"), "(a)");
        assert_eq!(rt("|AAECAwQF|"), "|AAECAwQF|");
        assert_eq!(rt("  x  "), "x");
        assert!(parse("(a").is_err());
        assert!(parse("a b").is_err());
        assert!(parse("3abc").is_err());
        assert_eq!(parse("").unwrap(), None);
    }

    #[test]
    fn builders() {
        let v = |s: &[&str]| -> Vec<String> { s.iter().map(|s| String::from(*s)).collect() };
        assert_eq!(subst(&v(&["mktext", "a b"])).unwrap(), ["\"a b\""]);
        assert_eq!(
            subst(&v(&["mktextlist", "a", "1", "héllo"])).unwrap(),
            ["(a \"1\" \"h\\xc3\\xa9llo\")"]
        );
        assert_eq!(
            subst(&v(&["textels", "(a \"h\\xc3\\xa9llo\")"])).unwrap(),
            ["a", "héllo"]
        );
        assert_eq!(subst(&v(&["mklist"])).unwrap(), ["()"]);
        assert_eq!(subst(&v(&["els", "(a (b c))"])).unwrap(), ["a", "(b c)"]);
        assert_eq!(subst(&v(&["b64", "(a)"])).unwrap(), ["{KDE6YSk=}"]);
        assert_eq!(islist(&v(&["islist", "a"])).unwrap(), "not a list");
    }
}
