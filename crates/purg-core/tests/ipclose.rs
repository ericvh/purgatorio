//! `#I` hangs a conversation up and forgets it at the last close, and
//! `/net/ipifc` and `/net/iproute` configure the platform's interface.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use purg_abi::mode;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;
use purg_platform::net::{Conn, Ifc, Listener, Net};

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
    ifc: std::sync::Mutex<Option<Ifc>>,
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
    fn interfaces(&self) -> Vec<Ifc> {
        self.ifc.lock().unwrap().iter().cloned().collect()
    }
    fn set_address(&self, n: usize, addr: [u8; 4], prefix: u8) -> purg_platform::Result<()> {
        assert_eq!(n, 0);
        let mut ifc = self.ifc.lock().unwrap();
        let i = ifc.as_mut().ok_or("no interface")?;
        (i.addr, i.prefix) = (addr, prefix);
        Ok(())
    }
    fn set_gateway(&self, gateway: Option<[u8; 4]>) -> purg_platform::Result<()> {
        let mut ifc = self.ifc.lock().unwrap();
        ifc.as_mut().ok_or("no interface")?.gateway = gateway;
        Ok(())
    }
}

fn read_file(p: &mut Prog, path: &str) -> String {
    let fd = p.open(path, mode::OREAD).unwrap();
    let mut buf = vec![0u8; 1024];
    let n = p.read(fd, &mut buf).unwrap();
    p.close(fd).unwrap();
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn write_file(p: &mut Prog, path: &str, text: &str) -> Result<(), String> {
    let fd = p.open(path, mode::OWRITE).unwrap();
    let r = p
        .write(fd, text.as_bytes())
        .map(|_| ())
        .map_err(|e| e.to_string());
    p.close(fd).unwrap();
    r
}

#[test]
fn ipifc_and_iproute_set_the_address_and_gateway() {
    let (mut p, mock) = setup();
    assert_eq!(read_file(&mut p, "#I/iproute"), "");
    *mock.net.ifc.lock().unwrap() = Some(Ifc {
        device: "ether0".into(),
        mtu: 1500,
        mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56],
        addr: [10, 0, 2, 15],
        prefix: 24,
        gateway: None,
    });
    assert_eq!(
        read_file(&mut p, "#I/ipifc/0/status"),
        "ether0 1500 525400123456\n10.0.2.15 255.255.255.0 10.0.2.0 0 0 0 0\n"
    );
    write_file(&mut p, "#I/ipifc/0/ctl", "add 192.168.7.9 255.255.0.0").unwrap();
    write_file(&mut p, "#I/iproute", "add 0.0.0.0 0.0.0.0 192.168.0.1").unwrap();
    assert_eq!(
        read_file(&mut p, "#I/ipifc/0/local"),
        "192.168.7.9 -> 192.168.255.255 192.168.0.0\n"
    );
    assert_eq!(
        read_file(&mut p, "#I/iproute"),
        "192.168.0.0 255.255.0.0 192.168.7.9 4i none 0\n0.0.0.0 0.0.0.0 192.168.0.1 4u none 0\n"
    );
    // A mask that is not a prefix, and routes other than the default one.
    assert!(write_file(&mut p, "#I/ipifc/0/ctl", "add 10.0.0.1 255.0.255.0").is_err());
    assert!(write_file(&mut p, "#I/iproute", "add 10.1.0.0 255.255.0.0 10.0.0.1").is_err());
    write_file(&mut p, "#I/iproute", "remove 0.0.0.0 0.0.0.0").unwrap();
    assert_eq!(mock.net.ifc.lock().unwrap().as_ref().unwrap().gateway, None);
    assert!(p.open("#I/ipifc/1", mode::OREAD).is_err());
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
