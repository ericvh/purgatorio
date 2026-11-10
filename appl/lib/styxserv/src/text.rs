//! The one-line text form of a 9P message, as `styxchat`, `styxmon` and
//! `iostats -d` print it. This is the form of Inferno's `Styx` module:
//! `Tmsg.Walk(1,0,1,array[] of {"a"})`, with qids as
//! `Qid(16r1,0,16r80)` and directory entries as `Dir(...)`.

use std::fmt::Write;

use purg_styx::{Dir, Fcall, Msg, Qid};

/// `Qid(16rpath,vers,16rtype)`.
pub fn qid(q: &Qid) -> String {
    format!("Qid(16r{:x},{},16r{:02x})", q.path, q.vers, q.qtype)
}

/// `Dir("name","uid","gid",Qid(...),8rmode,atime,mtime,length,16rtype,dev)`.
pub fn dir(d: &Dir) -> String {
    format!(
        "Dir(\"{}\",\"{}\",\"{}\",{},8r{:o},{},{},{},16r{:x},{})",
        d.name,
        d.uid,
        d.gid,
        qid(&d.qid),
        d.mode,
        d.atime,
        d.mtime,
        d.length,
        d.dtype,
        d.dev
    )
}

fn stat(b: &[u8]) -> String {
    match Dir::unpack(b) {
        Ok((d, _)) => dir(&d),
        Err(_) => String::from("nil"),
    }
}

/// The payload of a read or write, if the message has one.
pub fn payload<'a>(m: &Msg<'a>) -> Option<&'a [u8]> {
    match m.fcall {
        Fcall::Rread { data } | Fcall::Twrite { data, .. } => Some(data),
        _ => None,
    }
}

/// Data as `text("...")` when every byte is printable ASCII, a newline,
/// a tab or a carriage return (those three escaped), else as
/// `dump(hex...)`.
pub fn data(b: &[u8]) -> String {
    let printable = b
        .iter()
        .all(|&c| (0x20..0x7f).contains(&c) || matches!(c, b'\n' | b'\t' | b'\r'));
    if printable {
        let mut o = String::from("text(\"");
        for &c in b {
            match c {
                b'\n' => o.push_str("\\n"),
                b'\t' => o.push_str("\\t"),
                b'\r' => o.push_str("\\r"),
                b'"' => o.push_str("\\\""),
                b'\\' => o.push_str("\\\\"),
                c => o.push(char::from(c)),
            }
        }
        o.push_str("\")");
        return o;
    }
    let mut o = String::from("dump(");
    for x in b {
        let _ = write!(o, "{x:02x}");
    }
    o.push(')');
    o
}

/// One line for `m`.
pub fn msg(m: &Msg<'_>) -> String {
    let t = m.tag;
    match &m.fcall {
        Fcall::Tversion { msize, version } => {
            format!("Tmsg.Version({t},{msize},\"{version}\")")
        }
        Fcall::Tauth { afid, uname, aname } => {
            format!("Tmsg.Auth({t},{afid},\"{uname}\",\"{aname}\")")
        }
        Fcall::Tflush { oldtag } => format!("Tmsg.Flush({t},{oldtag})"),
        Fcall::Tattach {
            fid,
            afid,
            uname,
            aname,
        } => format!("Tmsg.Attach({t},{fid},{afid},\"{uname}\",\"{aname}\")"),
        Fcall::Twalk {
            fid,
            newfid,
            wnames,
        } => {
            let names = if wnames.is_empty() {
                String::from("nil")
            } else {
                let q: Vec<String> = wnames.iter().map(|n| format!("\"{n}\"")).collect();
                format!("array[] of {{{}}}", q.join(","))
            };
            format!("Tmsg.Walk({t},{fid},{newfid},{names})")
        }
        Fcall::Topen { fid, mode } => format!("Tmsg.Open({t},{fid},{mode})"),
        Fcall::Tcreate {
            fid,
            name,
            perm,
            mode,
        } => format!("Tmsg.Create({t},{fid},\"{name}\",8r{perm:o},{mode})"),
        Fcall::Tread { fid, offset, count } => format!("Tmsg.Read({t},{fid},{offset},{count})"),
        Fcall::Twrite { fid, offset, data } => {
            format!(
                "Tmsg.Write({t},{fid},{offset},array[{}] of byte)",
                data.len()
            )
        }
        Fcall::Tclunk { fid } => format!("Tmsg.Clunk({t},{fid})"),
        Fcall::Tremove { fid } => format!("Tmsg.Remove({t},{fid})"),
        Fcall::Tstat { fid } => format!("Tmsg.Stat({t},{fid})"),
        Fcall::Twstat { fid, stat: s } => format!("Tmsg.Wstat({t},{fid},{})", stat(s)),
        Fcall::Rversion { msize, version } => {
            format!("Rmsg.Version({t},{msize},\"{version}\")")
        }
        Fcall::Rauth { aqid } => format!("Rmsg.Auth({t},{})", qid(aqid)),
        Fcall::Rattach { qid: q } => format!("Rmsg.Attach({t},{})", qid(q)),
        Fcall::Rerror { ename } => format!("Rmsg.Error({t},\"{ename}\")"),
        Fcall::Rflush => format!("Rmsg.Flush({t})"),
        Fcall::Rwalk { wqids } => {
            let q: Vec<String> = wqids.iter().map(qid).collect();
            format!("Rmsg.Walk({t},array[] of {{{}}})", q.join(","))
        }
        Fcall::Ropen { qid: q, iounit } => format!("Rmsg.Open({t},{},{iounit})", qid(q)),
        Fcall::Rcreate { qid: q, iounit } => format!("Rmsg.Create({t},{},{iounit})", qid(q)),
        Fcall::Rread { data } => format!("Rmsg.Read({t},array[{}] of byte)", data.len()),
        Fcall::Rwrite { count } => format!("Rmsg.Write({t},{count})"),
        Fcall::Rclunk => format!("Rmsg.Clunk({t})"),
        Fcall::Rremove => format!("Rmsg.Remove({t})"),
        Fcall::Rstat { stat: s } => format!("Rmsg.Stat({t},{})", stat(s)),
        Fcall::Rwstat => format!("Rmsg.Wstat({t})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        let m = Msg {
            tag: 0xffff,
            fcall: Fcall::Tversion {
                msize: 8216,
                version: "9P2000",
            },
        };
        assert_eq!(msg(&m), "Tmsg.Version(65535,8216,\"9P2000\")");
        let m = Msg {
            tag: 2,
            fcall: Fcall::Rwalk {
                wqids: vec![Qid {
                    path: 1,
                    vers: 0,
                    qtype: 0x80,
                }],
            },
        };
        assert_eq!(msg(&m), "Rmsg.Walk(2,array[] of {Qid(16r1,0,16r80)})");
        let m = Msg {
            tag: 3,
            fcall: Fcall::Twalk {
                fid: 0,
                newfid: 1,
                wnames: vec![],
            },
        };
        assert_eq!(msg(&m), "Tmsg.Walk(3,0,1,nil)");
        assert_eq!(data(b"hi\n"), "text(\"hi\\n\")");
        assert_eq!(data(&[0, 1, 0xff]), "dump(0001ff)");
    }
}
