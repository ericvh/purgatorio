//! Acceptance tests for `mcpd`: launch `emu -r root mcpd` as an MCP client
//! would and drive whole sessions over its standard input and output, on
//! both WASM backends. Requires `cargo xtask build-root` first.
//!
//! Every line the server writes must be one JSON-RPC message; the harness
//! fails on anything else, which is how "nothing but protocol on stdout"
//! is checked throughout.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

#[allow(dead_code)]
#[path = "../../../appl/cmd/mcpd/src/json.rs"]
mod json;

use json::Value;

const VMS: [&str; 2] = ["wasmtime", "wasmi"];
const PRIMER: &str = include_str!("../../../appl/cmd/mcpd/src/primer.txt");
/// Longer than any single call should take, even on the interpreter.
const REPLY: Duration = Duration::from_secs(120);

fn root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root");
    assert!(
        root.join("bin/mcpd.wasm").exists(),
        "guest binaries missing; run `cargo xtask build-root` first"
    );
    root
}

struct Mcp {
    vm: &'static str,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: i64,
    /// Responses that came while waiting for another.
    early: Vec<Value>,
}

impl Mcp {
    /// Starts a server without the handshake.
    fn raw(vm: &'static str, args: &[&str]) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_emu"))
            .arg("-r")
            .arg(root())
            .arg(format!("--vm={vm}"))
            .arg("mcpd")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn emu");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.expect("utf-8 from the server")).is_err() {
                    break;
                }
            }
        });
        Mcp {
            vm,
            stdin: child.stdin.take(),
            child,
            lines,
            next_id: 1,
            early: Vec::new(),
        }
    }

    /// Starts a server and does the `initialize` handshake.
    fn start(vm: &'static str, args: &[&str]) -> Mcp {
        let mut m = Mcp::raw(vm, args);
        let r = m.request(
            "initialize",
            r#"{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"0"}}"#,
        );
        assert_eq!(
            r.get("protocolVersion").and_then(Value::as_str),
            Some("2025-11-25"),
            "{vm}"
        );
        m.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        m
    }

    fn send(&mut self, line: &str) {
        self.send_bytes(line.as_bytes());
    }

    fn send_bytes(&mut self, line: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    /// The next message from the server, which must be a JSON-RPC response.
    fn recv(&mut self) -> Value {
        let line = self
            .lines
            .recv_timeout(REPLY)
            .unwrap_or_else(|e| panic!("{}: no reply: {e}", self.vm));
        let v = json::parse(&line)
            .unwrap_or_else(|e| panic!("{}: not JSON on stdout ({e}): {line:?}", self.vm));
        assert_eq!(
            v.get("jsonrpc").and_then(Value::as_str),
            Some("2.0"),
            "{}: {line}",
            self.vm
        );
        assert!(
            v.get("result").is_some() != v.get("error").is_some(),
            "{}: {line}",
            self.vm
        );
        v
    }

    /// Sends a request and returns the whole response, checking its id.
    fn exchange(&mut self, method: &str, params: &str) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ));
        let v = self.recv();
        assert_eq!(
            v.get("id"),
            Some(&Value::Int(id)),
            "{}: {}",
            self.vm,
            v.encode()
        );
        v
    }

    /// Sends a request that must succeed and returns its result.
    fn request(&mut self, method: &str, params: &str) -> Value {
        let v = self.exchange(method, params);
        v.get("result")
            .unwrap_or_else(|| panic!("{}: {method} failed: {}", self.vm, v.encode()))
            .clone()
    }

    /// Calls a tool; returns `(isError, structuredContent or the text)`.
    fn tool(&mut self, name: &str, args: &str) -> (bool, Value) {
        let r = self.request(
            "tools/call",
            &format!(r#"{{"name":"{name}","arguments":{args}}}"#),
        );
        let is_error = r.get("isError").and_then(Value::as_bool).expect("isError");
        let Some(Value::Array(content)) = r.get("content") else {
            panic!("{}: no content: {}", self.vm, r.encode())
        };
        assert_eq!(content[0].get("type").and_then(Value::as_str), Some("text"));
        let text = content[0].get("text").and_then(Value::as_str).unwrap();
        match r.get("structuredContent") {
            Some(s) => {
                assert!(!is_error);
                // The text block carries the same JSON.
                assert_eq!(json::parse(text).unwrap(), *s, "{}", self.vm);
                (false, s.clone())
            }
            None => {
                assert!(is_error, "{}: {}", self.vm, r.encode());
                (true, Value::from(text))
            }
        }
    }

    /// Calls a tool that must succeed.
    fn ok(&mut self, name: &str, args: &str) -> Value {
        let (is_error, v) = self.tool(name, args);
        assert!(!is_error, "{}: {name} {args}: {}", self.vm, v.encode());
        v
    }

    /// Calls a tool that must fail; returns the message.
    fn fails(&mut self, name: &str, args: &str) -> String {
        let (is_error, v) = self.tool(name, args);
        assert!(is_error, "{}: {name} {args}: {}", self.vm, v.encode());
        String::from(v.as_str().unwrap())
    }

    /// Sends a tool call without waiting; returns its id.
    fn send_tool(&mut self, name: &str, args: &str) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
        ));
        id
    }

    /// The response to request `id`, keeping others that come first.
    fn recv_id(&mut self, id: i64) -> Value {
        if let Some(i) = self
            .early
            .iter()
            .position(|v| v.get("id") == Some(&Value::Int(id)))
        {
            return self.early.remove(i);
        }
        loop {
            let v = self.recv();
            if v.get("id") == Some(&Value::Int(id)) {
                return v;
            }
            self.early.push(v);
        }
    }

    /// The structured result of tool call `id`, which must succeed.
    fn tool_result(&mut self, id: i64) -> Value {
        let v = self.recv_id(id);
        let r = v.get("result").unwrap();
        assert_eq!(
            r.get("isError"),
            Some(&Value::Bool(false)),
            "{}: {}",
            self.vm,
            v.encode()
        );
        r.get("structuredContent").unwrap().clone()
    }

    /// Runs an sh command line with default limits.
    fn run(&mut self, command: &str) -> Run {
        self.run_with(command, "")
    }

    /// Runs a command with more arguments, given as JSON members.
    fn run_with(&mut self, command: &str, more: &str) -> Run {
        let sep = if more.is_empty() { "" } else { "," };
        let v = self.ok(
            "run",
            &format!(
                r#"{{"command":{}{sep}{more}}}"#,
                Value::from(command).encode()
            ),
        );
        let s = |k: &str| String::from(v.get(k).and_then(Value::as_str).unwrap());
        let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap();
        let r = Run {
            stdout: s("stdout"),
            stderr: s("stderr"),
            status: s("status"),
            timed_out: b("timed_out"),
            stdout_truncated: b("stdout_truncated"),
            stderr_truncated: b("stderr_truncated"),
        };
        assert_eq!(b("ok"), r.status.is_empty() && !r.timed_out);
        r
    }

    /// Closes the server's input and checks that it exits cleanly.
    fn finish(mut self) {
        drop(self.stdin.take());
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{}: {status}", self.vm);
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "{}: the server did not exit at end of input",
                self.vm
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            self.lines.recv_timeout(Duration::from_secs(5)).is_err(),
            "{}: output after the last reply",
            self.vm
        );
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
struct Run {
    stdout: String,
    stderr: String,
    status: String,
    timed_out: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no string {key} in {}", v.encode()))
}

fn int_of(v: &Value, key: &str) -> i64 {
    v.get(key)
        .and_then(Value::as_i64)
        .unwrap_or_else(|| panic!("no integer {key} in {}", v.encode()))
}

fn error_code(v: &Value) -> i64 {
    v.get("error")
        .map(|e| int_of(e, "code"))
        .unwrap_or_else(|| panic!("not an error: {}", v.encode()))
}

/// A scratch file or directory under the host `root/tmp`, removed on drop.
/// The emu sees it as `#U/tmp/<name>`; a session must not.
struct HostTemp {
    name: String,
    path: PathBuf,
}

impl HostTemp {
    fn new(tag: &str, vm: &str) -> HostTemp {
        let name = format!("mcp-{tag}-{vm}-{}", std::process::id());
        let path = root().join("tmp").join(&name);
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_file(&path);
        HostTemp { name, path }
    }
}

impl Drop for HostTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn handshake_discovery_and_tools_list() {
    for vm in VMS {
        // A client of the handshake revisions.
        let mut m = Mcp::raw(vm, &[]);
        let r = m.request(
            "initialize",
            r#"{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}"#,
        );
        assert_eq!(str_of(&r, "protocolVersion"), "2025-06-18", "{vm}");
        assert_eq!(str_of(r.get("serverInfo").unwrap(), "name"), "mcpd");
        assert_eq!(
            r.get("capabilities").unwrap().encode(),
            r#"{"tools":{"listChanged":false},"resources":{"subscribe":false,"listChanged":false}}"#
        );
        assert_eq!(str_of(&r, "instructions"), PRIMER, "{vm}");
        // A notification gets no reply: the next line answers the ping.
        m.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!(m.request("ping", "{}").encode(), "{}", "{vm}");
        let r = m.request("tools/list", "{}");
        let Some(Value::Array(tools)) = r.get("tools") else {
            panic!("{vm}: no tools")
        };
        let names: Vec<&str> = tools.iter().map(|t| str_of(t, "name")).collect();
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
            ],
            "{vm}"
        );
        for t in tools {
            assert!(str_of(t, "description").len() > 40);
            for schema in ["inputSchema", "outputSchema"] {
                assert_eq!(str_of(t.get(schema).unwrap(), "type"), "object");
            }
        }
        m.finish();

        // A client of the current, stateless revision: no handshake.
        let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"t","version":"0"},"io.modelcontextprotocol/clientCapabilities":{}}"#;
        let mut m = Mcp::raw(vm, &[]);
        let r = m.request("server/discover", &format!("{{{meta}}}"));
        assert_eq!(str_of(&r, "resultType"), "complete", "{vm}");
        assert!(
            r.get("supportedVersions")
                .unwrap()
                .encode()
                .starts_with(r#"["2026-07-28","#)
        );
        assert_eq!(str_of(&r, "instructions"), PRIMER);
        let r = m.request("tools/list", &format!("{{{meta}}}"));
        assert_eq!(str_of(&r, "resultType"), "complete");
        assert!(matches!(r.get("tools"), Some(Value::Array(t)) if t.len() == 16));
        let r = m.request(
            "tools/call",
            &format!(r#"{{"name":"run","arguments":{{"command":"echo stateless"}},{meta}}}"#),
        );
        assert_eq!(str_of(&r, "resultType"), "complete");
        assert_eq!(
            str_of(r.get("structuredContent").unwrap(), "stdout"),
            "stateless\n",
            "{vm}"
        );
        // Without the handshake or the metadata a request is malformed.
        let v = m.exchange("tools/list", "{}");
        assert_eq!(error_code(&v), -32602, "{vm}");
        m.finish();
    }
}

#[test]
fn run_reports_output_and_status() {
    for vm in VMS {
        let mut m = Mcp::start(vm, &[]);
        let r = m.run("echo out; echo err >[1=2]");
        assert_eq!(
            (r.stdout.as_str(), r.stderr.as_str()),
            ("out\n", "err\n"),
            "{vm}"
        );
        assert_eq!(r.status, "");
        assert!(!r.timed_out && !r.stdout_truncated && !r.stderr_truncated);

        // A failing command: Inferno's error text and status string.
        let r = m.run("echo before; cat /nonexistent");
        assert_eq!(r.stdout, "before\n", "{vm}");
        assert_eq!(r.stderr, "cat: /nonexistent: file does not exist\n", "{vm}");
        assert_eq!(r.status, "fail:errors", "{vm}");
        assert_eq!(m.run("exit 'my status'").status, "my status", "{vm}");
        // A syntax error and a missing command are reported the same way.
        let r = m.run("echo 'unterminated");
        assert!(r.status.starts_with("fail:syntax error"), "{vm}: {r:?}");
        let r = m.run("nosuchcommand");
        assert_eq!(r.stderr, "sh: nosuchcommand: file does not exist\n", "{vm}");

        // Standard input, the working directory, and what persists.
        let r = m.run_with("cat; pwd", r#""stdin":"line 1\nline 2\n","cwd":"/lib""#);
        assert_eq!(r.stdout, "line 1\nline 2\n/lib\n", "{vm}");
        assert_eq!(
            m.run("pwd; cat").stdout,
            "/tmp\n",
            "{vm}: empty stdin, cwd reset"
        );
        assert_eq!(m.run("cd /lib; x=kept; pwd").stdout, "/lib\n", "{vm}");
        assert_eq!(
            m.run("pwd; echo $x $home").stdout,
            "/tmp\nkept /tmp\n",
            "{vm}"
        );
        let e = m.fails("run", r#"{"command":"pwd","cwd":"/nonexistent"}"#);
        assert_eq!(e, "/nonexistent: file does not exist", "{vm}");
        // More input than a pipe holds, to a command that writes before it
        // has read it all.
        let big = "0123456789abcdef".repeat(16 * 1024);
        let r = m.run_with(
            "wc -c",
            &format!(r#""stdin":"{big}","max_output_bytes":1000"#),
        );
        assert_eq!(r.stdout.trim(), (big.len()).to_string(), "{vm}");

        // Output caps: the default and a given one, on each stream.
        let r = m.run("cat /bin/sh.wasm /bin/sh.wasm");
        assert!(r.stdout_truncated && !r.stderr_truncated, "{vm}");
        assert_eq!(r.status, "", "{vm}: the command still ran to its end");
        let r = m.run_with(
            "echo 0123456789; echo abcdefghij >[1=2]",
            r#""max_output_bytes":4"#,
        );
        assert_eq!(
            (r.stdout.as_str(), r.stderr.as_str()),
            ("0123", "abcd"),
            "{vm}"
        );
        assert!(r.stdout_truncated && r.stderr_truncated, "{vm}");
        let r = m.run_with("echo 0123", r#""max_output_bytes":5"#);
        assert!(r.stdout == "0123\n" && !r.stdout_truncated, "{vm}");

        // A timeout kills a spinning shell, and what a command started.
        let start = Instant::now();
        let r = m.run_with(
            "echo started; load std; while {} {}",
            r#""timeout_ms":1500"#,
        );
        assert!(r.timed_out, "{vm}: {r:?}");
        assert_eq!(r.stdout, "started\n", "{vm}");
        assert!(!r.status.is_empty(), "{vm}: {r:?}");
        assert!(start.elapsed() < Duration::from_secs(60), "{vm}");
        let r = m.run_with(
            "sleep 300 & sleep 300; echo not reached",
            r#""timeout_ms":1000"#,
        );
        assert!(r.timed_out && r.stdout.is_empty(), "{vm}: {r:?}");
        let ps = m.run("ps").stdout;
        assert!(
            !ps.contains("sleep"),
            "{vm}: the group was not killed:\n{ps}"
        );
        assert_eq!(ps.matches(" sh\n").count(), 1, "{vm}:\n{ps}");
        // The session still works.
        assert_eq!(m.run("echo alive").stdout, "alive\n", "{vm}");

        // A background prog does not hold the call open, and lives on.
        let start = Instant::now();
        let r = m.run("sleep 300 > /dev/null >[2] /dev/null & echo bg");
        assert_eq!(r.stdout, "bg\n", "{vm}");
        assert!(start.elapsed() < Duration::from_secs(60), "{vm}");
        assert!(m.run("ps").stdout.contains("sleep"), "{vm}");
        m.finish();
    }
}

#[test]
fn file_tools_are_exact_and_binary_safe() {
    for vm in VMS {
        let mut m = Mcp::start(vm, &[]);
        // Every byte value, written and read back as base64.
        let all: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let b64 = json::base64_encode(&all);
        let w = m.ok(
            "write_file",
            &format!(r#"{{"path":"bin.dat","content":"{b64}","encoding":"base64"}}"#),
        );
        assert_eq!(
            (int_of(&w, "bytes"), int_of(&w, "size")),
            (1000, 1000),
            "{vm}"
        );
        let r = m.ok("read_file", r#"{"path":"/tmp/bin.dat"}"#);
        assert_eq!(str_of(&r, "encoding"), "base64", "{vm}");
        assert_eq!(
            json::base64_decode(str_of(&r, "content")).unwrap(),
            all,
            "{vm}"
        );
        assert_eq!((int_of(&r, "bytes"), int_of(&r, "size")), (1000, 1000));
        assert_eq!(r.get("eof"), Some(&Value::Bool(true)));
        // The shell sees the same bytes.
        assert_eq!(m.run("wc -c < bin.dat").stdout.trim(), "1000", "{vm}");
        // Offset and length.
        let r = m.ok("read_file", r#"{"path":"bin.dat","offset":254,"length":4}"#);
        assert_eq!(
            json::base64_decode(str_of(&r, "content")).unwrap(),
            [254, 255, 0, 1],
            "{vm}"
        );
        assert_eq!(r.get("eof"), Some(&Value::Bool(false)));
        let r = m.ok("read_file", r#"{"path":"bin.dat","offset":2000}"#);
        assert_eq!(
            (int_of(&r, "bytes"), r.get("eof")),
            (0, Some(&Value::Bool(true)))
        );

        // Text comes back as text; a read never splits a character.
        m.ok(
            "write_file",
            r#"{"path":"t.txt","content":"héllo\nwörld\n"}"#,
        );
        let r = m.ok("read_file", r#"{"path":"t.txt"}"#);
        assert_eq!(str_of(&r, "encoding"), "utf8", "{vm}");
        assert_eq!(str_of(&r, "content"), "héllo\nwörld\n", "{vm}");
        assert_eq!(int_of(&r, "bytes"), 14);
        let r = m.ok("read_file", r#"{"path":"t.txt","length":2}"#);
        assert_eq!(
            (str_of(&r, "content"), int_of(&r, "bytes")),
            ("h", 1),
            "{vm}"
        );
        let r = m.ok("read_file", r#"{"path":"t.txt","offset":1,"length":3}"#);
        assert_eq!(
            (str_of(&r, "content"), int_of(&r, "bytes")),
            ("él", 3),
            "{vm}"
        );
        let r = m.ok(
            "read_file",
            r#"{"path":"t.txt","encoding":"base64","length":3}"#,
        );
        assert_eq!(str_of(&r, "content"), json::base64_encode("hé".as_bytes()));
        assert_eq!(m.run("cat t.txt").stdout, "héllo\nwörld\n", "{vm}");

        // create, truncate and append.
        let e = m.fails(
            "write_file",
            r#"{"path":"t.txt","content":"x","mode":"create"}"#,
        );
        assert_eq!(e, "t.txt: file already exists", "{vm}");
        m.ok(
            "write_file",
            r#"{"path":"n.txt","content":"one\n","mode":"create"}"#,
        );
        let w = m.ok(
            "write_file",
            r#"{"path":"n.txt","content":"two\n","mode":"append"}"#,
        );
        assert_eq!((int_of(&w, "bytes"), int_of(&w, "size")), (4, 8), "{vm}");
        m.ok(
            "write_file",
            r#"{"path":"new.txt","content":"a","mode":"append"}"#,
        );
        assert_eq!(m.run("cat n.txt new.txt").stdout, "one\ntwo\na", "{vm}");
        m.ok("write_file", r#"{"path":"n.txt","content":"short"}"#);
        assert_eq!(m.run("cat n.txt").stdout, "short", "{vm}");
        let e = m.fails("write_file", r#"{"path":"nodir/x","content":""}"#);
        assert_eq!(e, "nodir/x: file does not exist", "{vm}");
        let e = m.fails(
            "write_file",
            r#"{"path":"x","content":"!!","encoding":"base64"}"#,
        );
        assert!(e.contains("base64"), "{vm}: {e}");

        // list_dir, stat and remove.
        m.run("mkdir -p d/sub; echo hi > d/f; echo x > d/sub/g; chmod 600 d/f");
        let l = m.ok("list_dir", r#"{"path":"d"}"#);
        assert_eq!(l.get("truncated"), Some(&Value::Bool(false)));
        let Some(Value::Array(entries)) = l.get("entries") else {
            panic!("{vm}: no entries")
        };
        let seen: Vec<(&str, &str, i64, &str)> = entries
            .iter()
            .map(|e| {
                (
                    str_of(e, "name"),
                    str_of(e, "type"),
                    int_of(e, "size"),
                    str_of(e, "mode"),
                )
            })
            .collect();
        assert_eq!(seen[0], ("f", "file", 3, "600"), "{vm}");
        assert_eq!((seen[1].0, seen[1].1), ("sub", "dir"), "{vm}");
        assert_eq!(seen.len(), 2);
        assert!(entries.iter().all(|e| int_of(e, "mtime") >= 0));
        let s = m.ok("stat", r#"{"path":"d/f"}"#);
        assert_eq!(
            (str_of(&s, "name"), str_of(&s, "type"), int_of(&s, "size")),
            ("f", "file", 3),
            "{vm}"
        );
        assert_eq!(str_of(&s, "path"), "/tmp/d/f", "{vm}");
        assert!(!str_of(&s, "uid").is_empty());
        assert_eq!(str_of(&m.ok("stat", r#"{"path":"/bin"}"#), "type"), "dir");
        for (tool, args, want) in [
            (
                "stat",
                r#"{"path":"/tmp/none"}"#,
                "/tmp/none: file does not exist",
            ),
            (
                "read_file",
                r#"{"path":"none"}"#,
                "none: file does not exist",
            ),
            ("read_file", r#"{"path":"d"}"#, "d: file is a directory"),
            (
                "list_dir",
                r#"{"path":"none"}"#,
                "none: file does not exist",
            ),
            ("list_dir", r#"{"path":"d/f"}"#, "d/f: not a directory"),
            ("remove", r#"{"path":"none"}"#, "none: file does not exist"),
            ("remove", r#"{"path":"d"}"#, "d: directory not empty"),
        ] {
            assert_eq!(m.fails(tool, args), want, "{vm}: {tool} {args}");
        }
        let r = m.ok("remove", r#"{"path":"d/f"}"#);
        assert_eq!(int_of(&r, "removed"), 1, "{vm}");
        let r = m.ok("remove", r#"{"path":"d","recursive":true}"#);
        assert_eq!(int_of(&r, "removed"), 3, "{vm}");
        assert_eq!(m.fails("stat", r#"{"path":"d"}"#), "d: file does not exist");
        m.finish();
    }
}

#[test]
fn the_session_is_confined() {
    for vm in VMS {
        // A file the emu serves from the host, outside /bin and /lib.
        let host = HostTemp::new("secret", vm);
        std::fs::write(&host.path, "host only\n").unwrap();
        let name = &host.name;
        let mut m = Mcp::start(vm, &[]);

        // The root holds mount points only, and /tmp is private and empty.
        let r = m.run("ls /; ls /tmp /net /chan /n /mnt");
        assert_eq!(
            r.stdout, "/bin\n/chan\n/dev\n/env\n/fd\n/lib\n/mnt\n/n\n/net\n/prog\n/tmp\n",
            "{vm}: {r:?}"
        );
        let r = m.run(&format!("cat /tmp/{name}"));
        assert_eq!(r.stdout, "", "{vm}");
        assert_eq!(
            r.stderr,
            format!("cat: /tmp/{name}: file does not exist\n"),
            "{vm}"
        );
        let e = m.fails("read_file", &format!(r#"{{"path":"/tmp/{name}"}}"#));
        assert_eq!(e, format!("/tmp/{name}: file does not exist"), "{vm}");

        // No device can be attached once the namespace is built: not the
        // host tree, host commands or the network, by sh or by a tool.
        for dev in ["#U", "#C", "#I", "#c"] {
            let r = m.run(&format!("bind '{dev}' /n; ls /n; cat '{dev}/tmp/{name}'"));
            assert_eq!(r.stdout, "", "{vm} {dev}: {r:?}");
            assert!(
                r.stderr.starts_with(&format!(
                    "bind: cannot bind {dev} on /n: mount/attach disallowed\n"
                )),
                "{vm} {dev}: {r:?}"
            );
            let e = m.fails("read_file", &format!(r#"{{"path":"{dev}/tmp/{name}"}}"#));
            assert_eq!(
                e,
                format!("{dev}/tmp/{name}: mount/attach disallowed"),
                "{vm}"
            );
            let e = m.fails("list_dir", &format!(r#"{{"path":"{dev}"}}"#));
            assert_eq!(e, format!("{dev}: mount/attach disallowed"), "{vm}");
        }
        let r = m.run("ls /cmd; cat /net/tcp/clone; os echo escaped");
        assert_eq!(r.stdout, "", "{vm}: {r:?}");
        assert!(
            r.stderr.contains("/net/tcp/clone: file does not exist"),
            "{vm}: {r:?}"
        );
        assert!(
            r.stderr.contains("ls: /cmd: file does not exist"),
            "{vm}: {r:?}"
        );

        // The console is not the protocol stream: writes vanish and reads
        // see end of file. (The harness would fail on a stray line.)
        let r = m.run("echo stray > /dev/cons; cat /dev/cons; echo done");
        assert_eq!(
            (r.stdout.as_str(), r.status.as_str()),
            ("done\n", ""),
            "{vm}: {r:?}"
        );
        assert_eq!(m.run("ls /dev").stdout.matches('\n').count(), 9, "{vm}");

        // /bin and /lib can be read and run but not changed.
        let r = m.run(
            "echo x > /bin/new; echo x > /lib/new; echo x > /bin/cat.wasm; \
             echo x >> /lib/probe/hello.txt; rm /bin/cat.wasm; mkdir /lib/d; \
             mv /bin/cat.wasm /bin/c.wasm; chmod 777 /bin/cat.wasm; cat /lib/probe/hello.txt",
        );
        assert_eq!(r.stdout, "hello from the probe root\n", "{vm}: {r:?}");
        let errs: Vec<&str> = r.stderr.lines().collect();
        assert_eq!(errs.len(), 8, "{vm}: {r:?}");
        for e in &errs {
            assert!(
                e.ends_with("permission denied")
                    || e.ends_with("mounted directory forbids creation"),
                "{vm}: {e}"
            );
        }
        for (tool, args) in [
            ("write_file", r#"{"path":"/bin/cat.wasm","content":"x"}"#),
            (
                "write_file",
                r#"{"path":"/lib/probe/hello.txt","content":"x","mode":"append"}"#,
            ),
            ("write_file", r#"{"path":"/bin/new","content":"x"}"#),
            ("remove", r#"{"path":"/lib/probe/hello.txt"}"#),
            ("remove", r#"{"path":"/lib/probe","recursive":true}"#),
        ] {
            let e = m.fails(tool, args);
            assert!(
                e.ends_with("permission denied")
                    || e.ends_with("mounted directory forbids creation"),
                "{vm}: {tool} {args}: {e}"
            );
        }
        assert!(
            root().join("bin/cat.wasm").metadata().unwrap().len() > 1000,
            "{vm}"
        );
        assert_eq!(
            std::fs::read_to_string(root().join("lib/probe/hello.txt")).unwrap(),
            "hello from the probe root\n",
            "{vm}"
        );
        assert!(!root().join("bin/new").exists() && !root().join("lib/new").exists());

        // Nothing the session wrote reached the host's /tmp.
        m.run("echo mine > /tmp/session-file");
        assert!(!root().join("tmp/session-file").exists(), "{vm}");
        assert_eq!(std::fs::read_to_string(&host.path).unwrap(), "host only\n");
        m.finish();
    }
}

#[test]
fn a_namespace_file_grants_a_workspace() {
    for vm in VMS {
        let work = HostTemp::new("work", vm);
        std::fs::create_dir(&work.path).unwrap();
        std::fs::write(work.path.join("in.txt"), "from the host\n").unwrap();
        let policy = HostTemp::new("policy", vm);
        std::fs::write(
            &policy.path,
            format!(
                "# grant one host directory\nbind -c '#U/tmp/{}' /n/work\n\
                 # a mount point, bound whole (#97)\nbind /lib /n/lib\n\
                 bind -a $home /mnt/home/x\n",
                work.name
            ),
        )
        .unwrap();
        let mut m = Mcp::start(vm, &["-n", &format!("/tmp/{}", policy.name)]);
        let r = m.run("cat /n/work/in.txt; echo out > /n/work/out.txt; ls /n");
        assert_eq!(r.stdout, "from the host\n/n/lib\n/n/work\n", "{vm}: {r:?}");
        // /lib is an export mounted on the session's root; binding it binds
        // what is mounted, not the empty directory under it.
        let names = |m: &mut Mcp, dir: &str| {
            let v = m.ok("list_dir", &format!(r#"{{"path":"{dir}"}}"#));
            v.get("entries").unwrap().encode()
        };
        let lib = names(&mut m, "/lib");
        assert!(lib.contains(r#""name":"probe""#), "{vm}: {lib}");
        assert_eq!(names(&mut m, "/n/lib"), lib, "{vm}");
        // Variables are expanded, and missing mount points made.
        assert_eq!(m.run("ls -d /mnt/home/x").stdout, "/mnt/home/x\n", "{vm}");
        assert_eq!(
            std::fs::read_to_string(work.path.join("out.txt")).unwrap(),
            "out\n",
            "{vm}"
        );
        // Only that directory was granted.
        let r = m.run("bind '#U' /mnt; cat /tmp/in.txt");
        assert!(r.stderr.contains("mount/attach disallowed"), "{vm}: {r:?}");
        m.ok("reset", "{}");
        assert_eq!(
            m.run("cat /n/work/out.txt").stdout,
            "out\n",
            "{vm}: granted again"
        );
        m.finish();

        // A policy that cannot be applied is reported by every call.
        std::fs::write(&policy.path, "bind /nonexistent /n/work\n").unwrap();
        let mut m = Mcp::start(vm, &["-n", &format!("/tmp/{}", policy.name)]);
        let e = m.fails("run", r#"{"command":"echo hi"}"#);
        assert!(e.contains("namespace file"), "{vm}: {e}");
        m.finish();
    }
}

#[test]
fn reset_and_recovery() {
    for vm in VMS {
        let mut m = Mcp::start(vm, &[]);
        m.run("echo data > f; v=set; mkdir /tmp/u; bind /tmp/u /n; sleep 300 > /dev/null &");
        let before = m.run("cat f; echo $v; ns | grep -c '/tmp/u /n'; ps | grep sleep | wc -l");
        assert_eq!(before.stdout, "data\nset\n1\n      1\n", "{vm}");
        let r = m.ok("reset", "{}");
        assert_eq!(r.get("reset"), Some(&Value::Bool(true)), "{vm}");
        let after = m.run("ls /tmp; echo $#v; ns | grep -c '/tmp/u /n'; ps | grep sleep | wc -l");
        assert_eq!(after.stdout, "0\n0\n      0\n", "{vm}: {after:?}");
        // The old session's progs are gone: the front end, its relay, one
        // session prog, two memfs, two exports, and this command.
        let ps = m.run("ps").stdout;
        assert_eq!(ps.matches("mcpd\n").count(), 3, "{vm}:\n{ps}");
        assert_eq!(ps.matches("memfs\n").count(), 2, "{vm}:\n{ps}");
        assert_eq!(ps.matches("export\n").count(), 2, "{vm}:\n{ps}");

        // A command can kill the session's own server prog. The call says
        // so, and the next one finds a new session.
        m.run("echo x > survivor");
        let e = m.fails(
            "run",
            r#"{"command":"load std; l=`{ps | grep ' sh$'}; echo kill > /prog/${index 2 $l}/ctl; sleep 30"}"#,
        );
        if e.contains("the session ended") {
            assert_eq!(m.run("ls /tmp; echo fresh").stdout, "fresh\n", "{vm}");
        } else {
            panic!("{vm}: {e}");
        }
        m.finish();
    }
}

#[test]
fn malformed_and_unknown_requests() {
    for vm in VMS {
        let mut m = Mcp::start(vm, &[]);
        m.send("{this is not json");
        let v = m.recv();
        assert_eq!(error_code(&v), -32700, "{vm}");
        assert_eq!(v.get("id"), Some(&Value::Null), "{vm}");
        m.send_bytes(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"\xff\xfe\"}");
        assert_eq!(error_code(&m.recv()), -32700, "{vm}: invalid UTF-8");
        m.send("[]");
        assert_eq!(error_code(&m.recv()), -32600, "{vm}");
        m.send(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#);
        assert_eq!(error_code(&m.recv()), -32600, "{vm}");
        // Blank lines and unknown notifications are passed over.
        m.send("");
        m.send(r#"{"jsonrpc":"2.0","method":"notifications/nosuch"}"#);
        m.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":99}}"#);
        for method in ["prompts/get", "resources/subscribe", "nosuch"] {
            let v = m.exchange(method, "{}");
            assert_eq!(error_code(&v), -32601, "{vm}: {method}");
        }
        let v = m.exchange("tools/call", r#"{"name":"nosuch","arguments":{}}"#);
        assert_eq!(error_code(&v), -32602, "{vm}");
        // Bad arguments are for the model to see and fix.
        let e = m.fails("run", r#"{"cmd":"echo hi"}"#);
        assert_eq!(e, "arguments.command is required", "{vm}");
        let e = m.fails("run", r#"{"command":"echo hi","timeout_ms":"soon"}"#);
        assert_eq!(
            e, "arguments.timeout_ms must be of type integer, not string",
            "{vm}"
        );
        // A string id is echoed, and the server is still healthy.
        m.send(r#"{"jsonrpc":"2.0","id":"last","method":"ping"}"#);
        assert_eq!(m.recv().get("id"), Some(&Value::from("last")), "{vm}");
        assert_eq!(m.run("echo still here").stdout, "still here\n", "{vm}");
        m.finish();
    }
}

/// Every example in the primer the server sends to models does what the
/// primer says.
#[test]
fn primer_examples_are_true() {
    let mut examples: Vec<(&str, String)> = Vec::new();
    let mut open = false;
    for line in PRIMER.lines() {
        if let Some(cmd) = line.strip_prefix("; ") {
            examples.push((cmd, String::new()));
            open = true;
        } else if let (true, Some(out)) = (open, line.strip_prefix("  ")) {
            let last = examples.last_mut().unwrap();
            last.1.push_str(out);
            last.1.push('\n');
        } else {
            open = false;
        }
    }
    assert!(examples.len() >= 12, "{examples:?}");
    for vm in VMS {
        let mut m = Mcp::start(vm, &[]);
        for (cmd, want) in &examples {
            let r = m.run(cmd);
            assert_eq!(&format!("{}{}", r.stderr, r.stdout), want, "{vm}: {cmd}");
        }
        // The claims in the prose.
        for cmd in ["awk", "find", "head", "python", "git"] {
            let e = m.fails("stat", &format!(r#"{{"path":"/bin/{cmd}.wasm"}}"#));
            assert!(e.ends_with("file does not exist"), "{vm}: {e}");
        }
        let listed = PRIMER
            .lines()
            .find_map(|l| l.split_once("Among them: "))
            .and_then(|(_, rest)| rest.split_once('.'))
            .expect("the command list")
            .0;
        for cmd in listed.split_whitespace() {
            m.ok("stat", &format!(r#"{{"path":"/bin/{cmd}.wasm"}}"#));
        }
        m.finish();
    }
}

/// Reads job `id` until it has exited; returns the last result and all of
/// stdout and stderr read on the way.
fn read_to_exit(m: &mut Mcp, id: i64) -> (Value, String, String) {
    let (mut out, mut err) = (String::new(), String::new());
    loop {
        let v = m.ok("job_read", &format!(r#"{{"id":{id},"wait_ms":20000}}"#));
        out.push_str(str_of(&v, "stdout"));
        err.push_str(str_of(&v, "stderr"));
        if v.get("running") == Some(&Value::Bool(false)) {
            return (v, out, err);
        }
    }
}

#[test]
fn jobs_run_alongside_other_calls() {
    for vm in VMS {
        let mut m = Mcp::start(vm, &[]);
        let job = m.ok("job_start", r#"{"argv":["cat"]}"#);
        let id = int_of(&job, "id");
        assert!(int_of(&job, "pid") > 0, "{vm}");

        // A read that waits for output does not hold up other calls, nor
        // does the job, blocked reading its input.
        let waiting = m.send_tool("job_read", &format!(r#"{{"id":{id},"wait_ms":60000}}"#));
        let started = Instant::now();
        let r = m.run("echo meanwhile");
        assert_eq!(r.stdout, "meanwhile\n", "{vm}");
        let f = m.ok("read_file", r#"{"path":"/lib/probe/hello.txt"}"#);
        assert_eq!(str_of(&f, "content"), "hello from the probe root\n");
        let l = m.ok("job_list", "{}");
        assert!(
            l.encode().contains(r#""command":"cat","running":true"#),
            "{vm}: {}",
            l.encode()
        );
        assert!(
            !m.early
                .iter()
                .any(|v| v.get("id") == Some(&Value::Int(waiting))),
            "{vm}: the waiting read was answered early"
        );
        let w = m.ok("job_write", &format!(r#"{{"id":{id},"data":"hello\n"}}"#));
        assert_eq!(int_of(&w, "queued"), 6, "{vm}");
        let v = m.tool_result(waiting);
        assert!(started.elapsed() < Duration::from_secs(50), "{vm}");
        assert_eq!(str_of(&v, "stdout"), "hello\n", "{vm}: {}", v.encode());
        assert_eq!((int_of(&v, "offset"), int_of(&v, "next_offset")), (0, 6));
        assert_eq!(v.get("running"), Some(&Value::Bool(true)), "{vm}");

        // The next read starts where that one ended; offsets read again.
        let v = m.ok("job_read", &format!(r#"{{"id":{id}}}"#));
        assert_eq!(
            (str_of(&v, "stdout"), int_of(&v, "offset")),
            ("", 6),
            "{vm}"
        );
        let w = m.ok(
            "job_write",
            &format!(r#"{{"id":{id},"data":"Ynll","encoding":"base64","eof":true}}"#),
        );
        assert_eq!(w.get("eof"), Some(&Value::Bool(true)), "{vm}");
        let (v, out, _) = read_to_exit(&mut m, id);
        assert_eq!(out, "bye", "{vm}");
        assert_eq!(
            (str_of(&v, "status"), v.get("ok")),
            ("", Some(&Value::Bool(true)))
        );
        let v = m.ok("job_read", &format!(r#"{{"id":{id},"offset":0}}"#));
        assert_eq!(str_of(&v, "stdout"), "hello\nbye", "{vm}");
        let e = m.fails("job_write", &format!(r#"{{"id":{id},"data":"x"}}"#));
        assert_eq!(e, format!("job {id} has exited"), "{vm}");
        assert_eq!(m.fails("job_read", r#"{"id":999}"#), "job 999: no such job");

        // An sh command line, with stderr and a failing status.
        let j = m.ok(
            "job_start",
            r#"{"command":"echo out; echo err >[1=2]; cat /nonexistent","stdin":"unused"}"#,
        );
        let (v, out, err) = read_to_exit(&mut m, int_of(&j, "id"));
        assert_eq!(out, "out\n", "{vm}");
        assert_eq!(err, "err\ncat: /nonexistent: file does not exist\n", "{vm}");
        assert!(!str_of(&v, "status").is_empty(), "{vm}: {}", v.encode());
        assert_eq!(v.get("ok"), Some(&Value::Bool(false)));

        // Input given at the start, then end of file.
        let j = m.ok("job_start", r#"{"argv":["wc","-c"],"stdin":"abc"}"#);
        let jid = int_of(&j, "id");
        m.ok("job_write", &format!(r#"{{"id":{jid},"eof":true}}"#));
        let (_, out, _) = read_to_exit(&mut m, jid);
        assert_eq!(out.trim(), "3", "{vm}");

        // A killed job reports it, and everything it started is gone.
        let j = m.ok("job_start", r#"{"command":"sleep 300 & sleep 300"}"#);
        let kid = int_of(&j, "id");
        let start = Instant::now();
        while m.run("ps | grep -c sleep").stdout != "2\n" {
            assert!(start.elapsed() < Duration::from_secs(30), "{vm}");
        }
        let k = m.ok("job_kill", &format!(r#"{{"id":{kid}}}"#));
        assert_eq!(k.get("killed"), Some(&Value::Bool(true)), "{vm}");
        let (v, _, _) = read_to_exit(&mut m, kid);
        // sh dies of the kill wherever it was blocked (issue #102).
        assert_eq!(
            (str_of(&v, "status"), v.get("ok")),
            ("killed", Some(&Value::Bool(false))),
            "{vm}: {}",
            v.encode()
        );
        let k = m.ok("job_kill", &format!(r#"{{"id":{kid}}}"#));
        assert_eq!(k.get("killed"), Some(&Value::Bool(false)), "{vm}");
        assert_eq!(m.run("ps | grep -c sleep").stdout, "0\n", "{vm}");

        // Output past the bound: the oldest is dropped and counted, and the
        // job is not held up.
        let j = m.ok("job_start", r#"{"command":"cat /bin/*.wasm"}"#);
        let bid = int_of(&j, "id");
        let mut total = 0;
        loop {
            let v = m.ok(
                "job_read",
                &format!(r#"{{"id":{bid},"wait_ms":20000,"max_bytes":0}}"#),
            );
            if v.get("running") == Some(&Value::Bool(false)) {
                break;
            }
        }
        let l = m.ok("job_list", "{}");
        let Some(Value::Array(jobs)) = l.get("jobs") else {
            panic!("{vm}: {}", l.encode())
        };
        for j in jobs {
            if int_of(j, "id") == bid {
                total = int_of(j, "stdout_bytes");
            }
        }
        assert!(total > 2 << 20, "{vm}: {total}");
        let v = m.ok(
            "job_read",
            &format!(r#"{{"id":{bid},"offset":0,"max_bytes":10}}"#),
        );
        assert_eq!(int_of(&v, "lost"), total - (1 << 20), "{vm}");
        assert_eq!(int_of(&v, "offset"), total - (1 << 20), "{vm}");
        assert!(int_of(&v, "next_offset") <= int_of(&v, "offset") + 10);

        // Jobs end with reset.
        m.ok("job_start", r#"{"command":"sleep 300"}"#);
        assert_eq!(m.run("ps | grep -c sleep").stdout, "1\n", "{vm}");
        m.ok("reset", "{}");
        assert_eq!(m.run("ps | grep -c sleep").stdout, "0\n", "{vm}");
        let l = m.ok("job_list", "{}");
        assert_eq!(l.encode(), r#"{"jobs":[]}"#, "{vm}");
        // And with the session: the harness checks the server exits.
        m.ok("job_start", r#"{"argv":["cat"]}"#);
        m.finish();
    }
}

#[test]
fn namespace_tools() {
    for vm in VMS {
        let mut m = Mcp::start(vm, &[]);
        let ns = m.ok("ns", "{}");
        let text = str_of(&ns, "ns");
        assert_eq!(text, m.run("ns").stdout, "{vm}");
        assert!(text.contains(" /tmp\n"), "{vm}: {text}");

        m.run("mkdir /tmp/u /tmp/v; echo u > /tmp/u/f; echo v > /tmp/v/g");
        m.ok("bind", r#"{"from":"/tmp/u","to":"/n"}"#);
        m.ok(
            "bind",
            r#"{"from":"/tmp/v","to":"/n","order":"after","create":true}"#,
        );
        assert_eq!(m.run("ls /n").stdout, "/n/f\n/n/g\n", "{vm}");
        let ns = m.ok("ns", "{}");
        assert!(
            str_of(&ns, "ns").contains(" -ac /tmp/v /n"),
            "{vm}: {}",
            ns.encode()
        );
        // Binds last for later calls, as those made by commands do.
        m.ok("unmount", r#"{"from":"/tmp/u","to":"/n"}"#);
        assert_eq!(m.run("ls /n").stdout, "/n/g\n", "{vm}");
        m.ok("unmount", r#"{"to":"/n"}"#);
        assert_eq!(m.run("ls /n").stdout, "", "{vm}");
        let e = m.fails("unmount", r#"{"to":"/n"}"#);
        assert!(e.starts_with("unmount: /n: "), "{vm}: {e}");
        let e = m.fails("bind", r#"{"from":"/nonexistent","to":"/n"}"#);
        assert_eq!(
            e, "bind: cannot bind /nonexistent on /n: file does not exist",
            "{vm}"
        );

        // NODEVS holds for the tools too.
        for (dev, at) in [("#C", "/cmd"), ("#C", "/n"), ("#U", "/n"), ("#I", "/net")] {
            let e = m.fails("bind", &format!(r#"{{"from":"{dev}","to":"{at}"}}"#));
            assert!(
                e.contains("mount/attach disallowed"),
                "{vm} {dev} {at}: {e}"
            );
        }
        let e = m.fails("mount", r#"{"source":"tcp!127.0.0.1!564","to":"/n"}"#);
        assert!(e.starts_with("mount: "), "{vm}: {e}");

        // A file server started by the mount.
        m.ok(
            "mount",
            r#"{"source":"{memfs --serve}","to":"/mnt","create":true}"#,
        );
        m.ok("write_file", r#"{"path":"/mnt/x","content":"in memfs"}"#);
        assert_eq!(m.run("cat /mnt/x").stdout, "in memfs", "{vm}");
        m.ok("reset", "{}");
        assert_eq!(m.run("ls /mnt /n").stdout, "", "{vm}");
        m.finish();
    }
}

#[test]
fn resources_are_session_files() {
    for vm in VMS {
        let mut m = Mcp::start(vm, &[]);
        m.ok("write_file", r#"{"path":"a b.txt","content":"hello"}"#);
        m.run("mkdir d");
        m.ok(
            "write_file",
            r#"{"path":"d/c.bin","content":"AAEC/w==","encoding":"base64"}"#,
        );
        let r = m.request("resources/list", "{}");
        assert_eq!(
            r.get("resources").unwrap().encode(),
            r#"[{"uri":"purg:///tmp/a%20b.txt","name":"/tmp/a b.txt","size":5},{"uri":"purg:///tmp/d/c.bin","name":"/tmp/d/c.bin","size":4}]"#,
            "{vm}"
        );
        let read = |m: &mut Mcp, uri: &str| {
            let r = m.request("resources/read", &format!(r#"{{"uri":"{uri}"}}"#));
            let Some(Value::Array(c)) = r.get("contents") else {
                panic!("{}", r.encode())
            };
            c[0].clone()
        };
        assert_eq!(
            read(&mut m, "purg:///tmp/a%20b.txt").encode(),
            r#"{"uri":"purg:///tmp/a%20b.txt","mimeType":"text/plain","text":"hello"}"#
        );
        let c = read(&mut m, "purg:///tmp/d/c.bin");
        assert_eq!(str_of(&c, "blob"), "AAEC/w==", "{vm}");
        assert_eq!(str_of(&c, "mimeType"), "application/octet-stream");
        let c = read(&mut m, "purg:///tmp");
        assert_eq!(str_of(&c, "text"), "a b.txt\nd/\n", "{vm}");
        assert_eq!(str_of(&c, "mimeType"), "inode/directory");
        // Any file in the namespace, through the template.
        let c = read(&mut m, "purg:///lib/probe/hello.txt");
        assert_eq!(str_of(&c, "text"), "hello from the probe root\n");
        let c = read(&mut m, "purg:///dev/sysname");
        assert!(!str_of(&c, "text").is_empty(), "{vm}");
        let t = m.request("resources/templates/list", "{}");
        assert!(t.encode().contains("purg:///{+path}"), "{vm}");

        let v = m.exchange("resources/read", r#"{"uri":"purg:///tmp/nope"}"#);
        assert_eq!(error_code(&v), -32002, "{vm}");
        let v = m.exchange("resources/read", r#"{"uri":"/tmp/a"}"#);
        assert_eq!(error_code(&v), -32602, "{vm}");
        // A URI names a file from the root, never a device.
        let v = m.exchange("resources/read", r#"{"uri":"purg:///%23U/tmp"}"#);
        assert_eq!(error_code(&v), -32002, "{vm}: {}", v.encode());
        // Too big for a resource.
        m.run("cat /bin/sh.wasm /bin/sh.wasm /bin/sh.wasm /bin/sh.wasm /bin/sh.wasm > big");
        let v = m.exchange("resources/read", r#"{"uri":"purg:///tmp/big"}"#);
        assert_eq!(error_code(&v), -32603, "{vm}");
        assert!(v.encode().contains("read_file"), "{vm}");
        m.finish();
    }
}

#[test]
fn the_audit_log() {
    for vm in VMS {
        let log = HostTemp::new("log", vm);
        let mut m = Mcp::start(vm, &["-l", &format!("/tmp/{}", log.name)]);
        m.run("echo hi");
        m.fails("read_file", r#"{"path":"/tmp/nope"}"#);
        m.fails("run", r#"{"command":"x","timeout_ms":0}"#);
        let long = "y".repeat(5000);
        m.ok(
            "write_file",
            &format!(r#"{{"path":"f","content":"{long}"}}"#),
        );
        m.request("resources/read", r#"{"uri":"purg:///tmp/f"}"#);
        // The session cannot reach the log: not by name, nor as its fd 2.
        m.ok("write_file", r#"{"path":"/fd/2","content":"forged\n"}"#);
        let e = m.fails(
            "read_file",
            &format!(r##"{{"path":"#U/tmp/{}"}}"##, log.name),
        );
        assert!(e.contains("mount/attach disallowed"), "{vm}: {e}");
        m.ok("reset", "{}");
        m.finish();

        let text = std::fs::read_to_string(&log.path).unwrap();
        // The call is logged with its arguments; the write went nowhere.
        assert!(
            !text.lines().any(|l| l.starts_with("forged")),
            "{vm}:\n{text}"
        );
        let lines: Vec<&str> = text.lines().collect();
        let mut rest = Vec::new();
        for l in &lines {
            let (secs, what) = l.split_once(' ').unwrap();
            assert!(secs.parse::<u64>().unwrap() > 1_600_000_000, "{vm}: {l}");
            rest.push(what);
        }
        let shape: Vec<String> = rest
            .iter()
            .map(|w| {
                let f: Vec<&str> = w.splitn(5, ' ').collect();
                match f.as_slice() {
                    ["session", "start", pid] => {
                        assert!(pid.parse::<u32>().is_ok());
                        String::from("session start")
                    }
                    [kind, name, status, ms, args] => {
                        assert!(ms.strip_suffix("ms").unwrap().parse::<u64>().is_ok(), "{w}");
                        assert!(args.starts_with('{'), "{w}");
                        format!("{kind} {name} {status}")
                    }
                    _ => String::from(*w),
                }
            })
            .collect();
        assert_eq!(
            shape,
            [
                "session start",
                "tool run ok",
                "tool read_file error",
                "tool run invalid",
                "tool write_file ok",
                "resource resources/read ok",
                "tool write_file ok",
                "tool read_file error",
                "session reset",
                "session start",
                "tool reset ok",
                "session end",
            ],
            "{vm}:\n{text}"
        );
        assert!(
            text.contains(r#"tool run ok "#) && text.contains(r#"{"command":"echo hi"}"#),
            "{vm}:\n{text}"
        );
        // Long arguments are cut.
        assert!(text.contains("...(5000 bytes)"), "{vm}:\n{text}");
        assert!(lines.iter().all(|l| l.len() < 600), "{vm}");
    }
}
