//! File name patterns: `*` matches any sequence, `?` any one character and
//! `[class]` any character in the class, which may hold ranges `a-z`. `/`
//! must appear explicitly: a pattern is matched one path element at a time.
//!
//! There are two forms of each function. The plain one takes a string in
//! which every `*`, `?` and `[` is a metacharacter, as mash wants. The
//! `_marked` one takes [`Mark`]s, so a caller that has quoting (sh) can say
//! which characters are metacharacters; in that form a class that starts
//! with `^` matches the characters not in it, as sh(1) has it.

use std::os::fd::AsRawFd;

use purgatorio::abi::mode;

/// A pattern character, and whether it acts as a metacharacter if it is
/// `*`, `?` or `[`.
pub type Mark = (char, bool);

/// `pat` with every character able to be a metacharacter.
pub fn marks(pat: &str) -> Vec<Mark> {
    pat.chars().map(|c| (c, true)).collect()
}

fn text(p: &[Mark]) -> String {
    p.iter().map(|m| m.0).collect()
}

fn is_meta(p: &[Mark]) -> bool {
    p.iter().any(|&(c, on)| on && matches!(c, '*' | '?' | '['))
}

/// Whether `name` matches the pattern element `pat`.
pub fn matches(pat: &str, name: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    match_at(&marks(pat), &n, false)
}

/// Whether `name` matches the pattern element `pat`, with `[^class]`.
pub fn matches_marked(pat: &[Mark], name: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    match_at(pat, &n, true)
}

fn match_at(p: &[Mark], n: &[char], complement: bool) -> bool {
    let Some((&(c, on), rest)) = p.split_first() else {
        return n.is_empty();
    };
    match c {
        '*' if on => (0..=n.len()).any(|i| match_at(rest, &n[i..], complement)),
        '?' if on => !n.is_empty() && match_at(rest, &n[1..], complement),
        '[' if on => {
            let Some(end) = rest.iter().position(|m| m.0 == ']') else {
                return n.first() == Some(&'[') && match_at(rest, &n[1..], complement);
            };
            let Some(&ch) = n.first() else {
                return false;
            };
            let mut class = &rest[..end];
            let negate = complement && class.first().is_some_and(|m| m.0 == '^');
            if negate {
                class = &class[1..];
            }
            let mut ok = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1].0 == '-' {
                    ok |= class[i].0 <= ch && ch <= class[i + 2].0;
                    i += 3;
                } else {
                    ok |= class[i].0 == ch;
                    i += 1;
                }
            }
            ok != negate && match_at(&rest[end + 1..], &n[1..], complement)
        }
        _ => n.first() == Some(&c) && match_at(rest, &n[1..], complement),
    }
}

fn join(dir: &str, name: &str) -> String {
    match dir {
        "" => String::from(name),
        "/" => format!("/{name}"),
        _ => format!("{dir}/{name}"),
    }
}

fn entries(dir: &str) -> Vec<String> {
    let path = if dir.is_empty() { "." } else { dir };
    let Ok(fd) = purgatorio::open(path, mode::OREAD) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    while let Ok(batch) = purgatorio::dirread(fd.as_raw_fd()) {
        if batch.is_empty() {
            break;
        }
        names.extend(batch.into_iter().map(|d| d.name));
    }
    names.sort();
    names.dedup();
    names
}

/// The names `pat` matches, sorted, or `pat` itself if none do.
pub fn expand(pat: &str) -> Vec<String> {
    expand_with(&marks(pat), false)
}

/// The names `pat` matches, sorted, or its text if none do, with
/// `[^class]`.
pub fn expand_marked(pat: &[Mark]) -> Vec<String> {
    expand_with(pat, true)
}

fn expand_with(pat: &[Mark], complement: bool) -> Vec<String> {
    let absolute = pat.first().is_some_and(|m| m.0 == '/');
    let mut dirs = vec![String::from(if absolute { "/" } else { "" })];
    let elems: Vec<&[Mark]> = pat
        .split(|m| m.0 == '/')
        .filter(|e| !e.is_empty())
        .collect();
    for (i, e) in elems.iter().enumerate() {
        let last = i + 1 == elems.len();
        let mut next = Vec::new();
        for d in &dirs {
            if is_meta(e) {
                for name in entries(d) {
                    let n: Vec<char> = name.chars().collect();
                    if match_at(e, &n, complement) {
                        let p = join(d, &name);
                        if last || purgatorio::stat(&p).is_ok_and(|s| s.qid.is_dir()) {
                            next.push(p);
                        }
                    }
                }
            } else {
                let p = join(d, &text(e));
                if purgatorio::stat(&p).is_ok() {
                    next.push(p);
                }
            }
        }
        dirs = next;
        if dirs.is_empty() {
            break;
        }
    }
    if dirs.is_empty() || elems.is_empty() {
        return vec![text(pat)];
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        assert!(matches("*.c", "a.c"));
        assert!(matches("*", ""));
        assert!(!matches("*.c", "a.b"));
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(matches("[a-c]x", "bx"));
        assert!(!matches("[a-c]x", "dx"));
        assert!(matches("[xyz]", "y"));
        assert!(matches("*a*b", "xxaxxb"));
        assert!(matches("[x", "[x"));
    }

    #[test]
    fn complement_is_for_marked_patterns() {
        assert!(matches("[^a]", "^"));
        assert!(!matches("[^a]", "b"));
        assert!(matches_marked(&marks("[^a-c]x"), "dx"));
        assert!(!matches_marked(&marks("[^a-c]x"), "bx"));
        assert!(!matches_marked(&marks("[^a]"), ""));
    }

    #[test]
    fn unmarked_characters_are_literal() {
        let mut p = marks("a*?[b]");
        for m in &mut p[1..] {
            m.1 = false;
        }
        assert!(matches_marked(&p, "a*?[b]"));
        assert!(!matches_marked(&p, "axyb"));
        p[1].1 = true;
        assert!(matches_marked(&p, "axy?[b]"));
    }
}
