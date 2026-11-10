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
rcmd [-A] [-k keyfile] [-e alg] [-x dir] host [cmd [arg...]]
cpu  [-A] [-k keyfile] [-C alg] [net!]host [command [arg...]]
auxi/rstyxd [-A] [-k keyfile]
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
  as Inferno spells it).
- Without `-A` the connection is encrypted after the proof (see
  "Encryption"). `-e` and `-C` name the algorithm: `chacha20poly1305`,
  the only one and the default, or `none` (or `clear`), which is accepted
  only with `-A`. `rcmd -e none` without `-A` fails with `an
  authenticated connection is always encrypted; use -A for none`.
- `auxi/rstyxd` serves one client on fd 0. Start it from `listen` or
  `styxlisten` with its module path:

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
  its namespace would: `/usr/<user>/key.pub`, `/lib/test/auth.key.pub`,
  `/lib/keys/signers`, `/lib/keys/revoked`. If it trusts any key or
  signer it sends an offer: a 32-byte challenge and an ephemeral X25519
  key. The client answers with a version 3 proof for its user name and
  the attach name `rstyx`, made from `-k keyfile` or the default sources
  (factotum, `#¤`, the default authinfo, the user's seed). The attach
  name keeps a proof for a mount from being used here and the other way
  round. A client with no proof gets `authentication required`; a bad
  one gets the error a mount would (`untrusted signer`, `certificate
  expired`, ...). A version 1 or 2 proof is refused with `no session key
  offered`, since it carries no key to encrypt with.
- **Server to client.** The client serves its namespace with the
  kernel's `export`, which asks whoever mounts it for a proof when the
  client's namespace trusts anything. The server answers from `-k
  keyfile` on `rstyxd`, or else from its user's default keys.

`rstyxd -A` asks for no proof. `rcmd -A` gives none and exports without
asking for one. Without `-A`, `rcmd` refuses a server that asks for no
proof, or that sends a bare 32-byte challenge, with `no session key
offered`: it does not go on in the clear unless told to.

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

The 9P conversation inside STYX frames runs its own handshake, so the
client's export to the server is a version 3 mount over the encrypted
connection and is sealed twice. That costs a second cipher pass on
`/n/client` traffic and nothing else.

What is not done: the server does not prove itself to the client. An
attacker who can answer for the server's address can pose as the server
(the client's proof is for that attacker's key, so it cannot be replayed
to the real one). The client's export still asks the server for a proof
before serving `/n/client` when the client's namespace trusts any key, as
it does with the stock test key.

The stock root holds the public test key pair, so an unchanged system
authenticates with it in both directions. Deployments remove it.

## Protocol

Everything on the connection is a frame: `kind u8 | length u32 LE |
data`, with at most 64 KiB of data.

| kind | name | direction | data |
|---|---|---|---|
| 1 | HELLO | C to S | `pgrstyx2 <user>` |
| 2 | CHAL | S to C | `challenge[32] || S[32]`, or empty when no proof is wanted |
| 3 | PROOF | C to S | a version 3 proof, or empty |
| 4 | OK | S to C | empty |
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

- The server does not authenticate itself (above).
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
