//! The commands of the modules that run shell code or keep state: `arg`,
//! `getcsv`, `echo`, `mload`, `getsexprs` and `report`, and the dispatch
//! from a loaded module's command to its code.

use purgatorio::Error;

use crate::exec::{Flow, Io, Shell, Stop};
use crate::input::Input;
use crate::modules::{Loaded, Module};

/// A builtin's failure. With a `name` it is an Inferno exception: the
/// message is printed and the exception raised, which abandons the script
/// unless a loop builtin catches it. Without one the command just fails.
#[derive(Debug)]
pub struct Fail {
    pub name: Option<String>,
    pub msg: String,
}

impl Fail {
    pub fn raise(name: &str, msg: &str) -> Fail {
        Fail {
            name: Some(String::from(name)),
            msg: String::from(msg),
        }
    }

    pub fn plain(msg: &str) -> Fail {
        Fail {
            name: None,
            msg: String::from(msg),
        }
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail::plain(&e.0)
    }
}

/// Why a module command stopped: shell code it ran stopped, or it failed.
pub enum Ex {
    Stop(Stop),
    Fail(Fail),
}

impl From<Stop> for Ex {
    fn from(s: Stop) -> Ex {
        Ex::Stop(s)
    }
}

impl From<Fail> for Ex {
    fn from(f: Fail) -> Ex {
        Ex::Fail(f)
    }
}

/// Whether an argument is a `{command block}`.
pub fn is_block(w: &str) -> bool {
    w.starts_with('{')
}

/// `load test`: the fd that `report` writes to, if the test runs under
/// itest(1).
pub struct TestConf {
    mfd: Option<i32>,
}

/// Saved values of variables a builtin sets for the commands it runs.
pub struct Saved(Vec<(String, Option<Vec<String>>, bool)>);

/// Severities for `report`, in itslib's order.
const SEVERITIES: &[&str] = &["INF", "WRN", "ERR", "FTL"];
/// itslib's start-time message.
const S_STIME: usize = 4;

fn usage(form: &str) -> Fail {
    Fail::raise("usage", &format!("usage: {form}"))
}

/// `pctl flag... [fd...]`, from sh-std(1): changes this shell's prog as
/// `sys->pctl` does. The flags are `newfd`, `forkfd`, `newns`, `forkns`,
/// `newpgrp` and `nodevs`; the fds are the ones `newfd` keeps.
fn pctl(argv: &[String]) -> Result<String, Fail> {
    let (flags, keep) = pctl_args(argv)?;
    purgatorio::pctl(flags, &keep).map_err(|e| Fail::plain(&format!("pctl: {e}")))?;
    Ok(String::new())
}

/// The flags and fds of a `pctl` command.
fn pctl_args(argv: &[String]) -> Result<(u32, Vec<i32>), Fail> {
    use purgatorio::abi::pctl::*;
    if argv.len() < 2 {
        return Err(usage("pctl option... [fdnum...]"));
    }
    let (mut flags, mut keep) = (0, Vec::new());
    for w in &argv[1..] {
        flags |= match w.as_str() {
            "newfd" => NEWFD,
            "forkfd" => FORKFD,
            "newns" => NEWNS,
            "forkns" => FORKNS,
            "newpgrp" => NEWPGRP,
            "nodevs" => NODEVS,
            _ => match w.parse::<i32>() {
                Ok(fd) if fd >= 0 => {
                    keep.push(fd);
                    0
                }
                _ => return Err(Fail::raise("usage", &format!("pctl: unknown flag {w}"))),
            },
        };
    }
    Ok((flags, keep))
}

/// Limbo's `int s`: the leading decimal number, or 0.
fn int(s: &str) -> i64 {
    let (neg, d) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s),
    };
    let n = d.bytes().take_while(u8::is_ascii_digit).fold(0i64, |n, b| {
        n.saturating_mul(10).saturating_add((b - b'0') as i64)
    });
    if neg { -n } else { n }
}

impl Shell {
    pub(crate) fn save_vars(&self, names: &[&str]) -> Saved {
        Saved(
            names
                .iter()
                .map(|n| {
                    (
                        String::from(*n),
                        self.vars.get(*n).cloned(),
                        self.locals.contains(*n),
                    )
                })
                .collect(),
        )
    }

    pub(crate) fn restore_vars(&mut self, saved: Saved) {
        for (name, value, local) in saved.0 {
            match value {
                Some(v) => {
                    self.vars.insert(name.clone(), v);
                }
                None => {
                    self.vars.remove(&name);
                }
            }
            if !local {
                self.locals.remove(&name);
            }
        }
    }

    /// Prints a failure and turns it into the command's status, or into
    /// the exception it raises.
    fn report_fail(&mut self, f: Fail, io: &Io) -> Flow<String> {
        match f.name {
            Some(name) => {
                io.write(2, &format!("{}\n", f.msg));
                Err(Stop::Raise(name))
            }
            None => {
                io.write(2, &format!("sh: {}\n", f.msg));
                Ok(format!("fail:{}", f.msg))
            }
        }
    }

    /// Runs command `argv[0]` of module `m`.
    pub(crate) fn mod_cmd(&mut self, m: Module, argv: &[String], io: &Io) -> Flow<String> {
        let res: Result<String, Ex> = match m {
            Module::Std if argv[0] == "pctl" => pctl(argv).map_err(Ex::from),
            Module::Std => return self.std_cmd(argv),
            Module::Expr => crate::num::ntest(argv, false).map_err(Ex::from),
            Module::Mpexpr => crate::num::ntest(argv, true).map_err(Ex::from),
            Module::String => crate::string::cmd(argv).map_err(|e| Fail::from(e).into()),
            Module::Regex => crate::regex::match_cmd(argv).map_err(|e| Fail::from(e).into()),
            Module::Arg => self.arg(argv),
            Module::Csv => self.getlines(argv, io),
            Module::Sexprs if argv[0] != "islist" => self.getlines(argv, io),
            Module::Sexprs => crate::sexp::islist(argv).map_err(Ex::from),
            Module::Echo => Ok(echo(argv, io)),
            Module::File2chan if argv[0] == "file2chan" => self.file2chan(argv, io),
            Module::File2chan => self.reply_cmd(argv, io).map_err(Ex::from),
            Module::Mload => self.mload(argv, io).map_err(Ex::from),
            Module::Test => self.report(argv, io).map_err(Ex::from),
        };
        match res {
            Ok(s) => Ok(s),
            Err(Ex::Stop(s)) => Err(s),
            Err(Ex::Fail(f)) => self.report_fail(f, io),
        }
    }

    /// The loaded module in namespace `ns` with command (`cmd`) or `${}`
    /// builtin `name`.
    fn ns_lookup(&self, ns: &str, name: &str, cmd: bool) -> Result<Loaded, Fail> {
        let Some((_, mods)) = self.namespaces.iter().find(|(n, _)| n == ns) else {
            return Err(Fail::raise(
                "unknown",
                &format!("unknown namespace {}", crate::exec::quote(ns)),
            ));
        };
        mods.iter()
            .rev()
            .find(|l| {
                if cmd {
                    l.has_cmd(name)
                } else {
                    l.has_subst(name)
                }
            })
            .cloned()
            .ok_or_else(|| {
                Fail::raise(
                    "unknown cmd",
                    &format!("unknown command {}", crate::exec::quote(name)),
                )
            })
    }

    /// `ns cmd arg ...`: a command of a module in an `mload` namespace.
    pub(crate) fn ns_cmd(&mut self, argv: &[String], io: &Io) -> Flow<String> {
        if argv.len() < 2 {
            return self.report_fail(usage(&format!("{} command", argv[0])), io);
        }
        match self.ns_lookup(&argv[0], &argv[1], true) {
            Ok(Loaded::Builtin(m)) => self.mod_cmd(m, &argv[1..], io),
            Ok(Loaded::Reactor(..)) => Ok(String::new()),
            Err(f) => self.report_fail(f, io),
        }
    }

    /// `${ns name arg ...}`.
    pub(crate) fn ns_subst(&mut self, argv: &[String]) -> Result<Vec<String>, Fail> {
        if argv.len() < 2 {
            return Err(usage(&format!("{} command", argv[0])));
        }
        let l = self.ns_lookup(&argv[0], &argv[1], false)?;
        self.mod_subst(&l, &argv[1..])
    }

    /// `mload name [module ...]` and `munload name [module ...]`.
    fn mload(&mut self, argv: &[String], io: &Io) -> Result<String, Fail> {
        let cmd = argv[0].as_str();
        let Some(ns) = argv.get(1) else {
            return Err(usage(&format!("{cmd} name [module...]")));
        };
        for name in &argv[2..] {
            if crate::shmods::is_block(name) {
                return Err(usage(&format!("{cmd} namespace [module...]")));
            }
            if cmd == "mload" {
                let have = self
                    .namespaces
                    .iter()
                    .any(|(n, mods)| n == ns && mods.iter().any(|l| l.name() == name));
                if have {
                    continue;
                }
                let l = self.find_module(name)?;
                if l == Loaded::Builtin(Module::Test) {
                    self.test_init(io);
                }
                match self.namespaces.iter_mut().find(|(n, _)| n == ns) {
                    Some((_, mods)) => mods.push(l),
                    None => self.namespaces.push((ns.clone(), vec![l])),
                }
                continue;
            }
            let Some(i) = self.namespaces.iter().position(|(n, _)| n == ns) else {
                io.write(
                    2,
                    &format!("munload: no such namespace {}\n", crate::exec::quote(ns)),
                );
                continue;
            };
            let mods = &mut self.namespaces[i].1;
            let Some(j) = mods.iter().position(|l| l.name() == name) else {
                io.write(
                    2,
                    &format!("munload: no such module {}\n", crate::exec::quote(name)),
                );
                continue;
            };
            mods.remove(j);
            if mods.is_empty() {
                self.namespaces.remove(i);
            }
        }
        Ok(String::new())
    }

    /// `arg [opts {command}]... - args`, after sh-arg(1).
    fn arg(&mut self, argv: &[String]) -> Result<String, Ex> {
        let bad = || Fail::raise("usage", "usage: arg [opts {command}]... - args");
        let mut specs: Vec<(&str, &str)> = Vec::new();
        let mut i = 1;
        let mut dash = None;
        while i < argv.len() {
            if argv[i] == "-" {
                dash = Some(i);
                break;
            }
            if is_block(&argv[i]) || i + 1 == argv.len() || !is_block(&argv[i + 1]) {
                return Err(bad().into());
            }
            specs.push((&argv[i], &argv[i + 1]));
            i += 2;
        }
        let mut args: std::collections::VecDeque<String> = match dash {
            Some(d) => argv[d + 1..].iter().cloned().collect(),
            None => self.var("*").into(),
        };
        let saved = self.save_vars(&["opt", "arg"]);
        let mut status = String::new();
        // The rest of the current cluster of option letters.
        let mut cluster: Vec<char> = Vec::new();
        let res = loop {
            let opt = if cluster.is_empty() {
                let Some(next) = args.front() else {
                    break Ok(());
                };
                if next.chars().count() < 2 || !next.starts_with('-') {
                    break Ok(());
                }
                let next = args.pop_front().unwrap_or_default();
                if next == "--" {
                    break Ok(());
                }
                cluster = next.chars().skip(1).collect();
                cluster.remove(0)
            } else {
                cluster.remove(0)
            };
            let found = specs.iter().find_map(|(opts, cmd)| {
                let letters = opts.trim_end_matches('+');
                let n = opts.len() - letters.len();
                (!letters.is_empty() && letters.contains([opt, '*'])).then_some((n, *cmd))
            });
            let Some((n, cmd)) = found else {
                break Err(Fail::raise("usage", &format!("unknown option -{opt}")).into());
            };
            let mut vals = Vec::new();
            for _ in 0..n {
                if !cluster.is_empty() {
                    vals.push(cluster.drain(..).collect::<String>());
                } else if let Some(a) = args.pop_front() {
                    vals.push(a);
                } else {
                    break;
                }
            }
            if vals.len() < n {
                let msg = format!("option -{opt} requires {n} arguments");
                break Err(Fail::raise("usage", &msg).into());
            }
            self.set_var("opt", vec![opt.to_string()], true);
            self.set_var("arg", vals, true);
            match self.run_block(cmd) {
                Ok(()) => status = self.status.clone(),
                Err(Stop::Raise(e)) if e == "break" => {
                    self.restore_vars(saved);
                    return Ok(status);
                }
                Err(s) => break Err(Ex::Stop(s)),
            }
        };
        self.restore_vars(saved);
        res?;
        // Inferno's arg leaves the remaining arguments in $args as well.
        let rest: Vec<String> = args.into();
        self.set_var("args", rest.clone(), false);
        self.set_var("*", rest, false);
        Ok(status)
    }

    /// `getcsv {cmd}` and `getsexprs {cmd}`: runs `cmd` for each line of
    /// CSV, with `$line` set to its items, or for each S-expression, with
    /// `$sexp` set to its text. `break` and `continue` exceptions work as
    /// in a loop.
    fn getlines(&mut self, argv: &[String], io: &Io) -> Result<String, Ex> {
        let name = argv[0].as_str();
        let csv = name == "getcsv";
        if argv.len() != 2 || !is_block(&argv[1]) {
            return Err(usage(&format!("{name} {{cmd}}")).into());
        }
        let Some(fd) = io.get(0) else {
            let msg = format!("{name}: cannot open stdin: bad fd number");
            return Err(Fail::raise("bad input", &msg).into());
        };
        let mut inp = Input::fd(fd);
        let var = if csv { "line" } else { "sexp" };
        let saved = self.save_vars(&[var]);
        let mut status = String::new();
        let mut errors = 0;
        let res = loop {
            let value = if csv {
                match crate::csv::line(&mut inp) {
                    Some(items) => items,
                    None => break Ok(()),
                }
            } else {
                match crate::sexp::read(&mut inp) {
                    Ok(Some(e)) => {
                        errors = 0;
                        vec![e.text()]
                    }
                    Ok(None) => break Ok(()),
                    Err(e) => {
                        io.write(2, &format!("getsexprs: error on read: {e}\n"));
                        errors += 1;
                        if errors > 10 {
                            break Err(Ex::Stop(Stop::Raise(String::from("too many errors"))));
                        }
                        continue;
                    }
                }
            };
            self.set_var(var, value, true);
            match self.run_block(&argv[1]) {
                Ok(()) => status = self.status.clone(),
                Err(Stop::Raise(e)) if e == "break" => break Ok(()),
                Err(Stop::Raise(e)) if e == "continue" => {}
                Err(s) => break Err(Ex::Stop(s)),
            }
        };
        self.restore_vars(saved);
        res.map(|()| status)
    }

    /// Called when `load test` loads the module: finds itest's message fd
    /// in `$ITS_MFD` and verbosity in `$ITS_VERBOSITY`.
    pub(crate) fn test_init(&mut self, io: &Io) {
        let v = self.var("ITS_VERBOSITY");
        let m = self.var("ITS_MFD");
        let (Some(_), Some(m)) = (v.first(), m.first()) else {
            self.test = Some(TestConf { mfd: None });
            if !self.quiet {
                io.write(2, "Test is running standalone\n");
            }
            return;
        };
        let fd = i32::try_from(int(m)).unwrap_or(-1);
        let fd = io.get(fd).unwrap_or(fd);
        self.test = Some(TestConf { mfd: Some(fd) });
        if !self.quiet {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() % (1 << 31));
            self.its_report(S_STIME, 0, &ms.to_string());
        }
    }

    /// Sends one message to itest, if there is one.
    fn its_report(&mut self, sev: usize, verb: i64, msg: &str) {
        let Some(TestConf { mfd: Some(fd) }) = self.test else {
            return;
        };
        let mut io = Io::std();
        io.set(fd, fd);
        if io
            .try_write(fd, format!("{sev}{verb}{msg}\n").as_bytes())
            .is_err()
        {
            // The master test process was probably killed.
            self.test = Some(TestConf { mfd: None });
        }
    }

    /// `report severity verbosity message ...`.
    fn report(&mut self, argv: &[String], io: &Io) -> Result<String, Fail> {
        let bad = || usage("report INF|WRN|ERR|FTL verbosity message[...]");
        if argv.len() < 4 {
            return Err(bad());
        }
        let Some(sev) = SEVERITIES.iter().position(|s| *s == argv[1]) else {
            return Err(bad());
        };
        let msg = argv[3..].join(" ");
        if self.test.is_none() {
            io.write(2, &format!("[itslib missing] {} {msg}\n", argv[1]));
        }
        self.its_report(sev, int(&argv[2]), &msg);
        Ok(String::new())
    }
}

/// `echo [-n] [arg ...]`, as echo(1).
fn echo(argv: &[String], io: &Io) -> String {
    let mut words = &argv[1..];
    let mut newline = true;
    if let Some(w) = words.first()
        && (w == "-n" || w == "--")
    {
        newline = w != "-n";
        words = &words[1..];
    }
    let mut s = words.join(" ");
    if newline {
        s.push('\n');
    }
    match io.try_write(1, s.as_bytes()) {
        Ok(()) => String::new(),
        Err(e) => {
            io.write(2, &format!("echo: write error: {}\n", Error::from(e)));
            String::from("write error")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limbo_int() {
        assert_eq!(int("12abc"), 12);
        assert_eq!(int("-3"), -3);
        assert_eq!(int("x"), 0);
    }

    #[test]
    fn pctl_flags() {
        use purgatorio::abi::pctl::*;
        let v = |s: &[&str]| s.iter().map(|s| String::from(*s)).collect::<Vec<_>>();
        let (flags, keep) = pctl_args(&v(&["pctl", "newpgrp", "forkns", "newfd", "0", "2"]))
            .ok()
            .unwrap();
        assert_eq!(flags, NEWPGRP | FORKNS | NEWFD);
        assert_eq!(keep, [0, 2]);
        let bad = pctl_args(&v(&["pctl", "forkenv"])).err().unwrap();
        assert_eq!(bad.name.as_deref(), Some("usage"));
        assert_eq!(bad.msg, "pctl: unknown flag forkenv");
        assert!(pctl_args(&v(&["pctl"])).is_err());
    }
}
