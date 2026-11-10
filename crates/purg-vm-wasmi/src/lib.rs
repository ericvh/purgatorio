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
//! sample, and after a trap.
#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

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

/// The status of a killed prog.
pub use purg_vm::KILLED;

/// The backend. It holds only the configuration: each prog runs in a
/// wasmi engine of its own.
///
/// A wasmi engine keeps every function it has translated, and the module
/// bytes behind those not yet translated, until the engine is dropped;
/// dropping the `Module` frees neither. One engine for the whole system
/// therefore grew with every command run, until the kernel ran out of
/// memory. With an engine per prog, all of a prog's code, its reactors'
/// code and its value stack are freed when it exits.
pub struct Wasmi {
    config: Config,
    /// [`Wasmi::config`] with room for [`TRACED_DEPTH`] frames.
    traced: Config,
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
        // Translating a function on its first call is host work and costs
        // no fuel: wasmi 2 reports running out of fuel there as an error,
        // not as a call that can be resumed.
        config.fuel_cost(CustomFuelCosts {
            bytes_copied_per_fuel: 64,
            fuel_per_bytes_translated: 0,
            fuel_per_bytes_validated: 0,
        });
        config.set_max_recursion_depth(DEPTH);
        let mut traced = config.clone();
        traced.set_max_recursion_depth(TRACED_DEPTH);
        let wasmi = Wasmi { config, traced };
        // Linking fails the same way every time, so report it here.
        linker(&Engine::new(&wasmi.config))?;
        Ok(wasmi)
    }
}

/// A linker for `engine` with the host calls and `load`/`call`.
fn linker(engine: &Engine) -> Result<Linker<Ctx>, Error> {
    let mut linker = Linker::new(engine);
    link(&mut linker).map_err(|e| Error::from(format!("{e}")))?;
    register_modules(&mut linker).map_err(|e| Error::from(format!("{e}")))?;
    Ok(linker)
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

/// Records the call stack of the traced instance that exports `stack`
/// (as `trace::STACK`) in its prog.
fn record(mut store: impl AsContextMut<Data = Ctx>, stack: Option<Memory>, sample: bool) {
    if let Some(stack) = stack {
        let (data, ctx) = stack.data_and_store_mut(&mut store);
        trace::record(&ctx.prog.proc, data, sample);
    }
}

/// [`record`] for a host call's caller. Only the prog's command is
/// traced: a call from a reactor leaves the stack as its `call` saw it.
fn record_caller(caller: &mut Caller<'_, Ctx>) {
    if !caller.data().prog.proc.debug.traced() {
        return;
    }
    let stack = caller
        .get_export(trace::STACK)
        .and_then(Extern::into_memory);
    record(caller, stack, false);
}

/// [`record`] for a prog's command instance.
fn record_instance(mut store: impl AsContextMut<Data = Ctx>, instance: &Instance, sample: bool) {
    let stack = instance.get_memory(&store, trace::STACK);
    record(&mut store, stack, sample);
}

fn link(linker: &mut Linker<Ctx>) -> Result<(), wasmi::Error> {
    macro_rules! register {
        ($module:expr, $name:literal, $f:path, ($($a:ident : $t:ty),*) -> $r:ty) => {
            linker.func_wrap(
                $module,
                $name,
                |mut caller: Caller<'_, Ctx>, $($a: $t),*| -> Result<$r, wasmi::Error> {
                    record_caller(&mut caller);
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

fn register_modules(linker: &mut Linker<Ctx>) -> Result<(), wasmi::Error> {
    let modules: Modules = Arc::new(spin::Mutex::new(BTreeMap::new()));
    let loading = modules.clone();
    let calling = modules;
    linker.func_wrap(
        purg_abi::MODULE,
        purg_abi::sys::LOAD,
        move |mut caller: Caller<'_, Ctx>, path: i32, plen: i32| -> Result<i32, wasmi::Error> {
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
    Ok(())
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
    let linker = linker(&engine)?;
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
                    record_instance(&mut ctx, instance, true);
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
    fn call(engine: &Engine, store: &mut Store<Ctx>, wasm: Vec<u8>) -> Result<Stop, Error> {
        let module = Module::new(engine, &wasm).map_err(bad_exec)?;
        // The engine has its own copy of what it still needs.
        drop(wasm);
        // A start function runs unsliced: it cannot be resumed.
        store.set_fuel(u64::MAX).map_err(bad_exec)?;
        let instance = linker(engine)?
            .instantiate_and_start(&mut *store, &module)
            .map_err(bad_exec)?;
        let entry = instance
            .get_typed_func::<(), ()>(&*store, purg_abi::ENTRY)
            .map_err(bad_exec)?;
        store.set_fuel(SLICE).map_err(bad_exec)?;
        let traced = store.data().prog.proc.debug.traced().then_some(&instance);
        Ok(match call_sliced(&mut *store, &entry, (), traced) {
            Ok(()) => Stop::Exit(String::new()),
            Err(e) => match e.downcast_ref::<HostStop>() {
                Some(stop) => stop.0.clone(),
                None => {
                    let status = format!("trap: {e}");
                    // A traced prog that traps is broken: its stack at
                    // the trap and the trap itself stay readable.
                    if let Some(instance) = traced {
                        record_instance(&mut *store, instance, false);
                        store.data().prog.proc.debug.set_exception(&status);
                    }
                    Stop::Exit(status)
                }
            },
        })
    }
}

impl purg_vm::Engine for Wasmi {
    fn name(&self) -> &'static str {
        "wasmi"
    }

    fn run(&self, wasm: Vec<u8>, ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        let config = if ctx.prog.proc.debug.traced() {
            &self.traced
        } else {
            &self.config
        };
        let engine = Engine::new(config);
        let mut store = Store::new(&engine, ctx);
        let res = Wasmi::call(&engine, &mut store, wasm);
        (store.into_data(), res)
    }
}
