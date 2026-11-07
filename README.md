# Purgatorio

Purgatorio is a clean-sheet rebuild of [Inferno](https://github.com/inferno-os/inferno-os)
in Rust. The kernel and the hosted emulator are Rust. Applications are Rust
programs compiled to WebAssembly, which replace Limbo and the Dis VM.

Inferno's ideas stay the same: every resource is a file, each process has its
own namespace built with `bind` and `mount`, and one protocol, Styx (9P2000),
connects everything. The same `.wasm` binaries are meant to run everywhere,
just as `.dis` files did.

The work is phased:

1. A hosted `emu` on Linux and macOS. **Milestone 2 is in progress.**
2. A native aarch64 kernel under QEMU `virt`.
3. Graphics (draw, Tk, wm), deliberately last.

Every milestone is validated on aarch64. CI runs natively on Linux aarch64 and
macOS arm64, and the shared kernel crates build for `aarch64-unknown-none`.

## Status: Milestone 2

`emu` boots a namespace like Inferno's emu. `#/` is the root, the host tree
`#U` is unioned after it and takes creates, `#c` is bound before `/dev`, `#e`
on `/env` and `#p` on `/prog`. `emu` then runs a WASM command as the first
prog, whose fds 0, 1 and 2 are `/dev/cons`; each prog it spawns runs on its
own host thread.

| Piece | State |
|---|---|
| `#/` root, `#c` (`cons`, `null`, `user`, `sysname`, `time`, `pid`, `drivers`), `#U` (read/write host fs) | done |
| `#e` (per-Egrp environment), `#\|` (pipes over qio queues), `#p` (`wait` only until M3) | done |
| Namespace: `namec`, `bind` with `MREPL`/`MBEFORE`/`MAFTER`/`MCREATE` unions, `unmount`, `chdir`, lexical `..` | done |
| Progs: `spawn_prog`, `pctl` (`NEWFD FORKFD NEWNS FORKNS NEWPGRP NODEVS NEWENV FORKENV`), wait messages; `Rendez` and qio checked with loom | done |
| WASM backends: wasmtime (default, with a compiled-module cache) and wasmi (`--vm=wasmi`) | done |
| App ABI v0.2: WASI preview1 subset plus the `purgatorio` module (see [docs/abi.md](docs/abi.md)) | done |
| Apps: `echo`, `cat`, `emuinit`, ABI and procs tests | done |
| `sh` subset and the W1 commands | next |
| Styx 9P2000 codec, tested against captured 9fans.net traffic | done |
| `purg-probe`: bare-metal aarch64 QEMU image running the portable subset on wasmi | done |

## Layout

```
crates/
  purg-abi/          no_std  ABI constants shared by host and guest
  purg-styx/         no_std  Styx (9P2000) message, Qid and Dir codec
  purg-platform/     no_std  Platform trait and lock facade (the kernel/emu seam)
  purg-core/         no_std  Chan, Dev, devtab, namespaces, Fgrp, Prog, qio; #/ #c #e #| #p
  purg-vm/           no_std  WASI p1 + purgatorio host functions, loader, spawn
  purg-vm-wasmi/     no_std  wasmi interpreter backend
  purg-vm-wasmtime/  std     wasmtime (Cranelift) backend
  purg-emu/          std     the `emu` binary: #U, console, src/host/{unix,linux,macos}.rs
  purg-probe/        no_std  bare-metal aarch64 test image for QEMU virt (PL011, semihosting)
sdk/
  purgatorio/                guest SDK (errstr, Dir, open/create, bind, pctl, spawn, wait, ...)
  purgatorio-macros/         #[purgatorio::main]
appl/cmd/{echo,cat,emuinit}/ commands, built to root/bin/<name>.wasm
appl/test/{abi,procs}/       conformance guests, built to root/bin/test/<name>.wasm
root/                        the file tree emu serves as #U (build-root adds root/tmp)
xtask/                       build tasks
```

The `no_std` crates are the code the aarch64 kernel will share unchanged.
Only `purg-emu` and `purg-vm-wasmtime` use `std`. Inside `purg-emu`, all
OS-specific code is under `src/host/`.

## Building and running

Use Rust 1.99.1. The file `rust-toolchain.toml` pins it: channel `1.99.1`,
profile `minimal`, components `rustfmt` and `clippy`, and targets
`wasm32-wasip1`, `wasm32-unknown-unknown`, and `aarch64-unknown-none`. The
host target comes with the toolchain. This covers Linux and macOS, including
aarch64. Do not use a distro `rustc` package.

Install rustup from [https://rustup.rs](https://rustup.rs):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then, in this repo, run `rustup show`. That installs 1.99.1 plus those
components and targets.

```sh
cargo xtask build-root                       # build apps into root/bin/*.wasm
cargo xtask run-emu -- echo hello world      # hello world
cargo xtask run-emu -- cat /dev/sysname /dev/user
echo hi | cargo xtask run-emu -- cat         # hi
cargo xtask run-emu -- --vm=wasmi cat /lib/probe/hello.txt
cargo xtask run-emu -- /bin/test/abi.wasm    # ABI conformance checks
cargo xtask run-emu -- /bin/test/procs.wasm  # spawn, pipes, env, files (writes in /tmp)
cargo xtask run-emu                          # boots /bin/emuinit.wasm
```

You can also run the binary directly: `target/debug/emu [-r root] [--vm=wasmtime|wasmi] [cmd [args...]]`.
A command name `cmd` runs `/bin/cmd.wasm`. The exit code is 0 when the prog's
Inferno exit status is empty and 1 otherwise.

### Tests

```sh
cargo xtask build-root    # acceptance tests run the built apps
cargo test --workspace
cargo build --target aarch64-unknown-none \
  -p purg-abi -p purg-styx -p purg-platform -p purg-core -p purg-vm -p purg-vm-wasmi
cargo xtask probe         # needs qemu-system-aarch64
RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p purg-core --features std --test loom --release
```

`cargo xtask probe` builds `purg-probe`, a bare-metal aarch64 image that
embeds a read-only root (`/bin/{echo,cat}.wasm`, `/bin/test/abi.wasm`,
`/lib/probe/hello.txt`) and runs the milestone's portable test subset on
`purg-core` and wasmi, with `#c` on the PL011 UART. It boots the image with
`qemu-system-aarch64 -M virt -cpu cortex-a72 -nographic -semihosting`,
requires a semihosting exit code of 0 within 30 s, and compares the serial
output with `crates/purg-probe/expected.out`.

CI (`.github/workflows/ci.yml`) runs fmt and clippy, then runs the unit and
acceptance tests on both WASM backends on Linux x86_64, Linux aarch64 and
macOS arm64. It also builds and lints the shared crates for
`aarch64-unknown-none`, boots `purg-probe` in QEMU on both Linux runners,
runs the loom models, checks names on a case-sensitive APFS volume, and
verifies that guest `.wasm` files are byte-identical across all three hosts.

### Writing an app

An app is a normal Cargo crate under `appl/cmd/<name>` with a `[[bin]]` named
`<name>`. It uses Rust `std`, and the SDK supplies the Inferno-specific parts:

```rust
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match std::fs::read_to_string(&args[1]) {
        Ok(s) => print!("{s}"),
        Err(_) => eprintln!("{}: {}: {}", args[0], args[1], purgatorio::errstr()),
    }
    Ok(())
}
```

## License

Purgatorio is licensed under the Apache License, Version 2.0 ([LICENSE](LICENSE)).

Inferno is used only as a reference for behavior and design. No Inferno
code, fonts or manual pages are copied into this repository.
