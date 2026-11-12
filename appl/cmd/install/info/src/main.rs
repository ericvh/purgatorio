//! install/info [-r root] package: describe a package file made by
//! install/create, or the installed package of that name in `root/wrap`
//! (default root `/`), as info(8): one line for the package, then one for
//! each base package, full update or update that makes it up, oldest
//! first, with its stamp, the number of files it records and its
//! description.

use std::io::BufReader;

use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Error};
use wrap::{Reader, Unit};

const USAGE: &str = "install/info [-r root] package";

/// The wrap header at the start of a package file.
fn from_archive(path: &str) -> Result<(String, Vec<Unit>), String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{path}: {}", Error::from(e)))?;
    let mut r = Reader::new(BufReader::new(f));
    let mut name = None;
    let mut unit = Unit::default();
    while let Some(h) = r.next_hdr().map_err(|e| format!("{path}: {e}"))? {
        let Some(rest) = h.name.strip_prefix("/wrap/") else {
            if h.name == "/wrap" {
                continue;
            }
            break;
        };
        let parts: Vec<&str> = rest.split('/').collect();
        match parts.as_slice() {
            [n] => name = Some(String::from(*n)),
            [_, stamp] if h.mode & DMDIR != 0 => unit.stamp = stamp.parse().unwrap_or(0),
            [_, _, elem] => {
                let data = r.data().map_err(|e| format!("{path}: {e}"))?;
                let text = String::from_utf8_lossy(&data);
                match *elem {
                    "package" => unit.full = true,
                    "update" => unit.update_of = text.trim().parse().ok(),
                    "desc" => unit.desc = String::from(text.trim_end()),
                    "md5sum" => unit.md5 = wrap::parse_md5(&text),
                    "remove" => unit.remove = text.lines().map(String::from).collect(),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| format!("{path}: not a package"))?;
    Ok((name, vec![unit]))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut root = String::from("/");
    while let Some(c) = arg.opt() {
        match c {
            'r' => root = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let [package] = arg
        .argv()
        .try_into()
        .map_err(|_| Error::from(format!("usage: {USAGE}")))?;
    let is_file = purgatorio::stat(&package).is_ok_and(|d| d.mode & DMDIR == 0);
    let (name, units, what) = if is_file {
        let (n, u) = from_archive(&package)?;
        (n, u, "package file")
    } else {
        let u = wrap::units(&proto::mkpath(&root, &format!("wrap/{package}")));
        if u.is_empty() {
            return Err(Error::from(format!("{package}: no such package in {root}")));
        }
        (package.clone(), u, "installed package")
    };
    println!("{what} {name}");
    for u in &units {
        let mut line = format!("\t{} {}", u.stamp, u.kind());
        if let Some(t) = u.update_of {
            line.push_str(&format!(" of {t}"));
        }
        let n = u.md5.len();
        line.push_str(&format!(", {n} file{}", if n == 1 { "" } else { "s" }));
        if !u.remove.is_empty() {
            line.push_str(&format!(", {} removed", u.remove.len()));
        }
        if !u.desc.is_empty() {
            line.push_str(&format!(": {}", u.desc));
        }
        println!("{line}");
    }
    Ok(())
}
