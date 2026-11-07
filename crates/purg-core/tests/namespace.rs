//! Namespace resolution, union directories, fds and the portable devices,
//! driven through a mock platform.

use std::sync::{Arc, Mutex};

use purg_abi::mount::{MAFTER, MBEFORE, MREPL};
use purg_abi::{error, mode};
use purg_core::devcons::DevCons;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Kernel, Pgrp, Platform, Prog};
use purg_platform::Console;

#[derive(Default)]
struct MockConsole {
    input: Mutex<Vec<u8>>,
    output: Mutex<Vec<u8>>,
}

impl Console for MockConsole {
    fn read(&self, buf: &mut [u8]) -> purg_platform::Result<usize> {
        let mut input = self.input.lock().unwrap();
        let n = buf.len().min(input.len());
        buf[..n].copy_from_slice(&input[..n]);
        input.drain(..n);
        Ok(n)
    }

    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        self.output.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
}

#[derive(Default)]
struct Mock {
    console: MockConsole,
    pid: Mutex<u32>,
}

impl Platform for Mock {
    fn now_ns(&self) -> u64 {
        1_700_000_000_000_000_000
    }
    fn monotonic_ns(&self) -> u64 {
        42
    }
    fn entropy(&self, buf: &mut [u8]) {
        buf.fill(7);
    }
    fn console(&self) -> &dyn Console {
        &self.console
    }
    fn user(&self) -> String {
        "inferno".into()
    }
    fn sysname(&self) -> String {
        "purgatorio".into()
    }
    fn current_pid(&self) -> u32 {
        *self.pid.lock().unwrap()
    }
    fn set_current_pid(&self, pid: u32) {
        *self.pid.lock().unwrap() = pid;
    }
}

/// A prog whose namespace is `#/` with two static trees available as `#A`
/// and `#B`, and `#c`.
fn setup() -> (Prog, Arc<Mock>) {
    let mock = Arc::new(Mock::default());
    let platform: Arc<dyn Platform> = mock.clone();
    let mut a = StaticTree::new('A', "a", "a");
    a.add_file("/x", &b"a's x"[..]);
    a.add_file("/only-a", &b"in a"[..]);
    a.add_file("/sub/deep", &b"deep"[..]);
    let mut b = StaticTree::new('B', "b", "b");
    b.add_file("/x", &b"b's x"[..]);
    b.add_file("/only-b", &b"in b"[..]);
    let devs: Vec<Arc<dyn Dev>> = vec![
        Arc::new(StaticTree::root("inferno")),
        Arc::new(DevCons::new(platform.clone())),
        Arc::new(a),
        Arc::new(b),
    ];
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone());
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    (Prog::new(kernel, pgrp), mock)
}

fn read_all(p: &mut Prog, path: &str) -> Result<String, String> {
    let fd = p.open(path, mode::OREAD).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let n = p.read(fd, &mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    p.close(fd).unwrap();
    Ok(String::from_utf8(out).unwrap())
}

fn names(p: &mut Prog, path: &str) -> Vec<String> {
    let fd = p.open(path, mode::OREAD).unwrap();
    let names = p.dirents(fd).unwrap().into_iter().map(|d| d.name).collect();
    p.close(fd).unwrap();
    names
}

#[test]
fn device_paths_resolve_without_binds() {
    let (mut p, _) = setup();
    assert_eq!(read_all(&mut p, "#A/x").unwrap(), "a's x");
    assert_eq!(read_all(&mut p, "#A/sub/../x").unwrap(), "a's x");
    assert_eq!(read_all(&mut p, "#c/sysname").unwrap(), "purgatorio");
    assert_eq!(read_all(&mut p, "#Z/x").unwrap_err(), error::EBADSHARP);
    assert_eq!(read_all(&mut p, "#A/nope").unwrap_err(), error::ENONEXIST);
}

#[test]
fn bind_replace() {
    let (mut p, _) = setup();
    p.bind("#c", "/dev", MREPL).unwrap();
    assert_eq!(read_all(&mut p, "/dev/user").unwrap(), "inferno");
    assert!(names(&mut p, "/dev").contains(&"cons".to_string()));
}

#[test]
fn union_order_before_and_after() {
    let (mut p, _) = setup();
    p.bind("#A", "/n", MREPL).unwrap_err();
    p.bind("#A", "/net", MREPL).unwrap();
    p.bind("#B", "/net", MAFTER).unwrap();
    assert_eq!(read_all(&mut p, "/net/x").unwrap(), "a's x");
    assert_eq!(read_all(&mut p, "/net/only-b").unwrap(), "in b");
    assert_eq!(names(&mut p, "/net"), ["x", "only-a", "sub", "x", "only-b"]);

    let (mut p, _) = setup();
    p.bind("#A", "/net", MREPL).unwrap();
    p.bind("#B", "/net", MBEFORE).unwrap();
    assert_eq!(read_all(&mut p, "/net/x").unwrap(), "b's x");
    assert_eq!(read_all(&mut p, "/net/only-a").unwrap(), "in a");
}

#[test]
fn union_includes_original_directory() {
    let (mut p, _) = setup();
    p.bind("#A", "/", MAFTER).unwrap();
    // #/ entries come first, then #A's.
    let n = names(&mut p, "/");
    assert_eq!(n[..6], ["chan", "dev", "env", "fd", "net", "prog"]);
    assert!(n.contains(&"only-a".to_string()));
    assert_eq!(read_all(&mut p, "/sub/deep").unwrap(), "deep");
    // Binds on a union member found by walking still apply.
    p.bind("#c", "/dev", MBEFORE).unwrap();
    assert_eq!(read_all(&mut p, "/dev/sysname").unwrap(), "purgatorio");
}

#[test]
fn dotdot_is_lexical_across_mounts() {
    let (mut p, _) = setup();
    p.bind("#A", "/net", MREPL).unwrap();
    assert_eq!(read_all(&mut p, "/net/sub/../x").unwrap(), "a's x");
    assert_eq!(
        read_all(&mut p, "/net/sub/../../net/only-a").unwrap(),
        "in a"
    );
    p.dot = "/net/sub".into();
    assert_eq!(read_all(&mut p, "deep").unwrap(), "deep");
    assert_eq!(read_all(&mut p, "../x").unwrap(), "a's x");
}

#[test]
fn bind_requires_matching_kinds() {
    let (p, _) = setup();
    assert!(p.bind("#A/x", "/dev", MREPL).is_err());
    assert!(p.bind("#A", "/nonexistent", MREPL).is_err());
}

#[test]
fn fds_and_modes() {
    let (mut p, mock) = setup();
    p.bind("#c", "/dev", MREPL).unwrap();
    let r = p.open("/dev/cons", mode::OREAD).unwrap();
    let w = p.open("/dev/cons", mode::OWRITE).unwrap();
    assert_eq!((r, w), (0, 1));
    assert_eq!(p.write(r, b"x").unwrap_err().as_str(), error::EBADUSEFD);
    p.write(w, b"hello").unwrap();
    assert_eq!(&*mock.console.output.lock().unwrap(), b"hello");
    p.close(r).unwrap();
    assert_eq!(p.close(r).unwrap_err().as_str(), error::EBADFD);
    // The lowest free fd is reused.
    assert_eq!(p.open("/dev/null", mode::OWRITE).unwrap(), 0);
    assert_eq!(
        p.open("/dev", mode::OWRITE).unwrap_err().as_str(),
        error::EISDIR
    );
    assert_eq!(
        p.open("/dev/user", mode::OWRITE).unwrap_err().as_str(),
        error::EPERM
    );
}

#[test]
fn cons_reads_a_line_at_a_time() {
    let (mut p, mock) = setup();
    mock.console
        .input
        .lock()
        .unwrap()
        .extend_from_slice(b"one\ntwo\nthree");
    let fd = p.open("#c/cons", mode::OREAD).unwrap();
    let mut buf = [0u8; 64];
    let mut lines = Vec::new();
    loop {
        let n = p.read(fd, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        lines.push(String::from_utf8_lossy(&buf[..n]).into_owned());
    }
    assert_eq!(lines, ["one\n", "two\n", "three"]);
}

#[test]
fn cons_files() {
    let (mut p, mock) = setup();
    mock.set_current_pid(p.pid);
    assert_eq!(read_all(&mut p, "#c/pid").unwrap(), p.pid.to_string());
    assert_eq!(read_all(&mut p, "#c/time").unwrap(), "1700000000000000");
    assert_eq!(read_all(&mut p, "#c/null").unwrap(), "");
    assert_eq!(
        read_all(&mut p, "#c/drivers").unwrap(),
        "#/ root\n#c cons\n#A a\n#B b\n"
    );
}

#[test]
fn stat_and_dir_reads() {
    let (mut p, _) = setup();
    let d = p.stat("#A/x").unwrap();
    assert_eq!((d.name.as_str(), d.length, d.qid.is_dir()), ("x", 5, false));
    let root = p.stat("/").unwrap();
    assert_eq!(root.name, "/");
    assert!(root.qid.is_dir());

    // Directory reads return whole packed entries and resume where they
    // left off.
    let fd = p.open("#A", mode::OREAD).unwrap();
    let mut all = Vec::new();
    let mut buf = [0u8; 70];
    loop {
        let n = p.dirread(fd, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        all.extend(purg_styx::dirs(&buf[..n]).map(|d| d.unwrap().name));
    }
    assert_eq!(all, ["x", "only-a", "sub"]);
    let mut tiny = [0u8; 10];
    p.seek(fd, 0, 0).unwrap();
    assert_eq!(
        p.dirread(fd, &mut tiny).unwrap_err().as_str(),
        error::ESHORTSTAT
    );
}
