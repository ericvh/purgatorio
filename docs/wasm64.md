# wasm64 guests

Purgatorio's applications are `wasm32-wasip1` modules. Since 1.0 a guest
can also be a wasm64 module: Memory64, with 64-bit pointers. The two
widths coexist. A prog is one or the other, and progs of both widths
share a name space, pipes and files.

This is the first cut (issue #50). It runs wasm64 guests on both engines,
on the emu and on the kernel, and ports the SDK. It does not port every command. What is left is at the
end.

## Why a second ABI

A wasm32 guest gets Rust `std` (`std::fs`, `std::io`, `println!`,
`std::env::args`) through WASI preview1, which the host implements over
the name space. WASI preview1 is specified with `i32` pointers and 8-byte
iovecs, and Rust has no `wasm64-wasip1` target. So a wasm64 guest does
not use WASI at all. It is built for `wasm64-unknown-unknown` and reaches
the host only through the `purgatorio64` import module.

wasm32 and wasm64 are not one module with a flag. A module's memory is
`i32` or `i64`, and its imports have the matching types. A wasm64 module
that imports `wasi_snapshot_preview1` or `purgatorio` does not link, and
fails with `bad executable`.

## The `purgatorio64` module

`purgatorio64` is the `purgatorio` module (`docs/abi.md`) with two
changes:

- Every pointer, and every `usize` (a byte length or a count), is an
  `i64`. File descriptors, flags, modes and results stay `i32`. A count
  above `i32::MAX` is `bad arg in system call`. An address or length that
  does not lie in the guest's memory is `bad address`, as for wasm32.
- It holds the calls a wasm32 guest gets from WASI:

| call | result | what |
|---|---|---|
| `read(fd, buf, len)` | `i32` | `sys->read`: one read, which may be short |
| `write(fd, buf, len)` | `i32` | `sys->write` |
| `pread(fd, buf, len, off)` | `i32` | read at an offset |
| `pwrite(fd, buf, len, off)` | `i32` | write at an offset |
| `seek(fd, off, whence)` | `i64` | `sys->seek`; the new offset |
| `close(fd)` | `i32` | |
| `args(buf, len)` | `i32` | the arguments, each followed by a NUL; returns their size and copies them if `len` holds them |
| `sleep(ms)` | `i32` | `sys->sleep` |
| `nsec()` | `i64` | nanoseconds since 1970 UTC |
| `mono()` | `i64` | nanoseconds on the monotonic clock |
| `random(buf, len)` | `i32` | random bytes |

As in `purgatorio`, a failed call returns -1 and sets the errstr. One
read or write moves at most `i32::MAX` bytes, so that its count fits the
result; the guest asks again for the rest.

Structures in memory keep their layout where they hold no pointers:
`pipe`'s two fds, `pctl`'s fd list, `spawn_prog`'s fd pairs and `wait`'s
ops are `i32`s in both widths. An `alt` arm holds pointers, so it is 40
bytes in a wasm64 guest (`handle[4] kind[4] buf[8] len[8] caps[8]
ncaps[8]`) and 24 in a wasm32 one.

Not in `purgatorio64` yet: `load` and `call` (reactors).

There is no environment call. The environment is `/env`, as in Inferno,
and `purgatorio::env` reads it in both widths. Relative names go to the
host as they are; `open` resolves them against the prog's current
directory.

The entry point is `_start`, as for wasm32, and the module carries the
same `purgatorio.abi` section. The host tells the widths apart by the
module's memory type.

## In the host

The host functions are written once. A guest pointer is a `purg_vm::Ptr`,
which is `i32` or `i64`, and `Mem` bounds-checks either; each function
that takes pointers is generic over it. `for_each_import!` registers the
`i32` instances as `purgatorio` and `for_each_import64!` the `i64`
instances as `purgatorio64`, in wasmtime and in wasmi. Nothing else in
the host knows the width: devices, Styx, the name space and the
scheduler are unchanged.

A wasm64 module is not traced (`stack`, `prof`, `cprof`, `mprof`): the
rewrite that keeps a call stack assumes `i32` memory operands. It runs,
untraced.

Memory limits (`memlimit`) apply as they do to wasm32. A wasm64 guest may
declare more than 4 GiB; this has not been exercised yet.

## The SDK

The same source builds for both widths if it names files and standard
I/O through the SDK instead of `std`:

| instead of | use |
|---|---|
| `std::fs::{File, OpenOptions, read, read_to_string, write, read_dir, remove_file, create_dir}` | `purgatorio::fs::...` |
| `std::os::fd::{OwnedFd, AsRawFd, FromRawFd, IntoRawFd}` | `purgatorio::fd::...` |
| `std::io::{stdin, stdout, stderr}` | `purgatorio::io::...` |
| `println!`, `print!`, `eprintln!`, `eprint!` | `use purgatorio::{println, ...}` |
| `std::time::SystemTime`, `Instant`, `std::thread::sleep` | `purgatorio::time::{nsec, now, mono, sleep}` |
| `std::env::args` | the `args` of `#[purgatorio::main]` |

In a wasm32 build each of these is the `std` item itself, re-exported,
so a wasm32 module does not change. In a wasm64 build they are the SDK's
own types over `purgatorio64`. `std::io`'s traits (`Read`, `Write`,
`Seek`, `BufRead`) and `BufReader`, `BufWriter`, collections, `String`
and `format!` are `std`'s in both.

A wasm64 guest's `std` still has `std::fs`, `println!` and `std::time`:
`wasm64-unknown-unknown` compiles them as stubs. `std::fs` calls fail,
`println!` prints nothing, and `SystemTime::now` panics. Code that was
not switched builds and then misbehaves, so switch all of a command at
once and run its tests.

## Building

A crate is built as wasm64 as well as wasm32 when its manifest says so:

```toml
[package.metadata.purgatorio]
wasm64 = true
```

`cargo xtask build-root` then builds it a second time for
`wasm64-unknown-unknown` and installs it under `/bin/64`:
`appl/cmd/cat` is `/bin/cat.wasm` and `/bin/64/cat.wasm`, and
`appl/test/wide` is `/bin/test/wide.wasm` and `/bin/64/test/wide.wasm`.

Rust ships no `std` for this target (it is tier 3), so the build
compiles `std` from the toolchain's `rust-src` component with
`-Zbuild-std`, let through on the pinned stable compiler with
`RUSTC_BOOTSTRAP=1`. The compiler and the `std` source are the pinned
release's own, so the build is as reproducible as the wasm32 one, and
CI compares the hashes of `/bin/64` across hosts with the rest.
`rust-toolchain.toml` lists `rust-src`.

`cargo xtask clippy-guests` also lints the SDK and the wasm64 crates for
the wasm64 target.

## Tests

`appl/test/wide` is one source built for both widths. It exercises
files, seeks, stat, directories, errors, pipes, `ready`, the
environment, a child of the other width, the clocks and bad pointers,
and must print the same lines in both widths but the first
(`pointer 64 bits`). The acceptance tests run it and the wasm64 `echo`
and `cat` on wasmtime and wasmi; the probe and the kernel run the wasm64
build from their boot image.

## Choosing a width

`/bin` holds the wasm32 commands and `/bin/64` the wasm64 ones. Nothing
in the shell or in `spawn_prog` knows about widths. A name space chooses,
as `/$objtype/bin` does in Plan 9:

```
bind -b /bin/64 /bin
```

After that a command with a wasm64 build runs it, and a command without
one runs its wasm32 build, in the same pipeline. A wasm64 prog can spawn
a wasm32 one and the other way round. A path names a width directly:
`/bin/64/cat.wasm`.

A union does not merge subdirectories: with the bind above, `/bin/test`
is `/bin/64/test`. Bind those the same way where both are wanted.

(An earlier version of this note put the widths in `/bin/32` and
`/bin/64` and taught the shell and `spawn_prog` to pick. The bind does
the same with no new code, and leaves every existing path as it is.)

## What is left

- Port the commands. `echo` and `cat` are done; `sh` is the large one.
  Each is the mechanical swap in the table above.
- Reactors (`load`, `call`) for wasm64 guests.
- Tracing and profiling of wasm64 modules.
- AOT images for wasm64 modules. On the kernel a wasm64 module runs on
  wasmi, with or without the AOT engine: `cargo xtask aot` leaves
  `/bin/64` out.
- A measurement: what 64-bit pointers cost in module size and time.
