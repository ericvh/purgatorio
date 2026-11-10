//! File name patterns: `*` matches any sequence, `?` any one character and
//! `[class]` any character in the class, which may hold ranges `a-z`. `/`
//! must appear explicitly.

use std::os::fd::AsRawFd;

use purgatorio::abi::mode;

fn is_meta(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Whether `name` matches the pattern element `pat`.
pub fn matches(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    match_at(&p, &n)
}

fn match_at(p: &[char], n: &[char]) -> bool {
    let Some((&c, rest)) = p.split_first() else {
        return n.is_empty();
    };
    match c {
        '*' => (0..=n.len()).any(|i| match_at(rest, &n[i..])),
        '?' => !n.is_empty() && match_at(rest, &n[1..]),
        '[' => {
            let Some(end) = rest.iter().position(|&c| c == ']') else {
                return n.first() == Some(&'[') && match_at(rest, &n[1..]);
            };
            let Some(&ch) = n.first() else {
                return false;
            };
            let class = &rest[..end];
            let mut ok = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == '-' {
                    ok |= class[i] <= ch && ch <= class[i + 2];
                    i += 3;
                } else {
                    ok |= class[i] == ch;
                    i += 1;
                }
            }
            ok && match_at(&rest[end + 1..], &n[1..])
        }
        _ => n.first() == Some(&c) && match_at(rest, &n[1..]),
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
    let mut dirs = vec![String::from(if pat.starts_with('/') { "/" } else { "" })];
    let elems: Vec<&str> = pat.split('/').filter(|e| !e.is_empty()).collect();
    for (i, e) in elems.iter().enumerate() {
        let last = i + 1 == elems.len();
        let mut next = Vec::new();
        for d in &dirs {
            if is_meta(e) {
                for name in entries(d) {
                    if matches(e, &name) {
                        let p = join(d, &name);
                        if last || purgatorio::stat(&p).is_ok_and(|s| s.qid.is_dir()) {
                            next.push(p);
                        }
                    }
                }
            } else {
                let p = join(d, e);
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
        return vec![String::from(pat)];
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::matches;

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
    }
}
