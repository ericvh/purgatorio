# Venti and vac

`vacput`, `vacget` and `vacfs` keep file trees in a Venti server, as
Inferno's vacget(1) and vacfs(4) do. They share `appl/lib/venti`: the
Venti protocol client (Inferno's venti(2)) and the vac archive format.
Plan 9's venti(6) and vac(1) describe the formats; the aim is that Plan 9
and plan9port tools read what these write and the other way round.

## The commands

```
vacput [-dv] [-i | -x] [-a addr] [-b blocksize] [-n name] [-u uid] [-g gid] path ...
vacget [-dptv] [-a addr] [vac:]score
vacfs [-Ddp] [-a addr] [[vac:]score]
```

`vacput` prints the archive's score, `vac:` and 40 hex digits. `vacget`
writes the archive under the current directory. `vacfs` speaks Styx on
standard input, so it is mounted with `mount {vacfs vac:...} /n/vac`;
without a score its root is empty and walking to a score opens that
archive. All three dial `$venti` (the `venti` attribute in the network
database; the service is `tcp=venti port=17034` in `/lib/ndb/common`)
unless given `-a addr`.

## The protocol

A connection starts with each side writing a line
`venti-<versions>-<comment>`; each picks the first version in the
other's list that it speaks. We speak `02`. Then comes a hello. Messages
are `size[2] type[1] tag[1]` and a body, all integers big-endian. The
client implements hello, ping, read, write, sync and goodbye. As Plan 9's
libventi does, it checks the hash of each block it reads and the score a
write returns, and never sends the empty block (its score is the SHA-1 of
nothing, `da39a3ee...`).

Block types are the wire types Inferno uses and Plan 9 translates its own
to (`vttodisktype`): root 1, directory 2, pointer levels 3 to 9, data 13.

## The archive

A file is a stream: data blocks of `dsize` bytes under a tree of pointer
blocks of `psize` bytes, described by a 40-byte entry. Trailing zeros of
a block, and trailing zero scores of a pointer block, are not stored. A
directory is a stream of entries and a stream of meta blocks. Each meta
block holds directory records (Plan 9's `VacDir`, version 9) sorted in
vac's order, in which a name sorts after the longer names it begins. The
root block (300 bytes, type `vac`) points at a block of three entries:
the root directory's two streams and a stream with the root's own record.

The reader also takes what Plan 9 and fossil write that we do not:
records of versions 7 and 8, meta blocks with free space and fossil's
order (magic `0x5656fc7a`), entries and roots with block sizes in the
large form, directory streams whose block size is not a multiple of 40,
and fossil's extra level above the three top entries.

## Differences from Inferno

- `vacput` writes a directory's files sorted by name, so the same tree
  with the same times gives the same blocks. The root record's change
  time is still the time of the run, as in Inferno.
- An empty directory's stream has depth 0, and all the records of a
  directory are sorted together rather than block by block. Both forms
  read the same.
- `vacput -b` takes 512 to 57344 bytes.
- `vacfs` refuses an open for writing; Inferno's refuses the write.
  A missing name is `no such file or directory`, as in Inferno's.
- A root block whose type is not `vac` is refused, as Plan 9 does.

## Tests

`appl/lib/venti/src/tests.rs` has known answers for the message, root,
entry, record and meta block layouts, written from the formats in
venti(6) and Plan 9's sources (SHA-1 vectors from FIPS 180-4), and round
trips with deep trees, zero blocks, many meta blocks and a fossil-style
root. `root/lib/test/sh/vac.sh` runs the commands against
`test/netsrv venti`, an in-memory server, on both VMs, in the probe and
in the kernel.

Not done: incremental archives (Plan 9's `vac -d`), and a Venti server
command (`venti/venti`, Inferno's `ventisrv`) beyond the test server.
