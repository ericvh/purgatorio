//! The bodies of the fuzz targets, kept apart from libFuzzer so that
//! `cargo test -p purg-styx` runs the same checks without it: the tests
//! `fuzz_smoke` and `prop` include this file as a module.
//!
//! Each function takes arbitrary bytes and panics only when the codec
//! breaks one of its promises.

use purg_styx::dotu::{Shim, stat_from_u, stat_to_u};
use purg_styx::{Dir, Fcall, Msg, dirs, message_size};

/// The decoder: `Msg::decode`, `Dir::unpack`, `dirs` and `stat_from_u`.
pub fn decode(data: &[u8]) {
    for msize in [u32::MAX, 8192, 64] {
        decode_msg(data, msize);
    }
    decode_dir(data);
}

/// A message that decodes fits `msize`, fills the buffer, and encodes back
/// to the same bytes.
fn decode_msg(data: &[u8], msize: u32) {
    let Ok(msg) = Msg::decode(data, msize) else {
        return;
    };
    assert!(data.len() <= msize as usize, "decoded past msize {msize}");
    assert_eq!(message_size(data), Some(data.len()));
    assert_eq!(msg.encode().expect("a decoded message encodes"), data);
    match msg.fcall {
        Fcall::Rstat { stat } | Fcall::Twstat { stat, .. } => decode_dir(stat),
        Fcall::Rread { data } | Fcall::Twrite { data, .. } => decode_dir(data),
        _ => {}
    }
}

/// A stat entry that unpacks stays inside the buffer and packs back to the
/// same bytes.
fn decode_dir(data: &[u8]) {
    if let Ok((d, used)) = Dir::unpack(data) {
        assert!(used <= data.len());
        assert_eq!(d.packed_len(), used);
        assert_eq!(d.pack().expect("an unpacked Dir packs"), &data[..used]);
    }
    // The iterator stops at the first error and never runs past the end.
    let mut left = data.len();
    for d in dirs(data) {
        match d {
            Ok(d) => {
                left = left
                    .checked_sub(d.packed_len())
                    .expect("dirs ran past the end");
            }
            Err(_) => break,
        }
    }
    if let Ok((d, used)) = stat_from_u(data) {
        assert!(used <= data.len());
        // Numeric ids can make the names too long to pack; that is an
        // error, not a panic.
        if let Ok(u) = stat_to_u(&d) {
            let (again, n) = stat_from_u(&u).expect("stat_to_u output is a .u stat");
            assert_eq!((again, n), (d, u.len()));
        }
    }
}

/// The 9P2000.u shim. `data` is a conversation: messages one after the
/// other, each framed by its `size[4]`. T-messages (even type byte) go to
/// [`Shim::request`], everything else to [`Shim::response`]. Bytes that do
/// not frame are fed whole.
pub fn dotu(data: &[u8]) {
    let mut shim = Shim::new();
    let mut rest = data;
    while !rest.is_empty() {
        let n = match message_size(rest) {
            Some(n) if (7..=rest.len()).contains(&n) => n,
            _ => rest.len(),
        };
        let (frame, tail) = rest.split_at(n);
        rest = tail;
        if frame.len() > 4 && frame[4] % 2 == 0 {
            request(&mut shim, frame);
        } else {
            response(&mut shim, frame);
        }
    }
}

/// Feeds one client message to the shim. What it accepts was 9P2000, and
/// what it produces is one framed message of the same type and tag.
pub fn request(shim: &mut Shim, t: &[u8]) -> Option<Vec<u8>> {
    let out = shim.request(t).ok()?;
    let msg = Msg::decode(t, u32::MAX).expect("the shim accepted a bad T-message");
    assert_eq!(msg.fcall.kind() % 2, 0, "the shim accepted an R-message");
    assert_eq!(message_size(&out), Some(out.len()));
    assert_eq!(out[4..7], t[4..7], "type or tag changed");
    Some(out)
}

/// Feeds one server message to the shim. What it produces is a 9P2000
/// R-message of the same type and tag.
pub fn response(shim: &mut Shim, r: &[u8]) -> Option<Vec<u8>> {
    let out = shim.response(r).ok()?;
    let msg = Msg::decode(&out, u32::MAX).expect("the shim produced a bad R-message");
    assert_eq!(msg.fcall.kind() % 2, 1, "the shim produced a T-message");
    assert_eq!(out[4..7], r[4..7], "type or tag changed");
    Some(out)
}
