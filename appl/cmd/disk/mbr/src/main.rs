//! disk/mbr [-9] [-m mbrfile] disk: write the boot code of a disk's
//! master boot record (sector 0), keeping its partition table, as mbr(8).
//!
//! The code is the first 446 bytes of `mbrfile`; without `-m`, a short
//! x86 real-mode program of our own that prints `not a bootable disk` and
//! halts. `-9` also replaces the table with one active Plan 9 partition
//! from the end of the first track (sector 63) to the end of the disk.

use disk::Disk;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "disk/mbr [-9] [-m mbrfile] disk";
const TABLE: usize = 446;

/// The default boot code, loaded at 0x7c00: print the message with the
/// BIOS teletype call, then halt for good.
const NOT_BOOTABLE: &[u8] = &[
    0xfa, // cli
    0x31, 0xc0, // xor ax, ax
    0x8e, 0xd8, // mov ds, ax
    0xbe, 0x16, 0x7c, // mov si, 0x7c16 (the message)
    0xac, // 0x08: lodsb
    0x08, 0xc0, // or al, al
    0x74, 0x06, // jz 0x13
    0xb4, 0x0e, // mov ah, 0x0e
    0xcd, 0x10, // int 0x10
    0xeb, 0xf5, // jmp 0x08
    0xf4, // 0x13: hlt
    0xeb, 0xfd, // jmp 0x13
];
const MESSAGE: &[u8] = b"not a bootable disk\r\n\0";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    // As Inferno's mbr: any failure is `fail:error`.
    run(args).map_err(|e| e.fails_as("fail:error"))
}

fn run(args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut plan9, mut code_file) = (false, None);
    while let Some(c) = arg.opt() {
        match c {
            '9' => plan9 = true,
            'm' => code_file = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let [path] = arg
        .argv()
        .try_into()
        .map_err(|_| Error::from(format!("usage: {USAGE}")))?;
    let mut disk = Disk::open("disk/mbr", &path, None, true)?;
    if disk.secsize < 512 || disk.secs < 2 {
        return Err(Error::from(format!(
            "{path}: not a disk with 512-byte sectors"
        )));
    }
    let code = match &code_file {
        Some(f) => {
            let b = std::fs::read(f).map_err(|e| Error(format!("{f}: {}", Error::from(e))))?;
            if b.len() > 512 || (b.len() > TABLE && b.len() != 512) {
                return Err(Error::from(format!(
                    "{f}: more than {TABLE} bytes of boot code"
                )));
            }
            b[..b.len().min(TABLE)].to_vec()
        }
        None => [NOT_BOOTABLE, MESSAGE].concat(),
    };
    let mut mbr = disk.read(0, 1)?;
    mbr[..TABLE].fill(0);
    mbr[..code.len()].copy_from_slice(&code);
    if plan9 {
        let start: u64 = 63;
        let n = u32::try_from(disk.secs.saturating_sub(start))
            .map_err(|_| Error::from("disk too large for a PC partition table"))?;
        if n == 0 {
            return Err(Error::from(format!("{path}: too small for a partition")));
        }
        mbr[TABLE..510].fill(0);
        let e = &mut mbr[TABLE..TABLE + 16];
        e[0] = 0x80;
        e[1..4].copy_from_slice(&[0xfe, 0xff, 0xff]);
        e[4] = 0x39;
        e[5..8].copy_from_slice(&[0xfe, 0xff, 0xff]);
        e[8..12].copy_from_slice(&(start as u32).to_le_bytes());
        e[12..16].copy_from_slice(&n.to_le_bytes());
    }
    mbr[510] = 0x55;
    mbr[511] = 0xaa;
    disk.write(0, &mbr)?;
    Ok(())
}
