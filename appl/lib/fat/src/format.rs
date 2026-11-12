//! Empty FAT file systems for tests: 512-byte sectors, two FATs, no
//! partition table.

use super::Kind;

const BPS: u32 = 512;

fn put16(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 2].copy_from_slice(&(v as u16).to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// The cluster count, and the sectors of each FAT, for a layout.
fn fit(kind: Kind, total: u32, reserved: u32, root_secs: u32, spc: u32) -> Option<(u32, u32)> {
    let bits = match kind {
        Kind::Fat12 => 12,
        Kind::Fat16 => 16,
        Kind::Fat32 => 32,
    };
    let mut fat = 1;
    for _ in 0..32 {
        let data = total.checked_sub(reserved + 2 * fat + root_secs)?;
        let clusters = data / spc;
        let need = ((clusters + 2) * bits).div_ceil(8).div_ceil(BPS);
        if need <= fat {
            return Some((clusters, fat));
        }
        fat = need;
    }
    None
}

/// An empty file system of `kind` in `sectors` sectors.
pub fn format(kind: Kind, sectors: u32) -> Result<Vec<u8>, String> {
    let small = || format!("{sectors} sectors is the wrong size for {kind:?}");
    let (reserved, root_entries): (u32, u32) = match kind {
        Kind::Fat32 => (32, 0),
        _ if sectors < 1024 => (1, 64),
        _ => (1, 512),
    };
    let root_secs = (root_entries * 32).div_ceil(BPS);
    // The smallest cluster that keeps the count in range for the kind.
    let (spc, clusters, fat) = [1u32, 2, 4, 8, 16, 32, 64]
        .iter()
        .find_map(|&spc| {
            let (c, f) = fit(kind, sectors, reserved, root_secs, spc)?;
            let ok = match kind {
                Kind::Fat12 => (1..4085).contains(&c),
                Kind::Fat16 => (4085..65525).contains(&c),
                Kind::Fat32 => (1..0x0fff_fff5).contains(&c),
            };
            ok.then_some((spc, c, f))
        })
        .ok_or_else(small)?;
    let mut img = vec![0u8; sectors as usize * BPS as usize];
    let b = &mut img[..512];
    b[..3].copy_from_slice(if kind == Kind::Fat32 {
        &[0xeb, 0x58, 0x90]
    } else {
        &[0xeb, 0x3c, 0x90]
    });
    b[3..11].copy_from_slice(b"PURGATIO");
    put16(b, 11, BPS);
    b[13] = spc as u8;
    put16(b, 14, reserved);
    b[16] = 2;
    put16(b, 17, root_entries);
    if sectors < 65536 {
        put16(b, 19, sectors);
    } else {
        put32(b, 32, sectors);
    }
    b[21] = 0xf8;
    put16(b, 24, 32);
    put16(b, 26, 2);
    let ext = if kind == Kind::Fat32 {
        put32(b, 36, fat);
        put32(b, 44, 2);
        put16(b, 48, 1);
        put16(b, 50, 6);
        64
    } else {
        put16(b, 22, fat);
        36
    };
    b[ext] = 0x80;
    b[ext + 2] = 0x29;
    put32(b, ext + 3, 0x1234_5678);
    b[ext + 7..ext + 18].copy_from_slice(b"NO NAME    ");
    b[ext + 18..ext + 26].copy_from_slice(match kind {
        Kind::Fat12 => b"FAT12   ",
        Kind::Fat16 => b"FAT16   ",
        Kind::Fat32 => b"FAT32   ",
    });
    b[510] = 0x55;
    b[511] = 0xaa;
    if kind == Kind::Fat32 {
        let boot: Vec<u8> = img[..512].to_vec();
        img[6 * 512..7 * 512].copy_from_slice(&boot);
        let mut info = [0u8; 512];
        info[..4].copy_from_slice(b"RRaA");
        info[484..488].copy_from_slice(b"rrAa");
        put32(&mut info, 488, clusters - 1);
        put32(&mut info, 492, 3);
        info[510] = 0x55;
        info[511] = 0xaa;
        img[512..1024].copy_from_slice(&info);
        img[7 * 512..8 * 512].copy_from_slice(&info);
    }
    let head: &[u8] = match kind {
        Kind::Fat12 => &[0xf8, 0xff, 0xff],
        Kind::Fat16 => &[0xf8, 0xff, 0xff, 0xff],
        // Entries 0 and 1, and the root directory's cluster 2.
        Kind::Fat32 => &[
            0xf8, 0xff, 0xff, 0x0f, 0xff, 0xff, 0xff, 0x0f, 0xff, 0xff, 0xff, 0x0f,
        ],
    };
    for f in 0..2 {
        let at = ((reserved + f * fat) * BPS) as usize;
        img[at..at + head.len()].copy_from_slice(head);
    }
    Ok(img)
}
