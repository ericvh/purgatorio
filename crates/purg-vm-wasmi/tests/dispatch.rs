//! wasmi's dispatch runs in constant native stack, in the profile the tests
//! are built with (issue #239).
//!
//! Each of wasmi's instruction handlers ends by calling the next one, and
//! relies on LLVM turning that call into a jump. When it does not (in a
//! dev build with debug assertions, the standard library's checks take
//! the address of a local in every handler), each wasm instruction a prog
//! runs keeps a native frame until its slice of fuel runs out: tens of
//! MiB, and an emu died of the overflow. Here a loop of a few million
//! instructions runs on a thread with a small stack; it overflows (and
//! the test process aborts) unless the dispatch jumps.

use wasmi::{CompilationMode, Config, Engine, Linker, Module, Store};

/// `(func (export "run") (param i32) (loop (br_if 0 (local.tee 0
/// (i32.sub (local.get 0) (i32.const 1))))))`: counts its argument down
/// to zero.
const COUNTDOWN: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic, version
    0x01, 0x05, 0x01, 0x60, 0x01, 0x7f, 0x00, // type 0: (i32) -> ()
    0x03, 0x02, 0x01, 0x00, // func 0: type 0
    0x07, 0x07, 0x01, 0x03, b'r', b'u', b'n', 0x00, 0x00, // export "run"
    0x0a, 0x10, 0x01, 0x0e, 0x00, // code: one body, no locals
    0x03, 0x40, // loop
    0x20, 0x00, 0x41, 0x01, 0x6b, // local.get 0, i32.const 1, i32.sub
    0x22, 0x00, 0x0d, 0x00, // local.tee 0, br_if 0
    0x0b, 0x0b, // end, end
];

/// The native stack the loop gets: far less than one frame per
/// instruction would need, far more than translating it takes.
const STACK: usize = 512 << 10;

#[test]
fn dispatch_takes_no_stack_per_instruction() {
    std::thread::Builder::new()
        .name("countdown".into())
        .stack_size(STACK)
        .spawn(|| {
            // As purg-vm-wasmi configures its engines.
            let mut config = Config::default();
            config.consume_fuel(true);
            config.compilation_mode(CompilationMode::Lazy);
            let engine = Engine::new(&config);
            let module = Module::new(&engine, COUNTDOWN).unwrap();
            let mut store = Store::new(&engine, ());
            store.set_fuel(u64::MAX).unwrap();
            let instance = Linker::new(&engine)
                .instantiate_and_start(&mut store, &module)
                .unwrap();
            let run = instance.get_typed_func::<i32, ()>(&store, "run").unwrap();
            run.call(&mut store, 2_000_000).unwrap();
            u64::MAX - store.get_fuel().unwrap()
        })
        .unwrap()
        .join()
        .map(|used| assert!(used >= 2_000_000, "fuel used: {used}"))
        .unwrap();
}
