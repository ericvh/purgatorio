//! An experimental [`purg_vm::Engine`] for the kernel: wasmtime's runtime
//! without `std`, running modules compiled ahead of time on the build
//! host (`.cwasm`), as native aarch64 code or as Pulley bytecode
//! (`docs/perf.md`, decision D6).
//!
//! There is no compiler on the device. The build host compiles chosen
//! modules with [`config`] (`purg-vm-wasmtime`'s `cwasm` tool) into an
//! image set, [`Images`], that the kernel links into its text, so native
//! code is executable without changing a page's permissions at run time.
//! An image is found by the SHA-256 digest of its module, the digest the
//! loader already takes for the module policy (`docs/signing.md`). A
//! module with no image, and any module of a traced prog, runs on the
//! fallback engine (wasmi).
//!
//! Traps are explicit checks, not signals: linear memory is a heap
//! allocation with bounds checks, no guard pages, and the stack limit is
//! checked in each function's prologue. A running guest is interrupted
//! by epochs, which [`Aot::tick`] advances, to check the prog's note.
//!
//! The host functions are those of `purg-vm-wasmtime`, ported to
//! `no_std`. The embedder supplies wasmtime's platform hooks:
//! `wasmtime_tls_get`/`wasmtime_tls_set` (one pointer per kernel thread)
//! and the `wasmtime_sync_*` locks (the `custom-sync-primitives`
//! feature).
#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr::NonNull;

use purg_abi::error::{EBADADDR, EBADEXEC};
use purg_core::Error;
use purg_platform::sync::Mutex;
use purg_vm::{Ctx, Mem, Stop};
use wasmtime_runtime::{
    AsContextMut, Caller, Config, CustomCodeMemory, Engine, Extern, Instance, Linker, Module,
    Store, UpdateDeadline,
};

/// What the images hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// aarch64 machine code.
    Native,
    /// Pulley bytecode for a 64-bit little-endian host.
    Pulley,
}

impl Kind {
    pub fn target(self) -> &'static str {
        match self {
            Kind::Native => "aarch64-unknown-none",
            Kind::Pulley => "pulley64",
        }
    }
}

/// The most of a kernel thread's stack wasm code may use. A prog's
/// thread has 256 KiB; the host calls under the guest need about 20 KiB.
pub const MAX_WASM_STACK: usize = 128 << 10;

/// The configuration images are compiled with on the host and run with
/// in the kernel. The two must agree: wasmtime refuses an image whose
/// settings differ from the engine's.
pub fn config(kind: Kind) -> Result<Config, String> {
    let mut c = Config::new();
    c.target(kind.target()).map_err(|e| e.to_string())?;
    // No signals and no virtual memory in the kernel: explicit bounds
    // checks, memory that is a plain allocation, no guard pages.
    c.signals_based_traps(false);
    c.memory_reservation(0);
    c.memory_guard_size(0);
    c.memory_reservation_for_growth(1 << 20);
    c.memory_init_cow(false);
    c.epoch_interruption(true);
    c.max_wasm_stack(MAX_WASM_STACK);
    c.wasm_backtrace_max_frames(None);
    c.native_unwind_info(false);
    // No wasm offsets in traps: a trap reports its cause only.
    c.generate_address_map(false);
    Ok(c)
}

/// Images are aligned to this within the set, and the set itself in
/// memory, which covers any page size the code was laid out for.
pub const ALIGN: usize = 64 << 10;

/// The image set's header: `PURGAOT1`, the kind (0 native, 1 Pulley), and
/// the number of entries, then per entry the module's digest and the
/// image's offset and length, all little-endian.
pub const MAGIC: &[u8; 8] = b"PURGAOT1";
const HEADER: usize = 16;
const ENTRY: usize = 48;

/// A set of precompiled images, by module digest.
pub struct Images {
    kind: Kind,
    shared: Shared,
    /// Where the set is: the only code wasmtime may run.
    range: core::ops::Range<usize>,
}

impl Images {
    /// Reads a set built by [`Images::build`]. `blob` must start at an
    /// [`ALIGN`] boundary and, for native images, be executable.
    pub fn parse(blob: &'static [u8]) -> Result<Images, String> {
        let bad = || String::from("bad AOT image set");
        if blob.get(..8) != Some(MAGIC.as_slice()) {
            return Err(bad());
        }
        if !(blob.as_ptr() as usize).is_multiple_of(ALIGN) {
            return Err(String::from("AOT image set is not aligned"));
        }
        let u32_at = |i: usize| -> Result<u32, String> {
            Ok(u32::from_le_bytes(
                blob.get(i..i + 4)
                    .ok_or_else(bad)?
                    .try_into()
                    .map_err(|_| bad())?,
            ))
        };
        let u64_at = |i: usize| -> Result<usize, String> {
            let v = u64::from_le_bytes(
                blob.get(i..i + 8)
                    .ok_or_else(bad)?
                    .try_into()
                    .map_err(|_| bad())?,
            );
            usize::try_from(v).map_err(|_| bad())
        };
        let kind = match u32_at(8)? {
            0 => Kind::Native,
            1 => Kind::Pulley,
            _ => return Err(bad()),
        };
        let n = u32_at(12)? as usize;
        let mut by_digest = BTreeMap::new();
        for i in 0..n {
            let e = HEADER + i * ENTRY;
            let digest: [u8; 32] = blob
                .get(e..e + 32)
                .ok_or_else(bad)?
                .try_into()
                .map_err(|_| bad())?;
            let (off, len) = (u64_at(e + 32)?, u64_at(e + 40)?);
            let image = blob.get(off..off.checked_add(len).ok_or_else(bad)?);
            by_digest.insert(digest, image.ok_or_else(bad)?);
        }
        let base = blob.as_ptr() as usize;
        Ok(Images {
            kind,
            shared: Arc::new(by_digest),
            range: base..base + blob.len(),
        })
    }

    /// Lays out a set: `images` are (digest, image) pairs.
    pub fn build(kind: Kind, images: &[([u8; 32], Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(kind as u32).to_le_bytes());
        out.extend_from_slice(&(images.len() as u32).to_le_bytes());
        let table = out.len();
        out.resize(table + images.len() * ENTRY, 0);
        for (i, (digest, image)) in images.iter().enumerate() {
            out.resize(out.len().next_multiple_of(ALIGN), 0);
            let off = out.len();
            out.extend_from_slice(image);
            let e = table + i * ENTRY;
            out[e..e + 32].copy_from_slice(digest);
            out[e + 32..e + 40].copy_from_slice(&(off as u64).to_le_bytes());
            out[e + 40..e + 48].copy_from_slice(&(image.len() as u64).to_le_bytes());
        }
        out
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn len(&self) -> usize {
        self.shared.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shared.is_empty()
    }
}

/// Native code runs where it is in the image set, which the embedder
/// has made executable (the kernel links it into its text). Publishing
/// anything else is refused: there is no other code.
struct InImages(core::ops::Range<usize>);

impl CustomCodeMemory for InImages {
    fn required_alignment(&self) -> usize {
        1
    }

    fn publish_executable(&self, ptr: *const u8, len: usize) -> wasmtime_runtime::Result<()> {
        let start = ptr as usize;
        match start.checked_add(len) {
            Some(end) if self.0.start <= start && end <= self.0.end => Ok(()),
            _ => Err(wasmtime_runtime::Error::msg("code outside the AOT images")),
        }
    }

    fn unpublish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime_runtime::Result<()> {
        Ok(())
    }
}

pub struct Aot {
    engine: Engine,
    linker: Linker<Ctx>,
    images: Images,
    /// Modules made from images so far, by digest. A module holds only
    /// its metadata: the code stays in the image.
    modules: Mutex<BTreeMap<[u8; 32], Module>>,
    /// The engine for modules with no image.
    fallback: Arc<dyn purg_vm::Engine>,
}

impl Aot {
    pub fn new(images: Images, fallback: Arc<dyn purg_vm::Engine>) -> Result<Aot, Error> {
        let mut config = config(images.kind)?;
        config.with_custom_code_memory(Some(Arc::new(InImages(images.range.clone()))));
        let engine = Engine::new(&config).map_err(|e| Error::from(e.to_string()))?;
        let mut linker = Linker::new(&engine);
        link(&mut linker, &images.shared).map_err(|e| Error::from(e.to_string()))?;
        Ok(Aot {
            engine,
            linker,
            images,
            modules: Mutex::new(BTreeMap::new()),
            fallback,
        })
    }

    /// Advances the epoch: every running guest checks its prog's note at
    /// its next loop head or function entry. Call it every 10 ms or so.
    pub fn tick(&self) {
        self.engine.increment_epoch();
    }

    pub fn images(&self) -> &Images {
        &self.images
    }

    fn module(&self, digest: &[u8; 32]) -> Result<Option<Module>, Error> {
        if let Some(m) = self.modules.lock().get(digest) {
            return Ok(Some(m.clone()));
        }
        let Some(image) = self.images.shared.get(digest) else {
            return Ok(None);
        };
        // SAFETY: the image set is built by the host's `cwasm` tool from
        // wasmtime's own output and linked into the kernel's text, which
        // is never written; it lives for as long as the kernel.
        let module = unsafe {
            Module::deserialize_raw(&self.engine, NonNull::from(*image)).map_err(bad_exec)?
        };
        self.modules.lock().insert(*digest, module.clone());
        Ok(Some(module))
    }

    fn call(&self, store: &mut Store<Ctx>, module: &Module) -> Result<Stop, Error> {
        let instance = self
            .linker
            .instantiate(&mut *store, module)
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

impl purg_vm::Engine for Aot {
    fn name(&self) -> &'static str {
        match self.images.kind {
            Kind::Native => "wasmtime-aot",
            Kind::Pulley => "wasmtime-pulley",
        }
    }

    fn run(&self, wasm: Vec<u8>, ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        // The loader recorded the module's digest just before this.
        let digest = ctx.prog.proc.modules().last().map(|m| m.digest);
        let module = match digest {
            Some(d) if !ctx.prog.proc.debug.traced() => self.module(&d),
            _ => Ok(None),
        };
        let module = match module {
            Ok(Some(m)) => m,
            Ok(None) => return self.fallback.run(wasm, ctx),
            Err(e) => return (ctx, Err(e)),
        };
        drop(wasm);
        let mut store = Store::new(&self.engine, ctx);
        store.epoch_deadline_callback(|state| {
            if state.data().prog.proc.note.killed() {
                return Err(wasmtime_runtime::Error::new(Stop::killed()));
            }
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);
        let res = self.call(&mut store, &module);
        (store.into_data(), res)
    }
}

fn memory<'c>(
    caller: &'c mut Caller<'_, Ctx>,
) -> wasmtime_runtime::Result<(&'c mut [u8], &'c mut Ctx)> {
    let Some(Extern::Memory(mem)) = caller.get_export(purg_abi::MEMORY) else {
        return Err(wasmtime_runtime::Error::msg("guest exports no memory"));
    };
    Ok(mem.data_and_store_mut(caller.as_context_mut()))
}

/// Images by digest, shared with the `load` host function.
type Shared = Arc<BTreeMap<[u8; 32], &'static [u8]>>;

fn link(linker: &mut Linker<Ctx>, images: &Shared) -> wasmtime_runtime::Result<()> {
    macro_rules! register {
        ($module:expr, $name:literal, $f:path, ($($a:ident : $t:ty),*) -> $r:ty) => {
            linker.func_wrap(
                $module,
                $name,
                |mut caller: Caller<'_, Ctx>, $($a: $t),*| -> wasmtime_runtime::Result<$r> {
                    let (data, ctx) = memory(&mut caller)?;
                    purg_vm::host_call(ctx, |ctx| $f(ctx, &mut Mem(data), $($a),*))
                        .map_err(wasmtime_runtime::Error::new)
                },
            )?;
        };
    }
    purg_vm::for_each_import!(register);
    register_reactors(linker, images)
}

/// Reactors a prog has loaded, by pid.
type Reactors = Arc<Mutex<BTreeMap<u32, Vec<Instance>>>>;

/// `load` and `call`. A reactor runs only if it has an image: a prog
/// running native code cannot call into an interpreter's instance.
fn register_reactors(linker: &mut Linker<Ctx>, images: &Shared) -> wasmtime_runtime::Result<()> {
    let images = images.clone();
    let modules: Reactors = Arc::new(Mutex::new(BTreeMap::new()));
    let loading = modules.clone();
    let calling = modules;
    linker.func_wrap(
        purg_abi::MODULE,
        purg_abi::sys::LOAD,
        move |mut caller: Caller<'_, Ctx>, path: i32, plen: i32| -> wasmtime_runtime::Result<i32> {
            killed(&caller)?;
            let r = match load_reactor(&mut caller, &images, &loading, path, plen) {
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
    Ok(())
}

fn killed(caller: &Caller<'_, Ctx>) -> wasmtime_runtime::Result<()> {
    if caller.data().prog.proc.note.killed() {
        return Err(wasmtime_runtime::Error::new(Stop::killed()));
    }
    Ok(())
}

const PAGE: usize = 65536;

fn guest_vec(caller: &mut Caller<'_, Ctx>, ptr: i32, len: i32) -> Result<Vec<u8>, Error> {
    let (data, _) = memory(caller).map_err(|_| Error::new(EBADADDR))?;
    Ok(Mem(data).slice(ptr, len)?.to_vec())
}

fn guest_str(caller: &mut Caller<'_, Ctx>, ptr: i32, len: i32) -> Result<String, Error> {
    Ok(core::str::from_utf8(&guest_vec(caller, ptr, len)?)
        .map_err(|_| Error::new(purg_abi::error::EBADARG))?
        .to_string())
}

fn load_reactor(
    caller: &mut Caller<'_, Ctx>,
    images: &Shared,
    modules: &Reactors,
    path: i32,
    plen: i32,
) -> Result<i32, Error> {
    let path = guest_str(caller, path, plen)?;
    let digest = {
        let ctx = caller.data_mut();
        drop(purg_vm::load_module(&mut ctx.prog, &path)?);
        ctx.prog.proc.modules().last().map(|m| m.digest)
    };
    let engine = caller.engine().clone();
    let image = digest
        .and_then(|d| images.get(&d).copied())
        .ok_or_else(|| Error::from(format!("{EBADEXEC}: {path}: no AOT image")))?;
    // SAFETY: as in `Aot::module`.
    let module =
        unsafe { Module::deserialize_raw(&engine, NonNull::from(image)).map_err(bad_exec)? };
    let mut linker = Linker::new(&engine);
    link(&mut linker, images).map_err(|e| Error::from(e.to_string()))?;
    let instance = linker
        .instantiate(&mut *caller, &module)
        .map_err(bad_exec)?;
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
        let table = modules.lock();
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

fn bad_exec(e: impl core::fmt::Display) -> Error {
    Error::from(format!("{EBADEXEC}: {e}"))
}
