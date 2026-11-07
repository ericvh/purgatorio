# Purgatorio application ABI (v0.2)

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
section containing `0.2`. The loader rejects a mismatched version. Modules
without the section, such as third-party WASI programs, are accepted.

The host implements both modules natively over the Purgatorio namespace in
`purg-vm`, which is `no_std`. Neither `wasi-common` nor `wasmtime-wasi` is
used, so the emu, the bare-metal probe and the kernel share one
implementation. `purg_vm::for_each_import!` is the authoritative import list,
and `cargo xtask build-root` rejects modules that import anything outside it.

## Conventions

- **One fd table.** WASI fds and `purgatorio` fds are the same integers from
  the prog's Fgrp. Fds 0, 1 and 2 are `/dev/cons`.
- **Preopens.** Fd 3 is `.`, which always resolves against the prog's
  *current* dot. Fd 4 is `/`, the prog's namespace root. wasi-libc strips
  both names to an empty prefix and prefers the preopen it registered last,
  so `/` comes second and absolute names resolve from the root. Both numbers are
  reserved in every prog: `pctl(NEWFD)` and `spawn_prog` reinstall them, and
  an fd map may not target them. `path_open(dirfd, path)` resolves `path`
  with `namec` in the prog's namespace, so binds and unions apply.
- **Current directory.** wasi-libc keeps its own copy of the current
  directory (initially `/`) and turns relative names into absolute ones
  under the `/` preopen. The SDK therefore sets it from `fd2path(3)` at
  start-up and after `chdir` and `pctl(NEWNS|FORKNS)`. Third-party WASI
  programs see relative names resolved from `/`.
- **Errors.** WASI calls return an errno, and the host also records the
  exact Inferno string as the prog's errstr. `purgatorio.errstr` returns that
  string. String-to-errno mapping: `file does not exist`→NOENT,
  `permission denied`→ACCES, `interrupted`→INTR,
  `i/o on hungup channel`→PIPE, `file already exists`→EXIST,
  `not a directory`→NOTDIR, `file is a directory`→ISDIR, bad or unusable
  fd→BADF, `bad address`→FAULT, bad argument or file name→INVAL,
  `file system full`→NOSPC, `not implemented`→NOTSUP,
  `seek on a stream`→SPIPE, `directory not empty`→NOTEMPTY,
  `rename across directories`→XDEV, and anything else→IO (the string is preserved).
- `purgatorio` calls return `-1` on error and set errstr, as in Inferno.
- Pointers are `u32` offsets into `memory`. Strings are `(ptr, len)` UTF-8.
  Every pointer is bounds-checked. An out-of-range pointer fails the call
  with EFAULT or `-1` and errstr `bad address`, and the guest keeps running.
- Seek whence values are the same in both worlds: 0, 1 and 2.

## Mapping of Inferno `Sys` onto the ABI

"M1" and "M2" mark what is implemented. Unmarked rows follow the plan's
milestones.

| Sys call | Interface | Fit | Notes |
|---|---|---|---|
| `read`, `write` | WASI `fd_read`, `fd_write` (M1) | clean | One read per call, so short reads such as cons lines are kept. |
| `pread`, `pwrite` | WASI `fd_pread`, `fd_pwrite` (M2) | clean | |
| `seek` | WASI `fd_seek` (M1) | clean | Same whence values. |
| close | WASI `fd_close` (M1) | clean | |
| argv to `init` | WASI `args_get`, `args_sizes_get` (M1) | clean | |
| environment | WASI `environ_get`, `environ_sizes_get` (M1; the Egrp from M2) | clean at startup | Live changes go through `/env` files. |
| `millisec` | WASI `clock_time_get(MONOTONIC)` (M1) | clean | |
| `#c/random` | WASI `random_get` (M1) | clean | |
| `sleep` | WASI `poll_oneoff` (clock, M2) | clean | fd subscriptions report ready at once. |
| `open` | `purgatorio.open` (M2); WASI `path_open` (M1) | partial in WASI | WASI has no `ORCLOSE`, and Inferno's `OEXCL` differs. `path_open` supports `O_CREAT`, `O_EXCL`, `O_TRUNC`, `O_DIRECTORY` and append (M2). |
| `create` | `purgatorio.create` (M2); WASI `path_open(O_CREAT)`, `path_create_directory` (M2) | partial in WASI | WASI has no perm bits or `DMDIR`/`DMAPPEND`/`DMEXCL`. |
| `stat`, `fstat` | `purgatorio.stat`, `purgatorio.fstat` (M1) | WASI lacks name, uid, gid, muid, mode and full qid | WASI `path_filestat_get` and `fd_filestat_get` are also implemented (M1), with `ino` set to `qid.path`. |
| `dirread` | `purgatorio.dirread` (M1) | WASI entries lack Dir fields | WASI `fd_readdir` is also implemented (M1). |
| `wstat`, `fwstat` | `purgatorio.wstat`, `purgatorio.fwstat` (M2) | partial in WASI | WASI `fd_filestat_set_size`, `fd_filestat_set_times`, `path_filestat_set_times` and `path_rename` map onto wstat (M2). A rename is a wstat of the name, so it stays within one directory; WASI renames across directories fail with `EXDEV`. |
| `remove` | `purgatorio.remove`; WASI `path_unlink_file`, `path_remove_directory` (M2) | clean | |
| `chdir` | `purgatorio.chdir` (M2) | missing in WASI | Updates the dot behind the `.` preopen. |
| `fd2path`, `dup`, `pipe`, `iounit` | `purgatorio.*` (M2) | missing or mismatched in WASI | `fd_renumber` (M2) moves an fd rather than duplicating it. `iounit` is 0 (no limit) until a device sets one. |
| `bind`, `unmount`, `pctl` | `purgatorio.*` (M2) | missing in WASI | |
| `mount`, `export`, `fversion`, `fauth`, `file2chan`, `stream` | `purgatorio.*` | missing in WASI | M3–M5 |
| `werrstr`, `%r` | `purgatorio.errstr`, `purgatorio.werrstr` (M1) | WASI errnos are lossy | |
| exit status | `purgatorio.exits(status)` (M1) | WASI `proc_exit(i32)` is numeric | `proc_exit(0)` means `""` and `proc_exit(n)` means `"exit n"` (M1). |
| `spawn` (of a command) | `purgatorio.spawn_prog` (M2) | missing in WASI | Exit messages are read from `/prog/<pid>/wait`. |
| `spawn` (in-module), `chan of T`, `alt` | SDK tasks and `chan_*` | missing in WASI | M2: `purgatorio::task` runs tasks cooperatively in one instance with `Chan<T>` and needs no imports. Kernel channels (`chan_*`) and `alt` come in M3. |
| `dial`, `announce`, `listen` | SDK file operations on `/net` | WASI p1 sockets are not used | M4 |
| `load` | `purgatorio.load`, `purgatorio.call` | missing in WASI | M5 |
| `print`, `sprint`, `tokenize`, UTF helpers | Rust `std` and the SDK | library | No host call. |

## `purgatorio` imports in v0.2

```
errstr(buf, blen) -> n            copies the errstr, truncated to blen
werrstr(ptr, len) -> 0
exits(status, slen) -> !          "" is success
stat(path, plen, buf, blen) -> n  one Dir in 9P stat wire format
fstat(fd, buf, blen) -> n
dirread(fd, buf, blen) -> n       whole packed Dirs; 0 at end
open(path, plen, mode) -> fd      Inferno open mode (OREAD..OEXEC|OTRUNC|ORCLOSE)
create(path, plen, mode, perm) -> fd
wstat(path, plen, dir, dlen) -> 0 dir is a packed Dir; ~0 and "" fields are untouched
fwstat(fd, dir, dlen) -> 0
remove(path, plen) -> 0
fd2path(fd, buf, blen) -> n       fd 3 gives the current directory
chdir(path, plen) -> 0
dup(old, new) -> fd               new = -1 picks the lowest free fd
pipe(fds) -> 0                    writes two i32 fds at fds
iounit(fd) -> n
bind(name, nlen, old, olen, flags) -> 0
unmount(name, nlen, old, olen) -> 0     nlen = 0 removes everything at old
pctl(flags, movefd, n) -> pid     movefd: n i32 fds that NEWFD keeps
spawn_prog(path, plen, argv, alen, flags, fdmap, n) -> pid
```

All of these return -1 and set errstr on failure.

`spawn_prog` starts a command (resolved like a shell command name, so `cat`
means `/bin/cat.wasm`) as a new prog on its own kernel process. `argv` is a
sequence of NUL-terminated strings. The module is loaded through the
caller's namespace before the child starts, so a missing or invalid module
is the caller's error. The child shares the caller's namespace, fd table
and environment, then applies `flags` as `pctl` would. `fdmap` is `n` pairs
of i32 `(caller fd, child fd)` copied into the child; a non-empty map
implies `FORKFD` (unless `NEWFD` is given), so the caller's table is never
changed. Fds 3 and 4 are reinstalled in the child and cannot be mapped.

When a prog ends, its fds are closed and then its parent can read one exit
message from `/prog/<parent pid>/wait`: `pid name status`, where `name` is
the module's base name without `.wasm` and `status` is the exit status. A
field that is empty or contains white space or a quote is quoted as
Inferno's `%q` does. `wait` blocks until a child has exited. A failed child
start (such as a missing module) produces no message.

`exec` from v0.1 is gone; `emuinit` now spawns its command and waits for it.

## WASI p1 functions implemented

M1: `args_get`, `args_sizes_get`, `environ_get`, `environ_sizes_get`,
`fd_read`, `fd_write`, `fd_close`, `fd_seek`, `fd_fdstat_get`,
`fd_prestat_get`, `fd_prestat_dir_name`, `fd_filestat_get`, `fd_readdir`,
`path_open`, `path_filestat_get`, `clock_time_get`, `random_get`,
`proc_exit`.

M2: `fd_pread`, `fd_pwrite`, `fd_tell`, `fd_fdstat_set_flags` (clearing
only), `fd_filestat_set_size`, `fd_filestat_set_times`, `fd_sync`,
`fd_datasync`, `fd_renumber`, `path_create_directory`,
`path_remove_directory`, `path_unlink_file`, `path_rename`,
`path_filestat_set_times`, `path_readlink` (always `ENOTSUP`: Inferno has no
links), `poll_oneoff`, `sched_yield`. `environ_get` returns the prog's
environment group (`/env`) as it was when the prog started.
