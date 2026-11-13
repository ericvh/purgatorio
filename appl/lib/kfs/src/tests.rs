use super::*;

fn image(blocks: usize, bsize: usize) -> Fs<Vec<u8>> {
    Fs::ream(vec![0u8; blocks * bsize], bsize, 1000).unwrap()
}

fn report(fs: &mut Fs<Vec<u8>>) -> Vec<String> {
    check(fs, check_flags("q").unwrap())
}

/// A quiet check of a good file system prints only the free list lines.
fn assert_clean(fs: &mut Fs<Vec<u8>>) {
    let r = report(fs);
    assert_eq!(r.len(), 2, "{r:#?}");
    assert_eq!(r[0], "check free list");
    assert!(r[1].starts_with("lo = "), "{r:#?}");
}

fn file(name: &str, mode: u16) -> Dentry {
    Dentry {
        name: String::from(name),
        mode,
        atime: 5,
        mtime: 5,
        ..Dentry::default()
    }
}

#[test]
fn layout_on_disk() {
    let fs = image(64, 1024);
    let img = &fs.dev;
    assert_eq!(&img[256..256 + 21], b"kfs wren device\n1024\n");
    assert!(img[256 + 21..1024].iter().all(|&b| b == 0));
    let sb = &img[1024..2048];
    // The tag: pad, Tsuper, QPSUPER.
    assert_eq!(&sb[1016..], &[0, 0, 1, 0, 2, 0, 0, 0]);
    assert_eq!(get4(sb, 0), 1, "fstart");
    assert_eq!(get4(sb, 4), 64, "fsize");
    assert_eq!(get4(sb, 8), 61, "tfree: blocks 3 to 63");
    assert_eq!(get4(sb, 12), 10, "qidgen");
    // The free list head: nfree, then free[0] = 0 (the end), 63, 62, ...
    assert_eq!(get4(sb, 32), 62);
    assert_eq!(get4(sb, 36), 0);
    assert_eq!(get4(sb, 40), 63);
    assert_eq!(get4(sb, 36 + 4 * 61), 3);
    let root = &img[2048..3072];
    assert_eq!(&root[1016..], &[0, 0, 2, 0, 1, 0, 0, 0]);
    assert_eq!(&root[..2], b"/\0");
    assert_eq!(get2(root, 28), 0xffff, "uid -1");
    assert_eq!(get2(root, 32), 0xc1ff, "DALLOC|DDIR|0777");
    assert_eq!(get4(root, 36), 0x8000_0001, "path with QPDIR");
    assert_eq!(get4(root, 80), 1000, "atime");
    assert_eq!(get4(root, 84), 1000, "mtime");
}

#[test]
fn geometry() {
    let fs = image(16, 1024);
    assert_eq!(fs.bufsize(), 1016);
    assert_eq!(fs.dirperbuf(), 11);
    assert_eq!(fs.indperbuf(), 254);
    assert_eq!(fs.feperbuf(), 245);
    let fs = image(16, 4096);
    assert_eq!(fs.dirperbuf(), 46);
    assert_eq!(fs.feperbuf(), 1013);
    assert!(Fs::ream(vec![0u8; 8192], 1000, 0).is_err());
    assert!(Fs::ream(vec![0u8; 3 * 1024], 1024, 0).is_err());
}

#[test]
fn dentry_round_trip() {
    let d = Dentry {
        name: String::from("a-name-of-27-bytes-exactly!"),
        uid: 10006,
        gid: -1,
        mode: DALLOC | DAPND | 0o640,
        muid: 3,
        path: 77,
        vers: 9,
        size: 123_456,
        dblock: [1, 2, 3, 4, 5, 6],
        iblock: 7,
        diblock: 8,
        atime: 11,
        mtime: 12,
    };
    let mut b = [0u8; DENTRYSIZE];
    d.pack(&mut b);
    assert_eq!(b[27], 0, "NUL-terminated");
    assert_eq!(get2(&b, 34), 3, "muid after mode");
    assert_eq!(get4(&b, 48), 1, "dblock[0]");
    assert_eq!(get4(&b, 72), 7, "iblock");
    assert_eq!(get4(&b, 76), 8, "diblock");
    assert_eq!(Dentry::unpack(&b), d);
}

#[test]
fn reopen() {
    let fs = image(64, 2048);
    let mut fs = Fs::open(fs.dev, false).unwrap();
    assert_eq!(fs.bsize, 2048);
    assert_eq!(fs.superblock().tfree, 61);
    assert!(fs.set_fsok(true).is_ok());
    let fs = Fs::open(fs.dev, true).unwrap();
    assert_eq!(fs.superblock().fsok, 1);
    assert!(Fs::open(vec![0u8; 65536], false).is_err());
}

#[test]
fn directories_grow_and_files_are_found() {
    let mut fs = image(64, 1024);
    let mut made = Vec::new();
    for i in 0..30 {
        let (loc, d) = fs.create(ROOT, &file(&format!("f{i}"), 0o644)).unwrap();
        assert_eq!(d.path, 11 + i);
        made.push(loc);
    }
    assert_eq!(fs.create(ROOT, &file("f3", 0o644)).unwrap_err(), EEXIST);
    let root = fs.getd(ROOT, QPROOT).unwrap();
    // 1 + 30 entries over 11 per block.
    assert_ne!(root.dblock[1], 0);
    assert_ne!(root.dblock[2], 0);
    assert_eq!(root.dblock[3], 0);
    let names: Vec<String> = fs
        .entries(&root)
        .unwrap()
        .into_iter()
        .map(|(_, d)| d.name)
        .collect();
    assert_eq!(names.len(), 30);
    let (loc, _) = fs.lookup(&root, "f17").unwrap().unwrap();
    assert_eq!(loc, made[17]);
    fs.remove(loc).unwrap();
    assert!(fs.lookup(&root, "f17").unwrap().is_none());
    // The freed slot is reused.
    let (again, _) = fs.create(ROOT, &file("new", 0o644)).unwrap();
    assert_eq!(again, made[17]);
    assert_clean(&mut fs);
}

#[test]
fn a_directory_must_be_empty_to_go() {
    let mut fs = image(64, 1024);
    let (dloc, _) = fs.create(ROOT, &file("d", DDIR | 0o755)).unwrap();
    let (floc, _) = fs.create(dloc, &file("x", 0o644)).unwrap();
    assert_eq!(fs.remove(dloc).unwrap_err(), EEMPTY);
    fs.remove(floc).unwrap();
    fs.remove(dloc).unwrap();
    assert_clean(&mut fs);
    // The root keeps the block its entries were in.
    assert_eq!(fs.superblock().tfree, 60);
}

#[test]
fn big_files_use_both_indirect_blocks() {
    let mut fs = image(1024, 1024);
    let free = fs.superblock().tfree;
    let (loc, d) = fs.create(ROOT, &file("big", 0o644)).unwrap();
    // Direct (6) and single indirect (254) blocks hold 260 * 1016 bytes.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 + i / 1016) as u8).collect();
    for chunk in data.chunks(8192).enumerate() {
        let off = (chunk.0 * 8192) as u64;
        let n = fs.write(loc, d.path, off, chunk.1, 3, 9).unwrap();
        assert_eq!(n, chunk.1.len());
    }
    let d = fs.getd(loc, d.path).unwrap();
    assert_eq!(d.size, 300_000);
    assert_eq!(d.muid, 3);
    assert_ne!(d.iblock, 0);
    assert_ne!(d.diblock, 0);
    assert_eq!(fs.read(&d, 0, 400_000).unwrap(), data);
    assert_eq!(fs.read(&d, 299_990, 100).unwrap(), &data[299_990..]);
    // 296 data blocks, an indirect block, a double indirect block and one
    // block it points to.
    assert_eq!(fs.superblock().tfree, free - 296 - 3 - 1);
    assert_clean(&mut fs);
    let mut d = d;
    fs.trunc(&mut d).unwrap();
    fs.putd(loc, &d).unwrap();
    assert_eq!(fs.superblock().tfree, free - 1);
    assert_clean(&mut fs);
}

#[test]
fn holes_read_as_zeros() {
    let mut fs = image(64, 1024);
    let (loc, d) = fs.create(ROOT, &file("sparse", 0o644)).unwrap();
    fs.write(loc, d.path, 5000, b"end", 0, 1).unwrap();
    let d = fs.getd(loc, d.path).unwrap();
    assert_eq!(d.size, 5003);
    let r = fs.read(&d, 0, 6000).unwrap();
    assert!(r[..5000].iter().all(|&b| b == 0));
    assert_eq!(&r[5000..], b"end");
    assert_eq!(d.dblock.iter().filter(|&&a| a != 0).count(), 1);
}

#[test]
fn append_only_files_write_at_the_end() {
    let mut fs = image(64, 1024);
    let (loc, d) = fs.create(ROOT, &file("log", DAPND | 0o644)).unwrap();
    fs.write(loc, d.path, 0, b"one\n", 0, 1).unwrap();
    fs.write(loc, d.path, 0, b"two\n", 0, 1).unwrap();
    let d = fs.getd(loc, d.path).unwrap();
    assert_eq!(fs.read(&d, 0, 100).unwrap(), b"one\ntwo\n");
}

#[test]
fn the_free_list_spans_blocks_and_runs_out() {
    // 2048 blocks need nine free list blocks of 245 entries.
    let mut fs = image(2048, 1024);
    assert_clean(&mut fs);
    let free = fs.superblock().tfree;
    assert_eq!(free, 2045);
    let mut got = Vec::new();
    loop {
        match fs.balloc(TFILE, 99) {
            Ok(a) => got.push(a),
            Err(e) => {
                assert_eq!(e, EFULL);
                break;
            }
        }
    }
    assert_eq!(got.len() as u32, free);
    got.sort_unstable();
    got.dedup();
    assert_eq!(got.len() as u32, free);
    assert_eq!(got[0], 3);
    assert_eq!(fs.superblock().tfree, 0);
    let list: Vec<(u32, u32)> = got.iter().map(|&a| (a, 0)).collect();
    fs.bfree(&list).unwrap();
    assert_eq!(fs.superblock().tfree, free);
    assert_clean(&mut fs);
}

#[test]
fn full_disks_say_so() {
    let mut fs = image(16, 1024);
    let (loc, d) = fs.create(ROOT, &file("f", 0o644)).unwrap();
    let e = fs.write(loc, d.path, 0, &[1u8; 20_000], 0, 1).unwrap_err();
    assert_eq!(e, EFULL);
    // What was written is kept, and nothing leaked.
    let d = fs.getd(loc, d.path).unwrap();
    assert_eq!(d.size, 20_000);
    assert_clean(&mut fs);
}

#[test]
fn check_reports_and_repairs() {
    let mut fs = image(128, 1024);
    let (aloc, a) = fs.create(ROOT, &file("a", 0o644)).unwrap();
    fs.write(aloc, a.path, 0, &[7u8; 3000], 0, 1).unwrap();
    let (bloc, b) = fs.create(ROOT, &file("b", 0o644)).unwrap();
    fs.write(bloc, b.path, 0, b"bee", 0, 1).unwrap();
    let full = check(&mut fs, CheckFlags::default());
    assert_eq!(full[0], "checking file system: main");
    assert!(full.contains(&String::from("       3 files")), "{full:#?}");
    assert!(full.contains(&String::from("     127 blocks in the file system")));
    assert!(
        full.contains(&String::from("       7 used blocks")),
        "{full:#?}"
    );
    assert!(
        full.contains(&String::from("     120 free blocks")),
        "{full:#?}"
    );
    assert!(full.contains(&String::from("      12 maximum qid path")));

    // b's block is also a's: a duplicate, and one block missing.
    let mut bd = fs.getd(bloc, b.path).unwrap();
    let lost = bd.dblock[0];
    bd.dblock[0] = fs.getd(aloc, a.path).unwrap().dblock[1];
    fs.putd(bloc, &bd).unwrap();
    let r = report(&mut fs);
    assert!(
        r.iter().any(|l| l.starts_with("check: \"/b\": dup ")),
        "{r:#?}"
    );
    assert!(
        r.contains(&String::from("       1 missing blocks")),
        "{r:#?}"
    );
    assert!(r.contains(&String::from("       1 address duplications")));
    assert!(r.contains(&format!("missing: {lost}")), "{r:#?}");

    // An address out of range is dropped with `d`.
    bd.dblock[0] = 5000;
    fs.putd(bloc, &bd).unwrap();
    let r = check(&mut fs, check_flags("dq").unwrap());
    assert!(
        r.contains(&String::from("       1 bad block addresses")),
        "{r:#?}"
    );
    assert_eq!(fs.getd(bloc, b.path).unwrap().dblock[0], 0);

    // `f` puts the lost block back on the free list.
    let r = check(&mut fs, check_flags("fq").unwrap());
    assert!(r.is_empty(), "{r:#?}");
    assert_clean(&mut fs);

    // A bad tag is reported, and fixed with `t`.
    let ad = fs.getd(aloc, a.path).unwrap();
    let mut blk = fs.read_block(ad.dblock[0]).unwrap();
    set_tag(&mut blk, 1024, TDIR, 4);
    fs.write_block(ad.dblock[0], &blk).unwrap();
    let r = check(&mut fs, check_flags("rq").unwrap());
    assert!(
        r.contains(&String::from("check: \"/a\": xtag: checktag")),
        "{r:#?}"
    );
    let r = check(&mut fs, check_flags("rtq").unwrap());
    assert!(
        r.contains(&String::from("file system was modified")),
        "{r:#?}"
    );
    assert_clean(&mut fs);
    assert_eq!(fs.read(&ad, 0, 3000).unwrap(), vec![7u8; 3000]);
}

#[test]
fn damaged_images_do_not_panic() {
    let mut fs = image(64, 1024);
    let (loc, d) = fs.create(ROOT, &file("d", DDIR | 0o755)).unwrap();
    for i in 0..5 {
        let (l, f) = fs.create(loc, &file(&format!("x{i}"), 0o644)).unwrap();
        fs.write(l, f.path, 0, &[i as u8; 2500], 0, 1).unwrap();
    }
    let _ = d;
    let good = fs.dev.clone();
    let mut seed = 12345u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for _ in 0..300 {
        let mut img = good.clone();
        for _ in 0..8 {
            let at = next() as usize % (12 * 1024);
            img[1024 + at] = next() as u8;
        }
        let Ok(mut fs) = Fs::open(img, false) else {
            continue;
        };
        let _ = check(&mut fs, CheckFlags::default());
        if let Ok(root) = fs.getd(ROOT, QPROOT)
            && let Ok(es) = fs.entries(&root)
        {
            for (l, e) in es {
                let _ = fs.read(&e, 0, 10_000);
                if e.is_dir() {
                    let _ = fs.entries(&e);
                }
                let _ = fs.write(l, e.path, 100, b"x", 0, 1);
                let _ = fs.remove(l);
            }
        }
        let _ = fs.create(ROOT, &file("new", 0o644));
    }
}

#[test]
fn users_files() {
    let u = Users::default();
    assert_eq!(u.uid("adm"), -1);
    assert_eq!(u.uid("inferno"), 10006);
    assert_eq!(u.uid("nobody-here"), NONE);
    assert_eq!(u.name(10002), "bootes");
    assert_eq!(u.name(42), "#42");
    assert!(u.leads(10001, 10001));
    let u = Users::parse(
        "-1:adm:adm:\n0:none:adm:\n9999:noworld::\n10000:sys::glenda\n\
         # a comment\n100:glenda:glenda:\n101:dev:glenda:glenda,ken\n102:ken::\n",
    )
    .unwrap();
    assert_eq!(u.uid("glenda"), 100);
    assert!(u.in_group(100, 101));
    assert!(u.in_group(100, 10000));
    assert!(u.in_group(102, 101), "members may come later in the file");
    assert!(u.leads(100, 101));
    assert!(!u.leads(102, 101));
    assert!(Users::parse("x:glenda::\n").is_err());
    assert!(Users::parse("01:glenda::\n").is_err());
    assert!(Users::parse("\n").is_err());
}
