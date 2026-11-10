# mcpd: Purgatorio as an MCP server

`mcpd` lets an LLM client drive a sandboxed Purgatorio session through the
Model Context Protocol (MCP). It is an ordinary guest command
(`/bin/mcpd.wasm`). It speaks MCP on its standard input and output, so an
MCP client starts it by starting `emu`. The plan and its later stages are
in issue #70. This page describes stages 1 to 3.

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

- `-l logfile` appends the audit log to `logfile`, a name in the emu's
  own namespace (`-l /tmp/mcpd.log` is `root/tmp/mcpd.log` on the host).
  Without it nothing is logged. `mcpd` writes nothing else to its
  standard error, because on emu a guest's fd 2 is the console, which is
  the emu's standard output: the protocol stream. See "The audit log".
- `-n nsfile` grants the session more than the default, with a
  namespace(6) file. See below.
- `-t ms` is how long a file tool may take before it is abandoned
  (default 30000). See "Protocol".

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
  `io.modelcontextprotocol/protocolVersion`,
  `io.modelcontextprotocol/clientInfo` (an object with a `name` and a
  `version`) and `io.modelcontextprotocol/clientCapabilities` in
  `params._meta`. `mcpd` answers `server/discover`, `tools/list`,
  `tools/call` and the three resource methods, and every result has
  `resultType`. A request without the metadata, or with a `clientInfo`
  that lacks either string, gets `-32602`; another version gets `-32022`
  with the supported list. Clients of the earlier revisions need none of
  this.
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
- Each file tool (`read_file`, `write_file`, `list_dir`, `stat`,
  `remove`, `ns`, `bind`, `unmount` and the resource methods) runs in a
  worker prog of its own, in the session's namespace. One that takes
  longer than `-t` (30 seconds by default), such as a read of a pipe
  nobody writes, fails with `timed out after 30000 ms ...`; its worker is
  killed, and the session, its files and its jobs go on. Only if the
  session prog itself stops answering for 15 seconds past a call's limit
  is it replaced as by `reset`.
- A client may send requests without waiting for answers. The session
  begins them in order, one at a time: a `run` or `mount` holds up the
  calls after it until it ends. The one exception is a `job_read` with
  `wait_ms`: it waits on its own, and the calls after it go ahead. Replies
  are sent in the order of the requests, again except a waiting
  `job_read`, which is answered as soon as it can be. A `reset` waits for
  the calls before it, except waiting `job_read`s, which it ends.
- `notifications/cancelled` is accepted and ignored; use `timeout_ms`.
- Prompts, resource subscriptions and server-to-client requests are not
  implemented.

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
| `job_start` | `command` (an sh line) or `argv` (a program and its arguments, without sh), optional `stdin`, `cwd` | `id`, `pid` |
| `job_read` | `id`, optional `offset`, `stderr_offset`, `wait_ms` (default 0, at most 600000), `max_bytes` (default 65536 for each stream, at most 1 MiB) | `stdout`, `stderr`, `offset`, `next_offset`, `stderr_offset`, `next_stderr_offset`, `lost`, `stderr_lost`, `running`, `status`, `ok` |
| `job_write` | `id`, optional `data`, `encoding` (`utf8` or `base64`), `eof` | `queued`, `eof` |
| `job_kill` | `id` | `killed` |
| `job_list` | none | `jobs` (each `id`, `pid`, `command`, `running`, `status`, `stdout_bytes`, `stderr_bytes`) |
| `ns` | none | `ns`: the namespace as `ns` prints it |
| `bind` | `from`, `to`, optional `order` (`replace`, `before`, `after`), `create` | `ok` |
| `mount` | `source` (a dial string or a `{command}` block), `to`, optional `spec`, `order`, `create`, `authenticate` (default true), `timeout_ms` (default 30000) | `ok` |
| `unmount` | `to`, optional `from` | `ok` |
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
  close when the call returns. Use a job for such programs.

`read_file` returns text when the bytes are valid UTF-8 and base64
otherwise. A read that would end inside a character stops before it, so
reading a text file in pieces never splits one.

### Jobs

A job is a program the session keeps running between calls: a server, a
long build, an interactive program.

- `job_start` starts `sh -c command`, or `argv` directly (its first word
  is found as sh finds a command), in a new process group, in the
  session's namespace and environment, with pipes for its three standard
  files. It returns at once. At most 32 jobs run at a time.
- The session reads each job's stdout and stderr as they come, so a job
  never waits on output nobody has read. It keeps the last 1 MiB of each
  stream. Older output is dropped; a read that asks for it starts at the
  oldest byte kept and says how many bytes it missed in `lost` (or
  `stderr_lost`).
- Offsets count bytes from the start of a stream. Without `offset`, a
  `job_read` starts where the previous one ended, so repeated reads give
  new output only. A character cut at the end of what has come so far is
  left for the next read. Bytes that are not UTF-8 become U+FFFD.
- With `wait_ms`, `job_read` waits until there is output past the
  offsets, the job has exited, or the time is up. Other calls are
  answered meanwhile.
- `running` is true until the job has exited and its pipes are drained
  (programs it started may hold them open; the session stops reading 200
  ms after the exit). Then `status` is Inferno's exit status and `ok`
  says whether it is empty.
- `job_write` queues data for the job's input and returns at once; a
  helper prog writes it as the job reads, in order. `eof` closes the
  input after the data. Writing to a job that has exited, or after `eof`,
  fails.
- `job_kill` kills the job's process group. A killed sh reports an
  interrupted wait as its status (`fail:lost`) rather than `killed`.
- Exited jobs stay in `job_list` and can be read until more than 32 have
  exited; the oldest go first. `reset` and the end of the session kill
  every job.

### Namespace tools

`ns`, `bind`, `mount` and `unmount` change the session's namespace as the
commands of the same names do, with the same error messages, and the
change lasts until `reset`. `ns` reads the session prog's
`/prog/n/ns`, which is what `ns` run by `run` prints. `bind` and
`unmount` are done by the session prog itself. `mount` runs `/bin/mount`
with the flags the arguments give, so a `{command}` source (such as
`{memfs --serve}`) and authentication work as they do from sh.

The session gave up the right to attach devices before it served its
first call, so `bind '#C' /cmd` or `bind '#U' /n` fails with
`mount/attach disallowed`, whichever tool or command asks. A policy file
is the only way to grant a device.

### Resources

Files are also resources, with `purg://` URIs: `purg:///tmp/notes.txt` is
`/tmp/notes.txt` in the session's namespace. Bytes outside RFC 3986's
unreserved and sub-delimiter sets are percent-encoded. A URI always names
a file from the root, so it cannot name a device.

- `resources/list` lists the files under `/tmp`, at most 1000, in one
  page, with `name` (the file name) and `size`.
- `resources/templates/list` gives one template, `purg:///{+path}`, for
  any other file, such as `purg:///prog/1/status`.
- `resources/read` returns the whole file as `text` (`text/plain`) when it
  is valid UTF-8 and as a base64 `blob` (`application/octet-stream`)
  otherwise. A directory reads as its names, one per line, with `/` after
  each directory (`inode/directory`). A file longer than 1 MiB is refused
  with `-32603`; read it with `read_file`. A file that does not exist is
  `-32602` under 2026-07-28 and `-32002` under the earlier revisions,
  with the URI in `data`.
- Under 2026-07-28, list and read results carry `ttlMs: 0` and
  `cacheScope: "private"`; the template list is cacheable like
  `tools/list`.
- There are no subscriptions or change notifications.

### The audit log

With `-l logfile`, the front end appends one line per event:

```
1794334357 session start 3
1794334357 tool run ok 650ms {"command":"echo hi"}
1794334357 tool read_file error 1ms {"path":"/tmp/nope"}
1794334357 tool run invalid 0ms {"command":"x","timeout_ms":0}
1794334357 tool write_file ok 3ms {"path":"f","content":"yyyyyyyy...(5000 bytes)"}
1794334357 resource resources/read ok 8ms {"path":"/tmp/f"}
1794334357 session reset
1794334357 session end
```

The first field is seconds since 1970. A call's line is `tool` or
`resource`, its name, its result (`ok`; `error` when the tool failed or
the request got an error; `invalid` when the arguments did not fit the
schema), the time from receipt to reply, and the arguments as JSON with
every string longer than 64 characters cut to 48 and its length, and the
whole cut at 400 characters. Session lines are `session start <pid>`,
`session failed <why>`, `session reset` and `session end`.

The log is opened by the front end in the emu's namespace. The session
prog's own fd 2 is `/dev/null`, so the session cannot write to the log
through `/fd/2`, and the log's name is not in its namespace unless a
policy file binds it there.

## The sandbox

`mcpd` is three progs, and a worker per file tool. The front end speaks the protocol, writes the log
and touches no file a tool names. A relay copies the client's input to a
pipe, because the front end must wait on the client and the session at
once and the console cannot be waited on. The front end starts a session
prog with a copy of the namespace and the environment and a new process
group. The front end writes to the session only as much as the session
has acknowledged reading, so neither can block the other with a large
request and a large reply at once. The session prog builds the
namespace below, gives up the right to attach devices, and only then
serves tool calls. Every tool runs in that prog or in a prog it starts,
so no tool can name anything outside the session's namespace.

File tools run in a worker (`mcpd --file`) that the session prog starts
for each call, sharing its namespace and environment but none of its
file descriptors. The worker reads the request on its standard input
and writes the answer, followed by its length, as the last thing on its
standard output. A tool that writes `/fd/1` therefore writes into its
own worker's output, ahead of the real answer, and the session reads
only that answer and only its `result` or `error`. The session prog's
own pipes to the front end are not reachable from any tool or command,
so a session cannot forge a reply or claim a process group for the
front end to kill.

### The session's `/prog`

`/prog` is `#pdomain` (`docs/abi.md`), bound by the session prog before
it gives up devices. It shows the session prog, its file servers, and
every command, job and worker it starts, with their descendants, in
whatever process group. Nothing else exists there: `ls /prog/<pid>`,
`kill <pid>` and a write to `/prog/<pid>/ctl` for the front end, its
relay, another session or any other prog fail with `file does not
exist`. Under `NODEVS` the session cannot attach `#p` again to see the
whole table.

The default namespace:

| Name | What it is |
|---|---|
| `/` | A new `memfs` (in memory), holding only the names below. It becomes the root with `pctl(NEWNS)`, so the emu's root, and with it the host tree `#U`, is not in the namespace. |
| `/tmp` | A second `memfs`, private to the session and empty at the start. |
| `/bin`, `/lib` | The server's own `/bin` and `/lib`, mounted from `export -r`: a read-only export. |
| `/dev` | `null`, `random`, `time`, `user`, `sysname`, `pid`, `memory` and `drivers` from `#c`. `cons` is `#c/null`. The real console, `consctl`, `kprint` and `sysctl` are absent. |
| `/env`, `/prog`, `/fd` | `#e` (the session's own copy of the environment), `#pdomain`, `#d`. `/prog` shows only the session's own progs; see below. |
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

- A session can still kill its own progs, the session prog and its file
  servers among them. If it kills the session prog, the call in progress
  fails and a new session starts.
- `NODEVS` still allows `#e`, `#s`, `#|`, `#M` and `#¤`, as in Inferno.
  `#s` is the system-wide `file2chan` registry and `#¤` is the factotum.
  A session can therefore reach a service another prog posted in `#s`,
  and can ask a factotum that holds a key to sign. A stock `emu mcpd` has
  neither.
- There are no memory or CPU limits yet (stage 5). The `memfs` trees grow
  with what is written to them, and a command can use a core until its
  timeout.
- A prog that leaves its process group and whose parent has exited is not
  found when the session ends. No command in `/bin` does this. (Such a
  prog is still in the session's domain, so the session sees it; the
  front end's own view does not say which domain a prog is in.)
- The session prog itself still does a little that names files: it
  changes to `/tmp` (or `cwd`) before it starts a command or job, and
  the spawn looks up the command. A session that mounts a server that
  never answers on `/tmp` can stall those, and then the front end
  replaces the session.
- Guest modules run with the engine's normal isolation; module signing
  (#33) is not applied.

## Granting more: `-n nsfile`

`mcpd -n nsfile` reads `nsfile` (a name in the emu's namespace), a
namespace file in the format of namespace(6), and for every session has
`nsbuild` apply it to the new namespace after it is built and before
`NODEVS`. At that point devices can still be attached. If the file cannot
be applied, no session is built and every tool call reports nsbuild's
error.

- The verbs are nsbuild's: `bind`, `mount`, `import`, `unmount`, `cd` and
  `.`, with their flags (`-a`, `-b`, `-c`, `-r`, `-i`, and `-A` for
  `mount`). Lines starting with `#` are comments. `$user`, `$sysname` and
  `$home` (`/tmp`) are set in the session's environment and expand in
  words.
- nsbuild runs in the session's namespace and reads the file from its
  standard input, so names in the file, including a `.` include, are
  names in the session's namespace, where `#U` is still the host tree.
- namespace(6) cannot make a directory, and the session's root holds only
  a few. So `mcpd` first makes every missing mount point named as the
  target of a `bind`, `mount` or `import` line in the file (not in
  included files, and not when the target names a variable).
- `fork`, `new`, `nodev` and `clear` change only nsbuild's own namespace
  and have no effect on the session; `mcpd` does the equivalent itself.

Stage 1 ran each line as an sh command. Such a file still works if its
lines are binds and mounts; drop any `mkdir` line, which nsbuild rejects
and which is no longer needed.

A workspace from the host. On emu, `#U` is the directory given to `-r`, so
the workspace must be inside it (here `root/usr/work`):

```
# root/lib/mcp/work.ns: used as  emu -r root mcpd -n /lib/mcp/work.ns
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
bind '#C' /cmd
```

A mount point can be bound whole: `bind /lib /n/lib` shows the read-only
`/lib`, since #97.

## A remote server: `rcmd host mcpd`

`rcmd` carries a command's standard input and output to another
Purgatorio over an authenticated connection (`docs/rcmd.md`), so the same
stdio server runs remotely with no bridge of its own. On the system that
is to hold the sessions:

```sh
listen 'tcp!*!rstyx' /bin/auxi/rstyxd.wasm &
```

and as the client's command, on an emu that holds a key that system
trusts:

```sh
emu -r /path/to/root rcmd tcp!host!6668 mcpd
```

Each connection is one `mcpd` with its own sessions. The caller's
namespace is mounted on `/n/client` in `rstyxd`'s copy of the namespace;
a session does not see it unless its policy binds it. The connection is
authenticated but not encrypted yet. `crates/purg-emu/tests/rcmd.rs`
drives a session this way on loopback.

## Later stages

From issue #70:

3. Remote sessions: `mcpd` behind `listen` on `/net` with the mount
   authentication, a host bridge from stdio to that connection, and the
   kernel over virtio-net. Per-session `/prog` filtering belongs here.
4. `mcpfs`, which mounts other MCP servers' tools as files. (`rcmd` as
   the transport is done, above.)
5. Fuel and memory limits per session.

