//! Progs, the process table, and `pctl`.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use purg_abi::{error, pctl};
use purg_platform::Platform;
use purg_platform::sync::{Mutex, MutexGuard, Rendez};

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

/// No limit, in [`Proc::memlimit`] and the domain limits.
pub const UNLIMITED: u64 = u64::MAX;

/// Bytes charged for each element of a wasm table, against the memory
/// limits.
pub const TABLE_ELEM_BYTES: u64 = 8;

/// The limits of a prog domain (`#pdomain`), set through the `ctl` of a
/// prog in it: the most wasm memory its members may hold between them,
/// and the most members it may have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DomainLimits {
    pub mem: u64,
    pub progs: u64,
}

impl Default for DomainLimits {
    fn default() -> DomainLimits {
        DomainLimits {
            mem: UNLIMITED,
            progs: UNLIMITED,
        }
    }
}

/// The kernel-visible part of a prog: what devices such as `#e` and `#p`
/// need to find from the calling kproc's pid.
pub struct Proc {
    pub pid: u32,
    /// Parent pid; 0 for the first prog.
    pub ppid: u32,
    group: AtomicU32,
    name: Mutex<String>,
    /// The user this prog acts as (Inferno's `up->env->user`): what
    /// `/dev/user` reads, the name its mounts authenticate as, and the
    /// owner of the files it creates in a RAM tree. Inherited at spawn;
    /// only the host owner may change it ([`Procs::set_user`]).
    user: Mutex<Arc<str>>,
    egrp: Mutex<Arc<Egrp>>,
    fgrp: Mutex<Arc<Fgrp>>,
    pgrp: Mutex<Arc<Pgrp>>,
    /// Set by `kill` and by `ctl`. Blocking syscalls on this prog watch it.
    pub note: Arc<Note>,
    /// The prog survives an interrupt (`catchint on` on `ctl`): it gets
    /// the note instead of being killed. Not inherited.
    catchint: AtomicBool,
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
    /// The prog domains this prog is in, outermost first, each named by
    /// the process group that founded it (`#pdomain`, see `devprog`).
    /// Inherited from the parent; a prog only ever adds to its own.
    domains: Mutex<Arc<[u32]>>,
    /// The most wasm memory (linear memory, and tables at
    /// [`TABLE_ELEM_BYTES`] an element) this prog may hold, in bytes;
    /// [`UNLIMITED`] for none. Inherited by the progs it starts.
    memlimit: AtomicU64,
    /// The wasm memory it holds now, as its engine reported it.
    mem: AtomicU64,
}

impl Proc {
    /// The user this prog acts as.
    pub fn user(&self) -> String {
        String::from(&**self.user.lock())
    }

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

    /// The prog domains this prog is in, outermost first.
    pub fn domains(&self) -> Arc<[u32]> {
        self.domains.lock().clone()
    }

    /// Whether this prog is in domain `id`.
    pub fn in_domain(&self, id: u32) -> bool {
        self.domains.lock().contains(&id)
    }

    /// Puts this prog in domain `id`, inside the ones it is in, unless
    /// that is its innermost one already.
    pub fn join_domain(&self, id: u32) {
        let mut d = self.domains.lock();
        if d.last() != Some(&id) {
            let mut chain = d.to_vec();
            chain.push(id);
            *d = Arc::from(chain);
        }
    }

    pub fn group(&self) -> u32 {
        self.group.load(Ordering::Relaxed)
    }

    /// The prog's memory limit in bytes, [`UNLIMITED`] for none.
    pub fn memlimit(&self) -> u64 {
        self.memlimit.load(Ordering::Acquire)
    }

    /// Lowers the prog's memory limit to `bytes`. A limit can only be
    /// lowered: raising it is [`error::EPERM`]. It applies to the memory
    /// the prog grows from now on, and to the progs it starts.
    pub fn set_memlimit(&self, bytes: u64) -> Result<()> {
        self.memlimit
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (bytes <= old).then_some(bytes)
            })
            .map(drop)
            .map_err(|_| Error::new(error::EPERM))
    }

    /// The wasm memory the prog holds, in bytes.
    pub fn mem(&self) -> u64 {
        self.mem.load(Ordering::Acquire)
    }

    /// Forgets the memory charged so far: its engine is starting afresh.
    pub fn clear_mem(&self) {
        self.mem.store(0, Ordering::Release);
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
        // The old table may hold the last reference to a `#M` file, whose
        // clunk waits for the file server; drop it after the lock.
        let old = core::mem::replace(&mut *self.fgrp.lock(), fgrp);
        drop(old);
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

    /// Whether the prog survives interrupts (`docs/abi.md`, "Interrupts").
    pub fn catches_interrupt(&self) -> bool {
        self.catchint.load(Ordering::Acquire)
    }

    pub fn set_catch_interrupt(&self, on: bool) {
        self.catchint.store(on, Ordering::Release);
    }

    /// `running` until [`Proc::kill`] or a trap under tracing, then
    /// `broken`; `debug` while a debugger holds it stopped.
    pub fn state(&self) -> &'static str {
        if self.note.killed() || self.debug.broken() {
            "broken"
        } else if self.debug.stopped() {
            "debug"
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

/// The longest user name [`Procs::set_user`] takes, in bytes.
pub const USER_MAX: usize = 64;

/// Whether `user` can be a user name: a single file name of at most
/// [`USER_MAX`] bytes with no white space or control characters, so it
/// can name `/usr/<user>` and be quoted nowhere.
pub fn valid_user(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= USER_MAX
        && user != "."
        && user != ".."
        && !user
            .chars()
            .any(|c| c == '/' || c.is_whitespace() || c.is_control())
}

/// The process table.
pub struct Procs {
    platform: Arc<dyn Platform>,
    user: String,
    next_pid: AtomicU32,
    table: Mutex<BTreeMap<u32, Arc<Proc>>>,
    /// The module policy of progs with no parent.
    modpolicy: Mutex<ModPolicy>,
    /// The limits set on prog domains, by domain. The lock also makes
    /// each memory charge to a limited domain atomic with its check.
    domain_limits: Mutex<BTreeMap<u32, DomainLimits>>,
    /// Held by a spawn into a domain with a prog limit from the check until
    /// the child is counted as a member, so two spawns cannot both take
    /// the last place.
    admit: Mutex<()>,
}

impl Procs {
    pub fn new(platform: Arc<dyn Platform>) -> Arc<Procs> {
        Arc::new(Procs {
            user: platform.user(),
            platform,
            next_pid: AtomicU32::new(1),
            table: Mutex::new(BTreeMap::new()),
            modpolicy: Mutex::new(ModPolicy::default()),
            domain_limits: Mutex::new(BTreeMap::new()),
            admit: Mutex::new(()),
        })
    }

    /// The limits of domain `id`.
    pub fn domain_limits(&self, id: u32) -> DomainLimits {
        self.domain_limits
            .lock()
            .get(&id)
            .copied()
            .unwrap_or_default()
    }

    /// Lowers the limits of domain `id` to those in `new` that are lower.
    /// Raising one is [`error::EPERM`], and changes nothing.
    pub fn set_domain_limits(&self, id: u32, new: DomainLimits) -> Result<()> {
        let mut all = self.domain_limits.lock();
        let cur = all.get(&id).copied().unwrap_or_default();
        if new.mem > cur.mem || new.progs > cur.progs {
            return Err(Error::new(error::EPERM));
        }
        all.insert(id, new);
        Ok(())
    }

    /// The members of domain `id` now, and the wasm memory they hold.
    pub fn domain_usage(&self, id: u32) -> (u64, u64) {
        let table = self.table.lock();
        let members = table.values().filter(|p| p.in_domain(id));
        members.fold((0, 0), |(n, mem), p| (n + 1, mem + p.mem()))
    }

    /// Asks whether `p` may grow its wasm memory from `current` to
    /// `desired` bytes, and charges it if so: it must stay within its own
    /// limit and within the memory limit of every domain it is in. The
    /// engines call this from their resource limiters.
    pub fn charge_mem(&self, p: &Proc, current: u64, desired: u64) -> bool {
        let grow = desired.saturating_sub(current);
        if p.mem().saturating_add(grow) > p.memlimit() {
            return false;
        }
        let domains = p.domains();
        let all = self.domain_limits.lock();
        for d in domains.iter() {
            if let Some(l) = all.get(d)
                && l.mem != UNLIMITED
                && self.domain_usage(*d).1.saturating_add(grow) > l.mem
            {
                return false;
            }
        }
        p.mem.fetch_add(grow, Ordering::AcqRel);
        true
    }

    /// Whether a prog in `domains` may start another: every one of those
    /// domains with a prog limit has fewer members than that. Fails with
    /// [`error::ENOPROC`] otherwise. When a limit applies, the guard
    /// returned must be held until the new prog is in its domains.
    fn check_prog_limit(&self, domains: &[u32]) -> Result<Option<MutexGuard<'_, ()>>> {
        let limits: Vec<(u32, u64)> = {
            let all = self.domain_limits.lock();
            domains
                .iter()
                .filter_map(|d| all.get(d).map(|l| (*d, l.progs)))
                .filter(|&(_, n)| n != UNLIMITED)
                .collect()
        };
        if limits.is_empty() {
            return Ok(None);
        }
        let admit = self.admit.lock();
        for (d, n) in limits {
            if self.domain_usage(d).0 >= n {
                return Err(Error::new(error::ENOPROC));
            }
        }
        Ok(Some(admit))
    }

    /// Sets the system default module policy: that of every prog made
    /// with [`Prog::new`] from now on. `pgrp` is the namespace its trust
    /// lists are read in; a copy is kept.
    pub fn set_module_policy(&self, level: crate::modsig::Level, pgrp: &Pgrp) {
        let trust = (level != crate::modsig::Level::None).then(|| Arc::new(pgrp.fork()));
        *self.modpolicy.lock() = ModPolicy { level, trust };
    }

    /// The host owner (Inferno's `eve`): the platform's user, which the
    /// first prog acts as. It owns the devices and may become any user.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The user the prog on the calling kproc acts as; the host owner when
    /// no prog is running on it (a kernel thread of its own).
    pub fn current_user(&self) -> String {
        match self.current() {
            Ok(p) => p.user(),
            Err(_) => self.user.clone(),
        }
    }

    /// Whether the calling prog acts as the host owner (Inferno's
    /// `iseve`).
    pub fn is_owner(&self) -> bool {
        self.current_user() == self.user
    }

    /// Makes `p` act as `user` from now on, and the progs it starts after.
    /// Only a prog acting as the host owner may (the caller, which is the
    /// prog on this kproc); anyone else gets [`error::EPERM`], even for
    /// their own name, as in Inferno. A user name that could not be a
    /// file name (empty, `.`, `..`, or with `/`, white space or a control
    /// character) is [`error::EBADARG`].
    pub fn set_user(&self, p: &Proc, user: &str) -> Result<()> {
        if !self.is_owner() {
            return Err(Error::new(error::EPERM));
        }
        if !valid_user(user) {
            return Err(Error::new(error::EBADARG));
        }
        *p.user.lock() = Arc::from(user);
        Ok(())
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
            user: Mutex::new(Arc::from(self.user.as_str())),
            egrp: Mutex::new(egrp),
            fgrp: Mutex::new(fgrp),
            pgrp: Mutex::new(pgrp),
            note: Arc::new(Note::new()),
            catchint: AtomicBool::new(false),
            kgrp: Arc::new(Kgrp::new()),
            waits: Mutex::new(VecDeque::new()),
            waitr: Rendez::new(),
            wait_watchers: Mutex::new(Vec::new()),
            modpolicy: Mutex::new(modpolicy),
            modules: Mutex::new(Vec::new()),
            debug: Debug::default(),
            domains: Mutex::new(Arc::from([])),
            memlimit: AtomicU64::new(UNLIMITED),
            mem: AtomicU64::new(0),
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

    /// Sends the `interrupt` note to `pid`: a prog that catches it has
    /// its pending blocking call, or its next, fail with `interrupted`;
    /// any other is killed.
    pub fn interrupt(&self, pid: u32) {
        match self.get(pid) {
            Some(p) if p.catches_interrupt() => p.note.interrupt(),
            Some(_) => self.kill(pid),
            None => {}
        }
    }

    /// Sends the `interrupt` note to every prog in `group`.
    pub fn interrupt_group(&self, group: u32) {
        self.interrupt_all(|p| p.group() == group);
    }

    /// Sends the `interrupt` note to every prog `which` picks, those that
    /// catch it first, so that one waiting for another sees the note
    /// before the other's death.
    pub fn interrupt_all(&self, which: impl Fn(&Proc) -> bool) {
        let members: Vec<Arc<Proc>> = self
            .table
            .lock()
            .values()
            .filter(|p| which(p))
            .cloned()
            .collect();
        for p in members.iter().filter(|p| p.catches_interrupt()) {
            p.note.interrupt();
        }
        for p in members.iter().filter(|p| !p.catches_interrupt()) {
            self.kill(p.pid);
        }
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
        // Its engine has gone, and its memory with it.
        p.mem.store(0, Ordering::Release);
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
        // The samples and counts of a profiled prog and of its children
        // go to its parent, where `prof`, `cprof` and `mprof` read them
        // after the prog has gone.
        if let Some(parent) = &parent {
            p.debug.give(&parent.debug, &p.module_path());
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
        let domains = self.proc.domains();
        let admit = self.kernel.procs.check_prog_limit(&domains)?;
        let proc = self.kernel.procs.register(
            self.pid,
            Some(self.proc.group()),
            self.egrp(),
            self.fgrp.clone(),
            self.pgrp.clone(),
            self.proc.module_policy(),
        );
        // Set before the child runs, so nothing has charged it yet.
        proc.memlimit.store(self.proc.memlimit(), Ordering::Release);
        let user = self.proc.user.lock().clone();
        *proc.user.lock() = user;
        proc.debug.inherit(&self.proc.debug);
        *proc.domains.lock() = domains;
        drop(admit);
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
        // of file no later than the exit message. The Proc's reference to
        // the table goes too: anything holding the Proc (an open `#p`
        // file, a syscall in another kproc) would otherwise keep the
        // files open past the exit.
        let empty = Arc::new(Fgrp::new());
        self.proc.set_fgrp(empty.clone());
        self.fgrp = empty;
        // Then the namespace, while the prog is still in the table, as
        // Inferno's `progexit` closes the pgrp before `delprog`. Releasing
        // a `#M` Chan clunks its fid and waits for the file server; a
        // prog that has left the table could not be killed out of that
        // wait, and its thread would outlive it (issue #110).
        let empty = self.pgrp.exited();
        self.proc.set_pgrp(empty.clone());
        self.pgrp = empty;
        self.kernel.srv.remove_server(self.pid);
        self.kernel
            .procs
            .exit(self.pid, self.report.then_some(&*self.status));
    }
}
