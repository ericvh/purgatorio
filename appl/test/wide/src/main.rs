//! Conformance for the SDK calls that a wasm64 guest makes through
//! `purgatorio64` and a wasm32 guest through WASI and `purgatorio`
//! (`docs/wasm64.md`). The same source is built for both widths, as
//! `/bin/test/wide.wasm` and `/bin/64/test/wide.wasm`, and both must print
//! the same lines but the first.
//!
//! wide [dir]: `dir` is a scratch directory, `/tmp` by default.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};

use purgatorio::abi::mode;
use purgatorio::fd::AsRawFd;
use purgatorio::fs::{self, File, OpenOptions};
use purgatorio::{Context, Error, Ready, println};

fn check(name: &str, ok: bool) {
    println!("{} {name}", if ok { "ok" } else { "FAIL" });
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    println!("pointer {} bits", usize::BITS);
    check(
        "argv0",
        args.first().is_some_and(|a| a.ends_with("wide.wasm")),
    );
    println!("args {:?}", &args[1.min(args.len())..]);
    let dir = args.get(1).map_or("/tmp", String::as_str);
    let path = format!("{dir}/wide.{}", purgatorio::pid());

    // Create, write, read back, seek.
    fs::write(&path, b"hello, wide world\n")?;
    check(
        "read_to_string",
        fs::read_to_string(&path)? == "hello, wide world\n",
    );
    let mut f = OpenOptions::new().read(true).write(true).open(&path)?;
    check("seek end", f.seek(SeekFrom::End(0))? == 18);
    f.write_all(b"second line\n")?;
    check("seek start", f.seek(SeekFrom::Start(7))? == 7);
    let mut word = [0u8; 4];
    f.read_exact(&mut word)?;
    check("read at offset", &word == b"wide");
    let lines: Vec<String> = BufReader::new(File::open(&path)?)
        .lines()
        .collect::<Result<_, _>>()?;
    check("lines", lines == ["hello, wide world", "second line"]);

    // Stat, by name and by fd, and the directory.
    let d = purgatorio::stat(&path)?;
    check("stat length", d.length == 30);
    check("fstat", purgatorio::fstat(f.as_raw_fd())?.name == d.name);
    check(
        "read_dir",
        fs::read_dir(dir)?.any(|e| e.is_ok_and(|e| e.file_name().to_string_lossy() == d.name)),
    );
    check("fd2path", purgatorio::fd2path(f.as_raw_fd())? == path);
    drop(f);

    // Errors carry Inferno's strings.
    fs::remove_file(&path)?;
    let err = File::open(&path).map_err(Error::from).err();
    check(
        "errstr",
        err.is_some_and(|e| e.0.ends_with("file does not exist")),
    );
    purgatorio::werrstr("set by hand");
    check("werrstr", purgatorio::errstr() == "set by hand");

    // A pipe, and waiting on it.
    let [r, w] = purgatorio::pipe()?;
    let (mut r, mut w) = (File::from(r), File::from(w));
    check(
        "not ready",
        purgatorio::ready(&[Ready::Read(r.as_raw_fd())], 0)?.is_none(),
    );
    w.write_all(b"through a pipe")?;
    check(
        "ready",
        purgatorio::ready(&[Ready::Read(r.as_raw_fd())], 1000)? == Some(0),
    );
    let mut buf = [0u8; 64];
    let n = r.read(&mut buf)?;
    check("pipe", &buf[..n] == b"through a pipe");

    // The namespace and the environment are files.
    purgatorio::env::set("wide", "as a file")?;
    check(
        "env",
        purgatorio::env::get("wide").as_deref() == Some("as a file"),
    );

    // A child of the other width: `echo` in /bin is a wasm32 module.
    let [r, w] = purgatorio::pipe()?;
    let argv = [
        String::from("echo"),
        String::from("from"),
        String::from("a child"),
    ];
    let pid = purgatorio::spawn("echo", &argv, 0, &[(w.as_raw_fd(), 1)])?;
    drop(w);
    let mut out = String::new();
    File::from(r).read_to_string(&mut out)?;
    check("spawn", out == "from a child\n");
    let msg = purgatorio::wait()?;
    check("wait", msg.pid == pid && msg.status.is_empty());

    // Clocks, sleep and random bytes.
    let (t0, wall) = (purgatorio::time::mono(), purgatorio::time::now());
    purgatorio::time::sleep(20);
    let slept = purgatorio::time::mono() - t0;
    check("sleep", (20_000_000..5_000_000_000).contains(&slept));
    check("clock", wall > 1_700_000_000);
    let (mut a, mut b) = ([0u8; 16], [0u8; 16]);
    purgatorio::auth::Rng::fill(&mut a);
    purgatorio::auth::Rng::fill(&mut b);
    check("random", a != b);

    // A pointer outside the guest's memory is refused, not followed.
    let mut buf = [0u8; 128];
    let bad = (usize::MAX - 4096) as *const u8;
    let n = unsafe { purgatorio::sys::stat(bad, 8, buf.as_mut_ptr(), buf.len()) };
    check(
        "bad pointer",
        n == -1 && purgatorio::errstr() == purgatorio::abi::error::EBADADDR,
    );
    let n = unsafe { purgatorio::sys::stat(c"/".as_ptr().cast(), usize::MAX, buf.as_mut_ptr(), 1) };
    check(
        "bad length",
        n == -1 && purgatorio::errstr() == purgatorio::abi::error::EBADADDR,
    );

    // A raw open with an Inferno mode.
    check(
        "open mode",
        purgatorio::open("/dev/null", mode::ORDWR).is_ok(),
    );
    check(
        "open missing",
        purgatorio::open("/no/such", mode::OREAD).is_err(),
    );
    Ok(())
}
