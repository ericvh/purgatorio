//! p [-number] [file ...]: copy the input to standard output a page
//! (default 22 lines) at a time, waiting at the end of each page for a
//! line from the console. That line may be `q` to quit or `!command` to
//! run a shell command first.

use std::io::{self, BufRead, BufReader, Write};

use purgatorio::{Context, Error};

struct Pager {
    page: usize,
    count: usize,
    cons: Option<BufReader<std::fs::File>>,
}

impl Pager {
    /// Waits for the user; returns false to quit.
    fn pause(&mut self) -> Result<bool, Error> {
        if self.cons.is_none() {
            let f = std::fs::File::open("/dev/cons").map_err(|_| Error::last())?;
            self.cons = Some(BufReader::new(f));
        }
        let cons = self.cons.as_mut().unwrap();
        loop {
            let mut l = String::new();
            if cons.read_line(&mut l)? == 0 {
                return Ok(false);
            }
            let l = l.trim_end_matches('\n');
            if l == "q" {
                return Ok(false);
            }
            let Some(cmd) = l.strip_prefix('!') else {
                return Ok(true);
            };
            let argv = vec![String::from("sh"), String::from("-c"), String::from(cmd)];
            let pid = purgatorio::spawn("/bin/sh.wasm", &argv, 0, &[])?;
            while purgatorio::wait()?.pid != pid {}
            println!("!");
        }
    }

    fn copy(&mut self, r: &mut dyn BufRead) -> Result<bool, Error> {
        let mut out = io::stdout().lock();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if r.read_until(b'\n', &mut buf)? == 0 {
                return Ok(true);
            }
            out.write_all(&buf)?;
            self.count += 1;
            if self.count == self.page {
                out.flush()?;
                self.count = 0;
                if !self.pause()? {
                    return Ok(false);
                }
            }
        }
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut page = 22;
    let mut files = &args[1.min(args.len())..];
    if let Some(n) = files.first().and_then(|a| a.strip_prefix('-')) {
        page = n
            .parse()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::from("usage: p [-number] [file...]"))?;
        files = &files[1..];
    }
    let mut p = Pager {
        page,
        count: 0,
        cons: None,
    };
    if files.is_empty() {
        p.copy(&mut io::stdin().lock())?;
    }
    for f in files {
        let file =
            std::fs::File::open(f).map_err(|_| Error(format!("{f}: {}", purgatorio::errstr())))?;
        if !p.copy(&mut BufReader::new(file))? {
            break;
        }
    }
    io::stdout().flush()?;
    Ok(())
}
