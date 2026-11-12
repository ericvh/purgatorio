//! A prog's debugging state (`docs/debug.md`): whether it is traced, its
//! last call stack, the trap that broke it, its profiles, and the
//! debugger's hold on it.
//!
//! The VM fills this in; `#p` serves it as the `stack`, `exception`,
//! `prof`, `cprof`, `mprof` and `heap` files and sets the switches from
//! `ctl`.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use purg_abi::error;
use purg_platform::sync::{Mutex, Note, Rendez};

use crate::error::{Error, Result};

/// Modules this prog starts are instrumented with a call stack. Inherited.
const TRACE: u32 = 1;
/// Traced progs are sampled. Inherited.
const PROF: u32 = 2;
/// This prog's own module carries a call stack.
const TRACED: u32 = 4;
/// This prog trapped while traced: it stays in the table until killed.
const BROKEN: u32 = 8;
/// Traced modules count the calls of each function. Inherited.
const CPROF: u32 = 16;
/// Traced modules count the memory each function grows. Inherited.
const MPROF: u32 = 32;
/// The debugger asked the prog to stop.
const STOP: u32 = 64;
/// The prog is stopped, its engine serving the debugger.
const STOPPED: u32 = 128;
/// The breakpoints changed since the engine last saw them.
const BREAKS: u32 = 256;

/// The most frames a stack snapshot keeps: the innermost ones.
pub const MAX_FRAMES: usize = 256;

/// The error of reading the `stack` of a prog started without tracing,
/// and of debugging one.
pub const ENOTTRACED: &str = "prog is not traced";

/// The error of using the debugger's files on a prog that is running.
pub const ENOTSTOPPED: &str = "prog not stopped";

/// The most bytes one read of `heap` returns.
pub const MAX_ACCESS: usize = 64 * 1024;

/// A call stack: function indices in the prog's command module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stack {
    /// The innermost frame first; at most [`MAX_FRAMES`].
    pub frames: Vec<u32>,
    /// The number of frames the prog had.
    pub depth: u32,
}

/// Counts by `(module path, function index)`.
pub type Profile = BTreeMap<(String, u32), u64>;

/// What a profile counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Samples taken at the engines' check points (`prof`).
    Samples,
    /// Calls of each function (`cprof`).
    Calls,
    /// 64 KiB pages of memory grown in each function, with what it
    /// called (`mprof`).
    Pages,
}

const KINDS: [Kind; 3] = [Kind::Samples, Kind::Calls, Kind::Pages];

impl Kind {
    fn bit(self) -> u32 {
        match self {
            Kind::Samples => PROF,
            Kind::Calls => CPROF,
            Kind::Pages => MPROF,
        }
    }

    fn slot(self) -> usize {
        self as usize
    }
}

/// Where a traced module keeps what the host reads and writes in its
/// stack memory (`purg_vm::trace`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Layout {
    /// The functions the module imports: its own come after them.
    pub imported: u32,
    /// The functions the module defines.
    pub funcs: u32,
    /// The offset of the breakpoint bytes, one per defined function.
    pub breaks: u32,
    /// The offset of the call counts, a `u64` per defined function, when
    /// the module counts them.
    pub calls: Option<u32>,
    /// The offset of the pages grown, a `u32` per defined function, when
    /// the module counts them.
    pub pages: Option<u32>,
}

/// A request of the debugger to a stopped prog's engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// Read up to `len` bytes of linear memory at `at`.
    Read { at: u64, len: usize },
    /// Write `data` at `at`.
    Write { at: u64, data: Vec<u8> },
}

/// How a stopped prog goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// Until a breakpoint or a `stop`.
    Cont,
    /// Until it enters a function.
    Step,
}

type Answer = core::result::Result<Vec<u8>, &'static str>;

/// The debugger's side of a stopped prog: its requests and their
/// answers, and how the prog is to go on.
#[derive(Default)]
struct Door {
    next: u64,
    queue: VecDeque<(u64, Access)>,
    answers: BTreeMap<u64, Answer>,
    resume: Option<Resume>,
}

#[derive(Default)]
struct Tally {
    /// This prog's counts, by function index.
    own: BTreeMap<u32, u64>,
    /// The counts of its children that have exited.
    children: Profile,
}

pub struct Debug {
    flags: AtomicU32,
    stack: Mutex<Stack>,
    exception: Mutex<String>,
    layout: Mutex<Option<Layout>>,
    tallies: [Mutex<Tally>; 3],
    breakpoints: Mutex<BTreeSet<u32>>,
    door: Mutex<Door>,
    /// Where the stopped engine and the debugger wait for each other.
    rendez: Rendez,
}

impl Default for Debug {
    fn default() -> Debug {
        Debug {
            flags: AtomicU32::new(0),
            stack: Mutex::new(Stack::default()),
            exception: Mutex::new(String::new()),
            layout: Mutex::new(None),
            tallies: Default::default(),
            breakpoints: Mutex::new(BTreeSet::new()),
            door: Mutex::new(Door::default()),
            rendez: Rendez::new(),
        }
    }
}

impl Debug {
    fn has(&self, bit: u32) -> bool {
        self.flags.load(Ordering::Acquire) & bit != 0
    }

    fn set(&self, bit: u32, on: bool) {
        if on {
            self.flags.fetch_or(bit, Ordering::AcqRel);
        } else {
            self.flags.fetch_and(!bit, Ordering::AcqRel);
        }
    }

    /// A child's switches: the inherited ones of its parent.
    pub(crate) fn inherit(&self, parent: &Debug) {
        let bits = parent.flags.load(Ordering::Acquire) & (TRACE | PROF | CPROF | MPROF);
        self.flags.store(bits, Ordering::Release);
    }

    /// Whether commands this prog starts are to carry a call stack.
    pub fn trace(&self) -> bool {
        self.has(TRACE)
    }

    /// Turning tracing off turns the profiles off too.
    pub fn set_trace(&self, on: bool) {
        self.set(TRACE, on);
        if !on {
            self.set(PROF | CPROF | MPROF, false);
        }
    }

    /// Whether this prog, if traced, is sampled, and whether it collects
    /// the samples of its children.
    pub fn profiling(&self) -> bool {
        self.collects(Kind::Samples)
    }

    /// Whether the modules this prog starts count `kind` (or, for
    /// samples, are sampled), and whether it collects the counts of its
    /// children.
    pub fn collects(&self, kind: Kind) -> bool {
        self.has(kind.bit())
    }

    /// Turning profiling on also turns tracing on, and clears the samples
    /// collected before.
    pub fn set_prof(&self, on: bool) {
        self.set_collect(Kind::Samples, on);
    }

    /// [`Debug::set_prof`] for any kind of profile.
    pub fn set_collect(&self, kind: Kind, on: bool) {
        if on && !self.has(kind.bit()) {
            *self.tallies[kind.slot()].lock() = Tally::default();
            self.set(TRACE, true);
        }
        self.set(kind.bit(), on);
    }

    /// Whether this prog's module carries a call stack.
    pub fn traced(&self) -> bool {
        self.has(TRACED)
    }

    /// Marks this prog's module as carrying a call stack, laid out as
    /// `layout` says.
    pub fn set_traced(&self, layout: Layout) {
        *self.layout.lock() = Some(layout);
        self.set(TRACED, true);
    }

    /// Where this prog's traced module keeps its counts and breakpoints.
    pub fn layout(&self) -> Option<Layout> {
        *self.layout.lock()
    }

    /// Whether this prog trapped while traced.
    pub fn broken(&self) -> bool {
        self.has(BROKEN)
    }

    /// Records the stack the prog has now.
    pub fn record(&self, stack: Stack) {
        *self.stack.lock() = stack;
    }

    pub fn stack(&self) -> Stack {
        self.stack.lock().clone()
    }

    /// Records the trap that stopped a traced prog and marks it broken.
    pub fn set_exception(&self, what: &str) {
        *self.exception.lock() = String::from(what);
        self.set(BROKEN, true);
    }

    /// The trap that broke the prog; empty when it is not broken.
    pub fn exception(&self) -> String {
        self.exception.lock().clone()
    }

    /// Counts one sample in function `func` of this prog's module.
    pub fn sample(&self, func: u32) {
        *self.tallies[Kind::Samples.slot()]
            .lock()
            .own
            .entry(func)
            .or_insert(0) += 1;
    }

    /// Replaces this prog's own counts of `kind`, by function index, with
    /// those its module holds now.
    pub fn set_counts(&self, kind: Kind, counts: BTreeMap<u32, u64>) {
        self.tallies[kind.slot()].lock().own = counts;
    }

    /// This prog's samples under `path`, its module, with those of its
    /// exited children.
    pub fn profile(&self, path: &str) -> Profile {
        self.counts(Kind::Samples, path)
    }

    /// This prog's counts of `kind` under `path`, its module, with those
    /// of its exited children.
    pub fn counts(&self, kind: Kind, path: &str) -> Profile {
        let t = self.tallies[kind.slot()].lock();
        let mut all = t.children.clone();
        for (&func, &n) in t.own.iter() {
            *all.entry((String::from(path), func)).or_insert(0) += n;
        }
        all
    }

    /// Adds the counts of `kind` of a child that has exited.
    pub(crate) fn absorb(&self, kind: Kind, child: Profile) {
        let mut t = self.tallies[kind.slot()].lock();
        for (key, n) in child {
            *t.children.entry(key).or_insert(0) += n;
        }
    }

    /// Gives `parent` the counts of each kind it collects, from this
    /// prog, which has exited and whose module was `path`, and from its
    /// children.
    pub(crate) fn give(&self, parent: &Debug, path: &str) {
        for kind in KINDS {
            if parent.collects(kind) {
                parent.absorb(kind, self.counts(kind, path));
            }
        }
    }

    // The debugger. A stop is asked for with `stop` on `ctl`; the engine
    // takes it at the prog's next host call or check point
    // (`purg_vm::trace`), and then serves the debugger's requests on its
    // own thread, the only one that may touch the prog's memory, until
    // the prog is resumed or killed.

    /// Asks a traced prog to stop.
    pub fn request_stop(&self) -> Result<()> {
        if !self.traced() {
            return Err(Error::new(ENOTTRACED));
        }
        self.set(STOP, true);
        Ok(())
    }

    /// Whether the prog is stopped.
    pub fn stopped(&self) -> bool {
        self.has(STOPPED)
    }

    /// Resumes a stopped prog as `how` says, and returns once it is no
    /// longer stopped: a debugger that then sees it stopped knows it has
    /// stopped again. A prog that is running forgets a stop not yet
    /// taken; `step` needs the prog stopped.
    pub fn resume(&self, how: Resume) -> Result<()> {
        {
            let mut door = self.door.lock();
            if !self.stopped() {
                if how == Resume::Step {
                    return Err(Error::new(ENOTSTOPPED));
                }
                self.set(STOP, false);
                return Ok(());
            }
            door.resume = Some(how);
        }
        self.rendez.wakeup();
        crate::block::park(&self.rendez, || {
            let door = self.door.lock();
            door.resume.is_none() || !self.stopped()
        })
    }

    /// Sets (`on`) or clears a breakpoint on entry to function `func`.
    pub fn set_break(&self, func: u32, on: bool) -> Result<()> {
        let Some(layout) = self.layout() else {
            return Err(Error::new(ENOTTRACED));
        };
        if func < layout.imported || func - layout.imported >= layout.funcs {
            return Err(Error::new(error::EBADARG));
        }
        let mut b = self.breakpoints.lock();
        if on {
            b.insert(func);
        } else {
            b.remove(&func);
        }
        self.set(BREAKS, true);
        Ok(())
    }

    /// The functions with a breakpoint.
    pub fn breakpoints(&self) -> Vec<u32> {
        self.breakpoints.lock().iter().copied().collect()
    }

    /// Reads or writes a stopped prog's memory, through its engine.
    pub fn access(&self, what: Access) -> Result<Vec<u8>> {
        let id = {
            let mut door = self.door.lock();
            if !self.stopped() {
                return Err(Error::new(ENOTSTOPPED));
            }
            let id = door.next;
            door.next += 1;
            door.queue.push_back((id, what));
            id
        };
        self.rendez.wakeup();
        let waited =
            crate::block::park(&self.rendez, || self.door.lock().answers.contains_key(&id));
        let answer = self.door.lock().answers.remove(&id);
        waited?;
        match answer {
            Some(Ok(data)) => Ok(data),
            Some(Err(e)) => Err(Error::new(e)),
            None => Err(Error::new(ENOTSTOPPED)),
        }
    }

    /// For the engine: whether it has anything to do for the debugger,
    /// given the hit word of the stack memory (non-zero when a function
    /// has stopped on entry).
    pub fn wanted(&self, hit: u32) -> bool {
        hit != 0 || self.flags.load(Ordering::Acquire) & (STOP | BREAKS) != 0
    }

    /// For the engine: takes a change of the breakpoints, if there was
    /// one.
    pub fn take_breaks(&self) -> bool {
        self.flags.fetch_and(!BREAKS, Ordering::AcqRel) & BREAKS != 0
    }

    /// For the engine: whether a stop was asked for.
    pub fn stop_requested(&self) -> bool {
        self.has(STOP)
    }

    /// For the engine: stops the prog and serves the debugger with
    /// `serve` until the prog is resumed, or killed (`None`). `note` is
    /// the prog's.
    pub fn hold(&self, note: &Note, mut serve: impl FnMut(&Access) -> Answer) -> Option<Resume> {
        {
            let _door = self.door.lock();
            self.set(STOPPED, true);
            self.set(STOP, false);
        }
        self.rendez.wakeup();
        loop {
            let ready = || {
                let door = self.door.lock();
                !door.queue.is_empty() || door.resume.is_some()
            };
            let alive = note.sleep(&self.rendez, ready);
            let mut door = self.door.lock();
            let queued: Vec<(u64, Access)> = door.queue.drain(..).collect();
            for (id, what) in queued {
                let answer = if alive {
                    serve(&what)
                } else {
                    Err(ENOTSTOPPED)
                };
                door.answers.insert(id, answer);
            }
            let how = door.resume.take();
            // Taking the resume and leaving the stop are one step, under
            // the lock `resume` waits with.
            let done = !alive || how.is_some();
            if done {
                self.set(STOPPED | STOP, false);
            }
            drop(door);
            self.rendez.wakeup();
            if done {
                break if alive { how } else { None };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches() {
        let d = Debug::default();
        assert!(!d.trace() && !d.profiling() && !d.traced() && !d.broken());
        d.set_prof(true);
        assert!(d.trace() && d.profiling());
        d.set_prof(false);
        assert!(d.trace() && !d.profiling());
        d.set_prof(true);
        d.set_collect(Kind::Calls, true);
        d.set_trace(false);
        assert!(!d.trace() && !d.profiling() && !d.collects(Kind::Calls));
    }

    /// A child gets the switches, not the state.
    #[test]
    fn inherits_the_switches_only() {
        let parent = Debug::default();
        parent.set_prof(true);
        parent.set_collect(Kind::Pages, true);
        parent.set_traced(Layout::default());
        parent.set_exception("trap: x");
        parent.sample(3);
        let child = Debug::default();
        child.inherit(&parent);
        assert!(child.trace() && child.profiling() && child.collects(Kind::Pages));
        assert!(!child.collects(Kind::Calls));
        assert!(!child.traced() && !child.broken());
        assert!(child.exception().is_empty());
        assert!(child.profile("/bin/c.wasm").is_empty());
        assert_eq!(child.layout(), None);
    }

    #[test]
    fn profiles_add_up() {
        let d = Debug::default();
        d.set_prof(true);
        d.sample(7);
        d.sample(7);
        d.sample(2);
        let mut child = Profile::new();
        child.insert((String::from("/bin/a.wasm"), 7), 5);
        child.insert((String::from("/bin/b.wasm"), 1), 1);
        d.absorb(Kind::Samples, child.clone());
        d.absorb(Kind::Samples, child);
        let all: Vec<_> = d.profile("/bin/a.wasm").into_iter().collect();
        assert_eq!(
            all,
            [
                ((String::from("/bin/a.wasm"), 2), 1),
                ((String::from("/bin/a.wasm"), 7), 12),
                ((String::from("/bin/b.wasm"), 1), 2),
            ]
        );
        // Profiling that is already on keeps its samples; turned on
        // again, it starts from nothing.
        d.set_prof(true);
        assert_eq!(d.profile("/bin/a.wasm").len(), 3);
        d.set_prof(false);
        d.set_prof(true);
        assert!(d.profile("/bin/a.wasm").is_empty());
    }

    /// Counts go to a parent that collects them, kind by kind.
    #[test]
    fn counts_go_to_who_collects_them() {
        let parent = Debug::default();
        parent.set_collect(Kind::Calls, true);
        let child = Debug::default();
        child.set_counts(Kind::Calls, BTreeMap::from([(4, 10), (5, 1)]));
        child.set_counts(Kind::Pages, BTreeMap::from([(4, 2)]));
        child.give(&parent, "/bin/x.wasm");
        child.give(&parent, "/bin/x.wasm");
        let calls: Vec<_> = parent
            .counts(Kind::Calls, "/bin/p.wasm")
            .into_iter()
            .collect();
        assert_eq!(
            calls,
            [
                ((String::from("/bin/x.wasm"), 4), 20),
                ((String::from("/bin/x.wasm"), 5), 2),
            ]
        );
        assert!(parent.counts(Kind::Pages, "/bin/p.wasm").is_empty());
        // Counts replace, samples add.
        child.set_counts(Kind::Calls, BTreeMap::from([(4, 11)]));
        assert_eq!(child.counts(Kind::Calls, "/x").len(), 1);
    }

    #[test]
    fn a_trap_breaks() {
        let d = Debug::default();
        d.record(Stack {
            frames: alloc::vec![4, 1],
            depth: 2,
        });
        d.set_exception("trap: unreachable");
        assert!(d.broken());
        assert_eq!(d.exception(), "trap: unreachable");
        assert_eq!(d.stack().frames, [4, 1]);
    }

    #[test]
    fn breakpoints_need_a_traced_function() {
        let d = Debug::default();
        assert_eq!(d.set_break(3, true).unwrap_err().as_str(), ENOTTRACED);
        assert_eq!(d.request_stop().unwrap_err().as_str(), ENOTTRACED);
        d.set_traced(Layout {
            imported: 2,
            funcs: 3,
            ..Layout::default()
        });
        assert!(d.set_break(1, true).is_err());
        assert!(d.set_break(5, true).is_err());
        assert!(!d.wanted(0));
        d.set_break(4, true).unwrap();
        d.set_break(2, true).unwrap();
        assert_eq!(d.breakpoints(), [2, 4]);
        assert!(d.wanted(0) && d.take_breaks() && !d.take_breaks());
        assert!(!d.wanted(0) && d.wanted(5));
        d.set_break(4, false).unwrap();
        assert_eq!(d.breakpoints(), [2]);
    }

    /// The debugger and a held engine on two threads: requests are served
    /// in order, the engine goes on when resumed, and a running prog
    /// refuses them.
    #[test]
    fn holds_and_serves() {
        extern crate std;
        use alloc::sync::Arc;
        let d = Arc::new(Debug::default());
        d.set_traced(Layout::default());
        let read = Access::Read { at: 0, len: 4 };
        assert_eq!(d.access(read.clone()).unwrap_err().as_str(), ENOTSTOPPED);
        assert_eq!(d.resume(Resume::Step).unwrap_err().as_str(), ENOTSTOPPED);
        d.request_stop().unwrap();
        assert!(d.stop_requested() && d.wanted(0));
        d.resume(Resume::Cont).unwrap();
        assert!(!d.stop_requested());
        let note = Arc::new(Note::new());
        for _ in 0..2 {
            let (engine, n) = (d.clone(), note.clone());
            let held = std::thread::spawn(move || {
                let mut mem = alloc::vec![1u8, 2, 3, 4, 5];
                let how = engine.hold(&n, |a| match a {
                    Access::Read { at, len } => {
                        let at = *at as usize;
                        Ok(mem[at..(at + len).min(mem.len())].to_vec())
                    }
                    Access::Write { at, data } => {
                        mem[*at as usize..][..data.len()].copy_from_slice(data);
                        Ok(Vec::new())
                    }
                });
                (how, mem)
            });
            while !d.stopped() {
                std::thread::yield_now();
            }
            assert_eq!(d.access(read.clone()).unwrap(), [1, 2, 3, 4]);
            d.access(Access::Write {
                at: 1,
                data: alloc::vec![9],
            })
            .unwrap();
            d.resume(Resume::Step).unwrap();
            let (how, mem) = held.join().unwrap();
            assert_eq!((how, mem), (Some(Resume::Step), alloc::vec![1, 9, 3, 4, 5]));
            assert!(!d.stopped());
        }
        // A kill ends the hold.
        let (engine, n) = (d.clone(), note.clone());
        let held = std::thread::spawn(move || engine.hold(&n, |_| Ok(Vec::new())));
        while !d.stopped() {
            std::thread::yield_now();
        }
        note.kill();
        assert_eq!(held.join().unwrap(), None);
        assert!(!d.stopped());
    }
}
