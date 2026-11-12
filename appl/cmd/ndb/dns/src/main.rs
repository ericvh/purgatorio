//! ndb/dns [-hrv] [-f dnsfile] [-x net]: the DNS resolver. It serves
//! `net/dns` (default `/net/dns`) and returns once the file is ready; the
//! server runs on in the background.
//!
//! A write of `name [type]` (type `ip`, `ipv6`, `cname`, `mx`, `ns`, `ptr`,
//! `soa`, `txt`, `hinfo` or `all`; default `ip`, or `ptr` for an address)
//! looks the name up; each later read returns one `name type value` line,
//! then end of file. Answers come from the local data first (`dom=` and
//! `ip=` in dnsfile, default `/lib/ndb/local`; `all` asks only these), then
//! from the kernel's translation, which on the emu is the host's resolver
//! (never with `-h`; with `-r`, only after the name servers), then from
//! the name servers the database lists with `dns=` (an address, or
//! `address!port`), asked over UDP with a timeout and one retry. Answers
//! from name servers are kept for their time to live. `-v` reports each
//! query on standard error. There is no recursion of its own: the name
//! servers must be recursive resolvers.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use ndb::dns::{self, Rr};
use ndb::{Entry, daemon};
use purgatorio::arg::Arg;
use purgatorio::dial::{self, cs_line};
use purgatorio::file2chan::Request;
use purgatorio::{Context, Error};

const USAGE: &str = "ndb/dns [-hrv] [-f dnsfile] [-x net]";
const SELF: &str = "/bin/ndb/dns.wasm";
/// How long to wait for one name server's reply, and how often to ask.
const WAIT_MS: u64 = 1500;
const TRIES: usize = 2;

struct Opts {
    nohost: bool,
    servers_first: bool,
    verbose: bool,
    file: String,
    net: String,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut argv = args;
    let serving = daemon::serving(&mut argv);
    let mut arg = Arg::new(argv.clone(), USAGE);
    let mut opts = Opts {
        nohost: false,
        servers_first: false,
        verbose: false,
        file: String::from("/lib/ndb/local"),
        net: String::from("/net"),
    };
    while let Some(c) = arg.opt() {
        match c {
            'h' => opts.nohost = true,
            'r' => opts.servers_first = true,
            'v' => opts.verbose = true,
            'f' => opts.file = arg.earg()?,
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
    daemon::start(SELF, &argv, opts.verbose)?;
    Ok(())
}

fn serve(opts: Opts) -> Result<(), Error> {
    let kernel = open_rw("#I/cs").ok();
    let mut d = Dns {
        db: Vec::new(),
        kernel,
        opts,
        replies: HashMap::new(),
        cache: HashMap::new(),
        id: (purgatorio::pid() as u16).wrapping_mul(7919),
        convs: HashMap::new(),
    };
    d.load();
    let io = match daemon::file2chan(&d.opts.net, "dns") {
        Ok(io) => io,
        Err(e) => {
            let msg = format!("can't make {}/dns: {e}", d.opts.net);
            daemon::ready(Err(&msg));
            return Err(Error(msg));
        }
    };
    daemon::ready(Ok(()));
    while let Some(req) = io.recv()? {
        d.handle(&req)?;
    }
    Ok(())
}

struct Dns {
    db: Vec<Entry>,
    kernel: Option<File>,
    opts: Opts,
    replies: HashMap<u32, VecDeque<String>>,
    /// Answers from name servers: (name, type) to the lines and when they
    /// expire.
    cache: HashMap<(String, u16), (Vec<String>, Instant)>,
    id: u16,
    /// One UDP conversation per name server, kept: `#I` gives a
    /// conversation's port back only when it is hung up.
    convs: HashMap<String, dial::Connection>,
}

impl Dns {
    fn load(&mut self) {
        let read = |f: &str| ndb::read_std(f, &purgatorio::errstr);
        self.db = ndb::open(&self.opts.file, &read).unwrap_or_else(|e| {
            eprintln!("ndb/dns: warning: {e}");
            Vec::new()
        });
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
            self.cache.clear();
            Ok(Vec::new())
        } else {
            self.lookup(&text)
        };
        if self.opts.verbose {
            match &got {
                Ok(l) => eprintln!("ndb/dns: {text} -> {} answers", l.len()),
                Err(e) => eprintln!("ndb/dns: {text}: {e}"),
            }
        }
        match got {
            Ok(lines) => {
                self.replies.insert(req.fid, lines.into());
                req.respond_write(req.count())
            }
            Err(e) => {
                self.replies.remove(&req.fid);
                req.respond_err(&e)
            }
        }
    }

    fn lookup(&mut self, q: &str) -> Result<Vec<String>, String> {
        let f: Vec<&str> = q.split_whitespace().collect();
        let (name, ty) = match f.as_slice() {
            [n] if is_addr(n) => (*n, "ptr"),
            [n] => (*n, "ip"),
            [n, t] => (*n, *t),
            _ => return Err(String::from("bad query")),
        };
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        if ty == "all" {
            let local = self.local(&name, None);
            return if local.is_empty() {
                Err(String::from("dns: name does not exist"))
            } else {
                Ok(local)
            };
        }
        let qt = dns::qtype(ty).ok_or_else(|| String::from("dns: unknown type"))?;
        let local = self.local(&name, Some(ty));
        if !local.is_empty() {
            return Ok(local);
        }
        let servers = self.servers();
        let host_first = !self.opts.nohost && !(self.opts.servers_first && !servers.is_empty());
        if host_first
            && ty == "ip"
            && let Some(ip) = self.kernel(&name)
        {
            return Ok(vec![format!("{name} ip {ip}\n")]);
        }
        if servers.is_empty() {
            return Err(String::from("dns: no name servers"));
        }
        let qname = if ty == "ptr" {
            dns::arpa(&name).unwrap_or(name.clone())
        } else {
            name.clone()
        };
        let got = self.remote(&servers, &qname, qt);
        // With -r the host's resolver comes after the name servers.
        if got.is_err()
            && !self.opts.nohost
            && !host_first
            && ty == "ip"
            && let Some(ip) = self.kernel(&name)
        {
            return Ok(vec![format!("{name} ip {ip}\n")]);
        }
        got
    }

    /// Answers from the local data. `None` asks for everything.
    fn local(&self, name: &str, ty: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        for e in &self.db {
            if is_addr(name) {
                if e.has("ip", Some(name)) && ty.is_none_or(|t| t == "ptr") {
                    for d in e.all("dom") {
                        out.push(format!("{name} ptr {d}\n"));
                    }
                }
            } else if e.has("dom", Some(name)) {
                match ty {
                    None => {
                        for p in e.pairs().filter(|p| p.attr != "dom") {
                            out.push(format!("{name} {} {}\n", p.attr, p.val));
                        }
                    }
                    Some("ip") => {
                        for ip in e.all("ip").into_iter().filter(|ip| ip.contains('.')) {
                            out.push(format!("{name} ip {ip}\n"));
                        }
                    }
                    Some("ipv6") => {
                        for ip in e.all("ip").into_iter().filter(|ip| ip.contains(':')) {
                            out.push(format!("{name} ipv6 {ip}\n"));
                        }
                    }
                    Some(_) => {}
                }
            }
        }
        out
    }

    /// The name servers the database lists, as `udp!addr!port`.
    fn servers(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let nets = self.db.iter().filter(|e| e.has("ipnet", None));
        let rest = self.db.iter().filter(|e| !e.has("ipnet", None));
        for e in nets.chain(rest) {
            for s in e.all("dns") {
                let a = match s.split_once('!') {
                    Some((h, p)) => format!("udp!{h}!{p}"),
                    None => format!("udp!{s}!53"),
                };
                if !out.contains(&a) {
                    out.push(a);
                }
            }
        }
        out
    }

    /// The kernel's translation of `name`.
    fn kernel(&mut self, name: &str) -> Option<String> {
        let f = self.kernel.as_mut()?;
        f.seek(SeekFrom::Start(0)).ok()?;
        f.write_all(format!("tcp!{name}!1").as_bytes()).ok()?;
        f.seek(SeekFrom::Start(0)).ok()?;
        let mut text = String::new();
        f.read_to_string(&mut text).ok()?;
        let t = cs_line(text.lines().next()?)?;
        let host = t.split('!').nth(1)?.to_string();
        is_addr(&host).then_some(host)
    }

    /// Asks the name servers in turn.
    fn remote(&mut self, servers: &[String], qname: &str, qt: u16) -> Result<Vec<String>, String> {
        let key = (qname.to_string(), qt);
        if let Some((lines, until)) = self.cache.get(&key) {
            if Instant::now() < *until {
                return Ok(lines.clone());
            }
            self.cache.remove(&key);
        }
        let mut last = String::from("dns: no response from name server");
        for s in servers {
            for _ in 0..TRIES {
                self.id = self.id.wrapping_add(1);
                if !self.convs.contains_key(s) {
                    match dial::dial_translated(s) {
                        Ok(c) => {
                            self.convs.insert(s.clone(), c);
                        }
                        Err(e) => {
                            last = format!("dns: {e}");
                            break;
                        }
                    }
                }
                let reply = match ask(&self.convs[s], self.id, qname, qt) {
                    Ok(Some(r)) => r,
                    Ok(None) => continue,
                    Err(e) => {
                        last = format!("dns: {e}");
                        self.convs.remove(s);
                        break;
                    }
                };
                if reply.rcode == dns::RCODE_NXDOMAIN {
                    return Err(String::from("dns: name does not exist"));
                }
                if reply.rcode != 0 {
                    last = format!("dns: server failure (rcode {})", reply.rcode);
                    break;
                }
                let lines = answer_lines(&reply.answers, qt);
                if lines.is_empty() {
                    return Err(String::from("dns: resource does not exist"));
                }
                let ttl = reply.answers.iter().map(|a| a.ttl).min().unwrap_or(0);
                let until = Instant::now() + Duration::from_secs(u64::from(ttl.min(86400)));
                self.cache.insert(key, (lines.clone(), until));
                return Ok(lines);
            }
        }
        Err(last)
    }
}

/// The answer lines for a query of type `qt`: records of that type, and a
/// CNAME when that is all there is.
fn answer_lines(answers: &[Rr], qt: u16) -> Vec<String> {
    let want: Vec<&Rr> = answers.iter().filter(|a| a.rtype == qt).collect();
    let pick = if want.is_empty() {
        answers.iter().filter(|a| a.rtype == 5).collect()
    } else {
        want
    };
    pick.iter()
        .filter_map(|a| {
            let t = dns::type_name(a.rtype)?;
            Some(format!("{} {t} {}\n", a.name, a.data))
        })
        .collect()
}

/// One exchange with `server`. `Ok(None)` when no reply came in time.
fn ask(
    conn: &dial::Connection,
    id: u16,
    qname: &str,
    qt: u16,
) -> Result<Option<dns::Reply>, String> {
    let data = conn.data.as_ref().ok_or("no connection")?;
    let fd = data.as_raw_fd();
    let mut f = std::mem::ManuallyDrop::new(file_of(fd));
    f.write_all(&dns::query(id, qname, qt))
        .map_err(|e| e.to_string())?;
    // A reply to an earlier try may come first.
    for _ in 0..4 {
        let Some(buf) = dial::read_timeout(fd, 4096, WAIT_MS).map_err(|e| e.0)? else {
            return Ok(None);
        };
        if let Some(r) = dns::parse(&buf)
            && r.id == id
        {
            return Ok(Some(r));
        }
    }
    Ok(None)
}

fn file_of(fd: i32) -> File {
    use std::os::fd::FromRawFd;
    // SAFETY: the caller keeps the descriptor open and does not drop this.
    unsafe { File::from_raw_fd(fd) }
}

/// A literal IPv4 or IPv6 address.
fn is_addr(s: &str) -> bool {
    (s.contains('.') && s.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
        || (s.contains(':') && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b':'))
}

fn open_rw(path: &str) -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}
