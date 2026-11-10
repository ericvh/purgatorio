//! fmt [-j] [-l n] [-i n] [file ...]: fill and indent text. Lines with the
//! same leading white space are joined into paragraphs and refilled to `n`
//! columns (`-l` or `-w`, default 70) indented by `-i` spaces. `-j` only
//! folds long lines. Empty lines and leading white space are kept, and an
//! empty line separates the input files.

use std::io::{self, BufRead, BufReader, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

/// Display width of `s` starting at column `col`, with tabs every 8.
fn width(s: &str, col: usize) -> usize {
    let mut w = col;
    for c in s.chars() {
        w = if c == '\t' { (w + 8) & !7 } else { w + 1 };
    }
    w - col
}

struct Fmt<W: Write> {
    out: W,
    length: usize,
    indent: String,
    join: bool,
    /// The paragraph being filled: its leading white space and words.
    lead: Option<String>,
    words: Vec<String>,
}

impl<W: Write> Fmt<W> {
    fn flush(&mut self) -> io::Result<()> {
        let Some(lead) = self.lead.take() else {
            return Ok(());
        };
        let start = format!("{}{lead}", self.indent);
        let start_w = width(&start, 0);
        let mut line = start.clone();
        let mut col = start_w;
        let mut empty = true;
        for w in self.words.drain(..) {
            let ww = width(&w, col + 1);
            if !empty && col + 1 + ww > self.length {
                writeln!(self.out, "{line}")?;
                line = start.clone();
                col = start_w;
                empty = true;
            }
            if !empty {
                line.push(' ');
                col += 1;
            }
            col += width(&w, col);
            line.push_str(&w);
            empty = false;
        }
        writeln!(self.out, "{line}")
    }

    fn line(&mut self, l: &str) -> io::Result<()> {
        if l.trim().is_empty() {
            self.flush()?;
            return writeln!(self.out);
        }
        let body = l.trim_start_matches([' ', '\t']);
        let lead = &l[..l.len() - body.len()];
        if !self.join || self.lead.as_deref() != Some(lead) {
            self.flush()?;
            self.lead = Some(String::from(lead));
        }
        self.words.extend(
            body.split([' ', '\t'])
                .filter(|w| !w.is_empty())
                .map(String::from),
        );
        Ok(())
    }

    fn file(&mut self, r: &mut dyn BufRead) -> io::Result<()> {
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if r.read_until(b'\n', &mut buf)? == 0 {
                return self.flush();
            }
            let l = String::from_utf8_lossy(&buf);
            self.line(l.trim_end_matches(['\n', '\r']))?;
        }
    }
}

fn num(arg: &mut Arg) -> Result<usize, Error> {
    let s = arg.earg()?;
    s.parse().map_err(|_| arg.usage())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "fmt [-j] [-i indent] [-l length] [file...]");
    let (mut length, mut indent, mut join) = (70, 0, true);
    while let Some(o) = arg.opt() {
        match o {
            'l' | 'w' => length = num(&mut arg)?,
            'i' => indent = num(&mut arg)?,
            'j' => join = false,
            _ => return Err(arg.usage()),
        }
    }
    let files = arg.argv();
    let mut f = Fmt {
        out: io::BufWriter::new(io::stdout().lock()),
        length,
        indent: " ".repeat(indent),
        join,
        lead: None,
        words: Vec::new(),
    };
    let mut failed = false;
    if files.is_empty() {
        f.file(&mut io::stdin().lock())?;
    }
    for (i, name) in files.iter().enumerate() {
        if i > 0 {
            writeln!(f.out)?;
        }
        match std::fs::File::open(name) {
            Ok(file) => f.file(&mut BufReader::new(file))?,
            Err(_) => {
                f.out.flush()?;
                eprintln!("fmt: {name}: {}", purgatorio::errstr());
                failed = true;
            }
        }
    }
    f.out.flush()?;
    if failed {
        purgatorio::exits("fail:errors");
    }
    Ok(())
}
