//! `/net/iproute`: the IP route table, as Inferno's ip(3) shows it.
//!
//! `#I` keeps the routes written to `iproute` here and lists them after
//! the routes each interface implies. The platform applies each change
//! ([`purg_platform::net::Net::add_route`]): smoltcp on the kernel and the
//! probe; nothing on the hosted emu, where the host OS routes.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use purg_platform::net::Ifc;

/// The tag a route gets when its file descriptor set none.
pub const DEFAULT_TAG: &str = "none";
/// The tag of the routes interfaces imply.
const IFC_TAG: &str = "ifc ";

/// A route written to `iproute`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub target: [u8; 4],
    pub mask: [u8; 4],
    pub gateway: [u8; 4],
    /// Up to four characters.
    pub tag: String,
}

/// A route tag: its first four characters, as Inferno keeps them.
pub fn tag(s: &str) -> String {
    s.chars().take(4).collect()
}

/// Whether `flush`'s `tag` names `route`'s tag (Inferno compares them
/// with `strncmp` over four bytes).
pub fn tag_matches(route: &str, tag: &str) -> bool {
    route
        .as_bytes()
        .iter()
        .take(4)
        .eq(tag.as_bytes().iter().take(4))
}

/// `target & mask`.
pub fn network(addr: [u8; 4], mask: [u8; 4]) -> [u8; 4] {
    (u32::from_be_bytes(addr) & u32::from_be_bytes(mask)).to_be_bytes()
}

/// A mask of `prefix` leading ones.
pub fn prefix_mask(prefix: u8) -> [u8; 4] {
    let bits = u32::MAX
        .checked_shl(32 - u32::from(prefix.min(32)))
        .unwrap_or(0);
    bits.to_be_bytes()
}

/// The class A, B or C mask of `a`.
pub fn class_mask(a: [u8; 4]) -> [u8; 4] {
    match a[0] {
        0..=127 => [255, 0, 0, 0],
        128..=191 => [255, 255, 0, 0],
        _ => [255, 255, 255, 0],
    }
}

pub fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for b in &mut out {
        let p = parts.next()?;
        if p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        *b = p.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// A mask as a write gives it: dotted (`255.255.255.0`), or as reads
/// print it, `/n` with `n` counting the 96 bits before an IPv4 address in
/// its IPv6 form (`/120`).
pub fn parse_mask(s: &str) -> Option<[u8; 4]> {
    match s.strip_prefix('/') {
        Some(n) => {
            let n: u8 = n.parse().ok()?;
            (96..=128).contains(&n).then(|| prefix_mask(n - 96))
        }
        None => parse_ip(s),
    }
}

pub fn ip(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

/// A mask as ip(3) prints it: `/n`, `n` being 96 plus the prefix length,
/// since Inferno keeps IPv4 masks in IPv6 form. A mask that is not a
/// prefix is printed dotted.
fn mask_text(mask: [u8; 4]) -> String {
    let ones = u32::from_be_bytes(mask).leading_ones();
    if prefix_mask(ones as u8) == mask {
        format!("/{}", 96 + ones)
    } else {
        ip(mask)
    }
}

/// One line of `iproute`: target, mask, gateway, type, tag and the
/// number of the interface the route leaves by (`-` if none), in
/// Inferno's columns.
fn line(target: [u8; 4], mask: [u8; 4], gw: [u8; 4], kind: &str, tag: &str, ifc: &str) -> String {
    format!(
        "{:<15} {:<4} {:<15} {:>4.4} {:>4.4} {:>3}\n",
        ip(target),
        mask_text(mask),
        ip(gw),
        format!("{kind:<4}"),
        tag,
        ifc
    )
}

/// The routes an interface address implies, as Inferno's ipifc adds them:
/// its network (type `4i`, or `4ip` for a /32), its own address (`4u`),
/// then the broadcast and network addresses of its subnet and of its
/// class, and the all-ones broadcast (`4b`), each once.
fn ifc_routes(n: usize, i: &Ifc, out: &mut String) {
    let mask = prefix_mask(i.prefix);
    let net = network(i.addr, mask);
    let ifc = format!("{n}");
    let kind = if i.prefix == 32 { "4ip" } else { "4i" };
    out.push_str(&line(net, mask, net, kind, IFC_TAG, &ifc));
    let all = [255; 4];
    out.push_str(&line(i.addr, all, i.addr, "4u", IFC_TAG, &ifc));
    let bcast =
        |a: [u8; 4], m: [u8; 4]| (u32::from_be_bytes(a) | !u32::from_be_bytes(m)).to_be_bytes();
    let cmask = class_mask(i.addr);
    let mut seen: Vec<[u8; 4]> = Vec::new();
    for a in [
        bcast(i.addr, mask),
        net,
        bcast(i.addr, cmask),
        network(i.addr, cmask),
        all,
    ] {
        if a == i.addr || seen.contains(&a) {
            continue;
        }
        seen.push(a);
        out.push_str(&line(a, all, a, "4b", IFC_TAG, &ifc));
    }
}

/// The interface on whose network `gw` lies, if any.
fn ifc_for(ifcs: &[Ifc], gw: [u8; 4]) -> Option<usize> {
    ifcs.iter().position(|i| {
        let mask = prefix_mask(i.prefix);
        network(i.addr, mask) == network(gw, mask)
    })
}

/// The text of `iproute`: the interfaces' routes, then those written.
pub fn text(ifcs: &[Ifc], routes: &[Route]) -> String {
    let mut out = String::new();
    for (n, i) in ifcs.iter().enumerate() {
        ifc_routes(n, i, &mut out);
    }
    for r in routes {
        let ifc = ifc_for(ifcs, r.gateway).map_or_else(|| String::from("-"), |n| format!("{n}"));
        out.push_str(&line(r.target, r.mask, r.gateway, "4", &r.tag, &ifc));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ifc() -> Ifc {
        Ifc {
            device: "ether0".into(),
            mtu: 1500,
            mac: [0; 6],
            addr: [10, 0, 2, 15],
            prefix: 24,
            gateway: None,
        }
    }

    #[test]
    fn lists_interface_and_written_routes() {
        let routes = [
            Route {
                target: [0; 4],
                mask: [0; 4],
                gateway: [10, 0, 2, 2],
                tag: tag("none"),
            },
            Route {
                target: [192, 0, 2, 0],
                mask: [255, 255, 255, 0],
                gateway: [198, 51, 100, 1],
                tag: tag("rip"),
            },
        ];
        let want = concat!(
            "10.0.2.0        /120 10.0.2.0        4i   ifc    0\n",
            "10.0.2.15       /128 10.0.2.15       4u   ifc    0\n",
            "10.0.2.255      /128 10.0.2.255      4b   ifc    0\n",
            "10.0.2.0        /128 10.0.2.0        4b   ifc    0\n",
            "10.255.255.255  /128 10.255.255.255  4b   ifc    0\n",
            "10.0.0.0        /128 10.0.0.0        4b   ifc    0\n",
            "255.255.255.255 /128 255.255.255.255 4b   ifc    0\n",
            "0.0.0.0         /96  10.0.2.2        4    none   0\n",
            "192.0.2.0       /120 198.51.100.1    4     rip   -\n",
        );
        assert_eq!(text(&[ifc()], &routes), want);
    }

    #[test]
    fn parses_masks_and_tags() {
        assert_eq!(parse_mask("/120"), Some([255, 255, 255, 0]));
        assert_eq!(parse_mask("/96"), Some([0; 4]));
        assert_eq!(parse_mask("/24"), None);
        assert_eq!(parse_mask("255.255.0.0"), Some([255, 255, 0, 0]));
        assert_eq!(parse_ip("1.2.3"), None);
        assert_eq!(parse_ip("1.2.3.x"), None);
        assert_eq!(tag("toolong"), "tool");
        assert!(tag_matches("rip", "rip"));
        assert!(tag_matches("tool", "toolong"));
        assert!(!tag_matches("none", "rip"));
        assert_eq!(mask_text([255, 0, 255, 0]), "255.0.255.0");
    }
}
