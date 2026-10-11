//! jcomp command-line interface.
use jcomp::{compress, decompress, decompress_to, Opts, Split};
use std::time::Instant;

type Res<T> = Result<T, String>;

// ------------------------------------------------------------------ CLI
const USAGE: &str = "usage:
  jcomp c [options] IN.json OUT.jc     compress
  jcomp d [options] IN.jc OUT.json     decompress
  jcomp t [options] IN.json            compress + decompress + verify byte-exact

options:
  --split auto|model|xz|xz=COL,COL     how record columns are divided between the
                                       structural model and xz (default auto:
                                       measure both per column, keep the cheaper)
  --chunks auto|N                      code records in N chunks: the first trains the
                                       model, the rest continue from copies of it and
                                       decode in parallel. auto (default) tries 1, 4
                                       and 8 and keeps the most parallel layout within
                                       0.5% of the smallest
  -j N                                 worker threads (default: all cores)
  -v                                   report decisions and sizes";

fn opts(args: &[String]) -> Res<(Opts, Vec<String>)> {
    let mut o = Opts::default();
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--split" => {
                let v = val("--split")?;
                o.split = match v.as_str() {
                    "auto" => Split::Auto,
                    "model" => Split::Model,
                    "xz" => Split::Xz,
                    s if s.starts_with("xz=") => Split::Cols(s[3..].split(',').filter(|x| !x.is_empty()).map(String::from).collect()),
                    _ => return Err(format!("bad --split {v}")),
                }
            }
            "--chunks" => {
                let v = val("--chunks")?;
                o.chunks = if v == "auto" { 0 } else { v.parse::<usize>().map_err(|_| "bad --chunks")?.max(1) };
            }
            "-j" => o.threads = val("-j")?.parse().map_err(|_| "bad -j")?,
            "-v" => o.verbose = true,
            _ => rest.push(a.clone()),
        }
    }
    o.threads = o.threads.max(1);
    Ok((o, rest))
}

fn run() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (o, a) = opts(&args)?;
    let read = |p: &str| std::fs::read(p).map_err(|e| format!("{p}: {e}"));
    let write = |p: &str, d: &[u8]| std::fs::write(p, d).map_err(|e| format!("{p}: {e}"));
    match (a.first().map(|s| s.as_str()), a.len()) {
        (Some("c"), 3) => write(&a[2], &compress(&read(&a[1])?, &o)),
        (Some("d"), 3) => {
            // written as it decodes; a failed decode leaves no partial file
            let r = std::fs::File::open(&a[1]).map_err(|e| format!("{}: {e}", a[1]))?;
            let w = std::fs::File::create(&a[2]).map_err(|e| format!("{}: {e}", a[2]))?;
            decompress_to(r, std::io::BufWriter::new(w), o.threads).inspect_err(|_| {
                let _ = std::fs::remove_file(&a[2]);
            })
        }
        (Some("t"), 2) => {
            let raw = read(&a[1])?;
            let t = Instant::now();
            let blob = compress(&raw, &o);
            let tc = t.elapsed().as_secs_f64();
            let t = Instant::now();
            let back = decompress(&blob, o.threads)?;
            let td = t.elapsed().as_secs_f64();
            println!(
                "{}  original {}  compressed {}  ratio {:.1}x  c={:.1}s d={:.1}s  {}",
                a[1],
                raw.len(),
                blob.len(),
                raw.len() as f64 / blob.len() as f64,
                tc,
                td,
                if back == raw { "EXACT" } else { "MISMATCH" }
            );
            if back == raw { Ok(()) } else { Err("round trip mismatch".into()) }
        }
        _ => Err(USAGE.into()),
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
