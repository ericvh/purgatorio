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

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};

use ndb::daemon;
use ndb::dns::{self, put_name};
use purgatorio::dial::{self, Connection};
use purgatorio::{Context, Error};

const SELF: &str = "/bin/test/netsrv.wasm";
const USAGE: &str =
    "usage: netsrv dns|ntp|http|whois|telnet | netsrv tftp get|put addr file [text]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut argv = args;
    let serving = daemon::serving(&mut argv);
    let kind = argv.get(1).cloned().ok_or_else(|| Error::from(USAGE))?;
    if kind == "tftp" {
        return tftp(&argv[2..]);
    }
    let proto = match kind.as_str() {
        "dns" | "ntp" => "udp",
        "http" | "whois" | "telnet" => "tcp",
        _ => return Err(Error::from(USAGE)),
    };
    if !serving {
        let [r, w] = purgatorio::pipe()?;
        let child = vec![
            String::from("netsrv"),
            String::from(daemon::SERVE),
            kind.clone(),
        ];
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
