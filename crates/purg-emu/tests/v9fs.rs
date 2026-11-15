//! Linux's own 9P client against a Purgatorio export: the emu serves a
//! directory with `styxlisten -A ... export`, and the host kernel mounts
//! it (`mount -t 9p`, version 9P2000) and reads, writes, renames and
//! removes through it, on both WASM backends (issue #43).
//!
//! Mounting needs root, so this runs only where `sudo -n` works and the
//! kernel has 9p, as on CI's Linux runners; elsewhere it says so and
//! passes. `PURG_V9FS=1` (set by CI on Linux) makes a missing `sudo` or
//! 9p a failure instead. Requires `cargo xtask build-root` first.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const VMS: [&str; 2] = ["wasmtime", "wasmi"];

fn root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root");
    assert!(
        root.join("bin/styxlisten.wasm").exists() && root.join("bin/export.wasm").exists(),
        "guest binaries missing; run `cargo xtask build-root` first"
    );
    root
}

/// Why this host cannot mount 9P, if it cannot.
fn unavailable() -> Option<String> {
    let sudo = Command::new("sudo")
        .args(["-n", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !sudo.is_ok_and(|s| s.success()) {
        return Some(String::from("no passwordless sudo"));
    }
    let fs = std::fs::read_to_string("/proc/filesystems").unwrap_or_default();
    let module = Command::new("sudo")
        .args(["-n", "modprobe", "9p"])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !fs.lines().any(|l| l.ends_with("\t9p")) && !module {
        return Some(String::from("no 9p file system in this kernel"));
    }
    None
}

fn sudo(args: &[&str]) -> bool {
    Command::new("sudo")
        .arg("-n")
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}

/// The emu serving `/tmp/<dir>` on a loopback port, and that port.
/// styxlisten's line naming the port goes to a file next to the
/// directory (the emu's own standard error is buffered), which this
/// reads from the host side of the root.
fn serve(vm: &str, dir: &str) -> (Child, u16) {
    let script = format!(
        "mkdir -p /tmp/{dir}; echo hello > /tmp/{dir}/a; \
         styxlisten -A tcp!127.0.0.1!0 export /tmp/{dir} >[2] /tmp/{dir}.port"
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_emu"))
        .arg("-r")
        .arg(root())
        .arg(format!("--vm={vm}"))
        .args(["sh", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("emu");
    let file = root().join("tmp").join(format!("{dir}.port"));
    let start = Instant::now();
    let port = loop {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        if let Some(p) = text
            .lines()
            .find_map(|l| l.strip_prefix("styxlisten: 127.0.0.1!"))
        {
            break p.trim().parse().expect("port");
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            panic!("{vm}: styxlisten did not say where it listens: {text:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (child, port)
}

/// Unmounts, stops the emu and removes the served directory when
/// dropped, whatever the test did.
struct Mounted {
    at: PathBuf,
    emu: Child,
    host: PathBuf,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        let at = self.at.to_string_lossy().into_owned();
        if !sudo(&["umount", &at]) {
            sudo(&["umount", "-l", &at]);
        }
        let _ = self.emu.kill();
        let _ = self.emu.wait();
        let _ = std::fs::remove_dir_all(&self.host);
        let _ = std::fs::remove_file(self.host.with_extension("port"));
    }
}

fn check(vm: &str, m: &Path, host: &Path) {
    // Read and list.
    assert_eq!(
        std::fs::read_to_string(m.join("a")).unwrap(),
        "hello\n",
        "{vm}"
    );
    let names: Vec<String> = std::fs::read_dir(m)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["a"], "{vm}: the listing");
    assert_eq!(std::fs::metadata(m.join("a")).unwrap().len(), 6, "{vm}");
    // Inferno's error strings map to errnos in v9fs.
    let e = std::fs::read(m.join("nosuch")).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{vm}: {e}");
    // Create, write, append, make a directory, rename and remove: each
    // lands in the emu's tree on the host.
    std::fs::write(m.join("b"), "from linux\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(host.join("b")).unwrap(),
        "from linux\n",
        "{vm}"
    );
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(m.join("b"))
            .unwrap();
        f.write_all(b"more\n").unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(m.join("b")).unwrap(),
        "from linux\nmore\n",
        "{vm}"
    );
    // A rename in one directory is a wstat of the name. 9P2000 cannot
    // move a file to another directory, so Linux may refuse that (EXDEV)
    // and a copy does it instead, as mv would.
    std::fs::rename(m.join("b"), m.join("c")).unwrap();
    assert!(host.join("c").is_file() && !host.join("b").exists(), "{vm}");
    std::fs::create_dir(m.join("d")).unwrap();
    if let Err(e) = std::fs::rename(m.join("c"), m.join("d/c")) {
        assert_eq!(e.raw_os_error(), Some(18), "{vm}: rename across: {e}");
        std::fs::copy(m.join("c"), m.join("d/c")).unwrap();
        std::fs::remove_file(m.join("c")).unwrap();
    }
    assert!(host.join("d/c").is_file(), "{vm}: the moved file");
    assert!(!host.join("c").exists(), "{vm}: the old name");
    std::fs::remove_file(m.join("d/c")).unwrap();
    std::fs::remove_dir(m.join("d")).unwrap();
    assert!(!host.join("d").exists(), "{vm}: the removed directory");
}

#[test]
fn linux_v9fs_mounts_an_export() {
    if let Some(why) = unavailable() {
        assert!(
            std::env::var_os("PURG_V9FS").is_none(),
            "PURG_V9FS is set, but {why}"
        );
        eprintln!("v9fs: skipped: {why}");
        return;
    }
    for vm in VMS {
        let dir = format!("v9fs-{vm}-{}", std::process::id());
        let host = root().join("tmp").join(&dir);
        let _ = std::fs::remove_dir_all(&host);
        let (emu, port) = serve(vm, &dir);
        let at = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(&dir);
        std::fs::create_dir_all(&at).unwrap();
        // Attach as the emu's user, the host's, who owns the directory.
        // With plain 9P2000 Linux shows every file as owned by dfltuid
        // and checks permissions against that: make it this user too.
        let id = |f: &str| {
            let o = Command::new("id").arg(f).output().expect("id");
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        let opts = format!(
            "trans=tcp,port={port},version=9p2000,access=any,uname={},dfltuid={},dfltgid={}",
            id("-un"),
            id("-u"),
            id("-g")
        );
        let at_s = at.to_string_lossy().into_owned();
        let mounted = Mounted {
            at,
            emu,
            host: host.clone(),
        };
        assert!(
            sudo(&["mount", "-t", "9p", "-o", &opts, "127.0.0.1", &at_s]),
            "{vm}: mount -t 9p failed"
        );
        check(vm, &mounted.at, &host);
    }
}
