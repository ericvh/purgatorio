# Roadmap

The task list for Purgatorio. The design and each milestone's acceptance
criteria are in [plan.md](plan.md); this file tracks what is done and what
comes next. Task IDs (`PG-n`) carry over from the earlier Cursor task list.

Status as of 2026-11-09: the hosted emu milestones M1 through M5 are on
`main`. Phase K, the native aarch64 kernel, is next.

## Now

- [ ] **PG-26** Merge #10, the macOS CI fix. Host clippy failed on macOS
  because `appl/test/wasi` and `appl/lib/add` put the `purgatorio.abi` section
  on every target, and Mach-O rejects that name. With the fix, the whole
  macOS job passes, including tests and the case-sensitive APFS check.
- [ ] **PG-27** Update `README.md` for M4 and M5. It still says "Milestone 3 is
  in progress", lists ABI v0.2 (now 0.4), and has no rows for `#I`, `#C`,
  dial/listen, virtio-net on the probe, `load std`/`load expr`, `load`/`call`,
  authenticated mount, full WASI p1, `#¤` and `#D`.

## Phase K: native aarch64 kernel under QEMU (PG-4)

Start from `purg-probe`, which already has the boot stub, PL011, semihosting,
cooperative kernel threads, a RAM root, and virtio-net with smoltcp. Plan
Section 10 says to move the shared boot code into a `purg-aarch64` module
once Phase K starts.

- [ ] **K1 Boot** (`purg-kernel`): EL1 boot on QEMU `virt`, DTB parsing,
  PL011, GICv2/v3, generic timer, MMU (identity plus kernel high half), heap
  allocator, kernel threads with context switch and `Rendez`.
  Accept: serial banner, and a timer-driven test with two kernel threads
  alternating.
- [ ] **K2 Shared core on metal**: kernel `Platform` impl, `purg-core` and
  `purg-vm-wasmi` linked, root from an embedded initrd served as a read-only
  tree, `#c` on PL011.
  Accept: boots to `sh` on serial and runs the M2 script corpus (minus `#U`
  and `#C`).
- [ ] **K3 Devices**: virtio-blk plus a filesystem (D7: virtio-9p for
  development, then a native Rust fs served over Styx), virtio-net plus
  smoltcp behind `#I`, `#t` eia, `#r` rtc, preemption via timer IRQ and wasmi
  fuel.
  Accept: M3 and M4 acceptance tests in QEMU, including mounting a hosted emu's
  export over virtio-net.
- [ ] **K4 Performance and hardening**: decide D6 (wasmi only, or wasmtime
  `no_std` with AOT `.cwasm`, or Pulley) from benchmarks, SMP (`-smp 4`),
  per-prog memory limits.
  Accept: benchmark suite (`cat` of 100 MB, `sh` loop, memfs ops) within the
  D6 targets, and stable under `-smp 4` stress for an hour.
- [ ] K5 (optional): a real board such as a Raspberry Pi 4/5. Out of scope for
  now.

## Phase G: graphics, last (PG-5)

- [ ] **G1** `#i` draw device and `purg-memdraw` (`no_std`), Inferno's
  `/dev/draw` protocol, hosted window via `winit` + `softbuffer`, QEMU ramfb or
  virtio-gpu, `#m` pointer and keyboard, `#^` snarf.
- [ ] **G2** Fonts (Inferno subfont format) and the guest `draw` SDK over
  `/dev/draw`.
- [ ] **G3** Tk (D10: guest-side Rust toolkit, optional Tk-string layer).
- [ ] **G4** `wm/wm`, `wm/sh`, `wm/edit`; `acme` later.

## Gaps against the plan

Items the plan lists for M1 to M5 that are not in the repo yet. None blocks
Phase K, but they belong to the hosted milestones' acceptance criteria.

- [ ] **PG-28** Interop tests: real Inferno emu mounting a Purgatorio export and
  the reverse, plan9port `9p` against `styxlisten`, and a Linux v9fs mount in a
  CI container (plan Section 9, M4 acceptance).
- [ ] **PG-29** Authenticated mount between a Linux and a macOS emu (M5
  acceptance).
- [ ] **PG-30** `cargo deny` license check (D1) and a fuzz smoke test of the
  Styx decoder, plus `proptest` round trips (plan Section 9).
- [ ] **PG-31** W3/W4 apps not yet written: `mntgen`, minimal `ndb`, `tar`,
  `gzip`/`gunzip`, `logfile`, cron-like; sh `string` and `regex` modules.
- [ ] **PG-32** macOS details from plan Section 10.1 to confirm on a real Mac:
  explicit prog thread stack sizes, a clean `no free procs` error at the thread
  limit, `.DS_Store`/`._*` hidden in `#U`, dual-stack `announce tcp!*!port`.

## Self-hosting

- [ ] **PG-43 Self-hosting the guest toolchain** ([selfhost.md](selfhost.md)):
  rustc as WebAssembly, run in Purgatorio, building guests byte-identical to
  the host builds. Stages: (A) wasm tools (`wasmparser`, `wat`) as guest
  commands; (B) spike with an existing rustc.wasm under the hosted emu,
  driven without cargo, after raising `max_wasm_stack`, a copy fallback for
  cross-directory WASI renames (rustc's rlib and rmeta writes hit `EXDEV`
  today) and a persistent module cache; (C) sysroot in `/lib/rust` and a
  build file from `cargo build -v` for `mk` or sh, with
  `#[purgatorio::main]` turned into a `macro_rules!` macro; (D) our own
  single-threaded rustc 1.99.1 for `wasm32-wasip1` with in-process LLD, or
  wasi-threads on wasmtime only if D4 allows; (E) the byte-identical check
  as a CI job. Kernel use waits for AOT (D6); wasm64 only for crates that
  outgrow 4 GiB.

## Later

- wasm64 guests as a separate `wasm64-purgatorio` ABI, binaries in `/bin/32`
  and `/bin/64` once both widths exist ([wasm64.md](wasm64.md)).
- Typed cross-module calls via the Component Model once wasmi and `no_std`
  support it (D5).
- Wasm threads for shared-heap `spawn`, revisited after K3 (D4).

## Done

- [x] **PG-1** Architecture and roadmap plan ([plan.md](plan.md)).
- [x] **PG-2** Code repository.
- [x] **PG-6** M1: hosted emu with WASI on wasmtime and wasmi (#1).
- [x] **PG-7** M1: bare-metal aarch64 probe image (#2).
- [x] **PG-8** M2a: progs, pipes, env, bind (#3).
- [x] **PG-9** M2b: shell and core apps (#4).
- [x] **PG-10** M2c: probe kernel threads and RAM root (#5).
- [x] **PG-11** M3: Styx servers, mount, `/prog`, file2chan (#6).
- [x] **PG-12** M4: networking (#7).
  - [x] **PG-13** Host dial, listen, export and import.
  - [x] **PG-14** virtio-net and smoltcp on the probe.
- [x] **PG-15** M5: shell, module loading, auth (#8).
  - [x] **PG-17** `load std` and `load expr` in the shell.
  - [x] **PG-21** `load` and `call` of a reactor module.
  - [x] **PG-23** Authenticated mount (Ed25519 challenge).
  - [x] **PG-18** The rest of WASI preview 1.
  - [x] **PG-25** Factotum device `#¤`.
  - [x] **PG-24** TLS 1.3 client device `#D`.
- [x] **PG-3** Hosted emu milestones as stacked PRs.
- [x] **PG-22** Land the stack tip onto `main` (#9).
- [x] **PG-19** Document the Rust toolchain in the README.
- [x] **PG-20** Record the wasm64 decision ([wasm64.md](wasm64.md)).
