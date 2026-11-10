//! The 9P2000 to 9P2000.u translator, checked against .u messages built by
//! hand from the 9P2000.u specification.

use purg_styx::dotu::{NONUNAME, Shim, VERSION_U, errno, errstr, stat_from_u, stat_to_u};
use purg_styx::{DMDIR, Dir, Fcall, Msg, NOFID, NOTAG, QTDIR, QTFILE, Qid, VERSION, dirs, kind};

/// A little-endian message builder for the .u side.
struct W(Vec<u8>);

impl W {
    fn new() -> W {
        W(Vec::new())
    }
    fn u8(mut self, v: u8) -> W {
        self.0.push(v);
        self
    }
    fn u16(mut self, v: u16) -> W {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(mut self, v: u32) -> W {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(mut self, v: u64) -> W {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn s(self, s: &str) -> W {
        let mut w = self.u16(s.len() as u16);
        w.0.extend_from_slice(s.as_bytes());
        w
    }
    fn bytes(mut self, b: &[u8]) -> W {
        self.0.extend_from_slice(b);
        self
    }
    fn qid(self, q: Qid) -> W {
        self.u8(q.qtype).u32(q.vers).u64(q.path)
    }
    /// Wraps the body in `size[4] type[1] tag[2]`.
    fn msg(self, ty: u8, tag: u16) -> Vec<u8> {
        let mut out = W::new().u32(7 + self.0.len() as u32).u8(ty).u16(tag).0;
        out.extend_from_slice(&self.0);
        out
    }
}

fn enc(tag: u16, fcall: Fcall<'_>) -> Vec<u8> {
    Msg { tag, fcall }.encode().unwrap()
}

fn dec(buf: &[u8]) -> Msg<'_> {
    Msg::decode(buf, u32::MAX).unwrap()
}

const DIRQ: Qid = Qid {
    path: 1,
    vers: 0,
    qtype: QTDIR,
};
const FILEQ: Qid = Qid {
    path: 2,
    vers: 3,
    qtype: QTFILE,
};

/// A .u stat entry as QEMU sends it: empty names, numeric ids.
#[allow(clippy::too_many_arguments)]
fn ustat(qid: Qid, mode: u32, length: u64, name: &str, ext: &str, ids: [u32; 3]) -> Vec<u8> {
    let body = W::new()
        .u16(0)
        .u32(0)
        .qid(qid)
        .u32(mode)
        .u32(1_700_000_000)
        .u32(1_700_000_001)
        .u64(length)
        .s(name)
        .s("")
        .s("")
        .s("")
        .s(ext)
        .u32(ids[0])
        .u32(ids[1])
        .u32(ids[2])
        .0;
    W::new().u16(body.len() as u16).bytes(&body).0
}

#[test]
fn version_renamed_both_ways() {
    let mut s = Shim::new();
    let t = s
        .request(&enc(
            NOTAG,
            Fcall::Tversion {
                msize: 8216,
                version: VERSION,
            },
        ))
        .unwrap();
    assert_eq!(
        t,
        W::new().u32(8216).s("9P2000.u").msg(kind::TVERSION, NOTAG)
    );
    let r = s
        .response(&W::new().u32(8192).s(VERSION_U).msg(kind::RVERSION, NOTAG))
        .unwrap();
    assert_eq!(
        dec(&r).fcall,
        Fcall::Rversion {
            msize: 8192,
            version: VERSION
        }
    );
    // A server that does not know the version says so, and the client
    // sees that unchanged.
    let r = s
        .response(&W::new().u32(8192).s("unknown").msg(kind::RVERSION, NOTAG))
        .unwrap();
    assert_eq!(
        dec(&r).fcall,
        Fcall::Rversion {
            msize: 8192,
            version: "unknown"
        }
    );
}

#[test]
fn attach_and_auth_gain_n_uname() {
    let mut s = Shim::new();
    let t = s
        .request(&enc(
            1,
            Fcall::Tattach {
                fid: 5,
                afid: NOFID,
                uname: "inferno",
                aname: "",
            },
        ))
        .unwrap();
    let want = W::new()
        .u32(5)
        .u32(NOFID)
        .s("inferno")
        .s("")
        .u32(NONUNAME)
        .msg(kind::TATTACH, 1);
    assert_eq!(t, want);
    let r = W::new().qid(DIRQ).msg(kind::RATTACH, 1);
    assert_eq!(s.response(&r).unwrap(), r);

    let t = s
        .request(&enc(
            2,
            Fcall::Tauth {
                afid: 9,
                uname: "inferno",
                aname: "x",
            },
        ))
        .unwrap();
    let want = W::new()
        .u32(9)
        .s("inferno")
        .s("x")
        .u32(NONUNAME)
        .msg(kind::TAUTH, 2);
    assert_eq!(t, want);
    // QEMU does not authenticate: any error to Tauth means "not required".
    let r = W::new()
        .s("Operation not supported")
        .u32(95)
        .msg(kind::RERROR, 2);
    assert_eq!(
        dec(&s.response(&r).unwrap()).fcall,
        Fcall::Rerror {
            ename: "authentication not required"
        }
    );
}

#[test]
fn rerror_drops_errno_and_maps_it() {
    let mut s = Shim::new();
    let cases: &[(u32, &str, &str)] = &[
        (
            errno::ENOENT,
            "No such file or directory",
            "file does not exist",
        ),
        (errno::EACCES, "Permission denied", "permission denied"),
        (errno::EPERM, "Operation not permitted", "permission denied"),
        (errno::EEXIST, "File exists", "file already exists"),
        (errno::ENOTDIR, "Not a directory", "not a directory"),
        (errno::EISDIR, "Is a directory", "file is a directory"),
        (
            errno::ENOTEMPTY,
            "Directory not empty",
            "directory not empty",
        ),
        (errno::ENOSPC, "No space left on device", "file system full"),
        (22, "Invalid argument", "invalid argument"),
    ];
    for (i, &(e, ename, want)) in cases.iter().enumerate() {
        let tag = i as u16;
        s.request(&enc(tag, Fcall::Tstat { fid: 1 })).unwrap();
        let r = W::new().s(ename).u32(e).msg(kind::RERROR, tag);
        let out = s.response(&r).unwrap();
        assert_eq!(dec(&out).fcall, Fcall::Rerror { ename: want }, "{ename}");
    }
    // An Rerror without the errno is still read.
    let r = W::new().s("Bad Thing").msg(kind::RERROR, 77);
    assert_eq!(
        dec(&s.response(&r).unwrap()).fcall,
        Fcall::Rerror { ename: "bad thing" }
    );
    assert_eq!(errstr(None, "Oops"), "oops");
    assert_eq!(errstr(Some(errno::EIO), "whatever"), "i/o error");
}

#[test]
fn create_gains_extension() {
    let mut s = Shim::new();
    let t = s
        .request(&enc(
            3,
            Fcall::Tcreate {
                fid: 4,
                name: "new",
                perm: DMDIR | 0o755,
                mode: 0,
            },
        ))
        .unwrap();
    let want = W::new()
        .u32(4)
        .s("new")
        .u32(DMDIR | 0o755)
        .u8(0)
        .s("")
        .msg(kind::TCREATE, 3);
    assert_eq!(t, want);
    // The created directory is now the fid, so its reads are directories.
    let r = W::new().qid(DIRQ).u32(8192).msg(kind::RCREATE, 3);
    assert_eq!(s.response(&r).unwrap(), r);
    s.request(&enc(
        4,
        Fcall::Tread {
            fid: 4,
            offset: 0,
            count: 8192,
        },
    ))
    .unwrap();
    let entry = ustat(FILEQ, 0o644, 1, "f", "", [0, 0, 0]);
    let r = W::new()
        .u32(entry.len() as u32)
        .bytes(&entry)
        .msg(kind::RREAD, 4);
    let out = s.response(&r).unwrap();
    let Fcall::Rread { data } = dec(&out).fcall else {
        panic!("not Rread")
    };
    assert_eq!(dirs(data).next().unwrap().unwrap().name, "f");
}

#[test]
fn stat_converted() {
    let mut s = Shim::new();
    s.request(&enc(6, Fcall::Tstat { fid: 1 })).unwrap();
    // A symlink bit in the mode, empty names and numeric ids.
    let ustat_bytes = ustat(
        FILEQ,
        0x0200_0000 | 0o644,
        23,
        "kept.txt",
        "",
        [1000, 100, NONUNAME],
    );
    let r = W::new()
        .u16(ustat_bytes.len() as u16)
        .bytes(&ustat_bytes)
        .msg(kind::RSTAT, 6);
    let out = s.response(&r).unwrap();
    let Fcall::Rstat { stat } = dec(&out).fcall else {
        panic!("not Rstat")
    };
    let (d, used) = Dir::unpack(stat).unwrap();
    assert_eq!(used, stat.len());
    assert_eq!(d.name, "kept.txt");
    assert_eq!(d.length, 23);
    assert_eq!(d.mode, 0o644);
    assert_eq!(d.qid, FILEQ);
    assert_eq!(
        (d.uid.as_str(), d.gid.as_str(), d.muid.as_str()),
        ("1000", "100", "")
    );
    assert_eq!((d.atime, d.mtime), (1_700_000_000, 1_700_000_001));
    // QEMU puts 0 in Rstat's n[2]; the entry's own size is what counts.
    s.request(&enc(7, Fcall::Tstat { fid: 1 })).unwrap();
    let r = W::new().u16(0).bytes(&ustat_bytes).msg(kind::RSTAT, 7);
    let out = s.response(&r).unwrap();
    let Fcall::Rstat { stat: got } = dec(&out).fcall else {
        panic!("not Rstat")
    };
    assert_eq!(Dir::unpack(got).unwrap().0, d);
    let r = W::new()
        .u16(0)
        .bytes(&ustat_bytes)
        .u8(0)
        .msg(kind::RSTAT, 8);
    assert!(s.response(&r).is_err());
}

#[test]
fn stat_round_trip() {
    let d = Dir {
        dtype: 0,
        dev: 0,
        qid: FILEQ,
        mode: DMDIR | 0o755,
        atime: 1,
        mtime: 2,
        length: 3,
        name: "n".into(),
        uid: "u".into(),
        gid: "g".into(),
        muid: "m".into(),
    };
    let u = stat_to_u(&d).unwrap();
    // size[2], the 9P2000 entry, extension[s] and three ids.
    assert_eq!(u.len(), d.packed_len() + 2 + 12);
    assert_eq!(u16::from_le_bytes([u[0], u[1]]) as usize, u.len() - 2);
    assert_eq!(&u[u.len() - 12..], &[0xff; 12]);
    assert_eq!(stat_from_u(&u).unwrap(), (d.clone(), u.len()));
    // A plain 9P2000 entry is accepted as is.
    let p = d.pack().unwrap();
    assert_eq!(stat_from_u(&p).unwrap(), (d, p.len()));
    // Truncated .u fields are an error, not a panic.
    assert!(stat_from_u(&u[..u.len() - 3]).is_err());
}

#[test]
fn wstat_sends_u_stat() {
    let mut s = Shim::new();
    let mut d = Dir::null();
    d.name = "renamed".into();
    let stat = d.pack().unwrap();
    let t = s
        .request(&enc(
            8,
            Fcall::Twstat {
                fid: 2,
                stat: &stat,
            },
        ))
        .unwrap();
    let u = stat_to_u(&d).unwrap();
    let want = W::new()
        .u32(2)
        .u16(u.len() as u16)
        .bytes(&u)
        .msg(kind::TWSTAT, 8);
    assert_eq!(t, want);
    let r = W::new().msg(kind::RWSTAT, 8);
    assert_eq!(s.response(&r).unwrap(), r);
}

/// Attach, walk to a directory and a file, open both, and read both: only
/// the directory's data is rewritten.
#[test]
fn directory_reads_follow_the_fid() {
    let mut s = Shim::new();
    let attach = Fcall::Tattach {
        fid: 1,
        afid: NOFID,
        uname: "u",
        aname: "",
    };
    s.request(&enc(1, attach)).unwrap();
    s.response(&W::new().qid(DIRQ).msg(kind::RATTACH, 1))
        .unwrap();
    // fid 2 = clone of the root (a directory), fid 3 = a file.
    let walk = |newfid, wnames| Fcall::Twalk {
        fid: 1,
        newfid,
        wnames,
    };
    let t = enc(2, walk(2, vec![]));
    assert_eq!(s.request(&t).unwrap(), t);
    s.response(&W::new().u16(0).msg(kind::RWALK, 2)).unwrap();
    s.request(&enc(3, walk(3, vec!["sub", "f"]))).unwrap();
    s.response(&W::new().u16(2).qid(DIRQ).qid(FILEQ).msg(kind::RWALK, 3))
        .unwrap();
    // A walk that stops short leaves fid 4 unknown (a file).
    s.request(&enc(4, walk(4, vec!["sub", "missing"]))).unwrap();
    s.response(&W::new().u16(1).qid(DIRQ).msg(kind::RWALK, 4))
        .unwrap();

    let dot = ustat(DIRQ, DMDIR | 0o755, 0, ".", "", [0, 0, 0]);
    let dotdot = ustat(DIRQ, DMDIR | 0o755, 0, "..", "", [0, 0, 0]);
    let a = ustat(DIRQ, DMDIR | 0o755, 0, "sub", "", [0, 0, 0]);
    let b = ustat(FILEQ, 0o644, 5, "hello.txt", "", [1000, 1000, 1000]);
    let data: Vec<u8> = [dot, a, dotdot, b].concat();
    let rread = |tag| {
        W::new()
            .u32(data.len() as u32)
            .bytes(&data)
            .msg(kind::RREAD, tag)
    };
    let tread = |fid| Fcall::Tread {
        fid,
        offset: 0,
        count: 8192,
    };
    s.request(&enc(5, tread(2))).unwrap();
    let out = s.response(&rread(5)).unwrap();
    let Fcall::Rread { data: plain } = dec(&out).fcall else {
        panic!("not Rread")
    };
    let names: Vec<String> = dirs(plain).map(|d| d.unwrap().name).collect();
    assert_eq!(names, ["sub", "hello.txt"]);
    let uids: Vec<String> = dirs(plain).map(|d| d.unwrap().uid).collect();
    assert_eq!(uids, ["0", "1000"]);
    // The file's data passes through, even though it looks like stats.
    s.request(&enc(6, tread(3))).unwrap();
    assert_eq!(s.response(&rread(6)).unwrap(), rread(6));
    s.request(&enc(7, tread(4))).unwrap();
    assert_eq!(s.response(&rread(7)).unwrap(), rread(7));
    // After a clunk, the fid number is a file until something says not.
    s.request(&enc(8, Fcall::Tclunk { fid: 2 })).unwrap();
    s.response(&W::new().msg(kind::RCLUNK, 8)).unwrap();
    s.request(&enc(9, tread(2))).unwrap();
    assert_eq!(s.response(&rread(9)).unwrap(), rread(9));
    // A bad entry in a directory read is an error.
    s.request(&enc(10, Fcall::Topen { fid: 1, mode: 0 }))
        .unwrap();
    s.response(&W::new().qid(DIRQ).u32(0).msg(kind::ROPEN, 10))
        .unwrap();
    s.request(&enc(11, tread(1))).unwrap();
    let bad = W::new().u32(3).bytes(&[9, 0, 1]).msg(kind::RREAD, 11);
    assert!(s.response(&bad).is_err());
}

#[test]
fn flush_and_passthrough() {
    let mut s = Shim::new();
    let tread = enc(
        1,
        Fcall::Tread {
            fid: 7,
            offset: 0,
            count: 10,
        },
    );
    assert_eq!(s.request(&tread).unwrap(), tread);
    let flush = enc(2, Fcall::Tflush { oldtag: 1 });
    assert_eq!(s.request(&flush).unwrap(), flush);
    let rflush = enc(2, Fcall::Rflush);
    assert_eq!(s.response(&rflush).unwrap(), rflush);
    // Writes, opens, clunks and removes are the same in both dialects.
    for f in [
        Fcall::Twrite {
            fid: 7,
            offset: 3,
            data: b"abc",
        },
        Fcall::Topen { fid: 7, mode: 1 },
        Fcall::Tremove { fid: 7 },
    ] {
        let t = enc(3, f);
        assert_eq!(s.request(&t).unwrap(), t);
    }
    let rwrite = enc(3, Fcall::Rwrite { count: 3 });
    assert_eq!(s.response(&rwrite).unwrap(), rwrite);
    // R-messages are not requests, and junk is not a response.
    assert!(s.request(&rwrite).is_err());
    assert!(s.response(&[1, 0, 0]).is_err());
    assert!(s.response(&W::new().msg(kind::RCLUNK, 1)[..6]).is_err());
}
