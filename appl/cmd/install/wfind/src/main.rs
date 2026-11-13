//! install/wfind [-r root] [-p package ...] file ...: find which
//! packages hold files, as Inferno's wfind. The packages are the package
//! files under `root/install/name/` (default root `/`) and the units
//! installed in `root/wrap/name/`, for the packages named with `-p` or
//! else all of those directories. A file named in an argument is any
//! name in a package's `md5sum` that ends with it.
//!
//! For each such name, wfind prints the name, then a line for each
//! package that holds it (its digest there, `package/stamp(kind)` and
//! where it was found), then the digest of the file as it is under
//! `root` now (`non-existent` if it is not). `-r` is ours; Inferno's
//! looks in `/install` and `/wrap` only, and in `/wrap` only for the
//! packages that also have a directory in `/install`.

use std::collections::BTreeMap;
use std::io::BufReader;

use purgatorio::{Context, DMDIR, Error};
use wrap::{Reader, Unit};

/// Where a package was found.
#[derive(Clone, Copy)]
enum Where {
    Install,
    Wrap,
}

impl Where {
    fn text(self) -> &'static str {
        match self {
            Where::Install => "/install",
            Where::Wrap => "/wrap",
        }
    }
}

struct Occ {
    md5: String,
    name: String,
    stamp: u32,
    kind: &'static str,
    at: Where,
    pkg: String,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut root = String::from("/");
    let mut pkgs: Vec<String> = Vec::new();
    let mut arg =
        purgatorio::arg::Arg::new(args, "install/wfind [-r root] [-p package ...] file ...");
    while let Some(c) = arg.opt() {
        match c {
            'p' => match arg.arg() {
                Some(p) => pkgs.push(p),
                None => fatal("missing package name"),
            },
            'r' => match arg.arg() {
                Some(r) => root = r,
                None => fatal("missing root name"),
            },
            c => fatal(&format!("bad argument -{c}")),
        }
    }
    let files = arg.argv();
    if files.is_empty() {
        fatal("usage: install/wfind [-p package ... ] file ...");
    }
    let install = proto::mkpath(&root, "install");
    let wrapdir = proto::mkpath(&root, "wrap");
    if pkgs.is_empty() {
        let mut all = subdirs(&install);
        all.extend(subdirs(&wrapdir));
        all.sort();
        all.dedup();
        pkgs = all;
    }
    let mut found: BTreeMap<String, Vec<Occ>> = BTreeMap::new();
    for pkg in &pkgs {
        let dir = format!("{install}/{pkg}");
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        for n in names {
            if let Some((name, u)) = package_file(&format!("{dir}/{n}")) {
                scan(&mut found, &files, &name, &u, Where::Install, pkg);
            }
        }
        for u in wrap::units(&format!("{wrapdir}/{pkg}")) {
            scan(&mut found, &files, pkg, &u, Where::Wrap, pkg);
        }
    }
    for (name, occs) in &found {
        println!("{name}");
        for o in occs {
            println!(
                "\t{} {}/{}({})\t{}/{}",
                o.md5,
                o.name,
                o.stamp,
                o.kind,
                o.at.text(),
                o.pkg
            );
        }
        let now = wrap::md5_file(&proto::mkpath(&root, name))
            .unwrap_or_else(|_| format!("{:<32}", "non-existent"));
        println!("\t{now} {name}");
    }
    Ok(())
}

fn fatal(msg: &str) -> ! {
    eprintln!("{msg}");
    purgatorio::exits("fail:error");
}

fn subdirs(dir: &str) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn kind(u: &Unit) -> &'static str {
    match (u.full, u.update_of.is_some()) {
        (true, false) => "package ",
        (false, true) => "update  ",
        (true, true) => "full upd",
        (false, false) => "???",
    }
}

fn scan(
    found: &mut BTreeMap<String, Vec<Occ>>,
    files: &[String],
    name: &str,
    u: &Unit,
    at: Where,
    pkg: &str,
) {
    for (file, md5) in &u.md5 {
        if files.iter().any(|f| file.ends_with(f.as_str())) {
            found.entry(file.clone()).or_default().push(Occ {
                md5: md5.clone(),
                name: String::from(name),
                stamp: u.stamp,
                kind: kind(u),
                at,
                pkg: String::from(pkg),
            });
        }
    }
}

/// The wrap header of a package file.
fn package_file(path: &str) -> Option<(String, Unit)> {
    let f = std::fs::File::open(path).ok()?;
    let mut r = Reader::new(BufReader::new(f));
    let mut name = None;
    let mut unit = Unit::default();
    while let Ok(Some(h)) = r.next_hdr() {
        if h.name == "/wrap" {
            continue;
        }
        let Some(rest) = h.name.strip_prefix("/wrap/") else {
            break;
        };
        let parts: Vec<&str> = rest.split('/').collect();
        if h.mode & DMDIR != 0 {
            match parts.as_slice() {
                [n] => name = Some(String::from(*n)),
                [_, stamp] => unit.stamp = stamp.parse().unwrap_or(0),
                _ => {}
            }
            continue;
        }
        let data = r.data().ok()?;
        let text = String::from_utf8_lossy(&data);
        match parts.as_slice() {
            [_, _, "package"] => unit.full = true,
            [_, _, "update"] => unit.update_of = text.trim().parse().ok(),
            [_, _, "md5sum"] => {
                unit.md5 = wrap::parse_md5(&text);
                // The rest of the header is not needed.
                if unit.full || unit.update_of.is_some() {
                    break;
                }
            }
            _ => {}
        }
    }
    Some((name?, unit))
}
