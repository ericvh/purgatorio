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
2. A native aarch64 kernel under QEMU `virt`. **K1 (boot) is done.**
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
| WASM backends: wasmtime (default, with a compiled-module cache) and wasmi (`--vm=wasmi`; modules compiled lazily, and progs sharing engines in generations that keep each module by digest, see [docs/perf.md](docs/perf.md) Section 9) | done |
| App ABI v0.7: all of WASI preview 1 plus the `purgatorio` module (see [docs/abi.md](docs/abi.md)). Stock Rust `std` programs with no SDK run and see the namespace | done |
| `sh` subset: `-e`, pipelines, `&`, `&&`, `\|\|`, `!`, blocks, redirections, `'quotes'`, `^`, list variables exported through `/env`, `` `{} ``, `<>` (read and write), builtins `cd exit run wait load` and `@{}` sub-shells (own variables, environment, fds and namespace); `load std` has `pctl` and `${pid}`; file-name patterns (`*`, `?`, `[class]`) | done |
| `load std`: `if`, `while`, `for`, `and`, `or`, `not`, `~` pattern match, `fn`, `raise`, `${hd}` `${tl}` `${index}` `${quote}` `${unquote}`. `load expr`: postfix `${expr}` | done |
| `load string`: the sh-string(1) builtins (`prefix`, `in`, `${splitl}`, `${slice}`, `${fields}`, `${padl}`, ...). `load regex`: `match` and `${re g\|v\|s\|sg\|m\|mg\|M}` | done |
| The rest of the sh modules: `load arg`, `csv`, `echo`, `file2chan`, `mload`, `mpexpr` (arbitrary precision), `sexprs`, `test` | done |
| `mash`, Inferno's other shell, with its built-in make (`load make`), history and `serve` | done |
| Plan 9 regular expressions (regexp(6)) in one shared crate, `appl/lib/regexp`, used by `grep`, `sed` and `load regex` | done |
| `load` and `call` of reactor modules from `/bin/lib` (the shell's `load add` gives `${add 20 22}`) | done |
| `#M` mount, `export`, `#s` file2chan, `#d`, `#%`, kernel channels | done |
| Fewer Styx round trips: `#M` walks up to 16 names in one `Twalk`, sends most `Tclunk`s without waiting, and asks for 64 KiB messages (a deep stat is 3 round trips, not 37; see [docs/lethe-perf.md](docs/lethe-perf.md) Section 10) | done |
| `#I` (`/net/tcp`, `/net/udp`, `/net/icmp`, `/net/cs`) on host sockets, `#C` host commands; `dial`, `announce` and `listen` are file operations in the SDK. `/net/icmp` is echo only: an unprivileged ICMP socket on the host (Linux allows it to the groups in `net.ipv4.ping_group_range`), a raw smoltcp socket on the kernel. UDP has `headers` mode. On the kernel `/net/ipifc/0` and `/net/iproute` set the address and the default route; the emu has none | done |
| Authenticated mount: the server checks an Ed25519 proof over a challenge before `Tattach`, from a key it holds or a certificate from a signer it trusts; `-A` skips it. The handshake also agrees a session key (ephemeral X25519), and the conversation after it is encrypted and authenticated with ChaCha20-Poly1305 records (`ring`'s in the emu). The server proves itself in turn, over the session's keys, and `mount -S` requires that, for the host dialled; `export -e` refuses conversations in the clear. The stock root trusts no key, and a server without `-A` that trusts none lets nobody in. `#¤` or a guest factotum signs for the client. See [docs/auth.md](docs/auth.md) | done |
| Auth tools: `auth/createsignerkey signer keyfs keysrv passwd changelogin factotum secstore countersigner mkauthinfo verify getpk`, `getauthinfo`, `crypt`, `netkey`; password login is SPAKE2 | done |
| Per-prog users: each prog acts as a user, inherited at spawn; the host owner may become another user through `/dev/user`, and `runas user cmd` runs a command so. `/lib/keys/serversigners` keeps the signers trusted for servers apart from users'; factotum keys take a `role` (`auth/factotum -s` for the server key). See [docs/auth.md](docs/auth.md) | done |
| `#D`: TLS 1.3 client with a pinned P-256 server key | done |
| Lethe, a stateless protocol for static trees: tickets sealed under a server key, every request sealed, `stat`, `read` (conditional on a version), `readdir` with a cookie, and `walk` of a whole path in one request. `#λ` trusts what it learnt for 1 s (`lethe -t ms`), caches 32 KiB blocks in an LRU and reads ahead for sequential readers; the emu seals with ring. `lethed` serves a host directory read-only; `#λ` and `lethe addr mtpt [path]` attach it, on the emu and the kernel. A clear mode with no authentication and no encryption is an explicit option on both ends (`lethed -A`, `lethe -A`), for trusted links and measurement; neither end falls back to it. See [docs/lethe.md](docs/lethe.md) | read-only first cut; writes and streams designed only |
| W1 commands: `bind cat cp date echo emuinit env ls mkdir mv pwd rm sleep unmount wc` | done |
| W2 commands: `cmp diff du fc grep kill md5sum memfs mount ns ps ramfile sed tail tee xd` | done |
| `sed`: sed(1) with `-n -g -e -f`, line, `$` and regular expression addresses, ranges and `!`, `{ }` blocks, labels, `b` and `t`, the hold space and `a i c d D g G h H l n N p P q r s w x y =`; `s` takes `g`, `p`, a count and `w file` | done |
| W3 commands: `cs dial export import listen netstat os styxlisten` | done |
| W4 archive commands: `gettar gunzip gzip lstar puttar tar` (ustar and gzip code in `appl/archive`) | done |
| Admin commands: `cron logfile mntgen ndb/query`; `mount {command}` | done |
| Namespace and Styx tools: `styxchat styxmon iostats nsbuild lockfs trfs fcp tarfs archfs` | done |
| Remote commands: `rcmd cpu auxi/rstyxd auxi/cpuslave`, authenticated with the mount proof both ways (`-S` requires the server's) and encrypted after it; `rcmd host mcpd` is a remote MCP server. See [docs/rcmd.md](docs/rcmd.md) | done |
| Plan 9 interop: `9export -A` and `import -9 tree` (exportfs's opening, no `p9any`) | tested against each other only |
| `dbfs` and `rawdbfs`: the records of a text file, or of an append-only binary log, as files (dbfs(4)) | done; rawdbfs's file format is its own |
| `fs`: trees as values, with all of fs(1)'s modules (`walk filter select merge write bundle unbundle exec pipe proto run setroot chstat compose mergewrite void` and the gates), `@{...}` command blocks | done; `mergewrite` takes its selector as `-c` |
| Network commands: `ndb/cs ndb/csquery ndb/dns ndb/dnsquery ndb/registry ndb/regquery webgrab telnet whois ip/sntp ip/tftpd ip/ping ip/dhcp ip/virgild`. `ip/dhcp` is for the kernel (QEMU's DHCP server) and does not renew its lease | done |
| `9660srv`: ISO 9660 images read-only, with Rock Ridge names and modes and Joliet names (9660srv(4)) | done; no High Sierra, Plan 9 extensions or multi-extent files |
| `dossrv`: FAT12, FAT16 and FAT32 read and write, with long names (dossrv(4)) | done; times are UTC, `-v -F -c -S` do nothing |
| `ftpfs`: an FTP server's files, passive or active, read and write, with renames (ftpfs(4)) | tested against /bin/test/ftpd only; no symbolic links |
| `dbm/store dbm/fetch dbm/delete dbm/keys dbm/list` | done; the hash is our own, so Inferno's dbm files are not readable |
| System tools: `stream timestamp pause mk plumber plumb install/mkproto install/proto2list install/ckproto install/create install/inst install/info install/wrap2list install/updatelog install/applylog disk/mkfs disk/mkext disk/mkdisk disk/chkdisk disk/prep disk/fdisk disk/mbr`; `mk` runs recipes in parallel up to `$NPROC`, each with `sh -e`; `#S` partitions (`part`/`delpart` on `ctl`) | done |
| `mcpd`: an MCP server that gives an LLM client a sandboxed session in its own namespace: `run`, file tools, jobs, namespace tools, resources, a namespace(6) policy file and an audit log; a `/prog` of its own progs only; remote sessions over encrypted `rcmd`, also on the kernel. Each session is a prog domain with a memory limit (`-M`) and a prog limit (`-P`), its own `#s` registry and no `#¤` unless the policy grants it; it ends by killing the whole domain. See [docs/mcp.md](docs/mcp.md) | stages 1 to 3 and 5 |
| `mcpfs`: another MCP server's tools as files (`tools/NAME/schema`, `tools/NAME/call`), with the server started as a guest command: `mcpd`, or `rcmd host mcpd`. See [docs/mcp.md](docs/mcp.md) | tools only; resources, notifications and host servers through `#C` open |
| Text tools: `sort uniq comm tr fmt strings freq look tsort tokenize sha1sum sum uuencode uudecode tcs units calc unicode bytes p mc cal randpass zeros ed` (data in `/lib/units` and `/lib/convcs/charsets`) | done |
| File and script tools: `basename cleanname touch chmod chgrp dd newer read test time xargs` | done |
| Exit statuses as in Inferno: `kill` goes on through its list (and takes module names), `diff` reports a file it cannot open without failing, `dd`, `time`, `tsort` and `calc` as their sources give them; subdirectory commands report their full names (`ndb/query`); files from a mounted server are of type `M` | done |
| Module and debugging tools: `wasmdump wasmdep` (Inferno's `disdump`, `disdep`), `trace stack broke prof`: call stacks and function profiles of traced progs through `/prog/<pid>/{stack,exception,prof}`, the same on both backends; function names in `/bin/<cmd>.names`. See [docs/debug.md](docs/debug.md) | done; `asm` open |
| `cprof` (call counts and coverage by function) and `mprof` (memory grown, by function and its callees), exact and the same on both backends, through `/prog/<pid>/{cprof,mprof}` | done; line coverage needs DWARF |
| `mdb pid`: stop a traced prog (`stop`/`start`/`step` on `ctl`), breakpoints on function entry (`bpt set`, `bpt del`), its stack, and its memory through `/prog/<pid>/heap`, on both backends | done; locals need DWARF |
| SDK: `arg`, `bufio`, `env`, `daytime`, the `task` executor with `Chan<T>` | done |
| `sh` script corpus (`root/lib/test/sh`), run on both backends | done |
| Styx 9P2000 codec, tested against captured 9fans.net traffic | done |
| `purg-probe`: bare-metal aarch64 QEMU image running the portable subset on wasmi, on the kernel's `purg-aarch64` boot path, interrupts and preemptive kernel threads: a writable RAM root, the `sh` corpus, `load`/`call`, the WASI test program, and `/net` over virtio-net and smoltcp, exported to and mounted from the host | done |

## Layout

```
crates/
  purg-abi/          no_std  ABI constants shared by host and guest
  purg-styx/         no_std  Styx (9P2000) message, Qid and Dir codec; a 9P2000.u translator
  purg-auth/         no_std  certificates, mount proofs, session keys and records, password login (SPAKE2), sealing
  purg-lethe/        no_std  Lethe: frames, tickets, the server engine and the client (docs/lethe.md)
  purg-platform/     no_std  Platform trait and lock facade (the kernel/emu seam)
  purg-core/         no_std  Chan, Dev, devtab, namespaces, Fgrp, Prog, qio, RAM tree, Styx mount/export;
                             #/ #c #e #| #p #s #d #% #M #I #C #D #¤ #λ
  purg-vm/           no_std  WASI p1 + purgatorio host functions, loader, spawn
  purg-vm-wasmi/     no_std  wasmi interpreter backend
  purg-vm-wasmtime/  std     wasmtime (Cranelift) backend
  purg-emu/          std     the `emu` binary: #U, console, host sockets and commands, src/host/;
                             the `lethed` binary, the Lethe server
  purg-probe/        no_std  bare-metal aarch64 test image for QEMU virt, on purg-aarch64
  purg-aarch64/      no_std  aarch64 support: boot, DTB, GICv2/v3, timer, vectors, MMU, PL011, scheduler, virtio-net, smoltcp, virtio-9p, virtio-blk
  purg-fs/           no_std  purgfs, the journaled disk file system, its checker, and #K (docs/fs.md)
  purg-system/       no_std  the bare-metal images' boot namespace and program runner
  purg-kernel/       no_std  the native kernel for QEMU virt (Phase K); #9 (virtio-9p), #S (virtio-blk), #K
sdk/
  purgatorio/                guest SDK (sys calls, arg, bufio, env, daytime, task)
  purgatorio-macros/         #[purgatorio::main]
appl/cmd/<name>/             commands (sh, ls, ...), built to root/bin/<name>.wasm
appl/lib/<name>/             loadable reactor modules, built to root/bin/lib/<name>.wasm; plain
                             libraries shared by commands (regexp, filepat, ndb, styxserv, ...)
appl/archive/                ustar and gzip library shared by the archive commands
appl/lib/regexp/             Plan 9 regular expressions, a library linked into grep, sed and sh
appl/test/{abi,procs,m3,net,wasi}/  conformance guests, built to root/bin/test/<name>.wasm
root/                        the file tree emu serves as #U (build-root adds root/tmp)
  lib/sh/profile             run by sh -l at boot
  lib/test/sh/               sh scripts and their expected output
xtask/                       build tasks
docs/                        design plan, ABI (the roadmap is GitHub issues)
```

The `no_std` crates are the code the aarch64 kernel will share unchanged.
Only `purg-emu` and `purg-vm-wasmtime` use `std`. Inside `purg-emu`, all
OS-specific code is under `src/host/`.

## Phase K: the native kernel

`purg-kernel` boots on QEMU `virt` at EL2 or EL1, turns on the MMU with an
identity map and a kernel high half (virtual `0xffff_0000_0000_0000 + p`),
and builds a heap over the RAM after the image. It reads the DTB that QEMU
leaves at the start of RAM, brings up the GIC (v2 or v3) and the virtual
timer at 100 Hz, and runs preemptive kernel threads. Its self-test ends with
two threads that only make progress when the timer switches between them.
Locks defer preemption instead of being preempted, the console is driven by
the UART interrupt, and an idle CPU waits in `wfi`. A console kproc runs the
line discipline as input arrives, so typing is echoed at once (typeahead
included), erasing knows how many columns a tab or a wide character took,
and ^C sends the `interrupt` note to the console's foreground process group
([docs/abi.md](docs/abi.md), "Interrupts"): an interactive `sh` abandons
the command and prompts again.

On top of that it runs the shared core (K2): `purg-core`, `purg-vm` and
wasmi, with the emu's boot namespace over a RAM root. The root is a ustar
archive of `root/bin` and `root/lib` compiled into the image, as Inferno's
kernels do; an initrd named by the DTB replaces it. The kernel is also an
arm64 Linux `Image`, so QEMU and boot loaders can pass it an initrd. It boots
to `sh -l` on the serial console. With `test` in the boot arguments it runs
the conformance guests and the sh corpus instead, and then `sh -i` on the
console, which `xtask` types at: a line with a tab erased, a line typed
ahead during a `sleep`, and ^C during a `sleep` and at the prompt. The
namespace is the emu's less `/cmd`: there is no host to run commands on, so
the kernel and the probe bind nothing there (`#C` is still in the device
table, and its `clone` fails with `not implemented`).

K3 networking: `#I` is on `/net`, over smoltcp on a virtio-net card
(`purg_aarch64::net`), with TCP and UDP. In test mode under `xtask kernel`
the kernel also mounts a hosted emu's `styxlisten` export from the host
(10.0.2.2 in QEMU user networking) and lists it, checks a UDP echo server on
the host, announces `udp!*!6670` and echoes what the host sends there
through a UDP `hostfwd` (a datagram of 4000 bytes among them: smoltcp
fragments and reassembles IPv4, so a datagram may be up to 8164 bytes), and
exports its own `/` on `tcp!*!6666`, which the host mounts through a QEMU
`hostfwd`.

K3 adds the kernel's own devices, bound after `/dev` as in Inferno's native
kernels: `#r` serves the PL031 clock as `/dev/rtc` (read the time, write
seconds to set it), and `#t` serves the console UART as `/dev/eia0`, with
`eia0ctl` and `eia0status`. A guest that computes without making host calls
is still killed: wasmi runs guests on fuel and checks the prog's note between
slices, as the emu's wasmtime backend does with epoch interruption.

K3 storage, first pass: a host directory over virtio-9p. `#9/<tag>` is the
device's one 9P conversation as a file (writes take T-messages, reads give
R-messages), and the kernel mounts it with `#M` on `/n/host` at boot. QEMU
speaks only 9P2000.u and .L, so a translator (`purg_styx::dotu`) sits
between `#M`, which speaks 9P2000, and the device. In test mode the kernel
lists, reads, writes, renames, chmods and removes files there, kills a
prog whose read waits on a second, throttled device (the mount flushes the
read through the translator), and `xtask` checks the host side. The
`Tflush` reaches QEMU at once, but QEMU sends the `Rflush` only when the
flushed request has finished: its 9P server waits for a cancelled request
to complete. So a flush there takes as long as the request it flushes.

K3 storage, second pass: virtio-blk disks are `#S` (`/dev/sdV0/ctl` and
`data`, after `/dev`), and the kernel mounts the first one holding a purgfs
(`#K`, [docs/fs.md](docs/fs.md)) on `/n/disk`, interactively and in test
mode. purgfs is kfs-like with a journal: a crash never leaves the metadata
half changed. `disk/mkdisk` and `disk/chkdisk` make and check images (also
`cargo xtask diskimg`). In test mode the kernel works on an image `xtask`
builds (files, a rename, a large file, the disk filled to `file system
full`, `disk/chkdisk` on the raw disk), kills a reader of a throttled disk
in the middle of a request, and `xtask` checks the image on the host and
boots a second time to check that the files persist.

K4 SMP: the kernel starts every CPU the DTB lists (PSCI `CPU_ON`, under
TCG and KVM) and runs kernel threads, and so progs, on all of them from
one run queue. A CPU with nothing to run waits in `wfi`, and a wakeup
sends it an IPI. `--smp N` boots QEMU with N CPUs, and `--stress SECS`
boots once in a stress mode that runs the sh corpus and the conformance
guests in many progs at once for that long, with a watchdog and a leak
check. A prog's wasm memory, and a `#pdomain` domain's memory and number
of progs, can be limited with `memlimit` messages on `/prog/<pid>/ctl`
([docs/abi.md](docs/abi.md)); past a limit `memory.grow` fails, on the
emu as on the kernel.

```sh
cargo xtask kernel          # four TCG runs: GICv2, GICv3, and booting at EL2 on cortex-a72 and max
# The whole sh corpus runs on GICv2 and on max, a smoke subset on the other two
# (KERNEL_CORPUS=full runs it on all four). Runs boot side by side, one per two
# CPUs; KERNEL_JOBS=N sets how many.
cargo xtask kernel --smp 4  # the same with four CPUs each, so fewer side by side
cargo xtask kernel --kvm    # one run under KVM on an aarch64 Linux host with /dev/kvm
cargo xtask kernel --kvm --smp 4 --stress 3600   # an hour of the stress mode on four CPUs
cargo xtask initrd          # target/initrd/root.tar, the same archive the kernel compiles in
# an interactive shell on the kernel (^C interrupts, ^D at the start of a line ends it):
qemu-system-aarch64 -accel kvm -cpu host -M virt,gic-version=3 -m 512M -nographic \
  -netdev user,id=net0 -global virtio-mmio.force-legacy=off \
  -device virtio-net-device,netdev=net0 \
  -fsdev local,id=host,path=$HOME,security_model=none \
  -device virtio-9p-device,fsdev=host,mount_tag=host \
  -drive file=disk.img,if=none,id=disk,format=raw \
  -device virtio-blk-device,drive=disk \
  -append exit=psci -kernel target/aarch64-unknown-none/kernel/purg-kernel.img
# disk.img from: cargo xtask diskimg --size 64m disk.img [dir]
```

Each run must match `crates/purg-kernel/expected.out`. Under KVM,
semihosting does not work, so the kernel powers off through PSCI
(`-append exit=psci`) and the result comes from the serial output. KVM on
Apple silicon offers only GICv3 and no EL2, so CI uses the TCG runs.

`cargo xtask bench` runs the benchmark suite (`docs/perf.md`): `cat` of
a 100 MiB file from the RAM tree, through Styx mounts and from purgfs,
an `sh` loop, spawns, file operations and a CPU-bound guest, on a
release emu with each engine and on the kernel under KVM or TCG. It
prints medians and writes every sample to `target/bench/results.json`.
With `--kprof` the kernel boots with its profiler, `#T` (`kpctl`,
`kpdata`, as Inferno's kprof(3), and `kpcaller`), and xtask prints where
each I/O benchmark spent its time, by kernel function.

`xtask` builds the kernel and the probe without strict alignment, which
makes wasmi about three times faster, except for `purg-aarch64` and its
dependencies, which hold the code that runs with the MMU off. Each build
checks that this code calls nothing outside those crates
(`xtask/src/align.rs`). A plain `cargo build` of the kernel keeps strict
alignment throughout.

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

To serve a host directory over Lethe and attach it in the emu
(`docs/lethe.md`):

```sh
cargo xtask lethed -- -k root/lib/test/auth.key.pub /some/dir &
cargo xtask run-emu -- sh -c 'mkdir -p /tmp/l; lethe tcp!127.0.0.1!5640 /tmp/l; ls /tmp/l'
```

The same with no authentication and no encryption, which both ends must
ask for with `-A`. The server then answers as the user `none`. Use it on
a trusted link only:

```sh
cargo xtask lethed -- -A /some/dir &
cargo xtask run-emu -- sh -c 'mkdir -p /tmp/l; lethe -A tcp!127.0.0.1!5640 /tmp/l; ls /tmp/l'
```

You can also run the binary directly: `target/debug/emu [-r root] [--vm=wasmtime|wasmi] [cmd [args...]]`.
A command name `cmd` runs `/bin/cmd.wasm`. The exit code is 0 when the prog's
Inferno exit status is empty and 1 otherwise.

### Tests

```sh
cargo xtask build-root    # acceptance tests run the built apps
cargo test --workspace
cargo build --target aarch64-unknown-none \
  -p purg-abi -p purg-styx -p purg-auth -p purg-lethe -p purg-platform -p purg-core \
  -p purg-vm -p purg-vm-wasmi -p purg-aarch64
cargo xtask probe         # needs qemu-system-aarch64
cargo xtask probe --kvm   # the same under KVM on an aarch64 Linux host with /dev/kvm
RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR=target/loom \
  cargo test -p purg-core --features std --test loom --release
```

`cargo xtask probe` builds `purg-probe`, a bare-metal aarch64 image that
embeds `root/bin` and `root/lib` and runs the milestone's portable test
subset on `purg-core` and wasmi, with `#c` on the PL011 UART. Below the
shared core it is built from `purg-aarch64`, as the kernel is: the same
boot path and link script, the GIC and a 100 Hz timer from the DTB, and
preemptive kernel threads with their own stacks for its kprocs. Its `#I` is
smoltcp over virtio-net (`purg_aarch64::net`). Its `#U` is a writable RAM tree seeded with the embedded
files, so it boots the emu's namespace (`#U` after `#/`, `#c`, `#e`, `#p`).
It runs the ABI and procs conformance guests, then every script in the `sh`
corpus with its output captured and compared with the `.out` file. It boots the image with
`qemu-system-aarch64 -M virt -cpu cortex-a72 -nographic -semihosting`,
requires a semihosting exit code of 0 within 300 s, and compares the serial
output with `crates/purg-probe/expected.out`. `--kvm` boots it with
`-accel kvm -cpu host -M virt,gic-version=3 -append exit=psci` instead: KVM
has no semihosting, so the probe exits through PSCI `SYSTEM_OFF` and the
verdict comes from the serial output alone.

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
