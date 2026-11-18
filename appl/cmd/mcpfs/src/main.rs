//! mcpfs [-m mntpt] command [arg ...]: mount the tools of a Model Context
//! Protocol server as files on `mntpt` (default `/n/mcp`).
//!
//! `command` is started with pipes for its standard input and output and
//! must speak MCP's stdio transport on them: `mcpd` for a local sandbox,
//! `rcmd host mcpd` for a remote one, or any other guest program that
//! does. mcpfs asks `server/discover` under revision 2026-07-28 and, if
//! the server speaks it, puts the client's `_meta` on every request;
//! otherwise it makes the `initialize` handshake of 2025-11-25. Then it
//! lists the tools, and serves:
//!
//! ```text
//! mntpt/server             the server's server/discover (or initialize)
//!                          result, as JSON
//! mntpt/tools/NAME/schema  the tool's definition from tools/list, as JSON
//! mntpt/tools/NAME/call    write the arguments (a JSON object), then read
//!                          the result
//! mntpt/resources/list     the server's resources/list result, as JSON
//! mntpt/resources/read     write a resource's URI, then read its contents
//! mntpt/prompts/list       the server's prompts/list result, as JSON
//! mntpt/prompts/get        write the parameters ({"name": ...,
//!                          "arguments": {...}}), then read the prompt
//! mntpt/log                the server's log and progress notifications,
//!                          one JSON line each, as they came
//! ```
//!
//! Each open of `call` is one call. What is written to it is the
//! arguments; nothing written is `{}`. The first read makes the call and
//! returns the CallToolResult as JSON (`content`, `structuredContent`,
//! `isError`), followed by a newline. A read that starts where the writes
//! ended, or at 0, gets its start, so `{echo $args >[1=0]; cat} <> call`
//! works as well as a seek to 0. A write after a read starts a new call
//! on the same open file. A request the server answers with a JSON-RPC
//! error fails the read with the error's message.
//!
//! `resources/read` works the same way with a URI for the arguments
//! (`purg:///tmp/f` for mcpd): the read returns the contents themselves,
//! text as it is and a blob decoded, each content after the other.
//!
//! Calls do not hold up the mount (`serve`): a read that needs the
//! server waits for its answer while other requests are served. A
//! changed tool list is fetched again. Log messages
//! (`notifications/message`) and progress (`notifications/progress`) are
//! kept in `log`, the last [`MAX_LOG`] bytes of them.

#[allow(dead_code)]
#[path = "../../mcpd/src/json.rs"]
mod json;
mod serve;

use purgatorio::fd::{AsRawFd, FromRawFd};
use purgatorio::fs::File;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};

use json::Value;
use purgatorio::abi::mount::MREPL;
use purgatorio::abi::{error, mode, pctl};
use purgatorio::arg::Arg;
use purgatorio::styx::{IOHDRSZ, StyxFs};
use purgatorio::{Context, DMDIR, Dir, Error, Qid};

const USAGE: &str = "mcpfs [-m mntpt] [-t ms] command [arg ...]";
const SELF: &str = "/bin/mcpfs.wasm";
/// The revision mcpfs asks for. `mcpd` and most servers answer it.
/// The revision spoken to a server without `server/discover`.
const PROTOCOL: &str = "2025-11-25";
/// The stateless revision, spoken to a server that answers
/// `server/discover` with it among its versions.
const MODERN: &str = "2026-07-28";
/// How long `server/discover` waits for an answer before the server is
/// taken to be of the earlier revision. A server answers an unknown
/// method with an error at once, so only one that says nothing waits
/// this long; and it must outlast a server that is slow to start (5 s
/// was too short for `mcpd` on a busy machine, whose `server` file then
/// showed the earlier revision).
const PROBE_MS: i32 = 30_000;
/// A longer line from the server is refused.
const MAX_LINE: usize = 16 << 20;
/// Pages of `tools/list` followed before giving up.
const MAX_PAGES: usize = 100;

const QTDIR: u8 = 0x80;
const QTFILE: u8 = 0;

const ROOT: u64 = 0;
const SERVER: u64 = 1;
const TOOLS: u64 = 2;
const RESOURCES: u64 = 3;
const RLIST: u64 = 4;
const RREAD: u64 = 5;
const PROMPTS: u64 = 6;
const PLIST: u64 = 7;
const PGET: u64 = 8;
const LOG: u64 = 9;

/// How much of the server's log `log` keeps: older lines go first.
const MAX_LOG: usize = 64 * 1024;
/// Tool `i` is `TOOL + 4i` (its directory), `+ 1` (schema), `+ 2` (call).
const TOOL: u64 = 16;

/// The client end of an MCP stdio connection.
struct Client {
    to: File,
    from: BufReader<File>,
    next: i64,
    /// Speaking 2026-07-28: every request carries the client's `_meta`.
    modern: bool,
}

impl Client {
    fn send(&mut self, v: &Value) -> Result<(), String> {
        let mut line = v.encode();
        line.push('\n');
        self.to
            .write_all(line.as_bytes())
            .map_err(|e| format!("mcp server: {}", Error::from(e)))
    }

    /// `params` with the `_meta` every 2026-07-28 request carries: the
    /// revision, the client and its (empty) capabilities, and a log level
    /// so that log messages still come.
    fn meta(&self, params: Value) -> Value {
        if !self.modern {
            return params;
        }
        with_meta(params)
    }

    /// Sends a request and returns its result, or the error's message.
    /// Requests from the server meanwhile are answered (`ping`) or refused;
    /// notifications are dropped.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        let params = self.meta(params);
        self.send(&json::obj([
            ("jsonrpc", "2.0".into()),
            ("id", id.into()),
            ("method", method.into()),
            ("params", params),
        ]))?;
        loop {
            let mut line = Vec::new();
            let n = (&mut self.from)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut line)
                .map_err(|e| format!("mcp server: {}", Error::from(e)))?;
            if n == 0 {
                return Err(String::from("mcp server: hungup"));
            }
            if line.len() > MAX_LINE {
                return Err(String::from("mcp server: message too long"));
            }
            let Ok(v) = json::parse(String::from_utf8_lossy(&line).trim()) else {
                continue;
            };
            if v.get("method").is_some() {
                if let Some(rid) = v.get("id") {
                    self.answer(&v, rid.clone())?;
                }
                continue;
            }
            if v.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            if let Some(e) = v.get("error") {
                let msg = e.get("message").and_then(Value::as_str).unwrap_or("error");
                return Err(String::from(msg));
            }
            let r = v
                .get("result")
                .cloned()
                .ok_or_else(|| String::from("mcp server: reply without a result"))?;
            return complete(r);
        }
    }

    /// Answers a request the server sent: `ping`, and nothing else.
    fn answer(&mut self, req: &Value, id: Value) -> Result<(), String> {
        let reply = if req.get("method").and_then(Value::as_str) == Some("ping") {
            json::obj([
                ("jsonrpc", "2.0".into()),
                ("id", id),
                ("result", Value::Object(Vec::new())),
            ])
        } else {
            json::obj([
                ("jsonrpc", "2.0".into()),
                ("id", id),
                (
                    "error",
                    json::obj([
                        ("code", Value::Int(-32601)),
                        ("message", "method not found".into()),
                    ]),
                ),
            ])
        };
        self.send(&reply)
    }

    fn notify(&mut self, method: &str) -> Result<(), String> {
        self.send(&json::obj([
            ("jsonrpc", "2.0".into()),
            ("method", method.into()),
        ]))
    }
}

/// `params` with the client's 2026-07-28 `_meta` added to any it has.
fn with_meta(params: Value) -> Value {
    let meta = [
        (
            "io.modelcontextprotocol/protocolVersion",
            Value::from(MODERN),
        ),
        (
            "io.modelcontextprotocol/clientInfo",
            json::obj([("name", "mcpfs".into()), ("version", "0".into())]),
        ),
        (
            "io.modelcontextprotocol/clientCapabilities",
            Value::Object(Vec::new()),
        ),
        ("io.modelcontextprotocol/logLevel", Value::from("info")),
    ];
    let mut fields = match params {
        Value::Object(f) => f,
        _ => Vec::new(),
    };
    let mut m = match fields.iter().position(|(k, _)| k == "_meta") {
        Some(i) => match fields.remove(i).1 {
            Value::Object(m) => m,
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    for (k, v) in meta {
        if !m.iter().any(|(n, _)| n == k) {
            m.push((String::from(k), v));
        }
    }
    fields.push((String::from("_meta"), Value::Object(m)));
    Value::Object(fields)
}

/// A result, unless it asks for input (a 2026-07-28 `input_required`
/// result), which mcpfs cannot give.
fn complete(r: Value) -> Result<Value, String> {
    match r.get("resultType").and_then(Value::as_str) {
        Some("input_required") => Err(String::from(
            "mcp server: the request needs input, which mcpfs cannot give",
        )),
        _ => Ok(r),
    }
}

struct Tool {
    name: String,
    schema: Vec<u8>,
    /// No longer listed by the server (`notifications/tools/list_changed`):
    /// it keeps its place, so other tools keep their qids, but cannot be
    /// walked to or listed.
    gone: bool,
}

/// Adds the tools of one `tools/list` page to `tools`, and returns the
/// cursor of the next page, if there is one.
fn add_page(tools: &mut Vec<Tool>, page: &Value) -> Option<Value> {
    if let Some(Value::Array(list)) = page.get("tools") {
        for t in list {
            let Some(name) = t.get("name").and_then(Value::as_str) else {
                continue;
            };
            // A name that cannot be a file name is left out.
            if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                continue;
            }
            if tools.iter().any(|x: &Tool| x.name == name) {
                continue;
            }
            let mut schema = t.encode().into_bytes();
            schema.push(b'\n');
            tools.push(Tool {
                name: String::from(name),
                schema,
                gone: false,
            });
        }
    }
    match page.get("nextCursor") {
        Some(c @ Value::Str(_)) => Some(c.clone()),
        _ => None,
    }
}

/// A `tools/list` made again after the server said its tools changed.
struct Refresh {
    /// The id of the page request out.
    id: i64,
    pages: usize,
    seen: Vec<Tool>,
}

/// One open of a `call` file.
#[derive(Clone, Default)]
struct Call {
    args: Vec<u8>,
    /// The offset after the last write.
    end: u64,
    result: Option<Result<Vec<u8>, String>>,
    /// The id of the `tools/call` sent for it and not yet answered.
    inflight: Option<i64>,
}

#[derive(Clone)]
struct Fid {
    path: u64,
    /// The packed listing a directory read returns, made at offset 0.
    snap: Vec<u8>,
    call: Call,
}

struct McpFs {
    client: Client,
    user: String,
    server: Vec<u8>,
    tools: Vec<Tool>,
    refresh: Option<Refresh>,
    /// The server's log and progress notifications, one JSON line each.
    log: Vec<u8>,
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
}

fn qid(path: u64) -> Qid {
    let dir = path == ROOT
        || path == TOOLS
        || path == RESOURCES
        || path == PROMPTS
        || (path >= TOOL && (path - TOOL).is_multiple_of(4));
    Qid {
        path,
        vers: 0,
        qtype: if dir { QTDIR } else { QTFILE },
    }
}

/// The bytes of `data` from `offset`, at most `count`.
fn slice(data: &[u8], offset: u64, count: u32) -> Vec<u8> {
    let start = (offset as usize).min(data.len());
    let end = start.saturating_add(count as usize).min(data.len());
    data[start..end].to_vec()
}

/// Whole directory entries of `snap` from `offset`, at most `count` bytes.
fn dir_slice(snap: &[u8], offset: u64, count: u32) -> Vec<u8> {
    let start = (offset as usize).min(snap.len());
    let end = start.saturating_add(count as usize).min(snap.len());
    let mut n = start;
    while n + 2 <= end {
        let len = u16::from_le_bytes([snap[n], snap[n + 1]]) as usize + 2;
        if n + len > end {
            break;
        }
        n += len;
    }
    snap[start..n].to_vec()
}

impl McpFs {
    /// The tool a path belongs to, and which of its files it is (0 the
    /// directory, 1 schema, 2 call).
    fn tool_of(&self, path: u64) -> Option<(usize, u64)> {
        let i = path.checked_sub(TOOL)?;
        let t = (i / 4) as usize;
        (t < self.tools.len() && !self.tools[t].gone).then_some((t, i % 4))
    }

    fn parent(&self, path: u64) -> u64 {
        match self.tool_of(path) {
            Some((_, 0)) => TOOLS,
            Some((t, _)) => TOOL + 4 * t as u64,
            None if path == RLIST || path == RREAD => RESOURCES,
            None if path == PLIST || path == PGET => PROMPTS,
            None => ROOT,
        }
    }

    /// Whether a read of `path` asks the server: a tool's `call`, or a
    /// file of `resources` or `prompts`.
    fn asks(&self, path: u64) -> bool {
        matches!(self.tool_of(path), Some((_, 2))) || [RLIST, RREAD, PLIST, PGET].contains(&path)
    }

    /// Whether `path` takes writes: the arguments of a request.
    fn takes_writes(&self, path: u64) -> bool {
        matches!(self.tool_of(path), Some((_, 2))) || path == RREAD || path == PGET
    }

    fn child(&self, path: u64, name: &str) -> Option<u64> {
        match (path, self.tool_of(path)) {
            (ROOT, _) => match name {
                "server" => Some(SERVER),
                "tools" => Some(TOOLS),
                "resources" => Some(RESOURCES),
                "prompts" => Some(PROMPTS),
                "log" => Some(LOG),
                _ => None,
            },
            (PROMPTS, _) => match name {
                "list" => Some(PLIST),
                "get" => Some(PGET),
                _ => None,
            },
            (RESOURCES, _) => match name {
                "list" => Some(RLIST),
                "read" => Some(RREAD),
                _ => None,
            },
            (TOOLS, _) => self
                .tools
                .iter()
                .position(|t| t.name == name && !t.gone)
                .map(|t| TOOL + 4 * t as u64),
            (_, Some((t, 0))) => match name {
                "schema" => Some(TOOL + 4 * t as u64 + 1),
                "call" => Some(TOOL + 4 * t as u64 + 2),
                _ => None,
            },
            _ => None,
        }
    }

    fn children(&self, path: u64) -> Vec<u64> {
        match (path, self.tool_of(path)) {
            (ROOT, _) => vec![SERVER, TOOLS, RESOURCES, PROMPTS, LOG],
            (PROMPTS, _) => vec![PLIST, PGET],
            (RESOURCES, _) => vec![RLIST, RREAD],
            (TOOLS, _) => (0..self.tools.len())
                .filter(|&t| !self.tools[t].gone)
                .map(|t| TOOL + 4 * t as u64)
                .collect(),
            (_, Some((t, 0))) => vec![TOOL + 4 * t as u64 + 1, TOOL + 4 * t as u64 + 2],
            _ => Vec::new(),
        }
    }

    fn dir(&self, path: u64) -> Dir {
        let (name, mode, length) = match (path, self.tool_of(path)) {
            (ROOT, _) => (String::from("/"), DMDIR | 0o555, 0),
            (SERVER, _) => (String::from("server"), 0o444, self.server.len()),
            (TOOLS, _) => (String::from("tools"), DMDIR | 0o555, 0),
            (RESOURCES, _) => (String::from("resources"), DMDIR | 0o555, 0),
            (RLIST, _) => (String::from("list"), 0o444, 0),
            (RREAD, _) => (String::from("read"), 0o666, 0),
            (PROMPTS, _) => (String::from("prompts"), DMDIR | 0o555, 0),
            (PLIST, _) => (String::from("list"), 0o444, 0),
            (PGET, _) => (String::from("get"), 0o666, 0),
            (LOG, _) => (String::from("log"), 0o444, self.log.len()),
            (_, Some((t, 0))) => (self.tools[t].name.clone(), DMDIR | 0o555, 0),
            (_, Some((t, 1))) => (String::from("schema"), 0o444, self.tools[t].schema.len()),
            _ => (String::from("call"), 0o666, 0),
        };
        Dir {
            qid: qid(path),
            mode,
            length: length as u64,
            name,
            uid: self.user.clone(),
            gid: self.user.clone(),
            muid: self.user.clone(),
            ..Dir::default()
        }
    }

    fn listing(&self, path: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for c in self.children(path) {
            let _ = self.dir(c).pack_into(&mut out);
        }
        out
    }

    fn fid(&self, fid: u32) -> Result<&Fid, String> {
        self.fids
            .get(&fid)
            .ok_or_else(|| String::from("unknown fid"))
    }

    /// Whether a read of `fid` must wait for a call: it is a `call` file
    /// whose result is not yet known.
    fn needs_call(&self, fid: u32) -> bool {
        self.fids
            .get(&fid)
            .is_some_and(|f| self.asks(f.path) && f.call.result.is_none())
    }

    /// The call already sent for `fid`, if one is.
    fn in_flight(&self, fid: u32) -> Option<i64> {
        self.fids.get(&fid).and_then(|f| f.call.inflight)
    }

    /// The request a read of `path` with `args` written makes: its method
    /// and parameters. Arguments that do not fit are the read's error.
    fn query(&self, path: u64, args: &[u8]) -> Result<(&'static str, Value), String> {
        match (path, self.tool_of(path)) {
            (_, Some((tool, 2))) => {
                let arguments = arguments(args)?;
                let name = self.tools[tool].name.clone();
                Ok((
                    "tools/call",
                    json::obj([("name", name.into()), ("arguments", arguments)]),
                ))
            }
            (RLIST, _) => Ok(("resources/list", Value::Object(Vec::new()))),
            (RREAD, _) => {
                let uri = String::from_utf8_lossy(args).trim().to_string();
                if uri.is_empty() {
                    return Err(String::from("no resource URI written"));
                }
                Ok(("resources/read", json::obj([("uri", uri.into())])))
            }
            (PLIST, _) => Ok(("prompts/list", Value::Object(Vec::new()))),
            (PGET, _) => Ok(("prompts/get", arguments(args)?)),
            _ => Err(String::from(error::EPERM)),
        }
    }

    /// What a read of `path` returns for the server's `result`: the JSON,
    /// or for `resources/read` the contents themselves.
    fn render(&self, path: u64, result: &Value) -> Vec<u8> {
        if path == RREAD {
            let mut out = Vec::new();
            if let Some(Value::Array(contents)) = result.get("contents") {
                for c in contents {
                    if let Some(t) = c.get("text").and_then(Value::as_str) {
                        out.extend_from_slice(t.as_bytes());
                    } else if let Some(b) = c.get("blob").and_then(Value::as_str) {
                        out.extend(json::base64_decode(b).unwrap_or_default());
                    }
                }
            }
            return out;
        }
        let mut out = result.encode().into_bytes();
        out.push(b'\n');
        out
    }

    /// Sends the request a read of `fid` makes and returns its id,
    /// without waiting for the answer. Arguments that do not fit are the
    /// read's error, kept as the call's result.
    fn start_call(&mut self, fid: u32) -> Result<i64, String> {
        let f = self.fid(fid)?.clone();
        let (method, params) = match self.query(f.path, &f.call.args) {
            Ok(q) => q,
            Err(e) => {
                if let Some(f) = self.fids.get_mut(&fid) {
                    f.call.result = Some(Err(e.clone()));
                }
                return Err(e);
            }
        };
        let id = self.send_request(method, params)?;
        if let Some(f) = self.fids.get_mut(&fid) {
            f.call.inflight = Some(id);
        }
        Ok(id)
    }

    /// The path `fid` is open on, for rendering its answer.
    fn path_of(&self, fid: u32) -> Option<u64> {
        self.fids.get(&fid).map(|f| f.path)
    }

    /// Sends a request without waiting for its answer, and returns its id.
    fn send_request(&mut self, method: &str, params: Value) -> Result<i64, String> {
        let id = self.client.next;
        self.client.next += 1;
        let params = self.client.meta(params);
        self.client.send(&json::obj([
            ("jsonrpc", "2.0".into()),
            ("id", id.into()),
            ("method", method.into()),
            ("params", params),
        ]))?;
        Ok(id)
    }

    /// The server's tools changed: asks for the list again, unless that is
    /// already under way.
    fn tools_changed(&mut self) -> Result<(), String> {
        if self.refresh.is_none() {
            let id = self.send_request("tools/list", Value::Object(Vec::new()))?;
            self.refresh = Some(Refresh {
                id,
                pages: 1,
                seen: Vec::new(),
            });
        }
        Ok(())
    }

    /// A reply to the refresh's request `id`, if it is one: adds its
    /// page and asks for the next, or, at the last page, makes the new
    /// list the tree's. True if the reply was the refresh's.
    fn refreshed(&mut self, id: i64, result: Option<&Value>) -> Result<bool, String> {
        let Some(mut r) = self.refresh.take_if(|r| r.id == id) else {
            return Ok(false);
        };
        // A failed list leaves the tools as they were.
        let Some(page) = result else {
            return Ok(true);
        };
        if let Some(c) = add_page(&mut r.seen, page)
            && r.pages < MAX_PAGES
        {
            r.id = self.send_request("tools/list", json::obj([("cursor", c)]))?;
            r.pages += 1;
            self.refresh = Some(r);
            return Ok(true);
        }
        for t in &mut self.tools {
            match r.seen.iter().position(|n| n.name == t.name) {
                Some(i) => {
                    t.schema = r.seen.remove(i).schema;
                    t.gone = false;
                }
                None => t.gone = true,
            }
        }
        self.tools.append(&mut r.seen);
        Ok(true)
    }

    /// Keeps a log or progress notification in `log`, dropping whole old
    /// lines past [`MAX_LOG`].
    fn note(&mut self, v: &Value) {
        let mut line = v.encode().into_bytes();
        line.truncate(MAX_LOG - 1);
        line.push(b'\n');
        self.log.extend_from_slice(&line);
        if self.log.len() > MAX_LOG {
            let over = self.log.len() - MAX_LOG;
            let cut = self.log[over..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(self.log.len(), |i| over + i + 1);
            self.log.drain(..cut);
        }
    }

    /// Tells the server to stop call `id` (`notifications/cancelled`).
    fn cancel(&mut self, id: i64, reason: &str) {
        let _ = self.client.send(&json::obj([
            ("jsonrpc", "2.0".into()),
            ("method", "notifications/cancelled".into()),
            (
                "params",
                json::obj([("requestId", id.into()), ("reason", reason.into())]),
            ),
        ]));
    }

    /// Forgets call `id` on `fid`, flushed before it was answered: a
    /// later read sends the call again.
    fn forget(&mut self, fid: u32, id: i64) {
        if let Some(f) = self.fids.get_mut(&fid)
            && f.call.inflight == Some(id)
        {
            f.call.inflight = None;
        }
    }

    /// Keeps call `id`'s result on `fid`, if the fid still waits for it,
    /// and returns where the writes to it ended.
    fn settle(&mut self, fid: u32, id: i64, result: Result<Vec<u8>, String>) -> u64 {
        match self.fids.get_mut(&fid) {
            Some(f) if f.call.inflight == Some(id) => {
                f.call.inflight = None;
                f.call.result = Some(result);
                f.call.end
            }
            Some(f) => f.call.end,
            None => 0,
        }
    }

    /// Makes the request a read of `path` with `args` written makes, and
    /// waits for it.
    fn call(&mut self, path: u64, args: &[u8]) -> Result<Vec<u8>, String> {
        let (method, params) = self.query(path, args)?;
        let result = self.client.request(method, params)?;
        Ok(self.render(path, &result))
    }
}

impl StyxFs for McpFs {
    fn set_msize(&mut self, msize: u32) {
        self.iounit = msize.saturating_sub(IOHDRSZ).max(1);
    }

    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, _aname: &str) -> Result<Qid, String> {
        if self.fids.contains_key(&fid) {
            return Err(String::from("fid in use"));
        }
        self.fids.insert(
            fid,
            Fid {
                path: ROOT,
                snap: Vec::new(),
                call: Call::default(),
            },
        );
        Ok(qid(ROOT))
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        if newfid != fid && self.fids.contains_key(&newfid) {
            return Err(String::from("fid in use"));
        }
        let mut at = self.fid(fid)?.path;
        let mut qids = Vec::new();
        for name in names {
            if qid(at).qtype != QTDIR {
                if qids.is_empty() {
                    return Err(String::from(error::ENOTDIR));
                }
                return Ok(qids);
            }
            let next = if *name == ".." {
                Some(self.parent(at))
            } else {
                self.child(at, name)
            };
            match next {
                Some(p) => {
                    at = p;
                    qids.push(qid(p));
                }
                None if qids.is_empty() => return Err(String::from(error::ENONEXIST)),
                None => return Ok(qids),
            }
        }
        self.fids.insert(
            newfid,
            Fid {
                path: at,
                snap: Vec::new(),
                call: Call::default(),
            },
        );
        Ok(qids)
    }

    fn open(&mut self, fid: u32, omode: u8) -> Result<(Qid, u32), String> {
        let path = self.fid(fid)?.path;
        let access = omode & mode::ACCESS;
        let ok = if self.takes_writes(path) {
            access != mode::OEXEC
        } else {
            access == mode::OREAD && omode & mode::OTRUNC == 0
        };
        if !ok || omode & mode::ORCLOSE != 0 {
            return Err(String::from(error::EPERM));
        }
        Ok((qid(path), self.iounit))
    }

    fn create(&mut self, _fid: u32, _name: &str, _perm: u32, _m: u8) -> Result<(Qid, u32), String> {
        Err(String::from(error::EPERM))
    }

    fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>, String> {
        let f = self.fid(fid)?.clone();
        let path = f.path;
        if qid(path).qtype == QTDIR {
            if offset == 0 {
                let snap = self.listing(path);
                if let Some(f) = self.fids.get_mut(&fid) {
                    f.snap = snap;
                }
            }
            return Ok(dir_slice(&self.fid(fid)?.snap, offset, count));
        }
        match (path, self.tool_of(path)) {
            (SERVER, _) => Ok(slice(&self.server, offset, count)),
            (LOG, _) => Ok(slice(&self.log, offset, count)),
            (_, Some((t, 1))) => Ok(slice(&self.tools[t].schema, offset, count)),
            _ if self.asks(path) => {
                let result = match f.call.result {
                    Some(r) => r,
                    None => {
                        let r = self.call(path, &f.call.args);
                        if let Some(f) = self.fids.get_mut(&fid) {
                            f.call.result = Some(r.clone());
                        }
                        r
                    }
                };
                let data = result?;
                let at = if offset >= f.call.end {
                    offset - f.call.end
                } else {
                    offset
                };
                Ok(slice(&data, at, count))
            }
            _ => Err(String::from(error::EPERM)),
        }
    }

    fn write(&mut self, fid: u32, offset: u64, data: &[u8]) -> Result<u32, String> {
        let path = self.fid(fid)?.path;
        if !self.takes_writes(path) {
            return Err(String::from(error::EPERM));
        }
        let f = self
            .fids
            .get_mut(&fid)
            .ok_or_else(|| String::from("unknown fid"))?;
        if f.call.result.is_some() {
            f.call = Call::default();
        }
        f.call.args.extend_from_slice(data);
        f.call.end = offset + data.len() as u64;
        Ok(data.len() as u32)
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        self.fids
            .remove(&fid)
            .map(drop)
            .ok_or_else(|| String::from("unknown fid"))
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        self.clunk(fid)?;
        Err(String::from(error::EPERM))
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let path = self.fid(fid)?.path;
        self.dir(path).pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, _fid: u32, _stat: &[u8]) -> Result<(), String> {
        Err(String::from(error::EPERM))
    }
}

/// The arguments written to a `call` file, as the JSON object
/// `tools/call` sends: nothing is `{}`.
fn arguments(args: &[u8]) -> Result<Value, String> {
    let text = String::from_utf8_lossy(args);
    let text = text.trim();
    if text.is_empty() {
        return Ok(Value::Object(Vec::new()));
    }
    match json::parse(text) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err(String::from("the arguments must be a JSON object")),
        Err(e) => Err(format!("the arguments are not JSON: {e}")),
    }
}

/// Starts `argv` with pipes for its standard input and output, and makes
/// the handshake and the tool list.
/// Asks the server for `server/discover` under 2026-07-28. The result, if
/// the server speaks that revision; `None` if it answers with another
/// error, or not within [`PROBE_MS`], as a server of the earlier revision
/// would. A 2026-07-28 server that does not take it is an error.
fn discover(client: &mut Client) -> Result<Option<Value>, String> {
    let id = client.next;
    client.next += 1;
    client.send(&json::obj([
        ("jsonrpc", "2.0".into()),
        ("id", id.into()),
        ("method", "server/discover".into()),
        ("params", with_meta(Value::Object(Vec::new()))),
    ]))?;
    let fd = client.from.get_ref().as_raw_fd();
    loop {
        if !client.from.buffer().contains(&b'\n') {
            match purgatorio::ready(&[purgatorio::Ready::Read(fd)], PROBE_MS) {
                Ok(Some(_)) => {}
                _ => return Ok(None),
            }
        }
        let mut line = Vec::new();
        let n = (&mut client.from)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("mcp server: {}", Error::from(e)))?;
        if n == 0 {
            return Err(String::from("mcp server: hungup"));
        }
        let Ok(v) = json::parse(String::from_utf8_lossy(&line).trim()) else {
            continue;
        };
        if v.get("method").is_some() || v.get("id").and_then(Value::as_i64) != Some(id) {
            continue;
        }
        if let Some(e) = v.get("error") {
            if e.get("code").and_then(Value::as_i64) == Some(-32022) {
                let supported = e
                    .get("data")
                    .and_then(|d| d.get("supported"))
                    .map(Value::encode)
                    .unwrap_or_default();
                return Err(format!(
                    "mcp server: does not speak {MODERN}: supports {supported}"
                ));
            }
            return Ok(None);
        }
        let Some(r) = v.get("result") else {
            return Ok(None);
        };
        let modern = matches!(r.get("supportedVersions"), Some(Value::Array(vs))
            if vs.iter().any(|v| v.as_str() == Some(MODERN)));
        return Ok(modern.then(|| r.clone()));
    }
}

fn connect(argv: &[String]) -> Result<McpFs, String> {
    let pipe = || purgatorio::pipe().map_err(|e| format!("pipe: {e}"));
    let [req_r, req_w] = pipe()?;
    let [resp_r, resp_w] = pipe()?;
    purgatorio::spawn(
        &argv[0],
        argv,
        pctl::NEWFD,
        &[(req_r.as_raw_fd(), 0), (resp_w.as_raw_fd(), 1), (2, 2)],
    )
    .map_err(|e| format!("{}: {e}", argv[0]))?;
    drop((req_r, resp_w));
    let mut client = Client {
        to: File::from(req_w),
        from: BufReader::new(File::from(resp_r)),
        next: 1,
        modern: false,
    };
    // 2026-07-28 first, as its stdio rules say: a server that answers
    // server/discover with that revision has no handshake; any other
    // answer, or none in time, is a server of the earlier revision.
    let init = match discover(&mut client)? {
        Some(d) => {
            client.modern = true;
            d
        }
        None => {
            let init = client.request(
                "initialize",
                json::obj([
                    ("protocolVersion", PROTOCOL.into()),
                    ("capabilities", Value::Object(Vec::new())),
                    (
                        "clientInfo",
                        json::obj([("name", "mcpfs".into()), ("version", "0".into())]),
                    ),
                ]),
            )?;
            client.notify("notifications/initialized")?;
            init
        }
    };
    // Under 2026-07-28 a changed tool list comes only on a subscription.
    let listens = client.modern
        && init
            .get("capabilities")
            .and_then(|c| c.get("tools"))
            .and_then(|t| t.get("listChanged"))
            .and_then(|b| match b {
                Value::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);
    let mut tools = Vec::new();
    let mut cursor: Option<Value> = None;
    for _ in 0..MAX_PAGES {
        let params = match cursor.take() {
            Some(c) => json::obj([("cursor", c)]),
            None => Value::Object(Vec::new()),
        };
        let page = client.request("tools/list", params)?;
        match add_page(&mut tools, &page) {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    let mut server = init.encode().into_bytes();
    server.push(b'\n');
    let user = purgatorio::fs::read_to_string("/dev/user").unwrap_or_else(|_| String::from("none"));
    let mut fs = McpFs {
        client,
        user: String::from(user.trim()),
        server,
        tools,
        refresh: None,
        log: Vec::new(),
        fids: BTreeMap::new(),
        iounit: 8192,
    };
    if listens {
        let params = json::obj([(
            "notifications",
            json::obj([("toolsListChanged", Value::Bool(true))]),
        )]);
        fs.send_request("subscriptions/listen", params)?;
    }
    Ok(fs)
}

/// `mcpfs --serve ms command...`: the file server, with a call timeout of
/// `ms` (0 for none). 9P comes on fd 0; fd 1 gets `ok` or why the server
/// could not be reached, then is closed.
fn serve(argv: &[String]) -> Result<(), Error> {
    let timeout = argv
        .first()
        .and_then(|t| t.parse::<u64>().ok())
        .filter(|&t| t > 0);
    let argv = argv.get(1..).unwrap_or(&[]);
    // SAFETY: fds 0 and 1 are the pipes the mounting prog gave this one.
    let (io, mut status) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
    let mut fs = match connect(argv) {
        Ok(fs) => fs,
        Err(e) => {
            let _ = writeln!(status, "{e}");
            return Ok(());
        }
    };
    let _ = writeln!(status, "ok");
    drop(status);
    serve::serve(io, &mut fs, timeout)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        return serve(&args[2..]);
    }
    let mut arg = Arg::new(args, USAGE);
    let mut mntpt = String::from("/n/mcp");
    let mut timeout = String::from("0");
    while let Some(c) = arg.opt() {
        match c {
            'm' => mntpt = arg.earg()?,
            't' => {
                timeout = arg.earg()?;
                if timeout.parse::<u64>().is_err() {
                    return Err(arg.usage());
                }
            }
            _ => return Err(arg.usage()),
        }
    }
    let argv = arg.argv().to_vec();
    if argv.is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let [fs_r, fs_w] = purgatorio::pipe()?;
    let [st_r, st_w] = purgatorio::pipe()?;
    let mut sargv = vec![String::from("mcpfs"), String::from("--serve"), timeout];
    sargv.extend(argv);
    purgatorio::spawn(
        SELF,
        &sargv,
        pctl::NEWFD,
        &[(fs_r.as_raw_fd(), 0), (st_w.as_raw_fd(), 1), (2, 2)],
    )?;
    drop((fs_r, st_w));
    let mut line = String::new();
    BufReader::new(File::from(st_r)).read_line(&mut line)?;
    match line.trim() {
        "ok" => {}
        "" => return Err(Error::from("the file server exited")),
        e => return Err(Error(String::from(e))),
    }
    purgatorio::mount(fs_w.as_raw_fd(), -1, &mntpt, MREPL, "")
        .map_err(|e| Error(format!("mount {mntpt}: {e}")))?;
    Ok(())
}
