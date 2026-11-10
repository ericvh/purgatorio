//! A recursive-descent parser for mash(1).
//!
//! Each top-level command is parsed and run before the next is read, as
//! mash does, so a syntax error later in a file does not stop the commands
//! before it. Precedence, lowest first: `if`, `for`, `while` and `case`
//! bodies; `else`; `|`. In expressions: `==` `!=` `~` (left), `::`
//! (right), `^` (left), then the prefix operators `!` `hd` `tl` `len`.

use std::rc::Rc;

use crate::ast::{Body, Cmd, Expr, Gen, RKind, Recipe, Redir, Word};
use crate::lex::{Kw, Lexer, Tok, ident};

pub const SYNTAX: &str = "syntax error";

pub struct Parser {
    lex: Lexer,
    peeked: Option<Tok>,
    /// Errors that do not stop the parse but drop the command, as mash
    /// reports them: repeated redirections, pipe conflicts and so on.
    pub errs: Vec<String>,
}

type R<T> = Result<T, String>;

/// One item of a simple command.
enum Item {
    Word(Word),
    Redir(Redir),
}

impl Parser {
    pub fn new(src: &str) -> Parser {
        Parser {
            lex: Lexer::new(src),
            peeked: None,
            errs: Vec::new(),
        }
    }

    pub fn line(&self) -> usize {
        self.lex.line
    }

    fn peek(&mut self) -> R<&Tok> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lex.next()?);
        }
        Ok(self.peeked.as_ref().expect("just filled"))
    }

    fn next(&mut self) -> R<Tok> {
        match self.peeked.take() {
            Some(t) => Ok(t),
            None => self.lex.next(),
        }
    }

    fn at(&mut self, t: &Tok) -> R<bool> {
        Ok(self.peek()? == t)
    }

    fn expect(&mut self, t: Tok) -> R<()> {
        if self.next()? == t {
            Ok(())
        } else {
            Err(String::from(SYNTAX))
        }
    }

    /// Skips the rest of the line after an error.
    pub fn recover(&mut self) {
        if self.peeked.take() != Some(Tok::Eof) {
            self.lex.skip_line();
        }
    }

    /// The next top-level command and whether it ends with `&`, or `None`
    /// at the end of the input. On `Ok`, `errs` lists errors that drop it.
    pub fn command(&mut self) -> R<Option<(Cmd, bool)>> {
        self.errs.clear();
        while self.at(&Tok::Semi)? {
            self.next()?;
        }
        if self.at(&Tok::Eof)? {
            return Ok(None);
        }
        let c = self.cmd()?;
        let bg = match self.next()? {
            Tok::Semi | Tok::Eof => false,
            Tok::Amp => true,
            _ => return Err(String::from(SYNTAX)),
        };
        Ok(Some((c, bg)))
    }

    /// A pipeline of one or more commands.
    fn cmd(&mut self) -> R<Cmd> {
        let mut stages = vec![self.unit()?];
        while self.at(&Tok::Pipe)? {
            self.next()?;
            stages.push(self.unit()?);
        }
        if stages.len() == 1 {
            return Ok(stages.pop().expect("one stage"));
        }
        let n = stages.len();
        for (i, c) in stages.iter().enumerate() {
            let piped = u8::from(i > 0) | (u8::from(i + 1 < n) << 1);
            let rs = match c {
                Cmd::Simple { redirs, .. } | Cmd::Group { redirs, .. } => redirs.as_slice(),
                _ => &[],
            };
            let clash = rs.iter().fold(0, |a, r| a | r.kind.sides()) & piped;
            if clash != 0 {
                self.errs.push(format!(
                    "{} redirection conflicts with pipe",
                    side_name(clash)
                ));
            }
        }
        Ok(Cmd::Pipe(stages))
    }

    fn unit(&mut self) -> R<Cmd> {
        match self.peek()?.clone() {
            Tok::LBrace => {
                let body = self.braces()?;
                let redirs = self.epilog()?;
                Ok(Cmd::Group {
                    body: Rc::new(body),
                    redirs,
                    scope: false,
                })
            }
            Tok::At => {
                self.next()?;
                if !self.at(&Tok::LBrace)? {
                    return Err(String::from(SYNTAX));
                }
                let body = self.braces()?;
                let redirs = self.epilog()?;
                Ok(Cmd::Group {
                    body: Rc::new(body),
                    redirs,
                    scope: true,
                })
            }
            Tok::Kw(Kw::For) => {
                self.next()?;
                self.expect(Tok::LParen)?;
                let var = self.sword()?;
                self.expect(Tok::Kw(Kw::In))?;
                let list = self.list()?;
                self.expect(Tok::RParen)?;
                let body = Box::new(self.cmd()?);
                Ok(Cmd::For { var, list, body })
            }
            Tok::Kw(Kw::If) => {
                self.next()?;
                let cond = self.paren_expr()?;
                let then = Box::new(self.cmd()?);
                let els = if self.at(&Tok::Kw(Kw::Else))? {
                    self.next()?;
                    Some(Box::new(self.cmd()?))
                } else {
                    None
                };
                Ok(Cmd::If { cond, then, els })
            }
            Tok::Kw(Kw::While) => {
                self.next()?;
                let cond = self.paren_expr()?;
                let body = Box::new(self.cmd()?);
                Ok(Cmd::While { cond, body })
            }
            Tok::Kw(Kw::Case) => {
                self.next()?;
                let subject = self.expr()?;
                self.expect(Tok::LBrace)?;
                let mut arms = Vec::new();
                loop {
                    while self.at(&Tok::Semi)? {
                        self.next()?;
                    }
                    if self.at(&Tok::RBrace)? {
                        self.next()?;
                        break;
                    }
                    let pat = self.expr()?;
                    self.expect(Tok::Arrow)?;
                    let c = self.cmd()?;
                    let bg = self.term()?;
                    arms.push((pat, c, bg));
                }
                Ok(Cmd::Case { subject, arms })
            }
            Tok::Kw(k @ (Kw::Fn | Kw::Rescue)) => {
                self.next()?;
                let name = self.word()?;
                let body = if self.at(&Tok::LBrace)? {
                    Some(Rc::new(self.braces()?))
                } else {
                    None
                };
                Ok(if k == Kw::Fn {
                    Cmd::Fn { name, body }
                } else {
                    Cmd::Rescue { name, body }
                })
            }
            _ => self.simple(),
        }
    }

    /// `;` or `&` after a command in braces; a closing brace ends the last
    /// command too.
    fn term(&mut self) -> R<bool> {
        match self.peek()? {
            Tok::Semi => {
                self.next()?;
                Ok(false)
            }
            Tok::Amp => {
                self.next()?;
                Ok(true)
            }
            Tok::RBrace | Tok::Eof => Ok(false),
            _ => Err(String::from(SYNTAX)),
        }
    }

    fn paren_expr(&mut self) -> R<Expr> {
        self.expect(Tok::LParen)?;
        let e = self.expr()?;
        self.expect(Tok::RParen)?;
        Ok(e)
    }

    /// `{ commands }`.
    fn braces(&mut self) -> R<Body> {
        self.expect(Tok::LBrace)?;
        let mut body = Vec::new();
        loop {
            while self.at(&Tok::Semi)? {
                self.next()?;
            }
            if self.at(&Tok::RBrace)? {
                self.next()?;
                return Ok(body);
            }
            if self.at(&Tok::Eof)? {
                return Err(String::from(SYNTAX));
            }
            let c = self.cmd()?;
            let bg = self.term()?;
            body.push((c, bg));
        }
    }

    /// Redirections after a group.
    fn epilog(&mut self) -> R<Vec<Redir>> {
        let mut rs: Vec<Redir> = Vec::new();
        while let Some(kind) = self.redir_kind()? {
            self.next()?;
            let word = self.word()?;
            self.add_redir(&mut rs, Redir { kind, word });
        }
        Ok(rs)
    }

    fn redir_kind(&mut self) -> R<Option<RKind>> {
        Ok(match self.peek()? {
            Tok::Less => Some(RKind::In),
            Tok::Great => Some(RKind::Out),
            Tok::GreatGreat => Some(RKind::Append),
            Tok::LessGreat => Some(RKind::InOut),
            _ => None,
        })
    }

    fn add_redir(&mut self, rs: &mut Vec<Redir>, r: Redir) {
        let have = rs.iter().fold(0, |a, x| a | x.kind.sides());
        let clash = have & r.kind.sides();
        if clash != 0 {
            self.errs
                .push(format!("repeat {} redirection", side_name(clash)));
        }
        rs.push(r);
    }

    /// An identifier, for `for` and assignments.
    fn sword(&mut self) -> R<String> {
        match self.next()? {
            Tok::Word(w) => sword(w).map_err(|e| {
                self.errs.push(e);
                String::from(SYNTAX)
            }),
            _ => Err(String::from(SYNTAX)),
        }
    }

    /// A word: text, `(expr)`, or words joined with `^`.
    fn word(&mut self) -> R<Word> {
        let mut w = self.word1()?;
        while self.at(&Tok::Caret)? {
            self.next()?;
            let r = self.word1()?;
            w = Word::Caret(Box::new(w), Box::new(r));
        }
        Ok(w)
    }

    fn word1(&mut self) -> R<Word> {
        match self.next()? {
            Tok::Word(w) => Ok(w),
            Tok::LParen => {
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                Ok(Word::Expr(Box::new(e)))
            }
            _ => Err(String::from(SYNTAX)),
        }
    }

    /// `` `{...} `` or `"{...}`, if one starts here.
    fn generator(&mut self) -> R<Option<Word>> {
        let g = match self.peek()? {
            Tok::Backq => Gen::Backq,
            Tok::Quote => Gen::Quote,
            _ => return Ok(None),
        };
        self.next()?;
        if !self.at(&Tok::LBrace)? {
            return Err(String::from(SYNTAX));
        }
        let body = self.braces()?;
        Ok(Some(Word::Gen(g, Rc::new(body))))
    }

    /// A word or a generator.
    fn witem(&mut self) -> R<Option<Word>> {
        match self.peek()? {
            Tok::Word(_) | Tok::LParen => Ok(Some(self.word()?)),
            Tok::Backq | Tok::Quote => self.generator(),
            _ => Ok(None),
        }
    }

    /// Words and generators up to the end of a list.
    fn list(&mut self) -> R<Vec<Word>> {
        let mut out = Vec::new();
        loop {
            if matches!(self.peek()?, Tok::Less | Tok::Great) {
                out.push(self.in_out_gen()?);
                continue;
            }
            match self.witem()? {
                Some(w) => out.push(w),
                None => return Ok(out),
            }
        }
    }

    /// `<{...}` or `>{...}` in a list, where `<` and `>` cannot be
    /// redirections.
    fn in_out_gen(&mut self) -> R<Word> {
        let g = if self.next()? == Tok::Less {
            Gen::In
        } else {
            Gen::Out
        };
        if !self.at(&Tok::LBrace)? {
            return Err(String::from(SYNTAX));
        }
        let body = self.braces()?;
        Ok(Word::Gen(g, Rc::new(body)))
    }

    /// A simple command, assignment, dependency or rule.
    fn simple(&mut self) -> R<Cmd> {
        let mut items: Vec<Item> = Vec::new();
        if !matches!(self.peek()?, Tok::Word(_) | Tok::LParen) {
            return Err(String::from(SYNTAX));
        }
        items.push(Item::Word(self.word()?));
        if let [Item::Word(w)] = items.as_slice() {
            let w = w.clone();
            match self.peek()? {
                Tok::Eq | Tok::DefEq => {
                    let local = self.next()? == Tok::DefEq;
                    let value = self.list()?;
                    return match sword(w) {
                        Ok(name) => Ok(Cmd::Assign { name, local, value }),
                        Err(e) => {
                            self.errs.push(e);
                            Ok(Cmd::Simple {
                                words: Vec::new(),
                                redirs: Vec::new(),
                            })
                        }
                    };
                }
                Tok::ColonTilde => {
                    self.next()?;
                    let dep = self.word()?;
                    let recipe = self.recipe()?.ok_or_else(|| String::from(SYNTAX))?;
                    return Ok(Cmd::Rule {
                        pattern: w,
                        dep,
                        recipe,
                    });
                }
                _ => {}
            }
        }
        loop {
            match self.peek()? {
                Tok::Word(_) | Tok::LParen => items.push(Item::Word(self.word()?)),
                Tok::Backq | Tok::Quote => {
                    let g = self.generator()?.expect("a generator starts here");
                    items.push(Item::Word(g));
                }
                Tok::Less | Tok::Great | Tok::GreatGreat | Tok::LessGreat => {
                    let kind = self.redir_kind()?.expect("a redirection starts here");
                    self.next()?;
                    if matches!(kind, RKind::In | RKind::Out) && self.at(&Tok::LBrace)? {
                        let g = if kind == RKind::In { Gen::In } else { Gen::Out };
                        let body = self.braces()?;
                        items.push(Item::Word(Word::Gen(g, Rc::new(body))));
                        continue;
                    }
                    let word = self.word()?;
                    items.push(Item::Redir(Redir { kind, word }));
                }
                Tok::Colon => {
                    self.next()?;
                    return self.depend(items);
                }
                _ => break,
            }
        }
        let mut words = Vec::new();
        let mut redirs = Vec::new();
        for i in items {
            match i {
                Item::Word(w) => words.push(w),
                Item::Redir(r) => self.add_redir(&mut redirs, r),
            }
        }
        Ok(Cmd::Simple { words, redirs })
    }

    fn depend(&mut self, items: Vec<Item>) -> R<Cmd> {
        let mut targets = Vec::new();
        for i in items {
            match i {
                Item::Word(w) => targets.push(w),
                Item::Redir(_) => self.errs.push(String::from("redirection in list")),
            }
        }
        let deps = self.list()?;
        let recipe = self.recipe()?;
        Ok(Cmd::Depend {
            targets,
            deps,
            recipe,
        })
    }

    /// `{ commands }` or `:{ commands }` after a dependency or rule.
    fn recipe(&mut self) -> R<Option<Recipe>> {
        let list = match self.peek()? {
            Tok::Colon => {
                self.next()?;
                true
            }
            Tok::LBrace => false,
            _ => return Ok(None),
        };
        let body = Rc::new(self.braces()?);
        Ok(Some(Recipe { list, body }))
    }

    pub fn expr(&mut self) -> R<Expr> {
        let mut l = self.cons()?;
        loop {
            let op = match self.peek()? {
                Tok::EqEq => Expr::Eq,
                Tok::NotEq => Expr::Ne,
                Tok::Tilde => Expr::Match,
                _ => return Ok(l),
            };
            self.next()?;
            let r = self.cons()?;
            l = op(Box::new(l), Box::new(r));
        }
    }

    fn cons(&mut self) -> R<Expr> {
        let l = self.cat()?;
        if self.at(&Tok::Cons)? {
            self.next()?;
            let r = self.cons()?;
            return Ok(Expr::Cons(Box::new(l), Box::new(r)));
        }
        Ok(l)
    }

    fn cat(&mut self) -> R<Expr> {
        let mut l = self.unary()?;
        while self.at(&Tok::Caret)? {
            self.next()?;
            let r = self.unary()?;
            l = Expr::Caret(Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn unary(&mut self) -> R<Expr> {
        let op = match self.peek()? {
            Tok::Not => Expr::Not,
            Tok::Kw(Kw::Hd) => Expr::Hd,
            Tok::Kw(Kw::Tl) => Expr::Tl,
            Tok::Kw(Kw::Len) => Expr::Len,
            _ => return self.primary(),
        };
        self.next()?;
        Ok(op(Box::new(self.unary()?)))
    }

    fn primary(&mut self) -> R<Expr> {
        match self.peek()? {
            Tok::Word(_) => match self.next()? {
                Tok::Word(w) => Ok(Expr::Word(w)),
                _ => unreachable!("peeked a word"),
            },
            Tok::LParen => {
                self.next()?;
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::Backq | Tok::Quote | Tok::Less | Tok::Great => {
                let tok = self.next()?;
                let g = match tok {
                    Tok::Backq => Gen::Backq,
                    Tok::Quote => Gen::Quote,
                    Tok::Less => Gen::In,
                    _ => Gen::Out,
                };
                if !self.at(&Tok::LBrace)? {
                    return Err(String::from(SYNTAX));
                }
                let body = self.braces()?;
                Ok(Expr::Word(Word::Gen(g, Rc::new(body))))
            }
            _ => Err(String::from(SYNTAX)),
        }
    }
}

fn side_name(sides: u8) -> &'static str {
    // mash spells it "ouput".
    match sides {
        1 => "input",
        2 => "ouput",
        _ => "input/output",
    }
}

/// The identifier a word names, or mash's complaint about it.
fn sword(w: Word) -> Result<String, String> {
    match &w {
        Word::Lit { text, .. } if ident(text) => Ok(text.clone()),
        _ => Err(format!("malformed identifier: {w}")),
    }
}

/// Parses all of `src`, for tests and for checking that a sub-mash's
/// text reads back.
#[cfg(test)]
pub fn parse_all(src: &str) -> Result<Vec<(Cmd, bool)>, String> {
    let mut p = Parser::new(src);
    let mut out = Vec::new();
    while let Some(c) = p.command()? {
        if let Some(e) = p.errs.first() {
            return Err(e.clone());
        }
        out.push(c);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(src: &str) -> String {
        let cmds = parse_all(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        let mut s = String::new();
        crate::ast::sequence(&mut s, &cmds, &crate::ast::Ctx { args: None });
        // The text must parse back to the same tree.
        assert_eq!(parse_all(&s).unwrap(), cmds, "{s}");
        s
    }

    #[test]
    fn simple_and_pipes() {
        assert_eq!(
            round("echo a b > f; cat < f | wc &"),
            "echo a b > f; cat < f | wc&"
        );
        assert_eq!(
            round("x = a b c; y := ; z=/tmp"),
            "x = a b c; y :=; z = /tmp;"
        );
        assert_eq!(round("echo $x^/*.c ($a ^ b)"), "echo $x ^ /*.c ($a ^ b);");
        assert_eq!(round("cmp <{old} <{new}"), "cmp <{ old; } <{ new; };");
        assert_eq!(round("echo `{ls} \"{pwd}"), "echo `{ ls; } \"{ pwd; };");
        assert_eq!(round("rw <>f"), "rw <> f;");
    }

    #[test]
    fn control_flow() {
        assert_eq!(
            round("if ($x == a) echo yes else echo no"),
            "if ($x == a) echo yes else echo no;"
        );
        assert_eq!(
            round("for (i in a b) { echo $i; echo again }"),
            "for (i in a b) { echo $i; echo again; };"
        );
        assert_eq!(round("while (! $done) x = y"), "while (! $done) x = y;");
        assert_eq!(
            round("case $x { a :: b => echo ab; c* => echo c; }"),
            "case $x { a :: b => echo ab; c* => echo c; };"
        );
        assert_eq!(
            round("fn f { echo $args }; fn f"),
            "fn f { echo $args; }; fn f;"
        );
        assert_eq!(round("@{ x = 1 } > out"), "@{ x = 1; } > out;");
        // if binds looser than |, so the pipe is the body.
        let cmds = parse_all("if (x) a | b").unwrap();
        assert!(matches!(&cmds[0].0, Cmd::If { then, .. } if matches!(**then, Cmd::Pipe(_))));
    }

    #[test]
    fn expressions() {
        let e = |s: &str| {
            let mut p = Parser::new(s);
            p.expr().unwrap().to_string()
        };
        assert_eq!(e("a ^ b :: c :: d == e"), "a ^ b :: c :: d == e");
        assert_eq!(e("(a :: b) :: c"), "(a :: b) :: c");
        assert_eq!(e("hd a ^ b"), "hd a ^ b");
        assert_eq!(e("hd (a ^ b)"), "hd (a ^ b)");
        assert_eq!(e("len tl $x"), "len tl $x");
        assert_eq!(e("!a == b"), "! a == b");
        assert_eq!(e("a == (b == c)"), "a == (b == c)");
    }

    #[test]
    fn make_rules() {
        assert_eq!(
            round("rotta.dis : rotta.b rotta.m { limbo $lflags rotta.b };"),
            "rotta.dis : rotta.b rotta.m { limbo $lflags rotta.b; };"
        );
        assert_eq!(round("all : a b {}; x:y"), "all : a b {}; x : y;");
        assert_eq!(
            round("*.dis :~ $1.b { lc $1.b }"),
            "*.dis :~ $1.b { lc $1.b; };"
        );
        assert_eq!(round("clean : { rm x }"), "clean : { rm x; };");
        assert_eq!(round("t : d :{ x }"), "t : d :{ x; };");
    }

    #[test]
    fn errors() {
        assert_eq!(parse_all("echo a=b").unwrap_err(), SYNTAX);
        assert_eq!(parse_all("echo if").unwrap_err(), SYNTAX);
        assert_eq!(parse_all("{ echo").unwrap_err(), SYNTAX);
        assert_eq!(
            parse_all("echo > a > b").unwrap_err(),
            "repeat ouput redirection"
        );
        assert_eq!(
            parse_all("echo > a | cat").unwrap_err(),
            "ouput redirection conflicts with pipe"
        );
        assert_eq!(
            parse_all("a.b = 1").unwrap_err(),
            "malformed identifier: a.b"
        );
        assert_eq!(parse_all("a > f : b").unwrap_err(), "redirection in list");
        // Commands before an error are complete and runnable.
        let mut p = Parser::new("echo one; echo (");
        assert!(p.command().unwrap().is_some());
        assert!(p.command().is_err());
    }
}
