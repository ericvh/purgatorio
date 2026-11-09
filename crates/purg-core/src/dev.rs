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
    /// Walks from directory `c` to its child `name`.
    fn walk(&self, c: &Chan, name: &str) -> Result<Walked>;
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
        Ok(Chan::new(dev.clone(), 0, w.qid, name, w.aux))
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
