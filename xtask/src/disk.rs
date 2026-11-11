//! `xtask diskimg`: purgfs images (`docs/fs.md`) built and checked on this
//! host with the kernel's own `purg-fs`, for its virtio-blk disk.

use std::fs::{File, OpenOptions};
use std::path::Path;

use purg_fs::file::{Copied, FileDisk, Stamp, add_tree, parse_size};
use purg_fs::layout::BSIZE;
use purg_fs::{Format, Fs, Mount, Report};

use crate::Result;

/// The owner of what an image holds: the kernel's one user.
pub const USER: &str = "inferno";

/// Makes `out` a new purgfs image of `size` bytes in `bsize` blocks,
/// holding a copy of directory `src` if given.
pub fn build(out: &Path, src: Option<&Path>, size: u64, bsize: usize) -> Result<Copied> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", out.display());
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)
        .map_err(|e| at(&e))?;
    file.set_len(size).map_err(|e| at(&e))?;
    let format = Format {
        bsize,
        user: String::from(USER),
        now: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32),
        ..Format::default()
    };
    let mut fs = Fs::format(FileDisk::new(file, size, 512), &format).map_err(|e| at(&e))?;
    let mut copied = Copied::default();
    if let Some(src) = src {
        let perm = |_: &Path, m: &std::fs::Metadata| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode()
        };
        let stamp = Stamp {
            user: USER,
            perm: &perm,
            mtime: None,
        };
        let root = fs.root();
        copied = add_tree(&mut fs, root, src, &stamp).map_err(|e| at(&e))?;
    }
    fs.sync().map_err(|e| at(&e))?;
    fs.into_disk().into_inner().sync_all().map_err(|e| at(&e))?;
    Ok(copied)
}

fn mount(img: &Path) -> Result<Fs<FileDisk<File>>> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", img.display());
    let file = File::open(img).map_err(|e| at(&e))?;
    let size = file.metadata().map_err(|e| at(&e))?.len();
    let mut disk = FileDisk::new(file, size, 512);
    disk.set_read_only(true);
    let m = Mount {
        read_only: true,
        ..Mount::default()
    };
    Fs::mount(disk, &m).map_err(|e| at(&e))
}

/// Checks image `img` without changing it.
pub fn check(img: &Path) -> Result<Report> {
    Ok(mount(img)?.check())
}

const USAGE: &str =
    "usage: cargo xtask diskimg [--size SIZE] [--bsize N] OUT [DIR] | diskimg --check IMAGE";

/// `xtask diskimg`.
pub fn cmd(args: &[String]) -> Result<()> {
    let (mut size, mut bsize, mut check_only) = (64u64 << 20, BSIZE, false);
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--size" => {
                size = it
                    .next()
                    .and_then(|s| parse_size(s))
                    .ok_or_else(|| String::from(USAGE))?
            }
            "--bsize" => {
                bsize = it
                    .next()
                    .and_then(|s| parse_size(s))
                    .ok_or_else(|| String::from(USAGE))? as usize
            }
            "--check" => check_only = true,
            _ => rest.push(a.clone()),
        }
    }
    if check_only {
        let [img] = rest.as_slice() else {
            return Err(String::from(USAGE));
        };
        let r = check(Path::new(img))?;
        for e in &r.errors {
            eprintln!("{img}: {e}");
        }
        eprintln!(
            "{img}: {} files, {} directories, {} blocks used, {} free",
            r.files, r.dirs, r.used, r.free
        );
        return if r.ok() {
            Ok(())
        } else {
            Err(format!("{img}: {} errors", r.errors.len()))
        };
    }
    let (out, src) = match rest.as_slice() {
        [out] => (out, None),
        [out, src] => (out, Some(Path::new(src))),
        _ => return Err(String::from(USAGE)),
    };
    let c = build(Path::new(out), src, size, bsize)?;
    eprintln!(
        "wrote {out} ({size} bytes): {} files, {} directories, {} bytes of data",
        c.files, c.dirs, c.bytes
    );
    Ok(())
}
