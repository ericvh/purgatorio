//! disk/prep [-bcfnprw] [-a name]... [-s sectorsize] plan9partition: edit
//! the Plan 9 partition table of a disk or partition, as prep(8).
//!
//! The table is the text in the partition's second sector, a line
//! `part name start end` for each subpartition, in sectors from the start
//! of the partition, `end` not included; the first sector is left for boot
//! code. Without `-p` or `-w`, prep runs the editor of prep(8) on
//! standard input (`a`, `d`, `.`, `p`, `P`, `w`, `q`, `h`; `-r` makes it
//! read only). `-p` prints the `ctl` messages that give the sd(3) device
//! the table, with the sectors moved by the partition's own start; `-w`
//! writes the table and exits. `w` and `-w` also update the device's
//! partitions when the disk has a `ctl` file. `-b` starts from an empty
//! table; `-a name` lays out the named partitions on an empty table (see
//! [`auto`]). `-c`, `-f` and `-n` are accepted and ignored.

use disk::{Disk, Edit, Part, Scheme};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "disk/prep [-bcfnprw] [-a partname]... [-s sectorsize] /dev/sdC0/plan9";

/// The partition names `-a` knows, in the order it lays them out.
const NAMES: &[&str] = &[
    "9fat", "nvram", "fscfg", "arenas", "isect", "fossil", "fs", "cache", "swap", "other",
];

struct Prep {
    /// The names in the table as it was read (or last written).
    orig: Vec<String>,
    /// Where the partition starts on its disk.
    offset: u64,
}

impl Scheme for Prep {
    fn check_name(&self, _ed: &Edit, name: &str) -> Result<(), String> {
        let bad = name.is_empty()
            || name.len() > 27
            || matches!(name, "ctl" | "data" | "." | "..")
            || name.contains(['/', ' ', '\t']);
        if bad {
            return Err(format!("bad partition name \"{name}\""));
        }
        Ok(())
    }

    fn ctl_lines(&self, ed: &Edit) -> Vec<String> {
        ed.parts
            .iter()
            .map(|p| {
                format!(
                    "part {} {} {}\n",
                    p.name,
                    p.start + self.offset,
                    p.end + self.offset
                )
            })
            .collect()
    }

    fn write(&mut self, ed: &Edit, disk: &mut Disk) -> Result<(), String> {
        let mut text = String::new();
        for p in &ed.parts {
            text.push_str(&format!("part {} {} {}\n", p.name, p.start, p.end));
        }
        let mut sector = text.into_bytes();
        if sector.len() > disk.secsize as usize {
            return Err(String::from("partition table too large for one sector"));
        }
        sector.resize(disk.secsize as usize, 0);
        disk.write(1, &sector)?;
        if disk.ctl.is_some() {
            let mut msgs = Vec::new();
            for name in &self.orig {
                if disk.ctl_parts.iter().any(|(n, _, _)| n == name) {
                    msgs.push(format!("delpart {name}\n"));
                }
            }
            msgs.extend(self.ctl_lines(ed));
            disk.ctl_write(&msgs)?;
            disk.ctl_parts = ed
                .parts
                .iter()
                .map(|p| (p.name.clone(), p.start + self.offset, p.end + self.offset))
                .collect();
        }
        self.orig = ed.parts.iter().map(|p| p.name.clone()).collect();
        Ok(())
    }
}

/// The table in sector 1, if it holds one.
fn read_table(disk: &mut Disk) -> Result<Vec<Part>, String> {
    if disk.secs < 2 {
        return Ok(Vec::new());
    }
    let sector = disk.read(1, 1)?;
    let n = sector.iter().position(|&b| b == 0).unwrap_or(sector.len());
    let text = String::from_utf8_lossy(&sector[..n]);
    let mut parts = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let ["part", name, start, end] = f.as_slice() else {
            // Not a table: start from an empty one.
            return Ok(Vec::new());
        };
        let (Ok(start), Ok(end)) = (start.parse(), end.parse()) else {
            return Ok(Vec::new());
        };
        parts.push(Part {
            name: String::from(*name),
            start,
            end,
            changed: false,
            ptype: 0,
            active: false,
        });
    }
    Ok(parts)
}

/// Lays out `names` on an empty table, in the order of [`NAMES`]: `9fat`
/// gets a tenth of the disk up to 10 MiB, `nvram` and `fscfg` a sector,
/// `cache` and `swap` a tenth each up to 512 MiB, and the rest of the disk
/// is shared equally by the others.
fn auto(ed: &mut Edit, scheme: &Prep, names: &[String], secsize: u64) -> Result<(), String> {
    for n in names {
        if !NAMES.contains(&n.as_str()) {
            return Err(format!("unknown partition name \"{n}\""));
        }
    }
    if !ed.parts.is_empty() {
        return Ok(());
    }
    let avail = ed.secs.saturating_sub(ed.lo);
    let mib = (1 << 20) / secsize;
    let fixed = |name: &str| -> Option<u64> {
        match name {
            "9fat" => Some((10 * mib).min(avail / 10).max(1)),
            "nvram" | "fscfg" => Some(1),
            "cache" | "swap" => Some((512 * mib).min(avail / 10).max(1)),
            _ => None,
        }
    };
    let chosen: Vec<&str> = NAMES
        .iter()
        .copied()
        .filter(|n| names.iter().any(|m| m == n))
        .collect();
    let used: u64 = chosen.iter().filter_map(|n| fixed(n)).sum();
    let shared = chosen.iter().filter(|n| fixed(n).is_none()).count() as u64;
    if used + shared > avail {
        return Err(String::from("disk too small for the partitions"));
    }
    let each = (avail - used).checked_div(shared).unwrap_or(0);
    // The last shared one also takes what the division left over.
    let spare = avail - used - each * shared;
    let mut at = ed.lo;
    let mut left = shared;
    for n in chosen {
        let len = fixed(n).unwrap_or_else(|| {
            left -= 1;
            if left == 0 { each + spare } else { each }
        });
        ed.add(scheme, n, at, at + len)?;
        at += len;
    }
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut blank, mut print, mut read_only, mut write) = (false, false, false, false);
    let mut autos = Vec::new();
    let mut secsize = None;
    while let Some(c) = arg.opt() {
        match c {
            'a' => autos.push(arg.earg()?),
            'b' => blank = true,
            'p' => print = true,
            'r' => read_only = true,
            'w' => write = true,
            's' => {
                let s = arg.earg()?;
                secsize = Some(s.parse::<u64>().map_err(|_| arg.usage())?);
            }
            'c' | 'f' | 'n' => {}
            _ => return Err(arg.usage()),
        }
    }
    let [path] = arg
        .argv()
        .try_into()
        .map_err(|_| Error::from(format!("usage: {USAGE}")))?;
    let editing = !print && !write;
    let mut disk = Disk::open(
        "disk/prep",
        &path,
        secsize,
        write || (editing && !read_only),
    )?;
    let parts = if blank {
        Vec::new()
    } else {
        read_table(&mut disk)?
    };
    let mut scheme = Prep {
        orig: parts.iter().map(|p| p.name.clone()).collect(),
        offset: disk.offset,
    };
    if disk.secs < 3 {
        return Err(Error::from(format!(
            "{path}: too small for a partition table"
        )));
    }
    let mut ed = Edit::new(parts, 2, disk.secs);
    ed.secsize = disk.secsize;
    ed.changed = blank;
    auto(&mut ed, &scheme, &autos, disk.secsize)?;
    if print {
        for l in scheme.ctl_lines(&ed) {
            print!("{l}");
        }
    }
    if write {
        scheme.write(&ed, &mut disk)?;
    }
    if editing {
        ed.read_only = read_only;
        ed.run(&mut scheme, &mut disk, &mut disk::stdin());
    }
    Ok(())
}
