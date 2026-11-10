//! The namespace side of archiving: adding files and trees to an archive,
//! and unpacking entries into the current directory.

use std::fs::File;
use std::io::{Read, Write};

use purgatorio::abi::mode;
use purgatorio::{DMDIR, Dir};

use crate::ustar::{Header, Kind, Reader, Writer};

/// Reports `prog: msg` on standard error.
pub fn warn(prog: &str, msg: &str) {
    let _ = std::io::stdout().flush();
    eprintln!("{prog}: {msg}");
}

/// Options for adding files to an archive.
pub struct Put<'a> {
    pub prog: &'a str,
    /// Add the contents of directories too.
    pub recurse: bool,
    /// Print each name on standard error.
    pub verbose: bool,
    /// Set when any file could not be added.
    pub failed: bool,
}

impl Put<'_> {
    fn fail(&mut self, msg: &str) {
        warn(self.prog, msg);
        self.failed = true;
    }

    /// Adds `path`, and its contents if it is a directory and `recurse` is
    /// set. Problems with single files are reported and skipped; an error
    /// writing the archive is returned.
    pub fn add<W: Write>(&mut self, w: &mut Writer<W>, path: &str) -> Result<(), String> {
        let d = match purgatorio::stat(path) {
            Ok(d) => d,
            Err(e) => {
                self.fail(&format!("{path}: {e}"));
                return Ok(());
            }
        };
        let name = if path.len() > 1 {
            path.trim_end_matches('/')
        } else {
            path
        };
        if d.qid.is_dir() {
            self.add_dir(w, name, &d)
        } else {
            self.add_file(w, name, &d)
        }
    }

    fn header(&self, name: &str, d: &Dir) -> Header {
        let kind = if d.qid.is_dir() {
            Kind::Dir
        } else {
            Kind::File
        };
        let mut h = Header::new(name, kind, d.mode & 0o777, d.length, d.mtime.into());
        h.uname = d.uid.clone();
        h.gname = d.gid.clone();
        h
    }

    fn put_header<W: Write>(&mut self, w: &mut Writer<W>, h: &Header) -> Result<bool, String> {
        if h.pack().is_err() {
            self.fail(&format!("{}: name too long", h.name));
            return Ok(false);
        }
        w.header(h)?;
        if self.verbose {
            eprintln!("{}", h.name);
        }
        Ok(true)
    }

    fn add_file<W: Write>(&mut self, w: &mut Writer<W>, name: &str, d: &Dir) -> Result<(), String> {
        let mut f = match File::open(name) {
            Ok(f) => f,
            Err(_) => {
                self.fail(&format!("{name}: {}", purgatorio::errstr()));
                return Ok(());
            }
        };
        let h = self.header(name, d);
        if !self.put_header(w, &h)? {
            return Ok(());
        }
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => w
                    .data(&buf[..n])
                    .map_err(|e| purgatorio::Error::from(e).0)?,
                Err(_) => {
                    self.fail(&format!("{name}: {}", purgatorio::errstr()));
                    break;
                }
            }
        }
        w.end_entry().map_err(|e| purgatorio::Error::from(e).0)
    }

    fn add_dir<W: Write>(&mut self, w: &mut Writer<W>, name: &str, d: &Dir) -> Result<(), String> {
        let entries = if self.recurse {
            match std::fs::read_dir(name) {
                Ok(rd) => {
                    let mut v: Vec<String> = rd
                        .filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    v.sort();
                    v
                }
                Err(_) => {
                    self.fail(&format!("{name}: {}", purgatorio::errstr()));
                    return Ok(());
                }
            }
        } else {
            Vec::new()
        };
        let h = self.header(name, d);
        if !self.put_header(w, &h)? {
            return Ok(());
        }
        w.end_entry().map_err(|e| purgatorio::Error::from(e).0)?;
        let base = name.trim_end_matches('/');
        for e in entries {
            self.add(w, &format!("{base}/{e}"))?;
        }
        Ok(())
    }
}

/// Whether `name` is one of `pats` or inside one of them. No patterns
/// select everything.
pub fn selected(name: &str, pats: &[String]) -> bool {
    pats.is_empty()
        || pats.iter().any(|p| {
            let p = p.trim_end_matches('/');
            name == p || (name.starts_with(p) && name[p.len()..].starts_with('/'))
        })
}

/// Options and state for unpacking an archive.
pub struct Extract<'a> {
    pub prog: &'a str,
    /// Keep existing files instead of overwriting them.
    pub keep: bool,
    /// Extract absolute names as they are rather than relative to `.`.
    pub rooted: bool,
    /// Set modification times from the archive.
    pub mtimes: bool,
    /// Print each name on standard error.
    pub verbose: bool,
    pub failed: bool,
    /// Directories whose mode and mtime are set once their contents are in.
    dirs: Vec<(String, u32, u32)>,
}

impl<'a> Extract<'a> {
    pub fn new(prog: &'a str) -> Extract<'a> {
        Extract {
            prog,
            keep: false,
            rooted: false,
            mtimes: true,
            verbose: false,
            failed: false,
            dirs: Vec::new(),
        }
    }

    fn fail(&mut self, msg: &str) {
        warn(self.prog, msg);
        self.failed = true;
    }

    /// The name to create for an archive name, or `None` to skip it.
    fn local(&self, name: &str) -> Option<String> {
        let name = if self.rooted {
            name
        } else {
            name.trim_start_matches(['/', '#'])
        };
        if name.is_empty() || name.split('/').any(|c| c == "..") && !self.rooted {
            return None;
        }
        Some(String::from(name))
    }

    /// Creates the missing parent directories of `path`.
    fn parents(&mut self, path: &str) -> Result<(), String> {
        let mut at = 0;
        while let Some(i) = path[at..].find('/') {
            let p = &path[..at + i];
            at += i + 1;
            if p.is_empty() || purgatorio::stat(p).is_ok() {
                continue;
            }
            purgatorio::create(p, mode::OREAD, DMDIR | 0o777).map_err(|e| format!("{p}: {e}"))?;
        }
        Ok(())
    }

    /// Unpacks the entry `h`, whose data `r` is positioned at.
    pub fn entry<R: Read>(&mut self, r: &mut Reader<R>, h: &Header) -> Result<(), String> {
        if !matches!(h.kind, Kind::File | Kind::Dir) {
            return Ok(());
        }
        let Some(path) = self.local(&h.name) else {
            self.fail(&format!("{}: bad file name", h.name));
            return Ok(());
        };
        if let Err(e) = self.parents(&path) {
            self.fail(&e);
            return Ok(());
        }
        if self.verbose {
            eprintln!("{path}");
        }
        let existing = purgatorio::stat(&path).ok();
        if h.kind == Kind::Dir {
            match existing {
                Some(d) if !d.qid.is_dir() => {
                    self.fail(&format!("{path}: not a directory"));
                    return Ok(());
                }
                Some(_) => {}
                None => {
                    // Writable until its contents are in; `finish` sets the
                    // archived mode.
                    if let Err(e) =
                        purgatorio::create(&path, mode::OREAD, DMDIR | 0o700 | (h.mode & 0o777))
                    {
                        self.fail(&format!("{path}: {e}"));
                        return Ok(());
                    }
                }
            }
            self.dirs.push((path, h.mode, h.mtime as u32));
            return Ok(());
        }
        if existing.is_some() && self.keep {
            return Ok(());
        }
        if existing.as_ref().is_some_and(|d| d.qid.is_dir()) {
            self.fail(&format!("{path}: is a directory"));
            return Ok(());
        }
        let mut f = match purgatorio::create(&path, mode::OWRITE, h.mode & 0o777) {
            Ok(fd) => File::from(fd),
            Err(e) => {
                self.fail(&format!("{path}: {e}"));
                return Ok(());
            }
        };
        if let Err(e) = r.copy_data(&mut f) {
            // A short archive is fatal; a failed write is not.
            if e.contains("end of archive") {
                return Err(e);
            }
            self.fail(&format!("{path}: {e}"));
            return Ok(());
        }
        drop(f);
        let mut d = Dir::null();
        d.mode = h.mode & 0o777;
        if self.mtimes {
            d.mtime = h.mtime as u32;
        }
        if let Err(e) = purgatorio::wstat(&path, &d) {
            self.fail(&format!("{path}: {e}"));
        }
        Ok(())
    }

    /// Sets the mode and mtime of the extracted directories, deepest first.
    pub fn finish(&mut self) {
        let mut dirs = std::mem::take(&mut self.dirs);
        dirs.sort_by(|a, b| b.0.cmp(&a.0));
        for (path, perm, mtime) in dirs {
            let mut d = Dir::null();
            d.mode = DMDIR | (perm & 0o777);
            if self.mtimes {
                d.mtime = mtime;
            }
            if let Err(e) = purgatorio::wstat(&path, &d) {
                self.fail(&format!("{path}: {e}"));
            }
        }
    }
}
