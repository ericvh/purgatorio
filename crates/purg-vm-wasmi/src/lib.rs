//! wasmi backend for [`purg_vm::Engine`]: a pure-Rust `no_std` interpreter,
//! used by the bare-metal probe and kernel and selectable in the emu with
//! `--vm=wasmi`.
#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;

use purg_abi::error::EBADEXEC;
use purg_core::Error;
use purg_vm::{Ctx, Mem, Stop};
use wasmi::{Caller, Engine, Extern, Linker, Module, Store};

pub struct Wasmi {
    engine: Engine,
    linker: Linker<Ctx>,
}

impl Wasmi {
    pub fn new() -> Result<Wasmi, Error> {
        let engine = Engine::default();
        let mut linker = Linker::new(&engine);
        link(&mut linker).map_err(|e| Error::from(format!("{e}")))?;
        Ok(Wasmi { engine, linker })
    }
}

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

fn link(linker: &mut Linker<Ctx>) -> Result<(), wasmi::Error> {
    macro_rules! register {
        ($module:expr, $name:literal, $f:path, ($($a:ident : $t:ty),*) -> $r:ty) => {
            linker.func_wrap(
                $module,
                $name,
                |mut caller: Caller<'_, Ctx>, $($a: $t),*| -> Result<$r, wasmi::Error> {
                    let (data, ctx) = memory(&mut caller)?;
                    $f(ctx, &mut Mem(data), $($a),*).map_err(|s| wasmi::Error::host(HostStop(s)))
                },
            )?;
        };
    }
    purg_vm::for_each_import!(register);
    Ok(())
}

fn bad_exec(e: impl core::fmt::Display) -> Error {
    Error::from(format!("{EBADEXEC}: {e}"))
}

impl Wasmi {
    fn call(&self, store: &mut Store<Ctx>, wasm: &[u8]) -> Result<Stop, Error> {
        let module = Module::new(&self.engine, wasm).map_err(bad_exec)?;
        let instance = self
            .linker
            .instantiate_and_start(&mut *store, &module)
            .map_err(bad_exec)?;
        let entry = instance
            .get_typed_func::<(), ()>(&*store, purg_abi::ENTRY)
            .map_err(bad_exec)?;
        Ok(match entry.call(&mut *store, ()) {
            Ok(()) => Stop::Exit(String::new()),
            Err(e) => match e.downcast_ref::<HostStop>() {
                Some(stop) => stop.0.clone(),
                None => Stop::Exit(format!("trap: {e}")),
            },
        })
    }
}

impl purg_vm::Engine for Wasmi {
    fn name(&self) -> &'static str {
        "wasmi"
    }

    fn run(&self, wasm: &[u8], ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        let mut store = Store::new(&self.engine, ctx);
        let res = self.call(&mut store, wasm);
        (store.into_data(), res)
    }
}
