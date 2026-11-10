//! plumber [-v] [-w] [-c wmchan] [rulefile ...]: route plumbing messages.
//! The rules (default `/usr/$user/plumbing`, else `/usr/$user/lib/plumbing`)
//! are read, then a server goes into the background with
//! `/chan/plumb.input` and a `/chan/plumb.<port>` file for each port the
//! rules name. A message written to the input is routed by the first rule
//! that matches it, or by its destination if it has one, and is queued
//! for the port's readers; each read returns one message. If nobody has
//! read the port yet and the rule says `plumb start`, the program is
//! started instead, in the message's directory (through `wmchan` with
//! `-w`). `-v` logs each message and the rules it met.
//!
//! `plumber --serve ...` is the server; it writes one ready byte to fd 5.

mod rules;

use std::collections::VecDeque;
use std::io::Write;
use std::os::fd::AsRawFd;

use plumbmsg::Msg;
use purgatorio::abi::{mode, pctl};
use purgatorio::file2chan::{self, FileIO, Request};
use purgatorio::{Context, Error};

use rules::{Inmsg, Rule};

const USAGE: &str = "usage: plumber [-vw] [-c wmchan] [rulefile ...]";
const READY_FD: i32 = 5;

struct Opts {
    verbose: bool,
    wm: bool,
    wmchan: String,
    files: Vec<String>,
}

fn options(args: &[String]) -> Result<Opts, Error> {
    let mut arg = purgatorio::arg::Arg::new(args.to_vec(), USAGE.trim_start_matches("usage: "));
    let mut o = Opts {
        verbose: false,
        wm: false,
        wmchan: String::from("/chan/wm"),
        files: Vec::new(),
    };
    while let Some(c) = arg.opt() {
        match c {
            'v' => o.verbose = true,
            'w' => o.wm = true,
            'c' => o.wmchan = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    o.files = arg.argv();
    Ok(o)
}

fn read_rules(files: &[String]) -> Result<Vec<Rule>, Error> {
    let mut files = files.to_vec();
    if files.is_empty() {
        let user = std::fs::read_to_string("/dev/user")
            .map_err(|e| Error(format!("can't read /dev/user: {}", Error::from(e))))?;
        let first = format!("/usr/{user}/plumbing");
        files.push(if purgatorio::stat(&first).is_ok() {
            first
        } else {
            format!("/usr/{user}/lib/plumbing")
        });
    }
    let mut out = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f)
            .map_err(|e| Error(format!("can't read {f}: {}", Error::from(e))))?;
        out.extend(rules::parse(f, &text).map_err(Error)?);
    }
    Ok(out)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let mut a = vec![args[0].clone()];
        a.extend_from_slice(&args[2..]);
        let o = options(&a)?;
        let rules = read_rules(&o.files)?;
        return serve(&o, &rules);
    }
    let o = options(&args)?;
    // Errors in the rules are reported here, before going into the
    // background.
    read_rules(&o.files)?;
    let [r, w] = purgatorio::pipe()?;
    let mut argv = vec![String::from("plumber"), String::from("--serve")];
    argv.extend_from_slice(&args[1..]);
    purgatorio::spawn(
        "/bin/plumber.wasm",
        &argv,
        pctl::NEWFD,
        &[(0, 0), (1, 1), (2, 2), (w.as_raw_fd(), READY_FD)],
    )?;
    drop(w);
    match file2chan::read_byte(std::fs::File::from(r)) {
        Ok(_) => Ok(()),
        Err(_) => purgatorio::exits("fail:init"),
    }
}

/// A port: its file, the messages waiting for it, and the reads waiting
/// for messages.
struct Port {
    name: String,
    io: FileIO,
    queue: VecDeque<Vec<u8>>,
    readers: Vec<Request>,
    /// Some program has read the port or said it started.
    started: bool,
}

impl Port {
    /// Answers waiting reads from the queue.
    fn deliver(&mut self) {
        while !self.queue.is_empty() && !self.readers.is_empty() {
            let req = self.readers.remove(0);
            let msg = &self.queue[0];
            if (req.count() as usize) < msg.len() {
                let _ = req.respond_err("buffer too short for message");
                continue;
            }
            // A reader that has gone away has closed its reply channel;
            // the message then goes to the next one.
            if req.respond_read(msg).is_ok() {
                self.queue.pop_front();
            }
        }
    }
}

fn log(verbose: bool, s: &str) {
    if verbose {
        eprintln!("plumb: {s}");
    }
}

fn describe(m: &Inmsg) -> String {
    let mut s = format!(
        "src={}; dst={}; dir={}; kind={}; attr='{}'",
        m.msg.src, m.msg.dst, m.msg.dir, m.msg.kind, m.msg.attr
    );
    if m.msg.kind == "text" {
        s.push_str(&format!("; data='{}'", m.text));
    }
    s
}

fn serve(o: &Opts, rules: &[Rule]) -> Result<(), Error> {
    let make = |name: &str| {
        file2chan::file2chan("/chan", name)
            .map_err(|e| Error(format!("can't establish /chan/{name}: {e}")))
    };
    let input = make("plumb.input")?;
    let mut ports = Vec::new();
    for name in rules::ports(rules) {
        ports.push(Port {
            io: make(&format!("plumb.{name}"))?,
            name,
            queue: VecDeque::new(),
            readers: Vec::new(),
            started: false,
        });
    }
    file2chan::write_fd(READY_FD, b"R")?;
    // SAFETY: fd 5 was given to this prog for the ready byte alone.
    unsafe {
        drop(<std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(
            READY_FD,
        ));
    }
    loop {
        let mut files: Vec<&FileIO> = vec![&input];
        files.extend(ports.iter().map(|p| &p.io));
        let (which, req) = match file2chan::recv_any(&files) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()),
            // A kill ends the wait; leave quietly.
            Err(e) => purgatorio::exits(&e.0),
        };
        if which > 0 {
            let p = &mut ports[which - 1];
            if req.is_read() {
                p.started = true;
                p.readers.retain(|r| r.reply != req.reply);
                p.readers.push(req);
                p.deliver();
            } else if req.is_trunc() {
                let _ = req.respond_write(0);
            } else {
                let _ = req.respond_err(purgatorio::abi::error::EPERM);
            }
            continue;
        }
        if req.is_read() {
            let _ = req.respond_read(&[]);
            continue;
        }
        if req.is_trunc() {
            let _ = req.respond_write(0);
            continue;
        }
        let n = req.data().len() as u32;
        let res = match Msg::unpack(req.data()) {
            Some(msg) => route(o, rules, &mut ports, msg),
            None => Ok(()),
        };
        let _ = match res {
            Ok(()) => req.respond_write(n),
            Err(e) => req.respond_err(&e),
        };
    }
}

/// Routes one message; an error is returned to its writer.
fn route(o: &Opts, rules: &[Rule], ports: &mut [Port], msg: Msg) -> Result<(), String> {
    // A control message from a program reading a port.
    if msg.kind == "text" && msg.dst == "plumb" {
        let text = String::from_utf8_lossy(&msg.data);
        let delta = match text.as_ref() {
            "start" => true,
            "stop" => false,
            _ => {
                eprintln!(
                    "plumb: unrecognized control message from {}: {text}",
                    msg.src
                );
                return Ok(());
            }
        };
        match ports.iter_mut().find(|p| p.name == msg.src) {
            Some(p) => p.started = delta,
            None => eprintln!(
                "plumb: \"start\" message from unrecognized port {}",
                msg.src
            ),
        }
        return Ok(());
    }
    let mut m = Inmsg::new(msg);
    log(o.verbose, &format!("msg: {}", describe(&m)));
    let known = |name: &str| ports.iter().any(|p| p.name == name);
    let routed = if m.msg.kind != "text" {
        false
    } else if !m.msg.dst.is_empty() {
        m.port = Some(m.msg.dst.clone());
        true
    } else {
        let stat = |name: &str| {
            purgatorio::stat(name)
                .ok()
                .map(|d| d.mode & purgatorio::DMDIR != 0)
        };
        rules::apply(rules, &mut m, &stat, &mut |s| log(o.verbose, s))
    };
    let port = match m.port.clone() {
        Some(p) if routed && known(&p) => p,
        Some(p) if routed => {
            eprintln!("plumb: unrecognized port {p}");
            return Err(String::from("no matching plumb rule"));
        }
        _ => {
            if m.msg.src != "acme" {
                eprintln!("plumb: don't know who message goes to");
            }
            return Err(String::from("no matching plumb rule"));
        }
    };
    let Some(p) = ports.iter_mut().find(|p| p.name == port) else {
        return Err(String::from("no matching plumb rule"));
    };
    if !p.started {
        let Some((prog, args)) = m.startup.clone() else {
            eprintln!("plumb: can't start application");
            return Ok(());
        };
        let prog = rules::expand(&m, &prog);
        let args: Vec<String> = args.iter().map(|a| rules::expand(&m, a)).collect();
        log(o.verbose, &format!("start {prog} port {port}"));
        start(o, &m.msg.dir, &prog, &args);
        return Ok(());
    }
    log(o.verbose, &format!("send \"{}\" to {port}", summary(&m)));
    p.queue.push_back(m.msg.pack());
    p.deliver();
    Ok(())
}

/// The first line of a text message, cut short, for the log.
fn summary(m: &Inmsg) -> String {
    if m.msg.kind != "text" {
        return format!("message of type {}", m.msg.kind);
    }
    let line = m.text.lines().next().unwrap_or("");
    if line.chars().count() > 51 {
        format!("{}...", line.chars().take(51).collect::<String>())
    } else {
        String::from(line)
    }
}

fn quote(s: &str) -> String {
    if !s.is_empty()
        && !s
            .chars()
            .any(|c| c.is_whitespace() || "'#;&|{}()<>$`^=\"".contains(c))
    {
        return String::from(s);
    }
    format!("'{}'", s.replace('\'', "''"))
}

/// Starts `prog` with `args` in directory `dir`, or asks the window
/// manager to.
fn start(o: &Opts, dir: &str, prog: &str, args: &[String]) {
    let mut words = vec![quote(prog)];
    words.extend(args.iter().map(|a| quote(a)));
    if o.wm {
        let mut all = vec![quote(dir)];
        all.extend(words);
        let res = purgatorio::open(&o.wmchan, mode::OWRITE).and_then(|fd| {
            std::fs::File::from(fd)
                .write_all(format!("s {}", all.join(" ")).as_bytes())
                .map_err(Error::from)
        });
        if let Err(e) = res {
            eprintln!("plumb: can't start {prog}: {e}");
        }
        return;
    }
    let mut cmd = words.join(" ");
    if !dir.is_empty() {
        cmd = format!("cd {} && {cmd}", quote(dir));
    }
    let argv = [String::from("sh"), String::from("-c"), cmd];
    if let Err(e) = purgatorio::spawn("/bin/sh.wasm", &argv, pctl::FORKENV, &[]) {
        eprintln!("plumb: can't load {prog}: {e}");
    }
}
