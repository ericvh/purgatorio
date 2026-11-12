//! install/create [-uvxo] [-p proto] [-r root] [-s source] [-N uid]
//! [-G gid] [-d desc] [-t time] name: make a package of the files a
//! prototype selects from `source` (default `/`), as create(8).
//!
//! The package is a disk/mkfs archive that starts with the wrap header
//! `/wrap/name/stamp/` (`package`, `desc`, `proto` and `md5sum`; see the
//! `wrap` library). It is written to a file named by the stamp (the time,
//! or `-t time`) in the current directory, or with `-o` to standard
//! output; `-x` lists the files (name, time and length) instead. `-v`
//! names each file on standard error as it goes in. `-N` and `-G` set the
//! owner and group of everything archived (default: what the prototype
//! says, else the file's own; `inferno` for the wrap header).
//!
//! With `-u`, the package is an update of the one installed in
//! `root/wrap/name` (default root `/`): only files whose MD5 differs from
//! the installed one go in, the files the installed package has that the
//! source has lost are listed in `remove`, and `update` holds the stamp of
//! the base package. The prototype defaults to the installed one. `-U`
//! (the hybrid package) is not provided.

use std::io::Write;

use proto::{Level, Namespace};
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Error};
use wrap::{Hdr, put_end, put_hdr, put_wrap, quote};

const USAGE: &str = "install/create [-uvxo] [-p proto] [-r root] [-s source] [-N uid] [-G gid] [-d description] [-t time] name";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut update, mut verbose, mut list, mut stdout) = (false, false, false, false);
    let (mut proto_file, mut root, mut source) = (None, String::from("/"), String::from("/"));
    let (mut uid, mut gid, mut desc, mut time) = (None, None, String::from("inferno"), None);
    while let Some(c) = arg.opt() {
        match c {
            'u' => update = true,
            'v' => verbose = true,
            'x' => list = true,
            'o' => stdout = true,
            'p' => proto_file = Some(arg.earg()?),
            'r' => root = arg.earg()?,
            's' => source = arg.earg()?,
            'N' => uid = Some(arg.earg()?),
            'G' => gid = Some(arg.earg()?),
            'd' => desc = arg.earg()?,
            't' => {
                let t = arg.earg()?;
                time = Some(t.parse::<u32>().map_err(|_| arg.usage())?);
            }
            _ => return Err(arg.usage()),
        }
    }
    let [name] = arg
        .argv()
        .try_into()
        .map_err(|_| Error::from(format!("usage: {USAGE}")))?;
    if name.is_empty() || name.contains('/') {
        return Err(Error::from(format!("bad package name {}", quote(&name))));
    }
    let now = time.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32)
    });
    let installed = if update {
        let u = wrap::units(&proto::mkpath(&root, &format!("wrap/{name}")));
        if u.is_empty() {
            return Err(Error::from(format!("no package {name} in {root}")));
        }
        u
    } else {
        Vec::new()
    };
    let proto_path = match (proto_file, installed.last()) {
        (Some(p), _) => p,
        (None, Some(u)) => proto::mkpath(&root, &format!("wrap/{name}/{}/proto", u.stamp)),
        (None, None) => return Err(Error::from("proto file missing")),
    };
    let text = std::fs::read_to_string(&proto_path)
        .map_err(|e| Error(format!("{proto_path}: {}", Error::from(e))))?;
    let mut files = Vec::new();
    let mut failed = false;
    proto::read(
        &text,
        &source,
        &Namespace,
        &mut |level, line, msg| {
            eprintln!("install/create: {proto_path}:{line}: {msg}");
            failed |= level == Level::Error;
        },
        &mut |f| files.push(f.clone()),
    );
    if failed {
        purgatorio::exits("error");
    }
    if list {
        for f in &files {
            println!("{}\t{}\t{}", quote(&f.new), f.dir.mtime, f.dir.length);
        }
        return Ok(());
    }
    // Each file's contents and digest; an update keeps the changed ones.
    let mut chosen = Vec::new();
    let mut md5 = Vec::new();
    for f in files {
        let isdir = f.dir.mode & DMDIR != 0;
        let data = if isdir {
            Vec::new()
        } else {
            match std::fs::read(&f.old) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!(
                        "install/create: can't read {}: {}",
                        quote(&f.old),
                        Error::from(e)
                    );
                    failed = true;
                    continue;
                }
            }
        };
        if !isdir {
            let digest = wrap::hex(&wrap::md5(&data));
            if update && wrap::installed_digest(&installed, &f.new) == Some(digest.as_str()) {
                md5.push((f.new.clone(), digest));
                continue;
            }
            md5.push((f.new.clone(), digest));
        } else if update && installed.iter().any(|u| u.digest(&f.new).is_some()) {
            continue;
        }
        chosen.push((f, data));
    }
    if failed {
        purgatorio::exits("error");
    }
    md5.sort();
    let mut remove = Vec::new();
    if update {
        let have: std::collections::HashSet<&str> = md5.iter().map(|(n, _)| n.as_str()).collect();
        let mut gone: Vec<String> = Vec::new();
        for u in installed.iter().rev() {
            for (n, _) in &u.md5 {
                if !have.contains(n.as_str())
                    && wrap::installed_digest(&installed, n).is_some()
                    && !gone.contains(n)
                {
                    gone.push(n.clone());
                }
            }
        }
        gone.sort();
        remove = gone;
    }
    let mut md5text = String::new();
    for (n, d) in &md5 {
        md5text.push_str(&format!("{} {d}\n", quote(n)));
    }
    let mut wrapfiles: Vec<(&str, Vec<u8>)> = vec![
        ("proto", text.into_bytes()),
        ("md5sum", md5text.into_bytes()),
    ];
    if update {
        let mut r = String::new();
        for n in &remove {
            r.push_str(n);
            r.push('\n');
        }
        wrapfiles.push(("remove", r.into_bytes()));
    }
    let base = installed.iter().rev().find(|u| u.full).map(|u| u.stamp);
    let wuid = uid.clone().unwrap_or_else(|| String::from("inferno"));
    let wgid = gid.clone().unwrap_or_else(|| String::from("inferno"));
    let mut out: Vec<u8> = Vec::new();
    put_wrap(&mut out, &name, now, &desc, base, &wuid, &wgid, &wrapfiles)?;
    for (f, data) in &chosen {
        let isdir = f.dir.mode & DMDIR != 0;
        let h = Hdr {
            name: f.new.clone(),
            mode: f.dir.mode,
            uid: uid
                .clone()
                .or_else(|| f.uid.clone())
                .unwrap_or_else(|| f.dir.uid.clone()),
            gid: gid
                .clone()
                .or_else(|| f.gid.clone())
                .unwrap_or_else(|| f.dir.gid.clone()),
            mtime: f.dir.mtime,
            length: if isdir { 0 } else { data.len() as u64 },
        };
        if verbose {
            eprintln!("{}", quote(&f.new));
        }
        put_hdr(&mut out, &h)?;
        out.extend_from_slice(data);
    }
    put_end(&mut out)?;
    if stdout {
        std::io::stdout().write_all(&out)?;
        return Ok(());
    }
    let file = wrap::stamp(now);
    println!("archiving package {name} to {file}");
    let fd = purgatorio::create(&file, purgatorio::abi::mode::OWRITE, 0o664)?;
    std::fs::File::from(fd).write_all(&out)?;
    Ok(())
}
