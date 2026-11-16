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

    /// `sys->remove` of a file.
    pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
        crate::remove(name(path.as_ref())?).map_err(|_| last_error())
    }

    /// Makes a directory.
    pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
        crate::create(name(path.as_ref())?, mode::OREAD, crate::DMDIR | 0o777)
            .map(drop)
            .map_err(|_| last_error())
    }
}
