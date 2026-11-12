use super::format::format;
use super::*;

const NOW: u32 = 1_709_288_430; // 2024-03-01 10:20:30

fn names(v: &mut Vol<Vec<u8>>, d: DirRef) -> Vec<String> {
    let mut n: Vec<String> = v.list(d).unwrap().into_iter().map(|e| e.name).collect();
    n.sort();
    n
}

fn exercise(kind: Kind, sectors: u32) -> Vec<u8> {
    let img = format(kind, sectors).unwrap();
    let mut v = Vol::open(img, 0).unwrap();
    assert_eq!(v.kind, kind);
    let root = v.root();
    assert!(v.list(root).unwrap().is_empty());
    let a = v.create(root, "readme.txt", 0, NOW).unwrap();
    assert_eq!(&a.short, b"README  TXT");
    let long = v
        .create(root, "A Long Mixed-Case Name.text", 0, NOW)
        .unwrap();
    assert_eq!(&long.short, b"ALONGM~1TEX");
    let dir = v.create(root, "Sub Dir", ATTR_DIR, NOW).unwrap();
    assert!(dir.is_dir());
    assert_eq!(v.create(root, "README.TXT", 0, NOW).unwrap_err(), EEXIST);
    assert_eq!(v.create(root, "a:b", 0, NOW).unwrap_err(), ESYNTAX);
    // Data across several clusters, and a write past the end.
    let cb = v.cluster_bytes() as usize;
    let data: Vec<u8> = (0..cb * 3 + 100).map(|i| (i % 251) as u8).collect();
    v.write(a.loc, 0, &data, NOW).unwrap();
    v.write(a.loc, data.len() as u64 + 10, b"tail", NOW)
        .unwrap();
    let a = v.get_ent(a.loc).unwrap();
    assert_eq!(a.size as usize, data.len() + 14);
    let back = v.read(&a, 0, 1 << 20).unwrap();
    assert_eq!(&back[..data.len()], &data[..]);
    assert_eq!(&back[data.len()..], b"\0\0\0\0\0\0\0\0\0\0tail");
    assert_eq!(v.read(&a, 5, 3).unwrap(), &data[5..8]);
    assert_eq!(a.mtime, NOW);
    // A directory with enough entries to need more than one cluster.
    let sub = dir.as_dir();
    for i in 0..(cb / 32 + 5) {
        v.create(sub, &format!("file number {i}"), 0, NOW).unwrap();
    }
    let x = v.create(sub, "x", 0, NOW).unwrap();
    v.write(x.loc, 0, b"in the sub dir", NOW).unwrap();
    assert_eq!(v.list(sub).unwrap().len(), cb / 32 + 6);
    assert_eq!(v.remove(&dir).unwrap_err(), EPERM);
    // Look-ups ignore case; renames keep the data.
    let found = v
        .lookup(root, "a long MIXED-case name.TEXT")
        .unwrap()
        .unwrap();
    assert_eq!(found.loc, long.loc);
    let r = v.rename(&long, "renamed").unwrap();
    assert_eq!(&r.short, b"RENAMED    ");
    let r = v.rename(&r, "Renamed Again.Long").unwrap();
    assert_eq!(r.name, "Renamed Again.Long");
    assert_eq!(v.rename(&r, "readme.txt").unwrap_err(), EEXIST);
    let dir2 = v.rename(&dir, "subdir").unwrap();
    assert!(dir2.is_dir());
    let x = v.lookup(dir2.as_dir(), "x").unwrap().unwrap();
    assert_eq!(v.read(&x, 0, 100).unwrap(), b"in the sub dir");
    assert_eq!(
        names(&mut v, root),
        ["Renamed Again.Long", "readme.txt", "subdir"]
    );
    // Removing frees the clusters.
    let free = v.free();
    let a = v.get_ent(a.loc).unwrap();
    v.remove(&a).unwrap();
    assert_eq!(v.free(), free + (data.len() + 14).div_ceil(cb) as u32);
    let b = v.create(root, "b", 0, NOW).unwrap();
    v.write(b.loc, 0, b"bee", NOW).unwrap();
    v.truncate(b.loc, NOW).unwrap();
    assert_eq!(v.get_ent(b.loc).unwrap().size, 0);
    v.set_attr(b.loc, ATTR_RO, ATTR_RO).unwrap();
    assert!(v.get_ent(b.loc).unwrap().readonly());
    v.set_mtime(b.loc, 315_532_800 + 86400).unwrap();
    assert_eq!(v.get_ent(b.loc).unwrap().mtime, 315_532_800 + 86400);
    v.flush().unwrap();
    // It all reads back from the bytes alone.
    let img = v.disk;
    let mut w = Vol::open(img.clone(), 0).unwrap();
    assert_eq!(names(&mut w, root), ["Renamed Again.Long", "b", "subdir"]);
    img
}

/// Checks an image with the host's fsck.fat, when it has one.
fn fsck(img: &[u8], tag: &str) {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../target");
    if !std::path::Path::new(dir).is_dir() {
        return;
    }
    let path = format!("{dir}/fat-{tag}.img");
    std::fs::write(&path, img).unwrap();
    let Ok(out) = std::process::Command::new("fsck.fat")
        .args(["-n", &path])
        .output()
    else {
        return;
    };
    assert!(
        out.status.success(),
        "{tag}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn fat12() {
    let img = exercise(Kind::Fat12, 2880);
    fsck(&img, "12");
}

#[test]
fn fat16() {
    let img = exercise(Kind::Fat16, 9000);
    fsck(&img, "16");
}

#[test]
fn fat32() {
    let img = exercise(Kind::Fat32, 4096);
    fsck(&img, "32");
}

#[test]
fn full() {
    let mut v = Vol::open(format(Kind::Fat12, 64).unwrap(), 0).unwrap();
    let root = v.root();
    let f = v.create(root, "big", 0, NOW).unwrap();
    let free = v.free();
    assert!(free > 0);
    let all = vec![1u8; free as usize * 512];
    v.write(f.loc, 0, &all, NOW).unwrap();
    assert_eq!(v.free(), 0);
    assert_eq!(
        v.write(f.loc, all.len() as u64, b"x", NOW).unwrap_err(),
        EFULL
    );
    let g = v.create(root, "g", 0, NOW).unwrap();
    assert_eq!(v.write(g.loc, 0, &[0; 600], NOW).unwrap_err(), EFULL);
    assert_eq!(v.get_ent(g.loc).unwrap().size, 0);
    // The fixed root fills up.
    let mut n = 0;
    let err = loop {
        match v.create(root, &format!("{n}"), 0, NOW) {
            Ok(_) => n += 1,
            Err(e) => break e,
        }
    };
    assert_eq!(err, EFULL);
    assert_eq!(n, 62);
}

#[test]
fn damage() {
    assert_eq!(Vol::open(vec![0u8; 4096], 0).err().unwrap(), EFORMAT);
    let img = format(Kind::Fat16, 9000).unwrap();
    for at in [11, 13, 14, 16, 22] {
        let mut bad = img.clone();
        bad[at] = 0;
        bad[at + 1] = 0;
        assert!(Vol::open(bad, 0).is_err(), "{at}");
    }
    // A FAT chain that loops is an error, not a hang.
    let mut v = Vol::open(img, 0).unwrap();
    let root = v.root();
    let f = v.create(root, "f", 0, NOW).unwrap();
    v.write(f.loc, 0, &[1; 3000], NOW).unwrap();
    let f = v.get_ent(f.loc).unwrap();
    let c = v.chain(f.cluster).unwrap();
    v.set(c[2], c[0]);
    assert!(v.chain(f.cluster).is_err());
    assert!(v.read(&f, 0, 3000).is_err());
    assert!(format(Kind::Fat16, 1000).is_err());
}

#[test]
fn times() {
    for t in [315_532_800, NOW, 4_000_000_000] {
        let (d, tm) = to_dos(t);
        assert_eq!(from_dos(d, tm), t);
    }
    assert_eq!(from_dos(0, 0), 0);
    assert_eq!(to_dos(0), to_dos(315_532_800));
}

/// Images made by the host's mkfs.fat, when it has one, are read and
/// written, and still pass its fsck.fat.
#[test]
fn foreign() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../target");
    if !std::path::Path::new(dir).is_dir() {
        return;
    }
    for (bits, kb) in [("12", "1440"), ("16", "16384"), ("32", "40000")] {
        let path = format!("{dir}/mkfs-fat{bits}.img");
        let _ = std::fs::remove_file(&path);
        let made = std::process::Command::new("mkfs.fat")
            .args(["-C", "-F", bits, &path, kb])
            .output();
        if !made.is_ok_and(|o| o.status.success()) {
            return;
        }
        let mut v = Vol::open(std::fs::read(&path).unwrap(), 0).unwrap();
        let root = v.root();
        let d = v.create(root, "Some Directory", ATTR_DIR, NOW).unwrap();
        let f = v.create(d.as_dir(), "a file.txt", 0, NOW).unwrap();
        v.write(f.loc, 0, &vec![9u8; 70000], NOW).unwrap();
        let f = v.get_ent(f.loc).unwrap();
        assert_eq!(v.read(&f, 69990, 100).unwrap(), [9u8; 10]);
        fsck(&v.disk, &format!("mkfs{bits}"));
    }
}
