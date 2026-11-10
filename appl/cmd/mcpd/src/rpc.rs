//! The JSON-RPC 2.0 and MCP layer: one request line in, at most one
//! response line out. Nothing here touches files or progs, so it is tested
//! on the build host.
//!
//! Specification followed: Model Context Protocol revision 2026-07-28, the
//! stdio transport. That revision has no handshake: every request carries
//! its protocol version and the client's capabilities in `params._meta`,
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
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// How long a client may cache `server/discover` and `tools/list`: they
/// change only with the binary.
const TTL_MS: i64 = 3_600_000;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const UNSUPPORTED_VERSION: i64 = -32022;

/// Runs tools. The front end of mcpd passes calls to the session prog; the
/// tests use a fake.
pub trait Host {
    /// Calls tool `name` with arguments already checked against its schema.
    /// `Ok` is the structured result; `Err` is the text of a failure the
    /// model can act on.
    fn call(&mut self, name: &str, args: &Value) -> Result<Value, String>;
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

/// Adds the members every 2026-07-28 result carries.
fn complete(result: Value, modern: bool, cacheable: bool) -> Value {
    let Value::Object(mut m) = result else {
        return result;
    };
    if modern {
        m.insert(0, (String::from("resultType"), Value::from("complete")));
        if cacheable {
            m.push((String::from("ttlMs"), Value::Int(TTL_MS)));
            m.push((String::from("cacheScope"), Value::from("public")));
        }
    }
    Value::Object(m)
}

impl Server {
    /// Handles one line from the client. `None` means no reply is due: the
    /// line was a notification, a response, or blank.
    pub fn handle(&mut self, line: &str, host: &mut dyn Host) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let msg = match json::parse(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(response(
                    Value::Null,
                    Err(RpcError::new(PARSE_ERROR, format!("Parse error: {e}"))),
                ));
            }
        };
        if matches!(msg, Value::Array(_)) {
            return Some(response(
                Value::Null,
                Err(RpcError::new(
                    INVALID_REQUEST,
                    "Invalid Request: batches are not supported",
                )),
            ));
        }
        if !msg.is_object() {
            return Some(response(
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
                return None;
            }
            return Some(response(
                id.cloned().unwrap_or(Value::Null),
                Err(RpcError::new(INVALID_REQUEST, "Invalid Request: no method")),
            ));
        };
        let Some(id) = id else {
            // A notification is never answered, whatever it says.
            return None;
        };
        let id_ok = matches!(id, Value::Str(_) | Value::Int(_) | Value::Float(_));
        let reply_id = if id_ok { id.clone() } else { Value::Null };
        let fail =
            |code, text: &str| Some(response(reply_id.clone(), Err(RpcError::new(code, text))));
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
        Some(response(
            reply_id.clone(),
            self.request(method, params, host),
        ))
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
                    Some(c) if c.is_object() => Ok(true),
                    _ => Err(RpcError::new(
                        INVALID_PARAMS,
                        format!("Invalid params: _meta.{META_CAPABILITIES} is required"),
                    )),
                }
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

    fn request(
        &mut self,
        method: &str,
        params: &Value,
        host: &mut dyn Host,
    ) -> Result<Value, RpcError> {
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
                Ok(obj([
                    ("protocolVersion", version.into()),
                    (
                        "capabilities",
                        obj([("tools", obj([("listChanged", false.into())]))]),
                    ),
                    ("serverInfo", server_info()),
                    ("instructions", crate::primer::PRIMER.into()),
                ]))
            }
            // Removed in 2026-07-28, but harmless to answer for anyone.
            "ping" => {
                let modern = params
                    .get("_meta")
                    .is_some_and(|m| m.get(META_VERSION).is_some());
                Ok(complete(Value::Object(Vec::new()), modern, false))
            }
            "server/discover" => {
                if !self.era(params)? {
                    return Err(RpcError::new(
                        METHOD_NOT_FOUND,
                        "Method not found: server/discover (not in the negotiated revision)",
                    ));
                }
                Ok(complete(
                    obj([
                        ("supportedVersions", supported()),
                        ("capabilities", obj([("tools", Value::Object(Vec::new()))])),
                        ("instructions", crate::primer::PRIMER.into()),
                        ("_meta", obj([(META_SERVER_INFO, server_info())])),
                    ]),
                    true,
                    true,
                ))
            }
            "tools/list" => {
                let modern = self.era(params)?;
                // Everything fits one page, so any cursor is a bad one.
                if params.get("cursor").is_some_and(|c| *c != Value::Null) {
                    return Err(RpcError::new(
                        INVALID_PARAMS,
                        "Invalid params: unknown cursor",
                    ));
                }
                Ok(complete(
                    obj([("tools", Value::Array(tooldefs::tools()))]),
                    modern,
                    true,
                ))
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
                let empty = Value::Object(Vec::new());
                let args = match params.get("arguments") {
                    None | Some(Value::Null) => &empty,
                    Some(a) => a,
                };
                // Bad arguments are reported as a tool failure, not a
                // protocol error, so that the model sees them and can
                // correct the call.
                let outcome = tool
                    .get("inputSchema")
                    .map_or(Ok(()), |schema| {
                        tooldefs::validate(args, schema, "arguments")
                    })
                    .and_then(|()| host.call(name, args));
                Ok(complete(
                    match outcome {
                        Ok(structured) => obj([
                            (
                                "content",
                                Value::Array(vec![obj([
                                    ("type", "text".into()),
                                    ("text", structured.encode().into()),
                                ])]),
                            ),
                            ("structuredContent", structured),
                            ("isError", false.into()),
                        ]),
                        Err(text) => obj([
                            (
                                "content",
                                Value::Array(vec![obj([
                                    ("type", "text".into()),
                                    ("text", text.into()),
                                ])]),
                            ),
                            ("isError", true.into()),
                        ]),
                    },
                    modern,
                    false,
                ))
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
                _ => Ok(obj([("echo", args.clone())])),
            }
        }
    }

    const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}"#;

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
        for m in ["resources/list", "prompts/list", "nope", "tools/nope", ""] {
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
                r#"{"tools":{"listChanged":false}}"#
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
        assert_eq!(r.get("capabilities").unwrap().encode(), r#"{"tools":{}}"#);
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
}
