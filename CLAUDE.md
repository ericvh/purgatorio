# Purgatorio

A clean-sheet rebuild of Inferno in Rust. The kernel and the hosted `emu` are
Rust; applications are Rust compiled to `wasm32-wasip1`, replacing Limbo and
Dis. Inferno's model stays: everything is a file, per-prog namespaces built
with `bind`/`mount`, and one protocol, Styx (9P2000).

- `docs/plan.md`: the reference design and the milestone acceptance
  criteria (Section 11) and open decisions (Section 12). Read the relevant
  section before starting a milestone.
- `docs/roadmap.md`: the task list. What is done, what is next, known gaps.
  Keep it current: tick tasks off and add new ones as work lands.
- `docs/abi.md`: the guest ABI (WASI p1 plus the `purgatorio` module).
- `docs/wasm64.md`: why we stay on wasm32 and how a later wasm64 port fits.
- `README.md`: user-facing status, layout and build instructions.

## Commands

```sh
cargo xtask build-root           # build guests into root/bin (tests need this first)
cargo test --workspace           # unit + acceptance tests, wasmtime and wasmi
cargo xtask run-emu -- sh -c 'ls /bin | wc -l'
cargo xtask probe                # bare-metal aarch64 image in QEMU (qemu-system-aarch64)
cargo xtask kernel               # purg-kernel under QEMU TCG: GICv2, GICv3, EL2 boot
cargo xtask kernel --kvm         # the same under KVM (aarch64 host, GICv3 only, PSCI exit)
cargo xtask clippy-guests        # lint the SDK and every guest for wasm32-wasip1
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --target aarch64-unknown-none \
  -p purg-abi -p purg-styx -p purg-platform -p purg-core -p purg-vm -p purg-vm-wasmi
RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p purg-core --features std --test loom --release
```

The toolchain is pinned to Rust 1.99.1 in `rust-toolchain.toml`. CI sets
`RUSTFLAGS=-D warnings`, so treat warnings as errors locally too.

Before pushing, run fmt, both clippies, `build-root`, and the tests you touched.
If the probe's output changes, update `crates/purg-probe/expected.out`.

## Rules

- **Shared crates are `no_std` + `alloc`:** `purg-abi`, `purg-styx`, `purg-aarch64`,
  `purg-platform`, `purg-core`, `purg-vm`, `purg-vm-wasmi`. They must build for
  `aarch64-unknown-none` unchanged; the kernel reuses them. Only `purg-emu`
  and `purg-vm-wasmtime` use `std`.
- **No `cfg(target_os)` outside `crates/purg-emu/src/host/`.** POSIX-common
  code goes in `host/unix.rs`, OS-specific code in `host/{linux,macos}.rs`.
- **Every blocking or locking path in `purg-core` goes through
  `purg-platform`** (`Rendez`, the lock facade, `spawn_kproc`), so the probe
  and the kernel exercise the same code.
- **aarch64 is first-class:** use explicit `Acquire`/`Release`, never assume
  4 KiB pages (macOS is 16 KiB), decode wire formats with `from_le_bytes` and
  no pointer casts. New lock-free code gets a `loom` model.
- **Every milestone passes the aarch64 gate** (plan Section 11): hosted tests on
  Linux x86_64, Linux aarch64 and macOS arm64 on both VMs; shared crates on
  `aarch64-unknown-none`; the milestone's portable subset in `purg-probe`;
  loom for new concurrency.
- **Inferno semantics, not POSIX.** Error strings match Inferno exactly
  (`file does not exist`, `permission denied`, ...), as do `ns` output, wait
  messages and command behavior. Use Inferno's man pages as the spec. No
  Inferno code, fonts or man pages are copied into the repo (Apache-2.0).
- **Guest `.wasm` must be byte-identical across build hosts.** CI compares
  hashes from all three runners. `xtask` sets reproducible rustflags and strips
  symbols; do not add anything host-dependent to guest builds.
- **Host-only attributes in guest crates must be gated to wasm.** The
  `purgatorio.abi` custom section is `#[cfg_attr(target_family = "wasm", ...)]`:
  host clippy builds every guest crate, and Mach-O rejects that section name.
- Do not add non-standard functions to the `wasi_snapshot_preview1` namespace.
  New host calls go in the `purgatorio` module; bump `purg_abi::ABI_VERSION`
  and update `docs/abi.md` when the ABI changes.
- Stay on `wasm32-wasip1`. A wasm64 port is a later, separate ABI
  (`docs/wasm64.md`).

## Layout

```
crates/purg-{abi,styx,platform,core,vm,vm-wasmi}   no_std shared core
crates/purg-vm-wasmtime                            wasmtime backend (std)
crates/purg-emu                                    the emu binary; host code in src/host/
crates/purg-probe                                  bare-metal aarch64 QEMU image; expected.out
crates/purg-aarch64                                DTB, GIC, timer, vectors, MMU, PL011, scheduler
crates/purg-kernel                                 the native kernel (Phase K); expected.out
sdk/purgatorio, sdk/purgatorio-macros              guest SDK, #[purgatorio::main]
appl/cmd/<name>/                                   commands -> root/bin/<name>.wasm
appl/lib/<name>/                                   cdylib crates are loadable reactors -> root/bin/lib/<name>.wasm;
                                                   others (regexp) are ordinary libraries
appl/test/{abi,procs,m3,net,wasi}/                 conformance guests -> root/bin/test/
root/                                              the tree emu serves as #U
root/lib/test/sh/                                  sh script corpus with .out goldens
xtask/                                             build-root, run-emu, probe, clippy-guests
```

New commands are normal Cargo crates under `appl/cmd/<name>` with a `[[bin]]`
named `<name>`, added to the workspace `members`.

## Working style

- Work moves through milestones without waiting for review between them.
  Keep each milestone or sub-feature a separate, reviewable commit or PR.
- Commit subjects are prefixed with the milestone (`M5: add a TLS 1.3 client
  device`, `K1: ...`). Subjects and bodies are short, plain sentences that say
  what changed and why.
- Docs and the README use plain, direct English: short sentences, no
  marketing tone.

## Macs and cross-checking

macOS-only breakage can be linted from Linux:

```sh
rustup target add aarch64-apple-darwin
CC_aarch64_apple_darwin=<stub-cc> AR_aarch64_apple_darwin=<stub-ar> \
  cargo clippy --workspace --all-targets --target aarch64-apple-darwin -- -D warnings
```

`ring` and `wasmtime` compile C in build scripts. For a lint nothing is linked,
so stub scripts that just create their `-o` output (and the `.a` archive) are
enough. This catches compile errors only; runtime behavior (case-insensitive
APFS, 512 KiB default thread stacks, Mach exception ports) needs a real Mac.
