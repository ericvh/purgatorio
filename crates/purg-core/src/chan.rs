use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;

use purg_platform::sync::Mutex;
use purg_styx::Qid;

use crate::dev::Dev;

/// Device-private data attached to a Chan at attach or walk time. It is
/// shared by clones of an unopened Chan.
pub type Aux = Option<Arc<dyn Any + Send + Sync>>;

/// One directory of a union, with the bind flags it was added with.
#[derive(Clone)]
pub struct Member {
    pub chan: Arc<Chan>,
    pub flags: u32,
}

impl Member {
    /// Whether `create` in the union may use this member (`MCREATE`).
    pub fn can_create(&self) -> bool {
        self.flags & purg_abi::mount::MCREATE != 0
    }
}

/// An open or walkable reference to a file, as Inferno's `Chan`.
///
/// A Chan is created by attach or walk (unopened), becomes open through
/// [`Dev::open`], and calls [`Dev::close`] when an open Chan is dropped.
pub struct Chan {
    pub dev: Arc<dyn Dev>,
    /// Device instance (Inferno's `c->dev`), distinguishing attaches.
    pub devno: u32,
    pub qid: Qid,
    /// The name this Chan was reached by, for `fd2path`.
    pub name: String,
    pub aux: Aux,
    /// Open mode; meaningful only when `open` is set.
    pub mode: u8,
    pub open: bool,
    pub offset: Mutex<u64>,
    /// Per-open device state, set by [`Dev::open`].
    pub state: Mutex<Option<Box<dyn Any + Send>>>,
    /// Members of the union directory this Chan was resolved to, in search
    /// order, when the name crossed a mount point. The first member is the
    /// Chan itself.
    pub union: Option<Arc<[Member]>>,
    /// Snapshot of packed directory entries for sequential directory reads.
    pub(crate) dirbuf: Mutex<Vec<u8>>,
}

impl Chan {
    pub fn new(dev: Arc<dyn Dev>, devno: u32, qid: Qid, name: String, aux: Aux) -> Chan {
        Chan {
            dev,
            devno,
            qid,
            name,
            aux,
            mode: 0,
            open: false,
            offset: Mutex::new(0),
            state: Mutex::new(None),
            union: None,
            dirbuf: Mutex::new(Vec::new()),
        }
    }

    /// A fresh unopened Chan for the same file.
    pub fn clone_unopened(&self) -> Chan {
        let mut c = Chan::new(
            self.dev.clone(),
            self.devno,
            self.qid,
            self.name.clone(),
            self.aux.clone(),
        );
        c.union = self.union.clone();
        c
    }

    /// Mount-table identity: Inferno's `eqchan` without the qid version.
    pub fn key(&self) -> (char, u32, u64) {
        (self.dev.dc(), self.devno, self.qid.path)
    }

    pub fn is_dir(&self) -> bool {
        self.qid.is_dir()
    }

    /// Downcasts [`Chan::aux`] to the device's concrete type.
    pub fn aux<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.aux.as_deref()?.downcast_ref()
    }
}

impl Drop for Chan {
    fn drop(&mut self) {
        if self.open {
            self.dev.close(self);
        }
    }
}

impl fmt::Debug for Chan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Chan")
            .field("dev", &self.dev.dc())
            .field("name", &self.name)
            .field("qid", &self.qid)
            .field("open", &self.open)
            .finish_non_exhaustive()
    }
}
