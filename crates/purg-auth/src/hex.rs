//! Lowercase hex, as every text format here uses.

use alloc::string::String;
use alloc::vec::Vec;

const DIGITS: &[u8; 16] = b"0123456789abcdef";

pub fn encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

fn digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Decodes lowercase hex of any even length.
pub fn decode(text: &[u8]) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.chunks(2)
        .map(|p| Some((digit(p[0])? << 4) | digit(p[1])?))
        .collect()
}

/// Decodes exactly `N` bytes of lowercase hex.
pub fn array<const N: usize>(text: &[u8]) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    decode(text)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        assert_eq!(encode(&[0, 0xab, 0xff]), "00abff");
        assert_eq!(decode(b"00abff").unwrap(), [0, 0xab, 0xff]);
        assert_eq!(array::<2>(b"0102"), Some([1, 2]));
        assert_eq!(array::<2>(b"010"), None);
        assert_eq!(decode(b"AB"), None);
        assert_eq!(decode(b"zz"), None);
    }
}
