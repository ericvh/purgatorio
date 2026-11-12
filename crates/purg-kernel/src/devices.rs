//! The kernel's own devices (K3), bound after `/dev` as Inferno's native
//! kernels do:
//!
//! - `#r`, the PL031 real-time clock (`purg_core::devrtc` over
//!   `purg_aarch64::pl031`): `/dev/rtc`.
//! - `#t`, the serial ports (`purg_core::deveia`): `/dev/eia0`,
//!   `/dev/eia0ctl` and `/dev/eia0status`. QEMU `virt` has one PL011, the
//!   console, so `eia0` is that UART, shared with `#c/cons` as Inferno's
//!   native ports share a serial console: writes go out interleaved with
//!   console output, and a read takes input that `cons` would otherwise
//!   get. Line settings are recorded but not programmed: the emulated
//!   PL011 has no baud rate or framing.
//!
//! [`System::device_tests`] checks both in test mode.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use purg_aarch64::pl031::Pl031;
use purg_core::Dev;
use purg_core::deveia::DevEia;
use purg_core::devrtc::{DevRtc, parse_seconds};
use purg_platform::Console;

use crate::system::System;

/// The console as an `eia` port.
struct Shared(&'static dyn Console);

impl Console for Shared {
    fn read(&self, buf: &mut [u8]) -> purg_platform::Result<usize> {
        self.0.read(buf)
    }

    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        self.0.write(buf)
    }
}

/// `#r` (if the DTB names a PL031 at `rtc`) and `#t` over `console`.
pub fn native(console: &'static dyn Console, rtc: Option<usize>, user: &str) -> Vec<Arc<dyn Dev>> {
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(DevEia::new(
        vec![Arc::new(Shared(console))],
        String::from(user),
    ))];
    if let Some(pa) = rtc {
        // SAFETY: the DTB names a PL031 there, in the device window.
        let rtc = Arc::new(unsafe { Pl031::new(pa) });
        devs.push(Arc::new(DevRtc::new(rtc, String::from(user))));
    }
    devs.extend(crate::kprof::dev(user));
    devs
}

/// What the eia check's script prints.
const EIA_WANT: &str = "b9600 c0 d0 e0 l7 m0 pe r1 s1 i1
 dtr rts
/dev/eia0
/dev/eia0ctl
/dev/eia0status
";

/// A day, the step the rtc check sets the clock forward by.
const DAY: u64 = 86_400;

impl System {
    fn rtc_seconds(&self) -> Result<u64, String> {
        let text = self.read_file("/dev/rtc").map_err(|e| format!("{e}"))?;
        if text.len() != 12 || text.last() != Some(&b' ') {
            return Err(format!(
                "/dev/rtc reads {:?}",
                String::from_utf8_lossy(&text)
            ));
        }
        parse_seconds(&text).map_err(|e| format!("{e}"))
    }

    fn set_rtc(&self, secs: u64) -> Result<(), String> {
        let cmd = format!("echo {secs} > /dev/rtc");
        match self.run(&["sh", "-c", &cmd], None) {
            Ok(s) if s.is_empty() => Ok(()),
            Ok(s) => Err(format!("{cmd}: {s}")),
            Err(e) => Err(format!("{cmd}: {e}")),
        }
    }

    /// `/dev/rtc` agrees with `/dev/time`, and setting it moves the
    /// hardware clock; the clock is put back afterwards.
    fn rtc_test(&self) -> Result<String, String> {
        let t0 = self.rtc_seconds()?;
        let time = self.read_file("/dev/time").map_err(|e| format!("{e}"))?;
        let now = core::str::from_utf8(&time)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .ok_or("bad /dev/time")?
            / 1_000_000;
        if t0.abs_diff(now) > 2 {
            return Err(format!("rtc {t0} but /dev/time {now}"));
        }
        self.set_rtc(t0 + DAY)?;
        let t1 = self.rtc_seconds()?;
        self.set_rtc(t1 - DAY)?;
        if !(t0 + DAY..=t0 + DAY + 2).contains(&t1) {
            return Err(format!("set the rtc to {} but it reads {t1}", t0 + DAY));
        }
        Ok(String::from("rtc: read, set and put back"))
    }

    /// Sets the line, reads the status and lists the port, then writes to
    /// `eia0`, which is the console.
    fn eia_test(&self) -> Result<String, String> {
        let script = "echo b9600 l7 pe > /dev/eia0ctl; cat /dev/eia0status; \
            echo b115200 l8 pn > /dev/eia0ctl; ls /dev | grep eia";
        let out = "/tmp/kernel-eia.got";
        let status = self
            .run(&["sh", "-c", script], Some(out))
            .map_err(|e| format!("error: {e}"))?;
        let got = self.read_file(out).map_err(|e| format!("{out}: {e}"))?;
        if got != EIA_WANT.as_bytes() {
            return Err(format!(
                "output differs\n--- got\n{}--- want\n{EIA_WANT}---",
                String::from_utf8_lossy(&got)
            ));
        }
        if !status.is_empty() {
            return Err(format!("status {status:?}"));
        }
        match self.run(
            &[
                "sh",
                "-c",
                "echo 'eia0: this line went to /dev/eia0' > /dev/eia0",
            ],
            None,
        ) {
            Ok(s) if s.is_empty() => Ok(String::from("eia0: ctl, status and data ok")),
            Ok(s) => Err(format!("status {s:?}")),
            Err(e) => Err(format!("error: {e}")),
        }
    }

    /// Runs the device checks; returns the number that failed.
    pub fn device_tests(&self) -> usize {
        let mut failed = 0;
        for t in [Self::rtc_test, Self::eia_test] {
            let v = t(self);
            self.verdict(&mut failed, v);
        }
        failed
    }
}
