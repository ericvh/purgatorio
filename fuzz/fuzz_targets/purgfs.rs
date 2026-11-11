#![no_main]

//! A purgfs image: the mount, the checker and the operations must fail
//! cleanly on any bytes, never panic (`crates/purg-fs/tests/fs.rs` runs
//! the same on damaged images without libFuzzer).

use purg_fs::layout::DMDIR;
use purg_fs::mem::MemDisk;
use purg_fs::{Fs, Mount};

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let mut img = data.to_vec();
    img.resize(img.len().next_multiple_of(512).max(512), 0);
    let Ok(mut fs) = Fs::mount(MemDisk::from_image(img, 512), &Mount::default()) else {
        return;
    };
    let _ = fs.check();
    let root = fs.root();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs.readdir(dir) else { continue };
        for (loc, d) in entries.into_iter().take(64) {
            if d.mode & DMDIR != 0 && stack.len() < 64 {
                stack.push(loc);
            } else {
                let mut buf = [0u8; 1024];
                let _ = fs.read(loc.me, 0, &mut buf);
                let _ = fs.write(loc.me, 10, b"x", "inferno");
            }
        }
    }
    let _ = fs.create(root, "new", 0o664, "inferno");
});
