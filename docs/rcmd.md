# Remote commands: rcmd, cpu, rstyxd

`rcmd host cmd` runs `cmd` on another Purgatorio. The command's standard
input, output and error are the caller's, the caller's namespace appears
there as `/n/client`, and the command's exit status becomes `rcmd`'s.
`cpu` is the same with the caller's `/dev` bound before the remote one.
This is the transport `mcpd` uses remotely: `rcmd host mcpd` is an MCP
server on standard input and output (`docs/mcp.md`).

The roles are Inferno's (rcmd(1), cpu(1), rstyxd(8)). The protocol is
new, as the authentication is (`docs/auth.md`), so these do not talk to
an Inferno `rstyxd`.

## Commands

```
rcmd [-AS] [-k keyfile] [-e alg] [-x dir] host [cmd [arg...]]
cpu  [-AS] [-k keyfile] [-C alg] [net!]host [command [arg...]]
auxi/rstyxd [-A] [-e] [-C alg] [-k keyfile]
auxi/cpuslave [command [arg...]]
```

- `host` is a dial string. A bare name is `tcp!name!rstyx` (port 6668 in
  `/lib/ndb/common`).
- With no command the remote runs `sh`. One argument is a command line
  for the remote `sh`. Several arguments are the words of one command and
  are quoted on the way, so they arrive as given.
- `rcmd -x dir` exports `dir` instead of `/`, so `/n/client` there is
  `dir` here.
- `-A` skips authentication in both directions and leaves the connection
  in the clear. `-k` names the authinfo to prove with (`-f` is accepted,
  as Inferno spells it). `-S` (an addition) requires the server to prove
  itself (see "Authentication"); it does not go with `-A`.
- Without `-A` the connection is encrypted after the proof (see
  "Encryption"). `-e` and `-C` name the algorithm: `chacha20poly1305`,
  the only cipher and the default, or `none` (or `clear`), which is
  accepted only with `-A`. `rcmd -e none` without `-A` fails with `an
  authenticated connection is always encrypted; use -A for none`. They
  also take a key exchange, as `mount -C` does: `x25519`, the default,
  or `x25519mlkem768`, which uses and requires the hybrid post-quantum
  exchange (see "Post-quantum key exchange"). Neither goes with `-A`
  (`-A exchanges no keys`).
- `auxi/rstyxd -C x25519mlkem768` requires the hybrid exchange of every
  client; `-C x25519` names the default. It does not go with `-A`.
- `auxi/rstyxd` serves one client on fd 0. `-k` is the authinfo it
  proves itself with. With nothing trusted it cannot ask for a proof, so
  it refuses every client with `server trusts no keys` (`docs/auth.md`,
  "Trust by default"); `-A` serves everyone in the clear. `-e` was for
  refusing to serve in the clear when nothing was trusted, which is now
  always so; it is accepted and changes nothing. Start it from `listen`
  or `styxlisten` with its module path:

  ```
  listen 'tcp!*!rstyx' /bin/auxi/rstyxd.wasm &
  ```

- `cpu` sends `auxi/cpuslave command` as its command. The slave binds
  `/n/client/dev` before `/dev` in its own namespace and runs the command.

The command runs under `sh -c` as the user `rstyxd` runs as (there is no
way to change user yet), in a copy of `rstyxd`'s namespace and
environment and in its own process group. When the command exits, what
is left of its group is killed: a program that should outlive the session
must leave the group.

## Authentication

Both directions use the mount proof of `docs/auth.md`.

- **Client to server.** The server reads the same files an `export` in
  its namespace would: `/usr/<user>/key.pub`, `/lib/keys/users`,
  `/lib/keys/signers`, `/lib/keys/revoked`. If it trusts no key and no
  signer it refuses the client with `server trusts no keys`. Otherwise it
  sends an offer: a 32-byte challenge and an ephemeral X25519 key. The client answers with a version 3 proof for its user name and
  the attach name `rstyx`, made from `-k keyfile` or the default sources
  (factotum, `#¤`, the default authinfo, the user's seed). The attach
  name keeps a proof for a mount from being used here and the other way
  round. A client with no proof gets `authentication required`; a bad
  one gets the error a mount would (`untrusted signer`, `certificate
  expired`, ...). A version 1 or 2 proof is refused with `no session key
  offered`, since it carries no key to encrypt with.
- **Server to client.** After an accepted proof the server proves itself
  in the OK frame: its data is a server proof (`docs/auth.md`, "Server
  authentication"), signed over this session's transcript and a value
  bound to its keys, from `-k keyfile` on `rstyxd`, or else from its
  user's default keys (the guest factotum, `#¤`,
  `/usr/<user>/keyring/default`, `/usr/<user>/key`, the test key). The
  client checks it against what its namespace trusts as a server for the
  host it dialled (`docs/auth.md`, "Servers per host"): a key that
  `/lib/keys/servers` lists for that host or `*`, or a certificate from a
  signer in `/lib/keys/signers` that names that host. With `-S` a
  missing or failing proof ends the session with `server did not
  authenticate`, `certificate name mismatch` (a certificate for another
  host) or `server authentication failed`. Without `-S` the client goes
  on either way.
- **The client's export.** The client serves its namespace with the
  kernel's `export`. When the server proved it is the host dialled, the
  export asks no proof of its own and runs in the clear inside the
  encrypted connection (see "Encryption"). Otherwise, whether its proof
  was missing, failed, or was good but for another host, the export asks
  whoever mounts it for a proof, as any export does, and the server
  answers from the same key. So `/n/client` goes without a second
  handshake only to the server this namespace knows as that host; any
  other server gets it only by proving a user this namespace trusts, and
  when the namespace trusts nobody, not at all.

`rstyxd -A` asks for no proof. `rcmd -A` gives none and exports without
asking for one. Without `-A`, `rcmd` refuses a server that asks for no
proof, or that sends a bare 32-byte challenge, with `no session key
offered`: it does not go on in the clear unless told to.

Older clients ignore OK's data, so the server's proof needed no new
protocol version. It travels in the clear, as OK does: it holds only the
server's public key or certificate and a signature for this session.

## Encryption

The keys come from the version 3 mount handshake (`docs/auth.md`,
"Mount handshake, version 3"), carried in CHAL and PROOF: the server's
offer is `challenge[32] || S[32]`, the client's proof carries `C` and
signs both, and each end derives `c2s` and `s2c` from the X25519 secret
and the signed message. The ephemeral secrets are dropped with the
exchange, so a recorded connection stays unreadable even to someone who
later gets a long-term key.

PROOF and the server's OK or ERR are the last frames in the clear. Every
frame after them, both ways, is one record of `purg_auth::record`:

```
record = size[4] || ChaCha20-Poly1305(kind u8 || length u32 || data) || tag[16]
```

with its own key and sequence number per direction, as in the mount's
record layer. A record that is changed, replayed, reordered or dropped
fails to open, and the end that sees it stops with `bad record mac`.
`rcmd` prints it and exits; `rstyxd` closes the connection.

The 9P conversation inside STYX frames is the client's export. When the
server has proved itself, the export runs without a handshake of its own
(EXEC flag 1, and `export -A` on the client), in the clear inside the
encrypted connection. That is safe because the connection already
authenticates both ends: the client's proof and the server's are bound to
its keys, only the two of them hold those keys, and every frame is sealed
under them, so nobody else can speak in the 9P conversation. Being the
server this namespace trusts for the host dialled is then what gets the
server `/n/client`, in place of being a user the export trusts. When the
server's proof is missing, does not check, or is for another host, the
export authenticates the server and encrypts again, as before, so such a
server still has to prove its user.

Measured on an Apple Silicon host under Linux with the release emu and
wasmtime, copying 16 MiB from `/n/client` with `rcmd host 'cat
/n/client/f | wc -c'` takes 12.3 to 14.0 s (mean 13.0 s) with the export
in the clear and 12.5 to 14.0 s (mean 13.3 s) sealed twice, five runs
each: within the noise. The session's cost is in the guests (the WASM
pumps and the connection's own cipher in `rcmd` and `rstyxd`); the
export's cipher is the kernel's, which is native and, in the emu, `ring`.
What the change saves is the second handshake per session and one cipher
pass in the kernel.

An attacker who can answer for the server's address can still pose as
the server to a client without `-S` (the client's proof is for that
attacker's key, so it cannot be replayed to the real one), though it gets
`/n/client` only as a trusted user; `-S` closes that.

The stock root trusts no key, so `rcmd` between two unchanged systems is
refused (`server trusts no keys`) until keys are set up
(`docs/auth.md`, "Setting up keys"). Tests bind `/lib/test/keys` on
`/lib/keys` at both ends, which trusts the public test key for any user
and any host.

### Post-quantum key exchange

`rcmd -e x25519mlkem768` (`cpu -C x25519mlkem768`) asks for the hybrid
X25519 + ML-KEM-768 exchange of the mount handshake's version 4
(`docs/auth.md`, "Hybrid key exchange (version 4)"). The client says
`pgrstyx3` in HELLO instead of `pgrstyx2`. The server answers with the
long offer in CHAL, `challenge[32] || S[32] || ek[1184]`, with a fresh
ephemeral ML-KEM-768 key, and takes only a version 4 proof, which
carries the client's ciphertext and signs the whole long offer. The
keys, the records and the server's proof are then version 4's;
everything else is as in version 2.

There is no "use it if the other end can" mode, so there is no quiet
downgrade:

- A client that asked for it refuses a CHAL that is not the long offer
  with `no post-quantum key offered`, so rewriting HELLO to `pgrstyx2`,
  or cutting the ML-KEM key off the offer, ends the session. A server
  that does not know `pgrstyx3` (one built before this) refuses the
  HELLO with `rstyx protocol botch`, which the client reports as `no
  post-quantum key offered`; an `rstyxd -A` refuses it with the same.
- A server that was asked for it refuses a version 3 proof with
  `post-quantum key exchange required`. Taking the ciphertext out of a
  version 4 proof does not make a version 3 proof either: the signature
  is over the `pgauth4` message and the whole offer.
- `rstyxd -C x25519mlkem768` refuses a `pgrstyx2` client with
  `post-quantum key exchange required`.

A client and a server that both could, but neither asks, use version 3,
as before. The exchange adds 2.3 KB on the wire, as on a mount
(`docs/auth.md`, "Cost of the post-quantum options"). Here both ends are
guests, so the ML-KEM work runs in WASM: the client's encapsulation in
`rcmd`, the server's key generation and decapsulation in `rstyxd`.

## Protocol

Everything on the connection is a frame: `kind u8 | length u32 LE |
data`, with at most 64 KiB of data.

| kind | name | direction | data |
|---|---|---|---|
| 1 | HELLO | C to S | `pgrstyx2 <user>`, or `pgrstyx3 <user>` for the hybrid exchange |
| 2 | CHAL | S to C | `challenge[32] || S[32]` (for `pgrstyx3` followed by `ek[1184]`), or empty when no proof is wanted |
| 3 | PROOF | C to S | a version 3 proof (version 4 for `pgrstyx3`), or empty |
| 4 | OK | S to C | the server's proof, or empty when it has none |
| 5 | ERR | S to C | the reason the client is refused |
| 6 | EXEC | C to S | `flags u8`, then the command line |
| 7 | STYX | both | bytes of the 9P conversation |
| 8 | IN | C to S | standard input; empty is end of file |
| 9 | OUT | S to C | standard output |
| 10 | ERRS | S to C | standard error |
| 11 | WIN | both | `stream u8, n u32 LE`: `n` more bytes of `stream` may be sent |
| 12 | STATUS | S to C | the exit status; the last frame |

The opening is HELLO, CHAL, PROOF, OK (or ERR), EXEC. From EXEC on,
frames are sealed unless there was no proof. EXEC flag 1 says the
client's export does not authenticate. Version 1 (`pgrstyx1`, before
encryption) is not spoken. After EXEC, frames 7 to 11
flow in any order until STATUS. The client closes the connection when it
has the status.

In the 9P conversation the client is the server: it runs `export /` on a
pipe and copies between the pipe and STYX frames. `rstyxd` does the same
with a pipe it mounts on `/n/client`.

### Flow control

One connection carries the 9P conversation and three byte streams. A
stream whose reader has stopped must not block the rest, so IN, OUT and
ERRS are windowed: each starts with 16 KiB of credit, a sender never
sends more than its credit, and the receiver returns credit with WIN once
it has passed the bytes on (to the command, or to the caller's
descriptor). On the server a helper prog writes input to the command and
reports each piece, so a command that never reads its input holds up
that helper and nothing else.

### How it is built

A WASM prog has one thread and a read of a connection blocks it. Each
end is therefore one prog that owns the connection's write side and
waits only on pipes (`purgatorio.wait`), with helper progs that do the
blocking reads and feed those pipes: one copies the connection in, and on
the client another copies standard input in. The kernel's `export` runs
in its own prog on the client.

## Limits

- The server authenticates itself only to a client that asks with `-S`
  (and, without `-S`, gets `/n/client` without a second handshake only
  when it proved the host dialled). The host is the dial string's, as
  written: a certificate for `fs.example` does not prove `10.0.0.5`.
- The remote command runs as the server's user.
- The client's namespace is served by the kernel's `export`, which
  serves requests concurrently, so a remote read that blocks (of
  `/n/client/dev/cons` under `cpu`, say) does not hold up other use of
  `/n/client`. The design does not depend on that: the command's
  standard input and output do not go through the export, and the
  session cannot deadlock on it.
- `rcmd` reads its standard input whether or not the remote command
  does, as `ssh` does. Give it `</dev/null` in a script that has input
  of its own.
- `rstyxd` mounts on `/n/client` and creates `/n` and `/n/client` when
  they are missing.
