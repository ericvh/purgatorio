# purgfs: the native disk file system

purgfs is the file system the native kernel keeps on a virtio-blk disk
(K3, plan decision D7). It is Plan 9's kfs with a journal: no inodes, a
file is its entry in its parent directory, and every change to the
metadata is one atomic transaction. The code is `crates/purg-fs`, a
`no_std` + `alloc` crate shared by the kernel, the `disk/` commands and
`xtask`.

## The device: `#K`

A mounted purgfs is a kernel device, `#K` (`purg_fs::dev::DevFs`, the
crate's `dev` feature). The kernel binds it on `/n/disk`; like any part of
the namespace it is served over Styx to whoever exports or imports it.

It is a device, not a kproc file server mounted with `#M`, because:

- the operations run in the calling prog's thread, so a prog killed while
  it waits for the disk stops at once, with no flush to send and no
  server thread to wake;
- there is no extra thread (whose stack would be 64 KiB) and no Styx
  encoding or copying between the prog and the file system;
- Inferno's native kernels serve their small disk file systems as devices
  too, and `#K` is unused in Inferno and here.

Inferno's own kfs is a user-level Styx server; a wasm guest could serve
purgfs the same way over `#S`, but every block would then go through the
interpreter.

The raw disks are `#S` (Inferno's storage device letter), one directory
per disk, `sdV0`, `sdV1`, ..., each with `ctl` (geometry and serial) and
`data` (the bytes, at any offset).

## Layout

The disk is an array of blocks of one size, 512 to 65536 bytes, a power of
two and a multiple of the device's block size (4096 by default). Block
numbers are 32 bits, so a file system has at most 2^32 blocks (16 TiB at
4 KiB). Every field is little-endian.

| Blocks | What |
|---|---|
| 0 | The superblock (bytes 0 to 255) and the root directory's entry (256 to 511) |
| 1 to J | The journal: a header block, then up to J-1 block images (J is 32 by default, at least 8) |
| J+1 to J+M | The free map: one bit per block, set when the block is in use. M = blocks / (8 x block size), rounded up |
| J+M+1 on | Data: file contents, directory blocks and indirect blocks |

### Superblock (block 0)

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | Magic, `purgfs\n1` |
| 8 | 4 | Format version, 1 |
| 12 | 4 | Block size |
| 16 | 8 | Blocks in the file system |
| 24 | 8 | First journal block (1) |
| 32 | 4 | Journal blocks |
| 36 | 4 | Free map blocks |
| 40 | 8 | First free map block |
| 48 | 8 | First data block |
| 56 | 8 | The next qid path |
| 64 | 188 | Zero |
| 252 | 4 | Checksum of bytes 0 to 251 and the root entry (bytes 256 to 511): 64-bit FNV-1a folded to 32 bits |
| 256 | 256 | The root directory's entry |

The mount checks the magic, the version, the checksum, that the regions
follow each other as the block size and count say, and that the file
system fits the device.

### Directory entries

A directory's data blocks hold 256-byte entries, as many as fit in a
block. A slot that is all zeros is free. A directory's length is its
blocks times the block size; a stat reports 0, as Inferno does.

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | 1 for a used slot |
| 1 | 1 | Name length (1 to 64) |
| 2, 3, 4 | 1 each | Lengths of the owner, group and last modifier (1 to 24) |
| 5 | 3 | Zero |
| 8 | 8 | Qid path, unique for the life of the file system |
| 16 | 4 | Qid version, bumped by every change to the contents |
| 20 | 4 | Mode: the permission bits, `DMDIR`, `DMAPPEND`, `DMEXCL` |
| 24 | 4 | Access time, seconds since the epoch |
| 28 | 4 | Modification time |
| 32 | 8 | Length in bytes |
| 40 | 64 | 16 direct block pointers |
| 104 | 4 | Single indirect block pointer |
| 108 | 4 | Double indirect block pointer |
| 112 | 4 | Zero |
| 116 | 64 | Name |
| 180 | 24 | Owner |
| 204 | 24 | Group |
| 228 | 24 | Last modifier |
| 252 | 4 | CRC-32C of bytes 0 to 251 |

A pointer of 0 is a hole, which reads as zeros. An indirect block holds
block size / 4 pointers. With 4 KiB blocks a file can have
16 + 1024 + 1024^2 blocks, a little over 4 GiB. A file's bytes past its
length, up to the end of its last block, are always zero, so a file that
grows again never shows old data.

### Journal

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | Magic, `purgjnl1` |
| 8 | 8 | Transaction sequence number |
| 16 | 4 | Image count, n |
| 20 | 4 | Checksum (64-bit FNV-1a folded to 32 bits) of bytes 0 to 19, the home list and the n images |
| 24 | 8n | The home block number of each image |

The images follow the header, one block each. A header that is not one,
whose checksum fails, or that names a home inside the journal or past the
end, is ignored.

The checksum over the images is FNV-1a, not a CRC. A CRC over a block of
entries that each end in their own CRC-32C does not depend on the entries
at all, so a torn journal of directory blocks would pass a CRC-32C check.
The crash test found this.

## Transactions and crashes

Every operation that changes metadata is one transaction. The
transaction's metadata blocks (directory blocks, indirect blocks, the free
map, block 0) are changed in memory; the file data it writes goes straight
to the disk. The commit:

1. flushes the disk, so the transaction's data and the previous
   transaction's home writes are on stable storage;
2. writes the header and the images to the journal in one request;
3. flushes again: the transaction is now durable;
4. writes each image to its home block.

At mount, a journal whose checksum matches is written home again. This
is idempotent: the journal holds the newest copy of each of its blocks.
A read-only mount keeps the images in memory instead. `Fs::sync` (a
clean unmount) flushes and then zeroes the journal header, so the next
mount has nothing to replay; a journal that is still there means the
file system was not synced since its last change.

The rules that make this safe:

- A block a transaction frees is not reused until the transaction
  commits, and a freed block is dropped from the transaction's images.
- New data blocks are written before the commit that makes anything
  point at them.
- The next transaction's first flush comes before its journal write, so
  a journal is never overwritten before the transaction it holds is home.
- Writes and flushes never return early: a prog killed in the middle of a
  commit finishes it. Only reads can be interrupted, and they change
  nothing.
- If a commit's write or flush fails, the file system stops writing
  (later changes fail with `i/o error`) until a remount replays the
  journal.

What a crash leaves:

- **The metadata is always consistent:** as it was after the last
  transaction whose second flush finished, or after the one in progress.
  Nothing is half created, half renamed or half removed, and the free map
  matches what the files use. The crash test in `crates/purg-fs/tests`
  cuts a workload short at every write, keeps an arbitrary subset of the
  unflushed writes (and an arbitrary prefix of the write it cut), and
  checks every result with the checker, read-only and after a replay.
- **Large changes are several transactions.** A write adds at most 16
  blocks per transaction, and truncating or removing a file frees at most
  16 per transaction (from the end), so a crash can leave a large write
  partly done (a shorter file) or a removed file shorter but still there.
  Each step is consistent on its own.
- **File data is not journaled.** An overwrite in place can be half
  done after a crash. New data never appears as garbage, since blocks are
  written before they are linked in.
- **Timing:** a change returns once its journal is durable, so a
  completed `write`, `create`, `remove` or `wstat` survives a crash.

## Inferno semantics

- **Qids:** the path is unique for the file system's life (the
  superblock's counter; the root is 0). The version goes up with each
  write or truncation of a file and each change to a directory's entries.
- **Owners and groups** are strings. A new file's owner is the creating
  user and its group the directory's group. A new file's permissions are
  `perm & (~0666 | dir & 0666)`, a directory's `perm & (~0777 | dir &
  0777)`.
- **Permission checks:** the owner's bits if the user owns the file, else
  the group's if the user's name is the group, else the others'. Walking
  needs search permission, creating and removing write permission on the
  directory. The kernel has one user (`inferno`), so in practice the
  checks follow the mode bits of files made by other tools.
- **wstat:** the name needs write permission on the parent and must be
  free there (`file already exists`); the length needs write permission,
  and a directory's cannot change; the mode, times and group need the
  owner; the owner cannot change, and `DMDIR` cannot be set or cleared
  (`permission denied`). Everything is checked before anything changes.
  A rename and the other fields are one transaction; a truncation to a
  shorter length comes first and can be several.
- **Times:** the modification time is set by writes, truncations and
  directory changes. Reads never write the disk: the access time is set
  at create and by an open when it is more than a minute old.
- **Errors** are Inferno's: `file does not exist`, `file already exists`,
  `permission denied`, `directory not empty`, `file is a directory`,
  `not a directory`, `file name syntax` (a name that is empty, `.`, `..`,
  holds `/` or NUL, or is over 64 bytes), `file system full`,
  `i/o error`.
- Create of an existing file truncates it, as Inferno's create does.

## Memory

The file system keeps a cache of metadata blocks (32 by default, 128 KiB
at 4 KiB blocks), least recently used out first. File data is never
cached: reads and writes go to the disk through one block-sized buffer.
A transaction holds at most J-1 images (31 blocks), and its commit
builds the journal write in one buffer of up to J blocks. So the file
system's RAM is bounded: about 128 KiB of cache, plus up to about 250 KiB
while a transaction commits. The virtio-blk driver adds four 16 KiB
bounce buffers per disk.

## Limits

| | |
|---|---|
| Block size | 512 to 65536, a power of two (4096 by default) |
| Blocks | 2^32 |
| File | 16 + P + P^2 blocks, P = block size / 4 (4 GiB and a little at 4 KiB) |
| Name | 64 bytes |
| Owner, group | 24 bytes |
| Entries per directory block | block size / 256 |
| Journal | 8 to 2^32 blocks; a transaction holds at most J-1 or (block size - 24) / 8 images |
| Times | 32-bit seconds (to 2106) |

Directories never shrink: a removed entry leaves a free slot that the
next create in that directory reuses. There are no hard links, symbolic
links, device files or extended attributes, as in Inferno.

## Tools

- `disk/mkdisk [-b blocksize] [-j journal] [-s size] [-u user] [-r dir]
  image` makes a new file system in an image file or a disk's
  `data` file, optionally holding a copy of `dir`.
- `disk/chkdisk [-v] image` checks one without changing it: each problem
  on a line, exit status `errors` if there are any; `-v` adds a summary.
- `cargo xtask diskimg [--size SIZE] [--bsize N] OUT [DIR]` makes an image
  on the build host from a host directory, with the same crate, and
  `cargo xtask diskimg --check IMAGE` checks one. `xtask kernel` uses
  both.

`disk/mkfs` is Inferno's prototype-driven archiver, not a file system
maker, so the new commands have new names.

## Checking

`Fs::check` (and `disk/chkdisk`) walks the tree from the root and checks
every entry's checksum, name, owner and qid path (unique, below the
superblock's counter), that each block pointer is in the data region and
used once, that no block lies past a file's end, and that the free map
says exactly which blocks are in use. It reads through the journal, as
the next mount would, and never writes. A corrupt image never panics the
mount, the checker or the operations: the tests feed thousands of
randomly damaged images through all three.
