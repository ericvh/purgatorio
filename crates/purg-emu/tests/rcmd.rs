//! `rcmd host mcpd`: an MCP session carried by the remote command
//! protocol (`docs/rcmd.md`), on both WASM backends. One emu runs both
//! ends on loopback: `auxi/rstyxd` behind `styxlisten`, and `rcmd` with
//! this test as its standard input and output, authenticated with the
//! test key. Requires `cargo xtask build-root` first.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// What a relay between `rcmd` and `rstyxd` saw, each way.
#[derive(Default)]
struct Seen {
    to_server: Vec<u8>,
    to_client: Vec<u8>,
}

/// Accepts one connection on `l` and relays it to the port named in
/// `portfile` once that exists, recording both directions. With `flip`,
/// the byte at that offset of the server's stream is changed on the way.
fn relay(l: TcpListener, portfile: PathBuf, flip: Option<usize>) -> Arc<Mutex<Seen>> {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let out = seen.clone();
    std::thread::spawn(move || {
        let Ok((client, _)) = l.accept() else { return };
        let start = Instant::now();
        let port = loop {
            let text = std::fs::read_to_string(&portfile).unwrap_or_default();
            if let Some(p) = text
                .trim()
                .rsplit('!')
                .next()
                .and_then(|p| p.parse::<u16>().ok())
            {
                break p;
            }
            assert!(start.elapsed() < REPLY, "no port from styxlisten");
            std::thread::sleep(Duration::from_millis(20));
        };
        let server = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let pipe =
            move |mut from: TcpStream, mut to: TcpStream, up: bool, seen: Arc<Mutex<Seen>>| {
                let mut buf = [0u8; 4096];
                let mut at = 0usize;
                loop {
                    let n = match from.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    if !up
                        && let Some(f) = flip
                        && (at..at + n).contains(&f)
                    {
                        buf[f - at] ^= 0x40;
                    }
                    at += n;
                    let mut s = seen.lock().unwrap();
                    if up {
                        s.to_server.extend_from_slice(&buf[..n]);
                    } else {
                        s.to_client.extend_from_slice(&buf[..n]);
                    }
                    drop(s);
                    if to.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = to.shutdown(std::net::Shutdown::Write);
            };
        let (c2, s2) = (client.try_clone().unwrap(), server.try_clone().unwrap());
        let seen2 = seen.clone();
        let up = std::thread::spawn(move || pipe(client, server, true, seen2));
        pipe(s2, c2, false, seen);
        let _ = up.join();
    });
    out
}

/// Runs `cmd` with `rcmd` through a relay this test holds, so that it can
/// see the bytes on the connection. Returns the emu's output and status.
fn relayed(
    vm: &str,
    tag: &str,
    cmd: &str,
    flip: Option<usize>,
) -> (bool, String, Arc<Mutex<Seen>>) {
    let name = format!("rcmd-relay-{tag}-{vm}-{}", std::process::id());
    let host_dir = root().join("tmp").join(&name);
    let _ = std::fs::remove_dir_all(&host_dir);
    std::fs::create_dir_all(&host_dir).unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay_port = l.local_addr().unwrap().port();
    let seen = relay(l, host_dir.join("l"), flip);
    let script = format!(
        "load std
d=/tmp/{name}
styxlisten tcp!127.0.0.1!0 /bin/auxi/rstyxd.wasm >[2] $d/l.tmp &
lpid=$apid
while {{! test -s $d/l.tmp}} {{sleep 0}}
sed 's/styxlisten: //' $d/l.tmp > $d/l
rcmd tcp!127.0.0.1!{relay_port} {cmd} </dev/null
s=$status
kill $lpid
exit $s
"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_emu"))
        .arg("-r")
        .arg(root())
        .arg(format!("--vm={vm}"))
        .args(["sh", "-c", &script])
        .stdin(Stdio::null())
        .output()
        .expect("run emu");
    let _ = std::fs::remove_dir_all(&host_dir);
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text, seen)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// After the proof the connection is sealed: the command, its output and
/// the 9P conversation do not appear on the wire, and a byte changed on
/// the way ends the session with `bad record mac`.
#[test]
fn the_connection_is_encrypted() {
    for vm in VMS {
        let cmd = "'echo the-output-marker; cat /n/client/lib/probe/hello.txt'";
        let (ok, out, seen) = relayed(vm, "seal", cmd, None);
        assert!(ok, "{vm}: {out}");
        assert_eq!(
            out, "the-output-marker\nhello from the probe root\n",
            "{vm}"
        );
        let s = seen.lock().unwrap();
        // The opening is in the clear: HELLO names the protocol and user.
        assert!(contains(&s.to_server, b"pgrstyx2 "), "{vm}");
        for (what, marker) in [
            ("command", &b"the-output-marker"[..]),
            ("9P walk", b"hello.txt"),
            ("9P data", b"hello from the probe root"),
        ] {
            assert!(
                !contains(&s.to_server, marker),
                "{vm}: {what} in the clear to the server"
            );
            assert!(
                !contains(&s.to_client, marker),
                "{vm}: {what} in the clear to the client"
            );
        }
        // CHAL (5 + 64 bytes) and OK (5) open the server's stream; the
        // first record follows. Change a byte inside it.
        let first_record = 5 + 64 + 5;
        assert!(s.to_client.len() > first_record + 20, "{vm}");
        drop(s);
        let (ok, out, _) = relayed(vm, "flip", "'echo never'", Some(first_record + 8));
        assert!(!ok, "{vm}: {out}");
        assert!(out.contains("bad record mac"), "{vm}: {out}");
        assert!(!out.contains("never"), "{vm}: {out}");
    }
}

/// Kills a child process on drop.
struct Reaper(std::process::Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The host bridge of docs/mcp.md: an MCP client starts
/// `emu rcmd tcp!host!port mcpd` and speaks MCP on its standard input and
/// output, to a session on another system (here a second emu), over an
/// encrypted connection. Revision 2026-07-28, with its per-request
/// metadata.
#[test]
fn an_mcp_client_reaches_another_system() {
    for vm in VMS {
        let mut server = Reaper(
            Command::new(env!("CARGO_BIN_EXE_emu"))
                .arg("-r")
                .arg(root())
                .arg(format!("--vm={vm}"))
                .args(["styxlisten", "tcp!127.0.0.1!0", "/bin/auxi/rstyxd.wasm"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn the server emu"),
        );
        let mut first = String::new();
        BufReader::new(server.0.stdout.take().unwrap())
            .read_line(&mut first)
            .unwrap();
        let addr = first
            .trim()
            .strip_prefix("styxlisten: ")
            .unwrap_or_else(|| panic!("{vm}: {first:?}"));
        let mut client = Reaper(
            Command::new(env!("CARGO_BIN_EXE_emu"))
                .arg("-r")
                .arg(root())
                .arg(format!("--vm={vm}"))
                .args(["rcmd", &format!("tcp!{addr}"), "mcpd"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn the client emu"),
        );
        let mut stdin = client.0.stdin.take().unwrap();
        let stdout = client.0.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.expect("utf-8")).is_err() {
                    break;
                }
            }
        });
        let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"bridge","version":"0"},"io.modelcontextprotocol/clientCapabilities":{}}"#;
        let mut send = |line: String| {
            stdin.write_all(line.as_bytes()).unwrap();
            stdin.write_all(b"\n").unwrap();
            stdin.flush().unwrap();
        };
        send(format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{{{meta}}}}}"#
        ));
        let v = recv(vm, &lines);
        assert_eq!(v.get("id"), Some(&Value::Int(1)), "{vm}: {}", v.encode());
        assert!(
            v.encode().contains(r#""name":"mcpd""#),
            "{vm}: {}",
            v.encode()
        );
        send(format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"run","arguments":{{"command":"echo far; ls /n"}},{meta}}}}}"#
        ));
        let v = recv(vm, &lines);
        let s = v
            .get("result")
            .and_then(|r| r.get("structuredContent"))
            .unwrap_or_else(|| panic!("{vm}: {}", v.encode()));
        // The caller's namespace is on /n/client in rstyxd's namespace,
        // not in the session's.
        assert_eq!(
            s.get("stdout").and_then(Value::as_str),
            Some("far\n"),
            "{vm}"
        );
        drop(send);
        drop(stdin);
        let start = Instant::now();
        let status = loop {
            if let Some(st) = client.0.try_wait().unwrap() {
                break st;
            }
            assert!(start.elapsed() < REPLY, "{vm}: rcmd did not exit");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "{vm}: {status}");
        assert!(lines.recv_timeout(Duration::from_secs(2)).is_err(), "{vm}");
    }
}
