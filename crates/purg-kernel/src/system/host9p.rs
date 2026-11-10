//! Storage on the kernel, first pass (K3, D7): a host directory over
//! virtio-9p (QEMU's `-fsdev local` with `-device virtio-9p-device`),
//! mounted through the ordinary 9P2000 client, `#M`.
//!
//! `#9` is the transport as a device. Its directory has one file, named
//! after the device's mount tag (`#9/host`). The file is one 9P
//! conversation: each write takes T-messages (whole or in pieces), and
//! reads return the R-messages as a byte stream, as from a pipe or a TCP
//! connection. So `mount` over an fd open on it works as over any other
//! connection, and `#M` is unchanged. One open at a time: the device has
//! one 9P session, and a second conversation would reset the first. The
//! letter is free in Inferno (9 for 9P).
//!
//! Dialect: QEMU answers `Tversion "9P2000"` with `"unknown"`; it speaks
//! only 9P2000.u and 9P2000.L. [`Dev9p::new`] asks with plain 9P2000 first
//! and, when that is refused, puts `purg_styx::dotu::Shim` between the
//! conversation and the device, so `#M` still sees 9P2000 (D8).
//!
//! At boot the kernel mounts `#9/host` on `/n/host`, Inferno's place for
//! another machine's files; the network check that mounts a hosted emu's
//! export uses `/n/remote`. In test mode [`System::host9p_tests`] works on
//! the files `xtask kernel` puts in the shared directory, and `xtask`
//! checks what the kernel left there.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use purg_aarch64::p9::Host9p;
use purg_abi::{error, mode};
use purg_core::dev::read_str;
use purg_core::{Chan, Dev, Error, Prog, Result, Walked};
use purg_platform::sync::Mutex;
use purg_styx::dotu::Shim;
use purg_styx::{DMDIR, Dir, Fcall, MAX_MSIZE, Msg, NOTAG, QTDIR, QTFILE, Qid, VERSION};

use super::System;

/// Where the kernel mounts the host's directory.
pub const MOUNT_POINT: &str = "/n/host";

const QROOT: u64 = 0;
const QDATA: u64 = 1;

/// The state of the one open conversation.
struct Conv {
    /// Bumped by each open; a reply to an earlier conversation's request
    /// (still in flight when it closed) carries an older cookie and is
    /// dropped.
    session: u64,
    /// Written bytes that are not yet a whole message.
    inbuf: Vec<u8>,
    /// The reply being read, and how much of it has been.
    out: Vec<u8>,
    outpos: usize,
    /// `Some` when the device needs 9P2000.u.
    shim: Option<Shim>,
}

/// `#9`: the virtio-9p transport as a file.
pub struct Dev9p {
    host: Host9p,
    name: String,
    dotu: bool,
    /// What the device said to plain 9P2000.
    plain_answer: String,
    busy: AtomicBool,
    conv: Mutex<Conv>,
}

impl Dev9p {
    /// Takes over the device and finds its dialect: a plain `Tversion
    /// "9P2000"` first, and 9P2000.u if that is refused. Runs on a kernel
    /// thread (it waits for the device).
    pub fn new(host: Host9p) -> Result<Dev9p> {
        let probe = Msg {
            tag: NOTAG,
            fcall: Fcall::Tversion {
                msize: MAX_MSIZE,
                version: VERSION,
            },
        }
        .encode()
        .map_err(|_| Error::new(error::EIO))?;
        host.submit(0, &probe)?;
        let (_, reply) = host.recv()?;
        let plain_answer = match Msg::decode(&reply, u32::MAX).map(|m| m.fcall) {
            Ok(Fcall::Rversion { version, .. }) => String::from(version),
            Ok(Fcall::Rerror { ename }) => format!("error {ename:?}"),
            _ => String::from("a bad reply"),
        };
        let dotu = plain_answer != VERSION;
        let name = match host.tag() {
            "" => String::from("9p"),
            t => String::from(t),
        };
        Ok(Dev9p {
            host,
            name,
            dotu,
            plain_answer,
            busy: AtomicBool::new(false),
            conv: Mutex::new(Conv {
                session: 0,
                inbuf: Vec::new(),
                out: Vec::new(),
                outpos: 0,
                shim: None,
            }),
        })
    }

    /// The file's name: the device's mount tag.
    pub fn file(&self) -> &str {
        &self.name
    }

    /// A line for the boot log: the dialect, and why.
    pub fn dialect(&self) -> String {
        if self.dotu {
            format!(
                "9P2000 through 9P2000.u (the device answered 9P2000 with {:?})",
                self.plain_answer
            )
        } else {
            String::from("9P2000")
        }
    }

    fn qid(path: u64) -> Qid {
        Qid {
            path,
            vers: 0,
            qtype: if path == QROOT { QTDIR } else { QTFILE },
        }
    }

    fn dir(&self, path: u64) -> Dir {
        let (name, mode) = if path == QROOT {
            (String::from("#9"), DMDIR | 0o555)
        } else {
            (self.name.clone(), 0o660)
        };
        Dir {
            dtype: '9' as u16,
            qid: Dev9p::qid(path),
            mode,
            name,
            uid: String::from("eve"),
            gid: String::from("eve"),
            ..Dir::default()
        }
    }
}

impl Dev for Dev9p {
    fn dc(&self) -> char {
        '9'
    }

    fn name(&self) -> &'static str {
        "virtio9p"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: Dev9p::qid(QROOT),
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        if c.qid.path != QROOT {
            return Err(Error::new(error::ENOTDIR));
        }
        if name != self.name {
            return Err(Error::new(error::ENONEXIST));
        }
        Ok(Walked {
            qid: Dev9p::qid(QDATA),
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.dir(c.qid.path))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        if c.qid.path == QROOT {
            return if omode & mode::ACCESS == mode::OREAD {
                Ok(())
            } else {
                Err(Error::new(error::EISDIR))
            };
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(Error::new(error::EINUSE));
        }
        let mut conv = self.conv.lock();
        conv.session += 1;
        conv.inbuf.clear();
        conv.out.clear();
        conv.outpos = 0;
        conv.shim = self.dotu.then(Shim::new);
        Ok(())
    }

    fn close(&self, c: &Chan) {
        if c.qid.path == QDATA {
            self.busy.store(false, Ordering::Release);
        }
    }

    /// The next bytes of the reply stream; waits for a reply when none is
    /// left. One reader at a time (the mount's reader).
    fn read(&self, c: &Chan, buf: &mut [u8], _offset: u64) -> Result<usize> {
        if c.qid.path == QROOT {
            return Err(Error::new(error::EISDIR));
        }
        loop {
            let session = {
                let mut conv = self.conv.lock();
                if conv.outpos < conv.out.len() {
                    let n = read_str(&conv.out, buf, conv.outpos as u64);
                    conv.outpos += n;
                    return Ok(n);
                }
                conv.session
            };
            let (cookie, reply) = self.host.recv()?;
            let mut conv = self.conv.lock();
            if cookie != session || conv.session != session {
                continue;
            }
            let reply = match conv.shim.as_mut() {
                // A reply the shim cannot read fails its request, not the
                // conversation.
                Some(shim) => shim.response(&reply).or_else(|_| io_error(&reply))?,
                None => reply,
            };
            conv.out = reply;
            conv.outpos = 0;
        }
    }

    fn dirread(&self, _c: &Chan) -> Result<Vec<Dir>> {
        Ok(alloc::vec![self.dir(QDATA)])
    }

    /// Takes T-messages; each whole one goes to the device.
    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        if c.qid.path == QROOT {
            return Err(Error::new(error::EISDIR));
        }
        let max = self.host.msize();
        let (session, msgs) = {
            let mut conv = self.conv.lock();
            conv.inbuf.extend_from_slice(buf);
            let mut msgs = Vec::new();
            while let Some(n) = purg_styx::message_size(&conv.inbuf) {
                if !(7..=max).contains(&n) {
                    conv.inbuf.clear();
                    return Err(Error::new(error::EIO));
                }
                if conv.inbuf.len() < n {
                    break;
                }
                let t: Vec<u8> = conv.inbuf.drain(..n).collect();
                let t = match conv.shim.as_mut() {
                    Some(shim) => shim.request(&t).map_err(|_| Error::new(error::EIO))?,
                    None => t,
                };
                msgs.push(t);
            }
            (conv.session, msgs)
        };
        for t in msgs {
            self.host.submit(session, &t)?;
        }
        Ok(buf.len())
    }
}

/// `Rerror "i/o error"` for the request `reply` answers.
fn io_error(reply: &[u8]) -> Result<Vec<u8>> {
    let tag = match reply.get(5..7) {
        Some(&[a, b]) => u16::from_le_bytes([a, b]),
        _ => return Err(Error::new(error::EIO)),
    };
    let fcall = Fcall::Rerror { ename: error::EIO };
    Msg { tag, fcall }
        .encode()
        .map_err(|_| Error::new(error::EIO))
}

/// Opens `#9/<file>` and mounts it on [`MOUNT_POINT`] in `prog`'s
/// namespace, making the mount point first.
pub fn mount(prog: &mut Prog, file: &str) -> Result<()> {
    for d in ["/n", MOUNT_POINT] {
        if prog.stat(d).is_err() {
            let fd = prog.create(d, mode::OREAD, DMDIR | 0o775)?;
            prog.close(fd)?;
        }
    }
    let fd = prog.open(&format!("#9/{file}"), mode::ORDWR)?;
    // Without MNOAUTH: the mount asks with Tauth, which QEMU refuses, and
    // the shim reports as "authentication not required".
    let flags = purg_abi::mount::MREPL | purg_abi::mount::MCREATE;
    let r = prog.mount(fd, -1, MOUNT_POINT, flags, "");
    prog.close(fd)?;
    r
}

/// Files `xtask kernel` puts in the shared directory.
const HELLO: &str = "hello from the host\n";
/// What the kernel writes, and `xtask` reads back from `kept.txt`.
pub const WRITTEN: &str = "written by the kernel\n";

/// Commands run on the console in test mode, and whether each must
/// succeed.
const STEPS: &[(&[&str], bool)] = &[
    (&["ls", "/n/host"], true),
    (&["cat", "/n/host/hello.txt"], true),
    (&["cat", "/n/host/sub/inner.txt"], true),
    (&["sh", "-c", "ls /n/host/many | wc -l"], true),
    (
        &["sh", "-c", "echo written by the kernel > /n/host/new.txt"],
        true,
    ),
    (&["cat", "/n/host/new.txt"], true),
    (&["mv", "/n/host/new.txt", "/n/host/kept.txt"], true),
    (&["chmod", "600", "/n/host/kept.txt"], true),
    (&["cp", "/bin/sh.wasm", "/n/host/big.wasm"], true),
    (&["cmp", "/bin/sh.wasm", "/n/host/big.wasm"], true),
    (&["rm", "/n/host/big.wasm"], true),
    (&["mkdir", "/n/host/newdir"], true),
    (&["cat", "/n/host/nonexistent"], false),
    (&["rm", "/n/host/sub"], false),
];

impl System {
    /// Mounts `dev`'s file on `/n/host`. `None` without a device, else
    /// whether the mount worked.
    pub fn mount_host9p(&self, dev: Option<&Dev9p>) -> Option<bool> {
        let dev = dev?;
        let file = dev.file();
        self.say(&format!("kernel: #9/{file}: {}", dev.dialect()));
        let mut prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
        self.kernel.platform.set_current_pid(prog.pid);
        match mount(&mut prog, file) {
            Ok(()) => {
                self.say(&format!("kernel: #9/{file} mounted on {MOUNT_POINT}"));
                Some(true)
            }
            Err(e) => {
                self.say(&format!("kernel: FAIL mount #9/{file}: {e}"));
                Some(false)
            }
        }
    }

    /// The virtio-9p checks, given what [`System::mount_host9p`] said;
    /// returns the number that failed.
    pub fn host9p_tests(&self, mounted: Option<bool>) -> usize {
        match mounted {
            None => return 0,
            Some(false) => return 1,
            Some(true) => {}
        }
        let mut failed = 0;
        for &(argv, ok) in STEPS {
            self.say(&format!("kernel: run {}", argv.join(" ")));
            let v = match (self.run(argv, None), ok) {
                (Ok(s), true) if s.is_empty() => Ok(String::from("ok")),
                (Ok(s), false) if !s.is_empty() => Ok(format!("failed as expected: {s}")),
                (Ok(s), _) => Err(format!("status {s:?}")),
                (Err(e), _) => Err(format!("error: {e}")),
            };
            self.verdict(&mut failed, v);
        }
        self.say("kernel: 9p: stat, wstat and errors");
        let v = self.host9p_stat().map(|()| String::from("ok"));
        self.verdict(&mut failed, v);
        failed
    }

    fn host9p_stat(&self) -> core::result::Result<(), String> {
        let prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
        let stat = |p: &str| prog.stat(p).map_err(|e| format!("stat {p}: {e}"));
        let d = stat("/n/host/kept.txt")?;
        if d.name != "kept.txt" || d.length != WRITTEN.len() as u64 || d.qid.is_dir() {
            return Err(format!("kept.txt: {d:?}"));
        }
        if d.mode != 0o600 {
            return Err(format!("kept.txt: mode {:o}, not 600", d.mode));
        }
        let d = stat("/n/host/sub")?;
        if !d.qid.is_dir() || d.mode & DMDIR == 0 || d.name != "sub" {
            return Err(format!("sub: {d:?}"));
        }
        let d = stat("/n/host/hello.txt")?;
        if d.length != HELLO.len() as u64 || d.uid.is_empty() {
            return Err(format!("hello.txt: {d:?}"));
        }
        // A rename back and forth by wstat, then the errors.
        let mut w = Dir::null();
        w.name = String::from("renamed.txt");
        prog.wstat("/n/host/hello.txt", &w)
            .map_err(|e| format!("wstat: {e}"))?;
        w.name = String::from("hello.txt");
        prog.wstat("/n/host/renamed.txt", &w)
            .map_err(|e| format!("wstat back: {e}"))?;
        let want = |r: Result<Dir>, e: &str, what: &str| match r {
            Err(got) if got.as_str() == e => Ok(()),
            Err(got) => Err(format!("{what}: {got}, not {e}")),
            Ok(_) => Err(format!("{what}: no error")),
        };
        want(
            prog.stat("/n/host/new.txt"),
            error::ENONEXIST,
            "stat new.txt",
        )?;
        want(
            prog.stat("/n/host/renamed.txt"),
            error::ENONEXIST,
            "stat renamed.txt",
        )?;
        match prog.remove("/n/host/sub") {
            Err(e) if e.as_str() == error::ENOTEMPTY => Ok(()),
            Err(e) => Err(format!("remove sub: {e}")),
            Ok(()) => Err(String::from("remove sub: no error")),
        }
    }
}

/// The first virtio-9p device in the DTB, brought up with buffers for
/// [`MAX_MSIZE`] messages and its interrupt routed (so after `irq::init`).
pub fn find(fdt: &purg_aarch64::dtb::Fdt<'_>) -> Option<Host9p> {
    match Host9p::open(fdt, MAX_MSIZE as usize) {
        Ok(h) => Some(h),
        Err(why) => {
            crate::kernel::println!("kernel: no host directory: {why}");
            None
        }
    }
}

/// The device as `#9`, once its dialect is known. Runs on a kernel thread.
pub fn open(host: Host9p) -> Option<Arc<Dev9p>> {
    match Dev9p::new(host) {
        Ok(d) => Some(Arc::new(d)),
        Err(e) => {
            crate::kernel::println!("kernel: virtio-9p: {e}");
            None
        }
    }
}
