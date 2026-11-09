//! Acceptance tests: run `emu` on the repository's `root/` on both WASM
//! backends. Requires `cargo xtask build-root` first.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const VMS: [&str; 2] = ["wasmtime", "wasmi"];

fn root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root");
    assert!(
        root.join("bin/echo.wasm").exists(),
        "guest binaries missing; run `cargo xtask build-root` first"
    );
    root
}

fn emu(vm: &str, args: &[&str], stdin: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_emu"))
        .arg("-r")
        .arg(root())
        .arg(format!("--vm={vm}"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn emu");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().expect("wait for emu")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn echo_hello_world() {
    for vm in VMS {
        let o = emu(vm, &["echo", "hello", "world"], b"");
        assert_eq!(stdout(&o), "hello world\n", "{vm}");
        assert!(o.status.success(), "{vm}: {o:?}");
    }
}

#[test]
fn cat_sysname_and_user() {
    let host = {
        let mut buf = [0u8; 256];
        // SAFETY: buf is valid for writes of its length.
        assert_eq!(
            unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) },
            0
        );
        let end = buf.iter().position(|&b| b == 0).unwrap();
        String::from_utf8_lossy(&buf[..end]).into_owned()
    };
    for vm in VMS {
        let sysname = emu(vm, &["cat", "/dev/sysname"], b"");
        assert_eq!(stdout(&sysname), host, "{vm}");
        let user = emu(vm, &["cat", "/dev/user"], b"");
        assert!(!stdout(&user).is_empty(), "{vm}");
        let both = emu(vm, &["cat", "/dev/sysname", "/dev/user"], b"");
        assert_eq!(stdout(&both), stdout(&sysname) + &stdout(&user), "{vm}");
        assert!(both.status.success(), "{vm}");
    }
}

#[test]
fn cat_nonexistent_uses_inferno_error() {
    for vm in VMS {
        let o = emu(vm, &["cat", "/nonexistent"], b"");
        assert_eq!(
            stdout(&o),
            "cat: /nonexistent: file does not exist\n",
            "{vm}"
        );
        assert!(!o.status.success(), "{vm}");
    }
}

#[test]
fn cat_copies_stdin() {
    for vm in VMS {
        let o = emu(vm, &["cat"], b"hi\nthere\n");
        assert_eq!(stdout(&o), "hi\nthere\n", "{vm}");
        assert!(o.status.success(), "{vm}");
    }
}

#[test]
fn abi_conformance() {
    for vm in VMS {
        let o = emu(vm, &["/bin/test/abi.wasm"], b"");
        let out = stdout(&o);
        assert!(o.status.success(), "{vm}:\n{out}");
        assert!(!out.contains("FAIL"), "{vm}:\n{out}");
        assert!(out.contains("ok bad_pointer_read"), "{vm}:\n{out}");
    }
}

/// Inferno names are byte-exact even on macOS's case-insensitive volumes.
#[test]
fn names_are_case_exact() {
    for vm in VMS {
        let ok = emu(vm, &["cat", "/lib/probe/hello.txt"], b"");
        assert_eq!(stdout(&ok), "hello from the probe root\n", "{vm}");
        let bad = emu(vm, &["cat", "/LIB/probe/hello.txt"], b"");
        assert_eq!(
            stdout(&bad),
            "cat: /LIB/probe/hello.txt: file does not exist\n",
            "{vm}"
        );
        assert!(!bad.status.success(), "{vm}");
    }
}

#[test]
fn emuinit_spawns_command() {
    for vm in VMS {
        let o = emu(vm, &["emuinit", "echo", "via", "emuinit"], b"");
        assert_eq!(stdout(&o), "via emuinit\n", "{vm}");
        assert!(o.status.success(), "{vm}");
        let failing = emu(vm, &["emuinit", "cat", "/nonexistent"], b"");
        assert!(!failing.status.success(), "{vm}");
        let missing = emu(vm, &["emuinit", "nosuch"], b"");
        assert!(!missing.status.success(), "{vm}");
        assert_eq!(
            stdout(&missing),
            "emuinit: /bin/nosuch.wasm: file does not exist\n",
            "{vm}"
        );
    }
}

#[test]
fn procs_conformance() {
    for vm in VMS {
        let o = emu(vm, &["/bin/test/procs.wasm"], b"");
        let out = stdout(&o);
        assert!(o.status.success(), "{vm}:\n{out}");
        assert!(!out.contains("FAIL"), "{vm}:\n{out}");
        assert!(out.contains("ok bind_and_unmount"), "{vm}:\n{out}");
    }
}

#[test]
fn stock_wasi_sees_the_namespace() {
    for vm in VMS {
        let o = emu(vm, &["/bin/test/wasi.wasm"], b"");
        let out = stdout(&o);
        assert!(
            o.status.success(),
            "{vm}:\n{out}\n{}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(out.contains("ok read"), "{vm}:\n{out}");
        assert!(out.contains("ok args"), "{vm}:\n{out}");
        assert!(!out.contains("FAIL"), "{vm}:\n{out}");
    }
}

#[test]
fn tcp_dial_export_and_os() {
    for vm in VMS {
        let o = emu(vm, &["/bin/test/net.wasm"], b"");
        let out = stdout(&o);
        assert!(
            o.status.success(),
            "{vm}:\n{out}\n{}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(out.contains("ok cs"), "{vm}:\n{out}");
        assert!(out.contains("ok os\n"), "{vm}:\n{out}");
        assert!(out.contains("ok dial"), "{vm}:\n{out}");
        assert!(out.contains("ok sock_accept"), "{vm}:\n{out}");
        assert!(out.contains("ok export"), "{vm}:\n{out}");
        assert!(
            !out.contains("net_skip"),
            "{vm}: the host has a network stack"
        );
    }
}

/// With no command, emu boots emuinit, which runs `sh -l` on the console.
#[test]
fn boots_to_login_shell() {
    for vm in VMS {
        let o = emu(vm, &[], b"echo $user\n");
        let user = stdout(&emu(vm, &["cat", "/dev/user"], b""));
        assert!(o.status.success(), "{vm}: {o:?}");
        assert_eq!(stdout(&o), format!("; {user}\n; "), "{vm}");
    }
}

/// Runs every `/lib/test/sh/*.sh` and compares its output with the `.out`
/// file next to it.
#[test]
fn sh_script_corpus() {
    let dir = root().join("lib/test/sh");
    let mut scripts: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".sh").map(String::from)
        })
        .collect();
    scripts.sort();
    assert!(scripts.len() >= 8, "{scripts:?}");
    for vm in VMS {
        for s in &scripts {
            let want = std::fs::read_to_string(dir.join(format!("{s}.out"))).unwrap();
            let o = emu(vm, &["sh", &format!("/lib/test/sh/{s}.sh")], b"");
            assert_eq!(stdout(&o), want, "{vm} {s}.sh");
            assert!(o.status.success(), "{vm} {s}.sh: {o:?}");
        }
    }
}

/// The plan's M2 session, typed at the login shell.
#[test]
fn interactive_session() {
    let session = [
        "ls -l /bin | wc -l",
        "x=hello",
        "echo $x > /env/x; cat /env/x",
        "rm -rf /tmp/ia /tmp/ib; mkdir /tmp/ia /tmp/ib",
        "echo 1 > /tmp/ia/one; echo 2 > /tmp/ib/two",
        "bind -b /tmp/ia /tmp/ib; ls /tmp/ib",
        "sleep 10 & ; echo bg",
        "echo a b c | cat | cat | wc -w",
        "unmount /tmp/ia /tmp/ib; rm -r /tmp/ia /tmp/ib",
    ];
    let input = session.join("\n") + "\n";
    let bins = std::fs::read_dir(root().join("bin")).unwrap().count();
    for vm in VMS {
        let start = std::time::Instant::now();
        let o = emu(vm, &[], input.as_bytes());
        let out = stdout(&o).replace("; ", "");
        assert!(o.status.success(), "{vm}: {o:?}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "{vm}: waited for the background sleep"
        );
        let want = format!("{bins:>7}\nhello\n/tmp/ib/one\n/tmp/ib/two\nbg\n      3\n");
        assert_eq!(out, want, "{vm}");
    }
}

#[test]
fn devices_are_listed() {
    for vm in VMS {
        let o = emu(vm, &["cat", "/dev/drivers"], b"");
        assert_eq!(
            stdout(&o),
            "#/ root\n#c cons\n#e env\n#| pipe\n#p prog\n#s srv\n#M mnt\n#d dup\n#% mem\n#I ip\n#C cmd\n#¤ factotum\n#D ssl\n#U fs\n",
            "{vm}"
        );
    }
}
