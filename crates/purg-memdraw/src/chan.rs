//! Channel descriptors (image(6)).
//!
//! A descriptor lists a pixel's channels from its most significant bits
//! to its least: `r8g8b8` is a 24-bit pixel with red in the top byte.
//! As a number, each channel is one byte, its type in the high nibble and
//! its width in bits in the low one, the first channel in the most
//! significant byte.

use core::fmt;

/// Channel types, in the order of the letters `rgbkamx`.
const RED: u8 = 0;
const GREEN: u8 = 1;
const BLUE: u8 = 2;
const GREY: u8 = 3;
const ALPHA: u8 = 4;
const MAP: u8 = 5;
const IGNORE: u8 = 6;
const LETTERS: &[u8; 7] = b"rgbkamx";

const fn dc(t: u8, n: u8) -> u32 {
    (((t & 15) << 4) | (n & 15)) as u32
}

pub const GREY1: Chan = Chan(dc(GREY, 1));
pub const GREY2: Chan = Chan(dc(GREY, 2));
pub const GREY4: Chan = Chan(dc(GREY, 4));
pub const GREY8: Chan = Chan(dc(GREY, 8));
pub const CMAP8: Chan = Chan(dc(MAP, 8));
pub const RGB15: Chan =
    Chan(dc(IGNORE, 1) << 24 | dc(RED, 5) << 16 | dc(GREEN, 5) << 8 | dc(BLUE, 5));
pub const RGB16: Chan = Chan(dc(RED, 5) << 16 | dc(GREEN, 6) << 8 | dc(BLUE, 5));
pub const RGB24: Chan = Chan(dc(RED, 8) << 16 | dc(GREEN, 8) << 8 | dc(BLUE, 8));
pub const BGR24: Chan = Chan(dc(BLUE, 8) << 16 | dc(GREEN, 8) << 8 | dc(RED, 8));
pub const RGBA32: Chan =
    Chan(dc(RED, 8) << 24 | dc(GREEN, 8) << 16 | dc(BLUE, 8) << 8 | dc(ALPHA, 8));
pub const ARGB32: Chan =
    Chan(dc(ALPHA, 8) << 24 | dc(RED, 8) << 16 | dc(GREEN, 8) << 8 | dc(BLUE, 8));
pub const XRGB32: Chan =
    Chan(dc(IGNORE, 8) << 24 | dc(RED, 8) << 16 | dc(GREEN, 8) << 8 | dc(BLUE, 8));
pub const ABGR32: Chan =
    Chan(dc(ALPHA, 8) << 24 | dc(BLUE, 8) << 16 | dc(GREEN, 8) << 8 | dc(RED, 8));
pub const XBGR32: Chan =
    Chan(dc(IGNORE, 8) << 24 | dc(BLUE, 8) << 16 | dc(GREEN, 8) << 8 | dc(RED, 8));

/// A channel descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chan(pub u32);

/// One channel of a pixel: its type, the bit it starts at, its width.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Field {
    pub kind: u8,
    pub shift: u8,
    pub bits: u8,
}

/// Where a descriptor's channels sit in a pixel, lowest first.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layout {
    pub fields: [Field; 4],
    pub n: usize,
    pub depth: u32,
    pub alpha: bool,
    pub grey: bool,
}

impl Chan {
    /// Parses a descriptor such as `r8g8b8`. Fails on an unknown letter,
    /// a width outside 1 to 8, more than four channels, or a total width
    /// that does not pack into bytes (1, 2, 4, 8, 16, 24 or 32 bits).
    pub fn parse(s: &str) -> Option<Chan> {
        let b = s.as_bytes();
        let (mut i, mut c, mut n) = (0, 0u32, 0);
        while i < b.len() {
            if b[i].is_ascii_whitespace() {
                break;
            }
            let t = LETTERS.iter().position(|&l| l == b[i])? as u8;
            i += 1;
            let start = i;
            let mut bits = 0u32;
            while i < b.len() && b[i].is_ascii_digit() {
                bits = bits * 10 + u32::from(b[i] - b'0');
                if bits > 8 {
                    return None;
                }
                i += 1;
            }
            if i == start || bits == 0 || n == 4 {
                return None;
            }
            c = c << 8 | dc(t, bits as u8);
            n += 1;
        }
        if b[i..].iter().any(|c| !c.is_ascii_whitespace()) {
            return None;
        }
        let chan = Chan(c);
        chan.layout().map(|_| chan)
    }

    /// Bits per pixel, or 0 for a descriptor that is not valid.
    pub fn depth(self) -> u32 {
        self.layout().map_or(0, |l| l.depth)
    }

    /// Whether the pixels have an alpha channel.
    pub fn has_alpha(self) -> bool {
        self.layout().is_some_and(|l| l.alpha)
    }

    pub(crate) fn layout(self) -> Option<Layout> {
        let mut l = Layout {
            fields: [Field::default(); 4],
            n: 0,
            depth: 0,
            alpha: false,
            grey: false,
        };
        let mut c = self.0;
        while c != 0 {
            let byte = (c & 0xFF) as u8;
            c >>= 8;
            // Only the high bytes may be empty.
            if byte == 0 {
                return None;
            }
            let (kind, bits) = (byte >> 4, byte & 15);
            if kind > IGNORE || bits == 0 || bits > 8 || l.n == 4 {
                return None;
            }
            l.fields[l.n] = Field {
                kind,
                shift: l.depth as u8,
                bits,
            };
            l.n += 1;
            l.depth += u32::from(bits);
            l.alpha |= kind == ALPHA;
            l.grey |= kind == GREY;
        }
        let d = l.depth;
        let ok = matches!(d, 1 | 2 | 4 | 8 | 16 | 24 | 32);
        let fields = &l.fields[..l.n];
        let count = |k: u8| fields.iter().filter(|f| f.kind == k).count();
        let colour = count(RED) + count(GREEN) + count(BLUE);
        // At most one of each; colour is all three or none; grey and
        // colour do not mix; a colour map stands alone.
        let unique = [RED, GREEN, BLUE, GREY, ALPHA, MAP]
            .iter()
            .all(|&k| count(k) <= 1);
        let shape = (colour == 0 || colour == 3)
            && !(colour == 3 && count(GREY) == 1)
            && (count(MAP) == 0 || l.n == 1);
        (ok && unique && shape && l.n > 0).then_some(l)
    }

    /// Whether memdraw can draw on and from images with this descriptor:
    /// any valid one except a colour map.
    pub fn supported(self) -> bool {
        self.layout()
            .is_some_and(|l| l.fields[..l.n].iter().all(|f| f.kind != MAP))
    }
}

impl Layout {
    /// Converts premultiplied `[r, g, b, a]` to a pixel value.
    pub fn pack(&self, c: [u8; 4]) -> u32 {
        let mut v = 0u32;
        for f in &self.fields[..self.n] {
            let x = match f.kind {
                RED => c[0],
                GREEN => c[1],
                BLUE => c[2],
                GREY => grey(c[0], c[1], c[2]),
                ALPHA => c[3],
                _ => 0,
            };
            v |= u32::from(x >> (8 - f.bits)) << f.shift;
        }
        v
    }

    /// Converts a pixel value to premultiplied `[r, g, b, a]`. Channels
    /// it lacks read as 0, except alpha, which reads as 255 (opaque);
    /// grey reads as equal red, green and blue.
    pub fn unpack(&self, v: u32) -> [u8; 4] {
        let mut c = [0, 0, 0, 255];
        for f in &self.fields[..self.n] {
            let x = expand((v >> f.shift) & ((1 << f.bits) - 1), f.bits);
            match f.kind {
                RED => c[0] = x,
                GREEN => c[1] = x,
                BLUE => c[2] = x,
                GREY => c = [x, x, x, c[3]],
                ALPHA => c[3] = x,
                _ => {}
            }
        }
        c
    }
}

/// Widens an `n`-bit channel value to 8 bits by repeating its bits, so
/// that all ones becomes 255 and zero stays 0.
pub(crate) fn expand(v: u32, n: u8) -> u8 {
    if n >= 8 {
        return v as u8;
    }
    let n = u32::from(n);
    let mut out = 0u32;
    let mut have = 0;
    while have < 8 {
        out = out << n | v;
        have += n;
    }
    (out >> (have - 8)) as u8
}

/// The grey level of a colour: 0.299 red, 0.587 green and 0.114 blue,
/// in 19-bit fixed point, so that equal channels give that value back.
pub(crate) fn grey(r: u8, g: u8, b: u8) -> u8 {
    let v = (156_763 * u32::from(r) + 307_758 * u32::from(g) + 59_769 * u32::from(b)) >> 19;
    v.min(255) as u8
}

impl fmt::Display for Chan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut c = self.0;
        let mut bytes = [0u8; 4];
        let mut n = 0;
        while c != 0 && n < 4 {
            bytes[n] = (c & 0xFF) as u8;
            c >>= 8;
            n += 1;
        }
        for &b in bytes[..n].iter().rev() {
            let t = usize::from(b >> 4);
            let letter = LETTERS.get(t).copied().unwrap_or(b'?');
            write!(f, "{}{}", letter as char, b & 15)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn names_round_trip() {
        for (s, c) in [
            ("k1", GREY1),
            ("k8", GREY8),
            ("m8", CMAP8),
            ("r5g6b5", RGB16),
            ("x1r5g5b5", RGB15),
            ("r8g8b8", RGB24),
            ("a8r8g8b8", ARGB32),
            ("x8r8g8b8", XRGB32),
            ("r8g8b8a8", RGBA32),
        ] {
            assert_eq!(Chan::parse(s), Some(c), "{s}");
            assert_eq!(c.to_string(), s);
        }
        assert_eq!(RGB24.0, 0x08_18_28);
        assert_eq!(ARGB32.0, 0x48_08_18_28);
        assert_eq!(GREY1.depth(), 1);
        assert_eq!(XRGB32.depth(), 32);
    }

    #[test]
    fn bad_names() {
        for s in [
            "", "q8", "r8g8", "r9g8b8", "k3", "r8g8b8k8", "k8k8", "r8 x", "r0g8b8",
        ] {
            assert_eq!(Chan::parse(s), None, "{s}");
        }
    }

    #[test]
    fn expansion() {
        assert_eq!(expand(1, 1), 255);
        assert_eq!(expand(0, 1), 0);
        assert_eq!(expand(0x1F, 5), 255);
        assert_eq!(expand(0x10, 5), 0x84);
        assert_eq!(expand(2, 2), 0xAA);
        assert_eq!(grey(255, 255, 255), 255);
        assert_eq!(grey(100, 100, 100), 100);
    }
}
