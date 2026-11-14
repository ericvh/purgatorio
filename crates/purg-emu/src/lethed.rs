//! `lethed`: serves a host directory read-only over Lethe
//! (`docs/lethe.md`).
//!
//! ```text
//! lethed [-v] [-A] [-C alg] [-a addr] [-u owner] [-t ticketkey] [-e lifetime] [-w window]
//!        [-k keyfile ...] [-U userdir] [-s signers] [-r revoked] root
//! ```
//!
//! `-C x25519mlkem768` issues tickets only from the hybrid X25519 +
//! ML-KEM-768 exchange (`Tauth2`), and refuses a `Tauth` and a request on
//! a ticket from one with `post-quantum key exchange required`. Without it
//! a client may still ask for the hybrid exchange.
//!
//! `-A` also answers requests with no authentication and no encryption,
//! as the user `none`. Without it, and without a key to trust, `lethed`
//! does not start.
//!
//! The protocol is `purg_lethe::server`; this is the host's half: TCP,
//! the clock, randomness, the trust files and a [`Tree`] over host files
//! with `#U`'s host semantics (`crate::host`).

use std::collections::HashMap;
use std::fs::{self, File};
use std::hash::Hash;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use purg_abi::error::{ENONEXIST, ENOTDIR};
use purg_lethe::client::read_frame;
use purg_lethe::server::{Config, Env, Event, Server, Tree, Trust};
use purg_styx::{Dir, Qid};

use crate::devfs;
use crate::host;

/// The usage line.
pub const USAGE: &str = "usage: lethed [-v] [-A] [-a addr] [-u owner] [-t ticketkey] [-e lifetime] [-w window] [-k keyfile ...] [-U userdir] [-s signers] [-r revoked] root";

/// What the command line says.
pub struct Options {
    pub verbose: bool,
    /// `-A`: answer clear requests too, as `none`.
    pub clear: bool,
    pub addr: String,
    pub owner: Option<String>,
    pub ticket_key: Option<PathBuf>,
    pub config: Config,
    /// Public key files trusted for any user.
    pub keys: Vec<PathBuf>,
    /// A directory of `<uname>/key.pub` files.
    pub user_keys: Option<PathBuf>,
    pub signers: Option<PathBuf>,
    pub revoked: Option<PathBuf>,
    pub root: PathBuf,
}

/// Parses the arguments after the command name.
pub fn parse(args: &[String]) -> Result<Options, String> {
    let mut o = Options {
        verbose: false,
        clear: false,
        addr: String::from("127.0.0.1:5640"),
        owner: None,
        ticket_key: None,
        config: Config::default(),
        keys: Vec::new(),
        user_keys: None,
        signers: None,
        revoked: None,
        root: PathBuf::new(),
    };
    let mut it = args.iter();
    let mut root = None;
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(|| String::from(USAGE));
        let secs = |s: String| s.parse::<u64>().map_err(|_| String::from(USAGE));
        match a.as_str() {
            "-v" => o.verbose = true,
            "-A" => o.clear = true,
            "-C" => {
                o.config.pq = match val()?.as_str() {
                    "x25519" => false,
                    "x25519mlkem768" => true,
                    a => return Err(format!("lethed: unknown algorithm: {a}")),
                }
            }
            "-a" => o.addr = val()?,
            "-u" => o.owner = Some(val()?),
            "-t" => o.ticket_key = Some(PathBuf::from(val()?)),
            "-e" => o.config.lifetime = secs(val()?)?,
            "-w" => o.config.window = secs(val()?)?,
            "-k" => o.keys.push(PathBuf::from(val()?)),
            "-U" => o.user_keys = Some(PathBuf::from(val()?)),
            "-s" => o.signers = Some(PathBuf::from(val()?)),
            "-r" => o.revoked = Some(PathBuf::from(val()?)),
            s if s.starts_with('-') => return Err(String::from(USAGE)),
            s if root.is_none() => root = Some(PathBuf::from(s)),
            _ => return Err(String::from(USAGE)),
        }
    }
    o.root = root.ok_or_else(|| String::from(USAGE))?;
    // Fail closed: nothing is served unless someone can authenticate or
    // the clear mode was asked for by name.
    if !o.clear && o.keys.is_empty() && o.signers.is_none() && o.user_keys.is_none() {
        return Err(String::from(
            "lethed: no client can authenticate: give -k, -U or -s, or -A to serve without authentication",
        ));
    }
    Ok(o)
}

/// How long `lethed`'s soft caches trust what they hold. A file renamed
/// over, or a link changed, is seen at most this late; nothing is lost
/// when they are dropped.
const SOFT_LIFE: Duration = Duration::from_secs(1);

/// A small map whose entries are trusted for [`SOFT_LIFE`]. When it is
/// full the old entries go, and if none are old, all of them.
struct Soft<K, V> {
    map: Mutex<HashMap<K, (V, Instant)>>,
    max: usize,
}

impl<K: Eq + Hash, V: Clone> Soft<K, V> {
    fn new(max: usize) -> Soft<K, V> {
        Soft {
            map: Mutex::new(HashMap::new()),
            max,
        }
    }

    fn get(&self, k: &K) -> Option<V> {
        let m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        m.get(k)
            .filter(|(_, at)| at.elapsed() < SOFT_LIFE)
            .map(|(v, _)| v.clone())
    }

    fn put(&self, k: K, v: V) {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= self.max {
            m.retain(|_, (_, at)| at.elapsed() < SOFT_LIFE);
            if m.len() >= self.max {
                m.clear();
            }
        }
        m.insert(k, (v, Instant::now()));
    }
}

/// A directory's names in byte order, for the qid they were read at.
type Names = (Qid, Arc<Vec<String>>);

/// Host files under a root, read only.
///
/// A stateless server answers each request from the start: the path is
/// resolved and checked against the root, the file opened, a directory
/// listed again for every page. The soft caches keep the last of each for
/// a second: resolved paths, open files (checked against the path's
/// current inode on each read) and directory listings (by the
/// directory's qid).
pub struct HostTree {
    root: PathBuf,
    owner: Option<String>,
    resolved: Soft<String, PathBuf>,
    files: Soft<PathBuf, (Arc<File>, u64)>,
    lists: Soft<PathBuf, Names>,
}

fn err(e: std::io::Error) -> String {
    host::error_string(&e)
}

impl HostTree {
    pub fn new(root: &Path, owner: Option<String>) -> Result<HostTree, String> {
        let root = fs::canonicalize(root).map_err(|e| format!("{}: {}", root.display(), err(e)))?;
        if !root.is_dir() {
            return Err(format!("{}: {ENOTDIR}", root.display()));
        }
        Ok(HostTree {
            root,
            owner,
            resolved: Soft::new(8192),
            files: Soft::new(64),
            lists: Soft::new(16),
        })
    }

    /// The host path of a Lethe path: each name exactly as given (case
    /// matters, even on a case-insensitive volume), and the whole path,
    /// symbolic links resolved, inside the root. A name that leads out is
    /// answered as one that does not exist.
    ///
    /// The parent is resolved first (usually from the cache), so a name
    /// that is not a link costs one `lstat`; a link is resolved in full
    /// and checked against the root.
    fn resolve(&self, p: &str) -> Result<PathBuf, String> {
        if p == "/" {
            return Ok(self.root.clone());
        }
        if let Some(real) = self.resolved.get(&String::from(p)) {
            return Ok(real);
        }
        let (parent, name) = match p.rfind('/') {
            Some(0) => ("/", &p[1..]),
            Some(i) => (&p[..i], &p[i + 1..]),
            None => return Err(String::from(ENONEXIST)),
        };
        let dir = self.resolve(parent)?;
        if !host::exact_name_exists(&dir, name).map_err(err)? {
            return Err(String::from(ENONEXIST));
        }
        let cand = dir.join(name);
        let md = fs::symlink_metadata(&cand).map_err(err)?;
        let real = if md.file_type().is_symlink() {
            self.inside(&cand)?
        } else {
            cand
        };
        self.resolved.put(String::from(p), real.clone());
        Ok(real)
    }

    /// The open file at `real`, whose current metadata is `md`: the cached
    /// one if it is still that file.
    fn open(&self, real: &Path, md: &fs::Metadata) -> Result<Arc<File>, String> {
        let id = devfs::host_qid(md).path;
        let key = real.to_path_buf();
        if let Some((f, was)) = self.files.get(&key)
            && was == id
        {
            return Ok(f);
        }
        let f = Arc::new(File::open(real).map_err(err)?);
        // Opened after the stat: keep it only if it is the same file.
        if f.metadata().is_ok_and(|m| devfs::host_qid(&m).path == id) {
            self.files.put(key, (f.clone(), id));
        }
        Ok(f)
    }

    /// The names in directory `path`, sorted as bytes, for its qid `q`.
    fn names(&self, path: &Path, q: Qid) -> Result<Arc<Vec<String>>, String> {
        let key = path.to_path_buf();
        if let Some((was, names)) = self.lists.get(&key)
            && was == q
        {
            return Ok(names);
        }
        let mut names = Vec::new();
        for e in fs::read_dir(path).map_err(err)? {
            let e = e.map_err(err)?;
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            if !host::hidden_entry(&name) {
                names.push(name);
            }
        }
        names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        let names = Arc::new(names);
        self.lists.put(key, (q, names.clone()));
        Ok(names)
    }

    fn inside(&self, path: &Path) -> Result<PathBuf, String> {
        let real = fs::canonicalize(path).map_err(err)?;
        if real.starts_with(&self.root) {
            Ok(real)
        } else {
            Err(String::from(ENONEXIST))
        }
    }

    fn entry(&self, name: &str, md: &fs::Metadata) -> Dir {
        let mut d = devfs::entry(String::from(name), md);
        if let Some(o) = &self.owner {
            d.uid = o.clone();
            d.gid = o.clone();
            d.muid = o.clone();
        }
        d
    }
}

fn last(p: &str) -> &str {
    if p == "/" {
        "/"
    } else {
        p.rsplit('/').next().unwrap_or(p)
    }
}

impl Tree for HostTree {
    fn stat(&self, p: &str) -> Result<Dir, String> {
        let path = self.resolve(p)?;
        let md = fs::metadata(&path).map_err(err)?;
        Ok(self.entry(last(p), &md))
    }

    fn read(&self, p: &str, offset: u64, buf: &mut [u8]) -> Result<(Qid, usize), String> {
        let real = self.resolve(p)?;
        let md = fs::metadata(&real).map_err(err)?;
        let f = self.open(&real, &md)?;
        let mut n = 0;
        while n < buf.len() {
            match host::read_at(&f, &mut buf[n..], offset + n as u64) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(err(e)),
            }
        }
        Ok((devfs::host_qid(&md), n))
    }

    fn list(&self, p: &str, after: &[u8], limit: usize) -> Result<(Qid, Vec<Dir>), String> {
        let path = self.resolve(p)?;
        let md = fs::metadata(&path).map_err(err)?;
        let q = devfs::host_qid(&md);
        // Names first, which costs no stat (and is kept while the
        // directory's qid stays); then only the entries this page can
        // hold are resolved and stat'ed.
        let names = self.names(&path, q)?;
        let from = names.partition_point(|n| n.as_bytes() <= after);
        let mut out = Vec::new();
        for name in &names[from..] {
            if out.len() == limit {
                break;
            }
            // An entry that is not a link costs one `lstat`. Links out of
            // the tree and dangling links are left out.
            let cand = path.join(name);
            let md = match fs::symlink_metadata(&cand) {
                Ok(md) if !md.file_type().is_symlink() => md,
                Ok(_) => match self.inside(&cand).map(fs::metadata) {
                    Ok(Ok(md)) => md,
                    _ => continue,
                },
                Err(_) => continue,
            };
            out.push(self.entry(name, &md));
        }
        Ok((q, out))
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_key(path: &Path) -> Option<[u8; 32]> {
    fs::read(path).ok()?.try_into().ok()
}

/// The trust for `uname`, read now from the files the options name: an
/// edit takes effect on the next ticket.
fn trust(o: &Options, uname: &str) -> Trust {
    let mut keys: Vec<purg_auth::sig::Id> = o
        .keys
        .iter()
        .filter_map(|p| read_key(p).map(purg_auth::sig::Id::Ed25519))
        .collect();
    if let Some(dir) = &o.user_keys
        && purg_auth::cert::valid_name(uname)
        && let Some(k) = read_key(&dir.join(uname).join("key.pub"))
    {
        keys.push(purg_auth::sig::Id::Ed25519(k));
    }
    let signers = o
        .signers
        .as_ref()
        .and_then(|p| fs::read(p).ok())
        .map(|t| purg_auth::keys::parse_list_lenient(&t))
        .unwrap_or_default();
    // A list that is missing revokes nothing; one that is there but does
    // not read or parse fails closed.
    let revoked = match &o.revoked {
        None => Some(Vec::new()),
        Some(p) => match fs::read(p) {
            Ok(t) => purg_auth::keys::parse_list(&t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Vec::new()),
            Err(_) => None,
        },
    };
    Trust {
        keys,
        signers,
        revoked,
    }
}

fn log(o: &Options, ev: &Event) {
    if !o.verbose {
        return;
    }
    let line = match ev {
        Event::Issued {
            uname,
            rights,
            prefix,
        } => format!("ticket for {uname} rights {rights} prefix {prefix}"),
        Event::Answered {
            uname,
            op,
            path,
            error,
        } => match error {
            Some(e) => format!("{uname} {op} {path}: {e}"),
            None => format!("{uname} {op} {path}"),
        },
        Event::Refused(e) => format!("refused: {e}"),
    };
    eprintln!("lethed: {line}");
}

fn serve(o: &Options, server: &Server, tree: &HostTree, mut s: TcpStream) {
    s.set_nodelay(true).ok();
    loop {
        let frame = {
            let mut read = |b: &mut [u8]| s.read_exact(b).map_err(|e| e.to_string());
            match read_frame::<String>(&mut read) {
                Ok(f) => f,
                Err(_) => return,
            }
        };
        let mut random = |b: &mut [u8]| host::entropy(b);
        let mut t = |u: &str| trust(o, u);
        let mut env = Env {
            now: now(),
            random: &mut random,
            trust: &mut t,
        };
        let (reply, ev) = server.handle(tree, &mut env, &frame);
        log(o, &ev);
        if s.write_all(&reply).is_err() {
            return;
        }
    }
}

/// Runs the server until the process is killed.
pub fn run(o: Options) -> Result<(), String> {
    let tree = HostTree::new(&o.root, o.owner.clone())?;
    let key = match &o.ticket_key {
        Some(p) => read_key(p).ok_or_else(|| format!("{}: not a 32-byte key", p.display()))?,
        None => {
            let mut k = [0u8; 32];
            host::entropy(&mut k);
            k
        }
    };
    let server = Server::new(o.config.clone(), key).with_clear(o.clear);
    let listener = TcpListener::bind(&o.addr).map_err(|e| format!("{}: {}", o.addr, err(e)))?;
    let addr = listener.local_addr().map_err(err)?;
    println!("lethed: listening on {addr}");
    std::io::stdout().flush().ok();
    let shared = Arc::new((o, server, tree));
    for s in listener.incoming() {
        let Ok(s) = s else { continue };
        let shared = shared.clone();
        std::thread::spawn(move || serve(&shared.0, &shared.1, &shared.2, s));
    }
    Ok(())
}

/// `lethed`'s main.
pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args).and_then(run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}
