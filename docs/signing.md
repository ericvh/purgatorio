# Module digests and signing

This is the design for signed modules (issue #33). It reuses the
authentication work in `docs/auth.md`: Ed25519 keys, `pgcert1`
certificates, key lists and the revocation list. Nothing here changes how
an unsigned module is built, and the default policy still loads anything.

## Model

A module has a **digest**: SHA-256 over its bytes without any signature.
A **signature** is an Ed25519 signature over that digest. Signatures
travel inside the module, in a custom section named `purgatorio.sig`, so a
signed module is still one file that any WASM tool can read and that `cp`
and `bind` move around unchanged.

Each prog has a **module policy**: `none`, `warn` or `require`. The loader
checks every module the prog loads against it: the command given to
`spawn_prog`, a reactor given to `load`, and the system's first prog.
A module passes when one of its signatures is by a key the policy's
**trust anchors** accept.

## Digest

The digest is SHA-256 over the module with every `purgatorio.sig` custom
section removed, wherever it sits. Exactly:

1. The module must start with the 8 bytes `\0asm` `01 00 00 00`.
2. The rest is a list of sections. Each is one id byte, a size in
   unsigned LEB128, and that many bytes of contents. The size uses at most
   5 bytes, and a fifth byte must be at most `0x0f`. The contents must end
   at or before the end of the file. The list must end exactly at the end
   of the file.
3. A section with id 0 is a custom section. Its contents start with a name:
   a length in the same LEB128 form and that many bytes, which must fit in
   the section. The name is compared as bytes.
4. Hash the 8 header bytes, then every section in file order, from its id
   byte to the last byte of its contents, as the bytes appear in the file.
   Skip, whole, each custom section whose name is exactly `purgatorio.sig`.

So the digest is the hash of the file with some byte ranges cut out.
Nothing is re-encoded: a size written with redundant LEB128 bytes is hashed
as written. A file that breaks rule 1, 2 or 3 has no digest and is not a
module (`bad executable`). The names of other custom sections and the
contents of all sections are not interpreted.

The digest of a signed module equals the digest of the module it was made
from, and removing the signature gives the original bytes back when the
signer appended the section (as both tools do).

## The `purgatorio.sig` section

A custom section named `purgatorio.sig`. After the name:

```
version u8          1
count   u8          1 to 8 entries
entry   ...         count times

entry  = kind u8 || signature[64] || body
kind 0: body = key[32]                      a bare signer key
kind 1: body = ncerts u8 (1 to 4)
               then ncerts times: len u16 LE (1 to 2048) || pgcert1 line
```

- The **signing key** of an entry is `key` for kind 0 and the `key` of the
  first certificate for kind 1.
- `signature` is the signing key's Ed25519 signature over

  ```
  "pgmodsig1" 0x00 || digest[32] || len(abi) u8 || abi
  ```

  where `abi` is the contents of the module's first `purgatorio.abi`
  section, or empty when it has none (third-party WASI modules). The
  digest already covers that section; naming the ABI version in the
  message as well means the signed statement reads "this code, for this
  ABI" on its own, for a log or a tool that holds only the digest.
- A certificate line is a `pgcert1` certificate exactly as `docs/auth.md`
  defines it, with no newline. The first certifies the signing key. Each
  later one certifies a key that signed the one before it. That is the
  **chain**: a module can be signed by a key that a trusted signer
  certified, directly or through up to three intermediates.
- **Countersigning** is one more entry. No signing key may appear twice.

Parsing is strict: an unknown version or kind, a count or length out of
range, a duplicate signing key, a certificate that `pgcert1` parsing
rejects, or bytes left over make the whole section bad. A later format
gets a new version byte and old loaders reject it. The context string
`pgmodsig1` differs from `pgcert1` and `pgauth2`, so a signature made for
one purpose never verifies for another.

A signer writes one `purgatorio.sig` section, as the last section of the
module, holding all entries. A verifier accepts it at any position. A
module with more than one `purgatorio.sig` section is refused (`bad
signature`) by any policy that checks signatures; the digest still ignores
them all.

## Trust

A policy that checks signatures reads two key lists (the format of
`docs/auth.md`: 64 hex digits and an optional comment per line):

- **`/lib/keys/modules`**: the keys trusted to sign modules, or to certify
  keys that do.
- **`/lib/keys/revoked`**: the same revocation list mounts use. A key in it
  is refused as a signing key, as a certificate's key and as a signer of a
  certificate. A line may also be a module **digest**: that module is then
  refused whoever signed it (`module revoked`). A list that is there but
  does not parse, or cannot be read, refuses everything, as it does for
  mounts.

An entry is **trusted** when its signature verifies and:

- kind 0: its key is in `modules` and not revoked;
- kind 1: no certificate in the chain has expired, no certificate's key is
  revoked, and walking from the first certificate, some certificate has a
  valid signature by a key that is in `modules` and not revoked, while
  every certificate before it has a valid signature by the key of the next
  one. A signing key that is itself in `modules` is trusted without
  looking at who signed its certificate.

A module passes when at least one entry is trusted. Otherwise the error is
the first of these that applies:

| error | meaning |
|---|---|
| `module not signed` | there is no `purgatorio.sig` section |
| `module revoked` | the module's digest is in the revocation list |
| `bad signature` | the section is malformed or repeated, or no entry's signature verifies (the module was changed after signing) |
| `certificate revoked` | an entry verifies, but a key in it or its anchor is revoked, or the revocation list is damaged |
| `certificate expired` | an entry verifies, but a certificate in its chain has expired |
| `untrusted signer` | an entry verifies, but nothing links it to a key in `modules` |

`certificate revoked`, `certificate expired` and `untrusted signer` are the
strings mounts already use. The names in the certificates are labels for
people (`auth/verify -m` prints them). The loader does not compare them
with anything.

### Why a separate list

Module signers do not share `/lib/keys/signers`. A key in `signers` is
trusted to say "this key belongs to user X", and `auth/signer` hands such
certificates to anyone who knows a password. If the same list anchored
modules, every user with a login certificate would hold a key "certified
by a trusted signer" and could sign code the system must run. `pgcert1` has no
field that says what a certificate is for, so the only safe separation is
the anchor: a key certifies module signers only if someone put it in
`modules`. An operator who really wants one key for both can list it in
both files, and should then not run `auth/signer` with it. The revocation
list is shared, because a revoked key should stop working everywhere.

### Where the lists are read: the pinned namespace

Mount authentication reads its lists in the exporter's namespace, because
the exporter is the party that trusts. For modules the trusting party is
whoever set the policy, and the prog being confined is not trusted: it
can `bind` anything over `/lib/keys` in its own namespace. So the lists
are **not** resolved in the namespace of the prog at the time of the load.

Instead, when a prog's policy first leaves `none`, the kernel keeps a
private copy of that prog's namespace (its mount table as it is at that
moment, as `pctl(FORKNS)` would make). The policy carries that copy. Every
later check under the policy, in that prog and in every prog that inherits
it, resolves `/lib/keys/modules` and `/lib/keys/revoked` in the copy. The
files are opened and read again at each load, so adding a signer or
revoking a key takes effect at the next load. What cannot change is which
files those names mean. Tightening `warn` to `require` keeps the copy.

Someone who can write the real files can still change what is trusted, so
they need permissions that keep untrusted progs out, as for mounts.

## Policy

| level | a module that does not pass |
|---|---|
| `none` | loads. Nothing is checked and no list is read. This is the default, and today's behaviour. |
| `warn` | loads, after `<path>: warning: <error>` is written to the loading prog's fd 2. |
| `require` | does not load. `spawn_prog` or `load` fails with `<path>: <error>`. |

The digest is computed at every level, once per load, from the bytes that
are then run. There is no module cache yet; when there is one, the digest
and the verdict belong beside the cached bytes, keyed by the policy's
anchors.

### Setting and inheriting it

A prog's level is set by writing to its `ctl` file:

```
echo modules require > /prog/<pid>/ctl      (or warn)
```

- The level can only be **tightened**: `none` to `warn` or `require`,
  `warn` to `require`. Writing the current level does nothing. Lowering it
  fails with `permission denied`.
- A new prog **inherits** the policy (level and pinned namespace) of the
  prog that spawned it, at the time of the spawn.
- The **system default** is the policy of the first prog. `emu
  --modules=warn|require` sets it before the first module is loaded, pinned
  to the boot namespace, so the first prog is checked too. The probe and
  the kernel have no command line yet: their first prog comes from the
  boot image and is trusted like the kernel itself, and it sets the policy
  as its first act when the system wants one.
- `auth/modpolicy level cmd [arg ...]` sets its own level and runs `cmd`,
  for use from a script.

The choice was between a `pctl` flag and a `ctl` message. A `pctl` flag is
a new guest-visible call argument, so it needs an ABI bump and a rebuild of
every module, and only a prog itself could set it. A `ctl` message is a
file: it needs no ABI change, a supervisor can set it on a prog it started,
and `ps`-like tools read the result from the same directory.

The security argument for letting anyone who can open the `ctl` file write
this message:

- It only tightens. The worst a hostile writer can do is make a prog
  unable to load modules, which is less than `kill`, which the same file
  already takes.
- The pinned namespace is the **target** prog's, not the writer's, so a
  writer cannot choose another prog's anchors.
- A confined prog cannot loosen itself, cannot be loosened by a child, and
  cannot swap its anchors by binding over `/lib/keys`, because of the pin.
- A prog cannot escape by spawning: children inherit. Every way to run
  code in this system is `spawn_prog` or `load`, and both are checked.

### Seeing it

`/prog/<pid>/module` is a new read-only file. `status` keeps Inferno's
format.

```
policy <level>
module <digest> <signers> <path>
module ...
```

The first `module` line is the prog's command. Each reactor it loaded adds
a line. `digest` is 64 hex digits. `signers` is the signing keys of the
trusted entries, in hex, joined with commas, or `-` when none was verified
(the level was `none` at that load, or the check failed under `warn`).
`path` is the name the module was loaded by, quoted when it holds white space or a quote.

This is also how a guest asks for its own digest: it reads
`/prog/<its pid>/module`. No host call is added, and the ABI version
stays 0.6.

## Tools

| tool | role |
|---|---|
| `cargo xtask sign <module> --key <seed file> [--cert <file>]... [-o <out>]` | Signs on the build host. `--cert` gives the chain, first the certificate for the key. Rewrites the module in place without `-o`. An existing entry by the same key is replaced; other entries stay (countersigning). |
| `auth/sign [-k keyfile] [-c certfile]... [-o out] module` | The same inside the system. With `-k` the key is a seed file or an authinfo (whose certificate is used). Without it the signature comes from the guest factotum, then from `#¤`, so the private key need not be a readable file. |
| `auth/verify -m [-s modules] [-r revoked] [-t time] module` | Prints the module's digest and ABI version, and for each entry its signing key, its standing (`trusted`, `untrusted`, `revoked`, `expired`, `bad`) and the names in its certificates. Exits with the error the loader would give under `require`. It reads the lists in its own namespace (or the ones named), so it is a check for people, not the loader's verdict. Without `-m` it checks a certificate, as before. |
| `auth/modpolicy warn\|require cmd [arg ...]` | Sets its own module policy, then runs `cmd` under it. |

Signing through an agent:

- **`#¤`** signs the bytes written to `rpc`. `auth/sign` builds the
  `pgmodsig1` message itself and reads the public key and certificate from
  `#¤/ctl`.
- **`auth/factotum`** gains a role: `start proto=ed25519 role=modsign`,
  then `write <digest[32] || abi>`, then `read` returns `ok <entry>`, one
  encoded section entry. Factotum builds the `pgmodsig1` message itself,
  so this role cannot be used to sign a mount proof or a certificate, and
  the mount role cannot be used to sign a module.

Signing is a step after the build, never part of it. `cargo xtask
build-root` still writes unsigned modules, and CI still requires them to
be byte-identical across build hosts. Ed25519 signatures are
deterministic, so signing the same module with the same key gives the same
bytes everywhere; CI signs one module with the public test key on each
host and compares the hashes with the rest.

## What is and is not protected

- **Tampering.** Any change outside a `purgatorio.sig` section changes the
  digest, so no signature verifies: `bad signature`. That includes a
  section added after signing, a changed data segment, and bytes appended
  after the last section (which break the section list).
- **Downgrade to unsigned.** Removing the section gives a valid unsigned
  module. Under `require` it is refused (`module not signed`). Under
  `none` and `warn` it runs; that is what those levels mean.
- **The signature section itself** is not signed. Someone who can rewrite
  the file can remove entries or add entries of their own. Neither helps:
  an added entry needs a trusted key, and a module passes on one trusted
  entry, so removing others changes nothing unless it removes the only
  trusted one, which is the downgrade above.
- **Check and use.** The loader reads the file once. The bytes it hashes
  are the bytes the engine runs.
- **Replay of an old module.** Not prevented by the signature: an old
  module with a known bug, signed by a still-trusted key, verifies for
  ever. There is no version or time in a signature. The defences are to
  list the old module's digest in `/lib/keys/revoked`, or to revoke the
  key and sign again with a new one.
- **Expiry.** A certificate's expiry is checked against the loader's clock
  at each load. There is no timestamping, so a module signed through a
  certificate stops loading when the certificate expires. Use a bare key
  or a certificate with no expiry for modules meant to last.
- **Names and arguments.** A signature covers contents only: not the
  path, not the name, not the arguments or the environment. A signed `rm`
  bound or copied over `ls` still verifies. Under `require` a prog can
  only choose among signed modules, but it can choose any of them.
- **Interpreted code.** `sh`, `mash` and `mk` are signed modules that run
  unsigned scripts. `require` confines which WASM runs, not what a signed
  interpreter is told to do. `#C` (host commands on the emu) is outside it
  too.
- **Keys in agents.** `#¤/rpc` signs any bytes for any prog that can open
  it, so a key held there can sign a module for any local prog. Do not
  load a key that this system's own `modules` list trusts into `#¤` on a
  system that relies on `require`. The guest factotum is mounted in a
  namespace and so reaches only the progs that see it.
- **Certificates not meant for code.** `pgcert1` does not say what a
  certificate is for. Separation comes only from which list a key is in
  (see "Why a separate list").
- **The pinned namespace** holds whatever was mounted when the policy was
  set, for as long as a prog under the policy lives.

## Ahead-of-time images

The kernel can run a module as native code compiled on the build host
(`purg-vm-aot`, `docs/perf.md` Section 11). Native code is outside the
WebAssembly sandbox: it can do anything the kernel can. So an image is
trusted as the kernel is, never as a module is, and the module policy
alone never lets one run.

An image comes from one of two places:

- **Linked into the kernel** at build time (`PURG_AOT_IMAGES`, the
  `.text.aot` section). It is part of the kernel's image and needs no
  signature.
- **A signed image file** beside the module: `/bin/<cmd>.cwasm` next to
  `/bin/<cmd>.wasm`, read at run time.

### The signed image file

```
magic   "PURGIMG1"
kind    u8          0 native aarch64, 1 Pulley
zero    3 bytes
module  [32]        the module's digest, as above
len     u64 LE
image   len bytes   wasmtime's precompiled module
sig     ...         to the end: the contents of a purgatorio.sig section
```

The signature part is a `purgatorio.sig` body (version, count, entries)
with the same entry kinds, a bare key or a certificate chain. Each entry
signs

```
"pgaotimg1" 0x00 || kind u8 || module digest[32] || SHA-256(image)[32]
```

so a signature says "this code is a compilation of that module for this
target". The context string differs from `pgmodsig1`, `pgcert1` and
`pgauth2`, so a module signature never verifies as an image signature
or the other way round. `cwasm -s` writes these files, and `cargo xtask
aot` writes one for every module of the root, signed with the public
test key.

### Verification

When a prog that is not traced runs a module, the engine takes the digest
the loader computed for it (the bytes the policy checked), and looks for
an image: first in the linked set, then in the file `<path>.cwasm`, where
`<path>` is the name the module was loaded by, less `.wasm`. The file is
read through the prog's namespace, so a prog can bind any file there, and
the check does not depend on where it came from. It runs only if all of
these hold:

1. it parses strictly, its kind is the engine's, and its module digest
   is the digest of the module the prog loaded;
2. neither that digest nor the SHA-256 of the image is in
   `/lib/keys/revoked`, and the list is readable (a damaged list refuses
   everything);
3. one of its entries is **trusted** under the rules above, with
   **`/lib/keys/aot`** as the anchors instead of `/lib/keys/modules`.

Both lists are read in the **boot namespace**, at each image load: a
copy pinned when the engine starts, which no prog can bind over. Only an
image that passes is copied into executable memory (`purg_aarch64::code`:
mapped writable, filled, then remapped executable and read-only, with
the linear map's alias read-only too, and the caches made coherent). The
image is checked once per load, and the bytes checked are the bytes
copied. A verified image is shared by the progs of one generation and
freed with it (`docs/perf.md`).

An image that is missing or refused is not an error: the module runs on
wasmi, as it would without the engine, unless the kernel was booted with
`engine=aot`, which refuses it. A reactor loaded by a prog running native
code must have a verified image; one loaded by a prog on wasmi runs on
wasmi.

### Why a separate list

A key in `/lib/keys/modules` vouches for WebAssembly, which the sandbox
still confines. A key in `/lib/keys/aot` vouches for native code, which
nothing confines. Keeping the anchors apart means that trusting someone
to sign modules does not let them run code in the kernel. The
revocation list is shared.

The test initrd that `cargo xtask kernel --aot` builds trusts the public
test key in `/lib/keys/aot`; `root/` has no such file, so a system built
from it runs no image file until its operator lists a key.

### What it does not protect

- **The module policy still applies** to the module, before the image is
  looked for: under `require`, an unsigned module is refused whatever
  image it has. An image does not stand in for the module's own
  signature.
- **The compiler is trusted.** The signature says who vouches for the
  translation, not that it is correct. A bug in Cranelift, or a signer
  who signs code that is not wasmtime's output, can break the sandbox.
- **wasmtime's version.** An image for another wasmtime, or for other
  settings, verifies but does not load (wasmtime checks); the module then
  runs on wasmi.

## Implementation and tests

- `purg-auth::modsig` (`no_std` + `alloc`): the section walk, the digest,
  the section encoder and strict parser, the signed message, signing, and
  the check of a module against anchors, a revocation list and a time.
- `purg-core`: the policy and the list of loaded modules live in `Proc`;
  `Prog::check_module` computes the digest, reads the lists in the pinned
  namespace, applies the level and records the module; `#p` serves
  `module` and takes `modules <level>` on `ctl`.
- `purg-vm`: `load_module`, the one place every engine and embedder loads
  a module through, calls `check_module` after the ABI check. That covers
  `spawn_prog`, `load` and the first prog on the emu, the probe and the
  kernel.
- `purg-vm-aot::image`: the signed image format, its parser and its
  check, with unit tests for each refusal (another signer, module or
  kind, each revocation, a damaged list, a changed byte, every
  truncation) and for determinism. `purg_core::modsig::trust_lists`
  reads `/lib/keys/aot` and the revocation list as the policy reads its
  own. The kernel's test mode under `cargo xtask kernel --aot` runs every
  module of the root from a signed image file.
- Tests: unit tests in `purg-auth` (digest with the section first, in the
  middle and last, several entries, chains, every truncation of a signed
  module and of a section, a fixed test vector); loader tests in
  `crates/purg-vm/tests/modsig.rs` for each level and each error, the
  tighten-only rule, inheritance and the pinned namespace; emu tests in
  `crates/purg-emu/tests/modsig.rs` for `--modules`, a first prog signed
  on the host and signing through the guest factotum; the sh corpus
  script `modsig.sh`, which signs a module in the system through `#¤`,
  runs it under `require`, and shows an unsigned module, a changed one,
  an untrusted signer and a revoked signer refused, on both VMs, the
  probe and the kernel.
- Three faults found on the way are fixed: the SDK read `#¤/ctl` through
  wasi-libc, which cannot resolve a device name, and read the signature
  at the offset its own write had left, so a guest could not use `#¤`
  itself (the kernel's mount could); and `#¤` trimmed white space from a
  raw seed.

## Decisions and open questions

Decided here, for the owner to confirm:

1. **Separate anchor list** `/lib/keys/modules`, shared revocation list.
2. **`ctl` message, tighten-only, inherited, namespace pinned** when the
   policy is first set.
3. **A signature covers contents and the ABI version, not the name.**
   Names belong to namespaces here: `bind` gives any module any name, and
   a module is found by `/bin/<cmd>.wasm` in whatever is bound there, so a
   signed name would either break `bind` and `cp` or be ignored. The cost
   is the substitution noted above. If that matters, the fix is a signed
   optional attribute list in a version 2 entry (`name=`, `version=`),
   checked only when a policy asks.
4. **A guest reads its own digest** from `/prog/<pid>/module`. Any prog
   that can see `/prog` can read any prog's digest and signers, as it can
   read `status` and `ns`.

Still open:

1. **Boot-time default for the kernel and the probe.** They need a command
   line or a build-time setting to check the first prog. Today the boot
   image is the trust root.
2. **Thresholds.** One trusted signature is enough. Should a policy be
   able to ask for two (a builder and a reviewer)?
3. **Signed attributes** (name, version, a signing time) and with them
   rollback protection that does not need the digest list.
4. **A purpose field in certificates** (`pgcert2`), so one signer list
   could serve mounts and modules safely.
5. **Scripts.** Should `sh` refuse unsigned scripts under `require`?
