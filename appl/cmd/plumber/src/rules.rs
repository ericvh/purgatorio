//! Plumbing rules (plumbing(6)): parsing, and matching a message against
//! them.

use plumbmsg::Msg;
use regexp::Regex;

pub struct Pattern {
    pub field: String,
    pub pred: String,
    pub arg: String,
    pub extra: Vec<String>,
    /// The line mentioned `$`, so `arg` is expanded before use.
    pub expand: bool,
    pub regex: Option<Regex>,
}

pub struct Rule {
    pub patterns: Vec<Pattern>,
    pub actions: Vec<Pattern>,
}

/// A message being routed, with what the rules found out about it.
pub struct Inmsg {
    pub msg: Msg,
    pub text: String,
    /// `$0` to `$9` from the last `matches`.
    pub matches: Vec<String>,
    pub file: String,
    pub dir: String,
    /// The destination port, once known.
    pub port: Option<String>,
    /// `plumb start`: the program and its arguments, unexpanded.
    pub startup: Option<(String, Vec<String>)>,
}

impl Inmsg {
    pub fn new(msg: Msg) -> Inmsg {
        let text = String::from_utf8_lossy(&msg.data).into_owned();
        Inmsg {
            msg,
            text,
            matches: Vec::new(),
            file: String::new(),
            dir: String::new(),
            port: None,
            startup: None,
        }
    }
}

/// Splits a rule line into words: white space separates them, `'` quotes
/// (with `''` for a quote) and `#` starts a comment.
fn words(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|&c| c == ' ' || c == '\t').is_some() {}
        match chars.peek() {
            None | Some('#') => return Ok(out),
            Some('\'') => {
                chars.next();
                let mut w = String::new();
                loop {
                    match chars.next() {
                        None => return Err(String::from("unterminated quote")),
                        Some('\'') => {
                            if chars.next_if_eq(&'\'').is_none() {
                                break;
                            }
                            w.push('\'');
                        }
                        Some(c) => w.push(c),
                    }
                }
                out.push(w);
            }
            Some(_) => {
                let mut w = String::new();
                while let Some(c) = chars.next_if(|&c| c != ' ' && c != '\t') {
                    w.push(c);
                }
                out.push(w);
            }
        }
    }
}

const ACTIONS: [&str; 3] = ["alwaysstart", "start", "to"];
const PREDICATES: [&str; 5] = ["is", "isdir", "isfile", "matches", "set"];
const FIELDS: [&str; 6] = ["src", "dst", "dir", "kind", "attr", "data"];

fn rule(pats: Vec<Pattern>) -> Result<Rule, String> {
    let mut patterns = Vec::new();
    let mut actions = Vec::new();
    let mut haveto = false;
    for mut p in pats {
        let mut extra_ok = false;
        if p.field == "plumb" {
            if !ACTIONS.contains(&p.pred.as_str()) {
                return Err(format!("illegal predicate {} in action", p.pred));
            }
            match p.pred.as_str() {
                "start" => extra_ok = true,
                _ => {
                    if p.arg.is_empty() {
                        return Err(format!("\"plumb {}\" must have non-empty target", p.pred));
                    }
                    haveto = true;
                }
            }
            actions.push(p);
        } else if FIELDS.contains(&p.field.as_str()) {
            if !actions.is_empty() {
                return Err(String::from("actions must follow patterns in rule"));
            }
            if !PREDICATES.contains(&p.pred.as_str()) {
                return Err(format!("illegal predicate {} in pattern", p.pred));
            }
            if p.pred == "matches" {
                p.regex = Some(
                    Regex::new(&p.arg)
                        .map_err(|_| format!("error in regular expression '{}'", p.arg))?,
                );
            }
            if !p.extra.is_empty() {
                return Err(format!("too many words in '{}' pattern", p.field));
            }
            patterns.push(p);
        } else {
            return Err(format!("don't recognize pattern field {}", p.field));
        }
        if !extra_ok && actions.last().is_some_and(|a| !a.extra.is_empty()) {
            return Err(String::from("too many words in 'plumb' pattern"));
        }
    }
    if !haveto {
        return Err(String::from("rule must have \"plumb to\" action"));
    }
    Ok(Rule { patterns, actions })
}

/// Parses the rules in `text`, read from `file`.
pub fn parse(file: &str, text: &str) -> Result<Vec<Rule>, String> {
    let mut rules = Vec::new();
    let mut pats: Vec<Pattern> = Vec::new();
    let mut lineno = 0;
    for line in text.lines() {
        lineno += 1;
        let w = words(line).map_err(|e| format!("{file}:{lineno}: {e}"))?;
        if w.is_empty() {
            // A comment line does not end a rule; a blank line does.
            if !pats.is_empty() && line.trim().is_empty() {
                rules.push(
                    rule(std::mem::take(&mut pats))
                        .map_err(|e| format!("{file}:{}: {e}", lineno - 1))?,
                );
            }
            continue;
        }
        if w.len() < 3 {
            return Err(format!(
                "{file}:{lineno}: syntax error: too few words on line"
            ));
        }
        let mut w = w.into_iter();
        pats.push(Pattern {
            field: w.next().unwrap_or_default(),
            pred: w.next().unwrap_or_default(),
            arg: w.next().unwrap_or_default(),
            extra: w.collect(),
            expand: line.contains('$'),
            regex: None,
        });
    }
    if !pats.is_empty() {
        rules.push(rule(pats).map_err(|e| format!("{file}:{lineno}: {e}"))?);
    }
    Ok(rules)
}

/// The ports the rules can send to, in order.
pub fn ports(rules: &[Rule]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in rules {
        for a in &r.actions {
            if (a.pred == "to" || a.pred == "alwaysstart") && !out.contains(&a.arg) {
                out.push(a.arg.clone());
            }
        }
    }
    out
}

/// Replaces `$0`...`$9`, `$file` and `$dir` in `s`.
pub fn expand(m: &Inmsg, s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(d) = after.chars().next().and_then(|c| c.to_digit(10)) {
            out.push_str(m.matches.get(d as usize).map_or("", String::as_str));
            rest = &after[1..];
        } else if let Some(r) = after.strip_prefix("file") {
            out.push_str(&m.file);
            rest = r;
        } else if let Some(r) = after.strip_prefix("dir") {
            out.push_str(&m.dir);
            rest = r;
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Removes `.` and `..` elements from an absolute or relative name.
fn cleanname(s: &str) -> String {
    let rooted = s.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for p in s.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            p => parts.push(p),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => String::from("."),
        (false, false) => joined,
    }
}

/// Whether a file of the kind asked for exists: `Some(true)` for a
/// directory.
pub type Stat<'a> = dyn Fn(&str) -> Option<bool> + 'a;

fn field(m: &Inmsg, f: &str) -> String {
    match f {
        "src" => m.msg.src.clone(),
        "dst" => m.msg.dst.clone(),
        "dir" => m.msg.dir.clone(),
        "kind" => m.msg.kind.clone(),
        "attr" => m.msg.attr.clone(),
        _ => m.text.clone(),
    }
}

fn matchpat(m: &mut Inmsg, p: &Pattern, stat: &Stat<'_>) -> bool {
    match p.pred.as_str() {
        "is" => {
            let arg = if p.expand {
                expand(m, &p.arg)
            } else {
                p.arg.clone()
            };
            field(m, &p.field) == arg
        }
        "isfile" | "isdir" => {
            let mut name = if p.expand {
                expand(m, &p.arg)
            } else {
                p.arg.clone()
            };
            if name.is_empty() {
                return false;
            }
            if !m.msg.dir.is_empty() && !name.starts_with(['/', '#']) {
                name = format!("{}/{name}", m.msg.dir);
            }
            let name = cleanname(&name);
            match (stat(&name), p.pred == "isdir") {
                (Some(false), false) => {
                    m.file = name;
                    true
                }
                (Some(true), true) => {
                    m.dir = name;
                    true
                }
                _ => false,
            }
        }
        "matches" => {
            let text = field(m, &p.field);
            let Some(re) = &p.regex else { return false };
            // The whole field must match.
            match re.find(&text) {
                Some(caps) if caps[0] == Some((0, text.len())) => {
                    m.matches = caps
                        .iter()
                        .map(|c| c.map_or(String::new(), |(a, b)| String::from(&text[a..b])))
                        .collect();
                    true
                }
                _ => false,
            }
        }
        "set" => {
            let v = if p.expand {
                expand(m, &p.arg)
            } else {
                p.arg.clone()
            };
            match p.field.as_str() {
                "src" => m.msg.src = v,
                "dst" => m.msg.dst = v,
                "dir" => m.msg.dir = v,
                "kind" => m.msg.kind = v,
                "attr" => m.msg.attr = v,
                _ => {
                    m.msg.data = v.clone().into_bytes();
                    m.msg.kind = String::from("text");
                    m.text = v;
                }
            }
            true
        }
        _ => false,
    }
}

/// Applies the first rule whose patterns all match; false if none does.
/// `log` receives the trace for `plumber -v`.
pub fn apply(rules: &[Rule], m: &mut Inmsg, stat: &Stat<'_>, log: &mut dyn FnMut(&str)) -> bool {
    for r in rules {
        // A failed rule leaves no trace of what it set.
        let saved = (m.msg.clone(), m.text.clone(), m.file.clone(), m.dir.clone());
        if r.patterns.iter().all(|p| matchpat(m, p, stat)) {
            log("yes");
            for a in &r.actions {
                match a.pred.as_str() {
                    "to" | "alwaysstart" => {
                        if m.port.is_none() {
                            m.port = Some(a.arg.clone());
                            m.msg.dst = a.arg.clone();
                        }
                    }
                    _ => m.startup = Some((a.arg.clone(), a.extra.clone())),
                }
            }
            return true;
        }
        log("no");
        (m.msg, m.text, m.file, m.dir) = saved;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = "# module files go to the editor
kind is text
data matches '([a-zA-Z0-9]+\\.m)(:[0-9]+)?'
data isfile     /module/$1
data set        /module/$0
plumb to edit
plumb start /bin/edit $file$2

# anything else that is a file
data matches '[a-z]+'
data isfile $0
plumb to show
";

    fn msg(data: &str) -> Inmsg {
        Inmsg::new(Msg {
            src: String::from("t"),
            kind: String::from("text"),
            dir: String::from("/usr/x"),
            data: data.as_bytes().to_vec(),
            ..Msg::default()
        })
    }

    fn stat(name: &str) -> Option<bool> {
        match name {
            "/module/sys.m" | "/usr/x/notes" => Some(false),
            "/usr" => Some(true),
            _ => None,
        }
    }

    #[test]
    fn parse_and_route() {
        let rules = parse("plumbing", RULES).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(ports(&rules), ["edit", "show"]);
        let mut m = msg("sys.m:12");
        assert!(apply(&rules, &mut m, &stat, &mut |_| {}));
        assert_eq!(m.port.as_deref(), Some("edit"));
        assert_eq!(m.text, "/module/sys.m:12");
        let (prog, args) = m.startup.clone().unwrap();
        assert_eq!(prog, "/bin/edit");
        assert_eq!(
            args.iter().map(|a| expand(&m, a)).collect::<Vec<_>>(),
            ["/module/sys.m:12"]
        );
        let mut m = msg("notes");
        assert!(apply(&rules, &mut m, &stat, &mut |_| {}));
        assert_eq!(
            (m.port.as_deref(), m.file.as_str()),
            (Some("show"), "/usr/x/notes")
        );
        let mut m = msg("nothing here");
        assert!(!apply(&rules, &mut m, &stat, &mut |_| {}));
        assert_eq!(m.text, "nothing here");
    }

    #[test]
    fn errors() {
        let e = |t| parse("f", t).err().unwrap();
        assert_eq!(e("data is x\n"), "f:1: rule must have \"plumb to\" action");
        assert_eq!(e("data\n"), "f:1: syntax error: too few words on line");
        assert_eq!(
            e("data has x\nplumb to y\n"),
            "f:2: illegal predicate has in pattern"
        );
        assert_eq!(
            e("plumb to y\ndata is x\n"),
            "f:2: actions must follow patterns in rule"
        );
        assert_eq!(cleanname("/a/./b/../c"), "/a/c");
    }
}
