//! Schema-free JSON model.
//!
//! Every value has a *signature* (its path with array indices and map keys
//! wildcarded) and *coordinates* (`Dims`: the array indices and map keys along
//! its path). Objects whose children are all containers are treated as maps
//! (their keys are data, e.g. regions or operating systems); other objects are
//! records (their keys are schema and become part of the signature).
//!
//! A numeric leaf is predicted from values at neighbouring coordinates: the
//! previous array element, the previously coded or most common sibling map key,
//! and the first / previous field coded in the same object. Predictions are
//! copies, additive and ratio parallelograms over pairs of coordinates, and
//! differences shared between sibling map entries. The coder signals which
//! prediction is exact, or codes a residual against the historically best one,
//! on a decimal, rational (`k/den`) or ULP grid.
use std::hash::Hasher;
use crate::coder::{code_sint2, code_sintn, code_uint, code_uint2, ki, ks, CResult, Coder, Tags, NONE};
use crate::fx::{Map, Set};
use crate::json::{get, py_repr, Str, Val, V};
use crate::num::{self, classify, lg16, ord, parse_dec_exp, parse_numstr, render, unord, Grid, Num, StrFmt};
use std::sync::Arc;

// ------------------------------------------------------------------ signatures
pub const ELEM: u32 = u32::MAX - 1;
pub const WILD: u32 = u32::MAX - 2;

/// portable path component of a signature (for the header)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Comp {
    Elem,
    Wild,
    Key(String),
}

#[derive(Clone)]
pub struct Sigs {
    map: Map<(u32, u32), u32>,
    parent: Vec<(u32, u32)>,
}

impl Sigs {
    pub fn new() -> Sigs {
        Sigs { map: Map::default(), parent: vec![(u32::MAX, 0)] }
    }
    pub const ROOT: u32 = 0;
    pub fn child(&mut self, p: u32, step: u32) -> u32 {
        let next = self.parent.len() as u32;
        let id = *self.map.entry((p, step)).or_insert(next);
        if id == next {
            self.parent.push((p, step));
        }
        id
    }
    pub fn path(&self, mut s: u32, names: &[Box<str>]) -> Vec<Comp> {
        let mut out = Vec::new();
        while s != Self::ROOT {
            let (p, step) = self.parent[s as usize];
            out.push(match step {
                ELEM => Comp::Elem,
                WILD => Comp::Wild,
                k => Comp::Key(names[k as usize].to_string()),
            });
            s = p;
        }
        out.reverse();
        out
    }
    pub fn intern(&mut self, comps: &[Comp], c: &mut Coder) -> u32 {
        let mut s = Self::ROOT;
        for comp in comps {
            let step = match comp {
                Comp::Elem => ELEM,
                Comp::Wild => WILD,
                Comp::Key(k) => c.s.id(k),
            };
            s = self.child(s, step);
        }
        s
    }
}

/// what the encoder learned about the document, shipped in the header
#[derive(Clone, Default, Debug)]
pub struct Schema {
    pub maps: Vec<Vec<Comp>>,
    pub dens: Vec<(Vec<Comp>, Vec<Grid>)>,
    /// string signatures sent to the xz side stream instead of the model
    pub xz: Vec<Vec<Comp>>,
}

/// encoder statistics of a string signature: model cost (1/256 bit), bytes, sample
#[derive(Default, Clone)]
pub struct StrStat {
    pub cost: u64,
    pub bytes: u64,
    pub sample: Vec<u8>,
}

const STR_SAMPLE: usize = 256 << 10;

// ------------------------------------------------------------------ small stack vectors
/// fixed-capacity vector on the stack (per-value scratch without allocation)
#[derive(Clone, Copy)]
pub struct SVec<T: Copy + Default, const N: usize> {
    a: [T; N],
    n: usize,
}

impl<T: Copy + Default, const N: usize> SVec<T, N> {
    fn new() -> Self {
        SVec { a: [T::default(); N], n: 0 }
    }
    fn push(&mut self, x: T) {
        if self.n < N {
            self.a[self.n] = x;
            self.n += 1;
        }
    }
}

impl<T: Copy + Default, const N: usize> std::ops::Deref for SVec<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.a[..self.n]
    }
}

// ------------------------------------------------------------------ coordinates
const MAXD: usize = 7;
const PAD: u32 = u32::MAX;
const M_FIELDS: u32 = u32::MAX - 3;
const M_PEERS: u32 = u32::MAX - 4;
const M_MISS: u32 = u32::MAX - 5;
const M_ALTS: u32 = u32::MAX - 6;

type K = [u32; 8];

#[derive(Clone, Copy)]
pub struct Dims {
    v: [u32; MAXD],
    cs: [u32; MAXD],
    key: [bool; MAXD],
    n: usize,
}

impl Dims {
    pub fn root() -> Dims {
        Dims { v: [PAD; MAXD], cs: [PAD; MAXD], key: [false; MAXD], n: 0 }
    }
    pub fn push(&self, val: u32, container: u32, is_key: bool) -> Dims {
        let mut d = *self;
        if d.n < MAXD {
            d.v[d.n] = val;
            d.cs[d.n] = container;
            d.key[d.n] = is_key;
        }
        d.n += 1;
        d
    }
    fn k(&self, sig: u32) -> Option<K> {
        if self.n > MAXD {
            return None;
        }
        // slots beyond n always hold PAD: copy the whole fixed array
        let v = &self.v;
        Some([sig, v[0], v[1], v[2], v[3], v[4], v[5], v[6]])
    }
    fn with(&self, d: usize, val: u32) -> Dims {
        let mut x = *self;
        x.v[d] = val;
        x
    }
    /// key of the container at dimension d (its signature + coordinates above it)
    fn container(&self, d: usize) -> K {
        let mut k = [PAD; 8];
        k[0] = self.cs[d];
        for i in 0..MAXD {
            if i < d {
                k[1 + i] = self.v[i];
            }
        }
        k
    }
    fn innermost_key(&self) -> Option<usize> {
        (0..self.n.min(MAXD)).rev().find(|&d| self.key[d])
    }
}

// ------------------------------------------------------------------ generational stores
/// Keys whose second component is the record (top-level array index) coordinate.
pub trait RecKey: Eq + std::hash::Hash {
    fn rec(&self) -> u32;
}
impl RecKey for K {
    fn rec(&self) -> u32 {
        self[1]
    }
}
impl RecKey for (K, Num) {
    fn rec(&self) -> u32 {
        self.0[1]
    }
}

const NOREC: u32 = u32::MAX - 7;
/// flush per-value stores after this many stored values (documents without records)
const STORE_CAP: u64 = 1 << 18;

/// A map for per-record state. Predictions only ever look at the current and
/// the previous record, so in records mode entries are kept in two generations
/// and older ones dropped: the working set stays cache-sized instead of growing
/// with the document (output is unchanged). Keys of other shapes live in `perm`.
#[derive(Clone)]
pub struct GMap<Q: RecKey, T> {
    cur: Map<Q, T>,
    prev: Map<Q, T>,
    perm: Map<Q, T>,
    rec: u32,
}

impl<Q: RecKey, T> Default for GMap<Q, T> {
    fn default() -> Self {
        GMap { cur: Map::default(), prev: Map::default(), perm: Map::default(), rec: NOREC }
    }
}

impl<Q: RecKey, T> GMap<Q, T> {
    #[inline]
    fn which(&self, q: &Q) -> u8 {
        let r = q.rec();
        if self.rec != NOREC && r == self.rec {
            0
        } else if self.rec != NOREC && self.rec > 0 && r == self.rec - 1 {
            1
        } else {
            2
        }
    }
    #[inline]
    fn sel(&self, q: &Q) -> &Map<Q, T> {
        match self.which(q) {
            0 => &self.cur,
            1 => &self.prev,
            _ => &self.perm,
        }
    }
    #[inline]
    fn sel_mut(&mut self, q: &Q) -> &mut Map<Q, T> {
        match self.which(q) {
            0 => &mut self.cur,
            1 => &mut self.prev,
            _ => &mut self.perm,
        }
    }
    #[inline]
    pub fn get(&self, q: &Q) -> Option<&T> {
        self.sel(q).get(q)
    }
    pub fn get_mut(&mut self, q: &Q) -> Option<&mut T> {
        self.sel_mut(q).get_mut(q)
    }
    pub fn insert(&mut self, q: Q, t: T) {
        self.sel_mut(&q).insert(q, t);
    }
    pub fn entry(&mut self, q: Q) -> std::collections::hash_map::Entry<'_, Q, T> {
        self.sel_mut(&q).entry(q)
    }
    /// drop everything (bounded memory for documents without records)
    pub fn clear_all(&mut self) {
        self.cur.clear();
        self.prev.clear();
        self.perm.clear();
    }

    pub fn release(&mut self) {
        self.clear_all();
        self.cur.shrink_to_fit();
        self.prev.shrink_to_fit();
        self.perm.shrink_to_fit();
    }

    /// start coding record `rec`: keep it and the previous record, drop older ones
    pub fn advance(&mut self, rec: u32) {
        if rec == self.rec {
            return;
        }
        if self.rec != NOREC && rec == self.rec + 1 {
            std::mem::swap(&mut self.cur, &mut self.prev);
            self.cur.clear();
        } else {
            self.cur.clear();
            self.prev.clear();
        }
        self.rec = rec;
    }
}

// ------------------------------------------------------------------ kinds
pub const K_OBJ: u32 = 0;
pub const K_ARR: u32 = 1;
pub const K_NULL: u32 = 2;
pub const K_TRUE: u32 = 3;
pub const K_FALSE: u32 = 4;
pub const K_INT: u32 = 5;
pub const K_FLOAT: u32 = 6;
pub const K_NUMSTR: u32 = 7;
pub const K_STR: u32 = 8;
pub const K_BIGINT: u32 = 9;
/// a number token kept as literal text (forms the numeric coder can't reproduce)
pub const K_NUMLIT: u32 = 10;

pub fn kind_of(v: &Val) -> u32 {
    match v {
        Val::Obj(_) => K_OBJ,
        Val::Arr(_) => K_ARR,
        Val::Null => K_NULL,
        Val::Bool(true) => K_TRUE,
        Val::Bool(false) => K_FALSE,
        Val::Int(_) | Val::Float(_) => {
            if leaf_num(v).is_some() {
                if matches!(v, Val::Int(_)) { K_INT } else { K_FLOAT }
            } else if matches!(v, Val::Int(_)) {
                K_BIGINT
            } else {
                K_NUMLIT
            }
        }
        Val::Str(s) => {
            if parse_numstr(s).is_some() { K_NUMSTR } else { K_STR }
        }
    }
}

/// Cheap approximation of `kind_of`, used only to pick contexts (hints), where
/// exactness doesn't matter but speed does: no number parsing.
fn kind_hint(v: &Val) -> i64 {
    match v {
        Val::Int(t) if t.len() > 30 => K_BIGINT as i64,
        Val::Float(_) => K_FLOAT as i64,
        Val::Str(s) => match s.as_bytes().first() {
            Some(b'0'..=b'9' | b'-') => K_NUMSTR as i64,
            _ => K_STR as i64,
        },
        _ => kind_of(v) as i64,
    }
}

impl Model {
    /// `kind_of` for the encoder, remembering the parsed number for `num_leaf`
    fn kind_enc(&mut self, v: &V) -> u32 {
        let key = Arc::as_ptr(v) as usize;
        match &**v {
            Val::Int(_) | Val::Float(_) | Val::Str(_) => match leaf_num(v) {
                Some(ln) => {
                    self.sc_parsed.insert(key, ln);
                    match &**v {
                        Val::Int(_) => K_INT,
                        Val::Float(_) => K_FLOAT,
                        _ => K_NUMSTR,
                    }
                }
                None => match &**v {
                    Val::Int(_) => K_BIGINT,
                    Val::Float(_) => K_NUMLIT,
                    _ => K_STR,
                },
            },
            _ => kind_of(v),
        }
    }
}

fn numstr(s: &str) -> Option<(Num, StrFmt, Option<f64>)> {
    let (n, f) = parse_numstr(s)?;
    Some((n, f, (f == StrFmt::Repr).then(|| s.parse().unwrap())))
}

/// numeric value, text format and (for doubles) the double of a numeric leaf
pub fn leaf_num(v: &Val) -> Option<(Num, StrFmt, Option<f64>)> {
    match v {
        Val::Int(t) => {
            // only canonical integer text ("-0" etc. stay literal)
            let n = parse_dec_exp(t)?;
            (render(n, StrFmt::Canon).as_deref() == Some(&**t)).then_some((n, StrFmt::Canon, None))
        }
        // non-integer numbers and numeric strings: value + exact text format
        // float tokens: Python-repr form first (a constant format for Python-written
        // files), then plain decimals in their exact text form
        Val::Float(s) => match s.parse::<f64>() {
            Ok(x) if x.is_finite() && py_repr(x) == **s => Some((classify(x), StrFmt::Repr, Some(x))),
            _ => numstr(s),
        },
        Val::Str(s) => numstr(s),
        _ => None,
    }
}

// ------------------------------------------------------------------ predictor ids
const A_PREV: u32 = 0;
const A_REF: u32 = 1;
const F_FIRST: u32 = 0;
const F_PREV: u32 = 1;
const MAXC: usize = 16;
const VOCAB_CAP: usize = 4096;
const G_DEC: u32 = 0;
const G_ULP: u32 = 1;

/// dense index of a predictor id (for per-group statistics arrays)
fn pidx(p: u32) -> usize {
    let (kind, a) = (p >> 24, p & 0xffff);
    let c = match kind {
        3 | 4 => ((a >> 12) & 7) << 5 | ((a >> 8) & 1) << 4 | ((a >> 4) & 7) << 1 | (a & 1),
        5 | 6 => ((a >> 8) & 7) << 2 | ((a >> 4) & 1) << 1 | (a & 1),
        _ => a & 255,
    };
    (kind as usize) * 256 + c as usize
}
const NPID: usize = 14 * 256;
const NO_ERR: i64 = i64::MIN;
const GROUPS_PER_SIG: usize = 256;
const GROUPS_MAX: usize = 4096;

/// per-group predictor statistics: decayed hit score and running log-error
#[derive(Clone)]
struct PStat {
    score: Box<[f64]>,
    err: Box<[i64]>,
    /// the two best-scoring single-valued predictors (stage 1)
    top: (u32, u32),
    /// recent stage-1 hit rate, 16-bit fixed point
    s1rate: u32,
    /// pruning: evaluations and exact hits per predictor since the last reset
    ev: Box<[u16]>,
    ok: Box<[u16]>,
    nv: u32,
}

const NOPID: u32 = u32::MAX;
/// a predictor never right in this many evaluations is skipped...
const PRUNE_AFTER: u16 = 256;
/// ...until the group's statistics are reset after this many full evaluations
const PRUNE_RESET: u32 = 4096;
/// stage 1 is used while its recent hit rate stays above this (16-bit fixed point)
const S1_MIN: u32 = (0.95 * 65536.0) as u32;

/// predictor kinds that yield exactly one value (usable in stage 1)
fn stage1_ok(p: u32) -> bool {
    matches!(p >> 24, 1 | 3 | 4 | 5 | 6 | 9 | 10 | 12)
}

fn pid(kind: u32, a: u32) -> u32 {
    kind << 24 | a
}

#[derive(Clone)]
struct T {
    nf: u64,
    s1: u64,
    hc: u64,
    hm: u64,
    j: u64,
    grid: u64,
    ne: u64,
    res: u64,
    kind: u64,
    kl: u64,
    kp: u64,
    ni: u64,
    ip: u64,
    vi: u64,
    ll: u64,
    lit: u64,
    sc: u64,
    len: u64,
    alen: u64,
    akind: u64,
    keys: u64,
    strs: u64,
    anc: u64,
    tref: u64,
    split: u64,
}

#[derive(Clone)]
pub struct Model {
    pub c: Coder,
    tags: Tags,
    t: T,
    pub sigs: Sigs,
    maps: Set<u32>,
    dens: Map<u32, Arc<[Grid]>>,
    nums: GMap<K, Num>,
    strs: GMap<K, Str>,
    objs: GMap<K, V>,
    kids: GMap<K, Vec<u32>>,
    fields: GMap<K, Vec<u32>>,
    refcnt: Map<(u32, u32), u64>,
    refbest: Map<u32, (u32, u64)>,
    disc: GMap<K, Vec<Num>>,
    offs: GMap<K, Vec<Num>>,
    dos: GMap<(K, Num), Vec<u32>>,
    peers: GMap<K, Vec<u32>>,
    lastmiss: GMap<K, i64>,
    pgroup: Map<u64, usize>,
    pgroups_per_sig: Map<u32, usize>,
    /// tree-mode splitting (prime pass only) and the children it deferred
    pub split: Option<Split>,
    pub deferred: Vec<Deferred>,
    /// known tuples of numbers per sequence element signature
    tdict: Map<u32, TupleDict>,
    cat_costs: [u64; 4],
    pstats: Vec<PStat>,
    vocab: Map<u32, Vec<Str>>,
    kvocab: Map<u32, Vec<Str>>,
    /// debug statistics (JCOMP_STATS=1): (signature, grid, hit) -> [count, cost]
    pub stats: Option<Map<(u32, u32, bool), [u64; 2]>>,
    /// per-value scratch collections, reused to avoid allocation
    sc_parsed: Map<usize, (Num, StrFmt, Option<f64>)>,
    /// strings of `xz_sigs` signatures: collected (encoder) / consumed (decoder)
    pub xz_sigs: Set<u32>,
    pub xz_out: Vec<Str>,
    pub xz_in: Vec<Str>,
    xz_pos: usize,
    /// per-signature string coding statistics (encoder, when costs are tracked)
    pub str_stats: Option<Map<u32, StrStat>>,
    sc_cands: Vec<(u32, Num)>,
    sc_candj: Vec<u16>,
    sc_rank: Vec<u16>,
    sc_ranked: Vec<(usize, i64, usize)>,
    sc_ordered: Vec<(u32, Num)>,
    alts_cache: Option<(K, u64, SVec<(usize, u32, u32), 16>)>,
    alts_ver: u64,
    nstored: u64,
    sc_keys: Vec<u128>,
    sc_distinct: Vec<(i64, f64, u32, Num, usize)>,
}

impl Model {
    pub fn new(mut c: Coder, schema: &Schema) -> Model {
        let s = &mut c.s;
        let t = T {
            nf: s.c("nf"),
            s1: s.c("s1"),
            hc: s.c("hc"),
            hm: s.c("hm"),
            j: s.c("j"),
            grid: s.c("grid"),
            ne: s.c("ne"),
            res: s.c("res"),
            kind: s.c("kind"),
            kl: s.c("kl"),
            kp: s.c("kp"),
            ni: s.c("ni"),
            ip: s.c("ip"),
            vi: s.c("vi"),
            ll: s.c("ll"),
            lit: s.c("lit"),
            sc: s.c("sc"),
            len: s.c("len"),
            alen: s.c("alen"),
            akind: s.c("akind"),
            keys: s.c("keys"),
            strs: s.c("strs"),
            anc: s.c("anc"),
            tref: s.c("tref"),
            split: s.c("split"),
        };
        let tags = Tags::new(s);
        let mut sigs = Sigs::new();
        let maps = schema.maps.iter().map(|p| sigs.intern(p, &mut c)).collect();
        let dens = schema.dens.iter().map(|(p, g)| (sigs.intern(p, &mut c), Arc::from(&g[..]))).collect();
        let xz_sigs = schema.xz.iter().map(|p| sigs.intern(p, &mut c)).collect();
        Model {
            c,
            tags,
            t,
            sigs,
            maps,
            dens,
            nums: GMap::default(),
            strs: GMap::default(),
            objs: GMap::default(),
            kids: GMap::default(),
            fields: GMap::default(),
            refcnt: Map::default(),
            refbest: Map::default(),
            disc: GMap::default(),
            offs: GMap::default(),
            dos: GMap::default(),
            peers: GMap::default(),
            lastmiss: GMap::default(),
            pgroup: Map::default(),
            pgroups_per_sig: Map::default(),
            split: None,
            deferred: Vec::new(),
            tdict: Map::default(),
            cat_costs: [0; 4],
            pstats: Vec::new(),
            vocab: Map::default(),
            kvocab: Map::default(),
            stats: None,
            sc_parsed: Map::default(),
            xz_sigs,
            xz_out: Vec::new(),
            xz_in: Vec::new(),
            xz_pos: 0,
            str_stats: None,
            sc_cands: Vec::new(),
            sc_candj: Vec::new(),
            sc_rank: Vec::new(),
            sc_ranked: Vec::new(),
            sc_ordered: Vec::new(),
            alts_cache: None,
            alts_ver: 0,
            nstored: 0,
            sc_keys: Vec::new(),
            sc_distinct: Vec::new(),
        }
    }

    /// give a forked model its own side-stream strings
    pub fn reset_xz(&mut self, xz: Vec<Str>) {
        self.xz_in = xz;
        self.xz_pos = 0;
        self.xz_out.clear();
    }

    /// a copy of this trained model with a fresh coder stream
    pub fn fork(&self, data: Option<Vec<u8>>) -> Model {
        let c = self.c.fork(data);
        let mut m = Model { c: Coder::encoder(), ..self.clone() };
        m.c = c;
        // cost accounting restarts with the fresh stream
        m.cat_costs = [0; 4];
        m
    }

    fn enc(&self) -> bool {
        self.c.enc
    }

    // -------------------------------------------------------------- dispatch
    pub fn node(&mut self, kind: u32, v: Option<&V>, sig: u32, dims: &Dims) -> CResult<V> {
        match kind {
            K_OBJ => self.obj(v, sig, dims),
            K_ARR => self.arr(v, sig, dims),
            K_NULL => Ok(Arc::new(Val::Null)),
            K_TRUE => Ok(Arc::new(Val::Bool(true))),
            K_FALSE => Ok(Arc::new(Val::Bool(false))),
            K_INT | K_FLOAT | K_NUMSTR => {
                let c0 = self.c.cost_total();
                let r = self.num_leaf(kind, v, sig, dims);
                self.cat_cost(3, c0);
                r
            }
            K_STR => {
                let c0 = self.c.cost_total();
                let r = self.str_leaf(v, sig, dims);
                self.cat_cost(2, c0);
                r
            }
            K_BIGINT | K_NUMLIT => {
                let ctx = self.c.t(&[self.t.lit, ki(sig as i64), ki(kind as i64)]);
                let t = v.map(|v| match &**v {
                    Val::Int(t) | Val::Float(t) => t.to_string(),
                    _ => unreachable!(),
                });
                let s = self.literal(ctx, t.as_deref())?;
                if s.is_empty() {
                    return Err("corrupt stream (number literal)".into());
                }
                Ok(Arc::new(if kind == K_BIGINT { Val::Int(s.into()) } else { Val::Float(s.into()) }))
            }
            _ => Err("corrupt stream (kind)".into()),
        }
    }

    /// previous coordinate along each dimension (and the most common sibling key)
    fn dim_alts(&self, dims: &Dims) -> SVec<(usize, u32, u32), 16> {
        let mut out = SVec::new();
        for d in 0..dims.n.min(MAXD) {
            let v = dims.v[d];
            if !dims.key[d] {
                if v > 0 {
                    out.push((d, A_PREV, v - 1));
                }
                continue;
            }
            if let Some(l) = self.kids.get(&dims.container(d)) {
                if let Some(&p) = l.iter().rev().find(|&&x| x != v) {
                    out.push((d, A_PREV, p));
                }
            }
            if let Some(&(r, _)) = self.refbest.get(&dims.cs[d]) {
                if r != v && !out.iter().any(|&(dd, _, x)| dd == d && x == r) {
                    out.push((d, A_REF, r));
                }
            }
        }
        out
    }

    fn fkey(dims: &Dims, marker: u32) -> Option<K> {
        dims.k(marker)
    }

    #[inline]
    fn get(&self, sig: u32, dims: &Dims) -> Option<Num> {
        self.nums.get(&dims.k(sig)?).copied()
    }

    /// `dim_alts`, cached across the leaves of one object (it only changes when a
    /// sibling key finishes or a most-common key changes: both bump `alts_ver`)
    fn cached_alts(&mut self, dims: &Dims) -> SVec<(usize, u32, u32), 16> {
        let Some(k) = dims.k(M_ALTS) else { return self.dim_alts(dims) };
        if let Some((ck, v, a)) = &self.alts_cache {
            if *ck == k && *v == self.alts_ver {
                return *a;
            }
        }
        let a = self.dim_alts(dims);
        self.alts_cache = Some((k, self.alts_ver, a));
        a
    }

    fn candidates(&self, sig: u32, dims: &Dims, alts: &SVec<(usize, u32, u32), 16>, ps: usize, out: &mut Vec<(u32, Num)>) {
        out.clear();
        if dims.n > MAXD {
            return;
        }
        // predictors that have not been right once in this group lately are skipped
        let st = &self.pstats[ps];
        let dead = |p: u32| {
            let i = pidx(p);
            st.ev[i] >= PRUNE_AFTER && st.ok[i] == 0
        };
        let fl: &[u32] = Self::fkey(dims, M_FIELDS).and_then(|k| self.fields.get(&k)).map(|v| &v[..]).unwrap_or(&[]);
        let first = fl.first().copied();
        let mut falts: SVec<(u32, u32), 2> = SVec::new();
        if let Some(f) = first {
            falts.push((F_FIRST, f));
        }
        if let Some(&p) = fl.last() {
            if Some(p) != first {
                falts.push((F_PREV, p));
            }
        }
        // copies along one coordinate (values cached for the combinations below)
        let mut av: SVec<Option<Num>, 16> = SVec::new();
        for &(d, _, val) in alts.iter() {
            av.push(self.get(sig, &dims.with(d, val)));
        }
        let mut fv: SVec<Option<Num>, 2> = SVec::new();
        for &(_, fs) in falts.iter() {
            fv.push(self.get(fs, dims));
        }
        for (&(d, t, _), x) in alts.iter().zip(av.iter()) {
            if let Some(x) = *x {
                let p = pid(1, (d as u32) << 4 | t);
                if !dead(p) {
                    out.push((p, x));
                }
            }
        }

        // parallelograms over two coordinates
        for a in 0..alts.len() {
            let (d1, t1, v1) = alts[a];
            let da = dims.with(d1, v1);
            let Some(x1) = av[a] else { continue };
            for (b, &(d2, t2, v2)) in alts.iter().enumerate().skip(a + 1) {
                if d2 == d1 {
                    continue;
                }
                let id = (d1 as u32) << 12 | t1 << 8 | (d2 as u32) << 4 | t2;
                let (ka, km) = (!dead(pid(3, id)), !dead(pid(4, id)));
                if !(ka || km) {
                    continue;
                }
                if let (Some(x2), Some(x12)) = (av[b], self.get(sig, &da.with(d2, v2))) {
                    if let Some(p) = ka.then(|| num::para_add(x1, x2, x12)).flatten() {
                        out.push((pid(3, id), p));
                    }
                    if let Some(p) = km.then(|| num::para_mul(x1, x2, x12)).flatten() {
                        out.push((pid(4, id), p));
                    }
                }
            }
            for (f, &(ft, fs)) in falts.iter().enumerate() {
                let id = (d1 as u32) << 8 | t1 << 4 | ft;
                let (ka, km) = (!dead(pid(5, id)), !dead(pid(6, id)));
                if !(ka || km) {
                    continue;
                }
                if let (Some(x2), Some(x12)) = (fv[f], self.get(fs, &da)) {
                    if let Some(p) = ka.then(|| num::para_add(x1, x2, x12)).flatten() {
                        out.push((pid(5, id), p));
                    }
                    if let Some(p) = km.then(|| num::para_mul(x1, x2, x12)).flatten() {
                        out.push((pid(6, id), p));
                    }
                }
            }
        }
        // differences to the first field shared between sibling map entries
        if let (Some(m), Some(f0)) = (dims.innermost_key(), first) {
            if f0 != sig {
                if let Some(od) = self.get(f0, dims) {
                    if let Some(pe) = Self::fkey(dims, M_PEERS).and_then(|k| self.peers.get(&k)).filter(|_| !dead(pid(9, 0))) {
                        for &o2 in pe.iter().take(2) {
                            let d2 = dims.with(m, o2);
                            if let (Some(x), Some(y)) = (self.get(f0, &d2), self.get(sig, &d2)) {
                                if let Some(p) = num::add(x, y, true).and_then(|dd| num::add(od, dd, true)) {
                                    out.push((pid(9, 0), p));
                                }
                            }
                        }
                    }
                    let bases: &[Num] = dims.with(m, PAD).k(sig).and_then(|k| self.disc.get(&k)).map(|v| &v[..v.len().min(4)]).unwrap_or(&[]);
                    for (k, &dsc) in bases.iter().enumerate() {
                        if dead(pid(10, k as u32)) {
                            continue;
                        }
                        if let Some(p) = num::add(od, dsc, true) {
                            out.push((pid(10, k as u32), p));
                        }
                    }
                    if let Some(offs) = self.offs_key(dims, m, sig).and_then(|k| self.offs.get(&k)) {
                        for (k, &off) in offs.iter().take(4).enumerate() {
                            if off != Num::D(0, 0) && !dead(pid(11, k as u32)) {
                                for &dsc in bases.iter().take(3) {
                                    if let Some(p) = num::add(od, dsc, true).and_then(|x| num::add(x, off, true)) {
                                        out.push((pid(11, k as u32), p));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if fl.len() >= 2 && !dead(pid(12, 0)) {
            if let (Some(a), Some(b)) = (self.get(fl[fl.len() - 1], dims), self.get(fl[fl.len() - 2], dims)) {
                if let Some(p) = num::mid(a, b) {
                    out.push((pid(12, 0), p));
                }
            }
        }
    }

    /// The value of one (single-valued) predictor: the same arithmetic as in
    /// `candidates`, without computing the others.
    fn eval_pid(&self, p: u32, sig: u32, dims: &Dims, alts: &SVec<(usize, u32, u32), 16>) -> Option<Num> {
        if dims.n > MAXD {
            return None;
        }
        let (kind, a) = (p >> 24, p & 0xff_ffff);
        let alt = |d: usize, t: u32| alts.iter().find(|x| x.0 == d && x.1 == t).map(|x| x.2);
        let fl: &[u32] = Self::fkey(dims, M_FIELDS).and_then(|k| self.fields.get(&k)).map(|v| &v[..]).unwrap_or(&[]);
        let first = fl.first().copied();
        let falt = |ft: u32| -> Option<u32> {
            match ft {
                F_FIRST => first,
                _ => fl.last().copied().filter(|&p| Some(p) != first),
            }
        };
        match kind {
            1 => {
                let d = (a >> 4) as usize;
                self.get(sig, &dims.with(d, alt(d, a & 15)?))
            }
            3 | 4 => {
                let (d1, t1, d2, t2) = (((a >> 12) & 15) as usize, (a >> 8) & 15, ((a >> 4) & 15) as usize, a & 15);
                let (v1, v2) = (alt(d1, t1)?, alt(d2, t2)?);
                let da = dims.with(d1, v1);
                let (x1, x2, x12) = (self.get(sig, &da)?, self.get(sig, &dims.with(d2, v2))?, self.get(sig, &da.with(d2, v2))?);
                if kind == 3 { num::para_add(x1, x2, x12) } else { num::para_mul(x1, x2, x12) }
            }
            5 | 6 => {
                let (d1, t1, ft) = (((a >> 8) & 15) as usize, (a >> 4) & 15, a & 15);
                let (v1, fs) = (alt(d1, t1)?, falt(ft)?);
                let da = dims.with(d1, v1);
                let (x1, x2, x12) = (self.get(sig, &da)?, self.get(fs, dims)?, self.get(fs, &da)?);
                if kind == 5 { num::para_add(x1, x2, x12) } else { num::para_mul(x1, x2, x12) }
            }
            9 | 10 => {
                let (m, f0) = (dims.innermost_key()?, first?);
                if f0 == sig {
                    return None;
                }
                let od = self.get(f0, dims)?;
                if kind == 9 {
                    let pe = Self::fkey(dims, M_PEERS).and_then(|k| self.peers.get(&k))?;
                    pe.iter().take(2).find_map(|&o2| {
                        let d2 = dims.with(m, o2);
                        let (x, y) = (self.get(f0, &d2)?, self.get(sig, &d2)?);
                        num::add(x, y, true).and_then(|dd| num::add(od, dd, true))
                    })
                } else {
                    let bases = dims.with(m, PAD).k(sig).and_then(|k| self.disc.get(&k))?;
                    let dsc = *bases.get(a as usize).filter(|_| a < 4)?;
                    num::add(od, dsc, true)
                }
            }
            12 if fl.len() >= 2 => num::mid(self.get(fl[fl.len() - 1], dims)?, self.get(fl[fl.len() - 2], dims)?),
            _ => None,
        }
    }

    /// offsets are shared across the map level just outside the innermost one
    fn offs_key(&self, dims: &Dims, m: usize, sig: u32) -> Option<K> {
        let outer = (0..m).rev().find(|&d| dims.key[d])?;
        dims.with(outer, PAD).k(sig)
    }

    /// index of group g's predictor statistics
    fn pstat(&mut self, g: u64) -> usize {
        if let Some(&i) = self.pgroup.get(&g) {
            return i;
        }
        self.pstats.push(PStat { score: vec![0.0; NPID].into(), err: vec![NO_ERR; NPID].into(), top: (NOPID, NOPID), s1rate: S1_MIN, ev: vec![0; NPID].into(), ok: vec![0; NPID].into(), nv: 0 });
        self.pgroup.insert(g, self.pstats.len() - 1);
        self.pstats.len() - 1
    }

    /// predictor statistics for a number at (sig, innermost map key). Keys that
    /// never repeat (ids) would each get a group, so past a per-signature and a
    /// total cap new keys share the signature's keyless group.
    fn num_pstat(&mut self, sig: u32, dims: &Dims) -> usize {
        let g = self.group(sig, dims);
        if let Some(&i) = self.pgroup.get(&g) {
            return i;
        }
        if dims.innermost_key().is_some() {
            let n = self.pgroups_per_sig.entry(sig).or_insert(0);
            if *n >= GROUPS_PER_SIG || self.pstats.len() >= GROUPS_MAX {
                let g0 = self.c.t(&[ki(sig as i64), NONE]);
                return self.pstat(g0);
            }
            *n += 1;
        }
        self.pstat(g)
    }

    /// debug: bits by category (0 structure, 1 keys, 2 strings, 3 numbers)
    fn cat_cost(&mut self, cat: usize, c0: Option<u64>) {
        if let (Some(c0), Some(c1)) = (c0, self.c.cost_total()) {
            self.cat_costs[cat] += c1 - c0;
        }
    }

    pub fn cat_report(&self) -> [u64; 4] {
        let mut r = self.cat_costs;
        r[0] = self.c.cost_total().unwrap_or(0) - r[1] - r[2] - r[3];
        r.map(|x| x / 2048)
    }

    pub fn ngroups(&self) -> usize {
        self.pstats.len()
    }

    fn group(&mut self, sig: u32, dims: &Dims) -> u64 {
        let kc = match dims.innermost_key() {
            Some(d) => ks(dims.v[d]),
            None => NONE,
        };
        self.c.t(&[ki(sig as i64), kc])
    }

    // -------------------------------------------------------------- numbers
    fn num_leaf(&mut self, kind: u32, v: Option<&V>, sig: u32, dims: &Dims) -> CResult<V> {
        let c0 = self.c.cost_total();
        let r = self.num_leaf_(kind, v, sig, dims);
        if let (Some(st), Some(c0), Ok((_, gi, hit))) = (&mut self.stats, c0, &r) {
            let e = st.entry((sig, *gi, *hit)).or_insert([0; 2]);
            e[0] += 1;
            e[1] += self.c.cost_total().unwrap() - c0;
        }
        r.map(|x| x.0)
    }

    fn num_leaf_(&mut self, kind: u32, v: Option<&V>, sig: u32, dims: &Dims) -> CResult<(V, u32, bool)> {
        let enc = self.enc();
        let g = self.group(sig, dims);
        let ps = self.num_pstat(sig, dims);
        let (mut n, mut sf, xin) = match v {
            Some(v) => match self.sc_parsed.remove(&(Arc::as_ptr(v) as usize)) {
                Some(ln) => ln,
                None => leaf_num(v).ok_or("not numeric")?,
            },
            None => (Num::D(0, 0), StrFmt::Canon, None),
        };
        if kind == K_NUMSTR || kind == K_FLOAT {
            // text format: per group, mixed with a shared context so new groups
            // inherit the document's usual format
            let t = self.c.t(&[g, self.t.nf]);
            let tc = self.c.t(&[self.t.nf, ki(kind as i64)]);
            sf = StrFmt::from_code(code_uint2(&mut self.c, &self.tags, t, tc, sf.code() as u128)?.min(64) as u32);
        }
        // doubles (JSON floats, repr strings) are reproduced bit-exactly
        let xkind = sf == StrFmt::Repr;
        // grid: decimal, ulp, or one of this signature's rational grids
        let grids: Arc<[Grid]> = self.dens.get(&sig).cloned().unwrap_or_else(|| Arc::from(&[][..]));
        let coarse = |n: Num, gr: &Grid| !gr.scaled && gr.prec.is_some() && matches!(n, Num::D(_, e) if num::pow10(e.max(0) as u32).is_some_and(|p| p > gr.den)) && gr.holds(n).is_some();
        let mut gi = 0u32;
        let mut j = 0i64;
        if enc {
            let dec_grid = |n: Num| grids.iter().position(|gr| coarse(n, gr)).map(|i| i as u32 + 2).unwrap_or(G_DEC);
            match xin {
                None => gi = dec_grid(n),
                Some(x) => {
                    let f = Num::F(x.to_bits());
                    if let Some(i) = grids.iter().position(|gr| !gr.scaled && gr.prec.is_none() && gr.holds(f).is_some()) {
                        (gi, n) = (i as u32 + 2, f);
                    } else if let Some((d, jj)) = num::near(x) {
                        (n, j) = (d, jj);
                        gi = if jj == 0 { dec_grid(d) } else { G_DEC };
                    } else if let Some((i, k, jj)) = grids.iter().enumerate().find_map(|(i, gr)| if gr.scaled { gr.holds_near(x).map(|(k, jj)| (i, k, jj)) } else { None }) {
                        // a short decimal divided by a constant, plus float noise
                        (gi, n, j) = (i as u32 + 2, grids[i].value(k).unwrap(), jj);
                    } else {
                        (gi, n) = (G_ULP, f);
                    }
                }
            }
        }
        if kind != K_INT {
            let t = self.c.t(&[g, self.t.grid]);
            let tc = self.c.t(&[self.t.grid, ki(kind as i64)]);
            gi = code_uint2(&mut self.c, &self.tags, t, tc, gi as u128)? as u32;
        }
        if gi as usize >= grids.len() + 2 {
            return Err("corrupt stream (grid)".into());
        }
        let grid = (gi >= 2).then(|| grids[gi as usize - 2]);
        let alts = self.cached_alts(dims);
        let lm = Self::fkey(dims, M_MISS).and_then(|k| self.lastmiss.get(&k)).copied().unwrap_or(2);
        let snapg = |x: Num| -> Option<Num> {
            match grid {
                Some(gr) => gr.snap(x),
                None => Some(x),
            }
        };
        // stage 1: only the group's best predictor (and whether the runner-up agrees);
        // most values are settled here without computing every candidate
        let mut hit: Option<u32> = None;
        let mut rejected: Option<Num> = None;
        let mut s1_probe: Option<Num> = None;
        let (b1, b2) = self.pstats[ps].top;
        let c1 = (b1 != NOPID).then(|| self.eval_pid(b1, sig, dims, &alts)).flatten().and_then(snapg);
        let c2 = c1.and_then(|_| (b2 != NOPID).then(|| self.eval_pid(b2, sig, dims, &alts)).flatten().and_then(snapg));
        // only when the two best predictors agree: otherwise the full ordering
        // (with every predictor's vote) codes the value more cheaply
        // ...and only in groups where the shortcut has (recently) almost always been right
        let rate_ok = self.pstats[ps].s1rate >= S1_MIN;
        if let (Some(x1), true) = (c1, c1.is_some() && c1 == c2) {
            // the shortcut's outcome is tracked even when not used (from stage 2's result)
            if rate_ok {
                let t = self.c.t(&[g, self.t.s1, ki(b1 as i64), ki(lm)]);
                let tc = self.c.t(&[self.t.hc, self.t.s1, ki(b1 as i64), ki(lm)]);
                let tm = self.c.t(&[self.t.hm, self.t.s1, ki(sig as i64), ki(b1 as i64)]);
                if self.c.bitn(&[t, tc, tm], 1, enc && x1 == n) {
                    hit = Some(b1);
                    n = x1;
                } else {
                    rejected = Some(x1);
                }
            }
            s1_probe = Some(x1);
        }
        // stage 2: every candidate (minus the rejected value), ordered by agreement
        let mut cands = std::mem::take(&mut self.sc_cands);
        cands.clear();
        let mut distinct = std::mem::take(&mut self.sc_distinct);
        distinct.clear();
        if hit.is_none() {
            self.candidates(sig, dims, &alts, ps, &mut cands);
            cands.retain_mut(|(_, x)| match snapg(*x) {
                Some(s) if Some(s) != rejected => {
                    *x = s;
                    true
                }
                _ => false,
            });
            // distinct candidate values, each with how many predictors agree on it and
            // its best-scoring predictor
            let mut candj = std::mem::take(&mut self.sc_candj);
            candj.clear();
            for (i, &(p, x)) in cands.iter().enumerate() {
                let sc = self.pstats[ps].score[pidx(p)];
                // few distinct values per leaf: a linear scan beats hashing
                match distinct.iter().position(|d| d.3 == x) {
                    Some(j) => {
                        let e = &mut distinct[j];
                        e.0 += 1;
                        if sc > e.1 {
                            (e.1, e.2, e.4) = (sc, p, i);
                        }
                        candj.push(j as u16);
                    }
                    None => {
                        candj.push(distinct.len() as u16);
                        distinct.push((1, sc, p, x, i));
                    }
                }
            }
            self.sc_candj = candj;
            // one integer key per value: agreement desc, score desc (scores are >= 0, so
            // their bits order like the values), then candidate position
            let mut keys = std::mem::take(&mut self.sc_keys);
            keys.clear();
            keys.extend(distinct.iter().enumerate().map(|(j, d)| {
                ((u32::MAX - d.0 as u32) as u128) << 96 | ((u64::MAX - d.1.to_bits()) as u128) << 32 | (d.4 as u128) << 16 | j as u128
            }));
            keys.sort_unstable();
            let order: SVec<u16, 128> = {
                let mut o = SVec::new();
                for k in keys.iter().take(MAXC) {
                    o.push((k & 0xffff) as u16);
                }
                o
            };
            self.sc_keys = keys;
            for (pos, &j) in order.iter().enumerate() {
                let (agree, _, p, x, _) = distinct[j as usize];
                let t = self.c.t(&[g, ki(p as i64), ki(agree.min(4)), ki((pos as i64).min(2)), ki(lm)]);
                let tc = self.c.t(&[self.t.hc, ki(p as i64), ki(agree.min(4)), ki((pos as i64).min(2)), ki(lm)]);
                let tm = self.c.t(&[self.t.hm, ki(sig as i64), ki(p as i64), ki(agree.min(4)), ki((pos as i64).min(2))]);
                if self.c.bitn(&[t, tc, tm], 0, enc && x == n) {
                    hit = Some(p);
                    n = x;
                    break;
                }
            }
        }
        if let Some(p) = hit {
            let st = &mut self.pstats[ps];
            let e = &mut st.score[pidx(p)];
            *e = *e * 0.98 + 1.0;
            let sc = *e;
            // keep the group's two best single-valued predictors (scores only grow)
            if stage1_ok(p) {
                let (t1, t2) = st.top;
                if p == t1 {
                } else if t1 == NOPID || sc > st.score[pidx(t1)] {
                    st.top = (p, t1);
                } else if p != t2 && (t2 == NOPID || sc > st.score[pidx(t2)]) {
                    st.top.1 = p;
                }
            }
        } else {
            // candidates in rank order (value's agreement rank, then predictor score)
            // (no hashing or allocation: ranks by distinct index, reused buffers)
            let mut rank_of = std::mem::take(&mut self.sc_rank);
            rank_of.clear();
            rank_of.resize(distinct.len(), 0);
            for (r, k) in self.sc_keys.iter().enumerate() {
                rank_of[(k & 0xffff) as usize] = r as u16;
            }
            let mut ranked = std::mem::take(&mut self.sc_ranked);
            ranked.clear();
            let st = &self.pstats[ps];
            ranked.extend(cands.iter().enumerate().map(|(i, c)| (rank_of[self.sc_candj[i] as usize] as usize, -((st.score[pidx(c.0)] * 1e9) as i64), i)));
            ranked.sort_unstable();
            let mut ordered = std::mem::take(&mut self.sc_ordered);
            ordered.clear();
            ordered.extend(ranked.iter().map(|r| cands[r.2]));
            let res = self.residual(g, sig, ps, gi, grid, n, &ordered);
            self.sc_rank = rank_of;
            self.sc_ranked = ranked;
            self.sc_ordered = ordered;
            n = res?;
        }
        if !cands.is_empty() || hit.is_none() {
            // which predictors were exactly right (pruning statistics)
            let st = &mut self.pstats[ps];
            for &(p, x) in cands.iter() {
                let i = pidx(p);
                st.ev[i] = st.ev[i].saturating_add(1);
                st.ok[i] = st.ok[i].saturating_add((x == n) as u16);
            }
            st.nv += 1;
            if st.nv >= PRUNE_RESET {
                // give pruned predictors another chance now and then
                st.nv = 0;
                st.ev.fill(0);
                st.ok.fill(0);
            }
        }
        if let Some(x1) = s1_probe {
            // fixed-point hit rate, decay 1/32 (deterministic integer arithmetic)
            let r = &mut self.pstats[ps].s1rate;
            *r = *r - (*r >> 5) + if x1 == n { 1 << (16 - 5) } else { 0 };
        }
        self.sc_cands = cands;
        self.sc_distinct = distinct;
        if kind == K_INT && !matches!(n, Num::D(_, e) if e <= 0) {
            return Err("corrupt stream (int)".into());
        }
        // a double on the decimal grid: the decimal, plus float noise in ULPs
        let mut x = 0.0;
        if xkind {
            x = n.f64();
            if gi == G_DEC || grid.is_some_and(|gr| gr.scaled) {
                let t = self.c.t(&[g, self.t.j]);
                let tc = self.c.t(&[self.t.j, ki(gi.min(2) as i64)]);
                let jj = code_sint2(&mut self.c, &self.tags, t, tc, j as i128)?;
                if jj != 0 {
                    x = f64::from_bits(unord(ord(x.to_bits()) + jj).ok_or("corrupt stream (ulp)")?);
                }
            }
        }
        self.after_num(sig, dims, n, hit.is_none());
        // text (the encoder already has it)
        let hit = hit.is_some();
        if let Some(v) = v {
            return Ok((v.clone(), gi, hit));
        }
        Ok((Arc::new(match kind {
            K_INT => match n {
                Num::D(m, e) => Val::Int(m.checked_mul(num::pow10((-e) as u32).ok_or("int")?).ok_or("int")?.to_string().into()),
                _ => unreachable!(),
            },
            _ => {
                let t = if sf == StrFmt::Repr { py_repr(x) } else { render(n, sf).ok_or("corrupt stream (number)")? };
                if kind == K_FLOAT { Val::Float(t.into()) } else { Val::Str(t.into()) }
            }
        }), gi, hit))
    }

    /// human-readable statistics report (debug)
    pub fn stats_report(&self) -> Vec<(u64, String)> {
        let Some(st) = &self.stats else { return Vec::new() };
        let mut out = Vec::new();
        for (&(sig, gi, hit), &[n, cost]) in st {
            let path: Vec<String> = self.sigs.path(sig, &self.c.s.names).iter().map(|c| match c {
                Comp::Elem => "[]".into(),
                Comp::Wild => "*".into(),
                Comp::Key(k) => k.clone(),
            }).collect();
            let g = match gi { 0 => "dec".to_string(), 1 => "ulp".into(), k => format!("rat{}", k - 2) };
            out.push((cost / 256 / 8, format!("{:>8} B {:>9} vals {:>6.2} b/val  {:4} {}  {}", cost / 2048, n, cost as f64 / 256.0 / n as f64, g, if hit { "hit " } else { "MISS" }, path.join("/"))));
        }
        out
    }

    /// code a value that no candidate predicted: residual against the
    /// candidate whose predictor has had the lowest recent error
    #[allow(clippy::too_many_arguments)]
    fn residual(&mut self, g: u64, sig: u32, ps: usize, gi: u32, grid: Option<Grid>, n: Num, cands: &[(u32, Num)]) -> CResult<Num> {
        let enc = self.enc();
        let mut best: Option<Num> = None;
        let mut bp = NOPID;
        let mut eb: i64 = 1024;
        for &(p, x) in cands {
            let e = match self.pstats[ps].err[pidx(p)] {
                NO_ERR => 2047,
                e => e,
            };
            if best.is_none() || e < eb {
                best = Some(x);
                bp = p;
                eb = e;
            }
        }
        if bp != NOPID {
            // the residual base is useful even when never exactly right: keep it alive
            let o = &mut self.pstats[ps].ok[pidx(bp)];
            *o = o.saturating_add(1);
        }
        let base = best.unwrap_or(Num::D(0, 0));
        let have = best.is_some();
        let ectx = |step16: i64| if have { ((eb - step16).div_euclid(16)).clamp(0, 63) } else { -1 };
        let out = if let Some(gr) = grid {
            let pred = gr.index(base).unwrap_or(0);
            let ec = ectx(-lg16(gr.den as f64));
            let t = self.c.t(&[g, self.t.res, ki(2), ki(ec)]);
            let tc = self.c.t(&[self.t.res, ki(2), ki(ec)]);
            let tm = self.c.t(&[self.t.hm, self.t.res, ki(sig as i64), ki(ec)]);
            let k = if enc { gr.holds(n).unwrap() } else { 0 };
            let r = code_sintn(&mut self.c, &self.tags, &[t, tc, tm], k - pred)?;
            gr.value(pred + r).ok_or("corrupt stream (rational)")?
        } else if gi == G_ULP {
            let bf = base.f64();
            let pred = ord(bf.to_bits());
            let ec = ectx(lg16(bf) - 52 * 16);
            let t = self.c.t(&[g, self.t.res, ki(1), ki(ec)]);
            let tc = self.c.t(&[self.t.res, ki(1), ki(ec)]);
            let tm = self.c.t(&[self.t.hm, self.t.res, ki(sig as i64), ki(ec)]);
            let r = code_sintn(&mut self.c, &self.tags, &[t, tc, tm], if enc { ord(match n { Num::F(b) => b, _ => unreachable!() }) - pred } else { 0 })?;
            Num::F(unord(pred + r).ok_or("corrupt stream (ulp)")?)
        } else {
            let be = match base {
                Num::D(_, e) if have => e.clamp(-20, 20) as i64,
                _ => 99,
            };
            let t = self.c.t(&[g, self.t.ne, ki(be)]);
            let tc = self.c.t(&[self.t.ne, ki(be)]);
            let e = code_sint2(&mut self.c, &self.tags, t, tc, if let Num::D(_, e) = n { e as i128 } else { 0 })?;
            let e = i32::try_from(e).map_err(|_| "corrupt stream (exponent)")?;
            if e.abs() > 40 {
                return Err("corrupt stream (exponent)".into());
            }
            let pred = num::scale_round(base, e).unwrap_or(0);
            let ec = ectx(-(e as i64) * 53);
            let t = self.c.t(&[g, self.t.res, ki(0), ki(ec)]);
            let tc = self.c.t(&[self.t.res, ki(0), ki(ec)]);
            let tm = self.c.t(&[self.t.hm, self.t.res, ki(sig as i64), ki(ec)]);
            let m = if let Num::D(m, _) = n { m } else { 0 };
            let r = code_sintn(&mut self.c, &self.tags, &[t, tc, tm], m - pred)?;
            num::norm(pred + r, e)
        };
        // predictor error statistics (misses only: that is when a base is needed)
        for &(p, x) in cands {
            let e = num::add(x, out, true).map(|d| lg16(d.f64())).unwrap_or(2047);
            let v = &mut self.pstats[ps].err[pidx(p)];
            *v = if *v == NO_ERR { e } else { (*v * 15).div_euclid(16) + e.div_euclid(16) };
        }
        Ok(out)
    }

    fn after_num(&mut self, sig: u32, dims: &Dims, n: Num, miss: bool) {
        let Some(k) = dims.k(sig) else { return };
        self.nums.insert(k, n);
        self.stored();
        for d in 0..dims.n.min(MAXD) {
            if dims.key[d] {
                let c = self.refcnt.entry((dims.cs[d], dims.v[d])).or_insert(0);
                *c += 1;
                let c = *c;
                let b = self.refbest.entry(dims.cs[d]).or_insert((dims.v[d], 0));
                if c > b.1 {
                    if b.0 != dims.v[d] {
                        self.alts_ver += 1;
                    }
                    *b = (dims.v[d], c);
                }
            }
        }
        let fk = Self::fkey(dims, M_FIELDS).unwrap();
        let first = self.fields.get(&fk).and_then(|l| l.first().copied());
        self.fields.entry(fk).or_default().push(sig);
        self.lastmiss.insert(Self::fkey(dims, M_MISS).unwrap(), miss as i64);
        let (Some(m), Some(f0)) = (dims.innermost_key(), first) else { return };
        if f0 == sig {
            return;
        }
        let Some(od) = self.get(f0, dims) else { return };
        let Some(dsc) = num::add(od, n, true) else { return };
        let Some(dk) = dims.with(m, PAD).k(sig) else { return };
        let okey = self.offs_key(dims, m, sig);
        let lst = self.disc.entry(dk).or_default();
        if let Some(ok) = okey {
            let offs = self.offs.entry(ok).or_default();
            offs.clear();
            offs.extend(lst.iter().filter_map(|&b| num::add(dsc, b, true)));
        }
        let pk = Self::fkey(dims, M_PEERS).unwrap();
        match self.dos.get_mut(&(dk, dsc)) {
            Some(pl) => {
                self.peers.insert(pk, pl.iter().rev().copied().collect());
                pl.push(dims.v[m]);
            }
            None => {
                self.dos.insert((dk, dsc), vec![dims.v[m]]);
            }
        }
        let lst = self.disc.get_mut(&dk).unwrap();
        if let Some(p) = lst.iter().position(|&x| x == dsc) {
            lst.remove(p);
        }
        lst.insert(0, dsc);
    }

    // -------------------------------------------------------------- strings
    fn str_leaf(&mut self, v: Option<&V>, sig: u32, dims: &Dims) -> CResult<V> {
        if self.xz_sigs.contains(&sig) {
            // text better left to xz (side stream in part A)
            let s: Str = match v {
                Some(v) => {
                    let s: Str = v.as_str().unwrap().into();
                    self.xz_out.push(s.clone());
                    s
                }
                None => {
                    let s = self.xz_in.get(self.xz_pos).cloned().ok_or("corrupt stream (xz strings)")?;
                    self.xz_pos += 1;
                    s
                }
            };
            if let Some(k) = dims.k(sig) {
                self.strs.insert(k, s.clone());
                self.stored();
            }
            return Ok(Arc::new(Val::Str(s)));
        }
        let c0 = if self.str_stats.is_some() { self.c.cost_total() } else { None };
        let r = self.str_leaf_(v, sig, dims);
        if let (Some(c0), Some(st), Some(v)) = (c0, &mut self.str_stats, v) {
            let e = st.entry(sig).or_default();
            e.cost += self.c.cost_total().unwrap() - c0;
            let t = v.as_str().unwrap();
            e.bytes += t.len() as u64 + 3;
            if e.sample.len() < STR_SAMPLE {
                crate::json::write_str(&mut e.sample, t, false);
                e.sample.push(b',');
            }
        }
        r
    }

    fn str_leaf_(&mut self, v: Option<&V>, sig: u32, dims: &Dims) -> CResult<V> {
        let g = self.group(sig, dims);
        let cur = v.and_then(|v| v.as_str());
        let mut cands: Vec<Str> = Vec::new();
        for (d, _, val) in self.dim_alts(dims).iter().copied() {
            if let Some(s) = dims.with(d, val).k(sig).and_then(|k| self.strs.get(&k)) {
                if !cands.contains(s) {
                    cands.push(s.clone());
                }
            }
        }
        let mut out: Option<Str> = None;
        for (k, cd) in cands.iter().take(3).enumerate() {
            let t = self.c.t(&[g, self.t.sc, ki(k as i64)]);
            if self.c.bit(t, cur == Some(&**cd)) {
                out = Some(cd.clone());
                break;
            }
        }
        let out = match out {
            Some(s) => s,
            None => {
                let ctx = self.c.t(&[self.t.strs, ki(sig as i64)]);
                self.text_code(ctx, cur, sig, false, dims)?
            }
        };
        if let Some(k) = dims.k(sig) {
            self.strs.insert(k, out.clone());
            self.stored();
        }
        Ok(Arc::new(Val::Str(out)))
    }

    /// a string or key: either an enclosing map key plus a remainder (ids are
    /// often built from their parents' keys: "sku", "sku.offer", ...), or
    /// coded on its own with `vocab_code`
    fn text_code(&mut self, ctx: u64, s: Option<&str>, vkey: u32, keys: bool, dims: &Dims) -> CResult<Str> {
        let mut anc: SVec<u32, 3> = SVec::new();
        for d in (0..dims.n.min(MAXD)).rev() {
            if dims.key[d] && !anc.contains(&dims.v[d]) && !self.c.s.names[dims.v[d] as usize].is_empty() {
                anc.push(dims.v[d]);
                if anc.len() == 3 {
                    break;
                }
            }
        }
        if anc.is_empty() {
            return self.vocab_code(ctx, s, vkey, keys);
        }
        // longest enclosing key that s starts with (innermost on ties)
        let pick = s.and_then(|s| {
            let names = &self.c.s.names;
            (0..anc.len()).filter(|&i| s.starts_with(&*names[anc[i] as usize])).max_by_key(|&i| (names[anc[i] as usize].len(), usize::MAX - i))
        });
        let t = self.c.t(&[ctx, self.t.anc, ki(anc.len() as i64)]);
        let a = code_uint(&mut self.c, &self.tags, t, pick.map_or(0, |i| i + 1) as u128)? as usize;
        if a == 0 {
            return self.vocab_code(ctx, s, vkey, keys);
        }
        let a = *anc.get(a - 1).ok_or("corrupt stream (ancestor)")?;
        let pre: Str = (&*self.c.s.names[a as usize]).into();
        let rctx = self.c.t(&[ctx, self.t.anc]);
        let rest = self.vocab_code(rctx, s.map(|s| &s[pre.len()..]), vkey | 1 << 31, keys)?;
        Ok(if rest.is_empty() { pre } else { format!("{pre}{rest}").into() })
    }

    fn vocab_code(&mut self, ctx: u64, s: Option<&str>, vkey: u32, keys: bool) -> CResult<Str> {
        let voc = if keys { self.kvocab.entry(vkey).or_default() } else { self.vocab.entry(vkey).or_default() };
        let idx = match s {
            Some(s) => voc.iter().position(|x| &**x == s).unwrap_or(voc.len()),
            None => 0,
        };
        let len = voc.len();
        let t = self.c.t(&[ctx, self.t.vi]);
        let idx = code_uint(&mut self.c, &self.tags, t, idx as u128)? as usize;
        let out: Str = if idx < len {
            let voc = if keys { self.kvocab.get_mut(&vkey) } else { self.vocab.get_mut(&vkey) }.unwrap();
            voc.remove(idx)
        } else {
            self.literal(ctx, s)?.into()
        };
        let voc = if keys { self.kvocab.get_mut(&vkey) } else { self.vocab.get_mut(&vkey) }.unwrap();
        voc.insert(0, out.clone());
        voc.truncate(VOCAB_CAP);
        Ok(out)
    }

    /// length, then bytes with an order-1 binary context model
    fn literal(&mut self, ctx: u64, s: Option<&str>) -> CResult<String> {
        let b: &[u8] = s.map(|x| x.as_bytes()).unwrap_or(&[]);
        let t = self.c.t(&[ctx, self.t.ll]);
        let n = code_uint(&mut self.c, &self.tags, t, b.len() as u128)? as usize;
        if n > 1 << 30 {
            return Err("corrupt stream (literal)".into());
        }
        let mut out = Vec::with_capacity(n);
        let mut prev = 0u32;
        for k in 0..n {
            let byte = if self.enc() { b[k] as u32 } else { 0 };
            let mut node = 1u32;
            for j in (0..8).rev() {
                let t = self.c.t(&[self.t.lit, ki(prev as i64), ki(node as i64)]);
                let bit = self.c.bit(t, (byte >> j) & 1 == 1);
                node = node << 1 | bit as u32;
            }
            let byte = node & 0xff;
            out.push(byte as u8);
            prev = byte;
        }
        String::from_utf8(out).map_err(|_| "corrupt stream (utf-8)".into())
    }

    // -------------------------------------------------------------- containers
    /// previously coded objects/arrays at neighbouring coordinates
    fn analogs(&self, sig: u32, dims: &Dims) -> Vec<V> {
        let mut out: Vec<V> = Vec::new();
        for (d, t, val) in self.dim_alts(dims).iter().copied() {
            if t != A_PREV {
                continue;
            }
            if let Some(o) = dims.with(d, val).k(sig).and_then(|k| self.objs.get(&k)) {
                if !out.iter().any(|x| Arc::ptr_eq(x, o)) {
                    out.push(o.clone());
                }
            }
        }
        out
    }

    fn obj(&mut self, v: Option<&V>, sig: u32, dims: &Dims) -> CResult<V> {
        let enc = self.enc();
        let is_map = self.maps.contains(&sig);
        let an = self.analogs(sig, dims);
        let anobj: Vec<&[(Str, V)]> = an.iter().filter_map(|a| a.obj()).collect();
        let dobj = v.and_then(|v| v.obj());
        let ctx = self.c.t(&[self.t.keys, ki(sig as i64)]);
        let keys_in: Option<Vec<Str>> = dobj.map(|o| o.iter().map(|(k, _)| k.clone()).collect());
        let cands: Vec<Vec<Str>> = anobj.iter().map(|a| a.iter().map(|(k, _)| k.clone()).collect()).collect();
        let keys = self.keylist(ctx, sig, keys_in.as_deref(), &cands, dims)?;
        let kids: Vec<V> = match dobj {
            Some(o) => o.iter().map(|(_, x)| x.clone()).collect(),
            None => Vec::new(),
        };
        let keep = self.split_flag(v, keys.len())?;
        let d0 = self.deferred.len();
        let items = self.children(sig, dims, &keys, if enc { Some(&kids) } else { None }, &anobj, is_map, keep)?;
        let out = if enc { v.unwrap().clone() } else { Arc::new(Val::Obj(keys.into_iter().zip(items).collect())) };
        // (holding placeholders for deferred children, it is not a usable analog)
        if let Some(k) = dims.k(sig).filter(|_| self.deferred.len() == d0) {
            self.objs.insert(k, out.clone());
            self.stored();
        }
        Ok(out)
    }

    /// Code the kinds then the values of an object's children (ints last, so
    /// they can use the other fields of the object as predictors).
    pub fn children(&mut self, sig: u32, dims: &Dims, keys: &[Str], vals: Option<&[V]>, an: &[&[(Str, V)]], is_map: bool, keep: Option<usize>) -> CResult<Vec<V>> {
        let mut kinds = Vec::with_capacity(keys.len());
        for (ix, k) in keys.iter().enumerate() {
            let hint = an.iter().find_map(|a| get(a, k, ix)).map(|x| kind_hint(x)).unwrap_or(-1);
            let kc = if is_map { NONE } else { self.c.s.c(k) };
            let t = self.c.t(&[self.t.kind, ki(sig as i64), kc, ki(hint)]);
            let tc = self.c.t(&[self.t.kind, ki(hint)]);
            let kv = vals.map(|v| self.kind_enc(&v[ix])).unwrap_or(0);
            kinds.push(code_uint2(&mut self.c, &self.tags, t, tc, kv as u128)? as u32);
        }
        let mut out: Vec<Option<V>> = vec![None; keys.len()];
        for pass_int in [false, true] {
            for (ix, k) in keys.iter().enumerate() {
                if (kinds[ix] == K_INT) != pass_int {
                    continue;
                }
                let kid = self.c.s.id(k);
                let (csig, cd) = if is_map {
                    (self.sigs.child(sig, WILD), dims.push(kid, sig, true))
                } else {
                    (self.sigs.child(sig, kid), *dims)
                };
                let kidof = is_map.then_some((sig, *dims, kid));
                if keep.is_some_and(|kp| ix >= kp) && matches!(kinds[ix], K_OBJ | K_ARR) {
                    out[ix] = Some(self.defer(kinds[ix], vals.map(|v| &v[ix]), csig, cd, kidof));
                    continue;
                }
                let x = self.node(kinds[ix], vals.map(|v| &v[ix]), csig, &cd)?;
                if let Some((ps, pd, kid)) = kidof {
                    self.kid_done(ps, &pd, kid);
                }
                out[ix] = Some(x);
            }
        }
        Ok(out.into_iter().map(|x| x.unwrap()).collect())
    }

    fn keylist(&mut self, ctx: u64, sig: u32, keys: Option<&[Str]>, cands: &[Vec<Str>], dims: &Dims) -> CResult<Vec<Str>> {
        let mut seen: Vec<&Vec<Str>> = Vec::new();
        for cd in cands {
            if !seen.iter().any(|s| *s == cd) {
                seen.push(cd);
            }
        }
        for (k, cd) in seen.iter().take(3).enumerate() {
            let t = self.c.t(&[ctx, self.t.kl, ki(k as i64)]);
            if self.c.bit(t, keys.is_some_and(|ks| ks == cd.as_slice())) {
                return Ok((*cd).clone());
            }
        }
        let empty = Vec::new();
        let base: &Vec<Str> = seen.first().copied().unwrap_or(&empty);
        // edit against the first candidate: surviving keys (in order), then insertions
        let mut keep: Set<&str> = Set::default();
        if let Some(ks) = keys {
            let kset: Set<&str> = ks.iter().map(|k| &**k).collect();
            let kept: Vec<&str> = base.iter().map(|k| &**k).filter(|k| kset.contains(k)).collect();
            let mut it = ks.iter();
            if kept.iter().all(|k| it.any(|x| &**x == *k)) {
                keep = kept.into_iter().collect();
            }
        }
        let mut out: Vec<Str> = Vec::new();
        let mut pk = true;
        for k in base {
            let t = self.c.t(&[ctx, self.t.kp, ki(pk as i64)]);
            pk = self.c.bit(t, keep.contains(&**k));
            if pk {
                out.push(k.clone());
            }
        }
        let ins: Vec<(usize, Str)> = match keys {
            Some(ks) => ks.iter().enumerate().filter(|(_, k)| !keep.contains(&***k)).map(|(p, k)| (p, k.clone())).collect(),
            None => Vec::new(),
        };
        let t = self.c.t(&[ctx, self.t.ni]);
        let ni = code_uint(&mut self.c, &self.tags, t, ins.len() as u128)? as usize;
        let mut last = 0usize;
        for j in 0..ni {
            let t = self.c.t(&[ctx, self.t.ip]);
            let delta = if self.enc() { (ins[j].0 - last) as u128 } else { 0 };
            let p = code_uint(&mut self.c, &self.tags, t, delta)? as usize + last;
            let c0 = self.c.cost_total();
            let k = self.text_code(ctx, if self.enc() { Some(&*ins[j].1) } else { None }, sig, true, dims)?;
            self.cat_cost(1, c0);
            if p > out.len() {
                return Err("corrupt stream (key position)".into());
            }
            out.insert(p, k);
            last = p + 1;
        }
        Ok(out)
    }

    fn arr(&mut self, v: Option<&V>, sig: u32, dims: &Dims) -> CResult<V> {
        let enc = self.enc();
        let items: &[V] = match v.map(|v| &**v) {
            Some(Val::Arr(a)) => a,
            _ => &[],
        };
        let an = self.analogs(sig, dims);
        let mut lens: Vec<usize> = Vec::new();
        for a in &an {
            if let Val::Arr(x) = &**a {
                if !lens.contains(&x.len()) {
                    lens.push(x.len());
                }
            }
        }
        let ctx = self.c.t(&[self.t.len, ki(sig as i64)]);
        let mut n = None;
        for (k, &l) in lens.iter().take(2).enumerate() {
            let t = self.c.t(&[ctx, self.t.alen, ki(k as i64)]);
            if self.c.bit(t, items.len() == l) {
                n = Some(l);
                break;
            }
        }
        let n = match n {
            Some(l) => l,
            // checked before narrowing: usize is 32 bits on wasm
            None => usize::try_from(code_uint(&mut self.c, &self.tags, ctx, items.len() as u128)?).ok().filter(|&n| n as u64 <= 1 << 32).ok_or("corrupt stream (array length)")?,
        };
        let keep = self.split_flag(v, n)?;
        let d0 = self.deferred.len();
        let csig = self.sigs.child(sig, ELEM);
        let first_an: Option<&[V]> = an.iter().find_map(|a| match &**a {
            Val::Arr(x) => Some(&x[..]),
            _ => None,
        });
        let mut out = Vec::with_capacity(if enc { 0 } else { n });
        let mut prevk: i64 = -1;
        // content hash of the previous element, when it was a tuple of numbers
        let mut prev_h: Option<u64> = None;
        for j in 0..n {
            let hint = first_an.and_then(|x| x.get(j)).map(|x| kind_hint(x)).unwrap_or(prevk);
            let t = self.c.t(&[self.t.akind, ki(sig as i64), ki(hint)]);
            let kv = items.get(j).map(|x| kind_of(x)).unwrap_or(0);
            let kind = code_uint(&mut self.c, &self.tags, t, kv as u128)? as u32;
            let jd = dims.push(j as u32, sig, false);
            if keep.is_some_and(|kp| j >= kp) && matches!(kind, K_OBJ | K_ARR) {
                let x = self.defer(kind, items.get(j), csig, jd, None);
                if !enc {
                    out.push(x);
                }
                prev_h = None;
                prevk = kind as i64;
                continue;
            }
            let known = if kind == K_ARR { self.tuple_ref(csig, prev_h, items.get(j))? } else { None };
            let x = match known {
                Some(x) => {
                    self.tuple_replay(&x, csig, &jd);
                    x
                }
                None => self.node(kind, items.get(j), csig, &jd)?,
            };
            let cur = num_tuple(&x);
            if let Some((h, vals)) = &cur {
                self.tuple_seen(csig, prev_h, *h, &x, vals);
            }
            prev_h = cur.map(|c| c.0);
            prevk = kind as i64;
            if !enc {
                out.push(x);
            }
        }
        let out = if enc { v.unwrap().clone() } else { Arc::new(Val::Arr(out)) };
        if let Some(k) = dims.k(sig).filter(|_| self.deferred.len() == d0) {
            self.objs.insert(k, out.clone());
            self.stored();
        }
        Ok(out)
    }

    /// Records mode: code record `i`'s modelled fields (keys known from the shape).
    /// Count stored values; past STORE_CAP, flush all per-value stores. Encoder and
    /// decoder store the same sequence, so they flush at the same points. (In
    /// records mode the two-record generations keep the stores far below it.)
    fn stored(&mut self) {
        self.nstored += 1;
        if self.nstored >= STORE_CAP {
            self.flush_stores();
        }
    }

    /// drop every per-value store (what was learned stays in the coder and statistics)
    pub fn flush_stores(&mut self) {
        self.nstored = 0;
        self.nums.clear_all();
        self.strs.clear_all();
        self.objs.clear_all();
        self.kids.clear_all();
        self.fields.clear_all();
        self.disc.clear_all();
        self.offs.clear_all();
        self.dos.clear_all();
        self.peers.clear_all();
        self.lastmiss.clear_all();
        self.alts_cache = None;
    }

    /// move the per-record stores on to record `i`
    fn advance(&mut self, i: u32) {
        self.nstored = 0;
        self.nums.advance(i);
        self.strs.advance(i);
        self.objs.advance(i);
        self.kids.advance(i);
        self.fields.advance(i);
        self.disc.advance(i);
        self.offs.advance(i);
        self.dos.advance(i);
        self.peers.advance(i);
        self.lastmiss.advance(i);
    }

    pub fn record(&mut self, rsig: u32, i: u32, keys: &[Str], vals: Option<&[V]>, bucket_of: &dyn Fn(&str) -> usize) -> CResult<Vec<V>> {
        self.advance(i);
        let dims = Dims::root().push(i, Sigs::ROOT, false);
        let an = self.analogs(rsig, &dims);
        let anobj: Vec<&[(Str, V)]> = an.iter().filter_map(|a| a.obj()).collect();
        // per-column cost attribution: code each field separately
        let mut kinds = Vec::with_capacity(keys.len());
        for (ix, k) in keys.iter().enumerate() {
            self.c.bucket = bucket_of(k);
            let hint = anobj.iter().find_map(|a| get(a, k, ix)).map(|x| kind_hint(x)).unwrap_or(-1);
            let kc = self.c.s.c(k);
            let t = self.c.t(&[self.t.kind, ki(rsig as i64), kc, ki(hint)]);
            let tc = self.c.t(&[self.t.kind, ki(hint)]);
            let kv = vals.map(|v| self.kind_enc(&v[ix])).unwrap_or(0);
            kinds.push(code_uint2(&mut self.c, &self.tags, t, tc, kv as u128)? as u32);
        }
        let mut out: Vec<Option<V>> = vec![None; keys.len()];
        for pass_int in [false, true] {
            for (ix, k) in keys.iter().enumerate() {
                if (kinds[ix] == K_INT) != pass_int {
                    continue;
                }
                self.c.bucket = bucket_of(k);
                let kid = self.c.s.id(k);
                let csig = self.sigs.child(rsig, kid);
                out[ix] = Some(self.node(kinds[ix], vals.map(|v| &v[ix]), csig, &dims)?);
            }
        }
        let out: Vec<V> = out.into_iter().map(|x| x.unwrap()).collect();
        let rec = Arc::new(Val::Obj(keys.iter().cloned().zip(out.iter().cloned()).collect()));
        if let Some(k) = dims.k(rsig) {
            self.objs.insert(k, rec);
        }
        Ok(out)
    }
}


/// a short array of numbers (a point, a pair, ...): content hash and values
fn num_tuple(v: &Val) -> Option<(u64, SVec<Option<f64>, 4>)> {
    let Val::Arr(a) = v else { return None };
    if a.is_empty() || a.len() > 4 {
        return None;
    }
    let mut h = crate::fx::Fx::default();
    let mut vals = SVec::new();
    for x in a {
        let (Val::Int(t) | Val::Float(t)) = &**x else { return None };
        h.write(t.as_bytes());
        vals.push(t.parse::<f64>().ok());
    }
    Some((h.finish(), vals))
}

/// same tuple text (hash collisions must not change the output)
fn same_tuple(a: &Val, b: &Val) -> bool {
    let (Val::Arr(a), Val::Arr(b)) = (a, b) else { return false };
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(x, y)| match (&**x, &**y) {
            (Val::Int(s), Val::Int(t)) | (Val::Float(s), Val::Float(t)) => s == t,
            _ => false,
        })
}

const NO_PT: u32 = u32::MAX;
/// candidates offered for a repeated tuple
const TD_CANDS: usize = 16;
/// consecutive steps averaged before the spatial index's cell size is fixed
const TD_LEARN: u32 = 256;
/// distinct tuples kept per signature before the dictionary starts over
const TD_CAP: usize = 1 << 22;

#[derive(Clone)]
struct TPt {
    v: V,
    h: u64,
    x: f64,
    y: f64,
    /// the tuple that last followed / preceded this one in a sequence
    fwd: u32,
    back: u32,
}

/// Distinct tuples of numbers (points) seen in sequences of one signature, with
/// a grid over their first two components. Polygons and paths that share
/// vertices or edges with earlier ones code those as references.
#[derive(Clone, Default)]
struct TupleDict {
    pts: Vec<TPt>,
    seen: Map<u64, u32>,
    /// grid cell -> its points (coordinates inline: the neighbour search reads only these)
    cells: Map<(i64, i64), Vec<(f64, f64, u32)>>,
    /// grid cell size (0 while still learning the typical step)
    cell: f64,
    dsum: f64,
    dn: u32,
    last: Option<u32>,
}

impl TupleDict {
    fn cell_of(&self, x: f64, y: f64) -> (i64, i64) {
        ((x / self.cell).floor() as i64, (y / self.cell).floor() as i64)
    }
    fn index(&mut self, i: u32) {
        let p = &self.pts[i as usize];
        let c = self.cell_of(p.x, p.y);
        self.cells.entry(c).or_default().push((p.x, p.y, i));
    }
    /// retraced neighbours of the previous tuple, then known tuples nearest the last one
    fn candidates(&self, prev_h: Option<u64>) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        if let Some(&pi) = prev_h.and_then(|h| self.seen.get(&h)) {
            let p = &self.pts[pi as usize];
            for c in [p.fwd, p.back] {
                if c != NO_PT && !out.contains(&c) {
                    out.push(c);
                }
            }
        }
        let Some(r) = self.last else { return out };
        let (rx, ry) = (self.pts[r as usize].x, self.pts[r as usize].y);
        let (cx, cy) = self.cell_of(rx, ry);
        let mut near: Vec<(f64, u32)> = Vec::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(l) = self.cells.get(&(cx + dx, cy + dy)) {
                    for &(x, y, i) in l.iter().rev().take(64) {
                        if i != r && !out.contains(&i) {
                            near.push(((x - rx).powi(2) + (y - ry).powi(2), i));
                        }
                    }
                }
            }
        }
        // nearest first, ties by recency (a total order: indices are distinct),
        // so selecting the k smallest and sorting only those is exact
        let cmp = |a: &(f64, u32), b: &(f64, u32)| a.0.total_cmp(&b.0).then(b.1.cmp(&a.1));
        let k = TD_CANDS.saturating_sub(out.len());
        if near.len() > k && k > 0 {
            near.select_nth_unstable_by(k - 1, cmp);
            near.truncate(k);
        }
        near.sort_unstable_by(cmp);
        out.extend(near.iter().map(|x| x.1).take(k));
        out
    }
}

impl Model {
    /// A tuple in a sequence: a reference to a known tuple, or None (coded normally).
    fn tuple_ref(&mut self, csig: u32, prev_h: Option<u64>, item: Option<&V>) -> CResult<Option<V>> {
        let Some(td) = self.tdict.get(&csig).filter(|td| td.cell > 0.0) else { return Ok(None) };
        let cands = td.candidates(prev_h);
        if cands.is_empty() {
            return Ok(None);
        }
        let pick = match item.filter(|_| self.enc()) {
            Some(v) => num_tuple(v).and_then(|(h, _)| cands.iter().position(|&c| td.pts[c as usize].h == h && same_tuple(&td.pts[c as usize].v, v))),
            None => None,
        };
        let vs: Vec<V> = cands.iter().map(|&c| td.pts[c as usize].v.clone()).collect();
        let t = self.c.t(&[self.t.tref, ki(csig as i64), ki(vs.len() as i64)]);
        let k = code_uint(&mut self.c, &self.tags, t, pick.map_or(0, |p| p + 1) as u128)? as usize;
        if k == 0 {
            return Ok(None);
        }
        Ok(Some(vs.get(k - 1).ok_or("corrupt stream (tuple reference)")?.clone()))
    }

    /// the stores a normally coded tuple would have updated
    fn tuple_replay(&mut self, x: &V, csig: u32, jd: &Dims) {
        let Val::Arr(a) = &**x else { return };
        let esig = self.sigs.child(csig, ELEM);
        for (i, e) in a.iter().enumerate() {
            if let Some((n, _, _)) = leaf_num(e) {
                self.after_num(esig, &jd.push(i as u32, csig, false), n, false);
            }
        }
        if let Some(k) = jd.k(csig) {
            self.objs.insert(k, x.clone());
            self.stored();
        }
    }

    fn tuple_seen(&mut self, csig: u32, prev_h: Option<u64>, h: u64, x: &V, vals: &[Option<f64>]) {
        let (Some(Some(px)), Some(Some(py))) = (vals.first().copied(), vals.get(1).copied()) else { return };
        if !px.is_finite() || !py.is_finite() {
            return;
        }
        let td = self.tdict.entry(csig).or_default();
        if td.pts.len() >= TD_CAP {
            *td = TupleDict::default();
        }
        let i = match td.seen.get(&h) {
            Some(&i) if same_tuple(&td.pts[i as usize].v, x) => i,
            _ => {
                let i = td.pts.len() as u32;
                td.pts.push(TPt { v: x.clone(), h, x: px, y: py, fwd: NO_PT, back: NO_PT });
                td.seen.insert(h, i);
                if td.cell > 0.0 {
                    td.index(i);
                }
                i
            }
        };
        if let Some(&pi) = prev_h.and_then(|h| td.seen.get(&h)) {
            td.pts[pi as usize].fwd = i;
            td.pts[i as usize].back = pi;
        }
        if let (Some(l), true) = (td.last, td.cell == 0.0) {
            let q = &td.pts[l as usize];
            td.dsum += (q.x - px).abs() + (q.y - py).abs();
            td.dn += 1;
            if td.dn == TD_LEARN {
                td.cell = td.dsum / td.dn as f64 * 2.0;
                if !(td.cell > 0.0 && td.cell.is_finite()) {
                    td.cell = 1.0;
                }
                for k in 0..td.pts.len() as u32 {
                    td.index(k);
                }
            }
        }
        td.last = Some(i);
    }
}

/// Containers with at least this many children may be split across chunks.
const SPLIT_MIN: usize = 64;

/// A child of a split collection, left out of the prime pass and coded later
/// by a model forked from the prime's final state.
#[derive(Clone)]
pub struct Deferred {
    pub kind: u32,
    pub csig: u32,
    pub cd: Dims,
    /// map children: (map sig, map dims, key id) for the sibling-key list
    kidof: Option<(u32, Dims, u32)>,
    /// the value (encoder) or a unique placeholder (decoder)
    pub val: V,
}

/// Tree-mode splitting: in the prime pass, a large collection's children past
/// `keep` are deferred. The encoder names the collections (container pointer ->
/// keep); a flag in the stream tells the decoder.
#[derive(Clone, Default)]
pub struct Split {
    pub targets: Map<usize, usize>,
}

impl Model {
    fn split_flag(&mut self, v: Option<&V>, n: usize) -> CResult<Option<usize>> {
        let Some(sp) = &self.split else { return Ok(None) };
        if n < SPLIT_MIN {
            return Ok(None);
        }
        let keep = v.and_then(|v| sp.targets.get(&(Arc::as_ptr(v) as usize)).copied());
        let t = self.c.t(&[self.t.split]);
        if !self.c.bit(t, keep.is_some()) {
            return Ok(None);
        }
        let t = self.c.t(&[self.t.split, ki(1)]);
        let k = code_uint(&mut self.c, &self.tags, t, keep.unwrap_or(0) as u128)? as usize;
        if k > n {
            return Err("corrupt stream (split)".into());
        }
        Ok(Some(k))
    }

    fn defer(&mut self, kind: u32, v: Option<&V>, csig: u32, cd: Dims, kidof: Option<(u32, Dims, u32)>) -> V {
        let val = v.cloned().unwrap_or_else(|| Arc::new(Val::Null));
        self.deferred.push(Deferred { kind, csig, cd, kidof, val: val.clone() });
        val
    }

    /// a map child is done: it becomes the previous sibling for the next one
    fn kid_done(&mut self, sig: u32, dims: &Dims, kid: u32) {
        if let Some(ck) = (dims.n < MAXD).then(|| dims.push(0, sig, true).container(dims.n)) {
            self.kids.entry(ck).or_default().push(kid);
            self.alts_ver += 1;
        }
    }

    /// Code a deferred child (in a forked model).
    pub fn code_deferred(&mut self, d: &Deferred) -> CResult<V> {
        let x = self.node(d.kind, self.enc().then_some(&d.val), d.csig, &d.cd)?;
        if let Some((ps, pd, kid)) = d.kidof {
            self.kid_done(ps, &pd, kid);
        }
        Ok(x)
    }
}

impl Model {
    /// Before forking chunk models: drop the per-value stores and their memory.
    /// Forked chunks code values far from the prime's, so the stores would only
    /// be copied into every fork for little use. (Encoder and decoder both do it.)
    pub fn release_stores(&mut self) {
        self.flush_stores();
        self.nums.release();
        self.strs.release();
        self.objs.release();
        self.kids.release();
        self.fields.release();
        self.disc.release();
        self.offs.release();
        self.dos.release();
        self.peers.release();
        self.lastmiss.release();
    }
}
