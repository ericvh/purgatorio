//! Calendar time, after Inferno's `Daytime` module (daytime(2)).
//!
//! [`local`] uses the time zone in `$timezone` or, when that is unset,
//! `/locale/timezone`, read once. Either holds a zone in Inferno's format
//! (see [`Timezone`]) or the name of a file that does.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A broken-down time.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The time zone's name: `GMT`, `EST`, `BST`, ...
    pub zone: String,
    /// The zone's offset east of Greenwich in seconds, daylight time
    /// included.
    pub tzoff: i64,
}

/// A time zone as Inferno's `/locale` files give it: the names and
/// offsets (seconds east of Greenwich) of standard and daylight time, and
/// the periods of daylight time as pairs of start and end, in seconds
/// since the epoch as a clock on standard time counts them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timezone {
    pub stname: String,
    pub stdiff: i64,
    pub dlname: String,
    pub dldiff: i64,
    pub dlpairs: Vec<i64>,
}

/// Limbo's `int` of a string: an optional sign and the digits that
/// follow; 0 when there are none.
fn int(s: &str) -> i64 {
    let (neg, d) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let n = d.bytes().take_while(u8::is_ascii_digit).fold(0i64, |n, b| {
        n.saturating_mul(10).saturating_add(i64::from(b - b'0'))
    });
    if neg { -n } else { n }
}

impl Timezone {
    /// Greenwich, with no daylight time.
    pub fn gmt() -> Timezone {
        Timezone {
            stname: String::from("GMT"),
            stdiff: 0,
            dlname: String::new(),
            dldiff: 0,
            dlpairs: Vec::new(),
        }
    }

    /// A zone's text: `STD stdiff DL dldiff` and the pairs, separated by
    /// blanks and newlines. Fewer than four words give GMT, as in Inferno.
    pub fn parse(s: &str) -> Timezone {
        let w: Vec<&str> = s
            .split([' ', '\t', '\n', '\r'])
            .filter(|w| !w.is_empty())
            .collect();
        if w.len() < 4 {
            return Timezone::gmt();
        }
        Timezone {
            stname: String::from(w[0]),
            stdiff: int(w[1]),
            dlname: String::from(w[2]),
            dldiff: int(w[3]),
            dlpairs: w[4..].iter().map(|v| int(v)).collect(),
        }
    }

    /// Reads a zone as daytime(2)'s `readtimezone` does. With no `name`:
    /// `$timezone`, or `/locale/timezone` when that is unset or empty. A
    /// name that does not start with `/` or `#` is a file in `/locale`.
    /// When the text read is itself a path (starts with `/` or `#`), the
    /// zone is read from that file. Anything missing gives GMT.
    pub fn read(name: Option<&str>) -> Timezone {
        let file = |p: &str| std::fs::read_to_string(p).ok().filter(|s| !s.is_empty());
        let s = match name {
            None => crate::env::get("timezone")
                .filter(|s| !s.is_empty())
                .or_else(|| file("/locale/timezone")),
            Some(n) if n.starts_with(['/', '#']) => file(n),
            Some(n) => file(&format!("/locale/{n}")),
        };
        let Some(mut s) = s else {
            return Timezone::gmt();
        };
        if s.starts_with(['/', '#']) {
            let p = s.strip_suffix('\n').unwrap_or(&s);
            match file(p) {
                Some(t) => s = t,
                None => return Timezone::gmt(),
            }
        }
        Timezone::parse(&s)
    }

    /// `t` as local time in this zone.
    pub fn local(&self, t: i64) -> Tm {
        let st = t.saturating_add(self.stdiff);
        let dl = self
            .dlpairs
            .as_chunks::<2>()
            .0
            .iter()
            .any(|&[a, b]| st >= a && st < b);
        let (name, off) = if dl {
            (&self.dlname, self.dldiff)
        } else {
            (&self.stname, self.stdiff)
        };
        let mut tm = gmt(t.saturating_add(off));
        tm.zone.clone_from(name);
        tm.tzoff = off;
        tm
    }
}

/// The zone [`local`] uses, read the first time it is needed.
pub fn timezone() -> &'static Timezone {
    static TZ: OnceLock<Timezone> = OnceLock::new();
    TZ.get_or_init(|| Timezone::read(None))
}

/// `t` (seconds since the epoch) as local time: daytime(2)'s `local`.
pub fn local(t: i64) -> Tm {
    timezone().local(t)
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
        zone: String::from("GMT"),
        tzoff: 0,
    }
}

/// `Sat Nov 07 17:00:00 GMT 2026`, as `date` prints, with the time's
/// zone.
pub fn text(tm: &Tm) -> String {
    format!(
        "{} {} {:02} {:02}:{:02}:{:02} {} {}",
        DAYS[tm.wday as usize],
        MONTHS[tm.mon as usize],
        tm.mday,
        tm.hour,
        tm.min,
        tm.sec,
        tm.zone,
        tm.year
    )
}

/// A file time as `ls -l` shows it, in local time: `Nov  7 17:00` within
/// six months of `now`, otherwise `Nov  7  2025`.
pub fn filet(now: i64, t: i64) -> String {
    let tm = local(t);
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
    }

    const EASTERN: &str = "EST -18000 EDT -14400\n   9943200   25664400   41392800   57718800\n\
        1772935200 1793494800\n";

    #[test]
    fn zones() {
        let tz = Timezone::parse(EASTERN);
        assert_eq!(
            (tz.stdiff, tz.dldiff, tz.dlpairs.len()),
            (-18000, -14400, 6)
        );
        // 2026-11-13 12:34:56 GMT is standard time; 2026-07-04 is not.
        assert_eq!(
            text(&tz.local(1_794_573_296)),
            "Fri Nov 13 07:34:56 EST 2026"
        );
        assert_eq!(
            text(&tz.local(1_783_166_400)),
            "Sat Jul 04 08:00:00 EDT 2026"
        );
        // The changes: 2026-03-08 07:00 GMT and 2026-11-01 06:00 GMT.
        assert_eq!(tz.local(1_772_953_199).zone, "EST");
        assert_eq!(tz.local(1_772_953_200).zone, "EDT");
        assert_eq!(tz.local(1_793_512_799).zone, "EDT");
        assert_eq!(tz.local(1_793_512_800).zone, "EST");
        assert_eq!(tz.local(1_793_512_800).tzoff, -18000);
        // 1970: the first pair.
        assert_eq!(text(&tz.local(10_000_000)), "Sun Apr 26 13:46:40 EDT 1970");
        let jst = Timezone::parse("JST 32400 JST 32400\n");
        assert_eq!(text(&jst.local(0)), "Thu Jan 01 09:00:00 JST 1970");
        assert_eq!(Timezone::parse("EST -18000"), Timezone::gmt());
        assert_eq!(Timezone::parse(""), Timezone::gmt());
        assert_eq!(int("-5x"), -5);
        assert_eq!(int("x"), 0);
    }
}
