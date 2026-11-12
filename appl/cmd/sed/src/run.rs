//! The engine: runs a compiled script over a stream of lines.

use std::io::{BufRead, Write};

use regexp::Regex;

use crate::parse::{Addr, Cmd, Op, Piece, Script, Subst};

/// Where input lines come from: the files in order, or one reader. A
/// line of lookahead tells the engine which line is `$`.
/// One input stream.
pub type Reader<'a> = Box<dyn BufRead + 'a>;

/// Opens the next input stream, `None` when there are no more.
type Opener<'a> = Box<dyn FnMut() -> Result<Option<Reader<'a>>, String> + 'a>;

pub struct Input<'a> {
    readers: Opener<'a>,
    cur: Option<Reader<'a>>,
    ahead: Option<(String, bool)>,
    started: bool,
}

impl<'a> Input<'a> {
    /// `next` yields each input in turn, `None` when there are no more.
    pub fn new(next: impl FnMut() -> Result<Option<Reader<'a>>, String> + 'a) -> Input<'a> {
        Input {
            readers: Box::new(next),
            cur: None,
            ahead: None,
            started: false,
        }
    }

    /// Reads one line, without its newline, and whether it had one.
    fn read(&mut self) -> Result<Option<(String, bool)>, String> {
        let mut buf = Vec::new();
        loop {
            if self.cur.is_none() {
                match (self.readers)()? {
                    Some(r) => self.cur = Some(r),
                    None => return Ok(None),
                }
            }
            let r = self.cur.as_mut().expect("a current reader");
            buf.clear();
            let n = r.read_until(b'\n', &mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                self.cur = None;
                continue;
            }
            let nl = buf.last() == Some(&b'\n');
            if nl {
                buf.pop();
            }
            return Ok(Some((String::from_utf8_lossy(&buf).into_owned(), nl)));
        }
    }

    fn fill(&mut self) -> Result<(), String> {
        if !self.started {
            self.started = true;
            self.ahead = self.read()?;
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Option<(String, bool)>, String> {
        self.fill()?;
        let line = self.ahead.take();
        if line.is_some() {
            self.ahead = self.read()?;
        }
        Ok(line)
    }

    fn at_end(&mut self) -> Result<bool, String> {
        self.fill()?;
        Ok(self.ahead.is_none())
    }
}

/// Standard output. A last line without a newline is written without
/// one, unless more output follows it.
struct Out<'a> {
    w: &'a mut dyn Write,
    owe_nl: bool,
}

impl Out<'_> {
    fn write(&mut self, s: &str) -> Result<(), String> {
        if self.owe_nl {
            self.owe_nl = false;
            self.w.write_all(b"\n").map_err(|e| e.to_string())?;
        }
        self.w.write_all(s.as_bytes()).map_err(|e| e.to_string())
    }

    /// Writes `s` and a newline; `nl` false leaves the newline owed.
    fn line(&mut self, s: &str, nl: bool) -> Result<(), String> {
        self.write(s)?;
        if nl {
            self.write("\n")
        } else {
            self.owe_nl = true;
            Ok(())
        }
    }
}

enum Queued {
    Text(String),
    File(String),
}

pub struct Options {
    pub quiet: bool,
}

/// Runs `script` over `input`, writing to `out`; `wfiles` are the open
/// files for `w`, in the order of `script.wfiles`.
pub fn run(
    script: &Script,
    opts: &Options,
    input: &mut Input<'_>,
    out: &mut dyn Write,
    wfiles: &mut [Box<dyn Write + '_>],
) -> Result<(), String> {
    let mut e = Engine {
        out: Out {
            w: out,
            owe_nl: false,
        },
        ps: String::new(),
        nl: true,
        hold: String::new(),
        lineno: 0,
        tflag: false,
        queue: Vec::new(),
        active: vec![false; script.cmds.len()],
    };
    let r = e.cycles(script, opts, input, wfiles);
    // Whatever happened, flush what was written.
    let f = e.out.w.flush().map_err(|e| e.to_string());
    for w in wfiles.iter_mut() {
        w.flush().map_err(|e| e.to_string())?;
    }
    r.and(f)
}

struct Engine<'a> {
    out: Out<'a>,
    /// The pattern space, and whether its last line had a newline.
    ps: String,
    nl: bool,
    hold: String,
    lineno: usize,
    /// Whether an `s` succeeded since a line was last read (by a new
    /// cycle, `n` or `N`) or a `t` was taken.
    tflag: bool,
    queue: Vec<Queued>,
    /// For each two-address command, whether a range is open.
    active: Vec<bool>,
}

/// How a cycle ends.
enum End {
    /// Autoprint, then the next cycle.
    Next,
    /// No autoprint.
    Delete,
    /// No autoprint, and the pattern space is kept with no new line read.
    Restart,
    /// Autoprint, then stop.
    Quit,
    /// Stop with no autoprint.
    Stop,
}

impl Engine<'_> {
    fn cycles(
        &mut self,
        script: &Script,
        opts: &Options,
        input: &mut Input<'_>,
        wfiles: &mut [Box<dyn Write + '_>],
    ) -> Result<(), String> {
        let mut restart = false;
        loop {
            if !restart {
                let Some((line, nl)) = input.next()? else {
                    return Ok(());
                };
                self.ps = line;
                self.nl = nl;
                self.lineno += 1;
                self.tflag = false;
            }
            restart = false;
            let end = self.cycle(script, opts, input, wfiles)?;
            if matches!(end, End::Next | End::Quit) && !opts.quiet {
                self.out.line(&self.ps, self.nl || !input.at_end()?)?;
            }
            self.flush_queue()?;
            match end {
                End::Quit | End::Stop => return Ok(()),
                End::Restart => restart = true,
                End::Next | End::Delete => {}
            }
        }
    }

    fn cycle(
        &mut self,
        script: &Script,
        opts: &Options,
        input: &mut Input<'_>,
        wfiles: &mut [Box<dyn Write + '_>],
    ) -> Result<End, String> {
        let mut pc = 0;
        while let Some(cmd) = script.cmds.get(pc) {
            let last = input.at_end()?;
            if !self.selects(script, pc, cmd, last) {
                pc = match cmd.op {
                    Op::Block(end) => end,
                    _ => pc + 1,
                };
                continue;
            }
            pc += 1;
            match &cmd.op {
                Op::Block(_) | Op::Nop => {}
                Op::Append(t) => self.queue.push(Queued::Text(t.clone())),
                Op::Read(f) => self.queue.push(Queued::File(f.clone())),
                Op::Insert(t) => self.out.line(t, true)?,
                Op::Change(t) => {
                    // A range prints its text once, at its end.
                    if cmd.a2.is_none() || cmd.negate || !self.active[pc - 1] {
                        self.out.line(t, true)?;
                    }
                    return Ok(End::Delete);
                }
                Op::Branch(to, cond) => {
                    if *cond {
                        if !self.tflag {
                            continue;
                        }
                        self.tflag = false;
                    }
                    pc = to.unwrap_or(script.cmds.len());
                }
                Op::Delete => return Ok(End::Delete),
                Op::DeleteFirst => match self.ps.find('\n') {
                    Some(i) => {
                        self.ps.drain(..=i);
                        return Ok(End::Restart);
                    }
                    None => return Ok(End::Delete),
                },
                Op::Get => self.ps.clone_from(&self.hold),
                Op::GetAppend => {
                    self.ps.push('\n');
                    self.ps.push_str(&self.hold);
                }
                Op::Hold => self.hold.clone_from(&self.ps),
                Op::HoldAppend => {
                    self.hold.push('\n');
                    self.hold.push_str(&self.ps);
                }
                Op::Exchange => std::mem::swap(&mut self.ps, &mut self.hold),
                Op::List => {
                    let l = list(&self.ps);
                    self.out.write(&l)?;
                }
                Op::Next => {
                    if !opts.quiet {
                        self.out.line(&self.ps, self.nl || !last)?;
                    }
                    self.flush_queue()?;
                    match input.next()? {
                        Some((line, nl)) => {
                            self.ps = line;
                            self.nl = nl;
                            self.lineno += 1;
                            self.tflag = false;
                        }
                        None => return Ok(End::Stop),
                    }
                }
                Op::NextAppend => {
                    self.flush_queue()?;
                    match input.next()? {
                        Some((line, nl)) => {
                            self.ps.push('\n');
                            self.ps.push_str(&line);
                            self.nl = nl;
                            self.lineno += 1;
                            self.tflag = false;
                        }
                        None => return Ok(End::Stop),
                    }
                }
                Op::Print => self.out.line(&self.ps, self.nl || !last)?,
                Op::PrintFirst => match self.ps.find('\n') {
                    Some(i) => {
                        let first = self.ps[..i].to_string();
                        self.out.line(&first, true)?;
                    }
                    None => self.out.line(&self.ps, self.nl || !last)?,
                },
                Op::Quit => return Ok(End::Quit),
                Op::Subst(s) => {
                    if let Some(new) = subst(&self.ps, &script.regexes[s.re], s) {
                        self.ps = new;
                        self.tflag = true;
                        if s.print {
                            self.out.line(&self.ps, self.nl || !last)?;
                        }
                        if let Some(w) = s.wfile {
                            writeln!(wfiles[w], "{}", self.ps).map_err(|e| e.to_string())?;
                        }
                    }
                }
                Op::Write(w) => {
                    writeln!(wfiles[*w], "{}", self.ps).map_err(|e| e.to_string())?;
                }
                Op::Translate(map) => {
                    self.ps = self
                        .ps
                        .chars()
                        .map(|c| map.iter().find(|m| m.0 == c).map_or(c, |m| m.1))
                        .collect();
                }
                Op::LineNo => self.out.line(&self.lineno.to_string(), true)?,
            }
        }
        Ok(End::Next)
    }

    fn matches(&self, script: &Script, a: Addr, last: bool) -> bool {
        match a {
            Addr::Line(n) => self.lineno == n,
            Addr::Last => last,
            Addr::Re(r) => script.regexes[r].is_match(&self.ps),
        }
    }

    /// Whether command `i` applies to the current line, opening and
    /// closing its range as it goes.
    fn selects(&mut self, script: &Script, i: usize, cmd: &Cmd, last: bool) -> bool {
        let yes = match (cmd.a1, cmd.a2) {
            (None, _) => true,
            (Some(a), None) => self.matches(script, a, last),
            (Some(a1), Some(a2)) => {
                if self.active[i] {
                    let done = match a2 {
                        Addr::Line(n) => self.lineno >= n,
                        a => self.matches(script, a, last),
                    };
                    if done {
                        self.active[i] = false;
                    }
                    true
                } else if self.matches(script, a1, last) {
                    // A line number at or before this line, or $ on the
                    // last line, makes the range one line long. A regular
                    // expression is first tried on the next line.
                    self.active[i] = match a2 {
                        Addr::Line(n) => n > self.lineno,
                        Addr::Last => !last,
                        Addr::Re(_) => true,
                    };
                    true
                } else {
                    false
                }
            }
        };
        yes != cmd.negate
    }

    fn flush_queue(&mut self) -> Result<(), String> {
        for q in std::mem::take(&mut self.queue) {
            match q {
                Queued::Text(t) => self.out.line(&t, true)?,
                // A file that cannot be read adds nothing.
                Queued::File(f) => {
                    if let Ok(data) = std::fs::read(&f) {
                        self.out.write(&String::from_utf8_lossy(&data))?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// The result of `s` on `ps`, or `None` if nothing was replaced.
pub fn subst(ps: &str, re: &Regex, s: &Subst) -> Option<String> {
    let mut out = String::new();
    let mut at = 0; // copied up to here
    let mut from = 0; // search from here
    let mut count = 0;
    let mut done = false;
    // The end of the last non-empty match: an empty match right after it
    // is not a new match.
    let mut prev_end = None;
    while from <= ps.len() {
        let Some(caps) = re.find_at(ps, from) else {
            break;
        };
        let (b, e) = caps[0].expect("a match has a whole-match range");
        if b == e && prev_end == Some(b) {
            prev_end = None;
            match ps[b..].chars().next() {
                Some(c) => {
                    from = b + c.len_utf8();
                    continue;
                }
                None => break,
            }
        }
        count += 1;
        if count >= s.nth {
            out.push_str(&ps[at..b]);
            for p in &s.repl {
                match p {
                    Piece::Lit(t) => out.push_str(t),
                    Piece::Group(n) => {
                        if let Some(Some((x, y))) = caps.get(*n) {
                            out.push_str(&ps[*x..*y]);
                        }
                    }
                }
            }
            at = e;
            done = true;
            if !s.global {
                break;
            }
        }
        if b == e {
            match ps[e..].chars().next() {
                Some(c) => from = e + c.len_utf8(),
                None => break,
            }
            prev_end = None;
        } else {
            from = e;
            prev_end = Some(e);
        }
    }
    if !done {
        return None;
    }
    out.push_str(&ps[at..]);
    Some(out)
}

/// The `l` form of `ps`: printable ASCII as is, `\\`, `\b`, `\n`, `\r`
/// and `\t` escaped, anything else as `\u` and four hex digits. Long
/// lines fold with a `\` after 72 characters.
pub fn list(ps: &str) -> String {
    const WIDTH: usize = 72;
    let mut out = String::new();
    let mut col = 0;
    for c in ps.chars() {
        let esc = match c {
            '\\' => String::from("\\\\"),
            '\u{8}' => String::from("\\b"),
            '\n' => String::from("\\n"),
            '\r' => String::from("\\r"),
            '\t' => String::from("\\t"),
            ' '..='~' => c.to_string(),
            c => format!("\\u{:04x}", c as u32),
        };
        if col + esc.len() > WIDTH {
            out.push_str("\\\n");
            col = 0;
        }
        col += esc.len();
        out.push_str(&esc);
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::compile;

    fn sed_opts(script: &str, quiet: bool, input: &str) -> String {
        let s = compile(script, false).unwrap();
        let mut once = Some(input.as_bytes());
        let mut inp = Input::new(move || {
            Ok(once
                .take()
                .map(|b| Box::new(std::io::BufReader::new(b)) as Box<dyn BufRead>))
        });
        let mut out = Vec::new();
        let mut w: Vec<Box<dyn Write>> = s
            .wfiles
            .iter()
            .map(|_| Box::new(Vec::new()) as Box<dyn Write>)
            .collect();
        run(&s, &Options { quiet }, &mut inp, &mut out, &mut w).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn sed(script: &str, input: &str) -> String {
        sed_opts(script, false, input)
    }

    const ABC: &str = "a\nb\nc\nd\ne\n";

    #[test]
    fn addresses_and_ranges() {
        assert_eq!(sed("2d", ABC), "a\nc\nd\ne\n");
        assert_eq!(sed("$d", ABC), "a\nb\nc\nd\n");
        assert_eq!(sed("2,4d", ABC), "a\ne\n");
        assert_eq!(sed("2,4!d", ABC), "b\nc\nd\n");
        assert_eq!(sed("/b/,/d/d", ABC), "a\ne\n");
        assert_eq!(sed("3,1d", ABC), "a\nb\nd\ne\n");
        assert_eq!(sed("/c/,$d", ABC), "a\nb\n");
        // A regular expression end is first tried on the next line.
        assert_eq!(sed("/a/,/a|b/d", ABC), "c\nd\ne\n");
        // A range that never closes runs to the end, and may reopen.
        assert_eq!(sed("/[bd]/,/x/d", ABC), "a\n");
        assert_eq!(sed("/[ad]/,/[be]/s/$/!/", ABC), "a!\nb!\nc\nd!\ne!\n");
    }

    #[test]
    fn quiet_and_print() {
        assert_eq!(sed_opts("3p", true, ABC), "c\n");
        assert_eq!(sed_opts("$=", true, ABC), "5\n");
        assert_eq!(sed("1!G;h;$!d", "1\n2\n3\n"), "3\n2\n1\n");
        assert_eq!(sed_opts("N;P", true, "1\n2\n3\n4\n"), "1\n3\n");
        assert_eq!(sed("2q", ABC), "a\nb\n");
    }

    #[test]
    fn missing_final_newline() {
        assert_eq!(sed("s/c/C/", "a\nc"), "a\nC");
        assert_eq!(sed("p", "x"), "x\nx");
        assert_eq!(sed("$a\\\nend", "x"), "x\nend\n");
    }

    #[test]
    fn text() {
        assert_eq!(sed("2i\\\nnew", "a\nb\n"), "a\nnew\nb\n");
        assert_eq!(sed("1a\\\nnew", "a\nb\n"), "a\nnew\nb\n");
        assert_eq!(sed("2,3c\\\nmid", ABC), "a\nmid\nd\ne\n");
        assert_eq!(sed("/[bc]/c\\\nX", ABC), "a\nX\nX\nd\ne\n");
        // An appended line comes out before n reads the next.
        assert_eq!(sed_opts("1{a\\\nA\nn;p}", true, "x\ny\n"), "A\ny\n");
    }

    #[test]
    fn hold_space() {
        assert_eq!(sed("h;s/./X/;G;x;g", "ab\n"), "Xb\nab\n");
        assert_eq!(sed("H;$!d;x", "1\n2\n"), "\n1\n2\n");
        assert_eq!(sed("x", "1\n2\n"), "\n1\n");
    }

    #[test]
    fn next_and_delete() {
        assert_eq!(sed("n;d", ABC), "a\nc\ne\n");
        assert_eq!(sed("$!N;s/\\n/-/", ABC), "a-b\nc-d\ne\n");
        // N on the last line ends without printing.
        assert_eq!(sed("N", "1\n2\n3\n"), "1\n2\n");
        // D restarts with what is left, without reading.
        assert_eq!(sed("$!N;P;D", "1\n2\n3\n"), "1\n2\n3\n");
        assert_eq!(sed("$!N;$!D", "1\n2\n3\n4\n"), "3\n4\n");
    }

    #[test]
    fn branches() {
        assert_eq!(sed(":a;s/x//;ta", "xxax\n"), "a\n");
        assert_eq!(sed("/b/b skip;s/$/!/;:skip", "a\nb\n"), "a!\nb\n");
        assert_eq!(sed("b;p", "a\n"), "a\n");
        // t is reset by reading a line.
        assert_eq!(
            sed("s/a/A/;n;t yes;s/$/ no/;b;:yes;s/$/ yes/", "a\nb\n"),
            "A\nb no\n"
        );
        // Join lines ending in \.
        assert_eq!(
            sed(":a;/\\\\$/{N;s/\\\\\\n//;ba}", "a\\\nb\\\nc\nd\n"),
            "abc\nd\n"
        );
    }

    #[test]
    fn substitute() {
        assert_eq!(sed("s/o/0/", "foo\n"), "f0o\n");
        assert_eq!(sed("s/o/0/g", "foo\n"), "f00\n");
        assert_eq!(sed("s/o/0/2", "fooo\n"), "fo0o\n");
        assert_eq!(sed("s/o/0/2g", "fooo\n"), "fo00\n");
        assert_eq!(
            sed("s/([a-z]+)\\.c/<\\1>&/g", "a.c b.c\n"),
            "<a>a.c <b>b.c\n"
        );
        assert_eq!(sed("s/x*/-/g", "abc\n"), "-a-b-c-\n");
        assert_eq!(sed("s/a*/x/g", "baaac\n"), "xbxcx\n");
        assert_eq!(sed("s/b/\\n/", "abc\n"), "a\nc\n");
        assert_eq!(sed("s/^/> /g", "a\n"), "> a\n");
        assert_eq!(sed("$!N;s/^/> /g", "a\nb\n"), "> a\n> b\n");
        assert_eq!(sed_opts("s/a/A/p", true, "a\nb\n"), "A\n");
        assert_eq!(sed("s/z/Z/;s/(b)(c)/\\2\\1/", "abc\n"), "acb\n");
    }

    #[test]
    fn translate_and_list() {
        assert_eq!(sed("y/abc/xyz/", "aabbcc\n"), "xxyyzz\n");
        assert_eq!(list("a\tb\\c\u{1}é"), "a\\tb\\\\c\\u0001\\u00e9\n");
        let long = "x".repeat(80);
        assert_eq!(
            list(&long),
            format!("{}\\\n{}\n", "x".repeat(72), "x".repeat(8))
        );
        assert_eq!(sed_opts("N;l", true, "a\nb\n"), "a\\nb\n");
    }
}
