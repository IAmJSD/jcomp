//! Numbers: exact decimals and IEEE doubles, their JSON text forms, exact
//! arithmetic for predictions, and rational-grid helpers. Everything that can
//! influence coding decisions is integer or IEEE-basic-op arithmetic only (no
//! libm), so encoder and decoder agree on every platform.
use crate::json::py_repr;

/// A numeric value: exact decimal `m * 10^-e` (normalized: no trailing zeros
/// in m, e = 0 when m = 0) or a raw double (by bits).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Num {
    D(i128, i32),
    F(u64),
}

/// decimals with more significant digits than this are treated as doubles
pub const DMAX: u32 = 12;
const MAX_DIGITS: usize = 30;
const MAX_EXP: i32 = 40;

const POW10F: [f64; 23] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22];

const POW10I: [i128; 39] = {
    let mut t = [1i128; 39];
    let mut i = 1;
    while i < 39 {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
};

const POW10L: [i64; 19] = {
    let mut t = [1i64; 19];
    let mut i = 1;
    while i < 19 {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
};

#[inline]
pub fn pow10(k: u32) -> Option<i128> {
    POW10I.get(k as usize).copied()
}

pub fn norm(mut m: i128, mut e: i32) -> Num {
    if m == 0 {
        return Num::D(0, 0);
    }
    // hardware division when it fits (i128 division is a slow library call)
    if let Ok(mut s) = i64::try_from(m) {
        while s % 10 == 0 {
            s /= 10;
            e -= 1;
        }
        return Num::D(s as i128, e);
    }
    while m % 10 == 0 {
        m /= 10;
        e -= 1;
    }
    Num::D(m, e)
}

fn digits(m: i128) -> u32 {
    let mut m = m.unsigned_abs();
    let mut d = 0;
    while m > 0 {
        m /= 10;
        d += 1;
    }
    d.max(1)
}

/// floor division for b > 0
#[inline]
pub fn fdiv(a: i128, b: i128) -> i128 {
    a.div_euclid(b)
}

/// round-half-up of a/b (b != 0)
pub fn rdiv(a: i128, b: i128) -> Option<i128> {
    let (a, b) = if b < 0 { (a.checked_neg()?, -b) } else { (a, b) };
    Some(fdiv(a.checked_mul(2)?.checked_add(b)?, b.checked_mul(2)?))
}

impl Num {
    pub fn f64(self) -> f64 {
        match self {
            Num::F(b) => f64::from_bits(b),
            // exact fast path: m and 10^|e| are exact doubles, so one IEEE op is correctly rounded
            Num::D(m, e) if m.unsigned_abs() < 1 << 53 && e.abs() <= 22 => {
                let p = POW10F[e.unsigned_abs() as usize];
                if e >= 0 { m as f64 / p } else { m as f64 * p }
            }
            Num::D(m, e) => {
                // "<m>e<-e>" in a stack buffer, parsed with correct rounding
                use std::io::Write;
                let mut buf = [0u8; 64];
                let mut w = &mut buf[..];
                write!(w, "{}e{}", m, -e).unwrap();
                let n = 64 - w.len();
                std::str::from_utf8(&buf[..n]).unwrap().parse().unwrap()
            }
        }
    }
}

/// Classify a double: short shortest-repr -> exact decimal, else raw bits.
pub fn classify(x: f64) -> Num {
    if !x.is_finite() || (x == 0.0 && x.is_sign_negative()) {
        return Num::F(x.to_bits());
    }
    match parse_dec_exp(&py_repr(x)) {
        Some(Num::D(m, e)) if digits(m) <= DMAX && e.abs() <= MAX_EXP => Num::D(m, e),
        _ => Num::F(x.to_bits()),
    }
}

/// A double as a short decimal plus an offset in ULPs: `x == f64(d) + j ulps`,
/// for the shortest decimal d (<= DMAX significant digits) within 64 ULPs.
/// Float arithmetic on decimal inputs (sums of prices, `a * 0.9`) lands here.
pub fn near(x: f64) -> Option<(Num, i64)> {
    if !x.is_finite() || (x == 0.0 && x.is_sign_negative()) {
        return None;
    }
    // fast path for ordinary magnitudes: smallest e with round(x * 10^e) / 10^e
    // within 64 ULPs (no string formatting)
    let ax = x.abs();
    if ax >= 1e-6 && ax < 1e12 {
        let ox = ord(x.to_bits());
        for e in 0..=22usize {
            let y = x * POW10F[e];
            if y.abs() >= 1e12 {
                break;
            }
            let d = norm(y.round() as i128, e as i32);
            let j = ox - ord(d.f64().to_bits());
            if j.abs() <= 64 {
                return Some((d, j as i64));
            }
        }
        return None;
    }
    let Num::D(m, e) = parse_dec_exp(&py_repr(x))? else { return None };
    let nd = digits(m);
    if nd <= DMAX {
        return (e.abs() <= MAX_EXP).then_some((Num::D(m, e), 0));
    }
    let ox = ord(x.to_bits());
    for k in 1..=DMAX {
        let drop = nd - k;
        let d = norm(rdiv(m, pow10(drop)?)?, e - drop as i32);
        let Num::D(_, de) = d else { continue };
        if de.abs() > MAX_EXP {
            continue;
        }
        let j = ox - ord(d.f64().to_bits());
        if j.abs() <= 64 {
            return Some((d, j as i64));
        }
    }
    None
}

/// Parse `-?digits(.digits)?(e[+-]digits)?` into an exact decimal.
pub fn parse_dec_exp(t: &str) -> Option<Num> {
    let (mant, ex) = match t.find(['e', 'E']) {
        Some(p) => (&t[..p], t[p + 1..].parse::<i32>().ok()?),
        None => (t, 0),
    };
    let (neg, mant) = match mant.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, mant),
    };
    let (ip, fp) = mant.split_once('.').unwrap_or((mant, ""));
    if ip.is_empty() || !ip.bytes().all(|b| b.is_ascii_digit()) || !fp.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let all = format!("{ip}{fp}");
    let all = all.trim_start_matches('0');
    if all.len() > MAX_DIGITS {
        return None;
    }
    let m: i128 = if all.is_empty() { 0 } else { all.parse().ok()? };
    let e = fp.len() as i32 - ex;
    match norm(if neg { -m } else { m }, e) {
        Num::D(m, e) if e.abs() <= MAX_EXP => Some(Num::D(m, e)),
        Num::D(..) => None,
        f => Some(f),
    }
}

/// Text form of a numeric JSON string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StrFmt {
    /// shortest decimal text of the value
    Canon,
    /// Python repr of the double
    Repr,
    /// decimal with exactly this many fraction digits
    Fixed(u32),
}

impl StrFmt {
    pub fn code(self) -> u32 {
        match self {
            StrFmt::Canon => 0,
            StrFmt::Repr => 1,
            StrFmt::Fixed(k) => 2 + k,
        }
    }
    pub fn from_code(c: u32) -> StrFmt {
        match c {
            0 => StrFmt::Canon,
            1 => StrFmt::Repr,
            k => StrFmt::Fixed(k - 2),
        }
    }
}

pub fn render(n: Num, f: StrFmt) -> Option<String> {
    match (n, f) {
        (_, StrFmt::Repr) => Some(py_repr(n.f64())),
        (Num::D(m, e), StrFmt::Canon) => {
            if e <= 0 {
                Some((m.checked_mul(pow10((-e) as u32)?)?).to_string())
            } else {
                fixed(m, e, e as u32)
            }
        }
        (Num::D(m, e), StrFmt::Fixed(k)) => {
            if e > k as i32 {
                return None;
            }
            fixed(m, e, k)
        }
        _ => None,
    }
}

fn fixed(m: i128, e: i32, k: u32) -> Option<String> {
    // m * 10^-e with exactly k fraction digits (k >= e): print the scaled integer's
    // digits once, then place the decimal point (no division, no format machinery)
    let sh = k as i32 - e;
    if sh < 0 {
        return None;
    }
    let v = m.checked_mul(pow10(sh as u32)?)?;
    let mut buf = [0u8; 48];
    let mut i = buf.len();
    let a = v.unsigned_abs();
    if let Ok(mut a) = u64::try_from(a) {
        loop {
            i -= 1;
            buf[i] = b'0' + (a % 10) as u8;
            a /= 10;
            if a == 0 {
                break;
            }
        }
    } else {
        let mut a = a;
        loop {
            i -= 1;
            buf[i] = b'0' + (a % 10) as u8;
            a /= 10;
            if a == 0 {
                break;
            }
        }
    }
    let digits = &buf[i..];
    let k = k as usize;
    let mut s = String::with_capacity(digits.len() + k + 3);
    if v < 0 {
        s.push('-');
    }
    if digits.len() > k {
        let (ip, fp) = digits.split_at(digits.len() - k);
        s.push_str(std::str::from_utf8(ip).unwrap());
        if k > 0 {
            s.push('.');
            s.push_str(std::str::from_utf8(fp).unwrap());
        }
    } else {
        s.push('0');
        if k > 0 {
            s.push('.');
            for _ in digits.len()..k {
                s.push('0');
            }
            s.push_str(std::str::from_utf8(digits).unwrap());
        }
    }
    Some(s)
}

/// Numeric string -> (value, format), only when it re-renders exactly.
pub fn parse_numstr(t: &str) -> Option<(Num, StrFmt)> {
    let b = t.as_bytes();
    let body = t.strip_prefix('-').unwrap_or(t);
    let is_plain = !body.is_empty()
        && body.bytes().all(|c| c.is_ascii_digit() || c == b'.')
        && body.bytes().filter(|&c| c == b'.').count() <= 1
        && !body.starts_with('.')
        && !body.ends_with('.')
        && !(body.len() > 1 && body.starts_with('0') && body.as_bytes()[1] != b'.');
    if is_plain && !b.is_empty() {
        if let Some(n @ Num::D(m, _)) = parse_dec_exp(t) {
            if digits(m) <= DMAX {
                for f in [StrFmt::Canon, StrFmt::Fixed(body.split_once('.').map(|x| x.1.len()).unwrap_or(0) as u32)] {
                    if render(n, f).as_deref() == Some(t) {
                        return Some((n, f));
                    }
                }
            }
        }
    }
    // Python float repr text (long digit strings, exponents)
    if let Ok(x) = t.parse::<f64>() {
        if x.is_finite() && py_repr(x) == t && t.bytes().all(|c| c.is_ascii_digit() || b".-+e".contains(&c)) {
            return Some((classify(x), StrFmt::Repr));
        }
    }
    if is_plain {
        if let Some(n) = parse_dec_exp(t) {
            for f in [StrFmt::Canon, StrFmt::Fixed(body.split_once('.').map(|x| x.1.len()).unwrap_or(0) as u32)] {
                if render(n, f).as_deref() == Some(t) {
                    return Some((n, f));
                }
            }
        }
    }
    None
}

// ------------------------------------------------------------------ arithmetic
fn align(a: (i128, i32), b: (i128, i32)) -> Option<(i128, i128, i32)> {
    let e = a.1.max(b.1);
    let sa = pow10((e - a.1) as u32)?;
    let sb = pow10((e - b.1) as u32)?;
    Some((a.0.checked_mul(sa)?, b.0.checked_mul(sb)?, e))
}

pub fn add(a: Num, b: Num, sub: bool) -> Option<Num> {
    match (a, b) {
        (Num::D(ma, ea), Num::D(mb, eb)) => {
            if let (Ok(xa), Ok(xb)) = (i64::try_from(ma), i64::try_from(mb)) {
                let e = ea.max(eb);
                let r = (|| {
                    let x = xa.checked_mul(*POW10L.get((e - ea) as usize)?)?;
                    let y = xb.checked_mul(*POW10L.get((e - eb) as usize)?)?;
                    if sub { x.checked_sub(y) } else { x.checked_add(y) }
                })();
                if let Some(r) = r {
                    return Some(norm(r as i128, e));
                }
            }
            let (x, y, e) = align((ma, ea), (mb, eb))?;
            Some(norm(if sub { x.checked_sub(y)? } else { x.checked_add(y)? }, e))
        }
        _ => {
            let r = if sub { a.f64() - b.f64() } else { a.f64() + b.f64() };
            Some(Num::F(r.to_bits()))
        }
    }
}

/// a + b - c
pub fn para_add(a: Num, b: Num, c: Num) -> Option<Num> {
    if let (Num::D(ma, ea), Num::D(mb, eb), Num::D(mc, ec)) = (a, b, c) {
        // one alignment, one normalization; machine-word arithmetic when it fits
        let e = ea.max(eb).max(ec);
        if let (Ok(xa), Ok(xb), Ok(xc)) = (i64::try_from(ma), i64::try_from(mb), i64::try_from(mc)) {
            let sc = |m: i64, ex: i32| -> Option<i64> { m.checked_mul(*POW10L.get((e - ex) as usize)?) };
            if let Some(r) = (|| sc(xa, ea)?.checked_add(sc(xb, eb)?)?.checked_sub(sc(xc, ec)?))() {
                return Some(norm(r as i128, e));
            }
        }
        let sc = |m: i128, ex: i32| -> Option<i128> { m.checked_mul(pow10((e - ex) as u32)?) };
        return Some(norm(sc(ma, ea)?.checked_add(sc(mb, eb)?)?.checked_sub(sc(mc, ec)?)?, e));
    }
    add(add(a, b, false)?, c, true)
}

/// number of fraction digits used when rounding ratio predictions
fn prec(n: Num) -> i32 {
    match n {
        Num::D(_, e) => e.max(1),
        Num::F(_) => 0,
    }
}

/// a * b / c rounded to the decimal precision of `a` (exact for decimals)
pub fn para_mul(a: Num, b: Num, c: Num) -> Option<Num> {
    match (a, b, c) {
        (Num::D(ma, ea), Num::D(mb, eb), Num::D(mc, ec)) => {
            if mc == 0 {
                return None;
            }
            let t = prec(a);
            let mut num = ma.checked_mul(mb)?;
            let mut den = mc;
            let s = t - ea - eb + ec;
            if s >= 0 {
                num = num.checked_mul(pow10(s as u32)?)?;
            } else {
                den = den.checked_mul(pow10((-s) as u32)?)?;
            }
            let r = norm(rdiv(num, den)?, t);
            match r {
                Num::D(_, e) if e.abs() <= MAX_EXP => Some(r),
                _ => None,
            }
        }
        _ => {
            let c = c.f64();
            if c == 0.0 {
                return None;
            }
            Some(Num::F((a.f64() * b.f64() / c).to_bits()))
        }
    }
}

/// (a + b) / 2, exact for decimals
pub fn mid(a: Num, b: Num) -> Option<Num> {
    match add(a, b, false)? {
        Num::D(m, e) => Some(norm(m.checked_mul(5)?, e + 1)),
        Num::F(x) => Some(Num::F((f64::from_bits(x) / 2.0).to_bits())),
    }
}

/// round(n * 10^e) as an integer (prediction on a decimal grid)
pub fn scale_round(n: Num, e: i32) -> Option<i128> {
    match n {
        Num::D(m, ne) => {
            let sh = e - ne;
            if sh >= 0 { m.checked_mul(pow10(sh as u32)?) } else { rdiv(m, pow10((-sh) as u32)?) }
        }
        // exact decimal image of the double (its shortest repr), then as above
        Num::F(b) => match parse_dec_exp(&py_repr(f64::from_bits(b)))? {
            d @ Num::D(..) => scale_round(d, e),
            Num::F(_) => None,
        },
    }
}

// ------------------------------------------------------------------ rational grids
/// A grid of values k / den: either exact doubles (`prec` None: x == k as f64 / den as f64)
/// or decimals rounded half-up to `prec` digits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grid {
    pub den: i128,
    pub prec: Option<u32>,
    /// scaled: doubles `x = f64(k / 10^prec) / den` (a short decimal divided by a
    /// constant, e.g. a monthly price / 730 hours), up to a few ULPs of noise
    pub scaled: bool,
}

impl Grid {
    /// nearest grid index for a value
    pub fn index(&self, n: Num) -> Option<i128> {
        if self.scaled {
            let p = self.prec?;
            return match n {
                Num::D(m, e) => {
                    let num = m.checked_mul(self.den)?.checked_mul(pow10(p)?)?;
                    if e >= 0 { rdiv(num, pow10(e as u32)?) } else { num.checked_mul(pow10((-e) as u32)?) }
                }
                Num::F(b) => {
                    let y = f64::from_bits(b) * self.den as f64 * POW10F[p.min(22) as usize];
                    if y.is_finite() && y.abs() < 9e15 { Some(y.round() as i128) } else { None }
                }
            };
        }
        match n {
            Num::D(m, e) => {
                // round(m * 10^-e * den)
                if e >= 0 { rdiv(m.checked_mul(self.den)?, pow10(e as u32)?) } else { m.checked_mul(self.den)?.checked_mul(pow10((-e) as u32)?) }
            }
            Num::F(b) => {
                let x = f64::from_bits(b) * self.den as f64;
                if x.is_finite() && x.abs() < 9e15 { Some(x.round() as i128) } else { None }
            }
        }
    }
    pub fn value(&self, k: i128) -> Option<Num> {
        if self.scaled {
            let d = norm(k, self.prec? as i32);
            return Some(Num::F((d.f64() / self.den as f64).to_bits()));
        }
        match self.prec {
            Some(p) => Some(norm(rdiv(k.checked_mul(pow10(p)?)?, self.den)?, p as i32)),
            None => {
                if k.unsigned_abs() >= 1 << 53 {
                    return None;
                }
                Some(Num::F((k as f64 / self.den as f64).to_bits()))
            }
        }
    }
    /// is `n` exactly on this grid?
    pub fn holds(&self, n: Num) -> Option<i128> {
        let k = self.index(n)?;
        (self.value(k)? == n).then_some(k)
    }
    pub fn snap(&self, n: Num) -> Option<Num> {
        self.value(self.index(n)?)
    }
    /// scaled grids: grid index and ULP offset of a double within 64 ULPs of the grid
    pub fn holds_near(&self, x: f64) -> Option<(i128, i64)> {
        let k = self.index(Num::F(x.to_bits()))?;
        let Num::F(b) = self.value(k)? else { return None };
        let j = ord(x.to_bits()) - ord(b);
        (j.abs() <= 64).then_some((k, j as i64))
    }
}

/// Smallest denominator q <= limit with k/q == x exactly as doubles (continued fractions).
pub fn rational_den(x: f64, limit: i128) -> Option<i128> {
    if !x.is_finite() || x.abs() > 1e15 {
        return None;
    }
    let (mut h0, mut h1, mut k0, mut k1) = (0i128, 1i128, 1i128, 0i128);
    let mut y = x.abs();
    for _ in 0..40 {
        let a = y.floor();
        if a > 1e15 {
            break;
        }
        let a = a as i128;
        let h2 = a.checked_mul(h1)?.checked_add(h0)?;
        let k2 = a.checked_mul(k1)?.checked_add(k0)?;
        if k2 > limit {
            break;
        }
        if (h2 as f64) / (k2 as f64) == x.abs() {
            return Some(k2);
        }
        (h0, h1, k0, k1) = (h1, h2, k1, k2);
        let fr = y - a as f64;
        if fr == 0.0 {
            break;
        }
        y = 1.0 / fr;
    }
    None
}

pub fn gcd(a: i128, b: i128) -> i128 {
    if b == 0 { a.abs() } else { gcd(b, a % b) }
}

// ------------------------------------------------------------------ deterministic log2
/// floor(16 * log2|x|) from the bits of a double (no libm); -1024 for 0
pub fn lg16(x: f64) -> i64 {
    let x = x.abs();
    if x == 0.0 || !x.is_finite() {
        return if x == 0.0 { -1024 } else { 2047 };
    }
    let b = x.to_bits();
    let ex = ((b >> 52) & 0x7ff) as i64;
    if ex == 0 {
        return -1023 * 16;
    }
    (ex - 1023) * 16 + ((b >> 48) & 15) as i64
}

/// order-preserving integer image of a double (for ulp-distance residuals)
pub fn ord(b: u64) -> i128 {
    let u = if b >> 63 == 1 { !b } else { b | 1 << 63 };
    u as i128 - (1i128 << 63)
}

pub fn unord(o: i128) -> Option<u64> {
    let u = u64::try_from(o + (1i128 << 63)).ok()?;
    Some(if u >> 63 == 1 { u & !(1 << 63) } else { !u })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numstr() {
        for t in ["0.096", "0.0300000000", "12", "0", "1.5", "-0.25", "6.100196547945206", "1e-05", "100", "-0.0"] {
            let (n, f) = parse_numstr(t).unwrap();
            assert_eq!(render(n, f).unwrap(), t, "{t}");
        }
        for t in ["-0", "01", "1.", ".5", "1e5", "abc", "", "+1", "0x1"] {
            assert!(parse_numstr(t).is_none(), "{t}");
        }
    }
    #[test]
    fn fixed_render() {
        for (m, e, k, t) in [(5, 1, 1, "0.5"), (5, 1, 3, "0.500"), (-25, 2, 2, "-0.25"), (123, 0, 0, "123"), (12345, 2, 4, "123.4500"),
                             (3, 6, 10, "0.0000030000"), (0, 0, 2, "0.00"), (-1, 0, 1, "-1.0"), (987654321987654321, 9, 9, "987654321.987654321")] {
            assert_eq!(fixed(m, e, k).unwrap(), t);
        }
        assert!(fixed(5, 2, 1).is_none());
    }
    #[test]
    fn fast_f64() {
        for t in ["0.1", "123.456", "1e-22", "9007199254740991", "0.0300000000", "1e22", "123456789012e-15", "5e-40", "-2.5"] {
            let n = parse_dec_exp(t).unwrap();
            assert_eq!(n.f64(), t.parse::<f64>().unwrap(), "{t}");
        }
    }
    #[test]
    fn grids() {
        let g = Grid { den: 8760, prec: Some(6), scaled: false };
        assert!(g.holds(parse_numstr("0.056393").unwrap().0).is_some());
        let x = 15180.0 / 8760.0;
        assert_eq!(rational_den(x, 10_000_000), Some(146));
        let g = Grid { den: 8760, prec: None, scaled: false };
        let gs = Grid { den: 730, prec: Some(6), scaled: true };
        assert!(gs.holds_near(1079.963088 / 730.0).is_some_and(|(k, j)| k == 1079963088 && j == 0));
        assert_eq!(g.holds(Num::F(x.to_bits())), Some(15180));
        assert_eq!(near(0.08705500000000001), Some((Num::D(87055, 6), 1)));
        assert_eq!(near(15.711840000000002).map(|x| x.0), Some(Num::D(1571184, 5)));
        assert_eq!(near(1.5), Some((Num::D(15, 1), 0)));
        assert_eq!(near(15180.0 / 8760.0), None);
        for x in [0.0, -0.0, 1.5, -2.75e-300, f64::MAX, f64::MIN_POSITIVE, -1.0] {
            assert_eq!(unord(ord(x.to_bits())).unwrap(), x.to_bits());
        }
        assert!(ord(1.0f64.to_bits()) < ord(1.0000001f64.to_bits()));
        assert!(ord((-1.0f64).to_bits()) < ord(0.5f64.to_bits()));
    }
}
