//! Reading mkfiles: lines, comments, includes, assignments and rules, and
//! the expansion of variables and `` `{command} `` in non-recipe text.

use std::collections::HashMap;

/// Rule attributes (the letters between a rule's two colons).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Attrs {
    /// `D`: delete the target if the recipe fails.
    pub delete: bool,
    /// `E`: carry on when the recipe fails.
    pub errok: bool,
    /// `N`: a rule without a recipe updates its target's time.
    pub touch: bool,
    /// `n`: a meta-rule that only matches files.
    pub files: bool,
    /// `P`: the program that decides whether a target is out of date.
    pub prog: Option<String>,
    /// `Q`: do not print the recipe.
    pub quiet: bool,
    /// `R`: the target is a regular expression.
    pub regexp: bool,
    /// `U`: the targets count as updated whatever the recipe did.
    pub update: bool,
    /// `V`: the targets are virtual.
    pub virt: bool,
}

/// One rule as written: `targets: prereqs` and its recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub targets: Vec<String>,
    pub prereqs: Vec<String>,
    pub attrs: Attrs,
    /// The recipe with the first character of each line removed; `None`
    /// when the rule has no recipe lines.
    pub recipe: Option<String>,
    pub file: String,
    pub line: usize,
}

impl Rule {
    /// Whether the rule is a meta-rule: a regular expression or a target
    /// with `%` or `&`.
    pub fn is_meta(&self) -> bool {
        self.attrs.regexp || self.targets.iter().any(|t| t.contains(['%', '&']))
    }
}

/// A variable's words and whether recipes see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Var {
    pub words: Vec<String>,
    pub export: bool,
}

/// Runs a `` `{command} `` or `<|command` and returns its output.
pub type Runner<'a> = dyn FnMut(&str) -> Result<String, String> + 'a;

/// Reads an included file.
pub type Loader<'a> = dyn FnMut(&str) -> Result<String, String> + 'a;

/// Everything read from the mkfiles.
#[derive(Default)]
pub struct Mkfile {
    pub vars: HashMap<String, Var>,
    pub rules: Vec<Rule>,
    /// Variables set on the command line: each overrides the first
    /// assignment to it in the mkfiles, but not later ones.
    pub overrides: HashMap<String, bool>,
}

impl Mkfile {
    /// Sets a variable from the command line.
    pub fn set_override(&mut self, name: &str, words: Vec<String>) {
        self.vars.insert(
            String::from(name),
            Var {
                words,
                export: true,
            },
        );
        self.overrides.insert(String::from(name), false);
    }

    fn assign(&mut self, name: &str, words: Vec<String>, export: bool) {
        if let Some(used) = self.overrides.get_mut(name)
            && !*used
        {
            *used = true;
            return;
        }
        self.vars.insert(String::from(name), Var { words, export });
    }

    /// Reads the text of `file`, adding its assignments and rules in order.
    pub fn read(
        &mut self,
        text: &str,
        file: &str,
        run: &mut Runner<'_>,
        load: &mut Loader<'_>,
    ) -> Result<(), String> {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut i = 0;
        // The rule whose recipe lines are being collected.
        let mut current: Option<usize> = None;
        while i < lines.len() {
            let lineno = i + 1;
            let first = lines[i];
            i += 1;
            if first.starts_with([' ', '\t']) {
                if let Some(r) = current {
                    let body = &first[1..];
                    let recipe = self.rules[r].recipe.get_or_insert_with(String::new);
                    recipe.push_str(body);
                    recipe.push('\n');
                    continue;
                }
                if first.trim().is_empty() {
                    continue;
                }
            }
            // Fold backslash-newline.
            let mut line = String::from(first);
            while line.ends_with('\\') && i < lines.len() {
                line.pop();
                line.push_str(lines[i]);
                i += 1;
            }
            let line = strip_comment(&line);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            current = None;
            let at = |msg: &str| format!("{file}:{lineno}: syntax error; {msg}");
            if let Some(cmd) = line.strip_prefix("<|") {
                let cmd = cmd.trim();
                if cmd.is_empty() {
                    return Err(at("missing include program name"));
                }
                let words = self.expand(cmd, run)?;
                let text = run(&words.join(" "))?;
                self.read(&text, file, run, load)?;
                continue;
            }
            if let Some(name) = line.strip_prefix('<') {
                let words = self.expand(name.trim(), run)?;
                let Some(name) = words.first() else {
                    return Err(at("missing include file name"));
                };
                let text = load(name)?;
                self.read(&text, name, run, load)?;
                continue;
            }
            match find_sep(line) {
                Some((pos, '=')) => {
                    let name = line[..pos].trim();
                    if !is_name(name) {
                        return Err(at("bad variable name"));
                    }
                    let mut value = line[pos + 1..].trim_start();
                    let mut export = true;
                    if let Some(v) = value.strip_prefix("U=") {
                        export = false;
                        value = v;
                    }
                    let words = self.expand(value, run)?;
                    self.assign(name, words, export);
                }
                Some((pos, _)) => {
                    let targets = self.expand(&line[..pos], run)?;
                    if targets.is_empty() {
                        return Err(at("no target"));
                    }
                    let (attrs, tail) = parse_attrs(&line[pos + 1..])
                        .map_err(|e| format!("{file}:{lineno}: {e}"))?;
                    let prereqs = self.expand(tail, run)?;
                    self.rules.push(Rule {
                        targets,
                        prereqs,
                        attrs,
                        recipe: None,
                        file: String::from(file),
                        line: lineno,
                    });
                    current = Some(self.rules.len() - 1);
                }
                None => return Err(at("expected : or =")),
            }
        }
        Ok(())
    }

    /// The words of `s` after quoting, variable references and
    /// `` `{command} `` substitution.
    pub fn expand(&self, s: &str, run: &mut Runner<'_>) -> Result<Vec<String>, String> {
        expand(s, &|n| self.vars.get(n).map(|v| v.words.clone()), run)
    }
}

/// Whether `s` is a variable name.
pub fn is_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_name_char)
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Removes an unquoted `#` and what follows it.
fn strip_comment(s: &str) -> String {
    let mut quoted = false;
    for (i, c) in s.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '#' if !quoted => return String::from(&s[..i]),
            _ => {}
        }
    }
    String::from(s)
}

/// The first unquoted `:` or `=` outside `${...}` and `` `{...} ``.
fn find_sep(s: &str) -> Option<(usize, char)> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut quoted = false;
    while i < b.len() {
        let c = b[i];
        if quoted {
            if c == b'\'' {
                quoted = false;
            }
        } else if c == b'\'' {
            quoted = true;
        } else if (c == b'$' || c == b'`') && b.get(i + 1) == Some(&b'{') {
            i = skip_braces(b, i + 1);
            continue;
        } else if c == b':' || c == b'=' {
            return Some((i, c as char));
        }
        i += 1;
    }
    None
}

/// The index after the `}` that closes the `{` at `open`.
fn skip_braces(b: &[u8], open: usize) -> usize {
    let mut depth = 0;
    let mut quoted = false;
    for (i, &c) in b.iter().enumerate().skip(open) {
        if quoted {
            quoted = c != b'\'';
            continue;
        }
        match c {
            b'\'' => quoted = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    b.len()
}

/// Splits the text after a rule's first colon into its attributes and its
/// prerequisites. Attributes are letters up to a second colon, directly
/// after the first.
fn parse_attrs(s: &str) -> Result<(Attrs, &str), String> {
    let mut a = Attrs::default();
    let mut any = false;
    for (i, c) in s.char_indices() {
        match c {
            'D' => a.delete = true,
            'E' => a.errok = true,
            'N' => a.touch = true,
            'n' => a.files = true,
            'Q' => a.quiet = true,
            'R' => a.regexp = true,
            'U' => a.update = true,
            'V' => a.virt = true,
            'P' => {
                let rest = &s[i + 1..];
                let Some(end) = rest.find(':') else {
                    return Ok((Attrs::default(), s));
                };
                if rest[..end].trim().is_empty() {
                    return Err(String::from("missing program name"));
                }
                a.prog = Some(String::from(&rest[..end]));
                return Ok((a, &rest[end + 1..]));
            }
            ':' if any => return Ok((a, &s[i + 1..])),
            _ => return Ok((Attrs::default(), s)),
        }
        any = true;
    }
    Ok((Attrs::default(), s))
}

/// Expands `s` into words. `var` gives a variable's words; `run` runs a
/// `` `{command} ``.
pub fn expand(
    s: &str,
    var: &dyn Fn(&str) -> Option<Vec<String>>,
    run: &mut Runner<'_>,
) -> Result<Vec<String>, String> {
    let mut words: Vec<String> = Vec::new();
    // The word being built, or None between words.
    let mut cur: Option<String> = None;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    // Adds `vals` to the text: the first joins the current word, and each
    // later one starts a new word.
    fn splice(words: &mut Vec<String>, cur: &mut Option<String>, vals: Vec<String>) {
        for (k, v) in vals.into_iter().enumerate() {
            if k > 0
                && let Some(w) = cur.take()
            {
                words.push(w);
            }
            cur.get_or_insert_with(String::new).push_str(&v);
        }
    }
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            if let Some(w) = cur.take() {
                words.push(w);
            }
            i += 1;
        } else if c == '\'' {
            let w = cur.get_or_insert_with(String::new);
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err(String::from("missing closing '")),
                    Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                        w.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(&c) => {
                        w.push(c);
                        i += 1;
                    }
                }
            }
        } else if c == '`' && chars.get(i + 1) == Some(&'{') {
            let end = close(&chars, i + 1)?;
            let cmd: String = chars[i + 2..end].iter().collect();
            let out = run(&cmd)?;
            let vals: Vec<String> = out.split_whitespace().map(String::from).collect();
            if !vals.is_empty() {
                splice(&mut words, &mut cur, vals);
            }
            i = end + 1;
        } else if c == '$' && chars.get(i + 1) == Some(&'{') {
            let end = close(&chars, i + 1)?;
            let inner: String = chars[i + 2..end].iter().collect();
            let vals = subst(&inner, var)?;
            if !vals.is_empty() {
                splice(&mut words, &mut cur, vals);
            }
            i = end + 1;
        } else if c == '$' && chars.get(i + 1).is_some_and(|&c| is_name_char(c)) {
            let mut j = i + 1;
            while j < chars.len() && is_name_char(chars[j]) {
                j += 1;
            }
            let name: String = chars[i + 1..j].iter().collect();
            let vals = var(&name).unwrap_or_default();
            if !vals.is_empty() {
                splice(&mut words, &mut cur, vals);
            }
            i = j;
        } else {
            cur.get_or_insert_with(String::new).push(c);
            i += 1;
        }
    }
    if let Some(w) = cur {
        words.push(w);
    }
    Ok(words)
}

/// The index of the `}` closing the `{` at `open`.
fn close(chars: &[char], open: usize) -> Result<usize, String> {
    let mut depth = 0;
    for (i, &c) in chars.iter().enumerate().skip(open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(i);
                }
            }
            _ => {}
        }
    }
    Err(String::from("missing closing }"))
}

/// `${name}` or `${name:A%B=C%D}`.
fn subst(inner: &str, var: &dyn Fn(&str) -> Option<Vec<String>>) -> Result<Vec<String>, String> {
    let Some((name, pat)) = inner.split_once(':') else {
        return Ok(var(inner.trim()).unwrap_or_default());
    };
    let words = var(name.trim()).unwrap_or_default();
    let Some((from, to)) = pat.split_once('=') else {
        return Err(format!("bad substitution ${{{inner}}}"));
    };
    // The pattern and replacement may refer to variables themselves.
    let mut no_cmds = |_: &str| Err(String::from("command in substitution"));
    let from = expand(from, var, &mut no_cmds)?.join(" ");
    let to = expand(to, var, &mut no_cmds)?.join(" ");
    Ok(words
        .into_iter()
        .map(|w| subst_word(&w, &from, &to))
        .collect())
}

/// Rewrites `w` by `A%B=C%D` if it matches `A%B`.
pub fn subst_word(w: &str, from: &str, to: &str) -> String {
    let Some((a, b)) = from.split_once('%') else {
        return if w == from {
            String::from(to)
        } else {
            String::from(w)
        };
    };
    if w.len() < a.len() + b.len() || !w.starts_with(a) || !w.ends_with(b) {
        return String::from(w);
    }
    let stem = &w[a.len()..w.len() - b.len()];
    to.replacen('%', stem, 1)
}

/// Matches `name` against a meta-rule target with `%` (any string) or `&`
/// (any string without `.` or `/`), returning the stem.
pub fn match_meta(pattern: &str, name: &str) -> Option<String> {
    let (pos, amp) = match (pattern.find('%'), pattern.find('&')) {
        (Some(p), _) => (p, false),
        (None, Some(p)) => (p, true),
        (None, None) => return (pattern == name).then(String::new),
    };
    let (a, b) = (&pattern[..pos], &pattern[pos + 1..]);
    if name.len() < a.len() + b.len() || !name.starts_with(a) || !name.ends_with(b) {
        return None;
    }
    let stem = &name[a.len()..name.len() - b.len()];
    if amp && stem.contains(['.', '/']) {
        return None;
    }
    Some(String::from(stem))
}

/// Puts the stem in place of each `%` and `&` of a meta-rule prerequisite.
pub fn apply_stem(prereq: &str, stem: &str) -> String {
    prereq.replace(['%', '&'], stem)
}

/// Quotes a word for sh.
pub fn quote(s: &str) -> String {
    if !s.is_empty()
        && !s
            .chars()
            .any(|c| c.is_whitespace() || "'#;&|{}()<>$`^=\"*?[".contains(c))
    {
        return String::from(s);
    }
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Result<String, String> {
        Err(String::from("no commands"))
    }

    fn read(text: &str) -> Mkfile {
        let mut m = Mkfile::default();
        m.read(text, "mkfile", &mut none, &mut |_| Err(String::from("no")))
            .unwrap();
        m
    }

    fn words(m: &Mkfile, s: &str) -> Vec<String> {
        m.expand(s, &mut none).unwrap()
    }

    #[test]
    fn assignments_and_expansion() {
        let m = read("O=dis\nNAMES=a b c # comment\nOBJ=${NAMES:%=%.$O}\nX='a b'c\n");
        assert_eq!(words(&m, "$O"), ["dis"]);
        assert_eq!(words(&m, "$NAMES"), ["a", "b", "c"]);
        assert_eq!(words(&m, "$OBJ"), ["a.dis", "b.dis", "c.dis"]);
        assert_eq!(words(&m, "$X"), ["a bc"]);
        assert_eq!(words(&m, "x$NAMES.y"), ["xa", "b", "c.y"]);
        assert_eq!(words(&m, "${NAMES:b=B}"), ["a", "B", "c"]);
        assert_eq!(words(&m, "$nosuch z"), ["z"]);
        assert_eq!(words(&m, "'it''s' $"), ["it's", "$"]);
        assert!(m.vars["O"].export);
        let m = read("Y=U=quiet\n");
        assert!(!m.vars["Y"].export);
        assert_eq!(m.vars["Y"].words, ["quiet"]);
    }

    #[test]
    fn rules_and_recipes() {
        let m = read(
            "all:V: prog\n\nprog: a.o \\\n  b.o\n\tcat $prereq > $target\n\t\techo done\n\n%.o:Q: %.c\n cp $stem.c $target\nx:Pcmp -s: y\n",
        );
        assert_eq!(m.rules.len(), 4);
        assert!(m.rules[0].attrs.virt);
        assert_eq!(m.rules[0].prereqs, ["prog"]);
        assert_eq!(m.rules[0].recipe, None);
        assert_eq!(m.rules[1].prereqs, ["a.o", "b.o"]);
        assert_eq!(
            m.rules[1].recipe.as_deref(),
            Some("cat $prereq > $target\n\techo done\n")
        );
        assert!(m.rules[2].is_meta() && m.rules[2].attrs.quiet);
        assert_eq!(m.rules[2].line, 8);
        assert_eq!(m.rules[3].attrs.prog.as_deref(), Some("cmp -s"));
        assert_eq!(m.rules[3].prereqs, ["y"]);
    }

    #[test]
    fn variables_bind_when_read() {
        let m = read("bar=a.b\nfoo: $bar\nbar=b.b\n");
        assert_eq!(m.rules[0].prereqs, ["a.b"]);
        assert_eq!(words(&m, "$bar"), ["b.b"]);
        // A colon inside ${...} is not the rule separator.
        let m = read("S=x.c\nt: ${S:%.c=%.o}\n");
        assert_eq!(m.rules[0].prereqs, ["x.o"]);
    }

    #[test]
    fn overrides_and_commands() {
        let mut m = Mkfile::default();
        m.set_override("X", vec![String::from("cmd")]);
        let mut run = |c: &str| Ok(format!("{c} out\n"));
        m.read(
            "X=file\nY=`{echo hi}\nX=later\nZ=$X\n",
            "mkfile",
            &mut run,
            &mut |_| Err(String::from("no")),
        )
        .unwrap();
        assert_eq!(m.vars["Y"].words, ["echo", "hi", "out"]);
        assert_eq!(m.vars["Z"].words, ["later"]);
        let mut m = Mkfile::default();
        m.read("<inc\nA=$B\n", "mkfile", &mut none, &mut |f| {
            Ok(format!("B=from-{f}\n"))
        })
        .unwrap();
        assert_eq!(m.vars["A"].words, ["from-inc"]);
    }

    #[test]
    fn meta_matching() {
        assert_eq!(match_meta("%.o", "a/b.o").as_deref(), Some("a/b"));
        assert_eq!(match_meta("&.o", "a/b.o"), None);
        assert_eq!(match_meta("&.o", "ab.o").as_deref(), Some("ab"));
        assert_eq!(match_meta("x%y", "xy").as_deref(), Some(""));
        assert_eq!(match_meta("%.o", "a.c"), None);
        assert_eq!(apply_stem("%.c", "a"), "a.c");
        assert_eq!(subst_word("a.c", "%.c", "%.o"), "a.o");
        assert_eq!(subst_word("a.h", "%.c", "%.o"), "a.h");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("plain.c"), "plain.c");
    }

    #[test]
    fn errors() {
        let mut m = Mkfile::default();
        let e = m
            .read("just words\n", "mkfile", &mut none, &mut |_| {
                Err(String::from("no"))
            })
            .unwrap_err();
        assert_eq!(e, "mkfile:1: syntax error; expected : or =");
    }
}
