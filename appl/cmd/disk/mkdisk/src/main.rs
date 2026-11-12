//! disk/mkdisk [-b blocksize] [-j journal] [-s size] [-u user] [-r dir] image:
//! make a new, empty purgfs file system (`docs/fs.md`) in `image`, a disk
//! image file or a disk's `data` file (`#S/sdV0/data`). The file system
//! takes all of `image`, whose size `-s` sets (with an optional `k`, `m`
//! or `g`); without it the file must already have one. `-b` sets the
//! block size (4096 by default), `-j` the journal's blocks (32), and `-u`
//! the owner of the root and of what `-r` copies: the contents of
//! directory `dir`, recursively, with their permissions and times.
//! Anything already in `image` is lost.

use std::fs::OpenOptions;
use std::path::Path;

use purg_fs::file::{FileDisk, Stamp, add_tree, parse_size};
use purg_fs::layout::{BSIZE, JBLOCKS};
use purg_fs::{Format, Fs};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "disk/mkdisk [-b blocksize] [-j journal] [-s size] [-u user] [-r dir] image";

fn number(arg: &mut Arg, what: &str) -> Result<u64, Error> {
    let s = arg.earg()?;
    parse_size(&s).ok_or_else(|| Error::from(format!("bad {what} {s:?}")))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut format = Format {
        bsize: BSIZE,
        jblocks: JBLOCKS,
        ..Format::default()
    };
    let mut size = None;
    let mut src = None;
    while let Some(c) = arg.opt() {
        match c {
            'b' => format.bsize = number(&mut arg, "block size")? as usize,
            'j' => format.jblocks = number(&mut arg, "journal size")? as u32,
            's' => size = Some(number(&mut arg, "size")?),
            'u' => format.user = arg.earg()?,
            'r' => src = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let [image] = arg
        .argv()
        .try_into()
        .map_err(|_| Error::from(format!("usage: {USAGE}")))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(size.is_some())
        .truncate(false)
        .open(&image)
        .map_err(|e| Error::from(format!("{image}: {}", Error::from(e))))?;
    let size = match size {
        Some(s) => {
            file.set_len(s)
                .map_err(|e| Error::from(format!("{image}: {}", Error::from(e))))?;
            s
        }
        None => file.metadata().map(|m| m.len()).unwrap_or(0),
    };
    if size == 0 {
        return Err(Error::from(format!("{image}: no size: use -s")));
    }
    format.now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as u32);
    let user = format.user.clone();
    let at = |e: purg_fs::Error| Error::from(format!("{image}: {e}"));
    let mut fs = Fs::format(FileDisk::new(file, size, 512), &format).map_err(at)?;
    if let Some(dir) = src {
        let perm = |p: &Path, _: &std::fs::Metadata| {
            purgatorio::stat(&p.to_string_lossy()).map_or(0o644, |d| d.mode & 0o777)
        };
        let stamp = Stamp {
            user: &user,
            perm: &perm,
            mtime: None,
        };
        let root = fs.root();
        add_tree(&mut fs, root, Path::new(&dir), &stamp).map_err(at)?;
    }
    fs.sync().map_err(at)?;
    let _ = fs.into_disk().into_inner().sync_all();
    Ok(())
}
