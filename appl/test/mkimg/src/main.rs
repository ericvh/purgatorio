//! mkimg: disk images for the file server tests.
//!
//! `mkimg iso [-rj] image dir` writes an ISO 9660 image of the tree at
//! `dir`: `-r` adds Rock Ridge entries, `-j` a Joliet volume.
//!
//! `mkimg fat 12|16|32 sectors image [dir]` writes an empty FAT file
//! system of `sectors` 512-byte sectors, then, with `dir`, copies the
//! tree at `dir` into it.

use std::os::fd::AsRawFd;

use purgatorio::abi::mode::OREAD;
use purgatorio::{Context, DMDIR, Error};

const USAGE: &str = "usage: mkimg iso [-rj] image dir | mkimg fat 12|16|32 sectors image [dir]";

/// A file under the tree, by its path from the top.
struct Found {
    path: String,
    perm: u32,
    data: Option<Vec<u8>>,
}

fn walk(top: &str, rel: &str, out: &mut Vec<Found>) -> Result<(), Error> {
    let here = if rel.is_empty() {
        String::from(top)
    } else {
        format!("{top}/{rel}")
    };
    let fd = purgatorio::open(&here, OREAD)?;
    let mut all = Vec::new();
    loop {
        let batch = purgatorio::dirread(fd.as_raw_fd())?;
        if batch.is_empty() {
            break;
        }
        all.extend(batch);
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    for d in all {
        let path = if rel.is_empty() {
            d.name.clone()
        } else {
            format!("{rel}/{}", d.name)
        };
        if d.mode & DMDIR != 0 {
            out.push(Found {
                path: path.clone(),
                perm: d.mode & 0o777,
                data: None,
            });
            walk(top, &path, out)?;
        } else {
            let data = std::fs::read(format!("{top}/{path}")).map_err(Error::from)?;
            out.push(Found {
                path,
                perm: d.mode & 0o777,
                data: Some(data),
            });
        }
    }
    Ok(())
}

fn iso(args: &[String]) -> Result<(), Error> {
    let (flags, rest) = match args.split_first() {
        Some((f, rest)) if f.starts_with('-') => (f.as_str(), rest),
        _ => ("", args),
    };
    let [image, dir] = rest else {
        return Err(Error::from(USAGE));
    };
    let mut found = Vec::new();
    walk(dir, "", &mut found)?;
    let files: Vec<iso9660::build::File> = found
        .into_iter()
        .map(|f| iso9660::build::File {
            path: f.path,
            perm: f.perm,
            data: f.data,
        })
        .collect();
    let img = iso9660::build::build(&files, flags.contains('r'), flags.contains('j'));
    std::fs::write(image, img).map_err(Error::from)
}

fn fat_image(args: &[String]) -> Result<(), Error> {
    let (kind, sectors, image, dir) = match args {
        [k, n, i] => (k, n, i, None),
        [k, n, i, d] => (k, n, i, Some(d)),
        _ => return Err(Error::from(USAGE)),
    };
    let kind = match kind.as_str() {
        "12" => fat::Kind::Fat12,
        "16" => fat::Kind::Fat16,
        "32" => fat::Kind::Fat32,
        _ => return Err(Error::from(USAGE)),
    };
    let sectors: u32 = sectors.parse().map_err(|_| Error::from(USAGE))?;
    let img = fat::format::format(kind, sectors).map_err(Error)?;
    let mut v = fat::Vol::open(img, 0).map_err(Error)?;
    if let Some(dir) = dir {
        let mut found = Vec::new();
        walk(dir, "", &mut found)?;
        // Every file gets the same time, so images are alike.
        let now = 1_709_288_430;
        for f in found {
            let (parent, name) = match f.path.rsplit_once('/') {
                Some((p, n)) => (Some(p), n),
                None => (None, f.path.as_str()),
            };
            let mut d = v.root();
            if let Some(p) = parent {
                for e in p.split('/') {
                    let ent = v
                        .lookup(d, e)
                        .map_err(Error)?
                        .ok_or_else(|| Error::from(fat::ENONEXIST))?;
                    d = ent.as_dir();
                }
            }
            let mut attr = if f.data.is_none() { fat::ATTR_DIR } else { 0 };
            if f.perm & 0o200 == 0 {
                attr |= fat::ATTR_RO;
            }
            let ent = v.create(d, name, attr, now).map_err(Error)?;
            if let Some(data) = f.data
                && !data.is_empty()
            {
                v.write(ent.loc, 0, &data, now).map_err(Error)?;
            }
        }
    }
    v.flush().map_err(Error)?;
    std::fs::write(image, &v.disk).map_err(Error::from)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match args.get(1).map(String::as_str) {
        Some("iso") => iso(&args[2..]),
        Some("fat") => fat_image(&args[2..]),
        _ => Err(Error::from(USAGE)),
    }
}
