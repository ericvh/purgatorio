
# Purgatorio: Design and Phased Roadmap

Purgatorio is a clean-sheet rebuild of Inferno in Rust. The kernel and the hosted emulator are written in Rust. Applications are Rust compiled to WebAssembly (WASM), which replaces Limbo and the Dis VM. The build order is:

1. Hosted emu on Linux and macOS hosts.
2. Native kernel for aarch64, run under QEMU `virt`.
3. Graphics (draw, Tk, wm, prefab) last.

Standing user decision: every milestone is architecturally validated against aarch64, not only the kernel phase. Concretely, each milestone's acceptance criteria include three things:

1. Native runs on Linux aarch64 and macOS arm64.
2. The shared crates build for `aarch64-unknown-none`.
3. A bare-metal aarch64 probe image boots in QEMU and runs the milestone's portable subset (Section 9.1).

This document is the reference design. Section 11 lists the milestones, and Milestone 1 is scoped so a coding worker can start it today. Section 12 lists the decisions the user needs to make.

---

## 1. What we are rebuilding: the Inferno architecture in brief

Based on `github.com/inferno-os/inferno-os`:

| Inferno area | Location | Purgatorio equivalent |
|---|---|---|
| Portable kernel: channels (`Chan`), namespaces (`Pgrp`), file and env groups, queued I/O (`qio`), devices | `emu/port/*.c` (`chan.c`, `pgrp.c`, `qio.c`, `dev*.c`, `sysfile.c`, `devmnt.c`, `exportfs.c`, `styx.c`) | `purg-core` (`no_std` + `alloc`) |
| Host glue: kprocs on pthreads, host fs, sockets, tty | `emu/Linux`, `emu/port/kproc-pthreads.c`, `devfs-posix.c`, `ipif-posix.c` | `purg-emu` (`std`) |
| Native kernels | `os/pc`, `os/omap`, `os/pxa`, … plus `os/port` | `purg-kernel` (aarch64) |
| Dis VM: interpreter, JIT, GC, module loader, builtin modules | `libinterp` (`xec.c`, `comp-*.c`, `load.c`, `gc.c`, `alt.c`, `runt.c`) | WASM runtime behind the `purg-vm` trait |
| Builtin module interfaces | `module/sys.m`, `draw.m`, `tk.m`, `math.m`, `keyring.m`, `loader.m` | WASM host import modules plus the Rust SDK |
| Styx (9P2000) | `include/styx.h`, `lib9`, `appl/lib/styx*.b`, `styxservers` | `purg-styx` (`no_std`) |
| Applications | `appl/cmd/*.b`, `appl/cmd/sh/*`, `appl/lib/*.b`, `appl/wm`, `appl/acme`, `appl/charon` | `appl/` Rust crates built to `.wasm` |
| Graphics | `libdraw`, `libmemdraw`, `libmemlayer`, `libtk`, `libprefab`, `devdraw.c`, `devpointer.c`, `devtk.c` | Phase G (last) |

Inferno concepts we keep:

- **Everything is a file.** Each resource is a file tree served by a device driver (`#c`, `#e`, …) or by a Styx server.
- **Per-process namespaces.** `bind` and `mount` edit a process-group mount table. `pctl(FORKNS|NEWNS|NODEVS…)` controls sharing.
- **One protocol.** Remote and user-level file servers speak 9P2000 (Styx). `#M` turns a file descriptor into a mountable channel. `export` serves a namespace over a file descriptor.
- **`sys->file2chan`.** A user-level program serves a single file through `#s` without writing a full Styx server.
- **Lightweight progs with CSP channels.** These are Limbo `chan of T`, `alt`, and `spawn`. Many progs are scheduled by the VM, and blocking host calls are offloaded to kprocs.
- **Dynamic modules.** `load Module path` loads a `.dis` file. Commands are modules with `init(ctxt, argv)`. Builtin modules such as `$Sys` and `$Draw` are implemented natively.
- **Portable binaries.** The same `.dis` runs on every host and every native kernel. In Purgatorio, the same `.wasm` will play that role.

The full Sys API surface to cover (from `module/sys.m`): announce, aprint, bind, byte2char, char2byte, chdir, create, dial, dirread, dup, export, fauth, fd2path, fildes, file2chan, fprint, fstat, fversion, fwstat, iounit, listen, millisec, mount, open, pctl, pipe, pread, print, pwrite, read, readn, remove, seek, sleep, sprint, stat, stream, tokenize, unmount, utfbytes, werrstr, write, wstat. Formatting helpers (print, sprint, tokenize, utf) move into the SDK and do not need host calls.

---

## 2. Design principles

1. **A portable core with a thin platform layer.** All namespace, channel, device, and Styx logic lives in `no_std + alloc` crates from day one, so the aarch64 kernel reuses it unchanged. The hosted emu is "the kernel on the `std` platform", which is the same relationship `emu/port` has with `os/port`.
2. **Inferno semantics, not POSIX semantics.** We keep file descriptors, `errstr` strings, `Dir` and `Qid`, and the pctl flags. WASI p1 is used wherever it expresses those semantics exactly, and the `purgatorio` extension module covers the rest (Section 6.2).
3. **Abstract the WASM runtime.** The VM sits behind a trait, so hosted can use a JIT and the kernel can use an interpreter or AOT. Either can be swapped later.
4. **Test against real Inferno.** The 9P codec and file servers are checked against real Inferno emu and plan9port tools (Section 9).
5. **Graphics stays out of the core.** Nothing in the early phases depends on draw. `#i` and `#m` are ordinary devices added in Phase G.
6. **aarch64 first-class from day one.** Nothing may assume x86 behavior:
   - **Memory ordering:** aarch64 is weakly ordered. Use explicit `Acquire`/`Release` atomics and no "works on x86" `Relaxed` shortcuts. Lock-free code is model-checked with `loom`.
   - **Page size:** never hard-code 4 KiB. macOS arm64 uses 16 KiB, and Linux aarch64 may use 4, 16, or 64 KiB. Query it at runtime on hosted and from configuration in the kernel.
   - **Alignment:** use unaligned-safe codecs (`from_le_bytes`, no pointer casts) for 9P and guest memory.
   - **No x86-only dependencies or intrinsics** without a portable fallback.
   - **Kernel-shaped seams:** every `purg-core` blocking or locking path goes through `purg-platform`, so the bare-metal probe exercises the same code.

---

## 3. Workspace and crate layout

A single Cargo workspace:

```
purgatorio/
  Cargo.toml                 # workspace; shared lints and profiles
  rust-toolchain.toml        # pinned stable; targets: wasm32-unknown-unknown, wasm32-wasip1, aarch64-unknown-none
  crates/
    purg-styx/               # no_std: 9P2000 message codec, Qid/Dir/stat codec, fid tables,
                             #   server framework (StyxServer trait), client (used by #M)
    purg-core/               # no_std+alloc: Chan, Dev trait, devtab, Pgrp/Mount table (bind/mount/unmount,
                             #   union dirs, MREPL/MBEFORE/MAFTER/MCREATE), Fgrp, Egrp, qio Queue, Rendez,
                             #   Error/errstr, Prog table, portable devices (root, cons logic, env, pipe,
                             #   prog, srv/file2chan, mnt, dup, indir, cap, ssl later), sysfile ops (Sys API impl)
    purg-platform/           # no_std trait definitions: Platform { clock, entropy, spawn_kproc, Rendez impl,
                             #   console io, host fs (optional), net (optional) }; sync primitives facade
    purg-vm/                 # no_std+alloc: runtime-agnostic Prog executor interface, ABI host functions
                             #   (native no_std WASI p1 + `purgatorio` extension module), guest-memory helpers, module loader
    purg-vm-wasmtime/        # std: wasmtime backend (hosted default)
    purg-vm-wasmi/           # no_std: wasmi backend (kernel default; also built for hosted CI parity)
    purg-probe/              # no_std aarch64 bare-metal test image for QEMU virt (PL011 + semihosting exit):
                             #   links purg-core + purg-vm-wasmi, embeds a tiny root, runs milestone scripts.
                             #   Exists from M1; grows into / shares boot code with purg-kernel in Phase K
    purg-emu/                # std binary `emu` (Linux + macOS): src/host/{unix,linux,macos}.rs
                             #   Platform impl (threads, Condvar, std::fs, sockets, termios),
                             #   host devices #U (host fs), #I (ip), #C (cmd), #t (eia), CLI flags like
                             #   Inferno emu (-r root, -c, -d, -g, -p), boots /bin/emuinit.wasm
    purg-kernel/             # no_std aarch64 binary (Phase K): boot, MMU, GIC, timer, PL011, virtio, Platform impl
    purg-abi/                # no_std, shared by host and guest: syscall numbers/names, flag constants
                             #   (OREAD, ORCLOSE, MREPL, FORKNS...), error strings, Dir wire layout
  sdk/
    purgatorio/              # guest SDK crate (`purgatorio` on wasm32): sys bindings, Fd, Dir, typed channels,
                             #   spawn, alt/select, load, file2chan, Styx server helpers, arg, bufio-like io, env
    purgatorio-macros/       # #[purgatorio::main] for the init(ctxt, argv) entry point
  appl/
    cmd/<name>/              # one crate per command (cat, ls, sh, ...) -> <name>.wasm
    lib/<name>/              # shared Rust libraries (replacing appl/lib/*.b): styxservers, readdir, filepat, ...
  root/                      # the Inferno file tree served as / by #U at boot: bin/, lib/, dev/, n/, tmp/,
                             #   usr/, chan/, net/, prog/, env/, mnt/ (empty mount points, like Inferno's root)
  xtask/                     # `cargo xtask build-root`, `run-emu`, `qemu`, `test-styx`, `fmt-check`
  tests/                     # integration tests driving emu with scripts; golden outputs
  docs/
```

Rules:

- `purg-core`, `purg-styx`, `purg-platform`, `purg-vm`, `purg-abi`, and `purg-vm-wasmi` must build for `aarch64-unknown-none` from Milestone 1. CI enforces this with `cargo build -p … --target aarch64-unknown-none`.
- Only `purg-emu` and `purg-vm-wasmtime` may use `std`.
- Inside `purg-emu`, OS-specific code is confined to `src/host/linux.rs` and `src/host/macos.rs`. Shared POSIX code goes in `src/host/unix.rs`, using `rustix`/`libc`/`socket2`. No `cfg(target_os)` is allowed outside `src/host/`.
- Guest crates are built by `xtask` with `--target wasm32-wasip1` (the default; Rust `std` available) or `wasm32-unknown-unknown` (minimal `no_std` apps). Both use the ABI in Section 6.2. Apps are installed as `root/bin/<name>.wasm` (D3 resolved: binaries live in `/bin`).

---

## 4. The emu core: progs, scheduler, channels

### 4.1 Objects (mirroring `emu/port/dat.h`)

- **`Chan`**: an open reference to a file. It holds the device type, device number, `Qid`, offset, mode, a mount-point back-reference for unions, a device-private `aux`, and the path name used for `fd2path`. It is reference-counted (`Arc<Chan>`) and closed via `Dev::close` when the last reference drops.
- **`Dev` trait**: `dc` (the device char), `attach`, `walk`, `stat`, `open`, `create`, `read`, `write`, `bread`/`bwrite` (optional), `remove`, `wstat`, `close`. This mirrors Inferno's `Dev` struct. `devgen` helpers produce static directory trees.
- **`Pgrp`**: the namespace. It is a mount table and `nodevs`. Each entry holds the mounted-on `Chan` and an ordered `Mount` list for union directories. Holding the Chan keeps the file referenced (for a `#M` file, its fid) while the mount lasts, so a file server cannot forget the name or give its qid to another file. A Chan matches an entry when its (type, dev, qid.path) equals the held Chan's, as in Inferno's `findmount`. Shared via `Arc<RwLock<…>>`. Forked on `FORKNS`.
- **`Fgrp`**: the file descriptor table. Shared or forked via `FORKFD`/`NEWFD`.
- **`Egrp`**: environment variables. `#e` serves them. Shared or forked via `FORKENV`/`NEWENV`.
- **`Prog`**: the schedulable unit. It holds a pid, group id (`NEWPGRP`), the three groups, user, errstr, kill/exception state, a reference to its WASM instance, and status for `#p` (`status`, `ctl`, `ns`, `fd`, `wait`). `pctl` operates on it.
- **`Queue`** (qio): bounded byte or message queues with `Rendez`. Used by pipes, cons, mnt, and srv.
- **`Rendez`**: the sleep/wakeup primitive with a condition and interruption. Every blocking point uses it so that `kill` and `ctl kill`/`killgrp` work.

### 4.2 Execution model

Dis runs many progs on one interpreter thread, preempting every N instructions and offloading blocking host calls to kprocs. With WASM we have three options:

| Model | How | Pros | Cons |
|---|---|---|---|
| **A. Thread-per-prog (hosted)** | Each prog runs its instance on its own OS thread. Blocking host calls block that thread on a `Rendez` (Condvar). | Simplest. Blocking syscalls are natural. Works with wasmtime as-is. Like emu's kprocs. | Thousands of progs means thousands of threads (fine on Linux up to about 10k). Not usable in the kernel as-is. |
| **B. Async host calls** | wasmtime `async` support with fiber stacks; host calls return futures; a small executor runs them. | Many progs on few threads. | Fibers need `std`. Async contaminates all of `purg-core`'s blocking paths. |
| **C. Resumable calls (kernel)** | wasmi `call_resumable`: a host function that would block returns a "suspend" signal, the kernel scheduler parks the prog, and resumes it after wakeup. Fuel metering gives preemption. | Pure `no_std`. One kernel stack. Deterministic. Matches Dis's design closely. | Interpreter speed. Resumption design needs care (see below). |

**Decision (recommended):** `purg-core` is written in a **blocking style against `Rendez`**, which keeps it as simple as Inferno's C. The platform decides what blocking means:

- **Hosted:** Model A. `Rendez::sleep` parks the OS thread. The wasmtime epoch interruption callback handles `kill` while the prog is running guest code.
- **Kernel:** Kernel threads with their own small stacks (like `os/port` procs). Each prog runs on a kernel thread, and `Rendez::sleep` context-switches. This is Model A on our own scheduler. It avoids needing Model C's suspend/resume inside every host function and keeps one `purg-core`. wasmi fuel or a timer interrupt checkpoint gives preemption.

So the scheduler is a platform concern. `purg-core` only needs `Platform::spawn_kproc(fn)`, `Rendez`, `Lock`, and `clock`.

### 4.3 Channels (Limbo `chan of T`, `alt`, `spawn`)

Limbo channels carry typed values, including references, between progs that share a heap. WASM instances do not share memory unless we use the threads proposal. Design:

- **Intra-instance concurrency** (the common case for Limbo-style programs: `spawn` workers plus channels inside one app) is handled entirely **in the guest**. The SDK provides a small single-threaded executor (`purgatorio::task::spawn`, typed `Chan<T>`, a `select!`/`alt!` macro) built on Rust `async`. Blocking syscalls inside a task are a problem, so the SDK's async syscalls call a host `poll`-style primitive (Section 6.2, `purgatorio.wait`). This preserves Limbo's programming model at zero kernel cost. To `#p`, a guest task is part of one prog, which is a known divergence: Dis progs were each visible in `/prog`.
- **Inter-prog channels** are **kernel objects**: `chan_new(cap) -> handle`, `chan_send(h, bytes, handles[])`, `chan_recv`, `alt(handles[], ops[])`. Messages are byte buffers plus an optional list of transferred capabilities (fds, chan handles). The SDK wraps them as `KChan<T: Serialize>` using a compact encoding (postcard). They are used by `spawn_prog` (a new instance) and by `file2chan` (Section 5.4).
- **`spawn`** maps to two SDK calls:
  - `task::spawn(fut)` creates a guest task, the cheap and common case.
  - `sys::spawn_prog(module, entry, args, pctl_flags, chans)` creates a new prog running a fresh instance of the same or another module, with namespace and fd groups shared or forked according to the flags. This is what `sh` uses for `cmd &` and pipelines.
- The alternative of WASM threads with shared memory (wasi-threads style, one instance per thread over shared linear memory) would give true shared-heap `spawn`. wasmi lacks threads support, and it complicates the kernel, so it is deferred. See open decision D4.

---

## 5. Namespace, devices, Styx

### 5.1 Name resolution

`namec(path, mode)` follows Inferno's `chan.c`:

1. Pick the start point: `#x…` for devices, `/` for root, otherwise the dot.
2. Walk the elements, crossing mount points at each step via the `Pgrp` mount table. Unions are searched in order. `MCREATE` picks the create target.
3. Handle `..` with Plan 9 / Inferno lexical cleanup (`cleanname`), not with host semantics.

The `nodevs` restriction (`pctl NODEVS`) allows only `#e #s #| #M` and similar. This is implemented as in Inferno.

`bind(new, old, flags)` and `mount(fd, afd, old, flags, aname)` edit the mount table. `mount` wraps the fd in `#M`: it attaches a Styx client session over that fd and returns the attach `Chan`. `unmount` and `fd2path` are included.

### 5.2 Device table and priorities

| Dev | Name | Purpose | Phase |
|---|---|---|---|
| `#/` | root | Static boot directory skeleton (`dev`, `bin`, `n`, `prog`, `net`, `chan`, `env`, `tmp`…) for bind targets | M1 |
| `#c` | cons | `cons`, `consctl` (rawon/rawoff), `null`, `random`, `time`, `user`, `sysname`, `pid`, `drivers`, `sysctl`/`kprint`, `jit`, `memory` | M1 (cons, null, user, sysname, time, pid, drivers); M3 the rest |
| `#U` | fs (host) | Host directory as a tree (`-r` root). Implemented in emu only. The kernel uses a disk fs or 9P-over-virtio instead | M1 (read-only), M2 read/write |
| `#e` | env | Per-Egrp env vars as files | M2 |
| `#\|` | pipe | Bidirectional pipes `data`, `data1` (`sys->pipe`) | M2 |
| `#d` | dup | `/fd` view (`#d/0`, `#d/0ctl`) | M3 |
| `#p` | prog | `/prog/<pid>/{ctl,status,ns,fd,wait,nsgrp,pgrp,dbgctl,heap,exception…}`. For WASM, `heap` and `dbgctl` are replaced by WASM-appropriate debug files | M3 |
| `#s` | srv | Inferno's `#s` is the **file2chan registry**, not Plan 9's fd-posting srv: `sys->file2chan(dir, name)` makes a file whose reads and writes become messages on channels to the serving prog | M3 |
| `#M` | mnt | Styx client multiplexer: tags, flush, `msize`/`iounit`, a reader kproc per mount | M3 |
| `#*` | indir | Indirection (`#*dev/path`) | later |
| `#C` | cmd | Run host commands (`os` command, `/cmd/clone`…) | M4 (hosted only) |
| `#I` | ip | `/net/tcp`, `/net/udp`, `clone`, `ctl`, `data`, `listen`, `local`, `remote`, `status`; `/net/cs` via `cs` app; `dial`/`announce`/`listen` in SDK as in Inferno (pure file ops) | M4 |
| `#D` | ssl | TLS/SSL device. Replaced by a `rustls`-backed device or user-level TLS | M6 |
| `#¤` | cap | Capability device for `factotum`/`auth` | M6 |
| `#k` | ds | Disk stripes | later |
| `#P` | prof | Profiling | later (WASM-specific) |
| `#t` | eia | Serial ports | Phase K (PL011) and hosted optional |
| `#F`, logfs | tinyfs, logfs | Flash filesystems | Phase K optional |
| `#%` | mem | Memory stats | M3 (simple) |
| `#^` | snarf | Clipboard | Phase G |
| `#i`, `#m`, `#τ` | draw, pointer, tk | Graphics | Phase G |
| `#A` | audio | Audio | after G, optional |

Dynamically loaded devices (`devdynld`) are dropped. File servers in WASM cover that need.

### 5.3 Styx / 9P2000

`purg-styx` provides:

- **Codec:** all T and R messages (version, auth, attach, error, flush, walk, open, create, read, write, clunk, remove, stat, wstat) with `msize` enforcement, `Dir` and `Qid` codecs, and `NOTAG`/`NOFID` handling. It works over `&[u8]` with zero-copy reads. It is `no_std`.
- **Server framework:** `StyxServer` trait with `attach`, `walk`, `open`, …, plus a fid table, an outstanding-request table (flush), and a `Navigator`-style helper equivalent to `appl/lib/styxservers.b`. It is used in two places:
  1. In the kernel by `export` (`sys->export(fd, dir, flags)`, the `exportfs.c` equivalent), which serves a namespace subtree over an fd.
  2. Compiled into guests via the SDK (`purgatorio::styx`), so user-level file servers (memfs, ramfile, styxlisten targets) are ordinary WASM programs.
- **Client:** used by `#M` (the multiplexer in the kernel) and available in the SDK for apps that want to speak 9P directly.
- **Compatibility:** 9P2000 only. Version string `"9P2000"`. Inferno's Styx is 9P2000. No 9P2000.u or .L at first (open decision D8). One exception, at the edge: `purg_styx::dotu` translates a 9P2000 conversation to 9P2000.u for a server that speaks nothing else (QEMU's virtio-9p answers `Tversion "9P2000"` with `"unknown"`). The kernel's `#9` transport uses it, so `#M` above it still speaks 9P2000.
- **Auth:** M5 implements `fauth` and the `Tauth` path. Inferno's signed-certificate auth (`keyring`, `auth`, `login`) is replaced by a modern scheme (open decision D9).

### 5.4 file2chan

`file2chan(dir, name)` creates a file under `#s`. The serving prog receives `FileIO` messages:

- `read: chan of (off, count, fid, Rread)`
- `write: chan of (off, data, fid, Rwrite)`

Here `Rread` and `Rwrite` are reply channels. In Purgatorio they become kernel channels (Section 4.3). The SDK presents `FileIO` with `recv_read()` returning `(off, count, fid, Reply)`. This is easy and valuable: `sh` builtins, `plumber`, and many small servers rely on it.

---

## 6. WASM runtime and application ABI

### 6.1 Runtime choice

| Runtime | Lang | Exec | `no_std` | Notes |
|---|---|---|---|---|
| **wasmtime** | Rust | Cranelift JIT/AOT; Pulley interpreter | Partial: recent wasmtime supports `no_std` embedding of the runtime with custom platform hooks, and running **precompiled** modules (Cranelift itself is `std`). Pulley allows interpretation without native codegen | Best performance, mature, epoch interruption, fuel, component model, async fibers. Large dependency tree and binary. `no_std` support is real but newer, so verify on aarch64-none before relying on it |
| **wasmi** | Rust | Interpreter (register-based, fast for an interpreter) | Yes, a first-class goal | Small, pure Rust, fuel metering, **resumable calls**, good spec conformance. No threads proposal. Roughly 5–20x slower than JIT. Ideal for the kernel and for deterministic tests |
| **wasm3** | C | Interpreter (fast) | C, needs libc-ish shims | Fast interpreter, but C/FFI, unsafe surface, effectively in maintenance mode. Contradicts "written in Rust". Not recommended |
| wamr / wasmer | C / Rust | Interpreter/AOT/JIT | wamr C-only for `no_std` | Not better than the two above for our goals |

**Recommendation:**

- **Hosted emu:** wasmtime (Cranelift) by default, with a `--vm=wasmi` flag. CI runs the app test suite on both backends, which guarantees the guest ABI never depends on one runtime.
- **Kernel:** wasmi first because it is proven `no_std`. Later, evaluate wasmtime `no_std` with AOT-precompiled `.cwasm` produced on the host (`xtask` precompiles `root/bin/*.wasm` for aarch64) or Pulley, for speed. This is the analogue of Inferno's JIT (`comp-arm.c`) being optional.
- Hide both behind the `purg-vm` traits: `Engine`, `Module` (compile/validate, cache by qid/version), `Instance` (call entry, read/write memory), `HostCtx` (the Prog). Host functions are written once against a `GuestMemory` abstraction and registered with each backend by a small adapter macro.

### 6.2 The application ABI: WASI preview1 as the base, plus the `purgatorio` extension module

**Decision (resolved, D2):** WASI is used wherever it can express Inferno semantics exactly. Where it cannot, Purgatorio adds its own import module rather than bending the semantics. Every app imports two modules:

1. **`wasi_snapshot_preview1`** (WASI p1). It covers everything that maps cleanly onto the Inferno Sys API. With it, `wasm32-wasip1` Rust `std` (`std::fs`, `std::io`, `println!`, `std::env::args`, `std::time`) works out of the box, and third-party WASI p1 CLI tools run unmodified. `purg-vm` implements WASI p1 **natively over the Purgatorio namespace**, with no `wasi-common` or `wasmtime-wasi` dependency, in `no_std`. So the same implementation runs in the emu, the probe, and the kernel.
2. **`purgatorio`**, the extension module. It covers what WASI p1 lacks or gets semantically wrong: namespaces, Inferno open/create modes, full `Dir`/`Qid`, string errors and exit status, pipes, dup, kernel channels, progs, Styx plumbing, and module loading.

Why p1 and not p2 or p3? WASI p2 and later require the Component Model, which wasmi and `no_std` embeddings do not support today. p1 is plain core-wasm imports. Revisit when component support reaches `no_std` (D5).

#### Shared conventions

- Fds are one table: the prog's `Fgrp`. WASI fds and `purgatorio` fds are the same integers, and fds 0, 1, and 2 are the standard streams.
- **Preopens:** fd 3 is preopened as `/` (the prog's namespace root). fd 4 is preopened as `.`, a *live* handle that always resolves to the prog's current dot, so `purgatorio.chdir` takes effect for relative paths opened by wasi-libc and Rust std. `path_open(dirfd, path)` resolves `path` with `namec` relative to the `Chan` behind `dirfd`, so bind and mount unions apply. Absolute paths are routed by wasi-libc to the `/` preopen.
- **Errors:** WASI calls return an errno. The host also records the **exact Inferno error string** (`"file does not exist"`, `"permission denied"`, `"interrupted"`, `"i/o on hungup channel"`, …) in the prog's errstr. `purgatorio.errstr` fetches it, and the SDK's `Error` always carries the string. A fixed table maps strings to errnos (ENOENT, EACCES, EINTR, EPIPE, EEXIST, ENOTDIR, EISDIR, EBADF, EFAULT, EINVAL, ENOSPC, and EIO as the fallback). Unknown strings (for example from a remote Styx server) map to EIO, and the string is preserved. Plain Rust std code therefore gets usable `io::ErrorKind`s, and Inferno-aware code gets the exact text.
- `purgatorio` calls return `-1` on error and set errstr, as Inferno does. Pointers are `u32` offsets into the exported `memory`. Strings are `(ptr, len)` UTF-8.
- Seek whence values are identical in both worlds: Inferno `SEEKSTART/SEEKRELA/SEEKEND` = WASI `set/cur/end` = 0/1/2.

#### Mapping of `Sys` (`module/sys.m`) onto the ABI

| Sys call | WASI p1 | Fit | Purgatorio extension (when needed) |
|---|---|---|---|
| `read`, `write` | `fd_read`, `fd_write` | **Clean** | none |
| `pread`, `pwrite` | `fd_pread`, `fd_pwrite` | **Clean** | none |
| `seek` | `fd_seek` | **Clean** (same whence values) | none |
| close (Limbo closes on last reference) | `fd_close` | **Clean** | none |
| `fildes` | (fds are ints) | **Clean** | none (SDK no-op) |
| `sleep` | `poll_oneoff` with a clock subscription | **Clean** | none |
| `millisec` | `clock_time_get(MONOTONIC)` | **Clean** | none |
| `#c/random` (keyring/random use) | `random_get` | **Clean** | none |
| argv to `init` | `args_get`, `args_sizes_get` | **Clean** | none |
| environment (`env` module / `#e`) | `environ_get` (snapshot at start) | **Clean** for reading at startup | Live changes go through `/env/*` files; no syscall needed |
| `remove` | `path_unlink_file`, `path_remove_directory` | **Clean** (host dispatches both to one `remove`) | none |
| `create` of a directory (`DMDIR`) | `path_create_directory` | **Clean** for the common case | `purgatorio.create` when perm bits matter |
| `open` | `path_open` | **Partial**: rights and oflags cover `OREAD/OWRITE/ORDWR/OTRUNC`. WASI has no `ORCLOSE`, and `OEXCL` (Inferno: exclusive use) is not the same as `O_EXCL` | `purgatorio.open(path, mode)` with Inferno mode bits |
| `create` | `path_open(O_CREAT\|O_TRUNC)` | **Partial**: no permission bits, `DMDIR`/`DMAPPEND`/`DMEXCL`/`DMTMP`, or `ORCLOSE` | `purgatorio.create(path, mode, perm)` |
| `stat`, `fstat` | `path_filestat_get`, `fd_filestat_get` | **Partial**: WASI has ino, size, times, and file type. It lacks name, uid, gid, muid, permission mode, `qid.vers`/`qid.type`, and `Dir.type`/`Dir.dev`. The host fills WASI `ino` = `qid.path` | `purgatorio.stat`/`fstat` → full `Dir` in 9P stat wire format |
| `wstat`, `fwstat` | `fd_filestat_set_size`, `*_set_times`, `path_rename`, `fd_sync` (= wstat with a null Dir) | **Partial**: length, mtime, and sync map. A rename maps only when it stays in the same directory (Inferno renames only within a directory, so cross-directory WASI renames fail with EXDEV). Mode, uid, and gid have no WASI equivalent | `purgatorio.wstat`/`fwstat` with a full `Dir` (null fields = "don't change") |
| `dirread` | `fd_readdir` | **Partial**: entries carry only name, ino, and type | `purgatorio.dirread` → concatenated full `Dir`s |
| `chdir` | none (wasi-libc emulates cwd in userspace) | **Missing** | `purgatorio.chdir` (updates the dot behind the live `.` preopen) |
| `fd2path` | none | **Missing** | `purgatorio.fd2path` |
| `dup(old, new)` | `fd_renumber` moves the fd (and closes `old`), and there is no "dup to lowest free" | **Mismatch** | `purgatorio.dup(old, new)` (`new = -1` means lowest free) |
| `pipe` | none | **Missing** | `purgatorio.pipe` (`#\|`) |
| `iounit` | none | **Missing** | `purgatorio.iounit` |
| `bind`, `mount`, `unmount` | none | **Missing** | `purgatorio.bind`/`mount`/`unmount` |
| `pctl` | none | **Missing** | `purgatorio.pctl(flags, movefd list)` |
| `export` | none | **Missing** | `purgatorio.export` |
| `fversion`, `fauth` | none | **Missing** | `purgatorio.fversion`/`fauth` |
| `file2chan` | none | **Missing** | `purgatorio.file2chan` (returns channel handles; Section 5.4) |
| `stream` | none | **Missing** (an SDK loop works, but loses kernel zero-copy) | `purgatorio.stream` |
| `werrstr` / `%r` | errno only | **Mismatch** (numeric, lossy) | `purgatorio.errstr`/`werrstr` (exact strings) |
| exit / `raise` status | `proc_exit(i32)` | **Partial**: Inferno status is a string | `purgatorio.exits(str)`. `proc_exit(0)` = `""` and `proc_exit(n)` = `"exit n"` |
| `dial`, `announce`, `listen` | `sock_*` (p1 sockets are accept/recv/send only, with no connect or bind) | **Not used** | Pure SDK file operations on `/net` (`#I`), as in Inferno's `dial.b`. No syscalls |
| Limbo `spawn` | `wasi-threads` `thread-spawn` (shared memory) | **Not used** (see D4) | In-instance async tasks (SDK), plus `purgatorio.spawn_prog` for new progs |
| `chan of T`, `alt`, `<-` | none | **Missing** | `purgatorio.chan_new`/`chan_send`/`chan_recv`/`chan_close`/`alt` |
| fd readiness for the async executor | `poll_oneoff` (fd read/write and clock) | **Partial**: cannot wait on kernel channels | `purgatorio.wait(ops)` (fds, channels, and timeout in one wait) |
| `load` (Limbo module loading) | none | **Missing** | `purgatorio.load`/`call` (Section 6.3) |
| `print`, `fprint`, `sprint`, `aprint`, `tokenize`, `byte2char`, `char2byte`, `utfbytes`, `readn` | n/a | Library functions | Rust SDK / `std` (no host call) |

Other WASI p1 functions implemented for `std` compatibility:

- Implemented directly: `fd_fdstat_get`/`set_flags` (append only), `fd_prestat_get`/`dir_name`, `fd_sync`/`fd_datasync` (null-Dir wstat), and `sched_yield`.
- Return `ENOTSUP`: `fd_advise`, `fd_allocate`, `path_link`, `path_symlink`, `path_readlink`, and `proc_raise`. Inferno has no links, and `#U` follows host symlinks transparently.

#### `purgatorio` import module (v0)

```
# namespace and Inferno-exact file ops
open(path, plen, mode) -> fd                 create(path, plen, mode, perm) -> fd
stat(path, plen, buf, blen) -> n             fstat(fd, buf, blen) -> n        # 9P stat wire format
wstat(path, plen, dir, dlen) -> 0            fwstat(fd, dir, dlen) -> 0
dirread(fd, buf, blen) -> n                  fd2path(fd, buf, blen) -> n
chdir(path, plen) -> 0                       dup(old, new) -> fd
pipe(fds_ptr) -> 0                           iounit(fd) -> n
bind(name, nlen, old, olen, flags) -> 0      mount(fd, afd, old, olen, flags, aname, alen) -> 0
unmount(name, nlen, old, olen) -> 0          export(fd, dir, dlen, flags) -> 0
fversion(fd, msize, ver, vlen) -> n          fauth(fd, aname, alen) -> fd
file2chan(dir, dlen, name, nlen, handles_ptr) -> 0     # read/write request channel handles
stream(src, dst, bufsiz) -> n
# errors and process
errstr(buf, blen) -> n                       werrstr(ptr, len)
exits(status, slen) -> !                     pctl(flags, movefd_ptr, n) -> pid
spawn_prog(path, plen, argv_ptr, argv_len, flags, fdmap_ptr, n) -> pid
# kernel channels (Section 4.3)
chan_new(cap) -> h      chan_close(h)
chan_send(h, buf, n, handles_ptr, nh) -> 0   chan_recv(h, buf, blen, handles_ptr, maxh) -> n
alt(ops_ptr, nops, block) -> index           wait(ops_ptr, nops, timeout_ms) -> index
# modules
load(path, plen) -> modhandle                call(mod, name, nlen, args, alen, out, olen) -> n
```

Guest exports are the standard WASI command shape: `memory` and `_start` (the `#[purgatorio::main]` macro wraps it). Optional exports for loadable modules are described in Section 6.3. Versioning: a custom section `purgatorio.abi = "0.1"` is checked at load. Imports outside `wasi_snapshot_preview1` and `purgatorio` (and, in Phase G, `purgatorio_draw` if ever needed) fail at link time with a clear error.

**Extending WASI instead of adding a module:** we deliberately do **not** add non-standard functions inside the `wasi_snapshot_preview1` namespace. Doing so would break tooling and confuse third-party binaries. If upstream WASI later standardizes equivalents (for example a full stat or a cwd), the SDK can switch with no host changes, because the host implements both.

### 6.3 Module loading

Limbo `load` gives a typed module reference whose functions are called directly. The mapping:

1. **Commands** (`init(ctxt, argv)`) are the 95% case. They are WASI command modules in `/bin`. `sh` and `emuinit` run them with `purgatorio.spawn_prog`. A command is one `.wasm`, one instance, and one `_start`.
2. **Libraries** (`bufio`, `arg`, `string`, `styxservers`, `readdir`, `filepat`…) become **Rust crates statically linked** into each app. They are not loaded at runtime. This is the biggest simplification over Dis, and the cost is bigger binaries (mitigated by `wasm-opt` and LTO).
3. **Builtin modules** (`$Sys`, `$Math`, `$Keyring`, `$Draw`, `$Tk`) map as follows:
   - `$Sys` becomes the WASI plus `purgatorio` imports.
   - `$Math` becomes Rust `libm`.
   - `$Keyring` becomes Rust crypto crates in the guest.
   - `$Draw` and `$Tk` become guest libraries over `/dev/draw` (Phase G).
4. **Runtime-loaded plugin modules** (sh's `std`/`expr`/`regex`/`string` builtins loaded by `load std`, wm clients, `alphabet` filters) use `purgatorio.load(path) -> modhandle` plus `call(mod, export, args) -> result`. These are reactor-style modules in `/bin/lib/`. Each one is instantiated as a separate instance in the same prog, sharing that prog's fds and namespace, and its export is called with byte-serialized args (postcard). A typed alternative via the Component Model and WIT is still open (D5). This is deferred to M5. For M1–M4, sh builtins are compiled into sh.

### 6.4 Targets and compatibility

- **Default guest target: `wasm32-wasip1`.** Rust `std` is available. The SDK adds the `purgatorio` imports on top.
- **Minimal target: `wasm32-unknown-unknown` + SDK `no_std`.** It uses only `purgatorio` imports plus a few WASI ones (`fd_read`, `fd_write`, `args_*`, `proc_exit`) linked by the SDK. This is for tiny binaries; it is optional.
- **Unmodified third-party WASI p1 binaries** run with a namespace view: `/` = the prog's namespace and `.` = its dot. They cannot see Inferno-specific features, which is fine.
- WASI p2, p3, and the Component Model are not implemented (D5).

### 6.5 Resource limits and safety

Per-prog limits (memory pages cap, fuel or epoch for kill and quotas) are enforced by the runtime. Guest pointers are always bounds-checked through `GuestMemory`. There is no `unsafe` in host-function code beyond the backend adapters.

---

## 7. Rust app SDK and build pipeline

### 7.1 SDK (`sdk/purgatorio`)

- `sys`: safe wrappers. `Fd` with RAII close and `Read`/`Write` impls (the `core_io`-style traits on `no_std`, `std::io` on wasip1). `open`, `create`, `bind(MREPL|MBEFORE|MAFTER|MCREATE)`, `mount`, `pctl(Pctl::FORKNS | …)`, `Dir`/`Qid` types, `dirread` iterator, `errstr` as `Error`.
- `task`: single-threaded async executor, `Chan<T>` in-instance channels, `alt!`/`select!`, timers. Integrated with `purgatorio.wait`.
- `kchan`: kernel channels with serde-typed messages and handle passing.
- `styx`: 9P server framework (re-exports `purg-styx` types plus a `Navigator`/`StyxServer` helper) so file servers are about 100 lines.
- `file2chan`: a `FileIO` wrapper.
- `dial`: `dial`, `announce`, `listen`, `netmkaddr` as pure file operations on `/net` (a port of `appl/lib/dial.b`).
- `arg`, `bufio`, `env`, `readdir`, `filepat`, `string` helpers, ported from `appl/lib`.
- `#[purgatorio::main] fn main(ctx: Context, args: Vec<String>) -> Result<(), Error>` generates the export, sets up the allocator (`dlmalloc` or `talc` on `wasm32-unknown-unknown`), panic handler → errstr + `exit("panic: …")`.
- Defaults to `wasm32-wasip1` with `std`. An optional `no_std + alloc` mode for `wasm32-unknown-unknown` minimal apps.

### 7.2 Build pipeline

`cargo xtask build-root [--release]`:

1. Run `cargo build --target wasm32-unknown-unknown -p 'appl-cmd-*'` with `opt-level="s"`, `lto=true`, `panic="abort"`, `codegen-units=1`.
2. Post-process with `wasm-opt -Os` (optional), strip custom sections except `name` (debug builds) and `purgatorio.abi`.
3. Validate each module with `wasmparser`: allowed imports only, ABI section present.
4. Install as `root/bin/<cmd>.wasm` (and `root/bin/lib/…` for loadable modules).
5. Optionally precompile for aarch64 (`.cwasm`) for the kernel image.
6. For the kernel, pack `root/` into an initial ramdisk image (a simple archive like Inferno's `mkroot`/`kfs`) or serve it over virtio-9p.

`cargo xtask run-emu [-- args]` builds everything and runs `emu -r root/`.

Developer experience: an app is a normal Cargo crate, `cargo xtask new-cmd foo` scaffolds one, and there is no custom toolchain. The `name` section gives symbolized stack traces in `/prog/<pid>/exception`.

---

## 8. Which `appl/` programs to port, and in what order

Ports are rewrites guided by the Limbo source and man pages (`man/1`, `man/2`, `man/4`), not mechanical translations.

| Wave | Programs | Rationale |
|---|---|---|
| **W0 (M1)** | `emuinit` (minimal), `echo`, `cat` | Prove load, argv, cons, read, write, and the exit path |
| **W1 (M2)** | `sh` (subset: simple commands, pipelines `\|`, redirection `< > >>`, `;`, `&`, quoting, `$var`, `cd`, `pwd`, builtins `load` stub, `exit`), `ls` (`-l -d -p`), `pwd`, `mkdir`, `rm`, `cp`, `mv`, `date`, `sleep`, `wc`, `env`, `ps`, `kill` (with `#p`, M3) | A usable interactive system |
| **W2 (M3)** | `bind`, `mount`, `unmount`, `ns`, `memfs` (Styx server in WASM), `ramfile` (file2chan), `cat /prog/*/status`, `grep`, `sed`-like (`ed` later), `tail`, `cmp`, `diff`, `du`, `fc`, `tee`, `xd`, `md5sum` | Namespace and Styx story end to end |
| **W3 (M4)** | `styxlisten`, `listen`, `dial`, `export`, `import`, `cs` (connection server, `/net/cs`), `ndb/*` minimal, `os` (`#C`), `netstat`, `mntgen`, `9export`-style interop | Networked namespaces: import a remote Inferno or Plan 9 tree and export ours |
| **W4 (M5)** | full `sh` (`std`/`expr`/`regex`/`string` modules, `{}` blocks, `@{}` subshell, `${...}`, `fn`, `if`/`while`/`for` via `std`), `mash`-free, `mk` (optional), `tar`/`gettar`/`puttar`, `gzip`/`gunzip`, `auth` tools per D9, `logfile`, `cron`-like | Scriptability and admin |
| **W5 (Phase G)** | `wm/wm`, `wm/sh`, `wm/edit`, `acme` (much later), `charon` (likely never) | Graphics |

`sh` is the key porting effort. Inferno's `sh.y`/`sh.b` (with `std.b`, `expr.b`, `string.b`, `regex.b`) defines the target grammar and semantics, and `man/1/sh` is the spec.

---

## 9. Testing strategy

1. **Unit tests** (`cargo test`, host) for `purg-styx` (codec round-trip, property tests with `proptest`, fuzzing with `cargo-fuzz` on the decoder), `purg-core` (namespace resolution: bind/union/mount ordering, `..` across mounts, `cleanname`; pctl group forking; qio), and devices via an in-memory `Platform` mock.
2. **Golden namespace tests:** a table of `(ops, path) -> expected resolution/listing` cases derived from Inferno's `man/2/sys-bind` and real-emu behavior.
3. **Styx conformance:** a `purg-styx` test harness that serves `export`ed trees and drives them with an independent client. Specifically:
   - plan9port `9p` (`9p ls`, `9p read`) over TCP against `styxlisten`.
   - The Linux kernel v9fs client (`mount -t 9p -o trans=tcp,version=9p2000`) in CI containers where privileged mounts are possible.
   - **Real Inferno emu** (build from `inferno-os` in CI, run headless `emu -c0`) mounting Purgatorio and vice versa (`mount -A tcp!host!port /n/purg`).
4. **App integration tests:** `tests/scripts/*.sh` run inside emu (`emu -r root sh -c 'script'`), with stdout and exit status compared against golden files. Each test runs on both wasmtime and wasmi backends.
5. **ABI tests:** a dedicated guest test crate (`appl/test/abi`) exercising every import, including error paths, bounds violations (must give errstr and no host crash), kill during blocking read, and fd and handle passing.
6. **Kernel tests:** QEMU `virt` boots in CI (`qemu-system-aarch64 -M virt -cpu cortex-a72 -nographic`), driven over serial with `expect`-style harness (`rexpect`) running the same script corpus as hosted. Unit tests of shared crates also run under `aarch64-unknown-none` via a semihosting test runner (optional).
7. **macOS-specific tests:** `#U` on a case-insensitive APFS volume (the default) and on a case-sensitive one (a disk image created in CI with `hdiutil`). Unicode normalization tests for walk. Thread-stack-size and deep-recursion tests (Section 10.1).
8. **Concurrency tests:** `loom` models for `Rendez`, qio, and channel `alt`. Stress tests run on aarch64 runners specifically, because weak memory ordering surfaces bugs there that x86 hides.

### 9.1 aarch64 validation at every milestone

- **Native aarch64 hosts:** the full unit and integration suite runs natively on Linux aarch64 and macOS arm64, on both WASM backends. wasmtime's Cranelift aarch64 backend is therefore exercised from M1.
- **Bare-metal probe (`purg-probe`):** from M1, CI builds a `no_std` aarch64 image linking `purg-core` + `purg-vm-wasmi` and boots it with `qemu-system-aarch64 -M virt -cpu cortex-a72 -nographic -semihosting`. The probe embeds the milestone's portable script subset and `.wasm` apps, prints over PL011, and exits through semihosting with a pass or fail code. A milestone is not done until its probe subset passes. This keeps the real kernel's portable path continuously proven long before Phase K.
- **`aarch64-unknown-none` builds** of all shared crates, with `-D warnings`, on every PR.
- **Guest `.wasm` artifacts are byte-identical** across all build hosts (a reproducible-build check). The same artifacts run on every target.

### 9.2 CI matrix

| Job | Linux x86_64 | Linux aarch64 | macOS arm64 | `aarch64-unknown-none` |
|---|---|---|---|---|
| Runner | `ubuntu-24.04` | `ubuntu-24.04-arm` | `macos-14`+ (Apple Silicon) | cross-build on Linux x86_64 and aarch64 |
| fmt, clippy `-D warnings` | yes | yes | yes | clippy on shared crates |
| Unit tests (shared crates + emu) | yes | yes | yes | n/a (covered by the probe) |
| Build shared crates (`purg-abi`, `purg-styx`, `purg-platform`, `purg-core`, `purg-vm`, `purg-vm-wasmi`) | yes | yes | yes | **yes, every PR** |
| Integration scripts on wasmtime | yes | yes | yes | n/a |
| Integration scripts on wasmi | yes | yes | yes | n/a |
| `purg-probe` in QEMU (TCG) | yes | yes | optional (Homebrew QEMU) | the image under test |
| `loom` and stress tests | yes | **yes (required)** | yes | n/a |
| macOS case-insensitive and case-sensitive `#U` tests | n/a | n/a | yes | n/a |
| Interop (real Inferno emu, plan9port, v9fs) | yes | yes (v9fs if privileged) | plan9port only | n/a |
| Fuzz smoke, `cargo deny` | yes | n/a | n/a | n/a |
| Phase K: `purg-kernel` QEMU boot and script corpus | yes | yes | optional | the image under test |

All columns are required checks for merge, except where marked optional.

### 9.3 Property tests, fuzzing and the license check

**Property tests.** `crates/purg-styx/tests/prop.rs` uses `proptest` (a dev-dependency; the crate itself stays `no_std`). It checks that every message, `Dir` and `Qid` round-trips, that `decode(encode(x)) == x`, that a message cut at any length is an error, that `msize` is respected, and that the `.u` shim never panics and only hands the client well-formed 9P2000. They run under `cargo test` with a small case count. To run more:

```sh
PROPTEST_CASES=20000 cargo test -p purg-styx --test prop
```

A failure prints a minimal input. Turn it into a named test next to the fix; no regression files are kept.

**Fuzz smoke test.** `crates/purg-styx/tests/fuzz_smoke.rs` runs under `cargo test` and needs no tools. It mutates the captured fixtures and a `.u` conversation with a fixed-seed generator (a few thousand cases per target) and feeds them to the same checks the fuzzers use, so a failure reproduces on every host.

**Fuzzing.** `fuzz/` is a `cargo-fuzz` project with three targets: `decode` (`Msg::decode`, `Dir::unpack`, `dirs`, `stat_from_u`), `dotu` (a whole conversation through `dotu::Shim`) and `record` (the mount's record layer, `purg_auth::record`; its checks are in `fuzz/src/record.rs`, which `purg-auth`'s `fuzz_smoke` test includes). It is not a workspace member, so normal builds do not see it. The checks are in `fuzz/src/lib.rs`, which the two tests above include, so they are compiled and run on every `cargo test`. Fuzzing needs a nightly toolchain and `cargo install cargo-fuzz`; it is a manual job, not a CI job. CI only checks that the targets compile.

```sh
cd fuzz
# The decoder, seeded with the captured 9P2000 traffic.
cargo +nightly fuzz run decode corpus/decode ../crates/purg-styx/tests/fixtures/9fans -- -max_total_time=300
# The shim, seeded with a 9P2000 and a 9P2000.u conversation.
cargo +nightly fuzz run dotu corpus/dotu seeds/dotu -- -max_total_time=300
# The record layer.
cargo +nightly fuzz run record corpus/record -- -max_total_time=300
```

New inputs go to `fuzz/corpus/` and crashes to `fuzz/artifacts/`; neither is committed. Fix a crash with a named test in `crates/purg-styx/tests/`. The seeds in `fuzz/seeds/dotu` are built by `fuzz_smoke.rs`; after changing them there, run `PURG_BLESS=1 cargo test -p purg-styx --test fuzz_smoke`.

**License check.** `deny.toml` is the `cargo-deny` policy for decision D1. It lists the licenses the dependency graph uses, with the reason each is acceptable in an Apache-2.0 distribution, refuses registries other than crates.io and all git dependencies, and warns about duplicate versions. CI installs a pinned `cargo-deny` and runs `cargo deny check licenses bans sources` for the workspace and for `fuzz/`. `cargo deny check advisories` runs as a step that reports but does not fail the job, because the advisory database changes without any change of ours. A new license is added to `deny.toml` only with a reason. The check covers Cargo dependencies only: fonts and other data files still need a separate check (D1), and a binary release needs a file of the third-party notices that the MIT, BSD-3-Clause, ISC and Unicode-3.0 licenses require.

---

## 10. Sharing code between hosted emu and the aarch64 kernel

- **Shared unchanged:** `purg-abi`, `purg-styx`, `purg-core` (namespace, devices: root, cons logic, env, pipe, dup, prog, srv, mnt, mem, ssl/cap later), `purg-vm` (ABI host functions: WASI p1 + `purgatorio`, loader), `purg-vm-wasmi`. In Inferno this is `emu/port` versus `os/port`, a duplication we avoid.
- **Platform trait (`purg-platform`):**

  ```rust
  pub trait Platform: Send + Sync + 'static {
      fn now_ns(&self) -> u64;                 // millisec, #c/time
      fn entropy(&self, buf: &mut [u8]);       // #c/random
      fn spawn_kproc(&self, name: &str, f: Box<dyn FnOnce() + Send>);
      fn console(&self) -> &dyn Console;       // raw byte in/out, raw mode
      type Rendez: RendezImpl; type Lock: LockImpl;   // or generic facade types
      fn host_devices(&self) -> Vec<Arc<dyn Dev>>;   // #U/#I/#C on emu; #t/#S/#l/#I on kernel
  }
  ```

- **Locks:** a facade (`purg_platform::sync::{Mutex, RwLock}`) backed by `std::sync`/`parking_lot` on hosted and by spinlocks with IRQ masking on the kernel.
- **Allocation:** `alloc` everywhere. The kernel provides a global allocator (e.g. `talc` or `linked_list_allocator` over RAM from the DTB).
- **Networking:** `#I` is a shared device front-end with a `NetStack` trait backend. Hosted uses host sockets (`std::net`/`socket2`). The kernel uses `smoltcp` over virtio-net.
- **Probe-to-kernel continuity:** `purg-probe`'s boot stub (exception vectors, PL011, semihosting, a heap over a static region) is the seed of `purg-kernel`'s boot code. It is kept in a shared `purg-aarch64` module once Phase K starts.
- **Storage:** hosted uses `#U` (host fs). The kernel uses virtio-blk plus a Rust filesystem served as a Styx server (`kfs`-like or FAT via `fatfs`; open decision D7), or virtio-9p for development. The virtio-9p path exists (K3, first pass): `#9/<mount tag>` is the transport as a file, one 9P conversation, which the kernel mounts with `#M` on `/n/host` at boot. The native file system exists too (K3, second pass): purgfs (`docs/fs.md`), a journaled kfs-like format in the shared `purg-fs` crate, on virtio-blk (`#S`), served as the device `#K` on `/n/disk`.

### 10.1 Hosted platform notes: Linux and macOS

`src/host/unix.rs` holds everything POSIX-common: pthreads via `std::thread`, termios raw mode for `consctl`, `getpwuid`/`gethostname` for `user`/`sysname`, `posix_spawn` for `#C`, and `socket2` sockets. The rest is OS-specific:

| Area | Linux | macOS (arm64; x86_64 best-effort) |
|---|---|---|
| **Threads** | Default 8 MiB stacks, effectively unlimited thread count | Secondary threads default to **512 KiB stacks**, so always spawn progs with an explicit `std::thread::Builder::stack_size` (e.g. 2–8 MiB, configurable with `emu -s`). The per-process thread limit is about 4–8k (`kern.num_taskthreads`), so emu reports a clean `"no free procs"` error instead of aborting. Wake-ups use `Condvar` everywhere; no futex-specific code |
| **JIT / traps** | wasmtime uses signal handlers and `mmap` RWX toggling | wasmtime uses Mach exception ports and `MAP_JIT` with per-thread W^X (`pthread_jit_write_protect_np`), handled by wasmtime. A signed or notarized `emu` needs the `com.apple.security.cs.allow-jit` entitlement. Do not install our own SIGSEGV/SIGBUS handlers, which would conflict |
| **Page size** | 4 KiB on x86_64; 4, 16, or 64 KiB on aarch64 | 16 KiB. Query `sysconf(_SC_PAGESIZE)`, never assume |
| **File APIs** | `rustix` / `std::fs`. `O_PATH`, `openat2`, and `statx` are optional optimizations only | No `O_PATH`, `openat2`, or `statx`; `fcntl(F_FULLFSYNC)` for `fsync` semantics. Directory reading uses `std::fs::read_dir`; no `getdents` assumptions. `st_birthtime` is available but unused. Ignore `.DS_Store` and AppleDouble `._*` files in `#U` listings (configurable) |
| **Qids** | `qid.path` = hash of (`st_dev`, `st_ino`), `qid.vers` from mtime_ns+size | Same scheme. APFS inode numbers are stable, so it is fine |
| **Case sensitivity** | Case-sensitive | **The default APFS volume is case-insensitive and case-preserving, and normalization-insensitive.** Inferno semantics are case- and byte-exact, so `#U` walk does a host lookup and then verifies that the exact name bytes exist via a cached `read_dir` of the parent. A mismatch gives `"file does not exist"`. `create` of a name differing only by case from an existing file fails with `"file already exists"`. Detect with `pathconf(_PC_CASE_SENSITIVE)` and skip the check on case-sensitive volumes. The `root/` tree in the repo must be case-collision-free (a CI lint) |
| **Unicode names** | Bytes | APFS preserves the bytes as given, but HFS+ and SMB volumes return NFD. `#U` returns names as stored. The exact-byte walk check above also catches NFC/NFD mismatches (D13) |
| **Networking (`#I`)** | Blocking sockets per prog thread; no epoll needed | Same blocking model, so no kqueue is needed. Differences handled in `host/macos.rs`: no `SOCK_CLOEXEC`/`accept4` (`socket2` sets `FD_CLOEXEC`), `SO_NOSIGPIPE` instead of `MSG_NOSIGNAL`, explicit `IPV6_V6ONLY=0` for dual-stack `announce tcp!*!port`, and `SO_REUSEADDR` semantics differ slightly. The application firewall may prompt on listen, so tests bind `127.0.0.1` |
| **System info (`#c/memory`, `#%`)** | `/proc/meminfo`, `/proc/self/status` | `sysctl` (`hw.memsize`) and `mach_task_basic_info`; there is no `/proc` |
| **Entropy and time** | `getrandom`, `clock_gettime` | `getentropy`, `clock_gettime(CLOCK_MONOTONIC/REALTIME)` (both available) |
| **Console** | `/dev/tty` and termios | Same API. `TIOCGWINSZ` works identically |
| **Release artifacts** | `x86_64` and `aarch64` `-unknown-linux-gnu`, plus musl static builds | `aarch64-apple-darwin` (primary), with `x86_64-apple-darwin` and universal binaries optional |

---

## 11. Milestones and acceptance criteria

**Every milestone (M1–M5 and G1–G4) also has a standard aarch64 gate.** Its acceptance criteria are not met unless all of the following are true:

- **(a)** The milestone's hosted acceptance tests pass natively on Linux x86_64, Linux aarch64, and macOS arm64, on both wasmtime and wasmi.
- **(b)** All shared crates build for `aarch64-unknown-none` with `-D warnings`.
- **(c)** The `purg-probe` QEMU aarch64 image runs the milestone's **portable subset**, meaning every test not requiring host-only devices (`#U`, `#C`, host sockets), and exits with a pass code.
- **(d)** The `loom` and stress tests for any new concurrency code pass on Linux aarch64.

Each milestone below lists its specific probe subset.

### M1: "Hello from WASM in a namespace" (hosted, minimal)

Scope (a coding worker can start immediately):

1. Workspace skeleton per Section 3, with these crates: `purg-abi`, `purg-styx` (codec only, no server yet), `purg-platform`, `purg-core`, `purg-vm`, `purg-vm-wasmtime`, `purg-vm-wasmi` (needed by the probe, so it moves up from M2), `purg-emu` (with `host/{unix,linux,macos}`), `purg-probe`, `sdk/purgatorio`, `sdk/purgatorio-macros`, `appl/cmd/{echo,cat,emuinit}`, `xtask`.
2. `purg-core`: `Chan`, `Dev` trait, devtab, `namec` with `#x` device paths and a **root bind only** (`/` = `#U`, plus `#c` bound at `/dev` by emu startup in Rust). `Fgrp` (fds 0/1/2 = `/dev/cons`). Minimal `Prog` (pid, Fgrp, Pgrp, errstr).
3. Devices: `#/` (static), `#c` (`cons` line-buffered read and write, `null`, `user`, `sysname`, `time`, `pid`, `drivers`), `#U` (read-only host dir: walk, stat, open, read, dirread).
4. `purg-vm` plus the wasmtime and wasmi backends implement a WASI p1 subset natively over the namespace: `args_get`/`args_sizes_get`, `environ_get`/`environ_sizes_get` (empty is fine), `fd_read`, `fd_write`, `fd_close`, `fd_seek`, `fd_fdstat_get`, `fd_prestat_get`/`fd_prestat_dir_name` (preopens `/` and live `.`), `path_open` (read-only), `fd_filestat_get`, `path_filestat_get`, `fd_readdir`, `clock_time_get`, `random_get`, and `proc_exit`. They also implement the `purgatorio` subset `errstr`, `werrstr`, `exits`, `stat`, `fstat`, and `dirread`. Apps are built for `wasm32-wasip1` with Rust `std`. The entry point is `#[purgatorio::main]` over `_start`, with argv.
5. `purg-emu`: `emu [-r root] [cmd args...]` loads `/bin/emuinit.wasm`, or directly `/bin/<cmd>.wasm` if given. It runs one prog on the main thread and exits with the prog's status.
6. Apps: `echo`, `cat` (files and stdin), `emuinit` (runs argv[1] by loading it in the same prog via a host-side `exec` shortcut; spawn comes in M2).
7. `purg-styx`: full 9P2000 message codec with round-trip tests (no server yet). This is independent work that can be parallelized.
8. `xtask build-root`, `xtask run-emu`, and `xtask probe`. `xtask probe` builds `purg-probe` with an embedded read-only root: a `#/`-style static tree holding `/bin/{echo,cat}.wasm` and `/lib/probe/hello.txt`. It boots the image in `qemu-system-aarch64 -M virt -cpu cortex-a72 -nographic -semihosting` and checks the PL011 output. The `#c` cons in the probe is backed by PL011.
9. CI: the full Section 9.2 matrix from day one (Linux x86_64, Linux aarch64, macOS arm64, and `aarch64-unknown-none`).

Acceptance criteria:

- `cargo xtask run-emu -- echo hello world` prints `hello world` and exits 0.
- `cargo xtask run-emu -- cat /dev/sysname /dev/user` prints the host name and user.
- `cat /nonexistent` prints `cat: /nonexistent: file does not exist` (Inferno wording) on fd 2 and exits with non-empty status. The emu exit code is non-zero.
- `echo hi | cargo xtask run-emu -- cat` echoes stdin.
- A guest that passes an out-of-bounds pointer to `fd_read` gets errno `EFAULT`, and `purgatorio.errstr` returns `"bad address"`. The host does not crash (ABI test crate).
- `cat` is written with plain Rust `std::fs::File::open`, and its error text comes from `purgatorio.errstr`. This proves WASI-level std I/O and Inferno-exact errors coexist.
- `purg-styx` codec: round-trip tests for every message type, plus decoding of captured real 9P2000 traffic (fixtures committed).
- All of the above pass on Linux x86_64, Linux aarch64, and macOS arm64, on both wasmtime and wasmi (`emu --vm=wasmi`).
- On macOS's default case-insensitive volume, `cat /LIB/probe/hello.txt` fails with `file does not exist` while `cat /lib/probe/hello.txt` succeeds. On Linux this also passes trivially.
- `cargo build -p purg-abi -p purg-styx -p purg-platform -p purg-core -p purg-vm -p purg-vm-wasmi --target aarch64-unknown-none` succeeds.
- **Probe subset:** `xtask probe` boots in QEMU aarch64, runs `echo hello world` and `cat /lib/probe/hello.txt` (both `.wasm`, on wasmi, through `purg-core`'s `namec` and `#c` on PL011) and the bad-pointer test, prints the expected output, and exits through semihosting with code 0 in under 30 s under TCG.

### M2: Usable shell, processes, env, pipes

- `spawn_prog` with pctl flags. Thread-per-prog scheduler. `Rendez`. `wait` status. `pctl(NEWPGRP|FORKNS|FORKFD|FORKENV|NEWNS|NEWFD|NODEVS)`.
- `#e`, `#|`, `#U` read/write (create, remove, wstat).
- `bind` and `unmount` (no `mount` yet), with unions and MCREATE.
- SDK: `Fd` RAII, `bufio`, `arg`, `env`, the `task` executor, and in-instance `Chan<T>`.
- W1 apps, including the `sh` subset.
- Boot sequence like Inferno: emu binds `#c`→`/dev`, `#e`→`/env`, `#p`→`/prog` (M3), then runs `emuinit` → `sh -l` (`/lib/sh/profile`).
- **Accept:** an interactive `sh` session runs `ls -l /bin | wc -l`, `echo $x > /env/x; cat /env/x`, `bind -b /tmp/a /tmp/b; ls /tmp/b` (union), `sleep 10 & ; echo bg` returns immediately, and pipelines of three or more stages work. The script test corpus is green on wasmtime and wasmi on all three hosted targets. The macOS run covers thread stack sizing, with deep recursion in `sh` not crashing. `loom` models for `Rendez` and qio pass. **Probe subset:** `purg-probe` gains a kernel-thread `Platform` (cooperative context switch on aarch64 plus `Rendez`) and runs `sh -c` scripts with pipelines, `&`, `#e`, `#|`, and bind/unions over its embedded root (now writable via a RAM-backed tree).

### M3: Styx everywhere (servers, mount, /prog, file2chan)

- `#M` client mux (tags, flush, msize), `mount`, `export`, `#s` file2chan, kernel channels (`chan_*`, `alt`), and `purgatorio.wait`.
- `#p` (status, ctl kill/killgrp, ns, fd, wait), `#d`, `#%`, the rest of `#c` (`consctl` raw mode, `random`, `kprint`).
- SDK `styx` server framework. W2 apps, including `memfs` and `ramfile`.
- **Accept:** `memfs /tmp/m; echo x > /tmp/m/f; cat /tmp/m/f` works. `ns` output matches Inferno's format. `kill <pid>` interrupts a prog blocked in read. `cat /prog/*/status` works. A pipe-connected `export`/`mount` loopback (`pipe` → `export / ` on one end, `mount` the other at `/n/loop`) passes the namespace golden tests. File2chan `ramfile` works with `cat` and `echo >`. All of this passes on all three hosted targets, and `loom` models pass for `#M` tag/flush and kernel-channel `alt`. **Probe subset:** memfs, ramfile/file2chan, `#p` kill of a blocked prog, and the pipe-loopback `export`/`mount` golden tests run in QEMU aarch64.

### M4: Networking and interop

- `#I` on host sockets, `cs`, `dial`/`announce`/`listen` SDK, `styxlisten`, `listen`, `import`, `os` (`#C`).
- **Accept:** `styxlisten tcp!*!6666 export /` is mountable from **real Inferno emu** (`mount tcp!host!6666 /n/p; ls /n/p`) and from plan9port `9p -a tcp!host!6666 ls /`. Purgatorio `mount -A tcp!infernohost!port /n/inf` reads a real Inferno export. A Linux v9fs mount of a Purgatorio export works (CI container). `dial tcp!example.org!80` and an HTTP GET via `cat`/`echo` to `/net/tcp/*/data` works. The interop tests run on Linux x86_64 and aarch64. On macOS arm64, they run against plan9port, plus dual-stack `announce tcp!*!port` reachable over IPv4 and IPv6, and `os` (`#C`) running a host command. **Probe subset:** `purg-probe` gains virtio-net plus a `smoltcp` `NetStack` behind the shared `#I` front-end (pulled forward from K3 in minimal form). In QEMU user networking, `styxlisten` in the probe is mounted by a hosted emu on the CI host and the namespace tests pass. This proves `#I` and `#M` on aarch64 bare metal.

### M5: Full shell, module loading, auth

- `load`/`call` (or the component model per D5), sh loadable builtins (`std`, `expr`, `string`, `regex`), full WASI p1 coverage for third-party binaries (the remaining `fd_*`/`path_*` calls, `poll_oneoff` edge cases), `fauth`/`Tauth` with the chosen auth scheme (D9), `#D`/TLS, `#¤`.
- W4 apps.
- **Accept:** Inferno `man/1/sh` examples from `sh-std` and `sh-expr` run unchanged. A stock Rust `std` CLI compiled for `wasm32-wasip1` (e.g. a ripgrep-sized tool, or a smaller test program) runs and sees the Purgatorio namespace. Authenticated `styxlisten` and `mount` between two Purgatorio emus works, including a Linux↔macOS pair. Unauthenticated access is rejected. **Probe subset:** the sh-std and sh-expr examples, `load`/`call`, and the WASI p1 test program run in QEMU aarch64. The probe also takes part in authenticated mount with a hosted emu. The crypto crates must be `no_std` and built for `aarch64-unknown-none`.

### Phase K: native aarch64 kernel under QEMU

Because `purg-probe` has run every milestone's portable subset on aarch64 bare metal since M1, Phase K hardens and completes a proven path rather than porting cold. K1 and K2 start from the probe's boot code and kernel-thread `Platform`.

- **K1 Boot:** `purg-kernel` boots on QEMU `virt` (EL1, Linux-style Image or direct `-kernel` ELF), parses the DTB, PL011 console, GICv2/v3, generic timer, MMU identity plus a kernel high half, heap allocator, and kernel threads with a context switch and `Rendez`. **Accept:** serial prints a banner, and a timer-driven test shows two kernel threads alternating.
- **K2 Shared core on metal:** `Platform` impl for the kernel. `purg-core` plus `purg-vm-wasmi` linked. Root from an embedded initrd archive served as a read-only Styx/`Dev` tree. `#c` on PL011. **Accept:** boots to `sh` on serial and runs the M2 script corpus (minus `#U` and `#C`) green.
- **K3 Devices:** virtio-blk plus a filesystem (D7), virtio-net plus `smoltcp` behind `#I`, `#t` eia, `#r` rtc, preemption via timer IRQ plus wasmi fuel checks. **Accept:** M3 and M4 acceptance tests pass in QEMU, including mounting a hosted emu's export over virtio-net (user-mode networking).
- **K4 Performance and hardening:** optional wasmtime `no_std` with AOT `.cwasm` or Pulley (D6), SMP (`-smp 4`), per-prog memory limits. **Accept:** a benchmark suite (`cat` of a 100 MB file, `sh` loop, memfs ops) within the targets set in D6. Stable under `-smp 4` stress for an hour.
- (Optional K5: a real board such as a Raspberry Pi 4/5, out of scope for now.)

### Phase G: Graphics (last)

- **G1 `#i` draw device and memdraw in Rust:** port `libmemdraw` semantics (`draw`, `drawop`, compositing, alpha, `memimage` channels format, `libmemlayer` windows-as-layers) to a pure-Rust crate (`purg-memdraw`, `no_std`). Implement the `/dev/draw` protocol (`new`, `ctl`, `data`, `refresh`) to match Inferno's devdraw message format, so the protocol is unchanged. Host output: a hosted window via `winit` + `softbuffer` (or `minifb`). Kernel output: virtio-gpu or a ramfb framebuffer. `#m` pointer plus keyboard (`/dev/keyboard`, `/dev/pointer`), `#^` snarf.
- **G2 Fonts and the draw SDK:** Inferno subfont format plus `/fonts`, and optionally FreeType replaced by `fontdue`/`ab_glyph`. The SDK `draw` crate (`Display`, `Image`, `Screen`, `Font`, `Point`, `Rect`) speaks `/dev/draw` messages directly from the guest, which means no host import is needed beyond file I/O. This is a strong argument for keeping draw file-based.
- **G3 Tk:** decide between a faithful Tk port (`libtk` is large: about 30k lines of C) behind `#τ` or an SDK-side immediate-mode toolkit. The recommendation is SDK-side: Tk as a guest Rust library rendering through `/dev/draw`, with Tk-command-string compatibility (`tk->cmd(t, ".b configure …")`) as an optional layer (D10).
- **G4 wm and prefab:** `wm/wm` window manager with the `/chan/wm` protocol, `wm/sh`, `wm/edit`, and an `acme` port later. `prefab` (`libprefab`, the TV-oriented UI) is low priority, possibly never.
- **Accept (G1/G2):** a guest program opens `/dev/draw/new`, draws lines, text, and an image in a host window and in QEMU ramfb. Per-pixel output matches reference `memdraw` results on a test corpus (golden PNGs rendered by real Inferno). **Accept (G4):** `wm/wm` starts and hosts `wm/sh` terminals with mouse and keyboard focus, on both hosted and QEMU. The hosted window backend must work on both Linux (X11 and Wayland via `winit`) and macOS (Cocoa via `winit`; event loop on the main thread, so emu runs progs off-main on macOS). `purg-memdraw` golden tests pass on all four targets, with aarch64 bare metal through ramfb in the probe and kernel.

---

## 12. Open decisions for the user

| # | Decision | Options | Recommendation |
|---|---|---|---|
| D1 | License | **RESOLVED (user): Apache-2.0.** | Inferno sources remain reference only; no copied code. Fonts, man pages, and other data files still need a separate license check before inclusion. CI `cargo deny` allows only Apache-2.0-compatible dependency licenses |
| D2 | WASI | **RESOLVED (user): use WASI where it matches the Inferno ABI; otherwise use our own interface.** | WASI p1 base plus the `purgatorio` extension module, with a per-call mapping in Section 6.2. No non-standard functions are added inside the WASI namespace |
| D3 | Binary path | **RESOLVED (user): `/bin`.** | Commands are `/bin/<cmd>.wasm` and loadable modules are `/bin/lib/<mod>.wasm`. sh resolves `cmd` → `./cmd.wasm`, then `/bin/cmd.wasm` (Inferno's lookup order, with `/dis` replaced by `/bin`) |
| D4 | Intra-app `spawn` semantics | Guest async tasks (recommended) / wasm threads + shared memory for true shared-heap progs | Guest tasks now. Revisit threads after K3 |
| D5 | Typed cross-module calls (`load`) | Byte-serialized `call` (postcard) / WASM Component Model + WIT for Inferno modules | Byte-serialized `call` in M5. Evaluate the component model once wasmi and `no_std` support it |
| D6 | Kernel WASM execution | wasmi interpreter only / wasmtime `no_std` + host-AOT `.cwasm` / Pulley; set performance targets | wasmi first. Decide on AOT at K4 with benchmarks |
| D7 | Native kernel filesystem | **DECIDED (first pass): virtio-9p** to a host directory, mounted through `#M`; then a simple native Rust fs served as a Styx server. | Done for virtio-9p: `#9/<tag>` (a file that carries one 9P conversation) on `/n/host`, with a 9P2000.u shim because QEMU speaks only .u and .L (see D8). Done for the native fs: purgfs, kfs-like with a journal (`docs/fs.md`), on virtio-blk (`#S`) as `#K` on `/n/disk`. It is not the root yet |
| D8 | 9P dialect | 9P2000 only / also 9P2000.u or .L for Linux v9fs niceties | 9P2000 only. The client, the servers and the codec speak 9P2000; the one .u code is a translator (`purg_styx::dotu`) between `#M` and QEMU's virtio-9p, which refuses plain 9P2000 |
| D9 | Authentication | Re-implement Inferno keyring/`auth` protocol (compat with real Inferno signer) / Plan 9 p9sk1/dp9ik + factotum-like / modern (TLS + Ed25519 keys via `#D`) | Modern Ed25519 + TLS with a factotum-like agent. Interop with real Inferno only unauthenticated (`-A`) |
| D10 | Tk strategy | Faithful `libtk` port with the Tk command-string API / native Rust guest toolkit / both | Guest-side Rust toolkit over `/dev/draw` with an optional Tk-string compatibility layer |
| D11 | Compatibility target | Behavior-compatible with Inferno (errstrs, `ns` format, man pages as spec) vs. "Inferno-inspired" freedom | Behavior-compatible for the Sys API, devices, and core commands. Free elsewhere |
| D12 | Hosted platforms | **RESOLVED (user): Linux and macOS**, from M1 (Linux x86_64 and aarch64, macOS arm64; see Sections 9.2 and 10.1). Windows is out of scope. | Host code lives in `purg-emu/src/host/{unix,linux,macos}.rs` |
| D0 | aarch64 validation | **RESOLVED (user): every milestone is architecturally validated against aarch64** (native Linux aarch64 and macOS arm64 runs, `aarch64-unknown-none` builds, and the `purg-probe` QEMU gate; see Sections 9.1 and 11). | n/a |
| D13 | Character set and UTF | Strict UTF-8 everywhere (Rust strings) vs. Inferno's lenient runes in file names | UTF-8 strict on the ABI, with byte paths permitted internally for `#U` host files that are not UTF-8 |

---

## 13. Risks

- **`sh` fidelity:** Inferno sh semantics (lists, `${}` builtins, `load std`) are subtle. Mitigate by using `man/1/sh*` as executable tests.
- **Thread-per-prog limits on hosted:** fine for thousands of progs. If that becomes a bottleneck, add Model B (wasmtime async) behind the platform `Rendez` without touching `purg-core`.
- **`no_std` WASM performance in the kernel:** wasmi may be slow for heavy apps. Mitigated by the D6 AOT path.
- **ABI churn:** version the ABI section from day one, and keep the SDK the only supported way to call it.
- **Graphics scope (memdraw, Tk, wm):** large. It is explicitly last and isolated behind the `/dev/draw` file protocol, so it cannot block earlier phases.
