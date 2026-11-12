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

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use purg_abi::error::{EBADADDR, EBADEXEC};
use purg_core::Error;
use purg_vm::{Ctx, Mem, Stop, trace};
use wasmtime::{
    AsContext, AsContextMut, Caller, Config, Engine, Extern, Instance, Linker, Memory, Module,
    Store, UpdateDeadline,
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
        let mut linker = Linker::new(&engine);
        link(&mut linker).map_err(|e| Error::from(e.to_string()))?;
        register_modules(&mut linker).map_err(|e| Error::from(e.to_string()))?;
        Ok(Wasmtime {
            engine,
            linker,
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

fn memory<'c>(caller: &'c mut Caller<'_, Ctx>) -> wasmtime::Result<(&'c mut [u8], &'c mut Ctx)> {
    let Some(Extern::Memory(mem)) = caller.get_export(purg_abi::MEMORY) else {
        return Err(wasmtime::Error::msg("guest exports no memory"));
    };
    Ok(mem.data_and_store_mut(caller.as_context_mut()))
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

fn link(linker: &mut Linker<Ctx>) -> wasmtime::Result<()> {
    macro_rules! register {
        ($module:expr, $name:literal, $f:path, ($($a:ident : $t:ty),*) -> $r:ty) => {
            linker.func_wrap(
                $module,
                $name,
                |mut caller: Caller<'_, Ctx>, $($a: $t),*| -> wasmtime::Result<$r> {
                    debug_caller(&mut caller)?;
                    let (data, ctx) = memory(&mut caller)?;
                    purg_vm::host_call(ctx, |ctx| $f(ctx, &mut Mem(data), $($a),*))
                        .map_err(wasmtime::Error::new)
                },
            )?;
        };
    }
    purg_vm::for_each_import!(register);
    Ok(())
}

type Reactors = Arc<Mutex<BTreeMap<u32, Vec<Instance>>>>;

fn register_modules(linker: &mut Linker<Ctx>) -> wasmtime::Result<()> {
    let modules: Reactors = Arc::new(Mutex::new(BTreeMap::new()));
    let loading = modules.clone();
    let calling = modules;
    linker.func_wrap(
        purg_abi::MODULE,
        purg_abi::sys::LOAD,
        move |mut caller: Caller<'_, Ctx>, path: i32, plen: i32| -> wasmtime::Result<i32> {
            killed(&caller)?;
            let r = match load_reactor(&mut caller, &loading, path, plen) {
                Ok(handle) => handle,
                Err(e) => {
                    caller.data_mut().prog.set_error(&e);
                    -1
                }
            };
            killed(&caller)?;
            Ok(r)
        },
    )?;
    linker.func_wrap(
        purg_abi::MODULE,
        purg_abi::sys::CALL,
        move |mut caller: Caller<'_, Ctx>,
              handle: i32,
              name: i32,
              nlen: i32,
              args: i32,
              alen: i32,
              out: i32,
              olen: i32|
              -> wasmtime::Result<i32> {
            killed(&caller)?;
            let r = match call_reactor(
                &mut caller,
                &calling,
                Call {
                    handle,
                    name,
                    nlen,
                    args,
                    alen,
                    out,
                    olen,
                },
            ) {
                Ok(n) => n,
                Err(e) => {
                    caller.data_mut().prog.set_error(&e);
                    -1
                }
            };
            killed(&caller)?;
            Ok(r)
        },
    )?;
    Ok(())
}

/// Ends a killed prog at `load` or `call`, as [`purg_vm::host_call`] does
/// at the other host calls.
fn killed(caller: &Caller<'_, Ctx>) -> wasmtime::Result<()> {
    if caller.data().prog.proc.note.killed() {
        return Err(wasmtime::Error::new(Stop::killed()));
    }
    Ok(())
}

const PAGE: usize = 65536;

fn guest_vec(caller: &mut Caller<'_, Ctx>, ptr: i32, len: i32) -> Result<Vec<u8>, Error> {
    let (data, _) = memory(caller).map_err(|_| Error::new(EBADADDR))?;
    Ok(Mem(data).slice(ptr, len)?.to_vec())
}

fn guest_str(caller: &mut Caller<'_, Ctx>, ptr: i32, len: i32) -> Result<String, Error> {
    Ok(std::str::from_utf8(&guest_vec(caller, ptr, len)?)
        .map_err(|_| Error::new(purg_abi::error::EBADARG))?
        .to_string())
}

fn load_reactor(
    caller: &mut Caller<'_, Ctx>,
    modules: &Reactors,
    path: i32,
    plen: i32,
) -> Result<i32, Error> {
    let path = guest_str(caller, path, plen)?;
    let wasm = {
        let ctx = caller.data_mut();
        purg_vm::load_module(&mut ctx.prog, &path)?
    };
    let engine = caller.engine().clone();
    let module = Module::new(&engine, &wasm).map_err(bad_exec)?;
    let mut linker = Linker::new(&engine);
    link(&mut linker).map_err(|e| Error::from(e.to_string()))?;
    register_modules(&mut linker).map_err(|e| Error::from(e.to_string()))?;
    let instance = linker
        .instantiate(&mut *caller, &module)
        .map_err(bad_exec)?;
    let pid = caller.data().prog.pid;
    let mut table = modules.lock().unwrap_or_else(|e| e.into_inner());
    let list = table.entry(pid).or_default();
    list.push(instance);
    Ok(list.len() as i32)
}

struct Call {
    handle: i32,
    name: i32,
    nlen: i32,
    args: i32,
    alen: i32,
    out: i32,
    olen: i32,
}

fn call_reactor(
    caller: &mut Caller<'_, Ctx>,
    modules: &Reactors,
    call: Call,
) -> Result<i32, Error> {
    let export = guest_str(caller, call.name, call.nlen)?;
    let arguments = guest_vec(caller, call.args, call.alen)?;
    if arguments.len() > PAGE || call.olen < 0 {
        return Err(Error::from("bad argument"));
    }
    let instance = {
        let pid = caller.data().prog.pid;
        let table = modules.lock().unwrap_or_else(|e| e.into_inner());
        let list = table.get(&pid).ok_or(Error::from("bad module handle"))?;
        let index = usize::try_from(call.handle)
            .ok()
            .and_then(|h| h.checked_sub(1))
            .ok_or(Error::from("bad module handle"))?;
        *list.get(index).ok_or(Error::from("bad module handle"))?
    };
    let callee = instance
        .get_memory(&mut *caller, purg_abi::MEMORY)
        .ok_or(Error::from("module exports no memory"))?;
    let old = callee
        .grow(&mut *caller, 1)
        .map_err(|_| Error::from("out of memory"))?;
    let base = usize::try_from(old).map_err(|_| Error::from("out of memory"))? * PAGE;
    callee
        .write(&mut *caller, base, &arguments)
        .map_err(|_| Error::new(EBADADDR))?;
    let func = instance
        .get_typed_func::<(i32, i32), i32>(&mut *caller, &export)
        .map_err(|_| Error::from("export not found"))?;
    let n = func
        .call(&mut *caller, (base as i32, arguments.len() as i32))
        .map_err(|e| Error::from(format!("trap: {e}")))?;
    if n < 0 || n as usize > PAGE || n as usize > call.olen as usize {
        return Err(Error::from("bad result length"));
    }
    let mut result = vec![0u8; n as usize];
    callee
        .read(&mut *caller, base, &mut result)
        .map_err(|_| Error::new(EBADADDR))?;
    let (data, _) = memory(caller).map_err(|_| Error::new(EBADADDR))?;
    Mem(data).write(call.out, &result)?;
    Ok(n)
}

fn bad_exec(e: impl std::fmt::Display) -> Error {
    Error::from(format!("{EBADEXEC}: {e}"))
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
        Ok(match res {
            Ok(()) => Stop::Exit(String::new()),
            Err(e) => match e.downcast_ref::<Stop>() {
                Some(stop) => stop.clone(),
                None => {
                    let status = format!("trap: {}", e.root_cause());
                    // A traced prog that traps is broken: its stack at
                    // the trap and the trap itself stay readable.
                    if stack.is_some() {
                        store.data().prog.proc.debug.set_exception(&status);
                    }
                    Stop::Exit(status)
                }
            },
        })
    }
}

/// The prog's memory limits as wasmtime's resource limiter: the store's
/// data, seen as a limiter.
#[repr(transparent)]
struct Limiter(Ctx);

impl Limiter {
    fn of(ctx: &mut Ctx) -> &mut dyn wasmtime::ResourceLimiter {
        // SAFETY: `Limiter` is `repr(transparent)` over `Ctx`, so the two
        // have the same layout, and the borrow is the caller's.
        unsafe { &mut *(ctx as *mut Ctx).cast::<Limiter>() }
    }
}

impl wasmtime::ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(purg_vm::memory_growing(&self.0, current, desired))
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(purg_vm::table_growing(&self.0, current, desired))
    }
}

impl purg_vm::Engine for Wasmtime {
    fn name(&self) -> &'static str {
        "wasmtime"
    }

    fn run(&self, wasm: Vec<u8>, ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        let mut store = Store::new(&self.engine, ctx);
        store.limiter(Limiter::of);
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
        (store.into_data(), res)
    }
}
