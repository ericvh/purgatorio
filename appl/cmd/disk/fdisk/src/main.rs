//! disk/fdisk [-abfprw] [-s sectorsize] disk: edit the PC partition table
//! in a disk's master boot record, as fdisk(8).
//!
//! The partitions are the primary ones, `p1` to `p4`, and secondary
//! (extended) ones, `s1`, `s2`, ..., which a chain of extended boot
//! records holds in place of a primary entry. A secondary partition's
//! range includes its boot record: its data starts one track (63 sectors)
//! in, or where the boot record read from the disk says. Each run of
//! secondary partitions that follow one another with no gap is one chain,
//! and the primary partitions plus the chains cannot be more than four.
//! The extended entries themselves are not listed. Sizes and positions
//! are in sectors (the disks here report no cylinders). Without `-p` or `-w`, fdisk runs the editor of fdisk(8) on
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
use purgatorio::{print, println};

const USAGE: &str = "disk/fdisk [-abfprw] [-s sectorsize] /dev/sdC0/data";

/// Where the table starts in sector 0, and the signature after it.
const TABLE: usize = 446;
const PLAN9: u8 = 0x39;
/// The type of a primary entry fdisk writes for a chain.
const EXTENDED: u8 = 0x05;
/// The most boot records a chain is followed through: a loop on the disk
/// stops here.
const MAX_CHAIN: usize = 128;

fn is_extended(t: u8) -> bool {
    matches!(t, 0x05 | 0x0f | 0x85)
}

/// The number of a secondary partition's name, `s1` and on.
fn secondary(name: &str) -> Option<u32> {
    name.strip_prefix('s')
        .filter(|n| !n.starts_with('0'))
        .and_then(|n| n.parse().ok())
        .filter(|&n| n > 0)
}

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

/// A secondary partition read from the disk: its name, its first sector
/// and where its data starts from there.
type Gap = (String, u64, u64);

struct Fdisk {
    offset: u64,
    /// Where a new secondary partition's data starts, from its boot
    /// record.
    track: u64,
    /// The secondary partitions read from the disk, kept while their
    /// start is.
    gaps: Vec<Gap>,
}

impl Fdisk {
    /// Where secondary partition `p`'s data starts, from its first
    /// sector.
    fn gap(&self, p: &Part) -> u64 {
        self.gaps
            .iter()
            .find(|(n, s, _)| *n == p.name && *s == p.start)
            .map_or(self.track, |g| g.2)
    }

    /// The sectors that hold data: for a secondary partition, after its
    /// boot record.
    fn data(&self, p: &Part) -> (u64, u64) {
        if secondary(&p.name).is_some() {
            ((p.start + self.gap(p)).min(p.end), p.end)
        } else {
            (p.start, p.end)
        }
    }

    /// The runs of secondary partitions, in order, that follow one
    /// another with no gap and no primary partition between them.
    fn chains<'a>(&self, ed: &'a Edit) -> Vec<Vec<&'a Part>> {
        let mut parts: Vec<&Part> = ed.parts.iter().collect();
        parts.sort_by_key(|p| p.start);
        let mut chains: Vec<Vec<&Part>> = Vec::new();
        let mut open = false;
        for p in parts {
            if secondary(&p.name).is_none() {
                open = false;
                continue;
            }
            match chains.last_mut() {
                Some(c) if open && c.last().is_some_and(|q| q.end == p.start) => c.push(p),
                _ => chains.push(vec![p]),
            }
            open = true;
        }
        chains
    }

    /// The primary slots, 1 to 4, that no primary partition names.
    fn free_slots(ed: &Edit) -> Vec<usize> {
        (1..=4)
            .filter(|k| !ed.parts.iter().any(|p| p.name == format!("p{k}")))
            .collect()
    }
}

impl Scheme for Fdisk {
    fn check_name(&self, _ed: &Edit, name: &str) -> Result<(), String> {
        match name {
            "p1" | "p2" | "p3" | "p4" => Ok(()),
            _ if secondary(name).is_some() => Ok(()),
            _ => Err(format!(
                "name must be p1, p2, p3, p4 or s1, s2, ..., not \"{name}\""
            )),
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
            let (start, end) = self.data(p);
            out.push(format!(
                "part {name} {} {}\n",
                start + self.offset,
                end + self.offset
            ));
        }
        out
    }

    fn write(&mut self, ed: &Edit, disk: &mut Disk) -> Result<(), String> {
        let chains = self.chains(ed);
        let free = Self::free_slots(ed);
        if chains.len() > free.len() {
            return Err(String::from(
                "too many primary partitions and chains of secondary partitions",
            ));
        }
        let mut mbr = disk.read(0, 1)?;
        for slot in 0..4 {
            entry(&mut mbr, slot, None)?;
            if let Some(p) = ed.parts.iter().find(|p| p.name == format!("p{}", slot + 1)) {
                entry(
                    &mut mbr,
                    slot,
                    Some((p.ptype, p.active, p.start, p.end, &p.name)),
                )?;
            }
        }
        for (chain, &slot) in chains.iter().zip(&free) {
            let (first, last) = (chain[0], chain[chain.len() - 1]);
            let base = first.start;
            entry(
                &mut mbr,
                slot - 1,
                Some((EXTENDED, false, base, last.end, &first.name)),
            )?;
            for (k, p) in chain.iter().enumerate() {
                let mut ebr = disk.read(p.start, 1)?;
                let (start, end) = self.data(p);
                entry(
                    &mut ebr,
                    0,
                    Some((p.ptype, false, start - p.start, end - p.start, &p.name)),
                )?;
                entry(&mut ebr, 1, None)?;
                if let Some(n) = chain.get(k + 1) {
                    entry(
                        &mut ebr,
                        1,
                        Some((EXTENDED, false, n.start - base, n.end - base, &n.name)),
                    )?;
                }
                entry(&mut ebr, 2, None)?;
                entry(&mut ebr, 3, None)?;
                ebr[510] = 0x55;
                ebr[511] = 0xaa;
                disk.write(p.start, &ebr)?;
            }
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
                // A chain takes the first free slot when the table is
                // written.
                let chains = self.chains(ed).len();
                for k in Self::free_slots(ed).into_iter().skip(chains) {
                    println!("p{k}");
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

/// Fills in table entry `slot` of a boot sector: `None` clears it, and
/// `(type, active, start, end, name)` sets it, with the start and end
/// relative to what the table counts from.
fn entry(
    sector: &mut [u8],
    slot: usize,
    part: Option<(u8, bool, u64, u64, &str)>,
) -> Result<(), String> {
    let e = &mut sector[TABLE + 16 * slot..TABLE + 16 * (slot + 1)];
    e.fill(0);
    let Some((ptype, active, start, end, name)) = part else {
        return Ok(());
    };
    let (Ok(lba), Ok(n)) = (u32::try_from(start), u32::try_from(end - start)) else {
        return Err(format!("{name}: past the 2^32 sectors a table can address"));
    };
    e[0] = if active { 0x80 } else { 0 };
    // No cylinder-head-sector addresses, only block numbers.
    e[1..4].copy_from_slice(&[0xfe, 0xff, 0xff]);
    e[4] = ptype;
    e[5..8].copy_from_slice(&[0xfe, 0xff, 0xff]);
    e[8..12].copy_from_slice(&lba.to_le_bytes());
    e[12..16].copy_from_slice(&n.to_le_bytes());
    Ok(())
}

/// Table entry `slot` of a boot sector: its active flag, type, start and
/// length; `None` when it is empty.
fn read_entry(sector: &[u8], slot: usize) -> Option<(bool, u8, u64, u64)> {
    let e = &sector[TABLE + 16 * slot..TABLE + 16 * (slot + 1)];
    let lba = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
    let n = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as u64;
    (e[4] != 0 && n != 0).then_some((e[0] & 0x80 != 0, e[4], lba, n))
}

fn signed(sector: &[u8]) -> bool {
    sector[510] == 0x55 && sector[511] == 0xaa
}

/// The partitions in sector 0, if it holds a table: the primary ones, and
/// the secondary ones of each chain, with where each one's data starts
/// after its boot record.
fn read_table(disk: &mut Disk) -> Result<(Vec<Part>, Vec<Gap>), String> {
    let mbr = disk.read(0, 1)?;
    let (mut parts, mut gaps) = (Vec::new(), Vec::new());
    if !signed(&mbr) {
        return Ok((parts, gaps));
    }
    let mut nsec = 0;
    for slot in 0..4 {
        let Some((active, ptype, lba, n)) = read_entry(&mbr, slot) else {
            continue;
        };
        if !is_extended(ptype) {
            parts.push(Part {
                name: format!("p{}", slot + 1),
                start: lba,
                end: lba + n,
                changed: false,
                ptype,
                active,
            });
            continue;
        }
        // The chain: each boot record holds a partition, counted from
        // the record, and the next record, counted from the chain's base.
        let (base, limit) = (lba, lba + n);
        let mut at = Some(base);
        for _ in 0..MAX_CHAIN {
            let Some(ebr_at) = at.take() else {
                break;
            };
            if ebr_at >= disk.secs || ebr_at >= limit {
                break;
            }
            let ebr = disk.read(ebr_at, 1)?;
            if !signed(&ebr) {
                break;
            }
            if let Some((_, ptype, rel, len)) = read_entry(&ebr, 0) {
                nsec += 1;
                let name = format!("s{nsec}");
                gaps.push((name.clone(), ebr_at, rel));
                parts.push(Part {
                    name,
                    start: ebr_at,
                    end: ebr_at + rel + len,
                    changed: false,
                    ptype,
                    active: false,
                });
            }
            at = read_entry(&ebr, 1)
                .filter(|e| is_extended(e.1))
                .map(|e| base + e.2)
                .filter(|&next| next > ebr_at);
        }
    }
    Ok((parts, gaps))
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
    // As Inferno's fdisk: any failure is `fail:error`.
    run(args).map_err(|e| e.fails_as("fail:error"))
}

fn run(args: Vec<String>) -> Result<(), Error> {
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
    let mut disk = Disk::open(
        "disk/fdisk",
        &path,
        secsize,
        write || (editing && !read_only),
    )?;
    if disk.secs < 2 {
        return Err(Error::from(format!(
            "{path}: too small for a partition table"
        )));
    }
    let (parts, gaps) = if blank {
        (Vec::new(), Vec::new())
    } else {
        read_table(&mut disk)?
    };
    // The first track (63 sectors) is left for the boot code, as PCs do,
    // and so is the first track of each secondary partition for its
    // boot record.
    let lo = if disk.secs > 126 { 63 } else { 1 };
    let mut scheme = Fdisk {
        offset: disk.offset,
        track: lo,
        gaps,
    };
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
