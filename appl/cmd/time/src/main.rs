//! time command [arg ...]: run a command and report on standard error its
//! load time, its real time and the total, in seconds, as
//! `0.01l 0.2r 0.21t`. A command that fails has its wait message printed
//! first. As in Inferno's time, nothing sets a status: not a usage
//! error, a command that cannot be loaded, nor the command's own failure.

use std::time::Instant;

use purgatorio::{Context, Error};

/// C's `%.4g`: four significant digits, trailing zeros dropped.
fn g4(x: f64) -> String {
    if x == 0.0 {
        return String::from("0");
    }
    let e = x.abs().log10().floor() as i32;
    if !(-4..4).contains(&e) {
        let s = format!("{x:.3e}");
        let (m, exp) = s.split_once('e').unwrap_or((&s, "0"));
        let m = if m.contains('.') {
            m.trim_end_matches('0').trim_end_matches('.')
        } else {
            m
        };
        let exp: i32 = exp.parse().unwrap_or(0);
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{m}e{sign}{:02}", exp.abs());
    }
    let s = format!("{x:.*}", (3 - e) as usize);
    if s.contains('.') {
        String::from(s.trim_end_matches('0').trim_end_matches('.'))
    } else {
        s
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 2 {
        eprintln!("usage: time cmd ...");
        return Ok(());
    }
    let argv = args[1..].to_vec();
    let t0 = Instant::now();
    let started =
        purgatorio::find_command(&argv[0]).and_then(|path| purgatorio::spawn(&path, &argv, 0, &[]));
    let pid = match started {
        Ok(pid) => pid,
        Err(e) => {
            eprintln!("time: {}: {e}", argv[0]);
            return Ok(());
        }
    };
    let t1 = Instant::now();
    loop {
        let w = purgatorio::wait()?;
        if !w.status.is_empty() {
            eprintln!("{} \"{}\":{}", w.pid, w.name, w.status);
        }
        if w.pid == pid {
            break;
        }
    }
    let t2 = Instant::now();
    let load = (t1 - t0).as_secs_f64();
    let real = (t2 - t1).as_secs_f64();
    eprintln!("{}l {}r {}t", g4(load), g4(real), g4(load + real));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::g4;

    #[test]
    fn g() {
        assert_eq!(g4(0.0), "0");
        assert_eq!(g4(0.012), "0.012");
        assert_eq!(g4(1.5), "1.5");
        assert_eq!(g4(12.34567), "12.35");
        assert_eq!(g4(12346.0), "1.235e+04");
        assert_eq!(g4(0.00001), "1e-05");
    }
}
