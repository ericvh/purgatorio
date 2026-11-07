//! `#U`: a host directory tree.

use std::any::Any;
use std::fs::{self, File, Metadata, OpenOptions};
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use purg_abi::{error, mode};
use purg_core::{Chan, Dev, Dir, Error, Qid, Result, Walked};
use purg_styx::{DMDIR, QTDIR, QTFILE};

use crate::host;

/// The host path behind a Chan.
struct HostPath(PathBuf);

pub struct DevFs {
    root: PathBuf,
}

fn host_err(e: std::io::Error) -> Error {
    Error::from(host::error_string(&e))
}

fn qid(md: &Metadata) -> Qid {
    let id = host::file_id(md);
    Qid {
        path: id.path,
        vers: id.vers,
        qtype: if md.is_dir() { QTDIR } else { QTFILE },
    }
}

impl DevFs {
    pub fn new(root: PathBuf) -> Result<DevFs> {
        let root = fs::canonicalize(&root).map_err(|e| {
            Error::from(format!(
                "#U root {}: {}",
                root.display(),
                host::error_string(&e)
            ))
        })?;
        Ok(DevFs { root })
    }

    fn path<'c>(&self, c: &'c Chan) -> Result<&'c Path> {
        c.aux::<HostPath>()
            .map(|p| p.0.as_path())
            .ok_or(Error::new(error::ENONEXIST))
    }

    fn walked(path: PathBuf) -> Result<Walked> {
        let md = fs::metadata(&path).map_err(host_err)?;
        Ok(Walked {
            qid: qid(&md),
            aux: Some(Arc::new(HostPath(path)) as Arc<dyn Any + Send + Sync>),
        })
    }

    fn dir(&self, path: &Path, md: &Metadata) -> Dir {
        let name = if path == self.root {
            String::from("/")
        } else {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let mut perm = md.mode() & 0o777;
        if md.is_dir() {
            perm |= DMDIR;
        }
        let uid = host::owner_name(md.uid());
        Dir {
            dtype: 'U' as u16,
            dev: 0,
            qid: qid(md),
            mode: perm,
            atime: md.atime() as u32,
            mtime: md.mtime() as u32,
            length: if md.is_dir() { 0 } else { md.size() },
            name,
            muid: uid.clone(),
            uid,
            gid: host::group_name(md.gid()),
        }
    }
}

impl Dev for DevFs {
    fn dc(&self) -> char {
        'U'
    }

    fn name(&self) -> &'static str {
        "fs"
    }

    fn attach(&self, spec: &str) -> Result<Walked> {
        if !spec.is_empty() {
            return Err(Error::new(error::EBADARG));
        }
        DevFs::walked(self.root.clone())
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let dir = self.path(c)?;
        let path = dir.join(name);
        let w = DevFs::walked(path.clone())?;
        if !host::exact_name_exists(dir, name).map_err(host_err)? {
            return Err(Error::new(error::ENONEXIST));
        }
        Ok(w)
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        let path = self.path(c)?;
        let md = fs::metadata(path).map_err(host_err)?;
        Ok(self.dir(path, &md))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let access = omode & mode::ACCESS;
        let write = access == mode::OWRITE || access == mode::ORDWR;
        if c.is_dir() {
            if write || omode & mode::OTRUNC != 0 {
                return Err(Error::new(error::EISDIR));
            }
            return Ok(());
        }
        let f = OpenOptions::new()
            .read(!write || access == mode::ORDWR)
            .write(write)
            .truncate(write && omode & mode::OTRUNC != 0)
            .open(self.path(c)?)
            .map_err(host_err)?;
        *c.state.lock() = Some(Box::new(f));
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let state = c.state.lock();
        let f = state
            .as_ref()
            .and_then(|s| s.downcast_ref::<File>())
            .ok_or(Error::new(error::EBADUSEFD))?;
        host::read_at(f, buf, offset).map_err(host_err)
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let path = self.path(c)?;
        let mut out = Vec::new();
        for entry in fs::read_dir(path).map_err(host_err)? {
            let entry = entry.map_err(host_err)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if host::hidden_entry(name) {
                continue;
            }
            // Entries that vanish or are dangling symlinks are skipped.
            if let Ok(md) = fs::metadata(entry.path()) {
                out.push(self.dir(&entry.path(), &md));
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        let state = c.state.lock();
        let f = state
            .as_ref()
            .and_then(|s| s.downcast_ref::<File>())
            .ok_or(Error::new(error::EBADUSEFD))?;
        host::write_at(f, buf, offset).map_err(host_err)
    }

    fn create(&self, dir: &Chan, name: &str, perm: u32) -> Result<Walked> {
        check_name(name)?;
        let path = self.path(dir)?.join(name);
        if perm & DMDIR != 0 {
            fs::DirBuilder::new()
                .mode(perm & 0o777)
                .create(&path)
                .map_err(host_err)?;
        } else {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(perm & 0o777)
                .open(&path)
                .map_err(host_err)?;
        }
        DevFs::walked(path)
    }

    fn remove(&self, c: &Chan) -> Result<()> {
        let path = self.path(c)?;
        if path == self.root {
            return Err(Error::new(error::EPERM));
        }
        if c.is_dir() {
            fs::remove_dir(path).map_err(host_err)
        } else {
            fs::remove_file(path).map_err(host_err)
        }
    }

    fn wstat(&self, c: &Chan, d: &Dir) -> Result<()> {
        let path = self.path(c)?;
        let md = fs::metadata(path).map_err(host_err)?;
        if d.length != !0 {
            if md.is_dir() {
                return Err(Error::new(error::EISDIR));
            }
            OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|f| f.set_len(d.length))
                .map_err(host_err)?;
        }
        if d.mode != !0 {
            if (d.mode & DMDIR != 0) != md.is_dir() {
                return Err(Error::new(error::EPERM));
            }
            fs::set_permissions(path, fs::Permissions::from_mode(d.mode & 0o777))
                .map_err(host_err)?;
        }
        if d.mtime != !0 {
            let t = UNIX_EPOCH + Duration::from_secs(d.mtime.into());
            File::open(path)
                .and_then(|f| f.set_modified(t))
                .map_err(host_err)?;
        }
        // The rename comes last: the Chan still names the old path.
        if !d.name.is_empty() && path.file_name().is_some_and(|n| n != d.name.as_str()) {
            check_name(&d.name)?;
            if path == self.root {
                return Err(Error::new(error::EPERM));
            }
            let to = path.with_file_name(&d.name);
            if fs::symlink_metadata(&to).is_ok() {
                return Err(Error::new(error::EEXIST));
            }
            fs::rename(path, to).map_err(host_err)?;
        }
        Ok(())
    }
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(Error::new(error::EBADARG));
    }
    Ok(())
}
