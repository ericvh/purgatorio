# wasm64 later, wasm32 now

Purgatorio stays on `wasm32-wasip1` for the current milestones. A wasm64 port is a later rewrite. The two widths can coexist. Nothing in the open pull requests needs to be rewritten or rebased for this.

## Decision

- Apps today are `wasm32-wasip1`. Rust `std` (`std::fs`, `std::io`, `println!`, `std::env::args`) works because the host implements WASI preview1 over the Purgatorio namespace.
- The emu and the later aarch64 kernel are native Rust binaries. They are not WebAssembly. The TCP stack and the filesystem live there. A guest reaches them by a host import, which then operates the namespace (`#U`, the RAM tree, 9P `mount`/`export`, `/net/tcp`, `/net/udp`, `/net/cs`).
- When a 64-bit guest is wanted, it is a separate ABI, `wasm64-purgatorio`: wasm64 (Memory64, `i64` pointers) plus the `purgatorio` import module. That is the same role `wasm32-wasip1` fills for WASI. It does not extend WASI preview1. WASI preview1 is specified with `i32` pointers and 8-byte iovecs, and there is no `wasm64-wasip1` target.
- wasm32 and wasm64 are not one module with a flag. Adopting Memory64 for an existing app means that app stops being `wasm32-wasip1`. Bump `purgatorio.abi` so an old `.wasm` fails cleanly instead of being misread.

## What stays put

Devices, 9P, the namespace, the scheduler, host sockets, and the probe's TCP stack do not change with pointer width. `dial`, `announce`, and `listen` are already file operations on `/net`. `mount` and `export` already take a file descriptor.

The `purgatorio` imports already include `open`, `create`, `stat`, and `dirread`. `read`, `write`, `seek`, and `close` exist only as WASI `fd_*` today. A wasm64 guest needs those four beside `open`, implemented by the same prog methods the WASI wrappers use, and a small SDK `File` with `Read` and `Write`. `dial`, the Styx helper, and `file2chan` then use that `File` instead of `std::fs`.

Until a Rust `std` backend calls those imports, the SDK crate is the contract. A target spec such as `wasm64-unknown-purgatorio.json` is optional packaging (`cfg(target_os = "purgatorio")`). The name alone does not provide `std::fs`.

## Layout when both widths exist

`/bin/cat.wasm` cannot be both widths. Use `/bin/32` and `/bin/64`. A prog runs the build that matches its own memory index: `cat` from a wasm32 shell is `/bin/32/cat.wasm`, and from a wasm64 shell is `/bin/64/cat.wasm`.

Leave files in `/bin` until a wasm64 app exists. The shell lookup and `spawn_prog` learn the two directories at that point, not before.

## What the later rewrite touches

Do it once on the tip of the stack. Do not edit pull requests 1 through 8 in place. Those commits were written against `std`.

- `Mem` and every pointer argument in the `purgatorio` imports become `i64`, in both wasmtime and wasmi. iovecs, `pipe`'s fd pair, and `pctl`'s fd list widen. wasmtime 50 and wasmi 2 can already run a Memory64 module.
- Drop `wasi_snapshot_preview1` for the wasm64 guest. Arguments, the environment, and file descriptors 0 through 4 are handed to a wasm32 app by WASI today. `open` already walks the namespace, so relative paths can go through that call.
- The SDK `extern` block is `cfg(target_os = "wasi")` with `i32` signatures. `dial`, the Styx helper, `file2chan`, and `bufio` call `std::fs`. Those switch to the SDK `File`.
- About thirty commands and the tests use `std::fs`, `std::io`, or `println!`. Each wasm64 build of a command is a mechanical swap. `sh` is the large one. Rebuild the `/bin/64` blobs. The probe picks up new blobs more than new kernel code.

wasm32 apps keep WASI and keep running. A command is rebuilt as wasm64 only when that command should exist in both widths.

