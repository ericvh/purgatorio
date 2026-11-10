//! `rcmd host mcpd`: an MCP session carried by the remote command
//! protocol (`docs/rcmd.md`), on both WASM backends. One emu runs both
//! ends on loopback: `auxi/rstyxd` behind `styxlisten`, and `rcmd` with
//! this test as its standard input and output, authenticated with the
//! test key. Requires `cargo xtask build-root` first.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

#[allow(dead_code)]
#[path = "../../../appl/cmd/mcpd/src/json.rs"]
mod json;

use json::Value;

const VMS: [&str; 2] = ["wasmtime", "wasmi"];
const REPLY: Duration = Duration::from_secs(120);

fn root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root");
    assert!(
        root.join("bin/rcmd.wasm").exists() && root.join("bin/auxi/rstyxd.wasm").exists(),
        "guest binaries missing; run `cargo xtask build-root` first"
    );
    root
}

/// Starts the listener, runs `rcmd` with `flags` and `cmd` against it,
/// and stops the listener when `rcmd` is done.
fn script(vm: &str, tag: &str, flags: &str, cmd: &str) -> String {
    format!(
        "load std
d=/tmp/rcmd-{tag}-{vm}
rm -rf $d
mkdir -p $d
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm >[2] $d/l &
lpid=$apid
while {{! test -s $d/l}} {{sleep 0}}
rcmd {flags} tcp!`{{sed 's/styxlisten: //' $d/l}} {cmd}
s=$status
kill $lpid
rm -rf $d
exit $s
"
    )
}

fn recv(vm: &str, lines: &Receiver<String>) -> Value {
    let line = lines
        .recv_timeout(REPLY)
        .unwrap_or_else(|e| panic!("{vm}: no reply: {e}"));
    json::parse(&line).unwrap_or_else(|e| panic!("{vm}: not JSON ({e}): {line:?}"))
}

#[test]
fn mcp_session_over_rcmd() {
    for vm in VMS {
        let mut child = Command::new(env!("CARGO_BIN_EXE_emu"))
            .arg("-r")
            .arg(root())
            .arg(format!("--vm={vm}"))
            .args(["sh", "-c", &script(vm, "mcp", "", "mcpd")])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn emu");
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.expect("utf-8")).is_err() {
                    break;
                }
            }
        });
        let mut send = |line: &str| {
            stdin.write_all(line.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
            stdin.flush().unwrap();
        };

        send(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
        );
        let v = recv(vm, &lines);
        assert_eq!(v.get("id"), Some(&Value::Int(1)), "{vm}: {}", v.encode());
        let result = v.get("result").unwrap_or_else(|| panic!("{vm}"));
        assert_eq!(
            result.get("protocolVersion").and_then(Value::as_str),
            Some("2025-11-25"),
            "{vm}"
        );
        send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

        send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
        let v = recv(vm, &lines);
        assert_eq!(v.get("id"), Some(&Value::Int(2)), "{vm}");
        let tools = v.get("result").and_then(|r| r.get("tools"));
        assert!(
            tools.is_some_and(|t| t.encode().contains(r#""name":"run""#)),
            "{vm}: {}",
            v.encode()
        );

        // A command in the remote session, and a file that outlives it.
        send(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run","arguments":{"command":"echo over rcmd > f; cat f; exit odd"}}}"#,
        );
        let v = recv(vm, &lines);
        assert_eq!(v.get("id"), Some(&Value::Int(3)), "{vm}");
        let s = v
            .get("result")
            .and_then(|r| r.get("structuredContent"))
            .unwrap_or_else(|| panic!("{vm}: {}", v.encode()));
        assert_eq!(
            s.get("stdout").and_then(Value::as_str),
            Some("over rcmd\n"),
            "{vm}"
        );
        assert_eq!(s.get("status").and_then(Value::as_str), Some("odd"), "{vm}");
        send(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"/tmp/f"}}}"#,
        );
        let v = recv(vm, &lines);
        assert_eq!(v.get("id"), Some(&Value::Int(4)), "{vm}");
        assert!(v.encode().contains("over rcmd"), "{vm}: {}", v.encode());

        // End of input ends mcpd, and with it rcmd, with a clean status.
        drop(stdin);
        let out = child.wait_with_output().expect("wait");
        assert!(
            out.status.success(),
            "{vm}: {:?}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            lines.recv_timeout(REPLY).is_err(),
            "{vm}: output after the session"
        );
    }
}

/// The exit status and the caller's files reach the other side, and a
/// client with no proof is refused.
#[test]
fn status_namespace_and_refusal() {
    for vm in VMS {
        let run = |flags: &str, cmd: &str, input: &str| {
            let mut child = Command::new(env!("CARGO_BIN_EXE_emu"))
                .arg("-r")
                .arg(root())
                .arg(format!("--vm={vm}"))
                .args(["sh", "-c", &script(vm, "st", flags, cmd)])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn emu");
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            let out = child.wait_with_output().expect("wait");
            // The emu's console carries both the guest's fd 1 and fd 2.
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            (out.status.success(), text)
        };
        let (ok, out) = run(
            "",
            "'cat; wc -c < /n/client/lib/test/auth.key.pub; echo oops >[1=2]; exit bad'",
            "from the caller\n",
        );
        assert!(!ok, "{vm}");
        assert_eq!(out, "from the caller\n     32\noops\n", "{vm}");
        let (ok, out) = run("-A", "echo no", "");
        assert!(!ok, "{vm}");
        assert_eq!(out, "rcmd: authentication required\n", "{vm}");
    }
}
