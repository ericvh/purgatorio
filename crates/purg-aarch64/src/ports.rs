//! The choice of an ephemeral port for the kernel's network stack, kept
//! apart from the stack so the host can test it.

/// The first port of the dynamic range (RFC 6335): ports from here to
/// 65535 are handed out for dials and for announces of port 0.
pub const FIRST_EPHEMERAL: u16 = 49152;

/// The next ephemeral port at or after `*next` that `in_use` does not
/// claim, wrapping from 65535 back to [`FIRST_EPHEMERAL`]; `*next` moves
/// past it. With every port in use, the one `*next` named is returned,
/// and the bind that follows reports the clash.
pub fn next_free(next: &mut u16, in_use: impl Fn(u16) -> bool) -> u16 {
    let first = (*next).max(FIRST_EPHEMERAL);
    let mut port = first;
    loop {
        let after = port.checked_add(1).unwrap_or(FIRST_EPHEMERAL);
        if !in_use(port) {
            *next = after;
            return port;
        }
        port = after;
        if port == first {
            *next = first.checked_add(1).unwrap_or(FIRST_EPHEMERAL);
            return first;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_up_and_skips_ports_in_use() {
        let mut next = FIRST_EPHEMERAL;
        assert_eq!(next_free(&mut next, |_| false), 49152);
        assert_eq!(next_free(&mut next, |p| p == 49153 || p == 49154), 49155);
        assert_eq!(next, 49156);
    }

    #[test]
    fn wraps_past_the_top_and_still_skips() {
        let mut next = u16::MAX;
        assert_eq!(next_free(&mut next, |_| false), u16::MAX);
        assert_eq!(next, FIRST_EPHEMERAL);
        let mut next = u16::MAX;
        assert_eq!(next_free(&mut next, |p| p == u16::MAX || p == 49152), 49153);
    }

    #[test]
    fn every_port_in_use_gives_the_next_one_back() {
        let mut next = 50000;
        assert_eq!(next_free(&mut next, |_| true), 50000);
        assert_eq!(next, 50001);
    }
}
