//! Whitespace and string-escaping layout of arbitrary JSON text.
//!
//! Every gap between two tokens has a kind (after `{`, before `:`, after `,`
//! in an array, ...) and a nesting depth. Serializers are consistent per
//! (kind, depth), so the layout is a table of the most common whitespace for
//! each, plus exceptions (gap number, whitespace) where the text differs.
//! Strings whose source escaping differs from the canonical escaping are kept
//! verbatim as exceptions as well. Numbers keep their source text in `Val`, so
//! together this reproduces any valid JSON text byte for byte (Python,
//! Jackson, Go, JavaScript, minified, CRLF, ...).
use crate::fx::Map;
use crate::json::{write_str, Val};

pub const LEAD: u8 = 0;
pub const TRAIL: u8 = 1;
const O_OPEN: u8 = 2;
const O_EMPTY: u8 = 3;
const O_BCOLON: u8 = 4;
const O_ACOLON: u8 = 5;
const O_BCOMMA: u8 = 6;
const O_ACOMMA: u8 = 7;
const O_CLOSE: u8 = 8;
const A_OPEN: u8 = 9;
const A_EMPTY: u8 = 10;
const A_BCOMMA: u8 = 11;
const A_ACOMMA: u8 = 12;
const A_CLOSE: u8 = 13;
const MAXDEP: usize = 64;

/// gap context: kind, nesting depth, and object-nesting depth (some printers,
/// e.g. Jackson's, indent by objects only and keep arrays inline)
pub fn kd(kind: u8, depth: usize, odepth: usize) -> u32 {
    let m = MAXDEP as u32 + 1;
    (kind as u32 * m + depth.min(MAXDEP) as u32) * m + odepth.min(MAXDEP) as u32
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// strings without escapes for non-ASCII characters
    pub ascii: bool,
    /// most common whitespace per kd()
    pub table: Map<u32, Vec<u8>>,
    /// (gap number, whitespace) where the text differs from the table
    pub gap_exc: Vec<(u64, Vec<u8>)>,
    /// (string number, source text incl. quotes) where escaping differs
    pub str_exc: Vec<(u64, Vec<u8>)>,
}

/// Scans valid JSON text (as accepted by `json::parse`), calling `gap` for
/// every inter-token gap and `string` for every string token, in the order the
/// writer below emits them.
struct Scan<'a, G: FnMut(u32, &[u8]), S: FnMut(&[u8])> {
    s: &'a [u8],
    pos: usize,
    gap: G,
    string: S,
}

impl<'a, G: FnMut(u32, &[u8]), S: FnMut(&[u8])> Scan<'a, G, S> {
    fn ws_end(&self) -> usize {
        let mut p = self.pos;
        while let Some(&c) = self.s.get(p) {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                p += 1;
            } else {
                break;
            }
        }
        p
    }
    fn gap_now(&mut self, kind: u8, depth: usize, od: usize) {
        let e = self.ws_end();
        (self.gap)(kd(kind, depth, od), &self.s[self.pos..e]);
        self.pos = e;
    }
    /// gap whose kind depends on the next character
    fn gap_then(&mut self, depth: usize, od: usize, if_c: u8, kind_c: u8, kind_else: u8) -> u8 {
        let e = self.ws_end();
        let next = self.s.get(e).copied().unwrap_or(0);
        let kind = if next == if_c { kind_c } else { kind_else };
        (self.gap)(kd(kind, depth, od), &self.s[self.pos..e]);
        self.pos = e;
        next
    }
    fn string_tok(&mut self) {
        let start = self.pos;
        self.pos += 1;
        while let Some(&c) = self.s.get(self.pos) {
            self.pos += 1;
            match c {
                b'\\' => self.pos += 1,
                b'"' => break,
                _ => {}
            }
        }
        (self.string)(&self.s[start..self.pos]);
    }
    fn value(&mut self, depth: usize, od: usize) {
        match self.s.get(self.pos).copied().unwrap_or(0) {
            b'{' => {
                self.pos += 1;
                let od = od + 1;
                if self.gap_then(depth, od, b'}', O_EMPTY, O_OPEN) == b'}' {
                    self.pos += 1;
                    return;
                }
                loop {
                    self.string_tok();
                    self.gap_now(O_BCOLON, depth, od);
                    self.pos += 1; // ':'
                    self.gap_now(O_ACOLON, depth, od);
                    self.value(depth + 1, od);
                    if self.gap_then(depth, od, b',', O_BCOMMA, O_CLOSE) == b',' {
                        self.pos += 1;
                        self.gap_now(O_ACOMMA, depth, od);
                    } else {
                        self.pos += 1; // '}'
                        return;
                    }
                }
            }
            b'[' => {
                self.pos += 1;
                if self.gap_then(depth, od, b']', A_EMPTY, A_OPEN) == b']' {
                    self.pos += 1;
                    return;
                }
                loop {
                    self.value(depth + 1, od);
                    if self.gap_then(depth, od, b',', A_BCOMMA, A_CLOSE) == b',' {
                        self.pos += 1;
                        self.gap_now(A_ACOMMA, depth, od);
                    } else {
                        self.pos += 1; // ']'
                        return;
                    }
                }
            }
            b'"' => self.string_tok(),
            _ => {
                // number or literal: up to the next delimiter
                while let Some(&c) = self.s.get(self.pos) {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    self.pos += 1;
                }
            }
        }
    }
    fn document(&mut self) {
        self.gap_now(LEAD, 0, 0);
        self.value(0, 0);
        self.gap_now(TRAIL, 0, 0);
    }
}

/// canonical source text of a string token's decoded content
fn canonical_string(raw: &[u8], ascii: bool) -> Option<Vec<u8>> {
    let t = std::str::from_utf8(raw).ok()?;
    let v = crate::json::parse(t, 4).ok()?;
    let Val::Str(s) = &*v else { return None };
    let mut out = Vec::with_capacity(raw.len());
    write_str(&mut out, s, ascii);
    Some(out)
}

/// Learn the layout of `raw` (valid JSON text).
pub fn learn(raw: &[u8]) -> Layout {
    let ascii = !raw.iter().any(|&b| b >= 0x80);
    // pass 1: whitespace frequencies per gap context
    let mut counts: Map<u32, Map<Vec<u8>, u64>> = Map::default();
    {
        let mut sc = Scan {
            s: raw,
            pos: 0,
            gap: |k: u32, w: &[u8]| {
                let m = counts.entry(k).or_default();
                match m.get_mut(w) {
                    Some(c) => *c += 1,
                    None => {
                        m.insert(w.to_vec(), 1);
                    }
                }
            },
            string: |_: &[u8]| {},
        };
        sc.document();
    }
    let mut table: Map<u32, Vec<u8>> = Map::default();
    for (k, m) in counts {
        // most common; ties broken by the bytes themselves (deterministic)
        let best = m.into_iter().max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0))).unwrap().0;
        table.insert(k, best);
    }
    // pass 2: exceptions
    let mut gap_exc = Vec::new();
    let mut str_exc = Vec::new();
    {
        let (mut gi, mut si) = (0u64, 0u64);
        let tbl = &table;
        let mut sc = Scan {
            s: raw,
            pos: 0,
            gap: |k: u32, w: &[u8]| {
                if tbl.get(&k).map(|x| &x[..]) != Some(w) {
                    gap_exc.push((gi, w.to_vec()));
                }
                gi += 1;
            },
            string: |r: &[u8]| {
                // fast path: no backslash and (ascii mode) no non-ASCII: canonical iff no control chars
                let simple = !r.contains(&b'\\') && r.iter().all(|&b| b >= 0x20 && (!ascii || b < 0x7f));
                if !simple && canonical_string(r, ascii).as_deref() != Some(r) {
                    str_exc.push((si, r.to_vec()));
                }
                si += 1;
            },
        };
        sc.document();
    }
    Layout { ascii, table, gap_exc, str_exc }
}

/// Serializes `v` with a layout (the inverse of `learn` + `json::parse`).
pub fn dumps(v: &Val, l: &Layout) -> Vec<u8> {
    let mut w = W { l, out: Vec::new(), gi: 0, si: 0, ge: 0, se: 0 };
    w.gap(LEAD, 0, 0);
    w.value(v, 0, 0);
    w.gap(TRAIL, 0, 0);
    w.out
}

/// Writes a non-empty root array element by element (for streaming); the
/// concatenated output equals `dumps` of the whole array.
pub struct ArrayWriter {
    l: Layout,
    gi: u64,
    si: u64,
    ge: usize,
    se: usize,
}

impl ArrayWriter {
    pub fn new(l: Layout) -> ArrayWriter {
        ArrayWriter { l, gi: 0, si: 0, ge: 0, se: 0 }
    }

    /// run `f` on a writer that continues where the last call stopped
    fn with(&mut self, out: &mut Vec<u8>, f: impl FnOnce(&mut W)) {
        let mut w = W { l: &self.l, out: std::mem::take(out), gi: self.gi, si: self.si, ge: self.ge, se: self.se };
        f(&mut w);
        (self.gi, self.si, self.ge, self.se) = (w.gi, w.si, w.ge, w.se);
        *out = w.out;
    }

    pub fn begin(&mut self, out: &mut Vec<u8>) {
        self.with(out, |w| {
            w.gap(LEAD, 0, 0);
            w.out.push(b'[');
        });
    }

    pub fn item(&mut self, v: &Val, first: bool, out: &mut Vec<u8>) {
        self.with(out, |w| {
            if first {
                w.gap(A_OPEN, 0, 0);
            } else {
                w.gap(A_BCOMMA, 0, 0);
                w.out.push(b',');
                w.gap(A_ACOMMA, 0, 0);
            }
            w.value(v, 1, 0);
        });
    }

    pub fn end(&mut self, out: &mut Vec<u8>) {
        self.with(out, |w| {
            w.gap(A_CLOSE, 0, 0);
            w.out.push(b']');
            w.gap(TRAIL, 0, 0);
        });
    }
}

struct W<'a> {
    l: &'a Layout,
    out: Vec<u8>,
    gi: u64,
    si: u64,
    ge: usize,
    se: usize,
}

impl W<'_> {
    fn gap(&mut self, kind: u8, depth: usize, od: usize) {
        match self.l.gap_exc.get(self.ge) {
            Some((i, w)) if *i == self.gi => {
                self.out.extend_from_slice(w);
                self.ge += 1;
            }
            _ => {
                if let Some(w) = self.l.table.get(&kd(kind, depth, od)) {
                    self.out.extend_from_slice(w);
                }
            }
        }
        self.gi += 1;
    }
    fn string(&mut self, s: &str) {
        match self.l.str_exc.get(self.se) {
            Some((i, r)) if *i == self.si => {
                self.out.extend_from_slice(r);
                self.se += 1;
            }
            _ => write_str(&mut self.out, s, self.l.ascii),
        }
        self.si += 1;
    }
    fn value(&mut self, v: &Val, depth: usize, od: usize) {
        match v {
            Val::Obj(o) => {
                self.out.push(b'{');
                let od = od + 1;
                if o.is_empty() {
                    self.gap(O_EMPTY, depth, od);
                } else {
                    for (i, (k, x)) in o.iter().enumerate() {
                        if i > 0 {
                            self.gap(O_BCOMMA, depth, od);
                            self.out.push(b',');
                            self.gap(O_ACOMMA, depth, od);
                        } else {
                            self.gap(O_OPEN, depth, od);
                        }
                        self.string(k);
                        self.gap(O_BCOLON, depth, od);
                        self.out.push(b':');
                        self.gap(O_ACOLON, depth, od);
                        self.value(x, depth + 1, od);
                    }
                    self.gap(O_CLOSE, depth, od);
                }
                self.out.push(b'}');
            }
            Val::Arr(a) => {
                self.out.push(b'[');
                if a.is_empty() {
                    self.gap(A_EMPTY, depth, od);
                } else {
                    for (i, x) in a.iter().enumerate() {
                        if i > 0 {
                            self.gap(A_BCOMMA, depth, od);
                            self.out.push(b',');
                            self.gap(A_ACOMMA, depth, od);
                        } else {
                            self.gap(A_OPEN, depth, od);
                        }
                        self.value(x, depth + 1, od);
                    }
                    self.gap(A_CLOSE, depth, od);
                }
                self.out.push(b']');
            }
            Val::Str(s) => self.string(s),
            Val::Int(t) | Val::Float(t) => self.out.extend_from_slice(t.as_bytes()),
            Val::Bool(true) => self.out.extend_from_slice(b"true"),
            Val::Bool(false) => self.out.extend_from_slice(b"false"),
            Val::Null => self.out.extend_from_slice(b"null"),
        }
    }
}
