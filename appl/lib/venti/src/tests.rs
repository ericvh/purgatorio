//! Known answers from the formats in Plan 9's venti(6) and vac(1) and
//! Inferno's venti(2), and round trips through a [`MemStore`].

use super::vac::*;
use super::*;
use purgatorio::{DMAPPEND, DMDIR, Dir, Qid};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    let s: String = s.split_whitespace().collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn scores() {
    // The score of the empty block, libventi's vtzeroscore.
    assert_eq!(
        Score::zero().to_string(),
        "da39a3ee5e6b4b0d3255bfef95601890afd80709"
    );
    assert_eq!(Score::of(b""), Score::zero());
    // FIPS 180-4's examples.
    assert_eq!(
        Score::of(b"abc").to_string(),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(
        Score::of(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq").to_string(),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    let mut s = sha1::Sha1::new();
    for _ in 0..1000 {
        s.update(&[b'a'; 1000]);
    }
    assert_eq!(hex(&s.finish()), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    let a = Score::of(b"abc");
    assert_eq!(Score::parse(&a.to_string()), Some(a));
    assert_eq!(
        Score::parse("A9993E364706816ABA3E25717850C26C9CD0D89D"),
        Some(a)
    );
    assert_eq!(Score::parse("a9993e"), None);
    assert_eq!(parse_tagged(&format!("vac:{a}"), "vac"), Ok(a));
    assert_eq!(parse_tagged(&a.to_string(), "vac"), Ok(a));
    assert_eq!(
        parse_tagged(&format!("fossil:{a}"), "vac"),
        Err(String::from("bad score type: fossil"))
    );
    assert_eq!(
        parse_tagged("vac:xyz", "vac"),
        Err(String::from("bad score: xyz"))
    );
}

#[test]
fn messages() {
    // size[2] Thello tag[1] version[s] uid[s] strength[1] crypto[n] codec[n]
    let m = Msg::Thello {
        version: String::from("02"),
        uid: String::from("anonymous"),
        strength: 0,
        crypto: vec![],
        codec: vec![],
    };
    let w = m.pack(0).unwrap();
    assert_eq!(
        hex(&w),
        "0014 04 00 0002 3032 0009 616e6f6e796d6f7573 00 00 00".replace(' ', "")
    );
    assert_eq!(Msg::unpack(&w[2..]).unwrap(), (0, m));
    // size[2] Tread tag[1] score[20] type[1] pad[1] count[2]
    let m = Msg::Tread {
        score: Score::zero(),
        btype: DATATYPE,
        count: 8192,
    };
    let w = m.pack(7).unwrap();
    assert_eq!(hex(&w), format!("001a0c07{}0d002000", Score::zero()));
    assert_eq!(Msg::unpack(&w[2..]).unwrap(), (7, m));
    // Version 04's four-byte count.
    let mut b = w[2..w.len() - 2].to_vec();
    b.extend_from_slice(&[0, 1, 0, 0]);
    assert!(matches!(
        Msg::unpack(&b).unwrap().1,
        Msg::Tread { count: 65536, .. }
    ));
    // size[2] Twrite tag[1] type[1] pad[3] data[]
    let m = Msg::Twrite {
        btype: ROOTTYPE,
        data: b"xy".to_vec(),
    };
    assert_eq!(
        hex(&m.pack(1).unwrap()),
        "00080e010100000078 79".replace(' ', "")
    );
    for m in [
        Msg::Rerror(String::from("no")),
        Msg::Tping,
        Msg::Rping,
        Msg::Rhello {
            sid: String::from("anonymous"),
            crypto: 0,
            codec: 0,
        },
        Msg::Tgoodbye,
        Msg::Rread(b"data".to_vec()),
        Msg::Rwrite(Score::of(b"x")),
        Msg::Tsync,
        Msg::Rsync,
    ] {
        let w = m.pack(3).unwrap();
        assert_eq!(usize::from(u16::from_be_bytes([w[0], w[1]])), w.len() - 2);
        assert_eq!(w[2], m.mtype());
        assert_eq!(Msg::unpack(&w[2..]).unwrap(), (3, m));
    }
    assert_eq!(hex(&Msg::Tsync.pack(9).unwrap()), "00021009");
    assert!(Msg::unpack(&[99, 0]).is_err());
    assert!(Msg::unpack(&[TSYNC, 0, 1]).is_err());
}

#[test]
fn root_and_entry_layout() {
    // version[2] name[128] type[128] score[20] blocksize[2] prev[20]
    let r = Root {
        name: String::from("vac"),
        rtype: String::from("vac"),
        score: Score::of(b"abc"),
        blocksize: 8192,
        prev: None,
    };
    let d = r.pack().unwrap();
    let mut want = vec![0u8; ROOTSIZE];
    want[0..2].copy_from_slice(&[0, 2]);
    want[2..5].copy_from_slice(b"vac");
    want[130..133].copy_from_slice(b"vac");
    want[258..278].copy_from_slice(&Score::of(b"abc").0);
    want[278..280].copy_from_slice(&[0x20, 0x00]);
    assert_eq!(d.to_vec(), want);
    assert_eq!(Root::unpack(&d).unwrap(), r);
    // Plan 9's big form of a block size: (n>>5)<<(n&31).
    want[0] = 0x80;
    want[278..280].copy_from_slice(&((2u16 << 5) | 15).to_be_bytes());
    assert_eq!(Root::unpack(&want).unwrap().blocksize, 65536);

    // generation[4] psize[2] dsize[2] flags[1] zero[5] size[6] score[20]; the
    // depth is in bits 2-4 of the flags.
    let e = Entry {
        generation: 1,
        psize: 8180,
        dsize: 8160,
        depth: 2,
        flags: ENTRYACTIVE | ENTRYDIR,
        size: 0x0102_0304_0506,
        score: Score::of(b"abc"),
    };
    assert_eq!(
        hex(&e.pack()),
        format!(
            "00000001 1ff4 1fe0 0b 0000000000 010203040506 {}",
            Score::of(b"abc")
        )
        .replace(' ', "")
    );
    assert_eq!(Entry::unpack(&e.pack()).unwrap(), e);
    // An old archive's empty file has block sizes of 0.
    let mut z = Entry {
        psize: 0,
        dsize: 0,
        depth: 0,
        flags: ENTRYACTIVE,
        size: 0,
        score: Score::zero(),
        ..e
    }
    .pack();
    assert_eq!(Entry::unpack(&z).unwrap().dsize, 4096);
    z[20] ^= 1;
    assert!(Entry::unpack(&z).is_err());
}

#[test]
fn zero_truncation() {
    assert_eq!(zero_truncate(DATATYPE, b"ab\0c\0\0"), b"ab\0c");
    assert_eq!(zero_truncate(DIRTYPE, &[0; 40]), b"");
    let mut p = Score::of(b"a").0.to_vec();
    p.extend_from_slice(&Score::zero().0);
    p.extend_from_slice(&[1, 2]);
    assert_eq!(zero_truncate(POINTERTYPE0, &p), &Score::of(b"a").0);
    assert_eq!(zero_truncate(ROOTTYPE, &[0; 300]).len(), 300);
}

fn direntry(elem: &str, mode: u32) -> Direntry {
    Direntry {
        elem: String::from(elem),
        entry: 4,
        mentry: 5,
        qid: 0x11,
        uid: String::from("u"),
        gid: String::from("g"),
        mid: String::from("m"),
        mtime: 0x6000_0000,
        mode,
        ..Direntry::default()
    }
}

#[test]
fn direntry_layout() {
    let de = Direntry {
        qidspace: Some((0, 9)),
        ..direntry("ab", 0o644)
    };
    let p = de.pack();
    assert_eq!(
        hex(&p),
        concat!(
            "1c4d9072 0009 0002 6162",                      // magic, version, elem
            "00000004 00000000 00000005 00000000",          // entry generation mentry mgen
            "0000000000000011",                             // qid
            "0001 75 0001 67 0001 6d",                      // uid gid mid
            "60000000 00000000 00000000 00000000 000001a4", // times, mode
            "03 0010 0000000000000000 0000000000000009",    // qid space
        )
        .replace(' ', "")
    );
    assert_eq!(Direntry::unpack(&p).unwrap(), de);
    // Version 8 has no gens and mentry is entry+1 (vac.b, vdunpack).
    let mut v8 = unhex("1c4d9072 0008 0001 78 00000007 0000000000000002 0000 0000 0000");
    v8.extend_from_slice(&[0; 20]);
    let d8 = Direntry::unpack(&v8).unwrap();
    assert_eq!((d8.elem.as_str(), d8.entry, d8.mentry), ("x", 7, 8));
    assert!(Direntry::unpack(&v8[..10]).is_err());

    let d = Direntry::from_dir(&Dir {
        name: String::from("d"),
        mode: DMDIR | DMAPPEND | DMTMP | 0o750,
        qid: Qid {
            path: 3,
            ..Qid::default()
        },
        ..Dir::default()
    });
    assert_eq!(d.mode, MODEDIR | MODEAPPEND | MODETEMP | 0o750);
    let back = d.to_dir();
    assert_eq!(back.mode, DMDIR | DMAPPEND | DMTMP | 0o750);
    assert_eq!(back.qid.qtype, 0x80 | 0x40 | 0x04);
}

#[test]
fn meta_order_and_plan9_blocks() {
    use std::cmp::Ordering::*;
    assert_eq!(elem_cmp(b"ab", b"a", false), Less);
    assert_eq!(elem_cmp(b"ab", b"a", true), Greater);
    assert_eq!(elem_cmp(b"a", b"b", false), Less);

    // A block laid out as Plan 9's vac lays it out: room for ten index
    // entries, one in use, the record after the index and free space.
    let rec = direntry("f", 0o444).pack();
    let off = METAHEADERSIZE + 10 * METAINDEXSIZE;
    let mut b = vec![0u8; 512];
    b[0..4].copy_from_slice(&METAMAGIC.to_be_bytes());
    b[4..6].copy_from_slice(&((off + rec.len()) as u16).to_be_bytes());
    b[8..10].copy_from_slice(&10u16.to_be_bytes());
    b[10..12].copy_from_slice(&1u16.to_be_bytes());
    b[12..14].copy_from_slice(&(off as u16).to_be_bytes());
    b[14..16].copy_from_slice(&(rec.len() as u16).to_be_bytes());
    b[off..off + rec.len()].copy_from_slice(&rec);
    let mb = MetaBlock::parse(&b).unwrap();
    assert_eq!(mb.direntry(0).unwrap().elem, "f");
    assert_eq!(mb.search("f").unwrap().unwrap().elem, "f");
    assert!(mb.search("g").unwrap().is_none());
    b[12..14].copy_from_slice(&4u16.to_be_bytes());
    assert!(MetaBlock::parse(&b).unwrap().direntry(0).is_err());
}

/// Writes the files `(path, contents)` (a trailing `/` makes a
/// directory) as vacput does, with fixed metadata.
fn archive(v: &mut MemStore, bsize: u32, files: &[(&str, Vec<u8>)]) -> Score {
    fn dir(
        v: &mut MemStore,
        bsize: u32,
        prefix: &str,
        files: &[(&str, Vec<u8>)],
    ) -> (Entry, Entry) {
        let mut ds = DirSink::new(bsize);
        let mut ms = MetaSink::new(bsize);
        for (p, data) in files {
            let Some(rest) = p.strip_prefix(prefix) else {
                continue;
            };
            if rest.is_empty() || rest.trim_end_matches('/').contains('/') {
                continue;
            }
            let name = rest.trim_end_matches('/');
            let mut de = direntry(name, 0o644);
            if rest.ends_with('/') {
                let (e, me) = dir(v, bsize, p, files);
                de.mode = MODEDIR | 0o755;
                de.entry = ds.add(&e);
                de.mentry = ds.add(&me);
            } else {
                let mut w = StreamWriter::new(DATATYPE, psize(bsize), bsize);
                for c in data.chunks(bsize as usize) {
                    w.write(v, c).unwrap();
                }
                if data.is_empty() {
                    w.write(v, &[]).unwrap();
                }
                de.entry = ds.add(&w.finish(v).unwrap());
                de.mentry = 0;
            }
            ms.add(de);
        }
        (ds.finish(v).unwrap(), ms.finish(v).unwrap())
    }
    let (e0, e1) = dir(v, bsize, "", files);
    let top = Direntry {
        qidspace: Some((0, 100)),
        ..direntry("vac", MODEDIR | 0o777)
    };
    write_root(v, "vac", bsize, &e0, &e1, top).unwrap()
}

fn read_all(v: &mut MemStore, e: Entry) -> Vec<u8> {
    let mut s = Source::new(e);
    let mut out = Vec::new();
    loop {
        let b = s.read_at(v, out.len() as u64, 1000).unwrap();
        if b.is_empty() {
            return out;
        }
        out.extend(b);
    }
}

#[test]
fn round_trip() {
    let mut v = MemStore::new();
    let big: Vec<u8> = (0..300_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
    let mut sparse = vec![0u8; 5000];
    sparse[4999] = 1;
    let mut files: Vec<(String, Vec<u8>)> = vec![
        (String::from("a"), b"hello\n".to_vec()),
        (String::from("ab"), b"x".to_vec()),
        (String::from("empty"), vec![]),
        (String::from("zeros"), vec![0; 3000]),
        (String::from("sparse"), sparse.clone()),
        (String::from("big"), big.clone()),
        (String::from("d/"), vec![]),
        (String::from("d/inner"), b"in d\n".to_vec()),
        (String::from("d/e/"), vec![]),
    ];
    // Enough files to need several meta blocks and directory blocks.
    for i in 0..60 {
        files.push((
            format!("many/{i:03}-file-with-a-long-name"),
            vec![i as u8; i],
        ));
    }
    files.push((String::from("many/"), vec![]));
    let refs: Vec<(&str, Vec<u8>)> = files.iter().map(|(p, d)| (p.as_str(), d.clone())).collect();
    // A small block size makes deep trees: 25 scores to a pointer block.
    let score = archive(&mut v, 512, &refs);
    let (mut root, de, r) = vac::vdroot(&mut v, &score).unwrap();
    assert_eq!((r.name.as_str(), r.blocksize), ("vac", 512));
    assert_eq!(de.elem, "vac");
    assert_eq!(de.qidspace, Some((0, 100)));
    let names: Vec<String> = root
        .list(&mut v)
        .unwrap()
        .into_iter()
        .map(|d| d.elem)
        .collect();
    // vac's order: a name after the longer names it begins.
    assert_eq!(
        names,
        ["ab", "a", "big", "d", "empty", "many", "sparse", "zeros"]
    );
    for (name, want) in [
        ("a", b"hello\n".to_vec()),
        ("ab", b"x".to_vec()),
        ("empty", vec![]),
        ("zeros", vec![0; 3000]),
        ("sparse", sparse),
        ("big", big),
    ] {
        let de = root.walk(&mut v, name).unwrap().unwrap();
        let (e, me) = root.open(&mut v, &de).unwrap();
        assert!(me.is_none());
        assert_eq!(e.size as usize, want.len(), "{name}");
        assert_eq!(read_all(&mut v, e.clone()), want, "{name}");
        // A read in the middle that crosses blocks.
        if want.len() > 2000 {
            let mut s = Source::new(e);
            assert_eq!(s.read_at(&mut v, 1000, 600).unwrap(), want[1000..1600]);
        }
    }
    assert!(root.walk(&mut v, "nope").unwrap().is_none());
    let d = root.walk(&mut v, "d").unwrap().unwrap();
    let mut dd = root.child(&mut v, &d).unwrap();
    let names: Vec<String> = dd
        .list(&mut v)
        .unwrap()
        .into_iter()
        .map(|d| d.elem)
        .collect();
    assert_eq!(names, ["e", "inner"]);
    let e = dd.walk(&mut v, "e").unwrap().unwrap();
    assert!(
        dd.child(&mut v, &e)
            .unwrap()
            .list(&mut v)
            .unwrap()
            .is_empty()
    );
    let m = root.walk(&mut v, "many").unwrap().unwrap();
    let mut md = root.child(&mut v, &m).unwrap();
    assert!(md.meta.nblocks() > 1);
    let l = md.list(&mut v).unwrap();
    assert_eq!(l.len(), 60);
    for de in &l {
        let i: usize = de.elem[..3].parse().unwrap();
        let (e, _) = md.open(&mut v, de).unwrap();
        assert_eq!(read_all(&mut v, e), vec![i as u8; i]);
        assert_eq!(md.walk(&mut v, &de.elem).unwrap().as_ref(), Some(de));
    }

    // The same tree gives the same archive.
    let mut v2 = MemStore::new();
    assert_eq!(archive(&mut v2, 512, &refs), score);
    assert_eq!(v2.len(), v.len());
    // A golden score: a change to any layout shows here.
    let mut v3 = MemStore::new();
    let s3 = archive(&mut v3, 8192, &[("hello", b"hello, world\n".to_vec())]);
    assert_eq!(s3.to_string(), "91cea28211bc7dfcff9b21f2e524cbd6cf7d85c0");
}

#[test]
fn deep_tree() {
    // 3 levels of pointers over 256-byte blocks with 12 scores each.
    let mut v = MemStore::new();
    let data: Vec<u8> = (0..256 * 200u32).map(|i| (i % 253) as u8 + 1).collect();
    let mut w = StreamWriter::new(DATATYPE, 240, 256);
    for c in data.chunks(256) {
        w.write(&mut v, c).unwrap();
    }
    let e = w.finish(&mut v).unwrap();
    assert_eq!(e.depth, 3);
    assert_eq!(read_all(&mut v, e), data);
    // Exactly one full pointer block: one level.
    let mut w = StreamWriter::new(DATATYPE, 240, 256);
    for c in data.chunks(256).take(12) {
        w.write(&mut v, c).unwrap();
    }
    assert_eq!(w.finish(&mut v).unwrap().depth, 1);
}

#[test]
fn fossil_root() {
    // Fossil's archives put the three entries one stream further down.
    let mut v = MemStore::new();
    let s = archive(&mut v, 8192, &[("f", b"fossil\n".to_vec())]);
    let r = Root::unpack(&v.read(&s, ROOTTYPE, ROOTSIZE).unwrap()).unwrap();
    let three = v.read(&r.score, DIRTYPE, 120).unwrap();
    let e = Entry {
        generation: 0,
        psize: 8180,
        dsize: 8160,
        depth: 0,
        flags: ENTRYACTIVE | ENTRYDIR,
        size: 120,
        score: Score::of(&three),
    };
    let top = v.write(DIRTYPE, &e.pack()).unwrap();
    let froot = v
        .write(ROOTTYPE, &Root { score: top, ..r }.pack().unwrap())
        .unwrap();
    let (mut d, _, _) = vac::vdroot(&mut v, &froot).unwrap();
    let f = d.walk(&mut v, "f").unwrap().unwrap();
    let (e, _) = d.open(&mut v, &f).unwrap();
    assert_eq!(read_all(&mut v, e), b"fossil\n");
}

/// A session over an in-memory pipe to a [`MemStore`] answering as a
/// server does.
struct Loop {
    store: MemStore,
    out: Vec<u8>,
    inbuf: Vec<u8>,
    greeted: bool,
}

impl Read for Loop {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.out.len());
        buf[..n].copy_from_slice(&self.out[..n]);
        self.out.drain(..n);
        Ok(n)
    }
}

impl Write for Loop {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inbuf.extend_from_slice(buf);
        if !self.greeted {
            if let Some(i) = self.inbuf.iter().position(|&c| c == b'\n') {
                assert_eq!(&self.inbuf[..=i], b"venti-02-libventi\n");
                self.inbuf.drain(..=i);
                self.out.extend_from_slice(b"venti-04:02-libventi\n");
                self.greeted = true;
            }
            return Ok(buf.len());
        }
        while self.inbuf.len() >= 2 {
            let n = usize::from(u16::from_be_bytes([self.inbuf[0], self.inbuf[1]]));
            if self.inbuf.len() < n + 2 {
                break;
            }
            let body: Vec<u8> = self.inbuf.drain(..n + 2).skip(2).collect();
            let (tag, m) = Msg::unpack(&body).unwrap();
            let r = match m {
                Msg::Thello { version, .. } => {
                    assert_eq!(version, "02");
                    Msg::Rhello {
                        sid: String::from("anonymous"),
                        crypto: 0,
                        codec: 0,
                    }
                }
                Msg::Tping => Msg::Rping,
                Msg::Tsync => Msg::Rsync,
                Msg::Tread {
                    score,
                    btype,
                    count,
                } => match self.store.read(&score, btype, count as usize) {
                    Ok(d) => Msg::Rread(d),
                    Err(e) => Msg::Rerror(e),
                },
                Msg::Twrite { btype, data } => match self.store.write(btype, &data) {
                    Ok(s) => Msg::Rwrite(s),
                    Err(e) => Msg::Rerror(e),
                },
                _ => Msg::Rerror(String::from("unexpected")),
            };
            self.out.extend(r.pack(tag).unwrap());
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn session() {
    let lp = Loop {
        store: MemStore::new(),
        out: Vec::new(),
        inbuf: Vec::new(),
        greeted: false,
    };
    let mut s = Session::new(lp).unwrap();
    assert_eq!((s.version.as_str(), s.sid.as_str()), ("02", "anonymous"));
    s.ping().unwrap();
    let sc = s.write(DATATYPE, b"block").unwrap();
    assert_eq!(sc, Score::of(b"block"));
    assert_eq!(s.read(&sc, DATATYPE, 100).unwrap(), b"block");
    assert_eq!(s.read(&Score::zero(), DATATYPE, 0).unwrap(), b"");
    assert_eq!(
        s.read(&sc, DIRTYPE, 100),
        Err(format!("no block with score {sc}/2 exists"))
    );
    s.sync().unwrap();
    // An archive through the session.
    let mut ds = DirSink::new(8192);
    let mut w = StreamWriter::new(DATATYPE, psize(8192), 8192);
    w.write(&mut s, b"over the wire").unwrap();
    let mut de = direntry("f", 0o644);
    de.entry = ds.add(&w.finish(&mut s).unwrap());
    let mut ms = MetaSink::new(8192);
    ms.add(de);
    let e0 = ds.finish(&mut s).unwrap();
    let e1 = ms.finish(&mut s).unwrap();
    let root = write_root(&mut s, "vac", 8192, &e0, &e1, direntry("vac", MODEDIR)).unwrap();
    let (mut d, _, _) = vac::vdroot(&mut s, &root).unwrap();
    let f = d.walk(&mut s, "f").unwrap().unwrap();
    let (e, _) = d.open(&mut s, &f).unwrap();
    assert_eq!(
        Source::new(e).read_at(&mut s, 0, 100).unwrap(),
        b"over the wire"
    );
    s.goodbye().unwrap();

    assert_eq!(netmkaddr("$venti", "net", "venti"), "net!$venti!venti");
    assert_eq!(netmkaddr("tcp!h", "net", "venti"), "tcp!h!venti");
    assert_eq!(netmkaddr("tcp!h!1", "net", "venti"), "tcp!h!1");
}

#[test]
fn plan9_dir_stream() {
    // Plan 9's vac keeps the block size of a directory stream, 8192, and
    // puts 204 entries in each block: entry 250 is the 47th of block 1.
    let mut v = MemStore::new();
    let mut w = StreamWriter::new(DIRTYPE, psize(8192), 8192);
    let ent = |i: u64| Entry {
        generation: 0,
        psize: 8180,
        dsize: 8192,
        depth: 0,
        flags: ENTRYACTIVE,
        size: i,
        score: Score::zero(),
    };
    let all: Vec<[u8; ENTRYSIZE]> = (0..300).map(|i| ent(i).pack()).collect();
    for (bi, chunk) in all.chunks(204).enumerate() {
        let mut b = chunk.as_flattened().to_vec();
        if bi == 0 {
            b.resize(8192, 0);
        }
        w.write(&mut v, &b).unwrap();
    }
    let e = w.finish(&mut v).unwrap();
    assert_eq!(e.size, 8192 + 96 * 40);
    let mut s = Source::new(e);
    assert_eq!(s.nentries(), 300);
    assert_eq!(s.entry(&mut v, 250).unwrap().size, 250);
    assert_eq!(s.entry(&mut v, 203).unwrap().size, 203);
    assert!(s.entry(&mut v, 300).is_err());
}
