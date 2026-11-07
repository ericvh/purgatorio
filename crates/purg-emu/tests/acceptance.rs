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
fn boots_emuinit_without_command() {
    for vm in VMS {
        let o = emu(vm, &[], b"");
        assert!(o.status.success(), "{vm}: {o:?}");
        assert!(stdout(&o).contains("emuinit: no command"), "{vm}");
    }
}

#[test]
fn devices_are_listed() {
    for vm in VMS {
        let o = emu(vm, &["cat", "/dev/drivers"], b"");
        assert_eq!(
            stdout(&o),
            "#/ root\n#c cons\n#e env\n#| pipe\n#p prog\n#U fs\n",
            "{vm}"
        );
    }
}
