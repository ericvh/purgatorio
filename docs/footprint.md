# Memory footprint

Inferno's native kernels ran in about 1 MiB of RAM. This document records
where Purgatorio's memory goes, what was done about it in the first pass
of issue #66, what a small-memory configuration would look like, and
whether Arm M-class hardware (no MMU) is within reach.

All numbers are from this tree on 2026-11-10, on an Apple Silicon host
(QEMU 8.2.9, KVM and TCG), with the kernel on QEMU `virt`. "Before" is
`main` at `e56c3b6`; "after" is this branch. Where `main` cannot report a
number itself (it has no heap counters), "before" is this branch with the
change in question turned off, which Section 4 spells out.

## 1. Summary

| | Before | After |
|---|---|---|
| Kernel `.text` | 1,902 KiB | 1,425 KiB |
| Kernel `.text`, smallest build (`kernel-small` profile, no TLS, auth, net, 9p) | - | 769 KiB |
| `.rodata`, nearly all of it the compiled-in root | 9,057 KiB | 9,045 KiB |
| `.data` + `.bss` + boot stack | 408 KiB | 149 KiB |
| Image file | 10.71 MiB | 10.23 MiB |
| Kernel heap after boot, before any prog | 2,134 KiB | 342 KiB |
| Heap at an idle `sh` prompt | 9,790 KiB | 2,666 KiB |
| Heap peak during `ls /bin \| wc -l` | 17,634 KiB | 5,054 KiB |
| Heap in use at the end of the test mode | 265.1 MiB | 27.9 MiB |
| Peak kernel threads, test mode / idle `sh` | 21 / 3 | 21 / 3 |
| Kernel thread stack: size / deepest use | 2,048 / 22 KiB | 256 / 22 KiB |
| Service kproc stack: size / deepest use | 2,048 / 6 KiB | 64 / 6 KiB |
| Boot stack: size / deepest use | 256 / 5 KiB | 64 / 5 KiB |
| Initial linear memory of a `std` guest | 1,088 KiB | 320 KiB |
| RAM for one `echo` (thread, memory, module) | about 3.3 MiB | about 0.8 MiB |
| Smallest `-m`: boots to `sh` and runs a pipeline | 512M (required) | 19M |
| Smallest `-m`: the full test mode passes | 512M (required) | 56M |
| `cargo xtask kernel`, one TCG run / the KVM run | 50 s / 12.3 s | 28 s / 11.2 s |

What changed, in order of effect:

1. **wasmi modules are cached** (Section 4). The engine never frees a
   module's code, so every run of a command leaked a copy of it. This was
   the 265 MiB.
2. **Kernel thread stacks went from 2 MiB to 256 KiB**, and 64 KiB for the
   mount readers, from measured depths (Section 3).
3. **Guests are linked with a 256 KiB shadow stack** instead of the
   linker's 1 MiB (Section 6).
4. **The heap and page tables are sized from the DTB**, and the heap uses
   the RAM on both sides of the DTB and the initrd (Section 5).
5. **The kernel is built for size except for the interpreter**
   (`opt-level = "z"`, fat LTO, wasmi at 3): a quarter less code at the
   same guest speed. A `kernel-small` profile builds the interpreter for
   size too (Section 2).
6. **`auth`, `tls`, `net` and `p9` are cargo features** of the kernel, on
   by default; a build without them is 169 KiB smaller (Section 2).
7. The boot stack and the boot page-table pool were cut to what they use
   with room to spare, and a prog no longer keeps a copy of its module's
   file while it runs.

What was only measured: the `no_std` guest profile and multi-call
binaries (Section 6), the other build settings (Section 2), and the
engine alternatives (Section 7).

## 2. The image

`cargo xtask size` prints this report. `cargo xtask size --small` builds
the smallest kernel first (the `kernel-small` profile, without the
optional features), `cargo xtask size --guests` lists every module, and
`cargo xtask size --elf FILE` reports on a kernel built earlier.

```
kernel: target/aarch64-unknown-none/kernel/purg-kernel
  section         bytes        KiB
  .text         1458660     1424.5
  .rodata       9262104     9045.0
  .data            4336        4.2
  .bss            82560       80.6
  .stack          65536       64.0
  loaded       10725100    10473.7
  in RAM       10873196    10618.4
  code by crate (1424.5 KiB of text):
  wasmi                           729.6  51.2%
  core + alloc                    197.3  13.9%
  wasmparser                      161.5  11.3%
  purg_core                        97.3   6.8%
  purg_kernel + purg_aarch64       66.4   4.7%
  (other)                          47.5   3.3%
  purg_vm + purg_vm_wasmi          21.3   1.5%
  embedded_tls                     20.5   1.4%
  smoltcp                          20.4   1.4%
  curve25519_dalek                 10.3   0.7%
  purg_styx                         8.6   0.6%
  sha2                              7.3   0.5%
  (crates under 4 KiB)             28.1   2.0%
  (no symbol: asm, padding)         8.2   0.6%
guests: 103 modules, 8690.0 KiB; smallest 2.3 KiB, median 75.3 KiB, largest 305.7 KiB
guests: initial linear memory: 103 at 320 KiB
  /bin/echo.wasm     62781 bytes (code 50646, data 11214), memory 320 KiB
  /bin/sh.wasm      313056 bytes (code 276442, data 34463), memory 320 KiB
  /bin/calc.wasm    239132 bytes (code 181438, data 55999), memory 320 KiB
```

The root is most of the image: 103 modules, 8.5 MiB, in a ustar archive
that the RAM root borrows from without copying. The engine (wasmi with
its parser and validator, `wasmparser`) is the largest part of the code:
63% of it now, 54% on `main`.

The same split on `main`, for comparison (KiB):

| Crate | `main` | Now | Small build |
|---|---|---|---|
| wasmi | 765 | 730 | 317 |
| wasmparser | 269 | 162 | 123 |
| core + alloc | 257 | 197 | 125 |
| purg_core | 215 | 97 | 72 |
| purg_kernel + purg_aarch64 | 101 | 66 | 28 |
| purg_vm + purg_vm_wasmi | 54 | 21 | 19 |
| smoltcp | 45 | 20 | - |
| embedded_tls | 41 | 21 | - |
| curve25519_dalek | 21 | 10 | - |
| sha2 | 19 | 7 | 2 |
| **`.text`** | **1,902** | **1,425** | **769** |

### Build settings

Measured on this branch, with everything else the same. The time is one
TCG run of `cargo xtask kernel` (a Cortex-A72 with GICv3) and the KVM
run; the stack is the deepest any prog's kernel thread went in the test
mode.

| `[profile.kernel]` | `.text` KiB | TCG run | KVM run | CPU-bound guest | Deepest stack |
|---|---|---|---|---|---|
| `opt-level = 3` (as on `main`) | 1,913 | 21.7 s | 11.1 s | 2.32 s | 22 KiB |
| 3, fat LTO, 1 codegen unit | 1,951 | 17.5 s | 11.0 s | 2.32 s | 23 KiB |
| `"s"` | 1,637 | fails | fails | - | over 2 MiB |
| `"s"`, fat LTO, 1 codegen unit | 1,278 | 23.2 s | 11.9 s | - | 131 KiB |
| `"z"` | 1,238 | 61.5 s | 12.3 s | 7.14 s | 52 KiB |
| `"z"`, fat LTO, 1 codegen unit (**`kernel-small`**) | 937 | 46.6 s | 11.8 s | 3.45 s | 22 KiB |
| wasmi at 3, the rest `"z"` | 1,612 | 29.5 s | 11.4 s | 2.31 s | 22 KiB |
| wasmi at 3, the rest `"z"`, fat LTO, 1 codegen unit (**adopted**) | 1,425 | 27.6 s | 11.6 s | 2.33 s | 21 KiB |

The CPU-bound guest is `zeros 1024 16384 | sha1sum` under KVM (16 MiB
hashed through a pipe), timed with `time`. The KVM run of the test mode
is mostly waiting, so it says little about speed; the TCG run
exaggerates differences in the kernel's own code. The machine was shared
while these ran, so the times are good to about ten percent.

What the table shows:

- **Size and speed pull apart only in the interpreter.** Building
  everything but wasmi for size costs nothing a guest can measure. Building
  wasmi for size too halves the code again but a CPU-bound guest takes
  1.5 times as long (3.1 times without LTO).
- **Fat LTO only pays together with `"z"`.** At `opt-level` 3 it makes
  the kernel slightly larger.
- **`"s"` is not usable.** wasmi runs each instruction's handler and
  tail-calls the next; it relies on LLVM making those calls jumps (Section
  7.1). At `"s"` without LTO LLVM does not, and a prog's kernel stack grew
  past 2 MiB in the test mode; with LTO it reached 131 KiB. At 3 and at
  `"z"` the stack stays shallow. Nothing guarantees that across compiler
  versions, which is why every context switch now checks the stack.

**Adopted for `[profile.kernel]`:** `opt-level = "z"`, `lto = "fat"`,
`codegen-units = 1`, with the four wasmi crates at `opt-level = 3`. The
kernel's text goes from 1,902 KiB on `main` to 1,425 KiB with the same
guest speed, and every test passes on TCG and KVM. A TCG run of the test
mode takes about 28 s instead of 22 s with the old profile on this branch
(and instead of about 50 s on `main`, before the module cache).

**`[profile.kernel-small]`** is the same with wasmi at `"z"`: 937 KiB with
every feature, 769 KiB without them, for a board where code size matters
more than guest speed. The probe's profile is unchanged.

### Features

`purg-kernel` has four features, all on by default, so the default
build is unchanged:

| Feature | Leaves out when off | Code |
|---|---|---|
| `tls` | `#D` (`embedded-tls`, `p256`, AES-GCM) | 61 KiB |
| `auth` | Ed25519 mount authentication and `#¤` (`ed25519-dalek`, `curve25519-dalek`, SHA-512) | 28 KiB |
| `net` | virtio-net and smoltcp; `#I` stays and answers `network unreachable` | 51 KiB |
| `p9` | the virtio-9p transport, `#9` and the host directory on `/n/host` | 30 KiB |

`cargo build -p purg-kernel --target aarch64-unknown-none --profile kernel
--no-default-features` builds a kernel without them: 1,256 KiB of text
instead of 1,425, and 769 KiB with the `kernel-small` profile as well. It
boots to `sh` and runs pipelines (checked under KVM with 24 MiB); the
test mode needs all four features. The features are `purg-core`'s `auth`
and `tls` and `purg-aarch64`'s `net` and `p9`. The workspace depends on
both crates without default features, and the emu and the probe name the
features they use, so that the kernel can leave them out. Without `auth`
an export that asks for authentication accepts nobody and `mount` can
only attach unauthenticated.

`#C` and the 9P2000.u shim (`purg_styx::dotu`, used by `#9` only) were
not looked at separately.

## 3. Kernel RAM

The kernel's allocator now counts the bytes in use, their high-water
mark, and the allocations and frees. `/dev/memory` reports them in
Inferno's format, one line per pool:

```
; cat /dev/memory
    3950616   522833920     3956464        8105        5680           1   518883304 main
```

The columns are the bytes in use, the pool's size, the high-water mark,
the allocations, the frees, the times the pool grew, and the largest
block that could be allocated (here the free bytes: a first-fit list does
not track its largest hole). The emu reports its resident size and the
host's memory as one `main` pool in the same format.

With `memstat` in the bootargs (`cargo xtask kernel --memstat`, or
`-append memstat` interactively) the kernel prints `kernel: mem: ...`
lines: the RAM and the image's sections after the heap is up; what each
test run cost (the heap's peak above where it stood); once a second, when
it changed, the heap and thread count; and at the end the totals, the
peak thread count and the deepest stacks. `xtask kernel` skips these
lines when it compares the output, so a `--memstat` run is still a test.

### Stacks

Each kernel thread's stack is filled with a pattern when it is made. How
far the pattern has been overwritten is the deepest the thread went.
Across the whole test mode:

| Stack | Was | Deepest use | Now |
|---|---|---|---|
| A thread that runs a prog (wasmi and the host calls under it) | 2 MiB | 22 KiB | 256 KiB |
| A service kproc (the mount driver's reader) | 2 MiB | 6 KiB | 64 KiB |
| The boot stack (in the image) | 256 KiB | 5 KiB | 64 KiB |

wasmi keeps the guest's call stack and value stack on the heap, so deep
recursion in a guest (`depth.sh`) does not deepen the kernel thread's
stack. The depth does depend on how the kernel is compiled: see "Build
settings" above. Thread stacks are heap blocks with no guard page, so
every context switch now checks that the lowest word of the outgoing
stack still holds the pattern and panics with `kernel thread stack
overflow` if it does not. That check is what caught the `opt-level = "s"`
overflow. The boot stack has a real guard page.

A 2 MiB stack was also cleared on every `spawn`, which is where part of
the test-time improvement comes from.

### Static memory

`.bss` held 128 KiB of boot page tables; the map takes 8 pages, so the
pool is now 16 pages (64 KiB). The runtime tables, rebuilt on the heap
once it is up, were a fixed 256 KiB pool; a first build now counts the
pages and the tables that stay take exactly those (32 KiB with 512 MiB of
RAM). The boot tables and the boot stack are still not given back after
start-up: about 130 KiB that a later change could reclaim.

## 4. What a prog costs

Three things, per running prog:

1. **Its kernel thread's stack**: 256 KiB (was 2 MiB).
2. **Its linear memory**: 320 KiB to start with for every Rust `std`
   guest (was 1,088 KiB), of which 256 KiB is the shadow stack (Section 6).
3. **Its module in the engine.** wasmi translates WebAssembly to its own
   register code, function by function, the first time each function is
   called, and keeps the original function bodies until then.

`cargo run --release -p purg-vm-wasmi --example modsize -- FILE...`
measures the third on the host, with a counting allocator: the heap
wasmi holds after loading a module, and after translating all of it.

| Module | File | Held after loading | Fully translated | Ratio to the file |
|---|---|---|---|---|
| `echo` | 62,781 | 95,015 | 270,711 | 4.3x |
| `cat` | 67,245 | 100,181 | 286,436 | 4.3x |
| `ls` | 88,520 | 141,379 | 425,110 | 4.8x |
| `ed` | 129,838 | 184,128 | 544,121 | 4.2x |
| `sh` | 313,056 | 426,473 | 1,294,404 | 4.1x |
| `calc` | 239,132 | 339,499 | 916,747 | 3.8x |
| `no_std` `echo` (Section 6) | 1,398 | 6,241 | 18,803 | 13x |

So a module costs about one and a half times its size in RAM as soon as
it is loaded, and up to four and a half times as its functions are used.
On the kernel a single `echo hello world` leaves 191 KiB behind in the
engine; `sh` can reach 1.3 MiB.

### The engine never frees code

wasmi's engine stores every function in an append-only map that is only
freed when the engine is dropped (`wasmi-2.0.0/src/engine/code_map`).
Dropping a `Module` frees nothing. The kernel made a new `Module` for
every prog, so each run of a command added its code to the heap for good:
after the test mode the heap held 265 MiB, almost all of it dead
copies of modules.

`purg-vm-wasmi` now caches modules by the SHA-256 of their bytes. A
command is parsed, validated and translated once, and every prog running
it shares that copy. The digest is the same one module signing (#33)
will check, so the two can share it. The cost is that a module, once
run, stays in the engine: the bound is one copy of each distinct module
run since boot, which for the test mode (it runs nearly every command)
is still most of the 27.9 MiB left at the end. Giving that back needs
an engine that can free a module, or a second engine to move to when the
first has grown (Section 10).

A prog also held the bytes of its module file for as long as it ran (62
to 313 KiB). The engine now takes ownership of them and frees them once
the module is compiled or found in the cache.

### Measured per run

From `cargo xtask kernel --kvm --memstat`: the heap's peak above its level
before the run, for runs on an existing thread (add the thread's stack
for a spawned prog).

| Run | Before | After |
|---|---|---|
| `echo hello world` | 1,300 KiB | 532 KiB |
| `cat /lib/probe/hello.txt` | 1,270 KiB | 502 KiB |
| `/bin/test/abi.wasm` | 3,490 KiB | 1,218 KiB |
| `/bin/test/procs.wasm` (spawns commands) | 7,621 KiB | 1,910 KiB |
| `/bin/test/m3.wasm` (servers, mounts, exports) | 18,085 KiB | 3,105 KiB |
| `sh depth.sh` | 10,955 KiB | 2,066 KiB |
| `sh pipes.sh` | 15,612 KiB | 2,643 KiB |
| `sh mk.sh` (the largest before) | 41,353 KiB | 4,400 KiB |
| `sh admin.sh` (the largest now) | 36,544 KiB | 7,413 KiB |

"Before" is this branch with 2 MiB kernel stacks, no module cache and
1 MiB guest stacks (those guests are bit for bit `main`'s), since `main`
cannot report these numbers itself.

## 5. Booting with less RAM

The boot page tables are built before the MMU is on, from a fixed map,
and the kernel then insisted on finding exactly 512 MiB. Now:

- The boot map still covers a fixed 512 MiB window (nothing touches the
  part that is not there), but the kernel takes the machine's RAM from
  the DTB, requires only that it starts where the image is linked and
  holds the image, and builds the runtime tables over what the DTB
  reports.
- The heap is every free stretch of that RAM after the image, up to four
  of them. QEMU puts the initrd and the DTB in the middle of a small
  machine's RAM, and a heap of only the largest stretch lost half of it.
- With more than 512 MiB the heap grows into the rest once the runtime
  tables map it (checked with `-m 1G`).
- The heap self-test's 64 MiB block is now a quarter of the largest
  stretch when that is smaller.

`cargo xtask kernel --mem SIZE` runs the test mode with that `-m`.

| | Smallest `-m` | What stops it below |
|---|---|---|
| Boots to `sh` and runs `ls /bin \| wc -l` | 19M | At 19M the two-stage pipeline runs but a four-stage one does not (20M); at 18M a command fails to get its memory and then the kernel panics on an allocation; at 17M it panics on one while booting |
| The full test mode passes (KVM, root from an initrd) | 56M | At 52M an allocation fails in the kernel during the corpus. The test mode ends with 27.9 MiB in use: a copy of nearly every module in the engine, and the servers the scripts leave running |

The image is 10.4 MiB of that (the root is in RAM, and with an initrd it
is there twice), and QEMU loads the image 2 MiB above the start of RAM.
Running out of memory is clean where wasmi allocates a linear memory
(`bad executable: failed to instantiate memory`), but a failed
allocation anywhere else in the kernel is a panic.

## 6. Guests

### The shadow stack

A Rust guest's compiled code keeps part of its stack in linear memory:
the shadow stack. The linker's default is 1 MiB, placed below the data,
which is why every `std` guest started at 1,088 KiB. `xtask build-root`
now passes `-z stack-size`; the modules are otherwise unchanged.

The sh corpus on both VMs, with every guest built at each size:

| Shadow stack | Initial memory | Corpus scripts that fail (of 28, both VMs) |
|---|---|---|
| 1 MiB (the linker's default) | 1,088 KiB | none |
| 256 KiB (**adopted**) | 320 KiB | none; `cargo test --workspace`, the probe and the kernel also pass |
| 128 KiB | 192 KiB | none |
| 64 KiB | 128 KiB | `archive` (`gzip` traps), `depth` (`sh` traps at 100 nested blocks) |
| 16 KiB | 64 KiB (one module 128 KiB) | the same two |

So nearly everything the corpus runs fits in 16 KiB of shadow stack.
The two exceptions need between 64 and 128 KiB: `gzip`'s compressor (not
looked into; probably large tables in one stack frame), and `sh`, which
recurses once per nested block, with a limit of 100.

**Adopted: 256 KiB for every guest**, at least twice what anything in the
corpus needs. The flag is a constant, so the modules stay byte-identical
across build hosts (with the linker's own default spelled out,
`-z stack-size=1048576`, the modules came out bit for bit the same as
before, which confirms the flag changes nothing else). Running out of
shadow stack is a trap (`out of bounds memory access`), not corruption,
because the stack is below the data.

**Proposed next: per-crate sizes.** A crate can ask for its own size from
a build script (`cargo:rustc-link-arg-bins=-zstack-size=N`, for wasm
targets only); it wins over the default, and Cargo tracks it. That was
tried with `sh` and works. With a default of 64 KiB and 256 KiB for `sh`
and the commands that compress (`gzip` and the `tar` family), a typical
command would start at 128 KiB. It was not adopted here because the
corpus does not exercise deep recursion in most commands (`du`, `ls` and
`rm` on deep trees, the regular expression compiler in `grep` and `sed`,
`calc`), and nothing measures a guest's real stack depth yet. That
measurement (a stack-pointer low-water mark, read when a prog exits)
should come first.

### A `no_std` profile

`appl/small` is an experiment, a workspace of its own outside the system:
`echo`, `cat` and `sleep` for `wasm32-unknown-unknown` without `std`
(plan Section 6.4's minimal target), over a 235-line runtime
(`appl/small/rt`): the entry point and ABI section, the arguments, read,
write, open, close, errstr, exits and sleep over the same imports the
`std` guests end up calling, a bump allocator that never frees, and a
panic handler. They were checked by hand against the real commands on
both VMs (`echo` with and without `-n`, `cat` of files, of a missing file
and of standard input, `sleep` with a fraction): the same output, error
strings and exit statuses, except that `sleep` reports a bad argument
more tersely. `cargo xtask small` builds and compares them:

| Command | `std`: file | memory | held by wasmi | `no_std`: file | memory | held by wasmi |
|---|---|---|---|---|---|---|
| `echo` | 62,781 | 320 KiB | 95 to 271 KiB | 1,398 | 64 KiB | 6 to 19 KiB |
| `cat` | 67,245 | 320 KiB | 100 to 286 KiB | 1,536 | 64 KiB | 7 to 17 KiB |
| `sleep` | 81,655 | 320 KiB | - | 1,518 | 64 KiB | - |

A `no_std` command is about forty times smaller as a file, uses one
64 KiB page (the smallest a WebAssembly memory can be), and costs the
engine under 20 KiB. The whole of Rust `std`, its formatting and its
allocator are what the other 60 KiB of every small command are.

What the SDK would need to support this properly:

- a `no_std` feature of `sdk/purgatorio`: the `sys` layer as it is (it is
  already raw imports), `Fd` with `core`-only read and write traits, and
  `Error` over a fixed buffer or `alloc::string::String`;
- an entry macro that does not need `std::env::args` (the runtime above
  reads them itself);
- a choice of allocator: none (bump only, as here), or a small free-list
  allocator for commands that need `Vec` and `String`;
- formatting: `core::fmt` is a large part of what a small `std` command
  carries, so the small commands should avoid it, as these do;
- `xtask build-root` building some crates for `wasm32-unknown-unknown`
  and checking them the same way (the import check already passes);
- tests: the corpus should run against both builds of a command.

The commands worth porting first are the ones a shell session runs most
and that do little: `echo cat ls cp rm mkdir mv date sleep wc pwd
basename tee touch`.

### Multi-call binaries

Every command links its own copy of `std` and the SDK. Two experiments,
also in `appl/small`:

- **`std` commands.** `multi-std` builds sixteen small commands
  (`basename cat cleanname cmp cp date echo ls mkdir mv pwd rm sleep tee
  touch wc`) from their own sources into one module that picks the
  command from the name it is run as. It is 150,332 bytes; the sixteen
  separate modules are 1,144,473 bytes: 7.6 times less. It runs on both
  VMs. wasmi holds 221 KiB for it when loaded and 696 KiB fully
  translated, against about 1.7 and 4.9 MiB for the sixteen (from the
  ratios in Section 4). Applied to the whole root, where about 60 KiB of
  every command is the same `std` and SDK code, the 8.5 MiB would be 2 to
  3 MiB (an estimate).
- **`no_std` commands.** `multi` holds the three `no_std` commands in
  3,782 bytes, against 4,452 as separate modules: there is almost nothing
  left to share.

A multi-call module also shares one translated copy in the engine
between all the commands in it, and its functions are translated only
when used, so the RAM saving follows the file saving. The price is that
the commands are no longer separate files that can be replaced one at a
time, and `sh`'s lookup or the root's layout has to map the names to the
one module (bind or copy the module under each name; the cache makes
copies free in the engine, since it goes by content). The same saving is
available without merging commands if the SDK and `std` could be one
shared reactor module, but WebAssembly has no standard dynamic linking,
so that is a larger project.

`wasm-opt -Oz` was not tried: it is not installed, and it would have to
be pinned to keep modules byte-identical across hosts.

## 7. Engine alternatives (a paper study)

Nothing in this section was built or run. Every claim comes from the
projects' own documentation, read on 2026-11-10 through a tool that
summarises pages, so a quoted figure is worth checking at its source
before a decision rests on it. **V** marks a claim read from the cited
page; **I** marks an inference of ours. Numbers in brackets refer to the
source list at the end of the section.

What our guests need from an engine: the WebAssembly MVP plus what Rust
emits for `wasm32-wasip1` by default (mutable globals, sign extension,
non-trapping float-to-int, bulk memory, multi-value, reference types); no
threads, SIMD, GC or component model. What the kernel needs: `no_std` +
`alloc`, a way to stop a running guest every so often (wasmi's fuel today,
for `kill` and time slicing), and host calls that can block.

### 7.1 Trimming wasmi (2.0.0, released 2026-09-01 [1])

- **Skip validation on the device.** We build wasmi with its `validate`
  feature. Without it, `Module::new` is gone and modules load with
  `unsafe Module::new_unchecked`, which "does not validate the Wasm
  bytecode"; the docs say turning `validate` off lets users "with control
  over their inputs ... slim down Wasmi's binary size significantly", and
  the changelog puts the saving at "~200-300kB" (V [2][3][4][6]). The
  feature turns on `wasmparser/validate`, so what goes is the validator;
  the parser stays (I). An invalid module is then undefined behaviour in
  the kernel, so this is only sound with modules validated on the host and
  signed (#33): the loader checks a signature instead of validating.
  For scale, `wasmparser` is 162 KiB of our kernel's text as built now
  and 269 KiB at `opt-level` 3 (Section 2).
- **A more compact translated form.** The `indirect-dispatch` feature
  selects "a more compact IR encoding at the cost of execution
  performance"; the wasmi 2.0 announcement puts indirect threading at
  "roughly 10-15% slower" with "significantly less memory for its IR"
  (V [2][4][5]). This is the lever on the translated-code RAM measured in
  Section 4. Its effect on code size is not documented.
- **Dispatch and optimisation level.** We use `auto-dispatch`: wasmi
  runs each instruction's handler and tail-calls the next one where it
  can, and falls back to a loop otherwise; `portable-dispatch` forces the
  loop (V [4]). The tail calls are not guaranteed by the language:
  wasmi's build script says the dispatch "relies upon" LLVM's sibling-call
  optimisation and turns it on for every optimising build on a target
  known to support it (V, `wasmi-2.0.0/build.rs`). Where LLVM does not
  make a handler's call a jump, the kernel stack grows with the guest's
  instructions. That is what Section 2 measured: 22 KiB of stack at
  `opt-level` 3 and with `z` under LTO, but more than 2 MiB with `s`. Any
  change of optimisation level needs the stack depth measured again, and
  a build that must be safe by construction should use `portable-dispatch`
  and pay for the loop.
- **Other flags.** `simd` and `memory64` add code and are already off;
  `extra-checks` adds checks and about 20% overhead and is off;
  `prefer-btree-collections` avoids hash maps (V [3][4][5]).
- **What wasmi cannot do.** It always translates to its own IR, held in
  RAM; there is no API to serialise translated code or to load a
  precompiled module (not found in [3][6]; V that `Module` shows none).
  The translated code is shared by the instances of a module (V [5]),
  which our module cache now uses. The engine never frees a module's code
  (Section 4; our reading of `wasmi-2.0.0/src/engine/code_map`, which
  describes its storage as append-only).
- Licence Apache-2.0 or MIT; maintained by Robin Freyler (wasmi-labs);
  active (V [1][5]). The announcement claims 2.0 is "~2.2x faster than
  Wasmi 1.0" and level with wasm3 and Stitch (V [5]).

### 7.2 wasmtime without `std`

- `no_std` embedding is supported at Tier 2. Only the runtime builds:
  there is no Cranelift or Winch on the device, so modules must be
  precompiled to `.cwasm` on the host (V [8][9]). The embedder supplies an
  allocator, a panic handler and one pointer of thread-local storage;
  virtual memory, signals and fibers are optional, and without signal-based
  traps it falls back to explicit checks (V [7][11]). WASI is "not
  supported on custom targets", so we would keep our own WASI p1 layer,
  which is what we have (V [7], I).
- **Native AOT** exists only for 64-bit targets: Cranelift supports
  "x86_64, aarch64, s390x, and riscv64" and "No 32-bit platform is
  currently supported" (V [8][9]). So it is an option for the aarch64
  kernel (the D6 question, #27) and not for Cortex-M (I).
- **Pulley** is wasmtime's portable interpreter, Tier 2, with 32- and
  64-bit variants; the `.cwasm` is Pulley bytecode for one of them. The
  docs say to "expect a ~10x order-of-magnitude slowdown relative to
  native code" (V [9][12]). On Cortex-M this is the only wasmtime route
  (I). No Thumb target is in the tier list (V [9]).
- **Size.** The published figures are for the C API on x86-64 Linux,
  dated 2024-12: 2.1 MiB with no default features, 1.2 MiB with LTO,
  698 KiB with `build-std` and immediate-abort panics, and "400K" for the
  custom-platform minimal build; the Wasefire project is quoted as running
  wasmtime with Pulley in "256k RAM and ~300k flash" (V [7]). So the
  runtime alone is not smaller than wasmi with validation removed (I).
- **RAM.** `Module::deserialize_raw` "will not create a copy of the
  provided memory" (V [13b]), so precompiled code can in principle stay
  where the loader put it. Whether a Pulley `.cwasm` runs straight from
  flash under `no_std` is not documented (not found). A 2026 thread about
  wasmtime on a Pico 2 names 64 KiB pages and the copy made when a memory
  grows as the main RAM problem without an MMU (V [15]); that applies to
  any engine, ours included.
- **Stopping a guest.** Fuel and epoch interruption exist and show no
  cargo feature gate; their availability under `no_std` is not stated
  (I from [14][8]).
- Licence Apache-2.0 WITH LLVM-exception; monthly releases (50.0.1 on
  2026-10-29) (V [10][12b]).

### 7.3 Small interpreters that run the module in place

- **Wasefire's interpreter** (`wasefire-interpreter` 0.5.0, Google,
  Apache-2.0) (V [16][16b]). "A module is just a byte slice holding a
  WebAssembly module in binary format": it interprets the bytes where
  they are, with a side table that a host-side `prepare()` step validates
  and attaches as a custom section (V [16][16c]), so the validator stays
  off the device and the module can stay in flash (I). The project's FAQ
  says it "fits in 22kB when optimized for size and 66kB when optimized
  for speed" and is "designed to have minimal RAM overhead"; it rejected
  wasmi because it "consumes too much RAM for embedded" and wasm3 because
  it "only compiles to RAM (not flash)" (V [13]). `no_std`. Host calls
  return to the embedder and are resumed (`RunResult::Host`), which suits
  blocking host calls (V [16]). Not found: fuel or any other way to
  interrupt a running guest; which proposals beyond the MVP it supports
  (an `Unsupported` error exists); any benchmark. The FAQ says "the main
  concern is not performance" (V [13]).
- **DLR-FT `wasm-interpreter`** (Apache-2.0 or MIT): in-place
  interpretation, `core` + `alloc` only, WebAssembly 2.0, fuel with
  resumption, host calls that hand control back; described as work in
  progress, with no size or speed figures (V [23]). On paper the closest
  in-place engine to our needs (I).
- **tinywasm** (0.11.0, 2026-09-19, MIT or Apache-2.0): safe Rust,
  `no_std`, passes the WebAssembly 3.0 core suite, fuel with
  `resume_with_fuel`, "smaller than 300 KB" when built to load only its
  precompiled archive format (V [21][22]). It translates to its own format
  in RAM (I), so it does not remove the translated copy.
- Stitch is fast but "does not run on 32-bit platforms" (V [24]); wain is
  MVP-only and experimental (V [25]).
- The design these follow is Titzer's "A fast in-place interpreter for
  WebAssembly" (arXiv 2205.01183), which reports in-place interpretation
  "on-par with interpreting a custom-designed internal format" (V [20]).

### 7.4 C engines, as reference points only

- **wasm3** (MIT): "~64Kb for code and ~10Kb RAM"; it compiles to
  threaded code in RAM. The README still says the project "will enter a
  minimal maintenance phase", but there are releases and commits through
  2026-09, and it now lists gas metering and suspend/resume
  (V [17][18][19]).
- **WAMR** (Bytecode Alliance, Apache-2.0 WITH LLVM-exception, 2.4.5 on
  2026-06-29). Text sizes on Cortex-M4F: fast interpreter about 58.9 K,
  classic interpreter 56.3 K, AOT runtime 29.4 K. The classic interpreter
  runs the module's bytes; the fast one is about twice as fast and "consumes
  about 2X memory" for its precompiled code. `wamrc --xip` produces AOT
  code that runs in place from flash, with Thumb targets among the
  examples (V [26][27][28][29]). Instruction metering is interpreter-only
  (V [28]).

### 7.5 Ahead-of-time to native code

wasm2c (C output plus a small runtime; no metering) (V [31]), w2c2 (C89
output, CoreMark "~7% slower than native") (V [32]) and aWsm (LLVM to
ELF objects, Thumb supported, a research prototype) (V [33]) turn modules
into native code at build time. They give up loading a module that
arrives later, and the fuel-based time slicing we rely on (I).

### 7.6 Comparison

| Engine | Code size | Module code in RAM | `no_std` | Interrupt / resumable host calls | Licence | State (2026-11) |
|---|---|---|---|---|---|---|
| wasmi 2.0 (today) | 891 KiB of our kernel with its parser, measured (1,034 KiB at `opt-level` 3); "~200-300kB" less without `validate` (V) | translated copy, 1.5x to 4.4x the module, measured | yes | fuel / yes | Apache-2.0 or MIT | active |
| wasmtime + Pulley | "~300k flash" (Wasefire's figure), 400 K to 698 K minimal builds on x86-64 (V) | `.cwasm`; no copy with `deserialize_raw` (V); from flash not documented | Tier 2 | fuel, epochs (I under `no_std`) / through async | Apache-2.0 WITH LLVM-exception | active, monthly |
| wasmtime AOT | the same runtime | native code; 64-bit targets only (V) | Tier 2 | as above | as above | active |
| wasefire-interpreter | "22kB" / "66kB" (V) | none: in place, with a side table | yes | not found / yes | Apache-2.0 | 0.5.0, active |
| DLR-FT wasm-interpreter | not found | none: in place | `core` + `alloc` | fuel / yes | Apache-2.0 or MIT | work in progress |
| tinywasm | "smaller than 300 KB" (V) | translated copy (I) | yes | fuel / not found | MIT or Apache-2.0 | 0.11.0, active |
| wasm3 (C) | "~64Kb" (V) | threaded code | C | gas metering / snapshots | MIT | minimal maintenance, with 2026 commits |
| WAMR (C) | 56 K classic, 59 K fast, 29 K AOT runtime, Cortex-M4F (V) | classic: none; fast: about 2x; AOT: in place with XIP | C | interpreter only / not found | Apache-2.0 WITH LLVM-exception | 2.4.5, active |

Least certain: whether Pulley runs from flash under `no_std`; fuel and
epochs in `no_std` wasmtime; Wasefire's coverage of bulk memory, reference
types and multi-value, and whether a running guest can be interrupted;
the size of anything on Thumb, since the published wasmtime figures are
for x86-64.

### 7.7 What follows for Purgatorio

1. The cheapest step is wasmi without `validate`, once modules are signed
   (#33): about a quarter of the engine's code, by wasmi's own figure.
2. `indirect-dispatch` is worth one measurement: translated size and test
   time, with the stack depth checked.
3. For the aarch64 kernel, wasmtime AOT remains the speed option of D6. It
   does not reduce the footprint: the runtime is no smaller, and a
   `.cwasm` is larger than the module.
4. For an M-class port the translated copy is the problem (Section 4), and
   only an in-place interpreter removes it. Wasefire's is the smallest and
   is built for exactly this; the DLR-FT interpreter documents the fuel
   and resumption we need. Either would be a second backend behind
   `purg-vm`'s `Engine` trait. The first thing to establish, by building
   one, is whether it runs our `std` guests at all, and how much slower.

### Sources

All read 2026-11-10.

1. https://docs.rs/crate/wasmi/latest
2. https://docs.rs/crate/wasmi/latest/features
3. https://raw.githubusercontent.com/wasmi-labs/wasmi/main/CHANGELOG.md
4. https://docs.rs/wasmi/latest/wasmi/
5. https://wasmi-labs.github.io/blog/posts/wasmi-v2.0/ and https://github.com/wasmi-labs/wasmi
6. https://docs.rs/wasmi/latest/wasmi/struct.Module.html
7. https://docs.wasmtime.dev/examples-minimal.html
8. https://docs.wasmtime.dev/stability-platform-support.html
9. https://docs.wasmtime.dev/stability-tiers.html
10. https://github.com/bytecodealliance/wasmtime/releases
11. https://github.com/bytecodealliance/wasmtime/blob/main/examples/min-platform/README.md
12. https://docs.wasmtime.dev/examples-pulley.html
12b. https://docs.rs/pulley-interpreter/latest/pulley_interpreter/
13. https://google.github.io/wasefire/faq.html
13b. https://docs.rs/wasmtime/latest/wasmtime/struct.Module.html
14. https://docs.rs/wasmtime/latest/wasmtime/struct.Config.html
15. https://bytecodealliance.github.io/zulip-archive/stream/438936-SIG-Embedded/topic/Running.20Wasmtime.20on.20Pico.202.3A.20Overcoming.20Memory.20issues.html
16. https://docs.rs/wasefire-interpreter/latest/wasefire_interpreter/
16b. https://github.com/google/wasefire/tree/main/crates/interpreter
16c. https://raw.githubusercontent.com/google/wasefire/main/crates/interpreter/CHANGELOG.md
17. https://github.com/wasm3/wasm3
18. https://github.com/wasm3/wasm3/releases
19. https://github.com/wasm3/wasm3/commits/main
20. https://arxiv.org/abs/2205.01183
21. https://github.com/explodingcamera/tinywasm
22. https://crates.io/crates/tinywasm
23. https://github.com/DLR-FT/wasm-interpreter
24. https://github.com/makepad/stitch
25. https://github.com/rhysd/wain
26. https://github.com/bytecodealliance/wasm-micro-runtime
27. https://github.com/bytecodealliance/wasm-micro-runtime/releases
28. https://github.com/bytecodealliance/wasm-micro-runtime/blob/main/doc/build_wamr.md
29. https://github.com/bytecodealliance/wasm-micro-runtime/blob/main/doc/xip.md
31. https://github.com/WebAssembly/wabt/blob/main/wasm2c/README.md
32. https://github.com/turbolent/w2c2
33. https://github.com/gwsystems/aWsm

## 8. A small-memory profile

The profile is a kernel built small, guests that are small, and stacks
sized from measurement. Three steps, each with an estimated RAM budget
for the kernel, an idle shell and one command running. The first is
measured; the others are sums of measured parts.

**Step 1: this branch, default build (measured).**

| | RAM |
|---|---|
| Kernel `.data`, `.bss` and boot stack | 149 KiB |
| Kernel heap after boot (devices, the RAM root's index, the engine and its host functions) | 342 KiB |
| `emuinit` and `sh -l` at the prompt (two threads, two memories, two modules in the engine) | 2,324 KiB |
| One command (`echo`): thread 256, memory 320, module and store about 210 KiB | about 790 KiB |
| **Total** | **about 3.6 MiB** |

On QEMU the image is in RAM as well (10.4 MiB, 8.8 of it the root).
`ls /bin \| wc -l` peaks at 4.9 MiB of heap.

**Step 2: what is in reach without new subsystems (estimated).** The
`kernel-small` build (guests run at two thirds of the speed); 64 KiB
kernel thread stacks (three times the
deepest measured); per-crate guest stacks, so a command's memory is
128 KiB and `sh`'s 320 KiB; the small commands as one multi-call module;
the boot tables and boot stack reclaimed.

| | RAM |
|---|---|
| Kernel static data | about 90 KiB |
| Kernel heap after boot | 342 KiB |
| `emuinit`: thread 64, memory 128, module about 100 KiB | about 290 KiB |
| `sh`: thread 64, memory 320 plus what it allocates, module 430 KiB growing towards 1.3 MiB as more of it runs | about 1.3 MiB |
| One command from the multi-call module: thread 64, memory 128, the shared module about 230 KiB at first | about 420 KiB |
| **Total** | **about 2.5 MiB** |

**Step 3: `no_std` guests on an engine that runs modules in place
(estimated; needs Sections 6 and 7's larger items).** No translated copy
of any module; small commands in one 64 KiB page with a 32 KiB thread
stack (to be measured for that engine); `sh` still a `std` program.

| | RAM |
|---|---|
| Kernel static data | about 90 KiB |
| Kernel heap after boot (less whatever wasmi's engine took) | up to 342 KiB |
| `emuinit`, `no_std` | about 100 KiB |
| `sh`: thread 64, memory 320 plus what it allocates | about 600 KiB |
| One `no_std` command | about 100 KiB |
| **Total** | **about 1.2 MiB** |

Below 1 MiB the shell is what is left: a smaller shell, or none on the
device.

The recommended profile is Step 2 as the next target, since every part
of it is either done or measured here, and Step 3 as the direction for
small hardware.

## 9. M-class hardware (no MMU)

The design does not depend on an MMU. There is one address space, every
kernel thread is privileged, and isolation between progs comes from
WebAssembly's bounds checks. On the A-class kernel the MMU is on for
caching and device memory, section permissions and the boot stack's
guard page; an M-class MPU can give the last two.

A port needs:

- a sibling of `purg-aarch64` for the core: NVIC instead of the GIC,
  SysTick, a PendSV context switch, a Thumb target
  (`thumbv7em-none-eabihf` or `thumbv8m.main-none-eabihf`). The scheduler,
  the heap, the stack painting and the platform glue in `purg-aarch64`
  are mostly not architecture code and would move to a shared crate;
- 64-bit atomics replaced on ARMv7-M and ARMv8-M (`AtomicU64` is used in
  `purg-core` and the scheduler; `portable-atomic` or a lock);
- no ARMv6-M (Cortex-M0/M0+): it has no compare-and-swap;
- an engine that builds and performs on Thumb (wasmi is portable Rust; its
  tail-call dispatch on Thumb needs checking, see the stack depths in
  Section 2);
- execute-in-place: the kernel's 0.8 to 1.4 MiB of code and the root stay
  in flash. The RAM root already borrows file data from the archive, so a
  root in flash costs RAM only for its index.

What no MMU changes for memory:

- A linear memory must be one contiguous block, and growing it means
  finding a larger block and copying (today's allocator does the same).
  So memories must be small and bounded: a maximum per prog (plan Section
  6.5 has the limit; nothing enforces it yet), and guests that do not
  grow.
- WebAssembly's page is 64 KiB, so 64 KiB is the least a prog's memory
  can be. The custom-page-sizes proposal removes that floor. The linker
  has `--page-size` and wasmi 2.0 has a switch for the proposal
  (`Config::wasm_custom_page_sizes`); neither was tried.
- Fragmentation matters: a first-fit list over 1 MiB with 64 KiB and
  256 KiB blocks coming and going will fail early. Fixed pools for stacks
  and linear memories would be needed.

What board class is realistic at each step:

| Step | RAM needed | Flash needed | Board class |
|---|---|---|---|
| Step 1: today's default build | about 3.6 MiB, plus the image if it is not run in place | 10.5 MiB (kernel and root) | Cortex-M7, M33 or M85 boards with 8 MiB or more of external SDRAM or PSRAM and 16 MiB of flash. Evaluation-board territory. |
| Step 2: small kernel, small stacks, multi-call root | about 2.5 MiB | 3 to 4 MiB | The same cores with 4 MiB or more of external RAM. Few if any microcontrollers have this much on chip. |
| Step 3: `no_std` guests on an in-place interpreter | about 1.2 MiB with a `std` shell, less without one | 2 to 3 MiB | Microcontrollers with 1 to 2 MiB of RAM on chip, with no external memory. This is the first step where a single-chip board is realistic, and it is where Inferno's 1 MiB was. |
| Ordinary microcontrollers (256 KiB of RAM) | - | - | Out of reach with a shell on the device. Possible only as a fixed set of two or three `no_std` progs with pooled memories and custom page sizes, which is a different product. |

The board classes are from general knowledge of the parts, not from
checking data sheets; a port should start by picking one board and
confirming its numbers.

The decision this asks for: **target Step 3 on a part with 1 MiB or more
of RAM on chip, and treat Step 2 as the milestone that proves the port on
an evaluation board with external RAM.** Step 2 needs only work that also
helps the A-class kernel (items 1 to 6 below). Step 3 needs the two large
items: the `no_std` SDK and a second engine.

## 10. Follow-up work

In order of value for the footprint. Effort is rough: S is days, M is a
week or two, L is more.

| # | Work | Why | Effort |
|---|---|---|---|
| 1 | Measure guests' shadow-stack depth (a low-water mark read at exit, reported under `memstat`), then set per-crate sizes | A typical command would start at 128 KiB instead of 320 KiB; the data to do it safely is missing | S |
| 2 | Give memory back from the engine: recycle the wasmi engine when no prog is running, or keep two and retire one, or get module unloading into wasmi | The cache bounds the growth but a long-running system still holds every module it ever ran | M |
| 3 | A multi-call binary for the small `std` commands, with the names bound to it | 7.6x on the files in the experiment; one shared translated copy | M |
| 4 | Per-prog memory limits (a maximum number of pages) and allocation failures that fail the prog, not the kernel | Needed before any small-RAM target; today an allocation failure outside wasmi is a kernel panic (part of #27) | M |
| 5 | wasmi without `validate`, loading only signed modules (#33) | About a quarter of the engine's code, by wasmi's figure | S after #33 |
| 6 | Measure wasmi's `indirect-dispatch` (translated size, speed, stack) and `portable-dispatch` (stack bound by construction) | The translated copy is the largest per-prog cost; the stack depth depends on LLVM | S |
| 7 | A `no_std` mode of the SDK, and the dozen most-used commands ported to it | 40x smaller modules, one page of memory each | M to L |
| 8 | A second engine behind `purg-vm`'s `Engine`: an in-place interpreter (Wasefire's, or DLR-FT's) | The only way to drop the translated copy; decides whether Step 3 is possible | L |
| 9 | Split `purg-aarch64` into architecture code and shared kernel code (scheduler, heap, platform), then a Thumb build of the shared crates and the engine | The first step of an M-class port, and it shows what does not build | M |
| 10 | Reclaim the boot page tables and boot stack after start-up; do not keep the compiled-in root when an initrd replaces it; keep the root in flash on targets that have it | About 130 KiB, and 9 MiB when booting with an initrd | S |
| 11 | Pools for thread stacks and linear memories instead of first-fit | Fragmentation is what fails first in a small heap | M |
| 12 | Audit `purg-core` (97 KiB at `z`, 215 KiB at 3) for formatting and `String` errors on hot paths | The next largest crate after the engine and the standard library | M |
| 13 | Custom page sizes for `no_std` guests (the linker has `--page-size`, wasmi has `wasm_custom_page_sizes`; neither was tried) | A prog's memory below 64 KiB | S to M |
| 14 | Record `cargo xtask size` in CI against a budget, so a pull request that grows the kernel says so | Keeps the gains | S |
| 15 | `wasm-opt -Oz` on guests, pinned so modules stay byte-identical | Not tried here | S |
