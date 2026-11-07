//! Command-line option parsing in the style of Inferno's `arg` module:
//! single-letter options, which may be grouped (`-lp`), an option argument
//! either attached (`-ofile`) or in the next word, and `--` to end options.
//!
//! ```ignore
//! let mut arg = Arg::new(args, "ls [-dlp] [file ...]");
//! while let Some(c) = arg.opt() {
//!     match c {
//!         'l' => long = true,
//!         _ => return Err(arg.usage()),
//!     }
//! }
//! let files = arg.argv();
//! ```

use crate::Error;

pub struct Arg {
    args: Vec<String>,
    /// Index of the word being scanned.
    i: usize,
    /// Byte offset of the next option letter within `args[i]`, or 0 when
    /// between words.
    pos: usize,
    usage: String,
}

impl Arg {
    /// Starts parsing `args` (argv\[0\] first). `usage` is the synopsis
    /// shown by [`Arg::usage`].
    pub fn new(args: Vec<String>, usage: &str) -> Arg {
        Arg {
            args,
            i: 1,
            pos: 0,
            usage: String::from(usage),
        }
    }

    /// The program name, argv\[0\].
    pub fn progname(&self) -> &str {
        self.args.first().map_or("", String::as_str)
    }

    /// The next option letter, or `None` when the options end.
    pub fn opt(&mut self) -> Option<char> {
        if self.pos == 0 {
            let w = self.args.get(self.i)?;
            if w == "--" {
                self.i += 1;
                return None;
            }
            if !w.starts_with('-') || w == "-" {
                return None;
            }
            self.pos = 1;
        }
        let w = &self.args[self.i];
        let c = w[self.pos..].chars().next()?;
        self.pos += c.len_utf8();
        if self.pos >= w.len() {
            self.i += 1;
            self.pos = 0;
        }
        Some(c)
    }

    /// The argument of the option just returned: the rest of its word, or
    /// the next word.
    pub fn arg(&mut self) -> Option<String> {
        let w = self.args.get(self.i)?;
        let out = if self.pos > 0 {
            String::from(&w[self.pos..])
        } else {
            w.clone()
        };
        self.i += 1;
        self.pos = 0;
        Some(out)
    }

    /// Like [`Arg::arg`], but a missing argument is a usage error.
    pub fn earg(&mut self) -> Result<String, Error> {
        self.arg().ok_or_else(|| self.usage())
    }

    /// The `usage: ...` error to return for bad options.
    pub fn usage(&self) -> Error {
        Error(format!("usage: {}", self.usage))
    }

    /// The words left after the options.
    pub fn argv(self) -> Vec<String> {
        let skip = self.i + usize::from(self.pos > 0);
        self.args.into_iter().skip(skip).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Arg;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| String::from(*s)).collect()
    }

    #[test]
    fn grouped_and_arguments() {
        let mut a = Arg::new(v(&["x", "-lp", "-ofile", "-n", "3", "rest", "-q"]), "x");
        assert_eq!(a.opt(), Some('l'));
        assert_eq!(a.opt(), Some('p'));
        assert_eq!(a.opt(), Some('o'));
        assert_eq!(a.arg().as_deref(), Some("file"));
        assert_eq!(a.opt(), Some('n'));
        assert_eq!(a.earg().unwrap(), "3");
        assert_eq!(a.opt(), None);
        assert_eq!(a.argv(), ["rest", "-q"]);
    }

    #[test]
    fn dashes() {
        let mut a = Arg::new(v(&["x", "-a", "--", "-b"]), "x");
        assert_eq!(a.opt(), Some('a'));
        assert_eq!(a.opt(), None);
        assert_eq!(a.argv(), ["-b"]);
        let mut a = Arg::new(v(&["x", "-", "y"]), "x");
        assert_eq!(a.opt(), None);
        assert_eq!(a.argv(), ["-", "y"]);
        let mut a = Arg::new(v(&["x", "-o"]), "x [-o arg]");
        assert_eq!(a.opt(), Some('o'));
        assert_eq!(a.earg().unwrap_err().0, "usage: x [-o arg]");
    }
}
