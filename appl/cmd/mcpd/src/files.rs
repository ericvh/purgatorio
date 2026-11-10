//! The file tools and the resources. They run in the session prog, on
//! names in the session's namespace, and finish before the next request is
//! read. A file that blocks when read blocks the session; the front end
//! gives up on it after a minute.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;

use purgatorio::abi::mode::{OREAD, OWRITE};
use purgatorio::{DMAPPEND, DMDIR, Dir, Error};

use crate::json::{Value, base64_decode, base64_encode, obj};
use crate::rpc::uri_of;
use crate::session::err;
use crate::tooldefs::{DEFAULT_READ, MAX_ENTRIES, MAX_RESOURCE, MAX_RESOURCES};

/// `stat`: the directory entry and the cleaned name.
pub fn stat(path: &str) -> Result<Value, String> {
    let d = purgatorio::stat(path).map_err(|e| err(path, e))?;
    let full = purgatorio::open(path, OREAD)
        .ok()
        .and_then(|fd| purgatorio::fd2path(fd.as_raw_fd()).ok())
        .unwrap_or_else(|| String::from(path));
    let mut m = entry(&d);
    m.extend([
        (String::from("path"), Value::from(full)),
        (String::from("atime"), Value::from(d.atime)),
        (String::from("uid"), Value::from(d.uid)),
        (String::from("gid"), Value::from(d.gid)),
        (String::from("muid"), Value::from(d.muid)),
    ]);
    Ok(Value::Object(m))
}

/// `resources/list`: the files under /tmp, the session's own, by name.
/// Other files are reached through the template.
pub fn list_resources(home: &str) -> Result<Value, String> {
    let mut out = Vec::new();
    let mut dirs = vec![String::from(home)];
    while let Some(dir) = dirs.pop() {
        let Ok(mut entries) = read_dir(&dir) else {
            continue;
        };
        entries.sort_by(|a, b| b.name.cmp(&a.name));
        for d in entries {
            let path = format!("{dir}/{}", d.name);
            if d.mode & DMDIR != 0 {
                dirs.push(path);
                continue;
            }
            if out.len() == MAX_RESOURCES {
                break;
            }
            out.push(obj([
                ("uri", uri_of(&path).into()),
                ("name", path.into()),
                ("size", d.length.into()),
            ]));
        }
    }
    out.sort_by(|a, b| {
        a.get("uri")
            .map(Value::encode)
            .cmp(&b.get("uri").map(Value::encode))
    });
    Ok(obj([("resources", Value::Array(out))]))
}

/// `resources/read`: a whole file as text or base64, or a directory as its
/// names. A file longer than [`MAX_RESOURCE`] is refused.
pub fn read_resource(path: &str) -> Result<Value, String> {
    let d = purgatorio::stat(path).map_err(|e| err(path, e))?;
    if d.mode & DMDIR != 0 {
        let mut names: Vec<String> = read_dir(path)?
            .into_iter()
            .map(|d| {
                let slash = if d.mode & DMDIR != 0 { "/" } else { "" };
                format!("{}{slash}\n", d.name)
            })
            .collect();
        names.sort();
        return Ok(obj([
            ("mimeType", "inode/directory".into()),
            ("text", names.concat().into()),
        ]));
    }
    let fd = purgatorio::open(path, OREAD).map_err(|e| err(path, e))?;
    let mut data = Vec::new();
    // Device files report length 0, so the length read is what counts.
    File::from(fd)
        .take(MAX_RESOURCE as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|e| err(path, Error::from(e)))?;
    if data.len() > MAX_RESOURCE {
        return Err(format!(
            "{path}: longer than {MAX_RESOURCE} bytes; read it in pieces with read_file"
        ));
    }
    Ok(match String::from_utf8(data) {
        Ok(text) => obj([("mimeType", "text/plain".into()), ("text", text.into())]),
        Err(e) => obj([
            ("mimeType", "application/octet-stream".into()),
            ("blob", base64_encode(e.as_bytes()).into()),
        ]),
    })
}

fn entry(d: &Dir) -> Vec<(String, Value)> {
    let dir = d.mode & DMDIR != 0;
    vec![
        (String::from("name"), Value::from(d.name.as_str())),
        (
            String::from("type"),
            Value::from(if dir { "dir" } else { "file" }),
        ),
        (String::from("size"), Value::from(d.length)),
        (
            String::from("mode"),
            Value::from(format!("{:o}", d.mode & 0o777)),
        ),
        (
            String::from("append_only"),
            Value::from(d.mode & DMAPPEND != 0),
        ),
        (String::from("mtime"), Value::from(d.mtime)),
    ]
}

pub fn read_dir(path: &str) -> Result<Vec<Dir>, String> {
    let d = purgatorio::stat(path).map_err(|e| err(path, e))?;
    if d.mode & DMDIR == 0 {
        return Err(err(path, "not a directory"));
    }
    let fd = purgatorio::open(path, OREAD).map_err(|e| err(path, e))?;
    let mut all = Vec::new();
    loop {
        let batch = purgatorio::dirread(fd.as_raw_fd()).map_err(|e| err(path, e))?;
        if batch.is_empty() {
            return Ok(all);
        }
        all.extend(batch);
    }
}

pub fn list_dir(path: &str) -> Result<Value, String> {
    let mut all = read_dir(path)?;
    all.sort_by(|a, b| a.name.cmp(&b.name));
    let truncated = all.len() > MAX_ENTRIES;
    all.truncate(MAX_ENTRIES);
    Ok(obj([
        (
            "entries",
            Value::Array(all.iter().map(|d| Value::Object(entry(d))).collect()),
        ),
        ("truncated", truncated.into()),
    ]))
}

pub fn remove(path: &str, recursive: bool, removed: &mut u64) -> Result<(), String> {
    if recursive && purgatorio::stat(path).is_ok_and(|d| d.mode & DMDIR != 0) {
        for d in read_dir(path)? {
            remove(&format!("{path}/{}", d.name), true, removed)?;
        }
    }
    purgatorio::remove(path).map_err(|e| err(path, e))?;
    *removed += 1;
    Ok(())
}

pub fn read_file(path: &str, args: &Value) -> Result<Value, String> {
    let offset = args
        .get("offset")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u64;
    let length = args
        .get("length")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_READ)
        .max(0) as usize;
    let force = args.get("encoding").and_then(Value::as_str) == Some("base64");
    let io = |e: std::io::Error| err(path, Error::from(e));
    let fd = purgatorio::open(path, OREAD).map_err(|e| err(path, e))?;
    let size = purgatorio::fstat(fd.as_raw_fd())
        .map_err(|e| err(path, e))
        .and_then(|d| {
            if d.mode & DMDIR != 0 {
                Err(err(path, "file is a directory"))
            } else {
                Ok(d.length)
            }
        })?;
    let mut f = File::from(fd);
    if offset > 0 {
        f.seek(SeekFrom::Start(offset)).map_err(io)?;
    }
    let mut buf = vec![0u8; length];
    let mut n = 0;
    let mut eof = false;
    while n < length {
        match f.read(&mut buf[n..]).map_err(io)? {
            0 => {
                eof = true;
                break;
            }
            k => n += k,
        }
    }
    buf.truncate(n);
    eof |= size > 0 && offset + n as u64 >= size;
    // A read cut in the middle of a character gives the whole characters;
    // the next read starts at the cut one.
    if !force
        && let Err(e) = std::str::from_utf8(&buf)
        && e.error_len().is_none()
        && e.valid_up_to() > 0
    {
        buf.truncate(e.valid_up_to());
        eof = false;
    }
    let bytes = buf.len();
    let (content, encoding) = match String::from_utf8(buf) {
        Ok(s) if !force => (s, "utf8"),
        Ok(s) => (base64_encode(s.as_bytes()), "base64"),
        Err(e) => (base64_encode(e.as_bytes()), "base64"),
    };
    Ok(obj([
        ("content", content.into()),
        ("encoding", encoding.into()),
        ("offset", offset.into()),
        ("bytes", bytes.into()),
        ("size", size.into()),
        ("eof", eof.into()),
    ]))
}

pub fn write_file(path: &str, args: &Value) -> Result<Value, String> {
    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
    let data = match args.get("encoding").and_then(Value::as_str) {
        Some("base64") => base64_decode(content).map_err(|e| format!("arguments.content: {e}"))?,
        _ => content.as_bytes().to_vec(),
    };
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("truncate");
    let exists = purgatorio::stat(path).is_ok();
    let io = |e: std::io::Error| err(path, Error::from(e));
    let mut f = if mode == "create" && exists {
        return Err(err(path, "file already exists"));
    } else if mode == "append" && exists {
        // Inferno has no append open mode; the offset is set by hand.
        let fd = purgatorio::open(path, OWRITE).map_err(|e| err(path, e))?;
        let mut f = File::from(fd);
        f.seek(SeekFrom::End(0)).map_err(io)?;
        f
    } else {
        File::from(purgatorio::create(path, OWRITE, 0o666).map_err(|e| err(path, e))?)
    };
    f.write_all(&data).map_err(io)?;
    let size = purgatorio::fstat(f.as_raw_fd()).map_or(0, |d| d.length);
    Ok(obj([("bytes", data.len().into()), ("size", size.into())]))
}
