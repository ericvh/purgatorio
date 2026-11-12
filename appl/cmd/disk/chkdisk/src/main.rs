//! disk/chkdisk [-v] image: check the purgfs file system in `image` (an
//! image file or a disk's `data` file) without changing it. Each problem
//! is printed on a line of its own, after the image's name; the exit
//! status is `errors` if there were any. The check reads through a
//! pending journal, as the next mount would see the disk. `-v` adds a
//! summary: files, directories, blocks used and free, and the journal
//! transaction a mount would replay.

use std::fs::File;

use purg_fs::file::FileDisk;
use purg_fs::{Fs, Mount};
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "disk/chkdisk [-v] image";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let mut verbose = false;
    while let Some(c) = arg.opt() {
        match c {
            'v' => verbose = true,
            _ => return Err(arg.usage()),
        }
    }
    let [image] = arg
        .argv()
        .try_into()
        .map_err(|_| Error::from(format!("usage: {USAGE}")))?;
    let at = |e: String| Error::from(format!("{image}: {e}"));
    let file = File::open(&image).map_err(|e| at(Error::from(e).0))?;
    let size = file
        .metadata()
        .map(|m| m.len())
        .map_err(|e| at(Error::from(e).0))?;
    let mut disk = FileDisk::new(file, size, 512);
    disk.set_read_only(true);
    let mount = Mount {
        read_only: true,
        ..Mount::default()
    };
    let mut fs = Fs::mount(disk, &mount).map_err(|e| at(e.into_owned()))?;
    let r = fs.check();
    for e in &r.errors {
        println!("{image}: {e}");
    }
    if verbose {
        println!(
            "{image}: {} files, {} directories, {} blocks used, {} free",
            r.files, r.dirs, r.used, r.free
        );
        if let Some(seq) = r.replayed {
            println!("{image}: journal transaction {seq} to replay");
        }
    }
    if !r.ok() {
        purgatorio::exits("errors");
    }
    Ok(())
}
