//! test/netsrv kind: small loopback servers for the sh corpus, so the
//! network commands can be tested with no outside network.
//!
//! `netsrv dns|ntp|http|whois|telnet` starts that server on a free
//! loopback port in the background and prints `port pid`. The DNS server
//! knows a few names under `example.test` and never answers
//! `silent.example.test`; the NTP server always says Mon Nov 09 2026
//! 10:15:00 GMT; the HTTP server serves `/hello.txt`, `/index.html`,
//! `/moved` (a redirect), `/chunked` (HTTP/1.1 chunked) and `/headers`
//! (the request echoed); whois echoes the query; telnet negotiates, asks
//! for a name and greets it.
//!
//! `netsrv tftp get|put addr file [text]` is a TFTP client: `get` prints
//! the file, `put` stores `text` there.
//!
//! `netsrv virgil addr query` sends a virgil query (`user?machine`) to
//! `ip/virgild` at `addr` and prints the answer, or `no answer`.
//!
//! `netsrv smtp log` starts an SMTP server that appends each command and
//! each line of message data it gets, but not `QUIT`, to `log`, and
//! refuses recipients named `nobody@...`.
//!
//! `netsrv bootp addr ether [discover|request|inform [ip]]` sends a BOOTP
//! request (with a DHCP message type, a DHCP one) for the hardware
//! address `ether` to `ip/bootpd` at `addr` and prints the reply.
//!
//! `netsrv rip addr version route ...` sends one RIP response to `ip/rip`
//! at `addr`, a route being `dest/mask/nexthop/metric`.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};

use ndb::daemon;
use ndb::dns::{self, put_name};
use purgatorio::dial::{self, Connection};
use purgatorio::{Context, Error};

const SELF: &str = "/bin/test/netsrv.wasm";
const USAGE: &str = "usage: netsrv dns|ntp|http|whois|telnet | netsrv smtp log | netsrv tftp get|put addr file [text] | netsrv virgil addr query | netsrv bootp addr ether [type [ip]] | netsrv rip addr version route ...";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut argv = args;
    let serving = daemon::serving(&mut argv);
    let kind = argv.get(1).cloned().ok_or_else(|| Error::from(USAGE))?;
    if kind == "tftp" {
        return tftp(&argv[2..]);
    }
    if kind == "virgil" {
        return virgil(&argv[2..]);
    }
    if kind == "bootp" {
        return bootp(&argv[2..]);
    }
    if kind == "rip" {
        return rip(&argv[2..]);
    }
    let extra: Vec<String> = argv[2..].to_vec();
    let proto = match kind.as_str() {
        "dns" | "ntp" => "udp",
        "http" | "whois" | "telnet" => "tcp",
        "smtp" if extra.len() == 1 => "tcp",
        _ => return Err(Error::from(USAGE)),
    };
    if !serving {
        let [r, w] = purgatorio::pipe()?;
        let mut child = vec![
            String::from("netsrv"),
            String::from(daemon::SERVE),
            kind.clone(),
        ];
        child.extend(extra.iter().cloned());
        let pid = purgatorio::spawn(
            SELF,
            &child,
            purgatorio::abi::pctl::NEWFD,
            &[(w.as_raw_fd(), 1)],
        )?;
        drop(w);
        let mut buf = [0u8; 64];
        let n = File::from(r).read(&mut buf)?;
        let port = String::from_utf8_lossy(&buf[..n]).into_owned();
        let port = port
            .strip_prefix('R')
            .ok_or_else(|| Error(format!("netsrv: {port}")))?;
        println!("{port} {pid}");
        return Ok(());
    }
    let ann = match dial::announce(&format!("{proto}!127.0.0.1!0")) {
        Ok(a) => a,
        Err(e) => {
            daemon::ready(Err(&e.0));
            return Err(e);
        }
    };
    let local = dial::local(&ann.dir)?;
    let port = local.rsplit('!').next().unwrap_or("0").to_string();
    let _ = purgatorio::file2chan::write_fd(1, format!("R{port}").as_bytes());
    loop {
        let mut conn = dial::listen(&ann.dir)?;
        let mut f = data(&mut conn)?;
        let _ = match kind.as_str() {
            "dns" => dns_reply(&mut f),
            "ntp" => ntp_reply(&mut f),
            "http" => http(&mut f),
            "whois" => whois(&mut f),
            "smtp" => smtp(&mut f, &extra[0]),
            _ => telnet(&mut f),
        };
        drop(f);
        let mut ctl = File::from(conn.ctl);
        let _ = ctl.write_all(b"hangup");
    }
}

fn data(conn: &mut Connection) -> Result<File, Error> {
    let fd: OwnedFd = conn.data.take().ok_or_else(|| Error::from("no data"))?;
    Ok(File::from(fd))
}

fn dns_reply(f: &mut File) -> std::io::Result<()> {
    let mut q = vec![0u8; 512];
    let n = f.read(&mut q)?;
    q.truncate(n);
    let Some((_, name, qt)) = dns::question(&q) else {
        return Ok(());
    };
    let name = name.to_ascii_lowercase();
    let a = |s: &str, ip: [u8; 4]| (String::from(s), 1u16, ip.to_vec());
    let named = |s: &str, t: u16, target: &str| {
        let mut d = Vec::new();
        put_name(&mut d, target);
        (String::from(s), t, d)
    };
    let mut rcode = 0;
    let answers = match (name.as_str(), qt) {
        ("silent.example.test", _) => return Ok(()),
        ("www.example.test", 1) => vec![
            a("www.example.test", [10, 1, 2, 3]),
            a("www.example.test", [10, 1, 2, 4]),
        ],
        ("alias.example.test", 1 | 5) => {
            let mut v = vec![named("alias.example.test", 5, "www.example.test")];
            if qt == 1 {
                v.push(a("www.example.test", [10, 1, 2, 3]));
            }
            v
        }
        ("mail.example.test", 1) => vec![a("mail.example.test", [10, 1, 2, 5])],
        ("example.test", 15) => {
            let mut d = 10u16.to_be_bytes().to_vec();
            put_name(&mut d, "mail.example.test");
            vec![(String::from("example.test"), 15, d)]
        }
        ("example.test", 2) => vec![named("example.test", 2, "ns.example.test")],
        ("example.test", 16) => vec![(
            String::from("example.test"),
            16,
            b"\x0bhello world".to_vec(),
        )],
        ("3.2.1.10.in-addr.arpa", 12) => {
            vec![named("3.2.1.10.in-addr.arpa", 12, "www.example.test")]
        }
        ("www.example.test" | "mail.example.test" | "example.test", _) => Vec::new(),
        _ => {
            rcode = dns::RCODE_NXDOMAIN;
            Vec::new()
        }
    };
    if let Some(r) = dns::reply(&q, rcode, &answers) {
        f.write_all(&r)?;
    }
    Ok(())
}

/// Seconds from 1900 to 1970, and the time the server gives.
const NTP_EPOCH: u64 = 2_208_988_800;
const NOW: u64 = 1_794_219_300;

fn ntp_reply(f: &mut File) -> std::io::Result<()> {
    let mut q = [0u8; 48];
    let n = f.read(&mut q)?;
    if n < 48 {
        return Ok(());
    }
    let mut r = [0u8; 48];
    r[0] = 0x24; // version 4, server
    r[1] = 1; // stratum
    r[12..16].copy_from_slice(b"TEST");
    let t = ((NOW + NTP_EPOCH) as u32).to_be_bytes();
    // The client's transmit time is our originate time.
    r[24..32].copy_from_slice(&q[40..48]);
    for at in [16, 32, 40] {
        r[at..at + 4].copy_from_slice(&t);
    }
    f.write_all(&r)
}

/// Reads up to the blank line ending an HTTP request or a telnet line.
fn read_until(f: &mut File, end: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut got = Vec::new();
    let mut b = [0u8; 512];
    while !got.windows(end.len()).any(|w| w == end) {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        got.extend_from_slice(&b[..n]);
    }
    Ok(got)
}

fn http(f: &mut File) -> std::io::Result<()> {
    let req = read_until(f, b"\r\n\r\n")?;
    let text = String::from_utf8_lossy(&req).into_owned();
    let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
    let ok = |ty: &str, body: &str| {
        format!(
            "HTTP/1.0 200 OK\r\nContent-Type: {ty}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let resp = match path.as_str() {
        "/hello.txt" => ok("text/plain", "hello, world\n"),
        "/" | "/index.html" => ok("text/html", "<html><body>hello</body></html>\n"),
        "/headers" => ok("text/plain", &text.replace("\r\n", "\n")),
        "/moved" => {
            String::from("HTTP/1.0 302 Found\r\nLocation: /hello.txt\r\nContent-Length: 0\r\n\r\n")
        }
        "/chunked" => String::from(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\n\r\n\
             6\r\nchunk \r\n7\r\nby chun\r\n3\r\nk\n\n\r\n0\r\n\r\n",
        ),
        _ => String::from("HTTP/1.0 404 Not Found\r\nContent-Length: 10\r\n\r\nnot found\n"),
    };
    f.write_all(resp.as_bytes())
}

fn whois(f: &mut File) -> std::io::Result<()> {
    let q = read_until(f, b"\r\n")?;
    let q = String::from_utf8_lossy(&q).trim().to_string();
    write!(f, "% test whois server\r\nquery: {q}\r\nstatus: active\r\n")
}

const IAC: u8 = 255;
const WILL: u8 = 251;
const DO: u8 = 253;

fn telnet(f: &mut File) -> std::io::Result<()> {
    // Offer to echo and to suppress go-ahead, ask for the terminal type.
    f.write_all(&[IAC, WILL, 1, IAC, WILL, 3, IAC, DO, 24])?;
    f.write_all(b"name? ")?;
    let mut got = Vec::new();
    let mut b = [0u8; 256];
    let mut replies = Vec::new();
    // The client's answers to the options come first, then its line.
    while !got.contains(&b'\n') || replies.len() < 3 {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        let mut i = 0;
        while i < n {
            if b[i] == IAC && i + 2 < n {
                replies.push(format!("{} {}", b[i + 1], b[i + 2]));
                i += 3;
            } else {
                got.push(b[i]);
                i += 1;
            }
        }
    }
    let name = String::from_utf8_lossy(&got).trim().to_string();
    write!(
        f,
        "\r\nhello {name}\r\noptions answered: {}\r\n",
        replies.join(", ")
    )
}

/// The TFTP client.
/// A virgil client: one query to `addr`, a few tries, and the answer.
fn virgil(args: &[String]) -> Result<(), Error> {
    let [addr, query] = args else {
        return Err(Error::from(USAGE));
    };
    let mut conn = dial::dial(addr)?;
    let mut f = data(&mut conn)?;
    let fd = f.as_raw_fd();
    for _ in 0..3 {
        f.write_all(query.as_bytes())?;
        if let Some(r) = dial::read_timeout(fd, 512, 1000)? {
            println!("{}", String::from_utf8_lossy(&r));
            return Ok(());
        }
    }
    println!("no answer");
    Ok(())
}

fn tftp(args: &[String]) -> Result<(), Error> {
    let (op, addr, file) = match args {
        [op, addr, file, ..] => (op.as_str(), addr, file),
        _ => return Err(Error::from(USAGE)),
    };
    let mut conn = dial::dial(addr)?;
    let mut f = data(&mut conn)?;
    let fd = f.as_raw_fd();
    let opcode: u8 = if op == "get" { 1 } else { 2 };
    let mut req = vec![0, opcode];
    req.extend_from_slice(file.as_bytes());
    req.push(0);
    req.extend_from_slice(b"octet\0");
    let recv = |what: &str| -> Result<Vec<u8>, Error> {
        dial::read_timeout(fd, 1024, 3000)?
            .ok_or_else(|| Error(format!("tftp: timed out on {what}")))
    };
    // The request is sent again until the server answers: it may still be
    // starting.
    let mut first = None;
    for _ in 0..5 {
        f.write_all(&req)?;
        if let Some(p) = dial::read_timeout(fd, 1024, 1000)? {
            first = Some(p);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let mut first = Some(first.ok_or_else(|| Error::from("tftp: no answer"))?);
    if op == "get" {
        let mut out = Vec::new();
        let mut want: u16 = 1;
        loop {
            let p = match first.take() {
                Some(p) => p,
                None => recv("data")?,
            };
            if p.len() >= 4 && p[1] == 5 {
                let msg = String::from_utf8_lossy(&p[4..]);
                return Err(Error(format!("tftp: {}", msg.trim_end_matches('\0'))));
            }
            if p.len() < 4 || p[1] != 3 {
                return Err(Error::from("tftp: bad packet"));
            }
            let block = u16::from_be_bytes([p[2], p[3]]);
            f.write_all(&[0, 4, p[2], p[3]])?;
            if block == want {
                out.extend_from_slice(&p[4..]);
                want = want.wrapping_add(1);
                if p.len() < 516 {
                    break;
                }
            }
        }
        std::io::stdout().write_all(&out)?;
        return Ok(());
    }
    let text = args[3..].join(" ") + "\n";
    let p = first.take().unwrap_or_default();
    if p.len() < 4 || p[1] != 4 {
        let msg = String::from_utf8_lossy(p.get(4..).unwrap_or(&[]));
        return Err(Error(format!("tftp: {}", msg.trim_end_matches('\0'))));
    }
    let mut pkt = vec![0, 3, 0, 1];
    pkt.extend_from_slice(text.as_bytes());
    f.write_all(&pkt)?;
    let p = recv("ack")?;
    if p.len() < 4 || p[1] != 4 || p[3] != 1 {
        return Err(Error::from("tftp: no ack"));
    }
    println!("sent {} bytes", text.len());
    Ok(())
}

/// The SMTP server: every command but `QUIT`, and every data line, is
/// appended to `log` before the reply is sent.
fn smtp(f: &mut File, log: &str) -> std::io::Result<()> {
    let mut out = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log)?;
    f.write_all(b"220 netsrv SMTP\r\n")?;
    let mut data = false;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let line = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => buf.drain(..=i).collect::<Vec<u8>>(),
            None => {
                let mut b = [0u8; 512];
                match f.read(&mut b)? {
                    0 => return Ok(()),
                    n => buf.extend_from_slice(&b[..n]),
                }
                continue;
            }
        };
        let line = String::from_utf8_lossy(&line).into_owned();
        let l = line.trim_end_matches('\n').trim_end_matches('\r');
        if data {
            writeln!(out, "| {l}")?;
            if l == "." {
                data = false;
                f.write_all(b"250 accepted\r\n")?;
            }
            continue;
        }
        let verb = l
            .split([' ', ':'])
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if verb == "QUIT" {
            f.write_all(b"221 bye\r\n")?;
            return Ok(());
        }
        writeln!(out, "{l}")?;
        let reply: &[u8] = match verb.as_str() {
            "DATA" => {
                data = true;
                b"354 go ahead\r\n"
            }
            "RCPT" if l.to_ascii_lowercase().contains("<nobody@") => b"550 no such user\r\n",
            _ => b"250 ok\r\n",
        };
        f.write_all(reply)?;
    }
}

fn parse_ip(s: &str) -> Result<[u8; 4], Error> {
    let v: Vec<u8> = s.split('.').filter_map(|p| p.parse().ok()).collect();
    <[u8; 4]>::try_from(v).map_err(|_| Error(format!("bad address {s}")))
}

fn ip_text(a: &[u8]) -> String {
    let v: Vec<String> = a.iter().map(u8::to_string).collect();
    v.join(".")
}

/// A BOOTP or DHCP client: one request, a few tries, and the reply.
fn bootp(args: &[String]) -> Result<(), Error> {
    let (addr, ether, kind, want) = match args {
        [a, e] => (a, e, None, None),
        [a, e, k] => (a, e, Some(k.as_str()), None),
        [a, e, k, ip] => (a, e, Some(k.as_str()), Some(parse_ip(ip)?)),
        _ => return Err(Error::from(USAGE)),
    };
    let ha: Vec<u8> = (0..ether.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(ether.get(i..i + 2)?, 16).ok())
        .collect();
    if ha.len() != 6 {
        return Err(Error(format!("bad ether {ether}")));
    }
    let mut req = vec![0u8; 300];
    req[0] = 1;
    req[1] = 1;
    req[2] = 6;
    req[4..8].copy_from_slice(&0x1234_5678u32.to_be_bytes());
    req[28..34].copy_from_slice(&ha);
    if let Some(k) = kind {
        let t = match k {
            "discover" => 1,
            "request" => 3,
            "inform" => 8,
            _ => return Err(Error::from(USAGE)),
        };
        let mut o = vec![99, 130, 83, 99, 53, 1, t];
        if let Some(ip) = want {
            if t == 8 {
                req[12..16].copy_from_slice(&ip);
            } else {
                o.extend_from_slice(&[50, 4]);
                o.extend_from_slice(&ip);
            }
        }
        o.push(255);
        req[236..236 + o.len()].copy_from_slice(&o);
    }
    let mut conn = dial::dial(addr)?;
    let mut f = data(&mut conn)?;
    let fd = f.as_raw_fd();
    let mut reply = None;
    for _ in 0..3 {
        f.write_all(&req)?;
        if let Some(r) = dial::read_timeout(fd, 1024, 1000)? {
            reply = Some(r);
            break;
        }
    }
    let Some(r) = reply else {
        println!("no answer");
        return Ok(());
    };
    if r.len() < 240 || r[0] != 2 {
        println!("bad reply");
        return Ok(());
    }
    let cstr = |b: &[u8]| {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf8_lossy(&b[..end]).into_owned()
    };
    print!(
        "op={} xid={:x} ether={} yiaddr={} siaddr={} file={}",
        r[0],
        u32::from_be_bytes([r[4], r[5], r[6], r[7]]),
        r[28..34]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>(),
        ip_text(&r[16..20]),
        ip_text(&r[20..24]),
        cstr(&r[108..236])
    );
    let vend = &r[236..];
    if vend[..4] != [99, 130, 83, 99] {
        println!(" vend={}", cstr(vend));
        return Ok(());
    }
    let mut i = 4;
    while i + 1 < vend.len() && vend[i] != 255 {
        if vend[i] == 0 {
            i += 1;
            continue;
        }
        let (code, n) = (vend[i], usize::from(vend[i + 1]));
        let v = vend.get(i + 2..i + 2 + n).unwrap_or(&[]);
        let ips = || {
            let v: Vec<String> = v.chunks(4).map(ip_text).collect();
            v.join(",")
        };
        match code {
            53 => {
                let t = ["?", "discover", "offer", "request", "decline", "ack", "nak"];
                print!(" type={}", t.get(usize::from(v[0])).unwrap_or(&"?"));
            }
            54 => print!(" server={}", ips()),
            51 => print!(" lease={}", u32::from_be_bytes([v[0], v[1], v[2], v[3]])),
            1 => print!(" mask={}", ips()),
            3 => print!(" router={}", ips()),
            6 => print!(" dns={}", ips()),
            12 => print!(" host={}", String::from_utf8_lossy(v)),
            15 => print!(" dom={}", String::from_utf8_lossy(v)),
            _ => print!(" opt{code}"),
        }
        i += 2 + n;
    }
    println!();
    Ok(())
}

/// Sends one RIP response holding the routes `dest/mask/nexthop/metric`.
fn rip(args: &[String]) -> Result<(), Error> {
    let [addr, version, routes @ ..] = args else {
        return Err(Error::from(USAGE));
    };
    let version: u8 = version.parse().map_err(|_| Error::from(USAGE))?;
    let mut pkt = vec![2, version, 0, 0];
    for r in routes {
        let f: Vec<&str> = r.split('/').collect();
        let [dest, mask, hop, metric] = f.as_slice() else {
            return Err(Error(format!("bad route {r}")));
        };
        let metric: u32 = metric
            .parse()
            .map_err(|_| Error(format!("bad route {r}")))?;
        pkt.extend_from_slice(&[0, 2, 0, 0]);
        pkt.extend_from_slice(&parse_ip(dest)?);
        pkt.extend_from_slice(&parse_ip(mask)?);
        pkt.extend_from_slice(&parse_ip(hop)?);
        pkt.extend_from_slice(&metric.to_be_bytes());
    }
    let mut conn = dial::dial(addr)?;
    let mut f = data(&mut conn)?;
    f.write_all(&pkt)?;
    Ok(())
}
