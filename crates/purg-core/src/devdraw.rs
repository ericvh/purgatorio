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
//! | `b id[4] screenid[4] refresh[1] chan[4] repl[1] R[16] clipR[16] rgba[4]` | allocate an image filled with a colour; a window on screen `screenid` when that is not 0 |
//! | `f id[4]` | free an image |
//! | `A id[4] imageid[4] fillid[4] public[1]` | allocate screen `id` on image `imageid`, its background drawn from `fillid` |
//! | `F id[4]` | free a screen |
//! | `S id[4] chan[4]` | use public screen `id` |
//! | `t top[1] n[2] id[4n]` | move windows to the top (or, with `top` 0, the bottom) of their screen |
//! | `o id[4] rmin[8] screenrmin[8]` | give a window the logical origin `rmin` and put it at `screenrmin` on its screen |
//! | `N id[4] in[1] j[1] name[j]` | name an image (`in` 1) or take the name away (`in` 0) |
//! | `n id[4] j[1] name[j]` | make `id` refer to the image named `name` |
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
//! with the point by `width`. Errors are devdraw's strings.
//!
//! **Images, screens and windows.** Images belong to the device, not to
//! a connection: a connection's ids refer to them, and an image goes
//! when nothing refers to it (an id, a name's lookup, or a screen built
//! on it). Screen ids are the device's: a connection makes one with
//! `A`, and another may use it with `S` when it is public. A window is
//! an image with its own pixels (always a backing store, whatever
//! refresh method is asked for) that also shows on its screen's image,
//! stacked with the screen's other windows: after anything is drawn on
//! a window, the part drawn is copied to the screen's image and the
//! windows above it are copied over that, bottom to top, and so on up
//! to the display when the screen's image is itself a window. Freeing,
//! moving or restacking a window paints the area again from the
//! screen's fill and its windows. A window freed by the connection
//! that made it leaves its screen at once, even when other ids still
//! refer to its image. Drawing straight on an image that has a screen
//! draws over its windows, until they are painted again.
//!
//! When the display changes size ([`Screen::size`] and
//! [`purg_platform::screen::Input::resized`]) the device makes a new
//! screen image before its next request, keeping what fits of the old
//! one, and paints the screens on it again. A read of `refresh` blocks
//! until that happens and then returns a 20-byte record, as devdraw's
//! refresh records are: the image id (0, the screen) and its new
//! rectangle, four little-endian 4-byte integers. The client then reads
//! `ctl` for the screen and redraws.
//!
//! The screen image is the device's own; `v` gives the screen's damaged
//! rectangle to [`Screen::flush`]. A connection ends, and its ids and
//! screens go, when its last file is closed.

use alloc::collections::{BTreeMap, BTreeSet};
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
const ESCREENEXISTS: &str = "screen id in use";
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
const ENAMEUSED: &str = "image name in use";
const ENONAME: &str = "no image with that name";
const EWRONGNAME: &str = "wrong name for image";
const ECHAN: &str = "inconsistent chan";
const ENOTWINDOW: &str = "images are not windows";
const ENOTSAME: &str = "images are not on the same screen";
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

/// The key of the display's image in [`State::images`].
const DISPLAY: u64 = 0;

fn conn_of(path: u64) -> u32 {
    (path >> 8) as u32
}

fn kind_of(path: u64) -> u64 {
    if path >> 8 == 0 { path } else { path & 0xFF }
}

/// An image of the device.
struct DImage {
    image: Image,
    /// Connection ids and screens that refer to it.
    refs: u32,
    /// The font cache on it (`i`).
    font: Option<FontCache>,
    /// Where it shows, when it is a window.
    win: Option<Win>,
}

/// A window's place: its screen, the connection that made it, and where
/// its rectangle's top left corner is on the screen's image.
#[derive(Clone, Copy)]
struct Win {
    screen: u32,
    owner: u32,
    origin: Point,
}

/// A screen: an image, the fill for its background, and its windows.
struct DScreen {
    image: u64,
    fill: u64,
    /// Bottom first.
    wins: Vec<u64>,
    public: bool,
    owner: u32,
    /// Connections that hold it (`A`, `S`).
    users: u32,
}

/// One connection's state.
struct Conn {
    /// Its image ids and the images they name.
    ids: BTreeMap<u32, u64>,
    /// The screens it may put windows on.
    screens: BTreeSet<u32>,
    /// The image the next read of `ctl` reports on: the screen at first,
    /// then whatever `ctl` was last told; a read clears it.
    info: Option<u32>,
    /// The operator for the next drawing message.
    op: Op,
    /// Pixels waiting for a read of `data`, after `r`.
    readdata: Option<Vec<u8>>,
    /// The screen resizes `refresh` has reported.
    resizes: u64,
    /// Open files.
    opens: u32,
}

/// A font cache on an image: its ascent and its glyphs.
struct FontCache {
    ascent: i32,
    chars: Vec<md::CacheChar>,
}

impl Conn {
    fn new(resizes: u64) -> Conn {
        let mut ids = BTreeMap::new();
        ids.insert(0, DISPLAY);
        Conn {
            ids,
            screens: BTreeSet::new(),
            info: Some(0),
            op: Op::SOVER_D,
            readdata: None,
            resizes,
            opens: 1,
        }
    }

    fn key(&self, id: u32) -> Result<u64> {
        self.ids.get(&id).copied().ok_or(Error::new(ENODRAWIMAGE))
    }
}

/// Everything the device holds, under one lock (devdraw's `drawlock`).
struct State {
    images: BTreeMap<u64, DImage>,
    next_key: u64,
    names: BTreeMap<String, u64>,
    screens: BTreeMap<u32, DScreen>,
    conns: BTreeMap<u32, Conn>,
    next_conn: u32,
    /// What of the display has changed since the last `v`.
    damage: Option<Rect>,
    /// The display's resize count the display image was made for.
    resizes: u64,
    /// A `v` came.
    flush_wanted: bool,
}

pub struct DevDraw {
    display: Arc<dyn Screen>,
    user: String,
    state: Mutex<State>,
}

/// `r` less nothing: the union of two optional rectangles.
fn join(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.union(&b)),
        (a, None) => a,
        (None, b) => b,
    }
}

impl State {
    fn new(display: Image, resizes: u64) -> State {
        let r = display.r;
        let mut images = BTreeMap::new();
        images.insert(
            DISPLAY,
            DImage {
                image: display,
                refs: 1,
                font: None,
                win: None,
            },
        );
        State {
            images,
            next_key: DISPLAY + 1,
            names: BTreeMap::new(),
            screens: BTreeMap::new(),
            conns: BTreeMap::new(),
            next_conn: 1,
            damage: Some(r),
            resizes,
            flush_wanted: false,
        }
    }

    fn add_image(&mut self, image: Image) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        self.images.insert(
            key,
            DImage {
                image,
                refs: 0,
                font: None,
                win: None,
            },
        );
        key
    }

    fn image(&self, key: u64) -> &Image {
        &self.images[&key].image
    }

    /// Makes `id` of `conn` refer to `key`.
    fn install(&mut self, conn: &mut Conn, id: u32, key: u64) -> Result<()> {
        if conn.ids.contains_key(&id) {
            return Err(Error::new(EIMAGEEXISTS));
        }
        conn.ids.insert(id, key);
        if let Some(i) = self.images.get_mut(&key) {
            i.refs += 1;
        }
        Ok(())
    }

    /// Drops `id` of connection `cid`. A window made by that connection
    /// leaves its screen.
    fn uninstall(&mut self, conn: &mut Conn, cid: u32, id: u32) -> Result<()> {
        let key = conn.ids.remove(&id).ok_or(Error::new(ENODRAWIMAGE))?;
        if self.images[&key].win.is_some_and(|w| w.owner == cid) {
            self.unwindow(key);
        }
        self.deref(key);
        Ok(())
    }

    /// One less reference to `key`; the image goes with the last.
    fn deref(&mut self, key: u64) {
        if key == DISPLAY {
            return;
        }
        let Some(i) = self.images.get_mut(&key) else {
            return;
        };
        i.refs = i.refs.saturating_sub(1);
        if i.refs > 0 {
            return;
        }
        if i.win.is_some() {
            self.unwindow(key);
        }
        self.images.remove(&key);
        self.names.retain(|_, k| *k != key);
    }

    /// Where window `key` is on its screen's image.
    fn screen_rect(&self, key: u64) -> Option<Rect> {
        let i = &self.images[&key];
        let w = i.win?;
        Some(i.image.r.sub(i.image.r.min).add(w.origin))
    }

    /// Takes window `key` off its screen; it stays an image.
    fn unwindow(&mut self, key: u64) {
        let Some(sr) = self.screen_rect(key) else {
            return;
        };
        let Some(w) = self.images.get_mut(&key).and_then(|i| i.win.take()) else {
            return;
        };
        if let Some(s) = self.screens.get_mut(&w.screen) {
            s.wins.retain(|&k| k != key);
            self.repaint(w.screen, sr, 0, true);
            self.gc_screen(w.screen);
        }
    }

    /// Removes screen `sid` once no connection holds it and no window is
    /// on it.
    fn gc_screen(&mut self, sid: u32) {
        let Some(s) = self.screens.get(&sid) else {
            return;
        };
        if s.users > 0 || !s.wins.is_empty() {
            return;
        }
        let (image, fill) = (s.image, s.fill);
        self.screens.remove(&sid);
        self.deref(image);
        self.deref(fill);
    }

    /// Runs `f` on image `dst` with the images `srcs` (a source that is
    /// the destination is a copy of it). Records no damage.
    fn with_keys<R>(
        &mut self,
        dst: u64,
        srcs: &[u64],
        f: impl FnOnce(&mut Image, &[&Image]) -> R,
    ) -> R {
        let mut d = self.images.remove(&dst).expect("image");
        let copies: Vec<Option<Image>> = srcs
            .iter()
            .map(|&s| (s == dst).then(|| d.image.clone()))
            .collect();
        let refs: Vec<&Image> = srcs
            .iter()
            .zip(&copies)
            .map(|(s, c)| c.as_ref().unwrap_or_else(|| &self.images[s].image))
            .collect();
        let out = f(&mut d.image, &refs);
        self.images.insert(dst, d);
        out
    }

    /// Copies `r` of image `src`, from `sp`, onto image `dst`, ignoring
    /// both images' clips: a window's pixels onto its screen.
    fn composite(&mut self, dst: u64, r: Rect, src: u64, sp: Point) {
        let saved = {
            let s = &mut self.images.get_mut(&src).expect("image").image;
            let saved = (s.clipr, s.repl);
            s.clipr = s.r;
            s.repl = false;
            saved
        };
        self.with_keys(dst, &[src], |d, s| {
            let clip = d.clipr;
            d.clipr = d.r;
            md::draw(d, r, s[0], sp, None, sp, Op::S);
            d.clipr = clip;
        });
        let s = &mut self.images.get_mut(&src).expect("image").image;
        (s.clipr, s.repl) = saved;
    }

    /// Paints `r` of screen `sid`'s image from its windows, starting with
    /// the `from`th from the bottom, and the fill first when `fill`. Then
    /// passes the damage on.
    fn repaint(&mut self, sid: u32, r: Rect, from: usize, fill: bool) {
        let Some(s) = self.screens.get(&sid) else {
            return;
        };
        let (image, fillkey) = (s.image, s.fill);
        let wins: Vec<u64> = s.wins.iter().skip(from).copied().collect();
        let Some(r) = r.clip(&self.image(image).r) else {
            return;
        };
        if fill {
            self.with_keys(image, &[fillkey], |d, s| {
                let clip = d.clipr;
                d.clipr = d.r;
                md::draw(d, r, s[0], r.min, None, r.min, Op::S);
                d.clipr = clip;
            });
        }
        for w in wins {
            let Some(wr) = self.screen_rect(w) else {
                continue;
            };
            if let Some(part) = wr.clip(&r) {
                let min = self.image(w).r.min;
                let sp = part.min - wr.min + min;
                self.composite(image, part, w, sp);
            }
        }
        self.damaged(image, r);
    }

    /// Image `key` changed in `r` (its own coordinates): shows the change
    /// on its screen when it is a window, or on the display.
    fn damaged(&mut self, key: u64, r: Rect) {
        if key == DISPLAY {
            self.damage = join(self.damage, Some(r));
            return;
        }
        let Some(w) = self.images[&key].win else {
            return;
        };
        let Some(sr) = self.screen_rect(key) else {
            return;
        };
        let min = self.image(key).r.min;
        let Some(part) = r.sub(min).add(w.origin).clip(&sr) else {
            return;
        };
        let Some(from) = self
            .screens
            .get(&w.screen)
            .and_then(|s| s.wins.iter().position(|&k| k == key))
        else {
            return;
        };
        self.repaint(w.screen, part, from, false);
    }

    /// Runs `f` on image `dst` of `conn` with the images `srcs`, and shows
    /// the damage `f` returns.
    fn with<R>(
        &mut self,
        conn: &Conn,
        dst: u32,
        srcs: &[u32],
        f: impl FnOnce(&mut Image, &[&Image]) -> (R, Option<Rect>),
    ) -> Result<R> {
        let dkey = conn.key(dst)?;
        let skeys = srcs
            .iter()
            .map(|&s| conn.key(s))
            .collect::<Result<Vec<u64>>>()?;
        let (out, damage) = self.with_keys(dkey, &skeys, f);
        if let Some(d) = damage {
            self.damaged(dkey, d);
        }
        Ok(out)
    }

    /// The font cache on image `id` of `conn`.
    fn font(&self, conn: &Conn, id: u32) -> Result<&FontCache> {
        let key = conn.key(id)?;
        self.images[&key].font.as_ref().ok_or(Error::new(ENOTFONT))
    }

    /// The 144-byte description of image `id`, for `new` and `ctl`.
    fn info(&self, conn_id: u32, conn: &Conn, id: u32) -> Result<String> {
        let i = self.image(conn.key(id)?);
        let f = |v: i32| format!("{v:>11} ");
        let chan = format!("{}", i.chan());
        let mut s = format!("{conn_id:>11} {id:>11} {chan:>11} {:>11} ", i.repl as u32);
        for v in [i.r.min.x, i.r.min.y, i.r.max.x, i.r.max.y] {
            s.push_str(&f(v));
        }
        for v in [i.clipr.min.x, i.clipr.min.y, i.clipr.max.x, i.clipr.max.y] {
            s.push_str(&f(v));
        }
        Ok(s)
    }

    /// Ends connection `cid`: its ids and screens go.
    fn close(&mut self, cid: u32) {
        let Some(mut conn) = self.conns.remove(&cid) else {
            return;
        };
        let ids: Vec<u32> = conn.ids.keys().copied().collect();
        for id in ids {
            let _ = self.uninstall(&mut conn, cid, id);
        }
        for sid in core::mem::take(&mut conn.screens) {
            if let Some(s) = self.screens.get_mut(&sid) {
                s.users = s.users.saturating_sub(1);
            }
            self.gc_screen(sid);
        }
    }

    /// The display has a new image: paints the screens on it again.
    fn display_resized(&mut self, image: Image) {
        let r = image.r;
        let old = core::mem::replace(
            &mut self.images.get_mut(&DISPLAY).expect("display").image,
            image,
        );
        let new = &mut self.images.get_mut(&DISPLAY).expect("display").image;
        md::draw(new, old.r, &old, old.r.min, None, old.r.min, Op::S);
        self.damage = Some(r);
        let sids: Vec<u32> = self
            .screens
            .iter()
            .filter(|(_, s)| s.image == DISPLAY)
            .map(|(&k, _)| k)
            .collect();
        for sid in sids {
            self.repaint(sid, r, 0, true);
        }
    }

    /// Applies the messages in `a`, in order; stops at the first that
    /// fails.
    fn messages(&mut self, conn: &mut Conn, cid: u32, mut a: &[u8]) -> Result<()> {
        while !a.is_empty() {
            let n = self.message(conn, cid, a)?;
            a = &a[n..];
        }
        Ok(())
    }

    /// Applies the message at the start of `a`; returns its length.
    fn message(&mut self, conn: &mut Conn, cid: u32, a: &[u8]) -> Result<usize> {
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
                if conn.ids.contains_key(&id) {
                    return Err(Error::new(EIMAGEEXISTS));
                }
                let sid = u32le(a, 5);
                let chan = Chan(u32le(a, 10));
                if sid != 0 {
                    if !conn.screens.contains(&sid) {
                        return Err(Error::new(ENODRAWSCREEN));
                    }
                    let s = self.screens.get(&sid).ok_or(Error::new(ENODRAWSCREEN))?;
                    if self.image(s.image).chan() != chan {
                        return Err(Error::new(ECHAN));
                    }
                }
                let r = rect(a, 15);
                let mut i = Image::new(r, chan).ok_or(Error::new(EDRAWMEM))?;
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
                let key = self.add_image(i);
                self.install(conn, id, key)?;
                if sid != 0 {
                    self.images.get_mut(&key).expect("image").win = Some(Win {
                        screen: sid,
                        owner: cid,
                        origin: r.min,
                    });
                    let s = self.screens.get_mut(&sid).expect("screen");
                    s.wins.push(key);
                    let top = s.wins.len() - 1;
                    self.repaint(sid, r, top, false);
                }
                Ok(m)
            }
            b'f' => {
                let m = need(5)?;
                self.uninstall(conn, cid, u32le(a, 1))?;
                Ok(m)
            }
            b'A' => {
                let m = need(14)?;
                let sid = u32le(a, 1);
                if sid == 0 || self.screens.contains_key(&sid) {
                    return Err(Error::new(ESCREENEXISTS));
                }
                let (image, fill) = (conn.key(u32le(a, 5))?, conn.key(u32le(a, 9))?);
                for k in [image, fill] {
                    if let Some(i) = self.images.get_mut(&k) {
                        i.refs += 1;
                    }
                }
                self.screens.insert(
                    sid,
                    DScreen {
                        image,
                        fill,
                        wins: Vec::new(),
                        public: a[13] != 0,
                        owner: cid,
                        users: 1,
                    },
                );
                conn.screens.insert(sid);
                Ok(m)
            }
            b'F' => {
                let m = need(5)?;
                let sid = u32le(a, 1);
                if !conn.screens.remove(&sid) {
                    return Err(Error::new(ENODRAWSCREEN));
                }
                if let Some(s) = self.screens.get_mut(&sid) {
                    s.users = s.users.saturating_sub(1);
                }
                self.gc_screen(sid);
                Ok(m)
            }
            b'S' => {
                let m = need(9)?;
                let sid = u32le(a, 1);
                let s = self
                    .screens
                    .get(&sid)
                    .filter(|s| s.public || s.owner == cid)
                    .ok_or(Error::new(ENODRAWSCREEN))?;
                if self.image(s.image).chan() != Chan(u32le(a, 5)) {
                    return Err(Error::new(ECHAN));
                }
                if conn.screens.insert(sid) {
                    self.screens.get_mut(&sid).expect("screen").users += 1;
                }
                Ok(m)
            }
            b't' => {
                need(4)?;
                let n = usize::from(u16::from_le_bytes([a[2], a[3]]));
                let m = need(4 + 4 * n)?;
                let keys = (0..n)
                    .map(|i| conn.key(u32le(a, 4 + 4 * i)))
                    .collect::<Result<Vec<u64>>>()?;
                let Some(first) = keys.first() else {
                    return Ok(m);
                };
                let wins = keys
                    .iter()
                    .map(|k| self.images[k].win.ok_or(Error::new(ENOTWINDOW)))
                    .collect::<Result<Vec<Win>>>()?;
                let sid = wins[0].screen;
                if wins.iter().any(|w| w.screen != sid) {
                    return Err(Error::new(ENOTSAME));
                }
                let mut area = self.screen_rect(*first);
                for k in &keys {
                    area = join(area, self.screen_rect(*k));
                }
                if let Some(s) = self.screens.get_mut(&sid) {
                    // The first listed ends at the top (or the bottom).
                    for k in keys.iter().rev() {
                        s.wins.retain(|w| w != k);
                        if a[1] != 0 {
                            s.wins.push(*k);
                        } else {
                            s.wins.insert(0, *k);
                        }
                    }
                }
                if let Some(r) = area {
                    self.repaint(sid, r, 0, true);
                }
                Ok(m)
            }
            b'o' => {
                let m = need(21)?;
                let key = conn.key(u32le(a, 1))?;
                let (log, scr) = (point(a, 5), point(a, 13));
                let Some(w) = self.images[&key].win else {
                    return Ok(m);
                };
                let old = self.screen_rect(key);
                let i = &mut self.images.get_mut(&key).expect("image").image;
                if log != i.r.min {
                    let delta = log - i.r.min;
                    let mut moved =
                        Image::new(i.r.add(delta), i.chan()).ok_or(Error::new(EDRAWMEM))?;
                    let (clipr, repl) = (i.clipr, i.repl);
                    i.clipr = i.r;
                    i.repl = false;
                    let r = moved.r;
                    md::draw(&mut moved, r, i, i.r.min, None, i.r.min, Op::S);
                    moved.clipr = clipr.add(delta);
                    moved.repl = repl;
                    *i = moved;
                }
                self.images.get_mut(&key).expect("image").win = Some(Win { origin: scr, ..w });
                if let Some(r) = join(old, self.screen_rect(key)) {
                    self.repaint(w.screen, r, 0, true);
                }
                Ok(m)
            }
            b'N' => {
                need(7)?;
                let j = usize::from(a[6]);
                let m = need(7 + j)?;
                let key = conn.key(u32le(a, 1))?;
                let name = String::from_utf8_lossy(&a[7..7 + j]).into_owned();
                if a[5] != 0 {
                    if self.names.contains_key(&name) {
                        return Err(Error::new(ENAMEUSED));
                    }
                    self.names.insert(name, key);
                } else {
                    match self.names.get(&name) {
                        None => return Err(Error::new(ENONAME)),
                        Some(&k) if k != key => return Err(Error::new(EWRONGNAME)),
                        Some(_) => {
                            self.names.remove(&name);
                        }
                    }
                }
                Ok(m)
            }
            b'n' => {
                need(6)?;
                let j = usize::from(a[5]);
                let m = need(6 + j)?;
                let id = u32le(a, 1);
                let name = String::from_utf8_lossy(&a[6..6 + j]);
                let key = *self.names.get(&*name).ok_or(Error::new(ENONAME))?;
                self.install(conn, id, key)?;
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
            b'O' => {
                let m = need(2)?;
                conn.op = Op::from_byte(a[1]);
                Ok(m)
            }
            b'v' => {
                self.flush_wanted = true;
                Ok(1)
            }
            b'i' => {
                let m = need(10)?;
                let key = conn.key(u32le(a, 1))?;
                if key == DISPLAY {
                    return Err(Error::new(EDISPLAYFONT));
                }
                let n = i32le(a, 5);
                if n <= 0 || n > 4096 {
                    return Err(Error::new(EFONTSIZE));
                }
                self.images.get_mut(&key).expect("image").font = Some(FontCache {
                    ascent: i32::from(a[9]),
                    chars: alloc::vec![md::CacheChar::default(); n as usize],
                });
                Ok(m)
            }
            b'l' => {
                let m = need(37)?;
                let id = u32le(a, 1);
                let n = self.font(conn, id)?.chars.len();
                let src = u32le(a, 5);
                let ci = usize::from(u16::from_le_bytes([a[9], a[10]]));
                if ci >= n {
                    return Err(Error::new(EINDEX));
                }
                let (r, p) = (rect(a, 11), point(a, 27));
                self.with(conn, id, &[src], |d, s| {
                    ((), md::draw(d, r, s[0], p, None, p, Op::SOVER_D))
                })?;
                let key = conn.key(id)?;
                if let Some(f) = self.images.get_mut(&key).and_then(|i| i.font.as_mut()) {
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
                conn.key(dst)?;
                conn.key(src)?;
                let font = self.font(conn, fid)?;
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
                    let mut add = |r: Option<Rect>| damage = join(damage, r);
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

impl DevDraw {
    /// A draw device showing its screen on `display`. Fails if the
    /// display's size or channels make no image memdraw can draw on.
    pub fn new(display: Arc<dyn Screen>, user: String) -> Option<DevDraw> {
        let resizes = display.input().resizes();
        let image = DevDraw::screen_image(&*display)?;
        Some(DevDraw {
            display,
            user,
            state: Mutex::new(State::new(image, resizes)),
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
        let mut s = self.state.lock();
        if s.resizes == resizes {
            return;
        }
        s.resizes = resizes;
        let Some(image) = DevDraw::screen_image(&*self.display) else {
            return;
        };
        if image.r != s.image(DISPLAY).r {
            s.display_resized(image);
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

    /// Sends the screen's damage to the display.
    fn flush(&self, s: &mut State) {
        let damage = s.damage.take();
        let image = s.image(DISPLAY);
        let Some(r) = damage.and_then(|d| d.clip(&image.r)) else {
            return;
        };
        if let Ok(pixels) = image.unload(r) {
            let stride = md::bytes_per_line(r, image.depth());
            self.display
                .flush([r.min.x, r.min.y, r.max.x, r.max.y], &pixels, stride);
        }
    }

    /// Runs `f` on connection `cid`'s state, with the device's.
    fn conn<R>(&self, cid: u32, f: impl FnOnce(&mut State, &mut Conn) -> Result<R>) -> Result<R> {
        let mut s = self.state.lock();
        let mut conn = s.conns.remove(&cid).ok_or(Error::new(error::ENONEXIST))?;
        let out = f(&mut s, &mut conn);
        s.conns.insert(cid, conn);
        out
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
                if name != format!("{id}") || !self.state.lock().conns.contains_key(&id) {
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
                let resizes = self.display.input().resizes();
                let mut s = self.state.lock();
                let id = s.next_conn;
                s.next_conn += 1;
                s.conns.insert(id, Conn::new(resizes));
                c.qid = DevDraw::qid(u64::from(id) << 8 | KCTL);
                Ok(())
            }
            _ => self.conn(conn_of(path), |_, conn| {
                conn.opens += 1;
                Ok(())
            }),
        }
    }

    fn close(&self, c: &KChan) {
        let id = conn_of(c.qid.path);
        if id == 0 {
            return;
        }
        let mut s = self.state.lock();
        let Some(conn) = s.conns.get_mut(&id) else {
            return;
        };
        conn.opens = conn.opens.saturating_sub(1);
        if conn.opens == 0 {
            s.close(id);
            // What it showed in windows has gone: show that.
            self.flush(&mut s);
            drop(s);
            self.display.input().rendez.wakeup();
        }
    }

    fn read(&self, c: &KChan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let path = c.qid.path;
        match kind_of(path) {
            QTOP | QDRAW | KDIR => Err(Error::new(error::EISDIR)),
            KCTL => {
                self.sync_size();
                let cid = conn_of(path);
                self.conn(cid, |s, conn| {
                    if buf.len() < INFO {
                        return Err(Error::new(ESHORTREAD));
                    }
                    let id = conn.info.ok_or(Error::new(ENODRAWIMAGE))?;
                    let info = s.info(cid, conn, id)?;
                    buf[..info.len()].copy_from_slice(info.as_bytes());
                    // As devdraw: each read answers one request, so the
                    // next needs another `ctl` write.
                    conn.info = None;
                    Ok(info.len())
                })
            }
            KDATA => self.conn(conn_of(path), |_, conn| {
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
            }),
            KCOLORMAP => Ok(read_str(colormap().as_bytes(), buf, offset)),
            KREFRESH => {
                if buf.len() < 20 {
                    return Err(Error::new(error::EBADARG));
                }
                let id = conn_of(path);
                let input = self.display.input();
                // Only the screen changes size: windows keep their
                // pixels, so none needs refreshing. Wait for a resize,
                // the connection's end or an interrupt.
                let seen = self.conn(id, |_, conn| Ok(conn.resizes))?;
                crate::block::park(&input.rendez, || {
                    input.resizes() != seen || !self.state.lock().conns.contains_key(&id)
                })?;
                if input.resizes() == seen {
                    return Ok(0);
                }
                self.sync_size();
                let resizes = input.resizes();
                self.conn(id, |_, conn| {
                    conn.resizes = resizes;
                    Ok(())
                })?;
                let r = self.state.lock().image(DISPLAY).r;
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
                let ids: Vec<u32> = self.state.lock().conns.keys().copied().collect();
                for id in ids {
                    v.push(self.dir(u64::from(id) << 8 | KDIR));
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
                self.conn(conn_of(path), |_, conn| {
                    let id = u32le(buf, 0);
                    conn.key(id)?;
                    conn.info = Some(id);
                    Ok(4)
                })
            }
            KDATA => {
                self.sync_size();
                let cid = conn_of(path);
                let mut s = self.state.lock();
                let mut conn = s.conns.remove(&cid).ok_or(Error::new(error::ENONEXIST))?;
                let r = s.messages(&mut conn, cid, buf);
                s.conns.insert(cid, conn);
                if core::mem::take(&mut s.flush_wanted) {
                    self.flush(&mut s);
                }
                r.map(|()| buf.len())
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

    impl DevDraw {
        /// Messages from connection 1.
        fn messages(&self, c: &mut Conn, m: &[u8]) -> Result<()> {
            self.messages_from(1, c, m)
        }

        fn messages_from(&self, cid: u32, c: &mut Conn, m: &[u8]) -> Result<()> {
            self.state.lock().messages(c, cid, m)
        }

        /// The display's pixel at `(x, y)`, as `0xRRGGBB`.
        fn px(&self, x: i32, y: i32) -> u32 {
            let [r, g, b, _] = self.state.lock().image(DISPLAY).rgba(Point::new(x, y));
            u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)
        }
    }

    fn alloc(id: u32, chan: Chan, repl: bool, r: Rect, colour: u32) -> Vec<u8> {
        alloc_on(id, 0, chan, repl, r, colour)
    }

    fn alloc_on(id: u32, sid: u32, chan: Chan, repl: bool, r: Rect, colour: u32) -> Vec<u8> {
        let mut m = alloc::vec![b'b'];
        m.extend(id.to_le_bytes());
        m.extend(sid.to_le_bytes());
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
        assert_eq!(d.state.lock().damage, Some(Rect::new(0, 0, 16, 8)));
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
        let s = d.state.lock();
        assert_eq!(s.image(DISPLAY).r, Rect::new(0, 0, 20, 4));
        assert_eq!(s.damage, Some(Rect::new(0, 0, 20, 4)));
        // The old pixels that fit are kept; the new ones are white.
        assert_eq!(s.image(DISPLAY).rgba(Point::new(15, 3)), [255, 0, 0, 255]);
        assert_eq!(
            s.image(DISPLAY).rgba(Point::new(16, 0)),
            [255, 255, 255, 255]
        );
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
        let s = d.state.lock().info(7, &c, 0).unwrap();
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

    const BLUE: u32 = 0x0000FFFF;
    const RED: u32 = 0xFF0000FF;
    const GREEN: u32 = 0x00FF00FF;
    const YELLOW: u32 = 0xFFFF00FF;

    fn screen_msg(sid: u32, image: u32, fill: u32, public: bool) -> Vec<u8> {
        let mut m = alloc::vec![b'A'];
        for v in [sid, image, fill] {
            m.extend(v.to_le_bytes());
        }
        m.push(public as u8);
        m
    }

    fn window(id: u32, sid: u32, r: Rect, colour: u32) -> Vec<u8> {
        alloc_on(id, sid, md::XRGB32, false, r, colour)
    }

    /// Fills `r` of `dst` from the replicated `src` through the opaque
    /// mask 99.
    fn fill(dst: u32, src: u32, r: Rect) -> Vec<u8> {
        let mut m = alloc::vec![b'd'];
        for v in [dst, src, 99] {
            m.extend(v.to_le_bytes());
        }
        for v in [r.min.x, r.min.y, r.max.x, r.max.y, 0, 0, 0, 0] {
            m.extend(v.to_le_bytes());
        }
        m
    }

    fn top(top: bool, ids: &[u32]) -> Vec<u8> {
        let mut m = alloc::vec![b't', top as u8];
        m.extend((ids.len() as u16).to_le_bytes());
        for id in ids {
            m.extend(id.to_le_bytes());
        }
        m
    }

    fn name(id: u32, add: bool, name: &str) -> Vec<u8> {
        let mut m = alloc::vec![b'N'];
        m.extend(id.to_le_bytes());
        m.push(add as u8);
        m.push(name.len() as u8);
        m.extend(name.as_bytes());
        m
    }

    fn lookup(id: u32, name: &str) -> Vec<u8> {
        let mut m = alloc::vec![b'n'];
        m.extend(id.to_le_bytes());
        m.push(name.len() as u8);
        m.extend(name.as_bytes());
        m
    }

    /// Windows show on their screen in their stacking order, keep their
    /// pixels when covered, and the screen's fill shows where none is.
    #[test]
    fn windows_stack_and_move() {
        let (d, mut c) = conn();
        let mut m = alloc(99, md::GREY1, true, Rect::new(0, 0, 1, 1), 0xFFFFFFFF);
        m.extend(alloc(1, md::RGB24, true, Rect::new(0, 0, 1, 1), BLUE));
        m.extend(alloc(2, md::RGB24, true, Rect::new(0, 0, 1, 1), YELLOW));
        m.extend(screen_msg(9, 0, 1, false));
        m.extend(window(10, 9, Rect::new(0, 0, 8, 8), RED));
        m.extend(window(11, 9, Rect::new(4, 0, 12, 8), GREEN));
        d.messages(&mut c, &m).unwrap();
        assert_eq!(
            [d.px(2, 1), d.px(6, 1), d.px(14, 1)],
            [0xFF0000, 0xFF00, 0xFFFFFF]
        );
        // Drawing on the lower window shows only where it is not covered.
        d.messages(&mut c, &fill(10, 2, Rect::new(0, 0, 8, 8)))
            .unwrap();
        assert_eq!([d.px(2, 1), d.px(6, 1)], [0xFFFF00, 0xFF00]);
        // Raised, it shows the pixels it kept.
        d.messages(&mut c, &top(true, &[10])).unwrap();
        assert_eq!([d.px(6, 1), d.px(10, 1)], [0xFFFF00, 0xFF00]);
        d.messages(&mut c, &top(false, &[10])).unwrap();
        assert_eq!(d.px(6, 1), 0xFF00);
        // Freed, the other window and the fill show again.
        d.messages(&mut c, &[b'f', 11, 0, 0, 0]).unwrap();
        assert_eq!([d.px(6, 1), d.px(10, 1)], [0xFFFF00, 0xFF]);
        // Moved on the screen without moving its coordinates.
        let mut m = alloc::vec![b'o'];
        m.extend(10u32.to_le_bytes());
        for v in [0i32, 0, 8, 0] {
            m.extend(v.to_le_bytes());
        }
        d.messages(&mut c, &m).unwrap();
        assert_eq!([d.px(2, 1), d.px(10, 1)], [0xFF, 0xFFFF00]);
        let mut m = fill(10, 1, Rect::new(0, 0, 1, 1));
        m.push(b'r');
        m.extend(10u32.to_le_bytes());
        for v in [0i32, 0, 2, 1] {
            m.extend(v.to_le_bytes());
        }
        d.messages(&mut c, &m).unwrap();
        assert_eq!(
            c.readdata.take().unwrap(),
            [0xFF, 0, 0, 0, 0, 0xFF, 0xFF, 0]
        );
        assert_eq!([d.px(8, 0), d.px(9, 0)], [0xFF, 0xFFFF00]);
        // The screens' errors.
        let e =
            |c: &mut Conn, m: &[u8]| -> String { d.messages(c, m).unwrap_err().as_str().into() };
        assert_eq!(e(&mut c, &screen_msg(9, 0, 1, false)), ESCREENEXISTS);
        assert_eq!(
            e(&mut c, &window(12, 8, Rect::new(0, 0, 2, 2), RED)),
            ENODRAWSCREEN
        );
        let m = alloc_on(12, 9, md::GREY8, false, Rect::new(0, 0, 2, 2), RED);
        assert_eq!(e(&mut c, &m), ECHAN);
        assert_eq!(e(&mut c, &top(true, &[1])), ENOTWINDOW);
        assert_eq!(e(&mut c, &[b'F', 8, 0, 0, 0]), ENODRAWSCREEN);
    }

    /// A window named by one connection and looked up by another, which
    /// puts a screen and a window of its own on it: a window manager's
    /// window and its client's.
    #[test]
    fn named_windows_and_nested_screens() {
        let (d, mut wm) = conn();
        let mut client = Conn::new(0);
        let mut m = alloc(99, md::GREY1, true, Rect::new(0, 0, 1, 1), 0xFFFFFFFF);
        m.extend(alloc(1, md::RGB24, true, Rect::new(0, 0, 1, 1), BLUE));
        m.extend(screen_msg(9, 0, 1, true));
        m.extend(window(2, 9, Rect::new(2, 2, 14, 8), RED));
        m.extend(name(2, true, "window.1.1"));
        d.messages(&mut wm, &m).unwrap();
        let e = |cid: u32, c: &mut Conn, m: &[u8]| -> String {
            d.messages_from(cid, c, m).unwrap_err().as_str().into()
        };
        assert_eq!(e(1, &mut wm, &name(1, true, "window.1.1")), ENAMEUSED);
        assert_eq!(e(1, &mut wm, &name(1, false, "window.1.1")), EWRONGNAME);
        assert_eq!(e(2, &mut client, &lookup(5, "nonesuch")), ENONAME);
        // The client draws in a window of its own on the named image,
        // inside its border.
        let mut m = lookup(5, "window.1.1");
        m.extend(alloc(6, md::RGB24, true, Rect::new(0, 0, 1, 1), GREEN));
        m.extend(alloc(
            99,
            md::GREY1,
            true,
            Rect::new(0, 0, 1, 1),
            0xFFFFFFFF,
        ));
        m.extend(screen_msg(20, 5, 6, false));
        m.extend(window(7, 20, Rect::new(3, 3, 13, 7), YELLOW));
        d.messages_from(2, &mut client, &m).unwrap();
        assert_eq!(
            [d.px(2, 2), d.px(3, 3), d.px(12, 6), d.px(1, 1)],
            [0xFF0000, 0xFFFF00, 0xFFFF00, 0xFFFFFF]
        );
        d.messages_from(2, &mut client, &fill(7, 6, Rect::new(0, 0, 16, 8)))
            .unwrap();
        assert_eq!([d.px(2, 2), d.px(5, 5)], [0xFF0000, 0xFF00]);
        // Only the screen's owner, or anyone for a public one, may use it.
        let mut m = alloc::vec![b'S'];
        m.extend(20u32.to_le_bytes());
        m.extend(md::XRGB32.0.to_le_bytes());
        assert_eq!(e(1, &mut wm, &m), ENODRAWSCREEN);
        m[1] = 9;
        d.messages_from(1, &mut wm, &m).unwrap();
        // The window manager's freeing its window takes it off the screen
        // at once; the client's image stays until the client lets go.
        d.messages(&mut wm, &[b'f', 2, 0, 0, 0]).unwrap();
        assert_eq!([d.px(2, 2), d.px(5, 5)], [0xFF, 0xFF]);
        d.messages_from(2, &mut client, &fill(7, 6, Rect::new(0, 0, 16, 8)))
            .unwrap();
        assert_eq!(d.px(5, 5), 0xFF);
        let images = d.state.lock().images.len();
        d.state.lock().conns.insert(2, client);
        d.state.lock().close(2);
        // The client's window, colour, mask and the named image went, and with
        // them the name and the client's screen.
        assert_eq!(d.state.lock().images.len(), images - 4);
        assert!(d.state.lock().names.is_empty());
        assert!(!d.state.lock().screens.contains_key(&20));
    }
}
