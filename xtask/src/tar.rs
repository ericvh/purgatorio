//! A deterministic POSIX ustar writer for the kernel's initrd.
//!
//! Entries are sorted by path, with mtime 0, uid and gid 0, owner and group
//! `inferno`, and mode 0755 for directories and `.wasm` files, 0644
//! otherwise. The same tree always gives the same bytes. `purg_core::initrd`
//! reads the result.
//!
//! The function-name files `build-root` writes beside the guests
//! (`bin/<cmd>.names`, `docs/debug.md`) stay out of an image unless it is
//! asked for them ([`NAMES_ENV`]): they are a sixth of the size of the
//! modules. Those of the test guests in `bin/test` go in, because the sh
//! corpus reads them. The graphics programs ([`GRAPHICS`]) stay out: the
//! kernel and the probe have no draw device yet.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Result;

const BLOCK: usize = 512;
const OWNER: &[u8] = b"inferno";

/// Set to `1`, the kernel's and the probe's images carry every `.names`
/// file. `cargo xtask kernel|probe|initrd --names` sets it.
pub const NAMES_ENV: &str = "PURG_IMAGE_NAMES";

/// Whether [`NAMES_ENV`] asks for every `.names` file.
pub fn names_wanted() -> bool {
    std::env::var_os(NAMES_ENV).is_some_and(|v| v == "1")
}

/// Files for the draw device (docs/graphics.md), which only the emu has:
/// its programs, and `fontgen`, which makes fonts for it at build time. The fonts in
/// `root/fonts` stay out by not being under `bin` or `lib`.
const GRAPHICS: &[&str] = &[
    "bin/draw/",
    "bin/test/draw.",
    "bin/fontgen.",
    "bin/acme.",
    "bin/rio.",
];

/// Whether the file at `path` (relative to the root, `/`-separated) goes
/// into an image; `names` is [`names_wanted`].
pub fn in_image(path: &str, names: bool) -> bool {
    if GRAPHICS.iter().any(|g| path.starts_with(g)) {
        return false;
    }
    names || !path.ends_with(".names") || path.starts_with("bin/test/")
}

fn octal(field: &mut [u8], v: u64) -> Result<()> {
    let s = format!("{v:0w$o}", w = field.len() - 1);
    if s.len() >= field.len() {
        return Err(format!("{v} does not fit a {}-byte tar field", field.len()));
    }
    field[..s.len()].copy_from_slice(s.as_bytes());
    Ok(())
}

/// One ustar header block for `path` (relative, `/`-separated).
fn header(path: &str, dir: bool, mode: u32, size: u64) -> Result<[u8; BLOCK]> {
    let mut h = [0u8; BLOCK];
    let full = if dir {
        format!("{path}/")
    } else {
        path.to_string()
    };
    let (prefix, name) = if full.len() <= 100 {
        ("", full.as_str())
    } else {
        // Split at a `/` so the prefix fits 155 bytes and the name 100.
        let cut = full
            .char_indices()
            .filter(|&(i, c)| c == '/' && i <= 155 && full.len() - i - 1 <= 100)
            .map(|(i, _)| i)
            .next()
            .ok_or_else(|| format!("{path}: too long for a ustar header"))?;
        (&full[..cut], &full[cut + 1..])
    };
    if name.is_empty() {
        return Err(format!("{path}: too long for a ustar header"));
    }
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], u64::from(mode))?;
    octal(&mut h[108..116], 0)?;
    octal(&mut h[116..124], 0)?;
    octal(&mut h[124..136], size)?;
    octal(&mut h[136..148], 0)?;
    h[156] = if dir { b'5' } else { b'0' };
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[265..265 + OWNER.len()].copy_from_slice(OWNER);
    h[297..297 + OWNER.len()].copy_from_slice(OWNER);
    octal(&mut h[329..337], 0)?;
    octal(&mut h[337..345], 0)?;
    h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&c| u32::from(c)).sum();
    h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
    Ok(h)
}

/// Every directory and file under `dir`, relative to `base`.
fn walk(base: &Path, dir: &Path, out: &mut Vec<(PathBuf, bool)>) -> Result<()> {
    out.push((dir.strip_prefix(base).unwrap().to_path_buf(), true));
    for e in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let p = e.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        let t = fs::symlink_metadata(&p)
            .map_err(|e| format!("{}: {e}", p.display()))?
            .file_type();
        if t.is_dir() {
            walk(base, &p, out)?;
        } else if t.is_file() {
            out.push((p.strip_prefix(base).unwrap().to_path_buf(), false));
        } else {
            return Err(format!("{}: not a file or directory", p.display()));
        }
    }
    Ok(())
}

/// A ustar archive of the directories `dirs` under `base`, with paths
/// relative to `base` (`bin`, `bin/sh.wasm`, ...). Without `names` it
/// leaves out the `.names` files [`in_image`] keeps out.
pub fn archive(base: &Path, dirs: &[&str], names: bool) -> Result<Vec<u8>> {
    let mut paths = Vec::new();
    for d in dirs {
        walk(base, &base.join(d), &mut paths)?;
    }
    paths.sort();
    let mut out = Vec::new();
    for (rel, dir) in paths {
        let path = rel
            .to_str()
            .ok_or_else(|| format!("{}: not UTF-8", rel.display()))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        if !dir && !in_image(&path, names) {
            continue;
        }
        let mode = if dir || path.ends_with(".wasm") {
            0o755
        } else {
            0o644
        };
        if dir {
            out.extend_from_slice(&header(&path, true, mode, 0)?);
        } else {
            let data = fs::read(base.join(&rel)).map_err(|e| format!("{path}: {e}"))?;
            out.extend_from_slice(&header(&path, false, mode, data.len() as u64)?);
            out.extend_from_slice(&data);
            out.resize(out.len().next_multiple_of(BLOCK), 0);
        }
    }
    out.resize(out.len() + 2 * BLOCK, 0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use purg_core::initrd::{Kind, entries};

    /// A small tree: nested directories, an empty file, a long path that
    /// needs the ustar prefix, and a file that is not a multiple of 512.
    fn fixture(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
        let long = "d".repeat(60);
        fs::create_dir_all(dir.join("bin/lib")).unwrap();
        fs::create_dir_all(dir.join(format!("lib/{long}/{long}"))).unwrap();
        fs::create_dir_all(dir.join("lib/empty")).unwrap();
        fs::write(dir.join("bin/sh.wasm"), b"\0asm\x01\0\0\0").unwrap();
        fs::write(dir.join("bin/lib/add.wasm"), vec![7u8; 1000]).unwrap();
        fs::write(dir.join("lib/motd"), b"").unwrap();
        fs::write(dir.join(format!("lib/{long}/{long}/f.txt")), b"deep\n").unwrap();
        fs::write(dir.join("other"), b"not archived").unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let d = crate::workspace().join("target/tmp").join(name);
        fixture(&d);
        d
    }

    #[test]
    fn round_trip_and_deterministic() {
        let d = scratch("tar-round-trip");
        let a = archive(&d, &["bin", "lib"], false).unwrap();
        assert_eq!(a, archive(&d, &["bin", "lib"], false).unwrap());
        assert_eq!(a.len() % BLOCK, 0);
        let e = entries(&a).unwrap();
        let got: Vec<_> = e
            .iter()
            .map(|e| (e.path.as_str(), e.kind, e.perm))
            .collect();
        let long = "d".repeat(60);
        let deep = format!("lib/{long}/{long}");
        let deep_f = format!("{deep}/f.txt");
        let lib_long = format!("lib/{long}");
        assert_eq!(
            got,
            [
                ("bin", Kind::Dir, 0o755),
                ("bin/lib", Kind::Dir, 0o755),
                ("bin/lib/add.wasm", Kind::File, 0o755),
                ("bin/sh.wasm", Kind::File, 0o755),
                ("lib", Kind::Dir, 0o755),
                (lib_long.as_str(), Kind::Dir, 0o755),
                (deep.as_str(), Kind::Dir, 0o755),
                (deep_f.as_str(), Kind::File, 0o644),
                ("lib/empty", Kind::Dir, 0o755),
                ("lib/motd", Kind::File, 0o644),
            ]
        );
        assert_eq!(e[2].data, &[7u8; 1000][..]);
        assert_eq!(e[7].data, b"deep\n");
        assert!(e[9].data.is_empty());
    }

    /// Function names go in only when asked for, except the test guests'.
    #[test]
    fn names_only_when_asked() {
        let d = scratch("tar-names");
        fs::create_dir_all(d.join("bin/test")).unwrap();
        fs::write(d.join("bin/sh.names"), b"0 main\n").unwrap();
        fs::write(d.join("bin/test/t.names"), b"0 main\n").unwrap();
        let paths = |names| {
            let a = archive(&d, &["bin"], names).unwrap();
            let e = entries(&a).unwrap();
            e.iter()
                .map(|e| e.path.clone())
                .filter(|p| p.ends_with(".names"))
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(false), ["bin/test/t.names"]);
        assert_eq!(paths(true), ["bin/sh.names", "bin/test/t.names"]);
    }

    #[test]
    fn graphics_stay_out() {
        assert!(!in_image("bin/draw/demo.wasm", true));
        assert!(!in_image("bin/test/draw.wasm", true));
        assert!(!in_image("bin/test/draw.names", false));
        assert!(!in_image("bin/fontgen.wasm", false));
        assert!(!in_image("bin/acme.wasm", false));
        assert!(in_image("bin/test/drawn.wasm", false));
        assert!(in_image("bin/test/abi.wasm", false));
    }

    /// `crates/purg-core/tests/data/initrd.tar` is this writer's output for
    /// the fixture; purg-core's tests read it into a RamTree.
    #[test]
    fn fixture_is_current() {
        let d = scratch("tar-fixture");
        let a = archive(&d, &["bin", "lib"], false).unwrap();
        let path = crate::workspace().join("crates/purg-core/tests/data/initrd.tar");
        if std::env::var_os("UPDATE_FIXTURE").is_some() {
            fs::write(&path, &a).unwrap();
        }
        assert!(
            fs::read(&path).unwrap() == a,
            "{} is stale; rerun with UPDATE_FIXTURE=1",
            path.display()
        );
    }
}
