# mcpd: Purgatorio as an MCP server

`mcpd` lets an LLM client drive a sandboxed Purgatorio session through the
Model Context Protocol (MCP). It is an ordinary guest command
(`/bin/mcpd.wasm`). It speaks MCP on its standard input and output, so an
MCP client starts it by starting `emu`. The plan and its later stages are
in issue #70. This page describes stage 1.

## Running it

```sh
cargo xtask build-root
cargo build --release -p purg-emu
target/release/emu -r /path/to/purgatorio/root mcpd
```

That command is a stdio MCP server. Register it with a client as a
command and its arguments. With Claude Code:

```sh
claude mcp add purgatorio -- /path/to/purgatorio/target/release/emu \
    -r /path/to/purgatorio/root mcpd
```

Clients that read an `mcpServers` table (Claude Desktop, `.mcp.json`) take:

```json
{
  "mcpServers": {
    "purgatorio": {
      "command": "/path/to/purgatorio/target/release/emu",
      "args": ["-r", "/path/to/purgatorio/root", "mcpd"]
    }
  }
}
```

Use absolute paths: the client chooses the working directory. Add
`--vm=wasmi` before `mcpd` to use the interpreter.

Options, after `mcpd`:

- `-l logfile` appends a line per session start and tool call to
  `logfile`, a name in the emu's own namespace (`-l /tmp/mcpd.log` is
  `root/tmp/mcpd.log` on the host). Without it nothing is logged. `mcpd`
  writes no diagnostics to its standard error, because on emu a guest's
  fd 2 is the console, which is the emu's standard output: the protocol
  stream.
- `-n nsfile` grants the session more than the default. See below.

You can try it by hand:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"me","version":"0"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run","arguments":{"command":"echo hello; ns"}}}' \
  | target/release/emu -r root mcpd
```

## Protocol

`mcpd` follows MCP revision **2026-07-28**, stdio transport: JSON-RPC 2.0,
one message per line, nothing else on standard output. It exits when its
input ends.

- 2026-07-28 has no handshake. Each request carries
  `io.modelcontextprotocol/protocolVersion` and
  `io.modelcontextprotocol/clientCapabilities` in `params._meta`. `mcpd`
  answers `server/discover`, `tools/list` and `tools/call`, and every
  result has `resultType`. A request without the metadata gets `-32602`;
  another version gets `-32022` with the supported list.
- A client of 2025-11-25, 2025-06-18, 2025-03-26 or 2024-11-05 sends
  `initialize` first. `mcpd` answers with the client's version if it is
  one of those, and with 2025-11-25 otherwise. After that, requests need
  no metadata. `ping` is answered for any client.
- `instructions` (in the `initialize` result and in `server/discover`) is
  a primer on Inferno sh, because models write bash by habit. The text is
  `appl/cmd/mcpd/src/primer.txt`. A test runs every example in it.
- Unknown methods get `-32601`, a line that is not JSON gets `-32700`, and
  an array (a batch) or a malformed request gets `-32600`. Notifications
  are never answered. An unknown tool is `-32602`. Bad arguments and
  failures of a tool are results with `isError: true`, so the model sees
  them. Their text is Inferno's error string, such as
  `/tmp/x: file does not exist`.
- A file tool that gets no answer within 60 seconds (a read of a file
  that blocks, such as a pipe nobody writes) fails, and the session is
  replaced as by `reset`.
- One request is served at a time. `notifications/cancelled` is accepted
  and ignored; use `timeout_ms`.
- Resources, prompts, and server-to-client requests are not implemented.

## Tools

Every tool has a JSON Schema for its arguments and its result
(`tools/list` shows them). A successful result is in `structuredContent`
and, as the same JSON, in a text block. File names are absolute, or
relative to `/tmp`.

| Tool | Arguments | Result |
|---|---|---|
| `run` | `command`, and optional `stdin`, `cwd`, `timeout_ms` (default 30000, at most 600000), `max_output_bytes` (default 65536 for each stream, at most 1 MiB) | `stdout`, `stderr`, `status` (Inferno's exit status string; empty is success), `ok`, `timed_out`, `stdout_truncated`, `stderr_truncated` |
| `read_file` | `path`, optional `offset`, `length` (default 65536, at most 1 MiB), `encoding` (`auto` or `base64`) | `content`, `encoding` (`utf8` or `base64`), `offset`, `bytes`, `size`, `eof` |
| `write_file` | `path`, `content`, optional `encoding` (`utf8` or `base64`), `mode` (`truncate`, `create` or `append`) | `bytes`, `size` |
| `list_dir` | `path` | `entries` (each `name`, `type`, `size`, `mode`, `append_only`, `mtime`), `truncated` |
| `stat` | `path` | the same fields, and `path`, `atime`, `uid`, `gid`, `muid` |
| `remove` | `path`, optional `recursive` | `removed` |
| `reset` | none | `reset` |

Notes on `run`:

- The command line is run by `sh -c` in a new process group, with pipes
  for its three standard files. It starts in `cwd`, or `/tmp`.
- The namespace and the environment belong to the session, so `bind`,
  `mount` and variable assignments last until `reset`. The working
  directory does not.
- At the timeout the whole group is killed, and the call returns once the
  group is gone, with `timed_out` true and what was written so far.
- Output past the cap is read and dropped, so the command still runs to
  its end. Bytes that are not UTF-8 become U+FFFD; read binary data with
  `read_file`.
- A background program (`cmd &`) does not hold the call open. It keeps
  running until `reset` or the end of the session, but its output pipes
  close when the call returns. Stage 2 adds job tools for such programs.

`read_file` returns text when the bytes are valid UTF-8 and base64
otherwise. A read that would end inside a character stops before it, so
reading a text file in pieces never splits one.

## The sandbox

`mcpd` is two progs. The front end speaks the protocol and touches no file
a tool names. It starts a session prog with a copy of the namespace and
the environment and a new process group. The session prog builds the
namespace below, gives up the right to attach devices, and only then
serves tool calls. Every tool runs in that prog, so no tool can name
anything outside the session's namespace.

The default namespace:

| Name | What it is |
|---|---|
| `/` | A new `memfs` (in memory), holding only the names below. It becomes the root with `pctl(NEWNS)`, so the emu's root, and with it the host tree `#U`, is not in the namespace. |
| `/tmp` | A second `memfs`, private to the session and empty at the start. |
| `/bin`, `/lib` | The server's own `/bin` and `/lib`, mounted from `export -r`: a read-only export. |
| `/dev` | `null`, `random`, `time`, `user`, `sysname`, `pid`, `memory` and `drivers` from `#c`. `cons` is `#c/null`. The real console, `consctl`, `kprint` and `sysctl` are absent. |
| `/env`, `/prog`, `/fd` | `#e` (the session's own copy of the environment), `#p`, `#d`. |
| `/net`, `/chan`, `/n`, `/mnt` | Empty directories. |

After building it the session prog calls `pctl(NODEVS)`. From then on
neither it nor anything it starts can name a device with `#`, so
`bind '#U' /n`, `'#C'` (host commands) and `'#I'` (the network) all fail
with `mount/attach disallowed`.

What "read-only" means for `/bin` and `/lib`: the session reaches those
trees only through a 9P connection to an `export` started with the new
`RDONLY` flag (`export -r`; see `docs/abi.md`). That export answers
`permission denied` to an open for writing or truncation, and to create,
write, remove and wstat. The mount is also made without create permission,
so creating a file fails earlier with
`mounted directory forbids creation`. The check is in the kernel's export
code, not in the session, and the session has no other path to the files.

`reset` kills the session prog, its file servers, every command it ran
and their descendants, and starts a new session prog. The same happens
when the client closes the connection, and `mcpd` then exits.

Limits of this stage, stated plainly:

- **`/prog` shows every prog in the system**, and a session can kill any
  of them, including its own server. If it kills the session prog, the
  call in progress fails and a new session starts. If it kills the front
  end, the emu exits. On an emu that runs only `mcpd` nothing else is
  exposed. A kernel that serves several users needs `/prog` filtered per
  session before `mcpd` is offered there (stage 3).
- `NODEVS` still allows `#e`, `#s`, `#|`, `#M` and `#¤`, as in Inferno.
  `#s` is the system-wide `file2chan` registry and `#¤` is the factotum.
  A session can therefore reach a service another prog posted in `#s`,
  and can ask a factotum that holds a key to sign. A stock `emu mcpd` has
  neither.
- There are no memory or CPU limits yet (stage 5). The `memfs` trees grow
  with what is written to them, and a command can use a core until its
  timeout.
- A prog that leaves its process group and whose parent has exited is not
  found when the session ends. No command in `/bin` does this.
- Guest modules run with the engine's normal isolation; module signing
  (#33) is not applied.

## Granting more: `-n nsfile`

`mcpd -n nsfile` reads `nsfile` (a name in the emu's namespace) and, for
every session, runs each line as an sh command in the new namespace after
it is built and before `NODEVS`. Blank lines and lines starting with `#`
are skipped. At that point devices can still be attached, and the root is
writable, so a line can make a mount point and bind something there.
`bind`, `mount`, `mkdir` and `unmount` are the usual lines; the namespace(6)
verbs `fork`, `new` and `nodev` are not needed, because `mcpd` does them.
If a line fails, no session is built and every tool call reports the
error.

A workspace from the host. On emu, `#U` is the directory given to `-r`, so
the workspace must be inside it (here `root/usr/work`):

```
# root/lib/mcp/work.ns: used as  emu -r root mcpd -n /lib/mcp/work.ns
mkdir /n/work
bind -c '#U/usr/work' /n/work
```

The session can then read and write `/n/work`, and nothing else of the
host. Without `-c` it could change existing files but not create new
ones. To give it the network as well, add:

```
bind '#I' /net
```

and host commands (`os`), which removes the sandbox for all practical
purposes:

```
mkdir /cmd
bind '#C' /cmd
```

One known gap: `bind` of a mount point takes the directory underneath, not
what is mounted on it, so `bind /lib /n/lib` shows an empty directory.
Bind a directory inside the mount, or a device name.

## Later stages

From issue #70:

2. Jobs (`job_start`, `job_read`, `job_write`, `job_kill`), the namespace
   tools (`ns`, `bind`, `mount`, `unmount`), policy files with `nsbuild`,
   resources (`purg:///path`), and the audit log through `logfile`.
3. Remote sessions: `mcpd` behind `listen` on `/net` with the mount
   authentication, a host bridge from stdio to that connection, and the
   kernel over virtio-net.
4. `cpu`/`rcmd` as the transport, and `mcpfs`, which mounts other MCP
   servers' tools as files.
5. Fuel and memory limits per session.
