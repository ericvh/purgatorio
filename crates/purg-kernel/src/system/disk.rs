//! Storage on the kernel, second pass (K3, D7): virtio-blk disks as `#S`,
//! and a purgfs file system (`docs/fs.md`) on one of them as `#K`, bound on
//! `/n/disk`.
//!
//! `#S` is Inferno's storage device: one directory per disk, `sdV0`,
//! `sdV1`, ... in DTB order, each with `ctl` (what the disk is: its serial
//! number, its size in blocks and the block size) and `data`, the disk's
//! bytes, read and written at any offset (partial blocks are read and
//! written back whole). It is bound after `/dev`.
//!
//! At boot the kernel mounts the first disk that holds a purgfs and binds
//! it on [`MOUNT_POINT`], in test and interactive modes alike. Reads from
//! the disk can be interrupted by a kill; writes and flushes cannot (the
//! file system's commits must finish).
//!
//! The test mode works on the image `xtask kernel` builds ([`STEPS`],
//! then [`System::disk_tests`]): files, a rename, a large file, the disk
//! filled to `file system full`, a kill during a virtio-blk wait on a
//! throttled disk, and the files a second boot finds again
//! ([`System::disk_persist`]).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use purg_aarch64::blk::{Blk, SECTOR, Wait};
use purg_abi::{error, mode};
use purg_core::{Chan, Dev, Error, Platform, Prog, Result, Walked};
use purg_fs::dev::DevFs;
use purg_fs::{Fs, Mount};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use super::System;

/// One check of [`System::disk_tests`].
type Check<'a> = dyn Fn() -> core::result::Result<String, String> + 'a;

/// Where the kernel binds the file system.
pub const MOUNT_POINT: &str = "/n/disk";

/// A virtio-blk disk as a purgfs [`purg_fs::Disk`].
pub struct BlkDisk(Arc<Blk>);

impl purg_fs::Disk for BlkDisk {
    fn sector(&self) -> usize {
        self.0.block_size()
    }

    fn size(&self) -> u64 {
        self.0.size()
    }

    fn read(&mut self, off: u64, buf: &mut [u8]) -> purg_fs::Result<()> {
        self.0.read(off / SECTOR, buf, Wait::Interruptible)
    }

    fn write(&mut self, off: u64, buf: &[u8]) -> purg_fs::Result<()> {
        self.0.write(off / SECTOR, buf, Wait::Uninterruptible)
    }

    fn flush(&mut self) -> purg_fs::Result<()> {
        self.0.flush()
    }

    fn read_only(&self) -> bool {
        self.0.read_only()
    }
}

/// The disks [`find`] brought up.
pub type Blks = Vec<Arc<Blk>>;

/// The mounted file system's device.
pub type DiskFs = DevFs<BlkDisk>;

/// The disks, and the file system on one of them.
#[derive(Default)]
pub struct Disks {
    pub blks: Vec<Arc<Blk>>,
    /// The unit the file system is on, and its device once it is up.
    pub fs_unit: Option<usize>,
    pub fs: Option<Arc<DiskFs>>,
}

/// Every virtio-blk disk in the DTB, with interrupts routed (so after
/// `irq::init`).
pub fn find(fdt: &purg_aarch64::dtb::Fdt<'_>) -> Blks {
    let (blks, errors) = Blk::open_all(fdt);
    for e in errors {
        crate::kernel::println!("kernel: virtio-blk: {e}");
    }
    blks.into_iter().map(Arc::new).collect()
}

/// Metadata blocks the kernel's mount caches: 512 KiB at most, enough
/// for a directory of 2,000 entries and its parents. With purgfs's
/// default of 32, a search of a directory of more than 32 blocks (500
/// entries) evicted each block before the next search needed it, and
/// every create, walk or stat there read the whole directory from the
/// disk again (`docs/perf.md`).
const CACHE_BLOCKS: usize = 128;

fn mount_options() -> Mount {
    Mount {
        cache_blocks: CACHE_BLOCKS,
        ..Mount::default()
    }
}

/// Mounts the first disk that holds a purgfs. Runs on a kernel thread
/// (it waits for the disks).
pub fn open(blks: Blks, platform: &Arc<dyn Platform>) -> Disks {
    let mut disks = Disks {
        blks,
        ..Disks::default()
    };
    for (u, b) in disks.blks.iter().enumerate() {
        match Fs::mount(BlkDisk(b.clone()), &mount_options()) {
            Ok(fs) => {
                let use_ = fs.usage();
                crate::kernel::println!(
                    "kernel: sdV{u}: purgfs, {} blocks of {} bytes, {} free",
                    use_.blocks,
                    use_.bsize,
                    use_.free
                );
                disks.fs_unit = Some(u);
                disks.fs = Some(Arc::new(DevFs::new(fs, platform.clone())));
                break;
            }
            Err(e) => crate::kernel::println!("kernel: sdV{u}: no file system: {e}"),
        }
    }
    disks
}

/// `#S` and `#K` for the device table.
pub fn devs(disks: &Disks, user: &str) -> Vec<Arc<dyn Dev>> {
    let mut v: Vec<Arc<dyn Dev>> = Vec::new();
    if !disks.blks.is_empty() {
        v.push(Arc::new(DevSd {
            blks: disks.blks.clone(),
            user: String::from(user),
        }));
    }
    if let Some(fs) = &disks.fs {
        v.push(fs.clone());
    }
    v
}

const QROOT: u64 = 0;
const QCTL: u64 = 1;
const QDATA: u64 = 2;

/// `#S`: the disks.
pub struct DevSd {
    blks: Vec<Arc<Blk>>,
    user: String,
}

impl DevSd {
    /// Qid paths: 0 for the root, `(unit + 1) << 8 | kind` below it.
    fn qid(path: u64) -> Qid {
        let dir = path == QROOT || path & 0xff == 0;
        Qid {
            path,
            vers: 0,
            qtype: if dir { QTDIR } else { QTFILE },
        }
    }

    fn unit(&self, path: u64) -> Result<&Arc<Blk>> {
        let u = (path >> 8).checked_sub(1).ok_or(Error::new(error::EIO))?;
        self.blks
            .get(u as usize)
            .ok_or(Error::new(error::ENONEXIST))
    }

    fn dir(&self, path: u64) -> Dir {
        let u = (path >> 8).saturating_sub(1);
        let (name, mode, length) = match (path, path & 0xff) {
            (QROOT, _) => (String::from("#S"), DMDIR | 0o555, 0),
            (_, 0) => (format!("sdV{u}"), DMDIR | 0o555, 0),
            (_, QCTL) => (String::from("ctl"), 0o444, 0),
            _ => {
                let b = self.unit(path).ok();
                let ro = b.is_some_and(|b| b.read_only());
                let size = b.map_or(0, |b| b.size());
                (String::from("data"), if ro { 0o440 } else { 0o660 }, size)
            }
        };
        Dir {
            dtype: 'S' as u16,
            qid: DevSd::qid(path),
            mode,
            length,
            name,
            uid: self.user.clone(),
            gid: self.user.clone(),
            ..Dir::default()
        }
    }

    fn ctl(b: &Blk) -> String {
        let bs = b.block_size() as u64;
        let n = b.size() / bs;
        format!(
            "inquiry virtio-blk {}\ngeometry {n} {bs}\npart data 0 {n}\n",
            if b.serial().is_empty() {
                "-"
            } else {
                b.serial()
            }
        )
    }
}

/// `[off, off + len)` widened to whole blocks of `bs`, within `size`.
fn span(off: u64, len: usize, bs: u64, size: u64) -> (u64, u64) {
    let end = off.saturating_add(len as u64).min(size);
    (off / bs * bs, end.div_ceil(bs) * bs)
}

impl Dev for DevSd {
    fn dc(&self) -> char {
        'S'
    }

    fn name(&self) -> &'static str {
        "sd"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: DevSd::qid(QROOT),
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let path = c.qid.path;
        let next = if path == QROOT {
            let u = name
                .strip_prefix("sdV")
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|&u| u < self.blks.len() && name == format!("sdV{u}"))
                .ok_or(Error::new(error::ENONEXIST))?;
            ((u as u64) + 1) << 8
        } else if path & 0xff == 0 {
            match name {
                "ctl" => path | QCTL,
                "data" => path | QDATA,
                _ => return Err(Error::new(error::ENONEXIST)),
            }
        } else {
            return Err(Error::new(error::ENOTDIR));
        };
        Ok(Walked {
            qid: DevSd::qid(next),
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.dir(c.qid.path))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let path = c.qid.path;
        let write = matches!(omode & mode::ACCESS, mode::OWRITE | mode::ORDWR);
        if DevSd::qid(path).qtype & QTDIR != 0 || path & 0xff == QCTL {
            return if write {
                Err(Error::new(if path & 0xff == QCTL {
                    error::EPERM
                } else {
                    error::EISDIR
                }))
            } else {
                Ok(())
            };
        }
        if write && self.unit(path)?.read_only() {
            return Err(Error::new(error::EPERM));
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let path = c.qid.path;
        let b = self.unit(path)?;
        if path & 0xff == QCTL {
            return Ok(purg_core::dev::read_str(
                DevSd::ctl(b).as_bytes(),
                buf,
                offset,
            ));
        }
        let size = b.size();
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let bs = b.block_size() as u64;
        let (lo, hi) = span(offset, buf.len(), bs, size);
        let mut tmp = vec![0u8; (hi - lo) as usize];
        b.read(lo / SECTOR, &mut tmp, Wait::Interruptible)?;
        let skip = (offset - lo) as usize;
        let n = buf.len().min((size - offset) as usize);
        buf[..n].copy_from_slice(&tmp[skip..skip + n]);
        Ok(n)
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let path = c.qid.path;
        if path == QROOT {
            return Ok((0..self.blks.len() as u64)
                .map(|u| self.dir((u + 1) << 8))
                .collect());
        }
        if path & 0xff != 0 {
            return Err(Error::new(error::ENOTDIR));
        }
        Ok(vec![self.dir(path | QCTL), self.dir(path | QDATA)])
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        let path = c.qid.path;
        if path & 0xff != QDATA {
            return Err(Error::new(error::EPERM));
        }
        let b = self.unit(path)?;
        let size = b.size();
        if offset >= size {
            return Err(Error::new(error::EIO));
        }
        let n = buf.len().min((size - offset) as usize);
        let bs = b.block_size() as u64;
        let (lo, hi) = span(offset, n, bs, size);
        let mut tmp = vec![0u8; (hi - lo) as usize];
        let skip = (offset - lo) as usize;
        // The partial blocks at either end keep what they held.
        if skip != 0 || !n.is_multiple_of(bs as usize) {
            b.read(lo / SECTOR, &mut tmp, Wait::Uninterruptible)?;
        }
        tmp[skip..skip + n].copy_from_slice(&buf[..n]);
        b.write(lo / SECTOR, &tmp, Wait::Uninterruptible)?;
        Ok(n)
    }
}

/// Binds `#K` on [`MOUNT_POINT`] in `prog`'s namespace, making the mount
/// point first.
pub fn bind(prog: &mut Prog) -> Result<()> {
    for d in ["/n", MOUNT_POINT] {
        if prog.stat(d).is_err() {
            let fd = prog.create(d, mode::OREAD, DMDIR | 0o775)?;
            prog.close(fd)?;
        }
    }
    prog.bind(
        "#K",
        MOUNT_POINT,
        purg_abi::mount::MREPL | purg_abi::mount::MCREATE,
    )
}

/// What the kernel writes and the second boot reads back.
pub const PERSIST: &str = "written by the first boot\n";
/// What a file in the image `xtask kernel` builds holds.
const HELLO: &str = "hello from the disk image\n";

/// Commands run on the console in test mode, and whether each must
/// succeed.
const STEPS: &[(&[&str], bool)] = &[
    (&["ls", "/n/disk"], true),
    (&["cat", "/n/disk/hello.txt"], true),
    (&["cat", "/n/disk/sub/inner.txt"], true),
    (&["sh", "-c", "ls /n/disk/many | wc -l"], true),
    (
        &["sh", "-c", "echo written by the kernel > /n/disk/new.txt"],
        true,
    ),
    (&["cat", "/n/disk/new.txt"], true),
    (&["mv", "/n/disk/new.txt", "/n/disk/kept.txt"], true),
    (&["chmod", "600", "/n/disk/kept.txt"], true),
    (&["cp", "/bin/sh.wasm", "/n/disk/big.wasm"], true),
    (&["cmp", "/bin/sh.wasm", "/n/disk/big.wasm"], true),
    (&["rm", "/n/disk/big.wasm"], true),
    (&["mkdir", "/n/disk/newdir"], true),
    (&["cat", "/n/disk/nonexistent"], false),
    (&["rm", "/n/disk/sub"], false),
    (&["ls", "-l", "/dev/sdV0", "/dev/sdV1"], true),
    (&["cat", "/dev/sdV0/ctl", "/dev/sdV1/ctl"], true),
];

/// The serial number of the throttled disk the kill test reads.
const SLOW: &str = "slow";
/// How long the kill test waits for things, in ms.
const WAIT_MS: u64 = 10_000;

fn ms() -> u64 {
    purg_aarch64::timer::ticks_to_ns(purg_aarch64::timer::counter()) / 1_000_000
}

/// Sleeps for `n` ms.
fn nap(n: u64) {
    purg_platform::sync::Rendez::new().tsleep(|| false, n * 1_000_000);
}

/// Polls `cond` every 10 ms for up to `limit` ms.
fn until(limit: u64, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = ms();
    while ms() - t0 < limit {
        if cond() {
            return true;
        }
        nap(10);
    }
    cond()
}

impl System {
    /// Binds the file system on `/n/disk` in the boot namespace. `None`
    /// without one, else whether the bind worked.
    pub fn bind_disk(&self, disks: &Disks) -> Option<bool> {
        let u = disks.fs_unit?;
        disks.fs.as_ref()?;
        let mut prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
        self.kernel.platform.set_current_pid(prog.pid);
        match bind(&mut prog) {
            Ok(()) => {
                self.say(&format!("kernel: sdV{u} mounted on {MOUNT_POINT}"));
                Some(true)
            }
            Err(e) => {
                self.say(&format!("kernel: FAIL bind #K: {e}"));
                Some(false)
            }
        }
    }

    /// The disk checks, given what [`System::bind_disk`] said; returns the
    /// number that failed.
    pub fn disk_tests(&self, disks: &Disks, bound: Option<bool>) -> usize {
        let Some(fs) = disks.fs.as_ref() else {
            return 0;
        };
        match bound {
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
        let unit = disks.fs_unit.unwrap_or(0);
        let tests: [(&str, &Check<'_>); 4] = [
            ("disk: stat, wstat and errors", &|| self.disk_stat()),
            ("disk: fill it", &|| self.disk_fill(fs)),
            ("disk: check it", &|| self.disk_check(fs, unit)),
            ("disk: kill a reader of a slow disk", &|| {
                self.blk_kill(&disks.blks)
            }),
        ];
        for (what, t) in tests {
            self.say(&format!("kernel: {what}"));
            let v = t();
            self.verdict(&mut failed, v);
        }
        failed
    }

    fn disk_stat(&self) -> core::result::Result<String, String> {
        let prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
        let stat = |p: &str| prog.stat(p).map_err(|e| format!("stat {p}: {e}"));
        let d = stat("/n/disk/kept.txt")?;
        if d.name != "kept.txt" || d.length != 22 || d.qid.is_dir() || d.mode != 0o600 {
            return Err(format!("kept.txt: {d:?}"));
        }
        if d.uid != "inferno" || d.dtype != 'K' as u16 || d.qid.vers == 0 {
            return Err(format!("kept.txt: {d:?}"));
        }
        let d = stat("/n/disk/hello.txt")?;
        if d.length != HELLO.len() as u64 || d.mtime == 0 {
            return Err(format!("hello.txt: {d:?}"));
        }
        let d = stat("/n/disk/sub")?;
        if !d.qid.is_dir() || d.mode & DMDIR == 0 || d.length != 0 {
            return Err(format!("sub: {d:?}"));
        }
        let mut w = Dir::null();
        w.name = String::from("renamed.txt");
        prog.wstat("/n/disk/hello.txt", &w)
            .map_err(|e| format!("wstat: {e}"))?;
        w.name = String::from("hello.txt");
        prog.wstat("/n/disk/renamed.txt", &w)
            .map_err(|e| format!("wstat back: {e}"))?;
        let mut w = Dir::null();
        w.uid = String::from("glenda");
        match prog.wstat("/n/disk/hello.txt", &w) {
            Err(e) if e.as_str() == error::EPERM => {}
            r => return Err(format!("wstat uid: {r:?}")),
        }
        match prog.stat("/n/disk/renamed.txt") {
            Err(e) if e.as_str() == error::ENONEXIST => {}
            r => return Err(format!("stat renamed.txt: {r:?}")),
        }
        Ok(String::from("ok"))
    }

    /// Writes a file until the disk is full, checks the error, and removes
    /// it; the free blocks must come back.
    fn disk_fill(&self, fs: &DiskFs) -> core::result::Result<String, String> {
        let mut prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
        self.kernel.platform.set_current_pid(prog.pid);
        let free = fs.usage().free;
        let path = "/n/disk/fill";
        let fd = prog
            .create(path, mode::OWRITE, 0o664)
            .map_err(|e| format!("create: {e}"))?;
        let chunk = vec![0x5au8; 64 << 10];
        let mut total = 0u64;
        let err = loop {
            match prog.write(fd, &chunk) {
                Ok(0) => break String::from("a write of 0"),
                Ok(n) => total += n as u64,
                Err(e) => break String::from(e.as_str()),
            }
            if total > 1 << 30 {
                break String::from("no end");
            }
        };
        let _ = prog.close(fd);
        let left = fs.usage().free;
        prog.remove(path).map_err(|e| format!("remove: {e}"))?;
        if err != error::ENOSPACE || left != 0 {
            return Err(format!("{err} after {total} bytes, {left} blocks free"));
        }
        if fs.usage().free != free {
            return Err(format!("{} blocks free after, not {free}", fs.usage().free));
        }
        Ok(format!("{err} after {} blocks", total.div_ceil(4096)))
    }

    /// Writes the file the second boot looks for, then checks the whole
    /// file system and the raw disk with `disk/chkdisk`.
    fn disk_check(&self, fs: &DiskFs, unit: usize) -> core::result::Result<String, String> {
        let cmd = format!("echo {} > /n/disk/persist.txt", PERSIST.trim_end());
        match self.run(&["sh", "-c", &cmd], None) {
            Ok(s) if s.is_empty() => {}
            r => return Err(format!("{cmd}: {r:?}")),
        }
        let r = fs.check();
        if !r.ok() {
            return Err(format!("check: {:?}", r.errors));
        }
        fs.sync().map_err(|e| format!("sync: {e}"))?;
        let cmd = format!("disk/chkdisk /dev/sdV{unit}/data");
        match self.run(&["sh", "-c", &cmd], None) {
            Ok(s) if s.is_empty() => {}
            r => return Err(format!("{cmd}: {r:?}")),
        }
        Ok(format!(
            "{} files, {} directories, {} blocks used, {} free",
            r.files, r.dirs, r.used, r.free
        ))
    }

    /// Kills a prog blocked in a virtio-blk wait (a read of the throttled
    /// disk), then checks that the request it left finishes, frees its
    /// slot, and that the disk still reads right.
    fn blk_kill(&self, blks: &[Arc<Blk>]) -> core::result::Result<String, String> {
        let Some((u, slow)) = blks.iter().enumerate().find(|(_, b)| b.serial() == SLOW) else {
            return Err(format!("no disk with serial {SLOW:?}"));
        };
        let before = slow.stats();
        let data = format!("/dev/sdV{u}/data");
        let mut prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
        match prog.open(&data, mode::OWRITE) {
            Err(e) if e.as_str() == error::EPERM => {}
            r => return Err(format!("open the read-only disk to write: {r:?}")),
        }
        drop(prog);
        let pid = self
            .spawn(&["cat", &data], "/dev/null")
            .map_err(|e| format!("cat {data}: {e}"))?;
        // A request held up by the throttle, for a while.
        let mut since = None;
        let blocked = until(WAIT_MS, || match (slow.in_flight(), since) {
            (0, _) => {
                since = None;
                false
            }
            (_, None) => {
                since = Some(ms());
                false
            }
            (_, Some(t)) => ms() - t >= 200,
        });
        if !blocked {
            self.kernel.procs.kill(pid);
            return Err(String::from("the reader never waited on the disk"));
        }
        self.kernel.procs.kill(pid);
        if !until(WAIT_MS, || self.kernel.procs.get(pid).is_none()) {
            return Err(String::from("the killed reader did not go"));
        }
        let after = slow.stats();
        if after.abandoned != before.abandoned + 1 {
            return Err(format!(
                "{} requests abandoned, not 1",
                after.abandoned - before.abandoned
            ));
        }
        if !until(WAIT_MS, || slow.in_flight() == 0) {
            return Err(String::from("the abandoned request never finished"));
        }
        // The queue still works, and reads what xtask wrote.
        let mut buf = vec![0u8; 512];
        slow.read(3, &mut buf, Wait::Uninterruptible)
            .map_err(|e| format!("read after: {e}"))?;
        let want = b"slow sector 3\n";
        if &buf[..want.len()] != want {
            return Err(format!(
                "sector 3 reads {:?}",
                String::from_utf8_lossy(&buf[..want.len()])
            ));
        }
        Ok(String::from(
            "killed in the wait; its request finished unread",
        ))
    }

    /// The second boot's check: the files the first left are there, and
    /// the file system checks clean.
    pub fn disk_persist(&self, disks: &Disks, bound: Option<bool>) -> usize {
        let Some(fs) = disks.fs.as_ref() else {
            self.say("kernel: FAIL no file system on the disks");
            return 1;
        };
        if bound != Some(true) {
            return 1;
        }
        let mut failed = 0;
        for (argv, ok) in [
            (&["ls", "/n/disk"][..], true),
            (&["cat", "/n/disk/persist.txt"][..], true),
            (&["cat", "/n/disk/kept.txt"][..], true),
            (&["cat", "/n/disk/fill"][..], false),
        ] {
            self.say(&format!("kernel: run {}", argv.join(" ")));
            let v = match (self.run(argv, None), ok) {
                (Ok(s), true) if s.is_empty() => Ok(String::from("ok")),
                (Ok(s), false) if !s.is_empty() => Ok(format!("failed as expected: {s}")),
                (Ok(s), _) => Err(format!("status {s:?}")),
                (Err(e), _) => Err(format!("error: {e}")),
            };
            self.verdict(&mut failed, v);
        }
        self.say("kernel: disk: check it");
        let r = fs.check();
        let got = self.read_file("/n/disk/persist.txt").unwrap_or_default();
        let v = if !r.ok() {
            Err(format!("check: {:?}", r.errors))
        } else if got != PERSIST.as_bytes() {
            Err(format!(
                "persist.txt holds {:?}",
                String::from_utf8_lossy(&got)
            ))
        } else {
            Ok(format!(
                "{} files, {} directories, {} blocks used, {} free",
                r.files, r.dirs, r.used, r.free
            ))
        };
        self.verdict(&mut failed, v);
        failed
    }
}
