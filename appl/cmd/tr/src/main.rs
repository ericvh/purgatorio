//! tr [-cds] [string1 [string2]]: copy standard input to standard output,
//! mapping characters in string1 to the corresponding ones in string2
//! (padded with its last character). `-c` complements string1, `-d`
//! deletes the characters of string1, and `-s` squeezes runs of an output
//! character that is in string2 to one.
//!
//! In either string `a-z` is a range, `\` and one to three octal digits is
//! that character, `\x` and one to six hex digits likewise, and `\` before
//! anything else quotes it.

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Read, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

/// Expands a tr string into its characters.
fn expand(s: &str) -> Vec<char> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<char> = Vec::new();
    let mut i = 0;
    // Reads one possibly escaped character at `i`.
    let one = |i: &mut usize| -> char {
        let c = chars[*i];
        *i += 1;
        if c != '\\' || *i >= chars.len() {
            return c;
        }
        let (radix, max, start) = if chars[*i] == 'x' {
            (16, 6, *i + 1)
        } else {
            (8, 3, *i)
        };
        let mut v = 0u32;
        let mut n = 0;
        while n < max
            && let Some(d) = chars.get(start + n).and_then(|c| c.to_digit(radix))
        {
            v = v * radix + d;
            n += 1;
        }
        if n == 0 {
            if radix == 16 {
                // `\x` with no digits is just `x`.
                *i += 1;
                return 'x';
            }
            let q = chars[*i];
            *i += 1;
            return q;
        }
        *i = start + n;
        char::from_u32(v).unwrap_or(char::REPLACEMENT_CHARACTER)
    };
    while i < chars.len() {
        if chars[i] == '-' && !out.is_empty() && i + 1 < chars.len() {
            i += 1;
            let hi = one(&mut i) as u32;
            let lo = *out.last().unwrap() as u32 + 1;
            out.extend((lo..=hi).filter_map(char::from_u32));
            continue;
        }
        out.push(one(&mut i));
    }
    out
}

/// What happens to each input character.
struct Tr {
    complement: bool,
    delete: bool,
    squeeze: bool,
    set1: BTreeSet<char>,
    /// String1's mapping when not complemented: the last mapping wins.
    map: HashMap<char, char>,
    s2: Vec<char>,
    set2: BTreeSet<char>,
}

impl Tr {
    fn in1(&self, c: char) -> bool {
        self.set1.contains(&c) != self.complement
    }

    /// The output for `c`, or `None` to drop it.
    fn map(&self, c: char) -> Option<char> {
        if self.delete {
            return if self.in1(c) { None } else { Some(c) };
        }
        if self.s2.is_empty() {
            return Some(c);
        }
        if self.complement {
            if self.set1.contains(&c) {
                return Some(c);
            }
            // c's index among the characters not in string1.
            let below = self.set1.range(..c).count();
            let idx = c as usize - below;
            let idx = idx.min(self.s2.len() - 1);
            return Some(self.s2[idx]);
        }
        Some(self.map.get(&c).copied().unwrap_or(c))
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "tr [-cds] [string1 [string2]]");
    let (mut complement, mut delete, mut squeeze) = (false, false, false);
    while let Some(o) = arg.opt() {
        match o {
            'c' => complement = true,
            'd' => delete = true,
            's' => squeeze = true,
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let strs = arg.argv();
    if strs.len() > 2 {
        return Err(usage);
    }
    let s1 = strs.first().map(|s| expand(s)).unwrap_or_default();
    let mut s2 = strs.get(1).map(|s| expand(s)).unwrap_or_default();
    if s2.is_empty() && squeeze && !delete && strs.len() < 2 {
        // `tr -s abc` squeezes the characters of string1.
        s2 = if complement { Vec::new() } else { s1.clone() };
    }
    let mut map = HashMap::new();
    if !s2.is_empty() {
        for (i, &c) in s1.iter().enumerate() {
            map.insert(c, s2[i.min(s2.len() - 1)]);
        }
    }
    let tr = Tr {
        complement,
        delete,
        squeeze,
        set1: s1.iter().copied().collect(),
        map,
        set2: s2.iter().copied().collect(),
        s2,
    };
    // `tr -cs x` with no string2: squeeze everything not in string1.
    let squeeze_all_c = squeeze && complement && tr.s2.is_empty() && !delete;
    let mut out = io::BufWriter::new(io::stdout().lock());
    let mut input = io::stdin().lock();
    let mut buf = vec![0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    let mut last: Option<char> = None;
    let mut enc = [0u8; 4];
    loop {
        let n = input.read(&mut buf)?;
        let eof = n == 0;
        pending.extend_from_slice(&buf[..n]);
        let mut chars = Vec::new();
        let mut used = 0;
        loop {
            match std::str::from_utf8(&pending[used..]) {
                Ok(s) => {
                    chars.extend(s.chars());
                    used = pending.len();
                    break;
                }
                Err(e) => {
                    let ok = used + e.valid_up_to();
                    chars.extend(std::str::from_utf8(&pending[used..ok]).unwrap().chars());
                    match e.error_len() {
                        Some(bad) => {
                            chars.push(char::REPLACEMENT_CHARACTER);
                            used = ok + bad;
                        }
                        None if eof => {
                            chars.push(char::REPLACEMENT_CHARACTER);
                            used = pending.len();
                            break;
                        }
                        None => {
                            used = ok;
                            break;
                        }
                    }
                }
            }
        }
        pending.drain(..used);
        for c in chars {
            let Some(o) = tr.map(c) else {
                continue;
            };
            let squeezable = if squeeze_all_c {
                tr.in1(c)
            } else {
                tr.set2.contains(&o)
            };
            if tr.squeeze && squeezable && last == Some(o) {
                continue;
            }
            last = Some(o);
            out.write_all(o.encode_utf8(&mut enc).as_bytes())?;
        }
        if eof {
            break;
        }
    }
    out.flush()?;
    Ok(())
}
