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

## Scaling

`cargo xtask thread-scale [--max N] [--reps R] [--all]` runs
`appl/test/threadscale` with 1 to N threads in the release emu, and the
same source built as a native program, and prints a table for each of
four workloads (also in `target/thread-scale/results.md`). Run it on the
machine you care about.

On an Apple M1 under Linux (4 fast and 4 slow cores, so about 5.9 times
one fast core at best; other work was running on it, and runs differ by
a tenth or two), medians of 5 runs, speed-up over one thread:

| workload | emu, 4 threads | emu, 8 | native, 4 | native, 8 | emu time over native, 1 and 8 threads |
|---|---:|---:|---:|---:|---|
| `primes`: computation alone | 3.13 | 3.92 | 3.22 | 3.94 | 0.91, 0.91 |
| `memory`: sums over 256 MiB | 3.08 | 4.96 | 1.06 | 1.06 | 10.0, 2.14 |
| `alloc`: 20 million small allocations | 3.08 | 3.98 | 2.80 | 4.26 | 0.78, 0.83 |
| `lock`: one contended mutex | 0.32 | 0.25 | 0.19 | 0.12 | 1.38, 0.69 |

On an AMD Ryzen Threadripper 1950X under Linux (16 cores, 32 with SMT),
on an idle machine, medians of 5 runs:

| workload | emu, 8 threads | emu, 16 | emu, 32 | native, 8 | native, 16 | native, 32 | emu time over native, 1 and 32 threads |
|---|---:|---:|---:|---:|---:|---:|---|
| `primes` | 7.58 | 15.11 | 15.11 | 7.52 | 15.04 | 15.07 | 1.01, 1.01 |
| `memory` | 4.82 | 4.52 | 4.49 | 1.44 | 1.42 | 1.35 | 3.19, 0.96 |
| `alloc`, with wasi-libc's allocator | 0.09 | 0.07 | 0.07 | 7.59 | 6.62 | 6.79 | 7.8, 753 |
| `alloc`, with the SDK's, shards by hash | 7.27 | 2.46 | 4.72 | 6.19 | 15.29 | 14.44 | 8.1, 24.7 |
| `lock` | 0.33 | 0.33 | 0.31 | 0.28 | 0.26 | 0.22 | 1.05, 0.74 |

What the two hosts agree on:

- **Computation scales as the hardware does, and as native code does.**
  On both hosts the emu's curve is the native build's, at about the
  native build's cost: 3.9 times on the M1's 8 uneven cores, and on the
  Threadripper 7.6 times on 8 threads and 15.1 on 16, within 1% of
  native. The engine adds nothing that grows with the thread count.
  (SMT adds nothing to this workload: 32 threads are no faster than 16.)
- **The array sums scale until the memory is the limit.** The emu's
  single thread is bound by computation (no vector code at the guests'
  `opt-level = "s"`, and bounds checks), 4 to 10 times slower than
  native, so it has room to scale: to about 5 times, where it meets the
  native build at the memory's bandwidth and both stop.
- **Allocation scales with the SDK's allocator, and not without it.**
  wasi-libc's allocator has one lock, so threads that allocate all the
  time ran one after the other and paid for the waits: 8 threads took
  7.6 times as long as one on the M1, and 32 threads 14 times as long
  on the Threadripper, 28.6 s against 38 ms native. With the allocator
  below, 8 threads on the M1 are 4 times faster than one, and faster
  than the native build; on the Threadripper 8 threads are 7.3 times
  faster and 32 threads take 445 ms. That run picked a thread's shard by
  a hash, and 16 threads often shared one (2.5 times, not 16); each
  thread now has a shard of its own, which this host shows fixes it, and
  a Threadripper run of that is to come. A single thread is 8 times
  slower than native on the Threadripper and faster than native on the
  M1; the cause is not known.
- **A contended mutex gets slower with threads everywhere**; the emu
  loses less than the native build, on both hosts.

## The allocator

A threaded guest that links the SDK gets `purgatorio::alloc::Threaded`
as its global allocator. Blocks of up to 1 KiB come from size classes,
16 bytes apart, kept in free lists that are sharded: there are 128
shards, each behind a spin lock held for a few instructions and each on
cache lines of its own. A thread is given the next shard in turn when it
first allocates and keeps its number in a thread-local word, so threads
share a shard only once more than 128 have been started. A free goes to
the freeing thread's shard. A list that passes 256 blocks sends half to a
central list a class, from which an empty list takes a batch before it
asks for a new slab, so what one thread frees comes back to the threads
that allocate. Larger blocks, and slabs of 32 KiB, come from wasi-libc.
Slabs are not given back: a guest's heap is its high-water mark.

Stable Rust has no cfg that tells `wasm32-wasip1-threads` from
`wasm32-wasip1`, so `cargo xtask` passes `--cfg purgatorio_threads` when
it builds for the threads target, and the SDK installs the allocator
under it. A threaded guest built some other way keeps wasi-libc's.

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
