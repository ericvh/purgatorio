//! Files.
//!
//! In a wasm32 guest this is `std::fs`, which works through the host's
//! WASI layer. A wasm64 guest has no WASI: its `std::fs` exists but every
//! call fails. There this module is the SDK's own [`File`] over the
//! `purgatorio64` calls, with the parts of `std::fs` that commands use
//! (`docs/wasm64.md`). Code that says `purgatorio::fs` instead of
//! `std::fs` builds for both widths.

#[cfg(not(target_arch = "wasm64"))]
pub use std::fs::*;

#[cfg(target_arch = "wasm64")]
pub use wide::*;

#[cfg(target_arch = "wasm64")]
mod wide {
    use std::ffi::OsString;
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    use crate::abi::mode;
    use crate::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
    use crate::io::{last_error, read_fd, write_fd};
    use crate::sys;

    /// An open file. Reads and writes are single system calls, as
    /// `std::fs::File`'s are; wrap it in a `BufReader` or `BufWriter` for
    /// small ones.
    #[derive(Debug)]
    pub struct File(OwnedFd);

    /// A path as the host takes it. Names in Inferno are UTF-8.
    fn name(path: &Path) -> io::Result<&str> {
        path.to_str()
            .ok_or_else(|| io::Error::other(crate::abi::error::EBADARG))
    }

    impl File {
        /// Opens a file for reading.
        pub fn open(path: impl AsRef<Path>) -> io::Result<File> {
            OpenOptions::new().read(true).open(path)
        }

        /// Opens a file for writing, creating it or truncating it.
        pub fn create(path: impl AsRef<Path>) -> io::Result<File> {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
        }

        /// `OpenOptions::new()`.
        pub fn options() -> OpenOptions {
            OpenOptions::new()
        }

        /// The file's `Dir`, as `std`'s `Metadata`.
        pub fn metadata(&self) -> io::Result<Metadata> {
            crate::fstat(self.as_raw_fd())
                .map(Metadata)
                .map_err(|_| last_error())
        }

        /// Sets the file's length (`wstat`).
        pub fn set_len(&self, size: u64) -> io::Result<()> {
            let mut d = crate::Dir::null();
            d.length = size;
            crate::fwstat(self.as_raw_fd(), &d).map_err(|_| last_error())
        }

        /// Asks the server to commit the file: a `wstat` that changes
        /// nothing, as in Plan 9.
        pub fn sync_all(&self) -> io::Result<()> {
            crate::fwstat(self.as_raw_fd(), &crate::Dir::null()).map_err(|_| last_error())
        }

        /// [`File::sync_all`].
        pub fn sync_data(&self) -> io::Result<()> {
            self.sync_all()
        }

        /// A second handle on the same open file: they share an offset.
        pub fn try_clone(&self) -> io::Result<File> {
            self.0.try_clone().map(File)
        }

        /// Reads at `offset` without moving the file's own offset.
        pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            let n =
                unsafe { sys::pread(self.as_raw_fd(), buf.as_mut_ptr(), buf.len(), offset as i64) };
            if n < 0 {
                Err(last_error())
            } else {
                Ok(n as usize)
            }
        }

        /// Writes at `offset` without moving the file's own offset.
        pub fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
            let n =
                unsafe { sys::pwrite(self.as_raw_fd(), buf.as_ptr(), buf.len(), offset as i64) };
            if n < 0 {
                Err(last_error())
            } else {
                Ok(n as usize)
            }
        }
    }

    impl From<OwnedFd> for File {
        fn from(fd: OwnedFd) -> File {
            File(fd)
        }
    }

    impl From<File> for OwnedFd {
        fn from(f: File) -> OwnedFd {
            f.0
        }
    }

    impl AsRawFd for File {
        fn as_raw_fd(&self) -> RawFd {
            self.0.as_raw_fd()
        }
    }

    impl FromRawFd for File {
        unsafe fn from_raw_fd(fd: RawFd) -> File {
            // SAFETY: the caller's.
            File(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    impl IntoRawFd for File {
        fn into_raw_fd(self) -> RawFd {
            self.0.into_raw_fd()
        }
    }

    impl Read for File {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            read_fd(self.as_raw_fd(), buf)
        }
    }

    impl Read for &File {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            read_fd(self.as_raw_fd(), buf)
        }
    }

    impl Write for File {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            write_fd(self.as_raw_fd(), buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for &File {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            write_fd(self.as_raw_fd(), buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for File {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            let (off, whence) = match pos {
                SeekFrom::Start(n) => (n as i64, 0),
                SeekFrom::Current(n) => (n, 1),
                SeekFrom::End(n) => (n, 2),
            };
            let at = unsafe { sys::seek(self.as_raw_fd(), off, whence) };
            if at < 0 {
                Err(last_error())
            } else {
                Ok(at as u64)
            }
        }
    }

    /// How to open a file: the options of `std::fs::OpenOptions` that
    /// Inferno's open modes can express.
    #[derive(Debug, Clone, Default)]
    pub struct OpenOptions {
        read: bool,
        write: bool,
        append: bool,
        truncate: bool,
        create: bool,
    }

    impl OpenOptions {
        pub fn new() -> OpenOptions {
            OpenOptions::default()
        }

        pub fn read(&mut self, on: bool) -> &mut OpenOptions {
            self.read = on;
            self
        }

        pub fn write(&mut self, on: bool) -> &mut OpenOptions {
            self.write = on;
            self
        }

        /// Writes go to the end of the file: the offset starts there.
        /// (Inferno has no append mode; an append-only file, `DMAPPEND`,
        /// is the server's.)
        pub fn append(&mut self, on: bool) -> &mut OpenOptions {
            self.append = on;
            self
        }

        pub fn truncate(&mut self, on: bool) -> &mut OpenOptions {
            self.truncate = on;
            self
        }

        /// Makes the file if it does not exist.
        pub fn create(&mut self, on: bool) -> &mut OpenOptions {
            self.create = on;
            self
        }

        pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
            let path = name(path.as_ref())?;
            let writes = self.write || self.append;
            let omode = match (self.read, writes) {
                (_, false) => mode::OREAD,
                (false, true) => mode::OWRITE,
                (true, true) => mode::ORDWR,
            };
            let trunc = if self.truncate { mode::OTRUNC } else { 0 };
            let fd = match crate::open(path, omode | trunc) {
                Ok(fd) => fd,
                Err(e) if self.create && e.0.contains(crate::abi::error::ENONEXIST) => {
                    crate::create(path, omode, 0o666).map_err(|_| last_error())?
                }
                Err(_) => return Err(last_error()),
            };
            let mut f = File(fd);
            if self.append {
                f.seek(SeekFrom::End(0))?;
            }
            Ok(f)
        }
    }

    /// A file's `Dir`, with the questions `std::fs::Metadata` answers.
    #[derive(Debug, Clone)]
    pub struct Metadata(pub crate::Dir);

    impl Metadata {
        pub fn len(&self) -> u64 {
            self.0.length
        }

        pub fn is_empty(&self) -> bool {
            self.0.length == 0
        }

        pub fn is_dir(&self) -> bool {
            self.0.qid.is_dir()
        }

        pub fn is_file(&self) -> bool {
            !self.is_dir()
        }

        pub fn file_type(&self) -> FileType {
            FileType(self.is_dir())
        }

        pub fn modified(&self) -> io::Result<crate::time::SystemTime> {
            Ok(crate::time::UNIX_EPOCH + crate::time::Duration::from_secs(self.0.mtime.into()))
        }

        pub fn accessed(&self) -> io::Result<crate::time::SystemTime> {
            Ok(crate::time::UNIX_EPOCH + crate::time::Duration::from_secs(self.0.atime.into()))
        }
    }

    /// Whether a file is a directory: Inferno has no other kinds.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FileType(bool);

    impl FileType {
        pub fn is_dir(&self) -> bool {
            self.0
        }

        pub fn is_file(&self) -> bool {
            !self.0
        }

        pub fn is_symlink(&self) -> bool {
            false
        }
    }

    /// `sys->stat`.
    pub fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
        crate::stat(name(path.as_ref())?)
            .map(Metadata)
            .map_err(|_| last_error())
    }

    /// An entry of a directory, from [`read_dir`].
    #[derive(Debug, Clone)]
    pub struct DirEntry {
        dir: PathBuf,
        /// The entry's full Inferno `Dir`.
        pub stat: crate::Dir,
    }

    impl DirEntry {
        pub fn file_name(&self) -> OsString {
            OsString::from(&self.stat.name)
        }

        /// The entry's `Dir`, which the listing already holds.
        pub fn metadata(&self) -> io::Result<Metadata> {
            Ok(Metadata(self.stat.clone()))
        }

        pub fn file_type(&self) -> io::Result<FileType> {
            Ok(FileType(self.stat.qid.is_dir()))
        }

        pub fn path(&self) -> PathBuf {
            self.dir.join(&self.stat.name)
        }
    }

    /// The entries of a directory, read as the iterator is created.
    pub fn read_dir(
        path: impl AsRef<Path>,
    ) -> io::Result<std::vec::IntoIter<io::Result<DirEntry>>> {
        let dir = path.as_ref().to_path_buf();
        let fd = crate::open(name(&dir)?, mode::OREAD).map_err(|_| last_error())?;
        let mut out = Vec::new();
        loop {
            let batch = crate::dirread(fd.as_raw_fd()).map_err(|_| last_error())?;
            if batch.is_empty() {
                break;
            }
            out.extend(batch.into_iter().map(|stat| {
                Ok(DirEntry {
                    dir: dir.clone(),
                    stat,
                })
            }));
        }
        Ok(out.into_iter())
    }

    /// The whole contents of a file.
    pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        File::open(path)?.read_to_end(&mut out)?;
        Ok(out)
    }

    /// The whole contents of a file, which must be UTF-8.
    pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
        let mut out = String::new();
        File::open(path)?.read_to_string(&mut out)?;
        Ok(out)
    }

    /// Creates or truncates a file and writes `data` to it.
    pub fn write(path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> io::Result<()> {
        File::create(path)?.write_all(data.as_ref())
    }

    fn failed(msg: &str) -> io::Error {
        crate::werrstr(msg);
        last_error()
    }

    /// `sys->remove` of a file; a directory is refused.
    pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
        let path = name(path.as_ref())?;
        if metadata(path)?.is_dir() {
            return Err(failed(crate::abi::error::EISDIR));
        }
        crate::remove(path).map_err(|_| last_error())
    }

    /// `sys->remove` of a directory, which must be empty; a file is
    /// refused.
    pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
        let path = name(path.as_ref())?;
        if !metadata(path)?.is_dir() {
            return Err(failed(crate::abi::error::ENOTDIR));
        }
        crate::remove(path).map_err(|_| last_error())
    }

    /// Makes a directory.
    pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
        crate::create(name(path.as_ref())?, mode::OREAD, crate::DMDIR | 0o777)
            .map(drop)
            .map_err(|_| last_error())
    }

    /// Makes a directory and the directories above it that are missing.
    pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        if metadata(path).is_ok_and(|m| m.is_dir()) {
            return Ok(());
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_dir_all(parent)?;
        }
        match create_dir(path) {
            // Made meanwhile by someone else.
            Err(_) if metadata(path).is_ok_and(|m| m.is_dir()) => Ok(()),
            r => r,
        }
    }

    /// Renames a file within its directory (`wstat` of the name), taking
    /// the place of a file already called `to`. Inferno cannot move a
    /// file to another directory.
    pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
        let (from, to) = (name(from.as_ref())?, name(to.as_ref())?);
        let split = |p: &str| match p.rsplit_once('/') {
            Some((d, n)) => (d.to_string(), n.to_string()),
            None => (String::new(), p.to_string()),
        };
        let ((fdir, fname), (tdir, tname)) = (split(from), split(to));
        if fdir != tdir {
            return Err(failed(crate::abi::error::EXDEV));
        }
        metadata(from)?;
        if fname == tname {
            return Ok(());
        }
        if metadata(to).is_ok() {
            crate::remove(to).map_err(|_| last_error())?;
        }
        let mut d = crate::Dir::null();
        d.name = tname;
        crate::wstat(from, &d).map_err(|_| last_error())
    }
}
