//! wasmtime backend for [`purg_vm::Engine`]: Cranelift-compiled, the hosted
//! default.
//!
//! Guest code is interrupted every [`TICK`] (epoch interruption) to check
//! the prog's note: a prog killed while computing, with no host call to
//! end it at (see [`purg_vm::host_call`]), is stopped there with status
//! `killed`. The wasmi backend does the same with fuel.
//!
//! A traced prog (`docs/debug.md`) runs a module that keeps its own call
//! stack (`purg_vm::trace`), the same module wasmi runs, so that a stack
//! reads the same on both engines. The backend copies it into the prog
//! before each host call, at each tick, where it is also the profiler's
//! sample and the counts are copied, and after a trap. The same points
//! are where a debugger's `stop` and breakpoints take hold.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use purg_core::{Error, Proc};
use purg_vm::{Ctx, Stop, trace};
use purg_vm_aot::host::{self, Backend, Keep, Reactors, bad_exec};
use wasmtime::{
    AsContext, Caller, Config, Engine, Extern, ExternType, Linker, Memory, Module, SharedMemory,
    Store, UpdateDeadline,
};

/// Compiled modules kept for reuse; the cache is emptied when full.
const CACHED_MODULES: usize = 64;

/// How often running guests check their note.
pub const TICK: Duration = Duration::from_millis(10);

/// The status of a killed prog.
pub use purg_vm::KILLED;

pub struct Wasmtime(Arc<Inner>);

struct Inner {
    engine: Engine,
    /// Set once, when the engine is made: `thread-spawn` needs the linker
    /// it is registered in.
    linker: OnceLock<Linker<Ctx>>,
    reactors: Arc<Reactors>,
    /// Compiling is the slow part of starting a prog, so modules are cached
    /// by their exact bytes.
    modules: Mutex<HashMap<Box<[u8]>, Module>>,
    /// The threaded module each running prog is an instance of, by pid:
    /// what a thread it spawns is another instance of.
    threaded: Mutex<HashMap<u32, Threaded>>,
}

/// A threaded module, the memory its instances share and the progs that
/// are its threads now (`docs/threads.md`).
struct Group {
    module: Module,
    memory: SharedMemory,
    /// The prog that ran the command: when it ends, the others are killed.
    main: u32,
    members: Mutex<HashMap<u32, Arc<Proc>>>,
}

type Threaded = Arc<Group>;

impl Group {
    fn members(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Arc<Proc>>> {
        self.members.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wakes every thread that waits in `memory.atomic.wait`, as a
    /// spurious wake-up, which a futex's users expect. wasmtime parks a
    /// waiter where a kill cannot reach it, and has no way to wake them
    /// all, so this notifies each address a 32-bit wait can be on.
    fn wake_all(&self) {
        for addr in (0..self.memory.data_size() as u64).step_by(4) {
            let _ = self.memory.atomic_notify(addr, u32::MAX);
        }
    }
}

/// What a store starts: a command at its entry point, or a thread of a
/// threaded module at `wasi_thread_start(tid, arg)`.
enum Start {
    Command(Vec<u8>),
    Thread(Threaded, i32, i32),
}

/// The import a threaded module's memory comes by, and the import and
/// export of wasi-threads.
const SHARED_MEMORY: (&str, &str) = ("env", "memory");
const THREAD_SPAWN: (&str, &str) = ("wasi", "thread-spawn");
const THREAD_START: &str = "wasi_thread_start";

impl Wasmtime {
    pub fn new() -> Result<Wasmtime, Error> {
        let engine = Engine::new(
            Config::new()
                .epoch_interruption(true)
                // Threaded modules (`docs/threads.md`).
                .wasm_threads(true)
                .shared_memory(true),
        )
        .map_err(|e| Error::from(e.to_string()))?;
        let reactors = Reactors::new();
        let mut linker = host::linker(&engine, &Arc::new(Compiler), &reactors)?;
        let inner = Arc::new(Inner {
            engine,
            linker: OnceLock::new(),
            reactors,
            modules: Mutex::new(HashMap::new()),
            threaded: Mutex::new(HashMap::new()),
        });
        // The closure must not keep the engine alive: the engine owns it.
        let weak = Arc::downgrade(&inner);
        linker
            .func_wrap(
                THREAD_SPAWN.0,
                THREAD_SPAWN.1,
                move |mut caller: Caller<'_, Ctx>, arg: i32| -> wasmtime::Result<i32> {
                    let spawned = weak
                        .upgrade()
                        .ok_or(Error::new(purg_abi::error::EIO))
                        .and_then(|inner| inner.spawn_thread(caller.data(), arg));
                    // wasi-threads: the thread's id, or a negative number.
                    Ok(match spawned {
                        Ok(tid) => tid,
                        Err(e) => {
                            caller.data_mut().prog.set_error(&e);
                            -1
                        }
                    })
                },
            )
            .map_err(|e| Error::from(e.to_string()))?;
        let _ = inner.linker.set(linker);
        // The ticker ends with the engine.
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name(String::from("epoch"))
            .spawn(move || {
                while let Some(inner) = weak.upgrade() {
                    inner.engine.increment_epoch();
                    inner.wake_killed();
                    drop(inner);
                    std::thread::sleep(TICK);
                }
            })
            .map_err(|e| Error::from(e.to_string()))?;
        Ok(Wasmtime(inner))
    }
}

impl Inner {
    fn linker(&self) -> &Linker<Ctx> {
        self.linker.get().expect("set when the engine was made")
    }

    /// Wakes the threads of every group that has a killed thread still
    /// running, each tick until it is gone: a thread killed while it
    /// waits on a lock comes back into guest code, where the epoch that
    /// has just passed ends it.
    fn wake_killed(&self) {
        let groups: Vec<Threaded> = {
            let table = self.threaded.lock().unwrap_or_else(|e| e.into_inner());
            let mut groups: Vec<Threaded> = Vec::new();
            for g in table.values() {
                if !groups.iter().any(|h| Arc::ptr_eq(g, h)) {
                    groups.push(g.clone());
                }
            }
            groups
        };
        for g in groups {
            let killed = g.members().values().any(|p| p.note.killed());
            if killed {
                g.wake_all();
            }
        }
    }

    /// `thread-spawn`: starts another instance of the caller's threaded
    /// module, on the same memory, as a new prog. Returns its pid, which
    /// is the thread id the guest's `wasi_thread_start` is given.
    fn spawn_thread(self: Arc<Self>, parent: &Ctx, arg: i32) -> Result<i32, Error> {
        let lock = || self.threaded.lock().unwrap_or_else(|e| e.into_inner());
        let threaded = lock()
            .get(&parent.prog.pid)
            .cloned()
            .ok_or(Error::from("thread-spawn: not a threaded module"))?;
        let inner = self.clone();
        // The thread's id is its pid, which its own context holds.
        let pid = purg_vm::spawn_thread(parent, move |ctx| {
            let tid = ctx.prog.pid as i32;
            inner.run(Start::Thread(threaded, tid, arg), ctx)
        })?;
        // wasi-threads wants an id from 1 to 0x1fffffff.
        i32::try_from(pid)
            .ok()
            .filter(|p| (1..=0x1fff_ffff).contains(p))
            .ok_or(Error::from("thread-spawn: pid out of range"))
    }

    fn module(&self, wasm: &[u8]) -> Result<Module, Error> {
        let lock = || self.modules.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = lock().get(wasm) {
            return Ok(m.clone());
        }
        let module = Module::new(&self.engine, wasm).map_err(bad_exec)?;
        let mut cache = lock();
        if cache.len() >= CACHED_MODULES {
            cache.clear();
        }
        cache.insert(wasm.into(), module.clone());
        Ok(module)
    }
}

/// Records the call stack of a traced instance, whose stack memory
/// (`trace::STACK`) is `stack` and linear memory `main`, in its prog, and
/// does what a debugger wants of it there: a stop, or new breakpoints.
///
/// The tick callback has the store only to read: wasmtime keeps guests
/// from changing wasm state there. The debugger must write the stack
/// memory to release a function held at a breakpoint, and may write the
/// linear memory, so both are written through their raw contents.
fn debug_point(
    store: &impl AsContext<Data = Ctx>,
    (stack, main): Mems,
    sample: bool,
) -> Result<(), Stop> {
    let proc = &store.as_context().data().prog.proc;
    let hit = trace::record(proc, stack.data(store), sample);
    if !proc.debug.wanted(hit) {
        return Ok(());
    }
    // SAFETY, for both slices: this is the thread of the store's guest,
    // which is suspended in a host call or at an epoch check, and each
    // slice is the only reference to its memory's contents while it
    // lives. The memories cannot grow meanwhile: that needs the store,
    // which this only reads.
    let table = match main {
        Some(main) => {
            let (at, len) = raw(main, store);
            trace::stop_point(proc, hit, unsafe {
                std::slice::from_raw_parts_mut(at, len)
            })?
        }
        None => trace::stop_point(proc, hit, &mut [])?,
    };
    if let Some(table) = table {
        let (at, len) = raw(stack, store);
        trace::patch(unsafe { std::slice::from_raw_parts_mut(at, len) }, &table);
    }
    Ok(())
}

/// Where the contents of `mem` are, and their size.
fn raw(mem: Memory, store: &impl AsContext) -> (*mut u8, usize) {
    (mem.data_ptr(store), mem.data_size(store))
}

/// [`debug_point`] for a host call's caller. Only the prog's command is
/// traced: a call from a reactor leaves the stack as its `call` saw it.
fn debug_caller(caller: &mut Caller<'_, Ctx>) -> wasmtime::Result<()> {
    if !caller.data().prog.proc.debug.traced() {
        return Ok(());
    }
    let Some(stack) = caller
        .get_export(trace::STACK)
        .and_then(Extern::into_memory)
    else {
        return Ok(());
    };
    let main = caller
        .get_export(purg_abi::MEMORY)
        .and_then(Extern::into_memory);
    debug_point(caller, (stack, main), false).map_err(wasmtime::Error::new)
}

/// A traced command instance's stack memory and linear memory.
type Mems = (Memory, Option<Memory>);

/// The memories of a traced prog's command instance, for its store's
/// tick callback.
type Traced = Arc<Mutex<Option<Mems>>>;

/// Reactors are compiled when they are loaded, and a traced prog's
/// host calls are its debugger's points.
struct Compiler;

impl Backend for Compiler {
    fn reactor(
        &self,
        caller: &mut Caller<'_, Ctx>,
        _path: &str,
        wasm: Vec<u8>,
    ) -> Result<(Module, Keep), Error> {
        Ok((Module::new(caller.engine(), &wasm).map_err(bad_exec)?, None))
    }

    fn before_call(caller: &mut Caller<'_, Ctx>) -> wasmtime::Result<()> {
        debug_caller(caller)
    }
}

/// The type of the shared memory `module` imports, if it is a threaded
/// module.
fn shared_import(module: &Module) -> Option<wasmtime::MemoryType> {
    module.imports().find_map(|i| match i.ty() {
        ExternType::Memory(ty) if (i.module(), i.name()) == SHARED_MEMORY && ty.is_shared() => {
            Some(ty)
        }
        _ => None,
    })
}

impl Inner {
    fn call(&self, store: &mut Store<Ctx>, start: Start, traced: &Traced) -> Result<Stop, Error> {
        let pid = store.data().prog.pid;
        let (module, thread) = match start {
            Start::Command(wasm) => (self.module(&wasm)?, None),
            Start::Thread(threaded, tid, arg) => {
                let module = threaded.module.clone();
                self.join_group(store.data(), threaded);
                (module, Some((tid, arg)))
            }
        };
        // A threaded command makes the memory its threads will share.
        if thread.is_none()
            && let Some(ty) = shared_import(&module)
        {
            // The memory's first pages are the prog's, within its limits.
            // (What it grows by later is not charged: `docs/threads.md`.)
            let first = usize::try_from(ty.minimum().saturating_mul(65536)).unwrap_or(usize::MAX);
            if !purg_vm::memory_growing(store.data(), 0, first) {
                return Err(Error::new(purg_abi::error::ENOMEM));
            }
            let memory = SharedMemory::new(&self.engine, ty).map_err(bad_exec)?;
            self.join_group(
                store.data(),
                Arc::new(Group {
                    module: module.clone(),
                    memory,
                    main: pid,
                    members: Mutex::new(HashMap::new()),
                }),
            );
        }
        let shared = self
            .threaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pid)
            .map(|t| t.memory.clone());
        let instance = match shared {
            Some(memory) => {
                let mut linker = self.linker().clone();
                linker
                    .define(&mut *store, SHARED_MEMORY.0, SHARED_MEMORY.1, memory)
                    .map_err(bad_exec)?;
                linker.instantiate(&mut *store, &module)
            }
            None => self.linker().instantiate(&mut *store, &module),
        }
        .map_err(bad_exec)?;
        if let Some((tid, arg)) = thread {
            let entry = instance
                .get_typed_func::<(i32, i32), ()>(&mut *store, THREAD_START)
                .map_err(bad_exec)?;
            return Ok(host::stop(entry.call(&mut *store, (tid, arg))));
        }
        let entry = instance
            .get_typed_func::<(), ()>(&mut *store, purg_abi::ENTRY)
            .map_err(bad_exec)?;
        let stack = if store.data().prog.proc.debug.traced() {
            instance.get_memory(&mut *store, trace::STACK)
        } else {
            None
        };
        let main = instance.get_memory(&mut *store, purg_abi::MEMORY);
        *traced.lock().unwrap_or_else(|e| e.into_inner()) = stack.map(|s| (s, main));
        let res = entry.call(&mut *store, ());
        // A traced prog's last stack, and its counts, stay readable.
        if let Some(stack) = stack {
            let proc = &store.data().prog.proc;
            trace::record(proc, stack.data(&*store), false);
            trace::counts(proc, stack.data(&*store));
        }
        let trapped = res
            .as_ref()
            .is_err_and(|e| e.downcast_ref::<Stop>().is_none());
        let stop = host::stop(res);
        // A traced prog that traps is broken: its stack at the trap and
        // the trap itself stay readable.
        if trapped
            && stack.is_some()
            && let Stop::Exit(status) = &stop
        {
            store.data().prog.proc.debug.set_exception(status);
        }
        Ok(stop)
    }
}

impl purg_vm::Engine for Wasmtime {
    fn name(&self) -> &'static str {
        "wasmtime"
    }

    fn run(&self, wasm: Vec<u8>, ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        self.0.run(Start::Command(wasm), ctx)
    }

    fn threads(&self) -> bool {
        true
    }
}

impl Inner {
    /// Makes `ctx`'s prog a thread of `group`.
    fn join_group(&self, ctx: &Ctx, group: Threaded) {
        group.members().insert(ctx.prog.pid, ctx.prog.proc.clone());
        self.threaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ctx.prog.pid, group);
    }

    /// Takes a prog that has stopped out of its group, if it was a
    /// thread. The group's other threads are killed with it when it was
    /// the command's own prog, as a process's threads end with it, and
    /// when it did not end cleanly: a thread that traps, is killed or
    /// exits with a status leaves locks held and joins that would never
    /// return.
    ///
    /// A detached prog (`detach` on its `ctl`) is outside this both ways:
    /// its end kills nobody, and nobody's end kills it. It is a Limbo
    /// `spawn`, which the program has said it is written for.
    fn leave_group(&self, pid: u32, clean: bool) {
        let group = self
            .threaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pid);
        let Some(group) = group else {
            return;
        };
        let mut members = group.members();
        let me = members.remove(&pid);
        let detached = me.is_some_and(|p| p.detached());
        if !detached && (pid == group.main || !clean) {
            for p in members.values().filter(|p| !p.detached()) {
                p.kill();
            }
        }
    }

    fn run(&self, start: Start, ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        let pid = ctx.prog.pid;
        let mut store = Store::new(&self.engine, ctx);
        host::limit(&mut store);
        let traced: Traced = Arc::new(Mutex::new(None));
        let ticked = traced.clone();
        store.epoch_deadline_callback(move |state| {
            // Each tick records a traced prog's stack, and a sample if
            // the prog is profiled, and stops it for a debugger.
            let mems = *ticked.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mems) = mems {
                debug_point(&state, mems, true).map_err(wasmtime::Error::new)?;
            }
            if purg_vm::checkpoint(state.data()) {
                return Err(wasmtime::Error::new(Stop::killed()));
            }
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);
        let res = self.call(&mut store, start, &traced);
        let ctx = store.into_data();
        self.reactors.exited(pid);
        // The shared memory goes with its last thread.
        let clean = matches!(&res, Ok(Stop::Exit(status)) if status.is_empty());
        self.leave_group(pid, clean);
        (ctx, res)
    }
}
