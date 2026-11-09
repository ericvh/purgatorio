# Purgatorio application ABI (v0.1)

Apps are WASI preview1 command modules: they export `memory` and `_start`.
They may import from two modules:

- **`wasi_snapshot_preview1`**: used wherever WASI expresses Inferno
  semantics exactly. Because of this, Rust `std` (`std::fs`, `std::io`,
  `println!`, `std::env::args`, `std::time`) works unchanged.
- **`purgatorio`**: everything WASI cannot express faithfully, such as exact
  error strings, string exit status, full `Dir`s, namespaces, and later
  pctl, channels and spawn.

No non-standard functions are added to the `wasi_snapshot_preview1`
namespace. Modules built with the SDK also carry a `purgatorio.abi` custom
section containing `0.1`. The loader rejects a mismatched version. Modules
without the section, such as third-party WASI programs, are accepted.

The host implements both modules natively over the Purgatorio namespace in
`purg-vm`, which is `no_std`. Neither `wasi-common` nor `wasmtime-wasi` is
used, so the emu, the bare-metal probe and the kernel share one
implementation. `purg_vm::for_each_import!` is the authoritative import list,
and `cargo xtask build-root` rejects modules that import anything outside it.

## Conventions

- **One fd table.** WASI fds and `purgatorio` fds are the same integers from
  the prog's Fgrp. Fds 0, 1 and 2 are `/dev/cons`.
- **Preopens.** Fd 3 is `/`, the prog's namespace root. Fd 4 is `.`, which
  always resolves against the prog's *current* dot, so a future
  `purgatorio.chdir` affects relative paths in wasi-libc and Rust `std`.
  `path_open(dirfd, path)` resolves `path` with `namec` in the prog's
  namespace, so binds and unions apply.
- **Errors.** WASI calls return an errno, and the host also records the
  exact Inferno string as the prog's errstr. `purgatorio.errstr` returns that
  string. String-to-errno mapping: `file does not exist`→NOENT,
  `permission denied`→ACCES, `interrupted`→INTR,
  `i/o on hungup channel`→PIPE, `file already exists`→EXIST,
  `not a directory`→NOTDIR, `file is a directory`→ISDIR, bad or unusable
  fd→BADF, `bad address`→FAULT, bad argument or file name→INVAL,
  `file system full`→NOSPC, `not implemented`→NOTSUP,
  `seek on a stream`→SPIPE, and anything else→IO (the string is preserved).
- `purgatorio` calls return `-1` on error and set errstr, as in Inferno.
- Pointers are `u32` offsets into `memory`. Strings are `(ptr, len)` UTF-8.
  Every pointer is bounds-checked. An out-of-range pointer fails the call
  with EFAULT or `-1` and errstr `bad address`, and the guest keeps running.
- Seek whence values are the same in both worlds: 0, 1 and 2.

## Mapping of Inferno `Sys` onto the ABI

"M1" marks what this milestone implements. Unmarked rows follow the plan's
milestones.

| Sys call | Interface | Fit | Notes |
|---|---|---|---|
| `read`, `write` | WASI `fd_read`, `fd_write` (M1) | clean | One read per call, so short reads such as cons lines are kept. |
| `pread`, `pwrite` | WASI `fd_pread`, `fd_pwrite` | clean | |
| `seek` | WASI `fd_seek` (M1) | clean | Same whence values. |
| close | WASI `fd_close` (M1) | clean | |
| argv to `init` | WASI `args_get`, `args_sizes_get` (M1) | clean | |
| environment | WASI `environ_get`, `environ_sizes_get` (M1, empty) | clean at startup | Live changes go through `/env` files. |
| `millisec` | WASI `clock_time_get(MONOTONIC)` (M1) | clean | |
| `#c/random` | WASI `random_get` (M1) | clean | |
| `sleep` | WASI `poll_oneoff` (clock) | clean | |
| `open` | `path_open` (M1, read-only) | partial | WASI has no `ORCLOSE`, and Inferno's `OEXCL` differs. `purgatorio.open` will carry Inferno modes. |
| `create` | `path_open(O_CREAT)` | partial | No perm bits or `DMDIR`/`DMAPPEND`/`DMEXCL`. `purgatorio.create` (M2). |
| `stat`, `fstat` | `purgatorio.stat`, `purgatorio.fstat` (M1) | WASI lacks name, uid, gid, muid, mode and full qid | WASI `path_filestat_get` and `fd_filestat_get` are also implemented (M1), with `ino` set to `qid.path`. |
| `dirread` | `purgatorio.dirread` (M1) | WASI entries lack Dir fields | WASI `fd_readdir` is also implemented (M1). |
| `wstat`, `fwstat` | `purgatorio.wstat`, `purgatorio.fwstat` | partial in WASI | M2 |
| `remove` | WASI `path_unlink_file`, `path_remove_directory` | clean | M2 |
| `chdir` | `purgatorio.chdir` | missing in WASI | Updates the dot behind the `.` preopen. |
| `fd2path`, `dup`, `pipe`, `iounit` | `purgatorio.*` | missing or mismatched in WASI | `fd_renumber` moves an fd rather than duplicating it. |
| `bind`, `mount`, `unmount`, `pctl`, `export`, `fversion`, `fauth`, `file2chan`, `stream` | `purgatorio.*` | missing in WASI | M2–M5 |
| `werrstr`, `%r` | `purgatorio.errstr`, `purgatorio.werrstr` (M1) | WASI errnos are lossy | |
| exit status | `purgatorio.exits(status)` (M1) | WASI `proc_exit(i32)` is numeric | `proc_exit(0)` means `""` and `proc_exit(n)` means `"exit n"` (M1). |
| `spawn`, `chan of T`, `alt` | SDK tasks plus `purgatorio.spawn_prog` and `chan_*` | missing in WASI | M2–M3 |
| `dial`, `announce`, `listen` | SDK file operations on `/net` | WASI p1 sockets are not used | M4 |
| `load` | `purgatorio.load`, `purgatorio.call` | missing in WASI | M5 |
| `print`, `sprint`, `tokenize`, UTF helpers | Rust `std` and the SDK | library | No host call. |

## `purgatorio` imports in v0.1

```
errstr(buf, blen) -> n            copies the errstr, truncated to blen
werrstr(ptr, len) -> 0
exits(status, slen) -> !          "" is success
stat(path, plen, buf, blen) -> n  one Dir in 9P stat wire format
fstat(fd, buf, blen) -> n
dirread(fd, buf, blen) -> n       whole packed Dirs; 0 at end
exec(path, plen, argv, alen) -> -1 on bad arguments, else does not return
```

`exec` replaces the running module in the same prog: same fds, namespace and
pid. The path is resolved like a command name, so `cat` means
`/bin/cat.wasm`. `argv` is a sequence of NUL-terminated strings. `exec` is
the M1 shortcut that `emuinit` uses until `spawn_prog` exists in M2.

## WASI p1 functions implemented in M1

`args_get`, `args_sizes_get`, `environ_get`, `environ_sizes_get`, `fd_read`,
`fd_write`, `fd_close`, `fd_seek`, `fd_fdstat_get`, `fd_prestat_get`,
`fd_prestat_dir_name`, `fd_filestat_get`, `fd_readdir`, `path_open`
(read-only), `path_filestat_get`, `clock_time_get`, `random_get`,
`proc_exit`.
