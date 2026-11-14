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
//! - `edit`: runs `Edit` commands and the other built-in commands in
//!   windows it makes, by writing their text to a tag and an `x` event
//!   for it, and reports the bodies, dots, `ctl` fields and the dump
//!   file they lead to.
//! - `pipe`: `Edit` commands that run commands while acme goes on: a
//!   command that reads the window's body through acme(4), and one
//!   that waits (`sleep`) while acme answers, refuses writes to the
//!   body it holds and a second `Edit`, and takes a `New` the test
//!   clicks once `/tmp/acme/ready` is written; `Kill` ends it. Also the
//!   events of a `Zerox`ed text, read through the second window.
//! - `win`: the name, dot and body (the start of a long one) of each
//!   window. `layout` adds what `Dump` writes of the columns and
//!   windows, run in the first window.

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

/// A window made through `new/ctl` and named `name`: its id.
fn window(name: &str) -> Result<String, String> {
    let mut ctl = open(&format!("{ACME}/new/ctl"))?;
    let id = nums(&readf(&mut ctl)).first().cloned().unwrap_or_default();
    put(&format!("{ACME}/{id}/ctl"), &format!("name {name}\n"))?;
    Ok(id)
}

/// Executes `cmd` in window `id` as a button 2 click on it in the tag
/// would, and clears the tag after.
fn exec(id: &str, cmd: &str) -> Result<(), String> {
    let w = format!("{ACME}/{id}");
    put(&format!("{w}/ctl"), "cleartag\n")?;
    put(&format!("{w}/tag"), cmd)?;
    let q1 = read(&format!("{w}/tag"))?.chars().count();
    let q0 = q1 - cmd.chars().count();
    put(&format!("{w}/event"), &format!("Mx{q0} {q1}\n"))?;
    // The command may have deleted the window.
    let _ = put(&format!("{w}/ctl"), "cleartag\n");
    Ok(())
}

/// Window `id`'s body, and its dot.
fn state(id: &str, addr: &mut File) -> Result<String, String> {
    let w = format!("{ACME}/{id}");
    put(&format!("{w}/ctl"), "addr=dot\n")?;
    let _ = addr.seek(SeekFrom::Start(0));
    let dot = nums(&readf(addr)).join(" ");
    Ok(format!("{:?} {dot}", read(&format!("{w}/body"))?))
}

/// Sets window `id`'s body to `text` and its dot to `q0,q1`.
fn set(id: &str, addr: &mut File, text: &str, q0: usize, q1: usize) -> Result<(), String> {
    let w = format!("{ACME}/{id}");
    at(",", addr);
    put(&format!("{w}/data"), text)?;
    at(&format!("#{q0},#{q1}"), addr);
    put(&format!("{w}/ctl"), "dot=addr\n")?;
    Ok(())
}

/// The id of the (first) window named `name`.
fn find(name: &str) -> Result<Vec<String>, String> {
    Ok(read(&format!("{ACME}/index"))?
        .lines()
        .map(nums)
        .filter(|f| f.get(5).is_some_and(|n| n == name))
        .map(|f| f[0].clone())
        .collect())
}

/// The body of the window whose name ends with `suffix`.
fn body_of(suffix: &str) -> Result<String, String> {
    for l in read(&format!("{ACME}/index"))?.lines() {
        let f = nums(l);
        if f.get(5).is_some_and(|n| n.ends_with(suffix)) {
            return read(&format!("{ACME}/{}/body", f[0]));
        }
    }
    Ok(String::new())
}

/// Waits until `f` holds, for at most a minute.
fn until(what: &str, f: impl Fn() -> bool) -> Result<(), String> {
    for _ in 0..600 {
        if f() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(format!("{what}: timed out"))
}

fn running(name: &str) -> bool {
    let mut v = Vec::new();
    let _ = ps(&mut v);
    v.first()
        .is_some_and(|l| l.split_whitespace().any(|m| m == name))
}

/// What a dump file says: the directory, each column's place, and each
/// window's column, kind and name (and with `y`, its place).
fn dumped(path: &str, y: bool) -> Result<Vec<String>, String> {
    let s = read(path)?;
    let mut out = Vec::new();
    let mut it = s.lines();
    while let Some(l) = it.next() {
        if l.starts_with("dir ") {
            out.push(l.to_string());
        }
        let f: Vec<&str> = l.split(' ').collect();
        if f.first() == Some(&"c") && f.len() == 3 {
            out.push(format!("c {}", f[1]));
            it.next();
        }
        if f.first() == Some(&"w") && f.len() == 11 {
            let tag = it.next().unwrap_or("");
            let name = tag.split_whitespace().next().unwrap_or("");
            if y {
                out.push(format!("w {} {} {} {name}", f[1], f[2], f[5]));
            } else {
                out.push(format!("w {} {} {name}", f[1], f[5]));
            }
        }
    }
    Ok(out)
}

fn edit(out: &mut Vec<String>) -> Result<(), String> {
    let a = window("/tmp/acme/ea")?;
    let b = window("/tmp/acme/eb")?;
    let mut addr = open(&format!("{ACME}/{a}/addr"))?;
    let mut baddr = open(&format!("{ACME}/{b}/addr"))?;
    const T: &str = "one two\nthree four\nfive\n";
    let cases: &[(&str, usize, usize, &str)] = &[
        (T, 0, 0, "Edit ,s/o/0/g"),
        (T, 0, 0, "Edit 2"),
        (T, 0, 0, "Edit 2,3d"),
        (T, 0, 0, "Edit /four/c/4/"),
        (T, 0, 0, "Edit ,x/[a-z]+/ s/.*/<&>/"),
        (T, 0, 0, "Edit ,s/([a-z]+) ([a-z]+)/\\2 \\1/g"),
        (T, 0, 0, "Edit ,x g/w/ d"),
        (T, 0, 0, "Edit ,y/ / c/_/"),
        (T, 0, 0, "Edit 1m$"),
        (T, 0, 0, "Edit {\n1d\n3d\n}"),
        (T, 0, 0, "Edit ,|tr a-z A-Z"),
        (T, 8, 8, "Edit <echo hi"),
        (T, 0, 0, "Edit 9d"),
        (T, 4, 7, "|tr a-z A-Z"),
        (T, 4, 7, "<echo new"),
        (T, 8, 13, ">cat"),
    ];
    for (text, q0, q1, cmd) in cases {
        set(&a, &mut addr, text, *q0, *q1)?;
        exec(&a, cmd)?;
        out.push(format!("{cmd:?}: {}", state(&a, &mut addr)?));
    }
    out.push(format!("errors: {:?}", body_of("/tmp/acme/+Errors")?));

    // X and Y run commands in other windows; Edit is one Undo.
    set(&a, &mut addr, T, 0, 0)?;
    set(&b, &mut baddr, "alpha\n", 0, 0)?;
    exec(&a, "Edit X/e[ab]$/ $a/!/")?;
    exec(&a, "Edit Y/(Errors|ea)$/ ,s/alpha/beta/")?;
    out.push(format!(
        "X, Y: a {} b {}",
        state(&a, &mut addr)?,
        state(&b, &mut baddr)?
    ));
    exec(&a, "Edit ,s/one/1/ ,s/two/2/ ,s/five/5/")?;
    out.push(format!("before Undo: {}", state(&a, &mut addr)?));
    exec(&a, "Undo")?;
    out.push(format!("after Undo: {}", state(&a, &mut addr)?));

    // Snarf and Paste.
    set(&a, &mut addr, T, 4, 7)?;
    exec(&a, "Snarf")?;
    at("#0", &mut addr);
    put(&format!("{ACME}/{a}/ctl"), "dot=addr\n")?;
    exec(&a, "Paste")?;
    out.push(format!("Paste: {}", state(&a, &mut addr)?));

    // Zerox: two windows on one text.
    exec(&a, "Zerox")?;
    let both = find("/tmp/acme/ea")?;
    out.push(format!("Zerox: {} windows", both.len()));
    if let Some(z) = both.iter().find(|i| **i != a) {
        let mut zaddr = open(&format!("{ACME}/{z}/addr"))?;
        at("$", &mut zaddr);
        put(&format!("{ACME}/{z}/data"), "added\n")?;
        out.push(format!(
            "in the other: {:?}",
            read(&format!("{ACME}/{a}/body"))?
        ));
        exec(&a, "Undo")?;
        out.push(format!(
            "undone in the first: {:?}",
            read(&format!("{ACME}/{z}/body"))?
        ));
        put(&format!("{ACME}/{z}/ctl"), "delete\n")?;
    }

    // Tab and Font change what ctl says.
    let ctl =
        |id: &str| -> Result<Vec<String>, String> { Ok(nums(&read(&format!("{ACME}/{id}/ctl"))?)) };
    let tab0: i64 = ctl(&a)?[7].parse().unwrap_or(0);
    exec(&a, "Tab 8")?;
    let tab1: i64 = ctl(&a)?[7].parse().unwrap_or(0);
    out.push(format!("Tab 8: {}", if tab0 > 0 { tab1 / tab0 } else { 0 }));
    exec(&a, "Font")?;
    out.push(format!("Font: {}", ctl(&a)?[6]));
    exec(&a, "Font")?;
    out.push(format!("Font again: {}", ctl(&a)?[6]));

    // Kill: a command run from the window, killed from it.
    exec(&a, "sleep 1000")?;
    until("sleep starting", || running("sleep"))?;
    exec(&a, "Kill")?;
    until("sleep killed", || !running("sleep"))?;
    out.push(String::from("Kill: killed"));

    // Sort, Local and Dump: the dump says the order and the directory.
    let z = window("/tmp/acme/ez")?;
    let c = window("/tmp/acme/ec")?;
    exec(&z, "Sort")?;
    exec(&c, "Local cd /tmp/acme")?;
    exec(&c, "Dump /tmp/acme/dump")?;
    for l in dumped("/tmp/acme/dump", false)? {
        out.push(format!("dump: {l}"));
    }
    // Load puts it back after the windows go: the first warns, as the
    // modified window is named.
    for id in [&b, &z, &c] {
        put(&format!("{ACME}/{id}/ctl"), "delete\n")?;
    }
    exec(&a, "Load /tmp/acme/dump")?;
    exec(&a, "Load /tmp/acme/dump")?;
    let names: Vec<String> = read(&format!("{ACME}/index"))?
        .lines()
        .map(|l| nums(l).get(5).cloned().unwrap_or_default())
        .collect();
    out.push(format!("after Load: {}", names.join(" ")));
    let ea = find("/tmp/acme/ea")?;
    if let Some(id) = ea.first() {
        out.push(format!(
            "ea after Load: {:?}",
            read(&format!("{ACME}/{id}/body"))?
        ));
    }
    for l in read(&format!("{ACME}/index"))?.lines() {
        let n = nums(l)[0].clone();
        put(&format!("{ACME}/{n}/ctl"), "delete\n")?;
    }
    Ok(())
}

fn pipe(out: &mut Vec<String>) -> Result<(), String> {
    const T: &str = "one two\nthree four\nfive\n";
    let a = window("/tmp/acme/pa")?;
    let mut addr = open(&format!("{ACME}/{a}/addr"))?;

    // Every view of a shared text reports its changes.
    exec(&a, "Zerox")?;
    let z = find("/tmp/acme/pa")?
        .into_iter()
        .find(|i| *i != a)
        .ok_or("no Zerox")?;
    let mut zev = open(&format!("{ACME}/{z}/event"))?;
    put(&format!("{ACME}/{a}/body"), "x")?;
    out.push(format!("Zerox event: {:?}", events(&mut zev, 1)));
    drop(zev);
    put(&format!("{ACME}/{z}/ctl"), "delete\n")?;

    // A command that reads the window through acme(4) while acme waits
    // for it.
    set(&a, &mut addr, T, 4, 7)?;
    let cmd = "|{tr a-z A-Z; echo; sed -n 2p /mnt/acme/$winid/body}";
    exec(&a, cmd)?;
    out.push(format!("{cmd:?}: {}", state(&a, &mut addr)?));
    exec(&a, "Undo")?;
    out.push(format!("Undo: {}", state(&a, &mut addr)?));

    // A command that waits. Another prog runs it, as the write of the
    // event is answered when the command has been done.
    set(&a, &mut addr, T, 0, 3)?;
    let argv = ["acmefs", "-x", a.as_str(), "|{sleep 1000; tr a-z A-Z}"].map(String::from);
    purgatorio::spawn(
        "/bin/test/acmefs.wasm",
        &argv,
        purgatorio::abi::pctl::NEWFD,
        &[(0, 0), (1, 1), (2, 2)],
    )
    .map_err(|e| e.0)?;
    until("sleep starting", || running("sleep"))?;
    let index = read(&format!("{ACME}/index"))?;
    out.push(format!(
        "index while it runs: {}",
        index.lines().filter(|l| l.contains("/tmp/acme/pa")).count()
    ));
    out.push(format!(
        "body read while it runs: {:?}",
        read(&format!("{ACME}/{a}/body"))?
    ));
    out.push(format!(
        "body write while it runs: {:?}",
        put(&format!("{ACME}/{a}/body"), "no\n")
    ));
    let b = window("/tmp/acme/pb")?;
    put(&format!("{ACME}/{b}/body"), "free\n")?;
    out.push(format!(
        "another window: {:?}",
        read(&format!("{ACME}/{b}/body"))?
    ));
    exec(&b, "Edit ,d")?;
    out.push(format!(
        "a second Edit: {:?} {:?}",
        read(&format!("{ACME}/{b}/body"))?,
        body_of("/tmp/acme/+Errors")?
    ));
    // The test clicks New in the right column's tag.
    let windows = || read(&format!("{ACME}/index")).map_or(0, |s| s.lines().count());
    let n = windows();
    std::fs::write(format!("{DIR}/ready"), &a).map_err(|e| Error::from(e).0)?;
    until("New while it runs", || windows() > n)?;
    out.push(String::from("New while it runs: made"));
    exec(&a, "Kill")?;
    until("sleep killed", || !running("sleep"))?;
    let _ = purgatorio::wait();
    // The command wrote nothing before it was killed.
    out.push(format!("after Kill: {}", state(&a, &mut addr)?));

    for l in read(&format!("{ACME}/index"))?.lines() {
        let n = nums(l)[0].clone();
        put(&format!("{ACME}/{n}/ctl"), "delete\n")?;
    }
    Ok(())
}

/// Each window's name, dot and the start of its body; with `layout`,
/// the dump of the layout too.
fn win(out: &mut Vec<String>, layout: bool) -> Result<(), String> {
    let index = read(&format!("{ACME}/index"))?;
    for l in index.lines() {
        let f = nums(l);
        let id = f[0].clone();
        let mut a = open(&format!("{ACME}/{id}/addr"))?;
        put(&format!("{ACME}/{id}/ctl"), "addr=dot\n")?;
        let dot = nums(&readf(&mut a)).join(" ");
        let body = read(&format!("{ACME}/{id}/body"))?;
        let start: String = body.chars().take(24).collect();
        out.push(format!(
            "{}: dot {dot} len {} start {start:?}",
            f.get(5).cloned().unwrap_or_default(),
            body.chars().count()
        ));
    }
    if layout && let Some(first) = index.lines().next().map(nums) {
        let path = format!("{DIR}/layout");
        exec(&first[0], &format!("Dump {path}"))?;
        for l in dumped(&path, true)? {
            out.push(format!("dump: {l}"));
        }
    }
    Ok(())
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
    if args.get(1).map(String::as_str) == Some("-x") {
        // Executes a command in a window, for `pipe`.
        let id = args.get(2).cloned().unwrap_or_default();
        let cmd = args.get(3).cloned().unwrap_or_default();
        return exec(&id, &cmd).map_err(Error);
    }
    let mode = std::fs::read_to_string(format!("{DIR}/mode")).unwrap_or_default();
    let mut out = Vec::new();
    let r = match mode.trim() {
        "fs" => fs(&mut out),
        "body" => body(&mut out),
        "ps" => ps(&mut out),
        "plumb" => plumb(&mut out),
        "edit" => edit(&mut out),
        "pipe" => pipe(&mut out),
        "win" => win(&mut out, false),
        "layout" => win(&mut out, true),
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
