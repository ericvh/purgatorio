//! What a module costs wasmi in RAM, measured on the host with a counting
//! allocator (the numbers in docs/footprint.md):
//!
//! ```sh
//! cargo run --release -p purg-vm-wasmi --example modsize -- root/bin/{echo,sh,calc}.wasm
//! ```
//!
//! For each module it prints the heap wasmi holds after `Module::new` with
//! lazy translation (what the kernel does: the engine keeps the function
//! bodies and translates each on its first call), and with eager
//! translation (every function translated: the upper bound a long-running
//! prog approaches). The engine is configured as the kernel's is, and the
//! figures are for a 64-bit host, like the kernel.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use wasmi::{CompilationMode, Config, Engine, Module};

struct Counting;

static USED: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator, only counting bytes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        USED.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        USED.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Bytes the engine holds for `wasm` after `Module::new` in `mode`.
fn held(wasm: &[u8], mode: CompilationMode) -> Result<usize, wasmi::Error> {
    let mut config = Config::default();
    config.consume_fuel(true).compilation_mode(mode);
    let engine = Engine::new(&config);
    let before = USED.load(Ordering::Relaxed);
    let module = Module::new(&engine, wasm)?;
    let after = USED.load(Ordering::Relaxed);
    drop(module);
    Ok(after.saturating_sub(before))
}

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: modsize file.wasm...");
        std::process::exit(2);
    }
    println!(
        "{:<28} {:>9} {:>12} {:>12} {:>7}",
        "module", "wasm", "lazy", "translated", "ratio"
    );
    for f in &files {
        let wasm = match std::fs::read(f) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("{f}: {e}");
                std::process::exit(1);
            }
        };
        let sizes = held(&wasm, CompilationMode::LazyTranslation)
            .and_then(|lazy| Ok((lazy, held(&wasm, CompilationMode::Eager)?)));
        match sizes {
            Ok((lazy, eager)) => println!(
                "{:<28} {:>9} {:>12} {:>12} {:>6.1}x",
                f.rsplit('/').next().unwrap_or(f),
                wasm.len(),
                lazy,
                eager,
                eager as f64 / wasm.len() as f64
            ),
            Err(e) => {
                eprintln!("{f}: {e}");
                std::process::exit(1);
            }
        }
    }
}
