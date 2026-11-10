//! Property tests of the codec and of the 9P2000.u shim (plan Section 9).
//!
//! Case counts are small so that `cargo test` stays fast. A failure prints
//! a minimal input; turn it into a named test next to the fix. Set
//! `PROPTEST_CASES` to run more.

use proptest::collection::vec;
use proptest::prelude::*;

use purg_styx::dotu::{NONUNAME, Shim, VERSION_U, stat_from_u, stat_to_u};
use purg_styx::{
    Dir, Error, Fcall, MAXWELEM, Msg, QTDIR, Qid, STATFIXLEN, VERSION, dirs, kind, message_size,
};

#[path = "../../../fuzz/src/lib.rs"]
mod harness;

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: 96,
        // No regression files: failures become named tests instead.
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

fn string() -> impl Strategy<Value = String> {
    vec(any::<char>(), 0..10).prop_map(String::from_iter)
}

fn qid() -> impl Strategy<Value = Qid> {
    (any::<u64>(), any::<u32>(), any::<u8>()).prop_map(|(path, vers, qtype)| Qid {
        path,
        vers,
        qtype,
    })
}

fn dir() -> impl Strategy<Value = Dir> {
    (
        (any::<u16>(), any::<u32>(), qid(), any::<u32>()),
        (any::<u32>(), any::<u32>(), any::<u64>()),
        (string(), string(), string(), string()),
    )
        .prop_map(
            |((dtype, dev, qid, mode), (atime, mtime, length), (name, uid, gid, muid))| Dir {
                dtype,
                dev,
                qid,
                mode,
                atime,
                mtime,
                length,
                name,
                uid,
                gid,
                muid,
            },
        )
}

/// The fields any message could need, owned, so that a borrowed [`Fcall`]
/// can be made from them.
#[derive(Debug, Clone)]
struct Raw {
    which: usize,
    tag: u16,
    a: u32,
    b: u32,
    offset: u64,
    mode: u8,
    s1: String,
    s2: String,
    names: Vec<String>,
    qid: Qid,
    qids: Vec<Qid>,
    data: Vec<u8>,
}

/// One per message type: `Raw::which` picks.
const MESSAGES: usize = 27;
/// The `which` values of the T-messages.
const REQUESTS: [usize; 13] = [0, 2, 4, 7, 9, 11, 13, 15, 17, 19, 21, 23, 25];

impl Raw {
    fn fcall(&self) -> Fcall<'_> {
        let (fid, qid) = (self.a, self.qid);
        match self.which {
            0 => Fcall::Tversion {
                msize: self.a,
                version: &self.s1,
            },
            1 => Fcall::Rversion {
                msize: self.a,
                version: &self.s1,
            },
            2 => Fcall::Tauth {
                afid: self.a,
                uname: &self.s1,
                aname: &self.s2,
            },
            3 => Fcall::Rauth { aqid: qid },
            4 => Fcall::Tattach {
                fid,
                afid: self.b,
                uname: &self.s1,
                aname: &self.s2,
            },
            5 => Fcall::Rattach { qid },
            6 => Fcall::Rerror { ename: &self.s1 },
            7 => Fcall::Tflush {
                oldtag: self.b as u16,
            },
            8 => Fcall::Rflush,
            9 => Fcall::Twalk {
                fid,
                newfid: self.b,
                wnames: self.names.iter().map(String::as_str).collect(),
            },
            10 => Fcall::Rwalk {
                wqids: self.qids.clone(),
            },
            11 => Fcall::Topen {
                fid,
                mode: self.mode,
            },
            12 => Fcall::Ropen {
                qid,
                iounit: self.b,
            },
            13 => Fcall::Tcreate {
                fid,
                name: &self.s1,
                perm: self.b,
                mode: self.mode,
            },
            14 => Fcall::Rcreate {
                qid,
                iounit: self.b,
            },
            15 => Fcall::Tread {
                fid,
                offset: self.offset,
                count: self.b,
            },
            16 => Fcall::Rread { data: &self.data },
            17 => Fcall::Twrite {
                fid,
                offset: self.offset,
                data: &self.data,
            },
            18 => Fcall::Rwrite { count: self.b },
            19 => Fcall::Tclunk { fid },
            20 => Fcall::Rclunk,
            21 => Fcall::Tremove { fid },
            22 => Fcall::Rremove,
            23 => Fcall::Tstat { fid },
            24 => Fcall::Rstat { stat: &self.data },
            25 => Fcall::Twstat {
                fid,
                stat: &self.data,
            },
            26 => Fcall::Rwstat,
            _ => unreachable!(),
        }
    }

    fn msg(&self) -> Msg<'_> {
        Msg {
            tag: self.tag,
            fcall: self.fcall(),
        }
    }

    fn encode(&self) -> Vec<u8> {
        self.msg().encode().unwrap()
    }
}

/// The `which` values of the R-messages with the same layout in 9P2000.u:
/// all but `Rerror` and `Rstat`.
const PLAIN_REPLIES: [usize; 12] = [1, 3, 5, 8, 10, 12, 14, 16, 18, 20, 22, 26];

/// Any message of the type `which` selects.
fn raw_of(which: impl Strategy<Value = usize>) -> impl Strategy<Value = Raw> {
    (
        (which, any::<u16>(), any::<u32>(), any::<u32>()),
        (any::<u64>(), any::<u8>(), string(), string()),
        (
            vec(string(), 0..=MAXWELEM),
            qid(),
            vec(qid(), 0..=MAXWELEM),
            vec(any::<u8>(), 0..80),
        ),
    )
        .prop_map(
            |((which, tag, a, b), (offset, mode, s1, s2), (names, qid, qids, data))| Raw {
                which,
                tag,
                a,
                b,
                offset,
                mode,
                s1,
                s2,
                names,
                qid,
                qids,
                data,
            },
        )
}

fn raw() -> impl Strategy<Value = Raw> {
    raw_of(0..MESSAGES)
}

/// A message header with the right size in front of `body`.
fn frame(ty: u8, tag: u16, body: &[u8]) -> Vec<u8> {
    let mut out = (7 + body.len() as u32).to_le_bytes().to_vec();
    out.push(ty);
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Bytes that are not a message, at three depths: noise, a well-framed
/// message with a noise body, and a valid message with a few bytes changed.
fn garbage() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        vec(any::<u8>(), 0..200),
        (100u8..128, any::<u16>(), vec(any::<u8>(), 0..100))
            .prop_map(|(ty, tag, body)| frame(ty, tag, &body)),
        (
            raw(),
            vec((any::<prop::sample::Index>(), any::<u8>()), 1..4)
        )
            .prop_map(|(m, edits)| {
                let mut bytes = m.encode();
                for (at, v) in edits {
                    let at = at.index(bytes.len());
                    bytes[at] = v;
                }
                bytes
            }),
    ]
}

/// The .u stat entry for `d` with the given extension and numeric ids.
fn ustat(d: &Dir, extension: &str, ids: [u32; 3]) -> Vec<u8> {
    let mut out = d.pack().unwrap();
    out.extend_from_slice(&(extension.len() as u16).to_le_bytes());
    out.extend_from_slice(extension.as_bytes());
    for id in ids {
        out.extend_from_slice(&id.to_le_bytes());
    }
    let body = (out.len() - 2) as u16;
    out[..2].copy_from_slice(&body.to_le_bytes());
    out
}

/// A .u directory entry: a [`Dir`] (with empty user and group names when
/// `numeric`, as QEMU sends them), its extension and its numeric ids.
fn uentry() -> impl Strategy<Value = (Dir, String, [u32; 3])> {
    let id = prop_oneof![any::<u32>(), Just(NONUNAME), 0u32..2000];
    (dir(), string(), [id.clone(), id.clone(), id], any::<bool>()).prop_map(
        |(mut d, extension, ids, numeric)| {
            if numeric {
                d.uid.clear();
                d.gid.clear();
                d.muid.clear();
            }
            (d, extension, ids)
        },
    )
}

/// What the shim makes of a .u entry.
fn translated((d, _, ids): &(Dir, String, [u32; 3])) -> Dir {
    let mut d = d.clone();
    // DMDIR, DMAPPEND, DMEXCL, DMAUTH, DMTMP and the permissions.
    d.mode &= 0xec00_0000 | 0o777;
    for (name, id) in [&mut d.uid, &mut d.gid, &mut d.muid].into_iter().zip(ids) {
        if name.is_empty() && *id != NONUNAME {
            *name = id.to_string();
        }
    }
    d
}

/// A shim whose fid 1 is an attached directory.
fn shim_with_dir() -> Shim {
    let mut shim = Shim::new();
    let attach = Fcall::Tattach {
        fid: 1,
        afid: !0,
        uname: "u",
        aname: "",
    };
    let t = Msg {
        tag: 1,
        fcall: attach,
    };
    shim.request(&t.encode().unwrap()).unwrap();
    let mut qid = vec![QTDIR];
    qid.extend_from_slice(&[0; 12]);
    shim.response(&frame(kind::RATTACH, 1, &qid)).unwrap();
    shim
}

/// The message the shim produced.
fn dec(out: &Option<Vec<u8>>) -> Fcall<'_> {
    let bytes = out.as_ref().expect("the shim refused the message");
    Msg::decode(bytes, u32::MAX).unwrap().fcall
}

proptest! {
    #![proptest_config(config())]

    /// decode(encode(x)) == x for every message type, and the encoding is
    /// what its size field says.
    #[test]
    fn every_message_round_trips(m in raw()) {
        let bytes = m.encode();
        prop_assert_eq!(message_size(&bytes), Some(bytes.len()));
        prop_assert_eq!(bytes[4], m.fcall().kind());
        let back = Msg::decode(&bytes, u32::MAX);
        prop_assert_eq!(back.as_ref(), Ok(&m.msg()));
        prop_assert_eq!(back.unwrap().encode().unwrap(), bytes);
    }

    /// Every message type is generated (so the round trip covers them all).
    #[test]
    fn message_kinds_are_distinct(which in 0..MESSAGES, m in raw()) {
        let m = Raw { which, ..m };
        let expect = [100, 101, 102, 103, 104, 105, 107, 108, 109, 110, 111, 112, 113, 114,
            115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127];
        prop_assert_eq!(m.fcall().kind(), expect[which]);
        prop_assert_eq!(REQUESTS.contains(&which), expect[which] % 2 == 0);
    }

    /// A message decodes exactly when it fits `msize`.
    #[test]
    fn decode_respects_msize(m in raw(), msize in 0u32..400) {
        let bytes = m.encode();
        let got = Msg::decode(&bytes, msize);
        if bytes.len() > msize as usize {
            prop_assert_eq!(got, Err(Error::TooBig));
        } else {
            prop_assert_eq!(got, Ok(m.msg()));
        }
    }

    /// Cutting a message short at any length is an error, never a panic
    /// and never a different message; so is a byte too many.
    #[test]
    fn truncation_is_an_error(m in raw(), extra in any::<u8>()) {
        let bytes = m.encode();
        for n in 0..bytes.len() {
            // As a short read leaves it: the size field still says more.
            prop_assert_eq!(Msg::decode(&bytes[..n], u32::MAX), Err(Error::Short), "at {}", n);
            // As a lying sender frames it: the size field agrees.
            if n >= 4 {
                let mut cut = bytes[..n].to_vec();
                cut[..4].copy_from_slice(&(n as u32).to_le_bytes());
                prop_assert!(Msg::decode(&cut, u32::MAX).is_err(), "reframed at {}", n);
            }
        }
        let mut long = bytes.clone();
        long.push(extra);
        prop_assert_eq!(Msg::decode(&long, u32::MAX), Err(Error::Trailing));
        let n = long.len() as u32;
        long[..4].copy_from_slice(&n.to_le_bytes());
        prop_assert!(Msg::decode(&long, u32::MAX).is_err());
    }

    /// Arbitrary bytes never panic the decoder; what does decode fits
    /// `msize` and encodes back to the same bytes.
    #[test]
    fn decode_never_panics(bytes in garbage()) {
        harness::decode(&bytes);
    }

    #[test]
    fn dir_round_trips(d in dir()) {
        let bytes = d.pack().unwrap();
        prop_assert_eq!(bytes.len(), d.packed_len());
        prop_assert!(bytes.len() >= STATFIXLEN);
        prop_assert_eq!(Dir::unpack(&bytes), Ok((d.clone(), bytes.len())));
        for n in 0..bytes.len() {
            prop_assert_eq!(Dir::unpack(&bytes[..n]), Err(Error::Short), "at {}", n);
        }
        // Inside Rstat and Twstat, with the extra n[2].
        for fcall in [Fcall::Rstat { stat: &bytes }, Fcall::Twstat { fid: 1, stat: &bytes }] {
            let m = Msg { tag: 1, fcall };
            let enc = m.encode().unwrap();
            prop_assert_eq!(Msg::decode(&enc, u32::MAX), Ok(m));
        }
    }

    /// Entries packed one after another, as a directory read carries them.
    #[test]
    fn directory_reads_round_trip(ds in vec(dir(), 0..6), junk in vec(any::<u8>(), 1..20)) {
        let mut bytes = Vec::new();
        for d in &ds {
            d.pack_into(&mut bytes).unwrap();
        }
        let got: Result<Vec<Dir>, Error> = dirs(&bytes).collect();
        prop_assert_eq!(got.as_ref(), Ok(&ds));
        // Whatever follows, the good entries still come first and the
        // iterator ends.
        bytes.extend_from_slice(&junk);
        let got: Vec<_> = dirs(&bytes).take(ds.len() + 30).collect();
        prop_assert!(got.len() <= ds.len() + 20);
        for (g, d) in got.iter().zip(&ds) {
            prop_assert_eq!(g.as_ref(), Ok(d));
        }
    }

    /// A Qid survives every message that carries one.
    #[test]
    fn qid_round_trips(q in qid(), qs in vec(qid(), 0..=MAXWELEM), iounit in any::<u32>()) {
        for fcall in [
            Fcall::Rauth { aqid: q },
            Fcall::Rattach { qid: q },
            Fcall::Ropen { qid: q, iounit },
            Fcall::Rcreate { qid: q, iounit },
            Fcall::Rwalk { wqids: qs.clone() },
        ] {
            let m = Msg { tag: 7, fcall };
            let enc = m.encode().unwrap();
            prop_assert_eq!(Msg::decode(&enc, u32::MAX), Ok(m));
        }
        prop_assert_eq!(q.is_dir(), q.qtype & QTDIR != 0);
    }

    /// More walk elements than 9P allows are refused both ways.
    #[test]
    fn long_walks_are_refused(n in MAXWELEM + 1..40) {
        let wnames = vec!["a"; n];
        let t = Msg { tag: 1, fcall: Fcall::Twalk { fid: 0, newfid: 1, wnames } };
        prop_assert_eq!(t.encode(), Err(Error::TooManyWalk));
        let mut body = (n as u16).to_le_bytes().to_vec();
        body.resize(2 + n * Qid::LEN, 0);
        let r = frame(kind::RWALK, 1, &body);
        prop_assert_eq!(Msg::decode(&r, u32::MAX), Err(Error::TooManyWalk));
    }

    /// Arbitrary bytes, as requests and as replies, never panic the shim;
    /// what it lets through upward is well-formed 9P2000.
    #[test]
    fn shim_never_panics(frames in vec((garbage(), any::<bool>()), 1..8)) {
        let mut shim = shim_with_dir();
        for (bytes, up) in &frames {
            if *up {
                harness::response(&mut shim, bytes);
            } else {
                harness::request(&mut shim, bytes);
            }
        }
        let all: Vec<u8> = frames.into_iter().flat_map(|(b, _)| b).collect();
        harness::dotu(&all);
    }

    /// The shim takes every valid T-message except a `Twstat` whose stat is
    /// not one, and refuses every R-message.
    #[test]
    fn shim_translates_every_request(m in raw()) {
        let mut shim = shim_with_dir();
        let bytes = m.encode();
        let out = harness::request(&mut shim, &bytes);
        match m.fcall() {
            Fcall::Twstat { stat, .. } => {
                prop_assert_eq!(out.is_some(), Dir::unpack(stat).is_ok());
            }
            f if f.kind() % 2 == 1 => prop_assert_eq!(out, None),
            // These have .u fields added.
            Fcall::Tauth { .. } | Fcall::Tattach { .. } => {
                prop_assert_eq!(out.unwrap().len(), bytes.len() + 4);
            }
            Fcall::Tcreate { .. } => prop_assert_eq!(out.unwrap().len(), bytes.len() + 2),
            Fcall::Tversion { .. } => prop_assert!(out.is_some()),
            // A directory read asks for less; see the next property.
            Fcall::Tread { fid: 1, .. } => prop_assert_eq!(out.unwrap().len(), bytes.len()),
            _ => prop_assert_eq!(out, Some(bytes)),
        }
    }

    #[test]
    fn shim_translates_wstat(d in dir(), tag in any::<u16>(), fid in any::<u32>()) {
        let mut shim = Shim::new();
        let stat = d.pack().unwrap();
        let t = Msg { tag, fcall: Fcall::Twstat { fid, stat: &stat } }.encode().unwrap();
        let out = harness::request(&mut shim, &t).unwrap();
        // fid[4] n[2] and then the .u stat.
        prop_assert_eq!(&out[7..11], &fid.to_le_bytes());
        let (back, used) = stat_from_u(&out[13..]).unwrap();
        prop_assert_eq!(used, out.len() - 13);
        prop_assert_eq!(usize::from(u16::from_le_bytes([out[11], out[12]])), used);
        prop_assert_eq!(back, translated(&(d, String::new(), [NONUNAME; 3])));
    }

    /// A .u stat entry becomes a 9P2000 one; `stat_to_u` is its inverse on
    /// 9P2000 mode bits.
    #[test]
    fn ustat_translates(e in uentry()) {
        let u = ustat(&e.0, &e.1, e.2);
        let want = translated(&e);
        prop_assert_eq!(stat_from_u(&u), Ok((want.clone(), u.len())));
        for n in 0..u.len() {
            prop_assert!(stat_from_u(&u[..n]).is_err(), "at {}", n);
        }
        let again = stat_to_u(&want).unwrap();
        prop_assert_eq!(stat_from_u(&again), Ok((want, again.len())));
    }

    /// Every valid R-message with the same layout in both dialects passes
    /// the shim unchanged, solicited or not. (`Rerror`, `Rstat` and
    /// `Rversion "9P2000.u"` differ and have their own properties.)
    #[test]
    fn shim_passes_plain_replies(m in raw_of(prop::sample::select(PLAIN_REPLIES.to_vec()))) {
        prop_assert_eq!(m.fcall().kind() % 2, 1);
        prop_assume!(!matches!(m.fcall(), Fcall::Rversion { version, .. } if version == VERSION_U));
        let mut shim = shim_with_dir();
        let bytes = m.encode();
        prop_assert_eq!(harness::response(&mut shim, &bytes), Some(bytes));
    }

    /// The .u replies that differ translate to plain 9P2000.
    #[test]
    fn shim_translates_u_replies(
        tag in any::<u16>(),
        ename in string(),
        errno in any::<u32>(),
        msize in any::<u32>(),
        e in uentry(),
        n in any::<u16>(),
    ) {
        let mut shim = Shim::new();

        let mut body = (ename.len() as u16).to_le_bytes().to_vec();
        body.extend_from_slice(ename.as_bytes());
        // Without the errno (a 9P2000 Rerror) and with it.
        for with_errno in [false, true] {
            if with_errno {
                body.extend_from_slice(&errno.to_le_bytes());
            }
            let out = harness::response(&mut shim, &frame(kind::RERROR, tag, &body));
            prop_assert_eq!(dec(&out).kind(), kind::RERROR);
        }

        let mut body = msize.to_le_bytes().to_vec();
        body.extend_from_slice(&(VERSION_U.len() as u16).to_le_bytes());
        body.extend_from_slice(VERSION_U.as_bytes());
        let out = harness::response(&mut shim, &frame(kind::RVERSION, tag, &body));
        prop_assert_eq!(dec(&out), Fcall::Rversion { msize, version: VERSION });

        // QEMU's n[2] is 0; any value is ignored.
        let mut body = n.to_le_bytes().to_vec();
        body.extend_from_slice(&ustat(&e.0, &e.1, e.2));
        let out = harness::response(&mut shim, &frame(kind::RSTAT, tag, &body));
        let Fcall::Rstat { stat } = dec(&out) else {
            panic!("not Rstat");
        };
        prop_assert_eq!(Dir::unpack(stat), Ok((translated(&e), stat.len())));
    }

    /// A directory read: the server is asked for no more than the client
    /// asked, and whatever it then sends, the translated reply fits what
    /// the client asked for.
    #[test]
    fn shim_directory_read_fits_count(
        entries in vec(uentry(), 0..12),
        count in 0u32..1500,
        tag in any::<u16>(),
    ) {
        let mut shim = shim_with_dir();
        let tread = Fcall::Tread { fid: 1, offset: 0, count };
        let t = Msg { tag, fcall: tread }.encode().unwrap();
        let down = harness::request(&mut shim, &t).unwrap();
        let Fcall::Tread { fid: 1, offset: 0, count: asked } =
            Msg::decode(&down, u32::MAX).unwrap().fcall
        else {
            panic!("not the Tread");
        };
        prop_assert!(asked <= count);

        // The server sends the whole entries that fit.
        let mut data = Vec::new();
        let mut want = Vec::new();
        for e in &entries {
            let u = ustat(&e.0, &e.1, e.2);
            if data.len() + u.len() > asked as usize {
                break;
            }
            data.extend_from_slice(&u);
            if e.0.name != "." && e.0.name != ".." {
                want.push(translated(e));
            }
        }
        let mut body = (data.len() as u32).to_le_bytes().to_vec();
        body.extend_from_slice(&data);
        let up = harness::response(&mut shim, &frame(kind::RREAD, tag, &body)).unwrap();
        let Fcall::Rread { data } = Msg::decode(&up, u32::MAX).unwrap().fcall else {
            panic!("not Rread");
        };
        prop_assert!(data.len() <= count as usize, "{} bytes for a count of {}", data.len(), count);
        let got: Result<Vec<Dir>, Error> = dirs(data).collect();
        prop_assert_eq!(got, Ok(want));
    }
}
