//! procs [scratch-dir]: exercises progs, pipes, the environment and file
//! creation from inside a prog. Prints `ok <name>` per passing check and
//! `FAIL <name>: <why>` otherwise; exits with status `fail:procs` if any
//! check failed. File checks run under `scratch-dir` (default `/tmp`), which
//! must be writable.
//!
//! `procs -child <what>` is the helper the checks spawn.

use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use purgatorio::abi::{mode, mount, pctl};
use purgatorio::{Context, Dir, Error};

const SELF: &str = "/bin/test/procs.wasm";

type Check = fn(&str) -> Result<(), String>;

fn e(err: Error) -> String {
    err.0
}

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| String::from(*s)).collect()
}

/// Reads `fd` to end of file.
fn drain(fd: OwnedFd) -> Result<String, String> {
    let mut s = String::new();
    std::fs::File::from(fd)
        .read_to_string(&mut s)
        .map_err(|e| e.to_string())?;
    Ok(s)
}

/// Runs `cmd` with its stdout on a pipe; returns what it wrote and its
/// exit status.
fn capture(cmd: &str, argv: &[&str], flags: u32) -> Result<(String, String), String> {
    let [r, w] = purgatorio::pipe().map_err(e)?;
    let pid = purgatorio::spawn(cmd, &args(argv), flags, &[(w.as_raw_fd(), 1)]).map_err(e)?;
    drop(w);
    let out = drain(r)?;
    let msg = purgatorio::wait().map_err(e)?;
    if msg.pid != pid {
        return Err(format!("waited for {pid}, got {msg:?}"));
    }
    Ok((out, msg.status))
}

fn spawn_echo() -> Result<(), String> {
    let (out, status) = capture("echo", &["echo", "through", "a pipe"], 0)?;
    if out != "through a pipe\n" || !status.is_empty() {
        return Err(format!("out {out:?} status {status:?}"));
    }
    Ok(())
}

fn child_failure_status() -> Result<(), String> {
    let (out, status) = capture("cat", &["cat", "/nonexistent"], 0)?;
    if status != "fail:errors" {
        return Err(format!("status {status:?}, out {out:?}"));
    }
    Ok(())
}

fn spawn_missing() -> Result<(), String> {
    match purgatorio::spawn("nosuch", &args(&["nosuch"]), 0, &[]) {
        Ok(pid) => Err(format!("spawned pid {pid}")),
        Err(err) if err.0.contains("file does not exist") => Ok(()),
        Err(err) => Err(err.0),
    }
}

fn env_inherited() -> Result<(), String> {
    let f = purgatorio::create("/env/PROCSVAR", mode::OWRITE, 0o666).map_err(e)?;
    std::io::Write::write_all(&mut std::fs::File::from(f), b"shared value")
        .map_err(|e| e.to_string())?;
    let (out, _) = capture(SELF, &["procs", "-child", "env"], 0)?;
    if out != "shared value\n" {
        return Err(format!("child saw {out:?}"));
    }
    let (out, _) = capture(SELF, &["procs", "-child", "env"], pctl::NEWENV)?;
    if out != "unset\n" {
        return Err(format!("NEWENV child saw {out:?}"));
    }
    purgatorio::remove("/env/PROCSVAR").map_err(e)
}

fn newfd_child() -> Result<(), String> {
    let (out, _) = capture(SELF, &["procs", "-child", "fds"], pctl::NEWFD)?;
    if out != "0 closed, 1 open\n" {
        return Err(format!("child saw {out:?}"));
    }
    Ok(())
}

fn std_sleep() -> Result<(), String> {
    let t = Instant::now();
    std::thread::sleep(Duration::from_millis(20));
    if t.elapsed() < Duration::from_millis(20) {
        return Err(format!("slept {:?}", t.elapsed()));
    }
    Ok(())
}

fn scratch(base: &str) -> String {
    format!("{base}/procs-{}", purgatorio::pid())
}

fn files(base: &str) -> Result<(), String> {
    let dir = scratch(base);
    let io = |e: std::io::Error| e.to_string();
    std::fs::create_dir(&dir).map_err(io)?;
    let a = format!("{dir}/a");
    std::fs::write(&a, "first").map_err(io)?;
    std::fs::write(&a, "second").map_err(io)?;
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&a)
            .map_err(io)?;
        f.write_all(b"+more").map_err(io)?;
    }
    if std::fs::read_to_string(&a).map_err(io)? != "second+more" {
        return Err("bad contents after append".into());
    }
    let b = format!("{dir}/b");
    std::fs::rename(&a, &b).map_err(io)?;
    if std::fs::metadata(&a).is_ok() || std::fs::metadata(&b).map_err(io)?.len() != 11 {
        return Err("rename did not move the file".into());
    }
    let mut d = Dir::null();
    d.length = 3;
    purgatorio::wstat(&b, &d).map_err(e)?;
    if std::fs::read_to_string(&b).map_err(io)? != "sec" {
        return Err("wstat did not truncate".into());
    }
    if std::fs::remove_dir(&dir).is_ok() {
        return Err("removed a non-empty directory".into());
    }
    std::fs::remove_file(&b).map_err(io)?;
    std::fs::remove_dir(&dir).map_err(io)?;
    match purgatorio::stat(&dir) {
        Ok(_) => Err("directory still exists".into()),
        Err(_) => Ok(()),
    }
}

fn chdir_relative(base: &str) -> Result<(), String> {
    let dir = scratch(base);
    let io = |e: std::io::Error| e.to_string();
    std::fs::create_dir(&dir).map_err(io)?;
    let back = purgatorio::dot().map_err(e)?;
    purgatorio::chdir(&dir).map_err(e)?;
    std::fs::write("rel", "relative").map_err(io)?;
    let dot = purgatorio::dot().map_err(e)?;
    let via_sdk = purgatorio::open("rel", mode::OREAD).map_err(e)?;
    let named = purgatorio::fd2path(via_sdk.as_raw_fd()).map_err(e)?;
    let absolute = std::fs::read_to_string(format!("{dir}/rel")).map_err(io)?;
    purgatorio::chdir(&back).map_err(e)?;
    let content = std::fs::read_to_string(format!("{dir}/rel")).map_err(io)?;
    std::fs::remove_file(format!("{dir}/rel")).map_err(io)?;
    std::fs::remove_dir(&dir).map_err(io)?;
    if dot != dir || named != format!("{dir}/rel") || content != "relative" || absolute != content {
        return Err(format!(
            "dot {dot:?} fd2path {named:?} content {content:?} absolute {absolute:?}"
        ));
    }
    Ok(())
}

fn bind_and_unmount(base: &str) -> Result<(), String> {
    let dir = scratch(base);
    std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
    let res = (|| {
        purgatorio::bind("#c", &dir, mount::MREPL).map_err(e)?;
        purgatorio::stat(&format!("{dir}/cons")).map_err(e)?;
        purgatorio::unmount(None, &dir).map_err(e)?;
        match purgatorio::stat(&format!("{dir}/cons")) {
            Ok(_) => Err(String::from("still bound after unmount")),
            Err(_) => Ok(()),
        }
    })();
    std::fs::remove_dir(&dir).map_err(|e| e.to_string())?;
    res
}

fn child(what: &str) -> Result<(), Error> {
    match what {
        "env" => println!(
            "{}",
            std::env::var("PROCSVAR").unwrap_or_else(|_| "unset".into())
        ),
        "fds" => {
            let state = |fd| match purgatorio::fstat(fd) {
                Ok(_) => "open",
                Err(_) => "closed",
            };
            println!("0 {}, 1 {}", state(0), state(1));
        }
        _ => return Err(Error::from("unknown child")),
    }
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("-child") {
        return child(args.get(2).map_or("", String::as_str));
    }
    let base = args.get(1).map_or("/tmp", String::as_str);
    let checks: &[(&str, Check)] = &[
        ("spawn_echo", |_| spawn_echo()),
        ("child_failure_status", |_| child_failure_status()),
        ("spawn_missing", |_| spawn_missing()),
        ("env_inherited", |_| env_inherited()),
        ("newfd_child", |_| newfd_child()),
        ("std_sleep", |_| std_sleep()),
        ("files", files),
        ("chdir_relative", chdir_relative),
        ("bind_and_unmount", bind_and_unmount),
    ];
    let mut failed = false;
    for (name, check) in checks {
        match check(base) {
            Ok(()) => println!("ok {name}"),
            Err(why) => {
                println!("FAIL {name}: {why}");
                failed = true;
            }
        }
    }
    if failed {
        purgatorio::exits("fail:procs");
    }
    Ok(())
}
