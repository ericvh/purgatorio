# Self-hosting the guest toolchain

Can `rustc` run inside Purgatorio and build the guest `.wasm` files? This is
a paper study. Nothing was downloaded, built or run for it. Every claim is
marked **[src N]** (verified from source N in the list at the end, read on
2026-11-09) or **[inf]** (our inference, not yet tested).

The goal: a `rustc` that is itself WebAssembly runs as a Purgatorio prog,
reads the workspace and a sysroot from the namespace, and writes guest
modules that are byte-identical to the ones `cargo xtask build-root` makes on
the CI hosts.

## Verdict

It is possible, and other people have done the hard half: rustc with the LLVM
backend has been compiled to WASI and produces working `wasm32-wasip1`
binaries in the browser [src 5, 6, 7, 8]. None of it is upstream, and every
existing build has three properties Purgatorio does not accept today. They
are built for `wasm32-wasip1-threads`, so they need shared memory and the
`wasi` `thread-spawn` import (D4 says no threads) [src 6, 7, 9]. They come
from forks pinned to old compilers (rustc 1.83-dev in the newest release we
found) [src 7]. And their linker is patched in-process, because WASI cannot
start a process [src 7, 9]. On our side, rustc writes every `.rlib` and
`.rmeta` through a temporary subdirectory and renames the file out of it,
which Purgatorio's WASI layer refuses with `EXDEV` [src 12, 13]. Our own
guest build is small: four library crates, forty commands, one 60-line proc
macro with no dependencies, and no build scripts [inf, from the repo]. That
makes driving rustc without cargo practical. A first spike on the hosted emu
under wasmtime is a few weeks of work. Byte-identical output needs our own
rustc 1.99.1 build for WASI, which is the largest and riskiest stage.

## What our build needs

From the repo (`Cargo.toml`, `xtask/src/main.rs`, `rust-toolchain.toml`):

- Toolchain pinned to Rust 1.99.1, target `wasm32-wasip1`, profile `wasm`:
  `opt-level = "s"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`,
  `strip = "symbols"`.
- `xtask` runs cargo with `--remap-path-prefix` for the workspace, cargo
  home and rustup home, and with itself as `RUSTC_WORKSPACE_WRAPPER`, which
  replaces each `-C metadata=<hash>` with `metadata=purgatorio-<crate>`.
- The guest crate graph: `purg-abi`, `purg-styx`, `purgatorio`, the proc
  macro `purgatorio-macros` (60 lines, uses only `proc_macro`, no `syn`), and
  40 commands plus `appl/lib/add` and the test guests. About 8,500 lines.
  No guest crate has a `build.rs`.
- Linking uses the toolchain's self-contained `rust-lld` (wasm flavor) and the
  `wasi-libc` objects shipped in the `rust-std` component, so no external C
  toolchain is needed [src 10, 11].

So a self-hosted build needs: rustc with the LLVM wasm backend, the
`wasm32-wasip1` std rlibs, `wasm-ld` of the same LLVM version as `rust-lld`,
a way to run one proc macro, and a way to sequence about 50 rustc runs.

## 1. rustc built for a WASI host

- **Upstream:** not supported. Both `wasm32-wasip1` and
  `wasm32-wasip1-threads` are tier 2 targets *without host tools* [src 1,
  10, 11]. Rust ships the std for them, not rustc. `wasm32-wasip1-threads`
  has shipped prebuilt std since nightly 1.71.1 (2023-08-03) and was renamed
  from `wasm32-wasi-preview1-threads` in March 2024 [src 11, 2]. No wasip2
  host build exists either [inf].
- **Upstream preparation:** bjorn3's PR #130899, "Couple of changes to make it
  easier to compile rustc for wasm", merged 2024-10-07 for 1.83. It is "a
  subset of the patches I have on my rust fork to compile rustc for
  wasm32-wasip1" [src 3]. The rest stays in a fork.
- **History:** Miri issue #722 (opened 2019-05-02) tracks the effort. In 2019
  the patches covered thread pool spawning, sysroot lookup and the working
  directory. On 2024-09-26 bjorn3 reported linking with lld working
  (in-process, via `lldWasm`). On 2024-12-10 he said rustc.wasm "needs to be
  compiled for wasm32-wasip1-threads for the time being" because of C++ mutex
  support in wasi-sdk, and users hit a `current_dll_path not supported`
  panic in sysroot discovery [src 4].
- **Demos and builds:**
  - `bjorn3/wasm-rustc`: a patched rustc precompiled for
    `wasm32-wasip1-threads` for the `browser_wasi_shim` example, built from
    the `compile_rustc_for_wasm15` branch; last updated April 2024 [src 5, 6].
  - `oligamiq/rust_wasm`: CI that builds rustc with the LLVM backend (from
    `oligamiq/rust`), a Cranelift-backed rustc (from `bjorn3/rust`), and
    clang and `wasm-ld` (from YoWASP) as wasm. Releases v0.1.0 to v3.0.0 and
    v0.2.1, the latest on 29 Aug; rustc 1.83-dev for v3.0.0. Offers
    `rustc.wasm` and `rustc_unwind.wasm` and abort or unwind sysroots [src 7,
    8].
  - `oligamiq/rubrc`: rustc in the browser using those builds. It produces
    executables for `wasm32-wasip1` and `x86_64-unknown-linux-musl`; cargo
    works for local libraries, but external dependencies and proc macros do
    not. MIT OR Apache-2.0 [src 9].
  - `Mearman/wasm-rustc`: weekly builds of bjorn3's branch for
    `wasm32-wasip1-threads`. "~80MB + ~144MB sysroot". No linker yet, so it
    emits object files only [src 6].
  - `rntz-llm/rust-wasm-serve`: rustc.wasm about 31 MB plus a 25 MB sysroot
    (from rust_wasm v3.0.0), LLVM and lld linked in, rustc "patched to link
    in-process". Small programs at `opt-level=0` compile in 1 to 3 s in
    headless Chromium on 4 cores [src 8].
- **What it needs:**
  - **Threads.** Every build above targets `wasm32-wasip1-threads` and needs
    shared memory and `wasi-threads` (wasmtime `--wasi threads`) [src 6, 8,
    11]. rustc itself always runs the compiler on a spawned thread so it can
    set the stack size (`RUST_MIN_STACK`), and runs a thread pool for the
    parallel front end [src 14]. On plain `wasm32-wasip1`, spawning a thread
    "will always return an error" [src 10], so a single-threaded rustc needs a
    patch to run on the main thread [inf].
  - **The C++ side single-threaded.** The reason given for the threads target
    was LLVM's C++ mutexes [src 4]. libc++ support for `THREAD_MODEL=single`
    landed in wasi-sdk 26 (reported 2025-09-06) [src 15]. A single-threaded
    LLVM for `wasm32-wasip1` is therefore now plausible; YoWASP's clang
    already targets `wasm32-wasip1` [src 15, 17]. A single-threaded rustc
    built this way has not been reported [inf].
  - **No dylibs.** The codegen backend must be linked in (`feature = "llvm"`),
    not loaded from `codegen-backends/` [src 14]. Proc macros are dylibs too
    (see 4).
  - **Sysroot.** rustc cannot find its own location on WASI, so pass
    `--sysroot` explicitly [src 4].

## 2. LLVM, clang and lld as WASI programs

- **YoWASP/clang** (whitequark): LLVM, Clang and LLD built for WASI, published
  to PyPI (`yowasp-clang`) and npm (`@yowasp/clang`). Apache-2.0 [src 16,
  17]. The repository was archived on 2026-03-11 [src 17]; the npm package
  was updated 2026-03-12, version 22.0.0-git [src 16]. A user ran clang
  21.1.4 for target `wasm32-unknown-wasip1` with it [src 16]. Avoids threads
  because they need `SharedArrayBuffer`, and "Wasmtime seems to crash if LLD
  spawns a thread" (2024) [src 15]. The whole toolchain was "less than 23 MB"
  (2024-07-14) [src 15].
- **Upstreaming to LLVM:** RFC "Building LLVM for WebAssembly" (2024-05-19).
  Reviewers supported it. It waited on wasi-libc (merged 2024-12-03) and
  libc++ single-thread primitives (in wasi-sdk 26). As of the last posts
  (2025-09) the LLVM patches were not merged and no LLVM release carries them
  [src 15].
- **wasi-sdk** itself ships only native host builds (latest wasi-sdk-34, 25
  Aug, LLVM tip of tree from 2026-07-01). No wasm-hosted asset is listed
  [src 18].
- **License:** LLVM is Apache-2.0 WITH LLVM-exception; that is compatible
  with D1 [inf, standard LLVM license].
- **For us:** `wasm-ld` must be the same LLD version as the `rust-lld` that
  Rust 1.99.1 ships (Rust's LLVM fork), or the output will differ from the
  host build [inf]. YoWASP's version will not match. Building `wasm-ld`
  from Rust's `src/llvm-project` at the matching commit is the safe route
  [inf].

## 3. Alternatives to LLVM

- **Cranelift backend (`rustc_codegen_cranelift`):** emits code for x86_64,
  AArch64, riscv64 and s390x only [src 19]. In 2019 bjorn3 noted Cranelift
  "doesnt support wasm output yet" [src 4]; nothing since changes that.
  A Cranelift-backed rustc.wasm exists [src 7], but it produces native
  objects, not guests. It could one day build *kernel* or *emu* code inside
  Purgatorio, not apps [inf].
- **GCC backend, CLR backend and others:** no wasm output [inf].
- **So LLVM is required** for guest codegen. The LLVM part is also what makes
  rustc.wasm 30 to 80 MB [src 6, 8].
- **Smaller stepping stones** that need no compiler:
  - `wasm-tools` and its crates (`wasmparser`, `wasmprinter`, `wat`,
    `wasm-encoder`) are Rust, Apache-2.0 WITH LLVM-exception, and build for
    `wasm32-wasip1` [inf]. `wasmparser` is already in our host build. Guest
    commands for validate, print (wasm to wat), parse (wat to wasm), strip
    and dump of imports, exports and the `purgatorio.abi` section are
    straightforward. They cover much of PG-41 (`disdep`, `disdump`).
  - `wat2wasm` from WABT is C++; the `wat` crate replaces it [inf].
  - A wasm *object* linker written in Rust: we found no mature one [inf].
    `wasm-ld` stays the linker. Binaryen's `wasm-merge` merges final modules,
    not relocatable objects [inf].

## 4. Linker, proc macros and build scripts on WASI

- **Process spawning:** on `wasm32-wasip1`, spawning a process "will always
  return an error" [src 10]. rustc runs the linker with `std::process`, so
  stock rustc cannot link on WASI. Existing forks call LLD in-process
  [src 4, 8]. Cargo also runs rustc, build scripts and the linker as
  processes, so cargo needs heavier patching; rubrc did this partly [src 9].
- **Build scripts:** compiled and then executed as programs. Our guest graph
  has none, so we can skip them [inf, from the repo].
- **Proc macros:** rustc loads them as host dylibs. WASI has no dynamic
  loading, so stock rustc.wasm cannot run any proc macro; rubrc lists them
  as unsupported [src 9]. A web search summary mentioned a rubrc-based
  build with a "wasm proc-macro patch"; we could not find its page, so treat
  that as unverified. dtolnay's `watt` shows proc macros can themselves
  be wasm [inf]. Purgatorio options, cheapest first:
  1. Replace `#[purgatorio::main]` with a `macro_rules!` macro such as
     `purgatorio::main!(main);`. The guest graph then has no proc macros at
     all [inf]. This changes every command's source, so do it as its own
     change, built and hash-checked on the hosts first.
  2. Link `purgatorio-macros` into the rustc.wasm build as a built-in, the
     way rustc has built-in derives [inf]. Needs a rustc patch.
  3. Run proc macros as reactor modules through `purgatorio.load` and
     `call`, which already exist for `/bin/lib` [inf]. Needs a rustc patch
     and a stable token-stream encoding. Most general, most work.
- **rustc without cargo:** yes. `cargo build -v` prints each rustc command
  line. Those can be written into a build file once on the host, with paths
  rewritten [inf]. Useful flags, all stable: `--crate-type`, `--crate-name`,
  `--edition`, `--extern name=path`, `-L dependency=dir`, `--emit=link`,
  `--emit=obj`, `-C metadata=`, `-C lto`, `-C codegen-units=1`,
  `-C panic=abort`, `-C strip=symbols`, `-C linker=`, `--remap-path-prefix`,
  `--sysroot`, `--print link-args`. No `-Z` flag is needed for the plain
  build [inf]. Avoid `-C incremental` (hard links and file locks) [inf].
- **Linking options:**
  1. In-process LLD inside rustc.wasm, as in the forks [src 4, 8]. Simplest
     to drive; rustc.wasm gets bigger.
  2. `--emit=obj` with fat LTO, then a separate `wasm-ld` guest with the
     arguments rustc prints on the host with `--print link-args`, sequenced
     by sh or `mk` [inf]. No rustc linker patch, but the link line,
     including `crt1-command.o` and the bundled `libc.a`, must be reproduced
     exactly for byte identity.
  3. A small rustc patch: on WASI, run `-C linker=` through
     `purgatorio.spawn_prog` and wait on `/prog/<pid>/wait` [inf]. This keeps
     `wasm-ld` a separate `/bin` command and needs no change to std. It ties
     the fork to the `purgatorio` module, so prefer 1 or 2 unless rustc.wasm
     size becomes a problem.
- **Sequencing:** `mk` (in PG-40) or mash's built-in make (PG-42) can run the
  generated build file. Until then an `sh` script in dependency order works:
  the order is fixed and short [inf].

## 5. Resources and the file system

Checked against `docs/abi.md`, `crates/purg-vm*` and `crates/purg-emu`.

- **Module size:** 31 to 80 MB for rustc.wasm, 25 to 144 MB of sysroot
  depending on contents [src 6, 8]. Our sysroot only needs the
  `wasm32-wasip1` rlibs and the self-contained objects from `rust-std`, not
  `rust-src` [inf]. The loader reads the whole module into memory; fine
  hosted, but it matters for the kernel's initrd [inf].
- **Compile cost:** wasmtime compiles a module with Cranelift on first use.
  For an 80 MB module expect tens of seconds or more [inf]. `purg-vm-wasmtime`
  keeps an in-memory cache keyed by the module bytes, emptied when full, so a
  build that runs rustc 50 times compiles it once per emu run. A persistent
  cache (wasmtime's `cache` feature, off by default [src 20], or a `.cwasm`
  precompiled by xtask as plan Section 7.2 step 5 already suggests) is needed
  for good start-up [inf].
- **Memory:** wasm32 caps a guest at 4 GiB. Our crates are small and std
  comes prebuilt, but `lto = true` makes each command's final rustc run a fat
  LTO over std's bitcode. A few hundred MB per run is likely, well under
  4 GiB [inf, to be measured]. Large crates (wasmtime, cranelift, deep `syn`
  users) are where 4 GiB could bind. wasmtime reserves 4 GiB of address space
  per memory by default [src 20], so many concurrent rustc progs on the
  hosted emu are fine on 64-bit hosts. A wasm64 rustc is far off: it would
  need rustc built for `wasm64-purgatorio` with a Rust std over the
  `purgatorio` module (`docs/wasm64.md`), and LLVM built for it.
- **Threads:** existing builds need wasi-threads. Our wasmtime is built with
  `default-features = false` and features `cranelift, runtime, std`, so the
  `threads` feature (shared memory) is off [inf, from `Cargo.toml`; src 20].
  wasmi has no threads (plan 6.1). `build-root`'s import check would reject
  `wasi.thread-spawn`, and D4 defers threads. Either build rustc
  single-threaded (preferred) or allow wasi-threads on wasmtime only, as a
  spike-only exception decided under D4.
- **Stack:** rustc recurses deeply. wasmtime's default `max_wasm_stack` is
  512 KiB [src 20]; `purg-vm-wasmtime` uses `Config::new()` unchanged. rustc
  normally runs with an 8 MiB thread stack (`RUST_MIN_STACK`, [src 14]); on
  wasm that is the shadow stack in linear memory, set when rustc.wasm is
  linked, plus wasmtime's native stack for wasm frames [inf]. The emu runs
  each prog on a 64 MiB kproc stack (`KPROC_STACK`), so raising
  `max_wasm_stack` to 16 MiB or so for this prog is safe hosted [inf]. The
  wasmi recursion limit also needs checking and raising [inf].
- **Renames: the first hard blocker.** rustc builds an `.rlib` in a
  temporary directory created *inside* the output directory
  (`*.temp-archive/tmp.a`) and then calls `fs::rename` into the output
  directory [src 12]. `--emit=metadata` writes `full.rmeta` in an `rmeta*`
  subdirectory and moves it with `rename` too, with no copy fallback
  [src 13]. Purgatorio's `path_rename` maps rename to a wstat of the name and
  returns `EXDEV` when the parent directories differ (abi.md;
  `crates/purg-vm/src/host.rs`). So every library build fails. Options:
  (a) make WASI `path_rename` copy and remove when the directories differ on
  the same file server, not atomic, keeping Inferno's wstat semantics for
  `purgatorio.wstat`; (b) patch rustc to fall back to copy. (a) also helps
  other stock WASI programs [inf]. This is an ABI decision for the user.
- **Hard links:** rustc's `link_or_copy` falls back to `fs::copy` when
  `hard_link` fails [src 21]. Our `path_link` returns `NOTSUP`, so this works
  [inf].
- **mmap:** rustc reads files fully into a `Vec` instead of mapping them on
  `wasm32` [src 22]. No mmap needed, at the cost of memory.
- **Temp files:** created in the output directory, not `TMPDIR`, for rlibs,
  rmeta and the link step [src 12, 13]. Our `#U`-backed or RAM trees handle
  `O_CREAT|O_EXCL` and directory create and remove (abi.md, M2) [inf].
- **Environment:** `RUST_MIN_STACK` and `RUSTC_BOOTSTRAP` are read from the
  environment [src 14]. `environ_get` gives the prog's `/env` at start
  (abi.md), so `env` and sh variables reach rustc [inf].
- **Time and randomness:** `clock_time_get` and `random_get` exist (abi.md).
  rustc's output does not depend on time [inf].
- **Paths:** paths baked into panic messages must match the host build. The
  host remaps the workspace to `/purgatorio`; inside Purgatorio the same
  `--remap-path-prefix=<where the source is>=/purgatorio` gives the same
  strings [inf]. std's own paths are already fixed in the shipped rlibs.

## 6. Speed

- **wasmtime (hosted):** Cranelift JIT. The browser numbers (V8): 1 to 3 s
  for a small program at `opt-level=0` [src 8], and one rubrc workflow went
  from 61 s to 12 s after optimisation [src 9]. Our builds are `opt-level=s`
  with fat LTO, so expect several seconds to tens of seconds per command and
  a few minutes for the whole tree on the hosted emu [inf, to be measured].
- **wasmi (kernel today, and `--vm=wasmi` hosted):** an interpreter, about 5
  to 20 times slower than JIT (plan 6.1). That puts a full build in tens of
  minutes to hours, and a rustc start also pays wasmi's translation of an
  80 MB module [inf]. Usable for a correctness check, not for daily work.
- **AOT on the kernel (K4, D6):** wasmtime `no_std` running a `.cwasm`
  precompiled on the host would bring kernel speed close to hosted wasmtime
  [inf]. Pulley, wasmtime's interpreter, would sit between the two [inf].
  So self-hosting on the kernel waits for D6.

## Staged plan

Efforts are rough, for one person familiar with the repo.

**Stage A. wasm tools as guest commands (about 1 week, no download).**
`wasm` commands built on `wasmparser`, `wasmprinter`, `wat` and
`wasm-encoder`: validate, print, parse, strip, and a dump of imports,
exports and the ABI section. They prove that larger third-party Rust crates
build as guests, give us inspection tools inside Purgatorio, and cover part of
PG-41. Add a `sha256sum` beside `md5sum` for the hash check.

**Stage B. Spike: an existing rustc.wasm under the hosted emu (2 to 3 weeks,
needs downloads).**
1. Run a downloaded rustc.wasm and sysroot with the stock wasmtime CLI on the
   host to get a baseline (memory, time, flags needed).
2. Emu changes, each behind a flag or in its own commit: raise
   `max_wasm_stack` for large modules; WASI `path_rename` copy fallback (or
   decide on a rustc patch); a persistent compiled-module cache. If the
   build needs wasi-threads, enable wasmtime's `threads` feature and a
   `wasi.thread-spawn` host function on wasmtime only, as a spike exception
   to D4.
3. Inside the emu, compile `hello.rs` for `wasm32-wasip1` with explicit
   flags and `--sysroot=/lib/rust`, no cargo. Link in-process if the build
   has LLD, else `--emit=obj` and a separate `wasm-ld` guest.
4. Build `purg-abi`, then `cat` with its full graph, from a script.
Exit: a guest built in Purgatorio runs in Purgatorio. Byte identity is not
expected yet, since the compiler version differs from 1.99.1.

**Stage C. Sysroot and build file (about 1 week).**
- `/lib/rust/lib/rustlib/wasm32-wasip1/lib/` holds the `rust-std` rlibs and
  `self-contained/` objects for exactly the pinned version, copied from the
  host's rustup component. They are target artifacts, so the host's copy is
  the right copy [inf].
- `/lib/rust/bin/rustc.wasm` (and `wasm-ld.wasm` if separate). sh resolves
  commands in `/bin`, so add `/bin/rustc.wasm` as a bind or a small wrapper.
- `cargo xtask selfhost-mkfile` turns `cargo build -v` output into a build
  file for `mk` (PG-40) or an sh script, with the same `-C metadata` and
  remap flags `xtask` uses.
- Remove the proc macro from the guest graph (option 1 in section 4) as a
  separate change, hash-checked on the CI hosts first.

**Stage D. Our own rustc for WASI (3 to 6 weeks, high risk).**
- Build Rust 1.99.1 with the LLVM backend for `wasm32-wasip1`,
  single-threaded with wasi-sdk 26 or later, carrying bjorn3's patches
  rebased (main thread, sysroot, in-process LLD), plus `wasm-ld` from the same
  `src/llvm-project`. Done on a big Linux host or CI runner (about 32 GB of
  RAM is reported for building [src 6]), not in Purgatorio.
- If single-threaded fails, fall back to `wasm32-wasip1-threads` and
  revisit D4, wasmtime only.
- Pin the build like the guests: reproducible, checked into CI as an
  artifact, not into the repo.

**Stage E. Byte-identical proof (about 1 week).**
Inside the hosted emu, build the whole tree into `/tmp/root/bin`, then
compare with the host-built `/bin` using `cmp` or the hashes. Add it as a
CI job on one Linux runner under wasmtime. Since the three hosts already
agree, matching one of them is enough. This is the acceptance test for
PG-43.

**Later.**
- Threads (D4) only if a single-threaded rustc proves too slow.
- wasm64 for crates that outgrow 4 GiB, after the wasm64 ABI exists.
- The kernel after K4 (D6) gives it AOT; with wasmi only it is a slow
  correctness check.
- Cargo inside Purgatorio is not a goal. The generated build file is enough
  for our tree.

## Artifacts a hands-on spike would download

None of these were fetched for this study. Each needs approval first.

| Artifact | Source | License | Used in |
|---|---|---|---|
| rustc.wasm with LLVM and LLD, plus abort sysroot, release v3.0.0 (rustc 1.83-dev) | github.com/oligamiq/rust_wasm releases, mirrored at oligamiq.github.io/rust_wasm | Rust: MIT OR Apache-2.0; LLVM: Apache-2.0 WITH LLVM-exception. The repo states no license of its own [src 7] | B |
| or rustc.wasm without linker, weekly build | github.com/Mearman/wasm-rustc releases | Same upstream licenses; repo license not stated [src 6] | B (alternative) |
| clang and `wasm-ld` as wasm | npm `@yowasp/clang` or PyPI `yowasp-clang` | Apache-2.0 (package), LLVM's license inside [src 16, 17] | B, if the rustc build has no LLD |
| wasmtime CLI, current release | github.com/bytecodealliance/wasmtime releases | Apache-2.0 WITH LLVM-exception | B step 1 baseline |
| Rust 1.99.1 source tarball (`rustc-1.99.1-src.tar.xz`, includes `src/llvm-project`) | static.rust-lang.org | MIT OR Apache-2.0; LLVM Apache-2.0 WITH LLVM-exception | D |
| bjorn3's patch branch, latest `compile_rustc_for_wasm*` | github.com/bjorn3/rust | MIT OR Apache-2.0 | D |
| wasi-sdk 26 or later, Linux host build | github.com/WebAssembly/wasi-sdk releases | Apache-2.0 WITH LLVM-exception (and libc licenses: MIT, BSD) | D |
| `wasm-tools` crates | crates.io (normal cargo dependencies) | Apache-2.0 WITH LLVM-exception | A |

The `rust-std` component for `wasm32-wasip1` 1.99.1 is already installed by
`rust-toolchain.toml` and needs no new download.

## Open questions

1. **Rename semantics.** May WASI `path_rename` copy and remove across
   directories (not atomic), or should we patch rustc instead? Inferno's own
   wstat rename stays as it is either way.
2. **Threads.** Is a wasmtime-only wasi-threads exception acceptable for the
   spike if a single-threaded rustc is not ready, or is D4 strict?
3. **Proc macro.** Is replacing `#[purgatorio::main]` with a `macro_rules!`
   form acceptable? It changes every command's source once.
4. **Linker.** In-process LLD in the rustc fork, or a separate `wasm-ld`
   guest, sequenced by `mk` or reached through `spawn_prog`?
5. **Fork upkeep.** Who rebuilds rustc.wasm when `rust-toolchain.toml` moves?
   Byte identity only holds when the pins match exactly.
6. **Where builds run.** Should the rustc.wasm build live in this repo's CI
   (hours, large runner), or come from an external project we pin by hash?
7. **Memory.** Real peak memory of a fat-LTO guest link under wasm is
   unknown until measured. Does it fit beside the per-prog memory limits
   planned for K4?
8. **Module cache.** Persistent cache for wasmtime in `#U`, or precompiled
   `.cwasm` from xtask? The kernel will want the second anyway.

## Sources

All read on 2026-11-09.

1. rustc target spec, `wasm32_wasip1_threads.rs` (`host_tools: Some(false)`):
   https://doc.rust-lang.org/stable/nightly-rustc/src/rustc_target/spec/targets/wasm32_wasip1_threads.rs.html
2. rust-lang/rust PR #122170, rename to `wasm32-wasip1-threads` (merged
   2024-03-12): https://github.com/rust-lang/rust/pull/122170
3. rust-lang/rust PR #130899, "Couple of changes to make it easier to
   compile rustc for wasm" (merged 2024-10-07):
   https://github.com/rust-lang/rust/pull/130899
4. rust-lang/miri issue #722, running Miri and rustc in wasm (2019 to 2026):
   https://github.com/rust-lang/miri/issues/722
5. bjorn3/wasm-rustc: https://github.com/bjorn3/wasm-rustc
6. Mearman/wasm-rustc: https://github.com/Mearman/wasm-rustc
7. oligamiq/rust_wasm and its releases:
   https://github.com/oligamiq/rust_wasm,
   https://github.com/oligamiq/rust_wasm/releases
8. rntz-llm/rust-wasm-serve: https://github.com/rntz-llm/rust-wasm-serve
9. oligamiq/rubrc: https://github.com/oligamiq/rubrc
10. The rustc book, `wasm32-wasip1`:
    https://doc.rust-lang.org/rustc/platform-support/wasm32-wasip1.html
11. The rustc book, `wasm32-wasip1-threads`:
    https://doc.rust-lang.org/rustc/platform-support/wasm32-wasip1-threads.html
12. rustc `compiler/rustc_codegen_ssa/src/back/archive.rs` (rlib temp dir and
    rename):
    https://github.com/rust-lang/rust/blob/main/compiler/rustc_codegen_ssa/src/back/archive.rs
13. rustc `compiler/rustc_metadata/src/fs.rs` (rmeta temp dir and rename):
    https://github.com/rust-lang/rust/blob/main/compiler/rustc_metadata/src/fs.rs
14. rustc `compiler/rustc_interface/src/util.rs` (compiler thread, stack
    size, backend loading):
    https://github.com/rust-lang/rust/blob/main/compiler/rustc_interface/src/util.rs
15. LLVM Discourse, "[RFC] Building LLVM for WebAssembly" (2024-05-19 to
    2025-09-08), pages 1 and 3:
    https://discourse.llvm.org/t/rfc-building-llvm-for-webassembly/79073
16. `@yowasp/clang` on npm, via Socket: https://socket.dev/npm/package/@yowasp/clang
    and the clang 21.1.4 run reported at
    https://discourse.llvm.org/t/rfc-building-llvm-for-webassembly/79073/63
17. YoWASP/clang (archived 2026-03-11): https://github.com/YoWASP/clang
18. wasi-sdk releases: https://github.com/WebAssembly/wasi-sdk/releases
19. rustc_codegen_cranelift README, platform support:
    https://github.com/rust-lang/rustc_codegen_cranelift
20. wasmtime `Config` docs (`max_wasm_stack`, `memory_reservation`,
    `threads`, `cache`): https://docs.rs/wasmtime/latest/wasmtime/struct.Config.html
21. rustc `compiler/rustc_fs_util/src/lib.rs` (`link_or_copy`):
    https://github.com/rust-lang/rust/blob/main/compiler/rustc_fs_util/src/lib.rs
22. rustc `compiler/rustc_data_structures/src/memmap.rs` (wasm32 reads files
    into memory):
    https://github.com/rust-lang/rust/blob/main/compiler/rustc_data_structures/src/memmap.rs
