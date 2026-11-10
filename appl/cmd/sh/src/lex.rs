//! Tokens of the shell language. Words are sequences of parts that are
//! concatenated: unquoted text, `'quoted'` text (`''` is a quote), `$var`
//! references and `` `{...} `` substitutions. `^` joins words explicitly.

/// One piece of a word.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    /// Literal text; `true` if it was quoted.
    Lit(String, bool),
    /// `$name`, `$#name` or `$"name`.
    Var(String, VarKind),
    /// `` `{...} ``: the source between the braces.
    Subst(String),
    /// `${name args}`: a loaded builtin. The string is the source inside.
    Builtin(String),
    /// `(a b c)`: a list of words.
    List(Vec<Word>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VarKind {
    /// The list's elements.
    Plain,
    /// `$#x`: the number of elements.
    Count,
    /// `$"x`: the elements joined by spaces, as one word.
    Join,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Word {
    pub parts: Vec<Part>,
}

impl Word {
    pub fn lit(s: &str) -> Word {
        Word {
            parts: vec![Part::Lit(String::from(s), true)],
        }
    }

    /// The text of a word made only of unquoted literal text.
    pub fn bare(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [Part::Lit(s, false)] => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RedirKind {
    /// `<`
    In,
    /// `>`
    Out,
    /// `>>`
    Append,
    /// `>[a=b]`: fd `a` becomes a copy of fd `b`.
    Dup(i32),
    /// `>[a=]`: fd `a` is closed.
    Close,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Word(Word),
    Redir(RedirKind, i32),
    Semi,
    Amp,
    Pipe,
    AndAnd,
    OrOr,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Newline,
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
    /// A `^` joins this word or `)` to the next token, which starts a list.
    pub join: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SyntaxError {
    /// The input ends in the middle of a construct; more lines may finish
    /// it.
    Incomplete,
    Bad(String),
}

pub type Result<T> = std::result::Result<T, SyntaxError>;

fn bad<T>(msg: impl Into<String>) -> Result<T> {
    Err(SyntaxError::Bad(msg.into()))
}

/// The characters that separate words: space, tab, carriage return and
/// newline. Any other white space Unicode has, such as U+00A0, is an
/// ordinary character of a word.
pub fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

fn is_break(c: char) -> bool {
    is_blank(c) || ";&|{}()<>".contains(c)
}

fn is_ordinary(c: char) -> bool {
    !is_break(c) && !"'$`^".contains(c)
}

pub struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    join: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Lexer<'a> {
        Lexer {
            src,
            pos: 0,
            join: false,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\r')) {
            self.bump();
        }
    }

    pub fn next(&mut self) -> Result<Token> {
        self.skip_blanks();
        if self.peek() == Some('#') {
            while !matches!(self.peek(), None | Some('\n')) {
                self.bump();
            }
        }
        let start = self.pos;
        let Some(c) = self.peek() else {
            return Ok(Token {
                tok: Tok::Eof,
                start,
                end: start,
                join: false,
            });
        };
        self.join = false;
        let tok = match c {
            '\n' => {
                self.bump();
                Tok::Newline
            }
            ';' => {
                self.bump();
                Tok::Semi
            }
            '&' => {
                self.bump();
                if self.peek() == Some('&') {
                    self.bump();
                    Tok::AndAnd
                } else {
                    Tok::Amp
                }
            }
            '|' => {
                self.bump();
                if self.peek() == Some('|') {
                    self.bump();
                    Tok::OrOr
                } else {
                    Tok::Pipe
                }
            }
            '{' => {
                self.bump();
                Tok::LBrace
            }
            '}' => {
                self.bump();
                Tok::RBrace
            }
            '(' => {
                self.bump();
                Tok::LParen
            }
            ')' => {
                self.bump();
                let save = self.pos;
                self.skip_blanks();
                if self.peek() == Some('^') {
                    self.bump();
                    self.skip_blanks();
                    self.join = true;
                } else {
                    self.pos = save;
                }
                Tok::RParen
            }
            '<' | '>' => self.redir()?,
            _ => Tok::Word(self.word()?),
        };
        Ok(Token {
            tok,
            start,
            end: self.pos,
            join: self.join,
        })
    }

    fn redir(&mut self) -> Result<Tok> {
        let c = self.bump().expect("caller saw < or >");
        let (kind, default) = if c == '<' {
            (RedirKind::In, 0)
        } else if self.peek() == Some('>') {
            self.bump();
            (RedirKind::Append, 1)
        } else {
            (RedirKind::Out, 1)
        };
        if self.peek() != Some('[') {
            return Ok(Tok::Redir(kind, default));
        }
        self.bump();
        let fd = self.number()?;
        match self.bump() {
            Some(']') => Ok(Tok::Redir(kind, fd)),
            Some('=') => {
                let kind = if self.peek() == Some(']') {
                    RedirKind::Close
                } else {
                    RedirKind::Dup(self.number()?)
                };
                if self.bump() != Some(']') {
                    return bad("bad redirection");
                }
                Ok(Tok::Redir(kind, fd))
            }
            None => Err(SyntaxError::Incomplete),
            _ => bad("bad redirection"),
        }
    }

    fn number(&mut self) -> Result<i32> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
        }
        self.src[start..self.pos]
            .parse()
            .or_else(|_| bad("bad redirection"))
    }

    fn word(&mut self) -> Result<Word> {
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                None => {}
                Some('\'') => {
                    self.bump();
                    parts.push(Part::Lit(self.quoted()?, true));
                    continue;
                }
                Some('$') => {
                    self.bump();
                    parts.push(self.var()?);
                    continue;
                }
                Some('`') => {
                    self.bump();
                    if self.bump() != Some('{') {
                        return bad("` must be followed by {");
                    }
                    parts.push(Part::Subst(String::from(self.braced()?)));
                    continue;
                }
                Some('^') => {
                    self.bump();
                    self.skip_blanks();
                    if self.peek() == Some('(') {
                        self.join = true;
                        break;
                    }
                    continue;
                }
                Some(c) if is_ordinary(c) => {
                    let start = self.pos;
                    while self.peek().is_some_and(is_ordinary) {
                        self.bump();
                    }
                    parts.push(Part::Lit(String::from(&self.src[start..self.pos]), false));
                    continue;
                }
                Some(_) => {}
            }
            // A `^` after blanks still joins the next word to this one.
            let save = self.pos;
            self.skip_blanks();
            if self.peek() == Some('^') {
                continue;
            }
            self.pos = save;
            break;
        }
        if parts.is_empty() {
            return bad(format!(
                "syntax error near {:?}",
                self.peek().unwrap_or(' ')
            ));
        }
        Ok(Word { parts })
    }

    fn quoted(&mut self) -> Result<String> {
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return Err(SyntaxError::Incomplete),
                Some('\'') if self.peek() == Some('\'') => {
                    self.bump();
                    s.push('\'');
                }
                Some('\'') => return Ok(s),
                Some(c) => s.push(c),
            }
        }
    }

    fn var(&mut self) -> Result<Part> {
        if self.peek() == Some('{') {
            self.bump();
            return Ok(Part::Builtin(String::from(self.braced()?)));
        }
        let kind = match self.peek() {
            Some('#') => VarKind::Count,
            Some('"') => VarKind::Join,
            _ => VarKind::Plain,
        };
        if kind != VarKind::Plain {
            self.bump();
        }
        let start = self.pos;
        if self.peek() == Some('*') {
            self.bump();
        } else {
            while self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                self.bump();
            }
        }
        if self.pos == start {
            return match self.peek() {
                None => Err(SyntaxError::Incomplete),
                _ => bad("bad variable name"),
            };
        }
        Ok(Part::Var(String::from(&self.src[start..self.pos]), kind))
    }

    /// The source up to the `}` matching an already consumed `{`.
    fn braced(&mut self) -> Result<&'a str> {
        let start = self.pos;
        let mut depth = 1;
        loop {
            match self.bump() {
                None => return Err(SyntaxError::Incomplete),
                Some('\'') => {
                    self.quoted()?;
                }
                Some('{') => depth += 1,
                Some('}') => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(&self.src[start..self.pos - 1]);
                    }
                }
                Some(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        let mut l = Lexer::new(s);
        let mut out = Vec::new();
        loop {
            let t = l.next().unwrap().tok;
            if t == Tok::Eof {
                return out;
            }
            out.push(t);
        }
    }

    fn lit(s: &str, q: bool) -> Part {
        Part::Lit(s.into(), q)
    }

    #[test]
    fn words_and_operators() {
        let t = toks("echo 'it''s' a$x^b >>[2] f && x || y | z & # c\n");
        assert_eq!(
            t[0],
            Tok::Word(Word {
                parts: vec![lit("echo", false)]
            })
        );
        assert_eq!(
            t[1],
            Tok::Word(Word {
                parts: vec![lit("it's", true)]
            })
        );
        assert_eq!(
            t[2],
            Tok::Word(Word {
                parts: vec![
                    lit("a", false),
                    Part::Var("x".into(), VarKind::Plain),
                    lit("b", false)
                ]
            })
        );
        assert_eq!(t[3], Tok::Redir(RedirKind::Append, 2));
        assert_eq!(
            &t[5..],
            &[
                Tok::AndAnd,
                Tok::Word(Word {
                    parts: vec![lit("x", false)]
                }),
                Tok::OrOr,
                Tok::Word(Word {
                    parts: vec![lit("y", false)]
                }),
                Tok::Pipe,
                Tok::Word(Word {
                    parts: vec![lit("z", false)]
                }),
                Tok::Amp,
                Tok::Newline
            ]
        );
    }

    #[test]
    fn only_ascii_blanks_separate_words() {
        let t = toks("echo a\u{a0}b \u{a0} \u{3000}x\u{2028}'q'\u{85}\n");
        let word = |s: &str| {
            Tok::Word(Word {
                parts: vec![lit(s, false)],
            })
        };
        assert_eq!(t[1], word("a\u{a0}b"));
        assert_eq!(t[2], word("\u{a0}"));
        assert_eq!(
            t[3],
            Tok::Word(Word {
                parts: vec![
                    lit("\u{3000}x\u{2028}", false),
                    lit("q", true),
                    lit("\u{85}", false)
                ]
            })
        );
        assert_eq!(t[4..], [Tok::Newline]);
    }

    #[test]
    fn substitution_and_redirections() {
        let t = toks("x=`{ls '}' {a}} >[2=1] <[0] f >[3=]");
        assert_eq!(
            t[0],
            Tok::Word(Word {
                parts: vec![lit("x=", false), Part::Subst("ls '}' {a}".into())]
            })
        );
        assert_eq!(t[1], Tok::Redir(RedirKind::Dup(1), 2));
        assert_eq!(t[2], Tok::Redir(RedirKind::In, 0));
        assert_eq!(t[4], Tok::Redir(RedirKind::Close, 3));
    }

    #[test]
    fn incomplete() {
        for s in ["echo 'abc", "x=`{ls", "echo $", ">[2"] {
            let mut l = Lexer::new(s);
            let mut r = l.next();
            while let Ok(Token { tok, .. }) = &r {
                if *tok == Tok::Eof {
                    break;
                }
                r = l.next();
            }
            assert_eq!(r.unwrap_err(), SyntaxError::Incomplete, "{s}");
        }
    }
}
