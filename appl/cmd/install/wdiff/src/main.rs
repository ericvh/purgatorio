//! install/wdiff [-blp] [-r root] package: compare the files a package
//! file (made by install/create) holds with those installed under `root`
//! (default `/`), as Inferno's wdiff.
//!
//! For each file in the package's `md5sum` whose installed copy differs,
//! wdiff runs `diff` (with `-b`, `diff -b`) on the package's copy and the
//! installed one; with `-l` it only lists it as `name modified`. A file
//! that is gone is listed as `name removed`. Files that an update
//! installed later in `root/wrap/name` covers are left out, unless `-p`,
//! which compares with this package alone. Directories are skipped.

use std::collections::HashMap;
use std::io::{BufReader, Write};

use purgatorio::abi::pctl;
use purgatorio::{Context, DMDIR, Error};
use wrap::{Reader, Unit};

const DIFF: &str = "/bin/diff.wasm";

/// A package file: its name, its unit, and its files' data.
struct Package {
    name: String,
    unit: Unit,
    files: HashMap<String, Vec<u8>>,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut bflag = false;
    let mut listing = false;
    let mut package = false;
    let mut root = String::from("/");
    let mut arg = purgatorio::arg::Arg::new(args, "install/wdiff [-blp] [-r root] package");
    while let Some(c) = arg.opt() {
        match c {
            'b' => bflag = true,
            'l' => listing = true,
            'p' => package = true,
            'r' => match arg.arg() {
                Some(r) => root = r,
                None => fatal("missing root name"),
            },
            c => fatal(&format!("bad argument -{c}")),
        }
    }
    let rest = arg.argv();
    let [file] = rest.as_slice() else {
        fatal("usage: install/wdiff [-blp] [-r root] package");
    };
    if purgatorio::stat(file).is_err() {
        fatal(&format!("no such file {file}"));
    }
    let Some(pkg) = read_package(file) else {
        fatal("no such package found");
    };
    // Updates installed after this package cover the files they hold.
    let later: Vec<Unit> = if package {
        Vec::new()
    } else {
        wrap::units(&proto::mkpath(&root, &format!("wrap/{}", pkg.name)))
            .into_iter()
            .filter(|u| u.stamp > pkg.unit.stamp)
            .collect()
    };
    for (name, digest) in &pkg.unit.md5 {
        if later
            .iter()
            .any(|u| u.digest(name).is_some() || u.remove.iter().any(|r| r == name))
        {
            continue;
        }
        let local = proto::mkpath(&root, name);
        if purgatorio::stat(&local).is_ok_and(|d| d.mode & DMDIR != 0) {
            continue;
        }
        let Ok(now) = wrap::md5_file(&local) else {
            println!("{name} removed");
            continue;
        };
        if &now == digest {
            continue;
        }
        if listing {
            println!("{name} modified");
            continue;
        }
        let Some(data) = pkg.files.get(name) else {
            println!("cannot stat {}", proto::mkpath("/", name));
            continue;
        };
        diff(name, data, &local, bflag);
    }
    Ok(())
}

fn fatal(msg: &str) -> ! {
    eprintln!("{msg}");
    purgatorio::exits("fail:error");
}

/// The wrap header and files of a package file, or `None` if it is not
/// one.
fn read_package(path: &str) -> Option<Package> {
    let f = std::fs::File::open(path).ok()?;
    let mut r = Reader::new(BufReader::new(f));
    let mut name = None;
    let mut unit = Unit::default();
    let mut files = HashMap::new();
    while let Ok(Some(h)) = r.next_hdr() {
        if h.mode & DMDIR != 0 {
            if let Some(rest) = h.name.strip_prefix("/wrap/") {
                match rest.split('/').collect::<Vec<_>>().as_slice() {
                    [n] => name = Some(String::from(*n)),
                    [_, stamp] => unit.stamp = stamp.parse().unwrap_or(0),
                    _ => {}
                }
            }
            continue;
        }
        let data = r.data().ok()?;
        let wrapped = h
            .name
            .strip_prefix("/wrap/")
            .map(|r| r.split('/').collect::<Vec<_>>());
        match wrapped.as_deref() {
            Some([_, _, elem]) => {
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
            Some(_) => {}
            None => {
                files.insert(h.name.clone(), data);
            }
        }
    }
    Some(Package {
        name: name?,
        unit,
        files,
    })
}

/// Runs diff on the package's copy of `name` (put in a temporary file)
/// and the installed one.
fn diff(name: &str, data: &[u8], local: &str, bflag: bool) {
    let tmp = format!(
        "/tmp/wdiff.{}.{}",
        purgatorio::pid(),
        name.replace('/', "_")
    );
    let made = std::fs::File::create(&tmp).and_then(|mut f| f.write_all(data));
    if made.is_err() {
        println!("cannot stat {tmp}");
        return;
    }
    let mut argv = vec![String::from("diff")];
    if bflag {
        argv.push(String::from("-b"));
    }
    argv.push(tmp.clone());
    argv.push(String::from(local));
    let _ = std::io::stdout().flush();
    if purgatorio::spawn(DIFF, &argv, pctl::NEWFD, &[(0, 0), (1, 1), (2, 2)]).is_ok() {
        let _ = purgatorio::wait();
    }
    let _ = std::fs::remove_file(&tmp);
}
