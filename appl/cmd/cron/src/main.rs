//! cron [-f crontab] [-t time]: run commands from a crontab at the minutes
//! it names.
//!
//! The crontab defaults to `/cron/$user/cron`. Each line is five time
//! fields, minute (0-59), hour (0-23), day of the month (1-31), month (1-12)
//! and day of the week (0-6, 0 is Sunday), then a command that is run with
//! `sh -c`. A field is `*` or a comma-separated list of numbers and ranges
//! `a-b`, each optionally followed by `/step`. As in Unix cron, when both
//! day fields are restricted a day matching either one runs. `#` starts a
//! comment. Times are GMT.
//!
//! cron checks once a minute and rereads the crontab each time, so edits
//! take effect at the next minute. With `-t time` (seconds since the
//! epoch) it makes a single pass for the minute holding `time`, running the
//! due commands one after another in crontab order, and exits; tests use
//! this.

use purgatorio::arg::Arg;
use purgatorio::daytime::{self, Tm};
use purgatorio::{Context, Error};

const USAGE: &str = "cron [-f crontab] [-t time]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut file = None;
    let mut once = None;
    while let Some(c) = arg.opt() {
        match c {
            'f' => file = Some(arg.earg()?),
            't' => {
                let t = arg.earg()?;
                once = Some(t.parse::<i64>().map_err(|_| arg.usage())?);
            }
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let file = match file {
        Some(f) => f,
        None => {
            let user = std::fs::read_to_string("/dev/user").unwrap_or_default();
            format!("/cron/{}/cron", user.trim())
        }
    };
    if let Some(t) = once {
        return pass(&file, &daytime::gmt(t), true);
    }
    loop {
        // Sleep to the start of the next minute.
        let now = daytime::now();
        let next = (now.div_euclid(60) + 1) * 60;
        std::thread::sleep(std::time::Duration::from_secs((next - now) as u64));
        if let Err(e) = pass(&file, &daytime::gmt(next), false) {
            eprintln!("cron: {e}");
        }
    }
}

/// Starts every entry of `file` due at `tm`, waiting for each in turn when
/// `wait` is set.
fn pass(file: &str, tm: &Tm, wait: bool) -> Result<(), Error> {
    let text = std::fs::read_to_string(file)
        .map_err(|_| Error(format!("can't open {file}: {}", purgatorio::errstr())))?;
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let entry = match Entry::parse(line) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("cron: {file}:{}: {e}", n + 1);
                continue;
            }
        };
        if !entry.due(tm) {
            continue;
        }
        let argv = [String::from("sh"), String::from("-c"), entry.cmd];
        match purgatorio::spawn("/bin/sh.wasm", &argv, 0, &[]) {
            Ok(_) if wait => {
                purgatorio::wait()?;
            }
            Ok(_) => {}
            Err(e) => eprintln!("cron: {}: {e}", argv[2]),
        }
    }
    Ok(())
}

/// The allowed values of one field, as a bit set.
#[derive(Clone, Copy)]
struct Set(u64);

impl Set {
    fn has(self, v: u32) -> bool {
        v < 64 && self.0 & (1 << v) != 0
    }
}

struct Entry {
    min: Set,
    hour: Set,
    mday: Set,
    mon: Set,
    wday: Set,
    /// Whether the day fields were `*`.
    any_mday: bool,
    any_wday: bool,
    cmd: String,
}

impl Entry {
    fn parse(line: &str) -> Result<Entry, String> {
        let mut rest = line;
        let mut fields = Vec::new();
        for _ in 0..5 {
            rest = rest.trim_start();
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            if end == 0 {
                return Err(String::from("too few fields"));
            }
            fields.push(&rest[..end]);
            rest = &rest[end..];
        }
        let cmd = rest.trim();
        if cmd.is_empty() {
            return Err(String::from("no command"));
        }
        Ok(Entry {
            min: field(fields[0], 0, 59)?,
            hour: field(fields[1], 0, 23)?,
            mday: field(fields[2], 1, 31)?,
            mon: field(fields[3], 1, 12)?,
            wday: field(fields[4], 0, 6)?,
            any_mday: fields[2] == "*",
            any_wday: fields[4] == "*",
            cmd: String::from(cmd),
        })
    }

    fn due(&self, tm: &Tm) -> bool {
        let mday = self.mday.has(tm.mday);
        let wday = self.wday.has(tm.wday);
        let day = match (self.any_mday, self.any_wday) {
            (false, false) => mday || wday,
            _ => mday && wday,
        };
        self.min.has(tm.min) && self.hour.has(tm.hour) && self.mon.has(tm.mon + 1) && day
    }
}

fn field(s: &str, lo: u32, hi: u32) -> Result<Set, String> {
    let bad = || format!("bad field '{s}'");
    let mut set = 0u64;
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (r, st.parse::<u32>().map_err(|_| bad())?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(bad());
        }
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?)
        } else {
            let v = range.parse().map_err(|_| bad())?;
            (v, if step > 1 { hi } else { v })
        };
        if a < lo || b > hi || a > b {
            return Err(bad());
        }
        let mut v = a;
        while v <= b {
            set |= 1 << v;
            v += step;
        }
    }
    Ok(Set(set))
}
