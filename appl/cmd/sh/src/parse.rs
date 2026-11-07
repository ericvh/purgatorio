//! The shell grammar:
//!
//! ```text
//! list     := { andor ( ';' | '&' | newline ) }
//! andor    := pipeline { ( '&&' | '||' ) pipeline }
//! pipeline := [ '!' ] cmd { '|' cmd }
//! cmd      := '{' list '}' { redir } | { word | redir }
//! ```
//!
//! A simple command whose first word is `name=...` (or `name :=`) is an
//! assignment of the rest of its words. A `{...}` in argument position is
//! passed as one word holding its source, as Inferno's sh passes blocks.

use crate::lex::{Lexer, Part, RedirKind, Result, SyntaxError, Tok, Token, Word};

/// Deeper nesting is refused rather than risking the guest stack.
pub const MAX_DEPTH: usize = 100;

#[derive(Debug)]
pub struct Item {
    pub andor: AndOr,
    pub background: bool,
    /// The source of `andor`, for running it in a sub-shell.
    pub src: String,
}

#[derive(Debug)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(bool, Pipeline)>,
}

#[derive(Debug)]
pub struct Pipeline {
    pub bang: bool,
    pub cmds: Vec<Cmd>,
}

#[derive(Debug)]
pub struct Redir {
    pub kind: RedirKind,
    pub fd: i32,
    pub target: Option<Word>,
}

#[derive(Debug)]
pub enum CmdKind {
    Simple(Vec<Word>),
    Assign {
        name: String,
        local: bool,
        value: Vec<Word>,
    },
    Block(Vec<Item>),
}

#[derive(Debug)]
pub struct Cmd {
    pub kind: CmdKind,
    pub redirs: Vec<Redir>,
    /// The command's source, redirections included.
    pub src: String,
}

pub fn parse(src: &str) -> Result<Vec<Item>> {
    let mut p = Parser {
        src,
        lex: Lexer::new(src),
        peeked: None,
        last_end: 0,
        last_join: false,
        depth: 0,
    };
    let items = p.list(false)?;
    match p.peek()?.tok {
        Tok::Eof => Ok(items),
        _ => p.unexpected(),
    }
}

struct Parser<'a> {
    src: &'a str,
    lex: Lexer<'a>,
    peeked: Option<Token>,
    last_end: usize,
    last_join: bool,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&mut self) -> Result<&Token> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lex.next()?);
        }
        Ok(self.peeked.as_ref().expect("just filled"))
    }

    fn bump(&mut self) -> Result<Token> {
        self.peek()?;
        let t = self.peeked.take().expect("just peeked");
        self.last_end = t.end;
        self.last_join = t.join;
        Ok(t)
    }

    fn operand(&mut self, t: Token) -> Result<Word> {
        match t.tok {
            Tok::Word(w) => Ok(w),
            Tok::LParen => self.paren_list(),
            Tok::Eof => Err(SyntaxError::Incomplete),
            _ => Err(SyntaxError::Bad("bad concatenation".into())),
        }
    }

    /// The word or `(list)` starting with `t`, with whatever `^` joins to it.
    fn joined(&mut self, t: Token) -> Result<Word> {
        let mut w = self.operand(t)?;
        while self.last_join {
            let t = self.bump()?;
            w.parts.extend(self.operand(t)?.parts);
        }
        Ok(w)
    }

    fn unexpected<T>(&mut self) -> Result<T> {
        let t = self.peek()?.clone();
        match t.tok {
            Tok::Eof => Err(SyntaxError::Incomplete),
            Tok::Newline => Err(SyntaxError::Bad("syntax error near newline".into())),
            _ => Err(SyntaxError::Bad(format!(
                "syntax error near {:?}",
                &self.src[t.start..t.end]
            ))),
        }
    }

    fn skip_newlines(&mut self) -> Result<()> {
        while self.peek()?.tok == Tok::Newline {
            self.bump()?;
        }
        if self.peek()?.tok == Tok::Eof {
            return Err(SyntaxError::Incomplete);
        }
        Ok(())
    }

    fn list(&mut self, in_block: bool) -> Result<Vec<Item>> {
        let mut items = Vec::new();
        loop {
            while matches!(self.peek()?.tok, Tok::Newline | Tok::Semi) {
                self.bump()?;
            }
            match self.peek()?.tok {
                Tok::Eof if in_block => return Err(SyntaxError::Incomplete),
                Tok::Eof => return Ok(items),
                Tok::RBrace if in_block => return Ok(items),
                Tok::RBrace | Tok::RParen => return self.unexpected(),
                _ => {}
            }
            let start = self.peek()?.start;
            let andor = self.andor()?;
            let src = String::from(&self.src[start..self.last_end]);
            let background = match self.peek()?.tok {
                Tok::Amp => {
                    self.bump()?;
                    true
                }
                Tok::Semi | Tok::Newline => {
                    self.bump()?;
                    false
                }
                Tok::Eof | Tok::RBrace => false,
                _ => return self.unexpected(),
            };
            items.push(Item {
                andor,
                background,
                src,
            });
        }
    }

    fn andor(&mut self) -> Result<AndOr> {
        let first = self.pipeline()?;
        let mut rest = Vec::new();
        loop {
            let and = match self.peek()?.tok {
                Tok::AndAnd => true,
                Tok::OrOr => false,
                _ => return Ok(AndOr { first, rest }),
            };
            self.bump()?;
            self.skip_newlines()?;
            rest.push((and, self.pipeline()?));
        }
    }

    fn pipeline(&mut self) -> Result<Pipeline> {
        let bang = matches!(&self.peek()?.tok, Tok::Word(w) if w.bare() == Some("!"));
        if bang {
            self.bump()?;
        }
        let mut cmds = vec![self.cmd()?];
        while self.peek()?.tok == Tok::Pipe {
            self.bump()?;
            self.skip_newlines()?;
            cmds.push(self.cmd()?);
        }
        Ok(Pipeline { bang, cmds })
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(SyntaxError::Bad("nesting too deep".into()));
        }
        Ok(())
    }

    /// `{ list }` after the opening brace has been consumed.
    fn block_body(&mut self) -> Result<Vec<Item>> {
        self.enter()?;
        let items = self.list(true)?;
        match self.bump()?.tok {
            Tok::RBrace => {}
            Tok::Eof => return Err(SyntaxError::Incomplete),
            _ => return Err(SyntaxError::Bad("missing }".into())),
        }
        self.depth -= 1;
        Ok(items)
    }

    fn cmd(&mut self) -> Result<Cmd> {
        let start = self.peek()?.start;
        let mut redirs = Vec::new();
        if self.peek()?.tok == Tok::LBrace {
            self.bump()?;
            let items = self.block_body()?;
            while let Tok::Redir(..) = self.peek()?.tok {
                redirs.push(self.redir()?);
            }
            return Ok(Cmd {
                kind: CmdKind::Block(items),
                redirs,
                src: String::from(&self.src[start..self.last_end]),
            });
        }
        let mut words = Vec::new();
        loop {
            match self.peek()?.tok {
                Tok::Word(_) | Tok::LParen => {
                    let t = self.bump()?;
                    words.push(self.joined(t)?);
                }
                Tok::Redir(..) => redirs.push(self.redir()?),
                Tok::LBrace => {
                    let open = self.bump()?.start;
                    self.block_body()?;
                    words.push(Word::lit(&self.src[open..self.last_end]));
                }
                _ => break,
            }
        }
        if words.is_empty() && redirs.is_empty() {
            return self.unexpected();
        }
        let src = String::from(&self.src[start..self.last_end]);
        let kind = assignment(&mut words).unwrap_or(CmdKind::Simple(words));
        Ok(Cmd { kind, redirs, src })
    }

    fn paren_list(&mut self) -> Result<Word> {
        self.enter()?;
        let mut words = Vec::new();
        loop {
            let t = self.bump()?;
            match t.tok {
                Tok::Word(_) | Tok::LParen => words.push(self.joined(t)?),
                Tok::Newline => {}
                Tok::RParen => break,
                Tok::Eof => return Err(SyntaxError::Incomplete),
                _ => return Err(SyntaxError::Bad("bad list".into())),
            }
        }
        self.depth -= 1;
        Ok(Word {
            parts: vec![Part::List(words)],
        })
    }

    fn redir(&mut self) -> Result<Redir> {
        let Tok::Redir(kind, fd) = self.bump()?.tok else {
            unreachable!("caller peeked a redirection");
        };
        let target = match kind {
            RedirKind::Dup(_) | RedirKind::Close => None,
            _ => {
                let t = self.bump()?;
                match t.tok {
                    Tok::Word(_) => Some(self.joined(t)?),
                    Tok::Eof => return Err(SyntaxError::Incomplete),
                    _ => return Err(SyntaxError::Bad("redirection needs a file name".into())),
                }
            }
        };
        Ok(Redir { kind, fd, target })
    }
}

fn is_name(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && cs.all(|c| c.is_alphanumeric() || c == '_')
}

/// Recognises `name=value...`, `name:=value...`, `name = value...` and
/// `name := value...`.
fn assignment(words: &mut Vec<Word>) -> Option<CmdKind> {
    let first = words.first()?;
    if let Some(name) = first.bare().filter(|n| is_name(n))
        && let Some(op @ ("=" | ":=")) = words.get(1).and_then(Word::bare)
    {
        let (name, local) = (String::from(name), op == ":=");
        return Some(CmdKind::Assign {
            name,
            local,
            value: words.split_off(2),
        });
    }
    let Some(Part::Lit(text, false)) = first.parts.first() else {
        return None;
    };
    let eq = text.find('=')?;
    let (name, local) = match text[..eq].strip_suffix(':') {
        Some(n) => (n, true),
        None => (&text[..eq], false),
    };
    if !is_name(name) {
        return None;
    }
    let name = String::from(name);
    let rest_text = String::from(&text[eq + 1..]);
    let mut first = words.remove(0);
    first.parts.remove(0);
    if !rest_text.is_empty() {
        first.parts.insert(0, Part::Lit(rest_text, false));
    }
    let mut value = Vec::new();
    if !first.parts.is_empty() {
        value.push(first);
    }
    value.append(words);
    Some(CmdKind::Assign { name, local, value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structure_and_sources() {
        let items = parse("a | b c >f && ! d; { e } &\nx=(1 2)").unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].src, "a | b c >f && ! d");
        assert!(!items[0].background);
        let p = &items[0].andor.first;
        assert_eq!(p.cmds.len(), 2);
        assert_eq!(p.cmds[1].src, "b c >f");
        assert_eq!(p.cmds[1].redirs.len(), 1);
        assert!(items[0].andor.rest[0].0);
        assert!(items[0].andor.rest[0].1.bang);
        assert!(items[1].background);
        assert!(matches!(
            items[1].andor.first.cmds[0].kind,
            CmdKind::Block(_)
        ));
        match &items[2].andor.first.cmds[0].kind {
            CmdKind::Assign { name, local, value } => {
                assert_eq!((name.as_str(), *local, value.len()), ("x", false, 1));
            }
            k => panic!("{k:?}"),
        }
    }

    #[test]
    fn assignments() {
        for (src, name, local, n) in [
            ("x=a b", "x", false, 2),
            ("x = a", "x", false, 1),
            ("y:=", "y", true, 0),
            ("y := 1 2 3", "y", true, 3),
            ("z=$a.b", "z", false, 1),
        ] {
            let items = parse(src).unwrap();
            match &items[0].andor.first.cmds[0].kind {
                CmdKind::Assign {
                    name: got,
                    local: l,
                    value,
                } => assert_eq!((got.as_str(), *l, value.len()), (name, local, n), "{src}"),
                k => panic!("{src}: {k:?}"),
            }
        }
        assert!(matches!(
            parse("echo a=b").unwrap()[0].andor.first.cmds[0].kind,
            CmdKind::Simple(_)
        ));
    }

    #[test]
    fn caret_joins_lists() {
        let parts = |src: &str| match &parse(src).unwrap()[0].andor.first.cmds[0].kind {
            CmdKind::Simple(words) => words.iter().map(|w| w.parts.len()).collect::<Vec<_>>(),
            k => panic!("{src}: {k:?}"),
        };
        assert_eq!(parts("echo $x^(a b)"), [1, 2]);
        assert_eq!(parts("echo (a b) ^ .c d"), [1, 2, 1]);
        assert_eq!(parts("echo (a b)^(c d)^e"), [1, 3]);
        assert_eq!(parts("echo (a b) (c)"), [1, 1, 1]);
    }

    #[test]
    fn incomplete_and_errors() {
        for s in ["a |", "a &&", "{ a", "echo (a", "a >"] {
            assert_eq!(parse(s).unwrap_err(), SyntaxError::Incomplete, "{s}");
        }
        for s in ["a }", "a ; ; )", "| a"] {
            assert!(matches!(parse(s), Err(SyntaxError::Bad(_))), "{s}");
        }
        let deep = format!(
            "{}echo{}",
            "{".repeat(MAX_DEPTH + 1),
            "}".repeat(MAX_DEPTH + 1)
        );
        assert_eq!(
            parse(&deep).unwrap_err(),
            SyntaxError::Bad("nesting too deep".into())
        );
        let ok = format!("{}echo{}", "{".repeat(MAX_DEPTH), "}".repeat(MAX_DEPTH));
        parse(&ok).unwrap();
    }
}
