//! GIF images for `bit2gif` and `gif2bit`: a writer for images of at most
//! 256 colours, as Inferno's writegif makes them (GIF89a, one image, a
//! global colour table), and a reader for the first image of a GIF87a or
//! GIF89a file (global or local colour table, interlaced or not), giving
//! its pixels as RGB. Both sides of the LZW code are here.

/// Inferno's `rgbv` colour map (color(6)): 256 entries of four levels of
/// red, green and blue, each with four levels of value, by index.
pub fn rgbv() -> [[u8; 3]; 256] {
    let mut map = [[0u8; 3]; 256];
    for r in 0..4u32 {
        for g in 0..4u32 {
            for b in 0..4u32 {
                for v in 0..4u32 {
                    let den = r.max(g).max(b);
                    let c = if den == 0 {
                        [v * 17; 3]
                    } else {
                        let num = 17 * (4 * den + v);
                        [r * num / den, g * num / den, b * num / den]
                    };
                    let idx = r * 64 + v * 16 + ((g * 4 + b + v).wrapping_sub(r) & 15);
                    map[idx as usize] = c.map(|x| x as u8);
                }
            }
        }
    }
    map
}

/// The `rgbv` index nearest to `c`, by squared distance.
pub fn nearest(map: &[[u8; 3]; 256], c: [u8; 3]) -> u8 {
    let d = |m: &[u8; 3]| {
        (0..3)
            .map(|i| {
                let e = i32::from(m[i]) - i32::from(c[i]);
                e * e
            })
            .sum::<i32>()
    };
    let mut best = 0;
    for (i, m) in map.iter().enumerate() {
        if d(m) < d(&map[best]) {
            best = i;
        }
    }
    best as u8
}

/// Writes `pixels` (`width * height` indices into `palette`, each below
/// `1 << bits`) as a GIF89a with `palette` as its global colour table.
/// `bits` is 1 to 8; the table is padded to `1 << bits` entries.
pub fn encode(width: u16, height: u16, bits: u8, palette: &[[u8; 3]], pixels: &[u8]) -> Vec<u8> {
    let bits = bits.clamp(1, 8);
    let mut out = Vec::new();
    out.extend_from_slice(b"GIF89a");
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    // A global table, colour resolution 4 bits as writegif says, of
    // 1 << bits entries.
    out.push(0x80 | (3 << 4) | (bits - 1));
    out.push(0);
    out.push(0);
    for i in 0..1usize << bits {
        out.extend_from_slice(&palette.get(i).copied().unwrap_or([0; 3]));
    }
    out.push(0x2c);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.push(0);
    // LZW wants a minimum code size of at least 2.
    let min = bits.max(2);
    out.push(min);
    let data = lzw_encode(min, pixels);
    for chunk in data.chunks(255) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    out.push(0);
    out.push(0x3b);
    out
}

/// Packs codes into bytes least significant bit first, as GIF does.
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, code: u16, width: u32) {
        self.acc |= u32::from(code) << self.n;
        self.n += width;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

fn lzw_encode(min: u8, pixels: &[u8]) -> Vec<u8> {
    use std::collections::HashMap;
    let clear = 1u16 << min;
    let end = clear + 1;
    let mut w = BitWriter {
        out: Vec::new(),
        acc: 0,
        n: 0,
    };
    let mut width = u32::from(min) + 1;
    let mut table: HashMap<(u16, u8), u16> = HashMap::new();
    let mut next = end + 1;
    w.put(clear, width);
    let mut cur: Option<u16> = None;
    for &p in pixels {
        let Some(c) = cur else {
            cur = Some(u16::from(p));
            continue;
        };
        if let Some(&code) = table.get(&(c, p)) {
            cur = Some(code);
            continue;
        }
        w.put(c, width);
        if next < 4096 {
            table.insert((c, p), next);
            next += 1;
            if u32::from(next) > (1 << width) && width < 12 {
                width += 1;
            }
        } else {
            w.put(clear, width);
            table.clear();
            next = end + 1;
            width = u32::from(min) + 1;
        }
        cur = Some(u16::from(p));
    }
    if let Some(c) = cur {
        w.put(c, width);
    }
    w.put(end, width);
    w.finish()
}

/// A GIF's first image, its pixels as RGB in rows from the top.
pub struct Decoded {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<[u8; 3]>,
}

/// Inferno's readgif errors are its own; these say the same things.
const EFORMAT: &str = "not a GIF file";
const ESHORT: &str = "GIF file is too short";
const EBAD: &str = "bad GIF data";

/// Reads the first image of a GIF file.
pub fn decode(a: &[u8]) -> Result<Decoded, &'static str> {
    let mut pos = 0;
    let take = |pos: &mut usize, n: usize| -> Result<&[u8], &'static str> {
        let s = a.get(*pos..*pos + n).ok_or(ESHORT)?;
        *pos += n;
        Ok(s)
    };
    let sig = take(&mut pos, 6)?;
    if sig != b"GIF87a" && sig != b"GIF89a" {
        return Err(EFORMAT);
    }
    let lsd = take(&mut pos, 7)?;
    let table = |flags: u8| 3usize << ((flags & 7) + 1);
    let mut global: Vec<[u8; 3]> = Vec::new();
    if lsd[4] & 0x80 != 0 {
        let t = take(&mut pos, table(lsd[4]))?;
        global = t.chunks(3).map(|c| [c[0], c[1], c[2]]).collect();
    }
    loop {
        match take(&mut pos, 1)?[0] {
            // An extension: its label, then sub-blocks to a zero length.
            0x21 => {
                take(&mut pos, 1)?;
                loop {
                    let n = take(&mut pos, 1)?[0] as usize;
                    if n == 0 {
                        break;
                    }
                    take(&mut pos, n)?;
                }
            }
            0x2c => break,
            0x3b => return Err(EFORMAT),
            _ => return Err(EBAD),
        }
    }
    let d = take(&mut pos, 9)?;
    let width = usize::from(u16::from_le_bytes([d[4], d[5]]));
    let height = usize::from(u16::from_le_bytes([d[6], d[7]]));
    let flags = d[8];
    let palette = if flags & 0x80 != 0 {
        let t = take(&mut pos, table(flags))?;
        t.chunks(3).map(|c| [c[0], c[1], c[2]]).collect()
    } else {
        global
    };
    let min = take(&mut pos, 1)?[0];
    if !(1..=11).contains(&min) {
        return Err(EBAD);
    }
    let mut data = Vec::new();
    loop {
        let n = take(&mut pos, 1)?[0] as usize;
        if n == 0 {
            break;
        }
        data.extend_from_slice(take(&mut pos, n)?);
    }
    let mut idx = lzw_decode(min, &data, width * height)?;
    idx.resize(width * height, 0);
    if flags & 0x40 != 0 {
        idx = deinterlace(&idx, width, height);
    }
    let rgb = idx
        .iter()
        .map(|&i| palette.get(usize::from(i)).copied().unwrap_or([0; 3]))
        .collect();
    Ok(Decoded { width, height, rgb })
}

fn lzw_decode(min: u8, data: &[u8], want: usize) -> Result<Vec<u8>, &'static str> {
    let clear = 1usize << min;
    let end = clear + 1;
    let mut width = u32::from(min) + 1;
    // Each entry: its prefix code and its last byte.
    let mut prefix: Vec<u16> = vec![0; 4096];
    let mut suffix: Vec<u8> = vec![0; 4096];
    for (i, s) in suffix.iter_mut().enumerate().take(clear) {
        *s = i as u8;
    }
    let mut next = end + 1;
    let mut prev: Option<usize> = None;
    let mut out = Vec::with_capacity(want);
    let (mut acc, mut n, mut at) = (0u32, 0u32, 0usize);
    let string = |mut code: usize, prefix: &[u16], suffix: &[u8]| {
        let mut s = Vec::new();
        while code >= clear + 2 {
            s.push(suffix[code]);
            code = usize::from(prefix[code]);
        }
        s.push(suffix[code]);
        s.reverse();
        s
    };
    while out.len() < want {
        while n < width {
            let Some(&b) = data.get(at) else {
                return Ok(out);
            };
            acc |= u32::from(b) << n;
            n += 8;
            at += 1;
        }
        let code = (acc & ((1 << width) - 1)) as usize;
        acc >>= width;
        n -= width;
        if code == clear {
            width = u32::from(min) + 1;
            next = end + 1;
            prev = None;
            continue;
        }
        if code == end {
            break;
        }
        let Some(p) = prev else {
            if code >= clear {
                return Err(EBAD);
            }
            out.push(code as u8);
            prev = Some(code);
            continue;
        };
        let s = if code < next {
            string(code, &prefix, &suffix)
        } else if code == next {
            let mut s = string(p, &prefix, &suffix);
            s.push(s[0]);
            s
        } else {
            return Err(EBAD);
        };
        if next < 4096 {
            prefix[next] = p as u16;
            suffix[next] = s[0];
            next += 1;
            if next == 1 << width && width < 12 {
                width += 1;
            }
        }
        out.extend_from_slice(&s);
        prev = Some(code);
    }
    Ok(out)
}

/// Rows of an interlaced image in display order: GIF stores every eighth
/// row from 0, then from 4, every fourth from 2, every second from 1.
fn deinterlace(idx: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; idx.len()];
    let mut src = 0;
    for (start, step) in [(0, 8), (4, 8), (2, 4), (1, 2)] {
        let mut y = start;
        while y < height {
            out[y * width..(y + 1) * width].copy_from_slice(&idx[src * width..(src + 1) * width]);
            src += 1;
            y += step;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(w: u16, h: u16, bits: u8, pixels: &[u8]) {
        let pal: Vec<[u8; 3]> = (0..256u32)
            .map(|i| [i as u8, (i * 3) as u8, (255 - i) as u8])
            .collect();
        let g = encode(w, h, bits, &pal, pixels);
        let d = decode(&g).unwrap();
        assert_eq!((d.width, d.height), (usize::from(w), usize::from(h)));
        let want: Vec<[u8; 3]> = pixels.iter().map(|&p| pal[usize::from(p)]).collect();
        assert_eq!(d.rgb, want);
    }

    #[test]
    fn small_images_round_trip() {
        round_trip(1, 1, 1, &[1]);
        round_trip(4, 2, 2, &[0, 1, 2, 3, 3, 2, 1, 0]);
        round_trip(3, 3, 8, &[0, 255, 7, 7, 7, 7, 200, 1, 0]);
    }

    #[test]
    fn a_large_image_fills_the_table_and_clears_it() {
        let pixels: Vec<u8> = (0..300 * 200u32).map(|i| ((i * 7919) >> 3) as u8).collect();
        round_trip(300, 200, 8, &pixels);
        let flat = vec![5u8; 64 * 64];
        round_trip(64, 64, 4, &flat);
    }

    #[test]
    fn interlaced_rows_are_put_back() {
        // Rows 0..5 stored in interlaced order: 0, 4, 2, 1, 3.
        let idx: Vec<u8> = [0u8, 4, 2, 1, 3].iter().flat_map(|&r| [r; 2]).collect();
        let out = deinterlace(&idx, 2, 5);
        assert_eq!(out, [0, 0, 1, 1, 2, 2, 3, 3, 4, 4]);
    }

    #[test]
    fn rgbv_has_black_white_and_pure_colours() {
        let m = rgbv();
        assert_eq!(m[0], [0, 0, 0]);
        assert_eq!(m[nearest(&m, [255, 255, 255]) as usize], [255, 255, 255]);
        assert_eq!(m[nearest(&m, [255, 0, 0]) as usize], [255, 0, 0]);
        assert_eq!(nearest(&m, [0, 0, 0]), 0);
    }

    #[test]
    fn bad_files_are_refused() {
        assert!(decode(b"NOTGIF").is_err());
        assert!(decode(b"GIF89a").is_err());
    }
}
