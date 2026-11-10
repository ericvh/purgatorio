# Roadmap

The task list for Purgatorio. The design and each milestone's acceptance
criteria are in [plan.md](plan.md); this file tracks what is done and what
comes next. Task IDs (`PG-n`) carry over from the earlier Cursor task list.

Status as of 2026-11-09: the hosted emu milestones M1 through M5 are on
`main`. Phase K, the native aarch64 kernel, is next.

## Now

- [ ] Move the probe onto `purg-aarch64` (boot, PL011, scheduler) so the
  probe and the kernel share one copy, and give `xtask probe` a `--kvm` mode.

## Phase K: native aarch64 kernel under QEMU (PG-4)

Start from `purg-probe`, which already has the boot stub, PL011, semihosting,
cooperative kernel threads, a RAM root, and virtio-net with smoltcp. Plan
Section 10 says to move the shared boot code into a `purg-aarch64` module
once Phase K starts.

- [x] **K1 Boot** (`purg-kernel`): EL1 boot on QEMU `virt`, DTB parsing,
  PL011, GICv2/v3, generic timer, MMU (identity plus kernel high half), heap
  allocator, kernel threads with context switch and `Rendez`.
  Accept: serial banner, and a timer-driven test with two kernel threads
  alternating.
- [ ] **K2 Shared core on metal**: kernel `Platform` impl, `purg-core` and
  `purg-vm-wasmi` linked, root from an embedded initrd served as a read-only
  tree, `#c` on PL011.
  Accept: boots to `sh` on serial and runs the M2 script corpus (minus `#U`
  and `#C`).
  - [x] Preemption-safe locks: the no-std lock facade disables preemption
    while held (a per-thread count; the timer's switch is deferred to the
    last release) and blocks instead of spinning when contended. The heap
    masks IRQs.
  - [x] Interrupt-driven PL011 receive (INTID from the DTB) and a blocking
    kernel `Console`; ^D at the start of a line is end of input; CR becomes
    LF.
  - [x] Idle: when every thread is blocked the CPU waits in `wfi` (KVM: 0.6%
    of a host core with a console read blocked, against 100% before).
  - [ ] Console echo and line editing on the kernel (hosted relies on the
    host terminal for both).
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
- [ ] **PG-33** Mount points hold no reference to the file mounted on: the
  mount table matches them by qid. Inferno's holds a fid there. mntgen (#15)
  works around it by keeping each name's qid stable; fix it in purg-core.
- [ ] **PG-32** macOS details from plan Section 10.1 to confirm on a real Mac:
  explicit prog thread stack sizes, a clean `no free procs` error at the thread
  limit, `.DS_Store`/`._*` hidden in `#U`, dual-stack `announce tcp!*!port`.

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
- [x] **PG-26** Fix macOS CI: gate the ABI section to wasm (#10).
- [x] **PG-27** Bring the README and ABI doc up to M5 (#12).
- [x] **PG-19** Document the Rust toolchain in the README.
- [x] **PG-20** Record the wasm64 decision ([wasm64.md](wasm64.md)).
