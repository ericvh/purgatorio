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
//!
//! Polygon points are coded as devdraw codes them: each coordinate a byte
//! that is a 7-bit signed difference from the previous one, or, with the
//! top bit set, a 23-bit absolute value in three bytes. Screens and
//! windows (`A`, `F`, `S`, `t`), named images (`n`, `N`) and the font
//! cache (`i`, `l`, `s`, `x`) are not implemented yet and fail with
//! `bad draw command`. Errors are devdraw's strings.
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
use purg_platform::sync::{Mutex, Rendez};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan as KChan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};

const ENODRAWIMAGE: &str = "unknown id for draw image";
const ENODRAWSCREEN: &str = "unknown id for draw screen";
const ESHORTDRAW: &str = "short draw message";
const ESHORTREAD: &str = "draw read too short";
const EIMAGEEXISTS: &str = "image id in use";
const EDRAWMEM: &str = "image memory allocation failed";
const EREADOUTSIDE: &str = "readimage outside image";
const EWRITEOUTSIDE: &str = "writeimage outside image";
const EBADCMD: &str = "bad draw command";
const ENODATA: &str = "no draw data";
const ECTL: &str = "unknown draw control request";

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
    /// The image `ctl` reports on.
    info: u32,
    /// The operator for the next drawing message.
    op: Op,
    /// Pixels waiting for a read of `data`, after `r`.
    readdata: Option<Vec<u8>>,
}

struct Client {
    opens: Mutex<u32>,
    conn: Mutex<Conn>,
    /// `refresh` readers sleep here; with no windows nothing wakes them.
    refresh: Rendez,
}

struct ScreenImage {
    image: Image,
    damage: Option<Rect>,
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
        let (w, h) = display.size();
        let r = Rect::new(0, 0, i32::try_from(w).ok()?, i32::try_from(h).ok()?);
        let mut image = Image::new(r, Chan(display.chan()))?;
        image.fill(md::rgba(0xFFFF_FFFF));
        Some(DevDraw {
            display,
            user,
            screen: Mutex::new(ScreenImage {
                image,
                damage: Some(r),
            }),
            clients: Mutex::new(BTreeMap::new()),
            next: Mutex::new(1),
        })
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
                    conn: Mutex::new(Conn {
                        images: BTreeMap::new(),
                        screen: true,
                        info: 0,
                        op: Op::SOVER_D,
                        readdata: None,
                    }),
                    refresh: Rendez::new(),
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
            cl.refresh.wakeup();
        }
    }

    fn read(&self, c: &KChan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let path = c.qid.path;
        match kind_of(path) {
            QTOP | QDRAW | KDIR => Err(Error::new(error::EISDIR)),
            KCTL => {
                let cl = self.client(conn_of(path))?;
                let conn = cl.conn.lock();
                let s = self.info(conn_of(path), &conn, conn.info)?;
                if buf.len() < s.len() {
                    return Err(Error::new(ESHORTREAD));
                }
                buf[..s.len()].copy_from_slice(s.as_bytes());
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
                let cl = self.client(conn_of(path))?;
                let id = conn_of(path);
                // No windows, so nothing needs refreshing: wait until the
                // connection goes or the reader is interrupted.
                crate::block::park(&cl.refresh, || !self.clients.lock().contains_key(&id))?;
                Ok(0)
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
                conn.info = id;
                Ok(4)
            }
            KDATA => {
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
        let c = Conn {
            images: BTreeMap::new(),
            screen: true,
            info: 0,
            op: Op::SOVER_D,
            readdata: None,
        };
        (d, c)
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
