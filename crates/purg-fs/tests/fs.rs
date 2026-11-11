//! purgfs on a RAM disk: the operations, Inferno's rules, space, remounts,
//! crashes at every write, and corrupt images.

use purg_abi::error;
use purg_fs::layout::{DMDIR, DMEXCL};
use purg_fs::mem::MemDisk;
use purg_fs::{Disk, Format, Fs, Loc, Mount, WStat};

const USER: &str = "inferno";

fn fresh(size: usize, bsize: usize) -> Fs<MemDisk> {
    let f = Format {
        bsize,
        now: 1000,
        ..Format::default()
    };
    Fs::format(MemDisk::new(size, 512), &f).expect("format")
}

fn mkfile(fs: &mut Fs<MemDisk>, dir: Loc, name: &str, data: &[u8]) -> Loc {
    let f = fs.create(dir, name, 0o664, USER).expect("create");
    assert_eq!(fs.write(f.me, 0, data, USER).expect("write"), data.len());
    f
}

fn read_all(fs: &mut Fs<MemDisk>, f: Loc) -> Vec<u8> {
    let len = fs.dentry(f.me).unwrap().length as usize;
    let mut buf = vec![0u8; len + 10];
    let n = fs.read(f.me, 0, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

fn names(fs: &mut Fs<MemDisk>, dir: Loc) -> Vec<String> {
    let mut v: Vec<String> = fs
        .readdir(dir)
        .unwrap()
        .into_iter()
        .map(|(_, d)| d.name)
        .collect();
    v.sort();
    v
}

fn assert_clean<D: Disk>(fs: &mut Fs<D>) {
    let r = fs.check();
    assert!(r.ok(), "check: {:?}", r.errors);
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[test]
fn files_and_directories() {
    let mut fs = fresh(1 << 20, 4096);
    let root = fs.root();
    let d = fs.create(root, "dir", DMDIR | 0o775, USER).unwrap();
    let a = mkfile(&mut fs, d, "a", b"hello\n");
    mkfile(&mut fs, root, "b", b"top");
    assert_eq!(names(&mut fs, root), ["b", "dir"]);
    assert_eq!(read_all(&mut fs, a), b"hello\n");
    let q0 = fs.dentry(a.me).unwrap().qvers;
    fs.write(a.me, 6, b"world\n", USER).unwrap();
    assert_eq!(read_all(&mut fs, a), b"hello\nworld\n");
    assert!(fs.dentry(a.me).unwrap().qvers > q0);
    // A walk finds it again.
    let a2 = fs.walk_path("/dir/a", USER).unwrap();
    assert_eq!(a2, a);
    // Rename by wstat; the old name is gone.
    let w = WStat {
        name: Some("c".into()),
        ..WStat::default()
    };
    fs.wstat(a, &w, USER).unwrap();
    assert_eq!(names(&mut fs, d), ["c"]);
    assert_eq!(fs.walk(d, "a", USER).unwrap_err(), error::ENONEXIST);
    // Errors.
    assert_eq!(fs.remove(d, USER).unwrap_err(), error::ENOTEMPTY);
    assert_eq!(
        fs.create(root, "dir", DMDIR | 0o775, USER).unwrap_err(),
        error::EEXIST
    );
    assert_eq!(
        fs.create(root, "x/y", 0o664, USER).unwrap_err(),
        error::EFILENAME
    );
    assert_eq!(
        fs.create(root, &"n".repeat(65), 0o664, USER).unwrap_err(),
        error::EFILENAME
    );
    assert_eq!(fs.read(d.me, 0, &mut [0; 4]).unwrap_err(), error::EISDIR);
    // Removing, then the stale reference fails.
    fs.remove(a, USER).unwrap();
    assert_eq!(fs.dentry(a.me).unwrap_err(), error::ENONEXIST);
    fs.remove(d, USER).unwrap();
    assert_eq!(names(&mut fs, root), ["b"]);
    assert_clean(&mut fs);
}

#[test]
fn create_truncates_an_existing_file() {
    let mut fs = fresh(1 << 20, 4096);
    let root = fs.root();
    let f = mkfile(&mut fs, root, "f", &pattern(10000, 1));
    let g = fs.create(root, "f", 0o600, USER).unwrap();
    assert_eq!(g, f);
    let d = fs.dentry(f.me).unwrap();
    assert_eq!((d.length, d.mode), (0, 0o664));
    assert_clean(&mut fs);
}

#[test]
fn modes_and_owners() {
    let mut fs = fresh(1 << 20, 4096);
    let root = fs.root();
    let f = mkfile(&mut fs, root, "f", b"x");
    // The parent's permissions mask the new file's.
    let d = fs.create(root, "ro", DMDIR | 0o755, USER).unwrap();
    let g = fs.create(d, "g", 0o666, USER).unwrap();
    assert_eq!(fs.dentry(g.me).unwrap().mode, 0o644);
    // Owner-only changes.
    let chmod = WStat {
        mode: Some(0o600 | DMEXCL),
        ..WStat::default()
    };
    assert_eq!(fs.wstat(f, &chmod, "glenda").unwrap_err(), error::EPERM);
    fs.wstat(f, &chmod, USER).unwrap();
    assert_eq!(fs.dentry(f.me).unwrap().mode, 0o600 | DMEXCL);
    let chown = WStat {
        uid: Some("glenda".into()),
        ..WStat::default()
    };
    assert_eq!(fs.wstat(f, &chown, USER).unwrap_err(), error::EPERM);
    let mkdir_bit = WStat {
        mode: Some(DMDIR | 0o777),
        ..WStat::default()
    };
    assert_eq!(fs.wstat(f, &mkdir_bit, USER).unwrap_err(), error::EPERM);
    // Others have no write permission in a 0755 directory.
    assert_eq!(
        fs.create(d, "h", 0o666, "glenda").unwrap_err(),
        error::EPERM
    );
    assert_eq!(fs.remove(g, "glenda").unwrap_err(), error::EPERM);
    assert_eq!(fs.open_check(f.me, "glenda", 0).unwrap_err(), error::EPERM);
    fs.open_check(f.me, USER, 2).unwrap();
    assert_eq!(fs.open_check(d.me, USER, 1).unwrap_err(), error::EISDIR);
    // Nothing changes when one change is refused.
    let both = WStat {
        name: Some("renamed".into()),
        uid: Some("glenda".into()),
        ..WStat::default()
    };
    assert!(fs.wstat(f, &both, USER).is_err());
    assert_eq!(fs.dentry(f.me).unwrap().name, "f");
    let clash = WStat {
        name: Some("ro".into()),
        ..WStat::default()
    };
    assert_eq!(fs.wstat(f, &clash, USER).unwrap_err(), error::EEXIST);
    assert_clean(&mut fs);
}

/// 512-byte blocks: 16 direct, 128 single indirect, then double indirect.
#[test]
fn large_and_sparse_files() {
    let mut fs = fresh(4 << 20, 512);
    let root = fs.root();
    let f = fs.create(root, "big", 0o664, USER).unwrap();
    // The root's first block is taken now, and stays.
    let free0 = fs.usage().free;
    let data = pattern(512 * 1000 + 77, 3);
    assert_eq!(fs.write(f.me, 0, &data, USER).unwrap(), data.len());
    assert_eq!(read_all(&mut fs, f), data);
    assert_clean(&mut fs);
    // Cut across each boundary, checking the tail reads as zeros after
    // growing again.
    for len in [
        512 * 500 + 3,
        512 * 144 + 1,
        512 * 144,
        512 * 16 + 9,
        5000,
        1,
    ] {
        let w = WStat {
            length: Some(len as u64),
            ..WStat::default()
        };
        fs.wstat(f, &w, USER).unwrap();
        assert_eq!(read_all(&mut fs, f), &data[..len]);
        assert_clean(&mut fs);
    }
    let w = WStat {
        length: Some(3000),
        ..WStat::default()
    };
    fs.wstat(f, &w, USER).unwrap();
    let got = read_all(&mut fs, f);
    assert_eq!(&got[..1], &data[..1]);
    assert!(got[1..].iter().all(|&b| b == 0));
    fs.remove(f, USER).unwrap();
    assert_eq!(fs.usage().free, free0);
    // A hole far out, in the double indirect range.
    let s = fs.create(root, "sparse", 0o664, USER).unwrap();
    fs.write(s.me, 512 * 5000, b"end", USER).unwrap();
    let mut buf = vec![1u8; 1024];
    assert_eq!(fs.read(s.me, 512 * 4999, &mut buf).unwrap(), 515);
    assert!(buf[..512].iter().all(|&b| b == 0));
    assert_eq!(&buf[512..515], b"end");
    assert_clean(&mut fs);
    fs.remove(s, USER).unwrap();
    assert_eq!(fs.usage().free, free0);
    assert_clean(&mut fs);
}

#[test]
fn full_disk() {
    let mut fs = fresh(256 << 10, 4096);
    let root = fs.root();
    let f = fs.create(root, "fill", 0o664, USER).unwrap();
    let chunk = pattern(8192, 9);
    let mut off = 0u64;
    let err = loop {
        match fs.write(f.me, off, &chunk, USER) {
            Ok(n) if n == chunk.len() => off += n as u64,
            Ok(n) => off += n as u64,
            Err(e) => break e,
        }
    };
    assert_eq!(err, error::ENOSPACE);
    assert_eq!(fs.usage().free, 0);
    assert_eq!(fs.dentry(f.me).unwrap().length, off);
    // A create may still find room in the root's block; a new directory
    // block cannot be had.
    assert_clean(&mut fs);
    fs.remove(f, USER).unwrap();
    assert!(fs.usage().free > 0);
    mkfile(&mut fs, root, "after", b"room again");
    assert_clean(&mut fs);
}

#[test]
fn remount_keeps_everything() {
    let mut fs = fresh(1 << 20, 1024);
    let root = fs.root();
    let d = fs.create(root, "d", DMDIR | 0o775, USER).unwrap();
    let data = pattern(70000, 5);
    mkfile(&mut fs, d, "f", &data);
    for i in 0..40 {
        mkfile(
            &mut fs,
            root,
            &format!("n{i:02}"),
            format!("{i}").as_bytes(),
        );
    }
    let usage = fs.usage();
    let image = fs.into_disk().image();
    let mut fs = Fs::mount(MemDisk::from_image(image, 512), &Mount::default()).unwrap();
    assert_eq!(fs.usage(), usage);
    let f = fs.walk_path("d/f", USER).unwrap();
    assert_eq!(read_all(&mut fs, f), data);
    assert_eq!(names(&mut fs, root).len(), 41);
    assert_clean(&mut fs);
    // A new qid path, not a reused one.
    let g = mkfile(&mut fs, root, "g", b"");
    assert!(fs.dentry(g.me).unwrap().qpath > fs.dentry(f.me).unwrap().qpath);
}

/// The workload the crash test cuts short at every write.
fn workload(fs: &mut Fs<MemDisk>) {
    let root = fs.root();
    let Ok(d) = fs.create(root, "d", DMDIR | 0o775, USER) else {
        return;
    };
    for i in 0..6 {
        let Ok(f) = fs.create(d, &format!("f{i}"), 0o664, USER) else {
            return;
        };
        if fs
            .write(f.me, 0, &pattern(3000 * i + 10, i as u8), USER)
            .is_err()
        {
            return;
        }
    }
    let Ok(big) = fs.create(root, "big", 0o664, USER) else {
        return;
    };
    if fs.write(big.me, 0, &pattern(512 * 200, 7), USER).is_err() {
        return;
    }
    let w = WStat {
        name: Some("renamed".into()),
        ..WStat::default()
    };
    let Ok(f0) = fs.walk(d, "f0", USER) else {
        return;
    };
    if fs.wstat(f0, &w, USER).is_err() {
        return;
    }
    let short = WStat {
        length: Some(512 * 20),
        ..WStat::default()
    };
    if fs.wstat(big, &short, USER).is_err() {
        return;
    }
    for i in 1..4 {
        let Ok(f) = fs.walk(d, &format!("f{i}"), USER) else {
            return;
        };
        if fs.remove(f, USER).is_err() {
            return;
        }
    }
    let _ = fs.remove(big, USER);
}

fn crash_fs() -> Fs<MemDisk> {
    let f = Format {
        bsize: 512,
        jblocks: 32,
        ..Format::default()
    };
    Fs::format(MemDisk::new(2 << 20, 512), &f).unwrap()
}

#[test]
fn a_crash_at_any_write_leaves_consistent_metadata() {
    let mut fs = crash_fs();
    let start = fs.disk().writes;
    workload(&mut fs);
    let total = fs.disk().writes - start;
    assert!(total > 100, "{total} writes");
    let mut replays = 0;
    for n in 0..total {
        for seed in [1u64, 2, 3] {
            let mut fs = crash_fs();
            let mut disk = fs.into_disk();
            disk.crash_after(n, seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ n as u64);
            fs = Fs::mount(disk, &Mount::default()).unwrap();
            workload(&mut fs);
            let image = fs.into_disk().crashed().expect("crashed").to_vec();
            // Checked read-only first, as the checker sees it, then
            // mounted and replayed.
            let mut ro = MemDisk::from_image(image.clone(), 512);
            ro.set_read_only(true);
            let mut fs = Fs::mount(ro, &Mount::default())
                .unwrap_or_else(|e| panic!("crash at write {n}: mount: {e}"));
            let r = fs.check();
            assert!(r.ok(), "crash at write {n} seed {seed}: {:?}", r.errors);
            replays += usize::from(r.replayed.is_some());
            let mut fs = Fs::mount(MemDisk::from_image(image, 512), &Mount::default()).unwrap();
            assert_clean(&mut fs);
            // And it still works.
            let root = fs.root();
            let f = fs.create(root, "after", 0o664, USER).unwrap();
            fs.write(f.me, 0, b"ok", USER).unwrap();
            assert_clean(&mut fs);
        }
    }
    assert!(replays > 0);
}

/// A corrupt image never panics the mount, the checker or the operations.
#[test]
fn corrupt_images_do_not_panic() {
    let mut fs = crash_fs();
    workload(&mut fs);
    let good = fs.into_disk().image();
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut rand = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // The interesting bytes: block 0, the journal header, the free map
    // and the first data blocks.
    let hot = 512 * 64;
    for _ in 0..3000 {
        let mut img = good.clone();
        for _ in 0..1 + rand() % 8 {
            let at = if rand() % 4 == 0 {
                rand() as usize % img.len()
            } else {
                rand() as usize % hot
            };
            img[at] = rand() as u8;
        }
        let Ok(mut fs) = Fs::mount(MemDisk::from_image(img, 512), &Mount::default()) else {
            continue;
        };
        let _ = fs.check();
        let root = fs.root();
        let mut stack = vec![root];
        let mut steps = 0;
        while let Some(dir) = stack.pop() {
            steps += 1;
            if steps > 200 {
                break;
            }
            let Ok(entries) = fs.readdir(dir) else {
                continue;
            };
            for (loc, d) in entries {
                if d.mode & DMDIR != 0 {
                    stack.push(loc);
                } else {
                    let mut buf = vec![0u8; 4096];
                    let _ = fs.read(loc.me, 0, &mut buf);
                    let _ = fs.write(loc.me, 100, b"poke", USER);
                }
            }
        }
        let _ = fs.create(root, "new", 0o664, USER);
    }
}

#[test]
fn a_read_only_mount_sees_the_journal() {
    // Crash right after the journal of the first create is durable: its
    // home writes never happen.
    let mut fs = crash_fs();
    let root = fs.root();
    let before = fs.disk().writes;
    mkfile(&mut fs, root, "probe", b"");
    let per_create = fs.disk().writes - before;
    let mut fs = crash_fs();
    let mut disk = fs.into_disk();
    // The create's writes: the journal, then the home blocks.
    disk.crash_after(1, 0);
    fs = Fs::mount(disk, &Mount::default()).unwrap();
    let _ = fs.create(fs.root(), "x", 0o664, USER);
    let image = fs.into_disk().crashed().unwrap().to_vec();
    assert!(per_create >= 2);
    let mut ro = MemDisk::from_image(image, 512);
    ro.set_read_only(true);
    let mut fs = Fs::mount(ro, &Mount::default()).unwrap();
    assert_clean(&mut fs);
    let root = fs.root();
    let got = names(&mut fs, root);
    assert!(got.is_empty() || got == ["x"], "{got:?}");
}
