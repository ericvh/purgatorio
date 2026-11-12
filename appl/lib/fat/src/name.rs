//! Names: 8.3 short names, the long (VFAT) names stored before them, and
//! the short names made up for long ones.

/// Characters a short name may hold besides letters and digits.
const SHORT_EXTRA: &str = "!#$%&'()-@^_`{}~";

fn short_char(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || SHORT_EXTRA.contains(c)
}

/// The 11 bytes of `name` as a short name, when it is one as it stands:
/// at most 8 characters, a dot and at most 3 more, in lower case (a
/// short name reads back in lower case, so a name with capitals needs a
/// long name to keep them).
pub fn as_short(name: &str) -> Option<[u8; 11]> {
    let (base, ext) = match name.split_once('.') {
        Some((b, e)) => (b, e),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || name.ends_with('.') {
        return None;
    }
    let ok = |s: &str| {
        s.chars()
            .all(|c| !c.is_ascii_uppercase() && short_char(c.to_ascii_uppercase()))
    };
    if !ok(base) || !ok(ext) {
        return None;
    }
    let mut out = [b' '; 11];
    for (i, c) in base.bytes().enumerate() {
        out[i] = c.to_ascii_uppercase();
    }
    for (i, c) in ext.bytes().enumerate() {
        out[8 + i] = c.to_ascii_uppercase();
    }
    if out[0] == 0xe5 {
        out[0] = 0x05;
    }
    Some(out)
}

/// The short name for a long one, with tail `~n`: upper case, without
/// spaces or dots but the last, other characters as `_`.
pub fn basis(name: &str, n: u32) -> [u8; 11] {
    let clean = |s: &str| -> Vec<u8> {
        s.chars()
            .filter(|&c| c != ' ' && c != '.')
            .map(|c| {
                let u = c.to_ascii_uppercase();
                if u.is_ascii() && short_char(u) {
                    u as u8
                } else {
                    b'_'
                }
            })
            .collect()
    };
    let (base, ext) = match name.rsplit_once('.') {
        Some((b, e)) if !b.trim_start_matches('.').is_empty() => (clean(b), clean(e)),
        _ => (clean(name), Vec::new()),
    };
    let tail = format!("~{n}");
    let keep = base.len().min(8 - tail.len());
    let mut out = [b' '; 11];
    out[..keep].copy_from_slice(&base[..keep]);
    out[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
    for (i, &c) in ext.iter().take(3).enumerate() {
        out[8 + i] = c;
    }
    out
}

/// A short name as it reads: lower case, with a dot before an extension.
pub fn show_short(raw: &[u8; 11]) -> String {
    let mut b = raw.to_vec();
    if b[0] == 0x05 {
        b[0] = 0xe5;
    }
    let base: String = b[..8].iter().map(|&c| char::from(c)).collect();
    let ext: String = b[8..].iter().map(|&c| char::from(c)).collect();
    let (base, ext) = (base.trim_end(), ext.trim_end());
    let s = if ext.is_empty() {
        String::from(base)
    } else {
        format!("{base}.{ext}")
    };
    s.to_lowercase()
}

/// The checksum of a short name that its long name entries carry.
pub fn checksum(raw: &[u8; 11]) -> u8 {
    raw.iter().fold(0u8, |s, &c| {
        ((s & 1) << 7).wrapping_add(s >> 1).wrapping_add(c)
    })
}

/// Whether `name` can be a file name at all.
pub fn valid(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.encode_utf16().count() <= 255
        && !name
            .chars()
            .any(|c| c < ' ' || c == '\u{7f}' || "\"*/:<>?\\|".contains(c))
        && !name.ends_with(' ')
}

/// The long name entries for `name`, in the order they are stored (the
/// last part first), each 32 bytes.
pub fn lfn_slots(name: &str, sum: u8) -> Vec<[u8; 32]> {
    let mut units: Vec<u16> = name.encode_utf16().collect();
    let n = units.len().div_ceil(13);
    if units.len() < n * 13 {
        units.push(0);
        units.resize(n * 13, 0xffff);
    }
    let mut out = Vec::new();
    for i in (0..n).rev() {
        let mut e = [0u8; 32];
        e[0] = (i + 1) as u8 | if i + 1 == n { 0x40 } else { 0 };
        e[11] = 0x0f;
        e[13] = sum;
        let part = &units[i * 13..i * 13 + 13];
        for (j, &u) in part.iter().enumerate() {
            let at = match j {
                0..=4 => 1 + 2 * j,
                5..=10 => 14 + 2 * (j - 5),
                _ => 28 + 2 * (j - 11),
            };
            e[at..at + 2].copy_from_slice(&u.to_le_bytes());
        }
        out.push(e);
    }
    out
}

/// The 13 characters a long name entry holds.
pub fn lfn_part(e: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    for at in (1..11)
        .step_by(2)
        .chain((14..26).step_by(2))
        .chain((28..32).step_by(2))
    {
        out.push(u16::from_le_bytes([e[at], e[at + 1]]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        assert_eq!(&as_short("readme.txt").unwrap(), b"README  TXT");
        assert_eq!(&as_short("a").unwrap(), b"A          ");
        assert!(as_short("README.TXT").is_none());
        assert!(as_short("toolongname").is_none());
        assert!(as_short("a.b.c").is_none());
        assert!(as_short("a b").is_none());
        assert!(as_short("x.").is_none());
        assert_eq!(show_short(b"README  TXT"), "readme.txt");
        assert_eq!(show_short(b"NOEXT      "), "noext");
        assert_eq!(&basis("A Long Name.text", 1), b"ALONGN~1TEX");
        assert_eq!(&basis("x.y.z", 12), b"XY~12   Z  ");
        assert_eq!(&basis(".hidden", 1), b"HIDDEN~1   ");
        assert_eq!(&basis("caf\u{e9}", 1), b"CAF_~1     ");
    }

    #[test]
    fn long_names() {
        let sum = checksum(b"ALONGN~1TEX");
        let slots = lfn_slots("A Long Name.text", sum);
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0][0], 0x42);
        assert_eq!(slots[1][0], 0x01);
        let mut units = lfn_part(&slots[1]);
        units.extend(lfn_part(&slots[0]));
        let end = units.iter().position(|&u| u == 0).unwrap();
        assert_eq!(
            String::from_utf16(&units[..end]).unwrap(),
            "A Long Name.text"
        );
        assert!(valid("A Long Name.text"));
        assert!(!valid("a/b") && !valid("..") && !valid("a:b") && !valid(""));
        // Exactly 13 characters: no terminator.
        assert_eq!(lfn_slots(&"x".repeat(13), 0).len(), 1);
    }
}
