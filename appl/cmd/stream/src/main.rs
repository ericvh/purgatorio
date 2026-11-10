//! stream [-a] [-b bufsize] file1 [file2]: copy `file1` to standard output
//! with `sys->stream`, or cross-connect `file1` and `file2` with one copy in
//! each direction. Each copy runs in its own prog; `stream` waits for them
//! all unless `-a` is given. The default buffer is 8192 bytes.
//!
//! `stream --copy bufsize` is one copier: it streams fd 0 to fd 1.

use std::os::fd::AsRawFd;

use purgatorio::abi::{mode, pctl};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "stream [-a] [-b bufsize] file1 [file2]";

/// `Sys->ATOMICIO`.
const ATOMICIO: usize = 8192;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--copy") {
        let bufsiz = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(ATOMICIO);
        purgatorio::stream(0, 1, bufsiz)?;
        return Ok(());
    }
    let mut arg = Arg::new(args, USAGE);
    let (mut asynch, mut bufsiz) = (false, ATOMICIO);
    while let Some(c) = arg.opt() {
        match c {
            'a' => asynch = true,
            'b' => {
                bufsiz = arg
                    .earg()?
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or_else(|| arg.usage())?;
            }
            _ => return Err(arg.usage()),
        }
    }
    let files = arg.argv();
    let open = |name: &str, m: u8| {
        purgatorio::open(name, m).map_err(|e| Error(format!("cannot open {name}: {e}")))
    };
    let (f1, f2) = match files.as_slice() {
        [a] => (open(a, mode::OREAD)?, None),
        [a, b] => (open(a, mode::ORDWR)?, Some(open(b, mode::ORDWR)?)),
        _ => return Err(Error(format!("usage: {USAGE}"))),
    };
    let mut jobs = match &f2 {
        None => vec![(f1.as_raw_fd(), 1)],
        Some(f2) => vec![
            (f1.as_raw_fd(), f2.as_raw_fd()),
            (f2.as_raw_fd(), f1.as_raw_fd()),
        ],
    };
    // Without -a the last copy runs here, and the others in children.
    let inline = if asynch { None } else { jobs.pop() };
    let mut children = 0;
    for (src, dst) in jobs {
        purgatorio::spawn(
            "/bin/stream.wasm",
            &[
                String::from("stream"),
                String::from("--copy"),
                bufsiz.to_string(),
            ],
            pctl::NEWFD,
            &[(src, 0), (dst, 1), (2, 2)],
        )?;
        children += 1;
    }
    if let Some((src, dst)) = inline {
        purgatorio::stream(src, dst, bufsiz)?;
        for _ in 0..children {
            purgatorio::wait()?;
        }
    }
    Ok(())
}
