use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::error;
use purg_styx::{Dir, Qid};

use crate::chan::{Aux, Chan};
use crate::error::{Error, Result};

/// The result of attaching to a device or walking one step in it.
pub struct Walked {
    pub qid: Qid,
    pub aux: Aux,
}

/// The result of walking several names in one step ([`Dev::walk_names`]).
pub struct WalkedNames {
    /// The qids of the names walked, in order; at least one.
    pub qids: Vec<Qid>,
    /// The device's state for the last name walked, when the walk can go
    /// on from it ([`WalkEnd::Reached`]).
    pub aux: Aux,
    pub end: WalkEnd,
}

/// How a [`Dev::walk_names`] ended.
pub enum WalkEnd {
    /// Every name in `qids` was walked and the walk can go on from the
    /// last. That may be fewer names than were given: the rest are walked
    /// in another step.
    Reached,
    /// The name after the last in `qids` could not be walked: for this
    /// reason, or, when `None`, the one Inferno gives a walk that stops
    /// short (`file does not exist`, or `not a directory` below a file).
    /// Nothing was established for the names in `qids`.
    Stopped(Option<Error>),
}

/// A device driver, mirroring Inferno's `Dev` table entry.
///
/// Name resolution is lexical, so `walk` never sees `.` or `..`; directory
/// reads return whole [`Dir`]s and the core handles packing and offsets.
pub trait Dev: Send + Sync {
    /// The device character, as in `#c`.
    fn dc(&self) -> char;
    /// The driver name listed in `#c/drivers`.
    fn name(&self) -> &'static str;

    /// Called once when the device table is built.
    fn init(&self, _tab: &DevTab) {}

    fn attach(&self, spec: &str) -> Result<Walked>;
    /// The device instance (`Chan::devno`) an attach to `spec` gives, so
    /// that the files of two instances never share a mount-table key.
    /// Called after a successful [`Dev::attach`].
    fn devno(&self, _spec: &str) -> u32 {
        0
    }
    /// Walks from directory `c` to its child `name`.
    fn walk(&self, c: &Chan, name: &str) -> Result<Walked>;
    /// The most names [`Dev::walk_names`] takes at once. A device that
    /// walks one name at a time keeps the default of 1.
    fn max_walk(&self) -> usize {
        1
    }
    /// Walks from directory `c` through `names` (at most
    /// [`Dev::max_walk`] of them, at least one) in one step, as a 9P
    /// `Twalk` with several names does (`#M`), or a whole-path walk
    /// (`#λ`). Fails when the first name cannot be walked. The namespace
    /// looks the names walked past up in the mount table afterwards, and
    /// goes on from the first mount point among them.
    fn walk_names(&self, c: &Chan, names: &[&str]) -> Result<WalkedNames> {
        let first = names.first().ok_or(Error::new(error::ENONEXIST))?;
        let w = self.walk(c, first)?;
        Ok(WalkedNames {
            qids: alloc::vec![w.qid],
            aux: w.aux,
            end: WalkEnd::Reached,
        })
    }
    fn stat(&self, c: &Chan) -> Result<Dir>;
    /// Prepares `c` for I/O in `mode`; may store per-open state in
    /// [`Chan::state`]. The core sets `open` and `mode` afterwards.
    fn open(&self, c: &mut Chan, mode: u8) -> Result<()>;
    /// Reads from a file (never a directory) at `offset`.
    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize>;
    /// Lists directory `c`. It need not be open.
    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>>;
    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize>;
    /// Called when an open Chan is dropped.
    fn close(&self, _c: &Chan) {}

    /// Creates `name` in directory `dir` with Inferno permission bits `perm`
    /// (`DMDIR` for a directory). An existing file is truncated, as Inferno's
    /// `create` does. The core opens the result in the requested mode.
    fn create(&self, _dir: &Chan, _name: &str, _perm: u32) -> Result<Walked> {
        Err(Error::new(error::EPERM))
    }
    fn remove(&self, _c: &Chan) -> Result<()> {
        Err(Error::new(error::EPERM))
    }
    /// Applies the non-null fields of `d` (see [`Dir::null`]).
    fn wstat(&self, _c: &Chan, _d: &Dir) -> Result<()> {
        Err(Error::new(error::EPERM))
    }

    /// Largest atomic I/O, or 0 when the device does not limit it.
    fn iounit(&self, _c: &Chan) -> u32 {
        0
    }

    /// Attach name of a mounted channel (`#M`), empty otherwise.
    fn aname(&self, _c: &Chan) -> String {
        String::new()
    }

    /// For the root a mount attached (`#M`), the name of the file the 9P
    /// conversation is on, which `ns` shows as the mount's source. `None`
    /// for every other Chan, including the files walked to below that
    /// root: `ns` shows those as binds.
    fn mount_source(&self, _c: &Chan) -> Option<String> {
        None
    }

    /// Whether a read (`write == false`) or write would not block.
    fn ready(&self, _c: &Chan, _write: bool) -> bool {
        true
    }

    /// Wakes `w` when [`Dev::ready`] may have changed.
    fn watch(&self, _c: &Chan, _write: bool, _w: &purg_platform::sync::Rendez) {}

    fn unwatch(&self, _c: &Chan, _w: &purg_platform::sync::Rendez) {}
}

/// The device table: Inferno's `devtab`.
pub struct DevTab {
    devs: Vec<Arc<dyn Dev>>,
}

impl DevTab {
    pub fn new(devs: Vec<Arc<dyn Dev>>) -> Arc<DevTab> {
        let tab = Arc::new(DevTab { devs });
        for d in &tab.devs {
            d.init(&tab);
        }
        tab
    }

    pub fn lookup(&self, dc: char) -> Option<&Arc<dyn Dev>> {
        self.devs.iter().find(|d| d.dc() == dc)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Dev>> {
        self.devs.iter()
    }

    /// Attaches to `#<dc><spec>`, returning the device's root Chan.
    pub fn attach(&self, dc: char, spec: &str) -> Result<Chan> {
        let dev = self.lookup(dc).ok_or(Error::new(error::EBADSHARP))?;
        let w = dev.attach(spec)?;
        let mut name = String::from("#");
        name.push(dc);
        name.push_str(spec);
        Ok(Chan::new(dev.clone(), dev.devno(spec), w.qid, name, w.aux))
    }
}

/// Helpers for devices that serve string-valued files.
pub fn read_str(s: &[u8], buf: &mut [u8], offset: u64) -> usize {
    let Ok(off) = usize::try_from(offset) else {
        return 0;
    };
    if off >= s.len() {
        return 0;
    }
    let n = buf.len().min(s.len() - off);
    buf[..n].copy_from_slice(&s[off..off + n]);
    n
}
