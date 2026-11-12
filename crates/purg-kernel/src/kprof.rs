//! `#T`: the kernel profiler, after Inferno's kprof(3).
//!
//! With `kprof` in the bootargs (`kprof=HZ` for a rate other than 1,000
//! samples a second) the kernel binds `#T` after `/dev`: `kpctl` and
//! `kpdata`. The EL1 physical timer, which the scheduler does not use,
//! interrupts the CPU at the rate; while profiling is on, each interrupt
//! counts the interrupted PC in a histogram over the kernel's text, in
//! 8-byte buckets.
//!
//! - `kpctl` takes `start`, `stop` and `startclr` (zero the counts, then
//!   start).
//! - `kpdata` is the counts as big-endian 32-bit words: the samples taken,
//!   the samples whose PC was in the kernel's text, then one word per
//!   bucket from the start of the text.
//! - `kpcaller` is the same histogram for the link register (`x30`), so a
//!   leaf such as `memcpy` can be charged to its caller. It is not in
//!   Inferno.
//!
//! `cargo xtask bench --kprof` profiles each benchmark and symbolizes the
//! counts from the kernel's ELF (`docs/perf.md`). The timer is armed on
//! the CPU that starts profiling, so with several CPUs only that one is
//! sampled; the benchmarks run on one. Code that runs with interrupts
//! masked is charged to where it unmasks them.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use purg_aarch64::{boot, irq, mmu, timer};
use purg_abi::{error, mode};
use purg_core::chan::Chan;
use purg_core::dev::{Dev, Walked};
use purg_core::error::{Error, Result};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};
use spin::Once;

/// Bytes of text per bucket.
const BUCKET: u64 = 8;

struct Hist {
    /// The text's first virtual address.
    base: u64,
    pc: Box<[AtomicU32]>,
    lr: Box<[AtomicU32]>,
    total: AtomicU32,
    inside: AtomicU32,
}

static HIST: Once<Hist> = Once::new();
static ON: AtomicBool = AtomicBool::new(false);
static PERIOD_NS: AtomicU64 = AtomicU64::new(1_000_000);

fn count(h: &[AtomicU32], base: u64, at: u64) -> bool {
    let i = at.wrapping_sub(base) / BUCKET;
    match usize::try_from(i).ok().and_then(|i| h.get(i)) {
        Some(c) => {
            c.fetch_add(1, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

fn tick() -> irq::After {
    let Some(h) = HIST.get() else {
        timer::phys_disable();
        return irq::After::Return;
    };
    if !ON.load(Ordering::Relaxed) {
        timer::phys_disable();
        return irq::After::Return;
    }
    timer::phys_arm_after_ns(PERIOD_NS.load(Ordering::Relaxed));
    let (pc, lr) = irq::interrupted();
    h.total.fetch_add(1, Ordering::Relaxed);
    if count(&h.pc, h.base, pc) {
        h.inside.fetch_add(1, Ordering::Relaxed);
    }
    count(&h.lr, h.base, lr);
    irq::After::Return
}

/// Sets the profiler up, when the bootargs ask for it (`on`): the
/// histogram and the timer's interrupt. Call after `irq::init` and before
/// the secondary CPUs start.
pub fn init(on: bool, phys_intid: Option<u32>, hz: Option<&str>) {
    let Some(intid) = phys_intid.filter(|_| on) else {
        return;
    };
    let hz: u64 = hz.and_then(|s| s.parse().ok()).unwrap_or(1000).max(1);
    PERIOD_NS.store(1_000_000_000 / hz, Ordering::Relaxed);
    let text = boot::image().text;
    let n = (text.size as u64).div_ceil(BUCKET) as usize;
    let zeros = |n: usize| (0..n).map(|_| AtomicU32::new(0)).collect::<Box<[_]>>();
    HIST.call_once(|| Hist {
        base: (mmu::KERNEL_VBASE + text.base) as u64,
        pc: zeros(n),
        lr: zeros(n),
        total: AtomicU32::new(0),
        inside: AtomicU32::new(0),
    });
    irq::register(intid, tick);
}

/// `#T`, when [`init`] set the profiler up.
pub fn dev(user: &str) -> Option<Arc<dyn Dev>> {
    HIST.get()?;
    Some(Arc::new(DevKprof {
        user: String::from(user),
    }))
}

fn clear(h: &Hist) {
    for c in h.pc.iter().chain(h.lr.iter()) {
        c.store(0, Ordering::Relaxed);
    }
    h.total.store(0, Ordering::Relaxed);
    h.inside.store(0, Ordering::Relaxed);
}

fn ctl(cmd: &str) -> Result<()> {
    let h = HIST.get().ok_or_else(|| Error::new(error::ENONEXIST))?;
    match cmd {
        "startclr" => {
            ON.store(false, Ordering::Relaxed);
            clear(h);
            start();
        }
        "start" => start(),
        "stop" => {
            ON.store(false, Ordering::Relaxed);
            timer::phys_disable();
        }
        _ => return Err(Error::new(error::EBADARG)),
    }
    Ok(())
}

fn start() {
    ON.store(true, Ordering::Relaxed);
    timer::phys_arm_after_ns(PERIOD_NS.load(Ordering::Relaxed));
}

const QDIR: u64 = 0;
const QCTL: u64 = 1;
const QDATA: u64 = 2;
const QCALLER: u64 = 3;
const FILES: [(&str, u64, u32); 3] = [
    ("kpctl", QCTL, 0o220),
    ("kpdata", QDATA, 0o444),
    ("kpcaller", QCALLER, 0o444),
];

struct DevKprof {
    user: String,
}

impl DevKprof {
    fn dir(&self, path: u64) -> Dir {
        let (name, qtype, perm) = FILES
            .iter()
            .find(|f| f.1 == path)
            .map_or(("#T", QTDIR, DMDIR | 0o555), |f| (f.0, QTFILE, f.2));
        let length = match path {
            QDATA | QCALLER => HIST.get().map_or(0, |h| 8 + 4 * h.pc.len() as u64),
            _ => 0,
        };
        Dir {
            dtype: 'T' as u16,
            qid: Qid {
                path,
                vers: 0,
                qtype,
            },
            mode: perm,
            length,
            name: String::from(name),
            uid: self.user.clone(),
            gid: self.user.clone(),
            ..Dir::default()
        }
    }
}

impl Dev for DevKprof {
    fn dc(&self) -> char {
        'T'
    }

    fn name(&self) -> &'static str {
        "kprof"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: Qid {
                path: QDIR,
                vers: 0,
                qtype: QTDIR,
            },
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        if c.qid.path != QDIR {
            return Err(Error::new(error::ENOTDIR));
        }
        let f = FILES
            .iter()
            .find(|f| f.0 == name)
            .ok_or_else(|| Error::new(error::ENONEXIST))?;
        Ok(Walked {
            qid: Qid {
                path: f.1,
                vers: 0,
                qtype: QTFILE,
            },
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.dir(c.qid.path))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let access = omode & mode::ACCESS;
        match c.qid.path {
            QDIR | QDATA | QCALLER if access != mode::OREAD => Err(Error::new(error::EPERM)),
            QCTL if access != mode::OWRITE => Err(Error::new(error::EPERM)),
            _ => Ok(()),
        }
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let h = HIST.get().ok_or_else(|| Error::new(error::ENONEXIST))?;
        let counts = match c.qid.path {
            QDATA => &h.pc,
            QCALLER => &h.lr,
            _ => return Err(Error::new(error::EISDIR)),
        };
        let word = |i: usize| -> u32 {
            match i {
                0 => h.total.load(Ordering::Relaxed),
                1 => h.inside.load(Ordering::Relaxed),
                _ => counts[i - 2].load(Ordering::Relaxed),
            }
        };
        let len = 8 + 4 * counts.len();
        let Ok(off) = usize::try_from(offset) else {
            return Ok(0);
        };
        let end = len.min(off.saturating_add(buf.len()));
        let mut n = 0;
        for at in off..end {
            buf[n] = word(at / 4).to_be_bytes()[at % 4];
            n += 1;
        }
        Ok(n)
    }

    fn dirread(&self, _c: &Chan) -> Result<Vec<Dir>> {
        Ok(FILES.iter().map(|f| self.dir(f.1)).collect())
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        if c.qid.path != QCTL {
            return Err(Error::new(error::EPERM));
        }
        let s = core::str::from_utf8(buf).map_err(|_| Error::new(error::EBADARG))?;
        ctl(s.trim())?;
        Ok(buf.len())
    }
}
