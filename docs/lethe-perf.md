# Lethe and Styx: a performance study

This study compares Lethe (`docs/lethe.md`) with Styx (9P2000, the `#M`
mount driver against `export`) for read-only file system work, on the
hosted emu and on the kernel. It measures time, round trips and bytes on
the wire, explains where each protocol wins, and ranks changes for each.
It is part of issue #99. Neither protocol was redesigned for it; two
implementation faults found on the way were fixed in their own commits
(Section 3).

## 1. Setup

`cargo xtask lethe-perf` runs everything (Section 8). It makes a tree in
`root/tmp/lethe-perf/srv`:

| name | what |
|---|---|
| `one` | 64 bytes |
| `big1m`, `big100m` | 1 MiB and 100 MiB |
| `small/f00000` .. `f09999` | 10,000 files of 1 KiB; also the large directory |
| `deep/d01/.../d16/leaf` | a name 17 elements below `deep` |
| `tree/aNN/bNN/fNN` | 10 x 10 directories of 20 files (2,110 entries), for `du` |

The same tree is served read-only three ways:

| configuration | server | client |
|---|---|---|
| `styx-v3` | `export -e -r` in a second emu, behind `styxlisten` | `mount`: version 3 proof with the test key, then ChaCha20-Poly1305 records (ring on the emu) |
| `styx-A` | `export -A -r` | `mount -A`: no proof, in the clear |
| `lethe` | `lethed -k lib/test/auth.key.pub` | `lethe`: a ticket, every request sealed with XChaCha20-Poly1305 (RustCrypto) |

The servers trust the public test key (`root/lib/test/keys` bound on
`/lib/keys` for the export; `-k` for `lethed`), as the tests do. A
fourth configuration, Lethe in the clear (`lethed -A`, `lethe -A`), is
being added on branch `lethe-clear` and was not on `next` when these
numbers were taken. The harness runs it on its own when the `lethed` it
builds accepts `-A`; after that branch lands, add the column with:

```sh
cargo xtask lethe-perf --parts direct,counts,sweep
```

The client is `/bin/test/fsperf.wasm` (`appl/test/fsperf`), run in a
client emu (wasmtime, and wasmi for one run) or on the kernel. Each
case runs twice on one attach, `cold` (first) then `warm`:

| case | what one operation is |
|---|---|
| `lat` | open, read and close `one` (200 times) |
| `latread` | seek to 0 and read 64 bytes of an open `one`: one round trip (200 times) |
| `walk` | stat `deep/d01/.../d16/leaf` (200 times) |
| `stat` | stat `small/fNNNNN`, 10,000 names |
| `readdir` | list `small` (10,000 entries) |
| `lsl` | stat `small`, list it, stat each entry by name (`ls -l` style) |
| `du` | list `tree` recursively |
| `small` | open, read and close each of the 10,000 small files |
| `read1m`, `read100m` | read the file in 64 KiB reads |

Between client and server sits a host relay (`xtask/src/letheperf.rs`)
that counts frames and bytes each way by the `size[4]` prefix that Styx
messages, Styx records and Lethe frames all start with, and can delay
each direction by half a round trip without limiting throughput. The
guest reads the relay's totals from a control port before and after each
case. Timing runs at no added delay go straight to the server; the
counting and latency runs go through the relay.

The host is an Apple Silicon machine (8 CPUs) under Linux 6.14 (Asahi),
with other agents building on it: the load average was 7 to 17 during
the runs. Loopback times therefore spread widely (the tables give the
median and the range of 5 runs). The round trip and byte counts do not
depend on load, and the latency sweep is dominated by the injected delay,
so those are the solid results.

## 2. Round trips and bytes

One run through the counting relay, cold (warm is the same except where
noted). "RT/op" is client-to-server frames per operation, which is the
number of round trips, as both clients send one request at a time.

| case | styx-v3 RT/op | styx-A RT/op | lethe RT/op | styx-v3 bytes/op | styx-A bytes/op | lethe bytes/op |
|---|---:|---:|---:|---:|---:|---:|
| `lat` (open, read, close) | 5 | 5 | 3 | 430 | 230 | 818 |
| `latread` (one read) | 1 | 1 | 1 | 139 | 98 | 260 |
| `walk` (17 elements) | 37 | 37 | 19 | 2,695 | 1,215 | 6,404 |
| `stat` (2 elements) | 5 | 5 | 3 | 428 | 228 | 928 |
| `readdir`, 10,000 entries | 102 in all | 102 | 99 | 80 per entry | 79 | 82 |
| `lsl`, per entry | 5 | 5 | 3 | 508 | 307 | 1,010 |
| `du`, 2,110 entries | 975 in all | 975 | 432 | 109 per entry | 91 | 136 |
| `small` (open, read, close 1 KiB) | 7 | 7 | 4 | 1,537 | 1,257 | 2,176 |
| `read1m` | 132 | 132 | 130 | +0.9% | +0.4% | +3.2% |
| `read100m` | 12,804 | 12,804 | 12,802 | +0.9% | +0.4% | +3.2% |

(For the reads, bytes are given as the overhead over the file's size.)

Where the round trips go:

- **Styx walks one element per `Twalk` and clunks every fid on its own.**
  `#M`'s `walk` gets one name from the namespace and sends a one-name
  `Twalk`; the fid of each intermediate element is clunked, and each
  `Tclunk` waits for its `Rclunk`. A stat of an `n`-element name is
  `2n + 3` round trips (17 elements: 37). The 9P batch walk (up to 16
  names in one `Twalk`) is never used.
- **Lethe stats every element.** `#λ` has no state on the server to walk,
  so each element's walk is a `Tstat` of the joined path, and the final
  `stat` asks once more: `n + 2` round trips (17 elements: 19). That is
  half of Styx's count, but not the one request a whole-path stat could be.
- **Opening is free on Lethe**; `Topen` and `Tclunk` are round trips on
  Styx. Hence 3 against 5 for `lat` and 4 against 7 for a small file.
- **Reads are the same.** Both move 8 KiB per round trip: Styx's msize is
  8216 (iounit 8192) and `#λ` asks for one 8 KiB block per request (Lethe
  allows 32 KiB). A read that reaches end of file costs one more round trip
  on both.
- **Directories are the same**: 8 KiB pages, about 100 for 10,000 entries.
  `du` costs Lethe fewer round trips because opening a directory and
  walking into a subdirectory is one `Tstat`, against walk, open and clunk.

Bytes per request:

| | Styx clear | Styx v3 | Lethe |
|---|---:|---:|---:|
| request to read 8 KiB | 23 | 43 | 192 |
| reply overhead on 8 KiB of data | 11 | 31 | 69 |
| request to stat a path | 36 | 56 | 180 |

Lethe pays about 150 bytes a request: the ticket (about 100 bytes: key
id, nonce, the sealed master key, times, rights, user and prefix), a
24-byte nonce, the client's time and a 16-byte tag. The reply carries a
nonce, a tag and the qid. On reads that is 3% against Styx v3's 0.9%;
for small requests Lethe sends two to four times the bytes, but at these
sizes the round trip, not the bytes, is the cost.

The warm `read1m` on Lethe sent half the data (492 KiB of 1 MiB): the
block cache holds 1 MiB and is emptied when it fills, so only part of a
1 MiB file stays. Every warm read still costs its round trip (a
conditional read answered "unchanged").

## 3. Two faults fixed first

Both were found by this study and fixed in their own commits, with these
numbers taken before and after:

1. **The emu's TCP read slept 5 ms when no data was ready.** Its sockets
   were non-blocking and polled with a 5 ms sleep, so on the hosted emu
   *every* reply over `#I` or `#λ` waited for the sleep to end: a round
   trip on loopback cost about 5 ms, for Styx and Lethe alike. The sockets
   now block with a 5 ms timeout (so a kill is still noticed), and data
   wakes the reader at once.

   | over loopback, before -> after | styx-v3 | styx-A | lethe |
   |---|---:|---:|---:|
   | one read (`latread`) | 5.07 ms -> 49 us | 5.08 ms -> 40 us | 5.16 ms -> 82 us |
   | open, read, close (`lat`) | 25.3 ms -> 274 us | 25.3 ms -> 246 us | 15.4 ms -> 95 us |
   | read 1 MiB | 671 ms -> 8.5 ms | 669 ms -> 6.6 ms | 683 ms -> 25 ms |

2. **`lethed` stat'ed the whole directory for every readdir page.** A
   stateless server lists the directory again for each page, and `lethed`
   resolved and stat'ed every entry before skipping to the cookie, so a
   listing cost entries squared over the page size in stats. It now reads
   the names, sorts them, and stats only the entries after the cookie
   that one page can hold. The wire is unchanged. Listing 10,000 entries
   (99 pages) went from 9.0 s to 0.7 s.

The kernel's network stack is interrupt driven and had no such sleep.

## 4. Time on loopback

wasmtime, 5 runs, cold, median and range in ms, after the fixes, direct
(no relay):

| case | styx-v3 | styx-A | lethe |
|---|---|---|---|
| `lat`, per op | 101 us | 201 us | 256 us |
| `latread`, per op | 18 us | 34 us | 79 us |
| `walk` (200) | 559 (183-905) | 655 (432-1455) | 1585 (1435-2688) |
| `stat` (10,000) | 3244 (1392-7653) | 3276 (2701-9799) | 3228 (2152-3575) |
| `readdir` (10,000) | 65 (63-78) | 62 (41-113) | 783 (661-951) |
| `lsl` (10,000) | 3227 (2975-7589) | 3097 (2722-7024) | 4194 (3639-4585) |
| `du` (2,110) | 76 (66-131) | 81 (60-332) | 133 (43-178) |
| `small` (10,000) | 5886 (4270-13027) | 4483 (3518-8787) | 5023 (4074-5691) |
| `read1m` | 14.9 (9.0-21.3), 67 MiB/s | 5.6 (2.5-23.5), 179 MiB/s | 23.0 (17.7-42.8), 43 MiB/s |
| `read100m` | 1122 (1104-2945), 89 MiB/s | 769 (239-1685), 130 MiB/s | 2252 (1300-2656), 44 MiB/s |

One wasmi run gave the same picture (`read100m`: 84, 147 and 44 MiB/s;
`stat` 411, 355 and 219 us an operation). The guest does little besides
system calls, so the VM hardly matters here; the devices are native.

On loopback a round trip is tens of microseconds, so CPU per request
decides, and there Lethe is behind:

- **Lethe's bulk crypto is slow.** Run in one process with no network,
  a Lethe 8 KiB read costs about 68 us of CPU for client and server
  together (a stat about 6 us, a conditional read 8 us). The reply's
  8 KiB is sealed and opened with RustCrypto's XChaCha20-Poly1305, which
  on aarch64 uses portable code: `chacha20` 0.9 has a NEON backend only
  under `--cfg chacha20_force_neon`, and `poly1305` 0.8 none. Styx v3 on
  the emu uses ring's assembly (`purg-auth`'s `ring` feature, "about three
  times faster"). That is why Lethe reads at 44 MiB/s against 89 for
  Styx v3 and 130 in the clear. With `chacha20_force_neon` the 8 KiB
  figure dropped to about 43 us on the loaded host.
- **`lethed` does every request from the start:** it resolves and
  canonicalises the path (`canonicalize`, one `lstat` per element) and
  opens the file for every block. Styx's export holds the open file in
  the fid.
- **Each readdir page lists the directory again** (names and a sort,
  after fix 2): about 7 ms a page for 10,000 entries, so `readdir` is
  12 times Styx's 65 ms, which reads the directory once into the open fid.
- **Where Lethe has fewer round trips it keeps up or wins:** `stat`,
  `small` and `lsl` are close, with Lethe's 3 or 4 round trips against
  Styx's 5 or 7 offsetting its costlier requests.

## 5. The latency sweep

The relay adds a round trip of 1, 10 or 50 ms; 20 files and 20
repetitions; cold; time in ms with round trips in brackets.

| case | RTT | styx-v3 | styx-A | lethe |
|---|---:|---:|---:|---:|
| `lat` (20) | 1 ms | 132 (100) | 135 (100) | 73 (60) |
| | 10 ms | 1040 (100) | 1036 (100) | 672 (60) |
| | 50 ms | 5036 (100) | 5059 (100) | 3024 (60) |
| `latread` (20) | 50 ms | 1160 (23) | 1165 (23) | 1059 (21) |
| `walk` (20) | 1 ms | 1005 (740) | 1037 (740) | 587 (380) |
| | 10 ms | 7726 (740) | 7703 (740) | 3997 (380) |
| | 50 ms | 37372 (740) | 37393 (740) | 19310 (380) |
| `stat` (20) | 50 ms | 5058 (100) | 5065 (100) | 3028 (60) |
| `readdir` (10,000) | 1 ms | 240 (102) | 176 (102) | 862 (99) |
| | 10 ms | 1102 (102) | 1154 (102) | 1354 (99) |
| | 50 ms | 5166 (102) | 5218 (102) | 5772 (99) |
| `lsl` (list, stat 20) | 50 ms | 10445 (205) | 10454 (205) | 8863 (161) |
| `small` (20) | 1 ms | 171 (140) | 197 (140) | 93 (80) |
| | 10 ms | 1455 (140) | 1498 (140) | 846 (80) |
| | 50 ms | 7063 (140) | 7105 (140) | 4061 (80) |
| `read1m` | 1 ms | 168 (132) | 172 (132) | 172 (130) |
| | 10 ms | 1372 (132) | 1386 (132) | 1402 (130) |
| | 50 ms | 6666 (132) | 6653 (132) | 6575 (130) |

From 1 ms up, time is round trips times the RTT, within a few percent,
for both protocols. Encryption costs nothing visible: `styx-v3` and
`styx-A` are the same. So:

- Lethe is 1.7 to 1.9 times faster for anything that walks or opens
  (`lat`, `walk`, `stat`, `small`), exactly its round-trip saving.
- Reads are the same, 8 KiB per round trip on both: 1 MiB takes 6.6 s at
  50 ms, 160 KiB/s. Warm Lethe reads are no faster at distance (every
  block is revalidated with a round trip).
- `readdir` is the same at distance; Lethe's per-page server cost shows
  only at 1 ms.

## 6. A server restart mid-read

The client reads `big100m` through a 1 ms relay; 2 s in, the harness
kills the server, waits a second and starts it again on the same port.

- **Styx (both):** the read fails at once with `i/o on hungup channel`,
  and so does everything else on the mount (`stat`, `ls`). The mount is
  dead: its fids were the server's state. Mounting again after the server
  is back works, and a program would have to reopen its files.
- **Lethe, fresh ticket key (the default):** the read completes, all
  100 MiB, with no error. The longest single read took 1.58 s: the time
  the server was down plus the redial backoff (50 ms doubling). The new
  server answers the old ticket with `unknown ticket key`; the device
  gets a new ticket and resends. Three connections went through the relay.
- **Lethe with `-t` (the same ticket key across the restart):** the same,
  without the new ticket (1.57 s longest read).

## 7. Kernel under KVM

`purg-kernel` (the `kernel` profile, wasmi, the initrd root) boots under
KVM with 1 GiB, a virtio-net card on QEMU user networking and the script
on a virtio-9p share; the harness types `sh /n/host/perf.sh` on the
console. The kernel reaches the same three servers on the host at
`10.0.2.2` through the counting relay. 3 runs, cold, median and range in
ms:

| case | styx-v3 | styx-A | lethe |
|---|---|---|---|
| `lat`, per op | 571 us | 959 us | 696 us |
| `latread`, per op | 107 us | 184 us | 217 us |
| `walk` (200) | 1863 (1069-2465) | 1550 (1254-1552) | 1947 (965-1956) |
| `stat` (10,000) | 11285 (6547-12246) | 10471 (10000-11354) | 7550 (7220-7894) |
| `readdir` (10,000) | 254 (145-310) | 272 (241-294) | 925 (899-931) |
| `lsl` (10,000) | 9723 (8485-12409) | 9878 (8533-10352) | 8098 (7262-8162) |
| `du` (2,110) | 241 (229-323) | 237 (183-258) | 208 (206-215) |
| `small` (10,000) | 14423 (13585-18034) | 13718 (13208-14333) | 9652 (9605-10573) |
| `read1m` | 64 (36-95), 16 MiB/s | 43 (41-49), 23 MiB/s | 62 (58-65), 16 MiB/s |
| `read100m` | 6773 (3419-7148), 15 MiB/s | 4047 (3919-4184), 25 MiB/s | 6197 (5540-6400), 16 MiB/s |

The round trips and bytes are the same as on the emu, to the frame
(Section 2): the client devices are the same code. What changes is the
round trip, about 100 to 200 us through QEMU's user networking and the
kernel's smoltcp stack, which is enough for Lethe's smaller round-trip
counts to win: `stat` 1.5 times faster, `small` 1.5 times, `lsl` and `du`
somewhat. Reads are level: in the kernel both use pure Rust ciphers
(no ring), so Lethe's crypto is no longer behind Styx v3's, and the
clear Styx reads fastest. `readdir` is still Lethe's weak case, from the
server's work per page.

## 8. Running it

```sh
cargo xtask lethe-perf                        # direct, wasmi, counts, sweep, restart
cargo xtask lethe-perf --parts kernel --reps 3   # the kernel under KVM
cargo xtask lethe-perf --reps 3 --parts direct,counts
```

`--parts` takes any of `direct` (wasmtime, `--reps` runs), `wasmi` (one
run), `counts` (one run through the counting relay), `sweep`, `restart`
and `kernel` (aarch64 hosts with `/dev/kvm`). The emu and `lethed` are
built in release mode. Raw `fsperf` output and `summary.md` go to
`target/lethe-perf/`. The tree is made once (about 110 MiB) and kept
while its stamp matches. Nothing here runs in CI.

## 9. Recommendations

Ranked by expected gain, largest first. The round-trip figures come from
Section 2 and hold at any distance; the gains are largest where the RTT
is large, as Section 5 shows.

### Lethe

1. **Walk a whole path in one request.** Let `#λ` stat the full path (or
   send a `Twalk`-like op that returns each element's qid) instead of one
   `Tstat` per element, and drop the second stat of the final element
   (the walk's result can answer the `stat` that follows it). A 17-element
   stat goes from 19 round trips to 1 or 2, a 2-element one from 3 to 1,
   a small file read from 4 to 2: about 2 to 10 times faster at distance
   for name-heavy work.
2. **An attribute cache with a short lifetime** (NFS's `acregmin`, a
   second or two), for walks and stats. With it a `make` or a `ls -l` of
   names already seen costs no round trips. Off by default for trees that
   change.
3. **Validate per file, not per block.** A warm read revalidates every
   8 KiB block with its own round trip. Checking the version once per open
   (or once per lifetime of the attribute cache) and serving the rest from
   the cache would make warm reads free; with an LRU cache larger than
   1 MiB (it is emptied when full today) warm reads of big files would
   stay warm.
4. **Larger reads and readahead.** The protocol already allows 32 KiB
   reads and replies in any order; `#λ` asks for 8 KiB at a time. 32 KiB
   reads cut read round trips by four; a few requests in flight on the
   pooled connections (readahead on sequential reads) bring a 50 ms link
   from 160 KiB/s toward its bandwidth.
5. **Faster crypto for bulk data.** Use ring's ChaCha20-Poly1305 on the
   emu for the bulk of each record, with the XChaCha20 subkey from
   HChaCha20, and build `chacha20` with NEON on aarch64 (the kernel too).
   That roughly halves to thirds the CPU per 8 KiB block, which bounds
   loopback throughput today (44 MiB/s).
6. **Cheaper server per request.** Cache the path resolution and open
   files in `lethed` for a short time (soft state, lost harmlessly on a
   restart), and make readdir pages larger (32 KiB) so a big directory is
   listed again fewer times.
7. **Smaller request overhead** (about 150 bytes, mostly the ticket)
   matters only for bulk small requests on slow links; last.

### Styx

1. **Batch walks.** Send all the elements of a name in one `Twalk` (up to
   16 names), as 9P intends, instead of one `Twalk` per element; this
   needs the namespace to hand `#M` the rest of the path. A 17-element stat
   drops from 37 round trips to about 4, a 2-element one from 5 to 3.
2. **Clunk without waiting.** Each intermediate fid and each close sends
   a `Tclunk` and waits for `Rclunk`. Sent and not awaited (the reply is
   only an acknowledgement), the walk above drops further, and a small
   file read goes from 7 round trips to 4 or 5.
3. **A larger msize.** `MAX_MSIZE` is 8 KiB plus the header; 64 KiB would
   cut read round trips by eight (1 MiB at 50 ms: 6.6 s to under 1 s).
   Records grow to match; nothing in the protocol prevents it.
4. **Pipelined reads (readahead).** Tags allow several `Tread`s in flight;
   `#M` sends one at a time per reader.

Batch walks and asynchronous clunks would bring Styx's name costs below
Lethe's present ones; Lethe's recommendations 1 to 3 would take it back
below Styx, and Lethe keeps the property Section 6 shows: a server
restart costs a delay, not a dead mount.
