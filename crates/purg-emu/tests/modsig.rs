//! Module signing on the emu (`docs/signing.md`): `--modules` sets the
//! system policy and covers the first prog, modules signed on the host
//! verify inside the system, and the guest factotum signs.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use purg_auth::keys::list_line;
use purg_auth::modsig;

mod common;

const VMS: [&str; 2] = ["wasmtime", "wasmi"];
/// The public test seed, `root/lib/test/auth.key`.
const SEED: [u8; 32] = [0x42; 32];

fn root() -> PathBuf {
    std::env::var_os("PURGATORIO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../root"))
}

fn emu(root: &Path, vm: &str, args: &[&str]) -> Output {
    common::output(
        Command::new(env!("CARGO_BIN_EXE_emu"))
            .arg("-r")
            .arg(root)
            .arg(format!("--vm={vm}"))
            // The command's status, as the emu's.
            .arg("-x")
            .args(args),
        b"",
    )
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A small root: `echo`, `cat` and `auth/verify` signed with the test
/// key, `ls` unsigned, and the test key as the one module signer.
fn signed_root(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin/auth")).unwrap();
    std::fs::create_dir_all(dir.join("lib/keys")).unwrap();
    for m in ["echo", "cat", "auth/verify"] {
        let wasm = std::fs::read(root().join(format!("bin/{m}.wasm"))).unwrap();
        let signed = modsig::sign(&wasm, &SEED, Vec::new()).unwrap();
        assert_eq!(signed, modsig::sign(&wasm, &SEED, Vec::new()).unwrap());
        std::fs::write(dir.join(format!("bin/{m}.wasm")), signed).unwrap();
    }
    std::fs::copy(root().join("bin/ls.wasm"), dir.join("bin/ls.wasm")).unwrap();
    let key = purg_auth::public_key(&SEED);
    std::fs::write(dir.join("lib/keys/modules"), list_line(&key, "test")).unwrap();
    dir
}

#[test]
fn the_default_policy_runs_unsigned_modules() {
    for vm in VMS {
        let o = emu(&root(), vm, &["echo", "hi"]);
        assert_eq!(stdout(&o), "hi\n", "{vm}");
        let o = emu(&root(), vm, &["--modules=none", "cat", "/prog/1/module"]);
        let text = stdout(&o);
        let wasm = std::fs::read(root().join("bin/cat.wasm")).unwrap();
        let digest = purg_auth::hex::encode(&modsig::digest(&wasm).unwrap());
        assert_eq!(
            text,
            format!("policy none\nmodule {digest} - /bin/cat.wasm\n"),
            "{vm}"
        );
    }
}

#[test]
fn require_refuses_an_unsigned_first_prog() {
    for vm in VMS {
        let o = emu(&root(), vm, &["--modules=require", "echo", "hi"]);
        assert!(!o.status.success(), "{vm}");
        assert_eq!(stdout(&o), "", "{vm}");
        assert_eq!(
            stderr(&o),
            "emu: /bin/echo.wasm: module not signed\n",
            "{vm}"
        );
        let o = emu(&root(), vm, &["--modules=warn", "echo", "hi"]);
        assert!(o.status.success(), "{vm}");
        assert_eq!(
            stdout(&o),
            "/bin/echo.wasm: warning: module not signed\nhi\n",
            "{vm}"
        );
    }
    let o = emu(&root(), "wasmi", &["--modules=sometimes", "echo", "hi"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn require_runs_modules_signed_on_the_host() {
    let dir = signed_root("modsig-require");
    let key = purg_auth::hex::encode(&purg_auth::public_key(&SEED));
    for vm in VMS {
        let o = emu(&dir, vm, &["--modules=require", "echo", "signed"]);
        assert!(o.status.success(), "{vm}: {o:?}");
        assert_eq!(stdout(&o), "signed\n", "{vm}");
        // The first prog sees its own digest and signer.
        let o = emu(&dir, vm, &["--modules=require", "cat", "/prog/1/module"]);
        let wasm = std::fs::read(root().join("bin/cat.wasm")).unwrap();
        let digest = purg_auth::hex::encode(&modsig::digest(&wasm).unwrap());
        assert_eq!(
            stdout(&o),
            format!("policy require\nmodule {digest} {key} /bin/cat.wasm\n"),
            "{vm}"
        );
        let o = emu(&dir, vm, &["--modules=require", "ls", "/"]);
        assert!(!o.status.success(), "{vm}");
        assert_eq!(stderr(&o), "emu: /bin/ls.wasm: module not signed\n", "{vm}");
        // The guest's verifier agrees with the host's signer.
        let o = emu(&dir, vm, &["/bin/auth/verify.wasm", "-m", "/bin/echo.wasm"]);
        assert!(o.status.success(), "{vm}: {o:?}");
        assert!(
            stdout(&o).contains(&format!("signer {key} trusted\n")),
            "{vm}: {o:?}"
        );
        let o = emu(&dir, vm, &["/bin/auth/verify.wasm", "-m", "/bin/ls.wasm"]);
        assert!(!o.status.success(), "{vm}");
    }
    // A revoked signer stops the first prog.
    std::fs::write(
        dir.join("lib/keys/revoked"),
        list_line(&purg_auth::public_key(&SEED), ""),
    )
    .unwrap();
    let o = emu(&dir, "wasmi", &["--modules=require", "echo", "signed"]);
    assert_eq!(stderr(&o), "emu: /bin/echo.wasm: certificate revoked\n");
}

/// `/lib/test/sign/factotum.sh` against its `.out` file, on the emu only.
#[test]
fn signing_through_factotum() {
    let want = std::fs::read_to_string(root().join("lib/test/sign/factotum.out")).unwrap();
    for vm in VMS {
        let o = emu(&root(), vm, &["sh", "/lib/test/sign/factotum.sh"]);
        assert_eq!(stdout(&o), want, "{vm}");
        assert!(o.status.success(), "{vm}: {o:?}");
    }
}
