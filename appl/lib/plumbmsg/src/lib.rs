//! Plumbing messages as plumbmsg(2) packs them: five lines (source,
//! destination, directory, kind, attributes), a line with the length of
//! the data in decimal, then the data. Attributes are `name=value` pairs
//! separated by tabs.

/// The plumber's input file.
pub const INPUT: &str = "/chan/plumb.input";

/// The file a program reads to receive messages for `port`.
pub fn port_file(port: &str) -> String {
    format!("/chan/plumb.{port}")
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Msg {
    pub src: String,
    pub dst: String,
    pub dir: String,
    pub kind: String,
    pub attr: String,
    pub data: Vec<u8>,
}

impl Msg {
    pub fn pack(&self) -> Vec<u8> {
        let mut out = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            self.src,
            self.dst,
            self.dir,
            self.kind,
            self.attr,
            self.data.len()
        )
        .into_bytes();
        out.extend_from_slice(&self.data);
        out
    }

    /// The message packed in `b`, or `None` if it is malformed.
    pub fn unpack(b: &[u8]) -> Option<Msg> {
        let mut fields = Vec::new();
        let mut rest = b;
        for _ in 0..6 {
            let nl = rest.iter().position(|&c| c == b'\n')?;
            fields.push(String::from_utf8_lossy(&rest[..nl]).into_owned());
            rest = &rest[nl + 1..];
        }
        let n: usize = fields[5].trim().parse().ok()?;
        if rest.len() < n {
            return None;
        }
        let mut f = fields.into_iter();
        Some(Msg {
            src: f.next()?,
            dst: f.next()?,
            dir: f.next()?,
            kind: f.next()?,
            attr: f.next()?,
            data: rest[..n].to_vec(),
        })
    }
}

/// Splits an attribute string into `(name, value)` pairs. A value may be
/// quoted with `'`, with `''` for a quote inside.
pub fn string2attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return out;
        }
        let mut name = String::new();
        while let Some(c) = chars.next_if(|&c| c != '=' && !c.is_whitespace()) {
            name.push(c);
        }
        let mut val = String::new();
        if chars.next_if_eq(&'=').is_some() {
            if chars.next_if_eq(&'\'').is_some() {
                while let Some(c) = chars.next() {
                    if c == '\'' && chars.next_if_eq(&'\'').is_none() {
                        break;
                    }
                    val.push(c);
                }
            } else {
                while let Some(c) = chars.next_if(|&c| c != '\t') {
                    val.push(c);
                }
            }
        }
        out.push((name, val));
    }
}

/// Joins `(name, value)` pairs with tabs, quoting values that need it.
pub fn attrs2string(attrs: &[(String, String)]) -> String {
    attrs
        .iter()
        .map(|(n, v)| {
            if v.contains(|c: char| c.is_whitespace() || c == '\'') {
                format!("{n}='{}'", v.replace('\'', "''"))
            } else {
                format!("{n}={v}")
            }
        })
        .collect::<Vec<_>>()
        .join("\t")
}

/// The value of attribute `name`.
pub fn lookup<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack() {
        let m = Msg {
            src: String::from("plumb"),
            dst: String::new(),
            dir: String::from("/tmp"),
            kind: String::from("text"),
            attr: String::from("a=1"),
            data: b"two\nlines".to_vec(),
        };
        let b = m.pack();
        assert_eq!(b, b"plumb\n\n/tmp\ntext\na=1\n9\ntwo\nlines");
        assert_eq!(Msg::unpack(&b), Some(m));
        assert_eq!(Msg::unpack(b"a\nb\n"), None);
        assert_eq!(Msg::unpack(b"a\nb\nc\nd\ne\n9\nshort"), None);
    }

    #[test]
    fn attrs() {
        let a = vec![
            (String::from("action"), String::from("showdata")),
            (String::from("x"), String::from("a b")),
        ];
        let s = attrs2string(&a);
        assert_eq!(s, "action=showdata\tx='a b'");
        assert_eq!(string2attrs(&s), a);
        assert_eq!(lookup(&a, "x"), Some("a b"));
        assert_eq!(lookup(&a, "y"), None);
    }
}
