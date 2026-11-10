//! The parse tree, and its text: the form `whatis`, `depends`, `rules` and
//! `-d` print, and that a sub-mash re-reads.

use std::fmt::{self, Display, Formatter, Write};
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub enum Word {
    /// Text; `glob` if unquoted and holding `*`, `?` or `[`.
    Lit {
        text: String,
        quoted: bool,
        glob: bool,
    },
    /// `$name`.
    Var(String),
    /// `$"name`: the value joined by spaces.
    VarQ(String),
    /// `$n`: a match from a make rule.
    Arg(String),
    /// Two pieces written together: a free caret.
    Join(Box<Word>, Box<Word>),
    /// `a ^ b`.
    Caret(Box<Word>, Box<Word>),
    /// `(expr)`.
    Expr(Box<Expr>),
    /// `` `{...} ``, `"{...}`, `<{...}` or `>{...}`.
    Gen(Gen, Rc<Body>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gen {
    Backq,
    Quote,
    In,
    Out,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Word(Word),
    Caret(Box<Expr>, Box<Expr>),
    Hd(Box<Expr>),
    Tl(Box<Expr>),
    Len(Box<Expr>),
    Not(Box<Expr>),
    Cons(Box<Expr>, Box<Expr>),
    Eq(Box<Expr>, Box<Expr>),
    Ne(Box<Expr>, Box<Expr>),
    Match(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RKind {
    In,
    Out,
    Append,
    InOut,
}

impl RKind {
    pub fn text(self) -> &'static str {
        match self {
            RKind::In => "<",
            RKind::Out => ">",
            RKind::Append => ">>",
            RKind::InOut => "<>",
        }
    }

    /// Which of standard input (1) and output (2) this redirects.
    pub fn sides(self) -> u8 {
        match self {
            RKind::In => 1,
            RKind::Out | RKind::Append => 2,
            RKind::InOut => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Redir {
    pub kind: RKind,
    pub word: Word,
}

/// Commands in braces, each `true` if it ends with `&`.
pub type Body = Vec<(Cmd, bool)>;

/// The commands of a dependency or rule; `list` for the `:{...}` form.
#[derive(Debug, Clone, PartialEq)]
pub struct Recipe {
    pub list: bool,
    pub body: Rc<Body>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Simple {
        words: Vec<Word>,
        redirs: Vec<Redir>,
    },
    /// `{...}`, or `@{...}` (a new scope) when `scope`.
    Group {
        body: Rc<Body>,
        redirs: Vec<Redir>,
        scope: bool,
    },
    Pipe(Vec<Cmd>),
    For {
        var: String,
        list: Vec<Word>,
        body: Box<Cmd>,
    },
    If {
        cond: Expr,
        then: Box<Cmd>,
        els: Option<Box<Cmd>>,
    },
    While {
        cond: Expr,
        body: Box<Cmd>,
    },
    Case {
        subject: Expr,
        arms: Vec<(Expr, Cmd, bool)>,
    },
    Assign {
        name: String,
        local: bool,
        value: Vec<Word>,
    },
    Fn {
        name: Word,
        body: Option<Rc<Body>>,
    },
    Rescue {
        name: Word,
        body: Option<Rc<Body>>,
    },
    Depend {
        targets: Vec<Word>,
        deps: Vec<Word>,
        recipe: Option<Recipe>,
    },
    Rule {
        pattern: Word,
        dep: Word,
        recipe: Recipe,
    },
}

impl Word {
    pub fn lit(text: &str, quoted: bool) -> Word {
        Word::Lit {
            text: String::from(text),
            quoted,
            glob: false,
        }
    }
}

/// Characters that make `quote` quote a string.
fn needs_quote(c: char) -> bool {
    c.is_whitespace() || ":#;&|^$=@~`{}()<>*[]?'\"!\\".contains(c)
}

/// `s` in quotes, with quotes and backslashes escaped.
pub fn enquote(s: &str) -> String {
    let mut r = String::from("'");
    for c in s.chars() {
        if c == '\'' || c == '\\' {
            r.push('\\');
        }
        r.push(c);
    }
    r.push('\'');
    r
}

/// `s` as mash input: quoted if it has to be.
pub fn quote(s: &str) -> String {
    if s.is_empty() || s.chars().any(needs_quote) {
        enquote(s)
    } else {
        String::from(s)
    }
}

/// Values for `$1`, `$2` ... while a make rule's text is written for a
/// sub-mash, which does not have them.
pub struct Ctx<'a> {
    pub args: Option<&'a [String]>,
}

const PLAIN: Ctx<'static> = Ctx { args: None };

impl Word {
    pub fn write(&self, f: &mut String, cx: &Ctx) {
        match self {
            Word::Lit { text, quoted, .. } => {
                if *quoted {
                    f.push_str(&enquote(text));
                } else {
                    f.push_str(text);
                }
            }
            Word::Var(s) => {
                let _ = write!(f, "${s}");
            }
            Word::VarQ(s) => {
                let _ = write!(f, "$\"{s}");
            }
            Word::Arg(n) => match cx.args {
                Some(a) => {
                    let v = n.parse::<usize>().ok().and_then(|i| a.get(i));
                    match v {
                        Some(v) => f.push_str(&enquote(v)),
                        None => {
                            let _ = write!(f, "${n}");
                        }
                    }
                }
                None => {
                    let _ = write!(f, "${n}");
                }
            },
            Word::Join(a, b) => {
                a.write(f, cx);
                b.write(f, cx);
            }
            Word::Caret(a, b) => {
                a.write(f, cx);
                f.push_str(" ^ ");
                b.write(f, cx);
            }
            Word::Expr(e) => {
                f.push('(');
                e.write(f, cx, 0);
                f.push(')');
            }
            Word::Gen(g, body) => {
                f.push_str(match g {
                    Gen::Backq => "`",
                    Gen::Quote => "\"",
                    Gen::In => "<",
                    Gen::Out => ">",
                });
                group(f, body, cx);
            }
        }
    }
}

impl Expr {
    fn prec(&self) -> u8 {
        match self {
            Expr::Eq(..) | Expr::Ne(..) | Expr::Match(..) => 1,
            Expr::Cons(..) => 2,
            Expr::Caret(..) => 3,
            Expr::Hd(_) | Expr::Tl(_) | Expr::Len(_) | Expr::Not(_) => 4,
            Expr::Word(_) => 5,
        }
    }

    /// Writes the expression, in parentheses if it binds less tightly than
    /// `min`.
    pub fn write(&self, f: &mut String, cx: &Ctx, min: u8) {
        let paren = self.prec() < min;
        if paren {
            f.push('(');
        }
        let p = self.prec();
        let bin = |f: &mut String, a: &Expr, op: &str, b: &Expr, l: u8, r: u8| {
            a.write(f, cx, l);
            f.push_str(op);
            b.write(f, cx, r);
        };
        match self {
            Expr::Word(w) => w.write(f, cx),
            Expr::Caret(a, b) => bin(f, a, " ^ ", b, p, p + 1),
            Expr::Cons(a, b) => bin(f, a, " :: ", b, p + 1, p),
            Expr::Eq(a, b) => bin(f, a, " == ", b, p, p + 1),
            Expr::Ne(a, b) => bin(f, a, " != ", b, p, p + 1),
            Expr::Match(a, b) => bin(f, a, " ~ ", b, p, p + 1),
            Expr::Hd(a) | Expr::Tl(a) | Expr::Len(a) | Expr::Not(a) => {
                f.push_str(match self {
                    Expr::Hd(_) => "hd ",
                    Expr::Tl(_) => "tl ",
                    Expr::Len(_) => "len ",
                    _ => "! ",
                });
                a.write(f, cx, p);
            }
        }
        if paren {
            f.push(')');
        }
    }
}

fn words(f: &mut String, ws: &[Word], cx: &Ctx) {
    for (i, w) in ws.iter().enumerate() {
        if i > 0 {
            f.push(' ');
        }
        w.write(f, cx);
    }
}

fn redirs(f: &mut String, rs: &[Redir], cx: &Ctx) {
    for r in rs {
        f.push(' ');
        f.push_str(r.kind.text());
        f.push(' ');
        r.word.write(f, cx);
    }
}

/// The commands of a body, each followed by `;` or `&`.
pub fn sequence(f: &mut String, body: &Body, cx: &Ctx) {
    for (i, (c, bg)) in body.iter().enumerate() {
        if i > 0 {
            f.push(' ');
        }
        c.write(f, cx);
        f.push(if *bg { '&' } else { ';' });
    }
}

pub fn group(f: &mut String, body: &Body, cx: &Ctx) {
    if body.is_empty() {
        f.push_str("{ }");
        return;
    }
    f.push_str("{ ");
    sequence(f, body, cx);
    f.push_str(" }");
}

impl Recipe {
    pub fn write(&self, f: &mut String, cx: &Ctx) {
        if self.list {
            f.push(':');
        }
        if self.body.is_empty() {
            f.push_str("{}");
        } else {
            group(f, &self.body, cx);
        }
    }
}

impl Cmd {
    pub fn write(&self, f: &mut String, cx: &Ctx) {
        match self {
            Cmd::Simple {
                words: ws,
                redirs: rs,
            } => {
                words(f, ws, cx);
                redirs(f, rs, cx);
            }
            Cmd::Group {
                body,
                redirs: rs,
                scope,
            } => {
                if *scope {
                    f.push('@');
                }
                group(f, body, cx);
                redirs(f, rs, cx);
            }
            Cmd::Pipe(stages) => {
                for (i, c) in stages.iter().enumerate() {
                    if i > 0 {
                        f.push_str(" | ");
                    }
                    c.write(f, cx);
                }
            }
            Cmd::For { var, list, body } => {
                let _ = write!(f, "for ({var} in ");
                words(f, list, cx);
                f.push_str(") ");
                body.write(f, cx);
            }
            Cmd::If { cond, then, els } => {
                f.push_str("if (");
                cond.write(f, cx, 0);
                f.push_str(") ");
                then.write(f, cx);
                if let Some(e) = els {
                    f.push_str(" else ");
                    e.write(f, cx);
                }
            }
            Cmd::While { cond, body } => {
                f.push_str("while (");
                cond.write(f, cx, 0);
                f.push_str(") ");
                body.write(f, cx);
            }
            Cmd::Case { subject, arms } => {
                f.push_str("case ");
                subject.write(f, cx, 0);
                f.push_str(" {");
                for (pat, c, bg) in arms {
                    f.push(' ');
                    pat.write(f, cx, 0);
                    f.push_str(" => ");
                    c.write(f, cx);
                    f.push(if *bg { '&' } else { ';' });
                }
                f.push_str(" }");
            }
            Cmd::Assign { name, local, value } => {
                f.push_str(name);
                f.push_str(if *local { " :=" } else { " =" });
                if !value.is_empty() {
                    f.push(' ');
                    words(f, value, cx);
                }
            }
            Cmd::Fn { name, body } | Cmd::Rescue { name, body } => {
                f.push_str(if matches!(self, Cmd::Fn { .. }) {
                    "fn "
                } else {
                    "rescue "
                });
                name.write(f, cx);
                if let Some(b) = body {
                    f.push(' ');
                    group(f, b, cx);
                }
            }
            Cmd::Depend {
                targets,
                deps,
                recipe,
            } => {
                words(f, targets, cx);
                f.push_str(" :");
                if !deps.is_empty() {
                    f.push(' ');
                    words(f, deps, cx);
                }
                if let Some(r) = recipe {
                    f.push(' ');
                    r.write(f, cx);
                }
            }
            Cmd::Rule {
                pattern,
                dep,
                recipe,
            } => {
                pattern.write(f, cx);
                f.push_str(" :~ ");
                dep.write(f, cx);
                f.push(' ');
                recipe.write(f, cx);
            }
        }
    }

    pub fn text_with(&self, cx: &Ctx) -> String {
        let mut s = String::new();
        self.write(&mut s, cx);
        s
    }
}

impl Display for Word {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.write(&mut s, &PLAIN);
        f.write_str(&s)
    }
}

impl Display for Expr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.write(&mut s, &PLAIN, 0);
        f.write_str(&s)
    }
}

impl Display for Cmd {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text_with(&PLAIN))
    }
}

/// The text of a body, as `fn` and `whatis` show it.
pub fn body_text(body: &Body) -> String {
    let mut s = String::new();
    sequence(&mut s, body, &PLAIN);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote("plain-word.b"), "plain-word.b");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it\\'s'");
        assert_eq!(quote("a\\b"), "'a\\\\b'");
        assert_eq!(quote("*.c"), "'*.c'");
    }
}
