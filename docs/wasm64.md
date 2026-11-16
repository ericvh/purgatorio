# wasm64 guests

Purgatorio's applications are `wasm32-wasip1` modules. Since 1.0 a guest
can also be a wasm64 module: Memory64, with 64-bit pointers. The two
widths coexist. A prog is one or the other, and progs of both widths
share a name space, pipes and files.

Both engines run wasm64 guests, on the emu and on the kernel. The SDK and
nearly every command build for both widths (issue #50). What is not done
is at the end.

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

`load` and `call` are there too. A reactor is a wasm32 module with a
memory of its own, whatever the width of the prog that loads it: the
host copies the arguments and the result between the two memories, so
only the caller's pointers differ. There are no wasm64 reactors.

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

A wasm64 module is traced as a wasm32 one is (`stack`, `prof`, `cprof`,
`mprof`, `mdb`): the stack memory the rewrite adds is a 32-bit memory in
either width, and the page counts wrap `memory.size` to an `i32`.

The kernel's AOT engine runs wasm64 modules from images as it does
wasm32 ones: `cargo xtask aot` compiles those in the boot image.

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
| `std::time::{SystemTime, Instant, UNIX_EPOCH, Duration}` | `purgatorio::time::...` |
| `std::thread::sleep` | `purgatorio::thread::sleep` |
| `std::env::args` | the `args` of `#[purgatorio::main]`, or `purgatorio::rt::args()` |
| `std::env::vars`, `var` | `purgatorio::env::{vars, var}` |
| `std::env::set_current_dir`, `current_dir` | `purgatorio::chdir`, `purgatorio::dot` |
| WASI `random_get` | `purgatorio::auth::Rng::fill` |

In a wasm32 build each of these is the `std` item itself, re-exported,
so a wasm32 module does not change. In a wasm64 build they are the SDK's
own types over `purgatorio64`. `std::io`'s traits (`Read`, `Write`,
`Seek`, `BufRead`) and `BufReader`, `BufWriter`, collections, `String`
and `format!` are `std`'s in both.

A wasm64 guest's `std` still has `std::fs`, `println!` and `std::time`:
`wasm64-unknown-unknown` compiles them as stubs. `std::fs` calls fail,
`println!` prints nothing, and `SystemTime::now` panics. Code that was
not switched builds and then misbehaves, so switch all of a command at
once and run its tests. A failed allocation is the exception: the SDK
reports it on fd 2 as `std` does in a wasm32 guest.

`std::fs` under the SDK's name is not all of `std::fs`. A wasm64 `File`
has `open`, `create`, `options`, `metadata`, `set_len`, `sync_all`,
`try_clone`, `read_at` and `write_at`; `Metadata` answers `len`,
`is_dir`, `is_file` and `modified`; `rename` works within a directory,
as Inferno's `wstat` does. `io::Error`s carry Inferno's error string and
the `ErrorKind`s programs test (`NotFound`, `PermissionDenied`,
`AlreadyExists`, `Interrupted`).

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
A module installed there must import `purgatorio64` alone; the build
refuses one that names WASI.

Every command says so except these, which stay wasm32:

- `disk/mkdisk` and `disk/chkdisk`: `purg-fs` copies a host tree and
  opens a disk image with `std::fs`, for `xtask` as well as for them.
- `test/abi`, `test/wasi` and `test/net`: they test WASI itself.
- The reactors in `/bin/lib`, which are wasm32 for every caller.

Rust ships no `std` for this target (it is tier 3), so the build
compiles `std` from the toolchain's `rust-src` component with
`-Zbuild-std`, let through on the pinned stable compiler with
`RUSTC_BOOTSTRAP=1`. (The SDK also uses one unstable `std` call there,
the allocation error hook.) The compiler and the `std` source are the pinned
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
(`pointer 64 bits`).

The acceptance tests run it on wasmtime and wasmi, and run the whole sh
corpus a second time on the wasm64 `sh` with the wasm64 commands first
in `/bin` (`sh_script_corpus_wide`). A script's output is the same in
both widths, with four exceptions that have a `.out64` file beside
their `.out`: a debugger's stack (`mdb`), a module's imports
(`wasmtools`), an allocator's page counts (`cprof`) and a Styx tag
(`styxtools`: a wasm64 `cat` sends fewer requests, having no WASI
`fstat` to answer).

The probe and the kernel run the wasm64 conformance guest and a short
session on the wasm64 `sh` from their boot image. The image carries only
those wasm64 builds (`sh`, `echo`, `cat`, `sed`, `tr`, `test/wide`): all
of `/bin/64` would double it.

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
is `/bin/64/test`, and a command that exists only as `/bin/test/abi.wasm`
is not found. `/lib/sh/wide` does the bind, after putting the wasm32
directory behind each wasm64 directory that lacks commands:

```
run /lib/sh/wide
```

To start a session in wasm64 from the outside, name the shell:
`emu /bin/64/sh.wasm -c 'run /lib/sh/wide; ...'`.

(An earlier version of this note put the widths in `/bin/32` and
`/bin/64` and taught the shell and `spawn_prog` to pick. The bind does
the same with no new code, and leaves every existing path as it is.)

## Cost

Measured on the emu (release build, an 8-core arm64 Linux host, medians
of five runs) with the 218 commands that exist in both widths.

Size: the wasm64 modules are 19.7 MB against 25.3 MB, 22% smaller in
all and 27% at the median. They carry no wasi-libc. `sh` is 334 KB
against 353 KB, `ls` 67 KB against 101 KB, `echo` 38 KB against 62 KB.

Time, in seconds, wasm32 then wasm64. The wasmtime columns are less the
0.36 s and 0.45 s it takes to compile and start the shell in each width:

| case | wasmtime | wasmi |
|---|---|---|
| `grep -c` over 27 MB | 2.38, 2.91 | 18.9, 21.3 |
| a 50,000-turn `sh` loop | 0.35, 0.41 | 4.85, 5.12 |
| 1,000 `echo`s spawned | 0.39, 0.30 | 0.31, 0.27 |

Work in the guest is about 20% slower as wasm64 on wasmtime and 5 to
13% on wasmi. The likely cause on wasmtime is that a 64-bit memory
cannot use guard pages in place of bounds checks, so every access is
checked. Work in the host (spawning, I/O) costs the same.

So wasm64 is for a program that needs the address space. wasm32 stays
the default.

## What is left

- `disk/mkdisk` and `disk/chkdisk` as wasm64: `purg-fs` would take its
  files through a trait instead of `std::fs`.
- wasm64 reactors, if a reactor ever needs more than 4 GiB.
- A guest with more than 4 GiB of memory has not been tried.
- The kernel's image carries a few wasm64 builds, not all of them.
