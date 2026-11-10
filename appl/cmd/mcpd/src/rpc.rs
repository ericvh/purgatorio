//! The JSON-RPC 2.0 and MCP layer. A line from the client becomes a reply
//! at once or a [`Call`] for the session, whose reply is made when the
//! session answers; calls can therefore finish out of order. Nothing here
//! touches files or progs, so it is tested on the build host.
//!
//! Specification followed: Model Context Protocol revision 2026-07-28, the
//! stdio transport. That revision has no handshake: every request carries
//! its protocol version, the client's capabilities and its name and
//! version (`clientInfo`) in `params._meta`,
//! and `server/discover` describes the server. Clients of the earlier
//! revisions (2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05) open with
//! `initialize` instead; the specification lets a server answer both, and
//! this one does. `ping` exists only in the earlier revisions and is
//! answered for any client.

use crate::json::{self, Value, obj};
use crate::tooldefs;

/// The current revision: stateless, per-request metadata.
pub const MODERN: &str = "2026-07-28";
/// Handshake revisions, newest first.
pub const LEGACY: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub const SERVER_NAME: &str = "mcpd";
pub const SERVER_TITLE: &str = "Purgatorio sandbox";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// How long a client may cache `server/discover`, `tools/list` and
/// `resources/templates/list`: they change only with the binary.
const TTL_MS: i64 = 3_600_000;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const UNSUPPORTED_VERSION: i64 = -32022;
/// "Resource not found" before 2026-07-28, which uses -32602.
pub const RESOURCE_NOT_FOUND: i64 = -32002;

/// Runs calls at once, for the tests of this layer. The front end of mcpd
/// uses [`Server::begin`] and [`Call::finish`] instead.
#[cfg(test)]
pub trait Host {
    /// Calls tool `name` with arguments already checked against its schema.
    /// `Ok` is the structured result; `Err` is the text of a failure the
    /// model can act on.
    fn call(&mut self, name: &str, args: &Value) -> Result<Value, String>;
}

/// What a line from the client comes to.
pub enum Step {
    /// Nothing is due: a notification, a response, or a blank line.
    Quiet,
    /// The reply, ready to send.
    Reply(String),
    /// Work for the session. Its reply is [`Call::finish`] of the outcome.
    Call(Call),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum What {
    Tool,
    ListResources,
    ReadResource,
}

/// A request the session answers: a tool call or a resource request.
pub struct Call {
    id: Value,
    modern: bool,
    pub what: What,
    /// The tool, or `resources/list` or `resources/read`.
    pub name: String,
    /// The tool's arguments, or `{"path": ...}` for a resource read.
    pub args: Value,
    uri: String,
    /// Why the arguments were refused, for a tool call that failed its
    /// schema. Such a call is finished without running.
    pub rejected: Option<String>,
}

#[derive(Default)]
pub struct Server {
    /// The revision agreed by `initialize`, for a handshake client.
    legacy: Option<String>,
}

struct RpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }
}

fn response(id: Value, body: Result<Value, RpcError>) -> String {
    let mut members = vec![
        (String::from("jsonrpc"), Value::from("2.0")),
        (String::from("id"), id),
    ];
    match body {
        Ok(result) => members.push((String::from("result"), result)),
        Err(e) => {
            let mut err = vec![
                (String::from("code"), Value::Int(e.code)),
                (String::from("message"), Value::Str(e.message)),
            ];
            if let Some(d) = e.data {
                err.push((String::from("data"), d));
            }
            members.push((String::from("error"), Value::Object(err)));
        }
    }
    Value::Object(members).encode()
}

/// The reply to a request line longer than the server will hold.
pub fn too_long() -> String {
    response(
        Value::Null,
        Err(RpcError::new(
            INVALID_REQUEST,
            "Invalid Request: message too long",
        )),
    )
}

fn supported() -> Value {
    let mut v = vec![Value::from(MODERN)];
    v.extend(LEGACY.iter().map(|s| Value::from(*s)));
    Value::Array(v)
}

fn server_info() -> Value {
    obj([
        ("name", SERVER_NAME.into()),
        ("title", SERVER_TITLE.into()),
        ("version", SERVER_VERSION.into()),
    ])
}

/// Caching hints of a 2026-07-28 result.
#[derive(Clone, Copy)]
enum Cache {
    /// None given: a tool result.
    None,
    /// Changes only with the binary.
    Public,
    /// Depends on the session, and is stale at once.
    Private,
}

/// Adds the members every 2026-07-28 result carries.
fn complete(result: Value, modern: bool, cache: Cache) -> Value {
    let Value::Object(mut m) = result else {
        return result;
    };
    if modern {
        m.insert(0, (String::from("resultType"), Value::from("complete")));
        let (ttl, scope) = match cache {
            Cache::None => return Value::Object(m),
            Cache::Public => (TTL_MS, "public"),
            Cache::Private => (0, "private"),
        };
        m.push((String::from("ttlMs"), Value::Int(ttl)));
        m.push((String::from("cacheScope"), Value::from(scope)));
    }
    Value::Object(m)
}

/// The URI of a file: `purg://` and its absolute name, with every byte
/// outside RFC 3986's unreserved and sub-delimiter sets (and `/`, `:`
/// and `@`) percent-encoded.
pub fn uri_of(path: &str) -> String {
    let mut out = String::from("purg://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The absolute file name a `purg:///name` URI stands for.
pub fn path_of(uri: &str) -> Result<String, String> {
    let rest = uri
        .strip_prefix("purg:///")
        .ok_or_else(|| format!("not a purg:/// URI: {uri}"))?;
    let raw = rest.as_bytes();
    let mut bytes = vec![b'/'];
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'%' => {
                let byte = raw
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| format!("bad percent escape in {uri}"))?;
                bytes.push(byte);
                i += 3;
            }
            b'?' | b'#' => return Err(format!("a query or fragment in {uri}")),
            b => {
                bytes.push(b);
                i += 1;
            }
        }
    }
    match String::from_utf8(bytes) {
        Ok(p) if !p.contains('\0') => Ok(p),
        _ => Err(format!("not a file name: {uri}")),
    }
}

/// The one resource template: any file, by name.
fn templates() -> Value {
    Value::Array(vec![obj([
        ("uriTemplate", "purg:///{+path}".into()),
        ("name", "file".into()),
        ("title", "A file in the session".into()),
        (
            "description",
            "Any file in the session's namespace, by its absolute name without the leading \
             slash: purg:///tmp/notes.txt, purg:///prog/1/status. A directory reads as its \
             names, one per line, with / after each directory."
                .into(),
        ),
    ])])
}

fn capabilities(handshake: bool) -> Value {
    if handshake {
        obj([
            ("tools", obj([("listChanged", false.into())])),
            (
                "resources",
                obj([("subscribe", false.into()), ("listChanged", false.into())]),
            ),
        ])
    } else {
        obj([
            ("tools", Value::Object(Vec::new())),
            ("resources", Value::Object(Vec::new())),
        ])
    }
}

/// A `tools/call` result: the structured value and the same JSON as text,
/// or a failure as text with `isError`.
fn tool_result(outcome: Result<Value, String>) -> Value {
    let (text, structured) = match outcome {
        Ok(v) => (v.encode(), Some(v)),
        Err(text) => (text, None),
    };
    let mut m = vec![(
        String::from("content"),
        Value::Array(vec![obj([("type", "text".into()), ("text", text.into())])]),
    )];
    let failed = structured.is_none();
    if let Some(v) = structured {
        m.push((String::from("structuredContent"), v));
    }
    m.push((String::from("isError"), failed.into()));
    Value::Object(m)
}

impl Call {
    /// `tool` or `resource`, for the log.
    pub fn kind(&self) -> &'static str {
        match self.what {
            What::Tool => "tool",
            What::ListResources | What::ReadResource => "resource",
        }
    }

    /// The response to send, given what the session made of the call.
    pub fn finish(&self, outcome: Result<Value, String>) -> String {
        let (id, modern) = (self.id.clone(), self.modern);
        let body = match self.what {
            What::Tool => Ok(complete(tool_result(outcome), modern, Cache::None)),
            What::ListResources => outcome
                .map(|v| complete(v, modern, Cache::Private))
                .map_err(|e| RpcError::new(INTERNAL_ERROR, e)),
            What::ReadResource => match outcome {
                Ok(v) => {
                    let mut item = vec![(String::from("uri"), Value::from(self.uri.as_str()))];
                    if let Value::Object(m) = v {
                        item.extend(m);
                    }
                    Ok(complete(
                        obj([("contents", Value::Array(vec![Value::Object(item)]))]),
                        modern,
                        Cache::Private,
                    ))
                }
                Err(e) if e.ends_with("file does not exist") => Err(RpcError {
                    code: if modern {
                        INVALID_PARAMS
                    } else {
                        RESOURCE_NOT_FOUND
                    },
                    message: String::from("Resource not found"),
                    data: Some(obj([
                        ("uri", self.uri.as_str().into()),
                        ("reason", e.into()),
                    ])),
                }),
                Err(e) => Err(RpcError::new(INTERNAL_ERROR, e)),
            },
        };
        response(id, body)
    }
}

/// What a request comes to: a result now, or work for the session.
enum Answer {
    Now(Value),
    Later {
        what: What,
        name: String,
        args: Value,
        uri: String,
        rejected: Option<String>,
        modern: bool,
    },
}

impl Server {
    /// Handles one line from the client and runs any call at once. `None`
    /// means no reply is due.
    #[cfg(test)]
    pub fn handle(&mut self, line: &str, host: &mut dyn Host) -> Option<String> {
        match self.begin(line) {
            Step::Quiet => None,
            Step::Reply(r) => Some(r),
            Step::Call(c) => {
                let outcome = match &c.rejected {
                    Some(why) => Err(why.clone()),
                    None => host.call(&c.name, &c.args),
                };
                Some(c.finish(outcome))
            }
        }
    }

    /// Reads one line from the client.
    pub fn begin(&mut self, line: &str) -> Step {
        let line = line.trim();
        if line.is_empty() {
            return Step::Quiet;
        }
        let msg = match json::parse(line) {
            Ok(v) => v,
            Err(e) => {
                return Step::Reply(response(
                    Value::Null,
                    Err(RpcError::new(PARSE_ERROR, format!("Parse error: {e}"))),
                ));
            }
        };
        if matches!(msg, Value::Array(_)) {
            return Step::Reply(response(
                Value::Null,
                Err(RpcError::new(
                    INVALID_REQUEST,
                    "Invalid Request: batches are not supported",
                )),
            ));
        }
        if !msg.is_object() {
            return Step::Reply(response(
                Value::Null,
                Err(RpcError::new(
                    INVALID_REQUEST,
                    "Invalid Request: a message must be an object",
                )),
            ));
        }
        let id = msg.get("id");
        let Some(method) = msg.get("method") else {
            // A response to a request. This server sends none, so there is
            // nothing to match it with; a message with neither is broken.
            if msg.get("result").is_some() || msg.get("error").is_some() {
                return Step::Quiet;
            }
            return Step::Reply(response(
                id.cloned().unwrap_or(Value::Null),
                Err(RpcError::new(INVALID_REQUEST, "Invalid Request: no method")),
            ));
        };
        let Some(id) = id else {
            // A notification is never answered, whatever it says.
            return Step::Quiet;
        };
        let id_ok = matches!(id, Value::Str(_) | Value::Int(_) | Value::Float(_));
        let reply_id = if id_ok { id.clone() } else { Value::Null };
        let fail = |code, text: &str| {
            Step::Reply(response(reply_id.clone(), Err(RpcError::new(code, text))))
        };
        if !id_ok {
            return fail(
                INVALID_REQUEST,
                "Invalid Request: id must be a string or a number",
            );
        }
        if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return fail(INVALID_REQUEST, "Invalid Request: jsonrpc must be \"2.0\"");
        }
        let Some(method) = method.as_str() else {
            return fail(INVALID_REQUEST, "Invalid Request: method must be a string");
        };
        let empty = Value::Object(Vec::new());
        let params = match msg.get("params") {
            None => &empty,
            Some(p) if p.is_object() => p,
            Some(_) => return fail(INVALID_PARAMS, "Invalid params: params must be an object"),
        };
        match self.request(method, params) {
            Ok(Answer::Now(v)) => Step::Reply(response(reply_id, Ok(v))),
            Err(e) => Step::Reply(response(reply_id, Err(e))),
            Ok(Answer::Later {
                what,
                name,
                args,
                uri,
                rejected,
                modern,
            }) => Step::Call(Call {
                id: reply_id,
                modern,
                what,
                name,
                args,
                uri,
                rejected,
            }),
        }
    }

    /// Decides which revision governs a request. `Ok(true)` is 2026-07-28;
    /// `Ok(false)` is a client that did the `initialize` handshake.
    fn era(&self, params: &Value) -> Result<bool, RpcError> {
        let meta = params.get("_meta");
        match meta.and_then(|m| m.get(META_VERSION)) {
            Some(v) => {
                let requested = v.as_str().ok_or_else(|| {
                    RpcError::new(
                        INVALID_PARAMS,
                        format!("Invalid params: _meta.{META_VERSION} must be a string"),
                    )
                })?;
                if requested != MODERN {
                    // A handshake client may tag requests with the version
                    // it negotiated; that is not a per-request version.
                    if self.legacy.as_deref() == Some(requested) {
                        return Ok(false);
                    }
                    return Err(RpcError {
                        code: UNSUPPORTED_VERSION,
                        message: String::from("Unsupported protocol version"),
                        data: Some(obj([
                            ("supported", supported()),
                            ("requested", requested.into()),
                        ])),
                    });
                }
                match meta.and_then(|m| m.get(META_CAPABILITIES)) {
                    Some(c) if c.is_object() => {}
                    _ => {
                        return Err(RpcError::new(
                            INVALID_PARAMS,
                            format!("Invalid params: _meta.{META_CAPABILITIES} is required"),
                        ));
                    }
                }
                // An Implementation: name and version are required.
                let info = meta.and_then(|m| m.get(META_CLIENT_INFO));
                let field = |k: &str| info.and_then(|i| i.get(k)).and_then(Value::as_str);
                if field("name").is_none() || field("version").is_none() {
                    return Err(RpcError::new(
                        INVALID_PARAMS,
                        format!(
                            "Invalid params: _meta.{META_CLIENT_INFO} with a name and a version is required"
                        ),
                    ));
                }
                Ok(true)
            }
            None if self.legacy.is_some() => Ok(false),
            None => Err(RpcError::new(
                INVALID_PARAMS,
                format!(
                    "Invalid params: _meta.{META_VERSION} is required \
                     (revision {MODERN}), or send initialize first (revision {} and earlier)",
                    LEGACY[0]
                ),
            )),
        }
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Answer, RpcError> {
        // Every list here fits one page, so any cursor is a bad one.
        let no_cursor = || {
            if params.get("cursor").is_some_and(|c| *c != Value::Null) {
                Err(RpcError::new(
                    INVALID_PARAMS,
                    "Invalid params: unknown cursor",
                ))
            } else {
                Ok(())
            }
        };
        match method {
            "initialize" => {
                let requested = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        RpcError::new(
                            INVALID_PARAMS,
                            "Invalid params: protocolVersion must be a string",
                        )
                    })?;
                // The client's version if this server speaks it, otherwise
                // the newest handshake revision; the client then decides.
                let version = if LEGACY.contains(&requested) {
                    requested
                } else {
                    LEGACY[0]
                };
                self.legacy = Some(String::from(version));
                Ok(Answer::Now(obj([
                    ("protocolVersion", version.into()),
                    ("capabilities", capabilities(true)),
                    ("serverInfo", server_info()),
                    ("instructions", crate::primer::PRIMER.into()),
                ])))
            }
            // Removed in 2026-07-28, but harmless to answer for anyone.
            "ping" => {
                let modern = params
                    .get("_meta")
                    .is_some_and(|m| m.get(META_VERSION).is_some());
                Ok(Answer::Now(complete(
                    Value::Object(Vec::new()),
                    modern,
                    Cache::None,
                )))
            }
            "server/discover" => {
                if !self.era(params)? {
                    return Err(RpcError::new(
                        METHOD_NOT_FOUND,
                        "Method not found: server/discover (not in the negotiated revision)",
                    ));
                }
                Ok(Answer::Now(complete(
                    obj([
                        ("supportedVersions", supported()),
                        ("capabilities", capabilities(false)),
                        ("instructions", crate::primer::PRIMER.into()),
                        ("_meta", obj([(META_SERVER_INFO, server_info())])),
                    ]),
                    true,
                    Cache::Public,
                )))
            }
            "tools/list" => {
                let modern = self.era(params)?;
                no_cursor()?;
                Ok(Answer::Now(complete(
                    obj([("tools", Value::Array(tooldefs::tools()))]),
                    modern,
                    Cache::Public,
                )))
            }
            "tools/call" => {
                let modern = self.era(params)?;
                let name = params.get("name").and_then(Value::as_str).ok_or_else(|| {
                    RpcError::new(INVALID_PARAMS, "Invalid params: name must be a string")
                })?;
                let Some(tool) = tooldefs::tools()
                    .into_iter()
                    .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
                else {
                    return Err(RpcError::new(
                        INVALID_PARAMS,
                        format!("Unknown tool: {name}"),
                    ));
                };
                let args = match params.get("arguments") {
                    None | Some(Value::Null) => Value::Object(Vec::new()),
                    Some(a) => a.clone(),
                };
                // Bad arguments are reported as a tool failure, not a
                // protocol error, so that the model sees them and can
                // correct the call.
                let rejected = tool.get("inputSchema").and_then(|schema| {
                    tooldefs::validate(&args, schema, "arguments")
                        .and_then(|()| tooldefs::check(name, &args))
                        .err()
                });
                Ok(Answer::Later {
                    what: What::Tool,
                    name: String::from(name),
                    args,
                    uri: String::new(),
                    rejected,
                    modern,
                })
            }
            "resources/list" => {
                let modern = self.era(params)?;
                no_cursor()?;
                Ok(Answer::Later {
                    what: What::ListResources,
                    name: String::from(method),
                    args: Value::Object(Vec::new()),
                    uri: String::new(),
                    rejected: None,
                    modern,
                })
            }
            "resources/templates/list" => {
                let modern = self.era(params)?;
                no_cursor()?;
                Ok(Answer::Now(complete(
                    obj([("resourceTemplates", templates())]),
                    modern,
                    Cache::Public,
                )))
            }
            "resources/read" => {
                let modern = self.era(params)?;
                let uri = params.get("uri").and_then(Value::as_str).ok_or_else(|| {
                    RpcError::new(INVALID_PARAMS, "Invalid params: uri must be a string")
                })?;
                let path = path_of(uri)
                    .map_err(|e| RpcError::new(INVALID_PARAMS, format!("Invalid params: {e}")))?;
                Ok(Answer::Later {
                    what: What::ReadResource,
                    name: String::from(method),
                    args: obj([("path", path.into())]),
                    uri: String::from(uri),
                    rejected: None,
                    modern,
                })
            }
            _ => Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("Method not found: {method}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records calls and answers `run` with its command.
    #[derive(Default)]
    struct Fake {
        calls: Vec<(String, String)>,
    }

    impl Host for Fake {
        fn call(&mut self, name: &str, args: &Value) -> Result<Value, String> {
            self.calls.push((String::from(name), args.encode()));
            match name {
                "remove" => Err(String::from("/x: file does not exist")),
                "resources/list" => Ok(obj([(
                    "resources",
                    Value::Array(vec![obj([
                        ("uri", "purg:///tmp/f".into()),
                        ("name", "/tmp/f".into()),
                    ])]),
                )])),
                "resources/read" => match args.get("path").and_then(Value::as_str) {
                    Some("/tmp/f") => Ok(obj([
                        ("mimeType", "text/plain".into()),
                        ("text", "hi".into()),
                    ])),
                    Some("/locked") => Err(String::from("/locked: permission denied")),
                    Some(p) => Err(format!("{p}: file does not exist")),
                    None => Err(String::from("no path")),
                },
                _ => Ok(obj([("echo", args.clone())])),
            }
        }
    }

    const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"t","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}}"#;

    fn ask(s: &mut Server, h: &mut Fake, line: &str) -> Value {
        let out = s.handle(line, h).expect("a reply");
        assert!(!out.contains('\n'), "{out:?}");
        let v = json::parse(&out).unwrap();
        assert_eq!(v.get("jsonrpc").and_then(Value::as_str), Some("2.0"));
        assert!(
            v.get("result").is_some() != v.get("error").is_some(),
            "{out}"
        );
        v
    }

    fn code(v: &Value) -> i64 {
        v.get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_i64)
            .unwrap_or_else(|| panic!("no error in {}", v.encode()))
    }

    fn init(s: &mut Server, h: &mut Fake, version: &str) -> Value {
        ask(
            s,
            h,
            &format!(
                r#"{{"jsonrpc":"2.0","id":0,"method":"initialize","params":{{"protocolVersion":"{version}","capabilities":{{}},"clientInfo":{{"name":"t","version":"1"}}}}}}"#
            ),
        )
    }

    #[test]
    fn parse_errors_and_invalid_requests() {
        let (mut s, mut h) = (Server::default(), Fake::default());
        let v = ask(&mut s, &mut h, "{not json");
        assert_eq!(code(&v), PARSE_ERROR);
        assert_eq!(v.get("id"), Some(&Value::Null));
        for bad in [
            "[]",
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
            "3",
            "\"x\"",
            "{}",
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":{},"method":"ping"}"#,
            r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
            r#"{"id":1,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":7}"#,
        ] {
            assert_eq!(code(&ask(&mut s, &mut h, bad)), INVALID_REQUEST, "{bad}");
        }
        let v = ask(
            &mut s,
            &mut h,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":[1]}"#,
        );
        assert_eq!(code(&v), INVALID_PARAMS);
        assert!(h.calls.is_empty());
    }

    #[test]
    fn notifications_and_responses_get_no_reply() {
        let (mut s, mut h) = (Server::default(), Fake::default());
        for quiet in [
            "",
            "   ",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
            r#"{"jsonrpc":"2.0","method":"no/such/notification"}"#,
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"run"}}"#,
            r#"{"jsonrpc":"2.0","id":7,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":1,"message":"x"}}"#,
        ] {
            assert_eq!(s.handle(quiet, &mut h), None, "{quiet}");
        }
        assert!(h.calls.is_empty());
    }

    #[test]
    fn ids_are_echoed() {
        let (mut s, mut h) = (Server::default(), Fake::default());
        for id in ["1", "\"abc\"", "-5", "9007199254740993", "\"\""] {
            let v = ask(
                &mut s,
                &mut h,
                &format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#),
            );
            assert_eq!(v.get("id").unwrap().encode(), id);
            assert_eq!(v.get("result").unwrap().encode(), "{}");
        }
    }

    #[test]
    fn unknown_method() {
        let (mut s, mut h) = (Server::default(), Fake::default());
        for m in [
            "prompts/list",
            "resources/subscribe",
            "nope",
            "tools/nope",
            "",
        ] {
            let v = ask(
                &mut s,
                &mut h,
                &format!(r#"{{"jsonrpc":"2.0","id":"a","method":"{m}","params":{{{META}}}}}"#),
            );
            assert_eq!(code(&v), METHOD_NOT_FOUND, "{m}");
            assert_eq!(v.get("id").unwrap().as_str(), Some("a"));
        }
    }

    #[test]
    fn handshake_negotiates_a_version() {
        for (asked, got) in [
            ("2025-11-25", "2025-11-25"),
            ("2025-06-18", "2025-06-18"),
            ("2024-11-05", "2024-11-05"),
            ("2026-07-28", "2025-11-25"),
            ("1999-01-01", "2025-11-25"),
        ] {
            let (mut s, mut h) = (Server::default(), Fake::default());
            let v = init(&mut s, &mut h, asked);
            let r = v.get("result").unwrap();
            assert_eq!(r.get("protocolVersion").unwrap().as_str(), Some(got));
            assert_eq!(
                r.get("capabilities").unwrap().encode(),
                r#"{"tools":{"listChanged":false},"resources":{"subscribe":false,"listChanged":false}}"#
            );
            let info = r.get("serverInfo").unwrap();
            assert_eq!(info.get("name").unwrap().as_str(), Some("mcpd"));
            assert!(info.get("version").unwrap().as_str().is_some());
            let primer = r.get("instructions").unwrap().as_str().unwrap();
            assert!(primer.contains("not bash"), "{primer}");
            assert!(r.get("resultType").is_none());
        }
        let (mut s, mut h) = (Server::default(), Fake::default());
        let v = ask(
            &mut s,
            &mut h,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        );
        assert_eq!(code(&v), INVALID_PARAMS);
    }

    #[test]
    fn handshake_client_lists_and_calls() {
        let (mut s, mut h) = (Server::default(), Fake::default());
        // Before the handshake a request without metadata is malformed.
        let v = ask(
            &mut s,
            &mut h,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        );
        assert_eq!(code(&v), INVALID_PARAMS);
        init(&mut s, &mut h, "2025-06-18");
        assert_eq!(
            s.handle(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                &mut h
            ),
            None
        );
        let v = ask(
            &mut s,
            &mut h,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        );
        let r = v.get("result").unwrap();
        assert!(r.get("resultType").is_none() && r.get("ttlMs").is_none());
        let Some(Value::Array(tools)) = r.get("tools") else {
            panic!("no tools")
        };
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").unwrap().as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "run",
                "read_file",
                "write_file",
                "list_dir",
                "stat",
                "remove",
                "job_start",
                "job_read",
                "job_write",
                "job_kill",
                "job_list",
                "ns",
                "bind",
                "mount",
                "unmount",
                "reset"
            ]
        );
        let v = ask(
            &mut s,
            &mut h,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run","arguments":{"command":"echo hi"}}}"#,
        );
        let r = v.get("result").unwrap();
        assert_eq!(r.get("isError"), Some(&Value::Bool(false)));
        assert_eq!(
            r.get("structuredContent").unwrap().encode(),
            r#"{"echo":{"command":"echo hi"}}"#
        );
        let Some(Value::Array(content)) = r.get("content") else {
            panic!("no content")
        };
        assert_eq!(content[0].get("type").unwrap().as_str(), Some("text"));
        // The text block is the same JSON, for clients without structured
        // content.
        assert_eq!(
            content[0].get("text").unwrap().as_str(),
            Some(r#"{"echo":{"command":"echo hi"}}"#)
        );
        // server/discover belongs to the other revision.
        let v = ask(
            &mut s,
            &mut h,
            r#"{"jsonrpc":"2.0","id":4,"method":"server/discover"}"#,
        );
        assert_eq!(code(&v), METHOD_NOT_FOUND);
    }

    #[test]
    fn stateless_client_needs_no_handshake() {
        let (mut s, mut h) = (Server::default(), Fake::default());
        let v = ask(
            &mut s,
            &mut h,
            &format!(
                r#"{{"jsonrpc":"2.0","id":"d","method":"server/discover","params":{{{META}}}}}"#
            ),
        );
        let r = v.get("result").unwrap();
        assert_eq!(r.get("resultType").unwrap().as_str(), Some("complete"));
        assert_eq!(
            r.get("supportedVersions").unwrap().encode(),
            r#"["2026-07-28","2025-11-25","2025-06-18","2025-03-26","2024-11-05"]"#
        );
        assert_eq!(
            r.get("capabilities").unwrap().encode(),
            r#"{"tools":{},"resources":{}}"#
        );
        assert!(
            r.get("instructions")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("not bash")
        );
        assert_eq!(r.get("ttlMs").unwrap().as_i64(), Some(3_600_000));
        assert_eq!(r.get("cacheScope").unwrap().as_str(), Some("public"));
        let info = r
            .get("_meta")
            .and_then(|m| m.get("io.modelcontextprotocol/serverInfo"))
            .unwrap();
        assert_eq!(info.get("name").unwrap().as_str(), Some("mcpd"));

        let v = ask(
            &mut s,
            &mut h,
            &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{{META}}}}}"#),
        );
        let r = v.get("result").unwrap();
        assert_eq!(r.get("resultType").unwrap().as_str(), Some("complete"));
        assert!(r.get("tools").is_some() && r.get("ttlMs").is_some());

        let v = ask(
            &mut s,
            &mut h,
            &format!(
                r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"stat","arguments":{{"path":"/"}},{META}}}}}"#
            ),
        );
        let r = v.get("result").unwrap();
        assert_eq!(r.get("resultType").unwrap().as_str(), Some("complete"));
        assert_eq!(r.get("isError"), Some(&Value::Bool(false)));
        assert!(r.get("ttlMs").is_none());
    }

    #[test]
    fn stateless_metadata_is_checked() {
        let (mut s, mut h) = (Server::default(), Fake::default());
        let v = ask(
            &mut s,
            &mut h,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"1900-01-01","io.modelcontextprotocol/clientCapabilities":{}}}}"#,
        );
        assert_eq!(code(&v), UNSUPPORTED_VERSION);
        let data = v.get("error").unwrap().get("data").unwrap();
        assert_eq!(data.get("requested").unwrap().as_str(), Some("1900-01-01"));
        assert!(
            data.get("supported")
                .unwrap()
                .encode()
                .contains("2026-07-28")
        );
        // So is clientInfo, with a name and a version.
        for info in [
            "",
            r#","io.modelcontextprotocol/clientInfo":{"name":"t"}"#,
            r#","io.modelcontextprotocol/clientInfo":"t""#,
        ] {
            let v = ask(
                &mut s,
                &mut h,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"_meta":{{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{{}}{info}}}}}}}"#
                ),
            );
            assert_eq!(code(&v), INVALID_PARAMS, "{info}");
            assert!(v.encode().contains("clientInfo"), "{info}");
        }
        // Capabilities are required with the version.
        let v = ask(
            &mut s,
            &mut h,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#,
        );
        assert_eq!(code(&v), INVALID_PARAMS);
        let v = ask(
            &mut s,
            &mut h,
            r#"{"jsonrpc":"2.0","id":1,"method":"server/discover"}"#,
        );
        assert_eq!(code(&v), INVALID_PARAMS);
        let v = ask(
            &mut s,
            &mut h,
            &format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"cursor":"x",{META}}}}}"#
            ),
        );
        assert_eq!(code(&v), INVALID_PARAMS);
    }

    #[test]
    fn tool_errors() {
        let (mut s, mut h) = (Server::default(), Fake::default());
        init(&mut s, &mut h, "2025-11-25");
        let call = |s: &mut Server, h: &mut Fake, params: &str| {
            ask(
                s,
                h,
                &format!(r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{params}}}"#),
            )
        };
        // An unknown tool and a missing name are protocol errors.
        assert_eq!(
            code(&call(&mut s, &mut h, r#"{"name":"nope","arguments":{}}"#)),
            INVALID_PARAMS
        );
        assert_eq!(
            code(&call(&mut s, &mut h, r#"{"arguments":{}}"#)),
            INVALID_PARAMS
        );
        assert!(h.calls.is_empty());
        // Bad arguments and a failing tool are results with isError, which
        // the model reads.
        for (params, want) in [
            (r#"{"name":"run","arguments":{}}"#, "command"),
            (r#"{"name":"run"}"#, "command"),
            (r#"{"name":"run","arguments":{"command":7}}"#, "string"),
            (
                r#"{"name":"run","arguments":{"command":"x","bogus":1}}"#,
                "bogus",
            ),
            (
                r#"{"name":"run","arguments":{"command":"x","timeout_ms":0}}"#,
                "timeout_ms",
            ),
            (r#"{"name":"run","arguments":[1]}"#, "object"),
            (
                r#"{"name":"write_file","arguments":{"path":"/tmp/x","content":"","mode":"w"}}"#,
                "mode",
            ),
            (
                r#"{"name":"read_file","arguments":{"path":"/tmp/x","offset":-1}}"#,
                "offset",
            ),
        ] {
            let v = call(&mut s, &mut h, params);
            let r = v.get("result").unwrap_or_else(|| panic!("{}", v.encode()));
            assert_eq!(r.get("isError"), Some(&Value::Bool(true)), "{params}");
            assert!(r.get("structuredContent").is_none());
            assert!(r.encode().contains(want), "{params}: {}", r.encode());
        }
        assert!(h.calls.is_empty(), "{:?}", h.calls);
        let v = call(
            &mut s,
            &mut h,
            r#"{"name":"remove","arguments":{"path":"/x"}}"#,
        );
        let r = v.get("result").unwrap();
        assert_eq!(r.get("isError"), Some(&Value::Bool(true)));
        assert!(r.encode().contains("/x: file does not exist"));
        assert_eq!(h.calls.len(), 1);
    }

    #[test]
    fn uris_name_files() {
        assert_eq!(uri_of("/tmp/a b%.txt"), "purg:///tmp/a%20b%25.txt");
        assert_eq!(uri_of("/prog/1/status"), "purg:///prog/1/status");
        assert_eq!(
            path_of("purg:///tmp/a%20b%25.txt").unwrap(),
            "/tmp/a b%.txt"
        );
        assert_eq!(path_of("purg:///").unwrap(), "/");
        assert_eq!(path_of(&uri_of("/tmp/é")).unwrap(), "/tmp/é");
        for bad in [
            "file:///tmp/x",
            "purg://host/x",
            "purg:///x%2",
            "purg:///x?y",
            "purg:///%00",
        ] {
            assert!(path_of(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn resources() {
        for modern in [false, true] {
            let (mut s, mut h) = (Server::default(), Fake::default());
            let meta = if modern {
                format!(",{META}")
            } else {
                init(&mut s, &mut h, "2025-11-25");
                String::new()
            };
            let call = |s: &mut Server, h: &mut Fake, method: &str, params: &str| {
                let sep = if params.is_empty() {
                    &meta[1.min(meta.len())..]
                } else {
                    &meta
                };
                ask(
                    s,
                    h,
                    &format!(
                        r#"{{"jsonrpc":"2.0","id":5,"method":"{method}","params":{{{params}{sep}}}}}"#
                    ),
                )
            };
            let v = call(&mut s, &mut h, "resources/read", r#""uri":"purg:///tmp/f""#);
            let r = v.get("result").unwrap();
            assert_eq!(
                r.get("contents").unwrap().encode(),
                r#"[{"uri":"purg:///tmp/f","mimeType":"text/plain","text":"hi"}]"#
            );
            assert_eq!(r.get("resultType").is_some(), modern);
            if modern {
                assert_eq!(r.get("cacheScope").unwrap().as_str(), Some("private"));
                assert_eq!(r.get("ttlMs").unwrap().as_i64(), Some(0));
            }
            let v = call(
                &mut s,
                &mut h,
                "resources/read",
                r#""uri":"purg:///missing""#,
            );
            let want = if modern {
                INVALID_PARAMS
            } else {
                RESOURCE_NOT_FOUND
            };
            assert_eq!(code(&v), want);
            let data = v.get("error").unwrap().get("data").unwrap();
            assert_eq!(data.get("uri").unwrap().as_str(), Some("purg:///missing"));
            let v = call(
                &mut s,
                &mut h,
                "resources/read",
                r#""uri":"purg:///locked""#,
            );
            assert_eq!(code(&v), INTERNAL_ERROR);
            let v = call(&mut s, &mut h, "resources/read", r#""uri":"http://x/""#);
            assert_eq!(code(&v), INVALID_PARAMS);
            let v = call(&mut s, &mut h, "resources/list", "");
            assert_eq!(
                v.get("result").unwrap().get("resources").unwrap().encode(),
                r#"[{"uri":"purg:///tmp/f","name":"/tmp/f"}]"#
            );
            let v = call(&mut s, &mut h, "resources/list", r#""cursor":"x""#);
            assert_eq!(code(&v), INVALID_PARAMS);
            let v = call(&mut s, &mut h, "resources/templates/list", "");
            let t = v.get("result").unwrap().get("resourceTemplates").unwrap();
            assert!(t.encode().contains(r#""uriTemplate":"purg:///{+path}""#));
        }
    }

    #[test]
    fn job_start_needs_one_program() {
        let (mut s, mut h) = (Server::default(), Fake::default());
        init(&mut s, &mut h, "2025-11-25");
        for (args, ok) in [
            (r#"{"command":"cat"}"#, true),
            (r#"{"argv":["cat"]}"#, true),
            (r#"{}"#, false),
            (r#"{"argv":[]}"#, false),
            (r#"{"argv":[1]}"#, false),
            (r#"{"command":"cat","argv":["cat"]}"#, false),
        ] {
            let v = ask(
                &mut s,
                &mut h,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"job_start","arguments":{args}}}}}"#
                ),
            );
            let r = v.get("result").unwrap();
            assert_eq!(r.get("isError"), Some(&Value::Bool(!ok)), "{args}");
        }
    }
}
