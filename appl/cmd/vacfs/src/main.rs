//! vacfs [-Ddp] [-a addr] [[tag:]score]: serve the vac archive `score`
//! from a Venti server, read-only, as Styx on standard input (Inferno's
//! vacfs(4)), as in `mount {vacfs vac:...} /n/vac`.
//!
//! With no score the root is empty and walking to a name that is a score
//! opens that archive there. `-a` dials `addr` instead of `$venti`; `-p`
//! turns off permission checking; `-D` traces the Styx messages and `-d`
//! prints debugging messages, on standard error.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::time::{SystemTime, UNIX_EPOCH};

use purg_styx::{IOHDRSZ, MAX_MSIZE, Msg, Qid, StyxFs};
use purgatorio::abi::{error, mode};
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Dir, Error};
use styxserv::rotree::dir_slice;
use venti::vac::{self, Direntry, Source, VacDir};
use venti::{Score, Session};

const USAGE: &str = "vacfs [-Ddp] [-a addr] [[tag:]score]";

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    purgatorio::exits(&format!("fail:{msg}"));
}

enum Kind {
    /// The root when serving any archive: its children are scores.
    Scores(HashMap<Score, usize>),
    Dir {
        vd: VacDir,
        /// Children met so far, by name.
        kids: BTreeMap<String, usize>,
    },
    File(Source),
}

struct Node {
    dir: Dir,
    parent: usize,
    kind: Kind,
}

struct Fid {
    node: usize,
    uname: String,
    open: bool,
    /// The packed listing a directory read returns, made at offset 0.
    snap: Vec<u8>,
}

struct Vacfs {
    session: Session<File>,
    nodes: Vec<Node>,
    fids: HashMap<u32, Fid>,
    pflag: bool,
    dflag: bool,
    iounit: u32,
}

/// Whether `uname` may open a file of `perm`, owner `uid` and group
/// `gid` with `omode`, as Inferno's styxservers decides.
fn openok(uname: &str, omode: u8, perm: u32, uid: &str, gid: &str) -> bool {
    let mut t = match omode & mode::ACCESS {
        mode::OREAD => 4,
        mode::OWRITE => 2,
        mode::ORDWR => 6,
        _ => 1,
    };
    if omode & mode::OTRUNC != 0 {
        if perm & DMDIR != 0 {
            return false;
        }
        t |= 2;
    }
    if uname == uid && (perm >> 6) & t == t {
        return true;
    }
    if uname == gid && (perm >> 3) & t == t {
        return true;
    }
    perm & t == t
}

impl Vacfs {
    fn say(&self, s: &str) {
        if self.dflag {
            eprintln!("{s}");
        }
    }

    fn add(&mut self, mut dir: Dir, parent: usize, kind: Kind) -> usize {
        let n = self.nodes.len();
        dir.qid.path = n as u64;
        self.nodes.push(Node { dir, parent, kind });
        n
    }

    /// The node for the file `de` of directory node `n`.
    fn child(&mut self, n: usize, de: &Direntry) -> Result<usize, String> {
        if let Kind::Dir { kids, .. } = &self.nodes[n].kind
            && let Some(&c) = kids.get(&de.elem)
        {
            return Ok(c);
        }
        let Kind::Dir { vd, .. } = &mut self.nodes[n].kind else {
            return Err(String::from(error::ENOTDIR));
        };
        let (e, me) = vd.open(&mut self.session, de)?;
        let mut d = de.to_dir();
        let kind = match me {
            Some(me) => Kind::Dir {
                vd: VacDir::new(e, me),
                kids: BTreeMap::new(),
            },
            None => {
                d.length = e.size;
                Kind::File(Source::new(e))
            }
        };
        let c = self.add(d, n, kind);
        if let Kind::Dir { kids, .. } = &mut self.nodes[n].kind {
            kids.insert(de.elem.clone(), c);
        }
        Ok(c)
    }

    fn walk1(&mut self, n: usize, name: &str) -> Result<usize, String> {
        if name == ".." {
            return Ok(self.nodes[n].parent);
        }
        match &mut self.nodes[n].kind {
            Kind::Scores(m) => {
                let score = venti::parse_tagged(name, "vac")?;
                if let Some(&c) = m.get(&score) {
                    return Ok(c);
                }
                let (vd, de, _) = vac::vdroot(&mut self.session, &score)?;
                let mut d = de.to_dir();
                d.name = String::from(name);
                let c = self.add(
                    d,
                    n,
                    Kind::Dir {
                        vd,
                        kids: BTreeMap::new(),
                    },
                );
                if let Kind::Scores(m) = &mut self.nodes[n].kind {
                    m.insert(score, c);
                }
                Ok(c)
            }
            Kind::Dir { vd, .. } => {
                let de = vd
                    .walk(&mut self.session, name)?
                    .ok_or_else(|| String::from("no such file or directory"))?;
                self.child(n, &de)
            }
            Kind::File(_) => Err(String::from(error::ENOTDIR)),
        }
    }

    fn listing(&mut self, n: usize) -> Result<Vec<u8>, String> {
        let list = match &mut self.nodes[n].kind {
            Kind::Dir { vd, .. } => vd
                .list(&mut self.session)
                .map_err(|e| format!("reading directory: {e}"))?,
            _ => Vec::new(),
        };
        let mut out = Vec::new();
        for de in list {
            let c = self.child(n, &de)?;
            self.nodes[c]
                .dir
                .pack_into(&mut out)
                .map_err(|e| e.to_string())?;
        }
        Ok(out)
    }

    fn fid(&self, fid: u32) -> Result<&Fid, String> {
        self.fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))
    }
}

impl StyxFs for Vacfs {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, uname: &str, _aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        self.fids.insert(
            fid,
            Fid {
                node: 0,
                uname: String::from(uname),
                open: false,
                snap: Vec::new(),
            },
        );
        Ok(self.nodes[0].dir.qid)
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let f = self.fid(fid)?;
        if f.open {
            return Err(String::from("fid is open"));
        }
        let (mut at, uname) = (f.node, f.uname.clone());
        let mut qids = Vec::new();
        for name in names {
            if self.nodes[at].dir.mode & DMDIR == 0 {
                if qids.is_empty() {
                    return Err(String::from(error::ENOTDIR));
                }
                return Ok(qids);
            }
            self.say(&format!("walk {name}"));
            match self.walk1(at, name) {
                Ok(n) => {
                    at = n;
                    qids.push(self.nodes[n].dir.qid);
                }
                Err(e) if qids.is_empty() => return Err(e),
                Err(_) => return Ok(qids),
            }
        }
        self.fids.insert(
            newfid,
            Fid {
                node: at,
                uname,
                open: false,
                snap: Vec::new(),
            },
        );
        Ok(qids)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        let f = self.fid(fid)?;
        let d = &self.nodes[f.node].dir;
        let access = omode & mode::ACCESS;
        if (access != mode::OREAD && access != mode::OEXEC)
            || omode & (mode::OTRUNC | mode::ORCLOSE) != 0
        {
            return Err(String::from(error::EPERM));
        }
        if !self.pflag && !openok(&f.uname, omode, d.mode, &d.uid, &d.gid) {
            return Err(String::from(error::EPERM));
        }
        let q = d.qid;
        if let Some(f) = self.fids.get_mut(&fid) {
            f.open = true;
        }
        Ok((q, self.iounit))
    }

    fn create(&mut self, _fid: u32, _name: &str, _perm: u32, _m: u8) -> Result<(Qid, u32), String> {
        Err(String::from(error::EPERM))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let n = self.fid(fid)?.node;
        if self.nodes[n].dir.mode & DMDIR != 0 {
            if offset == 0 {
                let snap = self.listing(n)?;
                if let Some(f) = self.fids.get_mut(&fid) {
                    f.snap = snap;
                }
            }
            return Ok(dir_slice(&self.fid(fid)?.snap, offset, count));
        }
        let Vacfs { session, nodes, .. } = self;
        match &mut nodes[n].kind {
            Kind::File(src) => src.read_at(session, offset, count as usize),
            _ => Err(String::from(error::EIO)),
        }
    }

    fn write(&mut self, _fid: u32, _offset: u64, _data: &[u8]) -> Result<u32, String> {
        Err(String::from(error::EPERM))
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        self.fids
            .remove(&fid)
            .map(drop)
            .ok_or_else(|| String::from("unknown fid"))
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        self.clunk(fid)?;
        Err(String::from(error::EPERM))
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let n = self.fid(fid)?.node;
        self.nodes[n].dir.pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, _fid: u32, _stat: &[u8]) -> Result<(), String> {
        Err(String::from(error::EPERM))
    }
}

/// Serves 9P on fd 0 until the client goes away, tracing each message
/// with `trace`.
fn serve(fs: &mut Vacfs, trace: bool) -> Result<(), Error> {
    // SAFETY: fd 0 is the conversation the mounter gave this prog, and
    // nothing else in the prog uses it.
    let mut io = unsafe { File::from_raw_fd(0) };
    let mut msize = MAX_MSIZE;
    loop {
        let Some(buf) = styxserv::read_msg(&mut io)? else {
            return Ok(());
        };
        let m = styxserv::decode(&buf).map_err(Error)?;
        if trace {
            eprintln!("<- {}", styxserv::text::msg(&m));
        }
        let reply = purg_styx::dispatch(fs, &m, &mut msize);
        if trace && let Ok(r) = Msg::decode(&reply, styxserv::MAXMSG as u32) {
            eprintln!("-> {}", styxserv::text::msg(&r));
        }
        io.write_all(&reply)?;
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut addr = String::from(venti::DEFAULT_ADDR);
    let (mut trace, mut dflag, mut pflag) = (false, false, false);
    while let Some(c) = arg.opt() {
        match c {
            'D' => trace = true,
            'a' => addr = arg.earg()?,
            'd' => dflag = true,
            'p' => pflag = true,
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let rest = arg.argv();
    if rest.len() > 1 {
        return Err(usage);
    }
    let score = rest
        .first()
        .map(|s| venti::parse_tagged(s, "vac").unwrap_or_else(|e| fail(&e)));
    let session = venti::dial(&addr).unwrap_or_else(|e| fail(&e));
    let mut fs = Vacfs {
        session,
        nodes: Vec::new(),
        fids: HashMap::new(),
        pflag,
        dflag,
        iounit: 8192,
    };
    fs.say("have handshake");
    match score {
        None => {
            let u = purgatorio::auth::user();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as u32);
            let d = Dir {
                name: String::from("/"),
                uid: u.clone(),
                gid: u.clone(),
                muid: u,
                qid: Qid {
                    qtype: 0x80,
                    ..Qid::default()
                },
                mode: DMDIR | 0o755,
                atime: now,
                mtime: now,
                ..Dir::default()
            };
            fs.add(d, 0, Kind::Scores(HashMap::new()));
        }
        Some(score) => {
            let (vd, de, _) = vac::vdroot(&mut fs.session, &score).unwrap_or_else(|e| fail(&e));
            let kind = Kind::Dir {
                vd,
                kids: BTreeMap::new(),
            };
            fs.add(de.to_dir(), 0, kind);
        }
    }
    serve(&mut fs, trace)
}
