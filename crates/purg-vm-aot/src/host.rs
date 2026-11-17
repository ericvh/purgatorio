//! The host functions as wasmtime sees them, for both wasmtime backends:
//! `purg-vm-wasmtime` (the emu's, with Cranelift and `std`) and this
//! crate's AOT engine (the kernel's, `no_std`). The two link the same
//! `wasmtime` crate, so they share this code and cannot drift: the
//! imports of `purg_vm::for_each_import!`, `load` and `call` for
//! reactors, and the prog's memory limits.
//!
//! What differs is behind [`Backend`]: how a reactor's module is made
//! (compiled, or found as a verified image), and what happens before each
//! host call (the emu's debugger).

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;

use purg_abi::error::{EBADADDR, EBADEXEC};
use purg_core::Error;
use purg_platform::sync::Mutex;
use purg_vm::{Ctx, Mem, Ptr, Stop};
use wasmtime_runtime::{
    AsContextMut, Caller, Engine, Extern, Instance, Linker, Module, ResourceLimiter, Store,
};

/// Something that must outlive a reactor's instance: the memory its code
/// is in, for an image loaded at run time.
pub type Keep = Option<Arc<dyn Any + Send + Sync>>;

/// What a backend decides.
pub trait Backend: Send + Sync + 'static {
    /// The module for a reactor that `caller`'s prog loads from `path`:
    /// `wasm` is its bytes, already checked against the prog's module
    /// policy and recorded in the prog (`purg_vm::load_module`).
    fn reactor(
        &self,
        caller: &mut Caller<'_, Ctx>,
        path: &str,
        wasm: Vec<u8>,
    ) -> Result<(Module, Keep), Error>;

    /// Runs before every host call; an error unwinds the guest.
    fn before_call(_caller: &mut Caller<'_, Ctx>) -> wasmtime_runtime::Result<()> {
        Ok(())
    }
}

/// The reactors each running prog has loaded, by pid, with what each
/// needs kept alive. The engine calls [`Reactors::exited`] once a prog's
/// store is gone.
#[derive(Default)]
pub struct Reactors(Mutex<BTreeMap<u32, Vec<(Instance, Keep)>>>);

impl Reactors {
    pub fn new() -> Arc<Reactors> {
        Arc::new(Reactors::default())
    }

    /// Forgets prog `pid`'s reactors. Call it after the prog's store has
    /// been dropped (`Store::into_data`), so that what they kept goes
    /// after their instances.
    pub fn exited(&self, pid: u32) {
        let gone = self.0.lock().remove(&pid);
        drop(gone);
    }

    /// Progs with reactors now, for tests.
    pub fn progs(&self) -> usize {
        self.0.lock().len()
    }
}

/// A linker for `engine` with every host function, and `load` and `call`
/// recording reactors in `reactors`.
pub fn linker<B: Backend>(
    engine: &Engine,
    backend: &Arc<B>,
    reactors: &Arc<Reactors>,
) -> Result<Linker<Ctx>, Error> {
    let mut linker = Linker::new(engine);
    link::<B>(&mut linker).map_err(|e| Error::from(e.to_string()))?;
    register_reactors(&mut linker, backend, reactors).map_err(|e| Error::from(e.to_string()))?;
    Ok(linker)
}

/// Installs the prog's memory limits as the store's resource limiter.
pub fn limit(store: &mut Store<Ctx>) {
    store.limiter(Limiter::of);
}

/// How a call into the guest ended, as a prog's stop: a normal return is
/// `exit("")`, a host call's [`Stop`] stays one, and a trap is
/// `exit("trap: ...")`.
pub fn stop(res: wasmtime_runtime::Result<()>) -> Stop {
    match res {
        Ok(()) => Stop::Exit(String::new()),
        Err(e) => match e.downcast_ref::<Stop>() {
            Some(stop) => stop.clone(),
            None => Stop::Exit(format!("trap: {}", e.root_cause())),
        },
    }
}

/// The calling instance's linear memory and the prog.
pub fn memory<'c>(
    caller: &'c mut Caller<'_, Ctx>,
) -> wasmtime_runtime::Result<(&'c mut [u8], &'c mut Ctx)> {
    match caller.get_export(purg_abi::MEMORY) {
        Some(Extern::Memory(mem)) => Ok(mem.data_and_store_mut(caller.as_context_mut())),
        // A threaded module's memory (`docs/threads.md`), which only an
        // engine with threads makes.
        Some(Extern::SharedMemory(mem)) => {
            let cells = mem.data();
            let (at, len) = (cells.as_ptr() as *mut u8, cells.len());
            // SAFETY: a shared memory is never moved or shrunk, and the
            // caller's instance keeps it alive for the borrow, so the
            // slice stays in bounds. It is not exclusive: other threads
            // of the guest can write these bytes during a host call. A
            // program that does so has a race of its own making, but the
            // host then holds a `&mut [u8]` (and `&str`s checked from it)
            // that Rust assumes nobody else writes. `docs/threads.md`
            // lists this with the feature's other limits.
            let data = unsafe { core::slice::from_raw_parts_mut(at, len) };
            Ok((data, caller.data_mut()))
        }
        _ => Err(wasmtime_runtime::Error::msg("guest exports no memory")),
    }
}

/// Errors that end the module's start, as `bad executable: ...`.
pub fn bad_exec(e: impl core::fmt::Display) -> Error {
    Error::from(format!("{EBADEXEC}: {e}"))
}

fn link<B: Backend>(linker: &mut Linker<Ctx>) -> wasmtime_runtime::Result<()> {
    macro_rules! register {
        ($module:expr, $name:literal, $f:path, ($($a:ident : $t:ty),*) -> $r:ty) => {
            linker.func_wrap(
                $module,
                $name,
                |mut caller: Caller<'_, Ctx>, $($a: $t),*| -> wasmtime_runtime::Result<$r> {
                    B::before_call(&mut caller)?;
                    let (data, ctx) = memory(&mut caller)?;
                    purg_vm::host_call(ctx, |ctx| $f(ctx, &mut Mem(data), $($a),*))
                        .map_err(wasmtime_runtime::Error::new)
                },
            )?;
        };
    }
    purg_vm::for_each_import!(register);
    purg_vm::for_each_import64!(register);
    Ok(())
}

fn register_reactors<B: Backend>(
    linker: &mut Linker<Ctx>,
    backend: &Arc<B>,
    reactors: &Arc<Reactors>,
) -> wasmtime_runtime::Result<()> {
    // Once for each width of caller (`docs/wasm64.md`).
    macro_rules! register {
        ($module:expr, $p:ty) => {
            let (b, loading, calling) = (backend.clone(), reactors.clone(), reactors.clone());
            linker.func_wrap(
                $module,
                purg_abi::sys::LOAD,
                move |mut caller: Caller<'_, Ctx>,
                      path: $p,
                      plen: $p|
                      -> wasmtime_runtime::Result<i32> {
                    killed(&caller)?;
                    let r = match load_reactor(&mut caller, &b, &loading, path, plen) {
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
                $module,
                purg_abi::sys::CALL,
                move |mut caller: Caller<'_, Ctx>,
                      handle: i32,
                      name: $p,
                      nlen: $p,
                      args: $p,
                      alen: $p,
                      out: $p,
                      olen: $p|
                      -> wasmtime_runtime::Result<i32> {
                    killed(&caller)?;
                    let call = Call {
                        handle,
                        name,
                        nlen,
                        args,
                        alen,
                        out,
                        olen,
                    };
                    let r = match call_reactor(&mut caller, &calling, call) {
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
        };
    }
    register!(purg_abi::MODULE, i32);
    register!(purg_abi::MODULE64, i64);
    Ok(())
}

/// Ends a killed prog at `load` or `call`, as [`purg_vm::host_call`] does
/// at the other host calls.
fn killed(caller: &Caller<'_, Ctx>) -> wasmtime_runtime::Result<()> {
    if caller.data().prog.proc.note.killed() {
        return Err(wasmtime_runtime::Error::new(Stop::killed()));
    }
    Ok(())
}

const PAGE: usize = 65536;

fn guest_vec<P: Ptr>(caller: &mut Caller<'_, Ctx>, ptr: P, len: P) -> Result<Vec<u8>, Error> {
    let (data, _) = memory(caller).map_err(|_| Error::new(EBADADDR))?;
    Ok(Mem(data).slice(ptr, len)?.to_vec())
}

fn guest_str<P: Ptr>(caller: &mut Caller<'_, Ctx>, ptr: P, len: P) -> Result<String, Error> {
    Ok(core::str::from_utf8(&guest_vec(caller, ptr, len)?)
        .map_err(|_| Error::new(purg_abi::error::EBADARG))?
        .to_string())
}

fn load_reactor<B: Backend, P: Ptr>(
    caller: &mut Caller<'_, Ctx>,
    backend: &Arc<B>,
    reactors: &Arc<Reactors>,
    path: P,
    plen: P,
) -> Result<i32, Error> {
    let path = guest_str(caller, path, plen)?;
    let wasm = purg_vm::load_module(&mut caller.data_mut().prog, &path)?;
    let (module, keep) = backend.reactor(caller, &path, wasm)?;
    let engine = caller.engine().clone();
    let linker = linker(&engine, backend, reactors)?;
    let instance = linker
        .instantiate(&mut *caller, &module)
        .map_err(bad_exec)?;
    let pid = caller.data().prog.pid;
    let mut table = reactors.0.lock();
    let list = table.entry(pid).or_default();
    list.push((instance, keep));
    Ok(list.len() as i32)
}

/// The arguments of `call`; `P` is the caller's pointer width. The
/// module called is always a wasm32 reactor, with a memory of its own.
struct Call<P> {
    handle: i32,
    name: P,
    nlen: P,
    args: P,
    alen: P,
    out: P,
    olen: P,
}

impl<P: Ptr> Call<P> {
    /// The room for the result, or `None` for a negative wasm32 length
    /// (or a wasm64 one as large).
    fn room(&self) -> Option<usize> {
        usize::try_from(self.olen.addr())
            .ok()
            .filter(|n| *n <= i32::MAX as usize)
    }
}

fn call_reactor<P: Ptr>(
    caller: &mut Caller<'_, Ctx>,
    reactors: &Reactors,
    call: Call<P>,
) -> Result<i32, Error> {
    let export = guest_str(caller, call.name, call.nlen)?;
    let arguments = guest_vec(caller, call.args, call.alen)?;
    let Some(room) = call.room().filter(|_| arguments.len() <= PAGE) else {
        return Err(Error::from("bad argument"));
    };
    let instance = {
        let pid = caller.data().prog.pid;
        let table = reactors.0.lock();
        let list = table.get(&pid).ok_or(Error::from("bad module handle"))?;
        let index = usize::try_from(call.handle)
            .ok()
            .and_then(|h| h.checked_sub(1))
            .ok_or(Error::from("bad module handle"))?;
        list.get(index).ok_or(Error::from("bad module handle"))?.0
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
    if n < 0 || n as usize > PAGE || n as usize > room {
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

/// The prog's memory limits as wasmtime's resource limiter: the store's
/// data, seen as a limiter.
#[repr(transparent)]
struct Limiter(Ctx);

impl Limiter {
    fn of(ctx: &mut Ctx) -> &mut dyn ResourceLimiter {
        // SAFETY: `Limiter` is `repr(transparent)` over `Ctx`, so the two
        // have the same layout, and the borrow is the caller's.
        unsafe { &mut *(ctx as *mut Ctx).cast::<Limiter>() }
    }
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime_runtime::Result<bool> {
        Ok(purg_vm::memory_growing(&self.0, current, desired))
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime_runtime::Result<bool> {
        Ok(purg_vm::table_growing(&self.0, current, desired))
    }
}

/// Keeps `v` alive beside a reactor.
pub fn keep<T: Any + Send + Sync>(v: T) -> Keep {
    Some(Arc::new(v) as Arc<dyn Any + Send + Sync>)
}
