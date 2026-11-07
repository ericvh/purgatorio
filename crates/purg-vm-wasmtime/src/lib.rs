//! wasmtime backend for [`purg_vm::Engine`]: Cranelift-compiled, the hosted
//! default.

use purg_abi::error::EBADEXEC;
use purg_core::Error;
use purg_vm::{Ctx, Mem, Stop};
use wasmtime::{AsContextMut, Caller, Config, Engine, Extern, Linker, Module, Store};

pub struct Wasmtime {
    engine: Engine,
    linker: Linker<Ctx>,
}

impl Wasmtime {
    pub fn new() -> Result<Wasmtime, Error> {
        let engine = Engine::new(&Config::new()).map_err(|e| Error::from(e.to_string()))?;
        let mut linker = Linker::new(&engine);
        link(&mut linker).map_err(|e| Error::from(e.to_string()))?;
        Ok(Wasmtime { engine, linker })
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

fn bad_exec(e: impl std::fmt::Display) -> Error {
    Error::from(format!("{EBADEXEC}: {e}"))
}

impl Wasmtime {
    fn call(&self, store: &mut Store<Ctx>, wasm: &[u8]) -> Result<Stop, Error> {
        let module = Module::new(&self.engine, wasm).map_err(bad_exec)?;
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
        let res = self.call(&mut store, wasm);
        (store.into_data(), res)
    }
}
