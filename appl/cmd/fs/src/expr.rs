//! The expression syntax: a verb and its arguments, where an argument is
//! a word or a `{...}` block holding another expression. Words are
//! separated by white space; `'...'` quotes a word, with `''` for a quote.

/// An argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Word(String),
    /// The verb and arguments inside `{...}`.
    Block(Vec<Expr>),
}

struct Parser<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl Parser<'_> {
    fn space(&mut self) {
        while self.chars.next_if(|c| c.is_whitespace()).is_some() {}
    }

    /// The arguments up to the closing brace (when `inner`) or the end.
    fn list(&mut self, inner: bool) -> Result<Vec<Expr>, String> {
        let mut out = Vec::new();
        loop {
            self.space();
            match self.chars.peek() {
                None if inner => return Err(String::from("missing }")),
                None => return Ok(out),
                Some('}') if inner => {
                    self.chars.next();
                    return Ok(out);
                }
                Some('}') => return Err(String::from("unexpected }")),
                Some('{') => {
                    self.chars.next();
                    let block = self.list(true)?;
                    if block.is_empty() {
                        return Err(String::from("empty {}"));
                    }
                    out.push(Expr::Block(block));
                }
                Some(_) => out.push(Expr::Word(self.word()?)),
            }
        }
    }

    fn word(&mut self) -> Result<String, String> {
        let mut w = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() || c == '{' || c == '}' {
                break;
            }
            self.chars.next();
            if c != '\'' {
                w.push(c);
                continue;
            }
            loop {
                match self.chars.next() {
                    None => return Err(String::from("missing '")),
                    Some('\'') if self.chars.next_if_eq(&'\'').is_some() => w.push('\''),
                    Some('\'') => break,
                    Some(c) => w.push(c),
                }
            }
        }
        Ok(w)
    }
}

/// Parses the text of one expression: `verb arg...`.
pub fn parse(text: &str) -> Result<Vec<Expr>, String> {
    let mut p = Parser {
        chars: text.chars().peekable(),
    };
    let list = p.list(false)?;
    if list.is_empty() {
        return Err(String::from("empty expression"));
    }
    Ok(list)
}

/// The expression a command line gives: each argument is a word as it
/// stands, unless it is a whole `{...}` block, which is parsed.
pub fn from_args(args: &[String]) -> Result<Vec<Expr>, String> {
    let mut out = Vec::new();
    for a in args {
        if a.starts_with('{') && a.ends_with('}') && a.len() >= 2 {
            match parse(a)?.as_slice() {
                [block @ Expr::Block(_)] => out.push(block.clone()),
                _ => return Err(format!("bad block {a}")),
            }
        } else {
            out.push(Expr::Word(a.clone()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Expr {
        Expr::Word(String::from(s))
    }

    #[test]
    fn nesting_and_quotes() {
        assert_eq!(
            parse("print {filter -d {not {match '*.o'}} {walk 'a b'}}").unwrap(),
            vec![
                w("print"),
                Expr::Block(vec![
                    w("filter"),
                    w("-d"),
                    Expr::Block(vec![w("not"), Expr::Block(vec![w("match"), w("*.o")])]),
                    Expr::Block(vec![w("walk"), w("a b")]),
                ])
            ]
        );
        assert_eq!(
            parse("x 'it''s' ''").unwrap(),
            vec![w("x"), w("it's"), w("")]
        );
        assert_eq!(parse("a{b}c").unwrap().len(), 3);
    }

    #[test]
    fn errors() {
        assert_eq!(parse("a {b").unwrap_err(), "missing }");
        assert_eq!(parse("a }").unwrap_err(), "unexpected }");
        assert_eq!(parse("a {}").unwrap_err(), "empty {}");
        assert_eq!(parse("a 'b").unwrap_err(), "missing '");
        assert_eq!(parse("  ").unwrap_err(), "empty expression");
    }

    #[test]
    fn command_line() {
        let args = ["print", "{walk /}", "{", "a b"].map(String::from);
        assert_eq!(
            from_args(&args).unwrap(),
            vec![
                w("print"),
                Expr::Block(vec![w("walk"), w("/")]),
                w("{"),
                w("a b")
            ]
        );
        assert!(from_args(&[String::from("{a} {b}")]).is_err());
    }
}
