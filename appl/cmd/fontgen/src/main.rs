//! fontgen [-i indir] [-o outdir] recipe: makes Inferno subfonts and font
//! files (font(6)) from TrueType or OpenType fonts and from Unifont's
//! `.hex` bitmaps, as the recipe says.
//!
//! Each line of the recipe is one job; `#` starts a comment. Inputs are
//! read relative to `indir` (the recipe's directory by default) and
//! outputs written relative to `outdir` (`.` by default):
//!
//! - `ttf src px name`: rasterizes every glyph of `src` in the Basic
//!   Multilingual Plane at `px` pixels, plus its missing-glyph box as
//!   rune 0, into 8-bit grey subfonts `name/XXXX`, one for each block of
//!   256 runes starting at hexadecimal `XXXX`, and writes `name.font`,
//!   whose ranges cover only the runes the font has.
//! - `hex [min-max] src [combining] name`: the same from a Unifont `.hex`
//!   file, 1-bit, 16 pixels high with ascent 14, for the runes from `min`
//!   to `max` (hexadecimal; the BMP, `0-FFFF`, when not given).
//!   `combining` lists the combining marks and how far left of the point
//!   they draw (Unifont's `unifont-combining.txt`); those get that `left`
//!   and no advance.
//! - `merge name font...`: writes `name.font` with the ranges of each
//!   font in turn, file names made relative to the new font's directory,
//!   the largest ascent and the largest descent.
//! - `copy src dest`: copies a file (a licence, say) to the output.
//!
//! The output depends only on the inputs: fontgen is run as a wasm guest
//! so that floating-point rasterizing gives the same bytes on every host.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use purgatorio::arg::Arg;
use purgatorio::draw::memdraw::{FontFile, Fontchar, GREY1, GREY8, Image, Range, Rect, Subfont};
use purgatorio::{Context, Error};

/// One glyph, ready to place: its coverage rows and where it goes.
struct Glyph {
    w: i32,
    h: i32,
    /// Coverage, `w * h` bytes, 0 to 255.
    cover: Vec<u8>,
    /// Rows from the top of the subfont to the glyph's first row.
    top: i32,
    left: i32,
    advance: i32,
}

/// A face's glyphs by rune, with its height and ascent.
struct Face {
    height: i32,
    ascent: i32,
    glyphs: BTreeMap<u32, Glyph>,
    /// 1 or 8 bits of coverage per pixel.
    depth: u32,
}

fn err(path: &Path, e: impl std::fmt::Display) -> Error {
    Error(format!("{}: {e}", path.display()))
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(|e| err(path, e))
}

fn write(path: &Path, data: &[u8]) -> Result<(), Error> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d).map_err(|e| err(d, e))?;
    }
    fs::write(path, data).map_err(|e| err(path, e))
}

/// Rasterizes the BMP glyphs of a TrueType or OpenType font at `px`.
fn ttf(data: &[u8], px: f32) -> Result<Face, Error> {
    let settings = fontdue::FontSettings {
        scale: px,
        ..fontdue::FontSettings::default()
    };
    let font = fontdue::Font::from_bytes(data, settings).map_err(|e| Error(e.to_string()))?;
    let mut runes: Vec<(u32, u16)> = font
        .chars()
        .iter()
        .map(|(c, i)| (u32::from(*c), i.get()))
        .filter(|&(c, _)| (0x20..=0xFFFF).contains(&c) && !(0x7F..0xA0).contains(&c))
        .collect();
    runes.sort();
    // Rune 0 is the font's missing-glyph box, as libdraw draws for runes
    // a font lacks.
    runes.insert(0, (0, 0));
    let lm = font
        .horizontal_line_metrics(px)
        .ok_or_else(|| Error::from("no horizontal metrics"))?;
    let mut ascent = lm.ascent.ceil() as i32;
    let mut descent = (-lm.descent).ceil() as i32;
    let mut raw = Vec::new();
    for (c, i) in runes {
        let (m, cover) = font.rasterize_indexed(i, px);
        ascent = ascent.max(m.ymin + m.height as i32);
        descent = descent.max(-m.ymin);
        raw.push((c, m, cover));
    }
    let mut glyphs = BTreeMap::new();
    for (c, m, cover) in raw {
        glyphs.insert(
            c,
            Glyph {
                w: m.width as i32,
                h: m.height as i32,
                cover,
                top: ascent - (m.ymin + m.height as i32),
                left: m.xmin,
                advance: m.advance_width.round() as i32,
            },
        );
    }
    Ok(Face {
        height: ascent + descent,
        ascent,
        glyphs,
        depth: 8,
    })
}

/// The runes a `hex` job takes when the recipe names none: the BMP.
const BMP: (u32, u32) = (0, 0xFFFF);

/// A `min-max` range of runes in hexadecimal.
fn rune_range(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once('-')?;
    let a = u32::from_str_radix(a, 16).ok()?;
    let b = u32::from_str_radix(b, 16).ok()?;
    (a <= b && b <= 0x10_FFFF).then_some((a, b))
}

/// Unifont's bitmaps: lines of `XXXX:` (or more digits above the BMP)
/// and 16 rows of hexadecimal bits. Only runes in `runes` are kept.
fn hex(data: &[u8], combining: Option<&[u8]>, runes: (u32, u32)) -> Result<Face, Error> {
    let text = std::str::from_utf8(data).map_err(|_| Error::from("hex: not text"))?;
    let mut offsets = BTreeMap::new();
    if let Some(c) = combining {
        let c = std::str::from_utf8(c).map_err(|_| Error::from("combining: not text"))?;
        for line in c.lines().filter(|l| !l.trim().is_empty()) {
            let (r, o) = line
                .split_once(':')
                .ok_or_else(|| Error(format!("combining: bad line {line:?}")))?;
            let r = u32::from_str_radix(r.trim(), 16)
                .map_err(|_| Error::from("combining: bad rune"))?;
            let o: i32 = o
                .trim()
                .parse()
                .map_err(|_| Error::from("combining: bad offset"))?;
            offsets.insert(r, o);
        }
    }
    let mut glyphs = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let bad = || Error(format!("hex: bad line {line:?}"));
        let (r, bits) = line.split_once(':').ok_or_else(bad)?;
        let r = u32::from_str_radix(r, 16).map_err(|_| bad())?;
        if r < runes.0 || r > runes.1 {
            continue;
        }
        let bits = bits.trim();
        if bits.len() % 16 != 0 || bits.is_empty() || !bits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad());
        }
        // Each of the 16 rows has bits.len() / 16 hex digits.
        let digits = bits.len() / 16;
        let w = digits as i32 * 4;
        let mut cover = Vec::with_capacity(16 * w as usize);
        for row in bits.as_bytes().chunks(digits) {
            for d in row {
                let v = (*d as char).to_digit(16).expect("checked");
                for b in (0..4).rev() {
                    cover.push(if v >> b & 1 != 0 { 255 } else { 0 });
                }
            }
        }
        let (left, advance) = match offsets.get(&r) {
            Some(&o) => (o, (w + o).max(0)),
            None => (0, w),
        };
        glyphs.insert(
            r,
            Glyph {
                w,
                h: 16,
                cover,
                top: 0,
                left,
                advance,
            },
        );
    }
    Ok(Face {
        height: 16,
        ascent: 14,
        glyphs,
        depth: 1,
    })
}

/// Writes the face as subfonts under `out/name/` and `out/name.font`.
fn write_face(face: &Face, out: &Path, name: &str) -> Result<(), Error> {
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error(format!("bad font name {name:?}")))?;
    let mut blocks: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for &r in face.glyphs.keys() {
        blocks.entry(r & !0xFF).or_default().push(r);
    }
    let mut ranges = Vec::new();
    for (&start, runes) in &blocks {
        let n = runes.last().expect("non-empty") - start + 1;
        let width: i32 = runes.iter().map(|r| face.glyphs[r].w).sum();
        let chan = if face.depth == 1 { GREY1 } else { GREY8 };
        let mut bits = Image::new(Rect::new(0, 0, width, face.height), chan)
            .ok_or_else(|| Error::from("subfont too big"))?;
        let mut info = Vec::with_capacity(n as usize + 1);
        let mut x = 0;
        for i in 0..n {
            let fc = match face.glyphs.get(&(start + i)) {
                Some(g) => {
                    let mut cover = vec![0u8; (g.w * face.height) as usize];
                    // Rows outside the subfont's height are cut off.
                    let (y0, y1) = (g.top.max(0), (g.top + g.h).min(face.height));
                    for y in y0..y1 {
                        let src = ((y - g.top) * g.w) as usize;
                        let dst = (y * g.w) as usize;
                        cover[dst..dst + g.w as usize]
                            .copy_from_slice(&g.cover[src..src + g.w as usize]);
                    }
                    let r = Rect::new(x, 0, x + g.w, face.height);
                    if g.w > 0 {
                        let data: Vec<u8> = if face.depth == 1 {
                            pack1(&cover, g.w, x)
                        } else {
                            cover
                        };
                        bits.load(r, &data).map_err(Error::from)?;
                    }
                    let fc = Fontchar {
                        x: x as u16,
                        top: y0.min(y1) as u8,
                        bottom: y1.max(y0) as u8,
                        left: g.left.clamp(-128, 127) as i8,
                        width: g.advance.clamp(0, 255) as u8,
                    };
                    x += g.w;
                    fc
                }
                None => Fontchar {
                    x: x as u16,
                    ..Fontchar::default()
                },
            };
            info.push(fc);
        }
        info.push(Fontchar {
            x: x as u16,
            ..Fontchar::default()
        });
        if x > i32::from(u16::MAX) {
            return Err(Error(format!("{name}: block {start:04X} too wide")));
        }
        let sub = Subfont {
            height: face.height,
            ascent: face.ascent,
            info,
            bits,
        };
        let file = format!("{base}/{start:04X}");
        write(&out.join(format!("{name}/{start:04X}")), &sub.write(true))?;
        // One range for each run of runes the face has.
        let mut i = 0;
        while i < runes.len() {
            let mut j = i;
            while j + 1 < runes.len() && runes[j + 1] == runes[j] + 1 {
                j += 1;
            }
            ranges.push(Range {
                min: runes[i],
                max: runes[j],
                offset: runes[i] - start,
                file: file.clone(),
            });
            i = j + 1;
        }
    }
    let f = FontFile {
        height: face.height,
        ascent: face.ascent,
        ranges,
    };
    write(&out.join(format!("{name}.font")), f.format().as_bytes())
}

/// Packs one row-major coverage bitmap `w` wide into 1-bit rows that
/// start at pixel `x`, as [`Image::load`] takes them.
fn pack1(cover: &[u8], w: i32, x: i32) -> Vec<u8> {
    let start = x / 8;
    let bpl = ((x + w + 7) / 8 - start) as usize;
    let mut out = Vec::new();
    for row in cover.chunks(w as usize) {
        let mut b = vec![0u8; bpl];
        for (i, &c) in row.iter().enumerate() {
            if c >= 128 {
                let bit = x as usize + i - start as usize * 8;
                b[bit / 8] |= 0x80 >> (bit % 8);
            }
        }
        out.extend(b);
    }
    out
}

/// The path of `to` relative to directory `from`, both relative to the
/// same place and without `..`.
fn relative(from: &Path, to: &Path) -> PathBuf {
    let f: Vec<_> = from.components().collect();
    let t: Vec<_> = to.components().collect();
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let mut p = PathBuf::new();
    for _ in common..f.len() {
        p.push("..");
    }
    for c in &t[common..] {
        p.push(c);
    }
    p
}

/// `p` with each `..` taking away the name before it.
fn normal(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir if out.file_name().is_some() => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Writes `out/name.font` from the fonts `parts` (names under `out`).
fn merge(out: &Path, name: &str, parts: &[String]) -> Result<(), Error> {
    let dir = Path::new(name).parent().unwrap_or(Path::new(""));
    let (mut ascent, mut descent) = (0, 0);
    let mut ranges = Vec::new();
    for p in parts {
        let path = out.join(format!("{p}.font"));
        let text = String::from_utf8(read(&path)?).map_err(|_| err(&path, "not text"))?;
        let f = FontFile::parse(&text).map_err(|e| err(&path, e))?;
        ascent = i32::max(ascent, f.ascent);
        descent = i32::max(descent, f.height - f.ascent);
        let pdir = Path::new(p).parent().unwrap_or(Path::new(""));
        for mut r in f.ranges {
            let file = relative(dir, &normal(&pdir.join(&r.file)));
            r.file = file
                .to_str()
                .ok_or_else(|| err(&path, "bad file name"))?
                .replace('\\', "/");
            ranges.push(r);
        }
    }
    let f = FontFile {
        height: ascent + descent,
        ascent,
        ranges,
    };
    write(&out.join(format!("{name}.font")), f.format().as_bytes())
}

fn job(words: &[&str], indir: &Path, out: &Path) -> Result<(), Error> {
    let bad = || Error(format!("bad recipe line: {}", words.join(" ")));
    match words {
        ["ttf", src, px, name] => {
            let px: f32 = px.parse().map_err(|_| bad())?;
            let face = ttf(&read(&indir.join(src))?, px)?;
            write_face(&face, out, name)
        }
        ["hex", rest @ ..] => {
            let (runes, rest) = match rest {
                [r, rest @ ..] if let Some(runes) = rune_range(r) => (runes, rest),
                _ => (BMP, rest),
            };
            let (src, combining, name) = match rest {
                [src, name] => (src, None, name),
                [src, c, name] => (src, Some(read(&indir.join(c))?), name),
                _ => return Err(bad()),
            };
            let face = hex(&read(&indir.join(src))?, combining.as_deref(), runes)?;
            write_face(&face, out, name)
        }
        ["merge", name, parts @ ..] if !parts.is_empty() => {
            let parts: Vec<String> = parts.iter().map(|s| String::from(*s)).collect();
            merge(out, name, &parts)
        }
        ["copy", src, dest] => write(&out.join(dest), &read(&indir.join(src))?),
        _ => Err(bad()),
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "fontgen [-i indir] [-o outdir] recipe");
    let (mut indir, mut out) = (None, PathBuf::from("."));
    while let Some(c) = arg.opt() {
        match c {
            'i' => indir = Some(PathBuf::from(arg.earg()?)),
            'o' => out = PathBuf::from(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let [recipe] = arg.argv().try_into().map_err(|_| usage)?;
    let recipe = PathBuf::from(recipe);
    let indir = indir.unwrap_or_else(|| {
        recipe
            .parent()
            .map_or(PathBuf::from("."), Path::to_path_buf)
    });
    let text = String::from_utf8(read(&recipe)?).map_err(|_| err(&recipe, "not text"))?;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let words: Vec<&str> = line.split_ascii_whitespace().collect();
        if !words.is_empty() {
            job(&words, &indir, &out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        let r = relative(Path::new(""), Path::new("go/regular.13/0000"));
        assert_eq!(r, Path::new("go/regular.13/0000"));
        let r = relative(Path::new("sans"), Path::new("unifont/unifont.16/4E00"));
        assert_eq!(r, Path::new("../unifont/unifont.16/4E00"));
        let r = relative(Path::new("go"), Path::new("go/regular.13/0000"));
        assert_eq!(r, Path::new("regular.13/0000"));
        let n = normal(Path::new("sans/../go/regular.13/0000"));
        assert_eq!(n, Path::new("go/regular.13/0000"));
    }

    #[test]
    fn packing() {
        assert_eq!(pack1(&[255, 0, 255], 3, 6), vec![0b0000_0010, 0b1000_0000]);
    }

    #[test]
    fn unifont_lines() {
        let f = hex(
            b"0041:0000000018242442427E424242420000\n0300:00000000000000000000000000000000\n",
            Some(b"0300:-8\n"),
            BMP,
        )
        .unwrap();
        let a = &f.glyphs[&0x41];
        assert_eq!((a.w, a.h, a.advance, a.left), (8, 16, 8, 0));
        assert_eq!(&a.cover[4 * 8..5 * 8], &[0, 0, 0, 255, 255, 0, 0, 0]);
        let m = &f.glyphs[&0x300];
        assert_eq!((m.advance, m.left), (0, -8));
    }

    #[test]
    fn upper_planes() {
        let text = format!(
            "0041:0000000018242442427E424242420000\n01F600:{}\n",
            "03E0".repeat(16)
        );
        let text = text.as_bytes();
        assert_eq!(rune_range("10000-3FFFF"), Some((0x10000, 0x3FFFF)));
        assert_eq!(rune_range("3FFFF-10000"), None);
        assert_eq!(rune_range("0-110000"), None);
        let f = hex(text, None, (0x10000, 0x1FFFF)).unwrap();
        assert_eq!(f.glyphs.keys().copied().collect::<Vec<_>>(), [0x1F600]);
        assert_eq!(f.glyphs[&0x1F600].w, 16);
        let f = hex(text, None, BMP).unwrap();
        assert_eq!(f.glyphs.keys().copied().collect::<Vec<_>>(), [0x41]);
    }
}
