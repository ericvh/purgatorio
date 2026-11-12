//! ip/ping [-alq] [-s msgsize] [-i millisecs] [-n #pings] destination:
//! send ICMP echo requests to `destination` through `/net/icmp` and time
//! the replies, as Inferno's ping(8).
//!
//! It sends `-n` messages (32) of `-s` bytes (64, at least 32) `-i`
//! milliseconds apart (1000). Each reply prints its sequence number, its
//! round-trip time, the average so far and the reply's TTL; `-a` adds the
//! source and destination addresses, `-l` prints only the lost messages
//! and `-q` only the summary. If any message is lost, ping says how many
//! and exits with `fail:lost messages`.
//!
//! A message is lost if no reply comes before the next one is due, or
//! within a second when the interval is shorter. (Inferno sends and
//! receives in separate threads; a guest sends one message at a time.)
//!
//! The message is what Inferno's `icmp` data file takes: 20 bytes of room
//! for the IPv4 header, which the stack fills in, then the ICMP echo
//! request (type, code, checksum, identifier, sequence) and data whose
//! byte `i` is `i mod 256`. A reply comes back with its IPv4 header.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::{Duration, Instant};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error, dial};

const USAGE: &str = "ip/ping [-alq] [-s msgsize] [-i millisecs] [-n #pings] destination";
const IP_HEADER: usize = 20;
const ICMP_HEADER: usize = 8;
const ECHO_REQUEST: u8 = 8;
const ECHO_REPLY: u8 = 0;
const MAX_MSG: usize = 65 * 1024 - 1;
/// The least time a reply is waited for.
const MIN_WAIT_MS: u64 = 1000;

struct Opts {
    addrs: bool,
    lost_only: bool,
    quiet: bool,
    size: usize,
    interval: u64,
    count: u32,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some(READER_FLAG) {
        return reader();
    }
    let mut arg = Arg::new(args, USAGE);
    let mut o = Opts {
        addrs: false,
        lost_only: false,
        quiet: false,
        size: 64,
        interval: 1000,
        count: 32,
    };
    let number = |v: Option<String>| -> Result<u64, Error> {
        v.and_then(|s| s.parse().ok())
            .ok_or_else(|| Error(format!("usage: {USAGE}")))
    };
    while let Some(c) = arg.opt() {
        match c {
            'a' => o.addrs = true,
            'l' => o.lost_only = true,
            'q' => o.quiet = true,
            'd' => {}
            's' => o.size = number(arg.arg())? as usize,
            'i' => o.interval = number(arg.arg())?,
            'n' => o.count = number(arg.arg())?.min(u64::from(u32::MAX)) as u32,
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let [dest] = rest.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    if o.size < 32 {
        o.size = 64;
    }
    o.size = o.size.min(MAX_MSG);
    let addr = match dest.split('!').count() {
        1 => format!("icmp!{dest}!1"),
        2 => format!("icmp!{dest}"),
        _ => dest.clone(),
    };
    let conn = match dial::dial(&addr) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ip/ping: can't dial {addr}: {e}");
            purgatorio::exits("fail:dial");
        }
    };
    let Some(data) = conn.data else {
        eprintln!("ip/ping: can't dial {addr}: no data file");
        purgatorio::exits("fail:dial");
    };
    println!(
        "sending {} {} byte messages {} ms apart",
        o.count, o.size, o.interval
    );
    let data = File::from(data);
    let mut replies = Replies::start(&data)?;
    let lost = ping(&o, data, &mut replies);
    drop(replies);
    let lost = lost?;
    drop(conn.ctl);
    if lost > 0 {
        purgatorio::exits("fail:lost messages");
    }
    Ok(())
}

/// Sends the messages; returns how many were lost.
fn ping(o: &Opts, mut out: File, replies: &mut Replies) -> Result<u32, Error> {
    let (mut rcvd, mut lost, mut sum) = (0u32, 0u32, 0u64);
    let wait = o.interval.max(MIN_WAIT_MS);
    for i in 0..o.count {
        let seq = i as u16;
        let msg = request(o.size, seq);
        let sent = Instant::now();
        if let Err(e) = out.write_all(&msg) {
            eprintln!("ping: write failed: {e}");
            return Err(Error(String::from("write failed")));
        }
        let mut answered = false;
        loop {
            let left = wait.saturating_sub(sent.elapsed().as_millis() as u64);
            if left == 0 {
                break;
            }
            let r = match replies.next(left) {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(e) => {
                    println!("read: {e}");
                    break;
                }
            };
            let rtt = sent.elapsed().as_micros() as u64;
            if r.len() < o.size {
                println!("bad len {}/{}", r.len(), o.size);
                continue;
            }
            let (ptype, code) = (r[IP_HEADER], r[IP_HEADER + 1]);
            let rseq = u16::from_be_bytes([r[IP_HEADER + 6], r[IP_HEADER + 7]]);
            if ptype != ECHO_REPLY || code != 0 {
                println!("bad type/code {ptype}/{code} seq {rseq}");
                continue;
            }
            if rseq != seq {
                // A late reply to a message already counted lost.
                continue;
            }
            if r[IP_HEADER + ICMP_HEADER..o.size] != msg[IP_HEADER + ICMP_HEADER..] {
                println!("corrupted reply");
            }
            rcvd += 1;
            sum += rtt;
            answered = true;
            if !o.quiet && !o.lost_only {
                let ttl = r[8];
                let route = if o.addrs {
                    format!("{}->{} ", ip(&r[12..16]), ip(&r[16..20]))
                } else {
                    String::new()
                };
                println!(
                    "{i}: {route}rtt {rtt} µs, avg rtt {} µs, ttl = {ttl}",
                    sum / u64::from(rcvd)
                );
            }
            break;
        }
        if !answered {
            lost += 1;
            if !o.quiet {
                let avg = if rcvd == 0 { 0 } else { sum / u64::from(rcvd) };
                println!("lost {i}: avg rtt {avg} µs");
            }
        }
        if i + 1 < o.count {
            let due = Duration::from_millis(o.interval);
            if let Some(rest) = due.checked_sub(sent.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    }
    if lost > 0 {
        println!("{lost} out of {} message(s) lost", lost + rcvd);
    }
    Ok(lost)
}

/// The argument that runs ping as its own reply reader.
const READER_FLAG: &str = "-reader";
/// This command, which the reader is a copy of.
const SELF: &str = "/bin/ip/ping.wasm";

/// The replies, read by a second prog. A read of `/net/icmp` blocks
/// with no timeout, so a copy of ping ([`reader`]) reads the data file
/// and passes each message on through a pipe, which this prog can wait
/// on with a timeout (`purgatorio.wait`).
struct Replies {
    pipe: File,
    pid: u32,
}

impl Replies {
    fn start(data: &File) -> Result<Replies, Error> {
        let [r, w] = purgatorio::pipe()?;
        let pid = purgatorio::spawn(
            SELF,
            &[String::from("ping"), String::from(READER_FLAG)],
            purgatorio::abi::pctl::NEWFD,
            &[(data.as_raw_fd(), 0), (w.as_raw_fd(), 1), (2, 2)],
        )?;
        drop(w);
        Ok(Replies {
            pipe: File::from(r),
            pid,
        })
    }

    /// The next reply, if one comes within `ms` milliseconds.
    fn next(&mut self, ms: u64) -> Result<Option<Vec<u8>>, Error> {
        let ops = [0i32, self.pipe.as_raw_fd()];
        let ms = ms.min(i32::MAX as u64) as i32;
        // SAFETY: one (kind, fd) pair, as `purgatorio.wait` reads it.
        if unsafe { purgatorio::sys::wait(ops.as_ptr(), 1, ms) } < 0 {
            return Ok(None);
        }
        let mut len = [0u8; 4];
        self.pipe.read_exact(&mut len)?;
        let mut msg = vec![0u8; u32::from_le_bytes(len) as usize];
        self.pipe.read_exact(&mut msg)?;
        Ok(Some(msg))
    }
}

/// The reader is killed and waited for, so it does not outlive ping.
impl Drop for Replies {
    fn drop(&mut self) {
        let _ = std::fs::write(format!("/prog/{}/ctl", self.pid), "kill");
        let _ = purgatorio::wait();
    }
}

/// The reply reader: copies each message read from fd 0 (the icmp data
/// file) to fd 1 (the pipe) after its length, in one write.
fn reader() -> Result<(), Error> {
    // SAFETY: fds 0 and 1 are the ones ping gave this prog.
    let (mut data, mut pipe) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
    let mut buf = vec![0u8; MAX_MSG];
    loop {
        let n = data.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let mut frame = (n as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&buf[..n]);
        pipe.write_all(&frame)?;
    }
}

/// An echo request of `size` bytes, IPv4 header room included.
fn request(size: usize, seq: u16) -> Vec<u8> {
    let mut m = vec![0u8; size];
    m[IP_HEADER] = ECHO_REQUEST;
    m[IP_HEADER + 6..IP_HEADER + 8].copy_from_slice(&seq.to_be_bytes());
    for (i, b) in m.iter_mut().enumerate().skip(IP_HEADER + ICMP_HEADER) {
        *b = i as u8;
    }
    m
}

fn ip(b: &[u8]) -> String {
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_layout() {
        let m = request(64, 0x0102);
        assert_eq!(m.len(), 64);
        assert!(m[..IP_HEADER].iter().all(|&b| b == 0));
        assert_eq!(&m[20..28], &[8, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(m[28], 28);
        assert_eq!(m[63], 63);
    }
}
