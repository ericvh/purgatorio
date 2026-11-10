//! `#I` hangs a conversation up and forgets it at the last close.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use purg_abi::mode;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;
use purg_platform::net::{Conn, Listener, Net};

struct MockConsole;

impl Console for MockConsole {
    fn read(&self, _buf: &mut [u8]) -> purg_platform::Result<usize> {
        Ok(0)
    }
    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        Ok(buf.len())
    }
}

/// Counts the connections dialed and hung up.
#[derive(Default)]
struct MockNet {
    dialed: AtomicUsize,
    hungup: Arc<AtomicUsize>,
}

struct MockConn(Arc<AtomicUsize>);

impl Conn for MockConn {
    fn read(&self, _buf: &mut [u8]) -> purg_platform::Result<usize> {
        Ok(0)
    }
    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        Ok(buf.len())
    }
    fn local(&self) -> String {
        "127.0.0.1!1".into()
    }
    fn remote(&self) -> String {
        "127.0.0.1!2".into()
    }
    fn hangup(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

impl Net for MockNet {
    fn dial_tcp(&self, _host: &str, _port: u16) -> purg_platform::Result<Arc<dyn Conn>> {
        self.dialed.fetch_add(1, Ordering::AcqRel);
        Ok(Arc::new(MockConn(self.hungup.clone())))
    }
    fn announce_tcp(&self, _host: &str, _port: u16) -> purg_platform::Result<Arc<dyn Listener>> {
        Err("no listeners".into())
    }
    fn dial_udp(&self, host: &str, port: u16) -> purg_platform::Result<Arc<dyn Conn>> {
        self.dial_tcp(host, port)
    }
    fn announce_udp(&self, host: &str, port: u16) -> purg_platform::Result<Arc<dyn Listener>> {
        self.announce_tcp(host, port)
    }
    fn resolve(&self, host: &str, _port: u16) -> purg_platform::Result<String> {
        Ok(host.into())
    }
}

struct Mock {
    console: MockConsole,
    net: MockNet,
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
        0
    }
    fn set_current_pid(&self, _pid: u32) {}
    fn spawn_kproc(&self, _name: &str, _f: Box<dyn FnOnce() + Send>) -> purg_platform::Result<()> {
        Err(purg_abi::error::ENOPROC.into())
    }
    fn net(&self) -> Option<&dyn Net> {
        Some(&self.net)
    }
}

fn setup() -> (Prog, Arc<Mock>) {
    let mock = Arc::new(Mock {
        console: MockConsole,
        net: MockNet::default(),
    });
    let platform: Arc<dyn Platform> = mock.clone();
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root("inferno"))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    (Prog::new(kernel, pgrp), mock)
}

/// The number of conversations listed in `#I/tcp`.
fn convs(p: &mut Prog) -> usize {
    let fd = p.open("#I/tcp", mode::OREAD).unwrap();
    let mut buf = vec![0u8; 8192];
    let mut n = 0;
    loop {
        let got = p.dirread(fd, &mut buf).unwrap();
        if got == 0 {
            break;
        }
        let mut off = 0;
        while off < got {
            let (d, used) = purg_styx::Dir::unpack(&buf[off..got]).unwrap();
            n += usize::from(d.name != "clone");
            off += used;
        }
    }
    p.close(fd).unwrap();
    n
}

#[test]
fn last_close_hangs_up_and_forgets_the_conversation() {
    let (mut p, mock) = setup();
    let ctl = p.open("#I/tcp/clone", mode::ORDWR).unwrap();
    let mut buf = [0u8; 16];
    let n = p.read(ctl, &mut buf).unwrap();
    let id = std::str::from_utf8(&buf[..n]).unwrap().trim().to_string();
    p.write(ctl, b"connect 127.0.0.1!9").unwrap();
    let data = p.open(&format!("#I/tcp/{id}/data"), mode::ORDWR).unwrap();
    assert_eq!(convs(&mut p), 1);

    // The data file keeps the conversation after the ctl file closes.
    p.close(ctl).unwrap();
    assert_eq!(mock.net.hungup.load(Ordering::Acquire), 0);
    assert_eq!(p.write(data, b"x").unwrap(), 1);
    assert_eq!(convs(&mut p), 1);

    p.close(data).unwrap();
    assert_eq!(mock.net.hungup.load(Ordering::Acquire), 1);
    assert_eq!(convs(&mut p), 0);
    let e = p
        .open(&format!("#I/tcp/{id}/data"), mode::ORDWR)
        .unwrap_err();
    assert_eq!(e.as_str(), purg_abi::error::ENONEXIST);
}

#[test]
fn a_prog_that_exits_hangs_up_its_conversations() {
    let (mut p, mock) = setup();
    let mut child = p.spawn(purg_abi::pctl::NEWFD, &[]).unwrap();
    let ctl = child.open("#I/tcp/clone", mode::ORDWR).unwrap();
    child.write(ctl, b"connect 127.0.0.1!9").unwrap();
    assert_eq!(convs(&mut p), 1);
    drop(child);
    assert_eq!(mock.net.dialed.load(Ordering::Acquire), 1);
    assert_eq!(mock.net.hungup.load(Ordering::Acquire), 1);
    assert_eq!(convs(&mut p), 0);
}
