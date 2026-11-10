//! cleanname [-d dir] name ...: print each name with redundant slashes
//! removed and `.` and `..` resolved lexically. With `-d`, a name that does
//! not start with `/` or `#` is taken relative to `dir`.

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

/// Lexically cleans a path name, after Plan 9's cleanname(2). A `#x`
/// device name at the start is a root that `..` cannot climb above.
fn clean(name: &str) -> String {
    let rooted = name.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    let mut base = 0;
    for (i, p) in name.split('/').enumerate() {
        match p {
            "" | "." => {}
            ".." => {
                if parts.len() > base && parts.last() != Some(&"..") {
                    parts.pop();
                } else if !rooted && base == 0 {
                    parts.push("..");
                }
            }
            _ => {
                parts.push(p);
                if i == 0 && p.starts_with('#') {
                    base = 1;
                }
            }
        }
    }
    let body = parts.join("/");
    match (rooted, body.is_empty()) {
        (true, _) => format!("/{body}"),
        (false, true) => String::from("."),
        (false, false) => body,
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "cleanname [-d pwd] name ...");
    let mut dir: Option<String> = None;
    while let Some(o) = arg.opt() {
        match o {
            'd' => dir = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let names = arg.argv();
    if names.is_empty() {
        return Err(usage);
    }
    for n in &names {
        let full = match &dir {
            Some(d) if !n.starts_with('/') && !n.starts_with('#') => format!("{d}/{n}"),
            _ => n.clone(),
        };
        println!("{}", clean(&full));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn names() {
        for (a, b) in [
            ("/a//b/./c/../d", "/a/b/d"),
            ("a/../..", ".."),
            ("/..", "/"),
            ("", "."),
            ("./", "."),
            ("../../x", "../../x"),
            ("#c/../cons", "#c/cons"),
            ("a/b/", "a/b"),
        ] {
            assert_eq!(clean(a), b, "{a}");
        }
    }
}
