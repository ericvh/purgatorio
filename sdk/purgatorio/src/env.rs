//! The prog's environment group, through the files in `/env` (`#e`). Unlike
//! `std::env`, which is a snapshot taken when the prog started, these see
//! changes made by other progs sharing the group.

use std::io::{Read, Write};

use crate::abi::mode;
use crate::{Error, create, open, remove};

fn path(name: &str) -> Result<String, Error> {
    if name.is_empty() || name.contains(['/', '\0']) || name == "." || name == ".." {
        return Err(Error::from(crate::abi::error::EBADARG));
    }
    Ok(format!("/env/{name}"))
}

/// The value of `name`, or `None` if it is unset.
pub fn get(name: &str) -> Option<String> {
    let fd = open(&path(name).ok()?, mode::OREAD).ok()?;
    let mut s = Vec::new();
    std::fs::File::from(fd).read_to_end(&mut s).ok()?;
    Some(String::from_utf8_lossy(&s).into_owned())
}

/// Sets `name` to `value`.
pub fn set(name: &str, value: &str) -> Result<(), Error> {
    let fd = create(&path(name)?, mode::OWRITE, 0o666)?;
    std::fs::File::from(fd).write_all(value.as_bytes())?;
    Ok(())
}

/// Removes `name`; it is not an error if it was unset.
pub fn unset(name: &str) -> Result<(), Error> {
    match remove(&path(name)?) {
        Err(e) if e.0 == crate::abi::error::ENONEXIST => Ok(()),
        r => r,
    }
}

/// Every variable, sorted by name.
pub fn list() -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/env")? {
        let name = e?.file_name().to_string_lossy().into_owned();
        if let Some(v) = get(&name) {
            out.push((name, v));
        }
    }
    out.sort();
    Ok(out)
}
