use std::fs;
use std::path::PathBuf;

use purg_styx::{
    DMDIR, Dir, Error, Fcall, MAXWELEM, Msg, NOFID, NOTAG, QTDIR, Qid, VERSION, dirs, kind,
};

const MSIZE: u32 = 8192;

fn qid(path: u64) -> Qid {
    Qid {
        path,
        vers: 7,
        qtype: QTDIR,
    }
}

fn sample_dir() -> Dir {
    Dir {
        dtype: u16::from(b'U'),
        dev: 3,
        qid: qid(0x1122_3344_5566_7788),
        mode: DMDIR | 0o755,
        atime: 1_700_000_000,
        mtime: 1_700_000_001,
        length: 0,
        name: "usr".into(),
        uid: "inferno".into(),
        gid: "inferno".into(),
        muid: "".into(),
    }
}

fn every_message(stat: &[u8]) -> Vec<Fcall<'_>> {
    vec![
        Fcall::Tversion {
            msize: MSIZE,
            version: VERSION,
        },
        Fcall::Rversion {
            msize: MSIZE,
            version: VERSION,
        },
        Fcall::Tauth {
            afid: 1,
            uname: "inferno",
            aname: "",
        },
        Fcall::Rauth { aqid: qid(1) },
        Fcall::Tattach {
            fid: 0,
            afid: NOFID,
            uname: "inferno",
            aname: "main",
        },
        Fcall::Rattach { qid: qid(0) },
        Fcall::Rerror {
            ename: "file does not exist",
        },
        Fcall::Tflush { oldtag: 3 },
        Fcall::Rflush,
        Fcall::Twalk {
            fid: 0,
            newfid: 1,
            wnames: vec!["usr", "inferno", "lib"],
        },
        Fcall::Twalk {
            fid: 0,
            newfid: 1,
            wnames: vec![],
        },
        Fcall::Rwalk {
            wqids: vec![qid(1), qid(2)],
        },
        Fcall::Topen { fid: 1, mode: 0 },
        Fcall::Ropen {
            qid: qid(1),
            iounit: 8168,
        },
        Fcall::Tcreate {
            fid: 1,
            name: "new",
            perm: 0o644,
            mode: 1,
        },
        Fcall::Rcreate {
            qid: qid(9),
            iounit: 0,
        },
        Fcall::Tread {
            fid: 1,
            offset: 1 << 40,
            count: 4096,
        },
        Fcall::Rread {
            data: b"hello\0world",
        },
        Fcall::Twrite {
            fid: 1,
            offset: 12,
            data: &[0xff; 300],
        },
        Fcall::Rwrite { count: 300 },
        Fcall::Tclunk { fid: 1 },
        Fcall::Rclunk,
        Fcall::Tremove { fid: 1 },
        Fcall::Rremove,
        Fcall::Tstat { fid: 1 },
        Fcall::Rstat { stat },
        Fcall::Twstat { fid: 1, stat },
        Fcall::Rwstat,
    ]
}

#[test]
fn every_message_type_round_trips() {
    let stat = sample_dir().pack().unwrap();
    let msgs = every_message(&stat);
    let mut kinds: Vec<u8> = msgs.iter().map(Fcall::kind).collect();
    kinds.sort();
    kinds.dedup();
    let all: Vec<u8> = (kind::TVERSION..=kind::RWSTAT)
        .filter(|&k| k != kind::TERROR)
        .collect();
    assert_eq!(kinds, all, "test must cover every message type");
    for (i, fcall) in msgs.into_iter().enumerate() {
        let msg = Msg {
            tag: i as u16,
            fcall,
        };
        let bytes = msg.encode().unwrap();
        assert_eq!(purg_styx::message_size(&bytes), Some(bytes.len()));
        let back = Msg::decode(&bytes, MSIZE).unwrap();
        assert_eq!(back, msg);
    }
}

#[test]
fn tversion_wire_format() {
    let msg = Msg {
        tag: NOTAG,
        fcall: Fcall::Tversion {
            msize: 8192,
            version: VERSION,
        },
    };
    let want = [
        19, 0, 0, 0, 100, 0xff, 0xff, 0x00, 0x20, 0, 0, 6, 0, b'9', b'P', b'2', b'0', b'0', b'0',
    ];
    assert_eq!(msg.encode().unwrap(), want);
}

#[test]
fn dir_round_trips() {
    let d = sample_dir();
    let packed = d.pack().unwrap();
    assert_eq!(packed.len(), d.packed_len());
    assert_eq!(Dir::unpack(&packed).unwrap(), (d.clone(), packed.len()));
    let mut two = packed.clone();
    two.extend_from_slice(&Dir::null().pack().unwrap());
    let back: Vec<Dir> = dirs(&two).collect::<Result<_, _>>().unwrap();
    assert_eq!(back, [d, Dir::null()]);
}

#[test]
fn rejects_malformed_messages() {
    let good = Msg {
        tag: 1,
        fcall: Fcall::Tclunk { fid: 5 },
    }
    .encode()
    .unwrap();
    assert_eq!(
        Msg::decode(&good[..good.len() - 1], MSIZE),
        Err(Error::Short)
    );
    let mut long = good.clone();
    long.push(0);
    assert_eq!(Msg::decode(&long, MSIZE), Err(Error::Trailing));
    assert_eq!(Msg::decode(&good, 8), Err(Error::TooBig));
    let mut terror = good.clone();
    terror[4] = kind::TERROR;
    assert_eq!(
        Msg::decode(&terror, MSIZE),
        Err(Error::BadType(kind::TERROR))
    );
    let walk = Msg {
        tag: 1,
        fcall: Fcall::Twalk {
            fid: 0,
            newfid: 1,
            wnames: vec!["a"; MAXWELEM + 1],
        },
    };
    assert_eq!(walk.encode(), Err(Error::TooManyWalk));
    let mut bad_utf8 = Msg {
        tag: 1,
        fcall: Fcall::Rerror { ename: "ab" },
    }
    .encode()
    .unwrap();
    let n = bad_utf8.len();
    bad_utf8[n - 1] = 0xff;
    assert_eq!(Msg::decode(&bad_utf8, MSIZE), Err(Error::BadString));
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/9fans");
    let mut files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(&p).unwrap(),
            )
        })
        .collect()
}

/// Traffic captured from 9fans.net/go/plan9 (client and Fcall encoder),
/// generated by `tests/capture/main.go`.
#[test]
fn decodes_captured_9fans_traffic() {
    let fixtures = fixtures();
    assert_eq!(fixtures.len(), 34);
    for (name, bytes) in &fixtures {
        let msg = Msg::decode(bytes, MSIZE).unwrap_or_else(|e| panic!("{name}: {e}"));
        let type_name = name.split(['-', '.']).nth(1).unwrap();
        let debug = format!("{:?}", msg.fcall);
        assert!(debug.starts_with(type_name), "{name} decoded as {debug}");
        assert_eq!(
            &msg.encode().unwrap(),
            bytes,
            "{name} re-encodes differently"
        );
    }
    let get = |n: &str| -> Msg<'_> {
        let (_, b) = fixtures.iter().find(|(f, _)| f.starts_with(n)).unwrap();
        Msg::decode(b, MSIZE).unwrap()
    };
    assert!(matches!(
        get("02-").fcall,
        Fcall::Rversion {
            version: "9P2000",
            ..
        }
    ));
    assert!(matches!(
        get("10-").fcall,
        Fcall::Rread {
            data: b"hello, world\n"
        }
    ));
    assert!(matches!(
        get("18-").fcall,
        Fcall::Rerror {
            ename: "file does not exist"
        }
    ));
    let Fcall::Rstat { stat } = get("14-").fcall else {
        panic!("14 is not Rstat")
    };
    let (d, _) = Dir::unpack(stat).unwrap();
    assert_eq!(
        (d.name.as_str(), d.length, d.uid.as_str()),
        ("hello", 13, "glenda")
    );
    let Fcall::Rread { data } = get("30-").fcall else {
        panic!("30 is not Rread")
    };
    let names: Vec<String> = dirs(data).map(|d| d.unwrap().name).collect();
    assert_eq!(names, ["hello", "sub"]);
}
