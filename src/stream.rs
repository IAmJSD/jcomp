//! Streaming decompression. Input arrives in pieces (`push`) and output leaves
//! in pieces (`pull`) as soon as it is decoded.
//!
//! A record array (a root list of objects) streams record by record through
//! its first chunk. Each later chunk is decoded once all of its bytes are in,
//! in parallel with the other chunks available by then, and written in order.
//! Other documents are written in one piece once all of their input is in.
use super::*;

/// output is handed out in pieces of about this size
const PIECE: usize = 1 << 16;

/// Push-based streaming decompressor: feed it input with [`push`](Decoder::push),
/// collect output with [`pull`](Decoder::pull), and end with
/// [`finish`](Decoder::finish). The pieces concatenate to exactly the original.
pub struct Decoder {
    threads: usize,
    /// input not yet consumed
    buf: Vec<u8>,
    state: State,
}

enum State {
    Header,
    Records(Box<Records>),
    Tree(Box<Tree>),
    Done,
    /// an earlier call failed
    Failed,
}

/// how the root array is written
enum Writer {
    Python(Style, String),
    Layout(layout::ArrayWriter),
}

struct Chunk {
    /// records a..b
    a: usize,
    b: usize,
    nbytes: usize,
    xs: Vec<Str>,
    /// position in each xz column of the chunk's first record
    xstart: fx::Map<Str, usize>,
}

struct Records {
    schema: Schema,
    writer: Writer,
    shapes: Vec<Vec<Str>>,
    rec_shape: Vec<usize>,
    /// modelled keys of each record
    keys: Vec<Vec<Str>>,
    mset: fx::Set<Str>,
    xcols: fx::Map<Str, Vec<V>>,
    chunks: Vec<Chunk>,
    /// next chunk to decode
    next: usize,
    /// the model as the first chunk left it, and the record signature
    prime: Option<(Model, u32)>,
}

struct Tree {
    lay: Option<layout::Layout>,
    style: Style,
    tail: String,
    schema: Schema,
    /// per chunk: deferred children, bytes, xz strings
    chunks: Vec<(usize, usize, Vec<Str>)>,
}

impl Decoder {
    pub fn new(threads: usize) -> Decoder {
        Decoder { threads: threads.max(1), buf: Vec::new(), state: State::Header }
    }

    /// Add input.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Decode everything the input so far allows, handing the output to `emit`
    /// in order. Natively this needs a large stack (see `decompress_to`).
    pub fn pull(&mut self, emit: &mut dyn FnMut(Vec<u8>)) -> Res<()> {
        loop {
            let state = std::mem::replace(&mut self.state, State::Failed);
            self.state = match state {
                State::Header => match self.header(emit)? {
                    Some(s) => s,
                    None => {
                        self.state = State::Header;
                        return Ok(());
                    }
                },
                State::Records(mut r) => {
                    r.pull(&mut self.buf, self.threads, emit)?;
                    if r.next < r.chunks.len() {
                        self.state = State::Records(r);
                        return Ok(());
                    }
                    let mut out = Vec::new();
                    r.writer.end(&mut out);
                    emit(out);
                    State::Done
                }
                State::Tree(t) => {
                    let need = t.chunks.iter().try_fold(0usize, |n, c| n.checked_add(c.1)).ok_or("bad header")?;
                    if self.buf.len() < need {
                        self.state = State::Tree(t);
                        return Ok(());
                    }
                    emit(t.decode(&self.buf, self.threads)?);
                    self.buf = Vec::new();
                    State::Done
                }
                State::Done => {
                    self.state = State::Done;
                    return Ok(());
                }
                State::Failed => return Err("decoder failed earlier".into()),
            };
        }
    }

    /// The input is complete: decode the rest, or fail if it is truncated.
    pub fn finish(&mut self, emit: &mut dyn FnMut(Vec<u8>)) -> Res<()> {
        self.pull(emit)?;
        match self.state {
            State::Done => Ok(()),
            _ => Err("truncated".into()),
        }
    }

    /// Parse the container header and part A once they are complete.
    fn header(&mut self, emit: &mut dyn FnMut(Vec<u8>)) -> Res<Option<State>> {
        let b = &self.buf;
        if !b.starts_with(MAGIC) {
            return if MAGIC.starts_with(b) { Ok(None) } else { Err("not a jcomp v1 file".into()) };
        }
        if !b[4..].iter().take(10).any(|&c| c & 0x80 == 0) {
            return if b.len() >= 14 { Err("bad varint".into()) } else { Ok(None) };
        }
        let (na, p) = read_varint(b, 4)?;
        let end = p.checked_add(na).ok_or("bad header")?;
        if b.len() < end {
            return Ok(None);
        }
        let a = xz_decompress(&b[p..end])?;
        self.buf.drain(..end);
        let nl = a.iter().position(|&c| c == b'\n').ok_or("bad header")?;
        let hdr = parse(std::str::from_utf8(&a[..nl]).map_err(|_| "bad header")?, 64)?;
        let h = hdr.obj().ok_or("bad header")?;
        let payload = &a[nl + 1..];
        let mode = field(h, "mode").and_then(|v| v.as_str()).ok_or("bad header")?;
        if mode == "raw" {
            emit(payload.to_vec());
            self.buf = Vec::new();
            return Ok(Some(State::Done));
        }
        let lay = match field(h, "layout") {
            Some(l) => Some(layout_parse(l)?),
            None => None,
        };
        let (style, tail) = if lay.is_some() { (Style::new(None, None, true), String::new()) } else { style_from_header(h)? };
        let schema = schema_parse(field(h, "schema").ok_or("bad header")?)?;
        let Val::Arr(chunks) = &**field(h, "chunks").ok_or("bad header")? else { return Err("bad header".into()) };
        // part A: records layout and/or the xz side-stream strings
        let at = std::str::from_utf8(payload).map_err(|_| "bad part A")?;
        let av = if at.is_empty() { jobj(vec![]) } else { parse(at, MAX_DEPTH)? };
        let ao = av.obj().ok_or("bad part A")?;
        let xs: Vec<Str> = match field(ao, "xs").map(|v| &**v) {
            Some(Val::Arr(a)) => a.iter().map(|x| x.as_str().map(Str::from).ok_or_else(|| "bad part A".to_string())).collect::<Res<_>>()?,
            _ => Vec::new(),
        };
        // per chunk: items, bytes, xz strings
        let mut spans = Vec::new();
        let mut xfirst = 0usize;
        for ch in chunks {
            let Val::Arr(x) = &**ch else { return Err("bad header".into()) };
            let (nr, nb) = (as_int(x.first().ok_or("bad header")?)? as usize, as_int(x.get(1).ok_or("bad header")?)? as usize);
            let nx = match x.get(2) {
                Some(v) => as_int(v)? as usize,
                None => 0,
            };
            spans.push((nr, nb, xs.get(xfirst..xfirst + nx).ok_or("bad part A (strings)")?.to_vec()));
            xfirst += nx;
        }
        if mode == "tree" {
            return Ok(Some(State::Tree(Box::new(Tree { lay, style, tail, schema, chunks: spans }))));
        }
        let bad = || "bad part A".to_string();
        let Val::Arr(shapes) = &**field(ao, "shapes").ok_or_else(bad)? else { return Err(bad()) };
        let Val::Arr(sidx) = &**field(ao, "s").ok_or_else(bad)? else { return Err(bad()) };
        let Val::Obj(xcols) = &**field(ao, "c").ok_or_else(bad)? else { return Err(bad()) };
        let Val::Arr(mcols) = &**field(h, "model").ok_or("bad header")? else { return Err("bad header".into()) };
        let mset: fx::Set<Str> = mcols.iter().filter_map(|v| v.as_str().map(Str::from)).collect();
        let shapes: Vec<Vec<Str>> = shapes
            .iter()
            .map(|s| match &**s {
                Val::Arr(ks) => ks.iter().map(|k| k.as_str().map(Str::from).ok_or_else(bad)).collect(),
                _ => Err(bad()),
            })
            .collect::<Res<_>>()?;
        let rec_shape: Vec<usize> = sidx.iter().map(|s| as_int(s).map(|i| i as usize).ok().filter(|&i| i < shapes.len()).ok_or_else(bad)).collect::<Res<_>>()?;
        if spans.iter().map(|s| s.0).sum::<usize>() != rec_shape.len() {
            return Err("bad header (record count)".into());
        }
        let keys: Vec<Vec<Str>> = rec_shape.iter().map(|&s| shapes[s].iter().filter(|k| mset.contains(*k)).cloned().collect()).collect();
        let xcols: fx::Map<Str, Vec<V>> = xcols
            .iter()
            .map(|(k, v)| match &**v {
                Val::Arr(col) => Ok((k.clone(), col.clone())),
                _ => Err(bad()),
            })
            .collect::<Res<_>>()?;
        // where each chunk starts in each xz column
        let mut counts: fx::Map<Str, usize> = fx::Map::default();
        let mut out = Vec::with_capacity(spans.len());
        let mut a = 0;
        for (nr, nbytes, xs) in spans {
            out.push(Chunk { a, b: a + nr, nbytes, xs, xstart: counts.clone() });
            for &s in &rec_shape[a..a + nr] {
                for k in shapes[s].iter().filter(|k| !mset.contains(*k)) {
                    *counts.entry(k.clone()).or_insert(0) += 1;
                }
            }
            a += nr;
        }
        let writer = match lay {
            Some(l) => Writer::Layout(layout::ArrayWriter::new(l)),
            None => Writer::Python(style, tail),
        };
        Ok(Some(State::Records(Box::new(Records { schema, writer, shapes, rec_shape, keys, mset, xcols, chunks: out, next: 0, prime: None }))))
    }
}

impl Writer {
    fn begin(&mut self, out: &mut Vec<u8>) {
        match self {
            Writer::Python(..) => out.push(b'['),
            Writer::Layout(l) => l.begin(out),
        }
    }

    fn item(&mut self, v: &Val, first: bool, out: &mut Vec<u8>) {
        match self {
            Writer::Python(st, _) => json::dump_item(v, st, first, out),
            Writer::Layout(l) => l.item(v, first, out),
        }
    }

    fn end(&mut self, out: &mut Vec<u8>) {
        match self {
            Writer::Python(st, tail) => {
                json::dump_array_end(st, out);
                out.extend_from_slice(tail.as_bytes());
            }
            Writer::Layout(l) => l.end(out),
        }
    }
}

/// a decoded later chunk: serialized already (Python style, which needs no
/// state) or as records for the stateful layout writer
enum Piece {
    Bytes(Vec<u8>),
    Records(Vec<V>),
}

/// record `i` from its modelled values and the xz columns
fn assemble(vals: Vec<V>, shape: &[Str], mset: &fx::Set<Str>, xcols: &fx::Map<Str, Vec<V>>, xpos: &mut fx::Map<Str, usize>) -> Res<V> {
    let bad = || "bad part A".to_string();
    let mut mv = vals.into_iter();
    let mut o = Vec::with_capacity(shape.len());
    for k in shape {
        let v = if mset.contains(k) {
            mv.next().ok_or_else(bad)?
        } else {
            let p = xpos.entry(k.clone()).or_insert(0);
            let v = xcols.get(k).and_then(|c| c.get(*p)).ok_or_else(bad)?.clone();
            *p += 1;
            v
        };
        o.push((k.clone(), v));
    }
    Ok(Arc::new(Val::Obj(o)))
}

impl Records {
    fn pull(&mut self, buf: &mut Vec<u8>, threads: usize, emit: &mut dyn FnMut(Vec<u8>)) -> Res<()> {
        let mut used = 0;
        if self.prime.is_none() {
            // first chunk: a fresh model, records written as they decode
            let nb = self.chunks[0].nbytes;
            if buf.len() < nb {
                return Ok(());
            }
            used = nb;
            let mut m = Model::new(Coder::decoder(buf[..nb].to_vec()), &self.schema);
            m.xz_in = std::mem::take(&mut self.chunks[0].xs);
            let rsig = m.sigs.child(Sigs::ROOT, ELEM);
            let (a, b) = (self.chunks[0].a, self.chunks[0].b);
            let mut xpos = self.chunks[0].xstart.clone();
            let mut out = Vec::new();
            self.writer.begin(&mut out);
            for i in a..b {
                let vals = m.record(rsig, i as u32, &self.keys[i], None, &|_| 0)?;
                let rec = assemble(vals, &self.shapes[self.rec_shape[i]], &self.mset, &self.xcols, &mut xpos)?;
                self.writer.item(&rec, i == 0, &mut out);
                if out.len() >= PIECE {
                    emit(std::mem::take(&mut out));
                }
            }
            if !out.is_empty() {
                emit(out);
            }
            self.prime = Some((m, rsig));
            self.next = 1;
        }
        // later chunks: those whose bytes are all in
        let mut jobs = Vec::new();
        while self.next + jobs.len() < self.chunks.len() {
            let k = self.next + jobs.len();
            let nb = self.chunks[k].nbytes;
            if buf.len() - used < nb {
                break;
            }
            jobs.push((k, buf[used..used + nb].to_vec(), std::mem::take(&mut self.chunks[k].xs)));
            used += nb;
        }
        buf.drain(..used);
        let njobs = jobs.len();
        let Records { writer, shapes, rec_shape, keys, mset, xcols, chunks, prime, .. } = self;
        let (m0, rsig) = prime.as_ref().unwrap();
        let (st, mut lay) = match writer {
            Writer::Python(st, _) => (Some(&*st), None),
            Writer::Layout(l) => (None, Some(l)),
        };
        let mut err = None;
        par::map_in_order(
            jobs,
            threads,
            |(k, data, xs)| -> Res<Piece> {
                let c = &chunks[k];
                let mut m = m0.fork(Some(data));
                m.reset_xz(xs);
                let mut xpos = c.xstart.clone();
                let mut recs = Vec::with_capacity(c.b - c.a);
                for i in c.a..c.b {
                    let vals = m.record(*rsig, i as u32, &keys[i], None, &|_| 0)?;
                    recs.push(assemble(vals, &shapes[rec_shape[i]], mset, xcols, &mut xpos)?);
                }
                Ok(match st {
                    Some(st) => {
                        let mut out = Vec::new();
                        for r in &recs {
                            json::dump_item(r, st, false, &mut out);
                        }
                        Piece::Bytes(out)
                    }
                    None => Piece::Records(recs),
                })
            },
            |r| match (r, &mut err) {
                (_, Some(_)) => {}
                (Err(e), err) => *err = Some(e),
                (Ok(Piece::Bytes(b)), _) => emit(b),
                (Ok(Piece::Records(recs)), _) => {
                    let lay = lay.as_mut().unwrap();
                    let mut out = Vec::new();
                    for r in &recs {
                        lay.item(r, false, &mut out);
                        if out.len() >= PIECE {
                            emit(std::mem::take(&mut out));
                        }
                    }
                    if !out.is_empty() {
                        emit(out);
                    }
                }
            },
        );
        if let Some(e) = err {
            return Err(e);
        }
        self.next += njobs;
        Ok(())
    }
}

impl Tree {
    /// Decode the whole document from all of its chunk bytes.
    fn decode(self, buf: &[u8], threads: usize) -> Res<Vec<u8>> {
        let mut spans = Vec::new();
        let (mut first, mut pos) = (0usize, 0usize);
        for (nr, nb, xz) in self.chunks {
            spans.push((first, first + nr, buf[pos..pos + nb].to_vec(), xz));
            first += nr;
            pos += nb;
        }
        let mut spans = spans.into_iter();
        let (_, _, data, xz) = spans.next().ok_or("bad header")?;
        let rest: Vec<_> = spans.collect();
        let mut m = Model::new(Coder::decoder(data), &self.schema);
        m.reset_xz(xz);
        if !rest.is_empty() {
            m.split = Some(Default::default());
        }
        let t = m.c.t(&[coder::ki(-7)]);
        let tags = coder::Tags::new(&mut m.c.s);
        let k = coder::code_uint(&mut m.c, &tags, t, 0)? as u32;
        let root = m.node(k, None, Sigs::ROOT, &Dims::root())?;
        m.split = None;
        let deferred = std::mem::take(&mut m.deferred);
        if !rest.is_empty() {
            m.release_stores();
        }
        if rest.last().map_or(0, |s| s.1) != deferred.len() {
            return Err("bad header (deferred count)".into());
        }
        let doc = if deferred.is_empty() {
            root
        } else {
            let parts = par_map(rest, threads, |(a, b, data, xz)| -> Res<Vec<V>> {
                let mut f = m.fork(Some(data));
                f.reset_xz(xz);
                deferred.get(a..b).ok_or("bad header")?.iter().map(|d| f.code_deferred(d)).collect()
            });
            let mut rep: fx::Map<usize, V> = fx::Map::default();
            rep.reserve(deferred.len());
            // chunks cover the deferred children in order
            let mut ds = deferred.iter();
            for part in parts {
                for v in part? {
                    rep.insert(Arc::as_ptr(&ds.next().ok_or("bad chunks")?.val) as usize, v);
                }
            }
            fill(&root, &rep).unwrap_or(root)
        };
        let mut out = match &self.lay {
            Some(l) => layout::dumps(&doc, l),
            None => json::dumps_par(&doc, &self.style, threads),
        };
        out.extend_from_slice(self.tail.as_bytes());
        Ok(out)
    }
}
