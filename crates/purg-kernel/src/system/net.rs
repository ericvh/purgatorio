//! Networking on the kernel (K3): `#I` over smoltcp on the virtio-net card
//! (`purg_aarch64::net::NetStack`), and the network acceptance checks the
//! kernel runs in test mode when `xtask kernel` asks for them in the
//! bootargs:
//!
//! - `hostexport=PORT`: mount the hosted emu's `styxlisten` export at
//!   `tcp!10.0.2.2!PORT` (the host, in QEMU user networking) on `/n/remote`
//!   and list it.
//! - `udpecho=PORT`: send a datagram to the host's UDP echo server at
//!   `udp!10.0.2.2!PORT` and check the echo.
//! - `netexport`: announce `tcp!*!6666` and export `/` to the first
//!   connection, which the host mounts through a QEMU `hostfwd`.

use alloc::format;
use alloc::string::String;

use purg_aarch64::board::{bootarg, has_bootarg};
use purg_aarch64::dtb::{BootInfo, Fdt};
use purg_aarch64::net::{Ipv4Config, NetStack};
use purg_abi::mode;
use purg_core::{Error, Prog};

use super::System;
use crate::kernel::println;

/// The host as QEMU user networking shows it to the guest.
const HOST: &str = "10.0.2.2";

/// The port the kernel exports `/` on.
pub const EXPORT_PORT: u16 = 6666;

/// The network checks the bootargs ask for.
#[derive(Clone, Copy, Default)]
pub struct Checks {
    host_export: Option<u16>,
    udp_echo: Option<u16>,
    export: bool,
}

/// Brings up the network (after `irq::init`, which routes the card's
/// interrupt) and reads which checks to run.
pub fn open(fdt: &Fdt<'_>, info: &BootInfo<'_>) -> (NetStack, Checks) {
    let stack = NetStack::open(fdt, &Ipv4Config::QEMU_USER);
    if let Some(why) = stack.down() {
        println!("kernel: no network: {why}");
    }
    let port = |key| bootarg(info, key).and_then(|p| p.parse().ok());
    let checks = Checks {
        host_export: port("hostexport"),
        udp_echo: port("udpecho"),
        export: has_bootarg(info, "netexport"),
    };
    (stack, checks)
}

impl System {
    /// Runs the network checks; returns the number that failed.
    pub fn net_tests(&self, checks: &Checks) -> usize {
        let mut failed = 0;
        if let Some(port) = checks.host_export {
            self.say("kernel: mount the hosted emu's export on /n/remote");
            let v = self.mount_host(port);
            self.verdict(&mut failed, v);
        }
        if let Some(port) = checks.udp_echo {
            self.say("kernel: run /bin/test/net.wasm --udp (the host's UDP echo)");
            let addr = format!("udp!{HOST}!{port}");
            let v = match self.run(&["/bin/test/net.wasm", "--udp", &addr], None) {
                Ok(s) if s.is_empty() => Ok(String::from("ok")),
                Ok(s) => Err(format!("status {s:?}")),
                Err(e) => Err(format!("error: {e}")),
            };
            self.verdict(&mut failed, v);
        }
        if checks.export && failed == 0 {
            let v = self.serve_export().map(|()| String::from("net served"));
            self.verdict(&mut failed, v.map_err(|e| format!("net: {e}")));
        }
        failed
    }

    /// Mounts `tcp!10.0.2.2!port` on `/n/remote` and lists it: the host's
    /// `lib/probe`, and its `README`, which the kernel's root lacks.
    fn mount_host(&self, port: u16) -> core::result::Result<String, String> {
        let addr = format!("tcp!{HOST}!{port}");
        let steps: [&[&str]; 4] = [
            &["mkdir", "-p", "/n/remote"],
            &["mount", "-A", &addr, "/n/remote"],
            &["ls", "/n/remote/lib/probe"],
            &["ls", "-d", "/n/remote/README"],
        ];
        for argv in steps {
            match self.run(argv, None) {
                Ok(s) if s.is_empty() => {}
                Ok(s) => return Err(format!("{}: status {s:?}", argv[0])),
                Err(e) => return Err(format!("{}: {e}", argv[0])),
            }
        }
        Ok(String::from("ok"))
    }

    /// Announces `tcp!*!6666` and serves `/` to the first connection,
    /// returning when the client hangs up.
    fn serve_export(&self) -> Result<(), Error> {
        let mut prog = Prog::new(self.kernel.clone(), self.pgrp.clone());
        self.kernel.platform.set_current_pid(prog.pid);
        let ctl = prog.open("/net/tcp/clone", mode::ORDWR)?;
        let mut buf = [0u8; 32];
        let n = prog.read(ctl, &mut buf)?;
        let id = String::from(core::str::from_utf8(&buf[..n]).unwrap_or("").trim());
        prog.write(ctl, format!("announce *!{EXPORT_PORT}").as_bytes())?;
        self.say("kernel: net ready");
        let listen = prog.open(&format!("/net/tcp/{id}/listen"), mode::OREAD)?;
        let n = prog.read(listen, &mut buf)?;
        let new_id = core::str::from_utf8(&buf[..n]).unwrap_or("").trim();
        let data = prog.open(&format!("/net/tcp/{new_id}/data"), mode::ORDWR)?;
        let chan = prog.chan(data)?;
        purg_core::export::serve(
            self.pgrp.clone(),
            "/",
            &chan,
            &self.kernel.platform,
            false,
            false,
        )
    }
}
