//! 9660srv [-rab] [-RJ] cdfile dir: mount the ISO 9660 image `cdfile`
//! (a file or a device) read-only on `dir`. `-r` replaces what is there
//! (the default), `-a` and `-b` add the image after or before it. Rock
//! Ridge names and permissions are used when the image has them (`-R`
//! ignores them), and a Joliet volume's names when there is one (`-J`
//! ignores it). Names from a plain ISO volume are lowercased and lose
//! their `;` version. Files are owned by `cdrom`, in group `ridge`, `iso`
//! or `joliet` after the format; without Rock Ridge, files have mode
//! 0444 and directories 0555.
//!
//! Follows Inferno's 9660srv(4). Not here: High Sierra volumes, Plan 9's
//! own extensions to the records (`-9`), files of several extents, and
//! taking the image from the attach name.

use std::fs::File;

use iso9660::{Disk, Image, Options, Volume};
use purgatorio::abi::mount::MREPL;
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Dir, Error};
use styxserv::rotree::{RoTree, Source};

const USAGE: &str = "9660srv [-rab] [-RJ] cdfile dir";

/// More directories than any CD holds: an image whose directories loop
/// stops here.
const MAXDIRS: usize = 1 << 16;

struct Src(Image<File>);

impl Source for Src {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        self.0.read_at(off, buf)
    }
}

fn load(path: &str, opt: Options) -> Result<RoTree, String> {
    let f = File::open(path).map_err(|e| format!("cannot open {path}: {}", styxserv::ioerr(e)))?;
    let mut img = Image(f);
    let v = Volume::open(&mut img, opt).map_err(|e| format!("{path}: {e}"))?;
    let group = String::from(v.format.group());
    let dir = |perm: u32, isdir: bool, mtime: u32, length: u64| Dir {
        mode: perm | if isdir { DMDIR } else { 0 },
        atime: mtime,
        mtime,
        length,
        uid: String::from("cdrom"),
        gid: group.clone(),
        muid: String::from("cdrom"),
        ..Dir::default()
    };
    let root = v.root();
    let mut t = RoTree::new(dir(0o555, true, root.mtime, 0));
    let mut todo = vec![(String::new(), root)];
    let mut seen = 0;
    while let Some((path, e)) = todo.pop() {
        seen += 1;
        if seen > MAXDIRS {
            return Err(format!("{path}: too many directories"));
        }
        for k in v.list(&mut img, &e).map_err(|e| format!("{path}: {e}"))? {
            let p = format!("{path}/{}", k.name);
            let d = dir(k.perm, k.dir, k.mtime, if k.dir { 0 } else { k.size });
            if k.dir {
                t.add(&p, d, Vec::new());
                todo.push((p, k));
            } else {
                t.add_at(&p, d, k.offset);
            }
        }
    }
    t.set_source(Box::new(Src(img)));
    Ok(t)
}

fn options(flags: &str) -> Options {
    Options {
        rock: !flags.contains('R'),
        joliet: !flags.contains('J'),
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let [_, _, flags, path] = args.as_slice() else {
            return Err(Error::from("bad --serve arguments"));
        };
        let mut t = load(path, options(flags))?;
        return styxserv::bg::serve(&mut t);
    }
    let mut arg = Arg::new(args, USAGE);
    let mut flags = MREPL;
    let mut ext = String::from("-");
    while let Some(c) = arg.opt() {
        match c {
            'r' => flags = MREPL,
            'a' | 'b' => {
                styxserv::bg::flag(c, &mut flags);
            }
            'R' | 'J' => ext.push(c),
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [file, mtpt] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let file = styxserv::abs(file);
    // Read it here too, so a bad image is this command's error.
    load(&file, options(&ext))?;
    let argv = [String::from("9660srv"), String::from("--serve"), ext, file];
    styxserv::bg::mount("/bin/9660srv.wasm", &argv, mtpt, flags)
        .map_err(|e| Error(format!("cannot mount on {mtpt}: {e}")))
}
