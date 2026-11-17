# Threads

A guest can be a threaded module: several threads on one shared heap,
with `std::thread`, `Mutex`, `Condvar` and channels as Rust has them.
This is plan decision D4 (issue #52), in its first cut: it works on the
emu under wasmtime, and nowhere else yet.

The SDK's tasks (`purgatorio::task`) remain the portable way to do
several things in one prog: they run on every engine and on the kernel,
but on one thread, so a blocking call in one task blocks them all. A
threaded module is for a program that needs calls to overlap or more
than one CPU.

## What a thread is

A threaded module is built for `wasm32-wasip1-threads`. Its memory is
shared and imported (`env.memory`), it imports wasi-threads'
`wasi.thread-spawn`, and it exports `wasi_thread_start`. Rust's `std`
does the rest in the guest: `std::thread::spawn` allocates the new
thread's stack and thread-local storage from the heap and calls
`thread-spawn`.

The host starts each thread as a prog, as Inferno starts a Limbo
`spawn`:

- The thread is a new instance of the same module on the same memory,
  on a kproc of its own.
- It is a prog in `/prog`, with its own pid, the module's name, and its
  own `ctl`, `status` and `wait`. Its pid is the thread id wasi-threads
  asks for.
- It shares the spawning prog's file descriptors, name space and
  environment, all of them, as a `spawn` does. A file one thread opens,
  another can read; a bind one thread makes, the others see.
- Blocking calls block one thread. Four threads that each sleep 200 ms
  are done in about 200 ms.

So `ps` shows a threaded program as several progs, and a thread can be
killed by its pid.

## How a threaded program ends

Limbo's spawned progs outlive their parent. Rust's threads do not
outlive their process, and a Rust program relies on that. A threaded
module follows Rust:

- A thread that returns from its function ends alone, with no status.
- When the program's own prog ends (the one that ran the command), the
  threads still running are killed.
- When a thread does not end cleanly (it is killed, it traps, or it
  exits with a status, as a panic does), the whole program is killed.
  The thread may have held a lock, and another may be waiting to join
  it; neither would ever be released. The program's status is then
  `killed`; a panic's message is on fd 2.

### Detaching: a Limbo `spawn`

A thread can leave the program's fate: `purgatorio::thread::detach()`,
which writes `detach` to the thread's own `/prog/<pid>/ctl`. A detached
thread is not killed when the program's prog ends or another thread
fails, and its own failure does not kill the program. It is then what a
Limbo `spawn` makes: a prog on its own that still shares the heap, the
fds and the name space. It shows in `/prog` until it ends, and an emu
that waits for its last prog waits for it. If the program's own prog
detaches, its end kills nobody.

This is for a thread written for it: a server's worker that may be
shed, or one that must finish after the command returns. Nothing
releases a lock a detached thread held when it died, and nothing wakes a
`join` on it. There is no way back, and `detach` on a prog that is not a
thread does nothing. Any prog that may write the thread's `ctl` can
detach it, as it can kill it.

A thread that waits on a lock or a condition variable is in
`memory.atomic.wait`, parked inside wasmtime, where a kill's note does
not reach. The engine wakes it: every tick (10 ms), for a group with a
killed thread still running, it notifies every address a wait can be
on. To the guest that is a spurious wake-up, which futex users must
expect; the killed thread comes back into guest code and the epoch
check ends it.

## Building

A crate is a threaded guest when its manifest says so:

```toml
[package.metadata.purgatorio]
threads = true
```

`cargo xtask build-root` builds it for `wasm32-wasip1-threads` instead
of `wasm32-wasip1` and installs it where it would go otherwise.
`rust-toolchain.toml` lists the target. The SDK builds for it unchanged:
it is WASI preview1 with atomics.

`appl/test/threads` is the conformance guest (`/bin/test/threads.wasm`).
It checks a mutex, an atomic, joins, a channel, a condition variable,
thread-local storage, a pid each in `/prog`, overlapping sleeps, shared
fds and name space, a blocked read another thread satisfies, a prog
started from a thread, memory grown by a thread, and 64 threads at once.
The acceptance tests run it on wasmtime, check that wasmi refuses it,
and check the kills and the detaching above.

## In the host

- `purg_vm::shared_memory` tells a threaded module by its memory's
  limits. `Engine::threads` says whether an engine runs one;
  `run_module` refuses it otherwise:
  `bad executable: threads are not supported by wasmi`.
- `purg_vm::spawn_thread` makes the thread's prog, sharing every group
  of its parent, and starts its kproc. `run_with` is `run_module` for a
  start other than a module's entry point.
- `purg-vm-wasmtime` enables wasmtime's `threads` feature, makes the
  shared memory when a threaded command starts, keeps the thread group
  (the module, the memory, the member progs), registers `thread-spawn`,
  and does the ending and waking described above.
- A host call finds the caller's memory as before; for a shared memory
  it takes the bytes where they are.

## Limits

- **wasmtime on the emu only.** wasmi 2 has no shared memories or
  atomic instructions. The kernel's wasmtime runtime is built without
  `std`, and wasmtime's `threads` feature needs it. Neither engine is
  handed a threaded module, and the kernel's image leaves
  `/bin/test/threads.wasm` out.
- **The host reads memory the guest's other threads can write.** A host
  call takes a buffer or a path from guest memory as a Rust slice while
  another thread may be changing it. A correct program does not do that
  to itself, but a hostile one can, and the host then holds references
  Rust assumes nobody else writes. Until host calls copy what they
  check, do not run untrusted threaded modules. (wasmtime itself ranks
  threads as a tier 2 feature for the same reason.)
- **Memory is charged at the start only.** The first pages of the
  shared memory are charged to the program (`memlimit`); what it grows
  by is not, since wasmtime's limiter does not see a shared memory
  grow. The module's own maximum bounds it: 1 GiB for Rust's target.
- **No tracing.** `stack`, `prof`, `cprof`, `mprof` and `mdb` do not
  work on a threaded module; it runs untraced.
- **Waking a killed thread costs a pass over the memory**, a notify for
  every fourth byte, each tick until the thread is gone. That is
  tens of milliseconds for a few megabytes and seconds for a gigabyte.
- **No wasm64 threads**, and reactors are not threaded.
- Each thread counts as a prog against a domain's prog limit and holds
  a kproc stack.

## What the kernel would need

Either engine has to learn shared memories and atomics:

- wasmi: the threads proposal in the interpreter (shared memory,
  atomic loads, stores and read-modify-writes, wait and notify), which
  is a change to wasmi itself.
- The AOT engine: wasmtime's shared memory and its parking of waiters
  without `std`, on `purg-platform`'s locks and `Rendez`. The compiled
  code already has the atomic instructions.

A third way serves both, and would also make waits killable without the
pass over memory: rewrite `memory.atomic.wait` and `notify` in a
threaded module into host calls (`futex_wait`, `futex_wake`) on a
`Rendez`, as the trace rewrite adds its thunks. It needs an instruction
decoder for function bodies, which the trace rewrite avoids, and an
engine still has to share one memory between instances.
