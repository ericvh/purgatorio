//! A disk over a host file or any `Read + Write + Seek` (the `std`
//! feature): what `xtask` and the `disk/` commands work on.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use alloc::borrow::Cow;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::layout::DMDIR;
use crate::{Disk, Fs, Loc, Result, WStat};

/// A disk image in a file, in units of `sector` bytes.
pub struct FileDisk<F> {
    file: F,
    size: u64,
    sector: usize,
    read_only: bool,
}

fn io(e: std::io::Error) -> Cow<'static, str> {
    Cow::Owned(e.to_string())
}

impl<F: Read + Write + Seek> FileDisk<F> {
    /// The disk is the first `size` bytes of `file`.
    pub fn new(file: F, size: u64, sector: usize) -> FileDisk<F> {
        FileDisk {
            file,
            size,
            sector,
            read_only: false,
        }
    }

    pub fn set_read_only(&mut self, ro: bool) {
        self.read_only = ro;
    }

    pub fn into_inner(self) -> F {
        self.file
    }
}

impl<F: Read + Write + Seek> Disk for FileDisk<F> {
    fn sector(&self) -> usize {
        self.sector
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn read(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        if off.saturating_add(buf.len() as u64) > self.size {
            return Err(Cow::Borrowed(purg_abi::error::EIO));
        }
        self.file.seek(SeekFrom::Start(off)).map_err(io)?;
        // A sparse image may be shorter than its size: the rest reads as
        // zeros.
        let mut done = 0;
        while done < buf.len() {
            match self.file.read(&mut buf[done..]).map_err(io)? {
                0 => {
                    buf[done..].fill(0);
                    break;
                }
                n => done += n,
            }
        }
        Ok(())
    }

    fn write(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Cow::Borrowed(purg_abi::error::EPERM));
        }
        if off.saturating_add(buf.len() as u64) > self.size {
            return Err(Cow::Borrowed(purg_abi::error::EIO));
        }
        self.file.seek(SeekFrom::Start(off)).map_err(io)?;
        self.file.write_all(buf).map_err(io)
    }

    fn flush(&mut self) -> Result<()> {
        self.file.flush().map_err(io)
    }

    fn read_only(&self) -> bool {
        self.read_only
    }
}

/// What [`add_tree`] copied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Copied {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

/// How [`add_tree`] stamps what it copies.
pub struct Stamp<'a> {
    /// The owner and group.
    pub user: &'a str,
    /// The permission bits of a host file or directory.
    pub perm: &'a dyn Fn(&Path, &std::fs::Metadata) -> u32,
    /// A fixed modification time, or `None` for the host's.
    pub mtime: Option<u32>,
}

/// Copies the host directory `src` (its contents, recursively, in name
/// order) into directory `dir` of `fs`. Anything that is neither a file
/// nor a directory is skipped.
pub fn add_tree<D: Disk>(
    fs: &mut Fs<D>,
    dir: Loc,
    src: &Path,
    stamp: &Stamp<'_>,
) -> Result<Copied> {
    let mut copied = Copied::default();
    copy_dir(fs, dir, src, stamp, &mut copied)?;
    Ok(copied)
}

fn copy_dir<D: Disk>(
    fs: &mut Fs<D>,
    dir: Loc,
    src: &Path,
    stamp: &Stamp<'_>,
    copied: &mut Copied,
) -> Result<()> {
    let at = |e: std::io::Error| Cow::Owned(alloc::format!("{}: {e}", src.display()));
    let mut entries: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(src)
        .map_err(at)?
        .filter_map(|e| e.ok())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    entries.sort();
    for (name, path) in entries {
        let meta = std::fs::metadata(&path).map_err(at)?;
        let perm = (stamp.perm)(&path, &meta) & 0o777;
        let mtime = stamp.mtime.unwrap_or_else(|| {
            meta.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as u32)
        });
        let named = |e: crate::Error| Cow::Owned(alloc::format!("{}: {e}", path.display()));
        let loc = if meta.is_dir() {
            let loc = fs
                .create(dir, &name, DMDIR | 0o777, stamp.user)
                .map_err(named)?;
            copy_dir(fs, loc, &path, stamp, copied)?;
            copied.dirs += 1;
            loc
        } else if meta.is_file() {
            let loc = fs.create(dir, &name, perm, stamp.user).map_err(named)?;
            let data = std::fs::read(&path).map_err(at)?;
            let n = fs.write(loc.me, 0, &data, stamp.user).map_err(named)?;
            if n != data.len() {
                return Err(named(Cow::Borrowed(purg_abi::error::ENOSPACE)));
            }
            copied.files += 1;
            copied.bytes += n as u64;
            loc
        } else {
            continue;
        };
        // The parent's permissions masked the create's.
        let w = WStat {
            mode: Some((fs.dentry(loc.me).map_err(named)?.mode & DMDIR) | perm),
            mtime: Some(mtime),
            ..WStat::default()
        };
        fs.wstat(loc, &w, stamp.user).map_err(named)?;
    }
    Ok(())
}

/// Parses a size: a number of bytes, with an optional `k`, `m` or `g`
/// (powers of 1024).
pub fn parse_size(s: &str) -> Option<u64> {
    let (num, mul) = match s.as_bytes().last()? {
        b'k' | b'K' => (&s[..s.len() - 1], 1u64 << 10),
        b'm' | b'M' => (&s[..s.len() - 1], 1 << 20),
        b'g' | b'G' => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>().ok()?.checked_mul(mul)
}
