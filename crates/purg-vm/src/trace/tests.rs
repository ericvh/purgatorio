extern crate std;

use alloc::vec;
use alloc::vec::Vec;
use std::path::PathBuf;

use wasmi::{Engine, Linker, Module, Store};

use super::*;

/// The sections of a module: `(id, contents)`.
fn sections(wasm: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut r = Reader::new(&wasm[8..]);
    let mut out = Vec::new();
    while !r.rest().is_empty() {
        let id = r.byte().unwrap();
        let n = r.u32().unwrap() as usize;
        out.push((id, r.skip(n).unwrap().to_vec()));
    }
    out
}

fn module(parts: &[(u8, &[u8])]) -> Vec<u8> {
    let mut m = b"\0asm\x01\0\0\0".to_vec();
    for &(id, body) in parts {
        section(id, body, &mut m);
    }
    m
}

/// Two functions and no memory:
///
/// - function 0, `rec(n, t)`: at `n == 0` it traps if `t` and else
///   returns 0; otherwise it returns `rec(n - 1, t) + 1`;
/// - function 1, `top(n, t)`, calls `rec(n, t)`.
fn recursive() -> Vec<u8> {
    module(&[
        (1, &[1, 0x60, 2, 0x7f, 0x7f, 1, 0x7f]),
        (3, &[2, 0, 0]),
        (
            7,
            &[2, 3, b'r', b'e', b'c', 0, 0, 3, b't', b'o', b'p', 0, 1],
        ),
        (
            10,
            &[
                2, 29, 0, 0x20, 0, 0x45, 0x04, 0x40, 0x20, 1, 0x04, 0x40, 0x00, 0x0b, 0x41, 0,
                0x0f, 0x0b, 0x20, 0, 0x41, 1, 0x6b, 0x20, 1, 0x10, 0, 0x41, 1, 0x6a, 0x0b, 8, 0,
                0x20, 0, 0x20, 1, 0x10, 0, 0x0b,
            ],
        ),
    ])
}

fn root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../root"))
}

/// The rewritten module runs as the original does, and its stack memory
/// holds the call stack: empty after a return, whole after a trap.
#[test]
fn keeps_the_call_stack() {
    let traced = instrument(&recursive()).unwrap();
    let engine = Engine::default();
    let module = Module::new(&engine, &traced).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .unwrap();
    let top = instance
        .get_typed_func::<(i32, i32), i32>(&store, "top")
        .unwrap();
    let stack = instance.get_memory(&store, STACK).unwrap();

    assert_eq!(top.call(&mut store, (5, 0)).unwrap(), 5);
    assert_eq!(read(stack.data(&store)), Stack::default());

    assert!(top.call(&mut store, (5, 1)).is_err());
    let s = read(stack.data(&store));
    assert_eq!(s.depth, 7);
    assert_eq!(s.frames, [0, 0, 0, 0, 0, 0, 1]);

    // The next call starts from the stack the trap left: a prog does not
    // run on after a trap, but a thunk restores what it found.
    assert_eq!(top.call(&mut store, (2, 0)).unwrap(), 2);
    assert_eq!(read(stack.data(&store)).depth, 7);

    // Deeper than a snapshot keeps: the innermost frames and the depth.
    stack.data_mut(&mut store)[..4].fill(0);
    assert!(top.call(&mut store, (400, 1)).is_err());
    let s = read(stack.data(&store));
    assert_eq!(s.depth, 402);
    assert_eq!(s.frames, vec![0; MAX_FRAMES]);
}

#[test]
fn rewrites_only_what_it_must() {
    let wasm = recursive();
    let traced = instrument(&wasm).unwrap();
    let (old, new) = (sections(&wasm), sections(&traced));
    let ids: Vec<u8> = new.iter().map(|s| s.0).collect();
    // A memory section appears, in its place.
    assert_eq!(ids, [1, 3, 5, 7, 10]);
    assert_eq!(new[0], old[0]);
    assert_eq!(new[1].1, [4, 0, 0, 0, 0]);
    assert_eq!(new[2].1, [1, 1, 1, 1]);
    // The old exports, then the stack.
    let mut exports = old[2].1.clone();
    exports[0] = 3;
    exports.push(STACK.len() as u8);
    exports.extend_from_slice(STACK.as_bytes());
    exports.extend_from_slice(&[2, 0]);
    assert_eq!(new[3].1, exports);
    // Four bodies, and the last two are the original ones.
    assert_eq!(new[4].1[0], 4);
    assert!(new[4].1.ends_with(&old[3].1[1..]));
}

/// A module with every section, imports and a memory of its own: the
/// rewritten module still validates, and its stack is memory 1.
#[test]
fn rewrites_the_sample_module() {
    let wasm = std::fs::read(root().join("lib/test/tiny.wasm")).unwrap();
    let traced = instrument(&wasm).unwrap();
    Module::new(&Engine::default(), &traced).unwrap();
    let (old, new) = (sections(&wasm), sections(&traced));
    assert_eq!(
        old.iter().map(|s| s.0).collect::<Vec<_>>(),
        new.iter().map(|s| s.0).collect::<Vec<_>>()
    );
    for (o, n) in old.iter().zip(&new) {
        assert_eq!(matches!(o.0, 3 | 5 | 7 | 10), o.1 != n.1, "section {}", o.0);
    }
    let mem = &new.iter().find(|s| s.0 == 5).unwrap().1;
    assert_eq!(mem, &[2, 0, 17, 1, 1, 1]);
    let exports = &new.iter().find(|s| s.0 == 7).unwrap().1;
    assert!(exports.ends_with(&[2, 1]));
}

/// The guests the build installs, when they are there: each still
/// validates when traced.
#[test]
fn rewrites_built_guests() {
    for name in ["sh", "ls", "auth/factotum", "lib/add", "test/debug"] {
        let Ok(wasm) = std::fs::read(root().join(std::format!("bin/{name}.wasm"))) else {
            continue;
        };
        let traced = instrument(&wasm).unwrap_or_else(|| panic!("{name}: not rewritten"));
        Module::new(&Engine::default(), &traced).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn leaves_alone_what_it_cannot_rewrite() {
    assert_eq!(instrument(b""), None);
    assert_eq!(instrument(b"#!/bin/sh\n"), None);
    // No functions.
    assert_eq!(instrument(b"\0asm\x01\0\0\0"), None);
    // A function section without a code section.
    assert_eq!(
        instrument(&module(&[(1, &[1, 0x60, 0, 0]), (3, &[1, 0])])),
        None
    );
    // A type that is not a function type.
    let gc = module(&[(1, &[1, 0x5f, 0]), (3, &[1, 0]), (10, &[1, 2, 0, 0x0b])]);
    assert_eq!(instrument(&gc), None);
    // A function of a type the module does not have.
    let bad = module(&[(1, &[1, 0x60, 0, 0]), (3, &[1, 9]), (10, &[1, 2, 0, 0x0b])]);
    assert_eq!(instrument(&bad), None);
}

/// Truncated and damaged modules are rewritten or refused, never a
/// panic.
#[test]
fn damaged_modules_never_panic() {
    let samples = [
        recursive(),
        std::fs::read(root().join("lib/test/tiny.wasm")).unwrap(),
    ];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for wasm in &samples {
        for n in 0..wasm.len() {
            let _ = instrument(&wasm[..n]);
        }
        for at in 8..wasm.len() {
            for v in [0u8, 1, 0x7f, 0x80, 0xff] {
                let mut m = wasm.clone();
                m[at] = v;
                let _ = instrument(&m);
            }
        }
        for _ in 0..5000 {
            let mut m = wasm.clone();
            for _ in 0..1 + next() % 4 {
                let at = (next() % m.len() as u64) as usize;
                m[at] = next() as u8;
            }
            let _ = instrument(&m);
        }
    }
}

#[test]
fn reads_a_stack_memory() {
    assert_eq!(read(&[]), Stack::default());
    assert_eq!(read(&[8, 0, 0]), Stack::default());
    // Two frames: 7 called 9.
    let mut m = vec![0u8; 65536];
    m[0] = 8;
    m[8] = 7;
    m[12] = 9;
    let s = read(&m);
    assert_eq!((s.depth, s.frames.as_slice()), (2, &[9, 7][..]));
    // A memory shorter than its pointer says has no frames to give.
    assert!(read(&m[..12]).frames.is_empty());
    // The ring has wrapped: frame 8192 lies where frame 0 did.
    m[..4].copy_from_slice(&(4 * 8193u32).to_le_bytes());
    m[8] = 5;
    let s = read(&m);
    assert_eq!(s.depth, 8193);
    assert_eq!(s.frames.len(), MAX_FRAMES);
    assert_eq!(s.frames[0], 5);
}

#[test]
fn numbers() {
    let enc = |v: u32| {
        let mut out = Vec::new();
        sleb(v, &mut out);
        out
    };
    assert_eq!(enc(0), [0]);
    assert_eq!(enc(63), [63]);
    assert_eq!(enc(64), [0xc0, 0]);
    assert_eq!(enc(MASK), [0xfc, 0xff, 1]);
    // The top bit set is a negative i32.
    assert_eq!(enc(u32::MAX), [0x7f]);
    assert_eq!(enc(0x8000_0000), [0x80, 0x80, 0x80, 0x80, 0x78]);
}

fn instance(wasm: &[u8], fuel: bool) -> (Store<()>, wasmi::Instance) {
    let mut config = wasmi::Config::default();
    config.consume_fuel(fuel);
    // Translation is free, as in the backend, so running out of fuel
    // can only happen where a call can be resumed.
    config.fuel_cost(wasmi::CustomFuelCosts {
        bytes_copied_per_fuel: 64,
        fuel_per_bytes_translated: 0,
        fuel_per_bytes_validated: 0,
    });
    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm).unwrap();
    let mut store = Store::new(&engine, ());
    if fuel {
        store.set_fuel(1000).unwrap();
    }
    let instance = Linker::new(&engine)
        .instantiate_and_start(&mut store, &module)
        .unwrap();
    (store, instance)
}

fn u64_at(m: &[u8], at: u32) -> u64 {
    u64::from_le_bytes(m[at as usize..at as usize + 8].try_into().unwrap())
}

fn u32_at(m: &[u8], at: u32) -> u32 {
    u32::from_le_bytes(m[at as usize..at as usize + 4].try_into().unwrap())
}

/// Counted, every call of every function adds one.
#[test]
fn counts_calls() {
    let opts = Options {
        calls: true,
        pages: true,
    };
    let (traced, layout) = rewrite(&recursive(), opts).unwrap();
    // No memory 0: no pages to count.
    assert_eq!(layout.pages, None);
    let at = layout.calls.unwrap();
    assert_eq!((layout.imported, layout.funcs, at % 8), (0, 2, 0));
    let (mut store, instance) = instance(&traced, false);
    let top = instance
        .get_typed_func::<(i32, i32), i32>(&store, "top")
        .unwrap();
    assert_eq!(top.call(&mut store, (5, 0)).unwrap(), 5);
    assert_eq!(top.call(&mut store, (2, 0)).unwrap(), 2);
    let stack = instance.get_memory(&store, STACK).unwrap();
    let m = stack.data(&store);
    assert_eq!((u64_at(m, at), u64_at(m, at + 8)), (9, 2));
    assert_eq!(read(m), Stack::default());
}

/// Two functions: `o`, function 0, calls `g`, function 1, twice, and
/// `g` grows memory 0 by a page.
fn growing() -> Vec<u8> {
    module(&[
        (1, &[1, 0x60, 0, 0]),
        (3, &[2, 0, 0]),
        (5, &[1, 0, 1]),
        (7, &[1, 1, b'o', 0, 0]),
        (
            10,
            &[
                2, 6, 0, 0x10, 1, 0x10, 1, 0x0b, 7, 0, 0x41, 1, 0x40, 0, 0x1a, 0x0b,
            ],
        ),
    ])
}

/// A function's pages are those grown while it ran, in it or in what it
/// called.
#[test]
fn counts_pages() {
    let opts = Options {
        calls: false,
        pages: true,
    };
    let (traced, layout) = rewrite(&growing(), opts).unwrap();
    assert_eq!(layout.calls, None);
    let at = layout.pages.unwrap();
    let (mut store, instance) = instance(&traced, false);
    let o = instance.get_typed_func::<(), ()>(&store, "o").unwrap();
    o.call(&mut store, ()).unwrap();
    let m = instance.get_memory(&store, STACK).unwrap().data(&store);
    assert_eq!((u32_at(m, at), u32_at(m, at + 4)), (2, 2));
}

/// A function with its breakpoint set records its index plus one in the
/// hit word on entry, and waits there, with itself on the stack, until
/// the word is cleared.
#[test]
fn stops_at_a_breakpoint() {
    let (traced, layout) = rewrite(&recursive(), Options::default()).unwrap();
    assert_eq!(layout.breaks, BREAKS);
    let (mut store, instance) = instance(&traced, true);
    let stack = instance.get_memory(&store, STACK).unwrap();
    patch(stack.data_mut(&mut store), &[1, 0]);
    let top = instance
        .get_typed_func::<(i32, i32), i32>(&store, "top")
        .unwrap();
    let mut call = top.call_resumable(&mut store, (3, 0)).unwrap();
    for depth in 2..5 {
        let wasmi::TypedResumableCall::OutOfFuel(t) = call else {
            panic!("did not wait");
        };
        let m = stack.data(&store);
        assert_eq!(u32_at(m, HIT), 1);
        assert_eq!((read(m).depth, read(m).frames[0]), (depth, 0));
        // Released, it stops again at the next call of `rec`.
        patch(stack.data_mut(&mut store), &[1, 0]);
        store.set_fuel(1000).unwrap();
        call = t.resume(&mut store).unwrap();
    }
    // Released with the breakpoint taken away: it runs to the end.
    let wasmi::TypedResumableCall::OutOfFuel(t) = call else {
        panic!("did not wait");
    };
    patch(stack.data_mut(&mut store), &[0, 0]);
    store.set_fuel(1_000_000).unwrap();
    let wasmi::TypedResumableCall::Finished(n) = t.resume(&mut store).unwrap() else {
        panic!("did not finish");
    };
    assert_eq!(n, 3);
    assert_eq!(u32_at(stack.data(&store), HIT), 0);
}

/// Modules with more functions than the bytes and counts of one page
/// hold get a stack memory big enough.
#[test]
fn grows_the_stack_memory_for_big_modules() {
    let n = 9000u32;
    let mut funcs = Vec::new();
    leb(n, &mut funcs);
    funcs.resize(funcs.len() + n as usize, 0);
    let mut code = Vec::new();
    leb(n, &mut code);
    for _ in 0..n {
        code.extend_from_slice(&[2, 0, 0x0b]);
    }
    let wasm = module(&[
        (1, &[1, 0x60, 0, 0]),
        (3, &funcs),
        (5, &[1, 0, 1]),
        (10, &code),
    ]);
    let opts = Options {
        calls: true,
        pages: true,
    };
    let (traced, layout) = rewrite(&wasm, opts).unwrap();
    let end = layout.pages.unwrap() + 4 * n;
    let (store, instance) = instance(&traced, false);
    let stack = instance.get_memory(&store, STACK).unwrap();
    assert_eq!(
        stack.data(&store).len(),
        end.div_ceil(PAGE) as usize * 65536
    );
}

/// The guests the build installs, counted every way, still validate.
#[test]
fn rewrites_built_guests_with_counts() {
    let opts = Options {
        calls: true,
        pages: true,
    };
    for name in ["sh", "test/debug"] {
        let Ok(wasm) = std::fs::read(root().join(std::format!("bin/{name}.wasm"))) else {
            continue;
        };
        let (traced, layout) = rewrite(&wasm, opts).unwrap();
        assert!(layout.calls.is_some() && layout.pages.is_some());
        Module::new(&Engine::default(), &traced).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}
