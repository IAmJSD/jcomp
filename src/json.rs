//! JSON values, a parser and a serializer that follow Python's `json` module
//! exactly (number normalisation, float `repr`, `ensure_ascii` escaping,
//! duplicate keys, indentation), so `dumps(loads(x)) == x` holds in Rust
//! precisely when it holds in Python.
use crate::fx::Map;
use std::sync::Arc;

pub type Str = Arc<str>;

#[derive(Debug)]
pub enum Val {
    Null,
    Bool(bool),
    /// integer number token, as written in the source
    Int(Box<str>),
    /// non-integer number token (fraction/exponent, NaN, Infinity), as written
    Float(Box<str>),
    Str(Str),
    Arr(Vec<Arc<Val>>),
    Obj(Vec<(Str, Arc<Val>)>),
}

pub type V = Arc<Val>;

impl Val {
    pub fn obj(&self) -> Option<&[(Str, V)]> {
        match self {
            Val::Obj(o) => Some(o),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Look up `k` in an object, trying position `hint` first (analog dicts
/// usually share key order).
pub fn get<'a>(o: &'a [(Str, V)], k: &str, hint: usize) -> Option<&'a V> {
    if let Some((kk, v)) = o.get(hint) {
        if &**kk == k {
            return Some(v);
        }
    }
    o.iter().find(|(kk, _)| &**kk == k).map(|(_, v)| v)
}

// ------------------------------------------------------------------ parsing
pub struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
    depth: usize,
    max_depth: usize,
    keys: Map<Box<str>, Str>,
}

pub type PResult<T> = Result<T, String>;

pub fn parse(text: &str, max_depth: usize) -> PResult<V> {
    let mut p = Parser { s: text.as_bytes(), pos: 0, depth: 0, max_depth, keys: Map::default() };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.pos != p.s.len() {
        return Err("extra data".into());
    }
    Ok(v)
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while let Some(&c) = self.s.get(self.pos) {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn lit(&mut self, t: &str) -> bool {
        if self.s[self.pos..].starts_with(t.as_bytes()) {
            self.pos += t.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> PResult<V> {
        let c = *self.s.get(self.pos).ok_or("eof")?;
        Ok(Arc::new(match c {
            b'{' => self.object()?,
            b'[' => self.array()?,
            b'"' => Val::Str(self.string()?.into()),
            b'n' if self.lit("null") => Val::Null,
            b't' if self.lit("true") => Val::Bool(true),
            b'f' if self.lit("false") => Val::Bool(false),
            b'N' if self.lit("NaN") => Val::Float("NaN".into()),
            b'I' if self.lit("Infinity") => Val::Float("Infinity".into()),
            b'-' if self.lit("-Infinity") => Val::Float("-Infinity".into()),
            b'-' | b'0'..=b'9' => self.number()?,
            _ => return Err("unexpected character".into()),
        }))
    }

    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth > self.max_depth {
            return Err("nesting too deep".into());
        }
        Ok(())
    }

    fn object(&mut self) -> PResult<Val> {
        self.enter()?;
        self.pos += 1;
        let mut out: Vec<(Str, V)> = Vec::new();
        let mut index: Option<Map<Str, usize>> = None;
        self.ws();
        if self.s.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Val::Obj(out));
        }
        loop {
            self.ws();
            if self.s.get(self.pos) != Some(&b'"') {
                return Err("expected key".into());
            }
            let k = self.string()?;
            let k: Str = match self.keys.get(k.as_str()) {
                Some(r) => r.clone(),
                None => {
                    let r: Str = k.as_str().into();
                    self.keys.insert(k.into_boxed_str(), r.clone());
                    r
                }
            };
            self.ws();
            if self.s.get(self.pos) != Some(&b':') {
                return Err("expected ':'".into());
            }
            self.pos += 1;
            self.ws();
            let v = self.value()?;
            // Python dict semantics: a repeated key keeps its first position, last value
            if out.len() > 16 && index.is_none() {
                index = Some(out.iter().enumerate().map(|(i, (k, _))| (k.clone(), i)).collect());
            }
            let found = match &index {
                Some(ix) => ix.get(&k).copied(),
                None => out.iter().position(|(kk, _)| *kk == k),
            };
            match found {
                Some(i) => out[i].1 = v,
                None => {
                    if let Some(ix) = &mut index {
                        ix.insert(k.clone(), out.len());
                    }
                    out.push((k, v));
                }
            }
            self.ws();
            match self.s.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("expected ',' or '}'".into()),
            }
        }
        self.depth -= 1;
        Ok(Val::Obj(out))
    }

    fn array(&mut self) -> PResult<Val> {
        self.enter()?;
        self.pos += 1;
        let mut out = Vec::new();
        self.ws();
        if self.s.get(self.pos) == Some(&b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Val::Arr(out));
        }
        loop {
            self.ws();
            out.push(self.value()?);
            self.ws();
            match self.s.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("expected ',' or ']'".into()),
            }
        }
        self.depth -= 1;
        Ok(Val::Arr(out))
    }

    fn hex4(&mut self) -> PResult<u32> {
        let h = self.s.get(self.pos..self.pos + 4).ok_or("bad \\u escape")?;
        let t = std::str::from_utf8(h).map_err(|_| "bad \\u escape")?;
        if !t.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("bad \\u escape".into());
        }
        self.pos += 4;
        Ok(u32::from_str_radix(t, 16).unwrap())
    }

    fn string(&mut self) -> PResult<String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(&c) = self.s.get(self.pos) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // input is valid UTF-8 and we only stop on ASCII bytes
            out.push_str(unsafe { std::str::from_utf8_unchecked(&self.s[start..self.pos]) });
            match self.s.get(self.pos) {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let e = *self.s.get(self.pos).ok_or("eof in string")?;
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut u = self.hex4()?;
                            if (0xd800..0xdc00).contains(&u) && self.s[self.pos..].starts_with(b"\\u") {
                                let save = self.pos;
                                self.pos += 2;
                                let u2 = self.hex4()?;
                                if (0xdc00..0xe000).contains(&u2) {
                                    u = 0x10000 + ((u - 0xd800) << 10) + (u2 - 0xdc00);
                                } else {
                                    self.pos = save;
                                }
                            }
                            // lone surrogates are legal in Python but not in a Rust String
                            out.push(char::from_u32(u).ok_or("lone surrogate")?);
                        }
                        _ => return Err("bad escape".into()),
                    }
                }
                _ => return Err("invalid control character or eof in string".into()),
            }
        }
    }

    fn number(&mut self) -> PResult<Val> {
        let s = self.s;
        let start = self.pos;
        let mut p = self.pos;
        if s.get(p) == Some(&b'-') {
            p += 1;
        }
        match s.get(p) {
            Some(b'0') => p += 1,
            Some(b'1'..=b'9') => {
                while matches!(s.get(p), Some(b'0'..=b'9')) {
                    p += 1;
                }
            }
            _ => return Err("bad number".into()),
        }
        let mut float = false;
        if s.get(p) == Some(&b'.') && matches!(s.get(p + 1), Some(b'0'..=b'9')) {
            float = true;
            p += 1;
            while matches!(s.get(p), Some(b'0'..=b'9')) {
                p += 1;
            }
        }
        if matches!(s.get(p), Some(b'e' | b'E')) {
            let mut q = p + 1;
            if matches!(s.get(q), Some(b'+' | b'-')) {
                q += 1;
            }
            if matches!(s.get(q), Some(b'0'..=b'9')) {
                float = true;
                while matches!(s.get(q), Some(b'0'..=b'9')) {
                    q += 1;
                }
                p = q;
            }
        }
        self.pos = p;
        let t = unsafe { std::str::from_utf8_unchecked(&s[start..p]) };
        // numbers keep their source text, so any serializer's style round-trips
        if float {
            return Ok(Val::Float(t.into()));
        }
        Ok(Val::Int(t.into()))
    }
}

// ------------------------------------------------------------------ Python float repr
/// Python `repr(float)`: shortest round-trip digits, exponent form when the
/// decimal point position is <= -4 or > 16.
pub fn py_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    // fixed notation (decimal point position -3..=16): Rust's shortest digits,
    // plus ".0" for integral values
    let a = x.abs();
    if a == 0.0 || (1e-4..1e16).contains(&a) {
        let mut s = format!("{}", x);
        if !s.contains('.') {
            s.push_str(".0");
        }
        return s;
    }
    py_repr_general(x)
}

fn py_repr_general(x: f64) -> String {
    let e = format!("{:e}", x);
    let (neg, e) = match e.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, e.as_str()),
    };
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let nd = digits.len() as i32;
    let decpt = exp + 1;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if decpt > -4 && decpt <= 16 {
        if decpt <= 0 {
            out.push_str("0.");
            for _ in 0..-decpt {
                out.push('0');
            }
            out.push_str(&digits);
        } else if decpt >= nd {
            out.push_str(&digits);
            for _ in 0..decpt - nd {
                out.push('0');
            }
            out.push_str(".0");
        } else {
            out.push_str(&digits[..decpt as usize]);
            out.push('.');
            out.push_str(&digits[decpt as usize..]);
        }
    } else {
        out.push_str(&digits[..1]);
        if nd > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exp.abs()));
    }
    out
}

// ------------------------------------------------------------------ serializing
#[derive(Clone, Debug)]
pub struct Style {
    pub indent: Option<String>,
    pub item_sep: String,
    pub key_sep: String,
    pub ensure_ascii: bool,
}

impl Style {
    /// `json.dumps(indent=..., separators=..., ensure_ascii=...)`
    pub fn new(indent: Option<String>, separators: Option<(String, String)>, ensure_ascii: bool) -> Style {
        let (item_sep, key_sep) = separators.unwrap_or_else(|| {
            if indent.is_some() { (",".into(), ": ".into()) } else { (", ".into(), ": ".into()) }
        });
        Style { indent, item_sep, key_sep, ensure_ascii }
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn push_u(out: &mut Vec<u8>, u: u32) {
    out.extend_from_slice(b"\\u");
    for sh in [12, 8, 4, 0] {
        out.push(HEX[((u >> sh) & 15) as usize]);
    }
}

pub fn write_str(out: &mut Vec<u8>, s: &str, ascii: bool) {
    out.push(b'"');
    let b = s.as_bytes();
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        let c = ch as u32;
        let plain = if ascii { (0x20..0x7f).contains(&c) && ch != '"' && ch != '\\' } else { c >= 0x20 && ch != '"' && ch != '\\' };
        if plain {
            continue;
        }
        out.extend_from_slice(&b[start..i]);
        start = i + ch.len_utf8();
        match ch {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            _ if c < 0x10000 => push_u(out, c),
            _ => {
                let v = c - 0x10000;
                push_u(out, 0xd800 | (v >> 10));
                push_u(out, 0xdc00 | (v & 0x3ff));
            }
        }
    }
    out.extend_from_slice(&b[start..]);
    out.push(b'"');
}

fn newline(out: &mut Vec<u8>, ind: &str, level: usize) {
    out.push(b'\n');
    for _ in 0..level {
        out.extend_from_slice(ind.as_bytes());
    }
}

/// Serialize like Python's `json.dumps(v, **style)`.
pub fn dump(v: &Val, st: &Style, out: &mut Vec<u8>, level: usize) {
    match v {
        Val::Null => out.extend_from_slice(b"null"),
        Val::Bool(true) => out.extend_from_slice(b"true"),
        Val::Bool(false) => out.extend_from_slice(b"false"),
        Val::Int(t) => out.extend_from_slice(t.as_bytes()),
        Val::Float(t) => out.extend_from_slice(t.as_bytes()),
        Val::Str(s) => write_str(out, s, st.ensure_ascii),
        Val::Arr(a) => {
            if a.is_empty() {
                return out.extend_from_slice(b"[]");
            }
            out.push(b'[');
            for (k, x) in a.iter().enumerate() {
                if k > 0 {
                    out.extend_from_slice(st.item_sep.as_bytes());
                }
                if let Some(ind) = &st.indent {
                    newline(out, ind, level + 1);
                }
                dump(x, st, out, level + 1);
            }
            if let Some(ind) = &st.indent {
                newline(out, ind, level);
            }
            out.push(b']');
        }
        Val::Obj(o) => {
            if o.is_empty() {
                return out.extend_from_slice(b"{}");
            }
            out.push(b'{');
            for (k, (key, x)) in o.iter().enumerate() {
                if k > 0 {
                    out.extend_from_slice(st.item_sep.as_bytes());
                }
                if let Some(ind) = &st.indent {
                    newline(out, ind, level + 1);
                }
                write_str(out, key, st.ensure_ascii);
                out.extend_from_slice(st.key_sep.as_bytes());
                dump(x, st, out, level + 1);
            }
            if let Some(ind) = &st.indent {
                newline(out, ind, level);
            }
            out.push(b'}');
        }
    }
}

/// `dumps` of a top-level array with its elements serialized in parallel
/// (byte-identical to `dumps`).
pub fn dumps_par(v: &Val, st: &Style, threads: usize) -> Vec<u8> {
    let Val::Arr(a) = v else { return dumps(v, st) };
    if a.is_empty() || threads <= 1 {
        return dumps(v, st);
    }
    let parts = threads * 4;
    let jobs: Vec<(usize, usize)> = (0..parts).map(|k| (k * a.len() / parts, (k + 1) * a.len() / parts)).filter(|(x, y)| x < y).collect();
    let pieces = crate::par::par_map(jobs, threads, |(x, y)| {
        let mut out = Vec::new();
        for (k, item) in a.iter().enumerate().take(y).skip(x) {
            dump_item(item, st, k == 0, &mut out);
        }
        out
    });
    let mut out = Vec::with_capacity(pieces.iter().map(|p| p.len()).sum::<usize>() + 8);
    out.push(b'[');
    for p in pieces {
        out.extend_from_slice(&p);
    }
    dump_array_end(st, &mut out);
    out
}

/// One element of a non-empty top-level array as `dumps` writes it: the
/// separator before it (unless it is the first), its indentation, the value.
pub fn dump_item(v: &Val, st: &Style, first: bool, out: &mut Vec<u8>) {
    if !first {
        out.extend_from_slice(st.item_sep.as_bytes());
    }
    if let Some(ind) = &st.indent {
        newline(out, ind, 1);
    }
    dump(v, st, out, 1);
}

/// The end of a non-empty top-level array as `dumps` writes it.
pub fn dump_array_end(st: &Style, out: &mut Vec<u8>) {
    if let Some(ind) = &st.indent {
        newline(out, ind, 0);
    }
    out.push(b']');
}

pub fn dumps(v: &Val, st: &Style) -> Vec<u8> {
    let mut out = Vec::new();
    dump(v, st, &mut out, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repr() {
        for (x, s) in [(0.0, "0.0"), (-0.0, "-0.0"), (1e16, "1e+16"), (1e15, "1000000000000000.0"), (0.0001, "0.0001"),
                       (0.00001, "1e-05"), (1.5e300, "1.5e+300"), (218.75, "218.75"), (0.1, "0.1"), (123456789.0, "123456789.0"),
                       (1.2345e-7, "1.2345e-07"), (5e-324, "5e-324"), (1.7976931348623157e308, "1.7976931348623157e+308")] {
            assert_eq!(py_repr(x), s);
        }
        // the fixed-notation fast path agrees with the general formatting
        let mut r = 0x9e3779b97f4a7c15u64;
        for _ in 0..200_000 {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            let x = f64::from_bits(r);
            let small = (r % 100_000_000) as f64;
            for y in [x, small / 1e4, -small / 1e12, 1e-4 * (1.0 + small / 1e8), 9.99e15 + small, small] {
                if y.is_finite() {
                    assert_eq!(py_repr(y), py_repr_general(y));
                }
            }
        }
    }
}
