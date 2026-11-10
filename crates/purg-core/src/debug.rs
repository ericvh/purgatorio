//! A prog's debugging state (`docs/debug.md`): whether it is traced, its
//! last call stack, the trap that broke it, and its profile.
//!
//! The VM fills this in; `#p` serves it as the `stack`, `exception` and
//! `prof` files and sets the switches from `ctl`.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use purg_platform::sync::Mutex;

/// Modules this prog starts are instrumented with a call stack. Inherited.
const TRACE: u32 = 1;
/// Traced progs are sampled. Inherited.
const PROF: u32 = 2;
/// This prog's own module carries a call stack.
const TRACED: u32 = 4;
/// This prog trapped while traced: it stays in the table until killed.
const BROKEN: u32 = 8;

/// The most frames a stack snapshot keeps: the innermost ones.
pub const MAX_FRAMES: usize = 256;

/// A call stack: function indices in the prog's command module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stack {
    /// The innermost frame first; at most [`MAX_FRAMES`].
    pub frames: Vec<u32>,
    /// The number of frames the prog had.
    pub depth: u32,
}

/// Samples by `(module path, function index)`.
pub type Profile = BTreeMap<(String, u32), u64>;

pub struct Debug {
    flags: AtomicU32,
    stack: Mutex<Stack>,
    exception: Mutex<String>,
    /// This prog's samples, by function index.
    own: Mutex<BTreeMap<u32, u64>>,
    /// The samples of its children that have exited.
    children: Mutex<Profile>,
}

impl Default for Debug {
    fn default() -> Debug {
        Debug {
            flags: AtomicU32::new(0),
            stack: Mutex::new(Stack::default()),
            exception: Mutex::new(String::new()),
            own: Mutex::new(BTreeMap::new()),
            children: Mutex::new(Profile::new()),
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
        let bits = parent.flags.load(Ordering::Acquire) & (TRACE | PROF);
        self.flags.store(bits, Ordering::Release);
    }

    /// Whether commands this prog starts are to carry a call stack.
    pub fn trace(&self) -> bool {
        self.has(TRACE)
    }

    pub fn set_trace(&self, on: bool) {
        self.set(TRACE, on);
        if !on {
            self.set(PROF, false);
        }
    }

    /// Whether this prog, if traced, is sampled, and whether it collects
    /// the samples of its children.
    pub fn profiling(&self) -> bool {
        self.has(PROF)
    }

    /// Turning profiling on also turns tracing on, and clears the samples
    /// collected before.
    pub fn set_prof(&self, on: bool) {
        if on && !self.has(PROF) {
            self.own.lock().clear();
            self.children.lock().clear();
            self.set(TRACE, true);
        }
        self.set(PROF, on);
    }

    /// Whether this prog's module carries a call stack.
    pub fn traced(&self) -> bool {
        self.has(TRACED)
    }

    /// Marks this prog's module as carrying a call stack.
    pub fn set_traced(&self) {
        self.set(TRACED, true);
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
        *self.own.lock().entry(func).or_insert(0) += 1;
    }

    /// This prog's samples under `path`, its module, with those of its
    /// exited children.
    pub fn profile(&self, path: &str) -> Profile {
        let mut all = self.children.lock().clone();
        for (&func, &n) in self.own.lock().iter() {
            *all.entry((String::from(path), func)).or_insert(0) += n;
        }
        all
    }

    /// Adds the profile of a child that has exited.
    pub(crate) fn absorb(&self, child: Profile) {
        let mut mine = self.children.lock();
        for (key, n) in child {
            *mine.entry(key).or_insert(0) += n;
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
        d.set_trace(false);
        assert!(!d.trace() && !d.profiling());
    }

    /// A child gets the switches, not the state.
    #[test]
    fn inherits_the_switches_only() {
        let parent = Debug::default();
        parent.set_prof(true);
        parent.set_traced();
        parent.set_exception("trap: x");
        parent.sample(3);
        let child = Debug::default();
        child.inherit(&parent);
        assert!(child.trace() && child.profiling());
        assert!(!child.traced() && !child.broken());
        assert!(child.exception().is_empty());
        assert!(child.profile("/bin/c.wasm").is_empty());
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
        d.absorb(child.clone());
        d.absorb(child);
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
}
