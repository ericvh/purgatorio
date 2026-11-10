# Purgatorio

Purgatorio is a clean-sheet rebuild of [Inferno](https://github.com/inferno-os/inferno-os)
in Rust. The kernel and the hosted emulator are Rust. Applications are Rust
programs compiled to WebAssembly, which replace Limbo and the Dis VM.

Inferno's ideas stay the same: every resource is a file, each process has its
own namespace built with `bind` and `mount`, and one protocol, Styx (9P2000),
connects everything. The same `.wasm` binaries are meant to run everywhere,
just as `.dis` files did.

The work is phased:

1. A hosted `emu` on Linux and macOS. **Done through Milestone 5.**
2. A native aarch64 kernel under QEMU `virt`. **Next.**
3. Graphics (draw, Tk, wm), deliberately last.

Every milestone is validated on aarch64. CI runs natively on Linux aarch64 and
macOS arm64, and the shared kernel crates build for `aarch64-unknown-none`.

## Status: Milestone 5

`emu` boots a namespace like Inferno's emu. `#/` is the root, the host tree
`#U` is unioned after it and takes creates, `#c` is bound before `/dev`, and
`#e`, `#p`, `#s`, `#d`, `#I` and `#C` are bound on `/env`, `/prog`, `/chan`,
`/fd`, `/net` and `/cmd`. `emu` then runs a WASM command as the first
prog, whose fds 0, 1 and 2 are `/dev/cons`; each prog it spawns runs on its
own host thread. With no command it runs `emuinit`, which starts `sh -l`:
the shell runs `/lib/sh/profile`, then reads commands from the console with
a `; ` prompt.

| Piece | State |
|---|---|
| `#/` root, `#c` (`cons`, `null`, `user`, `sysname`, `time`, `pid`, `drivers`), `#U` (read/write host fs) | done |
| `#e` (per-Egrp environment), `#\|` (pipes over qio queues), `#p` (`status`, `ctl`, `ns`, `fd`, `wait`) | done |
| Namespace: `namec`, `bind` with `MREPL`/`MBEFORE`/`MAFTER`/`MCREATE` unions, `unmount`, `chdir`, lexical `..` | done |
| Progs: `spawn_prog`, `pctl` (`NEWFD FORKFD NEWNS FORKNS NEWPGRP NODEVS NEWENV FORKENV`), wait messages; `Rendez` and qio checked with loom | done |
| WASM backends: wasmtime (default, with a compiled-module cache) and wasmi (`--vm=wasmi`) | done |
| App ABI v0.5: all of WASI preview 1 plus the `purgatorio` module (see [docs/abi.md](docs/abi.md)). Stock Rust `std` programs with no SDK run and see the namespace | done |
| `sh` subset: pipelines, `&`, `&&`, `\|\|`, `!`, blocks, redirections, `'quotes'`, `^`, list variables exported through `/env`, `` `{} ``, builtins `cd exit run wait load` | done |
| `load std`: `if`, `while`, `for`, `and`, `or`, `not`, `~` pattern match, `fn`, `raise`, `${hd}` `${tl}` `${index}` `${quote}` `${unquote}`. `load expr`: postfix `${expr}` | done |
| `load string`: the sh-string(1) builtins (`prefix`, `in`, `${splitl}`, `${slice}`, `${fields}`, `${padl}`, ...). `load regex`: `match` and `${re g\|v\|s\|sg\|m\|mg\|M}` | done |
| Plan 9 regular expressions (regexp(6)) in one shared crate, `appl/lib/regexp`, used by `grep`, `sed` and `load regex` | done |
| `load` and `call` of reactor modules from `/bin/lib` (the shell's `load add` gives `${add 20 22}`) | done |
| `#M` mount, `export`, `#s` file2chan, `#d`, `#%`, kernel channels | done |
| `#I` (`/net/tcp`, `/net/udp`, `/net/cs`) on host sockets, `#C` host commands; `dial`, `announce` and `listen` are file operations in the SDK | done |
| Authenticated mount: the server checks an Ed25519 signature over a challenge before `Tattach`; `-A` skips it. `#¤` signs for the client so its seed need not be readable | done |
| `#D`: TLS 1.3 client with a pinned P-256 server key | done |
| W1 commands: `bind cat cp date echo emuinit env ls mkdir mv pwd rm sleep unmount wc` | done |
| W2 commands: `cmp diff du fc grep kill md5sum memfs mount ns ps ramfile sed tail tee xd` | done |
| W3 commands: `cs dial export import listen netstat os styxlisten` | done |
| W4 archive commands: `gettar gunzip gzip lstar puttar tar` (ustar and gzip code in `appl/archive`) | done |
| Admin commands: `cron logfile mntgen ndb/query`; `mount {command}` | done |
| SDK: `arg`, `bufio`, `env`, `daytime`, the `task` executor with `Chan<T>` | done |
| `sh` script corpus (`root/lib/test/sh`), run on both backends | done |
| Styx 9P2000 codec, tested against captured 9fans.net traffic | done |
| `purg-probe`: bare-metal aarch64 QEMU image running the portable subset on wasmi: cooperative kernel threads, a writable RAM root, the `sh` corpus, `load`/`call`, the WASI test program, and `/net` over virtio-net and smoltcp, exported to and mounted from the host | done |

## Layout

```
crates/
  purg-abi/          no_std  ABI constants shared by host and guest
  purg-styx/         no_std  Styx (9P2000) message, Qid and Dir codec
  purg-platform/     no_std  Platform trait and lock facade (the kernel/emu seam)
  purg-core/         no_std  Chan, Dev, devtab, namespaces, Fgrp, Prog, qio, RAM tree, Styx mount/export;
                             #/ #c #e #| #p #s #d #% #M #I #C #D #¤
  purg-vm/           no_std  WASI p1 + purgatorio host functions, loader, spawn
  purg-vm-wasmi/     no_std  wasmi interpreter backend
  purg-vm-wasmtime/  std     wasmtime (Cranelift) backend
  purg-emu/          std     the `emu` binary: #U, console, host sockets and commands, src/host/
  purg-probe/        no_std  bare-metal aarch64 test image for QEMU virt (PL011, kernel threads, virtio-net)
sdk/
  purgatorio/                guest SDK (sys calls, arg, bufio, env, daytime, task)
  purgatorio-macros/         #[purgatorio::main]
appl/cmd/<name>/             commands (sh, ls, ...), built to root/bin/<name>.wasm
appl/lib/<name>/             loadable reactor modules, built to root/bin/lib/<name>.wasm
appl/archive/                ustar and gzip library shared by the archive commands
appl/lib/regexp/             Plan 9 regular expressions, a library linked into grep, sed and sh
appl/test/{abi,procs,m3,net,wasi}/  conformance guests, built to root/bin/test/<name>.wasm
root/                        the file tree emu serves as #U (build-root adds root/tmp)
  lib/sh/profile             run by sh -l at boot
  lib/test/sh/               sh scripts and their expected output
xtask/                       build tasks
docs/                        design plan, roadmap, ABI
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
cargo xtask run-emu -- sh -c 'ls -l /bin | wc -l'
cargo xtask run-emu                          # boots emuinit, then sh -l on the console
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
embeds `root/bin` and `root/lib` and runs the milestone's portable test
subset on `purg-core` and wasmi, with `#c` on the PL011 UART. Its kprocs are
cooperative kernel threads with their own stacks, switched when a
`Rendez` sleeps. Its `#U` is a writable RAM tree seeded with the embedded
files, so it boots the emu's namespace (`#U` after `#/`, `#c`, `#e`, `#p`).
It runs the ABI and procs conformance guests, then every script in the `sh`
corpus with its output captured and compared with the `.out` file. It boots the image with
`qemu-system-aarch64 -M virt -cpu cortex-a72 -nographic -semihosting`,
requires a semihosting exit code of 0 within 120 s, and compares the serial
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
