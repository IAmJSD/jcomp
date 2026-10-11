//! Encoder-side analysis of a document, producing the `Schema` shipped in the
//! header: which objects are maps, and which numeric signatures live on a
//! rational grid (values k/den).
use crate::coder::Coder;
use crate::fx::{Map, Set};
use crate::json::Val;
use crate::model::{leaf_num, Schema, Sigs, ELEM, WILD};
use crate::num::{gcd, near, rational_den, Grid, Num};
use crate::par::par_map;

const SAMPLE: usize = 2048;
const MAX_DEN: i128 = 10_000_000;

pub fn analyze(doc: &Val, records: bool, threads: usize) -> Schema {
    let mut c = Coder::encoder();
    let mut sigs = Sigs::new();
    let rec_sig = records.then(|| sigs.child(Sigs::ROOT, ELEM));
    let mut maps = Vec::new();
    let mut samples: Map<u32, Vec<Num>> = Map::default();
    let mut order: Vec<u32> = Vec::new();
    let mut level: Vec<(&Val, u32)> = vec![(doc, Sigs::ROOT)];
    while !level.is_empty() {
        // an object signature is a map when (nearly) all its children are
        // containers and it has more than one distinct key
        let mut stat: Map<u32, ([usize; 2], Set<u32>)> = Map::default();
        for &(v, s) in &level {
            if let Val::Obj(o) = v {
                let e = stat.entry(s).or_insert_with(|| ([0, 0], Set::default()));
                for (k, x) in o {
                    e.0[matches!(**x, Val::Obj(_) | Val::Arr(_)) as usize] += 1;
                    if e.1.len() < 2 {
                        e.1.insert(c.s.id(k));
                    }
                }
            }
        }
        let mut ismap: Set<u32> = Set::default();
        let mut decided: Vec<u32> = stat.iter().filter(|(s, (n, ks))| n[1] >= 9 * n[0] && ks.len() >= 2 && Some(**s) != rec_sig).map(|(s, _)| *s).collect();
        decided.sort_unstable();
        for s in decided {
            ismap.insert(s);
            maps.push(sigs.path(s, &c.s.names));
        }
        let mut next = Vec::new();
        for &(v, s) in &level {
            match v {
                Val::Obj(o) => {
                    let m = ismap.contains(&s);
                    for (k, x) in o {
                        let step = if m { WILD } else { c.s.id(k) };
                        next.push((&**x, sigs.child(s, step)));
                    }
                }
                Val::Arr(a) => {
                    let cs = sigs.child(s, ELEM);
                    next.extend(a.iter().map(|x| (&**x, cs)));
                }
                leaf => {
                    if let Some((n, _, _)) = leaf_num(leaf) {
                        let e = samples.entry(s).or_insert_with(|| {
                            order.push(s);
                            Vec::new()
                        });
                        if e.len() < SAMPLE {
                            e.push(n);
                        }
                    }
                }
            }
        }
        level = next;
    }
    let jobs: Vec<(u32, Vec<Num>)> = order.iter().map(|s| (*s, samples.remove(s).unwrap())).collect();
    let found = par_map(jobs, threads, |(s, sample)| (s, find_grids(&sample)));
    let dens = found.into_iter().filter(|(_, g)| !g.is_empty()).map(|(s, g)| (sigs.path(s, &c.s.names), g)).collect();
    Schema { maps, dens, xz: Vec::new() }
}

/// 2^a * 5^b: such a grid is just a decimal grid
fn decimal_like(mut d: i128) -> bool {
    for p in [2, 5] {
        while d > 1 && d % p == 0 {
            d /= p;
        }
    }
    d == 1
}

/// simplest fraction p/q (smallest q) in the closed interval [ln/ld, hn/hd], 0 <= lo <= hi
fn simplest((ln, ld): (i128, i128), (hn, hd): (i128, i128)) -> Option<(i128, i128)> {
    if ln < 0 || ld <= 0 || hd <= 0 {
        return None;
    }
    let fl = ln / ld;
    if fl * ld == ln {
        return Some((fl, 1));
    }
    if (fl + 1).checked_mul(hd)? <= hn {
        return Some((fl + 1, 1));
    }
    // same integer part: recurse on the reciprocals of the fractional parts
    let (a, b) = simplest((hd, hn - fl * hd), (ld, ln - fl * ld))?;
    Some((fl.checked_mul(a)?.checked_add(b)?, a))
}

fn lcm(a: i128, b: i128) -> Option<i128> {
    (a / gcd(a, b)).checked_mul(b)
}

/// rational grids that cover most of a signature's values
fn find_grids(sample: &[Num]) -> Vec<Grid> {
    let mut out = Vec::new();
    // exact doubles k/den: denominators by continued fractions, merged by LCM
    let fs: Vec<Num> = sample.iter().copied().filter(|n| matches!(n, Num::F(_))).take(512).collect();
    if fs.len() >= 8 {
        let mut freq: Map<i128, usize> = Map::default();
        for n in &fs {
            if let Some(d) = rational_den(n.f64(), MAX_DEN) {
                *freq.entry(d).or_insert(0) += 1;
            }
        }
        let mut ds: Vec<(usize, i128)> = freq.into_iter().map(|(d, f)| (f, d)).collect();
        ds.sort_unstable_by(|a, b| b.cmp(a));
        let mut l = 1i128;
        for &(_, d) in &ds {
            if let Some(x) = lcm(l, d).filter(|&x| x <= MAX_DEN) {
                l = x;
            }
        }
        let mut best: Option<(usize, i128)> = None;
        for den in std::iter::once(l).chain(ds.iter().take(3).map(|x| x.1)) {
            if decimal_like(den) {
                continue;
            }
            let g = Grid { den, prec: None, scaled: false };
            let cov = fs.iter().filter(|&&n| g.holds(n).is_some()).count();
            if best.is_none_or(|b| cov > b.0) {
                best = Some((cov, den));
            }
        }
        if let Some((cov, den)) = best {
            if cov * 2 >= fs.len() {
                out.push(Grid { den, prec: None, scaled: false });
            }
        }
    }
    // doubles that are a short decimal divided by a constant (monthly / 730,
    // (upfront + hourly * 8760) / 8760): multiplying by den gives a near-decimal
    let rest: Vec<f64> = fs.iter().map(|n| n.f64()).filter(|&x| near(x).is_none() && out.iter().all(|g: &Grid| g.holds(Num::F(x.to_bits())).is_none())).take(256).collect();
    if rest.len() >= 8 {
        // cheap filter: x * den * 1e8 must be (close to) an integer
        let integral = |x: f64, den: i128| {
            let z = x * den as f64 * 1e8;
            z.is_finite() && z.abs() < 1e15 && (z - z.round()).abs() < 0.05
        };
        for den in 2..=20_000i128 {
            if decimal_like(den) || !rest[..8].iter().all(|&x| integral(x, den)) {
                continue;
            }
            let ok: Vec<(Num, i64)> = rest.iter().filter_map(|&x| near(x * den as f64)).collect();
            if ok.len() * 10 >= rest.len() * 9 {
                let p = ok.iter().map(|(n, _)| if let Num::D(_, e) = n { (*e).max(0) } else { 0 }).max().unwrap_or(0).min(15) as u32;
                let g = Grid { den, prec: Some(p), scaled: true };
                if rest.iter().filter(|&&x| g.holds_near(x).is_some()).count() * 10 >= rest.len() * 9 {
                    out.push(g);
                    break;
                }
            }
        }
    }
    // decimals rounded from k/den: the simplest fraction inside each value's
    // rounding interval gives a denominator; merge the common ones by LCM
    let ds: Vec<Num> = sample.iter().copied().filter(|n| matches!(n, Num::D(_, e) if *e >= 4)).take(512).collect();
    if ds.len() >= 8 {
        let p = ds.iter().map(|n| if let Num::D(_, e) = n { *e } else { 0 }).max().unwrap().min(15) as u32;
        let scale = 10i128.pow(p);
        let mut freq: Map<i128, usize> = Map::default();
        for n in &ds {
            let Num::D(m, e) = *n else { continue };
            let v = m.abs() * 10i128.pow(p - e as u32);
            // value*10^p in [v - 1/2, v + 1/2]
            if let Some((_, q)) = simplest((2 * v - 1, 2 * scale), (2 * v + 1, 2 * scale)) {
                if q > 1 && q <= MAX_DEN && !decimal_like(q) {
                    *freq.entry(q).or_insert(0) += 1;
                }
            }
        }
        let mut cands: Vec<(usize, i128)> = freq.into_iter().map(|(d, f)| (f, d)).collect();
        cands.sort_unstable_by(|a, b| b.cmp(a));
        let mut l = 1i128;
        for &(_, d) in &cands {
            if let Some(x) = lcm(l, d).filter(|&x| x <= MAX_DEN) {
                l = x;
            }
        }
        // a grid only helps values whose own decimal step is finer than 1/den
        let fine = |n: &Num, den: i128| matches!(n, Num::D(_, e) if 10i128.pow((*e).min(18) as u32) > 2 * den);
        // coverage of a candidate grid over the values it can coarsen
        let cover = |den: i128| -> Option<usize> {
            if den <= 1 || decimal_like(den) {
                return None;
            }
            let g = Grid { den, prec: Some(p), scaled: false };
            // the first 8 values the grid could coarsen must all be on it (cheap
            // rejection: almost every candidate fails on the first one)
            let mut it = ds.iter().filter(|n| fine(n, den));
            for _ in 0..8 {
                if g.holds(*it.next()?).is_none() {
                    return None;
                }
            }
            let (mut nf, mut cov) = (8usize, 8usize);
            for n in it {
                nf += 1;
                cov += g.holds(*n).is_some() as usize;
            }
            (cov * 5 >= nf * 4).then_some(cov)
        };
        let mut best: Option<(usize, i128)> = None;
        let mut consider = |den: i128| {
            if let Some(cov) = cover(den) {
                if best.is_none_or(|b| cov > b.0 || (cov == b.0 && den < b.1)) {
                    best = Some((cov, den));
                }
            }
        };
        for den in std::iter::once(l).chain(cands.iter().take(3).map(|x| x.1)) {
            consider(den);
        }
        // when values carry few digits the interval method is noisy: also search
        // small denominators directly (cheap: almost all fail on the first value)
        for den in 2..=200_000i128.min(scale / 4) {
            if cover(den).is_some() {
                consider(den);
                break;
            }
        }
        if let Some((_, den)) = best {
            out.push(Grid { den, prec: Some(p), scaled: false });
        }
    }
    out
}
