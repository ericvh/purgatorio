//! DNS messages (RFC 1035): enough to build a query and read the answers,
//! and, for test responders, to build a reply.

/// The record types `ndb/dns` knows, by their ndb attribute names.
pub const TYPES: &[(&str, u16)] = &[
    ("ip", 1),
    ("ns", 2),
    ("cname", 5),
    ("soa", 6),
    ("ptr", 12),
    ("hinfo", 13),
    ("mx", 15),
    ("txt", 16),
    ("ipv6", 28),
];

pub const CLASS_IN: u16 = 1;
pub const RCODE_NXDOMAIN: u8 = 3;

/// The type number for an attribute name.
pub fn qtype(name: &str) -> Option<u16> {
    TYPES.iter().find(|t| t.0 == name).map(|t| t.1)
}

/// The attribute name for a type number.
pub fn type_name(t: u16) -> Option<&'static str> {
    TYPES.iter().find(|x| x.1 == t).map(|x| x.0)
}

/// One resource record, its data already put as text: `10.1.2.3`,
/// `10 mail.example.org`, a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rr {
    pub name: String,
    pub rtype: u16,
    pub ttl: u32,
    pub data: String,
}

/// A decoded reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub id: u16,
    pub rcode: u8,
    pub answers: Vec<Rr>,
}

/// A recursive query for `name` of type `qtype`.
pub fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut m = Vec::with_capacity(32 + name.len());
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&[0x01, 0x00]); // RD
    m.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
    put_name(&mut m, name);
    m.extend_from_slice(&qtype.to_be_bytes());
    m.extend_from_slice(&CLASS_IN.to_be_bytes());
    m
}

/// Appends `name` as labels.
pub fn put_name(m: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() {
            continue;
        }
        let l = &label.as_bytes()[..label.len().min(63)];
        m.push(l.len() as u8);
        m.extend_from_slice(l);
    }
    m.push(0);
}

fn u16_at(m: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*m.get(i)?, *m.get(i + 1)?]))
}

fn u32_at(m: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *m.get(i)?,
        *m.get(i + 1)?,
        *m.get(i + 2)?,
        *m.get(i + 3)?,
    ]))
}

/// Reads a possibly compressed name at `i`. Returns it and the offset
/// just past it in the message.
pub fn get_name(m: &[u8], mut i: usize) -> Option<(String, usize)> {
    let mut out: Vec<String> = Vec::new();
    let mut end = None;
    for _ in 0..128 {
        let len = *m.get(i)? as usize;
        if len == 0 {
            let next = end.unwrap_or(i + 1);
            return Some((out.join("."), next));
        }
        if len & 0xc0 == 0xc0 {
            let ptr = ((len & 0x3f) << 8) | *m.get(i + 1)? as usize;
            end.get_or_insert(i + 2);
            i = ptr;
            continue;
        }
        let label = m.get(i + 1..i + 1 + len)?;
        out.push(String::from_utf8_lossy(label).into_owned());
        i += 1 + len;
    }
    None
}

/// The question's name and type.
pub fn question(m: &[u8]) -> Option<(u16, String, u16)> {
    let id = u16_at(m, 0)?;
    if u16_at(m, 4)? < 1 {
        return None;
    }
    let (name, i) = get_name(m, 12)?;
    Some((id, name, u16_at(m, i)?))
}

/// Decodes a reply, keeping the answer records.
pub fn parse(m: &[u8]) -> Option<Reply> {
    let id = u16_at(m, 0)?;
    let flags = u16_at(m, 2)?;
    if flags & 0x8000 == 0 {
        return None;
    }
    let qd = u16_at(m, 4)?;
    let an = u16_at(m, 6)?;
    let mut i = 12;
    for _ in 0..qd {
        let (_, next) = get_name(m, i)?;
        i = next + 4;
    }
    let mut answers = Vec::new();
    for _ in 0..an {
        let (name, next) = get_name(m, i)?;
        let rtype = u16_at(m, next)?;
        let ttl = u32_at(m, next + 4)?;
        let len = u16_at(m, next + 8)? as usize;
        let start = next + 10;
        let rdata = m.get(start..start + len)?;
        if let Some(data) = rdata_text(m, start, rtype, rdata) {
            answers.push(Rr {
                name,
                rtype,
                ttl,
                data,
            });
        }
        i = start + len;
    }
    Some(Reply {
        id,
        rcode: (flags & 0xf) as u8,
        answers,
    })
}

fn rdata_text(m: &[u8], start: usize, rtype: u16, rd: &[u8]) -> Option<String> {
    Some(match rtype {
        1 if rd.len() == 4 => format!("{}.{}.{}.{}", rd[0], rd[1], rd[2], rd[3]),
        28 if rd.len() == 16 => {
            let w: Vec<String> = rd
                .chunks(2)
                .map(|c| format!("{:x}", u16::from_be_bytes([c[0], c[1]])))
                .collect();
            w.join(":")
        }
        2 | 5 | 12 => get_name(m, start)?.0,
        15 => format!("{} {}", u16_at(m, start)?, get_name(m, start + 2)?.0),
        6 => {
            let (mname, i) = get_name(m, start)?;
            let (rname, i) = get_name(m, i)?;
            let n: Vec<String> = (0..5)
                .map(|k| u32_at(m, i + 4 * k).map(|v| v.to_string()))
                .collect::<Option<_>>()?;
            format!("{mname} {rname} {}", n.join(" "))
        }
        13 | 16 => {
            let mut parts = Vec::new();
            let mut i = 0;
            while i < rd.len() {
                let l = rd[i] as usize;
                let s = rd.get(i + 1..i + 1 + l)?;
                parts.push(quote(&String::from_utf8_lossy(s)));
                i += 1 + l;
            }
            parts.join(" ")
        }
        _ => return None,
    })
}

fn quote(s: &str) -> String {
    crate::quote(s)
}

/// `10.1.2.3` as `3.2.1.10.in-addr.arpa`, for a `ptr` query.
pub fn arpa(ip: &str) -> Option<String> {
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() != 4 || parts.iter().any(|p| p.parse::<u8>().is_err()) {
        return None;
    }
    let mut r: Vec<&str> = parts.into_iter().rev().collect();
    r.push("in-addr.arpa");
    Some(r.join("."))
}

/// Builds a reply to the query `q` with the records `answers` (each a
/// name, type and wire-format data) and `rcode`. Used by test responders.
pub fn reply(q: &[u8], rcode: u8, answers: &[(String, u16, Vec<u8>)]) -> Option<Vec<u8>> {
    let (id, name, qt) = question(q)?;
    let mut m = Vec::new();
    m.extend_from_slice(&id.to_be_bytes());
    m.push(0x81); // QR, RD
    m.push(0x80 | (rcode & 0xf)); // RA
    m.extend_from_slice(&1u16.to_be_bytes());
    m.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    m.extend_from_slice(&[0, 0, 0, 0]);
    put_name(&mut m, &name);
    m.extend_from_slice(&qt.to_be_bytes());
    m.extend_from_slice(&CLASS_IN.to_be_bytes());
    for (n, t, data) in answers {
        put_name(&mut m, n);
        m.extend_from_slice(&t.to_be_bytes());
        m.extend_from_slice(&CLASS_IN.to_be_bytes());
        m.extend_from_slice(&300u32.to_be_bytes());
        m.extend_from_slice(&(data.len() as u16).to_be_bytes());
        m.extend_from_slice(data);
    }
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_reply_round_trip() {
        let q = query(7, "www.example.test", 1);
        assert_eq!(question(&q), Some((7, String::from("www.example.test"), 1)));
        let mut mx = 10u16.to_be_bytes().to_vec();
        put_name(&mut mx, "mail.example.test");
        let mut cname = Vec::new();
        put_name(&mut cname, "www.example.test");
        let r = reply(
            &q,
            0,
            &[
                (String::from("alias.example.test"), 5, cname),
                (String::from("www.example.test"), 1, vec![10, 1, 2, 3]),
                (String::from("example.test"), 15, mx),
                (String::from("t.example.test"), 16, b"\x05hello".to_vec()),
            ],
        )
        .unwrap();
        let p = parse(&r).unwrap();
        assert_eq!(p.id, 7);
        assert_eq!(p.rcode, 0);
        let got: Vec<(&str, u16, &str)> = p
            .answers
            .iter()
            .map(|a| (a.name.as_str(), a.rtype, a.data.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("alias.example.test", 5, "www.example.test"),
                ("www.example.test", 1, "10.1.2.3"),
                ("example.test", 15, "10 mail.example.test"),
                ("t.example.test", 16, "hello"),
            ]
        );
    }

    #[test]
    fn compressed_names_and_arpa() {
        // "a.b" then a pointer to it.
        let m = [0, 0, 1, b'a', 1, b'b', 0, 0xc0, 2];
        assert_eq!(get_name(&m, 2), Some((String::from("a.b"), 7)));
        assert_eq!(get_name(&m, 7), Some((String::from("a.b"), 9)));
        assert_eq!(arpa("10.1.2.3").as_deref(), Some("3.2.1.10.in-addr.arpa"));
        assert_eq!(arpa("x"), None);
        assert_eq!(qtype("mx"), Some(15));
        assert_eq!(type_name(28), Some("ipv6"));
    }
}
