//! `load expr` and `load mpexpr`: `${expr}` and `ntest`, after sh-expr(1).
//! Both evaluate with the arbitrary-precision [`Big`]; `expr` wraps every
//! value to 64-bit two's complement, as Limbo's `big` does.

use std::cmp::Ordering;

use crate::shmods::Fail;

/// A signed integer of any size: a sign and a little-endian magnitude in
/// 32-bit limbs with no high zero limbs. Zero is never negative.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Big {
    neg: bool,
    mag: Vec<u32>,
}

fn trim(v: &mut Vec<u32>) {
    while v.last() == Some(&0) {
        v.pop();
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (a, b) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry = 0u64;
    for (i, &x) in a.iter().enumerate() {
        let s = x as u64 + *b.get(i).unwrap_or(&0) as u64 + carry;
        out.push(s as u32);
        carry = s >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    out
}

/// `a - b` for `a >= b`.
fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &x) in a.iter().enumerate() {
        let mut d = x as i64 - *b.get(i).unwrap_or(&0) as i64 - borrow;
        borrow = 0;
        if d < 0 {
            d += 1 << 32;
            borrow = 1;
        }
        out.push(d as u32);
    }
    trim(&mut out);
    out
}

fn mul_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, &x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, &y) in b.iter().enumerate() {
            let t = x as u64 * y as u64 + out[i + j] as u64 + carry;
            out[i + j] = t as u32;
            carry = t >> 32;
        }
        out[i + b.len()] = carry as u32;
    }
    trim(&mut out);
    out
}

/// Divides by a single limb, returning the quotient and remainder.
fn divmod_small(a: &[u32], d: u32) -> (Vec<u32>, u32) {
    let mut q = vec![0u32; a.len()];
    let mut r = 0u64;
    for i in (0..a.len()).rev() {
        let cur = (r << 32) | a[i] as u64;
        q[i] = (cur / d as u64) as u32;
        r = cur % d as u64;
    }
    trim(&mut q);
    (q, r as u32)
}

fn shl_mag(a: &[u32], n: usize) -> Vec<u32> {
    if a.is_empty() {
        return Vec::new();
    }
    let (limbs, bits) = (n / 32, n % 32);
    let mut out = vec![0u32; limbs];
    if bits == 0 {
        out.extend_from_slice(a);
    } else {
        let mut carry = 0u32;
        for &x in a {
            out.push((x << bits) | carry);
            carry = x >> (32 - bits);
        }
        out.push(carry);
    }
    trim(&mut out);
    out
}

fn shr_mag(a: &[u32], n: usize) -> Vec<u32> {
    let (limbs, bits) = (n / 32, n % 32);
    if limbs >= a.len() {
        return Vec::new();
    }
    let a = &a[limbs..];
    let mut out: Vec<u32> = if bits == 0 {
        a.to_vec()
    } else {
        (0..a.len())
            .map(|i| (a[i] >> bits) | a.get(i + 1).map_or(0, |&h| h << (32 - bits)))
            .collect()
    };
    trim(&mut out);
    out
}

/// Long division of magnitudes (Knuth's algorithm D). `v` is non-zero.
fn divmod_mag(u: &[u32], v: &[u32]) -> (Vec<u32>, Vec<u32>) {
    if cmp_mag(u, v) == Ordering::Less {
        return (Vec::new(), u.to_vec());
    }
    if v.len() == 1 {
        let (q, r) = divmod_small(u, v[0]);
        let mut r = vec![r];
        trim(&mut r);
        return (q, r);
    }
    const B: u64 = 1 << 32;
    let s = v[v.len() - 1].leading_zeros() as usize;
    let vn = shl_mag(v, s);
    let mut un = shl_mag(u, s);
    un.resize(u.len() + 1, 0);
    let (n, m) = (v.len(), u.len() - v.len());
    let mut q = vec![0u32; m + 1];
    for j in (0..=m).rev() {
        let num = ((un[j + n] as u64) << 32) | un[j + n - 1] as u64;
        let mut qhat = num / vn[n - 1] as u64;
        let mut rhat = num % vn[n - 1] as u64;
        while qhat >= B || qhat * vn[n - 2] as u64 > ((rhat << 32) | un[j + n - 2] as u64) {
            qhat -= 1;
            rhat += vn[n - 1] as u64;
            if rhat >= B {
                break;
            }
        }
        let (mut borrow, mut carry) = (0i64, 0u64);
        for i in 0..n {
            let p = qhat * vn[i] as u64 + carry;
            carry = p >> 32;
            let t = un[i + j] as i64 - borrow - (p & 0xffff_ffff) as i64;
            un[i + j] = t as u32;
            borrow = (t < 0) as i64;
        }
        let t = un[j + n] as i64 - borrow - carry as i64;
        un[j + n] = t as u32;
        if t < 0 {
            qhat -= 1;
            let mut c = 0u64;
            for i in 0..n {
                let s = un[i + j] as u64 + vn[i] as u64 + c;
                un[i + j] = s as u32;
                c = s >> 32;
            }
            un[j + n] = un[j + n].wrapping_add(c as u32);
        }
        q[j] = qhat as u32;
    }
    trim(&mut q);
    un.truncate(n);
    trim(&mut un);
    (q, shr_mag(&un, s))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with `=` padding.
pub fn b64_enc(data: &[u8]) -> String {
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes base64, skipping characters outside the alphabet and stopping
/// at `=`.
pub fn b64_dec(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut n) = (0u32, 0);
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let Some(v) = B64.iter().position(|&b| b == c) else {
            continue;
        };
        acc = acc << 6 | v as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((acc >> n) as u8);
        }
    }
    out
}

impl Big {
    pub fn zero() -> Big {
        Big::default()
    }

    pub fn from_i64(n: i64) -> Big {
        let m = n.unsigned_abs();
        let mut mag = vec![m as u32, (m >> 32) as u32];
        trim(&mut mag);
        Big { neg: n < 0, mag }
    }

    fn make(neg: bool, mut mag: Vec<u32>) -> Big {
        trim(&mut mag);
        let neg = neg && !mag.is_empty();
        Big { neg, mag }
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub fn is_neg(&self) -> bool {
        self.neg
    }

    /// The value, saturated to an `i64`.
    pub fn to_i64(&self) -> i64 {
        if self.mag.len() > 2 {
            return if self.neg { i64::MIN } else { i64::MAX };
        }
        let m = self.mag.first().copied().unwrap_or(0) as u64
            | (self.mag.get(1).copied().unwrap_or(0) as u64) << 32;
        if self.neg {
            if m > i64::MIN.unsigned_abs() {
                i64::MIN
            } else {
                (m as i64).wrapping_neg()
            }
        } else {
            i64::try_from(m).unwrap_or(i64::MAX)
        }
    }

    /// The low 64 bits in two's complement, as an `i64`: Limbo's `big`.
    pub fn wrap64(&self) -> Big {
        let m = self.mag.first().copied().unwrap_or(0) as u64
            | (self.mag.get(1).copied().unwrap_or(0) as u64) << 32;
        let v = if self.neg { m.wrapping_neg() } else { m };
        Big::from_i64(v as i64)
    }

    /// The number of bits in the magnitude.
    pub fn bits(&self) -> usize {
        match self.mag.last() {
            None => 0,
            Some(&top) => self.mag.len() * 32 - top.leading_zeros() as usize,
        }
    }

    pub fn neg(&self) -> Big {
        Big::make(!self.neg, self.mag.clone())
    }

    pub fn add(&self, o: &Big) -> Big {
        if self.neg == o.neg {
            return Big::make(self.neg, add_mag(&self.mag, &o.mag));
        }
        match cmp_mag(&self.mag, &o.mag) {
            Ordering::Less => Big::make(o.neg, sub_mag(&o.mag, &self.mag)),
            _ => Big::make(self.neg, sub_mag(&self.mag, &o.mag)),
        }
    }

    pub fn sub(&self, o: &Big) -> Big {
        self.add(&o.neg())
    }

    pub fn mul(&self, o: &Big) -> Big {
        Big::make(self.neg != o.neg, mul_mag(&self.mag, &o.mag))
    }

    /// Truncating division and its remainder, which has the dividend's
    /// sign. `None` if `o` is zero.
    pub fn divrem(&self, o: &Big) -> Option<(Big, Big)> {
        if o.is_zero() {
            return None;
        }
        let (q, r) = divmod_mag(&self.mag, &o.mag);
        Some((Big::make(self.neg != o.neg, q), Big::make(self.neg, r)))
    }

    /// The remainder in `[0, |m|)`.
    fn modulo(&self, m: &Big) -> Option<Big> {
        let (_, r) = self.divrem(m)?;
        Some(if r.neg {
            r.add(&Big::make(false, m.mag.clone()))
        } else {
            r
        })
    }

    pub fn shl(&self, n: usize) -> Big {
        Big::make(self.neg, shl_mag(&self.mag, n))
    }

    /// Shifts right, rounding toward minus infinity.
    pub fn shr(&self, n: usize) -> Big {
        if !self.neg {
            return Big::make(false, shr_mag(&self.mag, n));
        }
        let one = Big::from_i64(1);
        let m = self.neg().sub(&one);
        Big::make(false, shr_mag(&m.mag, n)).add(&one).neg()
    }

    /// The value in two's complement in `n` limbs.
    fn twos(&self, n: usize) -> Vec<u32> {
        let mut v = self.mag.clone();
        v.resize(n, 0);
        if self.neg {
            let mut carry = 1u64;
            for x in &mut v {
                let s = (!*x) as u64 + carry;
                *x = s as u32;
                carry = s >> 32;
            }
        }
        v
    }

    fn from_twos(mut v: Vec<u32>) -> Big {
        if v.last().is_some_and(|&t| t & 0x8000_0000 != 0) {
            let mut carry = 1u64;
            for x in &mut v {
                let s = (!*x) as u64 + carry;
                *x = s as u32;
                carry = s >> 32;
            }
            Big::make(true, v)
        } else {
            Big::make(false, v)
        }
    }

    pub fn bitop(&self, o: &Big, f: fn(u32, u32) -> u32) -> Big {
        let n = self.mag.len().max(o.mag.len()) + 1;
        let (a, b) = (self.twos(n), o.twos(n));
        Big::from_twos(a.iter().zip(&b).map(|(&x, &y)| f(x, y)).collect())
    }

    /// `self` to the power `e`, reduced modulo `m` if given. A negative
    /// exponent gives zero.
    pub fn pow(&self, e: &Big, m: Option<&Big>) -> Option<Big> {
        let reduce = |x: Big| match m {
            Some(m) => x.modulo(m),
            None => Some(x),
        };
        if e.neg {
            return Some(Big::zero());
        }
        let mut result = reduce(Big::from_i64(1))?;
        let mut base = reduce(self.clone())?;
        for i in 0..e.bits() {
            if e.mag[i / 32] >> (i % 32) & 1 == 1 {
                result = reduce(result.mul(&base))?;
            }
            if i + 1 < e.bits() {
                base = reduce(base.mul(&base))?;
            }
        }
        Some(result)
    }

    /// The inverse of `self` modulo `m`, or zero if there is none.
    pub fn invert(&self, m: &Big) -> Option<Big> {
        let m = Big::make(false, m.mag.clone());
        let (mut r0, mut r1) = (m.clone(), self.modulo(&m)?);
        let (mut t0, mut t1) = (Big::zero(), Big::from_i64(1));
        while !r1.is_zero() {
            let (q, r) = r0.divrem(&r1)?;
            let t = t0.sub(&q.mul(&t1));
            (r0, r1) = (r1, r);
            (t0, t1) = (t1, t);
        }
        if r0 != Big::from_i64(1) {
            return Some(Big::zero());
        }
        t0.modulo(&m)
    }

    fn be_bytes(&self) -> Vec<u8> {
        let mut out: Vec<u8> = self
            .mag
            .iter()
            .rev()
            .flat_map(|x| x.to_be_bytes())
            .collect();
        while out.len() > 1 && out[0] == 0 {
            out.remove(0);
        }
        if out.is_empty() {
            out.push(0);
        }
        out
    }

    fn from_be_bytes(b: &[u8]) -> Big {
        let mut mag = Vec::new();
        for chunk in b.rchunks(4) {
            let mut w = [0u8; 4];
            w[4 - chunk.len()..].copy_from_slice(chunk);
            mag.push(u32::from_be_bytes(w));
        }
        Big::make(false, mag)
    }

    /// The digits of the magnitude in `radix` (2 to 36, or 64 for
    /// base64), without a sign or radix prefix.
    pub fn digits(&self, radix: u32) -> String {
        if radix == 64 {
            return b64_enc(&self.be_bytes());
        }
        if self.is_zero() {
            return String::from("0");
        }
        let mut out = Vec::new();
        let mut m = self.mag.clone();
        while !m.is_empty() {
            let (q, r) = divmod_small(&m, radix);
            out.push(char::from_digit(r, radix).unwrap_or('?'));
            m = q;
        }
        out.iter().rev().collect()
    }

    /// Parses the leading digits of `s` in `radix`, stopping at the first
    /// character that is not one.
    pub fn parse_digits(s: &str, radix: u32) -> Big {
        if radix == 64 {
            return Big::from_be_bytes(&b64_dec(s));
        }
        // Radix 1 has no digits but 0 (and to_digit refuses it).
        if radix < 2 {
            return Big::zero();
        }
        let r = Big::from_i64(radix as i64);
        let mut n = Big::zero();
        for c in s.chars() {
            let Some(d) = c.to_digit(radix) else { break };
            n = n.mul(&r).add(&Big::from_i64(d as i64));
        }
        n
    }
}

impl PartialOrd for Big {
    fn partial_cmp(&self, o: &Big) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Big {
    fn cmp(&self, o: &Big) -> Ordering {
        match (self.neg, o.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => cmp_mag(&self.mag, &o.mag),
            (true, true) => cmp_mag(&o.mag, &self.mag),
        }
    }
}

/// Fills `buf` from WASI `random_get`, which reads the kernel's entropy.
#[cfg(target_family = "wasm")]
fn random(buf: &mut [u8]) {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    unsafe extern "C" {
        fn random_get(buf: *mut u8, len: usize) -> i32;
    }
    // SAFETY: the host writes at most `buf.len()` bytes into `buf`.
    unsafe { random_get(buf.as_mut_ptr(), buf.len()) };
}

/// Host builds (clippy, unit tests) have no WASI import.
#[cfg(not(target_family = "wasm"))]
fn random(buf: &mut [u8]) {
    buf.fill(0x5a);
}

/// A random number of at most `bits` bits.
fn rand_bits(bits: usize) -> Big {
    let mut buf = vec![0u8; bits.div_ceil(8)];
    random(&mut buf);
    if !bits.is_multiple_of(8)
        && let Some(top) = buf.first_mut()
    {
        *top &= (1u8 << (bits % 8)) - 1;
    }
    Big::from_be_bytes(&buf)
}

fn usage() -> Fail {
    Fail::raise("usage", "usage: expr [-r radix] [arg...]")
}

/// Parses a number as Limbo writes it: decimal, or `radix r digits` such
/// as `16rff`, with an optional leading `-`.
fn number(w: &str, mp: bool) -> Result<Big, Fail> {
    if !w.starts_with(|c: char| c == '-' || c.is_ascii_digit()) {
        return Err(Fail::raise(
            "usage",
            &format!("expr: unknown operator '{w}'"),
        ));
    }
    let (neg, s) = match w.strip_prefix('-') {
        Some(s) => (true, s),
        None => (false, w),
    };
    let (radix, digits) = match s.char_indices().take(3).find(|&(_, c)| c == 'r') {
        Some((i, _)) => {
            let r: u32 = s[..i].parse().unwrap_or(0);
            if r == 0 || (r > 36 && !(mp && r == 64)) {
                return Err(Fail::raise("usage", &format!("expr: bad number {w}")));
            }
            (r, &s[i + 1..])
        }
        None => (10, s),
    };
    let n = Big::parse_digits(digits, radix);
    Ok(if neg { n.neg() } else { n })
}

fn format(n: &Big, radix: u32) -> String {
    let digits = n.digits(radix);
    let sign = if n.is_neg() { "-" } else { "" };
    if radix == 10 {
        format!("{sign}{digits}")
    } else {
        format!("{sign}{radix}r{digits}")
    }
}

/// `ntest num`: true unless `num` is zero.
pub fn ntest(argv: &[String], mp: bool) -> Result<String, Fail> {
    let [_, n] = argv else {
        return Err(Fail::raise("usage", "usage: ntest n"));
    };
    let zero = number(n, mp).map_or(true, |n| n.is_zero());
    Ok(if zero {
        String::from("false")
    } else {
        String::new()
    })
}

/// How many operands an operator takes, or `None` if `w` is not one.
fn arity(w: &str, mp: bool) -> Option<usize> {
    Some(match w {
        "+" | "-" | "x" | "*" | "×" | "/" | "%" | "and" | "or" | "xor" | "<<" | "shl" | ">>"
        | "shr" | "=" | "==" | "eq" | "!=" | "neq" | ">" | "gt" | "<" | "lt" | ">=" | "ge"
        | "<=" | "le" | "seq" => 2,
        "_" | "~" | "!" | "not" => 1,
        "rep" => 0,
        "bits" | "rand" if mp => 1,
        "invert" | "exp" | "xx" | "**" if mp => 2,
        "expmod" if mp => 3,
        _ => return None,
    })
}

fn truth(b: bool) -> Big {
    Big::from_i64(b as i64)
}

fn shift(n: &Big) -> usize {
    usize::try_from(n.to_i64().clamp(0, 1 << 24)).unwrap_or(0)
}

/// Applies a one-, two- or three-operand operator to `a`, first pushed
/// first, pushing the result on `stk`.
fn apply(op: &str, a: &[Big], stk: &mut Vec<Big>) -> Result<(), Fail> {
    let div0 = || Fail::raise("divide by zero", "expr: division by zero");
    let r = match op {
        "+" => a[0].add(&a[1]),
        "-" => a[0].sub(&a[1]),
        "x" | "*" | "×" => a[0].mul(&a[1]),
        "/" => a[0].divrem(&a[1]).ok_or_else(div0)?.0,
        "%" => a[0].divrem(&a[1]).ok_or_else(div0)?.1,
        "and" => a[0].bitop(&a[1], |x, y| x & y),
        "or" => a[0].bitop(&a[1], |x, y| x | y),
        "xor" => a[0].bitop(&a[1], |x, y| x ^ y),
        "<<" | "shl" => a[0].shl(shift(&a[1])),
        ">>" | "shr" => a[0].shr(shift(&a[1])),
        "=" | "==" | "eq" => truth(a[0] == a[1]),
        "!=" | "neq" => truth(a[0] != a[1]),
        ">" | "gt" => truth(a[0] > a[1]),
        "<" | "lt" => truth(a[0] < a[1]),
        ">=" | "ge" => truth(a[0] >= a[1]),
        "<=" | "le" => truth(a[0] <= a[1]),
        "_" => a[0].neg(),
        "~" => a[0].neg().sub(&Big::from_i64(1)),
        "!" | "not" => truth(a[0].is_zero()),
        "bits" => Big::from_i64(a[0].bits() as i64),
        "rand" => rand_bits(shift(&a[0])),
        "exp" | "xx" | "**" => a[0].pow(&a[1], None).ok_or_else(div0)?,
        "expmod" => a[0].pow(&a[1], Some(&a[2])).ok_or_else(div0)?,
        "invert" => a[0].invert(&a[1]).ok_or_else(div0)?,
        _ => {
            // seq: the numbers from a[0] to a[1], either way.
            let step = Big::from_i64(if a[1] < a[0] { -1 } else { 1 });
            let mut n = a[0].clone();
            while n != a[1] {
                stk.push(n.clone());
                n = n.add(&step);
            }
            n
        }
    };
    stk.push(r);
    Ok(())
}

/// Pops `n` operands, first pushed first.
fn pop(stk: &mut Vec<Big>, n: usize, op: &str) -> Result<Vec<Big>, Fail> {
    if stk.len() < n {
        return Err(Fail::raise(
            "empty stack",
            &format!("expr: empty stack on op '{op}'"),
        ));
    }
    Ok(stk.split_off(stk.len() - n))
}

/// `${expr [-r radix] arg ...}`, with `argv[0]` the name. `mp` selects
/// mpexpr's arbitrary precision and extra operators.
pub fn expr(argv: &[String], mp: bool) -> Result<Vec<String>, Fail> {
    let mut args = &argv[1..];
    let mut radix = 10;
    if let Some(w) = args.first()
        && w.len() >= 2
        && w.starts_with('-')
        && !w.as_bytes()[1].is_ascii_digit()
    {
        if !w.starts_with("-r") {
            return Err(usage());
        }
        let r = if w.len() > 2 {
            args = &args[1..];
            &w[2..]
        } else {
            let Some(r) = args.get(1) else {
                return Err(usage());
            };
            args = &args[2..];
            r.as_str()
        };
        let n: i64 = r.bytes().take_while(u8::is_ascii_digit).fold(0i64, |n, d| {
            n.saturating_mul(10).saturating_add((d - b'0') as i64)
        });
        // Radix 1 would never finish printing; Inferno's mpexpr refuses it too.
        if n < 2 || (n > 36 && !(mp && n == 64)) {
            return Err(Fail::raise("usage", &format!("expr: invalid radix {n}")));
        }
        radix = n as u32;
    }
    let mut stk: Vec<Big> = Vec::new();
    let mut last: Option<(&str, usize)> = None;
    for w in args {
        let w = w.as_str();
        match arity(w, mp) {
            None => stk.push(number(w, mp)?),
            Some(0) => {
                // rep: repeat the last two-operand operator.
                let Some((op, 2)) = last.filter(|l| l.0 != "seq") else {
                    return Err(Fail::raise("usage", "expr: bad operator for rep"));
                };
                while stk.len() > 1 {
                    let a = pop(&mut stk, 2, op)?;
                    apply(op, &a, &mut stk)?;
                    if !mp {
                        wrap(&mut stk);
                    }
                }
            }
            Some(n) => {
                let a = pop(&mut stk, n, w)?;
                apply(w, &a, &mut stk)?;
            }
        }
        if !mp {
            wrap(&mut stk);
        }
        last = arity(w, mp).map(|n| (w, n));
    }
    Ok(stk.iter().map(|n| format(n, radix)).collect())
}

fn wrap(stk: &mut [Big]) {
    for n in stk {
        *n = n.wrap64();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(s: &str) -> Big {
        let (neg, d) = match s.strip_prefix('-') {
            Some(d) => (true, d),
            None => (false, s),
        };
        let n = Big::parse_digits(d, 10);
        if neg { n.neg() } else { n }
    }

    fn run(mp: bool, args: &str) -> Result<Vec<String>, Fail> {
        let mut argv = vec![String::from("expr")];
        argv.extend(args.split_whitespace().map(String::from));
        expr(&argv, mp)
    }

    #[test]
    fn arithmetic() {
        let a = big("123456789012345678901234567890");
        let b = big("-987654321987654321");
        assert_eq!(
            a.mul(&b),
            big("-121932631246761163237311385323609205901126352690")
        );
        let (q, r) = a.divrem(&b).unwrap();
        assert_eq!(
            (q.to_i64(), &r),
            (-124999998748, &big("432099904777777782"))
        );
        assert!(r.add(&q.mul(&b)) == a);
        assert_eq!(big("-7").divrem(&big("2")).unwrap(), (big("-3"), big("-1")));
        assert_eq!(big("-1").shr(1), big("-1"));
        assert_eq!(big("-8").shr(2), big("-2"));
        assert_eq!(big("5").bitop(&big("-2"), |x, y| x & y), big("4"));
        assert_eq!(big("-5").bitop(&big("3"), |x, y| x | y), big("-5"));
        assert_eq!(big("3").pow(&big("4"), Some(&big("5"))).unwrap(), big("1"));
        assert_eq!(big("3").invert(&big("7")).unwrap(), big("5"));
        assert_eq!(big("255").digits(16), "ff");
        assert_eq!(big("-9223372036854775808").to_i64(), i64::MIN);
        assert_eq!(big("18446744073709551617").wrap64(), big("1"));
        assert_eq!(b64_dec(&b64_enc(b"hello!?")), b"hello!?");
        assert_eq!(b64_enc(b"abc"), "YWJj");
    }

    #[test]
    fn long_division() {
        let two = big("2");
        let n = two.pow(&big("200"), None).unwrap().sub(&big("1"));
        let d = two.pow(&big("97"), None).unwrap().add(&big("12345"));
        let (q, r) = n.divrem(&d).unwrap();
        assert!(r < d);
        assert_eq!(q.mul(&d).add(&r), n);
    }

    #[test]
    fn expressions() {
        assert_eq!(run(false, "2 1 -").unwrap(), ["1"]);
        assert_eq!(run(false, "1 2 3 4 + rep").unwrap(), ["10"]);
        assert_eq!(run(false, "1 5 seq").unwrap(), ["1", "2", "3", "4", "5"]);
        assert_eq!(run(false, "-r 16 255 16r10").unwrap(), ["16rff", "16r10"]);
        assert_eq!(
            run(false, "9223372036854775807 1 +").unwrap(),
            ["-9223372036854775808"]
        );
        assert_eq!(
            run(true, "2 100 xx").unwrap(),
            ["1267650600228229401496703205376"]
        );
        assert_eq!(run(false, "0 !").unwrap(), ["1"]);
        assert!(run(false, "2 100 xx").is_err());
        assert!(run(false, "1 0 /").is_err());
        assert!(run(false, "+").is_err());
    }

    #[test]
    fn radix_one() {
        // A number in radix 1 is 0; printing in radix 1 is refused.
        assert_eq!(run(false, "1r0 1r5").unwrap(), ["0", "0"]);
        assert_eq!(run(true, "1r0").unwrap(), ["0"]);
        for mp in [false, true] {
            let e = run(mp, "-r 1 5").err().unwrap();
            assert_eq!(e.msg, "expr: invalid radix 1");
        }
    }
}
