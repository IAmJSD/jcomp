//! jcomp: lossless, structure-aware JSON compressor (library). Decompression reproduces
//! the input byte for byte.
//!
//! Container: MAGIC | varint len(header) | header JSON | varint len(A) | A | B chunks
//!   header: serialization style, mode, schema (maps, rational grids), split, chunk sizes
//!   A: xz (raw LZMA2) of record shapes and the columns kept out of the model
//!      (in raw mode: of the input bytes)
//!   B: one range-coded stream per chunk of records

mod analyze;
mod coder;
mod fx;
pub mod json;
mod layout;
mod model;
mod num;
mod par;
mod stream;
mod sys;

pub use stream::Decoder;


use coder::Coder;
use json::{dumps, parse, Str, Style, Val, V};
use model::{kind_of, Comp, Dims, Model, Schema, Sigs, ELEM};
use num::Grid;
use par::par_map;
use std::io::{Read, Write};
use std::sync::Arc;
use sys::Instant;

const MAGIC: &[u8] = b"JCv1";
/// Deeper documents are compressed as raw xz. Wasm engines cap the native call
/// stack at about 1 MB (roughly 1300 levels of model recursion), so wasm builds
/// stop well short of that.
const MAX_DEPTH: usize = if cfg!(target_arch = "wasm32") { 512 } else { 10_000 };
/// below this size, auto mode also tries plain xz of the whole input
const SMALL: usize = 32 << 20;
/// `--chunks auto` tries these chunk counts besides 1...
const AUTO_CHUNKS: [usize; 2] = [4, 8];
/// ...and accepts up to this much size growth (per mille) for parallel decoding
const AUTO_CHUNK_SLACK: usize = 5;
/// bytes of a column xz-compressed when estimating its xz size
const XZ_SAMPLE: usize = 1 << 20;

type Res<T> = Result<T, String>;

// ------------------------------------------------------------------ options
#[derive(Clone, Debug)]
pub enum Split {
    /// measure every column under both coders and keep the cheaper
    Auto,
    /// model every column
    Model,
    /// xz every column (plain xz of the input, in tree documents)
    Xz,
    /// xz exactly these columns, model the rest
    Cols(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct Opts {
    pub split: Split,
    /// chunks of records decoded in parallel; 0 = auto (most parallel layout
    /// within AUTO_CHUNK_SLACK of the best size)
    pub chunks: usize,
    pub threads: usize,
    /// report decisions on stderr
    pub verbose: bool,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts { split: Split::Auto, chunks: 0, threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1), verbose: false }
    }
}

// ------------------------------------------------------------------ xz
fn xz_stream(encoder: bool) -> liblzma::stream::Stream {
    let mut opts = liblzma::stream::LzmaOptions::new_preset(9 | 0x8000_0000).unwrap();
    opts.dict_size(1 << 26).literal_context_bits(3).literal_position_bits(0).position_bits(0);
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    if encoder {
        liblzma::stream::Stream::new_raw_encoder(&f).unwrap()
    } else {
        liblzma::stream::Stream::new_raw_decoder(&f).unwrap()
    }
}

/// xz size with a fast preset (for estimates; somewhat larger than `xz_compress`)
fn xz_size_fast(data: &[u8]) -> usize {
    let mut opts = liblzma::stream::LzmaOptions::new_preset(6).unwrap();
    opts.literal_context_bits(3).literal_position_bits(0).position_bits(0);
    let mut f = liblzma::stream::Filters::new();
    f.lzma2(&opts);
    let mut w = liblzma::write::XzEncoder::new_stream(Vec::new(), liblzma::stream::Stream::new_raw_encoder(&f).unwrap());
    w.write_all(data).unwrap();
    w.finish().unwrap().len()
}

fn xz_compress(data: &[u8]) -> Vec<u8> {
    let mut w = liblzma::write::XzEncoder::new_stream(Vec::new(), xz_stream(true));
    w.write_all(data).unwrap();
    w.finish().unwrap()
}

fn xz_decompress(data: &[u8]) -> Res<Vec<u8>> {
    let mut out = Vec::new();
    liblzma::read::XzDecoder::new_stream(data, xz_stream(false)).read_to_end(&mut out).map_err(|e| format!("xz: {e}"))?;
    Ok(out)
}

fn varint(mut n: usize, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        out.push(b | if n > 0 { 0x80 } else { 0 });
        if n == 0 {
            break;
        }
    }
}

fn read_varint(buf: &[u8], mut pos: usize) -> Res<(usize, usize)> {
    let (mut n, mut sh) = (0usize, 0);
    loop {
        let b = *buf.get(pos).ok_or("truncated")?;
        pos += 1;
        n |= ((b & 0x7f) as usize).checked_shl(sh).ok_or("bad varint")?;
        sh += 7;
        if b & 0x80 == 0 {
            return Ok((n, pos));
        }
    }
}

// ------------------------------------------------------------------ JSON helpers
fn jstr(x: &str) -> V {
    Arc::new(Val::Str(x.into()))
}
fn jint(x: i128) -> V {
    Arc::new(Val::Int(x.to_string().into()))
}
fn jarr(x: Vec<V>) -> V {
    Arc::new(Val::Arr(x))
}
fn jobj(x: Vec<(&str, V)>) -> V {
    Arc::new(Val::Obj(x.into_iter().map(|(k, v)| (k.into(), v)).collect()))
}
fn field<'a>(o: &'a [(Str, V)], k: &str) -> Option<&'a V> {
    o.iter().find(|(kk, _)| &**kk == k).map(|(_, v)| v)
}
fn compact() -> Style {
    Style::new(None, Some((",".into(), ":".into())), false)
}
fn as_int(v: &Val) -> Res<i128> {
    match v {
        Val::Int(t) => t.parse().map_err(|_| "bad header int".into()),
        _ => Err("bad header int".into()),
    }
}

// ------------------------------------------------------------------ styles
/// candidate json.dumps styles (Python's indent/separators/ensure_ascii)
fn styles() -> Vec<(Style, Vec<(&'static str, V)>)> {
    let mut out = Vec::new();
    let bases: Vec<(Option<&str>, Option<(&str, &str)>, Vec<(&'static str, V)>)> = vec![
        (Some(" "), None, vec![("indent", jint(1))]),
        (Some("  "), None, vec![("indent", jint(2))]),
        (Some("    "), None, vec![("indent", jint(4))]),
        (None, None, vec![]),
        (None, Some((",", ":")), vec![("separators", jarr(vec![jstr(","), jstr(":")]))]),
        (Some("\t"), None, vec![("indent", jstr("\t"))]),
    ];
    for (ind, sep, hdr) in bases {
        for ea in [true, false] {
            let mut h = hdr.clone();
            h.push(("ensure_ascii", Arc::new(Val::Bool(ea))));
            out.push((Style::new(ind.map(String::from), sep.map(|(a, b)| (a.into(), b.into())), ea), h));
        }
    }
    out
}

fn find_style(raw: &[u8], doc: &Val) -> Option<Vec<(&'static str, V)>> {
    for (st, mut hdr) in styles() {
        let out = dumps(doc, &st);
        for tail in ["", "\n"] {
            if raw.len() == out.len() + tail.len() && raw.starts_with(&out) && raw.ends_with(tail.as_bytes()) {
                hdr.push(("tail", jstr(tail)));
                return Some(hdr);
            }
        }
    }
    None
}

fn style_from_header(h: &[(Str, V)]) -> Res<(Style, String)> {
    let indent = match field(h, "indent").map(|v| &**v) {
        None | Some(Val::Null) => None,
        Some(Val::Int(n)) => Some(" ".repeat(n.parse::<usize>().map_err(|_| "bad indent")?)),
        Some(Val::Str(t)) => Some(t.to_string()),
        _ => return Err("bad indent".into()),
    };
    let seps = match field(h, "separators").map(|v| &**v) {
        None => None,
        Some(Val::Arr(a)) if a.len() == 2 => match (&*a[0], &*a[1]) {
            (Val::Str(x), Val::Str(y)) => Some((x.to_string(), y.to_string())),
            _ => return Err("bad separators".into()),
        },
        _ => return Err("bad separators".into()),
    };
    let ea = !matches!(field(h, "ensure_ascii").map(|v| &**v), Some(Val::Bool(false)));
    let tail = match field(h, "tail").map(|v| &**v) {
        Some(Val::Str(t)) => t.to_string(),
        _ => return Err("bad tail".into()),
    };
    Ok((Style::new(indent, seps, ea), tail))
}

// ------------------------------------------------------------------ layout in the header
fn bytes_str(b: &[u8]) -> V {
    // whitespace and string tokens of valid UTF-8 input
    jstr(std::str::from_utf8(b).unwrap())
}

fn layout_json(l: &layout::Layout) -> V {
    let mut t: Vec<(&u32, &Vec<u8>)> = l.table.iter().collect();
    t.sort();
    let exc = |e: &[(u64, Vec<u8>)]| {
        let mut last = 0u64;
        jarr(e
            .iter()
            .map(|(i, b)| {
                let d = i - last;
                last = *i;
                jarr(vec![jint(d as i128), bytes_str(b)])
            })
            .collect())
    };
    jobj(vec![
        ("ascii", Arc::new(Val::Bool(l.ascii))),
        ("t", jarr(t.into_iter().map(|(k, w)| jarr(vec![jint(*k as i128), bytes_str(w)])).collect())),
        ("g", exc(&l.gap_exc)),
        ("s", exc(&l.str_exc)),
    ])
}

fn layout_parse(v: &Val) -> Res<layout::Layout> {
    let bad = || "bad layout".to_string();
    let o = v.obj().ok_or_else(bad)?;
    let ascii = matches!(field(o, "ascii").map(|x| &**x), Some(Val::Bool(true)));
    let pairs = |k: &str| -> Res<Vec<(i128, Vec<u8>)>> {
        let Val::Arr(a) = &**field(o, k).ok_or_else(bad)? else { return Err(bad()) };
        a.iter()
            .map(|p| match &**p {
                Val::Arr(p) if p.len() == 2 => Ok((as_int(&p[0])?, p[1].as_str().ok_or_else(bad)?.as_bytes().to_vec())),
                _ => Err(bad()),
            })
            .collect()
    };
    let table = pairs("t")?.into_iter().map(|(k, w)| (k as u32, w)).collect();
    let acc = |e: Vec<(i128, Vec<u8>)>| -> Vec<(u64, Vec<u8>)> {
        let mut i = 0u64;
        e.into_iter()
            .map(|(d, b)| {
                i += d as u64;
                (i, b)
            })
            .collect()
    };
    Ok(layout::Layout { ascii, table, gap_exc: acc(pairs("g")?), str_exc: acc(pairs("s")?) })
}

// ------------------------------------------------------------------ schema in the header
fn comps_json(p: &[Comp]) -> V {
    jarr(p.iter().map(|c| match c {
        Comp::Elem => jint(0),
        Comp::Wild => jint(1),
        Comp::Key(k) => jstr(k),
    }).collect())
}

fn comps_parse(v: &Val) -> Res<Vec<Comp>> {
    let Val::Arr(a) = v else { return Err("bad schema".into()) };
    a.iter()
        .map(|c| match &**c {
            Val::Int(t) if &**t == "0" => Ok(Comp::Elem),
            Val::Int(t) if &**t == "1" => Ok(Comp::Wild),
            Val::Str(k) => Ok(Comp::Key(k.to_string())),
            _ => Err("bad schema".into()),
        })
        .collect()
}

fn schema_json(s: &Schema) -> V {
    let dens = s
        .dens
        .iter()
        .map(|(p, gs)| {
            let g = gs
                .iter()
                .map(|g| {
                    let mut v = vec![jint(g.den), g.prec.map(|p| jint(p as i128)).unwrap_or(Arc::new(Val::Null))];
                    if g.scaled {
                        v.push(Arc::new(Val::Bool(true)));
                    }
                    jarr(v)
                })
                .collect();
            jarr(vec![comps_json(p), jarr(g)])
        })
        .collect();
    jobj(vec![
        ("maps", jarr(s.maps.iter().map(|p| comps_json(p)).collect())),
        ("dens", jarr(dens)),
        ("xz", jarr(s.xz.iter().map(|p| comps_json(p)).collect())),
    ])
}

fn schema_parse(v: &Val) -> Res<Schema> {
    let o = v.obj().ok_or("bad schema")?;
    let bad = || "bad schema".to_string();
    let Val::Arr(maps) = &**field(o, "maps").ok_or_else(bad)? else { return Err(bad()) };
    let Val::Arr(dens) = &**field(o, "dens").ok_or_else(bad)? else { return Err(bad()) };
    let maps = maps.iter().map(|p| comps_parse(p)).collect::<Res<_>>()?;
    let mut ds = Vec::new();
    for d in dens {
        let Val::Arr(pair) = &**d else { return Err(bad()) };
        let (Some(p), Some(gs)) = (pair.first(), pair.get(1)) else { return Err(bad()) };
        let Val::Arr(gs) = &**gs else { return Err(bad()) };
        let mut grids = Vec::new();
        for g in gs {
            let Val::Arr(g) = &**g else { return Err(bad()) };
            let den = as_int(g.first().ok_or_else(bad)?)?;
            let prec = match g.get(1).map(|x| &**x) {
                Some(Val::Null) => None,
                Some(x) => Some(u32::try_from(as_int(x)?).map_err(|_| bad())?),
                None => return Err(bad()),
            };
            let scaled = matches!(g.get(2).map(|x| &**x), Some(Val::Bool(true)));
            if den <= 0 || prec.is_some_and(|p| p > 30) || (scaled && prec.is_none()) {
                return Err(bad());
            }
            grids.push(Grid { den, prec, scaled });
        }
        ds.push((comps_parse(p)?, grids));
    }
    let xz = match field(o, "xz").map(|v| &**v) {
        Some(Val::Arr(a)) => a.iter().map(|p| comps_parse(p)).collect::<Res<_>>()?,
        _ => Vec::new(),
    };
    Ok(Schema { maps, dens: ds, xz })
}

// ------------------------------------------------------------------ records layout
/// root is a non-empty list of objects
fn records(doc: &Val) -> Option<&[V]> {
    match doc {
        Val::Arr(a) if !a.is_empty() && a.iter().all(|r| r.obj().is_some()) => Some(a),
        _ => None,
    }
}

/// all record keys in order of first appearance
fn columns(recs: &[V]) -> Vec<Str> {
    let mut seen = fx::Set::default();
    let mut out = Vec::new();
    for r in recs {
        for (k, _) in r.obj().unwrap() {
            if seen.insert(k.clone()) {
                out.push(k.clone());
            }
        }
    }
    out
}

/// Part A for records: shapes (key orders), shape per record, and the xz columns.
fn part_a(recs: &[V], xzcols: &fx::Set<Str>, xs: &[Str]) -> Vec<u8> {
    let mut shapes: Vec<Vec<Str>> = Vec::new();
    let mut index: fx::Map<Vec<Str>, usize> = fx::Map::default();
    let mut sidx = Vec::new();
    let mut cols: Vec<(Str, Vec<V>)> = Vec::new();
    let mut colix: fx::Map<Str, usize> = fx::Map::default();
    for r in recs {
        let o = r.obj().unwrap();
        let sh: Vec<Str> = o.iter().map(|(k, _)| k.clone()).collect();
        let si = *index.entry(sh.clone()).or_insert_with(|| {
            shapes.push(sh);
            shapes.len() - 1
        });
        sidx.push(jint(si as i128));
        for (k, v) in o {
            if xzcols.contains(k) {
                let ci = *colix.entry(k.clone()).or_insert_with(|| {
                    cols.push((k.clone(), Vec::new()));
                    cols.len() - 1
                });
                cols[ci].1.push(v.clone());
            }
        }
    }
    let shapes = shapes.into_iter().map(|sh| jarr(sh.into_iter().map(|k| Arc::new(Val::Str(k))).collect())).collect();
    let a = Val::Obj(vec![
        ("shapes".into(), jarr(shapes)),
        ("s".into(), jarr(sidx)),
        ("c".into(), Arc::new(Val::Obj(cols.into_iter().map(|(k, v)| (k, jarr(v))).collect()))),
        ("xs".into(), jarr(xs.iter().map(|x| Arc::new(Val::Str(x.clone()))).collect())),
    ]);
    dumps(&a, &compact())
}


/// Records are coded in chunks: the first ("prime") chunk by a fresh model, every
/// later chunk by a fork of the model as trained on the prime, so the later
/// chunks encode and decode in parallel while sharing what the prime learned.
/// Boundaries balance work (`w`: weight per record), so parallel chunks finish together.
fn chunk_ranges(w: &[u64], chunks: usize) -> Vec<(usize, usize)> {
    let n = w.len();
    let c = chunks.clamp(1, n.max(1));
    if c == 1 {
        return vec![(0, n)];
    }
    let mut cum = Vec::with_capacity(n + 1);
    cum.push(0u64);
    for &x in w {
        cum.push(cum.last().unwrap() + x.max(1));
    }
    let total = cum[n];
    // first record index whose cumulative weight reaches `target`, within [lo, hi]
    let cut = |target: u64, lo: usize, hi: usize| cum.partition_point(|&x| x < target).clamp(lo, hi);
    let prime = cut((total / (2 * c as u64)).max(total / 8), 1, n - (c - 1));
    let mut out = vec![(0, prime)];
    let rest = total - cum[prime];
    let forks = c - 1;
    let mut start = prime;
    for k in 1..=forks {
        let end = if k == forks { n } else { cut(cum[prime] + rest * k as u64 / forks as u64, start + 1, n - (forks - k)) };
        out.push((start, end));
        start = end;
    }
    out
}

/// work estimate per record: number of leaf values
fn leaves(v: &Val) -> u64 {
    match v {
        Val::Obj(o) => o.iter().map(|(_, x)| leaves(x)).sum(),
        Val::Arr(a) => a.iter().map(|x| leaves(x)).sum(),
        _ => 1,
    }
}

/// decode work of a container itself, in leaves
const W_NODE: u64 = 4;
/// Estimated decode work per item, for balancing chunks: leaves, except that a
/// short numeric array seen before (a shared vertex) is one cheap reference.
fn decode_weights<'a>(items: impl Iterator<Item = &'a Val>) -> Vec<u64> {
    fn walk(v: &Val, seen: &mut fx::Set<u64>) -> u64 {
        match v {
            Val::Obj(o) => W_NODE + o.iter().map(|(_, x)| walk(x, seen)).sum::<u64>(),
            Val::Arr(a) if (1..=4).contains(&a.len()) && a.iter().all(|x| matches!(**x, Val::Int(_) | Val::Float(_))) => {
                let mut h = fx::Fx::default();
                for x in a {
                    if let Val::Int(t) | Val::Float(t) = &**x {
                        std::hash::Hasher::write(&mut h, t.as_bytes());
                    }
                }
                if seen.insert(std::hash::Hasher::finish(&h)) { a.len() as u64 } else { 1 }
            }
            Val::Arr(a) => W_NODE + a.iter().map(|x| walk(x, seen)).sum::<u64>(),
            _ => 1,
        }
    }
    let mut seen = fx::Set::default();
    items.map(|v| walk(v, &mut seen)).collect()
}

fn record_weights(recs: &[V]) -> Vec<u64> {
    recs.iter().map(|r| leaves(r)).collect()
}

/// Encode records chunk by chunk; returns each chunk's stream and per-column costs.
#[allow(clippy::too_many_arguments)]
/// one encoded chunk: stream, per-column costs, xz side strings, string statistics
struct ChunkOut {
    bytes: Vec<u8>,
    costs: Vec<u64>,
    xz: Vec<Str>,
    sstats: Vec<(Vec<model::Comp>, model::StrStat)>,
}

fn finish_model(mut m: Model) -> ChunkOut {
    if m.stats.is_some() {
        eprintln!("predictor groups: {}", m.ngroups());
        let mut r = m.stats_report();
        r.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, line) in r.iter().take(40) {
            eprintln!("{line}");
        }
    }
    if std::env::var_os("JCOMP_STATS").is_some() && m.c.costs.is_some() {
        eprintln!("bytes by category [structure, keys, strings, numbers]: {:?}", m.cat_report());
    }
    let sstats = m.str_stats.take().map(|st| st.into_iter().map(|(sig, v)| (m.sigs.path(sig, &m.c.s.names), v)).collect()).unwrap_or_default();
    let costs = m.c.costs.take().unwrap_or_default();
    let xz = std::mem::take(&mut m.xz_out);
    ChunkOut { bytes: m.c.finish(), costs, xz, sstats }
}

/// Signatures whose strings xz codes more cheaply than the model (from tracked statistics).
fn decide_xz(outs: &[ChunkOut], threads: usize, verbose: bool) -> Vec<Vec<model::Comp>> {
    let mut agg: fx::Map<Vec<model::Comp>, model::StrStat> = fx::Map::default();
    for o in outs {
        for (p, st) in &o.sstats {
            let e = agg.entry(p.clone()).or_default();
            e.cost += st.cost;
            e.bytes += st.bytes;
            if e.sample.len() < 256 << 10 {
                e.sample.extend_from_slice(&st.sample);
            }
        }
    }
    let mut items: Vec<(Vec<model::Comp>, model::StrStat)> = agg.into_iter().filter(|(_, st)| st.bytes > 0 && !st.sample.is_empty()).collect();
    items.sort_by(|a, b| format!("{:?}", a.0).cmp(&format!("{:?}", b.0)));
    let picks = par_map(items, threads, |(p, st)| {
        let model_b = st.cost / 2048;
        let xz_b = (xz_size_fast(&st.sample) as u128 * st.bytes as u128 / st.sample.len() as u128) as u64;
        (p, model_b, xz_b)
    });
    let mut out = Vec::new();
    for (p, m, x) in picks {
        if verbose && (m > 1000 || x > 1000) {
            eprintln!("  strings {:<60} model {:>9}  xz {:>9}  -> {}", format!("{p:?}").chars().take(60).collect::<String>(), m, x, if x < m { "xz" } else { "model" });
        }
        if x < m {
            out.push(p);
        }
    }
    out
}

fn encode_records(recs: &[V], ranges: &[(usize, usize)], schema: &Schema, model_cols: &fx::Set<Str>, cols: &[Str], track: bool, threads: usize) -> Res<Vec<ChunkOut>> {
    let mut c = Coder::encoder();
    let stats = std::env::var_os("JCOMP_STATS").is_some() && !track;
    if track || stats {
        c.track_costs(cols.len());
    }
    let colix: fx::Map<&str, usize> = cols.iter().enumerate().map(|(i, k)| (&**k, i)).collect();
    let mut m0 = Model::new(c, schema);
    if stats {
        m0.stats = Some(Default::default());
    }
    if track {
        m0.str_stats = Some(Default::default());
    }
    let rsig = m0.sigs.child(Sigs::ROOT, ELEM);
    let run = |m: &mut Model, (a, b): (usize, usize)| -> Res<()> {
        for (i, r) in recs.iter().enumerate().take(b).skip(a) {
            let o = r.obj().unwrap();
            let (keys, vals): (Vec<Str>, Vec<V>) = o.iter().filter(|(k, _)| model_cols.contains(k)).map(|(k, v)| (k.clone(), v.clone())).unzip();
            m.record(rsig, i as u32, &keys, Some(&vals), &|k| colix[k])?;
        }
        Ok(())
    };
    run(&mut m0, ranges[0])?;
    let forks: Vec<Res<ChunkOut>> = par_map(ranges[1..].to_vec(), threads, |rg| {
        let mut m = m0.fork(None);
        m.stats = None;
        m.xz_out.clear();
        if m.str_stats.is_some() {
            m.str_stats = Some(Default::default());
        }
        run(&mut m, rg)?;
        Ok(finish_model(m))
    });
    let mut out = vec![finish_model(m0)];
    for f in forks {
        out.push(f?);
    }
    Ok(out)
}

// ------------------------------------------------------------------ container
/// MAGIC | varint len(A) | A | B, where A = xz(header JSON, newline, payload)
fn container(hdr: &Val, payload: &[u8], b: &[u8]) -> Vec<u8> {
    let mut h = dumps(hdr, &compact());
    h.push(b'\n');
    h.extend_from_slice(payload);
    let a = xz_compress(&h);
    let mut out = MAGIC.to_vec();
    varint(a.len(), &mut out);
    out.extend_from_slice(&a);
    out.extend_from_slice(b);
    out
}

fn raw_container(raw: &[u8]) -> Vec<u8> {
    container(&jobj(vec![("mode", jstr("raw"))]), raw, &[])
}

/// Compress any bytes; JSON written by Python's `json.dumps` (any common
/// style) gets the structural model, everything else plain xz.
pub fn compress(raw: &[u8], o: &Opts) -> Vec<u8> {
    par::on_big_stack(|| compress_(raw, o))
}

fn compress_(raw: &[u8], o: &Opts) -> Vec<u8> {
    // small inputs in auto mode: plain xz of the input may win; try it concurrently
    if matches!(o.split, Split::Auto) && raw.len() < SMALL {
        // estimate plain xz from a sample (alongside the model when threaded); run
        // it in full only when it could plausibly win
        let estimate = || {
            let cap = raw.len().min(XZ_SAMPLE);
            (xz_compress(&raw[..cap]).len() as u128 * raw.len() as u128 / cap.max(1) as u128) as usize
        };
        let (structured, est) = if o.threads > 1 {
            par::join(|| compress_structured(raw, o), estimate)
        } else {
            (compress_structured(raw, o), estimate())
        };
        return match structured {
            Ok(Some(b)) if b.len() * 10 < est * 6 => b,
            Ok(Some(b)) => {
                let plain = raw_container(raw);
                if b.len() <= plain.len() { b } else { plain }
            }
            _ => raw_container(raw),
        };
    }
    let structured = compress_structured(raw, o);
    match structured {
        Ok(Some(b)) => b,
        Ok(None) => raw_container(raw),
        Err(e) => {
            if o.verbose {
                eprintln!("raw mode: {e}");
            }
            raw_container(raw)
        }
    }
}

/// Ok(None): raw mode is the better (or only) choice
fn compress_structured(raw: &[u8], o: &Opts) -> Res<Option<Vec<u8>>> {
    let t0 = Instant::now();
    let tl = std::cell::Cell::new(t0);
    let lap = |what: &str| {
        if o.verbose {
            let now = Instant::now();
            eprintln!("  [{:>6.2}s] {} (peak {} MB)", (now - tl.get()).as_secs_f64(), what, peak_rss() >> 20);
            tl.set(now);
        }
    };
    let text = std::str::from_utf8(raw).map_err(|_| "not utf-8")?;
    let doc = parse(text, MAX_DEPTH)?;
    lap("parse");
    // Python json.dumps styles have a compact description; any other text gets
    // a learned whitespace/escaping layout (verified to reproduce the input)
    let mut hdr = match find_style(raw, &doc) {
        Some(h) => h,
        None => {
            let lay = layout::learn(raw);
            if layout::dumps(&doc, &lay) != raw {
                return Err("layout not reproducible".into());
            }
            if o.verbose {
                eprintln!("layout: {} gap contexts, {} gap exceptions, {} string exceptions", lay.table.len(), lay.gap_exc.len(), lay.str_exc.len());
            }
            vec![("layout", layout_json(&lay))]
        }
    };
    lap("layout");
    lap("find style");
    let recs = records(&doc);
    if matches!(o.split, Split::Xz) && recs.is_none() {
        return Ok(None);
    }
    let mut schema = analyze::analyze(&doc, recs.is_some(), o.threads);
    lap("analysis");
    if o.verbose {
        eprintln!("analysis: {} map levels, {} rational grids ({:.1}s)", schema.maps.len(), schema.dens.len(), t0.elapsed().as_secs_f64());
        if std::env::var_os("JCOMP_STATS").is_some() {
            for m in &schema.maps {
                eprintln!("  map {m:?}");
            }
        }
    }
    let Some(recs) = recs else {
        // tree document: model it whole (strings that xz codes better go to xz)
        // large collections inside it are split: a prime pass codes the rest of
        // the document plus the first part of each collection, then models
        // forked from the prime code the remaining children in parallel chunks
        let targets = split_targets(&doc);
        let nchunks = match o.chunks {
            0 if !targets.is_empty() && leaves(&doc) >= TREE_SPLIT_MIN => (leaves(&doc) / TREE_CHUNK_LEAVES).clamp(2, TREE_MAX_CHUNKS as u64) as usize,
            0 => 1,
            c => c,
        };
        if o.verbose && nchunks > 1 {
            eprintln!("tree split: {} collections, {} chunks", targets.len(), nchunks);
        }
        let enc_tree = |schema: &Schema, track: bool| -> Res<(Vec<ChunkOut>, Vec<usize>)> {
            let mut c = Coder::encoder();
            if track {
                c.track_costs(1);
            }
            let mut m = Model::new(c, schema);
            if track {
                m.str_stats = Some(Default::default());
            } else if std::env::var_os("JCOMP_STATS").is_some() {
                m.stats = Some(Default::default());
                m.c.track_costs(1);
            }
            if nchunks > 1 && !targets.is_empty() {
                m.split = Some(model::Split { targets: targets.clone() });
            }
            let k = kind_of(&doc);
            let t = m.c.t(&[coder::ki(-7)]);
            let tags = coder::Tags::new(&mut m.c.s);
            coder::code_uint(&mut m.c, &tags, t, k as u128)?;
            m.node(k, Some(&doc), Sigs::ROOT, &Dims::root())?;
            let split = m.split.take().is_some();
            let deferred = std::mem::take(&mut m.deferred);
            if split {
                m.release_stores();
            }
            let ranges = if split { chunk_ranges(&decode_weights(deferred.iter().map(|d| &*d.val)), nchunks - 1) } else { Vec::new() };
            let counts = ranges.iter().map(|(a, b)| b - a).collect();
            let forks = par_map(ranges, o.threads, |(a, b)| -> Res<ChunkOut> {
                let mut f = m.fork(None);
                f.stats = None;
                f.xz_out.clear();
                if f.str_stats.is_some() {
                    f.str_stats = Some(Default::default());
                }
                for d in &deferred[a..b] {
                    f.code_deferred(d)?;
                }
                Ok(finish_model(f))
            });
            let mut outs = vec![finish_model(m)];
            for f in forks {
                outs.push(f?);
            }
            Ok((outs, counts))
        };
        let (mut outs, mut counts) = enc_tree(&schema, matches!(o.split, Split::Auto))?;
        if matches!(o.split, Split::Auto) {
            schema.xz = decide_xz(&outs, o.threads, o.verbose);
            if !schema.xz.is_empty() {
                (outs, counts) = enc_tree(&schema, false)?;
            }
        }
        lap("model encode");
        hdr.push(("schema", schema_json(&schema)));
        hdr.push(("mode", jstr("tree")));
        // [deferred children, bytes, xz strings] per chunk; the prime comes first
        let chunk_meta = outs.iter().zip(std::iter::once(0).chain(counts.iter().copied())).map(|(c, n)| jarr(vec![jint(n as i128), jint(c.bytes.len() as i128), jint(c.xz.len() as i128)])).collect();
        hdr.push(("chunks", jarr(chunk_meta)));
        let xs: Vec<V> = outs.iter().flat_map(|c| c.xz.iter()).map(|x| Arc::new(Val::Str(x.clone()))).collect();
        let payload = dumps(&jobj(vec![("xs", jarr(xs))]), &compact());
        let res = container(&jobj(hdr), &payload, &outs.iter().flat_map(|c| c.bytes.iter().copied()).collect::<Vec<u8>>());
        lap("part A (xz)");
        return Ok(Some(res));
    };
    let cols = columns(recs);
    // chunks == 0 (auto): decide the split on one chunk, then pick a chunk count below
    let weights = record_weights(recs);
    let mut ranges = chunk_ranges(&weights, o.chunks.max(1));
    let all: fx::Set<Str> = cols.iter().cloned().collect();
    type Streams = Vec<(Vec<u8>, Vec<Str>)>;
    let streams_of = |v: Vec<ChunkOut>| -> Streams { v.into_iter().map(|c| (c.bytes, c.xz)).collect() };
    let encode = |schema: &Schema, ranges: &[(usize, usize)], model_cols: &fx::Set<Str>| -> Res<Streams> { Ok(streams_of(encode_records(recs, ranges, schema, model_cols, &cols, false, o.threads)?)) };
    let (xzcols, streams): (fx::Set<Str>, Streams) = match &o.split {
        Split::Model => (fx::Set::default(), encode(&schema, &ranges, &all)?),
        Split::Xz => (all.clone(), ranges.iter().map(|_| (Vec::new(), Vec::new())).collect()),
        Split::Cols(names) => {
            let x: fx::Set<Str> = names.iter().map(|s| Str::from(s.as_str())).collect();
            let m: fx::Set<Str> = all.difference(&x).cloned().collect();
            (x, encode(&schema, &ranges, &m)?)
        }
        Split::Auto => {
            // model cost per column (all modelled) vs xz size of each column alone;
            // large inputs are measured on a prefix of the records
            let big = raw.len() >= SMALL;
            let ns = if big { (recs.len() / 8).max(recs.len().min(64)) } else { recs.len() };
            let probe = if big { chunk_ranges(&weights[..ns], o.threads.min(o.chunks.max(4))) } else { ranges.clone() };
            let (res, xzs) = par::join(
                || encode_records(recs, &probe, &schema, &all, &cols, true, o.threads),
                // fast xz estimate per column from a bounded sample; borderline
                // columns get an exact-preset estimate below
                || {
                    par_map(cols.clone(), o.threads.saturating_sub(probe.len()).max(1), |k| {
                        let col: Vec<V> = recs[..ns].iter().filter_map(|r| field(r.obj().unwrap(), &k).cloned()).collect();
                        let js = dumps(&Val::Arr(col), &compact());
                        let cap = js.len().min(XZ_SAMPLE);
                        let scale = |n: usize| (n as u128 * js.len() as u128 / cap.max(1) as u128) as u64;
                        (scale(xz_size_fast(&js[..cap])), js, cap)
                    })
                },
            );
            let res = res?;
            lap("split probe (model + xz per column)");
            // strings that xz codes better than the model, per signature
            schema.xz = decide_xz(&res, o.threads, o.verbose);
            let mut cost = vec![0u64; cols.len()];
            for c in &res {
                for (i, x) in c.costs.iter().enumerate() {
                    cost[i] += x;
                }
            }
            let model_of = |i: usize| cost[i] / 8 / 256;
            let border: Vec<usize> = (0..cols.len()).filter(|&i| { let (f, m) = (xzs[i].0, model_of(i)); f * 10 >= m * 7 && f * 10 <= m * 13 }).collect();
            let exact = par_map(border.clone(), o.threads, |i| {
                let (_, js, cap) = &xzs[i];
                (i, (xz_compress(&js[..*cap]).len() as u128 * js.len() as u128 / (*cap).max(1) as u128) as u64)
            });
            let mut xzs: Vec<u64> = xzs.iter().map(|x| x.0).collect();
            for (i, e) in exact {
                xzs[i] = e;
            }
            let mut xzcols = fx::Set::default();
            for (i, k) in cols.iter().enumerate() {
                let model_bytes = model_of(i);
                if o.verbose {
                    eprintln!("  column {:<40} model {:>9}  xz {:>9}  -> {}", k, model_bytes, xzs[i], if xzs[i] < model_bytes { "xz" } else { "model" });
                }
                if xzs[i] < model_bytes {
                    xzcols.insert(k.clone());
                }
            }
            // the probe's streams are reusable only for an unchanged configuration
            let reusable = !big && schema.xz.is_empty();
            let all_model: Streams = streams_of(res);
            if !reusable {
                let m: fx::Set<Str> = all.difference(&xzcols).cloned().collect();
                (xzcols, encode(&schema, &ranges, &m)?)
            } else if xzcols.is_empty() {
                (xzcols, all_model)
            } else {
                let m: fx::Set<Str> = all.difference(&xzcols).cloned().collect();
                // the split encode, and the xz sizes of the three part-A variants, concurrently
                let a0 = || xz_compress(&part_a(recs, &xzcols, &[])).len();
                let a1 = || xz_compress(&part_a(recs, &fx::Set::default(), &[])).len();
                // all-xz variant: estimated from a sample (it is the whole document)
                let s_xz = || {
                    let js = part_a(recs, &all, &[]);
                    let cap = js.len().min(XZ_SAMPLE);
                    let est = (xz_compress(&js[..cap]).len() as u128 * js.len() as u128 / cap.max(1) as u128) as usize;
                    (est, js)
                };
                let (split, ((a1, a0), s_xz)) = par::join(|| encode(&schema, &ranges, &m), || par::join(|| par::join(a1, a0), s_xz));
                let (xz_est, xz_js) = s_xz;
                let split = split?;
                let size = |st: &Streams, a: usize| st.iter().map(|x| x.0.len()).sum::<usize>() + a;
                let s_split = size(&split, a0);
                let s_model = size(&all_model, a1);
                // exact all-xz size only when the estimate says it could win
                let s_xz = if xz_est * 6 < s_split.min(s_model) * 10 { xz_compress(&xz_js).len() } else { usize::MAX };
                if s_xz < s_split.min(s_model) {
                    (all.clone(), ranges.iter().map(|_| (Vec::new(), Vec::new())).collect())
                } else if s_split <= s_model {
                    (xzcols, split)
                } else {
                    (fx::Set::default(), all_model)
                }
            }
        }
    };
    lap("model encode");
    let mut streams = streams;
    if o.chunks == 0 && !matches!(o.split, Split::Xz) && xzcols.len() < cols.len() {
        // auto: more chunks decode in parallel; take the most parallel layout that
        // costs at most AUTO_CHUNK_SLACK over the best size
        let m: fx::Set<Str> = all.difference(&xzcols).cloned().collect();
        let mut tried: Vec<(Vec<(usize, usize)>, Streams)> = vec![(ranges.clone(), streams.clone())];
        for c in AUTO_CHUNKS {
            if recs.len() >= 32 * c {
                let r = chunk_ranges(&weights, c);
                let st = encode(&schema, &r, &m)?;
                tried.push((r, st));
            }
        }
        let size = |st: &Streams| st.iter().map(|x| x.0.len()).sum::<usize>();
        let best = tried.iter().map(|t| size(&t.1)).min().unwrap();
        let pick = tried.into_iter().filter(|t| size(&t.1) * 1000 <= best * (1000 + AUTO_CHUNK_SLACK)).max_by_key(|t| t.0.len()).unwrap();
        if o.verbose {
            eprintln!("chunks: {} (sizes within {}‰ of best {})", pick.0.len(), AUTO_CHUNK_SLACK, best);
        }
        (ranges, streams) = pick;
        lap("chunk layout");
    }
    let xs: Vec<Str> = streams.iter().flat_map(|s| s.1.iter().cloned()).collect();
    let a = part_a(recs, &xzcols, &xs);
    let model_cols: Vec<V> = cols.iter().filter(|k| !xzcols.contains(*k)).map(|k| Arc::new(Val::Str(k.clone()))).collect();
    hdr.push(("schema", schema_json(&schema)));
    hdr.push(("mode", jstr("records")));
    hdr.push(("model", jarr(model_cols)));
    hdr.push(("chunks", jarr(ranges.iter().zip(&streams).map(|((x, y), s)| jarr(vec![jint((y - x) as i128), jint(s.0.len() as i128), jint(s.1.len() as i128)])).collect())));
    if o.verbose {
        eprintln!("records: {} columns modelled, {} via xz, {} string signatures via xz, {} chunks ({:.1}s)", cols.len() - xzcols.len(), xzcols.len(), schema.xz.len(), ranges.len(), t0.elapsed().as_secs_f64());
    }
    let bs: Vec<u8> = streams.iter().flat_map(|s| s.0.iter().copied()).collect();
    let out = container(&jobj(hdr), &a, &bs);
    lap("part A (xz)");
    Ok(Some(out))
}

/// Exact inverse of `compress`.
pub fn decompress(blob: &[u8], threads: usize) -> Result<Vec<u8>, String> {
    par::on_big_stack(|| {
        let mut d = Decoder::new(threads);
        d.push(blob);
        let mut out = Vec::new();
        d.finish(&mut |p| if out.is_empty() { out = p } else { out.extend_from_slice(&p) })?;
        Ok(out)
    })
}

/// `decompress` from a reader to a writer: output is written as it is
/// decoded (see `Decoder`), so a record array never has to fit in memory as text.
pub fn decompress_to(mut r: impl Read + Send, mut w: impl Write + Send, threads: usize) -> Result<(), String> {
    par::on_big_stack(move || {
        let mut d = Decoder::new(threads);
        let mut io_err = None;
        let mut emit = |p: Vec<u8>| {
            if io_err.is_none() {
                io_err = w.write_all(&p).err();
            }
        };
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = r.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            d.push(&buf[..n]);
            d.pull(&mut emit)?;
        }
        d.finish(&mut emit)?;
        if let Some(e) = io_err {
            return Err(e.to_string());
        }
        w.flush().map_err(|e| e.to_string())
    })
}

/// peak resident set size in bytes (diagnostics only)
#[cfg(not(unix))]
fn peak_rss() -> u64 {
    0
}

/// peak resident set size in bytes (diagnostics only)
#[cfg(unix)]
fn peak_rss() -> u64 {
    unsafe extern "C" {
        fn getrusage(who: i32, usage: *mut [i64; 18]) -> i32;
    }
    let mut r = [0i64; 18];
    // SAFETY: rusage is 18 words on 64-bit Unix; ru_maxrss follows two timevals
    if unsafe { getrusage(0, &mut r) } != 0 {
        return 0;
    }
    if cfg!(target_os = "macos") { r[4] as u64 } else { r[4] as u64 * 1024 }
}

/// tree documents with at least this many leaves are split by default...
const TREE_SPLIT_MIN: u64 = 1 << 20;
/// ...into about one chunk per this many leaves...
const TREE_CHUNK_LEAVES: u64 = 1 << 19;
/// ...up to this many
const TREE_MAX_CHUNKS: usize = 16;

/// Large collections in a tree document, to split across chunks: container
/// pointer -> children kept in the prime pass (the first ~1/16 of its weight).
/// A collection has many children, nearly all containers, none dominant; the
/// search descends from the root through heavy children until it finds them.
fn split_targets(doc: &V) -> fx::Map<usize, usize> {
    fn kids(v: &Val) -> Vec<&V> {
        match v {
            Val::Obj(o) => o.iter().map(|(_, x)| x).collect(),
            Val::Arr(a) => a.iter().collect(),
            _ => Vec::new(),
        }
    }
    fn find(v: &V, w: u64, total: u64, out: &mut fx::Map<usize, usize>) {
        let ks = kids(v);
        let cw: Vec<u64> = ks.iter().map(|x| leaves(x)).collect();
        let nc = ks.iter().filter(|x| matches!(****x, Val::Obj(_) | Val::Arr(_))).count();
        if ks.len() >= 64 && nc * 10 >= ks.len() * 9 && cw.iter().max().is_some_and(|&m| m * 4 <= w) {
            let mut acc = 0;
            let mut keep = ks.len();
            for (i, &x) in cw.iter().enumerate() {
                if acc * 16 >= w {
                    keep = i;
                    break;
                }
                acc += x;
            }
            out.insert(Arc::as_ptr(v) as usize, keep.max(1));
            return;
        }
        for (x, &xw) in ks.iter().zip(&cw) {
            if xw * 16 >= total {
                find(x, xw, total, out);
            }
        }
    }
    let total = leaves(doc);
    let mut out = fx::Map::default();
    find(doc, total, total, &mut out);
    out
}

/// `v` with the deferred placeholders (by pointer) replaced; None if unchanged
fn fill(v: &V, rep: &fx::Map<usize, V>) -> Option<V> {
    if let Some(r) = rep.get(&(Arc::as_ptr(v) as usize)) {
        return Some(r.clone());
    }
    match &**v {
        Val::Arr(a) => {
            let mut out: Option<Vec<V>> = None;
            for (i, x) in a.iter().enumerate() {
                if let Some(y) = fill(x, rep) {
                    out.get_or_insert_with(|| a[..i].to_vec()).push(y);
                } else if let Some(o) = &mut out {
                    o.push(x.clone());
                }
            }
            out.map(|o| Arc::new(Val::Arr(o)))
        }
        Val::Obj(ob) => {
            let mut out: Option<Vec<(Str, V)>> = None;
            for (i, (k, x)) in ob.iter().enumerate() {
                if let Some(y) = fill(x, rep) {
                    out.get_or_insert_with(|| ob[..i].to_vec()).push((k.clone(), y));
                } else if let Some(o) = &mut out {
                    o.push((k.clone(), x.clone()));
                }
            }
            out.map(|o| Arc::new(Val::Obj(o)))
        }
        _ => None,
    }
}
