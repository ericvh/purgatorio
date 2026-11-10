//! wasmtime backend for [`purg_vm::Engine`]: Cranelift-compiled, the hosted
//! default.
//!
//! Guest code is interrupted every [`TICK`] (epoch interruption) to check
//! the prog's note: a prog killed while computing, with no host call to
//! return `interrupted` from, is stopped with status `killed` at the tick
//! after the one that first saw it killed. That spare tick lets a guest
//! that did see `interrupted` exit with its own status. The wasmi backend
//! does the same with fuel.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use purg_abi::error::{EBADADDR, EBADEXEC};
use purg_core::Error;
use purg_vm::{Ctx, Mem, Stop};
use wasmtime::{
    AsContextMut, Caller, Config, Engine, Extern, Instance, Linker, Module, Store, UpdateDeadline,
};

/// Compiled modules kept for reuse; the cache is emptied when full.
const CACHED_MODULES: usize = 64;

/// How often running guests check their note.
pub const TICK: Duration = Duration::from_millis(10);

/// The status of a prog stopped by the [`TICK`] checks after a kill.
pub const KILLED: &str = "killed";

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

fn link(linker: &mut Linker<Ctx>) -> wasmtime::Result<()> {
    macro_rules! register {
        ($module:expr, $name:literal, $f:path, ($($a:ident : $t:ty),*) -> $r:ty) => {
            linker.func_wrap(
                $module,
                $name,
                |mut caller: Caller<'_, Ctx>, $($a: $t),*| -> wasmtime::Result<$r> {
                    let (data, ctx) = memory(&mut caller)?;
                    $f(ctx, &mut Mem(data), $($a),*).map_err(wasmtime::Error::new)
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
        move |mut caller: Caller<'_, Ctx>, path: i32, plen: i32| -> i32 {
            match load_reactor(&mut caller, &loading, path, plen) {
                Ok(handle) => handle,
                Err(e) => {
                    caller.data_mut().prog.set_error(&e);
                    -1
                }
            }
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
              -> i32 {
            match call_reactor(
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
            }
        },
    )?;
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
    fn call(&self, store: &mut Store<Ctx>, wasm: &[u8]) -> Result<Stop, Error> {
        let module = self.module(wasm)?;
        let instance = self
            .linker
            .instantiate(&mut *store, &module)
            .map_err(bad_exec)?;
        let entry = instance
            .get_typed_func::<(), ()>(&mut *store, purg_abi::ENTRY)
            .map_err(bad_exec)?;
        Ok(match entry.call(&mut *store, ()) {
            Ok(()) => Stop::Exit(String::new()),
            Err(e) => match e.downcast_ref::<Stop>() {
                Some(stop) => stop.clone(),
                None => Stop::Exit(format!("trap: {}", e.root_cause())),
            },
        })
    }
}

impl purg_vm::Engine for Wasmtime {
    fn name(&self) -> &'static str {
        "wasmtime"
    }

    fn run(&self, wasm: &[u8], ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        let mut store = Store::new(&self.engine, ctx);
        let mut seen_killed = false;
        store.epoch_deadline_callback(move |state| {
            if state.data().prog.proc.note.killed() {
                if seen_killed {
                    return Err(wasmtime::Error::new(Stop::Exit(String::from(KILLED))));
                }
                seen_killed = true;
            }
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);
        let res = self.call(&mut store, wasm);
        (store.into_data(), res)
    }
}
