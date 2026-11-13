//! A [`purg_vm::Engine`] for the kernel: wasmtime's runtime without
//! `std`, running modules compiled ahead of time on the build host, as
//! native aarch64 code or as Pulley bytecode (`docs/perf.md`, decision
//! D6).
//!
//! There is no compiler on the device. The build host compiles modules
//! with [`config`] (`purg-vm-wasmtime`'s `cwasm` tool) into images. An
//! image is found by the SHA-256 digest of its module, the digest the
//! loader already takes for the module policy (`docs/signing.md`), in
//! one of two places:
//!
//! - a set linked into the kernel's read-only text at build time
//!   ([`Images`]), trusted as the kernel is;
//! - a signed image file beside the module (`/bin/<cmd>.cwasm`, see
//!   [`image`]), read at run time, which runs only if a key in
//!   `/lib/keys/aot` in the boot namespace signed it for that module's
//!   digest. Its code is copied into memory the kernel maps executable
//!   only once it is written and no longer writable ([`Hooks::load_code`]).
//!
//! A module with no image, or whose image does not verify, runs on the
//! fallback engine (wasmi), and so does any module of a traced prog.
//! Under [`Mode::Only`] such a module is refused instead.
//!
//! Loaded images are shared in generations, as `purg-vm-wasmi` shares
//! its modules: a prog joins the current generation, which keeps each
//! image its progs load, and a generation and its images go when the last
//! prog that started in it exits.
//!
//! Traps are explicit checks, not signals: linear memory is a heap
//! allocation with bounds checks, no guard pages, and the stack limit is
//! checked in each function's prologue. Guest code runs on a stack of its
//! own of [`STACK`] bytes ([`Hooks::on_stack`]), so the kernel thread's
//! stack does not bound its recursion. A running guest is interrupted by
//! epochs, which [`Aot::tick`] advances, to check the prog's note.
//!
//! The host functions are shared with `purg-vm-wasmtime` ([`host`]). The
//! embedder supplies wasmtime's platform hooks: `wasmtime_tls_get` and
//! `wasmtime_tls_set` (one pointer per kernel thread) and the
//! `wasmtime_sync_*` locks (the `custom-sync-primitives` feature).
#![no_std]

extern crate alloc;

pub mod host;
pub mod image;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use purg_abi::error::EBADEXEC;
use purg_abi::mode;
use purg_core::{Error, Pgrp, Prog};
use purg_platform::sync::Mutex;
use purg_vm::{Ctx, Stop};
use wasmtime_runtime::{
    Caller, Config, CustomCodeMemory, Engine, Linker, Module, Store, UpdateDeadline,
};

pub use image::{ALIGN, Images, Kind, MAGIC, Signed};

use host::{Keep, Reactors};

/// The most stack wasm code may use: what wasmtime allows by default, and
/// so what the emu's guests have.
pub const MAX_WASM_STACK: usize = 512 << 10;

/// Room under the deepest wasm frame for the host calls the guest makes:
/// the deepest a prog's thread went in the kernel's test mode is 22 KiB.
pub const HOST_STACK: usize = 64 << 10;

/// The stack guest code runs on.
pub const STACK: usize = MAX_WASM_STACK + HOST_STACK;

/// The most bytes of images a generation takes before new progs start a
/// new one. Images are about five times their modules, so this is about
/// as many modules as a wasmi generation takes.
pub const GENERATION_BYTES: usize = 8 << 20;

/// The longest signed image file read.
pub const MAX_IMAGE: usize = 64 << 20;

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

/// Executable memory the embedder made from an image.
pub trait Code: Send + Sync {
    fn bytes(&self) -> &[u8];
}

/// What the engine needs from the kernel.
pub trait Hooks: Send + Sync + 'static {
    /// Runs `f` on a stack of `size` bytes.
    fn on_stack(&self, size: usize, f: &mut dyn FnMut());
    /// Copies a verified image into memory that is executable and no
    /// longer writable, for as long as the result lives.
    fn load_code(&self, image: &[u8]) -> Result<Box<dyn Code>, Error>;
    /// Whether `ptr..ptr + len` is inside memory [`Hooks::load_code`]
    /// made.
    fn is_code(&self, ptr: *const u8, len: usize) -> bool;
    /// Called before guest code runs on this CPU, for code another CPU
    /// loaded (an instruction barrier).
    fn sync_code(&self) {}
}

/// Which modules run on wasmtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Those with a verified image; the rest on the fallback engine.
    Auto,
    /// Only those with a verified image: any other is refused, except in
    /// a traced prog, which runs on the fallback engine.
    Only,
}

/// Where the engine finds images.
pub struct Sources {
    /// The kind of every image.
    pub kind: Kind,
    /// A set linked into the kernel.
    pub linked: Option<Images>,
    /// The namespace whose `/lib/keys/aot` and `/lib/keys/revoked` vouch
    /// for image files read at run time: the boot namespace, which no prog
    /// can change. `None` reads no image files.
    pub trust: Option<Arc<Pgrp>>,
}

/// An image made into a module, and the memory its code is in. The
/// module goes first.
struct Loaded {
    module: Module,
    _code: Option<Box<dyn Code>>,
    /// Bytes of image, for the generation's bound; 0 for a linked one.
    size: usize,
}

/// What a search for a module's image found.
enum Found<T> {
    Image(T),
    /// There is none.
    Missing,
    /// There is one, and it was refused.
    Refused(String),
}

/// What the engine and the reactors' `load` share.
struct Inner {
    engine: Engine,
    kind: Kind,
    linked: Option<Arc<image::ByDigest>>,
    trust: Option<Arc<Pgrp>>,
    hooks: Arc<dyn Hooks>,
}

/// Images loaded by the progs of a generation, by module digest.
#[derive(Default)]
struct Generation {
    modules: Mutex<BTreeMap<[u8; 32], Arc<Loaded>>>,
    /// Bytes of images loaded. Only a bound: accessed `Relaxed`.
    bytes: AtomicUsize,
}

impl Generation {
    fn full(&self) -> bool {
        self.bytes.load(Ordering::Relaxed) >= GENERATION_BYTES
    }
}

pub struct Aot {
    inner: Arc<Inner>,
    linker: Linker<Ctx>,
    reactors: Arc<Reactors>,
    mode: Mode,
    /// The generation new progs join, while a prog of it runs.
    current: Mutex<Weak<Generation>>,
    /// The engine for modules with no image.
    fallback: Arc<dyn purg_vm::Engine>,
    native: AtomicU64,
    interpreted: AtomicU64,
}

/// Code may run only where it was loaded: in the linked set, or in
/// memory the kernel made executable for a verified image. Publishing
/// anything else is refused.
struct Published {
    linked: Option<core::ops::Range<usize>>,
    hooks: Arc<dyn Hooks>,
}

impl CustomCodeMemory for Published {
    fn required_alignment(&self) -> usize {
        1
    }

    fn publish_executable(&self, ptr: *const u8, len: usize) -> wasmtime_runtime::Result<()> {
        let start = ptr as usize;
        let linked = self.linked.as_ref().is_some_and(|r| {
            start
                .checked_add(len)
                .is_some_and(|end| r.start <= start && end <= r.end)
        });
        if linked || self.hooks.is_code(ptr, len) {
            Ok(())
        } else {
            Err(wasmtime_runtime::Error::msg("code outside the AOT images"))
        }
    }

    fn unpublish_executable(&self, _ptr: *const u8, _len: usize) -> wasmtime_runtime::Result<()> {
        Ok(())
    }
}

impl Aot {
    pub fn new(
        sources: Sources,
        mode: Mode,
        hooks: Arc<dyn Hooks>,
        fallback: Arc<dyn purg_vm::Engine>,
    ) -> Result<Aot, Error> {
        if sources
            .linked
            .as_ref()
            .is_some_and(|l| l.kind != sources.kind)
        {
            return Err(Error::from("AOT images of two kinds"));
        }
        let mut config = config(sources.kind)?;
        config.with_custom_code_memory(Some(Arc::new(Published {
            linked: sources.linked.as_ref().map(|l| l.range.clone()),
            hooks: hooks.clone(),
        })));
        let engine = Engine::new(&config).map_err(|e| Error::from(e.to_string()))?;
        let inner = Arc::new(Inner {
            engine,
            kind: sources.kind,
            linked: sources.linked.map(|l| l.by_digest),
            trust: sources.trust,
            hooks,
        });
        let reactors = Reactors::new();
        let linker = host::linker(&inner.engine, &inner, &reactors)?;
        Ok(Aot {
            inner,
            linker,
            reactors,
            mode,
            current: Mutex::new(Weak::new()),
            fallback,
            native: AtomicU64::new(0),
            interpreted: AtomicU64::new(0),
        })
    }

    /// Advances the epoch: every running guest checks its prog's note at
    /// its next loop head or function entry. Call it every 10 ms or so.
    pub fn tick(&self) {
        self.inner.engine.increment_epoch();
    }

    /// Progs run so far on wasmtime, and on the fallback engine.
    pub fn counts(&self) -> (u64, u64) {
        (
            self.native.load(Ordering::Relaxed),
            self.interpreted.load(Ordering::Relaxed),
        )
    }

    /// Images linked into the kernel.
    pub fn linked(&self) -> usize {
        self.inner.linked.as_ref().map_or(0, |l| l.len())
    }

    /// The generation a new prog joins: the current one, or a new one if
    /// there is none or it is full.
    fn generation(&self) -> Arc<Generation> {
        let mut current = self.current.lock();
        if let Some(g) = current.upgrade()
            && !g.full()
        {
            return g;
        }
        let g = Arc::new(Generation::default());
        *current = Arc::downgrade(&g);
        g
    }

    /// The image for `ctx`'s module, loaded from `path` with `digest`,
    /// from the prog's generation or found and kept there.
    fn module(
        &self,
        ctx: &mut Ctx,
        path: &str,
        digest: &[u8; 32],
    ) -> (Option<Arc<Generation>>, Found<Arc<Loaded>>) {
        let g = self.generation();
        if let Some(l) = g.modules.lock().get(digest) {
            return (Some(g.clone()), Found::Image(l.clone()));
        }
        match self.inner.find(&mut ctx.prog, path, digest) {
            Found::Image(l) => {
                let l = Arc::new(l);
                g.bytes.fetch_add(l.size, Ordering::Relaxed);
                let l = if g.full() {
                    l
                } else {
                    g.modules.lock().entry(*digest).or_insert(l).clone()
                };
                (Some(g), Found::Image(l))
            }
            Found::Missing => (None, Found::Missing),
            Found::Refused(e) => (None, Found::Refused(e)),
        }
    }

    fn call(&self, store: &mut Store<Ctx>, module: &Module) -> Result<Stop, Error> {
        let instance = self
            .linker
            .instantiate(&mut *store, module)
            .map_err(host::bad_exec)?;
        let entry = instance
            .get_typed_func::<(), ()>(&mut *store, purg_abi::ENTRY)
            .map_err(host::bad_exec)?;
        Ok(host::stop(entry.call(&mut *store, ())))
    }
}

impl Inner {
    /// The image for the module `prog` loaded from `path`, whose digest is
    /// `digest`: in the linked set, or in a signed file beside the module.
    fn find(&self, prog: &mut Prog, path: &str, digest: &[u8; 32]) -> Found<Loaded> {
        if let Some(image) = self.linked.as_ref().and_then(|l| l.get(digest)) {
            // SAFETY: the linked set is part of the kernel's text, which
            // is never written and lives as long as the kernel; it holds
            // the `cwasm` tool's output of wasmtime's own compiler.
            return match unsafe { Module::deserialize_raw(&self.engine, NonNull::from(*image)) } {
                Ok(module) => Found::Image(Loaded {
                    module,
                    _code: None,
                    size: 0,
                }),
                Err(e) => Found::Refused(format!("{e}")),
            };
        }
        let Some(trust) = &self.trust else {
            return Found::Missing;
        };
        let Some(file) = path
            .strip_suffix(".wasm")
            .and_then(|p| read(prog, &format!("{p}.cwasm")))
        else {
            return Found::Missing;
        };
        let (anchors, revoked) = purg_core::modsig::trust_lists(trust, purg_core::modsig::AOT_KEYS);
        let now = prog.kernel.platform.now_ns() / 1_000_000_000;
        let verified = Signed::parse(&file).and_then(|s| {
            s.verify(
                self.kind,
                digest,
                &purg_auth::modsig::Trust {
                    anchors: &anchors,
                    revoked: revoked.as_deref(),
                    now,
                },
            )
        });
        let image = match verified {
            Ok(i) => i,
            Err(e) => return Found::Refused(String::from(e)),
        };
        let code = match self.hooks.load_code(image) {
            Ok(c) => c,
            Err(e) => return Found::Refused(format!("{e}")),
        };
        drop(file);
        // SAFETY: the code was verified (its digest is signed by a trusted
        // key for this module) and copied into memory that stays
        // executable and unwritten for as long as `code`, which the
        // module is kept beside and dropped after.
        match unsafe { Module::deserialize_raw(&self.engine, NonNull::from(code.bytes())) } {
            Ok(module) => Found::Image(Loaded {
                module,
                size: code.bytes().len(),
                _code: Some(code),
            }),
            Err(e) => Found::Refused(format!("{e}")),
        }
    }
}

/// Reads an image file through the prog's namespace, or `None`.
fn read(prog: &mut Prog, path: &str) -> Option<Vec<u8>> {
    let fd = prog.open(path, mode::OREAD).ok()?;
    let mut out = Vec::new();
    let mut ok = true;
    loop {
        if out.len() > MAX_IMAGE {
            ok = false;
            break;
        }
        let at = out.len();
        out.resize(at + (64 << 10), 0);
        match prog.read(fd, &mut out[at..]) {
            Ok(0) => {
                out.truncate(at);
                break;
            }
            Ok(n) => out.truncate(at + n),
            Err(_) => {
                ok = false;
                break;
            }
        }
    }
    let _ = prog.close(fd);
    ok.then_some(out)
}

impl host::Backend for Inner {
    /// A reactor runs only if it has a verified image: a prog running
    /// compiled code cannot call into an interpreter's instance.
    fn reactor(
        &self,
        caller: &mut Caller<'_, Ctx>,
        path: &str,
        wasm: Vec<u8>,
    ) -> Result<(Module, Keep), Error> {
        drop(wasm);
        let prog = &mut caller.data_mut().prog;
        let digest = prog
            .proc
            .modules()
            .last()
            .map(|m| m.digest)
            .ok_or_else(|| no_image(path, "no digest"))?;
        match self.find(prog, path, &digest) {
            Found::Image(l) => Ok((l.module.clone(), host::keep(l))),
            Found::Missing => Err(no_image(path, "no AOT image")),
            Found::Refused(e) => Err(no_image(path, &e)),
        }
    }
}

fn no_image(path: &str, why: &str) -> Error {
    Error::from(format!("{EBADEXEC}: {path}: {why}"))
}

impl purg_vm::Engine for Aot {
    fn name(&self) -> &'static str {
        match self.inner.kind {
            Kind::Native => "wasmtime-aot",
            Kind::Pulley => "wasmtime-pulley",
        }
    }

    fn run(&self, wasm: Vec<u8>, mut ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        // The loader recorded the module's digest and path just before.
        let last = ctx
            .prog
            .proc
            .modules()
            .last()
            .map(|m| (m.digest, m.path.clone()));
        let (generation, loaded) = match last {
            Some((digest, path)) if !ctx.prog.proc.debug.traced() => {
                match self.module(&mut ctx, &path, &digest) {
                    (g, Found::Image(l)) => (g, l),
                    (_, Found::Missing) if self.mode == Mode::Auto => {
                        return self.interpret(wasm, ctx);
                    }
                    (_, Found::Refused(_)) if self.mode == Mode::Auto => {
                        return self.interpret(wasm, ctx);
                    }
                    (_, Found::Missing) => {
                        return (ctx, Err(Error::from(format!("{EBADEXEC}: no AOT image"))));
                    }
                    (_, Found::Refused(e)) => {
                        return (ctx, Err(Error::from(format!("{EBADEXEC}: {e}"))));
                    }
                }
            }
            _ => return self.interpret(wasm, ctx),
        };
        drop(wasm);
        self.native.fetch_add(1, Ordering::Relaxed);
        let pid = ctx.prog.pid;
        let mut store = Store::new(&self.inner.engine, ctx);
        host::limit(&mut store);
        store.epoch_deadline_callback(|state| {
            if state.data().prog.proc.note.killed() {
                return Err(wasmtime_runtime::Error::new(Stop::killed()));
            }
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);
        self.inner.hooks.sync_code();
        let mut res = None;
        self.inner.hooks.on_stack(STACK, &mut || {
            res = Some(self.call(&mut store, &loaded.module));
        });
        let ctx = store.into_data();
        // The store is gone; now what its reactors and module kept.
        self.reactors.exited(pid);
        drop(loaded);
        drop(generation);
        let res = res.unwrap_or_else(|| Err(Error::from("the guest did not run")));
        (ctx, res)
    }
}

impl Aot {
    fn interpret(&self, wasm: Vec<u8>, ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        self.interpreted.fetch_add(1, Ordering::Relaxed);
        self.fallback.run(wasm, ctx)
    }
}

/// wasmtime's thread-local storage hook for this crate's own tests on the
/// host, where wasmtime may be built without `std` (the kernel supplies
/// its own).
#[cfg(test)]
mod test_tls {
    use core::sync::atomic::{AtomicPtr, Ordering};

    static TLS: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());

    #[unsafe(no_mangle)]
    extern "C" fn wasmtime_tls_get(_slot: usize) -> *mut u8 {
        TLS.load(Ordering::Relaxed)
    }

    #[unsafe(no_mangle)]
    extern "C" fn wasmtime_tls_set(_slot: usize, ptr: *mut u8) {
        TLS.store(ptr, Ordering::Relaxed);
    }
}
