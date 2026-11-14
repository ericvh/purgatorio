//! disk/format [-dfvx] [-b bootblock] [-c csize] [-l label] [-r nresrv]
//! [-t type] disk [file ...]: prepare a disk, partition or image for use,
//! as prep(8)'s format.
//!
//! With `-d` it makes a FAT file system (FAT12, or FAT16 when the
//! clusters do not fit in 12 bits) and copies each `file` into its root
//! directory, in order; a file named `9load` gets the system attribute.
//! `-c` is the cluster size in sectors, `-l` the volume label (in the
//! boot sector) and `-r` the number of reserved sectors at the start,
//! the boot sector included (`-r 2` keeps a Plan 9 partition table in
//! sector 1 of a `9fat` partition). `-b` copies the boot code of
//! `bootblock` into sector 0, keeping the parameter block; without `-d`
//! that is all it does.
//!
//! `-t` names the medium: `3½HD` (1.44 MB, the default for a plain file),
//! `3½DD`, `3½QD`, `5¼HD`, `5¼DD`, or `hard` (the whole file; the default
//! for a disk partition, a file beside a `ctl` file). A plain file is
//! extended to the medium's size if it is shorter.
//!
//! format refuses a `9fat` partition with fewer than two reserved sectors,
//! and a disk's whole `data` file (whose first sector holds the partition
//! table), unless `-x` is given. There are no floppy drives to format
//! physically, so `-f` changes nothing. `-v` prints the layout.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

use fat::format::{Params, head};
use fat::{ATTR_SYSTEM, Image, Kind, Vol};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error, daytime};

const USAGE: &str = "disk/format [-dfvx] [-b bootblock] [-c csize] [-l label] [-r nresrv] [-t type] disk [file ...]";

struct Medium {
    name: &'static str,
    /// Sectors per track, heads and tracks; 0 for a hard disk.
    spt: u32,
    heads: u32,
    tracks: u32,
    media: u8,
    cluster: u32,
    root: u32,
}

const MEDIA: &[Medium] = &[
    Medium {
        name: "3½HD",
        spt: 18,
        heads: 2,
        tracks: 80,
        media: 0xf0,
        cluster: 1,
        root: 224,
    },
    Medium {
        name: "3½DD",
        spt: 9,
        heads: 2,
        tracks: 80,
        media: 0xf9,
        cluster: 2,
        root: 112,
    },
    Medium {
        name: "3½QD",
        spt: 36,
        heads: 2,
        tracks: 80,
        media: 0xf9,
        cluster: 2,
        root: 224,
    },
    Medium {
        name: "5¼HD",
        spt: 15,
        heads: 2,
        tracks: 80,
        media: 0xf9,
        cluster: 1,
        root: 224,
    },
    Medium {
        name: "5¼DD",
        spt: 9,
        heads: 2,
        tracks: 40,
        media: 0xfd,
        cluster: 2,
        root: 112,
    },
    Medium {
        name: "hard",
        spt: 0,
        heads: 0,
        tracks: 0,
        media: 0xf8,
        cluster: 0,
        root: 512,
    },
];

/// Whether `path` is a partition of a disk: a file beside a `ctl` file.
fn is_partition(path: &str) -> bool {
    let dir = match path.rsplit_once('/') {
        Some(("", _)) => "/",
        Some((d, _)) => d,
        None => ".",
    };
    purgatorio::stat(&format!("{dir}/ctl")).is_ok()
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    // As Inferno's format: any failure is `fail:error`.
    run(args).map_err(|e| e.fails_as("fail:error"))
}

fn run(args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut dos, mut verbose, mut force) = (false, false, false);
    let mut boot = None;
    let mut csize = 0u32;
    let mut label = None;
    let mut nresrv = 1u32;
    let mut typ = None;
    while let Some(c) = arg.opt() {
        match c {
            'd' => dos = true,
            'f' => {}
            'v' => verbose = true,
            'x' => force = true,
            'b' => boot = Some(arg.earg()?),
            'c' => csize = arg.earg()?.parse().map_err(|_| arg.usage())?,
            'l' => label = Some(arg.earg()?),
            'r' => nresrv = arg.earg()?.parse().map_err(|_| arg.usage())?,
            't' => typ = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let argv = arg.argv();
    let Some((disk, files)) = argv.split_first() else {
        return Err(usage);
    };
    let part = is_partition(disk);
    let medium = match &typ {
        Some(t) => MEDIA
            .iter()
            .find(|m| m.name == t)
            .ok_or_else(|| Error(format!("unknown disk type {t}")))?,
        None if part => &MEDIA[MEDIA.len() - 1],
        None => &MEDIA[0],
    };
    if !force {
        if base(disk) == "9fat" && nresrv < 2 {
            return Err(Error(format!(
                "{disk}: a 9fat partition needs -r 2 or more for its partition table"
            )));
        }
        if part && base(disk) == "data" {
            return Err(Error(format!(
                "{disk}: will not format the start of a whole disk"
            )));
        }
    }
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(disk)
        .map_err(|e| Error(format!("can't open {disk}: {}", Error::from(e))))?;
    let size = f.seek(SeekFrom::End(0))?;
    let bootcode = match &boot {
        Some(b) => {
            let code = std::fs::read(b)
                .map_err(|e| Error(format!("can't open {b}: {}", Error::from(e))))?;
            if code.len() > 512 {
                return Err(Error(format!("{b}: boot block larger than a sector")));
            }
            Some(code)
        }
        None => None,
    };
    if !dos {
        if let Some(code) = bootcode {
            let mut sector = [0u8; 512];
            fat::Disk::read_at(&mut Image(&mut f), 0, &mut sector).map_err(Error)?;
            install(&mut sector, &code);
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&sector)?;
        }
        return Ok(());
    }
    let sectors = if medium.spt == 0 {
        size / 512
    } else {
        u64::from(medium.spt * medium.heads * medium.tracks)
    };
    let sectors = u32::try_from(sectors).map_err(|_| Error(format!("{disk}: too large")))?;
    if medium.spt != 0 && size < u64::from(sectors) * 512 {
        if part {
            return Err(Error(format!("{disk}: smaller than a {}", medium.name)));
        }
        f.set_len(u64::from(sectors) * 512)?;
    }
    let now = u32::try_from(daytime::now()).unwrap_or(0);
    let mut p = Params::new(Kind::Fat12, sectors);
    p.reserved = nresrv;
    p.spc = if csize != 0 { csize } else { medium.cluster };
    p.root_entries = medium.root;
    p.media = medium.media;
    if medium.spt != 0 {
        p.sectors_per_track = medium.spt;
        p.heads = medium.heads;
    } else {
        p.sectors_per_track = 63;
        p.heads = 255;
    }
    if let Some(l) = &label {
        p.label = l.to_ascii_uppercase();
    }
    p.volid = now;
    let mut img = match head(&p) {
        Ok(h) => h,
        Err(_) => {
            p.kind = Kind::Fat16;
            head(&p).map_err(|e| Error(format!("{disk}: {e}")))?
        }
    };
    if verbose {
        eprintln!(
            "format: {:?}, {} sectors, {} per cluster, {} reserved, {} root entries",
            p.kind, sectors, img[13], nresrv, p.root_entries
        );
    }
    if let Some(code) = &bootcode {
        install(&mut img[..512], code);
    }
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&img)?;
    f.flush()?;
    if files.is_empty() {
        return Ok(());
    }
    let mut v = Vol::open(Image(f), 0).map_err(|e| Error(format!("{disk}: {e}")))?;
    let root = v.root();
    for path in files {
        let data = std::fs::read(path)
            .map_err(|e| Error(format!("can't open {path}: {}", Error::from(e))))?;
        let name = base(path);
        let attr = if name == "9load" { ATTR_SYSTEM } else { 0 };
        let ent = v
            .create(root, name, attr, now)
            .map_err(|e| Error(format!("{name}: {e}")))?;
        if !data.is_empty() {
            v.write(ent.loc, 0, &data, now)
                .map_err(|e| Error(format!("{name}: {e}")))?;
        }
    }
    v.flush().map_err(Error)?;
    Ok(())
}

/// Copies the boot code of `code` into a boot sector, keeping its jump's
/// target area: the parameter block (bytes 3 to 61) stays.
fn install(sector: &mut [u8], code: &[u8]) {
    for (i, &b) in code.iter().enumerate().take(512) {
        if !(3..62).contains(&i) {
            sector[i] = b;
        }
    }
}
