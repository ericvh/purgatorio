//! Progs, the process table, and `pctl`.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use purg_abi::{error, pctl};
use purg_platform::Platform;
use purg_platform::sync::{Mutex, Rendez};

use purg_platform::sync::Note;

use crate::block::park;
use crate::debug::Debug;
use crate::dev::DevTab;
use crate::devenv::Egrp;
use crate::error::{Error, Result};
use crate::fgrp::Fgrp;
use crate::kchan::Kgrp;
use crate::modsig::{ModPolicy, Module};
use crate::names::quote;
use crate::pgrp::Pgrp;

/// Exit messages kept for a prog that has not read them; older ones are
/// dropped.
const MAXWAIT: usize = 1024;

/// The kernel-visible part of a prog: what devices such as `#e` and `#p`
/// need to find from the calling kproc's pid.
pub struct Proc {
    pub pid: u32,
    /// Parent pid; 0 for the first prog.
    pub ppid: u32,
    group: AtomicU32,
    name: Mutex<String>,
    egrp: Mutex<Arc<Egrp>>,
    fgrp: Mutex<Arc<Fgrp>>,
    pgrp: Mutex<Arc<Pgrp>>,
    /// Set by `kill` and by `ctl`. Blocking syscalls on this prog watch it.
    pub note: Arc<Note>,
    pub kgrp: Arc<Kgrp>,
    waits: Mutex<VecDeque<String>>,
    waitr: Rendez,
    /// Rendezvous registered by `purgatorio.wait` on this prog's `wait`
    /// file.
    wait_watchers: Mutex<Vec<Rendez>>,
    /// The module policy (`docs/signing.md`).
    modpolicy: Mutex<ModPolicy>,
    /// The modules loaded into this prog: its command, then its reactors.
    modules: Mutex<Vec<Module>>,
    /// Tracing, the last call stack and the profile (`docs/debug.md`).
    pub debug: Debug,
}

impl Proc {
    /// The name this prog's command was loaded by; empty before it is.
    pub fn module_path(&self) -> String {
        self.modules
            .lock()
            .first()
            .map(|m| m.path.clone())
            .unwrap_or_default()
    }

    /// This prog's profile with those of its exited children.
    pub fn profile(&self) -> crate::debug::Profile {
        self.debug.profile(&self.module_path())
    }

    pub fn module_policy(&self) -> ModPolicy {
        self.modpolicy.lock().clone()
    }

    /// Tightens the module policy to `level`. The first move off `none`
    /// pins a copy of this prog's namespace for the trust lists. Lowering
    /// the level is [`error::EPERM`].
    pub fn tighten_module_policy(&self, level: crate::modsig::Level) -> Result<()> {
        let pin = {
            let p = self.modpolicy.lock();
            if level < p.level {
                return Err(Error::new(error::EPERM));
            }
            level > p.level && p.trust.is_none()
        };
        // Fork outside the policy lock: it takes the namespace's own.
        let pinned = pin.then(|| Arc::new(self.pgrp().fork()));
        let mut p = self.modpolicy.lock();
        if level > p.level {
            p.level = level;
            if p.trust.is_none() {
                p.trust = pinned;
            }
        }
        Ok(())
    }

    pub fn modules(&self) -> Vec<Module> {
        self.modules.lock().clone()
    }

    pub(crate) fn add_module(&self, m: Module) {
        self.modules.lock().push(m);
    }

    pub fn group(&self) -> u32 {
        self.group.load(Ordering::Relaxed)
    }

    pub fn name(&self) -> String {
        self.name.lock().clone()
    }

    pub fn set_name(&self, name: &str) {
        *self.name.lock() = String::from(name);
    }

    pub fn egrp(&self) -> Arc<Egrp> {
        self.egrp.lock().clone()
    }

    pub fn fgrp(&self) -> Arc<Fgrp> {
        self.fgrp.lock().clone()
    }

    pub fn pgrp(&self) -> Arc<Pgrp> {
        self.pgrp.lock().clone()
    }

    pub fn set_fgrp(&self, fgrp: Arc<Fgrp>) {
        *self.fgrp.lock() = fgrp;
    }

    pub fn set_pgrp(&self, pgrp: Arc<Pgrp>) {
        // The old namespace may be the last reference to its mounts, whose
        // Chans clunk fids on drop; drop it after the lock.
        let old = core::mem::replace(&mut *self.pgrp.lock(), pgrp);
        drop(old);
    }

    /// Interrupts this prog wherever it is blocked.
    pub fn kill(&self) {
        self.note.kill();
    }

    /// `running` until [`Proc::kill`] or a trap under tracing, then
    /// `broken`.
    pub fn state(&self) -> &'static str {
        if self.note.killed() || self.debug.broken() {
            "broken"
        } else {
            "running"
        }
    }

    fn post_wait(&self, msg: String) {
        {
            let mut w = self.waits.lock();
            if w.len() == MAXWAIT {
                w.pop_front();
            }
            w.push_back(msg);
        }
        self.waitr.wakeup();
        let watchers = self.wait_watchers.lock().clone();
        for w in watchers {
            w.wakeup();
        }
    }

    /// Whether an exit message is waiting, so [`Proc::wait`] would not
    /// block.
    pub fn wait_ready(&self) -> bool {
        !self.waits.lock().is_empty()
    }

    /// Wakes `w` when an exit message arrives.
    pub fn watch_wait(&self, w: &Rendez) {
        self.wait_watchers.lock().push(w.clone());
    }

    pub fn unwatch_wait(&self, w: &Rendez) {
        self.wait_watchers.lock().retain(|x| !x.same(w));
    }

    /// Blocks until a child has exited and returns its exit message:
    /// `pid name status`, with name and status quoted when needed.
    /// A killed prog gets [`error::EINTR`](purg_abi::error::EINTR).
    pub fn wait(&self) -> Result<String> {
        loop {
            park(&self.waitr, || !self.waits.lock().is_empty())?;
            if let Some(m) = self.waits.lock().pop_front() {
                return Ok(m);
            }
        }
    }
}

/// The process table.
pub struct Procs {
    platform: Arc<dyn Platform>,
    user: String,
    next_pid: AtomicU32,
    table: Mutex<BTreeMap<u32, Arc<Proc>>>,
    /// The module policy of progs with no parent.
    modpolicy: Mutex<ModPolicy>,
}

impl Procs {
    pub fn new(platform: Arc<dyn Platform>) -> Arc<Procs> {
        Arc::new(Procs {
            user: platform.user(),
            platform,
            next_pid: AtomicU32::new(1),
            table: Mutex::new(BTreeMap::new()),
            modpolicy: Mutex::new(ModPolicy::default()),
        })
    }

    /// Sets the system default module policy: that of every prog made
    /// with [`Prog::new`] from now on. `pgrp` is the namespace its trust
    /// lists are read in; a copy is kept.
    pub fn set_module_policy(&self, level: crate::modsig::Level, pgrp: &Pgrp) {
        let trust = (level != crate::modsig::Level::None).then(|| Arc::new(pgrp.fork()));
        *self.modpolicy.lock() = ModPolicy { level, trust };
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    fn register(
        &self,
        ppid: u32,
        group: Option<u32>,
        egrp: Arc<Egrp>,
        fgrp: Arc<Fgrp>,
        pgrp: Arc<Pgrp>,
        modpolicy: ModPolicy,
    ) -> Arc<Proc> {
        let pid = self.next_pid.fetch_add(1, Ordering::Relaxed);
        let p = Arc::new(Proc {
            pid,
            ppid,
            group: AtomicU32::new(group.unwrap_or(pid)),
            name: Mutex::new(String::new()),
            egrp: Mutex::new(egrp),
            fgrp: Mutex::new(fgrp),
            pgrp: Mutex::new(pgrp),
            note: Arc::new(Note::new()),
            kgrp: Arc::new(Kgrp::new()),
            waits: Mutex::new(VecDeque::new()),
            waitr: Rendez::new(),
            wait_watchers: Mutex::new(Vec::new()),
            modpolicy: Mutex::new(modpolicy),
            modules: Mutex::new(Vec::new()),
            debug: Debug::default(),
        });
        self.table.lock().insert(pid, p.clone());
        p
    }

    /// Kills every prog in `group`, and removes its broken progs.
    pub fn kill_group(&self, group: u32) {
        let mut broken = Vec::new();
        self.table.lock().retain(|_, p| {
            if p.group() != group {
                return true;
            }
            p.kill();
            if p.debug.broken() {
                broken.push(p.clone());
            }
            !p.debug.broken()
        });
        // Dropped outside the table's lock.
        drop(broken);
    }

    /// Kills `pid`. A broken prog has already stopped: it is removed from
    /// the table.
    pub fn kill(&self, pid: u32) {
        let p = {
            let mut table = self.table.lock();
            match table.get(&pid) {
                Some(p) if p.debug.broken() => table.remove(&pid),
                Some(p) => {
                    p.kill();
                    None
                }
                None => None,
            }
        };
        drop(p);
    }

    pub fn get(&self, pid: u32) -> Option<Arc<Proc>> {
        self.table.lock().get(&pid).cloned()
    }

    /// The prog running on the calling kproc.
    pub fn current(&self) -> Result<Arc<Proc>> {
        self.get(self.platform.current_pid())
            .ok_or(Error::new(error::ENONEXIST))
    }

    pub fn pids(&self) -> Vec<u32> {
        self.table.lock().keys().copied().collect()
    }

    /// Removes `pid` and, given a status, tells its parent how it ended.
    ///
    /// A prog that trapped while traced is broken, as in Inferno: it
    /// stays in the table, without its files, namespace and environment,
    /// so that its `stack` and `exception` can be read, until it is
    /// killed. Its parent is told at once all the same.
    fn exit(&self, pid: u32, status: Option<&str>) {
        let Some(p) = self.get(pid) else {
            return;
        };
        if p.debug.broken() && status.is_some() {
            *p.egrp.lock() = Arc::new(Egrp::new());
            p.set_fgrp(Arc::new(Fgrp::new()));
            if let Ok(empty) = p.pgrp().newns() {
                p.set_pgrp(empty);
            }
        } else {
            self.table.lock().remove(&pid);
        }
        let parent = self.get(p.ppid);
        // The samples of a profiled prog and of its children go to its
        // parent, where `prof` reads them after the prog has gone.
        if let Some(parent) = &parent
            && parent.debug.profiling()
        {
            parent.debug.absorb(p.profile());
        }
        let Some(status) = status else {
            return;
        };
        if let Some(parent) = parent {
            parent.post_wait(alloc::format!(
                "{} {} {}",
                pid,
                quote(&p.name()),
                quote(status)
            ));
        }
    }
}

/// Kernel-wide state shared by all progs.
pub struct Kernel {
    pub platform: Arc<dyn Platform>,
    pub devtab: Arc<DevTab>,
    pub procs: Arc<Procs>,
    pub srv: Arc<crate::devsrv::DevSrv>,
    pub mnt: Arc<crate::devmnt::DevMnt>,
}

impl Kernel {
    pub fn new(
        platform: Arc<dyn Platform>,
        devtab: Arc<DevTab>,
        procs: Arc<Procs>,
        srv: Arc<crate::devsrv::DevSrv>,
        mnt: Arc<crate::devmnt::DevMnt>,
    ) -> Arc<Kernel> {
        Arc::new(Kernel {
            platform,
            devtab,
            procs,
            srv,
            mnt,
        })
    }
}

/// A prog: the schedulable unit, with its namespace, fds and errstr.
///
/// Dropping a prog closes its fds and then posts its exit message, with
/// [`Prog::status`], to its parent; a spawned child does so only once
/// [`Prog::started`].
pub struct Prog {
    pub pid: u32,
    pub kernel: Arc<Kernel>,
    pub proc: Arc<Proc>,
    pub pgrp: Arc<Pgrp>,
    pub fgrp: Arc<Fgrp>,
    pub errstr: String,
    /// Exit status reported to the parent: `""` is success.
    pub status: String,
    /// False for a child that never ran, which leaves without a message.
    report: bool,
}

impl Prog {
    /// A first prog with a fresh fd table and environment.
    pub fn new(kernel: Arc<Kernel>, pgrp: Arc<Pgrp>) -> Prog {
        let fgrp = Arc::new(Fgrp::new());
        let modpolicy = kernel.procs.modpolicy.lock().clone();
        let proc = kernel.procs.register(
            0,
            None,
            Arc::new(Egrp::new()),
            fgrp.clone(),
            pgrp.clone(),
            modpolicy,
        );
        Prog {
            pid: proc.pid,
            proc,
            kernel,
            pgrp,
            fgrp,
            errstr: String::new(),
            status: String::new(),
            report: true,
        }
    }

    /// Records `e` as the prog's errstr (Inferno's `kwerrstr`).
    pub fn set_error(&mut self, e: &Error) {
        self.errstr.clear();
        self.errstr.push_str(e.as_str());
    }

    pub fn platform(&self) -> &dyn Platform {
        &*self.kernel.platform
    }

    pub fn egrp(&self) -> Arc<Egrp> {
        self.proc.egrp()
    }

    /// The current directory, as a cleaned absolute name.
    pub fn dot(&self) -> String {
        self.pgrp.dot()
    }

    /// `sys->pctl`: changes which groups this prog shares, returning its
    /// pid. `movefd` lists the fds `NEWFD` keeps.
    pub fn pctl(&mut self, flags: u32, movefd: &[i32]) -> Result<u32> {
        if flags & pctl::NEWFD != 0 {
            self.fgrp = Arc::new(self.fgrp.keep(movefd));
        } else if flags & pctl::FORKFD != 0 {
            self.fgrp = Arc::new(self.fgrp.fork());
        }
        if flags & pctl::NEWNS != 0 {
            self.pgrp = self.pgrp.newns()?;
        } else if flags & pctl::FORKNS != 0 {
            self.pgrp = Arc::new(self.pgrp.fork());
        }
        if flags & pctl::NODEVS != 0 {
            self.pgrp.set_nodevs();
        }
        if flags & pctl::NEWENV != 0 {
            *self.proc.egrp.lock() = Arc::new(Egrp::new());
        } else if flags & pctl::FORKENV != 0 {
            let forked = self.egrp().fork();
            *self.proc.egrp.lock() = Arc::new(forked);
        }
        if flags & pctl::NEWPGRP != 0 {
            self.proc.group.store(self.pid, Ordering::Relaxed);
        }
        self.publish();
        Ok(self.pid)
    }

    fn publish(&self) {
        self.proc.set_fgrp(self.fgrp.clone());
        self.proc.set_pgrp(self.pgrp.clone());
    }

    /// A child prog sharing this prog's groups, then changed by `flags` as
    /// [`Prog::pctl`] would, then given `fdmap`'s `(parent fd, child fd)`
    /// pairs. A non-empty `fdmap` implies `FORKFD` unless `NEWFD` is set, so
    /// the parent's table is never changed.
    pub fn spawn(&self, flags: u32, fdmap: &[(i32, i32)]) -> Result<Prog> {
        let mut flags = flags;
        if !fdmap.is_empty() && flags & (pctl::NEWFD | pctl::FORKFD) == 0 {
            flags |= pctl::FORKFD;
        }
        let chans = fdmap
            .iter()
            .map(|&(from, to)| Ok((to, self.fgrp.get(from)?)))
            .collect::<Result<Vec<_>>>()?;
        let proc = self.kernel.procs.register(
            self.pid,
            Some(self.proc.group()),
            self.egrp(),
            self.fgrp.clone(),
            self.pgrp.clone(),
            self.proc.module_policy(),
        );
        proc.debug.inherit(&self.proc.debug);
        let mut child = Prog {
            pid: proc.pid,
            proc,
            kernel: self.kernel.clone(),
            pgrp: self.pgrp.clone(),
            fgrp: self.fgrp.clone(),
            errstr: String::new(),
            status: String::new(),
            report: false,
        };
        child.pctl(flags, &[])?;
        for (to, c) in chans {
            child.fgrp.install(to, c)?;
        }
        child.publish();
        Ok(child)
    }

    /// Marks a child from [`Prog::spawn`] as started: from now on, dropping
    /// it posts an exit message to the parent.
    pub fn started(&mut self) {
        self.report = true;
    }
}

impl Drop for Prog {
    fn drop(&mut self) {
        // Close fds first so a parent reading a pipe from this prog sees end
        // of file no later than the exit message.
        self.fgrp = Arc::new(Fgrp::new());
        self.kernel.srv.remove_server(self.pid);
        self.kernel
            .procs
            .exit(self.pid, self.report.then_some(&*self.status));
    }
}
