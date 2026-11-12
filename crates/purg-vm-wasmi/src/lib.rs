//! wasmi backend for [`purg_vm::Engine`]: a pure-Rust `no_std` interpreter,
//! used by the bare-metal probe and kernel and selectable in the emu with
//! `--vm=wasmi`.
//!
//! Guest code runs on fuel, [`SLICE`] units at a time. When a slice runs
//! out the backend checks the prog's note: a prog killed while computing,
//! with no host call to end it at (see [`purg_vm::host_call`]), is stopped
//! there with status `killed`. The emu's wasmtime backend does the same
//! with epoch interruption.
//!
//! wasmi cannot say which functions a guest is in. A traced prog
//! (`docs/debug.md`) runs a module that keeps its own call stack
//! (`purg_vm::trace`); the backend copies it into the prog before each
//! host call, at the end of each slice, where it is also the profiler's
//! sample and the counts are copied, and after a trap. The same points
//! are where a debugger's `stop` and breakpoints take hold.
//!
//! # Engines and the module cache
//!
//! A wasmi engine keeps every function it has translated, and the module
//! bytes behind those not yet translated, until the engine is dropped;
//! dropping the `Module` frees neither. So engines are not kept for good:
//! progs share an engine in *generations*. The current generation's
//! engine holds each module it has loaded, by its SHA-256 digest, so a
//! command run again while the generation lives is neither parsed nor
//! translated again. A generation lives while a prog that started in it
//! runs, and no longer: when the last one exits its engine and all its
//! code are freed, as an engine per prog was. It stops taking new
//! modules once [`GENERATION_BYTES`] of them are in it, and new progs
//! start a new one; when memory is tight ([`tight`]) a prog gets an
//! engine of its own, as does a traced prog, whose module is rewritten.
//!
//! Modules are compiled lazily ([`wasmi::CompilationMode::Lazy`]): a
//! function is validated and translated on its first call. Parsing the
//! module checks its structure, its types, imports and exports; a
//! function body that is not valid WebAssembly is found only when it is
//! first called, and that call traps.
#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicUsize, Ordering};

use purg_abi::error::{EBADADDR, EBADEXEC};
use purg_core::Error;
use purg_vm::{Ctx, Mem, Stop, trace};
use wasmi::{
    AsContextMut, Caller, Config, CustomFuelCosts, Engine, Extern, Instance, Linker, Memory,
    Module, Store, TypedFunc, TypedResumableCall, WasmParams, WasmResults,
};

/// Fuel per slice: roughly the number of wasm instructions run between
/// note checks.
pub const SLICE: u64 = 1 << 20;

/// The length of a module digest, SHA-256, as `purg_core::modsig::Module`
/// records it.
const DIGEST: usize = 32;

/// The status of a killed prog.
pub use purg_vm::KILLED;

/// The backend: the configuration and the current generation (see the
/// crate docs).
pub struct Wasmi {
    config: Config,
    /// [`Wasmi::config`] with room for [`TRACED_DEPTH`] frames.
    traced: Config,
    /// The generation new progs join, while a prog of it runs.
    current: spin::Mutex<Weak<Generation>>,
}

/// The bytes of WebAssembly a generation takes before new progs start a
/// new one. Its engine holds about four times as much once the code is
/// translated (`docs/footprint.md` Section 4), for as long as one of its
/// progs runs.
pub const GENERATION_BYTES: usize = 1 << 20;

/// An engine shared by the progs that started while it was current, with
/// the modules loaded into it and the linker its progs instantiate with.
struct Generation {
    engine: Engine,
    linker: Linker<Ctx>,
    /// The reactors `linker`'s `load` made, for each prog of the
    /// generation; a prog's are dropped when it exits.
    reactors: Modules,
    cache: Arc<Cache>,
}

/// Where a prog runs: in a generation, or in an engine of its own.
enum Home {
    Shared(Arc<Generation>),
    Own(Engine, Linker<Ctx>),
}

impl Home {
    fn engine(&self) -> &Engine {
        match self {
            Home::Shared(g) => &g.engine,
            Home::Own(e, _) => e,
        }
    }

    fn linker(&self) -> &Linker<Ctx> {
        match self {
            Home::Shared(g) => &g.linker,
            Home::Own(_, l) => l,
        }
    }
}

/// The modules loaded into a generation's engine, by digest.
#[derive(Default)]
struct Cache {
    modules: spin::Mutex<BTreeMap<[u8; DIGEST], Module>>,
    /// Bytes of WebAssembly loaded into the engine. Only a bound: no
    /// other memory is ordered by it, so it is accessed `Relaxed`.
    bytes: AtomicUsize,
}

impl Cache {
    /// Whether new progs start a new generation rather than join this one.
    fn full(&self) -> bool {
        self.bytes.load(Ordering::Relaxed) >= GENERATION_BYTES
    }

    /// The module `wasm`, whose digest is `digest`, in `engine`, for a prog
    /// of the generation (`ctx`): the one loaded before, or a new one, kept
    /// unless the generation is full. Loading a new one when memory is
    /// tight fills the generation, so that no new prog joins it.
    fn module(
        &self,
        ctx: &Ctx,
        engine: &Engine,
        digest: &[u8; DIGEST],
        wasm: &[u8],
    ) -> Result<Module, Error> {
        if let Some(m) = self.modules.lock().get(digest) {
            return Ok(m.clone());
        }
        let added = if tight(ctx) {
            GENERATION_BYTES
        } else {
            wasm.len()
        };
        // Parsed outside the lock; a prog that parsed the same module at
        // the same time uses the first one kept.
        let m = Module::new(engine, wasm).map_err(bad_exec)?;
        self.bytes.fetch_add(added, Ordering::Relaxed);
        if self.full() {
            return Ok(m);
        }
        Ok(self.modules.lock().entry(*digest).or_insert(m).clone())
    }
}

/// Whether memory is tight: under a quarter of it free. A prog that would
/// start a generation then runs in an engine of its own, which goes when
/// it exits.
fn tight(ctx: &Ctx) -> bool {
    let (used, total) = ctx.prog.kernel.platform.memory();
    total > 0 && total.saturating_sub(used) < total / 4
}

/// The digest of the module `ctx`'s prog loaded last: the one
/// `purg_vm::load_module` recorded for the bytes it returned.
fn last_digest(ctx: &Ctx) -> Option<[u8; DIGEST]> {
    ctx.prog.proc.modules().last().map(|m| m.digest)
}

/// wasmi's limit on nested calls (its default).
pub const DEPTH: usize = 1000;

/// The limit for a traced prog. Tracing puts a thunk's frame under every
/// function's (`docs/debug.md`), so a traced prog needs twice the frames
/// for the same recursion. wasmi keeps its frames on the heap and grows
/// them as they are used, so the limit costs nothing until a prog
/// recurses that deep.
pub const TRACED_DEPTH: usize = 2 * DEPTH + 64;

impl Wasmi {
    pub fn new() -> Result<Wasmi, Error> {
        let mut config = Config::default();
        config.consume_fuel(true);
        config.compilation_mode(wasmi::CompilationMode::Lazy);
        // Translating a function on its first call is host work and costs
        // no fuel: wasmi 2 reports running out of fuel there as an error,
        // not as a call that can be resumed.
        config.fuel_cost(CustomFuelCosts {
            bytes_copied_per_fuel: 64,
            fuel_per_bytes_translated: 0,
            fuel_per_bytes_validated: 0,
        });
        config.set_max_recursion_depth(DEPTH);
        // A shared engine would otherwise keep the value stacks of the
        // progs that ran in it.
        config.set_max_cached_stacks(0);
        let mut traced = config.clone();
        traced.set_max_recursion_depth(TRACED_DEPTH);
        let wasmi = Wasmi {
            config,
            traced,
            current: spin::Mutex::new(Weak::new()),
        };
        // Linking fails the same way every time, so report it here.
        linker(&Engine::new(&wasmi.config), None)?;
        Ok(wasmi)
    }

    /// The generation a new prog joins: the current one, or a new one if
    /// there is none or it is full. `None` when there is none and memory
    /// is tight.
    fn generation(&self, ctx: &Ctx) -> Result<Option<Arc<Generation>>, Error> {
        if let Some(g) = self.current.lock().upgrade()
            && !g.cache.full()
        {
            return Ok(Some(g));
        }
        if tight(ctx) {
            return Ok(None);
        }
        let engine = Engine::new(&self.config);
        let cache = Arc::new(Cache::default());
        let (linker, reactors) = linker(&engine, Some(cache.clone()))?;
        let g = Arc::new(Generation {
            engine,
            linker,
            reactors,
            cache,
        });
        let mut current = self.current.lock();
        // Another prog may have made one meanwhile.
        if let Some(other) = current.upgrade()
            && !other.cache.full()
        {
            return Ok(Some(other));
        }
        *current = Arc::downgrade(&g);
        Ok(Some(g))
    }

    /// The home of `ctx`'s prog, and its module `wasm` loaded there. A
    /// traced prog's module is rewritten (`purg_vm::trace::prepare`), so
    /// it is not the one its digest names, and it runs in an engine of
    /// its own.
    fn home(&self, ctx: &Ctx, wasm: &[u8]) -> Result<(Home, Module), Error> {
        let traced = ctx.prog.proc.debug.traced();
        if !traced
            && let Some(digest) = last_digest(ctx)
            && let Some(g) = self.generation(ctx)?
        {
            let module = g.cache.module(ctx, &g.engine, &digest, wasm)?;
            return Ok((Home::Shared(g), module));
        }
        let engine = Engine::new(if traced { &self.traced } else { &self.config });
        let module = Module::new(&engine, wasm).map_err(bad_exec)?;
        let (linker, _) = linker(&engine, None)?;
        Ok((Home::Own(engine, linker), module))
    }
}

/// A linker for `engine` with the host calls and `load`/`call`, and the
/// table of the reactors its `load` makes. `cache` is the engine's module
/// cache when it is a generation's.
fn linker(engine: &Engine, cache: Option<Arc<Cache>>) -> Result<(Linker<Ctx>, Modules), Error> {
    let mut linker = Linker::new(engine);
    link(&mut linker).map_err(|e| Error::from(format!("{e}")))?;
    let reactors = register_modules(&mut linker, cache).map_err(|e| Error::from(format!("{e}")))?;
    Ok((linker, reactors))
}

/// Reactor instances loaded by `load`, keyed by the prog that loaded them.
type Modules = Arc<spin::Mutex<BTreeMap<u32, Vec<Instance>>>>;

/// Carries a [`Stop`] through wasmi's trap machinery.
#[derive(Debug)]
struct HostStop(Stop);

impl core::fmt::Display for HostStop {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl wasmi::errors::HostError for HostStop {}

fn memory<'c>(
    caller: &'c mut Caller<'_, Ctx>,
) -> Result<(&'c mut [u8], &'c mut Ctx), wasmi::Error> {
    let Some(Extern::Memory(mem)) = caller.get_export(purg_abi::MEMORY) else {
        return Err(wasmi::Error::new("guest exports no memory"));
    };
    Ok(mem.data_and_store_mut(caller))
}

/// Records the call stack of a traced instance, whose stack memory
/// (`trace::STACK`) is `stack` and linear memory `main`, in its prog, and
/// does what a debugger wants of it there: a stop, or new breakpoints.
fn debug_point(
    mut store: impl AsContextMut<Data = Ctx>,
    stack: Memory,
    main: Option<Memory>,
    sample: bool,
) -> Result<(), Stop> {
    let hit = {
        let (data, ctx) = stack.data_and_store_mut(&mut store);
        trace::record(&ctx.prog.proc, data, sample)
    };
    if !store.as_context().data().prog.proc.debug.wanted(hit) {
        return Ok(());
    }
    let table = match main {
        Some(main) => {
            let (data, ctx) = main.data_and_store_mut(&mut store);
            trace::stop_point(&ctx.prog.proc, hit, data)?
        }
        None => {
            let proc = store.as_context().data().prog.proc.clone();
            trace::stop_point(&proc, hit, &mut [])?
        }
    };
    if let Some(table) = table {
        trace::patch(stack.data_mut(&mut store), &table);
    }
    Ok(())
}

/// [`debug_point`] for a host call's caller. Only the prog's command is
/// traced: a call from a reactor leaves the stack as its `call` saw it.
fn debug_caller(caller: &mut Caller<'_, Ctx>) -> Result<(), wasmi::Error> {
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
    debug_point(caller, stack, main, false).map_err(|s| wasmi::Error::host(HostStop(s)))
}

/// [`debug_point`] for a prog's command instance.
fn debug_instance(
    mut store: impl AsContextMut<Data = Ctx>,
    instance: &Instance,
    sample: bool,
) -> Result<(), Stop> {
    let Some(stack) = instance.get_memory(&store, trace::STACK) else {
        return Ok(());
    };
    let main = instance.get_memory(&store, purg_abi::MEMORY);
    debug_point(&mut store, stack, main, sample)
}

/// Records the stack of a prog's command instance, and its counts.
fn record_instance(mut store: impl AsContextMut<Data = Ctx>, instance: &Instance) {
    if let Some(stack) = instance.get_memory(&store, trace::STACK) {
        let (data, ctx) = stack.data_and_store_mut(&mut store);
        trace::record(&ctx.prog.proc, data, false);
        trace::counts(&ctx.prog.proc, data);
    }
}

fn link(linker: &mut Linker<Ctx>) -> Result<(), wasmi::Error> {
    macro_rules! register {
        ($module:expr, $name:literal, $f:path, ($($a:ident : $t:ty),*) -> $r:ty) => {
            linker.func_wrap(
                $module,
                $name,
                |mut caller: Caller<'_, Ctx>, $($a: $t),*| -> Result<$r, wasmi::Error> {
                    debug_caller(&mut caller)?;
                    let (data, ctx) = memory(&mut caller)?;
                    purg_vm::host_call(ctx, |ctx| $f(ctx, &mut Mem(data), $($a),*))
                        .map_err(|s| wasmi::Error::host(HostStop(s)))
                },
            )?;
        };
    }
    purg_vm::for_each_import!(register);
    Ok(())
}

fn register_modules(
    linker: &mut Linker<Ctx>,
    cache: Option<Arc<Cache>>,
) -> Result<Modules, wasmi::Error> {
    let modules: Modules = Arc::new(spin::Mutex::new(BTreeMap::new()));
    let loading = modules.clone();
    let calling = modules.clone();
    linker.func_wrap(
        purg_abi::MODULE,
        purg_abi::sys::LOAD,
        move |mut caller: Caller<'_, Ctx>, path: i32, plen: i32| -> Result<i32, wasmi::Error> {
            killed(&caller)?;
            let r = match load_reactor(&mut caller, &loading, cache.as_ref(), path, plen) {
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
              -> Result<i32, wasmi::Error> {
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
    Ok(modules)
}

/// Ends a killed prog at `load` or `call`, as [`purg_vm::host_call`] does
/// at the other host calls.
fn killed(caller: &Caller<'_, Ctx>) -> Result<(), wasmi::Error> {
    if caller.data().prog.proc.note.killed() {
        return Err(wasmi::Error::host(HostStop(Stop::killed())));
    }
    Ok(())
}

const PAGE: usize = 65536;

fn guest_vec(caller: &mut Caller<'_, Ctx>, ptr: i32, len: i32) -> Result<Vec<u8>, Error> {
    let (data, _) = memory(caller).map_err(|_| Error::new(EBADADDR))?;
    Ok(Mem(data).slice(ptr, len)?.to_vec())
}

fn guest_str(caller: &mut Caller<'_, Ctx>, ptr: i32, len: i32) -> Result<String, Error> {
    Ok(String::from(
        core::str::from_utf8(&guest_vec(caller, ptr, len)?)
            .map_err(|_| Error::new(purg_abi::error::EBADARG))?,
    ))
}

fn load_reactor(
    caller: &mut Caller<'_, Ctx>,
    modules: &Modules,
    cache: Option<&Arc<Cache>>,
    path: i32,
    plen: i32,
) -> Result<i32, Error> {
    let path = guest_str(caller, path, plen)?;
    let wasm = {
        let ctx = caller.data_mut();
        purg_vm::load_module(&mut ctx.prog, &path)?
    };
    let engine = caller.engine().clone();
    let module = match (cache, last_digest(caller.data())) {
        (Some(cache), Some(digest)) => cache.module(caller.data(), &engine, &digest, &wasm)?,
        _ => Module::new(&engine, &wasm).map_err(bad_exec)?,
    };
    drop(wasm);
    let (linker, _) = linker(&engine, cache.cloned())?;
    // A start function runs unsliced: it cannot be resumed.
    let fuel = caller.get_fuel().unwrap_or(SLICE);
    let _ = caller.set_fuel(u64::MAX);
    let instance = linker.instantiate_and_start(&mut *caller, &module);
    let _ = caller.set_fuel(fuel);
    let instance = instance.map_err(bad_exec)?;
    let pid = caller.data().prog.pid;
    let mut table = modules.lock();
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

fn call_reactor(caller: &mut Caller<'_, Ctx>, modules: &Modules, call: Call) -> Result<i32, Error> {
    let export = guest_str(caller, call.name, call.nlen)?;
    let arguments = guest_vec(caller, call.args, call.alen)?;
    if arguments.len() > PAGE {
        return Err(Error::from("argument too big"));
    }
    let instance = {
        let pid = caller.data().prog.pid;
        let table = modules.lock();
        let list = table.get(&pid).ok_or(Error::from("bad module handle"))?;
        let index = usize::try_from(call.handle)
            .ok()
            .and_then(|h| h.checked_sub(1))
            .ok_or(Error::from("bad module handle"))?;
        *list.get(index).ok_or(Error::from("bad module handle"))?
    };
    let callee = instance
        .get_memory(&*caller, purg_abi::MEMORY)
        .ok_or(Error::from("module exports no memory"))?;
    let old = callee.size(&*caller);
    callee
        .grow(&mut *caller, 1)
        .map_err(|_| Error::from("out of memory"))?;
    let base = usize::try_from(old).map_err(|_| Error::from("out of memory"))? * PAGE;
    callee.data_mut(&mut *caller)[base..base + arguments.len()].copy_from_slice(&arguments);
    let func = instance
        .get_typed_func::<(i32, i32), i32>(&*caller, &export)
        .map_err(|_| Error::from("export not found"))?;
    let n = call_sliced(
        &mut *caller,
        &func,
        (base as i32, arguments.len() as i32),
        None,
    )
    .map_err(|e| Error::from(format!("trap: {e}")))?;
    if n < 0 || call.olen < 0 || n as usize > PAGE || n as usize > call.olen as usize {
        return Err(Error::from("bad result length"));
    }
    let mut result = vec![0u8; n as usize];
    result.copy_from_slice(&callee.data(&*caller)[base..base + n as usize]);
    let (data, _) = memory(caller).map_err(|_| Error::new(EBADADDR))?;
    Mem(data).write(call.out, &result)?;
    Ok(n)
}

fn bad_exec(e: impl core::fmt::Display) -> Error {
    Error::from(format!("{EBADEXEC}: {e}"))
}

/// Calls `func` a [`SLICE`] of fuel at a time, checking the prog's note
/// between slices (see the crate docs). `traced` is the prog's command
/// instance when it keeps a call stack: each slice then ends by recording
/// the stack, and a sample if the prog is profiled.
fn call_sliced<P: WasmParams, R: WasmResults>(
    mut ctx: impl AsContextMut<Data = Ctx>,
    func: &TypedFunc<P, R>,
    params: P,
    traced: Option<&Instance>,
) -> Result<R, wasmi::Error> {
    let mut call = func.call_resumable(&mut ctx, params)?;
    loop {
        match call {
            TypedResumableCall::Finished(r) => return Ok(r),
            TypedResumableCall::HostTrap(t) => {
                let e = t.host_error();
                return Err(match e.downcast_ref::<HostStop>() {
                    Some(stop) => wasmi::Error::host(HostStop(stop.0.clone())),
                    None => wasmi::Error::new(format!("{e}")),
                });
            }
            TypedResumableCall::OutOfFuel(t) => {
                if let Some(instance) = traced {
                    debug_instance(&mut ctx, instance, true)
                        .map_err(|s| wasmi::Error::host(HostStop(s)))?;
                }
                if ctx.as_context().data().prog.proc.note.killed() {
                    return Err(wasmi::Error::host(HostStop(Stop::killed())));
                }
                ctx.as_context_mut()
                    .set_fuel(SLICE.max(t.required_fuel()))?;
                call = t.resume(&mut ctx)?;
            }
        }
    }
}

impl Wasmi {
    fn call(store: &mut Store<Ctx>, module: &Module, linker: &Linker<Ctx>) -> Result<Stop, Error> {
        // A start function runs unsliced: it cannot be resumed.
        store.set_fuel(u64::MAX).map_err(bad_exec)?;
        let instance = linker
            .instantiate_and_start(&mut *store, module)
            .map_err(bad_exec)?;
        let entry = instance
            .get_typed_func::<(), ()>(&*store, purg_abi::ENTRY)
            .map_err(bad_exec)?;
        store.set_fuel(SLICE).map_err(bad_exec)?;
        let traced = store.data().prog.proc.debug.traced().then_some(&instance);
        let res = call_sliced(&mut *store, &entry, (), traced);
        // A traced prog's last stack, and its counts, stay readable.
        if let Some(instance) = traced {
            record_instance(&mut *store, instance);
        }
        Ok(match res {
            Ok(()) => Stop::Exit(String::new()),
            Err(e) => match e.downcast_ref::<HostStop>() {
                Some(stop) => stop.0.clone(),
                None => {
                    let status = format!("trap: {e}");
                    // A traced prog that traps is broken: its stack at
                    // the trap and the trap itself stay readable.
                    if traced.is_some() {
                        store.data().prog.proc.debug.set_exception(&status);
                    }
                    Stop::Exit(status)
                }
            },
        })
    }
}

/// The prog's memory limits as wasmi's resource limiter: the store's data,
/// seen as a limiter.
#[repr(transparent)]
struct Limiter(Ctx);

impl Limiter {
    fn of(ctx: &mut Ctx) -> &mut dyn wasmi::ResourceLimiter {
        // SAFETY: `Limiter` is `repr(transparent)` over `Ctx`, so the two
        // have the same layout, and the borrow is the caller's.
        unsafe { &mut *(ctx as *mut Ctx).cast::<Limiter>() }
    }
}

/// wasmi's own defaults for the counts.
const LIMITER_COUNT: usize = 10_000;

impl wasmi::ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmi_core::LimiterError> {
        Ok(purg_vm::memory_growing(&self.0, current, desired))
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmi_core::LimiterError> {
        Ok(purg_vm::table_growing(&self.0, current, desired))
    }

    fn instances(&self) -> usize {
        LIMITER_COUNT
    }

    fn tables(&self) -> usize {
        LIMITER_COUNT
    }

    fn memories(&self) -> usize {
        LIMITER_COUNT
    }
}

impl purg_vm::Engine for Wasmi {
    fn name(&self) -> &'static str {
        "wasmi"
    }

    fn run(&self, wasm: Vec<u8>, ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        let home = self.home(&ctx, &wasm);
        // The engine has its own copy of what it still needs.
        drop(wasm);
        let (home, module) = match home {
            Ok(h) => h,
            Err(e) => return (ctx, Err(e)),
        };
        let pid = ctx.prog.pid;
        let mut store = Store::new(home.engine(), ctx);
        store.limiter(Limiter::of);
        let res = Wasmi::call(&mut store, &module, home.linker());
        if let Home::Shared(g) = &home {
            g.reactors.lock().remove(&pid);
        }
        (store.into_data(), res)
    }
}
