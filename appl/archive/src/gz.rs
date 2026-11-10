//! gzip streams (RFC 1952) over raw deflate from `miniz_oxide`.
//!
//! The header gzip writes is fixed: no file name, a zero modification time
//! and OS 255 (unknown), so the same input always gives the same bytes.

use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

const ID: [u8; 2] = [0x1f, 0x8b];
const CM_DEFLATE: u8 = 8;
const FHCRC: u8 = 0x02;
const FEXTRA: u8 = 0x04;
const FNAME: u8 = 0x08;
const FCOMMENT: u8 = 0x10;
const FRESERVED: u8 = 0xe0;

/// The default compression level.
pub const DEFAULT_LEVEL: u8 = 6;

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC: [u32; 256] = crc_table();

/// The CRC-32 gzip stores in its trailer.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// Whether `data` starts with the gzip magic number.
pub fn is_gzip(data: &[u8]) -> bool {
    data.starts_with(&ID)
}

/// Compresses `data` into one gzip member at `level` (1 to 9).
pub fn compress(data: &[u8], level: u8) -> Vec<u8> {
    let level = level.clamp(1, 9);
    let xfl = match level {
        9 => 2,
        1 => 4,
        _ => 0,
    };
    let body = miniz_oxide::deflate::compress_to_vec(data, level);
    let mut out = Vec::with_capacity(body.len() + 18);
    out.extend_from_slice(&ID);
    out.extend_from_slice(&[CM_DEFLATE, 0, 0, 0, 0, 0, xfl, 255]);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// Skips a gzip member header, returning the offset of the deflate data.
fn header(data: &[u8]) -> Result<usize, &'static str> {
    if data.len() < 10 || !is_gzip(data) {
        return Err("not in gzip format");
    }
    if data[2] != CM_DEFLATE {
        return Err("unknown compression method");
    }
    let flg = data[3];
    if flg & FRESERVED != 0 {
        return Err("unknown header flags");
    }
    let short = "unexpected end of input";
    let mut p = 10;
    if flg & FEXTRA != 0 {
        let x = data.get(p..p + 2).ok_or(short)?;
        p += 2 + usize::from(u16::from_le_bytes([x[0], x[1]]));
    }
    for f in [FNAME, FCOMMENT] {
        if flg & f != 0 {
            let n = data.get(p..).ok_or(short)?.iter().position(|&b| b == 0);
            p += n.ok_or(short)? + 1;
        }
    }
    if flg & FHCRC != 0 {
        p += 2;
    }
    if p > data.len() {
        return Err(short);
    }
    Ok(p)
}

/// Decompresses a gzip stream of one or more members. Zero bytes after the
/// last member (tape padding) are ignored.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::new();
    let mut p = 0;
    loop {
        p += header(&data[p..])?;
        let start = out.len();
        let mut st = InflateState::new_boxed(DataFormat::Raw);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let r = inflate(&mut st, &data[p..], &mut buf, MZFlush::None);
            p += r.bytes_consumed;
            out.extend_from_slice(&buf[..r.bytes_written]);
            match r.status {
                Ok(MZStatus::StreamEnd) => break,
                Ok(_) => {}
                Err(MZError::Buf) if r.bytes_consumed == 0 && r.bytes_written == 0 => {
                    return Err("unexpected end of input");
                }
                Err(MZError::Buf) => {}
                Err(_) => return Err("corrupt input"),
            }
        }
        let t = data.get(p..p + 8).ok_or("unexpected end of input")?;
        let crc = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
        let len = u32::from_le_bytes([t[4], t[5], t[6], t[7]]);
        p += 8;
        if crc != crc32(&out[start..]) {
            return Err("crc error");
        }
        if len != (out.len() - start) as u32 {
            return Err("length error");
        }
        if data[p..].iter().all(|&b| b == 0) {
            return Ok(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn round_trip() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
        for level in 1..=9 {
            let z = compress(&data, level);
            assert_eq!(decompress(&z).unwrap(), data, "level {level}");
        }
        assert_eq!(decompress(&compress(b"", 6)).unwrap(), b"");
    }

    #[test]
    fn fixed_header() {
        let z = compress(b"hello\n", 6);
        assert_eq!(&z[..10], &[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255]);
    }

    #[test]
    fn members_and_padding() {
        let mut z = compress(b"one ", 6);
        z.extend(compress(b"two", 9));
        z.extend([0u8; 100]);
        assert_eq!(decompress(&z).unwrap(), b"one two");
    }

    #[test]
    fn errors() {
        assert_eq!(decompress(b"plain text"), Err("not in gzip format"));
        let z = compress(b"hello hello hello", 6);
        assert_eq!(
            decompress(&z[..z.len() - 3]),
            Err("unexpected end of input")
        );
        let mut bad = z.clone();
        let n = bad.len();
        bad[n - 8] ^= 1;
        assert_eq!(decompress(&bad), Err("crc error"));
    }
}
