//! `#i`: the draw device (draw(3)), over a [`Screen`].
//!
//! `#i/draw/new` makes a connection: opening it allocates connection `n`
//! and the open file becomes `n/ctl`, which reads as 144 bytes, twelve
//! 12-byte fields: the connection number, the image the information is
//! about (0, the screen, until another id is written to `ctl`), its
//! channels, its replication bit, its rectangle and its clip rectangle.
//! `n/data` takes binary messages, little-endian, several to a write:
//!
//! | Message | Effect |
//! |---|---|
//! | `b id[4] screenid[4] refresh[1] chan[4] repl[1] R[16] clipR[16] rgba[4]` | allocate an image filled with a colour |
//! | `f id[4]` | free an image |
//! | `c id[4] repl[1] clipR[16]` | set an image's replication bit and clip |
//! | `d dst[4] src[4] mask[4] R[16] sp[8] mp[8]` | draw |
//! | `D on[1]` | debugging; ignored |
//! | `e dst[4] src[4] c[8] a[4] b[4] thick[4] sp[8] alpha[4] phi[4]` | ellipse or arc (alpha's top bit set) |
//! | `E` (as `e`) | filled ellipse or arc |
//! | `L dst[4] p0[8] p1[8] end0[4] end1[4] thick[4] src[4] sp[8]` | line |
//! | `p dst[4] n[2] end0[4] end1[4] thick[4] src[4] sp[8] dp[...]` | polyline of n+1 points |
//! | `P dst[4] n[2] wind[4] ignore[8] src[4] sp[8] dp[...]` | filled polygon |
//! | `r id[4] R[16]` | read pixels: the next read of `data` returns them |
//! | `y id[4] R[16] data[...]`, `Y` | write pixels, raw or compressed (image(6)) |
//! | `o id[4] rmin[8] screenrmin[8]` | set a window's origin; nothing for other images |
//! | `O op[1]` | the compositing operator for the next drawing message |
//! | `v` | flush damage to the display |
//! | `i id[4] n[4] ascent[1]` | make image `id` a font cache of `n` glyphs (at most 4096) |
//! | `l id[4] srcid[4] index[2] R[16] P[8] left[1] width[1]` | load glyph `index` of cache `id`: copy `R` from `srcid` at `P`, and record `R`, `left` and `width` |
//! | `s dst[4] src[4] font[4] P[8] clipR[16] sp[8] n[2] index[2n]` | draw glyphs of cache `font` from baseline point `P` |
//! | `x` (as `s`, with `bg[4] bgp[8]` after `n`) | the same over a background drawn from `bg` |
//!
//! Polygon points are coded as devdraw codes them: each coordinate a byte
//! that is a 7-bit signed difference from the previous one, or, with the
//! top bit set, a 23-bit absolute value in three bytes. A glyph drawn by
//! `s` is devdraw's `drawchar`: the cache rectangle recorded by `l`,
//! placed `left` pixels right of the point with its top `ascent - miny`
//! above the baseline, used as the mask for the source, which moves on
//! with the point by `width`. Screens and windows (`A`, `F`, `S`, `t`)
//! and named images (`n`, `N`) are not implemented and fail with `bad
//! draw command`. Errors are devdraw's strings.
//!
//! When the display changes size ([`Screen::size`] and
//! [`purg_platform::screen::Input::resized`]) the device makes a new
//! screen image before its next request, keeping what fits of the old
//! one. A read of `refresh` blocks until that happens and then returns a
//! 20-byte record, as devdraw's refresh records are: the image id (0, the
//! screen) and its new rectangle, four little-endian 4-byte integers. The
//! client then reads `ctl` for the screen and redraws.
//!
//! The screen image is the device's own; `v` gives the screen's damaged
//! rectangle to [`Screen::flush`]. A connection ends, and its images go,
//! when its last file is closed.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_memdraw::{self as md, Chan, Image, Op, Point, Rect};
use purg_platform::screen::Screen;
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan as KChan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};

const ENODRAWIMAGE: &str = "unknown id for draw image";
const ENODRAWSCREEN: &str = "unknown id for draw screen";
const ESHORTDRAW: &str = "short draw message";
const ESHORTREAD: &str = "draw read too short";
const EIMAGEEXISTS: &str = "image id in use";
const EDRAWMEM: &str = "out of memory: image";
const EREADOUTSIDE: &str = "readimage outside image";
const EWRITEOUTSIDE: &str = "writeimage outside image";
const EBADCMD: &str = "bad draw command";
const ENODATA: &str = "no draw data";
const ECTL: &str = "unknown draw control request";
const ENOTFONT: &str = "image not a font";
const EINDEX: &str = "character index out of range";
const EDISPLAYFONT: &str = "cannot use display as font";
const EFONTSIZE: &str = "bad font size (4096 chars max)";
/// What `new` and `ctl` read: twelve fields of 11 characters, each
/// followed by a blank.
const INFO: usize = 144;

const QTOP: u64 = 0;
const QDRAW: u64 = 1;
const QNEW: u64 = 2;
// A connection's files: path = id << 8 | kind.
const KDIR: u64 = 3;
const KCTL: u64 = 4;
const KDATA: u64 = 5;
const KCOLORMAP: u64 = 6;
const KREFRESH: u64 = 7;
const FILES: &[(&str, u64)] = &[
    ("colormap", KCOLORMAP),
    ("ctl", KCTL),
    ("data", KDATA),
    ("refresh", KREFRESH),
];

fn conn_of(path: u64) -> u32 {
    (path >> 8) as u32
}

fn kind_of(path: u64) -> u64 {
    if path >> 8 == 0 { path } else { path & 0xFF }
}

/// One connection's state.
struct Conn {
    images: BTreeMap<u32, Image>,
    /// Whether id 0 still names the screen (`f 0` drops it).
    screen: bool,
    /// The image the next read of `ctl` reports on: the screen at first,
    /// then whatever `ctl` was last told; a read clears it.
    info: Option<u32>,
    /// The operator for the next drawing message.
    op: Op,
    /// Pixels waiting for a read of `data`, after `r`.
    readdata: Option<Vec<u8>>,
    /// Font caches, by image id (`i`).
    fonts: BTreeMap<u32, FontCache>,
    /// The screen resizes `refresh` has reported.
    resizes: u64,
}

/// A font cache on an image: its ascent and its glyphs.
struct FontCache {
    ascent: i32,
    chars: Vec<md::CacheChar>,
}

impl Conn {
    fn new(resizes: u64) -> Conn {
        Conn {
            images: BTreeMap::new(),
            screen: true,
            info: Some(0),
            op: Op::SOVER_D,
            readdata: None,
            fonts: BTreeMap::new(),
            resizes,
        }
    }

    /// The font cache on image `id`.
    fn font(&self, id: u32) -> Result<&FontCache> {
        if !self.images.contains_key(&id) {
            return Err(Error::new(ENODRAWIMAGE));
        }
        self.fonts.get(&id).ok_or(Error::new(ENOTFONT))
    }
}

struct Client {
    opens: Mutex<u32>,
    conn: Mutex<Conn>,
}

struct ScreenImage {
    image: Image,
    damage: Option<Rect>,
    /// The display's resize count this image was made for.
    resizes: u64,
}

pub struct DevDraw {
    display: Arc<dyn Screen>,
    user: String,
    screen: Mutex<ScreenImage>,
    clients: Mutex<BTreeMap<u32, Arc<Client>>>,
    next: Mutex<u32>,
}

impl DevDraw {
    /// A draw device showing its screen on `display`. Fails if the
    /// display's size or channels make no image memdraw can draw on.
    pub fn new(display: Arc<dyn Screen>, user: String) -> Option<DevDraw> {
        let resizes = display.input().resizes();
        let image = DevDraw::screen_image(&*display)?;
        let r = image.r;
        Some(DevDraw {
            display,
            user,
            screen: Mutex::new(ScreenImage {
                image,
                damage: Some(r),
                resizes,
            }),
            clients: Mutex::new(BTreeMap::new()),
            next: Mutex::new(1),
        })
    }

    /// A white screen image of the display's size.
    fn screen_image(display: &dyn Screen) -> Option<Image> {
        let (w, h) = display.size();
        let r = Rect::new(0, 0, i32::try_from(w).ok()?, i32::try_from(h).ok()?);
        let mut image = Image::new(r, Chan(display.chan()))?;
        image.fill(md::rgba(0xFFFF_FFFF));
        Some(image)
    }

    /// Makes a new screen image if the display has changed size since the
    /// last one, with the old one's pixels where they fit.
    fn sync_size(&self) {
        let resizes = self.display.input().resizes();
        let mut s = self.screen.lock();
        if s.resizes == resizes {
            return;
        }
        s.resizes = resizes;
        let Some(mut image) = DevDraw::screen_image(&*self.display) else {
            return;
        };
        if image.r != s.image.r {
            let old = &s.image;
            md::draw(&mut image, old.r, old, old.r.min, None, old.r.min, Op::S);
            s.damage = Some(image.r);
            s.image = image;
        }
    }

    fn qid(path: u64) -> Qid {
        let dir = matches!(kind_of(path), QTOP | QDRAW | KDIR);
        Qid {
            path,
            vers: 0,
            qtype: if dir { QTDIR } else { QTFILE },
        }
    }

    fn dir(&self, path: u64) -> Dir {
        let id = conn_of(path);
        let (name, perm) = match kind_of(path) {
            QTOP => (String::from("#i"), DMDIR | 0o555),
            QDRAW => (String::from("draw"), DMDIR | 0o555),
            QNEW => (String::from("new"), 0o666),
            KDIR => (format!("{id}"), DMDIR | 0o555),
            k => {
                let name = FILES.iter().find(|f| f.1 == k).map_or("?", |f| f.0);
                (
                    String::from(name),
                    if k == KREFRESH { 0o444 } else { 0o666 },
                )
            }
        };
        Dir {
            dtype: 'i' as u16,
            qid: DevDraw::qid(path),
            mode: perm,
            name,
            uid: self.user.clone(),
            gid: self.user.clone(),
            ..Dir::default()
        }
    }

    fn client(&self, id: u32) -> Result<Arc<Client>> {
        self.clients
            .lock()
            .get(&id)
            .cloned()
            .ok_or(Error::new(error::ENONEXIST))
    }

    /// The 144-byte description of image `id`, for `new` and `ctl`.
    fn info(&self, conn_id: u32, conn: &Conn, id: u32) -> Result<String> {
        let describe = |i: &Image| {
            let f = |v: i32| format!("{v:>11} ");
            let chan = format!("{}", i.chan());
            let mut s = format!("{conn_id:>11} {id:>11} {chan:>11} {:>11} ", i.repl as u32);
            for v in [i.r.min.x, i.r.min.y, i.r.max.x, i.r.max.y] {
                s.push_str(&f(v));
            }
            for v in [i.clipr.min.x, i.clipr.min.y, i.clipr.max.x, i.clipr.max.y] {
                s.push_str(&f(v));
            }
            s
        };
        if id == 0 && conn.screen {
            return Ok(describe(&self.screen.lock().image));
        }
        conn.images
            .get(&id)
            .map(describe)
            .ok_or(Error::new(ENODRAWIMAGE))
    }

    /// Sends the screen's damage to the display.
    fn flush(&self) {
        let mut s = self.screen.lock();
        let Some(r) = s.damage.take().and_then(|d| d.clip(&s.image.r)) else {
            return;
        };
        if let Ok(pixels) = s.image.unload(r) {
            let stride = md::bytes_per_line(r, s.image.depth());
            self.display
                .flush([r.min.x, r.min.y, r.max.x, r.max.y], &pixels, stride);
        }
    }

    /// Runs `f` on destination image `dst` with the source images `srcs`
    /// (a source that is the destination is a copy of it), and records
    /// the damage when the destination is the screen.
    fn with<R>(
        &self,
        conn: &mut Conn,
        dst: u32,
        srcs: &[u32],
        f: impl FnOnce(&mut Image, &[&Image]) -> (R, Option<Rect>),
    ) -> Result<R> {
        let exists = |id: u32| {
            if id == 0 {
                conn.screen
            } else {
                conn.images.contains_key(&id)
            }
        };
        if !exists(dst) || !srcs.iter().all(|&s| exists(s)) {
            return Err(Error::new(ENODRAWIMAGE));
        }
        if dst == 0 {
            let mut g = self.screen.lock();
            let copies: Vec<Option<Image>> = srcs
                .iter()
                .map(|&s| (s == 0).then(|| g.image.clone()))
                .collect();
            let refs: Vec<&Image> = srcs
                .iter()
                .zip(&copies)
                .map(|(s, c)| c.as_ref().unwrap_or_else(|| &conn.images[s]))
                .collect();
            let (out, damage) = f(&mut g.image, &refs);
            if let Some(d) = damage {
                g.damage = Some(g.damage.map_or(d, |old| old.union(&d)));
            }
            return Ok(out);
        }
        let Some(mut d) = conn.images.remove(&dst) else {
            return Err(Error::new(ENODRAWIMAGE));
        };
        let out = {
            let g = srcs.contains(&0).then(|| self.screen.lock());
            let copies: Vec<Option<Image>> = srcs
                .iter()
                .map(|&s| (s == dst).then(|| d.clone()))
                .collect();
            let refs: Vec<&Image> = srcs
                .iter()
                .zip(&copies)
                .map(|(s, c)| match (c, *s) {
                    (Some(c), _) => c,
                    (None, 0) => &g.as_ref().expect("screen locked").image,
                    (None, s) => &conn.images[&s],
                })
                .collect();
            f(&mut d, &refs).0
        };
        conn.images.insert(dst, d);
        Ok(out)
    }

    /// Applies the messages in `a`, in order; stops at the first that
    /// fails.
    fn messages(&self, conn: &mut Conn, mut a: &[u8]) -> Result<()> {
        while !a.is_empty() {
            let n = self.message(conn, a)?;
            a = &a[n..];
        }
        Ok(())
    }

    /// Applies the message at the start of `a`; returns its length.
    fn message(&self, conn: &mut Conn, a: &[u8]) -> Result<usize> {
        let need = |m: usize| {
            if a.len() < m {
                Err(Error::new(ESHORTDRAW))
            } else {
                Ok(m)
            }
        };
        match a[0] {
            b'b' => {
                let m = need(51)?;
                let id = u32le(a, 1);
                if (id == 0 && conn.screen) || conn.images.contains_key(&id) {
                    return Err(Error::new(EIMAGEEXISTS));
                }
                if u32le(a, 5) != 0 {
                    return Err(Error::new(ENODRAWSCREEN));
                }
                let r = rect(a, 15);
                let mut i = Image::new(r, Chan(u32le(a, 10))).ok_or(Error::new(EDRAWMEM))?;
                i.repl = a[14] != 0;
                let clipr = rect(a, 31);
                i.clipr = if i.repl {
                    clipr
                } else {
                    clipr.clip(&r).unwrap_or(Rect {
                        min: r.min,
                        max: r.min,
                    })
                };
                i.fill(md::rgba(u32le(a, 47)));
                conn.images.insert(id, i);
                Ok(m)
            }
            b'f' => {
                let m = need(5)?;
                let id = u32le(a, 1);
                if id == 0 && conn.screen {
                    conn.screen = false;
                } else if conn.images.remove(&id).is_none() {
                    return Err(Error::new(ENODRAWIMAGE));
                }
                conn.fonts.remove(&id);
                Ok(m)
            }
            b'c' => {
                let m = need(22)?;
                let (repl, clipr) = (a[5] != 0, rect(a, 6));
                self.with(conn, u32le(a, 1), &[], |d, _| {
                    d.repl = repl;
                    d.clipr = clipr;
                    ((), None)
                })?;
                Ok(m)
            }
            b'd' => {
                let m = need(45)?;
                let (r, sp, mp) = (rect(a, 13), point(a, 29), point(a, 37));
                let op = take_op(conn);
                self.with(conn, u32le(a, 1), &[u32le(a, 5), u32le(a, 9)], |d, s| {
                    ((), md::draw(d, r, s[0], sp, Some(s[1]), mp, op))
                })?;
                Ok(m)
            }
            b'D' => need(2),
            b'e' | b'E' => {
                let m = need(45)?;
                let (c, ea, eb, thick) = (point(a, 9), i32le(a, 17), i32le(a, 21), i32le(a, 25));
                let (sp, mut alpha, phi) = (point(a, 29), i32le(a, 37), i32le(a, 41));
                if ea < 0 || eb < 0 {
                    return Err(Error::new("invalid ellipse semidiameter"));
                }
                if thick < 0 {
                    return Err(Error::new("negative ellipse thickness"));
                }
                // The top bit of alpha marks an arc; the angle itself is
                // 31-bit signed.
                let arc = alpha & (1 << 31) != 0;
                if arc && alpha & (1 << 30) == 0 {
                    alpha &= !(1 << 31);
                }
                let fill = a[0] == b'E';
                let op = take_op(conn);
                self.with(conn, u32le(a, 1), &[u32le(a, 5)], |d, s| {
                    let r = match (fill, arc) {
                        (false, false) => md::ellipse(d, c, ea, eb, thick, s[0], sp, op),
                        (true, false) => md::fill_ellipse(d, c, ea, eb, s[0], sp, op),
                        (false, true) => md::arc(d, c, ea, eb, thick, s[0], sp, alpha, phi, op),
                        (true, true) => md::fill_arc(d, c, ea, eb, s[0], sp, alpha, phi, op),
                    };
                    ((), r)
                })?;
                Ok(m)
            }
            b'L' => {
                let m = need(45)?;
                let (p0, p1, e0, e1) = (point(a, 5), point(a, 13), i32le(a, 21), i32le(a, 25));
                let (thick, sp) = (i32le(a, 29), point(a, 37));
                if thick < 0 {
                    return Err(Error::new("negative line width"));
                }
                let op = take_op(conn);
                self.with(conn, u32le(a, 1), &[u32le(a, 33)], |d, s| {
                    ((), md::line(d, p0, p1, e0, e1, thick, s[0], sp, op))
                })?;
                Ok(m)
            }
            b'p' | b'P' => {
                need(31)?;
                let n = i16::from_le_bytes([a[5], a[6]]);
                if n < 0 {
                    return Err(Error::new("negative count in polygon"));
                }
                let (e0, e1, thick, sp) = (i32le(a, 7), i32le(a, 11), i32le(a, 15), point(a, 23));
                if a[0] == b'p' && thick < 0 {
                    return Err(Error::new("negative polygon line width"));
                }
                let mut pts = Vec::with_capacity(n as usize + 1);
                let mut u = 31;
                let (mut ox, mut oy) = (0, 0);
                for _ in 0..=n {
                    let x = coord(a, &mut u, ox)?;
                    let y = coord(a, &mut u, oy)?;
                    pts.push(Point::new(x, y));
                    (ox, oy) = (x, y);
                }
                let op = take_op(conn);
                let fill = a[0] == b'P';
                self.with(conn, u32le(a, 1), &[u32le(a, 19)], |d, s| {
                    let r = if fill {
                        md::fill_poly(d, &pts, e0, s[0], sp, op)
                    } else {
                        md::poly(d, &pts, e0, e1, thick, s[0], sp, op)
                    };
                    ((), r)
                })?;
                Ok(u)
            }
            b'r' => {
                let m = need(21)?;
                let r = rect(a, 5);
                let data = self.with(conn, u32le(a, 1), &[], |d, _| (d.unload(r), None))?;
                conn.readdata = Some(data.map_err(|_| Error::new(EREADOUTSIDE))?);
                Ok(m)
            }
            b'y' | b'Y' => {
                need(21)?;
                let r = rect(a, 5);
                let data = &a[21..];
                let compressed = a[0] == b'Y';
                let used = self.with(conn, u32le(a, 1), &[], |d, _| {
                    let res = if compressed {
                        d.load_compressed(r, data)
                    } else {
                        d.load(r, data)
                    };
                    let damage = res.is_ok().then_some(r);
                    (res, damage)
                })?;
                match used {
                    Ok(n) => Ok(21 + n),
                    Err(md::EWRITEOUTSIDE) => Err(Error::new(EWRITEOUTSIDE)),
                    Err(md::ESHORT) => Err(Error::new(ESHORTDRAW)),
                    Err(e) => Err(Error::new(e)),
                }
            }
            b'o' => {
                let m = need(21)?;
                // Only windows have an origin to move; there are none yet.
                self.with(conn, u32le(a, 1), &[], |_, _| ((), None))?;
                Ok(m)
            }
            b'O' => {
                let m = need(2)?;
                conn.op = Op::from_byte(a[1]);
                Ok(m)
            }
            b'v' => {
                self.flush();
                Ok(1)
            }
            b'i' => {
                let m = need(10)?;
                let id = u32le(a, 1);
                if id == 0 && conn.screen {
                    return Err(Error::new(EDISPLAYFONT));
                }
                if !conn.images.contains_key(&id) {
                    return Err(Error::new(ENODRAWIMAGE));
                }
                let n = i32le(a, 5);
                if n <= 0 || n > 4096 {
                    return Err(Error::new(EFONTSIZE));
                }
                conn.fonts.insert(
                    id,
                    FontCache {
                        ascent: i32::from(a[9]),
                        chars: alloc::vec![md::CacheChar::default(); n as usize],
                    },
                );
                Ok(m)
            }
            b'l' => {
                let m = need(37)?;
                let id = u32le(a, 1);
                let n = conn.font(id)?.chars.len();
                let src = u32le(a, 5);
                let ci = usize::from(u16::from_le_bytes([a[9], a[10]]));
                if ci >= n {
                    return Err(Error::new(EINDEX));
                }
                let (r, p) = (rect(a, 11), point(a, 27));
                self.with(conn, id, &[src], |d, s| {
                    ((), md::draw(d, r, s[0], p, None, p, Op::SOVER_D))
                })?;
                if let Some(f) = conn.fonts.get_mut(&id) {
                    f.chars[ci] = md::CacheChar {
                        minx: r.min.x,
                        maxx: r.max.x,
                        miny: r.min.y as u8,
                        maxy: r.max.y as u8,
                        left: a[35] as i8,
                        width: a[36],
                    };
                }
                Ok(m)
            }
            b's' | b'x' => {
                let bg = a[0] == b'x';
                let head = if bg { 59 } else { 47 };
                need(head)?;
                let n = usize::from(u16::from_le_bytes([a[45], a[46]]));
                let m = need(head + 2 * n)?;
                let (dst, src, fid) = (u32le(a, 1), u32le(a, 5), u32le(a, 9));
                let (p, clipr, sp) = (point(a, 13), rect(a, 21), point(a, 37));
                let exists = |id: u32| (id == 0 && conn.screen) || conn.images.contains_key(&id);
                if !exists(dst) || !exists(src) {
                    return Err(Error::new(ENODRAWIMAGE));
                }
                let font = conn.font(fid)?;
                // The glyphs up to the first bad index. As in devdraw, `x`
                // checks them all before it paints the background, and `s`
                // draws those before the bad one and then fails.
                let glyphs: Vec<md::CacheChar> = (0..n)
                    .map(|i| {
                        usize::from(u16::from_le_bytes([a[head + 2 * i], a[head + 2 * i + 1]]))
                    })
                    .map_while(|ci| font.chars.get(ci).copied())
                    .collect();
                let bad = glyphs.len() < n;
                if bad && bg {
                    return Err(Error::new(EINDEX));
                }
                let ascent = font.ascent;
                let mut srcs = alloc::vec![src, fid];
                let bgp = if bg {
                    srcs.push(u32le(a, 47));
                    point(a, 51)
                } else {
                    Point::new(0, 0)
                };
                let op = take_op(conn);
                self.with(conn, dst, &srcs, |d, s| {
                    let saved = d.clipr;
                    d.clipr = clipr;
                    let mut damage: Option<Rect> = None;
                    let mut add = |r: Option<Rect>| {
                        if let Some(r) = r {
                            damage = Some(damage.map_or(r, |d| d.union(&r)));
                        }
                    };
                    if bg {
                        let w: i32 = glyphs.iter().map(|g| i32::from(g.width)).sum();
                        let y = p.y - ascent;
                        let r = Rect::new(p.x, y, p.x + w, y + s[1].r.dy());
                        add(md::draw(d, r, s[2], bgp, None, Point::new(0, 0), op));
                    }
                    let (mut q, mut sp) = (p, sp);
                    for g in &glyphs {
                        let (next, r) = md::draw_char(d, q, s[0], &mut sp, s[1], ascent, g, op);
                        add(r);
                        q = next;
                    }
                    d.clipr = saved;
                    ((), damage)
                })?;
                if bad {
                    return Err(Error::new(EINDEX));
                }
                Ok(m)
            }
            _ => Err(Error::new(EBADCMD)),
        }
    }
}

/// The operator for this drawing message; the next defaults again to
/// `SoverD`.
fn take_op(conn: &mut Conn) -> Op {
    core::mem::replace(&mut conn.op, Op::SOVER_D)
}

fn u32le(a: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([a[o], a[o + 1], a[o + 2], a[o + 3]])
}

fn i32le(a: &[u8], o: usize) -> i32 {
    u32le(a, o) as i32
}

fn point(a: &[u8], o: usize) -> Point {
    Point::new(i32le(a, o), i32le(a, o + 4))
}

fn rect(a: &[u8], o: usize) -> Rect {
    Rect {
        min: point(a, o),
        max: point(a, o + 8),
    }
}

/// Decodes one polygon coordinate at `a[*u]`: a byte holding a 7-bit
/// signed difference from `old`, or, with its top bit set, the low 7 bits
/// of a 23-bit signed value whose other 16 bits follow.
fn coord(a: &[u8], u: &mut usize, old: i32) -> Result<i32> {
    let b = *a.get(*u).ok_or(Error::new(ESHORTDRAW))?;
    *u += 1;
    let mut x = i32::from(b & 0x7F);
    if b & 0x80 != 0 {
        let more = a.get(*u..*u + 2).ok_or(Error::new(ESHORTDRAW))?;
        x |= i32::from(more[0]) << 7 | i32::from(more[1]) << 15;
        *u += 2;
        if x & (1 << 22) != 0 {
            x |= !0 << 23;
        }
        Ok(x)
    } else {
        if b & 0x40 != 0 {
            x |= !0 << 7;
        }
        Ok(old.wrapping_add(x))
    }
}

/// The `rgbv` colour map (color(6)): 256 entries of 4 levels of red,
/// green and blue, each with 4 levels of value.
fn colormap() -> String {
    let mut map = [(0u32, 0u32, 0u32); 256];
    for r in 0..4u32 {
        for g in 0..4u32 {
            for b in 0..4u32 {
                for v in 0..4u32 {
                    let den = r.max(g).max(b);
                    let (cr, cg, cb) = if den == 0 {
                        (v * 17, v * 17, v * 17)
                    } else {
                        let num = 17 * (4 * den + v);
                        (r * num / den, g * num / den, b * num / den)
                    };
                    let idx = r * 64 + v * 16 + ((g * 4 + b + v).wrapping_sub(r) & 15);
                    map[idx as usize] = (cr, cg, cb);
                }
            }
        }
    }
    let mut s = String::new();
    for (i, (r, g, b)) in map.iter().enumerate() {
        s.push_str(&format!("{i:>11} {r:>11} {g:>11} {b:>11}\n"));
    }
    s
}

impl Dev for DevDraw {
    fn dc(&self) -> char {
        'i'
    }

    fn name(&self) -> &'static str {
        "draw"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: DevDraw::qid(QTOP),
            aux: None,
        })
    }

    fn walk(&self, c: &KChan, name: &str) -> Result<Walked> {
        let path = c.qid.path;
        let next = match kind_of(path) {
            QTOP if name == "draw" => QDRAW,
            QDRAW if name == "new" => QNEW,
            QDRAW => {
                let id: u32 = name.parse().map_err(|_| Error::new(error::ENONEXIST))?;
                if name != format!("{id}") || !self.clients.lock().contains_key(&id) {
                    return Err(Error::new(error::ENONEXIST));
                }
                u64::from(id) << 8 | KDIR
            }
            KDIR => {
                let k = FILES
                    .iter()
                    .find(|f| f.0 == name)
                    .ok_or(Error::new(error::ENONEXIST))?
                    .1;
                path & !0xFF | k
            }
            QTOP => return Err(Error::new(error::ENONEXIST)),
            _ => return Err(Error::new(error::ENOTDIR)),
        };
        Ok(Walked {
            qid: DevDraw::qid(next),
            aux: None,
        })
    }

    fn stat(&self, c: &KChan) -> Result<Dir> {
        Ok(self.dir(c.qid.path))
    }

    fn open(&self, c: &mut KChan, omode: u8) -> Result<()> {
        let path = c.qid.path;
        match kind_of(path) {
            QTOP | QDRAW | KDIR => {
                if omode & mode::ACCESS != mode::OREAD {
                    return Err(Error::new(error::EISDIR));
                }
                Ok(())
            }
            QNEW => {
                let id = {
                    let mut n = self.next.lock();
                    let id = *n;
                    *n += 1;
                    id
                };
                let client = Arc::new(Client {
                    opens: Mutex::new(1),
                    conn: Mutex::new(Conn::new(self.display.input().resizes())),
                });
                self.clients.lock().insert(id, client);
                c.qid = DevDraw::qid(u64::from(id) << 8 | KCTL);
                Ok(())
            }
            _ => {
                let cl = self.client(conn_of(path))?;
                *cl.opens.lock() += 1;
                Ok(())
            }
        }
    }

    fn close(&self, c: &KChan) {
        let id = conn_of(c.qid.path);
        if id == 0 {
            return;
        }
        let Ok(cl) = self.client(id) else {
            return;
        };
        let last = {
            let mut n = cl.opens.lock();
            *n = n.saturating_sub(1);
            *n == 0
        };
        if last {
            self.clients.lock().remove(&id);
            self.display.input().rendez.wakeup();
        }
    }

    fn read(&self, c: &KChan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let path = c.qid.path;
        match kind_of(path) {
            QTOP | QDRAW | KDIR => Err(Error::new(error::EISDIR)),
            KCTL => {
                self.sync_size();
                let cl = self.client(conn_of(path))?;
                let mut conn = cl.conn.lock();
                if buf.len() < INFO {
                    return Err(Error::new(ESHORTREAD));
                }
                let id = conn.info.ok_or(Error::new(ENODRAWIMAGE))?;
                let s = self.info(conn_of(path), &conn, id)?;
                buf[..s.len()].copy_from_slice(s.as_bytes());
                // As devdraw: each read answers one request, so the next
                // needs another `ctl` write.
                conn.info = None;
                Ok(s.len())
            }
            KDATA => {
                let cl = self.client(conn_of(path))?;
                let mut conn = cl.conn.lock();
                let Some(data) = conn.readdata.as_ref() else {
                    return Err(Error::new(ENODATA));
                };
                if buf.len() < data.len() {
                    return Err(Error::new(ESHORTREAD));
                }
                let n = data.len();
                buf[..n].copy_from_slice(data);
                conn.readdata = None;
                Ok(n)
            }
            KCOLORMAP => Ok(read_str(colormap().as_bytes(), buf, offset)),
            KREFRESH => {
                if buf.len() < 20 {
                    return Err(Error::new(error::EBADARG));
                }
                let cl = self.client(conn_of(path))?;
                let id = conn_of(path);
                let input = self.display.input();
                // Only the screen changes size; there are no windows to
                // refresh. Wait for a resize, the connection's end or an
                // interrupt.
                let seen = cl.conn.lock().resizes;
                crate::block::park(&input.rendez, || {
                    input.resizes() != seen || !self.clients.lock().contains_key(&id)
                })?;
                if input.resizes() == seen {
                    return Ok(0);
                }
                self.sync_size();
                cl.conn.lock().resizes = input.resizes();
                let r = self.screen.lock().image.r;
                buf[..4].copy_from_slice(&0u32.to_le_bytes());
                for (i, v) in [r.min.x, r.min.y, r.max.x, r.max.y].into_iter().enumerate() {
                    buf[4 + 4 * i..8 + 4 * i].copy_from_slice(&v.to_le_bytes());
                }
                Ok(20)
            }
            _ => Err(Error::new(error::ENONEXIST)),
        }
    }

    fn dirread(&self, c: &KChan) -> Result<Vec<Dir>> {
        let path = c.qid.path;
        Ok(match kind_of(path) {
            QTOP => alloc::vec![self.dir(QDRAW)],
            QDRAW => {
                let mut v = alloc::vec![self.dir(QNEW)];
                for id in self.clients.lock().keys() {
                    v.push(self.dir(u64::from(*id) << 8 | KDIR));
                }
                v
            }
            KDIR => FILES.iter().map(|f| self.dir(path & !0xFF | f.1)).collect(),
            _ => return Err(Error::new(error::ENOTDIR)),
        })
    }

    fn write(&self, c: &KChan, buf: &[u8], _offset: u64) -> Result<usize> {
        let path = c.qid.path;
        match kind_of(path) {
            QTOP | QDRAW | KDIR => Err(Error::new(error::EISDIR)),
            KCTL => {
                if buf.len() != 4 {
                    return Err(Error::new(ECTL));
                }
                let cl = self.client(conn_of(path))?;
                let mut conn = cl.conn.lock();
                let id = u32le(buf, 0);
                let ok = if id == 0 {
                    conn.screen
                } else {
                    conn.images.contains_key(&id)
                };
                if !ok {
                    return Err(Error::new(ENODRAWIMAGE));
                }
                conn.info = Some(id);
                Ok(4)
            }
            KDATA => {
                self.sync_size();
                let cl = self.client(conn_of(path))?;
                let mut conn = cl.conn.lock();
                self.messages(&mut conn, buf)?;
                Ok(buf.len())
            }
            KCOLORMAP | KREFRESH | QNEW => Err(Error::new(error::EPERM)),
            _ => Err(Error::new(error::ENONEXIST)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use purg_platform::screen::Headless;

    fn conn() -> (DevDraw, Conn) {
        let d = DevDraw::new(Arc::new(Headless::new(16, 8)), String::from("me")).unwrap();
        (d, Conn::new(0))
    }

    fn alloc(id: u32, chan: Chan, repl: bool, r: Rect, colour: u32) -> Vec<u8> {
        let mut m = alloc::vec![b'b'];
        m.extend(id.to_le_bytes());
        m.extend(0u32.to_le_bytes());
        m.push(0);
        m.extend(chan.0.to_le_bytes());
        m.push(repl as u8);
        let clip = if repl { Rect::infinite() } else { r };
        for v in [
            r.min.x, r.min.y, r.max.x, r.max.y, clip.min.x, clip.min.y, clip.max.x, clip.max.y,
        ] {
            m.extend(v.to_le_bytes());
        }
        m.extend(colour.to_le_bytes());
        m
    }

    #[test]
    fn allocate_draw_and_read_back() {
        let (d, mut c) = conn();
        let mut m = alloc(1, md::RGB24, true, Rect::new(0, 0, 1, 1), 0xFF0000FF);
        m.extend(alloc(2, md::GREY1, true, Rect::new(0, 0, 1, 1), 0xFFFFFFFF));
        m.push(b'd');
        for v in [0u32, 1, 2] {
            m.extend(v.to_le_bytes());
        }
        for v in [2i32, 1, 5, 3, 0, 0, 0, 0] {
            m.extend(v.to_le_bytes());
        }
        m.push(b'r');
        m.extend(0u32.to_le_bytes());
        for v in [1i32, 1, 6, 2] {
            m.extend(v.to_le_bytes());
        }
        d.messages(&mut c, &m).unwrap();
        let px = c.readdata.take().unwrap();
        // x8r8g8b8, little-endian: white, then four red pixels.
        let mut want = alloc::vec![0xFF, 0xFF, 0xFF, 0];
        for _ in 0..3 {
            want.extend([0, 0, 0xFF, 0]);
        }
        want.extend([0xFF, 0xFF, 0xFF, 0]);
        assert_eq!(px, want);
        assert_eq!(d.screen.lock().damage, Some(Rect::new(0, 0, 16, 8)));
    }

    #[test]
    fn errors() {
        let (d, mut c) = conn();
        let e = |c: &mut Conn, m: &[u8]| d.messages(c, m).unwrap_err().as_str().into();
        let e = |c: &mut Conn, m: &[u8]| -> String { e(c, m) };
        assert_eq!(e(&mut c, b"b\x01"), ESHORTDRAW);
        assert_eq!(e(&mut c, b"Q"), EBADCMD);
        assert_eq!(e(&mut c, &[b'f', 9, 0, 0, 0]), ENODRAWIMAGE);
        let m = alloc(0, md::RGB24, false, Rect::new(0, 0, 1, 1), 0);
        assert_eq!(e(&mut c, &m), EIMAGEEXISTS);
        let m = alloc(3, Chan(0x99), false, Rect::new(0, 0, 1, 1), 0);
        assert_eq!(e(&mut c, &m), EDRAWMEM);
        let mut m = alloc::vec![b'r', 0, 0, 0, 0];
        for v in [0i32, 0, 17, 1] {
            m.extend(v.to_le_bytes());
        }
        assert_eq!(e(&mut c, &m), EREADOUTSIDE);
    }

    #[test]
    fn a_resize_makes_a_new_screen_image() {
        let screen = Arc::new(Headless::new(16, 8));
        let d = DevDraw::new(screen.clone(), String::from("me")).unwrap();
        let mut c = Conn::new(0);
        let mut m = alloc(1, md::RGB24, true, Rect::new(0, 0, 1, 1), 0xFF0000FF);
        m.extend(alloc(2, md::GREY1, true, Rect::new(0, 0, 1, 1), 0xFFFFFFFF));
        m.push(b'd');
        for v in [0u32, 1, 2] {
            m.extend(v.to_le_bytes());
        }
        for v in [0i32, 0, 16, 8, 0, 0, 0, 0] {
            m.extend(v.to_le_bytes());
        }
        d.messages(&mut c, &m).unwrap();
        screen.resize(20, 4);
        d.sync_size();
        let s = d.screen.lock();
        assert_eq!(s.image.r, Rect::new(0, 0, 20, 4));
        assert_eq!(s.damage, Some(Rect::new(0, 0, 20, 4)));
        // The old pixels that fit are kept; the new ones are white.
        assert_eq!(s.image.rgba(Point::new(15, 3)), [255, 0, 0, 255]);
        assert_eq!(s.image.rgba(Point::new(16, 0)), [255, 255, 255, 255]);
    }

    #[test]
    fn glyphs_from_the_cache() {
        let (d, mut c) = conn();
        // A 4x3 font image with one 2x2 glyph at (1, 0), ascent 2.
        let mut m = alloc(1, md::GREY8, false, Rect::new(0, 0, 4, 3), 0);
        m.extend(alloc(
            2,
            md::GREY1,
            false,
            Rect::new(0, 0, 8, 2),
            0xFFFFFFFF,
        ));
        m.extend(alloc(3, md::RGB24, true, Rect::new(0, 0, 1, 1), 0x0000FFFF));
        m.extend([b'i', 1, 0, 0, 0, 2, 0, 0, 0, 2]);
        m.extend([b'l', 1, 0, 0, 0, 2, 0, 0, 0, 1, 0]);
        for v in [1i32, 0, 3, 2, 0, 0] {
            m.extend(v.to_le_bytes());
        }
        m.extend([0xFF, 3]);
        // Two of glyph 1 from baseline (4, 6), then read the screen back.
        m.push(b's');
        for v in [0u32, 3, 1] {
            m.extend(v.to_le_bytes());
        }
        for v in [4i32, 6, 0, 0, 16, 8, 0, 0] {
            m.extend(v.to_le_bytes());
        }
        m.extend(2u16.to_le_bytes());
        m.extend([1, 0, 1, 0]);
        m.push(b'r');
        m.extend(0u32.to_le_bytes());
        for v in [0i32, 4, 16, 6] {
            m.extend(v.to_le_bytes());
        }
        d.messages(&mut c, &m).unwrap();
        let px = c.readdata.take().unwrap();
        let blue =
            |x: usize, y: usize| px[(y * 16 + x) * 4] == 0xFF && px[(y * 16 + x) * 4 + 2] == 0;
        // left -1: glyphs at x 3..5 and 6..8, rows 4 and 5.
        let got: Vec<usize> = (0..16).filter(|&x| blue(x, 0) && blue(x, 1)).collect();
        assert_eq!(got, [3, 4, 6, 7]);
    }

    /// devdraw's `s` draws the glyphs before a bad index, then fails;
    /// `x` checks every index before it paints anything.
    #[test]
    fn a_bad_glyph_index() {
        // The font of `glyphs_from_the_cache`, and a white background.
        let setup = |d: &DevDraw, c: &mut Conn| {
            let mut m = alloc(1, md::GREY8, false, Rect::new(0, 0, 4, 3), 0);
            m.extend(alloc(
                2,
                md::GREY1,
                false,
                Rect::new(0, 0, 8, 2),
                0xFFFFFFFF,
            ));
            m.extend(alloc(3, md::RGB24, true, Rect::new(0, 0, 1, 1), 0x0000FFFF));
            m.extend(alloc(4, md::RGB24, true, Rect::new(0, 0, 1, 1), 0xFF0000FF));
            m.extend([b'i', 1, 0, 0, 0, 2, 0, 0, 0, 2]);
            m.extend([b'l', 1, 0, 0, 0, 2, 0, 0, 0, 1, 0]);
            for v in [1i32, 0, 3, 2, 0, 0] {
                m.extend(v.to_le_bytes());
            }
            m.extend([0xFF, 3]);
            d.messages(c, &m).unwrap();
        };
        // Glyphs 1, 1 and 2 (out of range) from baseline (4, 6).
        let text = |bg: bool| {
            let mut m = alloc::vec![if bg { b'x' } else { b's' }];
            for v in [0u32, 3, 1] {
                m.extend(v.to_le_bytes());
            }
            for v in [4i32, 6, 0, 0, 16, 8, 0, 0] {
                m.extend(v.to_le_bytes());
            }
            m.extend(3u16.to_le_bytes());
            if bg {
                m.extend(4u32.to_le_bytes());
                m.extend([0u8; 8]);
            }
            m.extend([1, 0, 1, 0, 2, 0]);
            m
        };
        // The columns of rows 4 and 5 that are not white.
        let inked = |d: &DevDraw, c: &mut Conn| {
            let mut m = alloc::vec![b'r'];
            m.extend(0u32.to_le_bytes());
            for v in [0i32, 4, 16, 6] {
                m.extend(v.to_le_bytes());
            }
            d.messages(c, &m).unwrap();
            let px = c.readdata.take().unwrap();
            let white = |x: usize, y: usize| px[(y * 16 + x) * 4..][..3] == [0xFF; 3];
            (0..16)
                .filter(|&x| !white(x, 0) || !white(x, 1))
                .collect::<Vec<usize>>()
        };
        for bg in [false, true] {
            let (d, mut c) = conn();
            setup(&d, &mut c);
            let err = d.messages(&mut c, &text(bg)).unwrap_err();
            assert_eq!(err.as_str(), EINDEX);
            let want: &[usize] = if bg { &[] } else { &[3, 4, 6, 7] };
            assert_eq!(inked(&d, &mut c), want, "{}", if bg { 'x' } else { 's' });
        }
    }

    #[test]
    fn info_is_144_bytes() {
        let (d, c) = conn();
        let s = d.info(7, &c, 0).unwrap();
        assert_eq!(s.len(), 144);
        assert!(s.starts_with("          7           0    x8r8g8b8           0 "));
    }

    #[test]
    fn coordinates() {
        let mut u = 0;
        let a = [0x05, 0x7F, 0x80 | 0x01, 0x02, 0x00, 0xFF, 0xFF, 0xFF];
        assert_eq!(coord(&a, &mut u, 10), Ok(15));
        assert_eq!(coord(&a, &mut u, 10), Ok(9));
        assert_eq!(coord(&a, &mut u, 10), Ok(1 | 2 << 7));
        assert_eq!(coord(&a, &mut u, 0), Ok(-1));
        assert_eq!(u, 8);
    }

    #[test]
    fn rgbv() {
        let m = colormap();
        assert_eq!(m.lines().count(), 256);
        assert!(m.starts_with("          0           0           0           0\n"));
        assert!(m.ends_with("        255         255         255         255\n"));
    }
}
