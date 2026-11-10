//! iostats [-d] [-f dbfile] cmd [args...]: run `cmd` with its namespace
//! seen through a 9P connection that iostats counts, and when it is done
//! print on standard error what crossed it: bytes read and written, every
//! kind of message with its count, times and sizes, and every file opened
//! with its reads and writes. `-d` logs each message to `dbfile` (default
//! `iostats.out`), `->` before T-messages and `<-` before R-messages.
//!
//! The command's namespace is a copy of this one with `/` replaced by the
//! counting connection, which leads to an export of the old `/`; its
//! current directory is kept. Requests go through one at a time, so a
//! command that blocks reading a file blocks its other requests too.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::Duration;

use purg_styx::{Dir, Fcall, Msg, Qid};
use purgatorio::abi::{mode, mount, pctl};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};
use styxserv::text;

const USAGE: &str = "iostats [-d] [-f debugfile] cmds [args ...]";

/// The fd the helper finds the counting connection on.
const CONNFD: i32 = 5;

/// The message kinds in the order the report lists them.
const KINDS: [(&str, u8); 13] = [
    ("Tversion", 100),
    ("Tauth", 102),
    ("Tflush", 108),
    ("Tattach", 104),
    ("Twalk", 110),
    ("Topen", 112),
    ("Tcreate", 114),
    ("Tclunk", 120),
    ("Tread", 116),
    ("Twrite", 118),
    ("Tremove", 122),
    ("Tstat", 124),
    ("Twstat", 126),
];

#[derive(Default)]
struct Rpc {
    count: u64,
    low: Duration,
    high: Duration,
    total: Duration,
    tbytes: u64,
    rbytes: u64,
}

#[derive(Default)]
struct FileStat {
    opens: u32,
    reads: u64,
    rbytes: u64,
    writes: u64,
    wbytes: u64,
}

#[derive(Default)]
struct Stats {
    rpcs: BTreeMap<u8, Rpc>,
    /// The path each fid names.
    fids: BTreeMap<u32, String>,
    /// Per-file counts, by path, with the order each path was first opened.
    files: BTreeMap<String, (usize, FileStat)>,
    /// Fids open on directories.
    dirs: BTreeSet<u32>,
    /// The qid paths given to device roots, by name.
    roots: BTreeMap<String, u64>,
    read: u64,
    write: u64,
    protocol: u64,
}

fn join(base: &str, names: &[&str]) -> String {
    let mut elems: Vec<&str> = base.split('/').filter(|e| !e.is_empty()).collect();
    for n in names {
        match *n {
            "." => {}
            ".." => {
                elems.pop();
            }
            n => elems.push(n),
        }
    }
    format!("/{}", elems.join("/"))
}

impl Stats {
    /// Every device's root has qid path 0, so through the export the
    /// directories that hold them (`/dev`, `/prog`, ...) would look like one
    /// file to the kernel, and a bind on one would show on all. Such a
    /// qid, other than that of `/`, gets a path of its own, by name.
    fn qid(&mut self, path: &str, q: Qid) -> Qid {
        if q.path != 0 || !q.is_dir() || path == "/" {
            return q;
        }
        let n = self.roots.len() as u64;
        let p = *self
            .roots
            .entry(String::from(path))
            .or_insert(0x8000_0000_0000_0000 | (n + 1));
        Qid { path: p, ..q }
    }

    fn dir(&mut self, path: &str, mut d: Dir) -> Dir {
        d.qid = self.qid(path, d.qid);
        d
    }

    /// The reply with device roots' qids made distinct, when it needs it.
    fn fix(&mut self, t: &Msg<'_>, r: &Msg<'_>) -> Option<Vec<u8>> {
        let path = |s: &Stats, fid: &u32| s.fids.get(fid).cloned().unwrap_or_default();
        let fcall = match (&t.fcall, &r.fcall) {
            (Fcall::Twalk { fid, wnames, .. }, Fcall::Rwalk { wqids }) => {
                let base = path(self, fid);
                let wqids = wqids
                    .iter()
                    .enumerate()
                    .map(|(i, q)| self.qid(&join(&base, &wnames[..=i]), *q))
                    .collect();
                Fcall::Rwalk { wqids }
            }
            (Fcall::Topen { fid, .. }, Fcall::Ropen { qid, iounit }) => {
                if qid.is_dir() {
                    self.dirs.insert(*fid);
                }
                let p = path(self, fid);
                Fcall::Ropen {
                    qid: self.qid(&p, *qid),
                    iounit: *iounit,
                }
            }
            (Fcall::Tstat { fid }, Fcall::Rstat { stat }) => {
                let (d, _) = Dir::unpack(stat).ok()?;
                let p = path(self, fid);
                let packed = self.dir(&p, d).pack().ok()?;
                return Msg {
                    tag: r.tag,
                    fcall: Fcall::Rstat { stat: &packed },
                }
                .encode()
                .ok();
            }
            (Fcall::Tread { fid, .. }, Fcall::Rread { data }) if self.dirs.contains(fid) => {
                let base = path(self, fid);
                let mut out = Vec::new();
                for d in purg_styx::dirs(data) {
                    let d = d.ok()?;
                    let p = join(&base, &[&d.name]);
                    self.dir(&p, d).pack_into(&mut out).ok()?;
                }
                return Msg {
                    tag: r.tag,
                    fcall: Fcall::Rread { data: &out },
                }
                .encode()
                .ok();
            }
            _ => return None,
        };
        Msg { tag: r.tag, fcall }.encode().ok()
    }

    fn file(&mut self, fid: u32) -> Option<&mut FileStat> {
        let path = self.fids.get(&fid)?.clone();
        let n = self.files.len();
        Some(&mut self.files.entry(path).or_insert((n, FileStat::default())).1)
    }

    fn account(&mut self, t: &Msg<'_>, r: &Msg<'_>, tlen: usize, rlen: usize, took: Duration) {
        let e = self.rpcs.entry(t.fcall.kind()).or_default();
        if e.count == 0 || took < e.low {
            e.low = took;
        }
        e.high = e.high.max(took);
        e.count += 1;
        e.total += took;
        e.tbytes += tlen as u64;
        e.rbytes += rlen as u64;
        self.protocol += (tlen + rlen) as u64;
        let ok = !matches!(r.fcall, Fcall::Rerror { .. });
        match (&t.fcall, &r.fcall) {
            (Fcall::Tattach { fid, .. }, _) if ok => {
                self.fids.insert(*fid, String::from("/"));
            }
            (
                Fcall::Twalk {
                    fid,
                    newfid,
                    wnames,
                },
                Fcall::Rwalk { wqids },
            ) if wqids.len() == wnames.len() => {
                let base = self.fids.get(fid).cloned().unwrap_or_default();
                self.fids.insert(*newfid, join(&base, wnames));
            }
            (Fcall::Tcreate { fid, name, .. }, _) if ok => {
                let base = self.fids.get(fid).cloned().unwrap_or_default();
                self.fids.insert(*fid, join(&base, &[name]));
                if let Some(f) = self.file(*fid) {
                    f.opens += 1;
                }
            }
            (Fcall::Topen { fid, .. }, _) if ok => {
                if let Some(f) = self.file(*fid) {
                    f.opens += 1;
                }
            }
            (Fcall::Tread { fid, .. }, Fcall::Rread { data }) => {
                let n = data.len() as u64;
                self.read += n;
                if let Some(f) = self.file(*fid) {
                    f.reads += 1;
                    f.rbytes += n;
                }
            }
            (Fcall::Twrite { fid, .. }, Fcall::Rwrite { count }) => {
                self.write += u64::from(*count);
                if let Some(f) = self.file(*fid) {
                    f.writes += 1;
                    f.wbytes += u64::from(*count);
                }
            }
            (Fcall::Tclunk { fid } | Fcall::Tremove { fid }, _) => {
                self.fids.remove(fid);
                self.dirs.remove(fid);
            }
            _ => {}
        }
    }

    fn report(&self, out: &mut impl Write) -> io::Result<()> {
        let time = |k: &[u8]| -> Duration {
            k.iter()
                .filter_map(|k| self.rpcs.get(k))
                .map(|r| r.total)
                .sum()
        };
        let all: Duration = self.rpcs.values().map(|r| r.total).sum();
        let rpcs: u64 = self.rpcs.values().map(|r| r.count).sum();
        writeln!(out)?;
        writeln!(
            out,
            "read      {} bytes, {} Kb/sec",
            self.read,
            rate(self.read, time(&[116]))
        )?;
        writeln!(
            out,
            "write     {} bytes, {} Kb/sec",
            self.write,
            rate(self.write, time(&[118]))
        )?;
        writeln!(
            out,
            "protocol  {} bytes, {} Kb/sec",
            self.protocol,
            rate(self.protocol, all)
        )?;
        writeln!(out, "rpc       {rpcs} count")?;
        writeln!(out)?;
        writeln!(
            out,
            "{:<10} {:>5} {:>5} {:>5} {:>5} {:>5}           T        R",
            "Message", "Count", "Low", "High", "Time", "  Avg"
        )?;
        let ms = |d: Duration| d.as_millis();
        for (name, k) in KINDS {
            let Some(r) = self.rpcs.get(&k).filter(|r| r.count > 0) else {
                continue;
            };
            writeln!(
                out,
                "{:<10} {:>5} {:>5} {:>5} {:>5} {:>5} ms {:>8} {:>8} bytes",
                name,
                r.count,
                ms(r.low),
                ms(r.high),
                ms(r.total),
                ms(r.total) / u128::from(r.count),
                r.tbytes,
                r.rbytes
            )?;
        }
        if self.files.is_empty() {
            return Ok(());
        }
        writeln!(out)?;
        writeln!(out, "Opens    Reads  (bytes)   Writes  (bytes) File")?;
        let mut files: Vec<_> = self.files.iter().collect();
        files.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
        for (path, (_, f)) in files {
            let name = match path.as_str() {
                "/fd/0" => "(stdin)",
                "/fd/1" => "(stdout)",
                "/fd/2" => "(stderr)",
                p => p,
            };
            writeln!(
                out,
                "{:>5} {:>8} {:>8} {:>8} {:>8} {name}",
                f.opens, f.reads, f.rbytes, f.writes, f.wbytes
            )?;
        }
        Ok(())
    }
}

/// Kilobytes a second, printed as `%g` would.
fn rate(bytes: u64, d: Duration) -> String {
    let s = d.as_secs_f64();
    if s == 0.0 {
        return String::from("0");
    }
    let v = bytes as f64 / 1024.0 / s;
    let s = format!("{v:.6}");
    let digits = s.split('.').next().map_or(0, str::len);
    let mut s = format!("{v:.*}", 6usize.saturating_sub(digits));
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    s
}

/// The fd the T-message relay copies each message to, after the time it
/// passed.
const SIDEFD: i32 = 5;

/// Nanoseconds since the epoch: a clock every prog shares.
fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// The relay of T-messages, from the client on fd 0 to the server on fd
/// 1, each copied first to the side pipe for the parent, which relays the
/// replies and counts. When the client goes, the export is stopped, so
/// the parent sees the end of the conversation.
fn trelay(exporter: u32) -> Result<(), Error> {
    // SAFETY: the parent gave this prog these fds.
    let (mut client, mut server, mut side) = unsafe {
        (
            File::from_raw_fd(0),
            File::from_raw_fd(1),
            File::from_raw_fd(SIDEFD),
        )
    };
    while let Ok(Some(t)) = styxserv::read_msg(&mut client) {
        let mut copy = nanos().to_le_bytes().to_vec();
        copy.extend_from_slice(&t);
        if side.write_all(&copy).is_err() || server.write_all(&t).is_err() {
            break;
        }
    }
    if let Ok(mut ctl) = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/prog/{exporter}/ctl"))
    {
        let _ = ctl.write_all(b"kill");
    }
    Ok(())
}

/// The helper: in a namespace of its own, mount the counting connection
/// on `/`, return to `dot`, and run the command.
fn run(dot: &str, cmd: &[String]) -> Result<(), Error> {
    purgatorio::pctl(pctl::FORKNS, &[])?;
    // The wait file, opened before `/` changes, so waiting for the
    // command needs no request through the connection it is using.
    let wait = File::from(purgatorio::open(
        &format!("/prog/{}/wait", purgatorio::pid()),
        mode::OREAD,
    )?);
    purgatorio::mount(CONNFD, -1, "/", mount::MREPL | mount::MNOAUTH, "")
        .map_err(|e| Error(format!("iostats: can't mount /: {e}")))?;
    // SAFETY: the mount holds its own reference to the connection.
    drop(unsafe { File::from_raw_fd(CONNFD) });
    // Devices whose reads block (a wait file, the console, a pipe) are
    // bound again directly: the export serves one request at a time, and
    // a command waiting for its child there would stop the child too.
    for (dev, at, flags) in [
        ("#c", "/dev", mount::MBEFORE),
        ("#p", "/prog", mount::MREPL),
        ("#s", "/chan", mount::MREPL | mount::MCREATE),
        ("#d", "/fd", mount::MREPL),
        ("#I", "/net", mount::MREPL),
        ("#C", "/cmd", mount::MREPL),
    ] {
        let _ = purgatorio::bind(dev, at, flags);
    }
    purgatorio::chdir(dot).map_err(|e| Error(format!("iostats: can't chdir to {dot}: {e}")))?;
    let pid = purgatorio::spawn(&cmd[0], cmd, 0, &[])?;
    let mut wait = wait;
    let mut buf = vec![0u8; 8192];
    loop {
        let n = wait.read(&mut buf)?;
        let f = purgatorio::unquote_fields(&String::from_utf8_lossy(&buf[..n]));
        if f.first().and_then(|p| p.parse::<u32>().ok()) == Some(pid) {
            let status = f.get(2).cloned().unwrap_or_default();
            if !status.is_empty() {
                purgatorio::exits(&status);
            }
            return Ok(());
        }
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--trelay") {
        let exporter = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);
        return trelay(exporter);
    }
    if args.get(1).map(String::as_str) == Some("--run") && args.len() > 3 {
        return run(&args[2], &args[3..]);
    }
    let mut arg = Arg::new(args, USAGE);
    let (mut debug, mut dbfile) = (false, String::from("iostats.out"));
    while let Some(c) = arg.opt() {
        match c {
            'd' => debug = true,
            'f' => dbfile = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let cmd = arg.argv();
    if cmd.is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let mut log = if debug {
        let f = purgatorio::create(&dbfile, mode::OWRITE, 0o666)
            .map_err(|e| Error(format!("can't create '{dbfile}': {e}")))?;
        Some(File::from(f))
    } else {
        None
    };
    let dot = purgatorio::dot()?;
    let [srv, srvhere] =
        purgatorio::pipe().map_err(|e| Error(format!("can't create pipe: {e}")))?;
    let [mnt, mnthere] =
        purgatorio::pipe().map_err(|e| Error(format!("can't create pipe: {e}")))?;
    let exporter = purgatorio::spawn(
        "/bin/export.wasm",
        &[
            String::from("export"),
            String::from("-A"),
            String::from("/"),
        ],
        pctl::NEWFD,
        &[(srv.as_raw_fd(), 0), (2, 2)],
    )?;
    drop(srv);
    let mut argv = vec![String::from("iostats"), String::from("--run"), dot];
    argv.extend(cmd);
    let helper = purgatorio::spawn(
        "/bin/iostats.wasm",
        &argv,
        pctl::NEWFD,
        &[(mnt.as_raw_fd(), CONNFD), (0, 0), (1, 1), (2, 2)],
    )?;
    drop(mnt);
    let [side, sidehere] =
        purgatorio::pipe().map_err(|e| Error(format!("can't create pipe: {e}")))?;
    purgatorio::spawn(
        "/bin/iostats.wasm",
        &[
            String::from("iostats"),
            String::from("--trelay"),
            exporter.to_string(),
        ],
        pctl::NEWFD,
        &[
            (mnthere.as_raw_fd(), 0),
            (srvhere.as_raw_fd(), 1),
            (side.as_raw_fd(), SIDEFD),
            (2, 2),
        ],
    )?;
    drop(side);
    let mut server = File::from(srvhere);
    let mut client = File::from(mnthere);
    let mut side = File::from(sidehere);
    let mut stats = Stats::default();
    // T-messages sent and not yet answered, by tag, with when they went.
    let mut sent: BTreeMap<u16, (u128, Vec<u8>)> = BTreeMap::new();
    // Until the export ends, which it does when the command's namespace
    // is gone.
    while let Some(r) = styxserv::read_msg(&mut server)? {
        let now = nanos();
        let Ok(rm) = styxserv::decode(&r) else {
            client.write_all(&r)?;
            continue;
        };
        // The T-message went to the side pipe before it went to the
        // server, so it is there by now.
        while !sent.contains_key(&rm.tag) {
            let mut at = [0u8; 16];
            if side.read_exact(&mut at).is_err() {
                break;
            }
            let Some(t) = styxserv::read_msg(&mut side)? else {
                break;
            };
            if let Ok(tm) = styxserv::decode(&t) {
                sent.insert(tm.tag, (u128::from_le_bytes(at), t));
            }
        }
        let Some((at, t)) = sent.remove(&rm.tag) else {
            client.write_all(&r)?;
            continue;
        };
        let Ok(tm) = styxserv::decode(&t) else {
            client.write_all(&r)?;
            continue;
        };
        match stats.fix(&tm, &rm) {
            Some(fixed) => client.write_all(&fixed)?,
            None => client.write_all(&r)?,
        }
        if let Some(l) = log.as_mut() {
            let _ = writeln!(l, "->{}", text::msg(&tm));
            let _ = writeln!(l, "<-{}", text::msg(&rm));
        }
        let took = Duration::from_nanos(now.saturating_sub(at) as u64);
        stats.account(&tm, &rm, t.len(), r.len(), took);
    }
    drop(client);
    drop(server);
    drop(side);
    stats.report(&mut io::stderr().lock())?;
    // The command's exit status is the helper's.
    loop {
        match purgatorio::wait() {
            Ok(w) if w.pid == helper => {
                if !w.status.is_empty() {
                    purgatorio::exits(&w.status);
                }
                return Ok(());
            }
            Ok(_) => {}
            Err(_) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_rates() {
        assert_eq!(join("/", &["a", "b"]), "/a/b");
        assert_eq!(join("/a/b", &["..", "c"]), "/a/c");
        assert_eq!(join("/", &[]), "/");
        assert_eq!(rate(2048, Duration::from_secs(1)), "2");
        assert_eq!(rate(1536, Duration::from_secs(1)), "1.5");
        assert_eq!(rate(1, Duration::ZERO), "0");
    }
}
