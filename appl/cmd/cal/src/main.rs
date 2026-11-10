//! cal [month] [year]: print a calendar for a month, or with one number
//! above 12, for a whole year. A month is 1 to 12, a lower-case name, or
//! its first three letters; a year is 1 to 9999. The calendar is the
//! British one: Julian until September 1752, which loses eleven days, and
//! Gregorian after.

use std::io::{self, Write};

use purgatorio::{Context, Error};

const DAYW: &str = " S  M Tu  W Th  F  S";
const MONTHS: [&str; 12] = [
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

fn leap(y: i64) -> bool {
    if y <= 1752 {
        y % 4 == 0
    } else {
        (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
    }
}

/// Day of the week (0 is Sunday) of the first of month `m` in year `y`.
fn first_day(m: u32, y: i64) -> i64 {
    // The Julian day number of the first of the month, in whichever
    // calendar was in force then.
    let a = i64::from(14 - m) / 12;
    let yy = y + 4800 - a;
    let mm = i64::from(m) + 12 * a - 3;
    let mut jdn = 1 + (153 * mm + 2) / 5 + 365 * yy + yy / 4;
    if y > 1752 || (y == 1752 && m > 9) {
        jdn += -yy / 100 + yy / 400 - 32045;
    } else {
        jdn -= 32083;
    }
    (jdn + 1).rem_euclid(7)
}

fn month_len(m: u32, y: i64) -> u32 {
    match m {
        2 if leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The six week lines of a month.
fn month(m: u32, y: i64) -> [String; 6] {
    let mut lines: [String; 6] = Default::default();
    let mut d = first_day(m, y);
    let mut l = 0;
    let mut s = "   ".repeat(d as usize);
    let days: Vec<u32> = if y == 1752 && m == 9 {
        (1..=2).chain(14..=30).collect()
    } else {
        (1..=month_len(m, y)).collect()
    };
    for day in days {
        s.push_str(&format!("{day:2}"));
        d += 1;
        if d == 7 {
            d = 0;
            lines[l] = std::mem::take(&mut s);
            l += 1;
        } else {
            s.push(' ');
        }
    }
    if l < 6 {
        lines[l] = String::from(s.trim_end());
    }
    lines
}

fn bad() -> Error {
    Error::from("bad argument")
}

fn number(s: &str) -> Result<i64, Error> {
    let lower = s.to_lowercase();
    for (i, name) in MONTHS.iter().enumerate() {
        let name = name.to_lowercase();
        if lower == name || lower == name[..3] || (i == 8 && lower == "sept") {
            return Ok(i as i64 + 1);
        }
    }
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    s.parse().map_err(|_| bad())
}

fn short(m: i64, y: i64, out: &mut dyn Write) -> Result<(), Error> {
    if !(1..=12).contains(&m) || !(1..=9999).contains(&y) {
        return Err(bad());
    }
    writeln!(out, "   {} {y}", MONTHS[(m - 1) as usize])?;
    writeln!(out, "{DAYW}")?;
    for l in month(m as u32, y) {
        writeln!(out, "{l}")?;
    }
    Ok(())
}

fn long(y: i64, out: &mut dyn Write) -> Result<(), Error> {
    if !(1..=9999).contains(&y) {
        return Err(bad());
    }
    write!(out, "\n\n\n{:32}{y}\n\n", "")?;
    for q in (0..12).step_by(3) {
        writeln!(
            out,
            "         {:.3}                    {:.3}                    {:.3}",
            MONTHS[q],
            MONTHS[q + 1],
            MONTHS[q + 2]
        )?;
        writeln!(out, "{DAYW}   {DAYW}   {DAYW}")?;
        let ms: Vec<[String; 6]> = (0..3).map(|j| month((q + j + 1) as u32, y)).collect();
        for l in 0..6 {
            let s: String = ms.iter().map(|m| format!("{:<20.20}   ", m[l])).collect();
            writeln!(out, "{}", s.trim_end())?;
        }
    }
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let tm = purgatorio::daytime::gmt(purgatorio::daytime::now());
    let (curmon, curyr) = (i64::from(tm.mon) + 1, tm.year);
    let mut out = io::BufWriter::new(io::stdout().lock());
    match &args[1.min(args.len())..] {
        [] => short(curmon, curyr, &mut out)?,
        [a] => {
            let n = number(a)?;
            if (1..=12).contains(&n) {
                short(n, curyr, &mut out)?;
            } else {
                long(n, &mut out)?;
            }
        }
        [m, y] => short(number(m)?, number(y)?, &mut out)?,
        _ => return Err(Error::from("usage: cal [month] [year]")),
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekdays() {
        // 1 January 2000 was a Saturday, 1 November 2026 a Sunday, and
        // 1 September 1752 a Tuesday.
        assert_eq!(first_day(1, 2000), 6);
        assert_eq!(first_day(11, 2026), 0);
        assert_eq!(first_day(9, 1752), 2);
        assert_eq!(first_day(10, 1752), 0);
        assert_eq!(first_day(1, 1753), 1);
        assert_eq!(first_day(3, 1900), 4);
    }
}
