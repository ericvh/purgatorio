//! auth/secstore [-s dir] [-C m,t] [-g file | -G file | -p file | -r file]:
//! files encrypted under a password. The store is a directory (default
//! `/usr/<user>/secstore`), which may be an authenticated import of a
//! server's. Each file is sealed with ChaCha20-Poly1305 under an Argon2id
//! key from the password (one line on standard input), with its own salt.
//!
//! `-g` decrypts a stored file into the current directory, `-G` to
//! standard output, `-p` encrypts a local file into the store under its
//! base name, and `-r` removes one. With no option it lists the store.

use std::io::Write;

use authlib::Rng;
use authlib::fmt::login::{DEFAULT_M, DEFAULT_T, seal};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "auth/secstore [-s dir] [-C m,t] [-g file | -G file | -p file | -r file]";

enum Op {
    List,
    Get(String),
    Print(String),
    Put(String),
    Remove(String),
}

fn base(name: &str) -> Result<&str, Error> {
    let b = name.rsplit('/').next().unwrap_or(name);
    if b.is_empty() || b == "." || b == ".." {
        return Err(Error(format!("bad file name {name}")));
    }
    Ok(b)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut dir = format!("/usr/{}/secstore", authlib::user());
    let (mut m, mut t) = (DEFAULT_M, DEFAULT_T);
    let mut op = Op::List;
    while let Some(c) = arg.opt() {
        match c {
            's' => dir = arg.earg()?,
            'C' => (m, t) = authlib::cost(&arg.earg()?)?,
            'g' => op = Op::Get(arg.earg()?),
            'G' => op = Op::Print(arg.earg()?),
            'p' => op = Op::Put(arg.earg()?),
            'r' => op = Op::Remove(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    match op {
        Op::List => {
            let mut names: Vec<String> = std::fs::read_dir(&dir)
                .map_err(|e| Error(format!("{dir}: {}", Error::from(e))))?
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .collect();
            names.sort();
            for n in names {
                println!("{n}");
            }
            Ok(())
        }
        Op::Get(name) => {
            let data = open(&dir, &name)?;
            let b = base(&name)?;
            std::fs::write(b, data).map_err(|e| Error(format!("{b}: {}", Error::from(e))))
        }
        Op::Print(name) => {
            let data = open(&dir, &name)?;
            std::io::stdout().write_all(&data)?;
            Ok(())
        }
        Op::Put(file) => {
            let data =
                std::fs::read(&file).map_err(|e| Error(format!("{file}: {}", Error::from(e))))?;
            let password = authlib::password("password")?;
            let blob =
                seal::seal(password.as_bytes(), m, t, &data, &mut Rng).map_err(Error::from)?;
            if purgatorio::stat(&dir).is_err() {
                authlib::mkdirs_for(&format!("{dir}/x"))?;
            }
            let path = format!("{dir}/{}", base(&file)?);
            purgatorio::auth::write_secret(&path, &blob)
        }
        Op::Remove(name) => {
            let path = format!("{dir}/{}", base(&name)?);
            purgatorio::remove(&path).map_err(|e| Error(format!("{path}: {}", e)))
        }
    }
}

fn open(dir: &str, name: &str) -> Result<Vec<u8>, Error> {
    let path = format!("{dir}/{}", base(name)?);
    let blob = std::fs::read(&path).map_err(|e| Error(format!("{path}: {}", Error::from(e))))?;
    let password = authlib::password("password")?;
    let (data, _, _) = seal::open(password.as_bytes(), &blob)
        .map_err(|e| Error(format!("{name}: {}", Error::from(e))))?;
    Ok(data)
}
