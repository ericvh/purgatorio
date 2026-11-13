//! Fonts: font(6) files, their subfonts loaded when a rune needs them,
//! and a glyph cache on the draw device, as libdraw keeps one.
//!
//! A [`Font`] reads its font file when opened and each subfont file the
//! first time one of its runes is drawn or measured. Glyphs are copied
//! into a cache image on the device with `l` and drawn from there with
//! `s` and `x` (draw(3)). The cache has [`NCACHE`] slots, each as wide as
//! the widest glyph seen so far (the cache is made again, empty, when a
//! wider one comes), and the least recently used slot is reused.
//!
//! A rune is looked up in the font's ranges in order. Unlike libdraw,
//! which only tries the first range that holds the rune, a range whose
//! subfont lacks the glyph (or cannot be read) passes the rune on to the
//! next, so a font can list a fallback (Unifont) after its main face.
//! When no range has it, rune 0 is drawn, as in libdraw; when that is
//! missing too, the rune is skipped.

use std::collections::HashMap;

use purg_memdraw::{FontFile, Fontchar, Subfont};

use super::{Display, GREY8, Image, Point, Rect, TRANSPARENT, put_point, put_rect};
use crate::Error;

/// Glyph slots in a font's cache.
pub const NCACHE: usize = 512;

/// The most glyphs in one `s` or `x` message, as libdraw sends.
const MAXSTRING: usize = 100;

/// The font file a program gets when it names none: `$font`, or this.
pub const DEFAULT_FONT: &str = "/fonts/default.font";

/// A subfont: the file's glyphs, and its image on the device once a glyph
/// of it has been cached.
struct Sub {
    font: Subfont,
    image: Option<Image>,
}

/// Where a rune's glyph is: the subfont (by file) and the glyph in it.
#[derive(Clone, Copy)]
struct Found {
    sub: usize,
    index: usize,
}

struct Slot {
    rune: Option<char>,
    /// When the slot was last used, in [`Cache::clock`] ticks.
    used: u64,
}

struct Cache {
    image: Image,
    /// Slot width.
    wid: i32,
    slots: Vec<Slot>,
    map: HashMap<char, usize>,
    /// Advances once per message, so the slots a message being built
    /// uses are never the oldest.
    clock: u64,
}

pub struct Font {
    pub name: String,
    pub height: i32,
    pub ascent: i32,
    dir: String,
    file: FontFile,
    /// Subfonts by file name, loaded or not (`None`: it could not be
    /// read).
    by_name: HashMap<String, Option<usize>>,
    subs: Vec<Sub>,
    /// Lookups already done.
    found: HashMap<char, Option<Found>>,
    cache: Option<Cache>,
}

impl Font {
    /// Opens font file `name`. Subfonts are read when needed.
    pub fn open(name: &str) -> Result<Font, Error> {
        let text = std::fs::read_to_string(name)
            .map_err(|_| Error(format!("{name}: {}", crate::errstr())))?;
        let file = FontFile::parse(&text).map_err(|e| Error(format!("{name}: {e}")))?;
        let dir = match name.rfind('/') {
            Some(i) => String::from(&name[..i + 1]),
            None => String::new(),
        };
        Ok(Font {
            name: String::from(name),
            height: file.height,
            ascent: file.ascent,
            dir,
            file,
            by_name: HashMap::new(),
            subs: Vec::new(),
            found: HashMap::new(),
            cache: None,
        })
    }

    /// Opens the default font: the file `$font` names, or
    /// [`DEFAULT_FONT`].
    pub fn default_font() -> Result<Font, Error> {
        let name = crate::env::get("font")
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| String::from(DEFAULT_FONT));
        Font::open(&name)
    }

    /// The subfont in file `name`, read now if it has not been.
    fn sub(&mut self, name: &str) -> Option<usize> {
        if let Some(s) = self.by_name.get(name) {
            return *s;
        }
        let path = if name.starts_with('/') {
            String::from(name)
        } else {
            format!("{}{name}", self.dir)
        };
        let s = std::fs::read(&path)
            .ok()
            .and_then(|data| Subfont::read(&data).ok())
            .map(|font| {
                self.subs.push(Sub { font, image: None });
                self.subs.len() - 1
            });
        self.by_name.insert(String::from(name), s);
        s
    }

    fn find(&mut self, c: char) -> Option<Found> {
        if let Some(f) = self.found.get(&c) {
            return *f;
        }
        let r = u32::from(c);
        let mut found = None;
        for i in 0..self.file.ranges.len() {
            let range = &self.file.ranges[i];
            if r < range.min || r > range.max {
                continue;
            }
            let index = (r - range.min + range.offset) as usize;
            let name = range.file.clone();
            if let Some(sub) = self.sub(&name)
                && self.subs[sub].font.has(index)
            {
                found = Some(Found { sub, index });
                break;
            }
        }
        self.found.insert(c, found);
        found
    }

    /// The glyph for `c`, falling back to rune 0's.
    fn lookup(&mut self, c: char) -> Option<(Found, Fontchar, i32)> {
        let f = self.find(c).or_else(|| self.find('\0'))?;
        let info = &self.subs[f.sub].font.info;
        let cw = i32::from(info[f.index + 1].x) - i32::from(info[f.index].x);
        Some((f, info[f.index], cw))
    }

    /// The width of `s` in pixels: the sum of its glyphs' advances.
    pub fn string_width(&mut self, s: &str) -> i32 {
        s.chars()
            .filter_map(|c| self.lookup(c))
            .map(|(_, fc, _)| i32::from(fc.width))
            .sum()
    }

    /// Frees the font's images on the device.
    pub fn free(&mut self, d: &mut Display) -> Result<(), Error> {
        if let Some(c) = self.cache.take() {
            d.free(c.image)?;
        }
        for s in &mut self.subs {
            if let Some(i) = s.image.take() {
                d.free(i)?;
            }
        }
        Ok(())
    }

    /// Makes the cache, empty, with slots `wid` wide.
    fn new_cache(&mut self, d: &mut Display, wid: i32) -> Result<(), Error> {
        if let Some(c) = self.cache.take() {
            d.free(c.image)?;
        }
        let r = Rect::new(0, 0, NCACHE as i32 * wid, self.height);
        let image = d.alloc(r, GREY8, false, TRANSPARENT)?;
        let mut m = vec![b'i'];
        m.extend(image.id.to_le_bytes());
        m.extend((NCACHE as u32).to_le_bytes());
        m.push(self.ascent.clamp(0, 255) as u8);
        d.msg(&m)?;
        self.cache = Some(Cache {
            image,
            wid,
            slots: (0..NCACHE)
                .map(|_| Slot {
                    rune: None,
                    used: 0,
                })
                .collect(),
            map: HashMap::new(),
            clock: 1,
        });
        Ok(())
    }

    /// The cache slot holding `c`'s glyph, loading it if needed. `Ok(None)`
    /// means the cache had to be made again (wider) and the caller must
    /// send what it has built first, then ask again.
    fn slot(
        &mut self,
        d: &mut Display,
        c: char,
        f: Found,
        fc: Fontchar,
        cw: i32,
        pending: bool,
    ) -> Result<Option<usize>, Error> {
        let fits = self.cache.as_ref().is_some_and(|k| k.wid >= cw);
        if !fits {
            if pending {
                return Ok(None);
            }
            let wid = self
                .cache
                .as_ref()
                .map_or(self.height.max(cw), |k| k.wid.max(cw));
            self.new_cache(d, wid.max(1))?;
        }
        let cache = self.cache.as_mut().expect("made above");
        if let Some(&i) = cache.map.get(&c) {
            cache.slots[i].used = cache.clock;
            return Ok(Some(i));
        }
        // The least recently used slot; ties go to the lowest.
        let i = (0..NCACHE)
            .min_by_key(|&i| cache.slots[i].used)
            .expect("slots");
        if let Some(old) = cache.slots[i].rune.take() {
            cache.map.remove(&old);
        }
        cache.slots[i] = Slot {
            rune: Some(c),
            used: cache.clock,
        };
        cache.map.insert(c, i);
        let (cid, wid) = (cache.image.id, cache.wid);
        // The subfont's image goes to the device with its first glyph.
        let sub = &mut self.subs[f.sub];
        if sub.image.is_none() {
            let bits = &sub.font.bits;
            let img = d.alloc(bits.r, bits.chan(), false, TRANSPARENT)?;
            if !bits.r.is_empty() {
                d.load(&img, bits.r, &bits.unload(bits.r).map_err(Error::from)?)?;
            }
            sub.image = Some(img);
        }
        let sid = sub.image.as_ref().expect("loaded above").id;
        // Rows move by the difference in ascent, and are cut to the
        // cache's height.
        let shift = self.ascent - sub.font.ascent;
        let mut top = i32::from(fc.top) + shift;
        let bottom = (i32::from(fc.bottom) + shift).min(self.height);
        let mut sy = i32::from(fc.top);
        if top < 0 {
            sy -= top;
            top = 0;
        }
        let bottom = bottom.max(top);
        let x = i as i32 * wid;
        let mut m = vec![b'l'];
        m.extend(cid.to_le_bytes());
        m.extend(sid.to_le_bytes());
        m.extend((i as u16).to_le_bytes());
        put_rect(&mut m, Rect::new(x, top, x + cw, bottom));
        put_point(&mut m, Point::new(i32::from(fc.x), sy));
        m.push(fc.left as u8);
        m.push(fc.width);
        d.msg(&m)?;
        Ok(Some(i))
    }
}

impl Display {
    /// draw(2)'s `string`: draws `s` in `font` with the top left of its
    /// first glyph's line at `p`, `src` aligned so that `sp` falls on `p`.
    /// Returns the point after the last glyph.
    pub fn string(
        &mut self,
        dst: &Image,
        p: Point,
        src: &Image,
        sp: Point,
        font: &mut Font,
        s: &str,
    ) -> Result<Point, Error> {
        self.text(dst, p, src, sp, font, s, None)
    }

    /// draw(2)'s `stringbg`: as [`Display::string`], over a background
    /// drawn from `bg` aligned so that `bgp` falls on `p`.
    #[allow(clippy::too_many_arguments)]
    pub fn string_bg(
        &mut self,
        dst: &Image,
        p: Point,
        src: &Image,
        sp: Point,
        font: &mut Font,
        s: &str,
        bg: &Image,
        bgp: Point,
    ) -> Result<Point, Error> {
        self.text(dst, p, src, sp, font, s, Some((bg, bgp)))
    }

    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        dst: &Image,
        p: Point,
        src: &Image,
        mut sp: Point,
        font: &mut Font,
        s: &str,
        mut bg: Option<(&Image, Point)>,
    ) -> Result<Point, Error> {
        // The glyphs of the message being built: slots and their advance.
        let mut slots: Vec<u16> = Vec::new();
        let (mut at, mut width) = (p, 0);
        let mut chars = s.chars().peekable();
        loop {
            let next = chars.peek().copied();
            let mut send = next.is_none() || slots.len() == MAXSTRING;
            let mut glyph = None;
            if !send && let Some(c) = next {
                match font.lookup(c) {
                    None => {
                        chars.next();
                        continue;
                    }
                    Some((f, fc, cw)) => match font.slot(self, c, f, fc, cw, !slots.is_empty())? {
                        Some(i) => glyph = Some((i, fc.width)),
                        None => send = true,
                    },
                }
            }
            if let Some((i, w)) = glyph {
                chars.next();
                slots.push(i as u16);
                width += i32::from(w);
                continue;
            }
            if send && !slots.is_empty() {
                let cache = font.cache.as_mut().expect("glyphs are cached");
                let c = if bg.is_some() { b'x' } else { b's' };
                let mut m = vec![c];
                for id in [dst.id, src.id, cache.image.id] {
                    m.extend(id.to_le_bytes());
                }
                put_point(&mut m, Point::new(at.x, at.y + font.ascent));
                put_rect(&mut m, dst.clipr);
                put_point(&mut m, sp);
                m.extend((slots.len() as u16).to_le_bytes());
                if let Some((b, bp)) = &mut bg {
                    m.extend(b.id.to_le_bytes());
                    put_point(&mut m, *bp);
                    bp.x += width;
                }
                for i in &slots {
                    m.extend(i.to_le_bytes());
                }
                cache.clock += 1;
                self.msg(&m)?;
                at.x += width;
                sp.x += width;
                slots.clear();
                width = 0;
            }
            if next.is_none() {
                return Ok(at);
            }
        }
    }
}
