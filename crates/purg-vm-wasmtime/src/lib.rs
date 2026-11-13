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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use purg_core::Error;
use purg_vm::{Ctx, Stop, trace};
use purg_vm_aot::host::{self, Backend, Keep, Reactors, bad_exec};
use wasmtime::{
    AsContext, Caller, Config, Engine, Extern, Linker, Memory, Module, Store, UpdateDeadline,
};

/// Compiled modules kept for reuse; the cache is emptied when full.
const CACHED_MODULES: usize = 64;

/// How often running guests check their note.
pub const TICK: Duration = Duration::from_millis(10);

/// The status of a killed prog.
pub use purg_vm::KILLED;

pub struct Wasmtime {
    engine: Engine,
    linker: Linker<Ctx>,
    reactors: Arc<Reactors>,
    /// Compiling is the slow part of starting a prog, so modules are cached
    /// by their exact bytes.
    modules: Mutex<HashMap<Box<[u8]>, Module>>,
}

impl Wasmtime {
    pub fn new() -> Result<Wasmtime, Error> {
        let engine = Engine::new(Config::new().epoch_interruption(true))
            .map_err(|e| Error::from(e.to_string()))?;
        // The ticker ends with the engine.
        let weak = engine.weak();
        std::thread::Builder::new()
            .name(String::from("epoch"))
            .spawn(move || {
                while let Some(engine) = weak.upgrade() {
                    engine.increment_epoch();
                    drop(engine);
                    std::thread::sleep(TICK);
                }
            })
            .map_err(|e| Error::from(e.to_string()))?;
        let reactors = Reactors::new();
        let linker = host::linker(&engine, &Arc::new(Compiler), &reactors)?;
        Ok(Wasmtime {
            engine,
            linker,
            reactors,
            modules: Mutex::new(HashMap::new()),
        })
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

impl Wasmtime {
    fn call(&self, store: &mut Store<Ctx>, wasm: Vec<u8>, traced: &Traced) -> Result<Stop, Error> {
        let module = self.module(&wasm)?;
        drop(wasm);
        let instance = self
            .linker
            .instantiate(&mut *store, &module)
            .map_err(bad_exec)?;
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
            if state.data().prog.proc.note.killed() {
                return Err(wasmtime::Error::new(Stop::killed()));
            }
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);
        let res = self.call(&mut store, wasm, &traced);
        let ctx = store.into_data();
        self.reactors.exited(pid);
        (ctx, res)
    }
}
