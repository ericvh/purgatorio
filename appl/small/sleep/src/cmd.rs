//! sleep seconds, without `std`. Fractions such as `0.25` are allowed, as
//! in `appl/cmd/sleep`, but parsed here as decimal digits rather than
//! through `f64`, so exponents are not.

use small_rt::{STDERR, args, exits, sleep_ns, write};

/// `text` as nanoseconds: digits, then optionally a point and digits.
fn parse(text: &[u8]) -> Option<u64> {
    let mut parts = text.splitn(2, |&c| c == b'.');
    let (whole, frac) = (parts.next()?, parts.next().unwrap_or(b""));
    if whole.is_empty() && frac.is_empty() {
        return None;
    }
    let mut ns: u64 = 0;
    for &c in whole {
        ns = ns
            .checked_mul(10)?
            .checked_add(u64::from(c.checked_sub(b'0').filter(|d| *d < 10)?))?;
    }
    ns = ns.checked_mul(1_000_000_000)?;
    let mut unit = 100_000_000;
    for &c in frac {
        ns = ns.checked_add(unit * u64::from(c.checked_sub(b'0').filter(|d| *d < 10)?))?;
        unit /= 10;
    }
    Some(ns)
}

pub fn sleep() {
    let args = args();
    let ns = match (args.len(), args.get(1).and_then(parse)) {
        (2, Some(ns)) => ns,
        _ => {
            write(STDERR, b"usage: sleep seconds\n");
            exits("fail:usage")
        }
    };
    sleep_ns(ns);
}
