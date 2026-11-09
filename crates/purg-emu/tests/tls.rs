//! `#D` speaks TLS 1.3 to a stock server and checks the server's signature
//! with the pinned P-256 key.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use purg_abi::mode;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

struct MockConsole;

impl Console for MockConsole {
    fn read(&self, _buf: &mut [u8]) -> purg_platform::Result<usize> {
        Ok(0)
    }
    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        Ok(buf.len())
    }
}

struct Mock;

impl Platform for Mock {
    fn now_ns(&self) -> u64 {
        0
    }
    fn monotonic_ns(&self) -> u64 {
        0
    }
    fn entropy(&self, buf: &mut [u8]) {
        static NEXT: AtomicU64 = AtomicU64::new(0x1234_5678_9abc_def0);
        for chunk in buf.chunks_mut(8) {
            let n = NEXT.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
            chunk.copy_from_slice(&n.to_le_bytes()[..chunk.len()]);
        }
    }
    fn console(&self) -> &dyn Console {
        &CONSOLE
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
            .map_err(|_| "no free procs".into())
    }
}

static CONSOLE: MockConsole = MockConsole;

thread_local! {
    static PID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn setup() -> Prog {
    let platform: Arc<dyn Platform> = Arc::new(Mock);
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root(procs.user()))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    let p = Prog::new(kernel, pgrp);
    p.kernel.platform.set_current_pid(p.pid);
    purg_platform::sync::set_current_note(Some(p.proc.note.clone()));
    p
}

struct ProgIo {
    prog: Prog,
    fd: i32,
}

impl Read for ProgIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.prog
            .read(self.fd, buf)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

impl Write for ProgIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.prog
            .write(self.fd, buf)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn tls_client_exchanges_application_data() {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let params = CertificateParams::new(vec!["purgatorio".into()]).unwrap();
    let cert = params.self_signed(&key).unwrap();
    let point = key.public_key_raw();
    assert_eq!(point.len(), 65, "raw key {point:?}");
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert.der().to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .unwrap();

    let mut parent = setup();
    let [client, server] = parent.pipe().unwrap();
    let mut child = parent.spawn(0, &[(server, 0)]).unwrap();
    child.started();
    parent.close(server).unwrap();
    let note = child.proc.note.clone();
    let server = thread::spawn(move || {
        purg_platform::sync::set_current_note(Some(note));
        let mut conn = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        let mut io = ProgIo { prog: child, fd: 0 };
        let mut stream = rustls::Stream::new(&mut conn, &mut io);
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        stream.write_all(b"pong").unwrap();
    });

    let ctl = parent.open("#D/clone", mode::ORDWR).unwrap();
    let mut idbuf = [0u8; 32];
    let n = parent.read(ctl, &mut idbuf).unwrap();
    let id = std::str::from_utf8(&idbuf[..n])
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    for cmd in [
        format!("fd {client}\n"),
        format!("server purgatorio\n"),
        format!("pubkey {}\n", hex(point)),
        String::from("client\n"),
    ] {
        assert_eq!(
            parent.write(ctl, cmd.as_bytes()).unwrap(),
            cmd.len(),
            "{cmd}"
        );
    }
    let data = parent.open(&format!("#D/{id}/data"), mode::ORDWR).unwrap();
    assert_eq!(parent.write(data, b"ping").unwrap(), 4);
    let mut got = [0u8; 4];
    let mut filled = 0;
    while filled < got.len() {
        let n = parent.read(data, &mut got[filled..]).unwrap();
        assert!(n > 0, "tls data closed");
        filled += n;
    }
    assert_eq!(&got, b"pong");
    server.join().unwrap();
}
