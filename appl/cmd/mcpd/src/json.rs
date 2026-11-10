//! A small JSON reader and writer (RFC 8259), enough for JSON-RPC.
//!
//! Objects keep their members in order. Integers that fit an `i64` stay
//! exact, so a request id is echoed as it was sent. The writer never emits
//! a raw newline, which the MCP stdio transport forbids inside a message.

use std::fmt::Write;

/// Nesting deeper than this is refused, so hostile input cannot overflow
/// the stack.
const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Member `key` of an object.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// A whole number, including a float with no fraction (`3.0`).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            Value::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Some(*f as i64),
            _ => None,
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    /// The JSON type name, for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Int(_) => "integer",
            Value::Float(_) => "number",
            Value::Str(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    /// The compact, single-line encoding.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Float(f) if f.is_finite() => {
                let _ = write!(out, "{f}");
            }
            Value::Float(_) => out.push_str("null"),
            Value::Str(s) => write_str(s, out),
            Value::Array(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Value::Object(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Other controls, DEL, and the two line separators some readers
            // treat as newlines.
            c if (c as u32) < 0x20 || c == '\u{7f}' || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(String::from(s))
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Value {
        Value::Int(n)
    }
}

impl From<u64> for Value {
    fn from(n: u64) -> Value {
        match i64::try_from(n) {
            Ok(n) => Value::Int(n),
            Err(_) => Value::Float(n as f64),
        }
    }
}

impl From<u32> for Value {
    fn from(n: u32) -> Value {
        Value::Int(i64::from(n))
    }
}

impl From<usize> for Value {
    fn from(n: usize) -> Value {
        Value::from(n as u64)
    }
}

impl From<Vec<Value>> for Value {
    fn from(a: Vec<Value>) -> Value {
        Value::Array(a)
    }
}

/// Builds an object: `obj([("a", 1.into()), ...])`.
pub fn obj<const N: usize>(members: [(&str, Value); N]) -> Value {
    Value::Object(
        members
            .into_iter()
            .map(|(k, v)| (String::from(k), v))
            .collect(),
    )
}

/// Parses one JSON text; anything but white space after it is an error.
pub fn parse(src: &str) -> Result<Value, String> {
    let mut p = Parser {
        src: src.as_bytes(),
        text: src,
        pos: 0,
    };
    p.space();
    let v = p.value(0)?;
    p.space();
    if p.pos != p.src.len() {
        return Err(p.err("unexpected text after the value"));
    }
    Ok(v)
}

struct Parser<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, msg: &str) -> String {
        format!("{msg} at byte {}", self.pos)
    }

    fn space(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.src.get(self.pos) {
            self.pos += 1;
        }
    }

    fn literal(&mut self, word: &str, v: Value) -> Result<Value, String> {
        if self.src[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(self.err("bad literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(self.err("nested too deeply"));
        }
        match self.src.get(self.pos) {
            None => Err(self.err("unexpected end of text")),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'"') => self.string().map(Value::Str),
            Some(b'[') => {
                self.pos += 1;
                let mut items = Vec::new();
                self.space();
                if self.src.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                loop {
                    self.space();
                    items.push(self.value(depth + 1)?);
                    self.space();
                    match self.src.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(Value::Array(items));
                        }
                        _ => return Err(self.err("expected , or ]")),
                    }
                }
            }
            Some(b'{') => {
                self.pos += 1;
                let mut members: Vec<(String, Value)> = Vec::new();
                self.space();
                if self.src.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                    return Ok(Value::Object(members));
                }
                loop {
                    self.space();
                    if self.src.get(self.pos) != Some(&b'"') {
                        return Err(self.err("expected a member name"));
                    }
                    let key = self.string()?;
                    self.space();
                    if self.src.get(self.pos) != Some(&b':') {
                        return Err(self.err("expected :"));
                    }
                    self.pos += 1;
                    self.space();
                    let v = self.value(depth + 1)?;
                    // A repeated name keeps its last value.
                    match members.iter_mut().find(|(k, _)| *k == key) {
                        Some(m) => m.1 = v,
                        None => members.push((key, v)),
                    }
                    self.space();
                    match self.src.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(Value::Object(members));
                        }
                        _ => return Err(self.err("expected , or }")),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while let Some(b'0'..=b'9') = self.src.get(self.pos) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.src.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        let int_start = self.pos;
        let n = self.digits();
        if n == 0 || (n > 1 && self.src[int_start] == b'0') {
            return Err(self.err("bad number"));
        }
        let mut float = false;
        if self.src.get(self.pos) == Some(&b'.') {
            float = true;
            self.pos += 1;
            if self.digits() == 0 {
                return Err(self.err("bad number"));
            }
        }
        if let Some(b'e' | b'E') = self.src.get(self.pos) {
            float = true;
            self.pos += 1;
            if let Some(b'+' | b'-') = self.src.get(self.pos) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.err("bad number"));
            }
        }
        let text = &self.text[start..self.pos];
        if !float && let Ok(n) = text.parse::<i64>() {
            return Ok(Value::Int(n));
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Value::Float(f)),
            _ => Err(self.err("number out of range")),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self
            .src
            .get(self.pos..self.pos + 4)
            .and_then(|h| std::str::from_utf8(h).ok())
            .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| self.err("bad \\u escape"))?;
        self.pos += 4;
        u32::from_str_radix(h, 16).map_err(|_| self.err("bad \\u escape"))
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(&b) = self.src.get(self.pos) {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // The source is UTF-8 and the scan stops only at ASCII bytes,
            // so both ends are character boundaries.
            out.push_str(&self.text[start..self.pos]);
            match self.src.get(self.pos) {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let c = *self
                        .src
                        .get(self.pos)
                        .ok_or_else(|| self.err("unterminated string"))?;
                    self.pos += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xd800..0xdc00).contains(&hi) {
                                if self.src.get(self.pos..self.pos + 2) != Some(b"\\u") {
                                    return Err(self.err("unpaired surrogate"));
                                }
                                self.pos += 2;
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return Err(self.err("unpaired surrogate"));
                                }
                                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                            } else {
                                hi
                            };
                            out.push(
                                char::from_u32(cp).ok_or_else(|| self.err("unpaired surrogate"))?,
                            );
                        }
                        _ => return Err(self.err("bad escape")),
                    }
                }
                Some(_) => return Err(self.err("control character in a string")),
            }
        }
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (RFC 4648).
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Decodes standard base64. White space is skipped and padding may be
/// left off.
pub fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits, mut pad) = (0u32, 0u32, 0usize);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad += 1;
                continue;
            }
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            _ => return Err(String::from("bad base64 character")),
        };
        if pad > 0 {
            return Err(String::from("base64 data after padding"));
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if bits >= 6 || acc != 0 || pad > 2 {
        return Err(String::from("truncated base64"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let src = r#"{"a":[1,-2,3.5,1e3,true,false,null],"b":{"c":"d"},"e":""}"#;
        let v = parse(src).unwrap();
        assert_eq!(v.get("a").unwrap().type_name(), "array");
        assert_eq!(
            v.get("b").and_then(|b| b.get("c")).unwrap().as_str(),
            Some("d")
        );
        assert_eq!(
            v.encode(),
            r#"{"a":[1,-2,3.5,1000,true,false,null],"b":{"c":"d"},"e":""}"#
        );
        assert_eq!(parse(&v.encode()).unwrap().encode(), v.encode());
    }

    #[test]
    fn ids_stay_exact() {
        assert_eq!(
            parse("9007199254740993").unwrap().encode(),
            "9007199254740993"
        );
        assert_eq!(parse("-0").unwrap(), Value::Int(0));
        assert_eq!(parse("3.0").unwrap().as_i64(), Some(3));
        assert_eq!(parse("3.5").unwrap().as_i64(), None);
    }

    #[test]
    fn strings() {
        let v = parse(r#""a\"b\\c\/d\n\t\u00e9\ud83d\ude00\u0000""#).unwrap();
        assert_eq!(v.as_str(), Some("a\"b\\c/d\n\t\u{e9}\u{1f600}\0"));
        // Nothing the writer emits contains a raw newline or control.
        let out = Value::from("x\ny\r\u{1}\u{2028}\u{7f}é\u{1f600}").encode();
        assert_eq!(out, "\"x\\ny\\r\\u0001\\u2028\\u007f\u{e9}\u{1f600}\"");
        assert_eq!(
            parse(&out).unwrap().as_str(),
            Some("x\ny\r\u{1}\u{2028}\u{7f}é\u{1f600}")
        );
    }

    #[test]
    fn rejects_bad_text() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\":1,}",
            "{a:1}",
            "01",
            "1.",
            "-",
            "1e",
            "nul",
            "\"abc",
            "\"\\x\"",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "\"a\nb\"",
            "1 2",
            "1e999",
            "'a'",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let deep = "[".repeat(1000) + &"]".repeat(1000);
        assert!(parse(&deep).is_err());
        let ok = "[".repeat(100) + &"]".repeat(100);
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn duplicate_members_keep_the_last() {
        let v = parse(r#"{"a":1,"a":2}"#).unwrap();
        assert_eq!(v.get("a"), Some(&Value::Int(2)));
    }

    #[test]
    fn base64() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(raw.as_bytes()), enc);
            assert_eq!(base64_decode(enc).unwrap(), raw.as_bytes());
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&all)).unwrap(), all);
        assert_eq!(base64_decode("Zm9v\nYg").unwrap(), b"foob");
        assert!(base64_decode("Zm9*").is_err());
        assert!(base64_decode("Z").is_err());
        assert!(base64_decode("Zg==Zg==").is_err());
        assert!(base64_decode("Zh==").is_err());
    }
}
