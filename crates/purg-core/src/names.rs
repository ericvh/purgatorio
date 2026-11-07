//! Lexical path handling, as in Plan 9 and Inferno `cleanname`.

use alloc::string::String;
use alloc::vec::Vec;

/// Returns the shortest lexically equivalent path: collapses `//`, removes
/// `.` elements, and resolves `..` against the preceding element. `..` at the
/// root stays at the root. A device prefix such as `#c` or `#Uspec` is treated
/// as the root of its path.
pub fn cleanname(path: &str) -> String {
    let (prefix, rest) = split_device(path);
    let rooted = prefix.is_some() || rest.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for elem in rest.split('/') {
        match elem {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(&e) if e != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            e => out.push(e),
        }
    }
    let mut s = String::new();
    if let Some(p) = prefix {
        s.push_str(p);
        if !out.is_empty() {
            s.push('/');
        }
    } else if rooted {
        s.push('/');
    }
    s.push_str(&out.join("/"));
    if s.is_empty() {
        s.push('.');
    }
    s
}

/// Splits `#cspec/rest` into `(Some("#cspec"), "/rest")`; other paths return
/// `(None, path)`.
pub fn split_device(path: &str) -> (Option<&str>, &str) {
    if !path.starts_with('#') {
        return (None, path);
    }
    match path.find('/') {
        Some(i) => (Some(&path[..i]), &path[i..]),
        None => (Some(path), ""),
    }
}

/// The non-empty elements of a cleaned path, without any device prefix.
pub fn elements(path: &str) -> impl Iterator<Item = &str> {
    split_device(path).1.split('/').filter(|e| !e.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean() {
        for (input, want) in [
            ("/", "/"),
            ("", "."),
            ("a/b/../c", "a/c"),
            ("/a//b/./c/", "/a/b/c"),
            ("/..", "/"),
            ("/a/../../b", "/b"),
            ("../a", "../a"),
            ("a/../..", ".."),
            ("#c", "#c"),
            ("#c/", "#c"),
            ("#c/./cons", "#c/cons"),
            ("#Uspec/a/../b", "#Uspec/b"),
            ("#c/..", "#c"),
        ] {
            assert_eq!(cleanname(input), want, "cleanname({input:?})");
        }
    }

    #[test]
    fn device_split() {
        assert_eq!(split_device("#c/cons"), (Some("#c"), "/cons"));
        assert_eq!(split_device("#U"), (Some("#U"), ""));
        assert_eq!(split_device("/dev"), (None, "/dev"));
        assert_eq!(
            elements("#U/a/b").collect::<Vec<_>>(),
            alloc::vec!["a", "b"]
        );
    }
}
