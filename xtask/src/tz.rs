//! `cargo xtask timezones [--check]`: makes `root/locale` from the IANA
//! time zone database in `tzdata/`, in the format Inferno's daytime(2)
//! reads.
//!
//! An Inferno timezone file is one line `STD stdiff DL dldiff` (names and
//! offsets east of Greenwich in seconds) and then the daylight periods as
//! pairs of times. A pair is the start and end of a period in seconds
//! since 1970 as a clock on standard time would count them (the instant
//! plus `stdiff`), because `local` compares `t + stdiff` with them.
//!
//! This is a small zic(8): it reads the Rule, Zone and Link lines of the
//! sources and works out each zone's offsets from 1970 to 2037, the range
//! a 32-bit `int` holds. The format has one standard and one daylight
//! offset, so a zone gets those in force at the end of the range, and
//! every daylight period of the range; a zone whose standard offset
//! changed shows those years with today's. `tzdata/zones` names the files
//! and their zones. Everything is integer arithmetic on the text, so the
//! output is the same on every host. The output is committed, and the
//! test `locale_is_current` makes it again and compares.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::{Result, workspace};

/// The region files of the database that are read.
const SOURCES: &[&str] = &[
    "africa",
    "antarctica",
    "asia",
    "australasia",
    "backward",
    "etcetera",
    "europe",
    "northamerica",
    "southamerica",
];

/// The table covers 1970 to the end of 2037.
const FIRST: i64 = 0;
const LAST_YEAR: i64 = 2037;
/// The largest time a 32-bit `int` holds, 2038-01-19.
const INT_MAX: i64 = i32::MAX as i64;

/// When in the day a time is given: wall clock, standard time or UT.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Wall,
    Std,
    Univ,
}

/// A Rule's or Zone's day of the month.
#[derive(Clone, Copy, Debug)]
enum Day {
    Num(i64),
    /// The last of a weekday (0 is Sunday) in the month.
    Last(i64),
    /// The first weekday on or after the day.
    OnOrAfter(i64, i64),
    /// The last weekday on or before the day.
    OnOrBefore(i64, i64),
}

#[derive(Debug)]
struct Rule {
    from: i64,
    to: i64,
    /// 0 to 11.
    month: i64,
    day: Day,
    at: i64,
    kind: Kind,
    save: i64,
    letters: String,
}

#[derive(Debug)]
enum Rules {
    None,
    Fixed(i64),
    Named(String),
}

#[derive(Debug)]
struct ZoneLine {
    stdoff: i64,
    rules: Rules,
    format: String,
    /// Year, month, day, time of day and its kind.
    until: Option<(i64, i64, Day, i64, Kind)>,
}

/// The database: rules by name, zones by name, links to zones.
#[derive(Default)]
struct Db {
    rules: BTreeMap<String, Vec<Rule>>,
    zones: BTreeMap<String, Vec<ZoneLine>>,
    links: BTreeMap<String, String>,
}

/// A span of time with one offset: from `start` (UT) to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Span {
    start: i64,
    utoff: i64,
    dst: bool,
    abbr: String,
}

/// Days from 1970-01-01 to `y`-`m`-`d` (`m` 0 to 11, `d` from 1).
fn days(y: i64, m: i64, d: i64) -> i64 {
    let (y, m) = (y + m.div_euclid(12), m.rem_euclid(12) + 1);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn month_len(y: i64, m: i64) -> i64 {
    days(y, m + 1, 1) - days(y, m, 1)
}

/// 0 is Sunday.
fn weekday(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

/// The day (since 1970) that `d` names in `y`-`m`.
fn day_of(y: i64, m: i64, d: Day) -> i64 {
    match d {
        Day::Num(n) => days(y, m, n),
        Day::Last(w) => {
            let last = days(y, m, month_len(y, m));
            last - (weekday(last) - w).rem_euclid(7)
        }
        Day::OnOrAfter(w, n) => {
            let x = days(y, m, n);
            x + (w - weekday(x)).rem_euclid(7)
        }
        Day::OnOrBefore(w, n) => {
            let x = days(y, m, n);
            x - (weekday(x) - w).rem_euclid(7)
        }
    }
}

/// Matches `word` against `names` by unambiguous prefix, as zic does.
fn lookup(word: &str, names: &[&str]) -> Option<i64> {
    let w = word.to_ascii_lowercase();
    let hits: Vec<usize> = (0..names.len())
        .filter(|&i| !w.is_empty() && names[i].to_ascii_lowercase().starts_with(&w))
        .collect();
    match hits[..] {
        [i] => Some(i as i64),
        _ => names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(word))
            .map(|i| i as i64),
    }
}

const MONTHS: &[&str] = &[
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: &[&str] = &[
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn month(s: &str) -> Result<i64> {
    lookup(s, MONTHS).ok_or_else(|| format!("bad month {s:?}"))
}

fn day(s: &str) -> Result<Day> {
    let bad = || format!("bad day {s:?}");
    if let Some(w) = s.strip_prefix("last") {
        return Ok(Day::Last(lookup(w, WEEKDAYS).ok_or_else(bad)?));
    }
    for (op, ge) in [(">=", true), ("<=", false)] {
        if let Some((w, n)) = s.split_once(op) {
            let w = lookup(w, WEEKDAYS).ok_or_else(bad)?;
            let n = n.parse().map_err(|_| bad())?;
            return Ok(if ge {
                Day::OnOrAfter(w, n)
            } else {
                Day::OnOrBefore(w, n)
            });
        }
    }
    s.parse().map(Day::Num).map_err(|_| bad())
}

/// `[-]h[:mm[:ss]]` in seconds; `-` alone is 0.
fn hms(s: &str) -> Result<i64> {
    if s == "-" {
        return Ok(0);
    }
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let mut v = 0;
    let mut n = 0;
    for (i, part) in s.split(':').enumerate() {
        let p: i64 = part.parse().map_err(|_| format!("bad time {s:?}"))?;
        v += p * [3600, 60, 1]
            .get(i)
            .ok_or_else(|| format!("bad time {s:?}"))?;
        n += 1;
    }
    if n == 0 {
        return Err(format!("bad time {s:?}"));
    }
    Ok(if neg { -v } else { v })
}

/// A time of day with an optional kind suffix.
fn at(s: &str) -> Result<(i64, Kind)> {
    let (s, kind) = match s.chars().last() {
        Some('w') => (&s[..s.len() - 1], Kind::Wall),
        Some('s') => (&s[..s.len() - 1], Kind::Std),
        Some('u' | 'g' | 'z') => (&s[..s.len() - 1], Kind::Univ),
        _ => (s, Kind::Wall),
    };
    Ok((hms(s)?, kind))
}

fn year(s: &str, from: i64) -> Result<i64> {
    match s.to_ascii_lowercase().as_str() {
        "only" | "o" => Ok(from),
        "max" | "ma" => Ok(i64::MAX),
        "min" | "mi" => Ok(i64::MIN),
        _ => s.parse().map_err(|_| format!("bad year {s:?}")),
    }
}

fn zone_line(f: &[&str]) -> Result<ZoneLine> {
    if f.len() < 3 {
        return Err(format!("short zone line {f:?}"));
    }
    let rules = match f[1] {
        "-" => Rules::None,
        r if r.starts_with(|c: char| c.is_ascii_digit() || c == '-') => Rules::Fixed(hms(r)?),
        r => Rules::Named(String::from(r)),
    };
    let until = match &f[3..] {
        [] => None,
        u => {
            let y = u[0].parse().map_err(|_| format!("bad year {:?}", u[0]))?;
            let m = u.get(1).map_or(Ok(0), |s| month(s))?;
            let d = u.get(2).map_or(Ok(Day::Num(1)), |s| day(s))?;
            let (t, k) = u.get(3).map_or(Ok((0, Kind::Wall)), |s| at(s))?;
            Some((y, m, d, t, k))
        }
    };
    Ok(ZoneLine {
        stdoff: hms(f[0])?,
        rules,
        format: String::from(f[2]),
        until,
    })
}

impl Db {
    fn parse(&mut self, text: &str) -> Result<()> {
        let mut zone: Option<String> = None;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("");
            let f: Vec<&str> = line.split_ascii_whitespace().collect();
            if f.is_empty() {
                continue;
            }
            let cont = line.starts_with(char::is_whitespace);
            if cont {
                let name = zone
                    .as_ref()
                    .ok_or_else(|| format!("continuation without a zone: {line:?}"))?;
                let z = zone_line(&f)?;
                let more = z.until.is_some();
                self.zones.get_mut(name).expect("added").push(z);
                if !more {
                    zone = None;
                }
                continue;
            }
            match f[0] {
                "Rule" if f.len() == 10 => {
                    let from = year(f[2], 0)?;
                    let (at, kind) = at(f[7])?;
                    let r = Rule {
                        from,
                        to: year(f[3], from)?,
                        month: month(f[5])?,
                        day: day(f[6])?,
                        at,
                        kind,
                        save: hms(f[8])?,
                        letters: if f[9] == "-" {
                            String::new()
                        } else {
                            String::from(f[9])
                        },
                    };
                    self.rules.entry(String::from(f[1])).or_default().push(r);
                }
                "Zone" if f.len() >= 5 => {
                    let z = zone_line(&f[2..])?;
                    let more = z.until.is_some();
                    self.zones.insert(String::from(f[1]), vec![z]);
                    zone = more.then(|| String::from(f[1]));
                }
                "Link" if f.len() == 3 => {
                    self.links.insert(String::from(f[2]), String::from(f[1]));
                }
                _ => return Err(format!("bad line {line:?}")),
            }
        }
        Ok(())
    }

    /// The zone lines of `name`, following links.
    fn zone(&self, name: &str) -> Option<&[ZoneLine]> {
        let mut n = name;
        for _ in 0..8 {
            if let Some(z) = self.zones.get(n) {
                return Some(z);
            }
            n = self.links.get(n)?;
        }
        None
    }

    /// The spans of zone `name` up to the end of [`LAST_YEAR`] + 1.
    fn spans(&self, name: &str) -> Result<Vec<Span>> {
        let lines = self.zone(name).ok_or_else(|| format!("no zone {name}"))?;
        let mut out: Vec<Span> = Vec::new();
        let mut start = i64::MIN;
        let end_year = LAST_YEAR + 1;
        for z in lines {
            if start != i64::MIN && start >= days(end_year + 1, 0, 1) * 86_400 {
                break;
            }
            // The rule changes in force during this line, with their UT.
            let mut changes: Vec<(i64, i64, &str)> = Vec::new();
            let mut first_std = "";
            if let Rules::Named(r) = &z.rules {
                let rules = self
                    .rules
                    .get(r)
                    .ok_or_else(|| format!("{name}: no rule {r}"))?;
                first_std = rules
                    .iter()
                    .filter(|r| r.save == 0)
                    .min_by_key(|r| r.from)
                    .map_or("", |r| r.letters.as_str());
                let y0 = if start == i64::MIN {
                    rules.iter().map(|r| r.from).min().unwrap_or(1900).max(1800)
                } else {
                    civil_year(start) - 1
                };
                let y1 = z.until.map_or(end_year, |u| u.0.min(end_year));
                let mut local = Vec::new();
                for y in y0..=y1 {
                    for r in rules.iter().filter(|r| r.from <= y && y <= r.to) {
                        let t = day_of(y, r.month, r.day) * 86_400 + r.at;
                        local.push((t, r));
                    }
                }
                local.sort_by_key(|(t, _)| *t);
                let mut save = 0;
                for (t, r) in local {
                    let ut = match r.kind {
                        Kind::Univ => t,
                        Kind::Std => t - z.stdoff,
                        Kind::Wall => t - z.stdoff - save,
                    };
                    changes.push((ut, r.save, r.letters.as_str()));
                    save = r.save;
                }
            }
            // What is in force when the line starts.
            let (mut save, mut letters) = match &z.rules {
                Rules::None => (0, ""),
                Rules::Fixed(s) => (*s, ""),
                Rules::Named(_) => changes
                    .iter()
                    .rev()
                    .find(|c| c.0 <= start)
                    .map_or((0, first_std), |c| (c.1, c.2)),
            };
            // When the line ends.
            let until = match z.until {
                None => i64::MAX,
                Some((y, m, d, t, k)) => {
                    let local = day_of(y, m, d) * 86_400 + t;
                    let mut s = save;
                    for c in changes.iter().filter(|c| c.0 > start) {
                        if c.0 < local - z.stdoff - s {
                            s = c.1;
                        } else {
                            break;
                        }
                    }
                    match k {
                        Kind::Univ => local,
                        Kind::Std => local - z.stdoff,
                        Kind::Wall => local - z.stdoff - s,
                    }
                }
            };
            push(&mut out, span(start, z, save, letters));
            for c in changes.iter().filter(|c| c.0 > start && c.0 < until) {
                (save, letters) = (c.1, c.2);
                push(&mut out, span(c.0, z, save, letters));
            }
            start = until;
        }
        Ok(out)
    }
}

/// The year of UT time `t`.
fn civil_year(t: i64) -> i64 {
    let d = t.div_euclid(86_400);
    let mut y = 1970 + d.div_euclid(366);
    while days(y + 1, 0, 1) <= d {
        y += 1;
    }
    y
}

fn span(start: i64, z: &ZoneLine, save: i64, letters: &str) -> Span {
    let utoff = z.stdoff + save;
    let dst = save != 0;
    let abbr = if let Some((s, d)) = z.format.split_once('/') {
        String::from(if dst { d } else { s })
    } else if z.format.contains("%z") {
        z.format.replace("%z", &numeric(utoff))
    } else {
        z.format.replace("%s", letters)
    };
    Span {
        start,
        utoff,
        dst,
        abbr,
    }
}

/// `%z`: `+05`, `+0530` or `-033045`.
fn numeric(off: i64) -> String {
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
    match (m, s) {
        (0, 0) => format!("{sign}{h:02}"),
        (_, 0) => format!("{sign}{h:02}{m:02}"),
        _ => format!("{sign}{h:02}{m:02}{s:02}"),
    }
}

/// Adds `s`, dropping it if it changes nothing and replacing a span that
/// starts at the same time.
fn push(out: &mut Vec<Span>, s: Span) {
    if let Some(last) = out.last_mut() {
        if last.start == s.start {
            *last = s;
            let n = out.len();
            if n >= 2 && same(&out[n - 2], &out[n - 1]) {
                out.pop();
            }
            return;
        }
        if same(last, &s) {
            return;
        }
    }
    out.push(s);
}

fn same(a: &Span, b: &Span) -> bool {
    a.utoff == b.utoff && a.dst == b.dst && a.abbr == b.abbr
}

/// The Inferno timezone file for `spans`.
fn inferno(spans: &[Span]) -> String {
    let end = days(LAST_YEAR + 1, 0, 1) * 86_400;
    let in_range: Vec<(i64, i64, &Span)> = spans
        .iter()
        .enumerate()
        .map(|(i, s)| (s.start, spans.get(i + 1).map_or(i64::MAX, |n| n.start), s))
        .filter(|&(a, b, _)| b > FIRST && a < end)
        .collect();
    let std = in_range
        .iter()
        .rev()
        .find(|s| !s.2.dst)
        .or(in_range.last())
        .expect("a span in range")
        .2;
    let dl = in_range.iter().rev().find(|s| s.2.dst).map_or(std, |s| s.2);
    let mut s = format!("{} {} {} {}\n", std.abbr, std.utoff, dl.abbr, dl.utoff);
    let mut times = Vec::new();
    for &(a, b, sp) in &in_range {
        if sp.dst {
            // A period under way in 1970 keeps its real start.
            let a = (a.max(-INT_MAX) + std.utoff).min(INT_MAX);
            let b = b.saturating_add(std.utoff).min(INT_MAX);
            if a < b {
                times.push(a);
                times.push(b);
            }
        }
    }
    for line in times.chunks(6) {
        let words: Vec<String> = line.iter().map(|t| format!("{t:10}")).collect();
        s.push_str(&words.join(" "));
        s.push('\n');
    }
    s
}

/// Every file of `root/locale`, by name, made from `tzdata/`.
pub fn generate(tzdir: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let read = |n: &str| {
        fs::read_to_string(tzdir.join(n)).map_err(|e| format!("{}: {e}", tzdir.join(n).display()))
    };
    let mut db = Db::default();
    for s in SOURCES {
        db.parse(&read(s)?)
            .map_err(|e| format!("tzdata/{s}: {e}"))?;
    }
    let mut out = BTreeMap::new();
    for line in read("zones")?.lines() {
        let line = line.split('#').next().unwrap_or("");
        let f: Vec<&str> = line.split_ascii_whitespace().collect();
        match f[..] {
            [] => {}
            [file, zone] => {
                let spans = db.spans(zone)?;
                out.insert(String::from(file), inferno(&spans).into_bytes());
            }
            [file, "=", other] => {
                let data = out
                    .get(other)
                    .ok_or_else(|| format!("tzdata/zones: {other} not made yet"))?
                    .clone();
                out.insert(String::from(file), data);
            }
            _ => return Err(format!("tzdata/zones: bad line {line:?}")),
        }
    }
    out.insert(String::from("README"), read("README.locale")?.into_bytes());
    Ok(out)
}

pub fn cmd(args: &[String]) -> Result<()> {
    let check = match args {
        [] => false,
        [a] if a == "--check" => true,
        _ => return Err(String::from("usage: cargo xtask timezones [--check]")),
    };
    let ws = workspace();
    let made = generate(&ws.join("tzdata"))?;
    let dest = ws.join("root/locale");
    if check {
        return compare(&made, &dest);
    }
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    for (n, data) in &made {
        fs::write(dest.join(n), data).map_err(|e| format!("{n}: {e}"))?;
    }
    let bytes: usize = made.values().map(Vec::len).sum();
    eprintln!("root/locale: {} files, {bytes} bytes", made.len());
    Ok(())
}

/// Fails unless `dest` holds exactly the files `made`.
fn compare(made: &BTreeMap<String, Vec<u8>>, dest: &Path) -> Result<()> {
    let mut have = BTreeMap::new();
    for e in fs::read_dir(dest).map_err(|e| format!("{}: {e}", dest.display()))? {
        let p = e.map_err(|e| e.to_string())?.path();
        let n = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        have.insert(
            n,
            fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?,
        );
    }
    let mut bad = Vec::new();
    for (n, d) in made {
        match have.get(n) {
            None => bad.push(format!("missing from root/locale: {n}")),
            Some(h) if h != d => bad.push(format!("differs: {n}")),
            Some(_) => {}
        }
    }
    for n in have.keys().filter(|n| !made.contains_key(*n)) {
        bad.push(format!("not made from tzdata: {n}"));
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "root/locale is not what `cargo xtask timezones` makes:\n{}",
            bad.join("\n")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar() {
        assert_eq!(days(1970, 0, 1), 0);
        assert_eq!(days(2000, 2, 1), 11_017);
        assert_eq!(weekday(days(2026, 10, 13)), 5);
        // US daylight time started on the second Sunday of March 2026,
        // and ended on the first Sunday of November.
        assert_eq!(day_of(2026, 2, Day::OnOrAfter(0, 8)), days(2026, 2, 8));
        assert_eq!(day_of(2026, 10, Day::OnOrAfter(0, 1)), days(2026, 10, 1));
        assert_eq!(day_of(2026, 9, Day::Last(0)), days(2026, 9, 25));
        assert_eq!(day_of(2026, 3, Day::OnOrBefore(5, 1)), days(2026, 2, 27));
        assert_eq!(civil_year(-1), 1969);
        assert_eq!(civil_year(days(2037, 11, 31) * 86_400), 2037);
        assert_eq!(numeric(19_800), "+0530");
        assert_eq!(numeric(-10_800), "-03");
        assert_eq!(hms("-0:25:21").unwrap(), -1521);
    }

    const US: &str = "\
Rule US 1967 2006 - Oct lastSun 2:00 0 S
Rule US 1967 1973 - Apr lastSun 2:00 1:00 D
Rule US 1974 only - Jan 6 2:00 1:00 D
Rule US 1975 only - Feb lastSun 2:00 1:00 D
Rule US 1976 1986 - Apr lastSun 2:00 1:00 D
Rule US 1987 2006 - Apr Sun>=1 2:00 1:00 D
Rule US 2007 max - Mar Sun>=8 2:00 1:00 D
Rule US 2007 max - Nov Sun>=1 2:00 0 S
Zone X -5:00 US E%sT
Zone Y 9:00 - JST
Zone Z 0:00 - GMT 1971 Oct 31 2:00u
 0:00 GB GMT/BST
Rule GB 1972 1980 - Mar Sun>=16 2:00s 1:00 BST
Rule GB 1972 1980 - Oct Sun>=23 2:00s 0 GMT
";

    #[test]
    fn inferno_files() {
        let mut db = Db::default();
        db.parse(US).unwrap();
        let f = inferno(&db.spans("X").unwrap());
        // Inferno's own US_Eastern starts with the same periods.
        assert!(f.starts_with(
            "EST -18000 EDT -14400\n   9943200   25664400   41392800   57718800   73447200   89168400\n"
        ));
        assert_eq!(
            f.lines()
                .skip(1)
                .flat_map(str::split_ascii_whitespace)
                .count(),
            136
        );
        assert_eq!(inferno(&db.spans("Y").unwrap()), "JST 32400 JST 32400\n");
        let z = inferno(&db.spans("Z").unwrap());
        let (a, b) = (days(1972, 2, 19), days(1972, 9, 29));
        let want = format!(
            "GMT 0 BST 3600\n{:10} {:10}",
            a * 86_400 + 7200,
            b * 86_400 + 7200
        );
        assert!(z.starts_with(&want), "{z}");
    }

    /// `root/locale` is what `cargo xtask timezones` makes.
    #[test]
    fn locale_is_current() {
        let ws = workspace();
        let made = generate(&ws.join("tzdata")).unwrap();
        compare(&made, &ws.join("root/locale")).unwrap();
    }
}
