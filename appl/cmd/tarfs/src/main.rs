//! tarfs [-ab] [-p] tarfile mountpt: mount a read-only view of a tar
//! archive on `mountpt`. `-a` and `-b` add it after or before what is
//! there (the default replaces it); `-p` ignores the archive's permissions,
//! giving every file mode 0444 and every directory 0555. Owners are the
//! archive's user and group names, or `0` when it has none.

use std::fs::File;
use std::io::BufReader;

use archive::ustar::{Kind, Reader};
use purgatorio::abi::mount::MREPL;
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Dir, Error};
use styxserv::rotree::RoTree;

const USAGE: &str = "tarfs [-ab] [-p] tarfile mountpt";

fn load(path: &str, noperm: bool) -> Result<RoTree, String> {
    let f = File::open(path).map_err(|e| format!("cannot open {path}: {}", styxserv::ioerr(e)))?;
    let mut r = Reader::new(BufReader::new(f));
    let mut t = RoTree::new(Dir {
        mode: DMDIR | 0o555,
        uid: String::from("inferno"),
        gid: String::from("inferno"),
        muid: String::from("inferno"),
        ..Dir::default()
    });
    while let Some(h) = r.next_entry().map_err(|e| format!("{path}: {e}"))? {
        let dir = match h.kind {
            Kind::Dir => true,
            Kind::File => false,
            // Inferno has no links or devices.
            Kind::Link | Kind::Other => continue,
        };
        let mut data = Vec::new();
        if !dir {
            r.copy_data(&mut data).map_err(|e| format!("{path}: {e}"))?;
        }
        let perm = match (noperm, dir) {
            (true, true) => 0o555,
            (true, false) => 0o444,
            (false, _) => h.mode & 0o777,
        };
        let owner = |n: &str| {
            if n.is_empty() {
                String::from("0")
            } else {
                String::from(n)
            }
        };
        let d = Dir {
            mode: perm | if dir { DMDIR } else { 0 },
            atime: h.mtime as u32,
            mtime: h.mtime as u32,
            uid: owner(&h.uname),
            gid: owner(&h.gname),
            muid: owner(&h.uname),
            ..Dir::default()
        };
        t.add(&h.name, d, data);
    }
    Ok(t)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let noperm = args.get(2).map(String::as_str) == Some("-p");
        let path = args.last().cloned().unwrap_or_default();
        let mut t = load(&path, noperm)?;
        return styxserv::bg::serve(&mut t);
    }
    let mut arg = Arg::new(args, USAGE);
    let mut flags = MREPL;
    let mut noperm = false;
    while let Some(c) = arg.opt() {
        match c {
            'a' | 'b' => {
                styxserv::bg::flag(c, &mut flags);
            }
            'p' => noperm = true,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [file, mtpt] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let file = styxserv::abs(file);
    // Read it here too, so a bad archive is this command's error.
    load(&file, noperm)?;
    let mut argv = vec![String::from("tarfs"), String::from("--serve")];
    if noperm {
        argv.push(String::from("-p"));
    }
    argv.push(file);
    styxserv::bg::mount("/bin/tarfs.wasm", &argv, mtpt, flags)
        .map_err(|e| Error(format!("cannot mount on {mtpt}: {e}")))
}
