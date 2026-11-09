//! Mount, export, file2chan and kill.

use std::sync::Arc;
use std::thread;

use purg_abi::mode;
use purg_core::devram::RamTree;
use purg_core::devroot::StaticTree;
use purg_core::kchan::KMsg;
use purg_core::{Dev, DevTab, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;

#[derive(Default)]
struct MockConsole;

impl Console for MockConsole {
    fn read(&self, _buf: &mut [u8]) -> purg_platform::Result<usize> {
        Ok(0)
    }
    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        Ok(buf.len())
    }
}

struct Mock {
    console: MockConsole,
}

impl Platform for Mock {
    fn now_ns(&self) -> u64 {
        0
    }
    fn monotonic_ns(&self) -> u64 {
        0
    }
    fn entropy(&self, buf: &mut [u8]) {
        buf.fill(1);
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
        PID.with(|c| c.get())
    }
    fn set_current_pid(&self, pid: u32) {
        PID.with(|c| c.set(pid));
    }
    fn spawn_kproc(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> purg_platform::Result<()> {
        thread::Builder::new()
            .name(name.into())
            .spawn(f)
            .map(drop)
            .map_err(|_| purg_abi::error::ENOPROC.into())
    }
}

thread_local! {
    static PID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn setup() -> (Prog, Arc<Mock>) {
    let mock = Arc::new(Mock {
        console: MockConsole,
    });
    let platform: Arc<dyn Platform> = mock.clone();
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root("inferno"))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    let ram = RamTree::new('R', "ram", "inferno", platform.clone());
    ram.add_dir("/mnt");
    ram.add_dir("/box");
    devs.push(Arc::new(ram));
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    let p = Prog::new(kernel, pgrp);
    mock.set_current_pid(p.pid);
    purg_platform::sync::set_current_note(Some(p.proc.note.clone()));
    (p, mock)
}

#[test]
fn namespace_lists_binds() {
    let (p, _) = setup();
    p.bind("#c", "/dev", purg_abi::mount::MBEFORE).unwrap();
    p.bind(
        "#R",
        "/",
        purg_abi::mount::MAFTER | purg_abi::mount::MCREATE,
    )
    .unwrap();
    let ns = p.pgrp.namespace();
    assert!(ns.contains("bind -b '#c' /dev\n"), "{ns}");
    assert!(ns.contains("bind -ac '#R' /\n"), "{ns}");
    assert!(ns.ends_with("cd /\n"), "{ns}");
}

#[test]
fn file2chan_serves_a_read() {
    let (mut p, _) = setup();
    let (rh, _wh) = p.file2chan("#R/box", "g").unwrap();
    let kgrp = p.proc.kgrp.clone();
    let read_ch = kgrp.get(rh).unwrap();
    let server = thread::spawn(move || {
        let msg = read_ch.recv().unwrap().expect("request");
        let reply = i32::from_le_bytes(msg.bytes[17..21].try_into().unwrap());
        let reply_ch = kgrp.get(reply).unwrap();
        let mut bytes = vec![purg_abi::fio::OK_READ];
        bytes.extend_from_slice(b"xyz");
        reply_ch
            .send(KMsg {
                bytes,
                caps: Vec::new(),
            })
            .unwrap();
    });
    let fd = p.open("#R/box/g", mode::OREAD).unwrap();
    let mut buf = [0u8; 8];
    let n = p.read(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"xyz");
    server.join().unwrap();
}

#[cfg(feature = "std")]
#[test]
fn pipe_between_progs() {
    let (mut p, _) = setup();
    let [a, b] = p.pipe().unwrap();
    let mut child = p.spawn(0, &[(a, 0)]).unwrap();
    child.started();
    p.close(a).unwrap();
    let t = thread::spawn(move || {
        let mut buf = [0u8; 8];
        let n = child.read(0, &mut buf).unwrap();
        buf[..n].to_vec()
    });
    assert_eq!(p.write(b, b"hello").unwrap(), 5);
    assert_eq!(t.join().unwrap(), b"hello");
}

#[cfg(feature = "std")]
#[test]
fn export_mount_loopback() {
    let (mut p, mock) = setup();
    let [a, b] = p.pipe().unwrap();
    let [ready_r, ready_w] = p.pipe().unwrap();
    let mut child = p.spawn(0, &[(a, 0), (ready_w, 1)]).unwrap();
    child.started();
    p.close(a).unwrap();
    p.close(ready_w).unwrap();
    let mock = mock.clone();
    let server = thread::spawn(move || {
        mock.set_current_pid(child.pid);
        purg_platform::sync::set_current_note(Some(child.proc.note.clone()));
        child.write(1, b"Z").unwrap();
        child.export(0, "#R", 0).unwrap();
    });
    let mut flag = [0u8; 1];
    assert_eq!(p.read(ready_r, &mut flag).unwrap(), 1);
    p.mount(
        b,
        -1,
        "#R/mnt",
        purg_abi::mount::MREPL | purg_abi::mount::MCREATE,
        "",
    )
    .unwrap();
    p.close(b).unwrap();
    let fd = p.create("#R/mnt/hello", mode::OWRITE, 0o644).unwrap();
    assert_eq!(p.write(fd, b"hi").unwrap(), 2);
    p.close(fd).unwrap();
    let mut buf = [0u8; 8];
    let fd = p.open("#R/hello", mode::OREAD).unwrap();
    let n = p.read(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hi");
    p.close(fd).unwrap();
    let fd = p.open("#R/mnt/hello", mode::OREAD).unwrap();
    let n = p.read(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hi");
    p.close(fd).unwrap();
    let ns = p.pgrp.namespace();
    assert!(ns.contains("mount ") && ns.contains("#R/mnt"), "{ns}");
    p.unmount(None, "#R/mnt").unwrap();
    server.join().unwrap();
}

/// Hosted only: without the `std` feature the platform has no per-thread
/// note, so this is the emu's thread-per-prog model (`cargo test -p purg-core
/// --features std`).
#[cfg(feature = "std")]
#[test]
fn kill_interrupts_a_blocked_read() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (mut p, _) = setup();
    let [r, w] = p.pipe().unwrap();
    let mut child = p.spawn(0, &[(r, 0)]).unwrap();
    child.started();
    let pid = child.pid;
    let ready = Arc::new(AtomicBool::new(false));
    let flag = ready.clone();
    let join = thread::spawn(move || {
        purg_platform::sync::set_current_note(Some(child.proc.note.clone()));
        flag.store(true, Ordering::Release);
        let mut buf = [0u8; 4];
        let err = child.read(0, &mut buf).unwrap_err();
        assert_eq!(err.as_str(), purg_abi::error::EINTR);
    });
    while !ready.load(Ordering::Acquire) {
        thread::yield_now();
    }
    p.kernel.procs.get(pid).unwrap().kill();
    join.join().unwrap();
    let _ = (w, p);
}
