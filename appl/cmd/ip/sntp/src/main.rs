//! ip/sntp [-di] [server]: ask an SNTP server (RFC 4330; default
//! `udp!$ntp!ntp`, which needs `ndb/cs`) for the time and, if the answer
//! is plausible, set this machine's clock. A server named without a
//! network is `udp!server!123`. `-d` prints the time received; `-i` only
//! asks, and sets nothing.
//!
//! The clock is set by writing `/dev/time`; on a platform whose clock
//! cannot be set that fails, and sntp says so.

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::{SystemTime, UNIX_EPOCH};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error, daytime, dial};

const USAGE: &str = "sntp [-di] [server]";
/// Seconds from 1900, NTP's epoch, to 1970.
const EPOCH: u64 = 2_208_988_800;
/// Answers before 2020 are not believed.
const PLAUSIBLE: u64 = 1_577_836_800;
const TRIES: usize = 3;
const WAIT_MS: u64 = 2000;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut debug = false;
    let mut ask_only = false;
    while let Some(c) = arg.opt() {
        match c {
            'd' => debug = true,
            'i' => ask_only = true,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let server = match rest.as_slice() {
        [] => String::from("udp!$ntp!ntp"),
        [s] if s.contains('!') => s.clone(),
        [s] => format!("udp!{s}!123"),
        _ => return Err(Error(format!("usage: {USAGE}"))),
    };
    let fail = Error;
    let conn = dial::dial(&server).map_err(|e| fail(format!("can't dial {server}: {e}")))?;
    let data = conn
        .data
        .ok_or_else(|| fail(String::from("no connection")))?;
    let fd = data.as_raw_fd();
    let mut f = File::from(data);
    let mut secs = None;
    for _ in 0..TRIES {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut q = [0u8; 48];
        q[0] = 0x23; // version 4, client
        let tx = now.as_secs() + EPOCH;
        q[40..44].copy_from_slice(&(tx as u32).to_be_bytes());
        q[44..48].copy_from_slice(&now.subsec_nanos().to_be_bytes());
        f.write_all(&q).map_err(|e| fail(Error::from(e).0))?;
        let Some(r) = dial::read_timeout(fd, 1024, WAIT_MS).map_err(|e| fail(e.0))? else {
            continue;
        };
        if r.len() < 48 || !matches!(r[0] & 7, 4 | 5) || r[1] == 0 || r[24..32] != q[40..48] {
            if debug {
                eprintln!("sntp: ignoring a bad reply");
            }
            continue;
        }
        let t = u64::from(u32::from_be_bytes([r[40], r[41], r[42], r[43]]));
        secs = Some(t.saturating_sub(EPOCH));
        break;
    }
    let secs = secs.ok_or_else(|| fail(String::from("no reply from server")))?;
    if debug {
        let tm = daytime::gmt(secs as i64);
        println!("sntp: server time {} ({secs})", daytime::text(&tm));
    }
    if secs < PLAUSIBLE {
        return Err(fail(format!("implausible time {secs}")));
    }
    if ask_only {
        return Ok(());
    }
    std::fs::write("/dev/time", format!("{}", secs * 1_000_000))
        .map_err(|_| fail(format!("can't set the time: {}", purgatorio::errstr())))?;
    Ok(())
}
