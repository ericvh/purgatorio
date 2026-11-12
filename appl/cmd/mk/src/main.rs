//! mk [-f mkfile] ... [option ...] [var=value ...] [target ...]: bring
//! targets up to date by the rules in the mkfiles (default `mkfile`), as
//! Inferno's mk(1).
//!
//! Supported: assignments (`U=` keeps a variable out of recipes), rules
//! and recipes, `<file` and `<|command` includes, `` `{command} ``,
//! `$var`, `${var}` and `${var:A%B=C%D}`, meta-rules with `%` and `&`,
//! regular-expression meta-rules (`R`, with `\n` in prerequisites and
//! `$stem1`...), the attributes `D E N n P Q R U V`, and the options
//! `-a -e -i -k -n -s -t -w` and `-d` (accepted, no output). Recipes run
//! with `sh -e -c`, so a recipe fails at its first failing command, after
//! assignments that give them the mk variables and `$target`, `$prereq`,
//! `$stem`, `$alltarget`, `$newprereq`, `$pid` and `$nproc`. Up to
//! `$NPROC` recipes (default 1) run at once; `$nproc` is the job's slot,
//! from 0. When a recipe fails mk waits for the others, then stops
//! (unless `-k`).
//!
//! A variable from the environment is read as a list, as sh writes it to
//! `/env` (its quoted words, `purgatorio::env::unquote_list`). Inferno's
//! mk splits an imported value at NULs, the Plan 9 rc form, which
//! Inferno's sh does not write; reading sh's own form keeps a list that sh
//! exports a list in mk.

mod parse;

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::time::{SystemTime, UNIX_EPOCH};

use purgatorio::abi::{mode, pctl};
use purgatorio::{Context, Dir, Error};
use regexp::Regex;

use parse::{Attrs, Mkfile, Rule, Var};

const SH: &str = "/bin/sh.wasm";

#[derive(Default)]
struct Opts {
    all: bool,
    explain: bool,
    intermediates: bool,
    keep_going: bool,
    dry: bool,
    touch: bool,
    what_if: HashSet<String>,
}

#[derive(Clone, Copy)]
enum State {
    Busy,
    /// A recipe making it runs as this prog.
    Running(u32),
    Done(u64),
    Failed,
}

/// What `make` found.
enum Step {
    /// Up to date, with this date stamp.
    Done(u64),
    /// A recipe it needs is running, or waits for a free job slot.
    Wait,
}

/// A running recipe.
struct Job {
    name: String,
    plan: Plan,
    slot: usize,
    newest: u64,
    /// The first line of the recipe as shown, for messages.
    first: String,
}

/// How to make one target.
#[derive(Clone)]
struct Plan {
    prereqs: Vec<String>,
    /// The rule whose recipe makes the target.
    recipe: Option<usize>,
    stem: String,
    /// `\0` to `\9` of a regular-expression meta-rule.
    stems: Vec<String>,
    attrs: Attrs,
    virt: bool,
    alltargets: Vec<String>,
}

struct Mk {
    file: Mkfile,
    /// Compiled targets of regular-expression meta-rules, by rule.
    regexps: HashMap<usize, Vec<Regex>>,
    opts: Opts,
    state: HashMap<String, State>,
    now: u64,
    errors: usize,
    /// Something was run, printed or touched for the current target.
    did: bool,
    /// Recipes that may run at once.
    nproc: usize,
    jobs: HashMap<u32, Job>,
    /// Exit statuses of children reaped while waiting for another.
    reaped: HashMap<u32, String>,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let usage = || {
        Error::from(
            "usage: mk [-f file] [-n] [-a] [-e] [-t] [-k] [-i] [-d[egp]] [-wtarget,...] [targets ...]",
        )
    };
    let mut opts = Opts::default();
    let mut files = Vec::new();
    let mut flags = Vec::new();
    let mut i = 1;
    while i < args.len() && args[i].starts_with('-') && args[i] != "-" {
        let a = &args[i];
        i += 1;
        if a == "--" {
            break;
        }
        flags.push(a.clone());
        for (k, c) in a[1..].char_indices() {
            let rest = &a[1 + k + c.len_utf8()..];
            match c {
                'a' => opts.all = true,
                'e' => opts.explain = true,
                'i' => opts.intermediates = true,
                'k' => opts.keep_going = true,
                'n' => opts.dry = true,
                's' => {}
                't' => opts.touch = true,
                'd' => break,
                'f' | 'w' => {
                    let v = if rest.is_empty() {
                        let v = args.get(i).cloned().ok_or_else(usage)?;
                        flags.push(v.clone());
                        i += 1;
                        v
                    } else {
                        String::from(rest)
                    };
                    if c == 'f' {
                        files.push(v);
                    } else {
                        opts.what_if
                            .extend(v.split(',').filter(|s| !s.is_empty()).map(String::from));
                    }
                    break;
                }
                _ => return Err(usage()),
            }
        }
    }
    let mut file = Mkfile::default();
    for (k, v) in std::env::vars() {
        if parse::is_name(&k) {
            let words = purgatorio::env::unquote_list(&v);
            file.vars.insert(
                k,
                Var {
                    words,
                    export: false,
                },
            );
        }
    }
    let mut targets = Vec::new();
    for a in &args[i..] {
        match a.split_once('=') {
            Some((name, value)) if parse::is_name(name) => {
                flags.push(a.clone());
                let words = value.split_whitespace().map(String::from).collect();
                file.set_override(name, words);
            }
            _ => targets.push(a.clone()),
        }
    }
    let var = |words: Vec<String>| Var {
        words,
        export: true,
    };
    file.vars.insert(String::from("MKFLAGS"), var(flags));
    file.vars
        .insert(String::from("MKARGS"), var(targets.clone()));
    if files.is_empty() {
        files.push(String::from("mkfile"));
    }
    for f in &files {
        let text =
            std::fs::read_to_string(f).map_err(|e| Error(format!("{f}: {}", Error::from(e))))?;
        file.read(&text, f, &mut backquote, &mut |name: &str| {
            std::fs::read_to_string(name).map_err(|e| format!("{name}: {}", Error::from(e)))
        })
        .map_err(Error)?;
    }
    let mut regexps = HashMap::new();
    for (n, r) in file.rules.iter().enumerate() {
        if r.attrs.regexp {
            let mut v = Vec::new();
            for t in &r.targets {
                let re = Regex::new(&format!("^({t})$")).map_err(|e| {
                    Error(format!(
                        "{}:{}: regular expression error; {}",
                        r.file, r.line, e.0
                    ))
                })?;
                v.push(re);
            }
            regexps.insert(n, v);
        }
    }
    if targets.is_empty() {
        match file.rules.iter().find(|r| !r.is_meta()) {
            Some(r) => targets = r.targets.clone(),
            None => {
                eprintln!("mk: nothing to mk");
                purgatorio::exits("");
            }
        }
    }
    let nproc = file
        .vars
        .get("NPROC")
        .and_then(|v| v.words.first())
        .and_then(|w| w.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    let mut mk = Mk {
        file,
        regexps,
        opts,
        state: HashMap::new(),
        now: now(),
        errors: 0,
        did: false,
        nproc,
        jobs: HashMap::new(),
        reaped: HashMap::new(),
    };
    for t in &targets {
        mk.did = false;
        loop {
            match mk.make(t, None) {
                Ok(Step::Wait) => mk.waitup(),
                Ok(Step::Done(_)) => {
                    if !mk.did {
                        println!("mk: '{t}' is up to date");
                    }
                    break;
                }
                Err(()) => {
                    mk.drain();
                    break;
                }
            }
        }
    }
    if mk.errors > 0 {
        purgatorio::exits("error");
    }
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn mtime(name: &str) -> Option<u64> {
    purgatorio::stat(name).ok().map(|d| u64::from(d.mtime))
}

/// Runs `cmd` with sh in a copy of mk's environment and returns its
/// standard output.
fn backquote(cmd: &str) -> Result<String, String> {
    let [r, w] = purgatorio::pipe().map_err(|e| e.0)?;
    let argv = [String::from("sh"), String::from("-c"), String::from(cmd)];
    purgatorio::spawn(SH, &argv, pctl::FORKENV, &[(w.as_raw_fd(), 1)]).map_err(|e| e.0)?;
    drop(w);
    let mut out = String::new();
    let _ = std::fs::File::from(r).read_to_string(&mut out);
    purgatorio::wait().map_err(|e| e.0)?;
    Ok(out)
}

/// Starts `script` with sh in a copy of mk's environment.
fn spawn_shell(script: &str, errexit: bool) -> Result<u32, Error> {
    let mut argv = vec![String::from("sh")];
    if errexit {
        argv.push(String::from("-e"));
    }
    argv.extend([String::from("-c"), String::from(script)]);
    purgatorio::spawn(SH, &argv, pctl::FORKENV, &[])
}

fn dedup(words: &mut Vec<String>) {
    let mut seen = HashSet::new();
    words.retain(|w| seen.insert(w.clone()));
}

impl Mk {
    fn rule(&self, n: usize) -> &Rule {
        &self.file.rules[n]
    }

    /// The stem (and subexpressions) if meta-rule `n` matches `name`.
    fn meta_match(&self, n: usize, name: &str) -> Option<(String, Vec<String>)> {
        let r = self.rule(n);
        if let Some(res) = self.regexps.get(&n) {
            for re in res {
                if let Some(caps) = re.find(name) {
                    // Group 1 is the whole target; the rule's own groups
                    // follow it.
                    let stems = caps
                        .iter()
                        .skip(1)
                        .map(|c| c.map_or(String::new(), |(a, b)| String::from(&name[a..b])))
                        .collect();
                    return Some((String::new(), stems));
                }
            }
            return None;
        }
        r.targets
            .iter()
            .find_map(|t| parse::match_meta(t, name))
            .map(|s| (s, Vec::new()))
    }

    /// The prerequisites of meta-rule `n` for a stem.
    fn meta_prereqs(&self, n: usize, stem: &str, stems: &[String]) -> Vec<String> {
        let r = self.rule(n);
        r.prereqs
            .iter()
            .map(|p| {
                if r.attrs.regexp {
                    let mut out = String::new();
                    let mut chars = p.chars().peekable();
                    while let Some(c) = chars.next() {
                        if c == '\\'
                            && let Some(d) = chars.peek().and_then(|d| d.to_digit(10))
                        {
                            chars.next();
                            out.push_str(stems.get(d as usize).map_or("", String::as_str));
                        } else {
                            out.push(c);
                        }
                    }
                    out
                } else {
                    parse::apply_stem(p, stem)
                }
            })
            .collect()
    }

    fn explicit(&self, name: &str) -> Vec<usize> {
        (0..self.file.rules.len())
            .filter(|&n| {
                let r = self.rule(n);
                !r.is_meta() && r.targets.iter().any(|t| t == name)
            })
            .collect()
    }

    /// Whether `name` exists or some rule could make it, using meta-rules
    /// not already in `used`.
    fn can_make(&self, name: &str, used: &mut Vec<usize>) -> bool {
        if self.state.contains_key(name) || mtime(name).is_some() || !self.explicit(name).is_empty()
        {
            return true;
        }
        if used.len() > 8 {
            return false;
        }
        for n in 0..self.file.rules.len() {
            let r = self.rule(n);
            if !r.is_meta() || r.recipe.is_none() || used.contains(&n) {
                continue;
            }
            if let Some((stem, stems)) = self.meta_match(n, name) {
                used.push(n);
                let ok = self
                    .meta_prereqs(n, &stem, &stems)
                    .iter()
                    .all(|p| self.can_make(p, used));
                used.pop();
                if ok {
                    return true;
                }
            }
        }
        false
    }

    /// How to make `name`, or `None` if no rule mentions it.
    fn resolve(&self, name: &str) -> Result<Option<Plan>, String> {
        let mut chosen: Vec<usize> = Vec::new();
        for n in self.explicit(name) {
            // A later rule with the same prerequisites replaces the earlier.
            chosen.retain(|&c| self.rule(c).prereqs != self.rule(n).prereqs);
            chosen.push(n);
        }
        let recipes: Vec<usize> = chosen
            .iter()
            .copied()
            .filter(|&n| self.rule(n).recipe.is_some())
            .collect();
        if recipes.len() > 1 {
            let mut msg = format!("ambiguous recipes for {name}:");
            for &n in &recipes {
                let r = self.rule(n);
                msg.push_str(&format!(
                    "\n\t{name} <-({}:{})- {}",
                    r.file,
                    r.line,
                    r.prereqs.join(" ")
                ));
            }
            return Err(msg);
        }
        let virt = chosen.iter().any(|&n| self.rule(n).attrs.virt);
        let mut prereqs: Vec<String> = chosen
            .iter()
            .flat_map(|&n| self.rule(n).prereqs.clone())
            .collect();
        if let Some(&n) = recipes.first() {
            dedup(&mut prereqs);
            let r = self.rule(n);
            return Ok(Some(Plan {
                prereqs,
                recipe: Some(n),
                stem: String::new(),
                stems: Vec::new(),
                attrs: r.attrs.clone(),
                virt,
                alltargets: r.targets.clone(),
            }));
        }
        if !virt {
            for n in 0..self.file.rules.len() {
                let r = self.rule(n);
                if !r.is_meta() || r.recipe.is_none() {
                    continue;
                }
                let Some((stem, stems)) = self.meta_match(n, name) else {
                    continue;
                };
                let extra = self.meta_prereqs(n, &stem, &stems);
                let mut used = vec![n];
                if !extra.iter().all(|p| self.can_make(p, &mut used)) {
                    continue;
                }
                prereqs.extend(extra);
                dedup(&mut prereqs);
                return Ok(Some(Plan {
                    prereqs,
                    recipe: Some(n),
                    stem,
                    stems,
                    attrs: r.attrs.clone(),
                    virt: r.attrs.virt,
                    alltargets: vec![String::from(name)],
                }));
            }
        }
        if chosen.is_empty() {
            return Ok(None);
        }
        dedup(&mut prereqs);
        let mut attrs = Attrs::default();
        for &n in &chosen {
            let a = &self.rule(n).attrs;
            attrs.touch |= a.touch;
            attrs.update |= a.update;
            if a.prog.is_some() {
                attrs.prog = a.prog.clone();
            }
        }
        Ok(Some(Plan {
            prereqs,
            recipe: None,
            stem: String::new(),
            stems: Vec::new(),
            attrs,
            virt,
            alltargets: vec![String::from(name)],
        }))
    }

    fn fail<T>(&mut self, name: &str) -> Result<T, ()> {
        self.state.insert(String::from(name), State::Failed);
        self.errors += 1;
        if !self.opts.keep_going {
            self.drain();
            purgatorio::exits("error");
        }
        Err(())
    }

    /// Waits for the child `pid` and returns its exit status, keeping the
    /// statuses of other children for later.
    fn wait_pid(&mut self, pid: u32) -> String {
        loop {
            if let Some(s) = self.reaped.remove(&pid) {
                return s;
            }
            match purgatorio::wait() {
                Ok(w) => {
                    self.reaped.insert(w.pid, w.status);
                }
                Err(e) => return e.0,
            }
        }
    }

    /// Runs `script` with sh and returns its exit status.
    fn shell(&mut self, script: &str) -> Result<String, Error> {
        let pid = spawn_shell(script, false)?;
        Ok(self.wait_pid(pid))
    }

    /// Waits for one running recipe to finish and records the result.
    fn waitup(&mut self) {
        let done = self
            .reaped
            .keys()
            .copied()
            .find(|p| self.jobs.contains_key(p));
        let (pid, status) = match done {
            Some(pid) => (pid, self.reaped.remove(&pid).unwrap_or_default()),
            None => match purgatorio::wait() {
                Ok(w) => (w.pid, w.status),
                Err(e) => {
                    eprintln!("mk: {e}");
                    purgatorio::exits("error");
                }
            },
        };
        let Some(job) = self.jobs.remove(&pid) else {
            self.reaped.insert(pid, status);
            return;
        };
        if !status.is_empty() {
            let mut msg = format!("mk: {}: exit status={status}", job.first);
            if job.plan.attrs.delete && !job.plan.virt {
                msg.push_str(&format!(", deleting '{}'", job.name));
                let _ = purgatorio::remove(&job.name);
            }
            eprintln!("{msg}");
        }
        for t in &job.plan.alltargets {
            if matches!(self.state.get(t), Some(State::Running(p)) if *p == pid) {
                self.state.remove(t);
            }
        }
        if status.is_empty() || job.plan.attrs.errok {
            self.finish(&job.name, &job.plan, job.newest);
        } else {
            let _ = self.fail::<()>(&job.name);
        }
    }

    /// Waits for every running recipe.
    fn drain(&mut self) {
        while !self.jobs.is_empty() {
            self.waitup();
        }
    }

    /// Records the date stamps of what a recipe made and returns `name`'s.
    fn finish(&mut self, name: &str, plan: &Plan, newest: u64) -> u64 {
        let stamp = |mk: &Mk, t: &str| {
            if plan.virt {
                newest
            } else if mk.opts.dry || plan.attrs.update {
                mk.now.max(newest)
            } else {
                mtime(t).unwrap_or(mk.now.max(newest))
            }
        };
        // One run of a recipe makes all the targets of its rule.
        let made = if plan.recipe.is_some() {
            plan.alltargets.clone()
        } else {
            vec![String::from(name)]
        };
        for t in &made {
            if t == name || !self.state.contains_key(t) {
                let s = stamp(self, t);
                self.state.insert(t.clone(), State::Done(s));
            }
        }
        stamp(self, name)
    }

    /// Brings `name` up to date and returns its date stamp, or starts the
    /// recipes it needs and returns `Step::Wait`: the caller waits for one
    /// to finish and asks again. `parent` is the time of the file that
    /// needs it, for missing intermediates.
    fn make(&mut self, name: &str, parent: Option<u64>) -> Result<Step, ()> {
        match self.state.get(name) {
            Some(State::Busy) => {
                eprintln!("mk: cycle in graph detected at target {name}");
                self.drain();
                purgatorio::exits("error");
            }
            Some(State::Running(_)) => return Ok(Step::Wait),
            Some(&State::Done(t)) => return Ok(Step::Done(t)),
            Some(State::Failed) => return Err(()),
            None => {}
        }
        let plan = match self.resolve(name) {
            Ok(Some(p)) => p,
            Ok(None) => {
                let t = if self.opts.what_if.contains(name) {
                    Some(self.now)
                } else {
                    mtime(name)
                };
                return match t {
                    Some(t) => {
                        self.state.insert(String::from(name), State::Done(t));
                        Ok(Step::Done(t))
                    }
                    None => {
                        eprintln!("mk: don't know how to make '{name}'");
                        self.fail(name)
                    }
                };
            }
            Err(e) => {
                eprintln!("mk: {e}");
                return self.fail(name);
            }
        };
        self.state.insert(String::from(name), State::Busy);
        let ttime = if plan.virt {
            None
        } else if self.opts.what_if.contains(name) {
            Some(self.now)
        } else {
            mtime(name)
        };
        let mut times = Vec::new();
        let (mut failed, mut waiting) = (false, false);
        for p in &plan.prereqs {
            match self.make(p, ttime) {
                Ok(Step::Done(t)) => times.push(t),
                Ok(Step::Wait) => {
                    waiting = true;
                    times.push(0);
                }
                Err(()) => {
                    failed = true;
                    times.push(0);
                }
            }
        }
        if waiting {
            self.state.remove(name);
            return Ok(Step::Wait);
        }
        if failed {
            return self.fail(name);
        }
        let newest = times.iter().copied().max().unwrap_or(0);
        // A missing intermediate whose prerequisites are no newer than the
        // file that needs it is left unmade.
        if !plan.virt
            && ttime.is_none()
            && !plan.prereqs.is_empty()
            && !self.opts.intermediates
            && !self.opts.all
            && parent.is_some_and(|pt| newest <= pt)
        {
            self.state.insert(String::from(name), State::Done(newest));
            return Ok(Step::Done(newest));
        }
        let tt = ttime.unwrap_or(0);
        let newer: Vec<String> = plan
            .prereqs
            .iter()
            .zip(&times)
            .filter(|&(_, &t)| ttime.is_none() || t > tt)
            .map(|(p, _)| p.clone())
            .collect();
        let outofdate = if self.opts.all || plan.virt || ttime.is_none() {
            true
        } else if let Some(prog) = &plan.attrs.prog {
            let mut any = false;
            for p in &plan.prereqs {
                let cmd = format!("{prog} {} {}", parse::quote(name), parse::quote(p));
                if !self
                    .shell(&cmd)
                    .map_err(|e| eprintln!("mk: {e}"))?
                    .is_empty()
                {
                    any = true;
                }
            }
            any
        } else {
            !newer.is_empty()
        };
        if !outofdate {
            let t = ttime.unwrap_or(newest);
            self.state.insert(String::from(name), State::Done(t));
            return Ok(Step::Done(t));
        }
        let runs = plan.recipe.is_some() && !self.opts.dry && !self.opts.touch;
        if runs && self.jobs.len() >= self.nproc {
            self.state.remove(name);
            return Ok(Step::Wait);
        }
        if self.opts.explain {
            if plan.prereqs.is_empty() {
                println!("{name} has no prerequisites");
            }
            for (p, &t) in plan.prereqs.iter().zip(&times) {
                if ttime.is_none() || t > tt {
                    println!("{name}({tt}) < {p}({t})");
                }
            }
        }
        if plan.recipe.is_some() {
            match self.recipe(name, &plan, &newer, newest) {
                Ok(true) => {}
                Ok(false) => return Ok(Step::Wait),
                Err(()) if plan.attrs.errok => {}
                Err(()) => return self.fail(name),
            }
        } else if plan.attrs.touch {
            self.touch(name);
        } else if !plan.virt && ttime.is_none() {
            eprintln!("mk: no recipe to make '{name}'");
            return self.fail(name);
        }
        Ok(Step::Done(self.finish(name, &plan, newest)))
    }

    /// Updates a file's modification time, creating it if need be.
    fn touch(&mut self, name: &str) {
        self.did = true;
        println!("touch({name})");
        if self.opts.dry {
            return;
        }
        let res = if mtime(name).is_some() {
            let mut d = Dir::null();
            d.mtime = now() as u32;
            purgatorio::wstat(name, &d)
        } else {
            purgatorio::create(name, mode::OWRITE, 0o666).map(drop)
        };
        if let Err(e) = res {
            eprintln!("mk: {name}: {e}");
        }
    }

    /// Starts (or prints or touches) the recipe that makes `name`: true
    /// if that is all, false if a job was started, an error if it could
    /// not be.
    fn recipe(
        &mut self,
        name: &str,
        plan: &Plan,
        newer: &[String],
        newest: u64,
    ) -> Result<bool, ()> {
        self.did = true;
        let Some(n) = plan.recipe else {
            return Ok(true);
        };
        let slot = (0..self.nproc)
            .find(|s| self.jobs.values().all(|j| j.slot != *s))
            .unwrap_or(0);
        let rule = self.rule(n).clone();
        let recipe = rule.recipe.clone().unwrap_or_default();
        let mut env: Vec<(String, Vec<String>)> = Vec::new();
        let mut names: Vec<&String> = self.file.vars.keys().collect();
        names.sort();
        for k in names {
            let v = &self.file.vars[k];
            if v.export {
                env.push((k.clone(), v.words.clone()));
            }
        }
        let one = |s: &str| vec![String::from(s)];
        env.push((String::from("target"), one(name)));
        env.push((String::from("alltarget"), plan.alltargets.clone()));
        env.push((String::from("prereq"), plan.prereqs.clone()));
        env.push((String::from("newprereq"), newer.to_vec()));
        env.push((String::from("stem"), one(&plan.stem)));
        for (k, s) in plan.stems.iter().enumerate().take(10) {
            env.push((format!("stem{k}"), one(s)));
        }
        env.push((String::from("pid"), one(&purgatorio::pid().to_string())));
        env.push((String::from("nproc"), one(&slot.to_string())));
        let shown = show(&recipe, &env);
        if self.opts.touch && !self.opts.dry {
            for t in &plan.alltargets {
                if !plan.virt {
                    self.touch(t);
                }
            }
            return Ok(true);
        }
        if !rule.attrs.quiet || self.opts.dry {
            print!("{shown}");
        }
        if self.opts.dry {
            return Ok(true);
        }
        let mut script = String::new();
        for (k, v) in &env {
            let words: Vec<String> = v.iter().map(|w| parse::quote(w)).collect();
            match words.len() {
                0 => script.push_str(&format!("{k}=()\n")),
                1 => script.push_str(&format!("{k}={}\n", words[0])),
                _ => script.push_str(&format!("{k}=({})\n", words.join(" "))),
            }
        }
        script.push_str(&recipe);
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let first = String::from(shown.lines().next().unwrap_or("").trim());
        let pid = match spawn_shell(&script, true) {
            Ok(pid) => pid,
            Err(e) => {
                eprintln!("mk: {first}: exit status={e}");
                return Err(());
            }
        };
        for t in &plan.alltargets {
            if t == name || !self.state.contains_key(t) {
                self.state.insert(t.clone(), State::Running(pid));
            }
        }
        self.jobs.insert(
            pid,
            Job {
                name: String::from(name),
                plan: plan.clone(),
                slot,
                newest,
                first,
            },
        );
        Ok(false)
    }
}

/// The recipe as printed: references to variables mk knows are replaced by
/// their values; others are left for sh.
fn show(recipe: &str, env: &[(String, Vec<String>)]) -> String {
    let lookup = |n: &str| {
        env.iter()
            .rev()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.join(" "))
    };
    let chars: Vec<char> = recipe.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let name_char = |c: char| c.is_alphanumeric() || c == '_';
    while i < chars.len() {
        if chars[i] == '$' {
            if chars.get(i + 1) == Some(&'{')
                && let Some(end) = chars[i..].iter().position(|&c| c == '}')
            {
                let name: String = chars[i + 2..i + end].iter().collect();
                if let Some(v) = lookup(&name) {
                    out.push_str(&v);
                    i += end + 1;
                    continue;
                }
            }
            let mut j = i + 1;
            while j < chars.len() && name_char(chars[j]) {
                j += 1;
            }
            let name: String = chars[i + 1..j].iter().collect();
            if j > i + 1
                && let Some(v) = lookup(&name)
            {
                out.push_str(&v);
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::show;

    #[test]
    fn shown_recipes() {
        let env = vec![
            (String::from("target"), vec![String::from("a.o")]),
            (
                String::from("prereq"),
                vec![String::from("a.c"), String::from("b.h")],
            ),
        ];
        assert_eq!(
            show("cc $prereq ${target} $other $\n", &env),
            "cc a.c b.h a.o $other $\n"
        );
    }
}
