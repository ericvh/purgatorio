//! `Dump` and `Load`: acme's layout saved to a file and read back, as
//! acme(1) has them. The default file is `$home/acme.dump`; `acme -l
//! file` loads one at start.
//!
//! The file is acme's own text format (not Plan 9's). After a first line
//! naming the format come the directory, the fonts and then a record per
//! column and per window. Positions are kept in ten-thousandths of the
//! screen, so a dump loads on a screen of another size. A record's last
//! numbers count the runes of the texts that follow it, each followed by
//! a newline: a tag, and the body when it cannot be read back from a
//! file (an unnamed or modified window, or `+Errors`).
//!
//! ```text
//! acme dump 1
//! dir /usr/me
//! font /fonts/default.font
//! fixed /fonts/mono/unicode.13.font
//! c 0 15                         column at x 0, tag of 15 runes
//! New Delcol win
//! w 0 0 0 0 f 0 0 0 31 0         column, y, dot, kind, font, tab,
//! /tmp/x Del Snarf | Look Edit   indent, tag and body runes
//! ```
//!
//! A window's kind is `f` (read its file again), `b` (its body is in the
//! dump), `m` (the same, and modified), `z` (another view of the window
//! before it with the same name), or `x` (two more lines follow, a
//! directory and a command that makes the window again, as `win` sets
//! with acme(4)'s `dumpdir` and `dump`).

use crate::ui::{Kind, Text, Window, is_scratch};
use crate::{Acme, Tid};

const MAGIC: &str = "acme dump 1";
const SCALE: i64 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Col {
    pub x: i64,
    pub tag: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Win {
    pub col: usize,
    pub y: i64,
    pub q0: usize,
    pub q1: usize,
    pub kind: char,
    /// 0 for the main font, 1 for the fixed one.
    pub font: usize,
    pub tab: usize,
    pub indent: bool,
    pub tag: String,
    pub body: String,
    /// For kind `x`: the directory and the command.
    pub dumpdir: String,
    pub dump: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub dir: String,
    pub font: String,
    pub fixed: String,
    pub cols: Vec<Col>,
    pub wins: Vec<Win>,
}

pub fn format(s: &State) -> String {
    let mut out = format!(
        "{MAGIC}\ndir {}\nfont {}\nfixed {}\n",
        s.dir, s.font, s.fixed
    );
    for c in &s.cols {
        out.push_str(&format!("c {} {}\n{}\n", c.x, c.tag.chars().count(), c.tag));
    }
    for w in &s.wins {
        let body = matches!(w.kind, 'b' | 'm');
        out.push_str(&format!(
            "w {} {} {} {} {} {} {} {} {} {}\n{}\n",
            w.col,
            w.y,
            w.q0,
            w.q1,
            w.kind,
            w.font,
            w.tab,
            u8::from(w.indent),
            w.tag.chars().count(),
            if body { w.body.chars().count() } else { 0 },
            w.tag
        ));
        if body {
            out.push_str(&w.body);
            out.push('\n');
        }
        if w.kind == 'x' {
            out.push_str(&format!("{}\n{}\n", w.dumpdir, w.dump));
        }
    }
    out
}

/// Reads a dump.
pub fn parse(s: &str) -> Result<State, String> {
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    let line = |i: &mut usize| -> Option<String> {
        if *i >= cs.len() {
            return None;
        }
        let start = *i;
        while *i < cs.len() && cs[*i] != '\n' {
            *i += 1;
        }
        let l: String = cs[start..*i].iter().collect();
        *i += 1;
        Some(l)
    };
    let runes = |i: &mut usize, n: usize| -> Result<String, String> {
        if *i + n > cs.len() || cs.get(*i + n) != Some(&'\n') {
            return Err(String::from("dump file truncated"));
        }
        let t: String = cs[*i..*i + n].iter().collect();
        *i += n + 1;
        Ok(t)
    };
    let bad = || String::from("bad dump file");
    if line(&mut i).as_deref() != Some(MAGIC) {
        return Err(String::from("not an acme dump file"));
    }
    let mut st = State::default();
    let field = |l: Option<String>, key: &str| -> Result<String, String> {
        l.and_then(|l| l.strip_prefix(key).map(|v| v.trim_start().to_string()))
            .ok_or_else(bad)
    };
    st.dir = field(line(&mut i), "dir")?;
    st.font = field(line(&mut i), "font")?;
    st.fixed = field(line(&mut i), "fixed")?;
    while let Some(l) = line(&mut i) {
        let f: Vec<&str> = l.split(' ').collect();
        let num = |k: usize| -> Result<i64, String> {
            f.get(k).and_then(|v| v.parse().ok()).ok_or_else(bad)
        };
        match f.first() {
            Some(&"c") if f.len() == 3 => {
                let x = num(1)?;
                let tag = runes(&mut i, num(2)? as usize)?;
                st.cols.push(Col { x, tag });
            }
            Some(&"w") if f.len() == 11 => {
                let kind = f[5].chars().next().ok_or_else(bad)?;
                if !"fbmzx".contains(kind) {
                    return Err(bad());
                }
                let col = num(1)? as usize;
                if col >= st.cols.len() {
                    return Err(bad());
                }
                let tag = runes(&mut i, num(9)? as usize)?;
                let body = if matches!(kind, 'b' | 'm') {
                    runes(&mut i, num(10)? as usize)?
                } else {
                    String::new()
                };
                let (dumpdir, dump) = if kind == 'x' {
                    (line(&mut i).ok_or_else(bad)?, line(&mut i).ok_or_else(bad)?)
                } else {
                    (String::new(), String::new())
                };
                st.wins.push(Win {
                    col,
                    y: num(2)?,
                    q0: num(3)? as usize,
                    q1: num(4)? as usize,
                    kind,
                    font: num(6)? as usize,
                    tab: num(7)? as usize,
                    indent: num(8)? != 0,
                    tag,
                    body,
                    dumpdir,
                    dump,
                });
            }
            Some(&"") if f.len() == 1 => {}
            _ => return Err(bad()),
        }
    }
    Ok(st)
}

impl Acme {
    /// The file `Dump` and `Load` use when given none.
    fn dump_file(&self, arg: &str) -> String {
        if !arg.is_empty() {
            return self.resolve(&self.cwd, arg);
        }
        let home = purgatorio::env::get("home")
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| self.cwd.clone());
        crate::clean(&format!("{home}/acme.dump"))
    }

    /// The layout as a [`State`].
    fn state(&self) -> State {
        let r = self.screen.r;
        let (w, h) = (i64::from(r.dx().max(1)), i64::from(r.dy().max(1)));
        let mut st = State {
            dir: self.cwd.clone(),
            font: self.f.name.clone(),
            fixed: self.fix_name.clone(),
            ..State::default()
        };
        let mut seen: Vec<(usize, String)> = Vec::new();
        for (ci, c) in self.cols.iter().enumerate() {
            st.cols.push(Col {
                x: i64::from(c.r.min.x - r.min.x) * SCALE / w,
                tag: c.tag.b.all(),
            });
            for win in &c.wins {
                let name = win.name();
                let key = win.body.b.key();
                let kind = if win.fs.dump.is_some() {
                    'x'
                } else if seen.iter().any(|(k, n)| *k == key && *n == name) {
                    'z'
                } else if name.is_empty() || is_scratch(&name) {
                    'b'
                } else if win.modified() {
                    'm'
                } else {
                    'f'
                };
                seen.push((key, name));
                st.wins.push(Win {
                    col: ci,
                    y: i64::from(win.y - r.min.y) * SCALE / h,
                    q0: win.body.q0,
                    q1: win.body.q1,
                    kind,
                    font: usize::from(win.body.font != 0),
                    tab: win.body.tab.unwrap_or(0),
                    indent: win.indent,
                    tag: win.tag.b.all(),
                    body: if matches!(kind, 'b' | 'm') {
                        win.body.b.all()
                    } else {
                        String::new()
                    },
                    dumpdir: win.fs.dumpdir.clone().unwrap_or_else(|| win.dir(&self.cwd)),
                    dump: win.fs.dump.clone().unwrap_or_default(),
                });
            }
        }
        st
    }

    /// `Dump [file]`: writes the layout to the file.
    pub(crate) fn dump(&mut self, arg: &str) {
        let path = self.dump_file(arg);
        let s = format(&self.state());
        if std::fs::write(&path, s.as_bytes()).is_err() {
            let e = purgatorio::errstr();
            self.error("", &format!("Dump: {path}: {e}"));
        }
    }

    /// `Load [file]`: replaces the layout with the one in the file.
    /// Modified windows are named first, as `Exit` names them; a second
    /// `Load` goes ahead.
    pub(crate) fn load(&mut self, arg: &str) {
        let path = self.dump_file(arg);
        let st = match std::fs::read_to_string(&path) {
            Ok(s) => match parse(&s) {
                Ok(st) => st,
                Err(e) => {
                    self.error("", &format!("Load: {path}: {e}"));
                    return;
                }
            },
            Err(_) => {
                let e = purgatorio::errstr();
                self.error("", &format!("Load: {path}: {e}"));
                return;
            }
        };
        let modified: Vec<String> = self
            .cols
            .iter()
            .flat_map(|c| c.wins.iter())
            .filter(|w| w.modified())
            .map(Window::name)
            .collect();
        if !modified.is_empty() && !self.exit_warned {
            for n in modified {
                self.error("", &format!("{n} modified"));
            }
            self.exit_warned = true;
            return;
        }
        self.exit_warned = false;
        self.restore(st);
    }

    fn restore(&mut self, st: State) {
        if std::env::set_current_dir(&st.dir).is_ok() {
            self.cwd = st.dir.clone();
        }
        if !st.fixed.is_empty() {
            self.fix_name = st.fixed.clone();
        }
        let r = self.screen.r;
        let (w, h) = (i64::from(r.dx()), i64::from(r.dy()));
        self.cols.clear();
        self.seltext = None;
        self.typing = None;
        self.click = None;
        for c in &st.cols {
            let mut tag = Text::with(Kind::Tag, &c.tag);
            tag.dirty = true;
            let x = r.min.x + (c.x.clamp(0, SCALE) * w / SCALE) as i32;
            self.cols.push(crate::ui::Column {
                tag,
                wins: Vec::new(),
                r: purgatorio::draw::Rect::new(x, r.min.y, r.max.x, r.max.y),
            });
        }
        if self.cols.is_empty() {
            self.new_col();
        }
        if let Some(c) = self.cols.first_mut() {
            c.r.min.x = r.min.x;
        }
        // Columns left to right, each at least where the last began.
        for i in 1..self.cols.len() {
            let lo = self.cols[i - 1].r.min.x;
            if self.cols[i].r.min.x < lo {
                self.cols[i].r.min.x = lo;
            }
        }
        self.place();
        let fh = self.fh();
        let mut loaded: Vec<(String, u64)> = Vec::new();
        for d in &st.wins {
            if d.kind == 'x' {
                // The command makes its window again.
                let t = Tid::Col(d.col);
                let cmd = format!("cd {}; {}", crate::quote(&d.dumpdir), d.dump);
                let _ = self.start(t, &cmd, false);
                continue;
            }
            let name = d.tag.split_whitespace().next().unwrap_or("").to_string();
            let id = self.next_id;
            self.next_id += 1;
            let mut body = Text::new(Kind::Body);
            let mut is_dir = false;
            match d.kind {
                'z' => match loaded
                    .iter()
                    .find(|(n, _)| *n == name)
                    .and_then(|(_, i)| self.win(*i))
                {
                    Some(o) => {
                        body.b = o.body.b.share();
                        is_dir = o.is_dir;
                    }
                    None => {
                        if let Ok((s, dir)) = Acme::read(&name) {
                            body.b.load(&s);
                            is_dir = dir;
                        }
                    }
                },
                'f' => {
                    if !name.is_empty()
                        && let Ok((s, dir)) = Acme::read(&name)
                    {
                        body.b.load(&s);
                        is_dir = dir;
                    }
                }
                _ => {
                    body.b.load(&d.body);
                    if d.kind == 'm' {
                        body.b.set_dirty();
                    }
                }
            }
            body.font = if d.font != 0 {
                let fix = self.fix_name.clone();
                self.font_index_or_main(&fix)
            } else {
                0
            };
            body.tab = (d.tab > 0).then_some(d.tab);
            body.set_select(d.q0, d.q1);
            let mut win = Window::new(id, body);
            win.is_dir = is_dir;
            win.indent = d.indent;
            win.tag.b.insert(0, &d.tag);
            win.y = r.min.y + (d.y.clamp(0, SCALE) * h / SCALE) as i32;
            let n = win.tag.b.len();
            win.tag.set_select(n, n);
            let ci = d.col.min(self.cols.len() - 1);
            let c = &mut self.cols[ci];
            let at = c.wins.iter().take_while(|o| o.y <= win.y).count();
            c.wins.insert(at, win);
            loaded.push((name, id));
        }
        for c in &mut self.cols {
            c.place(fh, &mut self.f);
        }
        for (_, id) in loaded {
            self.retag(id);
            let q = self.win(id).map_or(0, |w| w.body.q0);
            self.show(Tid::Body(id), q);
        }
        self.full = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let st = State {
            dir: "/usr/me".into(),
            font: "/fonts/default.font".into(),
            fixed: "/fonts/mono/unicode.13.font".into(),
            cols: vec![
                Col {
                    x: 0,
                    tag: "New Delcol win ".into(),
                },
                Col {
                    x: 5000,
                    tag: "New Delcol win \nodd".into(),
                },
            ],
            wins: vec![
                Win {
                    col: 0,
                    y: 100,
                    q0: 1,
                    q1: 3,
                    kind: 'f',
                    font: 0,
                    tab: 0,
                    indent: false,
                    tag: "/tmp/x Del Snarf | Look ".into(),
                    body: String::new(),
                    dumpdir: String::new(),
                    dump: String::new(),
                },
                Win {
                    col: 1,
                    y: 4000,
                    q0: 0,
                    q1: 0,
                    kind: 'm',
                    font: 1,
                    tab: 8,
                    indent: true,
                    tag: "/tmp/é Del | ".into(),
                    body: "two\nlines\n".into(),
                    dumpdir: String::new(),
                    dump: String::new(),
                },
                Win {
                    col: 1,
                    y: 7000,
                    q0: 0,
                    q1: 0,
                    kind: 'x',
                    font: 0,
                    tab: 0,
                    indent: false,
                    tag: "/usr/me/-host Del | ".into(),
                    body: String::new(),
                    dumpdir: "/usr/me".into(),
                    dump: "win".into(),
                },
            ],
        };
        let s = format(&st);
        assert!(s.starts_with("acme dump 1\ndir /usr/me\n"), "{s}");
        assert_eq!(parse(&s), Ok(st));
        assert_eq!(parse("junk\n"), Err("not an acme dump file".into()));
        assert_eq!(
            parse("acme dump 1\ndir /\nfont f\nfixed g\nc 0 99\nshort\n"),
            Err("dump file truncated".into())
        );
        assert_eq!(
            parse("acme dump 1\ndir /\nfont f\nfixed g\nw 3 0 0 0 f 0 0 0 0 0\n\n"),
            Err("bad dump file".into())
        );
    }
}
