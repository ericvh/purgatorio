//! install/inst [-chtuvF] [-r root] package [prefix ...]: install a
//! package made by install/create into the tree at `root` (default `/`),
//! as inst(8). With prefixes, only the files below them are installed
//! (the package's `/wrap` record always is).
//!
//! A file that is already there is replaced only if it is what an earlier
//! install of the package left (its MD5 matches the package's record in
//! `root/wrap`); a file changed since, or one the package never had, is
//! left alone with a message, unless `-F`. `-h` lists the names in the
//! package instead; `-t` sets each file's modification time from the
//! package, `-u` its owner and group too; `-v` names each directory as it
//! is made. An update's `remove` list is removed afterwards, again only
//! where the files are as installed (or with `-F`). An error stops inst
//! unless `-c`.

use std::io::{BufReader, Write};

use purgatorio::abi::mode;
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Dir, Error};
use wrap::{Hdr, Reader, Unit, quote};

const USAGE: &str = "install/inst [-chtuvF] [-r root] package [prefix ...]";

/// Inferno's inst prints `Usage:` and raises `fail: usage`, with a space.
fn usage() -> ! {
    eprintln!("Usage: {USAGE}");
    purgatorio::exits("fail: usage");
}

struct Inst {
    root: String,
    times: bool,
    owners: bool,
    verbose: bool,
    force: bool,
    keep_going: bool,
    errors: usize,
    /// What `root/wrap/name` held before this install.
    installed: Option<Vec<Unit>>,
    /// The files an update's `remove` names.
    remove: Vec<String>,
}

impl Inst {
    fn fail(&mut self, msg: String) -> Result<(), Error> {
        eprintln!("install/inst: {msg}");
        self.errors += 1;
        if self.keep_going {
            Ok(())
        } else {
            Err(Error::from("error"))
        }
    }

    fn path(&self, name: &str) -> String {
        proto::mkpath(&self.root, name)
    }

    fn set_attrs(&mut self, path: &str, h: &Hdr) -> Result<(), Error> {
        let mut d = Dir::null();
        d.mode = h.mode;
        if self.times || self.owners {
            d.mtime = h.mtime;
        }
        if self.owners {
            d.gid = h.gid.clone();
        }
        if let Err(e) = purgatorio::wstat(path, &d) {
            return self.fail(format!("can't set modes of {}: {e}", quote(path)));
        }
        if self.owners {
            let mut d = Dir::null();
            d.uid = h.uid.clone();
            // Most file servers refuse a new owner; Inferno's inst goes on.
            let _ = purgatorio::wstat(path, &d);
        }
        Ok(())
    }

    fn dir(&mut self, h: &Hdr) -> Result<(), Error> {
        let path = self.path(&h.name);
        if let Some(name) = h.name.strip_prefix("/wrap/")
            && !name.contains('/')
            && self.installed.is_none()
        {
            self.installed = Some(wrap::units(&path));
        }
        match purgatorio::stat(&path) {
            Ok(d) if d.mode & DMDIR != 0 => return Ok(()),
            Ok(_) => return self.fail(format!("{}: not a directory", quote(&path))),
            Err(_) => {}
        }
        if self.verbose {
            println!("{}", quote(&h.name));
        }
        match purgatorio::create(&path, mode::OREAD, DMDIR | (h.mode & 0o777)) {
            Ok(_) => self.set_attrs(&path, h),
            Err(e) => self.fail(format!("can't create {}: {e}", quote(&path))),
        }
    }

    /// Removes the files an update dropped, if they are as installed.
    fn remove_files(&mut self) {
        for name in std::mem::take(&mut self.remove) {
            let path = self.path(&name);
            if purgatorio::stat(&path).is_err() {
                continue;
            }
            if !self.force {
                let local = wrap::md5_file(&path).unwrap_or_default();
                let units = self.installed.as_deref().unwrap_or(&[]);
                if wrap::installed_digest(units, &name) != Some(local.as_str()) {
                    eprintln!(
                        "install/inst: {}: locally modified; not removed",
                        quote(&name)
                    );
                    continue;
                }
            }
            if let Err(e) = purgatorio::remove(&path) {
                let _ = self.fail(format!("can't remove {}: {e}", quote(&path)));
            }
        }
    }

    fn file(&mut self, h: &Hdr, data: &[u8]) -> Result<(), Error> {
        let path = self.path(&h.name);
        if purgatorio::stat(&path).is_ok() {
            if !self.force && !h.name.starts_with("/wrap/") {
                let local = wrap::md5_file(&path).unwrap_or_default();
                if local == wrap::hex(&wrap::md5(data)) {
                    return self.set_attrs(&path, h);
                }
                let units = self.installed.as_deref().unwrap_or(&[]);
                match wrap::installed_digest(units, &h.name) {
                    Some(d) if d == local => {}
                    Some(_) => {
                        eprintln!(
                            "install/inst: {}: locally modified; not installed",
                            quote(&h.name)
                        );
                        return Ok(());
                    }
                    None => {
                        eprintln!(
                            "install/inst: {}: already exists; not installed",
                            quote(&h.name)
                        );
                        return Ok(());
                    }
                }
            }
            // The old file may be read only, as the /wrap files are.
            let _ = purgatorio::remove(&path);
        }
        let fd = match purgatorio::create(&path, mode::OWRITE, (h.mode & 0o777) | 0o200) {
            Ok(fd) => fd,
            Err(e) => return self.fail(format!("can't create {}: {e}", quote(&path))),
        };
        if let Err(e) = std::fs::File::from(fd).write_all(data) {
            return self.fail(format!("{}: write: {}", quote(&path), Error::from(e)));
        }
        self.set_attrs(&path, h)
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut inst = Inst {
        root: String::from("/"),
        times: false,
        owners: false,
        verbose: false,
        force: false,
        keep_going: false,
        errors: 0,
        installed: None,
        remove: Vec::new(),
    };
    let mut list = false;
    while let Some(c) = arg.opt() {
        match c {
            'c' => inst.keep_going = true,
            'h' => list = true,
            't' => inst.times = true,
            'u' => inst.owners = true,
            'v' => inst.verbose = true,
            'F' => inst.force = true,
            'r' => inst.root = arg.arg().unwrap_or_else(|| usage()),
            _ => usage(),
        }
    }
    let mut rest = arg.argv();
    if rest.is_empty() {
        usage();
    }
    let package = rest.remove(0);
    let prefixes = rest;
    let f = std::fs::File::open(&package)
        .map_err(|e| Error(format!("{package}: {}", Error::from(e))))?;
    let mut r = Reader::new(BufReader::new(f));
    while let Some(h) = r.next_hdr().map_err(|e| Error(format!("{package}: {e}")))? {
        if list {
            println!("{}", quote(&h.name));
            continue;
        }
        let wanted = h.name == "/wrap"
            || h.name.starts_with("/wrap/")
            || prefixes.is_empty()
            || prefixes.iter().any(|p| {
                let p = p.trim_end_matches('/');
                h.name == p || h.name.starts_with(&format!("{p}/")) || p.is_empty()
            });
        if !wanted {
            continue;
        }
        let res = if h.mode & DMDIR != 0 {
            inst.dir(&h)
        } else {
            let data = r.data().map_err(|e| Error(format!("{package}: {e}")))?;
            if h.name.starts_with("/wrap/") && h.name.ends_with("/remove") {
                let text = String::from_utf8_lossy(&data);
                inst.remove.extend(text.lines().map(String::from));
            }
            inst.file(&h, &data)
        };
        if res.is_err() {
            purgatorio::exits("fail: error");
        }
    }
    if !list {
        inst.remove_files();
    }
    if inst.errors > 0 {
        purgatorio::exits("fail: error");
    }
    Ok(())
}
