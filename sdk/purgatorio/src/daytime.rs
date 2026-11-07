//! Calendar time, after Inferno's `Daytime` module. There is no timezone
//! database yet, so all times are GMT.

use std::time::{SystemTime, UNIX_EPOCH};

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A broken-down time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tm {
    pub sec: u32,
    pub min: u32,
    pub hour: u32,
    /// Day of the month, 1-31.
    pub mday: u32,
    /// Month, 0-11.
    pub mon: u32,
    pub year: i64,
    /// Day of the week, 0 is Sunday.
    pub wday: u32,
}

/// Seconds since the epoch, now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Splits `t` (seconds since 1970-01-01 00:00 GMT) into calendar fields.
pub fn gmt(t: i64) -> Tm {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400) as u32;
    // Count from 0000-03-01 so that leap days fall at the end of a year:
    // 719468 days separate it from 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let mday = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mon = if mp < 10 { mp + 2 } else { mp - 10 } as u32;
    let year = yoe + era * 400 + i64::from(mon <= 1);
    Tm {
        sec: secs % 60,
        min: secs / 60 % 60,
        hour: secs / 3600,
        mday,
        mon,
        year,
        wday: (days + 4).rem_euclid(7) as u32,
    }
}

/// `Sat Nov 07 17:00:00 GMT 2026`, as `date` prints.
pub fn text(tm: &Tm) -> String {
    format!(
        "{} {} {:02} {:02}:{:02}:{:02} GMT {}",
        DAYS[tm.wday as usize], MONTHS[tm.mon as usize], tm.mday, tm.hour, tm.min, tm.sec, tm.year
    )
}

/// A file time as `ls -l` shows it: `Nov  7 17:00` within six months of
/// `now`, otherwise `Nov  7  2025`.
pub fn filet(now: i64, t: i64) -> String {
    let tm = gmt(t);
    let month = MONTHS[tm.mon as usize];
    if (now - t).abs() < 180 * 86_400 {
        format!("{month} {:2} {:02}:{:02}", tm.mday, tm.hour, tm.min)
    } else {
        format!("{month} {:2} {:5}", tm.mday, tm.year)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(text(&gmt(0)), "Thu Jan 01 00:00:00 GMT 1970");
        assert_eq!(text(&gmt(951_782_400)), "Tue Feb 29 00:00:00 GMT 2000");
        assert_eq!(text(&gmt(1_794_070_800)), "Sat Nov 07 17:00:00 GMT 2026");
        assert_eq!(text(&gmt(-1)), "Wed Dec 31 23:59:59 GMT 1969");
        assert_eq!(filet(1_794_070_800, 1_794_070_800), "Nov  7 17:00");
        assert_eq!(filet(1_794_070_800, 0), "Jan  1  1970");
    }
}
