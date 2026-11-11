//! Lethe paths: absolute, with no empty, `.` or `..` element, no NUL and
//! no trailing `/` except the root. The client resolves `..` itself.

use alloc::string::String;

/// The longest path.
pub const MAX_PATH: usize = 1024;
/// The longest element.
pub const MAX_ELEM: usize = 255;

/// Whether `p` is a valid Lethe path.
pub fn valid(p: &str) -> bool {
    if p == "/" {
        return true;
    }
    if p.len() > MAX_PATH || !p.starts_with('/') || p.contains('\0') {
        return false;
    }
    p[1..]
        .split('/')
        .all(|e| !e.is_empty() && e != "." && e != ".." && e.len() <= MAX_ELEM)
}

/// Whether `name` can be one element of a path.
pub fn valid_elem(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= MAX_ELEM
        && !name.contains(['/', '\0'])
}

/// Whether `p` is `prefix` or below it. Both must be valid.
pub fn within(p: &str, prefix: &str) -> bool {
    prefix == "/"
        || p == prefix
        || (p.starts_with(prefix) && p.as_bytes().get(prefix.len()) == Some(&b'/'))
}

/// `dir` joined with the element `name`.
pub fn join(dir: &str, name: &str) -> String {
    let mut s = String::with_capacity(dir.len() + 1 + name.len());
    s.push_str(dir);
    if !dir.ends_with('/') {
        s.push('/');
    }
    s.push_str(name);
    s
}

/// The directories above `p`, from the root down, not including `p`.
pub fn ancestors(p: &str) -> impl Iterator<Item = &str> {
    let ends = p
        .char_indices()
        .filter(|&(_, c)| c == '/')
        .map(|(i, _)| i)
        .collect::<alloc::vec::Vec<_>>();
    let n = if p == "/" { 0 } else { ends.len() };
    (0..n).map(move |k| if ends[k] == 0 { "/" } else { &p[..ends[k]] })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn rules() {
        for ok in ["/", "/a", "/a/b", "/a.b/..c", "/λ"] {
            assert!(valid(ok), "{ok}");
        }
        for bad in [
            "", "a", "//", "/a/", "/a//b", "/.", "/..", "/a/../b", "/a/./b", "/a\0",
        ] {
            assert!(!valid(bad), "{bad:?}");
        }
        let long = alloc::format!("/{}", "x".repeat(MAX_ELEM + 1));
        assert!(!valid(&long));
        let deep = "/x".repeat(MAX_PATH / 2 + 1);
        assert!(!valid(&deep));
    }

    #[test]
    fn prefixes() {
        assert!(within("/a/b", "/"));
        assert!(within("/a", "/a"));
        assert!(within("/a/b", "/a"));
        assert!(!within("/ab", "/a"));
        assert!(!within("/", "/a"));
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(join("/a", "b"), "/a/b");
        let a: Vec<&str> = ancestors("/a/b/c").collect();
        assert_eq!(a, ["/", "/a", "/a/b"]);
        assert_eq!(ancestors("/a").collect::<Vec<_>>(), ["/"]);
        assert_eq!(ancestors("/").count(), 0);
    }
}
