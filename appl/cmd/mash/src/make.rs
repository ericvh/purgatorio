//! The make builtins of mash-make(1): dependencies (`targets : prereqs`),
//! explicit rules (`targets : prereqs { commands }`), implicit rules
//! (`pattern :~ prereq { commands }`), and `make`, `depends`, `match` and
//! `rules`.
//!
//! A target is rebuilt when it does not exist or a prerequisite is newer
//! or was rebuilt. With no explicit rule, the shortest chain of implicit
//! rules that ends at an existing file or an explicit target is used. In
//! a pattern, `*` matches within one path element; `$1`, `$2` ... are the
//! text each `*` matched and `$0` the whole target.

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{Ctx, Recipe, Word, quote};
use crate::exec::{Flow, Io, List, Shell, Stop};

pub struct Depend {
    targets: List,
    deps: List,
    recipe: Option<Recipe>,
}

pub struct Rule {
    pattern: String,
    elems: Vec<String>,
    absolute: bool,
    dep: Word,
    recipe: Recipe,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Noexist,
    Exist,
    Stale,
    Made,
}

struct Node {
    state: State,
    mtime: u32,
    mark: bool,
}

#[derive(Default)]
pub struct Make {
    deps: Vec<Rc<Depend>>,
    by_target: HashMap<String, Vec<Rc<Depend>>>,
    rules: Vec<Rc<Rule>>,
    nodes: HashMap<String, Node>,
    level: usize,
}

/// One step of an implicit chain: the rule, its matches, and the target
/// it makes.
#[derive(Clone)]
struct Step {
    rule: Rc<Rule>,
    args: List,
    target: String,
}

fn elems(s: &str) -> Vec<String> {
    s.split('/')
        .filter(|e| !e.is_empty())
        .map(String::from)
        .collect()
}

/// The text each `*` in `p` matches in `s`, shortest first, if `p`
/// matches all of `s`.
fn star_caps(p: &[char], s: &[char]) -> Option<Vec<String>> {
    match p.split_first() {
        None => s.is_empty().then(Vec::new),
        Some(('*', rest)) => (0..=s.len()).find_map(|i| {
            let mut caps = star_caps(rest, &s[i..])?;
            caps.insert(0, s[..i].iter().collect());
            Some(caps)
        }),
        Some((c, rest)) => (s.first() == Some(c))
            .then(|| star_caps(rest, &s[1..]))
            .flatten(),
    }
}

impl Rule {
    /// `$0` (the target) and the text matched by each `*`, if the rule's
    /// pattern matches `target`.
    fn matches(&self, target: &str) -> Option<List> {
        let t = elems(target);
        if t.len() != self.elems.len() || target.starts_with('/') != self.absolute {
            return None;
        }
        let mut out = vec![String::from(target)];
        for (p, s) in self.elems.iter().zip(&t) {
            let p: Vec<char> = p.chars().collect();
            let s: Vec<char> = s.chars().collect();
            out.extend(star_caps(&p, &s)?);
        }
        Some(out)
    }
}

fn recipe_text(r: &Recipe) -> String {
    let mut s = String::new();
    r.write(&mut s, &Ctx { args: None });
    s
}

/// A path as make keys its files, without leading `./`.
fn key(s: &str) -> String {
    let mut s = s;
    while let Some(r) = s.strip_prefix("./") {
        s = r;
    }
    String::from(s)
}

impl Shell {
    /// Reports an error and returns make's -1.
    fn rep(&self, io: &Io, msg: &str) -> Flow<i32> {
        match self.report(io, msg) {
            Stop::Err => Ok(-1),
            s => Err(s),
        }
    }

    pub fn depend(
        &mut self,
        targets: &[Word],
        deps: &[Word],
        recipe: Option<&Recipe>,
        io: &Io,
    ) -> Flow {
        if !self.top {
            return Err(self.report(io, "dependency not at top level"));
        }
        let targets = self.words(targets, io)?;
        if targets.is_empty() {
            return Ok(());
        }
        let deps = self.words(deps, io)?;
        let d = Rc::new(Depend {
            targets,
            deps,
            recipe: recipe.cloned(),
        });
        for t in &d.targets {
            self.make
                .by_target
                .entry(t.clone())
                .or_default()
                .push(d.clone());
        }
        self.make.deps.push(d);
        Ok(())
    }

    pub fn rule(&mut self, pattern: &Word, dep: &Word, recipe: &Recipe, io: &Io) -> Flow {
        if !self.top {
            return Err(self.report(io, "rule not at top level"));
        }
        let (v, _) = self.word(pattern, io)?;
        let pattern = match v.as_slice() {
            [p] => p.clone(),
            [] => return Err(self.report(io, "rule pattern evaluates to nil")),
            _ => return Err(self.report(io, "rule pattern evaluates to a list")),
        };
        self.make.rules.push(Rc::new(Rule {
            elems: elems(&pattern),
            absolute: pattern.starts_with('/'),
            pattern,
            dep: dep.clone(),
            recipe: recipe.clone(),
        }));
        Ok(())
    }

    fn node(&mut self, name: &str) -> &mut Node {
        self.make
            .nodes
            .entry(key(name))
            .or_insert_with(|| match purgatorio::stat(name) {
                Ok(d) => Node {
                    state: State::Exist,
                    mtime: d.mtime,
                    mark: false,
                },
                Err(_) => Node {
                    state: State::Noexist,
                    mtime: 0,
                    mark: false,
                },
            })
    }

    fn rulematch(&self, target: &str) -> Vec<(Rc<Rule>, List)> {
        self.make
            .rules
            .iter()
            .filter_map(|r| Some((r.clone(), r.matches(target)?)))
            .collect()
    }

    /// Runs a rule's commands, echoing each and stopping at a failure.
    fn run_recipe(&mut self, r: &Recipe, args: Option<List>, io: &Io) -> Flow {
        let flags = self.flags;
        self.flags.echo = true;
        self.flags.raise = true;
        let margs = std::mem::replace(&mut self.margs, args);
        let res = self.run_body(&r.body, io);
        self.margs = margs;
        self.flags = flags;
        res
    }

    /// Makes `s`: 1 if it was made or is up to date, 0 if it exists and
    /// nothing says how to make it, -1 (reported) if it cannot be made.
    fn mk(&mut self, s: &str, io: &Io) -> Flow<i32> {
        let n = self.node(s);
        if n.state == State::Made {
            return Ok(1);
        }
        if n.mark {
            return self.rep(io, &format!("make: cycle in dependencies for target {s}"));
        }
        n.mark = true;
        if self.make.by_target.contains_key(s) {
            let x = self.explicit(s, io)?;
            if x != 0 {
                self.node(s).mark = false;
                return Ok(x);
            }
        }
        let x = self.implicit(s, io)?;
        let n = self.node(s);
        n.mark = false;
        Ok(match (x, n.state) {
            (0, State::Exist) => 0,
            (0, _) => -1,
            (x, _) => x,
        })
    }

    fn explicit(&mut self, t: &str, io: &Io) -> Flow<i32> {
        let ds = self.make.by_target.get(t).cloned().unwrap_or_default();
        let mut rule = None;
        for d in &ds {
            if d.recipe.is_some() {
                if rule.is_some() {
                    return self.rep(io, &format!("make: too many rules for {t}"));
                }
                rule = Some(d.clone());
            }
        }
        for d in &ds {
            for s in &d.deps {
                let x = self.mk(s, io)?;
                if x < 0 {
                    io.out.print(&format!("don't know how to make {s}\n"));
                    return Ok(x);
                }
                let m = self.node(s);
                let (state, mtime) = (m.state, m.mtime);
                let n = self.node(t);
                if state == State::Made || mtime > n.mtime {
                    n.state = State::Stale;
                }
            }
        }
        let Some(d) = rule else {
            return Ok(0);
        };
        if matches!(self.node(t).state, State::Noexist | State::Stale) {
            let recipe = d.recipe.clone().expect("a rule has a recipe");
            let args = self.margs.clone();
            self.run_recipe(&recipe, args, io)?;
            if d.targets.len() > 1 {
                for x in &d.targets {
                    self.node(x).state = State::Made;
                }
            }
            self.node(t).state = State::Made;
        }
        Ok(1)
    }

    /// Finds the shortest chain of implicit rules that makes `base` and
    /// runs it.
    fn implicit(&mut self, base: &str, io: &Io) -> Flow<i32> {
        let mut cand: Vec<(String, Vec<Step>)> = vec![(String::from(base), Vec::new())];
        loop {
            let mut win = Vec::new();
            let mut lose = Vec::new();
            for (b, chain) in &cand {
                for (rule, args) in self.rulematch(b) {
                    if chain.iter().any(|s| Rc::ptr_eq(&s.rule, &rule)) {
                        continue;
                    }
                    let margs = self.margs.replace(args.clone());
                    let v = self.word(&rule.dep, io);
                    self.margs = margs;
                    let v = match v {
                        Ok((v, _)) => v,
                        Err(Stop::Err) => return Ok(-1),
                        Err(s) => return Err(s),
                    };
                    let [v] = v.as_slice() else {
                        continue;
                    };
                    let mut y = chain.clone();
                    y.push(Step {
                        rule,
                        args,
                        target: b.clone(),
                    });
                    if self.node(v).state != State::Noexist || self.make.by_target.contains_key(v) {
                        win.push((v.clone(), y));
                    } else {
                        lose.push((v.clone(), y));
                    }
                }
            }
            if win.len() > 1 {
                let r = self.rep(io, &format!("{} rules match for {base}", win.len()));
                for (z, chain) in &win {
                    let mut line = z.clone();
                    for s in chain.iter().rev() {
                        line.push_str(&format!(" -> {}", s.target));
                    }
                    io.err.print(&format!("{line}\n"));
                }
                return r;
            }
            if let Some((z, chain)) = win.pop() {
                return self.run_chain(base, &z, &chain, io);
            }
            if lose.is_empty() {
                return Ok(0);
            }
            cand = lose;
        }
    }

    fn run_chain(&mut self, base: &str, z: &str, chain: &[Step], io: &Io) -> Flow<i32> {
        let inner = &chain[1..];
        for s in inner {
            let n = self.node(&s.target);
            if n.mark {
                return self.rep(
                    io,
                    &format!("make: cycle in dependencies for target {}", s.target),
                );
            }
            n.mark = true;
        }
        let margs = self.margs.take();
        let x = self.mk(z, io);
        self.margs = margs;
        let x = x?;
        if x < 0 {
            io.out.print(&format!("don't know how to make {z}\n"));
            return Ok(x);
        }
        let zn = self.node(z);
        let (zs, zm) = (zn.state, zn.mtime);
        let bn = self.node(base);
        if zs == State::Made || zm > bn.mtime || bn.state == State::Stale {
            for s in chain.iter().rev() {
                self.run_recipe(&s.rule.recipe, Some(s.args.clone()), io)?;
                let n = self.node(&s.target);
                n.state = State::Made;
                if s.target != base {
                    n.mark = false;
                }
            }
        } else {
            for s in inner {
                self.node(&s.target).mark = false;
            }
        }
        Ok(1)
    }

    /// `make`, `depends`, `match` and `rules`.
    pub fn make_cmd(&mut self, argv: &[String], io: &Io) -> Flow {
        let args = &argv[1..];
        match argv[0].as_str() {
            "make" => self.domake(args, io),
            "depends" => {
                if args.is_empty() {
                    for d in self.make.deps.clone() {
                        io.out.print(&prdep(&d));
                    }
                }
                for t in args {
                    io.out.print(&format!("{t}:\n"));
                    for d in self.make.by_target.get(t).cloned().unwrap_or_default() {
                        io.out.print(&prdep(&d));
                    }
                }
                Ok(())
            }
            "rules" => {
                if args.is_empty() {
                    for r in &self.make.rules {
                        io.out.print(&prrule(r));
                    }
                }
                for t in args {
                    io.out.print(&format!("{t}:\n"));
                    for (r, _) in self.rulematch(t) {
                        io.out.print(&prrule(&r));
                    }
                }
                Ok(())
            }
            "match" => {
                for t in args {
                    io.out.print(&format!("{t}:\n"));
                    for (r, a) in self.rulematch(t) {
                        io.out.print(&format!("\tlhs {}\n", r.pattern));
                        for (i, x) in a.iter().enumerate() {
                            io.out.print(&format!("\t${i} '{x}'\n"));
                        }
                        let margs = self.margs.replace(a);
                        let v = self.word(&r.dep, io);
                        self.margs = margs;
                        match v?.0.as_slice() {
                            [v] => io.out.print(&format!("\trhs '{v}'\n")),
                            l => io.out.print(&format!("\trhs list {}\n", l.len())),
                        }
                        io.out.print(&format!("\t{};\n", recipe_text(&r.recipe)));
                    }
                }
                Ok(())
            }
            _ => unreachable!("not a make builtin"),
        }
    }

    fn domake(&mut self, args: &[String], io: &Io) -> Flow {
        if !self.top {
            return Err(self.report(io, "make not at top level"));
        }
        if self.make.level == 0 {
            self.make.nodes.clear();
        }
        let default = [String::from("default")];
        let args = if args.is_empty() { &default[..] } else { args };
        self.make.level += 1;
        let r = self.domake1(args, io);
        self.make.level -= 1;
        r
    }

    fn domake1(&mut self, args: &[String], io: &Io) -> Flow {
        for s in args {
            if s.starts_with('-') {
                if s == "-clear" {
                    self.make.deps.clear();
                    self.make.by_target.clear();
                    self.make.rules.clear();
                } else {
                    let e = self.report(io, &format!("make: unknown option: {s}"));
                    if !matches!(e, Stop::Err) {
                        return Err(e);
                    }
                }
                continue;
            }
            if self.mk(s, io)? < 0 {
                io.out.print(&format!("don't know how to make {s}\n"));
                return Err(Stop::Fail(String::from("fail: make error")));
            }
        }
        Ok(())
    }
}

fn prdep(d: &Depend) -> String {
    let mut s = String::new();
    for t in &d.targets {
        s.push_str(&quote(t));
        s.push(' ');
    }
    s.push(':');
    for x in &d.deps {
        s.push(' ');
        s.push_str(&quote(x));
    }
    if let Some(r) = &d.recipe {
        s.push(' ');
        s.push_str(&recipe_text(r));
    }
    s.push_str(";\n");
    s
}

fn prrule(r: &Rule) -> String {
    format!("{} :~ {} {};\n", r.pattern, r.dep, recipe_text(&r.recipe))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(p: &str, s: &str) -> Option<Vec<String>> {
        let p: Vec<char> = p.chars().collect();
        let s: Vec<char> = s.chars().collect();
        star_caps(&p, &s)
    }

    fn rule(p: &str) -> Rule {
        Rule {
            pattern: String::from(p),
            elems: elems(p),
            absolute: p.starts_with('/'),
            dep: Word::lit("x", false),
            recipe: Recipe {
                list: false,
                body: Rc::new(Vec::new()),
            },
        }
    }

    #[test]
    fn stars() {
        assert_eq!(caps("*.dis", "rotta.dis").unwrap(), ["rotta"]);
        assert_eq!(caps("*.*", "a.b.c").unwrap(), ["a", "b.c"]);
        assert_eq!(caps("x*", "x").unwrap(), [""]);
        assert!(caps("*.dis", "rotta.b").is_none());
        assert_eq!(caps("abc", "abc").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn rule_matches() {
        let r = rule("/*/*.m");
        assert_eq!(
            r.matches("/module/rotta.m").unwrap(),
            ["/module/rotta.m", "module", "rotta"]
        );
        assert!(r.matches("module/rotta.m").is_none());
        assert!(r.matches("/a/b/c.m").is_none());
        assert_eq!(rule("*.dis").matches("x.dis").unwrap(), ["x.dis", "x"]);
        assert!(rule("*.dis").matches("d/x.dis").is_none());
    }
}
