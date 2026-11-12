//! mcpfs [-m mntpt] command [arg ...]: mount the tools of a Model Context
//! Protocol server as files on `mntpt` (default `/n/mcp`).
//!
//! `command` is started with pipes for its standard input and output and
//! must speak MCP's stdio transport on them: `mcpd` for a local sandbox,
//! `rcmd host mcpd` for a remote one, or any other guest program that
//! does. mcpfs makes the `initialize` handshake (revision 2025-11-25),
//! lists the tools, and serves:
//!
//! ```text
//! mntpt/server             the server's initialize result, as JSON
//! mntpt/tools/NAME/schema  the tool's definition from tools/list, as JSON
//! mntpt/tools/NAME/call    write the arguments (a JSON object), then read
//!                          the result
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
//! The file server is one prog that answers one request at a time, so a
//! call holds up every other request to the mount until the server
//! answers. Resources, prompts, notifications (such as a changed tool
//! list) and progress are not served yet.

#[allow(dead_code)]
#[path = "../../mcpd/src/json.rs"]
mod json;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};

use json::Value;
use purgatorio::abi::mount::MREPL;
use purgatorio::abi::{error, mode, pctl};
use purgatorio::arg::Arg;
use purgatorio::styx::{self, IOHDRSZ, StyxFs};
use purgatorio::{Context, DMDIR, Dir, Error, Qid};

const USAGE: &str = "mcpfs [-m mntpt] command [arg ...]";
const SELF: &str = "/bin/mcpfs.wasm";
/// The revision mcpfs asks for. `mcpd` and most servers answer it.
const PROTOCOL: &str = "2025-11-25";
/// A longer line from the server is refused.
const MAX_LINE: usize = 16 << 20;
/// Pages of `tools/list` followed before giving up.
const MAX_PAGES: usize = 100;

const QTDIR: u8 = 0x80;
const QTFILE: u8 = 0;

const ROOT: u64 = 0;
const SERVER: u64 = 1;
const TOOLS: u64 = 2;
/// Tool `i` is `TOOL + 4i` (its directory), `+ 1` (schema), `+ 2` (call).
const TOOL: u64 = 16;

/// The client end of an MCP stdio connection.
struct Client {
    to: File,
    from: BufReader<File>,
    next: i64,
}

impl Client {
    fn send(&mut self, v: &Value) -> Result<(), String> {
        let mut line = v.encode();
        line.push('\n');
        self.to
            .write_all(line.as_bytes())
            .map_err(|e| format!("mcp server: {}", Error::from(e)))
    }

    /// Sends a request and returns its result, or the error's message.
    /// Requests from the server meanwhile are answered (`ping`) or refused;
    /// notifications are dropped.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
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
            return v
                .get("result")
                .cloned()
                .ok_or_else(|| String::from("mcp server: reply without a result"));
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

struct Tool {
    name: String,
    schema: Vec<u8>,
}

/// One open of a `call` file.
#[derive(Clone, Default)]
struct Call {
    args: Vec<u8>,
    /// The offset after the last write.
    end: u64,
    result: Option<Result<Vec<u8>, String>>,
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
    fids: BTreeMap<u32, Fid>,
    iounit: u32,
}

fn qid(path: u64) -> Qid {
    let dir = path == ROOT || path == TOOLS || (path >= TOOL && (path - TOOL).is_multiple_of(4));
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
        (t < self.tools.len()).then_some((t, i % 4))
    }

    fn parent(&self, path: u64) -> u64 {
        match self.tool_of(path) {
            Some((_, 0)) => TOOLS,
            Some((t, _)) => TOOL + 4 * t as u64,
            None => ROOT,
        }
    }

    fn child(&self, path: u64, name: &str) -> Option<u64> {
        match (path, self.tool_of(path)) {
            (ROOT, _) => match name {
                "server" => Some(SERVER),
                "tools" => Some(TOOLS),
                _ => None,
            },
            (TOOLS, _) => self
                .tools
                .iter()
                .position(|t| t.name == name)
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
            (ROOT, _) => vec![SERVER, TOOLS],
            (TOOLS, _) => (0..self.tools.len()).map(|t| TOOL + 4 * t as u64).collect(),
            (_, Some((t, 0))) => vec![TOOL + 4 * t as u64 + 1, TOOL + 4 * t as u64 + 2],
            _ => Vec::new(),
        }
    }

    fn dir(&self, path: u64) -> Dir {
        let (name, mode, length) = match (path, self.tool_of(path)) {
            (ROOT, _) => (String::from("/"), DMDIR | 0o555, 0),
            (SERVER, _) => (String::from("server"), 0o444, self.server.len()),
            (TOOLS, _) => (String::from("tools"), DMDIR | 0o555, 0),
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

    /// Makes the call a `call` fid holds the arguments for.
    fn call(&mut self, tool: usize, args: &[u8]) -> Result<Vec<u8>, String> {
        let text = String::from_utf8_lossy(args);
        let text = text.trim();
        let arguments = if text.is_empty() {
            Value::Object(Vec::new())
        } else {
            match json::parse(text) {
                Ok(v) if v.is_object() => v,
                Ok(_) => return Err(String::from("the arguments must be a JSON object")),
                Err(e) => return Err(format!("the arguments are not JSON: {e}")),
            }
        };
        let name = self.tools[tool].name.clone();
        let result = self.client.request(
            "tools/call",
            json::obj([("name", name.into()), ("arguments", arguments)]),
        )?;
        let mut out = result.encode().into_bytes();
        out.push(b'\n');
        Ok(out)
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
        let is_call = matches!(self.tool_of(path), Some((_, 2)));
        let ok = if is_call {
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
            (_, Some((t, 1))) => Ok(slice(&self.tools[t].schema, offset, count)),
            (_, Some((t, 2))) => {
                let result = match f.call.result {
                    Some(r) => r,
                    None => {
                        let r = self.call(t, &f.call.args);
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
        if !matches!(self.tool_of(path), Some((_, 2))) {
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

/// Starts `argv` with pipes for its standard input and output, and makes
/// the handshake and the tool list.
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
    };
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
    let mut tools = Vec::new();
    let mut cursor: Option<Value> = None;
    for _ in 0..MAX_PAGES {
        let params = match cursor.take() {
            Some(c) => json::obj([("cursor", c)]),
            None => Value::Object(Vec::new()),
        };
        let page = client.request("tools/list", params)?;
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
                });
            }
        }
        match page.get("nextCursor") {
            Some(c @ Value::Str(_)) => cursor = Some(c.clone()),
            _ => break,
        }
    }
    let mut server = init.encode().into_bytes();
    server.push(b'\n');
    let user = std::fs::read_to_string("/dev/user").unwrap_or_else(|_| String::from("none"));
    Ok(McpFs {
        client,
        user: String::from(user.trim()),
        server,
        tools,
        fids: BTreeMap::new(),
        iounit: 8192,
    })
}

/// `mcpfs --serve command...`: the file server. 9P comes on fd 0; fd 1
/// gets `ok` or why the server could not be reached, then is closed.
fn serve(argv: &[String]) -> Result<(), Error> {
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
    styx::serve(io, &mut fs)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        return serve(&args[2..]);
    }
    let mut arg = Arg::new(args, USAGE);
    let mut mntpt = String::from("/n/mcp");
    while let Some(c) = arg.opt() {
        match c {
            'm' => mntpt = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let argv = arg.argv().to_vec();
    if argv.is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let [fs_r, fs_w] = purgatorio::pipe()?;
    let [st_r, st_w] = purgatorio::pipe()?;
    let mut sargv = vec![String::from("mcpfs"), String::from("--serve")];
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
