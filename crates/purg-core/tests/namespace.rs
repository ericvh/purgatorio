//! Namespace resolution, union directories, fds and the portable devices,
//! driven through a mock platform.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use purg_abi::mount::{MAFTER, MBEFORE, MCREATE, MREPL};
use purg_abi::{error, mode, pctl};
use purg_core::devram::RamTree;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Kernel, Pgrp, Platform, Procs, Prog};
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

thread_local! {
    static PID: Cell<u32> = const { Cell::new(0) };
}

#[derive(Default)]
struct Mock {
    console: MockConsole,
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
        PID.get()
    }
    fn set_current_pid(&self, pid: u32) {
        PID.set(pid);
    }
    fn spawn_kproc(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> purg_platform::Result<()> {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(f)
            .map(drop)
            .map_err(|_| error::ENOPROC.into())
    }
}

/// A prog whose namespace is `#/`, with the portable devices and two static
/// trees available as `#A` and `#B`. The calling thread runs as that prog.
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
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root("inferno"))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    devs.push(Arc::new(a));
    devs.push(Arc::new(b));
    let ram = RamTree::new('R', "ram", "inferno", platform.clone());
    ram.add_file("/seed/hello", &b"seeded"[..], 0o644);
    devs.push(Arc::new(ram));
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    let p = Prog::new(kernel, pgrp);
    mock.set_current_pid(p.pid);
    (p, mock)
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
    p.chdir("/net/sub").unwrap();
    assert_eq!(p.dot(), "/net/sub");
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
    let (mut p, _) = setup();
    assert_eq!(read_all(&mut p, "#c/pid").unwrap(), p.pid.to_string());
    assert_eq!(read_all(&mut p, "#c/time").unwrap(), "1700000000000000");
    assert_eq!(read_all(&mut p, "#c/null").unwrap(), "");
    assert_eq!(
        read_all(&mut p, "#c/drivers").unwrap(),
        "#/ root\n#c cons\n#e env\n#| pipe\n#p prog\n#s srv\n#M mnt\n#d dup\n#% mem\n#I ip\n#C cmd\n#¤ factotum\n#D ssl\n#A a\n#B b\n#R ram\n"
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

#[test]
fn unmount_restores_the_previous_binding() {
    let (mut p, _) = setup();
    p.bind("#A", "/net", MREPL).unwrap();
    p.bind("#B", "/net", MBEFORE).unwrap();
    p.unmount(Some("#B"), "/net").unwrap();
    assert_eq!(read_all(&mut p, "/net/x").unwrap(), "a's x");
    assert_eq!(
        p.unmount(Some("#B"), "/net").unwrap_err().as_str(),
        error::EUNMOUNT
    );
    p.unmount(None, "/net").unwrap();
    assert_eq!(read_all(&mut p, "/net/x").unwrap_err(), error::ENONEXIST);
}

#[test]
fn create_goes_to_the_mcreate_member() {
    let (mut p, _) = setup();
    p.bind("#A", "/net", MREPL).unwrap();
    assert_eq!(
        p.create("/net/new", mode::OWRITE, 0o666)
            .unwrap_err()
            .as_str(),
        error::ENOCREATE
    );
    p.bind("#e", "/net", MAFTER | MCREATE).unwrap();
    let fd = p.create("/net/new", mode::OWRITE, 0o666).unwrap();
    p.write(fd, b"made").unwrap();
    p.close(fd).unwrap();
    assert_eq!(read_all(&mut p, "#e/new").unwrap(), "made");
    assert_eq!(read_all(&mut p, "/net/new").unwrap(), "made");
    p.remove("/net/new").unwrap();
    assert_eq!(read_all(&mut p, "#e/new").unwrap_err(), error::ENONEXIST);
}

#[test]
fn chdir_and_fd2path() {
    let (mut p, _) = setup();
    p.bind("#A", "/net", MREPL).unwrap();
    p.chdir("/net").unwrap();
    assert!(p.chdir("x").is_err());
    let fd = p.open("sub/deep", mode::OREAD).unwrap();
    assert_eq!(p.fd2path(fd).unwrap(), "/net/sub/deep");
    assert_eq!(p.dup(fd, -1).unwrap(), fd + 1);
    assert_eq!(p.fd2path(fd + 1).unwrap(), "/net/sub/deep");
}

#[test]
fn pipes_carry_data_and_hang_up() {
    let (mut p, _) = setup();
    let [r, w] = p.pipe().unwrap();
    assert_eq!(p.write(w, b"through").unwrap(), 7);
    let mut buf = [0u8; 16];
    let n = p.read(r, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"through");
    // Pipes are bidirectional.
    p.write(r, b"back").unwrap();
    let n = p.read(w, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"back");
    p.close(w).unwrap();
    assert_eq!(p.read(r, &mut buf).unwrap(), 0);
    assert!(p.write(r, b"x").is_err());
}

#[test]
fn pipe_reader_blocks_until_a_writer_on_another_kproc() {
    let (mut p, mock) = setup();
    let [r, w] = p.pipe().unwrap();
    let child = p.spawn(0, &[(w, 0)]).unwrap();
    p.close(w).unwrap();
    let pid = child.pid;
    mock.spawn_kproc(
        "writer",
        Box::new(move || {
            let mut child = child;
            child.started();
            PID.set(child.pid);
            std::thread::sleep(std::time::Duration::from_millis(20));
            child.write(0, b"late").unwrap();
            child.status = "done".into();
        }),
    )
    .unwrap();
    let mut buf = [0u8; 16];
    let n = p.read(r, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"late");
    assert_eq!(p.read(r, &mut buf).unwrap(), 0);
    let wait = format!("#p/{}/wait", p.pid);
    let fd = p.open(&wait, mode::OREAD).unwrap();
    let n = p.read(fd, &mut buf).unwrap();
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        format!("{pid} '' done")
    );
}

#[test]
fn unstarted_children_leave_quietly() {
    let (mut p, _) = setup();
    let quiet = p.spawn(0, &[]).unwrap();
    drop(quiet);
    let mut child = p.spawn(0, &[]).unwrap();
    child.started();
    let pid = child.pid;
    drop(child);
    let fd = p.open(&format!("#p/{}/wait", p.pid), mode::OREAD).unwrap();
    let mut buf = [0u8; 64];
    let n = p.read(fd, &mut buf).unwrap();
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        format!("{pid} '' ''")
    );
}

#[test]
fn env_is_per_group_and_forkable() {
    let (mut p, _) = setup();
    let fd = p.create("#e/HOME", mode::OWRITE, 0o666).unwrap();
    p.write(fd, b"/usr/inferno").unwrap();
    p.close(fd).unwrap();
    assert_eq!(p.egrp().environ(), ["HOME=/usr/inferno"]);

    let shared = p.spawn(0, &[]).unwrap();
    let forked = p.spawn(pctl::FORKENV, &[]).unwrap();
    let fresh = p.spawn(pctl::NEWENV, &[]).unwrap();
    p.egrp().set("HOME", b"/tmp");
    assert_eq!(shared.egrp().get("HOME").unwrap(), b"/tmp");
    assert_eq!(forked.egrp().get("HOME").unwrap(), b"/usr/inferno");
    assert!(fresh.egrp().get("HOME").is_none());
}

#[test]
fn pctl_namespace_and_fd_groups() {
    let (mut p, _) = setup();
    let fd = p.open("#A/x", mode::OREAD).unwrap();
    let mut shared = p.spawn(0, &[]).unwrap();
    let mut forked = p.spawn(pctl::FORKNS | pctl::FORKFD, &[]).unwrap();
    p.bind("#A", "/net", MREPL).unwrap();
    assert_eq!(read_all(&mut shared, "/net/x").unwrap(), "a's x");
    assert_eq!(
        read_all(&mut forked, "/net/x").unwrap_err(),
        error::ENONEXIST
    );
    forked.close(fd).unwrap();
    assert!(p.fstat(fd).is_ok());

    let mut fresh = p.spawn(pctl::NEWFD, &[(fd, 5)]).unwrap();
    assert!(fresh.fstat(fd).is_err());
    assert_eq!(read_all(&mut fresh, "#A/only-a").unwrap(), "in a");
    assert_eq!(fresh.fstat(5).unwrap().name, "x");
    fresh.pctl(pctl::NEWFD, &[]).unwrap();
    assert!(fresh.fstat(5).is_err());

    p.pctl(pctl::NODEVS, &[]).unwrap();
    assert_eq!(read_all(&mut p, "#A/x").unwrap_err(), error::ENOATTACH);
    assert_eq!(read_all(&mut p, "#c/user").unwrap_err(), error::ENOATTACH);
    p.pipe().unwrap();
}

#[test]
fn ram_tree_is_writable() {
    let (mut p, _) = setup();
    p.bind("#R", "/chan", MREPL | MCREATE).unwrap();
    assert_eq!(read_all(&mut p, "/chan/seed/hello").unwrap(), "seeded");
    let fd = p.create("/chan/f", mode::OWRITE, 0o644).unwrap();
    p.write(fd, b"hello world").unwrap();
    p.close(fd).unwrap();
    assert_eq!(read_all(&mut p, "/chan/f").unwrap(), "hello world");
    let fd = p.open("/chan/f", mode::OWRITE).unwrap();
    p.write(fd, b"J").unwrap();
    p.close(fd).unwrap();
    assert_eq!(read_all(&mut p, "/chan/f").unwrap(), "Jello world");
    let fd = p
        .open("/chan/seed/hello", mode::OWRITE | mode::OTRUNC)
        .unwrap();
    p.write(fd, b"new").unwrap();
    p.close(fd).unwrap();
    assert_eq!(read_all(&mut p, "/chan/seed/hello").unwrap(), "new");

    let mut d = purg_core::Dir::null();
    d.name = "g".into();
    p.wstat("/chan/f", &d).unwrap();
    assert_eq!(names(&mut p, "/chan"), ["g", "seed"]);
    let mut d = purg_core::Dir::null();
    d.length = 5;
    p.wstat("/chan/g", &d).unwrap();
    assert_eq!(read_all(&mut p, "/chan/g").unwrap(), "Jello");

    p.create("/chan/d", mode::OREAD, purg_styx::DMDIR | 0o755)
        .unwrap();
    let e = p
        .create("/chan/d", mode::OREAD, purg_styx::DMDIR | 0o755)
        .unwrap_err();
    assert_eq!(e.to_string(), error::EEXIST);
    p.create("/chan/d/inner", mode::OWRITE, 0o644).unwrap();
    assert_eq!(
        p.remove("/chan/d").unwrap_err().to_string(),
        error::ENOTEMPTY
    );
    p.remove("/chan/d/inner").unwrap();
    p.remove("/chan/d").unwrap();
    assert_eq!(names(&mut p, "/chan"), ["g", "seed"]);
}
