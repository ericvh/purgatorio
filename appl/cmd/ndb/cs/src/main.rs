//! ndb/cs [-v] [-f database] [-x net]: the connection server. It serves
//! `net/cs` (default `/net/cs`) with file2chan and returns once the file is
//! ready; the server runs on in the background.
//!
//! A write of `net!host!service` is translated with the network database
//! (default `/lib/ndb/local`, then `/lib/ndb/common` for well-known
//! services unless local names its own `database=` files): each later read
//! returns one `/net/tcp/clone 10.0.0.2!6666` line, then end of file.
//! `host` may be numeric, `*` (the reply then has no address), a `sys=` or
//! `dom=` name, or `$attr`, the value of `attr` in this host's entry, an
//! `ipnet` entry or the `infernosite` entry. A name the database lacks is
//! asked of `/net/dns` when an `ndb/dns` serves it, and then of the
//! kernel's own translation, which stays underneath as the fallback (on the
//! emu it reaches the host's resolver). A network `net` means `tcp`.
//!
//! A write of `!attr=value ...` (a value of `*` matches anything) returns
//! the matching entries instead, one per read, in attrdb format. A write
//! of `refresh` rereads the database. `-v` reports each translation on
//! standard error.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use ndb::{Entry, daemon};
use purgatorio::arg::Arg;
use purgatorio::dial::cs_line;
use purgatorio::file2chan::{self, Request};
use purgatorio::{Context, Error};

const USAGE: &str = "ndb/cs [-v] [-f database] [-x net]";
const SELF: &str = "/bin/ndb/cs.wasm";

struct Opts {
    verbose: bool,
    file: Option<String>,
    net: String,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut argv = args;
    let serving = daemon::serving(&mut argv);
    let mut arg = Arg::new(argv.clone(), USAGE);
    let mut opts = Opts {
        verbose: false,
        file: None,
        net: String::from("/net"),
    };
    while let Some(c) = arg.opt() {
        match c {
            'v' | 'D' => opts.verbose = true,
            'f' => opts.file = Some(arg.earg()?),
            'x' => opts.net = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    if serving {
        return serve(opts);
    }
    // Start the server and wait for it to say the file is there.
    daemon::start(SELF, &argv, opts.verbose)?;
    Ok(())
}

fn serve(opts: Opts) -> Result<(), Error> {
    // The kernel's translation, which ours covers.
    let kernel = open_rw("#I/cs")
        .or_else(|_| open_rw(&format!("{}/cs", opts.net)))
        .ok();
    let mut cs = Cs {
        db: Vec::new(),
        kernel,
        opts,
        replies: HashMap::new(),
    };
    cs.load();
    let io = match file2chan::file2chan(&cs.opts.net, "cs") {
        Ok(io) => io,
        Err(e) => {
            let msg = format!("can't make {}/cs: {e}", cs.opts.net);
            daemon::ready(Err(&msg));
            return Err(Error(msg));
        }
    };
    daemon::ready(Ok(()));
    while let Some(req) = io.recv()? {
        cs.handle(&req)?;
    }
    Ok(())
}

struct Cs {
    db: Vec<Entry>,
    kernel: Option<File>,
    opts: Opts,
    /// Replies not yet read, by fid.
    replies: HashMap<u32, VecDeque<String>>,
}

impl Cs {
    fn load(&mut self) {
        let read = |f: &str| ndb::read_std(f, &purgatorio::errstr);
        let file = self.opts.file.as_deref().unwrap_or("/lib/ndb/local");
        let mut db = match ndb::open(file, &read) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("ndb/cs: warning: {e}");
                Vec::new()
            }
        };
        if self.opts.file.is_none()
            && ndb::files(&db).is_empty()
            && let Ok(common) = read("/lib/ndb/common").and_then(|t| ndb::parse(&t, "common"))
        {
            db.extend(common);
        }
        self.db = db;
    }

    fn handle(&mut self, req: &Request) -> Result<(), Error> {
        if req.is_trunc() {
            return req.respond_write(0);
        }
        if req.is_read() {
            let line = self
                .replies
                .get_mut(&req.fid)
                .and_then(VecDeque::pop_front)
                .unwrap_or_default();
            return req.respond_read(line.as_bytes());
        }
        let text = String::from_utf8_lossy(req.data()).trim().to_string();
        let got = if text == "refresh" {
            self.load();
            Ok(Vec::new())
        } else if let Some(q) = text.strip_prefix('!') {
            self.query(q)
        } else {
            self.translate(&text)
        };
        match got {
            Ok(lines) => {
                self.replies.insert(req.fid, lines.into());
                req.respond_write(req.count())
            }
            Err(e) => {
                self.replies.remove(&req.fid);
                if self.opts.verbose {
                    eprintln!("ndb/cs: {text}: {e}");
                }
                req.respond_err(&e)
            }
        }
    }

    /// `!attr=value ...`: the matching entries.
    fn query(&self, q: &str) -> Result<Vec<String>, String> {
        let mut pairs = Vec::new();
        for w in ndb::words(q) {
            let (a, v) = w.split_once('=').unwrap_or((w.as_str(), "*"));
            if a.is_empty() {
                return Err(String::from("bad query"));
            }
            pairs.push((a.to_string(), v.to_string()));
        }
        if pairs.is_empty() {
            return Err(String::from("bad query"));
        }
        let out: Vec<String> = self
            .db
            .iter()
            .filter(|e| e.matches(&pairs))
            .map(Entry::text)
            .collect();
        if out.is_empty() {
            Err(String::from("no match"))
        } else {
            Ok(out)
        }
    }

    fn translate(&mut self, addr: &str) -> Result<Vec<String>, String> {
        let parts: Vec<&str> = addr.split('!').collect();
        let (net, host, svc) = match parts.as_slice() {
            [host] => ("net", *host, ""),
            [net, host] => (*net, *host, ""),
            [net, host, svc, ..] => (*net, *host, *svc),
            _ => ("", "", ""),
        };
        if host.is_empty() {
            return Err(String::from("bad format request"));
        }
        let net = if net == "net" || net.is_empty() {
            "tcp"
        } else {
            net
        };
        let clone = format!("{}/{net}/clone", self.opts.net);
        if purgatorio::stat(&clone).is_err() {
            return Err(format!("network unavailable {net}"));
        }
        let port = self.service(net, svc)?;
        let ips = if host == "*" {
            Vec::new()
        } else {
            self.host(host, net, port.as_deref(), 0)?
        };
        let lines: Vec<String> = match (ips.is_empty(), &port) {
            (true, Some(p)) => vec![format!("{clone} {p}\n")],
            (true, None) => vec![format!("{clone} *\n")],
            (false, Some(p)) => ips.iter().map(|ip| format!("{clone} {ip}!{p}\n")).collect(),
            (false, None) => ips.iter().map(|ip| format!("{clone} {ip}\n")).collect(),
        };
        if self.opts.verbose {
            eprintln!("ndb/cs: {net}!{host}!{svc} -> {}", lines[0].trim_end());
        }
        Ok(lines)
    }

    /// The port for `svc` on `net`: numeric, or from a `net=svc port=n`
    /// entry. `None` when there is no service.
    fn service(&self, net: &str, svc: &str) -> Result<Option<String>, String> {
        if svc.is_empty() {
            return Ok(None);
        }
        if svc == "*" || svc.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(Some(svc.to_string()));
        }
        self.db
            .iter()
            .find(|e| e.has(net, Some(svc)))
            .and_then(|e| {
                e.near(net, Some(svc), "port")
                    .first()
                    .map(|p| p.to_string())
            })
            .map(Some)
            .ok_or_else(|| String::from("cs: can't translate service"))
    }

    /// The addresses of `host`.
    fn host(
        &mut self,
        host: &str,
        net: &str,
        port: Option<&str>,
        depth: u32,
    ) -> Result<Vec<String>, String> {
        if let Some(attr) = host.strip_prefix('$') {
            let name = self
                .server(attr)
                .ok_or_else(|| format!("cs: can't translate {host}"))?;
            if depth > 4 {
                return Err(format!("cs: can't translate {host}"));
            }
            return self.host(&name, net, port, depth + 1);
        }
        if numeric(host) {
            return Ok(vec![host.to_string()]);
        }
        let mut ips: Vec<String> = Vec::new();
        for e in &self.db {
            if e.has("sys", Some(host)) || e.has("dom", Some(host)) {
                for ip in e.all("ip") {
                    if !ips.iter().any(|i| i == ip) {
                        ips.push(ip.to_string());
                    }
                }
            }
        }
        if !ips.is_empty() {
            return Ok(ips);
        }
        if let Some(ips) = self.dns(host) {
            return Ok(ips);
        }
        if let Some(ip) = self.kernel(net, host, port) {
            return Ok(vec![ip]);
        }
        Err(String::from("cs: unknown host"))
    }

    /// `$attr`: from this host's entry, then an `ipnet` entry, then the
    /// `infernosite` entry.
    fn server(&self, attr: &str) -> Option<String> {
        let me = std::fs::read_to_string("/dev/sysname").unwrap_or_default();
        let me = me.trim();
        let mine = self.db.iter().filter(|e| e.has("sys", Some(me)));
        let nets = self.db.iter().filter(|e| e.has("ipnet", None));
        let site = self.db.iter().filter(|e| e.has("infernosite", None));
        mine.chain(nets)
            .chain(site)
            .find_map(|e| e.get(attr))
            .filter(|v| !v.is_empty())
            .map(String::from)
    }

    /// Asks `/net/dns`, if a resolver serves it.
    fn dns(&self, host: &str) -> Option<Vec<String>> {
        let mut f = open_rw(&format!("{}/dns", self.opts.net)).ok()?;
        f.write_all(format!("{host} ip").as_bytes()).ok()?;
        f.seek(SeekFrom::Start(0)).ok()?;
        let mut text = String::new();
        f.read_to_string(&mut text).ok()?;
        let ips: Vec<String> = text
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                match f.as_slice() {
                    [_, "ip", v] => Some(v.to_string()),
                    _ => {
                        eprintln!("ndb/cs: odd result from dns: {l}");
                        None
                    }
                }
            })
            .collect();
        (!ips.is_empty()).then_some(ips)
    }

    /// The kernel's translation of `host`.
    fn kernel(&mut self, net: &str, host: &str, port: Option<&str>) -> Option<String> {
        let f = self.kernel.as_mut()?;
        let port = port.filter(|p| p.bytes().all(|b| b.is_ascii_digit()));
        f.seek(SeekFrom::Start(0)).ok()?;
        f.write_all(format!("{net}!{host}!{}", port.unwrap_or("1")).as_bytes())
            .ok()?;
        f.seek(SeekFrom::Start(0)).ok()?;
        let mut text = String::new();
        f.read_to_string(&mut text).ok()?;
        let t = cs_line(text.lines().next()?)?;
        let host = t.split('!').nth(1)?;
        (numeric(host) && !host.is_empty()).then(|| host.to_string())
    }
}

/// A literal IPv4 or IPv6 address.
fn numeric(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    let v4 = host.contains('.') && host.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    let v6 = host.contains(':')
        && host
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'.' || b == b':');
    v4 || v6
}

fn open_rw(path: &str) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}
