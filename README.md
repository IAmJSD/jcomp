# jcomp

A lossless, structure-aware JSON compressor written in Rust. Decompression
reproduces the input **byte for byte**.

The model contains no knowledge of any particular dataset. The structure it
exploits is discovered from the document.

```
cargo build --release
target/release/jcomp c [options] in.json out.jc      # compress
target/release/jcomp d [options] in.jc out.json      # decompress
target/release/jcomp t [options] in.json             # compress + decompress + verify
cargo test --release                                 # round-trip test suite

options:
  --split auto|model|xz|xz=COL,COL   how record columns are divided between the model
                                     and xz (default auto: measure each column under
                                     both, keep the cheaper; small inputs also try xz)
  --chunks N                         parallel chunks (see below; default 1 = best ratio)
  -j N                               worker threads (default: all cores)
  -v                                 print the per-column split decisions
```

Decompression streams: for a JSON array of objects, `jcomp d` writes records
as they decode instead of building the whole document first. In Rust,
`jcomp::Decoder` takes input in pieces and hands out output as soon as it is
decoded; `jcomp::decompress_to` wraps it for a reader and a writer.

`js/` is the npm package `jcomp-wasm`, with entry points `jcomp-wasm/compress`
and `jcomp-wasm/decompress`. It runs in Web Workers, in parallel on
cross-origin isolated pages, and produces the same files as the CLI.
Decompression can stream from a fetch body. See `js/README.md`.

## Results

Sizes are in bytes. The same binary was used with no per-file tuning. xz and
brotli were run on each file minified by Python's
`json.dumps(separators=(",", ":"))`; jcomp was run on the original files, and
every file decodes byte-exact. "ratio" is the original size divided by jcomp's,
and "vs best" is the smaller of xz and brotli divided by jcomp.
`python3 scripts/bench.py` downloads every file and regenerates this table.

| file | original | xz -9e | brotli -11 | **jcomp** | saving | ratio | vs best |
|---|---:|---:|---:|---:|---:|---:|---:|
| [AWS RDS price list](https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonRDS/current/index.json) (Jackson style) | 528,334,457 | 10,265,976 | 12,653,985 | **8,322,578** | 98.4% | 63.5× | 1.23× |
| [EC2 instances](https://instances.vantage.sh/instances.json) | 317,788,470 | 12,629,000 | 11,604,790 | **1,308,490** | 99.6% | 242.9× | 8.9× |
| [NVD CVE 2024](https://nvd.nist.gov/feeds/json/cve/2.0/nvdcve-2.0-2024.json.gz) (text heavy) | 292,011,120 | 10,089,312 | 10,835,472 | **9,887,355** | 96.6% | 29.5× | 1.02× |
| [SF city lots](https://raw.githubusercontent.com/zemirco/sf-city-lots-json/master/citylots.json) (GeoJSON) | 189,778,220 | 11,717,152 | 11,842,244 | **9,065,333** | 95.2% | 20.9× | 1.29× |
| [Azure VMs](https://instances.vantage.sh/azure/instances.json) | 40,667,283 | 1,371,808 | 1,350,034 | **455,067** | 98.9% | 89.4× | 3.0× |
| [RDS instances](https://instances.vantage.sh/rds/instances.json) | 40,471,669 | 719,012 | 698,914 | **302,753** | 99.3% | 133.7× | 2.3× |
| [EC2 instances, China](https://instances.vantage.sh/instances-cn.json) | 11,622,568 | 751,688 | 662,719 | **117,048** | 99.0% | 99.3× | 5.7× |
| [GCP instances](https://instances.vantage.sh/gcp/instances.json) | 6,313,689 | 501,860 | 484,627 | **121,700** | 98.1% | 51.9× | 4.0× |
| [OpenSearch instances](https://instances.vantage.sh/opensearch/instances.json) | 2,748,411 | 130,072 | 125,847 | **43,717** | 98.4% | 62.9× | 2.9× |
| [ElastiCache nodes](https://instances.vantage.sh/cache/instances.json) | 2,511,690 | 111,640 | 106,542 | **34,305** | 98.6% | 73.2× | 3.1× |
| [RDS instances, China](https://instances.vantage.sh/rds/instances-cn.json) | 1,395,235 | 46,928 | 44,875 | **20,453** | 98.5% | 68.2× | 2.2× |
| [ElastiCache nodes, China](https://instances.vantage.sh/cache/instances-cn.json) | 202,889 | 14,584 | 13,768 | **5,683** | 97.2% | 35.7× | 2.4× |
| [OpenSearch instances, China](https://instances.vantage.sh/opensearch/instances-cn.json) | 178,907 | 11,840 | 11,251 | **4,670** | 97.4% | 38.3× | 2.4× |
| [Redshift nodes](https://instances.vantage.sh/redshift/instances.json) | 124,567 | 8,876 | 8,332 | **4,638** | 96.3% | 26.9× | 1.8× |
| [Redshift nodes, China](https://instances.vantage.sh/redshift/instances-cn.json) | 13,020 | 1,416 | 1,216 | 1,396 | 89.3% | 9.3× | 0.87× |

The Redshift China file is 13 KB with 10 records. That is too little data to
learn from, so jcomp falls back to xz on the original (pretty-printed) bytes.

The price list, NVD and city lots files each have a root object rather than a
list of records, so they are coded in *tree mode* and split into parallel
chunks (see below).

Timings were measured on a machine under heavy background load (load average
4–10); an idle machine is faster. Full EC2 (single chunk) takes about 22 s to
compress and 15 s to decompress. Azure and RDS (40 MB each) take about 3 s to
compress, and the smaller instance catalogs take 0.1–1.6 s. The price list,
NVD and city lots decode in 5.4–5.9 s, 2.1–2.8 s and 3.9–5.2 s (9 s, 3.5 s and
12 s on one thread), with peak decode memory of 1.6–2.9 GB.

### Parallelism (`--chunks`)

One adaptive model is inherently sequential. Records are therefore split into
chunks. The first ("prime", about 1/8 of the work) is coded by a fresh model.
Each later chunk is coded by a copy of the model as trained on the prime, so
those chunks decode in parallel. Chunk boundaries balance work (leaf values per
record), not record counts, so the parallel chunks finish together.

The default, `--chunks auto`, encodes with 1, 4 and 8 chunks. It keeps the most
parallel layout whose size is within 0.5% of the smallest. Because the forks
inherit the trained model, including its mixer, they usually cost almost
nothing. EC2 and RDS come out *smaller* with 8 chunks, and auto picks 8 for
both, and 4 or 8 for Azure. Small files keep a single chunk. `--chunks N`
forces a layout.

| EC2 instances | size | decompress |
|---|---:|---:|
| 1 chunk | 1,311,580 | ~10 s |
| auto (8 chunks) | 1,308,490 | ~4 s |

### Tree documents

A document that isn't a list of records is split in a similar way. The
encoder looks for large collections inside it. A collection is a container
with at least 64 children, nearly all of them containers, none holding more
than a quarter of its weight: `features` in GeoJSON, `products` and
`terms.OnDemand` in an AWS price list. A *prime* pass codes the rest of the
document plus the first ~1/16 of each collection. The remaining children are
spread over parallel chunks, each coded by a copy of the model as the prime
left it. A flag bit, sent only for containers with 64 or more children, tells
the decoder which ones were split and how far. Documents with over 1M leaves
get one chunk per 512K leaves, up to 16. Chunk boundaries balance estimated
decode work rather than item counts: each leaf counts 1, each container 4, and
a short numeric array already seen earlier (a shared polygon vertex) counts 1,
because the decoder gets it as a cheap reference. On the price list, NVD and city
lots files, splitting changed size by −0.2% (AWS) to +0.7% (NVD).

### MessagePack?

I measured MessagePack as an intermediate format. Under xz it was larger than
minified JSON on 8 of the 12 instance catalogs (Azure +12%, OpenSearch +14%). Binary
doubles hide the short-decimal regularity that the text form shows. Only RDS
improved (−7%). The structural model already parses JSON into typed values and
models numbers directly, which goes further than MessagePack would. Input and
output are always JSON.

## How it works

The input is parsed exactly as Python's `json.loads` would parse it, and
the serialization style is identified: Python `json.dumps` indent, separators
and `ensure_ascii`, plus a trailing newline. Number tokens keep their source
text. If no Python style matches (Jackson, Go, CRLF, hand-edited, ...), a
*layout* is learned instead. It records the most common whitespace for each
gap kind × nesting depth × object depth, plus a list of exceptions, and keeps
verbatim any strings whose escaping differs from the canonical one. Input that
still doesn't reproduce byte-exactly (invalid JSON, duplicate keys, ...) is
compressed as raw xz.

**Analysis (encoder only, sent in the header).**

- **Maps vs records.** An object whose children are (≥90%) containers and
  which has more than one key is a *map*: its keys are data, like regions or
  operating systems. Other objects are *records*, whose keys are schema. Every
  value then has a *signature* (its path, with map keys and array indices
  wildcarded) and *coordinates* (the array indices and map keys along its path).
- **Rational grids.** For each numeric signature, the analysis looks for a
  denominator such that values are `k/den`, or short decimals divided by a
  constant. That covers exact doubles (found
  with continued fractions, merged by LCM) and decimals rounded from `k/den`
  (found with the simplest fraction in each rounding interval, or a direct
  search). This finds per-hour reservation prices such as `/8760`, `/26280`
  and `cents/876000` without being told.

**Split.** When the root is a list of objects, each top-level field (column)
goes either to the model or to xz. In auto mode the encoder measures each
column's model cost on a prefix of the records, compares it with xz on that
column, and keeps the cheaper. Record shapes and the xz columns form part A,
which is compressed with xz together with the header.

**Model.** A binary adaptive range coder with structured contexts:

- **Containers.** Object key lists are predicted from the same object at
  neighbouring coordinates, then coded as "same as candidate k" or as an edit.
  Child kinds and array lengths are predicted the same way.
- **Numbers.** Numbers are exact decimals, or doubles kept bit-exact. A double
  is coded as a short decimal plus an offset in ULPs (*near-decimal*) when it
  came from float arithmetic on decimals, e.g. `0.08705500000000001`,
  `15.711840000000002`. It is coded on a rational grid when it is `k/den`, or as a
  short decimal divided by a constant plus a few ULPs (e.g. a monthly price
  / 730, or `(upfront + hourly × 8760) / 8760`), and otherwise as a ULP
  distance from the prediction. Decimal strings keep their
  exact text, including trailing zeros.
- **Predictions.** Candidate predictions come from values at neighbouring
  coordinates:
  - the previous array element
  - the previous sibling map key, and the most common one
  - the first and previous fields of the same object

  These combine as copies, additive and ratio parallelograms over any two
  coordinates, and differences to the first field that are shared between
  sibling map entries. Coding is two-stage. When the column's two best
  predictors agree, and that shortcut has recently been right ≥95% of the time
  in this column, one bit confirms the value. Otherwise every candidate is
  computed and ordered by how many predictors agree and by each predictor's hit
  history, and the coder signals which one is exact. A miss is coded as a
  residual against the predictor with the lowest recent error. Predictors that
  are never right in a column (neither exact nor the best residual base) are
  skipped, and periodically re-checked.
- **Strings.** Strings are coded as copies from neighbours, then a
  move-to-front vocabulary per signature, then an order-1 literal model.
  Strings and map keys may also be coded as *an enclosing map key plus a
  remainder*. Ids are often built from their parents' keys: in the AWS price
  list, `"SKU.OFFER"` sits under `"SKU"`, `"SKU.OFFER.RATE"` sits under that,
  and `"sku"` and `"rateCode"` repeat them. In auto mode, signatures whose
  strings xz codes more cheaply (prose such as CVE descriptions) go to an xz
  side stream in part A instead.
- **Tuple sequences.** In arrays of short numeric arrays (coordinates,
  points), each distinct tuple goes into a dictionary with a spatial grid over
  its first two components. A tuple can then be coded as a reference. The
  candidates are the tuples that followed or preceded the previous tuple
  before (a retraced edge, in either direction), then known tuples nearest the
  last one. GeoJSON polygons that share vertices with their neighbours cost a
  few bits per shared vertex instead of ~70.
- **Memory.** Per-value stores are flushed at a fixed count, and predictor
  statistics per map key are capped. Maps keyed by unique ids would otherwise
  grow without bound, and both sides flush at the same points.
- **Context mixing.** The most important bits are coded from several contexts
  at once: the specific one (column and map key), one shared by the signature,
  and one shared globally. They are combined by a small logistic mixer
  (stretch, weighted sum, squash, PAQ-style). It uses integer-only tables and
  selects its weights by bit type and by how mature the specific context is.
  Context probabilities adapt quickly (rate 1/16). Context state is found by
  hashing the context tuple into a 2^24-slot table rather than through an
  interning map. A collision only shares statistics, identically in the
  encoder and decoder.

Every coding decision uses integer or basic IEEE arithmetic (no libm), so
files decode identically on any platform. No hash-only identity is used, so a
hash collision can never desynchronize the decoder.

## Format

`JCv1 | varint len(A) | A | B…`

- `A` is xz (raw LZMA2) of the header JSON, a newline, and the part-A payload.
- `B` is one range-coded stream per chunk.

Inputs that are not UTF-8, that `json.dumps` can't reproduce, or that use lone
surrogates are stored as raw xz. Corrupt input produces an error, not a crash.

## Layout

```
src/lib.rs      container, split decisions, chunking; public compress/decompress API
src/main.rs     command-line interface
src/model.rs    schema-free model: signatures, coordinates, predictors, coding
src/analyze.rs  map detection and rational-grid discovery
src/num.rs      exact decimals, near-decimals, grids, deterministic arithmetic
src/coder.rs    range coder, context interning, integer coders
src/json.rs     Python-compatible JSON parser/serializer
src/par.rs      parallel helpers: scoped threads natively, rayon on wasm
src/stream.rs   streaming decoder (input and output in pieces)
src/sys.rs      platform shims for wasm
tests/          round trips over edge cases, all split modes and chunkings
scripts/bench.py  downloads the files above and regenerates the results table
js/             TypeScript package (WebAssembly in Web Workers); see js/README.md
```

## License

MIT. See [LICENSE](LICENSE).
