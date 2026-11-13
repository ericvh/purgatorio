# Performance and the kernel's engine (K4, decision D6)

This is the benchmark suite of issue #27, its results on one host, the
performance targets for decision D6 (`docs/plan.md` Section 12), and what
each D6 option measured: wasmi alone and tuned, wasmtime without `std`
running modules compiled ahead of time on the host, and Pulley,
wasmtime's interpreter. It ends with a recommendation. Section 9 is the
follow-up that adopted the first part of it: the kernel built without
strict alignment, and wasmi's modules compiled lazily and shared.
Section 10 is the data path, and Section 11 the kernel heap on several
CPUs (#193). Section 12 makes the AOT engine a real option: images
loaded at run time and signed, and the whole test mode passing on it.

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
tail-call dispatch (Section 5.3). The last column is wasmi as the kernel
is built now (Section 9), measured a day later under similar load; the
one after it adds the data path changes of Section 10.

| Benchmark (kernel under KVM) | Target | `next` (wasmi) | wasmi, no strict-align | wasmtime AOT | Pulley | wasmi now | + data path |
|---|---|---|---|---|---|---|---|
| `cat` 100 MiB from the RAM tree | 0.5 | 1.05 (0.67) | 0.14 (0.12) | 0.045 (0.045) | 0.58 (0.58) | 0.12 (0.12) | 0.012 (0.011) |
| `cat` 100 MiB through a Styx mount | 1 | 0.82 (0.74) | 0.33 (0.31) | 0.23 (0.21) | 0.76 (0.74) | 0.28 (0.28) | 0.101 (0.099) |
| the same, encrypted | 2 | 3.56 (2.44) | 1.94 (1.89) | 1.92 (1.90) | 2.34 (2.31) | 1.89 (1.88) | 0.549 (0.548) |
| `cat` 100 MiB from purgfs on virtio-blk | 2 | 4.20 (2.61) | 2.15 (1.37) | 1.40 (1.15) | 1.91 (1.70) | 1.40 (1.23) | 0.142 (0.131) |
| SHA-1 of 16 MiB (CPU-bound) | 1 | 2.83 (2.22) | 0.73 (0.73) | 0.092 (0.092) | 1.66 (1.66) | 0.73 (0.73) | 0.73 (0.73) |
| `sh`: 10,000 loops of a builtin | 0.5 | 0.76 (0.54) | 0.35 (0.34) | 0.092 (0.082) | 0.78 (0.77) | 0.35 (0.35) | 0.35 (0.35) |
| spawn and wait, per spawn (ms) | 1 | 1.35 (1.34) | 1.27 (1.21) | 0.39 (0.39) | 0.43 (0.42) | 0.26 (0.26) | 0.26 (0.26) |
| memfs: 1,000 creates | 0.1 | 0.072 (0.065) | 0.051 (0.047) | 0.028 (0.028) | 0.091 (0.091) | 0.048 (0.048) | 0.049 (0.048) |
| purgfs: 1,000 creates | 1 | 1.38 (0.86); 7.18 before the fix | 0.82 (0.50) | 0.65 (0.55) | 0.55 (0.52) | 0.58 (0.53) | 0.48 (0.35) |

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
- **Since then (Section 9)** the kernel is built without strict
  alignment outside the code that runs with the MMU off, and wasmi
  compiles modules lazily and shares engines between progs in
  generations. wasmi now meets every target, and a spawn takes 0.26 ms.
- **AOT is now a real option (Section 12):** signed image files beside
  the modules, loaded at run time into a W^X code region; the whole
  test mode passes with every module compiled; CI runs it and checks the
  images are the same on every host. It stays off by default.
- **Then the data path (Section 10):** a kernel profiler (`#T`) showed
  the encrypted mount's cipher built for size, `cat` searching every
  block for a newline in std's line-buffered stdout, byte-at-a-time pipe
  reads and one disk request per block. With those fixed, `cat` of 100
  MiB takes 0.011 s from RAM, 0.10 s through a Styx mount, 0.55 s
  encrypted and 0.13 s from purgfs.
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
cargo xtask bench --kvm --label strict --strict-align        # strict alignment throughout, as before Section 9
cargo xtask bench --kvm --label chacha --config profile.kernel.package.chacha20.opt-level=3
```

The emu is a release build. The kernel boots with `bench` in its
bootargs (its `Mode::Bench`), 1 GiB of RAM, and one virtio-blk disk that
`xtask` makes: purgfs holding the 100 MiB file and the number of rounds.
`xtask` prints a table (median, fastest, spread) and writes every sample
and the load average before and after each boot to
`target/bench/results.json`. Builds with `--strict-align`, `--rustflags` or `--config`
use a target directory of their own under `target/bench/build/<label>`.
`--rustflags` adds to the environment's flags and to the kernel's own
(Section 9).

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

The flag was not adopted at first. Two things had to be settled: the
code that runs with the MMU off (`boot_map` and what it calls) must not
contain an unaligned access, which was guaranteed by the target and
nothing else; and the flag has to reach every kernel build, including
CI's, where `RUSTFLAGS=-D warnings` replaces any `rustflags` in
`.cargo/config.toml`. rustc also warns that `strict-align` is an
unstable feature name. Section 9 settles both, and the kernel and the
probe are now built this way.

**Optimisation levels.** wasmi is already at `opt-level` 3 in the kernel
profile (`docs/footprint.md` Section 2). The crates the kernel runs on
the hot paths were at `"z"`. Putting `sha2` at 3 makes a spawn 13%
cheaper on wasmi (1.34 to 1.16 ms) and 34% cheaper with AOT (0.39 to
0.26 ms): the loader takes the SHA-256 digest of every module it loads
for the module policy (`docs/signing.md`). This is adopted. Putting
`chacha20`, `poly1305` and `chacha20poly1305` at 3 as well made no
difference to `cat-styx-sealed` (1.89 s against 1.91 s). That was
because the cipher's generic code is instantiated in `purg-auth`, which
was still at `"z"`; with `purg-auth` at 3 too, the encrypted read takes
half the time (Section 10).

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
for it (the kernel gave each prog its own wasmi engine, #79). Section 9
brings it to a quarter of a millisecond.

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

**Producing and checking images.** `cargo run -p purg-cwasm --bin
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
modules, are follow-ups, done in Section 12.

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

- **Strict alignment** (Section 5.1): the largest single cost. Fixed
  in Section 9.
- **Encrypted Styx**, **purgfs's one request per block** and **the
  copies in a Styx read**: profiled and mostly fixed; Section 10.
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
  `docs/footprint.md` Section 4). Fixed in Section 9 with lazy
  compilation and engines shared in generations, which are freed whole.

## 7. Recommendation

1. **Keep wasmi as the kernel's engine and make it fast:** build the
   kernel without strict alignment once the boot path is protected
   (Section 5.1). That alone meets every D6 target but spawn latency, at
   no cost in size or risk to the sandbox. Done, with spawn latency too
   (Section 9).
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
- AOT: done in Section 12, which lists what is left of it.
- The hot spots in Section 6, and what Section 10.4 leaves.
- The SHA-256 digest is most of a spawn now (Section 9.2): the CPU's
  SHA-256 instructions, detected at boot.

## 9. Follow-up: strict alignment and the module cache

Measured on 2026-11-12, on the same host, on `next` at `4a6cd0b` with
these changes. Other agents were building the workspace: the load
average was 5 to 9 during the KVM runs and 4 to 6 during the emu runs.
Each KVM column is three rounds in each of two boots, median and
fastest in brackets, in seconds.

### 9.1 Strict alignment outside the boot path

The kernel and the probe are now built with `-C
target-feature=-strict-align`, except for `purg-aarch64` and every crate
it depends on, which keep the target's strict alignment
(`xtask/src/align.rs`). Those crates hold all the Rust code that runs
with the MMU off: `purg_boot_map` and `purg_boot_enable`, which `_start`
calls before the MMU is on (`purg_aarch64::boot`). The rest of that
path, the secondary CPUs' path up to their MMU switch and the early
exception vectors are assembly, which the flag does not touch.

Why that is enough: rustc compiles each function with the target
features of the crate that generates its code, and records them on the
function, so fat LTO keeps them. A crate's code can call only its own
functions, those of the crates it depends on (including the generic
instantiations it reuses from them) and the prebuilt `core`, `alloc` and
`compiler_builtins`, which are built for the target as it is. A function
of a crate built without strict alignment can be inlined into a strict
one, but it is then compiled as part of the strict one. What the
compiler cannot see is a call to an `extern` function defined in a
later crate, or a call through a pointer. So `xtask kernel`, `probe`
and `bench` read each image they build (`size` and `lethe-perf` build
the same way but do not check), follow every `bl` and `b` from those
two functions, and fail if one reaches a function of another crate or
a `blr`. Functions that LLVM's machine outliner made at `opt-level =
"z"` (`OUTLINED_FUNCTION_N`) hold instructions copied from their
callers and are followed like them. Today the kernel's boot path
reaches 32 functions and the probe's 4. Comparing the object code of
the kernel's twelve named ones, with the outlined parts put back in
place, between a kernel built with strict alignment throughout and one
built this way, found them identical.

How the flag reaches the build: xtask passes it in
`CARGO_ENCODED_RUSTFLAGS`, which takes precedence over `RUSTFLAGS` and
the config, after the environment's own flags, so CI's `-D warnings`
still applies. Cargo has no stable way to give one crate other flags,
so xtask runs itself as cargo's `RUSTC_WRAPPER` for these builds and
adds `-C target-feature=+strict-align` for the strict crates; a
`--cfg` naming that set goes in every crate's flags, so that cargo,
which does not see what the wrapper adds, rebuilds when the set
changes. The wrapper also drops rustc's warning that `strict-align` is not a known
feature: it is not a lint, so `-D warnings` leaves it a warning, nothing
can allow it, and it would be printed once for every crate. A plain
`cargo build -p purg-kernel --target aarch64-unknown-none` still builds
with strict alignment throughout, which is safe and slow;
`cargo xtask bench --strict-align` builds that way on purpose.

The kernel's text is 328 KiB smaller (1,778,276 bytes to 1,441,968),
since an unaligned load is one instruction instead of fifteen. The
kernel passes its test mode under KVM and TCG, with one and four CPUs,
and the probe passes.

### 9.2 Spawn latency

With the alignment fixed, a spawn under KVM cost 1.1 ms. On the host,
`Module::new` for `echo` took 540 µs in wasmi's default mode and 40 µs
in its lazy mode. Timing a spawn's parts inside the kernel with lazy
compilation (one run of the suite, averaged over 1,000 spawns of
`echo`): reading the module 15 µs, the module policy (its SHA-256
digest) 190 µs, `Module::new` 60 µs, the linker 17 µs, instantiation 21
µs, and the run of `echo` itself, most of which is translating the
functions it calls, 340 to 440 µs.

Two changes, both in `purg-vm-wasmi`:

- **Lazy compilation.** wasmi's default mode validates the whole
  module at load and translates each function on its first call;
  `CompilationMode::Lazy` validates each function on its first call
  too. `Module::new` then checks only the module's structure (types,
  imports, exports, sections): 60 µs instead of 540 µs for `echo`, and
  a spawn costs 0.80 ms. The cost is that a function body that is not
  valid WebAssembly is found only when it is called, and the call
  traps; the prog may have run until then. Every function is still
  validated before it runs. The emu's wasmtime backend, which the
  hosted tests run every installed module on, still validates whole
  modules at load.
- **Engines in generations** (`docs/footprint.md` Section 4). The
  current generation's engine keeps each module it has loaded, by the
  digest the module policy takes anyway, so a command run again is
  neither parsed nor translated again, and its progs share one linker.
  A generation lives while a prog that started in it runs: when the
  last one exits, its engine and all its code are freed, as an engine
  per prog was. It stops taking new progs once 1 MiB of modules has
  been loaded into it, or when one is loaded with under a quarter of
  memory free; then a new prog starts a new generation, or, if memory
  is tight, runs in an engine of its own. A traced prog's module is
  rewritten, so its digest does not name it: it always gets an engine
  of its own. Shared engines keep no value stacks between calls. A
  spawn costs 0.26 ms.

The modules a long-running prog's generation holds stay until that prog
exits: an interactive shell keeps the commands run beside it, up to
1 MiB of modules and about four times as much translated, then a new
generation starts. Translated code cannot be freed module by module in
wasmi 2.

Memory in the kernel's test mode under KVM (`KERNEL_MEMSTAT=1`), an
engine per prog against generations: the heap at the end is the same,
1,495 KiB; the largest peaks of a single test are within 10% of each
other (24.4 and 57.6 MiB against 26.6 and 51.7 MiB, the 64 MiB block of
the heap self-test aside); the run makes 37% fewer allocations (6.01 M
against 3.80 M). The leak check passes.

### 9.3 Results

| benchmark | Target | strict (as before) | no strict-align | + lazy | + generations |
|---|---|---|---|---|---|
| cat-root | 0.5 | 0.533 (0.532) | 0.124 (0.123) | 0.123 (0.122) | 0.123 (0.122) |
| cat-styx | 1 | 0.735 (0.721) | 0.320 (0.309) | 0.296 (0.284) | 0.278 (0.277) |
| cat-styx-sealed | 2 | 2.42 (2.40) | 1.90 (1.88) | 1.87 (1.85) | 1.89 (1.88) |
| cat-disk | 2 | 1.92 (1.80) | 1.34 (1.27) | 1.39 (1.15) | 1.40 (1.23) |
| sha1-pipe-16m | 1 | 2.30 (2.29) | 0.768 (0.767) | 0.765 (0.764) | 0.766 (0.765) |
| sha1-file-16m | 1 | 2.19 (2.19) | 0.728 (0.726) | 0.727 (0.727) | 0.728 (0.728) |
| sh-builtin-10k | 0.5 | 0.588 (0.586) | 0.386 (0.375) | 0.358 (0.350) | 0.352 (0.349) |
| sh-spawn-1k | | 1.38 (1.36) | 1.27 (1.24) | 0.851 (0.836) | 0.327 (0.326) |
| spawn-1k-total | 1 | 1.19 (1.15) | 1.11 (1.09) | 0.795 (0.763) | 0.263 (0.261) |
| fs-root-1k-create | 0.1 | 0.046 (0.043) | 0.045 (0.042) | 0.039 (0.034) | 0.029 (0.028) |
| fs-root-1k-read | 0.1 | 0.015 (0.015) | 0.014 (0.014) | 0.014 (0.013) | 0.013 (0.013) |
| fs-root-1k-stat | 0.1 | 0.022 (0.021) | 0.019 (0.018) | 0.018 (0.018) | 0.017 (0.017) |
| fs-root-1k-remove | 0.1 | 0.016 (0.015) | 0.015 (0.015) | 0.014 (0.014) | 0.013 (0.012) |
| fs-memfs-1k-create | 0.1 | 0.068 (0.067) | 0.051 (0.050) | 0.051 (0.050) | 0.048 (0.048) |
| fs-memfs-1k-read | 0.1 | 0.042 (0.041) | 0.033 (0.033) | 0.033 (0.033) | 0.032 (0.031) |
| fs-memfs-1k-stat | 0.1 | 0.050 (0.050) | 0.038 (0.037) | 0.038 (0.038) | 0.038 (0.037) |
| fs-memfs-1k-remove | 0.1 | 0.030 (0.030) | 0.024 (0.024) | 0.024 (0.024) | 0.023 (0.023) |
| fs-disk-1k-create | 1 | 0.596 (0.580) | 0.559 (0.479) | 0.605 (0.479) | 0.580 (0.528) |
| fs-disk-1k-read | 0.1 | 0.076 (0.067) | 0.068 (0.065) | 0.080 (0.061) | 0.070 (0.064) |
| fs-disk-1k-stat | 0.1 | 0.023 (0.022) | 0.020 (0.019) | 0.020 (0.019) | 0.019 (0.019) |
| fs-disk-1k-remove | 1 | 0.501 (0.396) | 0.459 (0.417) | 0.448 (0.426) | 0.492 (0.405) |

A second run with strict alignment throughout agreed with the first to
within 5% on every median. The disk rows vary by up to 30% between runs
of the same build.

The kernel now meets every D6 target (Section 4), spawn latency by a
factor of four, and spawns faster than the AOT prototype did (0.26 ms
against 0.26 to 0.39 ms). Code that computes is still 8 times slower
than AOT (0.73 s against 0.092 s for the 16 MiB hash).

The emu on wasmi gains the same way (three rounds, median and fastest):
`spawn-1k-total` 2.62 s (2.09) before, 0.35 s (0.33) and 0.36 s (0.35) in
two runs after; `sh-spawn-1k` 2.74 s (2.57) before, 0.41 s (0.37) and 0.43
s (0.42) after. Its Styx and memfs rows moved by up to 30% either way
between runs, with or without the change.

What remains for spawns: the SHA-256 digest of each module is now most
of a spawn (190 µs of 260 µs for `echo`, by the timing above). The
kernel's `sha2` is the portable code; the CPU's SHA-256 instructions
(FEAT_SHA256, which this host has) would need detection at boot, since
not every aarch64 CPU has them.

## 10. The data path

The follow-ups of Section 6 for the I/O benchmarks, measured with a
profiler built for them. Numbers are from 2026-11-12, same host, on
`next` at `d1ca20f` (the base, before Section 9) and with these
changes, and the last column of the table in 10.4 with both; the load average
was 4 to 9 (other agents were building). Seconds, median of six samples
(three rounds in each of two boots) with the fastest in brackets, under
KVM with wasmi as on `next`.

### 10.1 The profiler: `#T`

The host has no `perf`, so the kernel profiles itself. With `kprof` in
the bootargs (`kprof=HZ` for a rate other than 1,000 a second) the
kernel binds `#T` after `/dev`, after Inferno's kprof(3):

- `kpctl` takes `start`, `stop` and `startclr`;
- `kpdata` is big-endian 32-bit counts: the samples, the samples in the
  kernel's text, then one count per 8 bytes of text;
- `kpcaller` is the same for the link register (`x30`), which charges a
  leaf such as `memcpy` to its caller. It is not in Inferno.

The clock is the EL1 physical timer, which the scheduler does not use,
so the 100 Hz tick and its preemption are unchanged; each sample costs
an interrupt. `cargo xtask bench --kprof` boots the kernel at 2,000
samples a second; `bench.sh` brackets each I/O benchmark with
`startclr` and `stop` (outside the timed command) and copies the counts
to the bench disk, and xtask sums them over rounds and boots and charges
each bucket to a function from the kernel ELF's symbol table. It prints
the top functions by PC and by caller per benchmark and writes them to
`target/bench/kprof-kvm+LABEL.txt`. Limits: only the CPU that starts
profiling is sampled, and code that runs with interrupts masked is
charged to where it unmasks them. The device costs nothing without the
bootarg; with it, two histograms half the size of the text (1.8 MiB
in all for 1.8 MiB of text).

### 10.2 What it found

| Benchmark | Where the time went (base) |
|---|---|
| `cat-root` | 97% in wasmi running `cat` itself: `std::io::stdout` is line-buffered, so every 8 KiB block was searched for a newline, in interpreted code |
| `cat-styx` | `cat`'s own search again (70%), then `Queue::read`, which moved each byte through `VecDeque::drain` (9%), and copies |
| `cat-styx-sealed` | ChaCha20 (45%), Poly1305 (6%), `memcpy` (14%), `cat` (24%). The cipher's quarter round was an out-of-line function: code at `"z"` |
| `cat-disk` | 59% idle waiting for the disk and 7% kicking it, one request per 4 KiB block; `cat` 31% |
| `fs-disk-1k` | 55% idle, 20% kicking the disk: each create or remove is a dozen requests in a row |

The record layer's allocations, the first suspect, were under 2%.

### 10.3 Changes

- **The cipher at `opt-level` 3**, with `purg-auth`, where its generic
  code is instantiated (`Cargo.toml`). Alone: `cat-styx-sealed` 2.43 s
  to 1.44 s.
- **NEON ChaCha20**: the record layer takes its AEAD from
  `chacha20poly1305` 0.11, whose ChaCha20 (0.10) uses NEON on aarch64.
  0.9 needs `--cfg chacha20_force_neon`, which CI's `RUSTFLAGS` would
  drop. The records are the same bytes (the fixed vectors pass); the
  login protocol and Lethe stay on 0.10 for now.
- **Records without a copy or an allocation each**: `Sealer::seal_buf`
  builds each record in a buffer the sealer reuses, and
  `Opener::open_body` opens a record read without its header in place,
  so the plaintext does not move. The mount reader and export use them.
- **Pipe reads copy slices**: `Queue::read` copies the ring's two runs.
- **`cat` writes each read straight to fd 1**, as Inferno's cat does,
  not through `std::io::stdout`. This is the largest change for every
  `cat` benchmark, and it is the guest's cost, not the kernel's.
- **purgfs reads contiguous blocks in one request** and a sequential read
  takes up to 64 KiB ahead (`docs/fs.md`, Memory); the virtio-blk
  driver's bounce buffers are 64 KiB (from 16), so a read-ahead is one
  request.
- **A commit writes its home blocks at once**: `Disk::write_many`, which
  the kernel's disk keeps up to four requests in flight for. The order
  of the journal and the flushes is unchanged; the crash test passes.

### 10.4 Results

`kernel only` is every change but `cat`'s (with `cat` as on `next`); it
ran under a load average of 7 to 9, so its medians are three times its
fastest samples: read the brackets.

| Benchmark | Target | base | kernel only | all | all, with Section 9 |
|---|---|---|---|---|---|
| `cat-root` | 0.5 | 0.536 (0.533) | 1.59 (0.533) | 0.016 (0.014) | 0.012 (0.011) |
| `cat-styx` | 1 | 0.738 (0.721) | 1.95 (0.664) | 0.147 (0.140) | 0.101 (0.099) |
| `cat-styx-sealed` | 2 | 2.43 (2.40) | 3.08 (1.14) | 0.618 (0.613) | 0.549 (0.548) |
| `cat-disk` | 2 | 2.40 (1.70) | 1.81 (0.852) | 0.203 (0.178) | 0.142 (0.131) |
| `fs-disk-1k-create` | 1 | 0.586 (0.478) | 0.782 (0.580) | 0.630 (0.357) | 0.481 (0.350) |
| `fs-disk-1k-read` | 0.1 | 0.083 (0.060) | 0.101 (0.081) | 0.080 (0.063) | 0.069 (0.053) |
| `fs-disk-1k-stat` | 0.1 | 0.030 (0.022) | 0.040 (0.022) | 0.023 (0.022) | 0.019 (0.019) |
| `fs-disk-1k-remove` | 1 | 0.510 (0.421) | 0.626 (0.389) | 0.460 (0.345) | 0.385 (0.286) |

The last column ran with Section 9's build (no strict alignment outside
the boot path, lazy wasmi modules) under a load average of 5 to 6; the
suite's other rows there match Section 9.3. The cipher settings in
`Cargo.toml` still apply under its build flags: the profile shows the
cipher inlined, as at `opt-level` 3.

So, by the fastest samples: encrypted Styx 2.1 times faster in the
kernel (3.9 with `cat`), 165 MB/s; purgfs reads 2 times (9.5 with
`cat`), 560 MB/s; the clear mount 715 MB/s; creates and removes about
20% faster, within the noise of the medians. The rest of the suite did
not change.

Memory (`memstat`, the kernel's test mode under KVM): text 1,740 KiB to
1,784 KiB (the second cipher version and the crypto at 3); the heap in
use at the end 1.50 MB to 1.89 MB, which is the larger bounce buffers
(two disks in that mode, 4 x 48 KiB more each) and one read-ahead
buffer; the peaks are unchanged.

Where the time goes now:

- `cat-styx-sealed`: ChaCha20 32%, Poly1305 28% (portable code; there
  is no NEON Poly1305 in RustCrypto), `memcpy` 19%.
- `cat-styx`: `memcpy` 44%, zeroed allocations 7%. The copies left are
  export's read into a buffer and its `Rread` encoding, the pipe in and
  out, and `devmnt` copying the reply into the caller's buffer. Reading
  straight into the caller's buffer needs the mount driver to hand the
  reader the buffer of the request it answers: that is protocol work in
  `devmnt.rs`, left for its owner.
- `cat-disk`: 75% idle, waiting for QEMU to do 1,600 reads of 64 KiB.
  More read-ahead in flight at once (several requests, or larger ones)
  would help further.
- `fs-disk-1k`: still the commit's flushes and journal write in a row,
  which the crash rules require.

## 11. The kernel heap on several CPUs

The kernel's heap was one `linked_list_allocator` heap per stretch of
RAM: first fit, under one spin lock, with IRQs masked. Under the stress
mode its free list grows to thousands of holes, and every allocation and
free walks it under the lock. On four CPUs most of the time went to
walking it or waiting for it, and four CPUs completed fewer stress runs
than one (#193). This section replaces it, in
`crates/purg-aarch64/src/kalloc.rs` (the allocator, plain code tested on
the host) and `heap.rs` (the bare-metal wrapper).

### 11.1 Setup

All runs are from 2026-11-12 on the host of Section 1, on `next` at
`7fd13aa` with #192's stress-mode reporting (`stress-hangs`) on top, so
that a run also counts progs slow to die. The stress mode ran all 83
jobs (the default: 8 workers, every corpus script that does not share
global state, and the guests) for 300 s, with `-m 512M`, from saved
images, one boot at a time. The QEMU process was pinned to the four fast
cores (`taskset -c 4-7`), and everything else this work built or ran
(builds, the benchmarks) to the four slow ones. Other agents were
building on the host at the same time: the load average was 2 to 8.

Time in the heap came from a throwaway build (not committed) that read
the virtual counter (24 MHz under KVM) around every `alloc` and
`dealloc`, inside the IRQ mask, and kept per-CPU totals, the longest
call, and how many calls took more than 10 µs, 100 µs and 1 ms; those
runs were 120 s. The longest calls under KVM include the host
descheduling a vCPU in the middle of one, so the counts of slow calls are
the better measure; they varied by up to ten times between identical
runs.

### 11.2 Design

Three layers; every critical section is short and bounded, and no lock is
held across a walk of a list.

- **Size classes.** A block of at most 16 KiB, aligned to at most 64, is
  rounded up to one of 36 classes: 16 to 128 bytes in steps of 16, then
  four to each doubling, so at most a quarter is wasted. Larger or more
  aligned blocks go to the large heap.
- **A cache per CPU.** Each CPU keeps a free list per class, holding up
  to 32 KiB of the class (at least 4 objects, at most 32). `alloc` and
  `dealloc` touch only the CPU's own list, under its own lock, which
  only another CPU's drain contends. An empty list takes half its
  limit from the class's depot; an overfull one gives half back.
- **Depots and slabs.** A class's depot is a list of slabs (16 to
  128 KiB, holding at least seven objects) with free objects, under a
  ticket lock of its own. A batch moves at most 32 objects under one hold.
  A slab whose objects are all free goes back to the large heap; each
  depot keeps one empty slab.
- **The large heap** is one TLSF heap (`rlsf` 0.2, MIT or Apache-2.0,
  `no_std`, no allocation of its own) over every stretch of RAM, under one
  ticket lock: allocation and free take constant time. It serves blocks
  over 16 KiB and the slabs. A region added at its top later (the
  kernel's heap grows past the boot map) becomes another TLSF pool.
- **The slab map.** `dealloc` gets the `Layout`, so the class is known,
  but not the slab. Slabs are not aligned to their size: TLSF keeps an
  aligned block's padding, which would waste up to half of each slab.
  Each stretch of RAM starts instead with a map, four bytes for each
  16 KiB, naming the slab that holds the last byte of that granule. An
  object's slab is its granule's entry if that slab starts at or before
  the object, otherwise the entry before it (a slab is at least a
  granule long, so it holds the last byte of the granule before the one
  it ends in). A slab sets at most nine entries when it is made and
  clears them when it is released.

IRQs stay masked for each operation, as before: an interrupt handler
allocates too, and masking pins the thread to the CPU whose cache it
uses (`cpu::id()`). `Heap::init` sets the boot CPU's index to 0, which
the scheduler used to do only later. Moving a large block in `realloc`
copies with IRQs enabled; a block that stays in its class stays where it
is. Reports (`Heap::stats`, `Heap::reclaim`) mask IRQs for one step at a
time: one class of one CPU, one slab, or one try of the largest-block
search.

**Accounting.** `used`, `live` and `allocs` count what callers asked for,
per CPU (a block freed on another CPU than it came from makes that CPU's
count go negative; the sum is right). Cached objects and slabs are free,
so the leak checks of the test and stress modes see only what callers
hold, with nothing to drain first. `HeapStats` gains `cached` (free
objects in the CPUs' caches) and `slabs` (the slabs' bytes); `memstat`
prints both and first gives back what the CPUs cache, so that `slabs`
and `largest` are what the tests left. The peak is sampled: whenever a
CPU's count has grown by 64 KiB, and on every visit to a depot or the
large heap, so it can miss at most 64 KiB a CPU. An allocation that finds
no room drains every CPU's cache and the kept slabs and tries again, so
the caches never cause an out-of-memory.

**Checks.** Host unit tests run random mixes of sizes and alignments on
four threads, each a CPU, with blocks freed on other CPUs than they came
from, and check contents, alignment, the counts, that everything goes
back, exhaustion (null, after the other CPUs' caches are drained),
`realloc`, two regions, a region extended at its top, and the worst case
for slabs below. `crates/purg-aarch64/tests/loom.rs` has three loom
models (CPUs never share an object; blocks freed on another CPU go home;
a drain racing a CPU's frees loses nothing), which CI's loom job runs.

Other designs considered: `talc` and `linked_list_allocator` are both
one lock over one heap, so they need the same per-CPU front; first fit
under the front still walked its list for the large blocks and slabs (the
middle column of 11.3 spent 78% of its heap time there on four CPUs);
size-aligned slabs need either a first-fit heap (whose alignment holes
the slabs then fragment) or waste in TLSF.

### 11.3 Results

Stress runs completed in 300 s (`--stress 300`). `ff + slabs` is the
same front over the old first-fit heap, the step before TLSF.

| | before | ff + slabs | after |
|---|---|---|---|
| KVM, 1 CPU | 326 (1 failed, 1 prog slow to die) | 4,112 | 4,285 |
| KVM, 4 CPUs | 254 | 6,063 | 6,290 |
| TCG, 1 CPU | 40 (1 failed, 2 slow to die) | 323 | 325 |
| TCG, 4 CPUs | 33 (2 failed, 3 slow to die) | 980 | 967 |

Before, a single CPU managed fewer runs each minute as the heap
fragmented (115 in the first minute, 42 in the fifth); after, the rate
is steady. Four CPUs now complete 1.5 times the runs of one under KVM
and 3.0 times under TCG. The progs slow to die (#192: 5 to 30 s to go,
unwinding their memory through the heap) are gone: none in any run
after. The failed runs before were `mcpjobs.sh`, `rawdbfs.sh` and
`mcpd.sh` (twice), whose output differed under the load (the flakes #193
mentions). None failed in the runs of the final build; one profiling
run of the first-fit step had `styxtools.sh` differ. A 30-minute run on
four CPUs (`--kvm --smp 4 --stress 1800`) completed 36,099 runs at a
steady 1,200 a minute and left nothing behind; 8 failed, every one shown
`styxtools.sh` printing `unmount: n: not mounted`, a race in the script
(`styxchat -s {mount ...}` can exit before its `mount` is done) that the
old heap was too slow to reach.

Time in the heap (120 s runs, the profiling build):

| | runs | heap time per CPU | calls over 100 µs | over 1 ms |
|---|---|---|---|---|
| before, 1 CPU | 191 | 116 s | 80,072 | 14 |
| before, 4 CPUs | 126 | 132 s | 1,576,853 | 3,008 |
| ff + slabs, 4 CPUs | 2,384 | 12.6 s (78% in the first-fit heap) | 5,988 | 5,424 |
| after, 1 CPU | 1,777 | 5.5 s | 18 | 5 |
| after, 4 CPUs | 2,390 | 2.8 s | 46 | 18 |

(A heap time over 120 s is the boot and the reap after the run.) Before,
an average call took 7 µs on one CPU and 38 µs on four, and the heap was
nearly all of the CPUs' time; after, 41 ns and 57 ns, and 2 to 5% of it. The large heap took
432 ms of the 11.4 s on four CPUs, for 2.1 million calls. On KVM four
CPUs are now limited by something other than the heap; under TCG they
scale further.

The benchmark suite (`cargo xtask bench --kvm --rounds 3 --boots 2`, one
CPU, on the slow cores, load average 4 to 5) did not change much:
`fs-root-1k-create` 0.073 to 0.061 s and `cat-styx` 0.252 to 0.240 s
faster, every other row within 3% of `next` either way (`sha1-pipe-16m`
1.427 to 1.456 s).
A fresh boot's heap is not fragmented, so first fit was cheap there.

**Memory.** At the end of the test mode (`KERNEL_MEMSTAT=1 cargo xtask
kernel --kvm --mem 80M`): in use 2,177,164 bytes before and after; the
peak 28.5 MB and 28.8 MB; slabs 1.5 MB holding what is left; the largest
free block 17.8 MB and 16.8 MB (TLSF rounds a request up to its list,
so up to a sixteenth of a free block cannot be had in one piece). The
allocator's own data is about 10 KiB of `.bss` and the slab map 0.025%
of the heap. After 300 s of stress on four CPUs the heap held 0.89 MB in
use (0.82 MB before) and 3.7 MB of slabs, and its largest free block was
240 MiB of 467 MiB (373 MiB before; 256 MiB after the one-CPU run). TLSF
does not keep the free memory together at the top as address-ordered
first fit does, so a long-lived block can end up in the middle. The smallest
`-m` for the full test mode under KVM went from 76M (72M fails) to 80M
(76M fails): the slabs and TLSF fragment a small heap a little more than
first fit did. (Section 5 of `docs/footprint.md` measured 52M before the
root grew.)

The worst case for slabs is a unit test: 40,000 blocks of up to 2 KiB,
keeping every fiftieth. The 800 kept blocks (820 KB) pin 10.8 MB of
slabs, and the largest free block is 18.9 MB of 64 MiB, where first fit
would keep the 23 MiB it never touched.

### 11.4 What remains

- Slabs pinned by a few long-lived objects are not reused by other
  classes. Allocating from the fullest slab first, or moving the
  long-lived kernel tables to classes of their own, would help a small
  heap.
- Linear memories and thread stacks are large blocks in TLSF; pools for
  them (`docs/footprint.md`, item 12) would cut fragmentation further.
- TLSF takes the first block of a list, wherever it is, so after a long
  run the free memory is in a few large pieces rather than one. Keeping
  each list in address order, or taking long-lived blocks from the low
  end, would keep it together.
- What limits four CPUs under KVM now is outside the heap; `kprof`
  samples only the CPU that starts it, so finding it needs a profiler
  for every CPU.

## 12. Follow-up: AOT as a real option

Measured on 2026-11-12, same host, on `next` at `9292ad9` with these
changes. Other agents were building at times: the load average was 1.4
to 3.2 during the benchmark runs.

### 12.1 What changed

The prototype of Section 5.2 had seven follow-ups. All are done:

- **The whole test mode passes with every module compiled.** `cargo
  xtask kernel --aot` builds the kernel with the `aot` feature and boots
  it on an initrd with a signed image beside every module (191 modules,
  20.5 MB of wasm, 101.5 MB of images). Under KVM and TCG, on one and on
  four CPUs, every check passes, the leak check and `mash`'s deepest
  recursion included. A run under KVM ran 4,630 progs compiled and 37 on
  wasmi: traced progs, and modules the corpus makes at run time, which
  have no image.
- **Stacks.** Guest code now runs on a stack of its own
  (`purg_aarch64::sched::on_stack`), 576 KiB from the heap for each
  running compiled prog: wasmtime's default 512 KiB of wasm stack, as on
  the emu, and 64 KiB under it for the host calls (the deepest a prog's
  thread went is 22 KiB). The thread's own 256 KiB stack no longer
  bounds recursion. The heap block has no guard page, but wasmtime
  checks the stack in each function's prologue against its 512 KiB.
- **Freeing modules.** Loaded images are shared in generations, as
  wasmi's modules are (Section 9.2): a generation keeps each image its
  progs load, up to 8 MiB of images, and goes with the last prog that
  started in it. At the end of the test mode the code region is empty.
  Reactors are forgotten when their prog exits, on the emu's wasmtime
  too, which kept them for ever.
- **Thread-local storage** is a word in the scheduler's thread
  (`purg_aarch64::sched::tls`), found through `TPIDR_EL1`, which follows
  the thread from CPU to CPU, so nothing is locked or switched to reach
  it. The prototype looked up a map by pid under a lock on every entry
  to wasm.
- **One set of host functions.** `purg_vm_aot::host` has the imports,
  `load` and `call`, and the memory limits, for both wasmtime backends:
  the emu's and the kernel's link the same `wasmtime` crate. A backend
  supplies how a reactor's module is made and what runs before a host
  call (the emu's debugger). The prototype's copy had no memory limits.
- **Images at run time.** Beside the linked set (unchanged), the engine
  reads `<path>.cwasm` beside a module, checks its signature against
  `/lib/keys/aot` in a copy of the boot namespace, and copies it into
  the kernel's code region (`docs/signing.md` has the format and the
  rules). Nothing unverified runs: an image that is missing, does not
  verify or does not load leaves the module on wasmi.
- **Determinism and CI.** `cargo xtask aot` writes the signed images
  and their digests; the test matrix's three runners each do it and the
  `reproducible` job compares the lists, as for the modules. A new job,
  `kernel-aot`, runs `cargo xtask kernel --aot` under TCG; here it takes
  146 s, against 127 s for the same machine on wasmi. Two builds here
  gave identical images.
- **Selection.** `engine=wasmi|aot|auto` at boot: `auto`, the default
  in a kernel with the feature, runs a module compiled where it has a
  verified image; `aot` refuses a module without one (a traced prog
  still runs on wasmi); `wasmi` never uses an image.

### 12.2 The code region

The kernel's maps were W^X with nothing written at run time executable.
Images loaded at run time need memory that is written and then run, so
`purg_aarch64::code` makes it without ever having one mapping both
writable and executable:

- Pages come from 2 MiB chunks of the heap. While the region owns a
  chunk, its block in the linear map is read-only, so the code cannot be
  changed through the linear map either. The runtime page tables no
  longer use 1 GiB blocks, so that one 2 MiB block can change alone,
  and the chunk is the region's, so no other access sees the change.
- Each image gets a range of a 1 GiB window of the high half that only
  the region maps. Its pages are mapped there read-write and never
  executable, filled, cleaned to the point of unification (`dc cvau`),
  remapped read-only and executable with `tlbi vaae1is` for every CPU,
  and the instruction cache is invalidated over the range (`ic ivau`,
  broadcast) before an `isb`. A context switch now ends with an `isb`,
  so a thread that moves to another CPU fetches nothing stale.
- Dropping the image unmaps its pages, invalidates their TLB entries and
  returns them; a chunk with no page in use goes back to the heap,
  writable again. The window's page tables stay (a few pages).

wasmtime's `CustomCodeMemory` accepts code only in the linked set or in
the window.

### 12.3 Results

Seconds under KVM, three rounds in each of two boots, median and
fastest in brackets. `wasmi` is the kernel without the feature;
`files` is `bench --aot-files native` (every module of the root has an
image file, loaded at run time); `linked` is `bench --aot native` (the
nine modules of the suite linked into the kernel, as in Section 5.2).

| benchmark | Target | wasmi | AOT, image files | AOT, linked |
|---|---|---|---|---|
| cat-root | 0.5 | 0.011 (0.011) | 0.010 (0.010) | 0.010 (0.010) |
| cat-styx | 1 | 0.098 (0.096) | 0.096 (0.096) | 0.103 (0.100) |
| cat-styx-sealed | 2 | 0.550 (0.548) | 0.541 (0.540) | 0.555 (0.552) |
| cat-disk | 2 | 0.153 (0.115) | 0.167 (0.116) | 0.157 (0.077) |
| sha1-pipe-16m | 1 | 0.753 (0.751) | 0.102 (0.102) | 0.102 (0.102) |
| sha1-file-16m | 1 | 0.725 (0.725) | 0.092 (0.092) | 0.092 (0.092) |
| sh-builtin-10k | 0.5 | 0.352 (0.349) | 0.090 (0.084) | 0.089 (0.082) |
| sh-spawn-1k | | 0.327 (0.326) | 0.266 (0.266) | 0.268 (0.265) |
| spawn-1k-total | 1 | 0.263 (0.261) | 0.250 (0.248) | 0.252 (0.249) |
| fs-root-1k-create | 0.1 | 0.030 (0.029) | 0.031 (0.030) | 0.031 (0.030) |
| fs-root-1k-read | 0.1 | 0.013 (0.013) | 0.016 (0.015) | 0.016 (0.016) |
| fs-root-1k-stat | 0.1 | 0.018 (0.018) | 0.019 (0.018) | 0.020 (0.019) |
| fs-root-1k-remove | 0.1 | 0.013 (0.013) | 0.015 (0.015) | 0.012 (0.012) |
| fs-memfs-1k-create | 0.1 | 0.049 (0.049) | 0.035 (0.034) | 0.031 (0.030) |
| fs-memfs-1k-read | 0.1 | 0.032 (0.031) | 0.022 (0.021) | 0.020 (0.019) |
| fs-memfs-1k-stat | 0.1 | 0.037 (0.037) | 0.022 (0.021) | 0.020 (0.019) |
| fs-memfs-1k-remove | 0.1 | 0.024 (0.024) | 0.016 (0.015) | 0.014 (0.014) |
| fs-disk-1k-create | 1 | 0.495 (0.333) | 0.461 (0.367) | 0.509 (0.241) |
| fs-disk-1k-read | 0.1 | 0.063 (0.045) | 0.064 (0.051) | 0.064 (0.042) |
| fs-disk-1k-stat | 0.1 | 0.019 (0.019) | 0.018 (0.018) | 0.017 (0.017) |
| fs-disk-1k-remove | 1 | 0.327 (0.257) | 0.380 (0.204) | 0.322 (0.281) |

- Code that computes is 7 to 8 times faster than on wasmi (the 16 MiB
  hash 0.092 s against 0.725 s) and `sh`'s loop 4 times, from image
  files as from the linked set. The I/O rows are the kernel's, not the
  guest's, and do not change. The disk rows vary by up to 80% between
  runs, as before.
- A spawn costs the same as on wasmi, 0.25 ms: both now keep modules in
  generations, so a command run again is neither read for its image nor
  verified again. The first spawn of a command in a generation reads its
  image file, hashes it (SHA-256 over 0.5 MiB for a typical command),
  checks one Ed25519 signature and copies it; that cost was not timed
  on its own.
- The first run of the benchmark found that `bench.sh` binds over
  `/lib/keys` in the boot namespace itself, which hid `/lib/keys/aot`
  from every later load: the engine now keeps its own copy of the boot
  namespace, as a module policy does.

Sizes and memory:

- Kernel text: 1,524 KiB without the feature, 1,955 KiB with it and no
  linked images (+430 KiB: wasmtime's runtime, Pulley's interpreter,
  `purg-vm-aot`, and the code region).
- Images: 101.5 MB for the 191 modules of the root (20.5 MB of wasm),
  5 times the modules; the initrd with them is 123 MB, against 21 MB,
  so `xtask kernel --aot` boots with 1 GiB of RAM.
- The test mode under KVM with `memstat`: the heap at the end 2.20 MB on
  wasmi and 2.58 MB with AOT (the difference is in the RAM tree and the
  window's tables); the largest peak of a single test 52 MB against 67
  MB, the 64 MiB block of the heap self-test aside, from the images held
  by a generation and the 576 KiB stacks; 4.0 M allocations against 2.4
  M.

### 12.4 Recommendation

AOT is now a working option, and a system that needs speed for code
that computes should use it: build with `--features aot`, ship signed
images beside the modules that matter, and list the signer in
`/lib/keys/aot`. It should not be the default yet:

- It costs 430 KiB of text and five times the modules in images, which
  a small system does not want (`docs/footprint.md`).
- The trust model is new: one more key list, whose keys can run native
  code in the kernel, and a compiler (Cranelift) inside the trusted
  base. Signing images in a release process and auditing that list
  should come first.
- It has had one day of testing. It passes every check here, under
  KVM and TCG, on one and four CPUs, but not yet the one-hour stress, a
  real Mac, or real hardware with a different cache line or page size.

What remains:

- The one-hour `-smp 4` stress under `--aot`, and an sh corpus test
  that a tampered or unsigned image beside a module is refused in the
  kernel (the unit tests in `purg_vm_aot::image` cover the check
  itself).
- Images for reactors are not cached: each `load` reads and verifies
  its image again.
- The cost of the first spawn of a command, as above, and the SHA-256
  instructions of Section 9.3, which would cut it.
- A boot-time way to name the image signer other than a file in the
  root (a key compiled in, or a boot argument), for systems whose root
  is not trusted like the kernel.
