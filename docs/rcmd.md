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
- `-A` skips authentication in both directions. `-k` names the authinfo
  to prove with (`-f` is accepted, as Inferno spells it).
- `-e` and `-C` name an algorithm for the connection. Only `none` (or
  `clear`) is accepted: the connection is authenticated when it opens but
  is not encrypted or integrity-protected after that. Run it over a
  trusted network until there is a TLS server.
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
  signer it sends a 32-byte challenge. The client answers with a
  version 2 proof for its user name and the attach name `rstyx`, made
  from `-k keyfile` or the default sources (factotum, `#¤`, the default
  authinfo, the user's seed). The attach name keeps a proof for a mount
  from being used here and the other way round. A client with no proof
  gets `authentication required`; a bad one gets the error a mount would
  (`untrusted signer`, `certificate expired`, ...).
- **Server to client.** The client serves its namespace with the
  kernel's `export`, which asks whoever mounts it for a proof when the
  client's namespace trusts anything. The server answers from `-k
  keyfile` on `rstyxd`, or else from its user's default keys.

`rstyxd -A` asks for no proof. `rcmd -A` gives none and exports without
asking for one.

The stock root holds the public test key pair, so an unchanged system
authenticates with it in both directions. Deployments remove it.

## Protocol

Everything on the connection is a frame: `kind u8 | length u32 LE |
data`, with at most 64 KiB of data.

| kind | name | direction | data |
|---|---|---|---|
| 1 | HELLO | C to S | `pgrstyx1 <user>` |
| 2 | CHAL | S to C | `challenge[32]`, or empty when no proof is wanted |
| 3 | PROOF | C to S | the proof, or empty |
| 4 | OK | S to C | empty |
| 5 | ERR | S to C | the reason the client is refused |
| 6 | EXEC | C to S | `flags u8`, then the command line |
| 7 | STYX | both | bytes of the 9P conversation |
| 8 | IN | C to S | standard input; empty is end of file |
| 9 | OUT | S to C | standard output |
| 10 | ERRS | S to C | standard error |
| 11 | WIN | both | `stream u8, n u32 LE`: `n` more bytes of `stream` may be sent |
| 12 | STATUS | S to C | the exit status; the last frame |

The opening is HELLO, CHAL, PROOF, OK (or ERR), EXEC. EXEC flag 1 says
the client's export does not authenticate. After EXEC, frames 7 to 11
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

- No encryption after the handshake (above).
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
