# Purgatorio application ABI (v0.9)

Apps are WASI preview1 command modules: they export `memory` and `_start`.
They may import from two modules:

- **`wasi_snapshot_preview1`**: used wherever WASI expresses Inferno
  semantics exactly. Because of this, Rust `std` (`std::fs`, `std::io`,
  `println!`, `std::env::args`, `std::time`) works unchanged.
- **`purgatorio`**: everything WASI cannot express faithfully, such as exact
  error strings, string exit status, full `Dir`s, namespaces, pctl,
  progs, kernel channels, Styx plumbing and module loading.

No non-standard functions are added to the `wasi_snapshot_preview1`
namespace. Modules built with the SDK also carry a `purgatorio.abi` custom
section containing `0.9`. The loader rejects a mismatched version. Modules
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
- **Device names.** A name that starts with `#` is a device name, as in
  `namec`. Since wasi-libc makes every relative name absolute before the
  host sees it, the host takes a `#` element right after dot back out:
  `#c/sysname` with dot `/usr/me` arrives as `usr/me/#c/sysname` and is
  read as `#c/sysname`. This has two limits. First, the joined name cannot
  be told apart from a real file whose name starts with `#` in dot, or in
  `/` (where `/#x` arrives as `#x`), so such a file cannot be opened through
  WASI or std. The `purgatorio` module's `open`, `create`, `stat` and the
  rest take the name as given, so `purgatorio::open("/#x", ...)` still
  reaches the file. Second, a
  stock WASI program (no SDK) that calls `chdir`
  (`std::env::set_current_dir`) changes only libc's copy of the current
  directory, not the prog's dot, so its `#` names relative to the new
  directory are not recognised. Programs built with the SDK use
  `purgatorio::chdir`, which keeps both in step.
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

Each row is marked with the milestone that implemented it.

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
| `mount`, `export`, `fversion`, `file2chan` | `purgatorio.*` (M3) | missing in WASI | `mount` and `export` authenticate (M5); see below. |
| `fauth` | `purgatorio.fauth` (v0.5) | missing in WASI | Returns the auth fd that `mount` then takes as `afd`. With `afd = -1`, `mount` still authenticates by itself. |
| `stream` | `purgatorio.stream` (v0.5) | missing in WASI | The copy loop runs in the kernel. |
| `werrstr`, `%r` | `purgatorio.errstr`, `purgatorio.werrstr` (M1) | WASI errnos are lossy | |
| exit status | `purgatorio.exits(status)` (M1) | WASI `proc_exit(i32)` is numeric | `proc_exit(0)` means `""` and `proc_exit(n)` means `"exit n"` (M1). |
| `spawn` (of a command) | `purgatorio.spawn_prog` (M2) | missing in WASI | Exit messages are read from `/prog/<pid>/wait`. |
| `spawn` (in-module), `chan of T`, `alt` | SDK tasks and `chan_*` | missing in WASI | M2: `purgatorio::task` runs tasks cooperatively in one instance with `Chan<T>` and needs no imports. Kernel channels (`chan_*`), `alt` and `wait` (M3) carry messages and fds between progs. |
| `dial`, `announce`, `listen` | SDK file operations on `/net` (M4) | WASI p1 sockets cannot connect or bind | WASI `sock_*` (M5) work on fds from `/net`, so stock programs can serve connections. |
| `load` | `purgatorio.load`, `purgatorio.call` (M5) | missing in WASI | See below. |
| `print`, `sprint`, `tokenize`, UTF helpers | Rust `std` and the SDK | library | No host call. |

## `purgatorio` imports in v0.9

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
mount(fd, afd, old, olen, flags, aname, alen) -> 0     afd = -1: mount authenticates
export(fd, dir, dlen, flags) -> 0
export_key(fd, dir, dlen, flags, key, klen) -> 0     export -k: the server proof's authinfo (v0.9)
fversion(fd, msize, ver, vlen) -> n
fauth(fd, aname, alen) -> afd     Tauth on fd's conversation (v0.5)
stream(src, dst, bufsiz) -> n     copy src to dst in the kernel (v0.5)
file2chan(dir, dlen, name, nlen, handles) -> 0
chan_new(cap) -> h
chan_close(h) -> 0
chan_send(h, buf, n, handles, nh) -> 0
chan_recv(h, buf, blen, handles, maxh) -> n
alt(ops, nops, block) -> index
wait(ops, nops, timeout_ms) -> index
load(path, plen) -> handle        instantiate a reactor module in this prog
call(handle, name, nlen, args, alen, out, olen) -> n
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

`file2chan` writes the server's read and write channel handles at
`handles`. Each request is a message on one of them, in the format of
`purg_abi::fio`: a read (offset, count, fid, reply handle), a write
(offset, fid, reply handle, data) or a truncate. When a client closes the
file, a write with no data and reply handle -1 arrives on the write
channel for that fid, as Inferno sends a write with a nil reply channel.
It takes no reply, and is dropped if the server's channel is full. The
message format and the host calls are unchanged, so the ABI version is
too. The SDK's `FileIO::recv` skips closes; `FileIO::recv_with_close`
returns them, and `sh`'s `file2chan` runs its close command for each.

### Module digests and signatures

Every module loaded by `spawn_prog` or `load` gets a digest, and a prog may
be under a module policy that refuses unsigned or untrusted modules
(`docs/signing.md`). None of this adds a host call, so the ABI version is
unchanged. It is all files:

- A module may carry a `purgatorio.sig` custom section, which the loader
  ignores under the default policy.
- `/prog/<pid>/module` lists the prog's policy and its modules: `policy
  <level>`, then `module <digest> <signers> <path>` for the command and
  each loaded reactor.
- Writing `modules warn` or `modules require` to `/prog/<pid>/ctl`
  tightens the policy. Under `require`, `spawn_prog` and `load` fail with
  `module not signed`, `bad signature`, `untrusted signer`, `certificate
  revoked`, `certificate expired` or `module revoked`, after the module's
  path.

### Debugging files

Call stacks and profiles (`docs/debug.md`) are also files and `ctl`
messages. No host call is added and the ABI version is unchanged.

- Writing `trace on` (or `trace off`) to `/prog/<pid>/ctl` sets whether
  the commands that prog starts from then on keep a call stack. `prof on`
  (or `prof off`) does the same for sampling, implies `trace on`, and
  makes the prog collect the samples of its children as they exit. Both
  are inherited by the progs started afterwards.
- `/prog/<pid>/stack`: for a traced prog, a line `<function index>
  <path>` per frame, the innermost first, as of its last host call or
  check point; `... <n>` ends a stack with more than 256 frames. Reading
  it for a prog that is not traced fails with `prog is not traced`.
- `/prog/<pid>/exception`: the trap that broke the prog, or empty.
- `/prog/<pid>/prof`: a line `<samples> <function index> <path>` per
  sampled function of the prog and of its exited children.
- `cprof on|off` and `mprof on|off` on `ctl` are as `prof on|off`, for
  call counts and for the pages of memory grown. `/prog/<pid>/cprof`
  and `/prog/<pid>/mprof` have the same lines as `prof`: `<calls>` or
  `<pages> <function index> <path>`.
- `stop` on `ctl` asks a traced prog to stop at its next host call or
  check point; it is then in state `debug`. `start` lets it go on, and
  `step` lets it go on until it enters a function; both return once it
  has left the stop. `bpt set <func>` and `bpt del <func>` set and clear
  a breakpoint on entry to function index `<func>` of its command
  module. `stop` and `bpt` on a prog that is not traced fail with `prog
  is not traced`, `step` on a running one with `prog not stopped`.
- `/prog/<pid>/heap`: the linear memory of a stopped prog, read and
  written at the file offset; a read past the end returns nothing. A
  running prog's fails with `prog not stopped`.
- A traced prog that traps stays in `/prog` with state `broken`, without
  its files and namespace, until `kill` is written to its `ctl`. Its
  parent gets the exit message when it traps.
- A traced prog's command module is rewritten in memory before it runs:
  it gets a second memory, exported as `purgatorio.stack`. A guest cannot
  tell, except that it can nest calls half as deep, and that it waits
  at a breakpoint.

### A restricted `/prog`: `#pdomain`

`#p` with the attach spec `domain` is a view of the prog table limited to
one prog domain. Inferno has no such view; it is how `mcpd` keeps a
session from seeing or killing progs that are not its own. No host call
is added and the ABI version is unchanged.

- Attaching `#pdomain` founds a domain for the attaching prog's process
  group: every prog then in that group joins it, and every prog started
  from a member afterwards is a member too, whatever its own process
  group. A prog never leaves a domain.
- The view lists only members. Any other pid does not exist in it:
  walking to it, and so `ls`, `kill` or a write to its `ctl`, fails with
  `file does not exist`. `killgrp` written to a member's `ctl` kills the
  members of that group, not the rest of it.
- Domains nest. A member that attaches `#pdomain` founds a domain inside
  its own; the outer view still shows its members.
- `#pdomain=N` is the view of domain `N` (the process group that founded
  it) without joining it. The prog that started a sandbox uses it to find
  every prog left in the sandbox, whatever their groups and whether or not
  their parents live, and kill them. A prog that may attach `#p` at all
  can see the whole table, so nothing more is checked.
- `#p` with no spec is the whole table, as before. Any other spec fails
  with `bad arg in system call`.
- `NODEVS` forbids `#p` with any spec. A sandbox binds `#pdomain` on
  `/prog` and then gives up devices, and from then on nothing it runs can
  get the whole table back.
- Each domain has its own `#s` registry. A file2chan file belongs to the
  innermost domain of the prog that made it, and an attach of `#s` shows
  only the files of the attaching prog's innermost domain (or, for a prog
  in no domain, those made outside every domain). Names are unique within
  a registry. `NODEVS` still allows `#s`, as in Inferno, so file2chan
  works in a sandbox, but the sandbox cannot see or open what the host or
  another sandbox posted. A file already bound into a namespace stays
  usable through that binding.

### Dropping single devices: `nodev`

Writing `nodev <chars>` to `/prog/<pid>/ctl` forbids that prog's
namespace to attach the devices named by `<chars>` (their device
characters, such as `¤` or `¤s`), whether or not `NODEVS` is set. Copies
of the namespace made afterwards (`FORKNS`, `NEWNS`) keep the list, and
there is no way to take a device back. An attach of a dropped device
fails with `mount/attach disallowed`. Inferno has no such message; it is
how `mcpd` keeps a session from `#¤`, which `NODEVS` allows. It is a
`ctl` message, so the ABI version is unchanged.

### Memory and prog limits (K4)

A prog's wasm memory can be limited, and so can a domain's. Inferno has
no direct analogue (its pools are system-wide). The limits are `ctl`
messages and a file, not host calls, and what a guest sees when one is
reached is ordinary wasm behavior, so the ABI version is unchanged.

- Writing `memlimit N` to `/prog/<pid>/ctl` limits the wasm memory the
  prog holds to `N` bytes: its linear memories and its tables, at 8 bytes
  a table element. `N` may end in `k`, `m` or `g` (2^10, 2^20, 2^30).
  The limit applies to growth from then on, and every prog the prog
  starts afterwards gets the same limit for itself.
- `domain memlimit N` limits the memory all members of the prog's
  innermost domain (`#pdomain`) hold between them, and `domain proglimit
  N` the number of members. A spawn that would pass the prog limit fails
  with `no free procs`; spawns into a domain with a prog limit are
  admitted one at a time, so two cannot both take the last place. A prog
  in no domain gets `bad arg in system call`. A prog is held to its own limit and to the limit of every domain it is
  in, so a domain founded inside a limited one cannot escape it.
- A limit can only be lowered. Writing a higher one fails with
  `permission denied` and changes nothing.
- `/prog/<pid>/mem` reads `<bytes held> <limit>`, with `-` for no limit.

Past a limit, `memory.grow` returns -1, as the wasm spec allows. The
guest decides what that means. Rust's `std` allocator then calls
`handle_alloc_error`: the prog prints `memory allocation of N bytes
failed` on its stderr and aborts, and its parent sees the status ``trap:
wasm `unreachable` instruction executed`` under wasmi (wasmtime says
``trap: wasm trap: wasm `unreachable` instruction executed``). A module whose initial memory is already past the
limit fails to instantiate. Both engines enforce the limits through
their resource limiters (`purg_vm::memory_growing`), so the emu and the
kernel behave alike.

Not counted: the prog's kernel thread stack, the engine's compiled code,
and what the prog's files hold in the kernel (a RAM file system grows
with what is written to it, and so does `#e`, where sh keeps its
variables). A domain's prog limit bounds the first two.

`load` and `call` (v0.4) are registered by each engine rather than in the
shared host functions, because they need the engine's store. `load`
instantiates a reactor module (no `_start`; loadable modules live in
`/bin/lib`) inside the calling prog, so it shares the prog's fds and
namespace. `call` runs export `name`, which has type `(i32, i32) -> i32`.
The host writes `args` into a fresh page of the module's memory, the export
overwrites that page with its result and returns the result length, and the
host copies up to `olen` bytes of it to `out`.

### Authentication (M5; version 2 proofs in v0.6, version 3 and encryption in v0.7, server proofs in v0.8, no trust by default in v0.9)

A mount authenticates before `Tattach` with an Ed25519 signature over a
challenge from the server. This is not Inferno's keyring protocol, so real
Inferno peers mount with `-A`. `docs/auth.md` has the formats, the tools
and a security review.

- The server trusts keys it holds directly (the 32-byte
  `/usr/<uname>/key.pub`, and the `<user> <key>` lines of
  `/lib/keys/users` for `uname` or `*`) and certificates from the signers
  listed in `/lib/keys/signers`, unless a key is in `/lib/keys/revoked`.
  These are read in the exporter's own namespace, never from under the
  exported tree. The stock root trusts nothing. Since v0.9 an export
  without `MNOAUTH` that trusts no key and no signer answers `Tauth` and
  `Tattach` with `server trusts no keys` (before, it answered
  "authentication not required" and let everyone in); an export with
  `MNOAUTH` answers `Tauth` with "authentication not required", as
  Inferno does (`docs/auth.md`, "Trust by default").
- The client writes its proof to the auth fid in one write. A version 1
  proof is exactly 96 bytes, `signature[64] || public_key[32]` over the raw
  challenge, and is accepted only for directly held keys. A version 2
  proof is `0x02 || kind || signature[64] || body`: the body is the public
  key (kind 0) or a `pgcert1` certificate (kind 1), and the signature
  covers a context string, the challenge, the `Tauth` uname and aname, the
  kind and the body. A rejected proof fails the write with `authentication
  failed`, `certificate expired`, `certificate revoked`, `untrusted signer`
  or `certificate name mismatch`.
- Since v0.7 the server offers `challenge[32] || server_key[32]`, an
  ephemeral X25519 key, and clients write a version 3 proof, `0x03 ||
  kind || signature[64] || client_key[32] || body`, whose signature also
  covers both ephemeral keys. Both ends then derive a key per direction,
  and every Styx message after the proof travels as a ChaCha20-Poly1305
  record. A client refuses a server that offers no key (`no session key
  offered`). Versions 1 and 2 are still accepted from older guests, and
  their conversations stay in the clear.
- The client's key comes from `#¤` (with a certificate loaded by `cert
  ...` on its `ctl`), an authinfo in `/usr/<user>/keyring/default`, or a
  seed in `/usr/<user>/key` (or the public test key `/lib/test/auth.key`,
  which no stock server trusts). The SDK also asks the guest factotum at
  `/mnt/factotum` first.
- Since v0.8 the server proves itself in turn: once a version 3 proof is
  accepted, the auth fid reads at offset 64
  (`purg_abi::auth::SERVER_PROOF_OFFSET`) as `kind || signature[64] ||
  body`, signed with the exporting user's key (or the authinfo given to
  `export_key`, or the guest factotum's key) over the session's
  transcript hash and a value bound to its keys (`docs/auth.md`, "Server
  authentication"). `MSRVAUTH` (`0x4000_0000`, `mount -S`) makes the
  mount read and check it against every key in `/lib/keys/servers`
  (whatever host its line names) and `/lib/keys/serversigners` (or,
  when there is no such file, `/lib/keys/signers`) in the
  mounter's namespace, before the attach, both with `afd = -1` and with
  an `afd`. No proof fails with `server did not authenticate`, a proof
  that does not check with `server authentication failed`. `MSRVAUTH`
  with `MNOAUTH` is `bad arg in system call`.
- The host a server proves is checked in the guest (`docs/auth.md`,
  "Servers per host"). Since v0.9 a read of the auth fd at offset 64 or
  later, once the conversation is encrypted, gives the server's proof as
  the kernel first read it from the server; every later read, and the
  `MSRVAUTH` check of a mount with that `afd`, use the same bytes. The
  SDK's `amount_host` reads it, checks the key or certificate for the
  host dialled, then mounts with `MSRVAUTH`.
- The `Tattach` must name the same uname and aname as the `Tauth`.
- `MNOAUTH` (`0x8000_0000`, the `-A` flag) skips `Tauth`. A server that
  authenticates then rejects the attach.
- A conversation that a version 3 proof switched to records, even a
  refused one, cannot be started again in the clear: `mount` with `afd =
  -1` and `fauth` on its fd fail with `conversation is encrypted` (v0.8).
- A conversation whose session is still alive (an auth fd from `fauth`
  still open, or a mount on it) cannot be versioned again: `mount` with
  `afd = -1` and `fauth` on its fd fail with `device or object already in
  use` (v0.9). Before, a second session raced the first one's reader for
  the replies.
- With `afd = -1`, `mount` does all of this itself. A guest can instead run
  the exchange with `fauth` (below) and pass the auth fd to `mount`.
  `MNOAUTH` is ignored when an `afd` is given.

### Users

Each prog acts as a user, as each Inferno process has `up->env->user`.
The first prog acts as the host owner: the platform's user (the login
name that started the emu; `inferno` on the probe and the kernel). A
spawned prog starts with its parent's user. No import changes; the user
is read and set through `#c/user`:

- A read of `/dev/user` gives the calling prog's user, with no newline.
- A write of a name at offset 0 makes the calling prog act as that user
  from then on, and the progs it starts afterwards. Progs it started
  before keep theirs. A trailing newline is dropped.
- Only a prog acting as the host owner may write it. Anyone else gets
  `permission denied`, even for its own name, so a prog that has become
  another user cannot become the host owner again. This is Inferno's
  rule (`iseve()` in devcons).
- A name must be a single path element of at most 64 bytes with no white
  space or control characters; else `bad arg in system call`.

The user is the uname a mount authenticates and attaches as, and whose
`/usr/<user>/keyring/default` and `/usr/<user>/key` it proves with; the
user an `export` serves as and signs its server proof as; the user in
`/prog/<pid>/status` and the owner of a prog's `/prog/<pid>` files; and
the owner of files a prog creates in a RAM tree (the probe's and the
kernel's root). Only the host owner may open `#¤/ctl` and `#¤/rpc`
(mode 0600), since `#¤` holds the host owner's key, and only a prog's
own user, or the host owner, may open its `/prog/<pid>/ctl`. On the emu
the host file tree is still read and written with the host's
permissions for the host user who runs it. `runas user cmd` is the
command for this (`docs/auth.md`, "Users").

### Read-only `export`

`export_key(fd, dir, dlen, flags, key, klen)` (v0.9) is `export` whose
server proof is signed with the authinfo at path `key` in the exporter's
namespace (`export -k`); an empty `key` is `export`.

`export(fd, dir, dlen, flags)` takes `MNOAUTH`, `RDONLY`
(`0x4000_0000`, `purg_abi::export::RDONLY`) and, since v0.8, `ENCRYPT`
(`0x2000_0000`, `export -e`): an encrypting export answers a version 1 or
2 proof, and an attach without a version 3 proof, with `encryption
required`. `ENCRYPT` with `MNOAUTH` is `bad arg in system call`. A
read-only export serves
walks, stats and reads. It answers `permission denied` to an open for
writing, with `OTRUNC` or with `ORCLOSE`, and to create, write, remove and
wstat. The `export` command sets it with `-r`. This is how `mcpd` shows
`/bin` and `/lib` to a session without letting it change them.

`purgatorio.wait` accepts a fd open on `/prog/<pid>/wait` as a read
operation: it is ready when an exit message is queued.

### `fauth` (v0.5)

`fauth(fd, aname, alen)` versions the 9P conversation on `fd` and sends
`Tauth` for the caller's user and `aname`. It returns a new fd on the auth
file. Since v0.7 a read of at least 96 bytes (`purg_abi::auth::SIGNED`) at
offset 0 gives the server's offer followed by an ephemeral key the kernel
made for this client: the bytes a version 3 proof signs. The kernel keeps
the secret half. Writing that proof switches the conversation to records,
and the kernel does the rest. A read of 32 bytes gives the bare challenge,
for a version 1 or 2 proof and a conversation in the clear; a read of 96
that returns fewer bytes means the server offers no key. Since v0.8 the
kernel makes one ephemeral key per auth fd: every read of the offer, from
any thread, returns the same 96 bytes, so a read beside the write of the
proof cannot change the key under it. Writing a proof
(above) completes the exchange. A wrong proof fails that write. The
caller then calls `mount(fd, afd, ...)` with the same `aname`, which
attaches with the auth fid. An `afd` that did not authenticate makes the attach fail with
`authentication required`. An `afd` that is not an auth fd, or that belongs
to a different conversation, fails with `inappropriate use of fd`.

As in Inferno, a server that does not authenticate makes `fauth` fail with
`authentication not required`. Then mount with `afd = -1`. That `fauth`
leaves nothing running on `fd`. Only one `fauth` may be outstanding on a
conversation. After a successful `fauth`, mount that conversation with the
`afd` and not with `afd = -1`.

The SDK has `purgatorio::fauth`, plus `purgatorio::auth::{proof, proof_for,
authenticate, authenticate_for, amount, amount_key}`. `proof_for` builds a
version 3 proof (or version 2, given a bare challenge) from the sources
above, or from an authinfo given as `keyfile`. `authenticate` reads 96
bytes and fails with `no session key offered` when fewer come. `amount` is Inferno's `auth->amount`: fauth, authenticate, then
mount, falling back to a plain mount when the server does not
authenticate. `mount` and `import` use it, with `-k keyfile` for
`amount_key`.

### `stream` (v0.5)

`stream(src, dst, bufsiz)` reads `src` into a buffer of `bufsiz` bytes
(clamped to 1 MiB). It writes the buffer to `dst` each time it fills, and
once more with whatever is left at the end. It stops at end of file, at a
failed read or write, or when the prog is killed. It returns the number of
bytes written to `dst`. This follows Inferno's `stream`:

- An error is not an error of the call. The count is still returned,
  and the errstr is set to the read or write error.
- A write of a full buffer that fails or is short ends the copy. That
  buffer is not counted. (Inferno counts the final partial buffer even if
  its write fails. Here only what `dst` accepted is counted.)
- A prog blocked reading in `stream` is ended by a kill, as one blocked in
  a `read` is (see Killed progs below). What it had written stays
  written.
- `bufsiz = 0` copies nothing and returns 0. A negative `bufsiz` or a bad
  fd returns -1.

## Killed progs

A kill (`kill` or `killgrp` on `/prog/<pid>/ctl`, or `proc_raise`) ends
the prog, as in Inferno. It is not an error handed back for the guest to
handle:

- A host call the prog is blocked in (a read, `wait`, a sleep in
  `poll_oneoff`, `spawn_prog` loading a module, and so on) is interrupted
  and does not return to the guest: the guest is unwound there.
- A killed prog makes no further host call: the next one unwinds it
  instead of running.
- A prog that computes without host calls is stopped at the VM's next
  check of the kill, every 10 ms under wasmtime (epoch interruption) and
  every 2^20 units of fuel under wasmi.

Its parent's exit message has the status `killed`: `pid name killed`. A
child that a killed `spawn_prog` had not yet started is not started.

There is no handler a guest can install to catch a kill. A guest still
sees `interrupted` from a call when a file server returns that error.

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

M5: the rest of preview 1, so stock `wasm32-wasip1` binaries link and run.
`fd_advise` checks its arguments and does nothing. `fd_allocate` grows the
file to cover the range. `fd_fdstat_set_rights` accepts any open fd, since
rights are not tracked. `path_link` and `path_symlink` return `NOTSUP`
(Inferno has no links). `clock_res_get` reports the clock resolution.
`proc_raise` kills the prog, as a kill from outside does (see Killed
progs). `sock_accept` on an Inferno `listen` file returns the new
conversation's `data` fd, and `sock_recv`, `sock_send` and `sock_shutdown`
work on such fds.
