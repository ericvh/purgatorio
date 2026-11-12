//! mdb pid: debug a traced prog, as Inferno's `mdb` debugs a Dis module,
//! with commands read a line at a time from standard input.
//!
//! `mdb` stops the prog (`stop` on its `ctl`, `docs/debug.md`), which
//! takes effect at the prog's next host call or VM check point, and waits
//! until it has stopped: a prog blocked in the system stops when its call
//! returns. While the prog is stopped its memory can be read and written
//! through `/prog/<pid>/heap`. Breakpoints are on entry to a function,
//! named or given by index; the prog stops when a traced call enters
//! it.
//!
//! The commands:
//!
//! - `t`: the stack, innermost first, by name;
//! - `x addr [n]`: `n` bytes (default 64) of linear memory at `addr`, in
//!   hex, 16 a line;
//! - `w addr byte ...`: write bytes, given in hex, at `addr`;
//! - `b func`: a breakpoint on entry to `func`; `b` alone lists them;
//! - `d func`: delete a breakpoint;
//! - `c`: continue until a breakpoint;
//! - `s`: step: continue until the prog enters a function;
//! - `k`: kill the prog, and quit;
//! - `q`, or the end of the input: delete the breakpoints, let the prog
//!   run on, and quit.
//!
//! Addresses and counts are decimal, or hex after `0x`. After each stop
//! `mdb` prints `stopped in <func>`; when the prog ends instead it prints
//! `exited`, or `broken` and the trap, and quits. Only progs started
//! traced (`trace`) can be debugged: the others are `prog is not
//! traced`. Values of locals need DWARF, which the guests do not carry.

use std::collections::BTreeSet;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::time::Duration;

use progdebug::{ctl, func_name, module_names, prog_file};
use purgatorio::{Context, Error};
use wasmobj::Names;

const USAGE: &str = "mdb pid";

/// How often `mdb` looks to see whether the prog has stopped.
const POLL: Duration = Duration::from_millis(10);

/// How a wait for the prog ended.
enum State {
    Stopped,
    Exited,
    Broken(String),
}

struct Mdb {
    pid: u32,
    names: Names,
    breaks: BTreeSet<u32>,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let [_, pid] = args.as_slice() else {
        return Err(Error(format!("usage: {USAGE}")));
    };
    let pid: u32 = pid.parse().map_err(|_| Error(format!("usage: {USAGE}")))?;
    let path = module_path(pid)?;
    let mut m = Mdb {
        pid,
        names: module_names(&path),
        breaks: BTreeSet::new(),
    };
    ctl(pid, "stop").map_err(|e| Error(format!("{pid}: {e}")))?;
    if !m.report(m.wait()?) {
        return Ok(());
    }
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let words: Vec<&str> = line.split_ascii_whitespace().collect();
        let Some((&cmd, rest)) = words.split_first() else {
            continue;
        };
        match m.command(cmd, rest) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => eprintln!("mdb: {e}"),
        }
        let _ = std::io::stdout().flush();
    }
    m.detach()
}

/// The prog's command module: the first `module` line of its `module`
/// file.
fn module_path(pid: u32) -> Result<String, Error> {
    let text = prog_file(pid, "module").map_err(|e| Error(format!("{pid}: {e}")))?;
    for line in text.lines() {
        let fields = purgatorio::unquote_fields(line);
        if let [kind, _, _, path] = fields.as_slice()
            && kind == "module"
        {
            return Ok(path.clone());
        }
    }
    Err(Error(format!("{pid}: no module")))
}

/// A decimal number, or hex after `0x`.
fn number(s: &str) -> Result<u64, Error> {
    let r = match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16),
        None => s.parse(),
    };
    r.map_err(|_| Error(format!("bad number {s}")))
}

impl Mdb {
    /// Runs one command; `false` when `mdb` is done.
    fn command(&mut self, cmd: &str, args: &[&str]) -> Result<bool, Error> {
        match (cmd, args) {
            ("t", []) => self.stack()?,
            ("x", [addr, rest @ ..]) if rest.len() <= 1 => {
                let n = rest.first().map(|n| number(n)).transpose()?.unwrap_or(64);
                self.examine(number(addr)?, n as usize)?;
            }
            ("w", [addr, bytes @ ..]) if !bytes.is_empty() => {
                let data = bytes
                    .iter()
                    .map(|b| u8::from_str_radix(b, 16).map_err(|_| Error(format!("bad byte {b}"))))
                    .collect::<Result<Vec<u8>, Error>>()?;
                self.write(number(addr)?, &data)?;
            }
            ("b", []) => {
                for &f in &self.breaks {
                    println!("breakpoint {}", func_name(&self.names, f));
                }
            }
            ("b", [func]) => {
                let f = self.func(func)?;
                ctl(self.pid, &format!("bpt set {f}"))?;
                self.breaks.insert(f);
            }
            ("d", [func]) => {
                let f = self.func(func)?;
                ctl(self.pid, &format!("bpt del {f}"))?;
                self.breaks.remove(&f);
            }
            ("c", []) => return self.go("start"),
            ("s", []) => return self.go("step"),
            ("k", []) => {
                ctl(self.pid, "kill")?;
                return Ok(false);
            }
            ("q", []) => {
                self.detach()?;
                return Ok(false);
            }
            ("h" | "?", []) => help(),
            _ => return Err(Error(format!("bad command: {cmd}; h for help"))),
        }
        Ok(true)
    }

    /// Resumes the prog with `msg` and waits for it to stop again.
    fn go(&mut self, msg: &str) -> Result<bool, Error> {
        ctl(self.pid, msg)?;
        Ok(self.report(self.wait()?))
    }

    /// Waits until the prog has stopped or gone.
    fn wait(&self) -> Result<State, Error> {
        loop {
            let Ok(status) = prog_file(self.pid, "status") else {
                return Ok(State::Exited);
            };
            match status.split(' ').nth(4) {
                Some("debug") => return Ok(State::Stopped),
                Some("broken") => {
                    let e = prog_file(self.pid, "exception").unwrap_or_default();
                    return Ok(State::Broken(String::from(e.trim_end())));
                }
                _ => std::thread::sleep(POLL),
            }
        }
    }

    /// Prints how a wait ended; `false` when the prog has gone.
    fn report(&self, state: State) -> bool {
        match state {
            State::Stopped => {
                let at = self.frames().ok().and_then(|f| f.first().copied());
                match at {
                    Some(f) => println!("stopped in {}", func_name(&self.names, f)),
                    None => println!("stopped"),
                }
                true
            }
            State::Exited => {
                println!("exited");
                false
            }
            State::Broken(e) => {
                println!("broken: {e}");
                false
            }
        }
    }

    /// The function indices of the prog's stack, innermost first.
    fn frames(&self) -> Result<Vec<u32>, Error> {
        let text = prog_file(self.pid, "stack")?;
        Ok(text
            .lines()
            .filter_map(|l| l.split(' ').next()?.parse().ok())
            .collect())
    }

    fn stack(&self) -> Result<(), Error> {
        let text = prog_file(self.pid, "stack")?;
        for line in text.lines() {
            let fields = purgatorio::unquote_fields(line);
            match fields.first().map(|f| (f.as_str(), f.parse::<u32>())) {
                Some(("...", _)) => println!("... {} more", fields.get(1).map_or("", |s| s)),
                Some((_, Ok(f))) => println!("{}", func_name(&self.names, f)),
                _ => {}
            }
        }
        Ok(())
    }

    /// Function `s`, by name or index.
    fn func(&self, s: &str) -> Result<u32, Error> {
        if let Ok(f) = s.parse() {
            return Ok(f);
        }
        let f = s.strip_prefix("func ").and_then(|f| f.parse().ok());
        f.or_else(|| self.names.iter().find(|(_, n)| *n == s).map(|(i, _)| i))
            .ok_or_else(|| Error(format!("no function {s}")))
    }

    fn heap(&self, write: bool) -> Result<std::fs::File, Error> {
        std::fs::File::options()
            .read(!write)
            .write(write)
            .open(format!("/prog/{}/heap", self.pid))
            .map_err(|_| Error::last())
    }

    fn examine(&self, addr: u64, n: usize) -> Result<(), Error> {
        let mut f = self.heap(false)?;
        f.seek(SeekFrom::Start(addr)).map_err(|_| Error::last())?;
        let mut data = vec![0u8; n];
        let mut got = 0;
        while got < n {
            match f.read(&mut data[got..]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(_) => return Err(Error::last()),
            }
        }
        for (i, row) in data[..got].chunks(16).enumerate() {
            let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
            println!("{:08x} {}", addr + 16 * i as u64, hex.join(" "));
        }
        Ok(())
    }

    fn write(&self, addr: u64, data: &[u8]) -> Result<(), Error> {
        let mut f = self.heap(true)?;
        f.seek(SeekFrom::Start(addr)).map_err(|_| Error::last())?;
        f.write_all(data).map_err(|_| Error::last())
    }

    /// Deletes the breakpoints and lets the prog run on.
    fn detach(&mut self) -> Result<(), Error> {
        for f in std::mem::take(&mut self.breaks) {
            let _ = ctl(self.pid, &format!("bpt del {f}"));
        }
        // A prog that has gone needs nothing.
        let _ = ctl(self.pid, "start");
        Ok(())
    }
}

fn help() {
    println!(
        "t  stack\n\
         x addr [n]  examine memory\n\
         w addr byte ...  write memory\n\
         b [func]  set or list breakpoints\n\
         d func  delete a breakpoint\n\
         c  continue\n\
         s  step to the next function\n\
         k  kill\n\
         q  quit"
    );
}
