//! POSIX-common host code, shared by Linux and macOS.

use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::Mutex;

use purg_abi::error;

/// Maps a host I/O error to an Inferno error string.
pub fn error_string(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => error::ENONEXIST.into(),
        io::ErrorKind::PermissionDenied => error::EPERM.into(),
        io::ErrorKind::NotADirectory => error::ENOTDIR.into(),
        io::ErrorKind::IsADirectory => error::EISDIR.into(),
        io::ErrorKind::AlreadyExists => error::EEXIST.into(),
        io::ErrorKind::Interrupted => error::EINTR.into(),
        _ => {
            let s = e.to_string();
            // Drop std's " (os error N)" suffix; Inferno strings are plain.
            match s.find(" (os error") {
                Some(i) => s[..i].to_lowercase(),
                None => s.to_lowercase(),
            }
        }
    }
}

/// Reads at an absolute offset without moving the file position.
pub fn read_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    f.read_at(buf, offset)
}

/// Writes at an absolute offset without moving the file position.
pub fn write_at(f: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    f.write_at(buf, offset)
}

/// Host identity of a file, used to build its Qid.
pub struct FileId {
    pub path: u64,
    pub vers: u32,
}

/// `qid.path` mixes `st_dev` and `st_ino`; `qid.vers` changes when the file's
/// mtime or size does.
pub fn file_id(md: &Metadata) -> FileId {
    let path = md.ino() ^ md.dev().rotate_left(48);
    let vers = (md.mtime_nsec() as u64 ^ md.mtime() as u64 ^ md.size()) as u32;
    FileId { path, vers }
}

fn user_by_uid(uid: u32) -> Option<String> {
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the call; `buf` outlives the
    // borrowed `pw_name`, which is copied before returning.
    let rc = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r succeeded, so pw_name is a NUL-terminated string in `buf`.
    let name = unsafe { CStr::from_ptr(pw.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

fn group_by_gid(gid: u32) -> Option<String> {
    let mut gr: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16384];
    let mut out: *mut libc::group = std::ptr::null_mut();
    // SAFETY: as for getpwuid_r above.
    let rc = unsafe { libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr(), buf.len(), &mut out) };
    if rc != 0 || out.is_null() {
        return None;
    }
    // SAFETY: getgrgid_r succeeded, so gr_name is a NUL-terminated string in `buf`.
    let name = unsafe { CStr::from_ptr(gr.gr_name) };
    Some(name.to_string_lossy().into_owned())
}

fn cached(
    cache: &Mutex<HashMap<u32, String>>,
    id: u32,
    lookup: fn(u32) -> Option<String>,
) -> String {
    let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
    c.entry(id)
        .or_insert_with(|| lookup(id).unwrap_or_else(|| id.to_string()))
        .clone()
}

/// Name of the user owning a file, as shown in `Dir.uid`.
pub fn owner_name(uid: u32) -> String {
    static USERS: std::sync::OnceLock<Mutex<HashMap<u32, String>>> = std::sync::OnceLock::new();
    cached(USERS.get_or_init(Default::default), uid, user_by_uid)
}

/// Name of a file's group, as shown in `Dir.gid`.
pub fn group_name(gid: u32) -> String {
    static GROUPS: std::sync::OnceLock<Mutex<HashMap<u32, String>>> = std::sync::OnceLock::new();
    cached(GROUPS.get_or_init(Default::default), gid, group_by_gid)
}

/// The invoking user's name, for `#c/user`.
pub fn user_name() -> String {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    user_by_uid(uid)
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| String::from("none"))
}

/// The host name, for `#c/sysname`.
pub fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for writes of buf.len() bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return String::from("purgatorio");
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Fills `buf` from the OS entropy source.
pub fn entropy(buf: &mut [u8]) {
    // getentropy is limited to 256 bytes per call on both Linux and macOS.
    for chunk in buf.chunks_mut(256) {
        // SAFETY: chunk is valid for writes of chunk.len() <= 256 bytes.
        let rc = unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) };
        assert_eq!(rc, 0, "getentropy failed");
    }
}
