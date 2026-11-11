# Performance and the kernel's engine (K4, decision D6)

This is the benchmark suite of issue #27, its results on one host, the
performance targets for decision D6 (`docs/plan.md` Section 12), and what
each D6 option measured: wasmi alone and tuned, wasmtime without `std`
running modules compiled ahead of time on the host, and Pulley,
wasmtime's interpreter. It ends with a recommendation.

All numbers are from 2026-11-11 on one Apple Silicon host (8 cores,
Asahi Linux 6.14, QEMU with KVM and TCG), on `next` at `746aa49` with
this branch's changes. The host was shared with other agents building
the workspace: the load average was 5 to 12 during the runs. Every
figure is the median of six samples (three rounds in each of two boots),
with the fastest sample in brackets, in seconds. Under that load the
fastest sample is the better estimate of the cost; the medians show
what the load added. The host has four fast cores (Firestorm) and four
slow ones (Icestorm): a run whose vCPU the host scheduled on a slow core
is three times slower throughout (one run after the rebase measured the
16 MiB hash at 6.7 s, the next four at 2.2 s), which is another reason
to read the fastest sample. The spread was up to ±200% for some rows, so
differences under about 20% between configurations are not meaningful.

## 1. Summary

Seconds (a spawn in milliseconds), median and fastest. The wasmi and
Pulley columns are built without strict alignment; Pulley also with its
tail-call dispatch (Section 5.3).

| Benchmark (kernel under KVM) | Target | `next` (wasmi) | wasmi, no strict-align | wasmtime AOT | Pulley |
|---|---|---|---|---|---|
| `cat` 100 MiB from the RAM tree | 0.5 | 1.05 (0.67) | 0.14 (0.12) | 0.045 (0.045) | 0.58 (0.58) |
| `cat` 100 MiB through a Styx mount | 1 | 0.82 (0.74) | 0.33 (0.31) | 0.23 (0.21) | 0.76 (0.74) |
| the same, encrypted | 2 | 3.56 (2.44) | 1.94 (1.89) | 1.92 (1.90) | 2.34 (2.31) |
| `cat` 100 MiB from purgfs on virtio-blk | 2 | 4.20 (2.61) | 2.15 (1.37) | 1.40 (1.15) | 1.91 (1.70) |
| SHA-1 of 16 MiB (CPU-bound) | 1 | 2.83 (2.22) | 0.73 (0.73) | 0.092 (0.092) | 1.66 (1.66) |
| `sh`: 10,000 loops of a builtin | 0.5 | 0.76 (0.54) | 0.35 (0.34) | 0.092 (0.082) | 0.78 (0.77) |
| spawn and wait, per spawn (ms) | 1 | 1.35 (1.34) | 1.27 (1.21) | 0.39 (0.39) | 0.43 (0.42) |
| memfs: 1,000 creates | 0.1 | 0.072 (0.065) | 0.051 (0.047) | 0.028 (0.028) | 0.091 (0.091) |
| purgfs: 1,000 creates | 1 | 1.38 (0.86); 7.18 before the fix | 0.82 (0.50) | 0.65 (0.55) | 0.55 (0.52) |

- **`next` misses seven of the ten targets** (Section 4).
  The CPU-bound guest takes three times as long in the kernel as on the
  hosted emu with the same engine (2.2 s against 0.74 s).
- **Most of that is one build setting.** `aarch64-unknown-none` is a
  `+strict-align` target: every unaligned load is compiled as single
  bytes, and wasmi decodes its instruction stream with unaligned loads.
  Built with `-C target-feature=-strict-align`, wasmi in the kernel is as
  fast as on the emu, and the kernel meets every target except spawn
  latency (1.2 ms against 1 ms). Section 5.1.
- **wasmtime runs in the kernel without `std`**, on modules compiled to
  aarch64 on the host and linked into the kernel's text. The prototype
  is behind the kernel's `aot` feature. It is 8 to 24 times faster than
  wasmi for code that computes, and spawns three times faster. Section
  5.2.
- **Pulley is slower than wasmi** for code that computes, even at
  `opt-level` 3 with its tail-call dispatch (1.66 s against 0.73 s); only
  its spawn is fast, because nothing is translated at load. Section 5.3.
- **Two cheap fixes landed:** purgfs directory searches no longer thrash
  the metadata cache (a create in a directory of 1,000 files went from 7
  ms to 0.5 ms), and the kernel builds SHA-256 at `opt-level` 3, which
  the loader runs over every module it loads (a spawn costs 13% less on
  wasmi and a third less with AOT). Section 6.

## 2. The suite

`cargo xtask bench` runs `root/lib/bench/bench.sh` inside the system and
collects its results. Every benchmark is timed inside the system, by
`time` or by the `bench` guest's own clock, so a kernel's boot and QEMU's
start are not in the numbers.

| Name | What it does |
|---|---|
| `cat-root` | `cat` of a 100 MiB file in `/tmp` to `/dev/null`: the kernel's RAM tree (`devram`), or the emu's host directory |
| `cat-styx` | the same file through `mount -A {export -A dir}`: a Styx conversation over a pipe, with the export served in the kernel |
| `cat-styx-sealed` | the same, encrypted: `mount -k` with an authinfo from a signer the exporter trusts, and `export -e` |
| `cat-disk` | a 100 MiB file in purgfs on virtio-blk (`/n/disk`; the kernel only) |
| `sha1-pipe-16m` | `zeros 1024 16384 \| sha1sum`, the CPU-bound guest `docs/footprint.md` uses |
| `sha1-file-16m` | `sha1sum` of a 16 MiB file in `/tmp` |
| `sh-builtin-10k` | `sh -c` with four nested `for` loops over ten words, 10,000 assignments |
| `sh-spawn-1k` | `sh -c` running `echo -n` 1,000 times |
| `spawn-1k-total` | the `bench` guest spawning `echo -n` 1,000 times and waiting for each |
| `fs-{root,memfs,disk}-1k-*` | the `bench` guest creating 1,000 files of 64 bytes in one directory, reading, stating and removing each: on `/tmp`, on a `memfs`, and on purgfs |

```sh
cargo xtask bench                  # emu with both engines, and the kernel under KVM (or TCG)
cargo xtask bench --emu --vm=wasmi
cargo xtask bench --kvm --rounds 3 --boots 2
cargo xtask bench --tcg
cargo xtask bench --kvm --aot native --label aot           # the AOT prototype
cargo xtask bench --kvm --label nsa --rustflags "-C target-feature=-strict-align"
cargo xtask bench --kvm --label chacha --config profile.kernel.package.chacha20.opt-level=3
```

The emu is a release build. The kernel boots with `bench` in its
bootargs (its `Mode::Bench`), 1 GiB of RAM, and one virtio-blk disk that
`xtask` makes: purgfs holding the 100 MiB file and the number of rounds.
`xtask` prints a table (median, fastest, spread) and writes every sample
and the load average before and after each boot to
`target/bench/results.json`. Builds with `--rustflags` or `--config` use
a target directory of their own under `target/bench/build/<label>`.

The suite is not part of CI's normal runs. `.github/workflows/bench.yml`
runs it by hand (`workflow_dispatch`) on an arm64 runner, under TCG,
since GitHub's runners have no KVM.

## 3. Results

Seconds; median of six samples, the fastest in brackets. The columns:

- `emu-wasmtime`, `emu-wasmi`: the release emu with each engine.
- `kvm`: the kernel under KVM as on `next` (wasmi, the `kernel` profile),
  with this branch's purgfs fix.
- `kvm-nsa`: built with `-C target-feature=-strict-align` (Section 5.1).
- `kvm-crypto`, `kvm-nsa-crypto`: with `sha2`, `chacha20`, `poly1305`
  and `chacha20poly1305` at `opt-level` 3.
- `kvm-aot`, `kvm-aot-crypto`: the `aot` feature with native images of
  the suite's nine modules (Section 5.2).
- `kvm-pulley-nsa-tail`: the same with Pulley images, no strict-align,
  and Pulley's tail-call dispatch (Section 5.3).

| benchmark | emu-wasmtime | emu-wasmi | kvm | kvm-nsa | kvm-crypto | kvm-nsa-crypto | kvm-aot | kvm-aot-crypto | kvm-pulley-nsa-tail |
|---|---|---|---|---|---|---|---|---|---|
| cat-root | 0.0703 (0.0507) | 0.153 (0.128) | 1.05 (0.669) | 0.14 (0.123) | 0.533 (0.531) | 0.124 (0.123) | 0.0451 (0.0448) | 0.0449 (0.0449) | 0.581 (0.577) |
| cat-styx | 0.694 (0.524) | 1.22 (0.939) | 0.822 (0.741) | 0.329 (0.31) | 0.74 (0.722) | 0.328 (0.316) | 0.228 (0.211) | 0.224 (0.207) | 0.764 (0.744) |
| cat-styx-sealed | 1.42 (0.953) | 1.56 (1.29) | 3.56 (2.44) | 1.94 (1.89) | 2.65 (2.62) | 1.98 (1.91) | 1.92 (1.9) | 2.12 (2.1) | 2.34 (2.31) |
| sha1-pipe-16m | 0.22 (0.135) | 0.879 (0.757) | 3.42 (2.35) | 0.77 (0.767) | 2.3 (2.29) | 0.772 (0.767) | 0.115 (0.115) | 0.115 (0.115) | 1.77 (1.77) |
| sha1-file-16m | 0.134 (0.0993) | 0.962 (0.741) | 2.83 (2.22) | 0.727 (0.725) | 2.19 (2.19) | 0.728 (0.725) | 0.0923 (0.0921) | 0.0922 (0.0922) | 1.66 (1.66) |
| sh-builtin-10k | 0.0651 (0.0472) | 0.332 (0.298) | 0.764 (0.54) | 0.35 (0.34) | 0.532 (0.522) | 0.361 (0.344) | 0.0919 (0.0823) | 0.0913 (0.0801) | 0.778 (0.768) |
| sh-spawn-1k | 0.824 (0.462) | 2.7 (2.61) | 1.85 (1.53) | 1.41 (1.37) | 1.39 (1.35) | 1.27 (1.24) | 0.413 (0.401) | 0.28 (0.27) | 0.555 (0.541) |
| spawn-1k-total | 0.842 (0.398) | 2.55 (2.27) | 1.35 (1.34) | 1.27 (1.21) | 1.19 (1.16) | 1.12 (1.08) | 0.394 (0.387) | 0.261 (0.255) | 0.426 (0.418) |
| fs-root-1k-create | 0.0367 (0.0203) | 0.0516 (0.0332) | 0.0474 (0.0421) | 0.0447 (0.0368) | 0.0446 (0.0393) | 0.0433 (0.0408) | 0.0363 (0.0304) | 0.0357 (0.0315) | 0.0396 (0.0349) |
| fs-root-1k-read | 0.00789 (0.00745) | 0.0227 (0.00877) | 0.0149 (0.0135) | 0.0135 (0.0124) | 0.0153 (0.014) | 0.0136 (0.0123) | 0.0188 (0.0157) | 0.019 (0.0165) | 0.0219 (0.0191) |
| fs-root-1k-stat | 0.0068 (0.00652) | 0.0184 (0.00918) | 0.0213 (0.02) | 0.0179 (0.0165) | 0.0215 (0.02) | 0.0181 (0.0165) | 0.0231 (0.0194) | 0.0232 (0.0203) | 0.0306 (0.0274) |
| fs-root-1k-remove | 0.0132 (0.0129) | 0.029 (0.0142) | 0.0144 (0.0139) | 0.0131 (0.0127) | 0.014 (0.0134) | 0.0132 (0.0129) | 0.0117 (0.0112) | 0.0114 (0.0108) | 0.0146 (0.0138) |
| fs-memfs-1k-create | 0.173 (0.126) | 0.218 (0.131) | 0.0717 (0.0648) | 0.051 (0.0474) | 0.0666 (0.0652) | 0.0484 (0.048) | 0.028 (0.0278) | 0.0284 (0.0278) | 0.0911 (0.0906) |
| fs-memfs-1k-read | 0.115 (0.0959) | 0.148 (0.075) | 0.0622 (0.0403) | 0.0321 (0.0309) | 0.0418 (0.0407) | 0.0319 (0.0315) | 0.0186 (0.0183) | 0.0186 (0.0184) | 0.0633 (0.0626) |
| fs-memfs-1k-stat | 0.082 (0.0727) | 0.136 (0.0452) | 0.0705 (0.0503) | 0.0376 (0.0366) | 0.0505 (0.0498) | 0.0375 (0.0371) | 0.0192 (0.019) | 0.0192 (0.019) | 0.0738 (0.0731) |
| fs-memfs-1k-remove | 0.0716 (0.0536) | 0.0886 (0.0345) | 0.0478 (0.0296) | 0.0229 (0.0225) | 0.0295 (0.0291) | 0.0229 (0.0226) | 0.0124 (0.0124) | 0.0124 (0.0124) | 0.0452 (0.045) |
| cat-disk | - | - | 4.2 (2.61) | 2.15 (1.37) | 1.98 (1.75) | 2.04 (1.31) | 1.4 (1.15) | 1.21 (1.15) | 1.91 (1.7) |
| fs-disk-1k-create | - | - | 1.38 (0.863) | 0.824 (0.498) | 0.616 (0.529) | 0.594 (0.519) | 0.648 (0.55) | 0.568 (0.516) | 0.55 (0.515) |
| fs-disk-1k-read | - | - | 0.161 (0.123) | 0.0931 (0.0642) | 0.0708 (0.0528) | 0.0688 (0.0567) | 0.0737 (0.0538) | 0.0634 (0.0576) | 0.0678 (0.0615) |
| fs-disk-1k-stat | - | - | 0.0585 (0.0398) | 0.0233 (0.0186) | 0.0221 (0.0216) | 0.019 (0.0185) | 0.0163 (0.0159) | 0.0162 (0.0158) | 0.0235 (0.0231) |
| fs-disk-1k-remove | - | - | 1.2 (0.997) | 0.602 (0.489) | 0.552 (0.467) | 0.492 (0.438) | 0.553 (0.477) | 0.482 (0.421) | 0.438 (0.372) |

The kernel under TCG (a Cortex-A72, GICv3), one round in each of two
boots: wasmi as on `next`, without strict-align, and with native AOT
images. TCG emulates the kernel's own code too, so it is ten times
slower than KVM for everything; the numbers matter only for how long
CI's TCG runs take.

| benchmark | tcg | tcg-nsa | tcg-aot |
|---|---|---|---|
| cat-root | 7.11 (6.98) | 6.71 (6.27) | 0.596 (0.583) |
| cat-styx | 10.8 (10.8) | 13.2 (8.38) | 4.17 (3.73) |
| cat-styx-sealed | 33 (32.9) | 38.2 (29.8) | 40 (38.9) |
| cat-disk | 12.8 (9.5) | 11.7 (7.47) | 4.33 (4.05) |
| sha1-pipe-16m | 28.4 (28.3) | 22.2 (16.4) | 2.5 (1.73) |
| sha1-file-16m | 39.3 (28.3) | 25.5 (15.3) | 2.39 (1.68) |
| sh-builtin-10k | 13.3 (8.58) | 10.3 (6.35) | 4.81 (4.31) |
| sh-spawn-1k | 36.9 (32.2) | 58.8 (54.1) | 3.53 (2.7) |
| spawn-1k-total | 27 (24.8) | 38.9 (36.1) | 3.6 (2.21) |
| fs-root-1k-create | 1.94 (1.23) | 1.35 (1.03) | 1.12 (1.09) |
| fs-root-1k-read | 0.731 (0.398) | 0.472 (0.384) | 0.566 (0.529) |
| fs-root-1k-stat | 0.86 (0.456) | 0.623 (0.423) | 0.585 (0.454) |
| fs-root-1k-remove | 0.571 (0.306) | 0.397 (0.297) | 0.396 (0.299) |
| fs-memfs-1k-create | 2.99 (1.5) | 2.35 (2.12) | 1.23 (1.12) |
| fs-memfs-1k-read | 1.67 (1.03) | 2.49 (1.75) | 1.45 (0.992) |
| fs-memfs-1k-stat | 1.21 (0.977) | 2.28 (1.58) | 1.47 (1.13) |
| fs-memfs-1k-remove | 0.687 (0.661) | 1.29 (0.944) | 1.06 (0.87) |
| fs-disk-1k-create | 3.88 (2.83) | 3.52 (3.23) | 3.4 (2.74) |
| fs-disk-1k-read | 1.08 (0.9) | 1.69 (0.983) | 0.676 (0.429) |
| fs-disk-1k-stat | 0.809 (0.44) | 1.27 (0.813) | 0.493 (0.356) |
| fs-disk-1k-remove | 2.33 (1.44) | 3.35 (2.92) | 2.09 (1.93) |

The load average (1, 5 and 15 minutes) was 5 to 7 during the emu runs,
6 to 13 during the KVM runs and 7 to 12 during the TCG runs. Spreads
(range over median) ran from under 5% to over 200%; the fastest samples
of the same configuration agreed far better than the medians.

## 4. Targets

D6 asked for performance targets. These are for the kernel under KVM on
a host like this one. The reasoning: the kernel should be no slower than
the hosted emu running the same engine, and the I/O paths should not be
what limits a program that streams data.

| Benchmark | Target | Why |
|---|---|---|
| `cat-root` | 0.5 s (200 MB/s) | a RAM file is the fastest source there is |
| `cat-styx` | 1 s (100 MB/s) | faster than any network the kernel has |
| `cat-styx-sealed` | 2 s (50 MB/s) | the cipher in portable Rust; the emu with `ring` does 1 s |
| `cat-disk` | 2 s (50 MB/s) | virtio-blk with 4 KiB blocks |
| `sha1-*-16m` | 1 s | the hosted emu on wasmi: 0.74 to 0.96 s |
| `sh-builtin-10k` | 0.5 s | 50 µs a loop: a script of a thousand lines is instant |
| `spawn-1k-total` | 1 s (1 ms a spawn) | a pipeline of five commands starts in 5 ms |
| `fs-root`, `fs-memfs` | 0.1 s a step (100 µs an operation) | |
| `fs-disk` | 1 s for create and remove, 0.1 s for read and stat | a create commits a journal transaction to the disk |

Against these, with the fastest samples:

| Benchmark | Target | `next` | best wasmi (`kvm-nsa-crypto`) | AOT (`kvm-aot-crypto`) |
|---|---|---|---|---|
| `cat-root` | 0.5 | 0.67, misses | 0.12 | 0.045 |
| `cat-styx` | 1 | 0.74 | 0.32 | 0.21 |
| `cat-styx-sealed` | 2 | 2.44, misses | 1.91 | 2.10, misses (load) |
| `cat-disk` | 2 | 2.61, misses | 1.31 | 1.15 |
| `sha1-file-16m` | 1 | 2.22, misses by 2.2x | 0.73 | 0.092 |
| `sh-builtin-10k` | 0.5 | 0.54, misses | 0.34 | 0.080 |
| `spawn-1k-total` | 1 | 1.34, misses | 1.08, misses by 8% | 0.26 |
| `fs-memfs-1k-*` | 0.1 each | 0.03 to 0.065 | 0.023 to 0.048 | 0.012 to 0.028 |
| `fs-disk-1k-create`, `-remove` | 1 each | 0.86, 1.00 (7.2, 5.4 before the fix) | 0.52, 0.44 | 0.52, 0.42 |
| `fs-disk-1k-read`, `-stat` | 0.1 each | 0.12, misses; 0.04 (7.2, 7.3 before the fix) | 0.057, 0.019 | 0.058, 0.016 |

`next` as it is misses seven of the ten. The best wasmi build misses
only spawn latency, narrowly. AOT meets all of them; its encrypted read
(2.10 s fastest, 1.90 s in the run without the crypto settings) is at
the target, limited by the kernel's record layer, not by the engine.

## 5. The D6 options

### 5.1 wasmi alone, tuned

**Unaligned loads.** The kernel's target, `aarch64-unknown-none`,
enables `+strict-align`, so LLVM never emits an unaligned load or store:
`ptr::read_unaligned::<u64>` becomes eight `ldrb` and seven `orr`. The
boot path needs this, because it runs Rust with the MMU off, when all of
memory is Device memory and an unaligned access faults
(`purg_aarch64::boot`). But it applies to every crate, and wasmi reads
its instruction stream (16- and 32-bit operands at any alignment) and
guest memory with unaligned loads. Building the kernel with
`-C target-feature=-strict-align` makes the CPU-bound guest three times
as fast (2.22 s to 0.73 s), `cat-root` five times (0.67 s to 0.12 s), and
`sh-builtin-10k` 1.6 times. `SCTLR_EL1.A` is clear, so unaligned
accesses to Normal memory are allowed once the MMU is on.

The flag is not adopted here. Two things have to be settled first: the
code that runs with the MMU off (`boot_map` and what it calls) must not
contain an unaligned access, which today is guaranteed by the target and
nothing else; and the flag has to reach every kernel build, including
CI's, where `RUSTFLAGS=-D warnings` replaces any `rustflags` in
`.cargo/config.toml`. rustc also warns that `strict-align` is an
unstable feature name. The kernel booted and ran the whole suite under
KVM built this way, which shows the boot path happens to be clean now,
not that it stays so. Options, in order of preference: compile only the
boot path with strict alignment (a crate of its own, built by a wrapper
that adds the flag for it alone), or keep the flag global and add a
check that `boot_map`'s object code has no unaligned access.

**Optimisation levels.** wasmi is already at `opt-level` 3 in the kernel
profile (`docs/footprint.md` Section 2). The crates the kernel runs on
the hot paths were at `"z"`. Putting `sha2` at 3 makes a spawn 13%
cheaper on wasmi (1.34 to 1.16 ms) and 34% cheaper with AOT (0.39 to
0.26 ms): the loader takes the SHA-256 digest of every module it loads
for the module policy (`docs/signing.md`). This is adopted. Putting
`chacha20`, `poly1305` and `chacha20poly1305` at 3 as well made no
difference to `cat-styx-sealed` (1.89 s against 1.91 s), so they stay at
`"z"`; that path's cost is elsewhere (Section 6).

**Fuel.** wasmi runs on fuel in slices of 2^20 units, about a million
instructions, then checks the prog's note and resumes (`purg-vm-wasmi`).
At 0.73 s for the 16 MiB hash, a slice lasts a few milliseconds and one
check costs a few microseconds, well under 1%, so the slice size is not
worth tuning. Turning fuel off is not an option: it is how a computing
prog is killed.

**Dispatch.** wasmi's `indirect-dispatch` (a compact IR, documented as
10 to 15% slower) and `portable-dispatch` (a loop instead of tail calls)
were not measured; `docs/footprint.md` Section 7.1 has why they matter
for memory and stack depth, not speed.

So the best wasmi kernel meets every target but spawn latency, which is
the time to read, hash and translate a module and to start an engine
for it (the kernel gives each prog its own wasmi engine, #79).

### 5.2 wasmtime without `std`, modules compiled on the host

wasmtime 50.0.1 builds for `aarch64-unknown-none` with
`default-features = false, features = ["runtime",
"custom-sync-primitives", "pulley"]`. There is no compiler on the
device: the host compiles each module with Cranelift for the
`aarch64-unknown-none` target, and the kernel runs the result. The
prototype is the crate `purg-vm-aot`, an engine for `purg-vm`, and the
kernel's `aot` feature. What it needs from the kernel:

- **Thread-local storage:** one pointer per thread
  (`wasmtime_tls_get`/`wasmtime_tls_set`). A prog's wasm runs only on its
  own kernel thread, so the prototype keeps a map from pid to pointer.
  A word in the scheduler's thread would be cheaper.
- **Locks:** without `custom-sync-primitives`, wasmtime's `no_std`
  locks panic on contention, and the kernel preempts. The prototype's
  `wasmtime_sync_*` spin on a word and yield.
- **No virtual memory and no signals.** With `signals_based_traps(false)`,
  no reservation and no guard pages, linear memory is a heap allocation
  that moves when it grows, every load and store has an explicit bounds
  check, a division checks its operands, and each function's prologue
  checks the stack against a limit (128 KiB of the prog's 256 KiB thread
  stack). Nothing needs the MMU or an exception handler. A trap unwinds
  through wasmtime's own frame walk, with no C code and no `setjmp`.
- **Executable code.** The kernel maps its heap never-executable. Rather
  than make heap pages executable at run time (which needs 4 KiB
  remapping of the linear map and TLB maintenance, and would make data
  written at run time executable), the image set is linked into the
  kernel's text (`.text.aot`), which is already read-only and executable.
  The engine loads each image in place with `Module::deserialize_raw`;
  its `CustomCodeMemory` refuses to publish code outside the set.
  wasmtime writes nothing into an image: a `.cwasm` has no relocations.
- **Interrupting a guest:** epochs. A kernel thread advances the epoch
  every 10 ms; at the next loop head or function entry the guest checks
  its prog's note and stops if killed, as wasmi does at the end of a
  slice.
- **Choosing the engine:** an image is found by the SHA-256 digest of
  its module, which the loader already takes for the module policy. A
  module with no image, and every module of a traced prog (whose module
  is rewritten, `docs/debug.md`), runs on wasmi. A reactor loaded by a
  prog running native code must have an image; one loaded from wasmi
  runs on wasmi as before.

**Producing and checking images.** `cargo run -p purg-vm-wasmtime --bin
cwasm -- -k native -o set.bin a.wasm ...` compiles modules with the same
`purg_vm_aot::config` the kernel runs them with (wasmtime refuses an
image built with other settings) and writes one set: a header, then per
module its digest and the image, each aligned to 64 KiB. The target is
named, not detected, so no feature of the build host's CPU goes into the
code; two builds of the same modules gave byte-identical sets here. The
cross-host check CI does for `.wasm` would apply to sets as well. Since
an image is native code the kernel runs, it must be trusted as the
kernel is: today that holds because the set is part of the kernel image.
Loading images at run time (from the initrd or a disk) would need them
signed, with the module's digest and the image's digest both under the
signature (`docs/signing.md`), and the remapping above.

**Size.** The runtime adds about 600 KiB to the kernel's text (from
1,660 KiB to 2,259 KiB, the image set aside): wasmtime 179 KiB, Pulley's
interpreter 79 KiB (the `pulley` feature; a native-only build could drop
it), `wasmtime-environ` 51 KiB, `purg-vm-aot` 51 KiB, `postcard` and
`serde` 42 KiB, the rest in `core` and `alloc`. wasmi stays, as the
fallback. Images are
large: for the nine modules of the suite (`sh`, `cat`, `echo`,
`sha1sum`, `zeros`, `time`, `memfs`, `export`, `test/bench`, 0.99 MB of
wasm) the set is 5.2 MB, 5.2 times the modules, mostly code (`sh`: 340
KiB of wasm, 1.25 MiB of code, explicit bounds checks included). That is
without the address map (no wasm offsets in traps), which saved 12%.
All 162 modules of `root/bin` (16.9 MB) make an 84 MB set, so a real
system would compile the commands that matter and leave the rest to
wasmi.

**Speed.** Native code is 8 times wasmi's speed for the CPU-bound guest
(0.092 s against 0.73 s with wasmi at its best, 24 times against `next`)
and as fast as the hosted emu's wasmtime (0.099 s fastest), which runs
the same Cranelift code with guard pages instead of bounds checks.
`sh-builtin-10k` is 4 times faster, a spawn 3 to 4 times. The I/O
benchmarks are limited by the kernel, not the guest: `cat-styx` 0.21 s,
`cat-disk` 1.15 s, `cat-styx-sealed` 1.9 s, the same as with wasmi.

**Correctness.** With images of all 162 modules linked in, the kernel's
whole test mode under KVM (`cargo xtask kernel --kvm --mem 2G` with the
feature on) ran 204 tests: the conformance guests, the sh corpus
(kills, reactors, pipes, mounts, the network and the disk), and every
one passed but one. `mash.sh` stops in its deepest-recursion check: on
native code the frames are larger than wasmi's, and the 128 KiB wasm
stack runs out before `mash`'s own nesting limit, where the emu's
wasmtime has 512 KiB. The leak check also failed, by design of the
prototype: it keeps every deserialized module (2.4 MB of metadata for
the 162). Running AOT progs on larger thread stacks, and freeing
modules, are follow-ups.

### 5.3 Pulley

The same prototype runs Pulley bytecode (`-k pulley`, `--aot pulley`):
no executable memory at all, and images 2.5 times the module instead of
5.5. Its interpreter is slower than wasmi:

| CPU-bound guest (`sha1-file-16m`) | seconds |
|---|---|
| Pulley, interpreter at `"z"` (the kernel profile's default) | 5.03 |
| Pulley at `opt-level` 3 | 3.60 |
| Pulley at 3, no strict-align | 3.35 |
| Pulley at 3, no strict-align, `--cfg pulley_assume_llvm_makes_tail_calls` | 1.66 |
| wasmi at 3, no strict-align | 0.73 |

The first three rows are single runs, the last two the fastest of six.

wasmtime's documentation expects Pulley to be about ten times slower
than native code; here it is 18 times. Its one advantage is that a spawn
translates nothing (0.42 ms against wasmi's 1.2). That is not worth a
second interpreter: wasmi keeps fuel, runs any module unprepared, and
is already in the kernel.

## 6. Hot spots

Fixed here:

- **purgfs directory searches thrashed the cache.** A walk, a create or a
  stat searched the directory through `Fs::entries`, which decodes every
  entry into a vector, and the mount kept 32 metadata blocks, least
  recently used out first. A directory of 1,000 entries is 63 blocks, so
  each search evicted every block before it was needed again and read the
  whole directory from the disk: 5 to 7 ms an operation. The search now
  decodes only the entry whose name matches and stops there
  (`Dentry::has_name`), and the kernel mounts with a cache of 128 blocks
  (512 KiB at most, used only as directories grow). Under KVM, for 1,000
  files: create 7.18 s to 0.50 s, read 7.23 s to 0.06 s, stat 7.32 s to
  0.02 s, remove 5.43 s to 0.49 s (the first runs, wasmi without
  strict-align and before, against the fastest samples after).
- **SHA-256 at `opt-level` `"z"`** on every module load (Section 5.1).

Found and not fixed:

- **Strict alignment** (Section 5.1): the largest single cost.
- **Encrypted Styx** reads at 50 MB/s whatever the engine and whatever
  the cipher's optimisation level; the clear mount does 330 to 500 MB/s. The
  record layer seals each message into a fresh allocation and the
  reader opens into another; that and the cipher's portable code are
  the suspects. Not profiled: the host has no `perf`.
- **purgfs reads one 4 KiB block per virtio-blk request**: 100 MiB is
  25,600 requests, 45 µs each under KVM. Reading contiguous blocks in one
  request would help large files.
- **Each Styx read copies the data four times**: the export reads into
  a buffer, encodes `Rread` into another, the pipe queues a copy, the
  mount reader copies it out, and `devmnt` copies it into the caller's
  buffer. Under KVM the copies cost less than the thread switches
  between the reader, the export worker and the prog.
- **The emu's numbers predate the fix to hosted TCP reads**, which
  polled every 5 ms instead of blocking (the `lethe-perf` work). The
  suite's Styx mounts run over pipes, not TCP, so `cat-styx` should not
  change, but the emu rows were not measured again after it.
- **The emu's Styx path is slower than the kernel's** (`cat-styx` 0.52 s
  on the emu with wasmtime against 0.21 s in the kernel with AOT): each
  message wakes two host threads through condition variables.
- **wasmi spawns translate the module from scratch** in a fresh engine
  per prog (#79), about 1 ms of a 1.2 ms spawn. Sharing translated code
  between progs would need wasmi to free a module's code (it does not;
  `docs/footprint.md` Section 4).

## 7. Recommendation

1. **Keep wasmi as the kernel's engine and make it fast:** build the
   kernel without strict alignment once the boot path is protected
   (Section 5.1). That alone meets every D6 target but spawn latency, at
   no cost in size or risk to the sandbox.
2. **Adopt wasmtime AOT as the kernel's speed option, not its only
   engine.** It works without `std`, needs only thread-local storage
   and locks from the kernel, and runs code that computes an order of
   magnitude faster than any interpreter. wasmi stays for modules
   without an image: third-party and unsigned modules, traced progs,
   modules that arrive at run time. Before it can be on by default it
   needs: images loaded at run time and signed (the MMU remap and
   `docs/signing.md`), a per-thread TLS word in the scheduler, the
   cross-host determinism check in CI, and the sh corpus passing under
   it in CI (it passes locally; Section 5.2).
3. **Do not pursue Pulley.** It is slower than wasmi in this kernel and
   brings nothing wasmi lacks except fast loading.

## 8. What remains

- The `-smp 4` stress test and per-prog memory limits of #27 (other
  work).
- Strict alignment in the boot path only (Section 5.1).
- AOT: run-time image loading with signatures, TLS in the scheduler,
  merging `purg-vm-aot` with `purg-vm-wasmtime`'s host functions (the
  prototype copies them), and CI running the test mode with it.
- The hot spots in Section 6.
