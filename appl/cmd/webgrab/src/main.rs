//! webgrab [-rv] [-o stem] [-p body] url: fetch `url` over HTTP (an
//! HTTP/1.1 GET, or a POST of `body` with `-p`) through `/net/tcp`, and
//! save it as `stem.suffix` in the current directory.
//!
//! The stem is the last element of the URL's path without its suffix, or,
//! with no path, the main part of the host name (`www.example.com` gives
//! `example`), or `webgrab`; `-o` sets it. The suffix is the path's, or one
//! for the content type. Redirects are followed. An HTML page gets a
//! comment at its end saying where and when it was fetched. `-v` reports
//! progress on standard error (`-vv` also shows the headers).
//!
//! Not yet done: the frames and images an HTML page refers to are not
//! fetched (so `-r`, which asks for just the page, is the only mode), the
//! proxy settings in `/services/webget/config` are not read, and there is
//! no https.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;

use purgatorio::arg::Arg;
use purgatorio::{Context, Error, daytime, dial};

const USAGE: &str = "webgrab [-rv] [-o stem] [-p body] url";
const MAX_REDIRECTS: usize = 10;

struct Url {
    host: String,
    port: String,
    path: String,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut verbose = 0;
    let mut stem = None;
    let mut body = None;
    while let Some(c) = arg.opt() {
        match c {
            'r' => {}
            'v' => verbose += 1,
            'o' => stem = Some(arg.earg()?),
            'p' => body = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [url] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let fail = Error;
    let mut u = parse(url).map_err(fail)?;
    let mut redirects = 0;
    let resp = loop {
        if verbose > 0 {
            eprintln!("webgrab: fetching http://{}:{}{}", u.host, u.port, u.path);
        }
        let resp = fetch(&u, body.as_deref(), verbose).map_err(fail)?;
        if (300..400).contains(&resp.code)
            && let Some(loc) = resp.header("location")
        {
            redirects += 1;
            if redirects > MAX_REDIRECTS {
                return Err(fail(String::from("too many redirects")));
            }
            u = relative(&u, loc).map_err(fail)?;
            continue;
        }
        break resp;
    };
    if !(200..300).contains(&resp.code) {
        return Err(fail(format!("{url}: {} {}", resp.code, resp.reason)));
    }
    let ctype = resp.header("content-type").unwrap_or("").to_string();
    let (s, suffix) = name(&u, &ctype);
    let file = match suffix.is_empty() {
        true => stem.clone().unwrap_or(s),
        false => format!("{}.{suffix}", stem.clone().unwrap_or(s)),
    };
    let mut data = resp.body;
    if ctype.starts_with("text/html") {
        let when = daytime::text(&daytime::gmt(daytime::now()));
        data.extend_from_slice(
            format!(
                "\n<!-- fetched from http://{}:{}{} on {when} by webgrab -->\n",
                u.host, u.port, u.path
            )
            .as_bytes(),
        );
    }
    std::fs::write(&file, &data)
        .map_err(|_| fail(format!("can't create {file}: {}", purgatorio::errstr())))?;
    if verbose > 0 {
        eprintln!("webgrab: wrote {} bytes to {file}", data.len());
    }
    Ok(())
}

fn parse(url: &str) -> Result<Url, String> {
    let rest = if let Some(r) = url.strip_prefix("http://") {
        r
    } else if url.starts_with("https://") {
        return Err(String::from("can't do https"));
    } else if url.contains("://") {
        return Err(format!("{url}: unknown scheme"));
    } else {
        url
    };
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (h, p),
        _ => (hostport, "80"),
    };
    if host.is_empty() {
        return Err(format!("{url}: no host"));
    }
    Ok(Url {
        host: host.to_string(),
        port: port.to_string(),
        path: path.to_string(),
    })
}

/// The target of a redirect.
fn relative(base: &Url, loc: &str) -> Result<Url, String> {
    if loc.contains("://") {
        return parse(loc);
    }
    let path = if loc.starts_with('/') {
        loc.to_string()
    } else {
        let dir = base.path.rsplit_once('/').map_or("", |(d, _)| d);
        format!("{dir}/{loc}")
    };
    Ok(Url {
        host: base.host.clone(),
        port: base.port.clone(),
        path,
    })
}

/// The stem and suffix of the saved file.
fn name(u: &Url, ctype: &str) -> (String, String) {
    let path = u.path.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let (mut stem, mut suffix) = match last.rsplit_once('.') {
        Some((s, x)) => (s.rsplit('.').next().unwrap_or(s).to_string(), x.to_string()),
        None => (last.to_string(), String::new()),
    };
    if stem.is_empty() {
        let parts: Vec<&str> = u.host.split('.').filter(|p| !p.is_empty()).collect();
        let parts: Vec<&str> = match parts.as_slice() {
            ["www", rest @ ..] => rest.to_vec(),
            p => p.to_vec(),
        };
        stem = match parts.as_slice() {
            [.., name, _top] => (*name).to_string(),
            _ => String::from("webgrab"),
        };
        if stem.bytes().all(|b| b.is_ascii_digit()) {
            stem = String::from("webgrab");
        }
    }
    if suffix.is_empty() {
        let mime = ctype.split(';').next().unwrap_or("").trim();
        suffix = match mime {
            "text/html" => "html",
            "text/plain" => "txt",
            "image/gif" => "gif",
            "image/jpeg" => "jpg",
            "image/png" => "png",
            "application/pdf" => "pdf",
            _ => "",
        }
        .to_string();
    }
    (stem, suffix)
}

struct Response {
    code: u32,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn fetch(u: &Url, body: Option<&str>, verbose: u32) -> Result<Response, String> {
    let conn = dial::dial(&format!("tcp!{}!{}", u.host, u.port))
        .map_err(|e| format!("can't dial {}: {e}", u.host))?;
    let data: OwnedFd = conn.data.ok_or("no connection")?;
    let mut f = File::from(data);
    let host = if u.port == "80" {
        u.host.clone()
    } else {
        format!("{}:{}", u.host, u.port)
    };
    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: webgrab\r\nAccept: */*\r\nConnection: close\r\n",
        if body.is_some() { "POST" } else { "GET" },
        u.path
    );
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    f.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    f.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("bad response: no header")?;
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    if verbose > 1 {
        eprintln!("webgrab: {status}");
    }
    let mut sf = status.splitn(3, ' ');
    let version = sf.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(format!("bad response: {status}"));
    }
    let code: u32 = sf
        .next()
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("bad response: {status}"))?;
    let reason = sf.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for l in lines {
        if verbose > 1 {
            eprintln!("webgrab: {l}");
        }
        if let Some((n, v)) = l.split_once(':') {
            headers.push((n.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut resp = Response {
        code,
        reason,
        headers,
        body: Vec::new(),
    };
    let rest = &raw[end + 4..];
    resp.body = if resp
        .header("transfer-encoding")
        .is_some_and(|t| t.eq_ignore_ascii_case("chunked"))
    {
        unchunk(rest)?
    } else if let Some(n) = resp
        .header("content-length")
        .and_then(|n| n.parse::<usize>().ok())
    {
        rest[..n.min(rest.len())].to_vec()
    } else {
        rest.to_vec()
    };
    Ok(resp)
}

/// Decodes a chunked body.
fn unchunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let eol = b
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("bad chunked body")?;
        let size = String::from_utf8_lossy(&b[..eol]);
        let size = size.split(';').next().unwrap_or("").trim();
        let n = usize::from_str_radix(size, 16).map_err(|_| "bad chunked body")?;
        b = &b[eol + 2..];
        if n == 0 {
            return Ok(out);
        }
        if b.len() < n {
            return Err(String::from("bad chunked body"));
        }
        out.extend_from_slice(&b[..n]);
        b = b.get(n + 2..).unwrap_or(&[]);
    }
}
