//! Binary adaptive range coder (LZMA-style carry handling) plus the context
//! machinery: every context is an interned tuple of components, mirroring the
//! Python tuples used as dict keys (so equal tuples share one probability).
use crate::fx::Map;

const PBITS: u32 = 16;
const PONE: u32 = 1 << PBITS;
const TOP: u32 = 1 << 24;
const LIMIT: u8 = 4;
const MIX_LR: i32 = 1;
const MIX_SH: i32 = 10;

// tuple components: tagged u64
const T_STR: u64 = 1 << 56;
pub const NONE: u64 = 2 << 56;
const T_TUP: u64 = 3 << 56;
/// context-state table size (log2)
const TBITS: u32 = 24;
const TMASK: usize = (1 << TBITS) - 1;
const HALF: u16 = 0x8000;

/// integer component (Python ints and bools; True == 1 like in Python)
#[inline]
pub fn ki(i: i64) -> u64 {
    i as u32 as u64
}
/// interned string component
#[inline]
pub fn ks(id: u32) -> u64 {
    T_STR | id as u64
}

#[derive(Clone)]
pub struct Strings {
    map: Map<Box<str>, u32>,
    pub names: Vec<Box<str>>,
}

impl Strings {
    pub fn new() -> Strings {
        Strings { map: Map::default(), names: Vec::new() }
    }
    pub fn id(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.map.get(s) {
            return i;
        }
        let i = self.names.len() as u32;
        self.map.insert(s.into(), i);
        self.names.push(s.into());
        i
    }
    pub fn c(&mut self, s: &str) -> u64 {
        ks(self.id(s))
    }
}

/// logistic squash, 12-bit domain, integer-only (deterministic everywhere)
fn squash(d: i32) -> i32 {
    const T: [i32; 33] = [
        1, 2, 3, 6, 10, 16, 27, 45, 73, 120, 194, 310, 488, 747, 1101, 1546, 2047, 2549, 2994, 3348, 3607, 3785, 3901, 3975, 4022, 4050, 4068,
        4079, 4085, 4089, 4092, 4093, 4094,
    ];
    if d > 2047 {
        return 4095;
    }
    if d < -2047 {
        return 1;
    }
    let w = d & 127;
    let i = ((d >> 7) + 16) as usize;
    (T[i] * (128 - w) + T[i + 1] * w + 64) >> 7
}

fn stretch_table() -> Vec<i16> {
    let mut t = vec![2047i16; 4096];
    let mut pi = 0usize;
    for x in -2047..=2047 {
        let v = squash(x) as usize;
        for item in t.iter_mut().take(v + 1).skip(pi) {
            *item = x as i16;
        }
        pi = pi.max(v + 1);
    }
    t
}

const NW: usize = 256;

#[derive(Clone)]
pub struct Coder {
    pub enc: bool,
    // encoder
    low: u64,
    rng: u32,
    cache: u8,
    csize: u64,
    out: Vec<u8>,
    // decoder
    data: Vec<u8>,
    pos: usize,
    code: u64,
    // contexts
    /// context state, indexed by a hash of the context tuple (allocated on first use)
    p: Vec<u16>,
    n: Vec<u8>,
    /// mixer: stretch table and weight sets (16.16 fixed point) for `bit2`
    st: std::sync::Arc<Vec<i16>>,
    w: Vec<[i32; 4]>,
    pub s: Strings,
    /// encoder-side cost accounting (1/256 bit units) per bucket, for split decisions
    pub costs: Option<Vec<u64>>,
    pub bucket: usize,
    cost_tab: Vec<u32>,
}

pub type CResult<T> = Result<T, String>;

impl Coder {
    fn new(enc: bool, data: Vec<u8>) -> Coder {
        let code = if data.len() >= 5 { u32::from_be_bytes(data[1..5].try_into().unwrap()) as u64 } else { 0 };
        Coder {
            enc,
            low: 0,
            rng: 0xFFFF_FFFF,
            cache: 0,
            csize: 1,
            out: Vec::new(),
            data,
            pos: 5,
            code,
            p: Vec::new(),
            n: Vec::new(),
            st: std::sync::Arc::new(stretch_table()),
            w: vec![[1 << 15, 1 << 15, 0, 0]; NW],
            s: Strings::new(),
            costs: None,
            bucket: 0,
            cost_tab: Vec::new(),
        }
    }

    pub fn cost_total(&self) -> Option<u64> {
        self.costs.as_ref().map(|c| c.iter().sum())
    }

    /// start attributing coded bits to `nbuckets` buckets (see `bucket`)
    pub fn track_costs(&mut self, nbuckets: usize) {
        self.costs = Some(vec![0; nbuckets]);
        // -log2(p / 4096) * 256 for the probability of the coded symbol
        self.cost_tab = (0..4096u32).map(|q| (-((q.max(1) as f64) / 4096.0).log2() * 256.0) as u32).collect();
    }
    /// Same contexts and learned probabilities, fresh stream (to encode, or to
    /// decode `data`). Used to continue a trained model on several chunks.
    pub fn fork(&self, data: Option<Vec<u8>>) -> Coder {
        let fresh = match data {
            Some(d) => Coder::decoder(d),
            None => Coder::encoder(),
        };
        Coder {
            p: self.p.clone(),
            n: self.n.clone(),
            st: self.st.clone(),
            w: self.w.clone(),
            s: self.s.clone(),
            costs: self.costs.as_ref().map(|c| vec![0; c.len()]),
            cost_tab: self.cost_tab.clone(),
            ..fresh
        }
    }

    pub fn encoder() -> Coder {
        Coder::new(true, Vec::new())
    }
    pub fn decoder(data: Vec<u8>) -> Coder {
        Coder::new(false, data)
    }

    /// A context tuple as a component usable in other tuples. Its state slot is
    /// a hash of the tuple (as in PAQ-family coders): no interning map; the rare
    /// collisions only share statistics and are identical in encoder and decoder.
    #[inline]
    pub fn t<const N: usize>(&mut self, parts: &[u64; N]) -> u64 {
        if self.p.is_empty() {
            // stored XOR 1/2 so the fresh table is all zeros (lazily mapped pages)
            self.p = vec![0; 1 << TBITS];
            self.n = vec![0; 1 << TBITS];
        }
        let mut h: u64 = 0x9e37_79b9_7f4a_7c15 ^ N as u64;
        for &x in parts.iter() {
            h = (h ^ x).wrapping_mul(0xff51_afd7_ed55_8ccd);
            h ^= h >> 29;
        }
        h = (h ^ (h >> 32)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        h ^= h >> 31;
        T_TUP | (h & ((1 << 56) - 1))
    }

    fn shift(&mut self) {
        let low = self.low;
        if low < 0xFF00_0000 || low >= 1 << 32 {
            let carry = (low >> 32) as u8;
            self.out.push(self.cache.wrapping_add(carry));
            for _ in 1..self.csize {
                self.out.push(0xFFu8.wrapping_add(carry));
            }
            self.csize = 0;
            self.cache = ((low >> 24) & 0xFF) as u8;
        }
        self.csize += 1;
        self.low = (low & 0x00FF_FFFF) << 8;
    }

    #[inline]
    fn byte(&mut self) -> u64 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b as u64
    }

    /// Code one bit in context `ctx` (an interned tuple). The encoder codes `b`;
    /// the decoder ignores it and returns the decoded bit.
    pub fn bit(&mut self, ctx: u64, b: bool) -> bool {
        let i = (ctx as usize) & TMASK;
        let p = (self.p[i] ^ HALF) as u32;
        let bound = (self.rng >> PBITS) * p;
        let n = self.n[i];
        let sh = if n < LIMIT - 1 { n + 1 } else { LIMIT };
        self.n[i] = n.saturating_add(1);
        let b = if self.enc {
            if b {
                self.low += bound as u64;
                self.rng -= bound;
            } else {
                self.rng = bound;
            }
            b
        } else if self.code >= bound as u64 {
            self.code -= bound as u64;
            self.rng -= bound;
            true
        } else {
            self.rng = bound;
            false
        };
        if let Some(cs) = &mut self.costs {
            let q = if b { PONE - p } else { p } >> 4;
            cs[self.bucket] += self.cost_tab[q as usize] as u64;
        }
        let p = if b { p - (p >> sh) } else { p + ((PONE - p) >> sh) };
        self.p[i] = p.clamp(32, PONE - 32) as u16 ^ HALF;
        while self.rng < TOP {
            self.rng <<= 8;
            if self.enc {
                self.shift();
            } else {
                self.code = (self.code << 8) | self.byte();
            }
        }
        b
    }

    /// probability of a 1 (12-bit) from context state i
    #[inline]
    fn p1(&self, i: usize) -> i32 {
        (4096 - ((self.p[i] ^ HALF) as i32 >> 4)).clamp(1, 4095)
    }

    #[inline]
    fn adapt(&mut self, i: usize, b: bool) {
        let p = (self.p[i] ^ HALF) as u32;
        let n = self.n[i];
        let sh = if n < LIMIT - 1 { n + 1 } else { LIMIT };
        self.n[i] = n.saturating_add(1);
        let p = if b { p - (p >> sh) } else { p + ((PONE - p) >> sh) };
        self.p[i] = p.clamp(32, PONE - 32) as u16 ^ HALF;
    }

    /// Code a bit from N (<= 3) contexts mixed by a learned logistic mixer.
    pub fn bitn<const N: usize>(&mut self, cs: &[u64; N], ws: usize, b: bool) -> bool {
        let mut idx = [0usize; 3];
        let mut st = [0i32; 4];
        for k in 0..N {
            idx[k] = (cs[k] as usize) & TMASK;
            st[k] = self.st[self.p1(idx[k]) as usize] as i32;
        }
        st[N] = 256;
        // weight set: bit type x how much the specific context has seen
        let nseen = self.n[idx[0]];
        let ws = ws * 4 + if nseen == 0 { 0 } else if nseen < 4 { 1 } else if nseen < 16 { 2 } else { 3 };
        let w = self.w[ws];
        let mut dot: i64 = 0;
        for k in 0..=N {
            dot += w[k] as i64 * st[k] as i64;
        }
        let pm = squash(((dot >> 16) as i32).clamp(-2047, 2047));
        let p0 = (((4096 - pm) << 4) as u32).clamp(32, PONE - 32);
        let bound = (self.rng >> PBITS) * p0;
        let b = if self.enc {
            if b {
                self.low += bound as u64;
                self.rng -= bound;
            } else {
                self.rng = bound;
            }
            b
        } else if self.code >= bound as u64 {
            self.code -= bound as u64;
            self.rng -= bound;
            true
        } else {
            self.rng = bound;
            false
        };
        if let Some(cs) = &mut self.costs {
            let q = if b { PONE - p0 } else { p0 } >> 4;
            cs[self.bucket] += self.cost_tab[q as usize] as u64;
        }
        // train the mixer, then each context estimate
        let err = ((b as i32) << 12) - pm;
        let wr = &mut self.w[ws];
        for k in 0..=N {
            wr[k] += (st[k] * err * MIX_LR) >> MIX_SH;
        }
        for &i in idx.iter().take(N) {
            self.adapt(i, b);
        }
        while self.rng < TOP {
            self.rng <<= 8;
            if self.enc {
                self.shift();
            } else {
                self.code = (self.code << 8) | self.byte();
            }
        }
        b
    }

    /// Code `nbits` equiprobable bits of `v` (msb first).
    pub fn raw(&mut self, v: u128, nbits: u32) -> u128 {
        if let Some(cs) = &mut self.costs {
            cs[self.bucket] += 256 * nbits as u64;
        }
        let mut out = 0u128;
        for k in (0..nbits).rev() {
            self.rng >>= 1;
            let b = if self.enc {
                let b = (v >> k) & 1 == 1;
                if b {
                    self.low += self.rng as u64;
                }
                b
            } else if self.code >= self.rng as u64 {
                self.code -= self.rng as u64;
                true
            } else {
                false
            };
            out = (out << 1) | b as u128;
            while self.rng < TOP {
                self.rng <<= 8;
                if self.enc {
                    self.shift();
                } else {
                    self.code = (self.code << 8) | self.byte();
                }
            }
        }
        out
    }

    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift();
        }
        self.out
    }
}

fn bit_length(v: u128) -> u32 {
    128 - v.leading_zeros()
}

/// Names of the string components used by the generic integer coders.
#[derive(Clone)]
pub struct Tags {
    pub l: u64,
    pub m: u64,
    pub z: u64,
    pub s: u64,
}

impl Tags {
    pub fn new(s: &mut Strings) -> Tags {
        Tags { l: s.c("L"), m: s.c("M"), z: s.c("z"), s: s.c("s") }
    }
}

/// Elias-gamma style: bit length in unary (context coded), then the mantissa
/// with its top five bits context coded and the rest raw.
pub fn code_uint(c: &mut Coder, tg: &Tags, ctx: u64, v: u128) -> CResult<u128> {
    let enc = c.enc;
    let nb = if enc { bit_length(v) } else { 0 };
    let mut k: u32 = 0;
    loop {
        let t = c.t(&[ctx, tg.l, ki(k as i64)]);
        if c.bit(t, k == nb) {
            break;
        }
        k += 1;
        if k > 127 {
            return Err("corrupt stream (integer too long)".into());
        }
    }
    let nb = k;
    if nb <= 1 {
        return Ok(nb as u128);
    }
    let mut val: u128 = 1;
    for j in (0..nb - 1).rev() {
        let bv = enc && (v >> j) & 1 == 1;
        let b = if j as i64 >= nb as i64 - 6 {
            let t = c.t(&[ctx, tg.m, ki(nb as i64), ki(val as i64)]);
            c.bit(t, bv)
        } else {
            c.raw(bv as u128, 1) == 1
        };
        val = (val << 1) | b as u128;
    }
    Ok(val)
}

/// `code_uint` with every bit mixed from contexts `cs` (specific first)
pub fn code_uintn<const N: usize>(c: &mut Coder, tg: &Tags, cs: &[u64; N], v: u128) -> CResult<u128> {
    let enc = c.enc;
    let nb = if enc { bit_length(v) } else { 0 };
    let mut k: u32 = 0;
    loop {
        let mut t = [0u64; N];
        for i in 0..N {
            t[i] = c.t(&[cs[i], tg.l, ki(k as i64)]);
        }
        if c.bitn(&t, 4 + k.min(11) as usize, k == nb) {
            break;
        }
        k += 1;
        if k > 127 {
            return Err("corrupt stream (integer too long)".into());
        }
    }
    let nb = k;
    if nb <= 1 {
        return Ok(nb as u128);
    }
    let mut val: u128 = 1;
    for j in (0..nb - 1).rev() {
        let bv = enc && (v >> j) & 1 == 1;
        let b = if j as i64 >= nb as i64 - 6 {
            let mut t = [0u64; N];
            for i in 0..N {
                t[i] = c.t(&[cs[i], tg.m, ki(nb as i64), ki(val as i64)]);
            }
            c.bitn(&t, 16 + (nb.min(31) as usize) % 32, bv)
        } else {
            c.raw(bv as u128, 1) == 1
        };
        val = (val << 1) | b as u128;
    }
    Ok(val)
}

pub fn code_uint2(c: &mut Coder, tg: &Tags, ctx: u64, cc: u64, v: u128) -> CResult<u128> {
    code_uintn(c, tg, &[ctx, cc], v)
}

/// `code_sint` with every bit mixed from contexts `cs` (specific first)
pub fn code_sintn<const N: usize>(c: &mut Coder, tg: &Tags, cs: &[u64; N], v: i128) -> CResult<i128> {
    let mut t = [0u64; N];
    for i in 0..N {
        t[i] = c.t(&[cs[i], tg.z]);
    }
    if c.bitn(&t, 2, v == 0) {
        return Ok(0);
    }
    for i in 0..N {
        t[i] = c.t(&[cs[i], tg.s]);
    }
    let neg = c.bitn(&t, 3, v < 0);
    let m = code_uintn(c, tg, cs, if c.enc { v.unsigned_abs() - 1 } else { 0 })? as i128 + 1;
    Ok(if neg { -m } else { m })
}

pub fn code_sint2(c: &mut Coder, tg: &Tags, ctx: u64, cc: u64, v: i128) -> CResult<i128> {
    code_sintn(c, tg, &[ctx, cc], v)
}

