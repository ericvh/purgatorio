//! disk/fdisk [-abfprw] [-s sectorsize] disk: edit the PC partition table
//! in a disk's master boot record, as fdisk(8).
//!
//! The partitions are the four primary ones, `p1` to `p4`; extended
//! (secondary) partitions are listed by their primary entry but not
//! followed. Sizes and positions are in sectors (the disks here report no
//! cylinders). Without `-p` or `-w`, fdisk runs the editor of fdisk(8) on
//! standard input: `a`, `d`, `.`, `p`, `P`, `w`, `q`, `h`, and fdisk's
//! own `A name` (make it the active partition), `e` (list the empty
//! slots) and `t name [type]` (set its type, by name, `ctl` name or hex
//! number);
//! `-r` makes it read only. `-p` prints the `ctl` messages that give the
//! sd(3) device the partitions, named by type (`plan9`, `dos`, `ntfs`,
//! `linux`, `linuxswap`, with `1`, `2`, ... after repeats). `-a` adds one
//! Plan 9 partition in the largest free space when there is none, active
//! if no other partition is; `-b` starts from an empty table; `-w` writes
//! the table and exits. Entries get no cylinder-head-sector addresses
//! (the conventional `1023/254/63` instead), only block numbers. `-f` is
//! accepted and ignored.

use disk::{Disk, Edit, Part, Scheme};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "disk/fdisk [-abfprw] [-s sectorsize] /dev/sdC0/data";

/// Where the table starts in sector 0, and the signature after it.
const TABLE: usize = 446;
const PLAN9: u8 = 0x39;

/// Partition types: the number, the name `p` and `t` use, and the name
/// for `ctl`, if the type has one.
const TYPES: &[(u8, &str, Option<&str>)] = &[
    (0x01, "FAT12", Some("dos")),
    (0x04, "FAT16", Some("dos")),
    (0x05, "EXTENDED", None),
    (0x06, "FATHUGE", Some("dos")),
    (0x07, "NTFS", Some("ntfs")),
    (0x0b, "FAT32", Some("dos")),
    (0x0c, "FAT32X", Some("dos")),
    (0x0e, "FATHUGEX", Some("dos")),
    (0x0f, "EXTENDEDX", None),
    (0x39, "PLAN9", Some("plan9")),
    (0x82, "LINUXSWAP", Some("linuxswap")),
    (0x83, "LINUX", Some("linux")),
    (0xee, "GPT", None),
    (0xef, "EFI", None),
];

fn type_name(t: u8) -> String {
    TYPES
        .iter()
        .find(|e| e.0 == t)
        .map_or(format!("TYPE{t:02X}"), |e| String::from(e.1))
}

struct Fdisk {
    offset: u64,
}

impl Scheme for Fdisk {
    fn check_name(&self, _ed: &Edit, name: &str) -> Result<(), String> {
        match name {
            "p1" | "p2" | "p3" | "p4" => Ok(()),
            _ => Err(format!("name must be p1, p2, p3 or p4, not \"{name}\"")),
        }
    }

    fn new_part(&self, name: &str, start: u64, end: u64) -> Part {
        Part {
            name: String::from(name),
            start,
            end,
            changed: true,
            ptype: PLAN9,
            active: false,
        }
    }

    fn describe(&self, p: &Part) -> String {
        format!(
            " {}{}",
            type_name(p.ptype),
            if p.active { " *" } else { "" }
        )
    }

    fn ctl_lines(&self, ed: &Edit) -> Vec<String> {
        let mut seen: Vec<&str> = Vec::new();
        let mut out = Vec::new();
        for p in &ed.parts {
            let Some(base) = TYPES.iter().find(|e| e.0 == p.ptype).and_then(|e| e.2) else {
                continue;
            };
            let n = seen.iter().filter(|s| **s == base).count();
            seen.push(base);
            let name = if n == 0 {
                String::from(base)
            } else {
                format!("{base}{n}")
            };
            out.push(format!(
                "part {name} {} {}\n",
                p.start + self.offset,
                p.end + self.offset
            ));
        }
        out
    }

    fn write(&mut self, ed: &Edit, disk: &mut Disk) -> Result<(), String> {
        let mut mbr = disk.read(0, 1)?;
        for slot in 0..4 {
            let e = &mut mbr[TABLE + 16 * slot..TABLE + 16 * (slot + 1)];
            e.fill(0);
            let Some(p) = ed.parts.iter().find(|p| p.name == format!("p{}", slot + 1)) else {
                continue;
            };
            let (Ok(lba), Ok(n)) = (u32::try_from(p.start), u32::try_from(p.end - p.start)) else {
                return Err(format!(
                    "{}: past the 2^32 sectors a table can address",
                    p.name
                ));
            };
            e[0] = if p.active { 0x80 } else { 0 };
            e[1..4].copy_from_slice(&[0xfe, 0xff, 0xff]);
            e[4] = p.ptype;
            e[5..8].copy_from_slice(&[0xfe, 0xff, 0xff]);
            e[8..12].copy_from_slice(&lba.to_le_bytes());
            e[12..16].copy_from_slice(&n.to_le_bytes());
        }
        mbr[510] = 0x55;
        mbr[511] = 0xaa;
        disk.write(0, &mbr)
    }

    fn command(&mut self, ed: &mut Edit, cmd: &str, args: &[&str]) -> Option<Result<(), String>> {
        let find = |ed: &mut Edit, name: &str| -> Result<usize, String> {
            ed.parts
                .iter()
                .position(|p| p.name == name)
                .ok_or_else(|| format!("cannot find partition \"{name}\""))
        };
        Some(match (cmd, args) {
            ("A", [name]) => find(ed, name).map(|i| {
                for (k, p) in ed.parts.iter_mut().enumerate() {
                    if p.active != (k == i) {
                        p.active = k == i;
                        p.changed = true;
                    }
                }
                ed.changed = true;
            }),
            ("A", _) => Err(String::from("usage: A name")),
            ("e", []) => {
                for k in 1..=4 {
                    let name = format!("p{k}");
                    if !ed.parts.iter().any(|p| p.name == name) {
                        println!("{name}");
                    }
                }
                Ok(())
            }
            ("t", [name]) => {
                let names: Vec<&str> = TYPES.iter().map(|e| e.1).collect();
                println!("{}", names.join(" "));
                find(ed, name).map(drop)
            }
            ("t", [name, t]) => find(ed, name).and_then(|i| {
                let ty = TYPES
                    .iter()
                    .find(|e| e.1.eq_ignore_ascii_case(t) || e.2 == Some(*t))
                    .map(|e| e.0)
                    .or_else(|| u8::from_str_radix(t, 16).ok().filter(|&v| v != 0))
                    .ok_or_else(|| format!("unknown type \"{t}\""))?;
                ed.parts[i].ptype = ty;
                ed.parts[i].changed = true;
                ed.changed = true;
                Ok(())
            }),
            ("t", _) => Err(String::from("usage: t name [type]")),
            _ => return None,
        })
    }

    fn help(&self) -> &'static str {
        "A name - set partition active\n\
         e - list empty partition slots\n\
         t name [type] - set partition type\n"
    }
}

/// The primary partitions in sector 0, if it holds a table.
fn read_table(disk: &mut Disk) -> Result<Vec<Part>, String> {
    let mbr = disk.read(0, 1)?;
    if mbr[510] != 0x55 || mbr[511] != 0xaa {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    for slot in 0..4 {
        let e = &mbr[TABLE + 16 * slot..TABLE + 16 * (slot + 1)];
        let lba = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
        let n = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as u64;
        if e[4] == 0 || n == 0 {
            continue;
        }
        parts.push(Part {
            name: format!("p{}", slot + 1),
            start: lba,
            end: lba + n,
            changed: false,
            ptype: e[4],
            active: e[0] & 0x80 != 0,
        });
    }
    Ok(parts)
}

/// `-a`: a Plan 9 partition in the largest free space, if there is none.
fn auto(ed: &mut Edit, scheme: &Fdisk) -> Result<(), String> {
    if ed.parts.iter().any(|p| p.ptype == PLAN9) {
        return Ok(());
    }
    let Some(slot) = (1..=4)
        .map(|k| format!("p{k}"))
        .find(|n| !ed.parts.iter().any(|p| p.name == *n))
    else {
        return Err(String::from("no empty partition slot"));
    };
    ed.sorted();
    let mut best = (0, 0);
    let mut at = ed.lo;
    let mut bounds: Vec<(u64, u64)> = ed.parts.iter().map(|p| (p.start, p.end)).collect();
    bounds.push((ed.secs, ed.secs));
    for (s, e) in bounds {
        if s > at && s - at > best.1 - best.0 {
            best = (at, s);
        }
        at = at.max(e);
    }
    if best.1 <= best.0 {
        return Err(String::from("no free space for a plan9 partition"));
    }
    let active = ed.parts.iter().any(|p| p.active);
    ed.add(scheme, &slot, best.0, best.1)?;
    if !active && let Some(p) = ed.parts.iter_mut().find(|p| p.name == slot) {
        p.active = true;
    }
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut auto_, mut blank, mut print, mut read_only, mut write) =
        (false, false, false, false, false);
    let mut secsize = None;
    while let Some(c) = arg.opt() {
        match c {
            'a' => auto_ = true,
            'b' => blank = true,
            'p' => print = true,
            'r' => read_only = true,
            'w' => write = true,
            's' => {
                let s = arg.earg()?;
                secsize = Some(s.parse::<u64>().map_err(|_| arg.usage())?);
            }
            'f' => {}
            _ => return Err(arg.usage()),
        }
    }
    let [path] = arg
        .argv()
        .try_into()
        .map_err(|_| Error::from(format!("usage: {USAGE}")))?;
    let editing = !print && !write;
    let mut disk = Disk::open("fdisk", &path, secsize, write || (editing && !read_only))?;
    if disk.secs < 2 {
        return Err(Error::from(format!(
            "{path}: too small for a partition table"
        )));
    }
    let parts = if blank {
        Vec::new()
    } else {
        read_table(&mut disk)?
    };
    let mut scheme = Fdisk {
        offset: disk.offset,
    };
    // The first track (63 sectors) is left for the boot code, as PCs do.
    let lo = if disk.secs > 126 { 63 } else { 1 };
    let mut ed = Edit::new(parts, lo, disk.secs);
    ed.sorted();
    ed.secsize = disk.secsize;
    ed.changed = blank;
    if auto_ {
        auto(&mut ed, &scheme)?;
    }
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
