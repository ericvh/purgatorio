# A Limbo compiler that targets WebAssembly

Could Purgatorio have a Limbo compiler that emits `.wasm` instead of `.dis`,
that is itself a guest program, and that lets Inferno's own Limbo programs
run here? This is a paper study for issue #53. Nothing was downloaded,
cloned, built or run for it. Web pages, documentation and single files of the
`inferno-os` repository were read through the web, and nothing from them is
copied into this repo. Every claim is marked **[src N]** (verified from
source N in the list at the end, read on 2026-11-10), **[repo]** (read from
this repository) or **[inf]** (our inference or prior knowledge, not checked
against a source in this study).

## Verdict

It is feasible, and nothing in it is a research problem. Limbo is small:
Inferno's compiler is 19 files and 483 KB of C and yacc, about 25,000 lines
[src 2, 3], a little more than the 15 to 20 thousand the issue guessed. A
rewrite in Rust is an ordinary `wasm32-wasip1` guest with no LLVM, no threads
and no process spawning, so none of the blockers in `docs/selfhost.md`
apply. The generated code needs only core WebAssembly 1.0 features, so it
runs on wasmtime and on wasmi, hosted and in the kernel [inf].

The hard part is not the compiler. It is three places where Limbo's model
does not fit the present ABI:

1. **Every Limbo thread is a kernel prog.** It has a pid, its own `pctl`
   state, its own wait message, and `/prog` shows it [src 10, inf]. A
   command may return from `init` and leave threads serving. Purgatorio's
   in-module tasks are invisible to the kernel (D4).
2. **Loaded modules share one heap** with their caller: they pass
   references, arrays and channels [src 4]. Purgatorio's reactors have their
   own memory and take byte buffers (D5).
3. **A blocking system call blocks only its thread** in Dis [inf]. In one
   wasm instance it blocks every thread, as it does for SDK tasks today
   [repo: `sdk/purgatorio/src/task.rs`].

Simple commands (`echo.b`, `cat.b`, `wc.b`, most filters) hit none of the
three and need no ABI change. `sh.b` hits all three.

Against self-hosting `rustc`, the Limbo compiler is more total work (about
five to six months to `sh.b`, against two to three months for the
`rustc` plan) but low risk, with no external toolchain to rebuild and no
30 to 80 MB module. The two are not substitutes: `rustc` builds the system's
own code, and Limbo adds a second language whose programs we may not commit
(see Licensing). The recommendation, the first step and the open questions
are at the end.

## 1. The language and what must map

Sources: Ritchie's reference [src 4], the 2005 addendum [src 5], the Dis
specification [src 6] and the grammar of Inferno's compiler [src 11].

### Types

| Limbo | Semantics | In wasm linear memory [inf] |
|---|---|---|
| `byte` | unsigned 8 bits [src 4, 4.2.1] | `i32`, stored as 1 byte |
| `int` | signed 32 bits [src 4] | `i32` |
| `big` | signed 64 bits [src 4] | `i64` |
| `real` | IEEE 64-bit [src 4] | `f64` |
| `fixed(scale, max)` | fixed point over `int`; `*` and `/` rescale; no `%` or bit operations [src 5] | `i32`, with `i64` intermediate for multiply and divide |
| `string` | a row of Unicode characters; value semantics; `s[i] = c` is allowed and `s[len s] = c` appends [src 4, 8.4.1] | heap object, counted, copy on write; Latin-1 bytes or 32-bit code points so indexing is constant time, as in Dis |
| tuple | value type [src 4] | laid out inline, like an adt |
| `adt` | value type; functions with `self` [src 4, 6.3] | inline fields |
| `ref adt` | reference [src 4] | pointer to a counted heap object |
| `pick` adt | tagged union, only used by `ref` [src 4, 6.3.1] | heap object with a tag word |
| `array of T` | reference; length is part of the value; a slice shares storage with its array [src 4, 8.1.6] | header (count, length, element descriptor, data pointer, parent array for slices) |
| `list of T` | reference; immutable cells; `hd`, `tl`, `::` [src 4] | counted cons cells |
| `chan of T`, `chan[n] of T` | reference; unbuffered sends block until a receive; `chan[n]` buffers `n` values; waiting threads queue first in, first out [src 4, 5] | heap object with a buffer and two queues of blocked threads |
| module | reference returned by `load`; each load is a new instance with its own data [src 4, 8.4.5] | pointer to (module data, table of function slots) |
| `ref fn(...)` | function reference, may name another module's function [src 5] | (module pointer, function table index) |
| exceptions | string exceptions with prefix matching (`"fail:*"`); declared exceptions carrying a tuple; `raises` clauses [src 5] | see below |
| `cyclic` | lets a `ref adt` field form a cycle; such data is freed "eventually, but not instantly" [src 4, 11.1] | flag in the type descriptor |
| polymorphism | `adt[T]`, `fn[T]`, `for { T => ... }` constraints | not in the reference or the addendum [src 4, 5]; the grammar has `polydec` and `forpoly` rules [src 11]. Type parameters are always references [inf], so one compiled body serves all uses |

All conversions are explicit [src 4, 8]. `string` to and from `array of
byte` is UTF-8 encoding and decoding [src 4], which is also what the ABI
wants at every system call [repo: `docs/abi.md`]. `**` (exponentiation) is in
the addendum [src 5].

Integer overflow and division by zero are "undefined" in the reference
[src 4, 8.6]. In Inferno a zero divide, a nil dereference and an array
index out of range raise string exceptions that a program can catch [inf].
A wasm trap cannot be caught without the exception-handling proposal, so
the compiler must emit explicit checks that raise the Limbo exception
[inf].

### Memory model

The reference says only that objects without cycles are destroyed
"immediately after they become non-referenced" [src 4, 11.1]. The Dis
specification says the VM "performs both reference counted and real time
mark and sweep garbage collection", and that each type has a descriptor
with a size and a pointer map [src 6].

The immediate release is visible to programs: a file is closed when the last
`ref Sys->FD` to it goes away, so `fd = nil` and a thread's exit close files
and pipes [inf]. Any design must keep that. This rules out a collector
without prompt finalisation (see 2b).

### Modules and `load`

A source file starts with `implement M;`. Interfaces live in `.m` files and
are pulled in with `include` [src 4]. `load M path` returns a new instance,
or `nil` if the file is missing or does not match the interface; the check
is made at run time [src 4, 8.4.5]. In a `.dis` file each exported function
has a link entry with a hash of its type signature, and `load` fails if a
needed function is absent or its signature differs [src 6, 7]. Newer `.dis`
files also carry an import section and an exception-handler section, flagged
`HASLDT` and `HASEXCEPT` [src 7]. Paths starting with `$` (`$Sys`, `$Math`)
name modules built into the VM [inf].

### Threads, channels, `alt`

`spawn f(args)` starts a thread. "The two threads share a common memory
space", scheduling is preemptive, and channels are the only synchronisation
[src 4, 9.13]. `alt` waits on several sends and receives, picks at random
among the ready ones, and does not block if it has a `*` arm [src 4, 9.8].
Since the addendum, any number of threads may wait in `alt` on one channel
[src 5].

A spawned thread starts out sharing its parent's namespace, file descriptors
and environment. `sys->pctl` then changes them for the calling thread only:
`NEWFD`, `FORKFD`, `NEWNS`, `FORKNS`, `NODEVS`, `NEWENV`, `FORKENV`,
`NEWPGRP`. It returns the thread's pid [src 10]. Inferno's `sh.b` depends on
this: it spawns a thread per command, and that thread forks the fd group
before it redirects [src 3, inf].

## 2. Mapping onto wasm and Purgatorio

Four designs. A is the one recommended.

### A. A linear-memory runtime, threads inside one instance

Everything is core wasm 1.0: one linear memory, `call_indirect`, no
proposals [inf].

**Heap.** A small allocator in linear memory. Each object has a header with
a reference count and a type descriptor index. Descriptors hold a size and a
pointer map, as in Dis [src 6]. The compiler emits the count updates. Freeing
walks the pointer map. An `FD` object's destructor calls WASI `fd_close`.
Types marked `cyclic` are also registered with a backup mark-and-sweep that
starts from module data and thread stacks; it can come late, since leaking
cycles in a short command is harmless [inf].

**Frames.** Limbo locals that hold references live in frames on a
per-thread stack allocated in the heap, described by pointer maps, as Dis
does [src 6]. Scalars may stay in wasm locals between calls. This makes
frames visible to the collector and makes a thread's whole state plain data.

**Threads without stack switching.** There are three ways to suspend a
thread in the middle of a call chain:

1. *The compiler makes functions resumable.* A call-graph pass marks every
   function that may block: one that uses a channel, `alt`, a blocking `Sys`
   call, an indirect call, or calls a marked function. A marked function is
   compiled as a state machine. Its live values are in its heap frame. After
   each call to another marked function it tests one global; if the callee is
   unwinding, it stores its resume point in the frame and returns. To resume,
   the scheduler calls the bottom function again and each one jumps
   (`br_table`) to its saved point. Unmarked functions are plain wasm. This
   is what Binaryen's Asyncify does to a finished module, where it costs
   about 50% in code size in the worst case and far less when the set of
   functions is known [src 18]; a compiler that owns code generation knows
   the set exactly. Go's own wasm backend works this way [inf, not checked].
2. *An interpreter for a compact IR.* The module carries bytecode and a
   small interpreter. Suspension is free, since thread state is data. It is
   Dis again, running inside a wasm interpreter in the kernel: slow twice
   over [inf].
3. *Continuation-passing style everywhere.* Simple to specify, but every
   call allocates and code grows [inf]. No advantage over 1.

Use 1. The same "is the callee unwinding?" test also carries exceptions:
`raise` sets the pending exception and unwinds, and each frame with a
handler checks the string against its guards [inf]. So exceptions cost
nothing extra and need no wasm proposal.

Preemption: Dis preempts threads [src 4]. Here threads switch at channel
operations, `alt`, `sys->sleep` and blocking calls. A thread in a long
computation holds the others up. A compiler flag can add a counter check on
loop back edges in marked functions [inf]. This is a divergence to accept or
decide (question 5).

**`chan`, `alt`, `spawn`.** They live in the guest runtime, the same choice
as D4 and the SDK's `task` module [repo]. Channels carry references, so
they cannot be kernel channels: those carry bytes between instances
[repo: `docs/abi.md`]. Kernel channels are used only where the kernel is the
other party: `sys->file2chan` (the runtime turns `purgatorio.file2chan`
requests into values on Limbo channels) and waiting for child progs. When
every thread is blocked, the scheduler calls `purgatorio.wait` on the
kernel-side operations it is waiting for [inf].

**Why not one prog per Limbo thread.** A new prog is a new instance with its
own memory. Limbo threads share a heap, so this only works with the wasm
threads proposal (shared memory, one instance per thread). It would give
real per-thread `pctl`, real blocking calls and real `/prog` entries. But
wasmi does not implement threads [src 16], wasmtime's Pulley interpreter
"does not support the threads proposal because there is no known safe way to
implement this with Rust's memory model" [src 13], wasmtime's `threads`
feature needs `std` [src 15], and D4 defers threads. It also makes every
reference count atomic. Not viable for the kernel; noted as the faithful
design if D4 ever changes.

**`load`.** Three cases, in order of difficulty:

1. *Builtin modules* (`$Sys`, `$Math`): calls into the runtime.
2. *A constant path* (`load Bufio Bufio->PATH`): the compiler links the
   module statically. It compiles the library from source (or from a cached
   object form) into the same wasm module, and `load` returns a fresh copy of
   that module's data. Section 6.3 of the plan already makes libraries static
   [repo]. No ABI change. The signature check happens at compile time.
3. *A path computed at run time.* For the `Command` interface
   (`init(ctxt: ref Draw->Context, argv: list of string)`), which is what
   `sh` loads, the module handle can be a proxy: calling `init` copies
   `argv`, runs `purgatorio.spawn_prog`, waits, and turns a non-empty status
   into the `fail:` exception a Limbo caller expects [inf]. For a plug-in
   with a real interface (sh's `std`, `expr`, `regex`, `string` modules pass
   `ref Context` and call back into sh [src 3, inf]) the callee must share
   the caller's heap. That needs dynamic linking: the loaded module imports
   `memory`, the function table and the runtime's functions instead of
   defining them. See "ABI additions".

**The type check.** Each compiled module carries a link table: module name,
and for each exported function its name, a hash of its type signature and
its table slot; and an import table of what it expects from the modules it
loads. This is the `.dis` link and import sections [src 7] under a new name.
Put it in a custom section, `purgatorio.limbo`, so tools can read it without
running the module (cf. `disdep` in PG-41), and also in the data segment so
the runtime can check it after instantiation. The check itself is guest
code. Use a truncated SHA-256 of the canonical signature text rather than
Dis's 32-bit hash [inf].

This is also a candidate answer to D5 and #51: a `.m` file is a typed
interface description. For values without references to shared state
(numbers, strings, tuples, lists and arrays of those) the compiler can
generate the byte encoding for today's `purgatorio.call`, so a Limbo program
could call an existing Rust reactor through a `.m` file that describes it
[inf].

**Builtin modules.**

| Module | Mapping | Notes |
|---|---|---|
| `$Sys` | WASI p1 and `purgatorio`, row for row as in `docs/abi.md` [repo] | `print`, `sprint`, `fprint` (with `%r`, `%q`), `tokenize`, `utfbytes`, `byte2char` and `char2byte` are runtime code. `Dir` is decoded from the 9P stat format. `dial`, `announce` and `listen` are file operations on `/net`, as in the SDK. `pctl`, `file2chan` and blocking reads meet the three problems above. |
| `$Math` | wasm `f64` operators for `sqrt`, `floor`, `ceil`, `fabs`; the Rust `libm` crate in the runtime for the rest | `FPcontrol` and `FPstatus` cannot be honoured: wasm has no rounding-mode or exception-flag control [inf]. |
| `$Keyring` | digests, ciphers and `IPint` from Rust crates, in a runtime part that is linked only when used | Inferno's certificates and its authentication protocol are not what `purg-auth` implements (Ed25519 and `pgcert1`, D9) [repo]. `Keyring->auth` and the `appl/cmd/auth` tools stay out unless D9 changes. |
| `$Draw`, `$Tk` | Phase G | Until then every command gets `nil` for `ctxt`, which is what non-graphical commands expect [inf]. `$Tk` needs the Tk command language (D10). |
| others | `$Loader`, `$Freetype`, `$Prefab`, `$IPints`, `$Crypt` exist in `libinterp` [src 2] | Not needed for commands. `$Loader` is Dis-specific and cannot be supported. |

**How the runtime gets into a module.** The runtime is `no_std` Rust built
by `xtask` into one wasm module with a fixed export list. The compiler,
which must work with no linker, copies the runtime functions a program
reaches and appends the generated code. That is a module-level merge the
compiler can do itself, since it assigns all the indices [inf]. With
dynamic linking (below) the runtime could instead be one shared file,
`/bin/lib/limbort.wasm`, and each command would be only its own code.

**ABI additions.** For programs that are one module after static linking,
and whose threads do not use `pctl` or outlive `init`: none. The
`purgatorio.limbo` section is a convention the loader ignores, like
`purgatorio.sig` [repo: `docs/abi.md`].

For `sh.b` and servers, three additions, each a decision for the owner:

1. **Light progs.** Calls that give a guest thread a kernel identity while
   it keeps running in the same instance: `lprog_new(flags) -> pid` (a new
   `Prog` entry sharing or forking the caller's groups, as `spawn_prog` does
   with its flags), `lprog_switch(pid)` (host calls from now on use that
   entry's fd table, namespace, environment, errstr and note),
   `lprog_exit(pid, status)` (post the wait message, close its fds). A kill
   written to `/prog/<pid>/ctl` is seen by the runtime at its next switch.
   The scheduler in the runtime calls `lprog_switch` when it changes thread.
   This restores per-thread `pctl`, per-thread wait messages, commands that
   return while a thread keeps serving, and threads in `ps`. It would also
   fix the same known divergence for SDK tasks (plan 4.3). It touches
   `purg-core`'s `Prog`, so it needs the owner's review [repo: CLAUDE.md].
2. **Shared-memory `load`.** A flag or a second call so that `load`
   instantiates a module whose imports of `memory`, the function table and a
   named set of runtime functions are satisfied from the calling instance.
   Imported memories and tables are core wasm, so both engines can do it
   [inf]; today `load_reactor` only handles a module that exports its own
   memory [repo: `crates/purg-vm-wasmi`, `crates/purg-vm-wasmtime`].
3. **Readiness for fds in `purgatorio.wait`.** Today `wait` takes kernel
   channels and `/prog/<pid>/wait` fds [repo: `docs/abi.md`]. Accepting any
   fd as a read or write operation lets the scheduler run other threads
   while one waits for input. Without it a thread blocked in `sys->read`
   stops the whole prog, which is the documented behaviour of SDK tasks
   today.

Each bumps `ABI_VERSION`. None is needed before stage 6 of the plan below.

### B. The wasm GC, function-reference, exception and stack-switching proposals

Status, verified on 2026-11-10:

| Proposal | Standard | wasmtime 50 | wasmi |
|---|---|---|---|
| GC, typed function references | finished, in Wasm 3.0 [src 17] | tier 1, Cranelift only (also when Cranelift emits Pulley bytecode); not Winch [src 12, 13]. Cargo features `gc`, `gc-drc`, `gc-null`, `gc-copying` [src 15] | planned, tracking issue only [src 16] |
| Exception handling | finished, in Wasm 3.0 [src 17] | tier 1 [src 12, 13] | planned [src 16] |
| Tail calls | finished, in Wasm 3.0 [src 17] | tier 1 [src 12] | supported [src 16] |
| Stack switching | phase 3 [src 17] | tier 3, work in progress, "only for x86_64 Linux" [src 12] | not listed [src 16] |
| Threads | phase 4 [src 17] | tier 2; needs `std`; not on Pulley [src 12, 13, 15] | planned [src 16] |

Our wasmtime is built with `default-features = false` and `cranelift,
runtime, std`, so `gc` is off today [repo: `Cargo.toml`]. wasmtime's
`no_std` build supports the `gc` feature but not Cranelift, so a kernel
would run precompiled modules or Pulley [src 14].

What they would buy:

- **GC** would remove the allocator, the counts and the cycle collector
  from the runtime. But it has no finalisers, so an `FD` could not close when
  its last reference goes, and Limbo's prompt release would be lost [inf].
  And wasmi, the kernel's engine and half of every test run, cannot load such
  a module at all [src 16]. Not usable.
- **Exception handling** would replace the unwind test after calls. The test
  is needed anyway for thread switches in design A. No gain worth losing
  wasmi.
- **Stack switching** is the right primitive for Limbo threads: no
  transform, no heap frames. It is not standard, not on aarch64 and not on
  either interpreter [src 12, 13, 16]. Revisit when it reaches phase 4 and
  wasmi.
- **Threads**: see "Why not one prog per Limbo thread" above.

So for `no_std` kernels the answer is the same as for the hosted emu on
wasmi: only design A's core-wasm code runs everywhere. The compiler should
keep thread switching and exceptions behind one internal interface so that a
stack-switching back end can be added later without touching the front end
[inf].

### C. Compile Limbo to Rust source

Translate each `.b` file to Rust over a small support crate: `Rc` for
references (so files still close promptly), `async fn` for functions that
may block (rustc builds the state machine), the SDK's `task::Chan` for
channels, `Result` for exceptions, static linking for `load` [inf].

- It needs the same front end and type checker as design A. Only the back
  end is cheaper: perhaps 2 to 3 weeks instead of 6 to 8 [inf].
- It is not self-hosting. The output needs `rustc`, which is a host tool
  until PG-43 lands.
- Guests are Rust `std` guests, 60 to 230 KiB each, against an estimated 15
  to 40 KiB for design A (section 5).
- A run-time `load` of a plug-in is as hard as in A.
- Generated Rust is a translation of Inferno's source and so a derivative
  work under its MIT licence. It could not be committed under the project
  rule (see Licensing), which removes the one attraction, using it to port
  the missing commands.

Not recommended, except as a throwaway check of the front end.

### D. Keep Dis as the intermediate form

Not asked for in the issue, but it is the cheapest way to the same value and
belongs in the comparison. Write a Dis interpreter as a guest, from the Dis
specification [src 6] and dis(6) [src 7]. It runs the `.dis` files a user
brings, including Inferno's own compiler (`dis/limbo.dis`, 343,726 bytes
[src 2]), so the compiler runs inside Purgatorio with no compiler written by
us. The core of Inferno's interpreter (`xec.c`, `load.c`, `heap.c`, `gc.c`,
`string.c`, `alt.c` and a few small files) is about 76 KB of C [src 2],
perhaps 4,000 lines. A variant translates `.dis` to `.wasm` ahead of time,
the way Inferno's JITs translate it to machine code (`comp-arm.c` is 44 KB
[src 2]).

It has the same three ABI problems as A. It brings back the VM the project
set out to replace. Under wasmi it is an interpreter inside an interpreter.
And it depends on MIT-licensed binaries or sources that cannot live in this
repo. It is the fallback if the goal shrinks to "let a user run Inferno's
`/dis` tree". A real Inferno emu on the host is also the natural test
oracle for design A, in line with plan principle 4 ("test against real
Inferno") [repo].

### Comparison

| | A: runtime in linear memory | B: GC and newer proposals | C: Limbo to Rust | D: Dis interpreter |
|---|---|---|---|---|
| Runs on wasmi and in the kernel | yes | no [src 16] | yes | yes, slowly |
| Self-hosting (compiler is a guest) | yes | yes | no | uses Inferno's compiler |
| `chan`, `alt`, `spawn` | guest runtime | guest, or stack switching | SDK tasks | interpreter |
| `load` | static; proxy for commands; shared-memory load for plug-ins | same | static only | interpreter's loader |
| Prompt `FD` close | yes | no | yes | yes |
| ABI additions for `cat.b` | none | engine features | none | none |
| ABI additions for `sh.b` | light progs, shared `load`, fd readiness | same | same | light progs, fd readiness |
| Module size | about 15 to 40 KiB, or 1 to 3 KiB with a shared runtime [inf] | smaller | 60 to 230 KiB | `.dis` size plus one interpreter |
| Effort to `cat.b` | 8 to 11 weeks | similar | 6 to 9 weeks | 4 to 6 weeks |

## 3. The compiler

### Inferno's compiler

There are two copies, kept in step: C in `limbo/` for the host build and
Limbo in `appl/cmd/limbo/` [src 2, 8, 19]. Sizes are from the repository
listing [src 2]; line counts are from the file pages [src 3] where given,
otherwise estimated from bytes.

| Part | Files in `limbo/` (bytes) | Lines |
|---|---|---|
| Lexer, options, includes | `lex.c` 22,652 | about 1,100 [inf] |
| Grammar | `limbo.y` 31,760 | 2,000 [src 3] |
| Tree, declarations | `nodes.c` 26,717; `decls.c` 24,783 | about 2,600 [inf] |
| Types, sizes, descriptors, signatures | `types.c` 93,917 | 4,745 [src 3] |
| Type checker, constant folding | `typecheck.c` 75,311 | 3,627 [src 3] |
| Code generation | `com.c` 28,836 (1,510 lines [src 3]); `ecom.c` 52,773 (2,558 lines [src 3]); `gen.c` 19,566 | about 5,000 |
| Optimiser | `optim.c` 31,161 | about 1,600 [inf] |
| Dis output | `dis.c` 11,767; `asm.c` 6,558; `optab.c` 10,685 | about 1,500 [inf] |
| Symbol file | `sbl.c` 6,786 | about 350 [inf] |
| C stubs for builtin modules (`-T`, `-t`) | `stubs.c` 14,158 | about 700 [inf] |
| Headers | `limbo.h` 14,114; `fns.h` 10,794 | about 1,200 [inf] |
| **Total** | 19 files, 483,217 bytes | about 24,700 [inf: 14,440 lines verified for 282,597 bytes, scaled] |

The roles are inferred from the file names and from limbo(1) [src 8]; the
file contents were not studied. Roughly 57% of the bytes are front end and
38% are Dis-specific.

The Limbo copy is 21 files and 492,388 bytes; its `limbo.b` (3,107 lines) is
the yacc output plus the main program [src 2, 3]. Compiled, it is
`dis/limbo.dis`, 343,726 bytes [src 2].

`limbo -g` writes a `.sbl` file: plain text with a file table, a table from
Dis pc to source position, a type table, a function table with arguments
and locals, and the module's globals [src 9]. It is tied to Dis pcs and
frame offsets. Our equivalent is the wasm `name` section plus a small
custom section mapping code offsets to lines and frame slots to names [inf].
It is only needed for a debugger, so it comes last.

### A rewrite in Rust

- **Front end.** A hand-written lexer and recursive-descent parser, the
  `include` search of limbo(1) (the current directory, `-I` directories,
  then `/module` [src 8]), declarations and scopes, types, the type checker,
  constant folding, and `import`. This is the largest part, about 8,000 to
  10,000 lines of Rust [inf]. The reference and the addendum are the
  specification [src 4, 5]. They do not cover polymorphism or inlining
  [src 4, 5, 11, 19], so for those the behaviour must be learned from
  Inferno's compiler and from programs that use them. Reading MIT-licensed
  source for reference is what plan D1 already allows; copying is not
  [repo].
- **Back end.** Lowering of expressions and statements, `case` (including
  string and range cases [src 4, 9.7]), `alt`, `pick`, exceptions, reference
  count insertion, the may-block analysis and the resumable-function
  transform, module data, the link table, and wasm encoding. About 5,000 to
  6,000 lines [inf]. The `wasm-encoder` and `wasmparser` crates do the
  encoding and the runtime merge; `docs/selfhost.md` stage A already plans to
  build them as guests [repo].
- **Runtime.** Allocator, counts and descriptors, strings and UTF, arrays
  and lists, the formatter for `print`, scheduler, channels and `alt`,
  exceptions, `$Sys` glue, `$Math`. About 4,000 to 5,000 lines of `no_std`
  Rust [inf].
- **Interface files.** No Limbo program compiles without `sys.m` and
  `draw.m`. Inferno's are MIT-licensed files we may not copy in. The
  builtin modules' interfaces must match our runtime anyway, so we write our
  own `sys.m`, `math.m` and a types-only `draw.m` from the man pages, as the
  SDK was written. `module/sys.m` is 4,114 bytes [src 2], so this is small.
  The other 170-odd `.m` files in `module/` [src 2] come with the user's
  Inferno tree (question 3).

About 20,000 lines of new Rust in all, plus tests.

### Rust or Limbo for the compiler itself?

- **Rust.** It is a guest today, built and tested by the existing
  toolchain, on both VMs, byte-identical across hosts like every other
  guest. It satisfies the issue's sense of self-hosting: the compiler runs
  inside Purgatorio and produces guests there. But rebuilding the compiler
  itself still needs host `rustc` until PG-43.
- **Limbo.** True self-hosting: the compiler compiles itself inside
  Purgatorio with no host tool, which `rustc` will not do cheaply. It costs
  a second implementation, or a bootstrap through a real Inferno (compile
  our Limbo-written compiler with Inferno's `limbo`, run it under Inferno's
  emu to compile itself to wasm) and a seed binary to keep [inf]. The
  runtime would still be Rust.
- **Inferno's Limbo-written front end with a new back end** is the shortest
  road to a Limbo-written compiler, since `appl/cmd/limbo` exists [src 2].
  It is Inferno code, so it could only live outside this repo, in a separate
  MIT-licensed project.

Write it in Rust. Decide on a Limbo version after stage 4, when the Rust
compiler can compile large programs and the cost is clearer.

### Staged plan

Efforts are rough, for one person who knows the repo. In-repo tests use
Limbo programs written for the purpose. A local, opt-in test takes the path
of a user's Inferno tree and runs the compiler over it; nothing from that
tree is committed.

| Stage | Work | Effort | Exit |
|---|---|---|---|
| 0. Spike | The runtime core in `no_std` Rust, and by hand (a Rust test that emits wasm) the code the compiler would generate for a `cat` and for two threads exchanging values on a channel through resumable functions. Measure size, and speed on both VMs against the Rust `cat`. | 1 week | The calling convention, the frame layout and the size estimate are confirmed or corrected. |
| 1. Front end | Lexer, parser, includes, declarations, types, type checker, constants. Our own `sys.m`, `math.m`, `draw.m`. | 4 to 6 weeks | Parses and type-checks our test programs; in the opt-in test, all of a user's `appl/` (945 `.b` files, 9.4 MB [src 2]) with a short list of known gaps. |
| 2. Subset back end | Scalars, strings, arrays, lists, tuples, adts, functions, control flow, `case`, counts, static data, `$Sys` file calls and `print`. The compiler as `/bin/limbo.wasm`. | 3 to 4 weeks | "hello", `echo.b` and `cat.b` (48 lines; `sys->open`, `read`, `write`, `fprint` with `%r`, `raise "fail:..."` [src 3]) compile inside Purgatorio and behave like our Rust commands on both VMs. |
| 3. Threads | May-block analysis, resumable functions, scheduler, `spawn`, `chan`, buffered channels, `alt`, `exit`, exceptions and `raises`. | 3 to 4 weeks | A channel test corpus passes; a deadlock leaves threads blocked, as in Limbo. |
| 4. Modules and the rest of the language | Static linking of constant-path `load`, link table and signature check, the `Command` proxy, `pick`, `ref fn`, polymorphism, `fixed`, `cyclic` and the cycle collector. | 3 to 4 weeks | A multi-module program loads `bufio`; a wrong signature gives `nil`. |
| 5. Library | The rest of `$Sys` (`file2chan`, `dial`, `mount`, `export`, `dirread`), `$Math`, `$Keyring` digests and `IPint`; the `appl/lib` modules that commands use, compiled from the user's tree. | 3 to 5 weeks | A list of `appl/cmd` programs that compile and pass our existing command tests unchanged. |
| 6. `sh.b` | Light progs, shared-memory `load`, fd readiness (the three ABI additions), then `sh.b` (2,875 lines; `spawn`, channels, exceptions, run-time `load Command` [src 3]) and its builtin modules. | 3 to 4 weeks plus ABI review | Inferno's `sh` runs the sh corpus in `root/lib/test/sh/` where the two shells agree. |
| 7. Tooling | limbo(1) options that still make sense (`-I`, `-o`, `-w`, `-g`, `-S` as wat), debug sections, a CI job that compiles the test corpus inside the emu on both VMs. | 1 to 2 weeks | |

To `cat.b`: 8 to 11 weeks. To channels and modules: 14 to 19 weeks. To
`sh.b`: 20 to 28 weeks.

## 4. Licensing

What the repository says [src 1, 2, 20]:

- The top-level `NOTICE` states: "The bulk of the tree is covered by the
  permissive MIT licence reproduced below. Individual components might have
  their own NOTICE and LICENCE files that cover their content." The MIT text
  follows, with copyright lines for Lucent, Vita Nuova and the Plan 9
  Foundation [src 1].
- Exceptions named there: FreeType under the FreeType licence, and Bigelow &
  Holmes fonts, which "allow redistribution only as part of an Inferno or
  Plan 9 distribution (see fonts/LICENCE)" [src 1].
- Other `NOTICE` files exist under `appl/cmd/install`, `appl/lib/ida`,
  `libdynld`, `libfreetype`, `liblogfs`, `libnandfs`, `libmp`, `libsec`,
  `os`, `tools`, `utils` and some subdirectories [src 2]. We read two:
  `appl/lib/ida/NOTICE` is an MIT-style licence with MIT's copyright;
  `appl/cmd/install/NOTICE` is a credit to Russ Cox's `wrap` with no other
  terms [src 20]. The rest were not read.
- There is no `NOTICE` under `limbo/`, `appl/cmd/limbo/`, `libinterp/`,
  `module/` or `doc/` [src 2], so the compiler, the VM, the interface files
  and the papers fall under the top-level MIT notice [inf].
- "Inferno and Limbo are registered trademarks of Vita Nuova Holdings
  Limited" [src 1].
- GitHub's licence API returns "Not Found" for the repository, because the
  terms are in `NOTICE` and not in a file it recognises [src 2].

What it means:

1. **A compiler written from the reference.** No obligation arises. MIT also
   permits reading the C compiler to settle what the documents leave out.
   MIT code may legally be included in an Apache-2.0 work with its notice
   kept, so the "no Inferno code" rule is project policy (D1), not a legal
   need. The compiler should stay within it: specification from the papers
   and man pages, Inferno source as reference only, nothing copied. The
   name needs a thought: "a compiler for the Limbo language" is descriptive
   use, but `limbo` as a command name uses a registered mark (question 6).
2. **Inferno's `appl/` as input.** Users may compile it, and may
   redistribute the results with the notice, since MIT covers "copies or
   substantial portions" and a compiled module is one [inf]. For this repo:
   - Sources and compiled modules from `appl/` do not go into the repo or
     into `root/`. The compiler takes a path to a tree the user supplies.
   - The same holds for `module/*.m` other than the few interfaces we write
     ourselves, and for machine translations of `.b` files (design C).
   - CI may fetch a pinned `inferno-os` commit as test input without
     committing it, the way `docs/selfhost.md` treats a built `rustc` as a CI
     artifact. That is a policy choice (question 3).
   - Shipping compiled Inferno commands in a root image would be lawful with
     the notice attached, but it would end the "Apache-2.0, no Inferno code"
     statement for that image. The fonts could not go in at all.

A consequence for the issue's third bullet: the compiler is not a shortcut
for the missing commands (#34 to #41) inside this repo. Those still have to
be written in Rust, or the rule has to change. What the compiler gives the
project itself is a way to run Inferno's version of a command beside ours
and compare them.

## 5. Comparison with self-hosting rustc

| | rustc as a guest (`docs/selfhost.md`) | Limbo compiler, design A |
|---|---|---|
| Effort | Stages A to E: 8 to 12 weeks [repo] | 8 to 11 weeks to `cat.b`; 20 to 28 to `sh.b` |
| Risk | High: our own rustc 1.99.1 build for WASI, single-threaded LLVM, forks to carry [repo] | Low: ordinary compiler work in this repo; the open points are ABI design, not feasibility |
| External code | rustc, LLVM, lld, wasi-sdk builds as CI artifacts [repo] | None. Two Apache-compatible crates for wasm encoding |
| Emu changes before it runs at all | Cross-directory rename, larger wasm stack, module cache, perhaps threads [repo] | None until stage 6 |
| Compiler size | 31 to 80 MB plus a 25 to 144 MB sysroot [repo] | Estimated 300 to 600 KiB [inf; Inferno's own is 344 KB of Dis, src 2] |
| Output size | Rust `std` guests: 60 to 230 KiB (the range given for this study; a local build of an older tree had `export` at 59.8 KiB and `sh` at 170.7 KiB; `docs/footprint.md` is not on `next`) [repo] | Runtime estimated 15 to 40 KiB after dropping unused parts, plus code. For scale, Inferno's `cat.dis` is 707 bytes and `sh.dis` 41,296 bytes [src 2], and our `no_std` reactor `add.wasm` is 2,337 bytes [repo]. With a shared runtime a small command would be 1 to 3 KiB [inf] |
| Speed under wasmi | A full build in tens of minutes to hours [repo] | Compiling a command should take well under a second on wasmtime and a few seconds on wasmi [inf]. Compiled Limbo on wasmi should be within a small factor of Dis's own interpreter, since both interpret one instruction at a time [inf, to be measured in stage 0] |
| Works in the kernel today | No: waits for D6 [repo] | Yes [inf] |
| What it gives | The system can rebuild itself, byte for byte | A small compiler on the system; Inferno's programs for users who bring them; a fidelity check for commands we rewrote |
| What it does not give | Anything for Limbo programs | A way to rebuild the Rust system; any Inferno code in this repo |

## Recommendation

1. Treat the Limbo compiler as a separate track from PG-43, not a
   replacement. It is the cheaper way to get *a* compiler running inside
   Purgatorio, including in the kernel, and the only one that brings
   Inferno's programs. It does not make the system self-building.
2. Choose design A: a reference-counted runtime in linear memory, threads
   as compiler-made resumable functions in one instance, static linking for
   `load` first, core wasm only. Write the compiler and runtime in Rust.
   Reject B (not on wasmi, loses prompt release) and C (not self-hosting,
   and its output cannot be committed). Keep D as the fallback and use a
   host Inferno as the test oracle.
3. Do not change the ABI for it now. Stages 0 to 5 need nothing. Decide the
   three additions (light progs, shared-memory `load`, fd readiness in
   `wait`) on their own merits before stage 6. Light progs and fd readiness
   would also improve SDK tasks.
4. **First step: stage 0**, one week, no new dependencies and no ABI
   change: the runtime core and a hand-lowered `cat` and channel test, run
   on wasmtime and wasmi, with size and speed recorded in this document.
   If the numbers hold, start stage 1.

## Open questions for the owner

1. **Priority.** Is a second guest language wanted at all, given that the
   plan's first line says wasm "replaces Limbo and the Dis VM"? If yes, does
   it come before or after PG-43 and Phase K work?
2. **Thread identity (D4).** Should a guest thread be able to have a kernel
   identity (light progs)? Without it `sh.b`, any server that returns from
   `init`, and per-thread `pctl` cannot be faithful. With it, `purg-core`
   gains a prog that does not own an instance.
3. **Inferno sources as inputs.** May CI fetch a pinned `inferno-os` tree
   as test data? May a user-facing `xtask` or script fetch it to build a
   root with Inferno commands? Or is it strictly "bring your own tree"?
4. **Our own `.m` files.** Is writing `sys.m`, `math.m` and `draw.m` from
   the man pages acceptable under the no-copy rule? The names and types
   will match Inferno's, as the SDK's already do.
5. **Scheduling.** Are cooperative Limbo threads acceptable, with optional
   yield checks on loops, or must the preemption of Dis be matched?
6. **Names.** Command `limbo`, source suffixes `.b` and `.m`, output
   `.wasm` in `/bin`? "Limbo" is a registered trademark of Vita Nuova
   [src 1].
7. **Typed `load` (D5, #51).** Should the Limbo link table and `.m` files
   become the typed module convention for Rust reactors too, or stay private
   to Limbo modules?
8. **Shared runtime.** Link the runtime into every module (no ABI change,
   15 to 40 KiB each), or load one shared runtime module (needs
   shared-memory `load` from the start, 1 to 3 KiB per command)?
9. **`$Keyring`.** Stop at digests, ciphers and `IPint`, or revisit D9 so
   that Inferno's authentication tools can run?
10. **A Limbo-written compiler.** Is true self-hosting (the compiler
    written in Limbo) a goal, or is "the compiler is a guest" enough?

## What was not verified

- Nothing was built or measured. Every size, speed and effort figure for our
  compiler and runtime is an estimate.
- Line counts were read for eight files only [src 3]. The total is scaled
  from bytes.
- The roles of the compiler's files are inferred from their names.
- The last 10,000 characters of the reference [src 4] and the instruction
  list of the Dis specification were read through a summarising tool, not
  line by line. `module/sys.m`, `sh.b` beyond its first third, and the
  `NOTICE` files other than three were not read.
- Limbo's polymorphism has no document we could find. Its presence is
  verified from the grammar only [src 11].
- That Inferno raises catchable exceptions for nil dereference, bounds and
  zero divide, that Dis runs blocking calls without stopping other threads,
  and that sh's builtin modules call back into sh, are from prior knowledge
  of Inferno, not from a source read in this study.
- One web search found no existing Limbo-to-wasm compiler or Dis-in-wasm
  port [src 21]. That is not proof there is none.
- Go's and TinyGo's handling of goroutines on wasm are cited from memory.

## Sources

All read on 2026-11-10.

1. `inferno-os` top-level `NOTICE`.
   <https://raw.githubusercontent.com/inferno-os/inferno-os/master/NOTICE>
2. GitHub API listing of `inferno-os/inferno-os` (branch `master`, last push
   2026-06-21): file names and sizes for `limbo/`, `appl/cmd/limbo/`,
   `libinterp/`, `appl/`, `module/`, `dis/`, `doc/` and all `NOTICE` and
   `LICENCE` files; the licence endpoint returns 404.
   <https://api.github.com/repos/inferno-os/inferno-os/git/trees/master?recursive=1>,
   <https://api.github.com/repos/inferno-os/inferno-os/license>
3. GitHub file pages, for line counts and a description of contents:
   `limbo/typecheck.c` (3,627 lines), `limbo/types.c` (4,745),
   `limbo/ecom.c` (2,558), `limbo/limbo.y` (2,000), `limbo/com.c` (1,510),
   `appl/cmd/limbo/limbo.b` (3,107), `appl/cmd/sh/sh.b` (2,875),
   `appl/cmd/cat.b` (48).
   <https://github.com/inferno-os/inferno-os/blob/master/limbo/typecheck.c>
   and siblings.
4. Dennis M. Ritchie, "The Limbo Programming Language".
   <https://www.vitanuova.com/inferno/papers/limbo.html>
5. Vita Nuova, "Addendum to The Limbo Programming Language", 30 March 2005
   (`doc/limbo/addendum.ms`).
   <https://raw.githubusercontent.com/inferno-os/inferno-os/master/doc/limbo/addendum.ms>
6. "Dis Virtual Machine Specification".
   <https://www.vitanuova.com/inferno/papers/dis.html>
7. dis(6), the Dis object file. <https://inferno-os.org/inferno/man/6/dis.html>
8. limbo(1). <https://inferno-os.org/inferno/man/1/limbo.html>
9. sbl(6). <https://inferno-os.org/inferno/man/6/sbl.html>
10. sys-pctl(2). <https://inferno-os.org/inferno/man/2/sys-pctl.html>
11. `limbo/limbo.y`, searched for the polymorphism, `fixed`, exception and
    `cyclic` rules.
    <https://raw.githubusercontent.com/inferno-os/inferno-os/master/limbo/limbo.y>
12. Wasmtime, "WebAssembly proposals".
    <https://docs.wasmtime.dev/stability-wasm-proposals.html>
13. Wasmtime, "Tiers of support".
    <https://docs.wasmtime.dev/stability-tiers.html>
14. Wasmtime, "Platform support" (`no_std`).
    <https://docs.wasmtime.dev/stability-platform-support.html>
15. `wasmtime` 50.0.1 Cargo features.
    <https://docs.rs/crate/wasmtime/latest/features>
16. Wasmi README, proposal table. <https://github.com/wasmi-labs/wasmi>
17. WebAssembly proposals and finished proposals.
    <https://github.com/WebAssembly/proposals>,
    <https://github.com/WebAssembly/proposals/blob/main/finished-proposals.md>
18. Alon Zakai, "Pause and Resume WebAssembly with Binaryen's Asyncify",
    2019. <https://kripken.github.io/blog/wasm/2019/07/16/asyncify.html>
19. `inferno-os` `CHANGES`, entries on `ref fn`, inlining and the paired
    edits to `limbo/` and `appl/cmd/limbo/`.
    <https://raw.githubusercontent.com/inferno-os/inferno-os/master/CHANGES>
20. `appl/lib/ida/NOTICE` and `appl/cmd/install/NOTICE`.
    <https://raw.githubusercontent.com/inferno-os/inferno-os/master/appl/lib/ida/NOTICE>,
    <https://raw.githubusercontent.com/inferno-os/inferno-os/master/appl/cmd/install/NOTICE>
21. Web search, "Limbo language compiler WebAssembly Dis virtual machine
    wasm Inferno": results were the Wikipedia article and limbo(1) mirrors
    only.
