//! acmefs: checks of acme(4) and `win`, run from inside acme by
//! `crates/purg-emu/tests/acme.rs`. What to check is in
//! `/tmp/acme/mode`; the results go to `/tmp/acme/out`, ending with an
//! `end` line, for the test to compare.
//!
//! - `fs`: makes a window through `new/ctl` and works its files: `ctl`,
//!   `body`, `addr`, `data`, `xdata`, `tag`, `index`, `event`. It writes
//!   `/tmp/acme/ready` and waits for the test to click button 2 on `New`
//!   in the window, reports the events, writes the expansion back (acme
//!   makes a new window), and checks that a reader of `event` that is
//!   killed lets go of it.
//! - `body`: the tag and body of the window whose name holds `/-`
//!   (`win`'s).
//! - `ps`: the modules of the progs running.
//! - `plumb`: runs `B -2 /tmp/acme/plumbed` and waits for acme to open
//!   the file from the plumber's `edit` port; reports the window's name
//!   and the selection (its second line).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use purgatorio::{Context, Error};

const ACME: &str = "/mnt/acme";
const DIR: &str = "/tmp/acme";

fn open(path: &str) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("{path}: {}", Error::from(e)))
}

fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {}", Error::from(e)))
}

fn readf(f: &mut File) -> String {
    let mut b = [0u8; 8192];
    let n = f.read(&mut b).unwrap_or(0);
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// Writes `s` to `path`, giving back the error string.
fn put(path: &str, s: &str) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("{path}: {}", Error::from(e)))?;
    f.write_all(s.as_bytes()).map_err(|_| purgatorio::errstr())
}

fn writef(f: &mut File, s: &str) -> String {
    match f.write_all(s.as_bytes()) {
        Ok(()) => String::from("ok"),
        Err(_) => purgatorio::errstr(),
    }
}

/// Sets `addr` to `a` and reads it back.
fn at(a: &str, addr: &mut File) -> String {
    let r = writef(addr, a);
    if r != "ok" {
        return r;
    }
    let _ = addr.seek(SeekFrom::Start(0));
    nums(&readf(addr)).join(" ")
}

fn nums(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

/// Reads `event` until `n` whole events have come.
fn events(ev: &mut File, n: usize) -> Vec<String> {
    let mut text = String::new();
    loop {
        let got = split(&text);
        if got.len() >= n {
            return got;
        }
        let s = readf(ev);
        if s.is_empty() {
            return got;
        }
        text.push_str(&s);
    }
}

/// The events in `s`, each without its final newline.
fn split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while rest.len() > 2 {
        let f: Vec<&str> = rest.splitn(6, ' ').collect();
        if f.len() < 5 {
            break;
        }
        let Ok(count) = f[3].parse::<usize>() else {
            break;
        };
        let head = f[..4].iter().map(|x| x.len() + 1).sum::<usize>();
        let body: String = rest[head..].chars().take(count).collect();
        let end = head + body.len();
        if !rest[end..].starts_with('\n') {
            break;
        }
        out.push(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
    out
}

fn fs(out: &mut Vec<String>) -> Result<(), String> {
    let mut ctl = open(&format!("{ACME}/new/ctl"))?;
    let c = nums(&readf(&mut ctl));
    let id = c.first().cloned().unwrap_or_default();
    let w = format!("{ACME}/{id}");
    out.push(format!(
        "ctl: {} fields, body {} dir {} dirty {} width>0 {} font {} tab>0 {}",
        c.len(),
        c[2],
        c[3],
        c[4],
        c[5].parse::<i32>().is_ok_and(|x| x > 0),
        c[6],
        c[7].parse::<i32>().is_ok_and(|x| x > 0)
    ));
    put(&format!("{w}/body"), "one\ntwo\nthree\n")?;
    out.push(format!("body: {:?}", read(&format!("{w}/body"))?));
    let mut addr = open(&format!("{w}/addr"))?;
    let mut data = open(&format!("{w}/data"))?;
    let mut xdata = open(&format!("{w}/xdata"))?;
    for a in [
        "2", "#3,#5", "/thr/", "0", "$", "3-1", "2,3", "/o/", "?o?", "/(/", "9",
    ] {
        let r = at(a, &mut addr);
        out.push(format!("addr {a}: {r}"));
    }
    at("#3,#5", &mut addr);
    out.push(format!("xdata #3,#5: {:?}", readf(&mut xdata)));
    at("/thr/", &mut addr);
    out.push(format!("dot=addr: {}", writef(&mut ctl, "dot=addr\n")));
    at("0", &mut addr);
    writef(&mut ctl, "addr=dot\n");
    out.push(format!("addr=dot: {}", at(".", &mut addr)));
    at("2", &mut addr);
    writef(&mut data, "TWO\n");
    out.push(format!("data write at 2: addr {}", at(".", &mut addr)));
    at("0", &mut addr);
    out.push(format!("data from 0: {:?}", readf(&mut data)));
    out.push(format!("addr after: {}", at(".", &mut addr)));
    let _ = ctl.seek(SeekFrom::Start(0));
    let c = nums(&readf(&mut ctl));
    out.push(format!("dirty: {}", c[4]));
    writef(&mut ctl, "clean\n");
    let mut c2 = open(&format!("{w}/ctl"))?;
    out.push(format!("clean: {}", nums(&readf(&mut c2))[4]));
    out.push(format!("bogus: {}", writef(&mut ctl, "bogus\n")));
    out.push(format!(
        "name: {}",
        writef(&mut ctl, "name /tmp/acme/fsw\n")
    ));
    let index = read(&format!("{ACME}/index"))?;
    let line = index
        .lines()
        .find(|l| nums(l).first() == Some(&id))
        .unwrap_or("");
    out.push(format!("index: {}", nums(line)[5..].join(" ")));
    put(&format!("{w}/tag"), "Extra")?;
    out.push(format!("tag: {}", read(&format!("{w}/tag"))?));
    writef(&mut ctl, "cleartag\n");
    out.push(format!("cleartag: {}", read(&format!("{w}/tag"))?));

    // The events of a button 2 click on New, the first word.
    at(",", &mut addr);
    writef(&mut data, "New\nend\n");
    let mut ev = open(&format!("{w}/event"))?;
    std::fs::write(format!("{DIR}/ready"), &id).map_err(|e| Error::from(e).0)?;
    let es = events(&mut ev, 2);
    for e in &es {
        // Where the click fell depends on the pixels; the flag does not.
        let f = nums(e);
        if f.len() == 4 && f[0].len() > 2 {
            out.push(format!("event: {} {} flag {}", &f[0][..2], f[3], f[2]));
        } else {
            out.push(format!("event: {e:?}"));
        }
    }
    let Some(back) = es.get(1) else {
        return Err(String::from("no expansion event"));
    };
    out.push(format!(
        "write back: {}",
        writef(&mut ev, &format!("{back}\n"))
    ));
    let index = read(&format!("{ACME}/index"))?;
    out.push(format!("windows after New: {}", index.lines().count()));
    put(&format!("{w}/body"), "more\n")?;
    out.push(format!("body write: {:?}", events(&mut ev, 1)));
    drop(ev);

    // A reader of event that is killed lets go of it.
    let mut ctl2 = open(&format!("{ACME}/new/ctl"))?;
    let id2 = nums(&readf(&mut ctl2)).first().cloned().unwrap_or_default();
    let reader = purgatorio::spawn(
        "/bin/test/acmefs.wasm",
        &[String::from("acmefs"), String::from("-r"), id2.clone()],
        purgatorio::abi::pctl::NEWFD,
        &[],
    )
    .map_err(|e| e.0)?;
    std::thread::sleep(std::time::Duration::from_millis(500));
    put(&format!("/prog/{reader}/ctl"), "kill")?;
    let _ = purgatorio::wait();
    let mut ev2 = open(&format!("{ACME}/{id2}/event"))?;
    put(&format!("{ACME}/{id2}/body"), "x")?;
    out.push(format!("after a killed reader: {:?}", events(&mut ev2, 1)));
    drop(ev2);

    for l in read(&format!("{ACME}/index"))?.lines() {
        let n = nums(l)[0].clone();
        put(&format!("{ACME}/{n}/ctl"), "delete\n")?;
    }
    out.push(format!(
        "index at end: {:?}",
        read(&format!("{ACME}/index"))?
    ));
    out.push(format!("deleted: {}", writef(&mut ctl, "clean\n")));
    Ok(())
}

fn body(out: &mut Vec<String>) -> Result<(), String> {
    for l in read(&format!("{ACME}/index"))?.lines() {
        let f = nums(l);
        if f.get(5).is_some_and(|n| n.contains("/-")) {
            let tag = read(&format!("{ACME}/{}/tag", f[0]))?;
            out.push(format!("tag: {tag}"));
            out.push(format!(
                "body: {:?}",
                read(&format!("{ACME}/{}/body", f[0]))?
            ));
        } else if f.get(5).is_some_and(|n| n.ends_with("+Errors")) {
            out.push(format!(
                "errors: {:?}",
                read(&format!("{ACME}/{}/body", f[0]))?
            ));
        }
    }
    Ok(())
}

fn ps(out: &mut Vec<String>) -> Result<(), String> {
    let mut mods = Vec::new();
    for e in std::fs::read_dir("/prog").map_err(|_| purgatorio::errstr())? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if let Ok(s) = read(&format!("/prog/{name}/status")) {
            mods.push(s.split_whitespace().last().unwrap_or("").to_string());
        }
    }
    mods.sort();
    out.push(format!("progs: {}", mods.join(" ")));
    Ok(())
}

/// `B -2 /tmp/acme/plumbed` through the plumber; acme opens the file
/// with its second line selected.
fn plumb(out: &mut Vec<String>) -> Result<(), String> {
    let file = format!("{DIR}/plumbed");
    let argv = ["B", "-2", file.as_str()].map(String::from);
    purgatorio::spawn(
        "/bin/B.wasm",
        &argv,
        purgatorio::abi::pctl::NEWFD,
        &[(0, 0), (1, 1), (2, 2)],
    )
    .map_err(|e| format!("B: {e}"))?;
    let w = purgatorio::wait().map_err(|e| format!("wait: {e}"))?;
    out.push(format!("B: {:?}", w.status));
    for _ in 0..600 {
        for l in read(&format!("{ACME}/index"))?.lines() {
            let f = nums(l);
            if f.get(5) == Some(&file) {
                // The window's address is reset when addr is first
                // opened, so open it before setting it.
                let mut a = open(&format!("{ACME}/{}/addr", f[0]))?;
                put(&format!("{ACME}/{}/ctl", f[0]), "addr=dot")?;
                let addr = readf(&mut a);
                out.push(format!("window: {}", f[5]));
                out.push(format!("dot: {}", nums(&addr).join(" ")));
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(String::from("acme opened no window"))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("-r") {
        // The reader to be killed: blocks on the window's event file.
        let id = args.get(2).cloned().unwrap_or_default();
        let mut ev = open(&format!("{ACME}/{id}/event")).map_err(Error)?;
        readf(&mut ev);
        return Ok(());
    }
    let mode = std::fs::read_to_string(format!("{DIR}/mode")).unwrap_or_default();
    let mut out = Vec::new();
    let r = match mode.trim() {
        "fs" => fs(&mut out),
        "body" => body(&mut out),
        "ps" => ps(&mut out),
        "plumb" => plumb(&mut out),
        m => Err(format!("unknown mode {m:?}")),
    };
    if let Err(e) = r {
        out.push(format!("FAIL: {e}"));
    }
    out.push(String::from("end\n"));
    // One write, which the test waits to see end with `end`.
    std::fs::write(format!("{DIR}/out"), out.join("\n"))?;
    Ok(())
}
