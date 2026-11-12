# Inspecting modules and debugging progs

Inferno has a set of tools for Dis: `disdump` and `disdep` read a module,
`stack` and `mdb` look into a prog, and `prof`, `cprof` and `mprof` say
where time, calls and memory go. Here
modules are WebAssembly, so the tools are new, but the roles and the
`/prog` files are Inferno's (issue #40).

| command | Inferno's | what it does |
|---|---|---|
| `wasmdump [-hsiemcf] file ...` | `disdump` | prints a module: size, digest, ABI version, sections, imports by module, exports, limits, custom sections, and with `-f` every function |
| `wasmdep file ...` | `disdep` | prints what a module needs: its ABI version and its imports |
| `trace command [arg ...]` | | runs a command with call stacks kept |
| `stack [-v] pid ...` | `stack`, `stackv` | prints a prog's call stack |
| `broke [user]` | `broke` | kills the broken progs |
| `prof [-n lines] command [arg ...]` | `prof` | runs a command and prints its hot functions |
| `cprof [-u] [-n lines] command [arg ...]` | `cprof` | runs a command and prints how often each function was called |
| `mprof [-n lines] command [arg ...]` | `mprof` | runs a command and prints which functions grew its memory |
| `mdb pid` | `mdb` | stops a traced prog, sets breakpoints, reads and writes its memory |

They are not called `disdump` and `disdep`: what they read is not Dis, and
a script written for the Inferno tools would not understand the output.

## Modules

`wasmdump` and `wasmdep` are built on `appl/lib/wasmobj`, a reader for the
module format of about 850 lines with no dependencies. It reads every
section except the function bodies, which it only measures, and reports
the offset of anything it cannot read. The digest and the signature
entries come from `purg-auth`'s `modsig`, the code the loader uses
(`docs/signing.md`).

```
% wasmdump /bin/lib/add.wasm
module /bin/lib/add.wasm
size 2337
digest 8f0134cf255fe3efe4029373980ec325cd8c44b923ad6fc9fc2aba408feeff34
abi 0.9
sections 8
	type offset 8 size 21
	...
	custom offset 2317 size 18 purgatorio.abi
imports 0
exports 2
	memory 0 memory
	func 2 add (i32 i32) -> i32
functions 5 imported 0
memory 0 min 17 max -
global 0 mut i32 1048576
start -
elements 0
data 1
custom purgatorio.abi 0.9
```

Imports are grouped under the module they come from
(`wasi_snapshot_preview1`, `purgatorio`), each with its index and type. A
`purgatorio.sig` section prints as `custom purgatorio.sig signatures <n>`
and a line `signer <key> [<name> ...]` per entry, the names being those
in its certificates.

`wasmdep` prints `abi <version>` (`abi -` for a module with no
`purgatorio.abi` section) and `import <module> <name>` per imported
function. For a module that can `load` or start others it also prints
`refers <path>` for each path ending in `.wasm` found in the module's
data. That is a guess from strings: `sh` and `mash` build such names at
run time and show none.

## Function names

Guests are stripped, and stay stripped: the `name` section the linker
writes holds the output file's name, which carries a hash of the build
host, and guests must be byte-identical across hosts. So the names go in
a file beside the module, as Inferno keeps a `.sbl` beside a `.dis`:

- `/bin/cat.wasm` has `/bin/cat.names`: a line per function, `<index>
  <name>`, the name demangled and running to the end of the line.
- `cargo xtask build-root` writes them. The linker now keeps the `name`
  section (`strip = "debuginfo"`), and `build-root` moves the function
  names to the `.names` file and removes the `name`, `producers` and
  `target_features` sections from the module it installs. The installed
  modules are byte for byte what `strip = "symbols"` gave before.
- The names files are about 16% of the size of the modules (2.4 MB
  beside 15.1 MB). The hosted root (`root/bin`) has them all.
- The probe's and the kernel's images leave them out, since the root is
  most of the kernel image. Only the test guests' (`bin/test/*.names`,
  about 120 KB) go in, because the sh corpus reads them. On bare metal
  `stack` and `prof` therefore show other commands' functions as `func
  <index>`, and `stack -v` gives the index and the module's path, which
  `wasmdump -f` on a hosted root turns into names.
- `cargo xtask kernel --names`, `probe --names` and `initrd --names` put
  every names file into the image (`PURG_IMAGE_NAMES=1` does the same for
  a plain `cargo build` of the probe or the kernel), for when readable
  stacks on bare metal are worth the size.

`stack`, `prof` and `wasmdump -f` look for the `.names` file first, then
for a `name` section in the module itself (a module built elsewhere), and
then use the names of exported functions. A function with no name prints
as `func <index>`.

## Call stacks

Neither engine offers what Inferno's interpreter has, a stack that can be
read from outside at any time. wasmtime can capture a backtrace, but only
from the thread that runs the guest; wasmi has no interface for it at
all, and wasmi is the kernel's engine. So the stack is kept by the guest
code itself, and only when asked for:

A prog started under **tracing** runs a rewritten copy of its command
module (`purg_vm::trace`). Each function keeps its index but becomes a
thunk that pushes its index on a stack, calls the original body, and
restores the stack. The original bodies are appended as new functions and
are not changed or even decoded, and since calls, tables and exports
still name the old indices, every call passes through a thunk. The stack
is a second memory of one page, exported as `purgatorio.stack`. The
engines copy it into the prog before each host call, at each check point
(the fuel slice of wasmi, the 10 ms tick of wasmtime) and after a trap.

Both engines run the same rewritten module, so a stack reads the same on
both, and on the emu, the probe and the kernel.

Tracing is switched through `ctl`, like the module policy, and is
inherited:

```
echo trace on > /prog/<pid>/ctl      (or trace off)
```

It applies to the commands that prog starts from then on, and to what
they start. The prog itself is not changed: its module is already
running. `trace command` does this for one command.

Whoever can write a prog's `ctl` can turn this on, as they can kill the
prog. What they gain is less than a kill: the prog's later children run
slower, and one that traps stays in `/prog` until it is killed. Those
children can also be debugged (below): whoever can write their `ctl`
can stop them and read and write their memory. That is more than a kill
gives, since memory can hold keys; as in Inferno, it is open to whoever
owns the progs, and a sandbox behind `#pdomain` sees only its own.

What tracing costs, and only for traced progs: two wasm frames per call;
one more 64 KiB page (more for a module of over 30,000 functions);
about twenty instructions per call (see Costs below). The frames
would halve how deep a traced prog can recurse, so wasmi, whose limit is
a count of frames, lets a traced prog nest twice as deep (2,064 frames
instead of 1,000; `trace mash ctl.mash` now runs as untraced). wasmi's
frames and values are on the heap and grow as they are used, so the
higher limit costs nothing until a prog recurses that deep. On the
kernel, `trace test/debug deep 900` (about 1,800 frames) peaks about
150 KiB above `deep 1`. wasmtime's limit is the size of its native
stack, which the thunks' small frames barely add to. The digest and the
signature check are of the module as it is on disk, before the rewrite. A module using a type the
rewriter does not know (the GC proposal) runs untraced.

### Broken progs

In Inferno a prog that faults is not removed. It stays `broken` so that
it can be examined, until someone kills it. Here a **traced** prog that
traps does the same:

- its parent gets the exit message at once, as for any prog;
- its files, namespace and environment are released at once;
- it stays in `/prog` with state `broken`, keeping its `stack` and
  `exception`;
- `kill` on its `ctl` (the `kill` command, or `broke` for all of them)
  removes it.

A prog that is not traced leaves nothing behind when it traps, as before.

### `/prog/<pid>/stack`

Read-only. A line per frame, the innermost first:

```
<function index> <path>
```

`path` is the prog's command module, quoted when it holds white space or a quote. A
snapshot keeps the 256 innermost frames; when the prog had more, a last
line `... <n>` gives the number left out. For a running prog it is the
stack at its last host call or check point, so for a prog blocked in the
system it is exact. Reading the `stack` of a prog that is not traced
fails with `prog is not traced`.

Only the command module is traced. While a prog is inside a reactor
(`load`, `call`) its stack ends at the function that made the `call`.

### `/prog/<pid>/exception`

Read-only. The trap that broke the prog and a newline, as in its exit
status (`trap: ...`, in the engine's words), or nothing for a prog that
is not broken.

`stack pid` reads both files and names the functions:

```
% trace test/debug trap
trace: 7 "debug": broken: trap: wasm trap: wasm `unreachable` instruction executed
% stack 7
exception trap: wasm trap: wasm `unreachable` instruction executed
debug::inner
debug::middle
debug::outer
debug::main
...
_start
% broke
7
```

## Profiles

Inferno's `prof` samples the Dis program counter from a kernel process.
Here the samples are taken at the points where the engines already stop a
guest to check for a kill: every 10 ms of running under wasmtime, every
2^20 units of fuel (about as many instructions) under wasmi. A sample is
the innermost function of the traced stack. So wasmtime's profile is by
time and wasmi's is by instructions executed; both leave out the time a
prog spends blocked in the system or inside a reactor.

```
echo prof on > /prog/<pid>/ctl       (or prof off)
```

`prof on` implies `trace on` and clears the prog's samples. Like tracing
it is inherited by the progs started afterwards. It also makes the prog
collect: when a child exits (or breaks), its samples, and those it
collected, are added to its parent's if the parent has profiling on.
That is how `prof` reads the profile of a command that has gone.

Nothing is sampled, and no engine does any work for this, unless
profiling is on: a prog that is not traced has no stack to sample.

### `/prog/<pid>/prof`

Read-only. A line per sampled function, for the prog and its exited
children, ordered by path and index:

```
<samples> <function index> <path>
```

`prof command` turns profiling on for itself, runs the command, and
prints the result on standard error:

```
% prof test/debug hot
module /bin/test/debug.wasm samples 8
100.0%       8 debug::burn
```

## Call counts and coverage

`cprof` counts calls, as Inferno's `cprof` counts the execution of each
line. A prog started with call counting has thunks that each add one to
a `u64` of their function, in the stack memory after the breakpoint
bytes (`purg_vm::trace`). The counts are exact, and the same on both
engines.

```
echo cprof on > /prog/<pid>/ctl      (or cprof off)
```

`cprof on` is like `prof on`: it implies `trace on`, clears the counts
the prog collected, is inherited, and makes the prog collect the counts
of its children as they exit. The engines copy a prog's counts at their
check points and when it ends.

### `/prog/<pid>/cprof`

Read-only. A line per function called, for the prog and its exited
children, as `prof`'s: `<calls> <function index> <path>`.

`cprof command` prints, per module, `module <path> calls <n> functions
<called> of <defined>`, the coverage by function, and then the functions
by calls; with `-u`, the functions never called:

```
% cprof -n 3 test/debug spin
module /bin/test/debug.wasm calls 118 functions 44 of 256
 12.7%      15 dlmalloc
 10.2%      12 malloc
  9.3%      11 free
```

Coverage by line needs the DWARF line tables, which the guests are
built without (`strip`); it is not done.

## Memory

Inferno's `mprof` records allocations in the Dis heap. Here the
allocator is inside the module, and the host sees only `memory.grow`.
So a prog started with page counting has thunks that note the size of
memory 0 on entry and, on return, add what it grew to a `u32` of their
function. A function's count is the pages grown while it ran, in it or
in anything it called: the outermost functions have the most, the
allocator's own the least, and the ones between say which paths took
the memory. Like call counts, they are exact and the same on both
engines.

```
echo mprof on > /prog/<pid>/ctl      (or mprof off)
```

`/prog/<pid>/mprof` has a line `<pages> <function index> <path>` per
function that grew memory. `mprof command` prints them in bytes, per
module the bytes the module grew and then the functions:

```
% mprof -n 4 test/debug grow
module /bin/test/debug.wasm bytes 4194304
100.0% 4194304 _start
100.0% 4194304 std::sys::backtrace::__rust_begin_short_backtrace::<fn(), ()>
100.0% 4194304 debug::heap
100.0% 4194304 alloc::vec::from_elem::<u8>
```

Sizes of allocations, rather than of growth, would need the SDK's
allocator to record them: a wrapper around the global allocator, behind
a feature so that guests not profiled stay byte-identical, that writes
counts where the host can read them. It is not done: it needs a guest
built for profiling, where everything else here works on any traced
module, and a place in the module the host knows of, which today means
either a new host call or a convention on an exported global.

## The debugger

`mdb pid` debugs a traced prog. It is built on four things:

- **Stopping.** `stop` on `ctl` asks the prog to stop. Its engine takes
  the stop at the prog's next host call or check point (the points
  where it records the stack) and then holds the prog there, its state
  `debug`, serving the debugger on the engine's own thread, the only
  one that may touch the prog's memory. `start` lets it go on; `step`
  lets it go on until it next enters a traced function. Both return once
  the prog has left the stop, so a debugger that then finds it in state
  `debug` knows it stopped again. A prog blocked in the system stops
  when its call returns. `kill` ends a stopped prog as any other.
- **Memory.** `/prog/<pid>/heap` is the stopped prog's linear memory, at
  the file offset: reads past the end return nothing, writes past it fail
  with `bad address`. A running prog's fails with `prog not stopped`.
  (`/prog/<pid>/mem` already gives the memory a prog holds and its
  limit, so the contents have another name; Inferno's `heap` file is
  also where a debugger reads a prog's data.)
- **Breakpoints.** After the frames, the stack memory has a byte per
  function. A thunk whose byte is set writes its index plus one to the
  stack memory's second word and spins until the host clears it. The
  spin runs into the engine's next check point, where the host sees the
  word and holds the prog, with the function on the stack. `bpt set
  <func>` and `bpt del <func>` on `ctl` change the set; the engine writes
  the bytes at the next host call or check point. `step` sets every
  byte, and the next stop puts back the set. No host call or import is
  added: an import would renumber every function, which the rewrite
  avoids.
- **Stacks**, as `stack` reads them.

wasmtime does not let a host change wasm state from its epoch callback,
where a prog spinning at a breakpoint is caught. The backend writes the
two memories there through their raw contents: the guest is suspended
on that same thread, and nothing can resize a memory without the store,
which the callback only reads.

The commands of `mdb`, a line each on its standard input:

| command | |
|---|---|
| `t` | the stack, innermost first |
| `x addr [n]` | `n` bytes (64) of memory at `addr`, in hex |
| `w addr byte ...` | writes bytes, in hex, at `addr` |
| `b [func]` | a breakpoint on entry to `func`, by name or index; alone, lists them |
| `d func` | deletes a breakpoint |
| `c` | continues until a breakpoint |
| `s` | continues until the prog enters a function |
| `k` | kills the prog |
| `q` | deletes the breakpoints, lets the prog go on, and quits; so does the end of the input |

```
% trace test/debug tick >[2] /tmp/addr &
% mdb 9
stopped in debug::inner
b debug::tick
c
stopped in debug::tick
x 0x436d4 4
000436d4 9c 00 00 00
q
```

Values of locals and arguments need DWARF, which the guests do not
carry, so `mdb` shows functions and memory, not variables.

## What each engine gives

| | wasmtime | wasmi |
|---|---|---|
| stack of a traced prog, running, blocked or broken | yes | yes, the same |
| stack of a prog that was not started traced | no | no |
| profile | by time, 10 ms | by fuel, 2^20 units |
| call and page counts | exact | exact, the same |
| `stop` taken | at a host call or a tick | at a host call or the end of a slice |
| breakpoints, `step`, `heap` | yes | yes, the same |
| trap wording in `exception` | wasmtime's | wasmi's |

wasmtime could also give the stack of an untraced prog at a trap
(`WasmBacktrace`). It is not used: the output would then depend on the
engine, and the kernel has only wasmi.

## Costs

Untraced progs do exactly what they did: one atomic load at each host
call and check point, as before. Traced, the thunk has the breakpoint
check as well, a load and a branch, and counting adds a few
instructions. `xd` of 6 MB piped to `sort`, the best of three runs on
the Apple Silicon build host (aarch64 Linux):

| | wasmtime | wasmi |
|---|---|---|
| untraced, before | 1.47 s | 6.04 s |
| untraced, now | 1.28 s | 5.85 s |
| `trace`, before | 2.01 s | 7.79 s |
| `trace`, now | 2.10 s | 7.80 s |
| `prof` | 2.16 s | 7.84 s |
| `cprof` | 2.18 s | 8.47 s |
| `mprof` | 2.20 s | 9.17 s |

The untraced runs differ only by noise.

## Not done

- **`asm`** (`wat2wasm`, `wasm2wat`). The `wat` and `wasmprinter` crates
  are a pure-Rust path, and both build for `wasm32-wasip1` (Apache-2.0
  with the LLVM exception, which `deny.toml` allows). They are not
  small: built as the guests are, a bare `wat2wasm` is 861 KB and a bare
  `wasm2wat` 1164 KB, where a typical command is 60 to 120 KB, and they
  bring about 20 crates into the guest graph, whose builds must then be
  byte-identical on every host. That is a decision for the self-hosting
  work (`docs/selfhost.md`, stage A), not a side effect of this one.
  For comparison, listing imports with `wasmparser` is 106 KB; `wasmdep`,
  with its own reader and the SDK, is 92 KB.
- Values of locals and globals in `mdb`, and coverage by line in
  `cprof`: both need DWARF, which the guests are built without.
- Allocation sizes in `mprof` (see above).
- **Stacks through reactors.** A reactor loaded with `load` runs
  untraced, so a stack ends at the command's `call`. Tracing it would
  take a stack memory per reactor instance, frames that name their
  module as well as their index (the `stack` file's format already
  allows it), and the engines joining the command's stack and the
  reactor's at each point.
- **Attaching** to a prog that was not started traced. The rewrite must
  happen before the module is compiled, and neither engine can replace
  the code of a running instance. wasmtime can capture the frames of a
  running guest (`WasmBacktrace`) from its own thread, but wasmi cannot,
  and the output would then depend on the engine.
- Demangling the `name` section of a module built elsewhere: `.names`
  files are demangled when written, a module's own section is shown as
  it is.
