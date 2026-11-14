//! Plan 9 regular expressions, with the syntax of regexp(6): literals,
//! `.`, `[...]` and `[^...]` classes with ranges, `^` and `$` line
//! anchors, `*` `+` `?` closures, `|` alternation and `( )` grouping.
//! `\` quotes the next character, and `\n` is a newline. `.` and negated
//! classes never match a newline.
//!
//! A search finds the leftmost match and, among those starting there, the
//! longest. It runs a Pike VM, so time is linear in the text for a given
//! expression and nothing backtracks.
//!
//! One engine serves `grep`, `sed` and the shell's `regex` module.

use std::fmt;

/// A compile error, with Plan 9's wording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err<T>(msg: &str) -> Result<T, Error> {
    Err(Error(String::from(msg)))
}

/// A character class: ranges, possibly negated.
#[derive(Clone, Debug)]
struct Class {
    negated: bool,
    ranges: Vec<(char, char)>,
}

impl Class {
    fn matches(&self, c: char) -> bool {
        if self.negated && c == '\n' {
            return false;
        }
        let inside = self.ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi);
        inside != self.negated
    }
}

#[derive(Debug)]
enum Node {
    Empty,
    Char(char),
    Any,
    Class(Class),
    Bol,
    Eol,
    Cat(Vec<Node>),
    Alt(Box<Node>, Box<Node>),
    Star(Box<Node>),
    Plus(Box<Node>),
    Quest(Box<Node>),
    Group(usize, Box<Node>),
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    groups: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// `cat ('|' cat)*`; `None` for an empty expression.
    fn alt(&mut self) -> Result<Option<Node>, Error> {
        let mut left = self.cat()?;
        while self.peek() == Some('|') {
            self.pos += 1;
            let right = self.cat()?;
            match (left, right) {
                (Some(l), Some(r)) => left = Some(Node::Alt(Box::new(l), Box::new(r))),
                _ => return err("missing operand for |"),
            }
        }
        Ok(left)
    }

    fn cat(&mut self) -> Result<Option<Node>, Error> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let mut atom = self.atom()?;
            while let Some(op @ ('*' | '+' | '?')) = self.peek() {
                self.pos += 1;
                atom = match op {
                    '*' => Node::Star(Box::new(atom)),
                    '+' => Node::Plus(Box::new(atom)),
                    _ => Node::Quest(Box::new(atom)),
                };
            }
            items.push(atom);
        }
        Ok(match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(Node::Cat(items)),
        })
    }

    fn atom(&mut self) -> Result<Node, Error> {
        let Some(c) = self.next() else {
            return err("missing operand");
        };
        Ok(match c {
            '(' => {
                self.groups += 1;
                let n = self.groups;
                let inner = self.alt()?;
                if self.next() != Some(')') {
                    return err("unmatched left paren");
                }
                match inner {
                    Some(e) => Node::Group(n, Box::new(e)),
                    None => return err("missing operand for ()"),
                }
            }
            '*' | '+' | '?' => return Err(Error(format!("missing operand for {c}"))),
            '[' => Node::Class(self.class()?),
            '.' => Node::Any,
            '^' => Node::Bol,
            '$' => Node::Eol,
            '\\' => Node::Char(self.escaped()?),
            c => Node::Char(c),
        })
    }

    /// The character after a `\`.
    fn escaped(&mut self) -> Result<char, Error> {
        match self.next() {
            Some('n') => Ok('\n'),
            Some(c) => Ok(c),
            None => err("trailing backslash"),
        }
    }

    /// The rest of a class after its `[`.
    fn class(&mut self) -> Result<Class, Error> {
        const BAD: &str = "malformed `[]'";
        let negated = self.peek() == Some('^');
        if negated {
            self.pos += 1;
        }
        let mut ranges = Vec::new();
        loop {
            let lo = match self.next() {
                None => return err(BAD),
                Some(']') if ranges.is_empty() => return err(BAD),
                Some(']') => break,
                Some('\\') => self.escaped().or(err(BAD))?,
                Some(c) => c,
            };
            let mut hi = lo;
            if self.peek() == Some('-') && self.chars.get(self.pos + 1).is_some_and(|&c| c != ']') {
                self.pos += 1;
                hi = match self.next() {
                    Some('\\') => self.escaped().or(err(BAD))?,
                    Some(c) => c,
                    None => return err(BAD),
                };
                if hi < lo {
                    return err(BAD);
                }
            }
            ranges.push((lo, hi));
        }
        Ok(Class { negated, ranges })
    }
}

#[derive(Debug)]
enum Inst {
    Char(char),
    Any,
    Class(Class),
    Bol,
    Eol,
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    Match,
}

fn emit(node: Node, prog: &mut Vec<Inst>) {
    match node {
        Node::Empty => {}
        Node::Char(c) => prog.push(Inst::Char(c)),
        Node::Any => prog.push(Inst::Any),
        Node::Class(c) => prog.push(Inst::Class(c)),
        Node::Bol => prog.push(Inst::Bol),
        Node::Eol => prog.push(Inst::Eol),
        Node::Cat(items) => {
            for n in items {
                emit(n, prog);
            }
        }
        Node::Alt(a, b) => {
            let split = prog.len();
            prog.push(Inst::Split(split + 1, 0));
            emit(*a, prog);
            let jmp = prog.len();
            prog.push(Inst::Jmp(0));
            let second = prog.len();
            emit(*b, prog);
            let end = prog.len();
            prog[split] = Inst::Split(split + 1, second);
            prog[jmp] = Inst::Jmp(end);
        }
        Node::Star(e) => {
            let split = prog.len();
            prog.push(Inst::Split(split + 1, 0));
            emit(*e, prog);
            prog.push(Inst::Jmp(split));
            let end = prog.len();
            prog[split] = Inst::Split(split + 1, end);
        }
        Node::Plus(e) => {
            let start = prog.len();
            emit(*e, prog);
            let next = prog.len() + 1;
            prog.push(Inst::Split(start, next));
        }
        Node::Quest(e) => {
            let split = prog.len();
            prog.push(Inst::Split(split + 1, 0));
            emit(*e, prog);
            let end = prog.len();
            prog[split] = Inst::Split(split + 1, end);
        }
        Node::Group(n, e) => {
            prog.push(Inst::Save(2 * n));
            emit(*e, prog);
            prog.push(Inst::Save(2 * n + 1));
        }
    }
}

/// A compiled regular expression.
#[derive(Debug)]
pub struct Regex {
    prog: Vec<Inst>,
    groups: usize,
}

/// The byte ranges of a match: element 0 is the whole match, element `n`
/// the `n`th parenthesized subexpression, `None` where it took no part.
pub type Captures = Vec<Option<(usize, usize)>>;

const UNSET: usize = usize::MAX;

struct Thread {
    pc: usize,
    caps: Vec<usize>,
}

/// The text a search runs over, and per-search scratch state. The text
/// is decoded only as far as the search goes, so a match near the start
/// of a long text costs no more than one in a short text.
struct Vm<'a> {
    prog: &'a [Inst],
    text: Vec<(usize, char)>,
    rest: core::str::CharIndices<'a>,
    from: usize,
    end: usize,
    /// The character before the search start, for `^`.
    before: Option<char>,
    /// The step at which each instruction was last added to a list.
    seen: Vec<usize>,
}

impl Vm<'_> {
    /// Decodes the text up to and including position `i`.
    fn fill(&mut self, i: usize) {
        while self.text.len() <= i {
            match self.rest.next() {
                Some((o, c)) => self.text.push((o + self.from, c)),
                None => break,
            }
        }
    }

    fn offset(&self, i: usize) -> usize {
        self.text.get(i).map_or(self.end, |t| t.0)
    }

    fn bol(&self, i: usize) -> bool {
        let prev = if i == 0 {
            self.before
        } else {
            Some(self.text[i - 1].1)
        };
        prev.is_none_or(|c| c == '\n')
    }

    fn eol(&self, i: usize) -> bool {
        self.text.get(i).is_none_or(|t| t.1 == '\n')
    }

    /// Adds the thread at `pc` to `list`, following the empty-width
    /// instructions at text position `i`.
    fn add(&mut self, list: &mut Vec<Thread>, pc: usize, mut caps: Vec<usize>, i: usize) {
        if self.seen[pc] == i {
            return;
        }
        self.seen[pc] = i;
        match self.prog[pc] {
            Inst::Jmp(to) => self.add(list, to, caps, i),
            Inst::Split(a, b) => {
                self.add(list, a, caps.clone(), i);
                self.add(list, b, caps, i);
            }
            Inst::Save(k) => {
                caps[k] = self.offset(i);
                self.add(list, pc + 1, caps, i);
            }
            Inst::Bol => {
                if self.bol(i) {
                    self.add(list, pc + 1, caps, i);
                }
            }
            Inst::Eol => {
                if self.eol(i) {
                    self.add(list, pc + 1, caps, i);
                }
            }
            _ => list.push(Thread { pc, caps }),
        }
    }
}

impl Regex {
    /// Compiles `pattern`. An empty pattern matches the empty string.
    pub fn new(pattern: &str) -> Result<Regex, Error> {
        let mut p = Parser {
            chars: pattern.chars().collect(),
            pos: 0,
            groups: 0,
        };
        let node = p.alt()?;
        if p.peek().is_some() {
            return err("unmatched right paren");
        }
        let mut prog = vec![Inst::Save(0)];
        emit(node.unwrap_or(Node::Empty), &mut prog);
        prog.push(Inst::Save(1));
        prog.push(Inst::Match);
        Ok(Regex {
            prog,
            groups: p.groups,
        })
    }

    /// The number of parenthesized subexpressions.
    pub fn groups(&self) -> usize {
        self.groups
    }

    /// Whether the expression matches anywhere in `s`.
    pub fn is_match(&self, s: &str) -> bool {
        self.find_at(s, 0).is_some()
    }

    /// The leftmost-longest match in `s`.
    pub fn find(&self, s: &str) -> Option<Captures> {
        self.find_at(s, 0)
    }

    /// The leftmost-longest match in `s` starting at or after byte offset
    /// `from`, which must be a character boundary. `^` still sees the text
    /// before `from`, so it matches there only after a newline.
    pub fn find_at(&self, s: &str, from: usize) -> Option<Captures> {
        let mut vm = Vm {
            prog: &self.prog,
            text: Vec::new(),
            rest: s[from..].char_indices(),
            from,
            end: s.len(),
            before: s[..from].chars().next_back(),
            seen: vec![UNSET; self.prog.len()],
        };
        let slots = 2 * (self.groups + 1);
        let mut clist: Vec<Thread> = Vec::new();
        let mut best: Option<Vec<usize>> = None;
        let mut i = 0;
        loop {
            vm.fill(i + 1);
            if best.is_none() {
                vm.add(&mut clist, 0, vec![UNSET; slots], i);
            } else if clist.is_empty() {
                break;
            }
            let mut nlist = Vec::new();
            for t in clist.drain(..) {
                if best.as_ref().is_some_and(|b| t.caps[0] > b[0]) {
                    continue;
                }
                let c = vm.text.get(i).map(|t| t.1);
                let step = match &self.prog[t.pc] {
                    Inst::Match => {
                        let better = best.as_ref().is_none_or(|b| {
                            t.caps[0] < b[0] || (t.caps[0] == b[0] && t.caps[1] > b[1])
                        });
                        if better {
                            best = Some(t.caps);
                        }
                        continue;
                    }
                    Inst::Char(want) => c == Some(*want),
                    Inst::Any => c.is_some_and(|c| c != '\n'),
                    Inst::Class(cl) => c.is_some_and(|c| cl.matches(c)),
                    _ => false,
                };
                if step {
                    vm.add(&mut nlist, t.pc + 1, t.caps, i + 1);
                }
            }
            clist = nlist;
            if i >= vm.text.len() {
                break;
            }
            i += 1;
        }
        best.map(|caps| {
            caps.chunks(2)
                .map(|p| (p[0] != UNSET && p[1] != UNSET).then_some((p[0], p[1])))
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of the whole match of `re` in `s`.
    fn m<'a>(re: &str, s: &'a str) -> Option<&'a str> {
        let caps = Regex::new(re).unwrap().find(s)?;
        let (a, b) = caps[0].unwrap();
        Some(&s[a..b])
    }

    fn sub<'a>(re: &str, s: &'a str) -> Vec<Option<&'a str>> {
        let caps = Regex::new(re).unwrap().find(s).unwrap();
        caps.iter().map(|c| c.map(|(a, b)| &s[a..b])).collect()
    }

    #[test]
    fn literals_and_dot() {
        assert_eq!(m("abc", "xxabcxx"), Some("abc"));
        assert_eq!(m("a.c", "abc"), Some("abc"));
        assert_eq!(m("a.c", "a\nc"), None);
        assert_eq!(m("abd", "abc"), None);
        assert_eq!(m("", "abc"), Some(""));
        assert_eq!(m("é.", "café!"), Some("é!"));
    }

    #[test]
    fn classes() {
        assert_eq!(m("[a-c]+", "xxbcaz"), Some("bca"));
        assert_eq!(m("[^a-c]+", "abxyc"), Some("xy"));
        assert_eq!(m("[^a]", "a\nb"), Some("b"));
        assert_eq!(m("[\\]x]+", "a]x]b"), Some("]x]"));
        assert_eq!(m("[a\\-z]+", "b-az"), Some("-az"));
        assert_eq!(m("[.*]+", "a.*b"), Some(".*"));
        assert_eq!(m("[\n]", "a\nb"), Some("\n"));
    }

    #[test]
    fn anchors() {
        assert_eq!(m("^a", "ba"), None);
        assert_eq!(m("^b", "a\nb"), Some("b"));
        assert_eq!(m("a$", "ab"), None);
        assert_eq!(m("a$", "a\nb"), Some("a"));
        assert_eq!(m("^$", ""), Some(""));
        let re = Regex::new("^a").unwrap();
        assert!(re.find_at("aa", 1).is_none());
        assert!(re.find_at("a\na", 2).is_some());
    }

    #[test]
    fn closures_and_alternation() {
        assert_eq!(m("ab*", "abbbc"), Some("abbb"));
        assert_eq!(m("ab+", "ac"), None);
        assert_eq!(m("colou?r", "color"), Some("color"));
        assert_eq!(m("a|bc", "xbc"), Some("bc"));
        // Concatenation binds tighter than |.
        assert_eq!(m("ab|cd", "acd"), Some("cd"));
        assert_eq!(m("a(b|c)d", "acd"), Some("acd"));
        assert_eq!(m("a**", "aa"), Some("aa"));
    }

    #[test]
    fn leftmost_longest() {
        assert_eq!(m("a|ab", "abc"), Some("ab"));
        assert_eq!(m("x*", "aaxx"), Some(""));
        assert_eq!(m("(a|ab)(c|bcd)", "abcd"), Some("abcd"));
        assert_eq!(m("b+|ab", "abbb"), Some("ab"));
        assert_eq!(m("\\.[a-z]+", "f.c.rs"), Some(".c"));
    }

    #[test]
    fn subexpressions() {
        assert_eq!(sub("(a+)(b+)?", "xaac"), [Some("aa"), Some("aa"), None]);
        assert_eq!(
            sub("([a-z]+)@([a-z]+)", "mail bob@host now"),
            [Some("bob@host"), Some("bob"), Some("host")]
        );
        assert_eq!(Regex::new("(a)(b(c))").unwrap().groups(), 3);
    }

    #[test]
    fn escapes() {
        assert_eq!(m("a\\.b", "axb a.b"), Some("a.b"));
        assert_eq!(m("\\(x\\)", "(x)"), Some("(x)"));
        assert_eq!(m("a\\nb", "a\nb"), Some("a\nb"));
        assert_eq!(m("\\*", "2*3"), Some("*"));
    }

    #[test]
    fn errors() {
        let e = |re: &str| Regex::new(re).unwrap_err().0;
        assert_eq!(e("*a"), "missing operand for *");
        assert_eq!(e("a|"), "missing operand for |");
        assert_eq!(e("|a"), "missing operand for |");
        assert_eq!(e("(a"), "unmatched left paren");
        assert_eq!(e("a)"), "unmatched right paren");
        assert_eq!(e("()"), "missing operand for ()");
        assert_eq!(e("[a"), "malformed `[]'");
        assert_eq!(e("[]"), "malformed `[]'");
        assert_eq!(e("[z-a]"), "malformed `[]'");
        assert_eq!(e("a\\"), "trailing backslash");
    }

    #[test]
    fn empty_loops_terminate() {
        assert_eq!(m("(a*)*", "aaa"), Some("aaa"));
        assert_eq!(m("(^)*b", "ab"), Some("b"));
        assert_eq!(m("(a?)+", "aa"), Some("aa"));
    }
}
