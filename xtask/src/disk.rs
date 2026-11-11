//! `xtask diskimg`: purgfs images (`docs/fs.md`) built and checked on this
//! host with the kernel's own `purg-fs`, for its virtio-blk disk.

use std::fs::{File, OpenOptions};
use std::path::Path;

use purg_fs::file::{Copied, FileDisk, Stamp, add_tree, parse_size};
use purg_fs::layout::BSIZE;
use purg_fs::{Format, Fs, Mount, Report};

use crate::Result;

/// The owner of what an image holds: the kernel's one user.
pub const USER: &str = "inferno";

/// Makes `out` a new purgfs image of `size` bytes in `bsize` blocks,
/// holding a copy of directory `src` if given.
pub fn build(out: &Path, src: Option<&Path>, size: u64, bsize: usize) -> Result<Copied> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", out.display());
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)
        .map_err(|e| at(&e))?;
    file.set_len(size).map_err(|e| at(&e))?;
    let format = Format {
        bsize,
        user: String::from(USER),
        now: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32),
        ..Format::default()
    };
    let mut fs = Fs::format(FileDisk::new(file, size, 512), &format).map_err(|e| at(&e))?;
    let mut copied = Copied::default();
    if let Some(src) = src {
        let perm = |_: &Path, m: &std::fs::Metadata| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode()
        };
        let stamp = Stamp {
            user: USER,
            perm: &perm,
            mtime: None,
        };
        let root = fs.root();
        copied = add_tree(&mut fs, root, src, &stamp).map_err(|e| at(&e))?;
    }
    fs.sync().map_err(|e| at(&e))?;
    fs.into_disk().into_inner().sync_all().map_err(|e| at(&e))?;
    Ok(copied)
}

fn mount(img: &Path) -> Result<Fs<FileDisk<File>>> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", img.display());
    let file = File::open(img).map_err(|e| at(&e))?;
    let size = file.metadata().map_err(|e| at(&e))?.len();
    let mut disk = FileDisk::new(file, size, 512);
    disk.set_read_only(true);
    let m = Mount {
        read_only: true,
        ..Mount::default()
    };
    Fs::mount(disk, &m).map_err(|e| at(&e))
}

/// Checks image `img` without changing it.
pub fn check(img: &Path) -> Result<Report> {
    Ok(mount(img)?.check())
}

/// The contents of file `path` (from the root) in image `img`.
pub fn read(img: &Path, path: &str) -> Result<Vec<u8>> {
    let mut fs = mount(img)?;
    let at = |e: &dyn std::fmt::Display| format!("{}: {path}: {e}", img.display());
    let loc = fs.walk_path(path, USER).map_err(|e| at(&e))?;
    let len = fs.dentry(loc.me).map_err(|e| at(&e))?.length;
    let mut buf = vec![0u8; len as usize];
    let n = fs.read(loc.me, 0, &mut buf).map_err(|e| at(&e))?;
    buf.truncate(n);
    Ok(buf)
}

/// Whether `path` (from the root) is in image `img`.
pub fn exists(img: &Path, path: &str) -> Result<bool> {
    let mut fs = mount(img)?;
    Ok(fs.walk_path(path, USER).is_ok())
}

/// The disks a kernel run attaches: a purgfs image it works on (and a
/// second boot checks), and a small read-only disk throttled to a few
/// requests a second, which the kill test reads. Removed when dropped.
pub struct KernelDisks {
    dir: std::path::PathBuf,
    pub image: std::path::PathBuf,
    slow: std::path::PathBuf,
}

/// The file system image's size: small, so filling it is quick.
const KERNEL_DISK_SIZE: u64 = 2 << 20;
/// The slow disk's size and its throttle, in requests a second.
const SLOW_SIZE: usize = 64 << 10;
const SLOW_IOPS: u32 = 2;
/// Files in `many/`, enough for several directory blocks.
const MANY: usize = 300;

/// What a second boot reads back from `persist.txt`, and the first
/// leaves in `kept.txt` (see `purg-kernel`'s `system/disk.rs`).
pub const PERSIST: &str = "written by the first boot\n";
pub const KEPT: &str = "written by the kernel\n";

impl KernelDisks {
    pub fn create(ws: &Path, run: &str) -> Result<KernelDisks> {
        let name: String = run
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let dir = ws
            .join("target/kernel-disk")
            .join(format!("{}-{name}", std::process::id()));
        let io = |e: std::io::Error| format!("{}: {e}", dir.display());
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(io)?;
        }
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("sub")).map_err(io)?;
        std::fs::create_dir_all(src.join("many")).map_err(io)?;
        std::fs::write(src.join("hello.txt"), "hello from the disk image\n").map_err(io)?;
        std::fs::write(src.join("sub/inner.txt"), "inner file\n").map_err(io)?;
        for i in 0..MANY {
            std::fs::write(src.join(format!("many/f{i:03}")), "").map_err(io)?;
        }
        let image = dir.join("disk.img");
        build(&image, Some(&src), KERNEL_DISK_SIZE, BSIZE)?;
        // Each sector of the slow disk says which it is.
        let mut slow = vec![0u8; SLOW_SIZE];
        for (i, sector) in slow.chunks_mut(512).enumerate() {
            let tag = format!("slow sector {i}\n");
            sector[..tag.len()].copy_from_slice(tag.as_bytes());
        }
        let slow_path = dir.join("slow.img");
        std::fs::write(&slow_path, &slow).map_err(io)?;
        Ok(KernelDisks {
            dir,
            image,
            slow: slow_path,
        })
    }

    /// QEMU's arguments for the two disks.
    pub fn args(&self) -> Result<Vec<String>> {
        let path = |p: &Path| -> Result<String> {
            let s = p.to_str().ok_or("disk path is not UTF-8")?;
            if s.contains(',') {
                return Err(format!("{s}: QEMU options cannot hold a comma"));
            }
            Ok(String::from(s))
        };
        Ok(vec![
            // Version 2 transports, as for the other virtio devices.
            String::from("-global"),
            String::from("virtio-mmio.force-legacy=off"),
            String::from("-drive"),
            format!(
                "file={},if=none,id=disk,format=raw,cache=unsafe",
                path(&self.image)?
            ),
            String::from("-device"),
            String::from("virtio-blk-device,drive=disk,serial=disk"),
            String::from("-drive"),
            format!(
                "file={},if=none,id=slow,format=raw,readonly=on,throttling.iops-total={SLOW_IOPS}",
                path(&self.slow)?
            ),
            String::from("-device"),
            String::from("virtio-blk-device,drive=slow,serial=slow"),
        ])
    }

    /// Checks what the kernel left on the file system: consistent, with
    /// `persist.txt` and `kept.txt`, and without what it removed.
    pub fn check(&self) -> Result<()> {
        let r = check(&self.image)?;
        if !r.ok() {
            return Err(format!("the disk image does not check: {:?}", r.errors));
        }
        for (path, want) in [
            ("persist.txt", PERSIST),
            ("kept.txt", KEPT),
            ("hello.txt", "hello from the disk image\n"),
        ] {
            let got = read(&self.image, path)?;
            if got != want.as_bytes() {
                return Err(format!(
                    "{path} holds {:?}, not {want:?}",
                    String::from_utf8_lossy(&got)
                ));
            }
        }
        for gone in ["new.txt", "big.wasm", "fill", "renamed.txt"] {
            if exists(&self.image, gone)? {
                return Err(format!("{gone} is still on the disk"));
            }
        }
        if !exists(&self.image, "newdir")? {
            return Err(String::from("newdir was not made"));
        }
        Ok(())
    }
}

impl Drop for KernelDisks {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const USAGE: &str =
    "usage: cargo xtask diskimg [--size SIZE] [--bsize N] OUT [DIR] | diskimg --check IMAGE";

/// `xtask diskimg`.
pub fn cmd(args: &[String]) -> Result<()> {
    let (mut size, mut bsize, mut check_only) = (64u64 << 20, BSIZE, false);
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--size" => {
                size = it
                    .next()
                    .and_then(|s| parse_size(s))
                    .ok_or_else(|| String::from(USAGE))?
            }
            "--bsize" => {
                bsize = it
                    .next()
                    .and_then(|s| parse_size(s))
                    .ok_or_else(|| String::from(USAGE))? as usize
            }
            "--check" => check_only = true,
            _ => rest.push(a.clone()),
        }
    }
    if check_only {
        let [img] = rest.as_slice() else {
            return Err(String::from(USAGE));
        };
        let r = check(Path::new(img))?;
        for e in &r.errors {
            eprintln!("{img}: {e}");
        }
        eprintln!(
            "{img}: {} files, {} directories, {} blocks used, {} free",
            r.files, r.dirs, r.used, r.free
        );
        return if r.ok() {
            Ok(())
        } else {
            Err(format!("{img}: {} errors", r.errors.len()))
        };
    }
    let (out, src) = match rest.as_slice() {
        [out] => (out, None),
        [out, src] => (out, Some(Path::new(src))),
        _ => return Err(String::from(USAGE)),
    };
    let c = build(Path::new(out), src, size, bsize)?;
    eprintln!(
        "wrote {out} ({size} bytes): {} files, {} directories, {} bytes of data",
        c.files, c.dirs, c.bytes
    );
    Ok(())
}
