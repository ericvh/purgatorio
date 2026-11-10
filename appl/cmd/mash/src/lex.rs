//! Tokens of the mash language, after mash(1).
//!
//! A word is unquoted text, `'quoted'` text (a quote or backslash inside is
//! escaped with a backslash), `$name`, `$"name` or `$n`. Pieces written
//! with nothing between them join with a free caret. Newlines are white
//! space: commands end with `;` or `&`, or at the end of the input.

use crate::ast::Word;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kw {
    Case,
    Else,
    Fn,
    For,
    Hd,
    If,
    In,
    Len,
    Rescue,
    Tl,
    While,
}

const KEYWORDS: &[(&str, Kw)] = &[
    ("case", Kw::Case),
    ("else", Kw::Else),
    ("fn", Kw::Fn),
    ("for", Kw::For),
    ("hd", Kw::Hd),
    ("if", Kw::If),
    ("in", Kw::In),
    ("len", Kw::Len),
    ("rescue", Kw::Rescue),
    ("tl", Kw::Tl),
    ("while", Kw::While),
];

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Word(Word),
    Kw(Kw),
    /// `;`
    Semi,
    /// `&`
    Amp,
    /// `|`
    Pipe,
    /// `^`
    Caret,
    /// `@`
    At,
    /// `!`
    Not,
    /// `!=`
    NotEq,
    /// `~`
    Tilde,
    /// `=`
    Eq,
    /// `==`
    EqEq,
    /// `=>`
    Arrow,
    /// `:`
    Colon,
    /// `:=`
    DefEq,
    /// `:~`
    ColonTilde,
    /// `::`
    Cons,
    /// `` ` ``
    Backq,
    /// `"`
    Quote,
    LBrace,
    RBrace,
    LParen,
    RParen,
    /// `<`
    Less,
    /// `>`
    Great,
    /// `>>`
    GreatGreat,
    /// `<>`
    LessGreat,
    Eof,
}

impl Tok {
    /// How the token is written.
    #[cfg(test)]
    pub fn text(&self) -> String {
        let s = match self {
            Tok::Word(w) => return w.to_string(),
            Tok::Kw(k) => KEYWORDS
                .iter()
                .find(|(_, x)| x == k)
                .map_or("?", |(s, _)| s),
            Tok::Semi => ";",
            Tok::Amp => "&",
            Tok::Pipe => "|",
            Tok::Caret => "^",
            Tok::At => "@",
            Tok::Not => "!",
            Tok::NotEq => "!=",
            Tok::Tilde => "~",
            Tok::Eq => "=",
            Tok::EqEq => "==",
            Tok::Arrow => "=>",
            Tok::Colon => ":",
            Tok::DefEq => ":=",
            Tok::ColonTilde => ":~",
            Tok::Cons => "::",
            Tok::Backq => "`",
            Tok::Quote => "\"",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::Less => "<",
            Tok::Great => ">",
            Tok::GreatGreat => ">>",
            Tok::LessGreat => "<>",
            Tok::Eof => "end of input",
        };
        String::from(s)
    }
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\0')
}

/// Characters that are a token by themselves and end a word.
fn is_one(c: char) -> bool {
    ":#;&|^$=@~`{}()<>".contains(c)
}

/// Whether `c` ends an unquoted word.
fn is_term(c: Option<char>) -> bool {
    c.is_none_or(|c| is_one(c) || is_space(c))
}

pub fn is_alpha(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

pub fn is_alnum(c: char) -> bool {
    is_alpha(c) || c.is_ascii_digit()
}

/// Whether `s` is an identifier: a letter or `_`, then letters, digits
/// and `_`.
pub fn ident(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(is_alpha) && c.all(is_alnum)
}

pub struct Lexer {
    src: Vec<char>,
    pos: usize,
    /// The current line, counting from 1.
    pub line: usize,
}

impl Lexer {
    pub fn new(src: &str) -> Lexer {
        Lexer {
            src: src.chars().collect(),
            pos: 0,
            line: 1,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<char> {
        self.src.get(self.pos + 1).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    /// Skips the rest of the current line, after a syntax error.
    pub fn skip_line(&mut self) {
        while let Some(c) = self.bump() {
            if c == '\n' {
                break;
            }
        }
    }

    /// The next token. `Err` is a lexical error to report.
    pub fn next(&mut self) -> Result<Tok, String> {
        loop {
            while self.peek().is_some_and(is_space) {
                self.bump();
            }
            let Some(c) = self.peek() else {
                return Ok(Tok::Eof);
            };
            let two = |l: &mut Lexer, next: char, yes: Tok, no: Tok| {
                l.bump();
                if l.peek() == Some(next) {
                    l.bump();
                    yes
                } else {
                    no
                }
            };
            let tok = match c {
                ':' => {
                    self.bump();
                    match self.peek() {
                        Some(':') => Tok::Cons,
                        Some('=') => Tok::DefEq,
                        Some('~') => Tok::ColonTilde,
                        _ => return Ok(Tok::Colon),
                    }
                }
                '#' => {
                    self.skip_line();
                    continue;
                }
                ';' => Tok::Semi,
                '&' => Tok::Amp,
                '|' => Tok::Pipe,
                '^' => Tok::Caret,
                '@' => Tok::At,
                '~' => Tok::Tilde,
                '`' => Tok::Backq,
                '"' => Tok::Quote,
                '{' => Tok::LBrace,
                '}' => Tok::RBrace,
                '(' => Tok::LParen,
                ')' => Tok::RParen,
                '!' => return Ok(two(self, '=', Tok::NotEq, Tok::Not)),
                '<' => return Ok(two(self, '>', Tok::LessGreat, Tok::Less)),
                '>' => return Ok(two(self, '>', Tok::GreatGreat, Tok::Great)),
                '=' => {
                    self.bump();
                    match self.peek() {
                        Some('>') => Tok::Arrow,
                        Some('=') => Tok::EqEq,
                        _ => return Ok(Tok::Eq),
                    }
                }
                '\\' if self.peek2() == Some('\n') => {
                    self.bump();
                    self.bump();
                    continue;
                }
                _ => return self.word(),
            };
            self.bump();
            return Ok(tok);
        }
    }

    /// A word: one or more pieces joined by free carets.
    fn word(&mut self) -> Result<Tok, String> {
        let mut w: Option<Word> = None;
        loop {
            let piece = if self.peek() == Some('$') {
                self.bump();
                self.dollar()
            } else {
                self.piece()?
            };
            w = Some(match w {
                None => piece,
                Some(l) => Word::Join(Box::new(l), Box::new(piece)),
            });
            let c = self.peek();
            if is_term(c) && c != Some('$') {
                break;
            }
        }
        let w = w.expect("a word has a piece");
        if let Word::Lit {
            text,
            quoted: false,
            ..
        } = &w
            && let Some((_, k)) = KEYWORDS.iter().find(|(s, _)| s == text)
        {
            return Ok(Tok::Kw(*k));
        }
        Ok(Tok::Word(w))
    }

    /// `$name`, `$"name` or `$n`, after the `$`.
    fn dollar(&mut self) -> Word {
        let mut s = String::new();
        if self.peek().is_some_and(|c| c.is_ascii_digit()) {
            while let Some(c) = self.peek().filter(char::is_ascii_digit) {
                s.push(c);
                self.bump();
            }
            return Word::Arg(s);
        }
        let joined = self.peek() == Some('"');
        if joined {
            self.bump();
        }
        if !self.peek().is_some_and(is_alpha) {
            return Word::lit(if joined { "$\"" } else { "$" }, false);
        }
        while let Some(c) = self.peek().filter(|&c| is_alnum(c)) {
            s.push(c);
            self.bump();
        }
        if joined { Word::VarQ(s) } else { Word::Var(s) }
    }

    /// A quoted or unquoted piece of a word.
    fn piece(&mut self) -> Result<Word, String> {
        let mut s = String::new();
        if self.peek() == Some('\'') {
            self.bump();
            loop {
                match self.bump() {
                    None | Some('\'') => break,
                    Some('\\') => match self.peek() {
                        Some(c @ ('\'' | '\\')) => {
                            self.bump();
                            s.push(c);
                        }
                        Some('\n') => {
                            self.bump();
                        }
                        _ => s.push('\\'),
                    },
                    Some('\n') => return Err(String::from("newline in quoted word")),
                    Some(c) => s.push(c),
                }
            }
            return Ok(Word::Lit {
                text: s,
                quoted: true,
                glob: false,
            });
        }
        let mut glob = false;
        while let Some(c) = self.peek() {
            if (is_term(Some(c)) || c == '\'') && !s.is_empty() {
                break;
            }
            glob |= matches!(c, '*' | '[' | '?');
            s.push(c);
            self.bump();
        }
        Ok(Word::Lit {
            text: s,
            quoted: false,
            glob,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        let mut l = Lexer::new(s);
        let mut out = Vec::new();
        loop {
            let t = l.next().unwrap();
            if t == Tok::Eof {
                return out;
            }
            out.push(t);
        }
    }

    fn texts(s: &str) -> Vec<String> {
        toks(s).iter().map(Tok::text).collect()
    }

    #[test]
    fn operators() {
        assert_eq!(
            texts("a:b :: c := d :~ e == f != g => h ~ !x <>f >>g"),
            [
                "a", ":", "b", "::", "c", ":=", "d", ":~", "e", "==", "f", "!=", "g", "=>", "h",
                "~", "!", "x", "<>", "f", ">>", "g"
            ]
        );
        assert_eq!(texts("x=/tmp;y&"), ["x", "=", "/tmp", ";", "y", "&"]);
        assert_eq!(
            texts("`{a} \"{b} @{c}"),
            ["`", "{", "a", "}", "\"", "{", "b", "}", "@", "{", "c", "}"]
        );
    }

    #[test]
    fn words_and_carets() {
        // limbo -$flags $stem.b is limbo -^$flags $stem^.b.
        let t = toks("limbo -$flags $stem.b");
        assert_eq!(t.len(), 3);
        assert_eq!(
            t[1],
            Tok::Word(Word::Join(
                Box::new(Word::lit("-", false)),
                Box::new(Word::Var("flags".into()))
            ))
        );
        assert_eq!(
            t[2],
            Tok::Word(Word::Join(
                Box::new(Word::Var("stem".into())),
                Box::new(Word::lit(".b", false))
            ))
        );
        assert_eq!(texts("a'b c'd"), ["a'b c'd"]);
        assert_eq!(
            texts("'it\\'s' 'a\\\\b' 'a\\qb'"),
            ["'it\\'s'", "'a\\\\b'", "'a\\\\qb'"]
        );
        assert_eq!(
            toks("$\"x $1 $"),
            [
                Tok::Word(Word::VarQ("x".into())),
                Tok::Word(Word::Arg("1".into())),
                Tok::Word(Word::lit("$", false)),
            ]
        );
    }

    #[test]
    fn keywords_comments_and_errors() {
        assert_eq!(
            toks("if 'if' ifx"),
            [
                Tok::Kw(Kw::If),
                Tok::Word(Word::lit("if", true)),
                Tok::Word(Word::lit("ifx", false)),
            ]
        );
        assert_eq!(texts("a # comment\nb \\\nc"), ["a", "b", "c"]);
        let mut l = Lexer::new("'a\nb'");
        assert_eq!(l.next(), Err(String::from("newline in quoted word")));
        let Tok::Word(Word::Lit { glob, .. }) = &toks("*.c")[0] else {
            panic!("not a word");
        };
        assert!(*glob);
    }
}
