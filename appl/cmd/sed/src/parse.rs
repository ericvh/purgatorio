//! The script compiler: turns sed's text into a flat list of commands.
//! Blocks become a jump past their `}` and labels are resolved to command
//! indices, so the engine only walks the list.

use regexp::Regex;

/// An address: a line number, the last line, or a regular expression
/// (an index into [`Script::regexes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Addr {
    Line(usize),
    Last,
    Re(usize),
}

/// One piece of an `s` replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Lit(String),
    /// `\0` to `\9`; 0 (or `&`) is the whole match.
    Group(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subst {
    pub re: usize,
    pub repl: Vec<Piece>,
    pub global: bool,
    /// Replace only the `nth` match (1 by default); with `global`, the
    /// `nth` and every one after it.
    pub nth: usize,
    pub print: bool,
    /// An index into [`Script::wfiles`].
    pub wfile: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// `{`: when the address does not select the line, go to the index.
    Block(usize),
    /// `}` and `:label` do nothing when reached.
    Nop,
    Append(String),
    Insert(String),
    Change(String),
    /// `b` (false) or `t` (true); `None` goes to the end of the script.
    Branch(Option<usize>, bool),
    Delete,
    DeleteFirst,
    Get,
    GetAppend,
    Hold,
    HoldAppend,
    List,
    Next,
    NextAppend,
    Print,
    PrintFirst,
    Quit,
    Read(String),
    Subst(Subst),
    Write(usize),
    Exchange,
    Translate(Vec<(char, char)>),
    LineNo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    pub a1: Option<Addr>,
    pub a2: Option<Addr>,
    pub negate: bool,
    pub op: Op,
}

/// A compiled script.
#[derive(Debug)]
pub struct Script {
    pub cmds: Vec<Cmd>,
    pub regexes: Vec<Regex>,
    /// The files `w` and `s///w` write, each named once.
    pub wfiles: Vec<String>,
}

/// Compiles `text`. `global` is `-g`: every `s` replaces all matches.
pub fn compile(text: &str, global: bool) -> Result<Script, String> {
    let mut p = Parser {
        s: text.chars().collect(),
        pos: 0,
        start: 0,
        global,
        script: Script {
            cmds: Vec::new(),
            regexes: Vec::new(),
            wfiles: Vec::new(),
        },
        last_re: None,
        labels: Vec::new(),
        branches: Vec::new(),
        blocks: Vec::new(),
    };
    p.run()?;
    Ok(p.script)
}

struct Parser {
    s: Vec<char>,
    pos: usize,
    /// Where the current command starts, for messages.
    start: usize,
    global: bool,
    script: Script,
    last_re: Option<usize>,
    labels: Vec<(String, usize)>,
    /// Branch commands and the labels they name.
    branches: Vec<(usize, String)>,
    /// Indices of open `{` commands.
    blocks: Vec<usize>,
}

/// Characters that mean something in a regexp(6) pattern.
const SPECIAL: &str = ".*+?[]()|^$\\";

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn blanks(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.pos += 1;
        }
    }

    /// The script line the current command starts on.
    fn line(&self) -> String {
        let b = self.s[..self.start]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |i| i + 1);
        let e = self.s[self.start..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(self.s.len(), |i| self.start + i);
        self.s[b..e].iter().collect()
    }

    /// The rest of the current script line from the cursor.
    fn rest(&self) -> String {
        self.s[self.pos..]
            .iter()
            .take_while(|&&c| c != '\n')
            .collect()
    }

    fn run(&mut self) -> Result<(), String> {
        loop {
            while matches!(self.peek(), Some(' ' | '\t' | '\n' | ';')) {
                self.pos += 1;
            }
            let Some(c) = self.peek() else { break };
            self.start = self.pos;
            if c == '#' {
                while !matches!(self.bump(), None | Some('\n')) {}
                continue;
            }
            self.command()?;
        }
        if !self.blocks.is_empty() {
            return Err(String::from("too many {'s"));
        }
        for &(at, ref name) in &self.branches {
            let Some(&(_, to)) = self.labels.iter().find(|(l, _)| l == name) else {
                return Err(format!("undefined label: '{name}'"));
            };
            if let Op::Branch(target, _) = &mut self.script.cmds[at].op {
                *target = Some(to);
            }
        }
        Ok(())
    }

    fn address(&mut self) -> Result<Option<Addr>, String> {
        match self.peek() {
            Some('0'..='9') => {
                let mut n = 0usize;
                while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
                    n = n.saturating_mul(10).saturating_add(d as usize);
                    self.pos += 1;
                }
                if n == 0 {
                    return Err(String::from("line no 0 is illegal address"));
                }
                Ok(Some(Addr::Line(n)))
            }
            Some('$') => {
                self.pos += 1;
                Ok(Some(Addr::Last))
            }
            Some('/') => {
                self.pos += 1;
                Ok(Some(Addr::Re(self.regex('/')?)))
            }
            Some('\\') => {
                self.pos += 1;
                match self.bump() {
                    Some(d) if d != '\n' && d != '\\' => Ok(Some(Addr::Re(self.regex(d)?))),
                    _ => Err(format!("bad regular expression delimiter: {}", self.line())),
                }
            }
            _ => Ok(None),
        }
    }

    /// Reads a regular expression up to an unescaped `delim` and compiles
    /// it; an empty one means the previous one.
    fn regex(&mut self, delim: char) -> Result<usize, String> {
        let mut pat = String::new();
        loop {
            match self.bump() {
                None => {
                    return Err(format!(
                        "no closing {delim} in regular expression: {}",
                        self.line()
                    ));
                }
                Some('\n') => return Err(format!("too much text: {}", self.line())),
                Some(c) if c == delim => break,
                Some('\\') => match self.bump() {
                    // \delim is the delimiter itself, kept quoted when it
                    // is also special in a pattern.
                    Some(c) if c == delim && !SPECIAL.contains(c) => pat.push(c),
                    Some('\n') => pat.push_str("\\n"),
                    Some(c) => {
                        pat.push('\\');
                        pat.push(c);
                    }
                    None => pat.push('\\'),
                },
                Some(c) => pat.push(c),
            }
        }
        if pat.is_empty() {
            return self
                .last_re
                .ok_or_else(|| String::from("First RE in address may not be null"));
        }
        let re = Regex::new(&pat).map_err(|e| format!("{e} '{pat}'"))?;
        self.script.regexes.push(re);
        let n = self.script.regexes.len() - 1;
        self.last_re = Some(n);
        Ok(n)
    }

    fn command(&mut self) -> Result<(), String> {
        let a1 = self.address()?;
        let mut a2 = None;
        if a1.is_some() && self.peek() == Some(',') {
            self.pos += 1;
            self.blanks();
            a2 = self.address()?;
            if a2.is_none() {
                return Err(format!("expected address: {}", self.line()));
            }
        }
        self.blanks();
        let mut negate = false;
        while self.peek() == Some('!') {
            negate = true;
            self.pos += 1;
            self.blanks();
        }
        let Some(c) = self.bump() else {
            return Err(format!("missing command: {}", self.line()));
        };
        if a2.is_some() && matches!(c, 'a' | 'i' | 'q' | '=') {
            return Err(format!("only one address allowed:  '{}'", self.line()));
        }
        if a1.is_some() && matches!(c, '}' | ':') {
            return Err(format!("did not expect address:{}", self.line()));
        }
        let here = self.script.cmds.len();
        let op = match c {
            '{' => {
                self.blocks.push(here);
                Op::Block(0)
            }
            '}' => {
                let Some(open) = self.blocks.pop() else {
                    return Err(format!("too many }}'s: {}", self.line()));
                };
                self.script.cmds[open].op = Op::Block(here);
                Op::Nop
            }
            'a' => Op::Append(self.text()),
            'i' => Op::Insert(self.text()),
            'c' => Op::Change(self.text()),
            ':' => {
                self.blanks();
                let name = self.label();
                if name.is_empty() {
                    return Err(format!("null label: '{}'", self.line()));
                }
                if self.labels.iter().any(|(l, _)| *l == name) {
                    return Err(format!("duplicate label: '{name}'"));
                }
                self.labels.push((name, here));
                Op::Nop
            }
            'b' | 't' => {
                self.blanks();
                let name = self.label();
                if !name.is_empty() {
                    self.branches.push((here, name));
                }
                Op::Branch(None, c == 't')
            }
            'r' => Op::Read(self.filename("r")?),
            'w' => {
                let f = self.filename("w")?;
                Op::Write(self.wfile(f))
            }
            's' => Op::Subst(self.subst()?),
            'y' => Op::Translate(self.translate()?),
            'd' => Op::Delete,
            'D' => Op::DeleteFirst,
            'g' => Op::Get,
            'G' => Op::GetAppend,
            'h' => Op::Hold,
            'H' => Op::HoldAppend,
            'l' => Op::List,
            'n' => Op::Next,
            'N' => Op::NextAppend,
            'p' => Op::Print,
            'P' => Op::PrintFirst,
            'q' => Op::Quit,
            'x' => Op::Exchange,
            '=' => Op::LineNo,
            c => return Err(format!("unrecognised command: '{}' ({c})", self.line())),
        };
        let ends_line = matches!(
            op,
            Op::Append(_) | Op::Insert(_) | Op::Change(_) | Op::Read(_) | Op::Write(_)
        );
        self.script.cmds.push(Cmd { a1, a2, negate, op });
        if c != '{' && !ends_line {
            self.blanks();
            if !matches!(self.peek(), None | Some('\n' | ';' | '}' | '#')) {
                return Err(format!("leftover junk: {}", self.rest()));
            }
        }
        Ok(())
    }

    /// A label: up to a blank, `;`, `}` or the end of the line.
    fn label(&mut self) -> String {
        let mut name = String::new();
        while let Some(c) = self.peek() {
            if matches!(c, ' ' | '\t' | '\n' | ';' | '}') {
                break;
            }
            name.push(c);
            self.pos += 1;
        }
        name
    }

    /// The rest of the line, after blanks, for `r` and `w`.
    fn filename(&mut self, cmd: &str) -> Result<String, String> {
        self.blanks();
        let name = self.rest();
        self.pos += name.chars().count();
        if name.is_empty() {
            return Err(format!("no filename in {cmd} command: {}", self.line()));
        }
        Ok(name)
    }

    fn wfile(&mut self, name: String) -> usize {
        match self.script.wfiles.iter().position(|f| *f == name) {
            Some(i) => i,
            None => {
                self.script.wfiles.push(name);
                self.script.wfiles.len() - 1
            }
        }
    }

    /// The text of `a`, `i` and `c`: either `\`, a newline and lines that
    /// end in `\` to continue, or (as a convenience) the rest of the line.
    /// A `\` quotes the next character.
    fn text(&mut self) -> String {
        self.blanks();
        if self.peek() == Some('\\') {
            self.pos += 1;
            self.blanks();
            if self.peek() == Some('\n') {
                self.pos += 1;
            }
        }
        let mut t = String::new();
        while let Some(c) = self.bump() {
            match c {
                '\n' => break,
                '\\' => match self.bump() {
                    Some(c) => t.push(c),
                    None => break,
                },
                c => t.push(c),
            }
        }
        t
    }

    fn subst(&mut self) -> Result<Subst, String> {
        let delim = match self.bump() {
            Some(d) if d != '\n' && d != '\\' => d,
            _ => return Err(format!("bad delimiter in s command: {}", self.line())),
        };
        let re = self.regex(delim)?;
        let mut repl = Vec::new();
        let mut lit = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => {
                    return Err(format!(
                        "no closing {delim} in replacement text: {}",
                        self.line()
                    ));
                }
                Some(c) if c == delim => break,
                Some('&') => {
                    if !lit.is_empty() {
                        repl.push(Piece::Lit(std::mem::take(&mut lit)));
                    }
                    repl.push(Piece::Group(0));
                }
                Some('\\') => match self.bump() {
                    Some(d @ '0'..='9') => {
                        if !lit.is_empty() {
                            repl.push(Piece::Lit(std::mem::take(&mut lit)));
                        }
                        repl.push(Piece::Group(d as usize - '0' as usize));
                    }
                    Some('n' | '\n') => lit.push('\n'),
                    Some(c) => lit.push(c),
                    None => {
                        return Err(format!(
                            "no closing {delim} in replacement text: {}",
                            self.line()
                        ));
                    }
                },
                Some(c) => lit.push(c),
            }
        }
        if !lit.is_empty() {
            repl.push(Piece::Lit(lit));
        }
        let mut s = Subst {
            re,
            repl,
            global: self.global,
            nth: 1,
            print: false,
            wfile: None,
        };
        let mut nth = None;
        loop {
            match self.peek() {
                Some('g') => s.global = true,
                Some('p') => s.print = true,
                Some('0'..='9') => {
                    let mut n = 0usize;
                    while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
                        n = n.saturating_mul(10).saturating_add(d as usize);
                        self.pos += 1;
                    }
                    if n == 0 || nth.is_some() {
                        return Err(format!("bad number in s command: {}", self.line()));
                    }
                    nth = Some(n);
                    continue;
                }
                Some('w') => {
                    self.pos += 1;
                    self.blanks();
                    let name = self.rest();
                    self.pos += name.chars().count();
                    if name.is_empty() {
                        return Err(format!("no filename in s with w: {}", self.line()));
                    }
                    s.wfile = Some(self.wfile(name));
                    break;
                }
                _ => break,
            }
            self.pos += 1;
        }
        s.nth = nth.unwrap_or(1);
        Ok(s)
    }

    /// One side of `y`, up to `delim`; `\\`, `\n` and `\delim` are quoted.
    fn yside(&mut self, delim: char) -> Option<Vec<char>> {
        let mut v = Vec::new();
        loop {
            match self.bump()? {
                '\n' => return None,
                c if c == delim => return Some(v),
                '\\' => match self.bump()? {
                    'n' => v.push('\n'),
                    c => v.push(c),
                },
                c => v.push(c),
            }
        }
    }

    fn translate(&mut self) -> Result<Vec<(char, char)>, String> {
        let delim = match self.bump() {
            Some(d) if d != '\n' && d != '\\' => d,
            _ => return Err(format!("expected args: {}", self.line())),
        };
        let Some(from) = self.yside(delim) else {
            return Err(format!("no lhs terminator: {}", self.line()));
        };
        let Some(to) = self.yside(delim) else {
            return Err(format!("no rhs: {}", self.line()));
        };
        if from.len() != to.len() {
            return Err(format!("y command needs same length sets: {}", self.line()));
        }
        Ok(from.into_iter().zip(to).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(s: &str) -> Vec<Op> {
        compile(s, false)
            .unwrap()
            .cmds
            .into_iter()
            .map(|c| c.op)
            .collect()
    }

    fn err(s: &str) -> String {
        compile(s, false).unwrap_err()
    }

    #[test]
    fn simple_commands() {
        assert_eq!(
            ops("p;d\nx ; =  # comment\nq"),
            [Op::Print, Op::Delete, Op::Exchange, Op::LineNo, Op::Quit]
        );
        assert_eq!(ops("  \n;;"), []);
    }

    #[test]
    fn addresses() {
        let s = compile("3p\n$d\n/a/,/b/!p\n2,$ s/x/y/\n\\,x,p", false).unwrap();
        let a: Vec<_> = s.cmds.iter().map(|c| (c.a1, c.a2, c.negate)).collect();
        assert_eq!(
            a,
            [
                (Some(Addr::Line(3)), None, false),
                (Some(Addr::Last), None, false),
                (Some(Addr::Re(0)), Some(Addr::Re(1)), true),
                (Some(Addr::Line(2)), Some(Addr::Last), false),
                (Some(Addr::Re(3)), None, false),
            ]
        );
        // An empty regular expression is the last one compiled.
        let s = compile("/a/p;//d", false).unwrap();
        assert_eq!(s.cmds[1].a1, Some(Addr::Re(0)));
    }

    #[test]
    fn blocks_and_labels() {
        assert_eq!(
            ops("/x/{p;d}\n:top\nb top;t"),
            [
                Op::Block(3),
                Op::Print,
                Op::Delete,
                Op::Nop,
                Op::Nop,
                Op::Branch(Some(4), false),
                Op::Branch(None, true),
            ]
        );
        assert_eq!(ops("b end\n:end"), [Op::Branch(Some(1), false), Op::Nop]);
    }

    #[test]
    fn text_commands() {
        assert_eq!(
            ops("a\\\none\\\ntwo\ni\\\n  lead\nc gone"),
            [
                Op::Append(String::from("one\ntwo")),
                Op::Insert(String::from("  lead")),
                Op::Change(String::from("gone")),
            ]
        );
        assert_eq!(ops("a\\"), [Op::Append(String::new())]);
    }

    #[test]
    fn substitute() {
        let s = compile("s/(a+)b/[\\1&\\n]/2gpw out", false).unwrap();
        assert_eq!(
            s.cmds[0].op,
            Op::Subst(Subst {
                re: 0,
                repl: vec![
                    Piece::Lit(String::from("[")),
                    Piece::Group(1),
                    Piece::Group(0),
                    Piece::Lit(String::from("\n]")),
                ],
                global: true,
                nth: 2,
                print: true,
                wfile: Some(0),
            })
        );
        assert_eq!(s.wfiles, ["out"]);
        // -g makes every s global; \delim is the delimiter.
        let s = compile("s,a\\,b,\\&\\,,", true).unwrap();
        let Op::Subst(sub) = &s.cmds[0].op else {
            panic!()
        };
        assert!(sub.global);
        assert_eq!(sub.repl, [Piece::Lit(String::from("&,"))]);
        assert!(s.regexes[0].is_match("a,b"));
        // A quoted special delimiter stays literal.
        let s = compile("s.a\\.b.x.", false).unwrap();
        assert!(!s.regexes[0].is_match("axb"));
    }

    #[test]
    fn files_and_translate() {
        let s = compile("w f1\ns/a/b/w f1\nr in;put", false).unwrap();
        assert_eq!(s.wfiles, ["f1"]);
        assert_eq!(s.cmds[2].op, Op::Read(String::from("in;put")));
        assert_eq!(
            ops("y/ab\\//\\n\\\\c/"),
            [Op::Translate(vec![('a', '\n'), ('b', '\\'), ('/', 'c')])]
        );
    }

    #[test]
    fn errors() {
        assert_eq!(err("k"), "unrecognised command: 'k' (k)");
        assert_eq!(err("0p"), "line no 0 is illegal address");
        assert_eq!(err("1,2q"), "only one address allowed:  '1,2q'");
        assert_eq!(err("pq"), "leftover junk: q");
        assert_eq!(err("p}"), "too many }'s: p}");
        assert_eq!(err("{p"), "too many {'s");
        assert_eq!(err("1}"), "did not expect address:1}");
        assert_eq!(err(":"), "null label: ':'");
        assert_eq!(err(":a\n:a"), "duplicate label: 'a'");
        assert_eq!(err("b nowhere"), "undefined label: 'nowhere'");
        assert_eq!(err("//p"), "First RE in address may not be null");
        assert_eq!(err("s/a/b"), "no closing / in replacement text: s/a/b");
        assert_eq!(err("s/a"), "no closing / in regular expression: s/a");
        assert_eq!(err("s/*/x/"), "missing operand for * '*'");
        assert_eq!(err("s/a/b/w"), "no filename in s with w: s/a/b/w");
        assert_eq!(err("s/a/b/0"), "bad number in s command: s/a/b/0");
        assert_eq!(err("w"), "no filename in w command: w");
        assert_eq!(err("y/ab/c/"), "y command needs same length sets: y/ab/c/");
        assert_eq!(err("y/ab"), "no lhs terminator: y/ab");
        assert_eq!(err("y/ab/cd"), "no rhs: y/ab/cd");
        assert_eq!(err("y"), "expected args: y");
        assert_eq!(err("1,p"), "expected address: 1,p");
    }
}
