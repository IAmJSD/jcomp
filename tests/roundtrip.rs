//! Round-trip tests: every input must decompress to exactly the original bytes,
//! under every split mode and chunking.
use jcomp::json::{dumps, parse, Style};
use jcomp::{compress, decompress, Opts, Split};

/// documents in compact JSON; each is re-serialized in several Python json.dumps styles
const DOCS: &[&str] = &[
    r#"[]"#,
    r#"{}"#,
    r#"null"#,
    r#"1"#,
    r#""x""#,
    r#"[1, 2, 3]"#,
    r#"{"a": [1, {"b": null}]}"#,
    r#"[{"instance_type": "m5.large", "vCPU": 2, "pricing": {}}]"#,
    r#"[{"pricing": "\u0000"}, {"pricing": "\u0000\u0000x", "k": "\u0000y"}]"#,
    r#"[{"instance_type": "a.b", "vCPU": 0, "pricing": {"us-east-1": {"linux": {"ondemand": "0.10", "x": "1.", "y": "01", "z": "-1", "w": "0.1234567", "v": "0", "u": "12", "t": "0.0300000000", "reserved": {"a": "0.1", "b": 3, "c": 1.5, "d": null, "e": true, "f": [1, "2", {"g": "0.5"}], "h": {}}}, "weird": "0.5", "list": [1, 2]}, "eu-west-1": "flat", "ap": {"linux": {"ondemand": "\u0000", "é": "ünï", "\u0000k": 1}}}}]"#,
    r#"[{"instance_type": "c.x", "vCPU": 4, "pricing": {"r": {"linux": {"ondemand": "1.25", "spot_avg": "0.5", "pct_savings_od": 60}}}}, {"instance_type": "c.y", "vCPU": 8, "pricing": {"r": {"linux": {"ondemand": "2.5", "spot_avg": "1.1", "pct_savings_od": -3}}}}]"#,
    r#"[{"pricing": {"r": {"o": {"n": 1329227995784915872903807060280344576}}}}]"#,
    r#"[{"pricing": {"r": {"o": {"p": "٣", "q": "1234567890123.5", "big": "123456789012345678901234567890123456789.5"}}}}]"#,
    r#"[1, {"pricing": {"r": {"linux": {"ondemand": "0.5"}}}}, {"pricing": {"r": {"linux": {"ondemand": "1"}}}}]"#,
    r#"[{"x": [1.5, -0.0, 1e16, 1e-7, 3.14159, 2.5e300, 0.1, 0.30000000000000004, 1.7328767123287672], "pricing": {"r": {"o": {"fl": [0.25, 1e22], "e": 1e-300}}}}]"#,
    r#"{"a": "😀 é   \u007f \u0001"}"#,
    r#"["\u0000", "\u0000\u0000"]"#,
    r#"[{"k": Infinity}, {"k": NaN}, {"k": -Infinity}]"#,
    r#"[[[[[[[[[[1]]]]]]]]]]"#,
    r#"[{"a": 1, "b": "0.08705500000000001", "c": 15.711840000000002}, {"a": 2, "b": "0.087055", "c": 1e-05}, {"b": "-0.0", "a": 3}]"#,
    r#"[{"m": {"x": {"p": 1.25}, "y": {"p": 2.5}, "z": 3}}, {"m": {"x": {"p": 1.5}, "y": {"p": 3}, "z": {"p": 1}}}]"#,
];

fn styles() -> Vec<(Style, &'static str)> {
    let s = |i: Option<&str>, sep: Option<(&str, &str)>, a| Style::new(i.map(String::from), sep.map(|(x, y)| (x.into(), y.into())), a);
    vec![
        (s(Some(" "), None, true), ""),
        (s(Some(" "), None, true), "\n"),
        (s(None, None, true), ""),
        (s(Some("  "), None, false), ""),
        (s(None, Some((",", ":")), true), "\n"),
        (s(Some("\t"), None, true), ""),
    ]
}

fn modes() -> Vec<Opts> {
    let o = |split, chunks| Opts { split, chunks, threads: 4, verbose: false };
    vec![
        o(Split::Auto, 1),
        o(Split::Model, 1),
        o(Split::Model, 3),
        o(Split::Xz, 1),
        o(Split::Cols(vec!["pricing".into(), "x".into()]), 2),
    ]
}

fn check(raw: &[u8]) {
    for o in modes() {
        let c = compress(raw, &o);
        let d = decompress(&c, 3).unwrap_or_else(|e| panic!("decode error {e} for {:?} with {o:?}", String::from_utf8_lossy(raw)));
        assert!(d == raw, "mismatch for {:?} with {o:?}", String::from_utf8_lossy(raw));
    }
}

#[test]
fn documents_in_all_styles() {
    for doc in DOCS {
        let v = parse(doc, 1000).unwrap_or_else(|e| panic!("{e}: {doc}"));
        for (st, tail) in styles() {
            let mut raw = dumps(&v, &st);
            raw.extend_from_slice(tail.as_bytes());
            check(&raw);
        }
    }
}

#[test]
fn non_reproducible_and_invalid_inputs() {
    let deep: Vec<u8> = [b"[".repeat(5000), b"]".repeat(5000)].concat();
    let lone = br#"["\ud800"]"#.to_vec();
    for raw in [
        &b"{\"a\" : 1}"[..], b"not json", b"", b"[1,\n2]", b"\xff\xfe", b"-0", b"1E5", b"[1.0]", b"\xef\xbb\xbf[1]",
        b"{\"a\":1,\"a\":2}", b"[NaN, Infinity, -Infinity]", b"[1, 2] x", b"[\"a\\u00e9\"]", &deep, &lone,
    ] {
        check(raw);
    }
    check("1".repeat(5000).as_bytes());
}

#[test]
fn arbitrary_formatting() {
    let texts: &[&str] = &[
        "{\n  \"a\" : 1,\n  \"b\" : [ 1, 2, 3 ],\n  \"c\" : { }\n}",
        "{\r\n  \"x\" : \"y\",\r\n  \"z\" : [ {\r\n    \"k\" : 1.0E-5\r\n  } ]\r\n}\r\n",
        "[{ \"t\": \"F\", \"c\": [ [ -122.422003528252475, 37.808480096967251, 0.0 ] ] },\n{ \"t\": \"F\", \"c\": [ ] }\n]\n",
        "  [1,2 ,3\t,  {\"a\":\"\\/x\\u00E9\"}]  ",
        "{\"n\": [-0, 1e+21, 1E5, 0.50, 1.0, -0.0, 123456789012345678901234567890]}",
        "[\n\t{\"a\": \"\\ud83d\\ude00 \u{00e9}\"},\n\t{\"a\": \"plain\"}\n]",
        "{\"a\":{\"b\":{\"c\":[1,{\"d\":[]},[[],{}]]}}}",
    ];
    for t in texts {
        check(t.as_bytes());
    }
    // many records in a non-Python style, enough for the model and chunking
    let mut s = String::from("[\r\n");
    for i in 0..400 {
        s.push_str(&format!("  {{ \"id\" : {i}, \"p\" : {{ \"r1\" : {{ \"od\" : {:.4}, \"x\" : \"{}\" }} }} }}", 0.0125 * (1 + i % 7) as f64, i % 3));
        s.push_str(if i < 399 { ",\r\n" } else { "\r\n" });
    }
    s.push(']');
    check(s.as_bytes());
}

#[test]
fn many_records_across_chunks() {
    // enough records for the model to learn, with map levels and float noise
    let mut s = String::from("[");
    for i in 0..300 {
        if i > 0 {
            s.push_str(", ");
        }
        let base = 0.0125 * (1 + i % 7) as f64;
        s.push_str(&format!(
            r#"{{"id": "t{i}.x", "n": {i}, "p": {{"r1": {{"a": {{"od": "{:.4}", "res": {}}}, "b": {{"od": "{:.4}", "res": {}}}}}, "r2": {{"a": {{"od": "{:.4}", "res": {}}}}}}}}}"#,
            base,
            jcomp::json::py_repr(base * 0.9),
            base + 0.05,
            jcomp::json::py_repr((base + 0.05) * 0.93),
            base * 1.1,
            jcomp::json::py_repr((i as f64 * 37.0) / 8760.0),
        ));
    }
    s.push(']');
    let v = parse(&s, 100).unwrap();
    for (st, tail) in styles().into_iter().take(3) {
        let mut raw = dumps(&v, &st);
        raw.extend_from_slice(tail.as_bytes());
        check(&raw);
    }
}

#[test]
fn corrupt_input_is_an_error_not_a_crash() {
    let raw = br#"[{"a": {"x": {"p": "1.25"}, "y": {"p": "2.5"}}}, {"a": {"x": {"p": "1.5"}, "y": {"p": "3"}}}]"#;
    let c = compress(raw, &Opts::default());
    for cut in [0, 3, 5, c.len() / 2, c.len() - 1] {
        let _ = decompress(&c[..cut], 1);
    }
    let mut bad = c.clone();
    let n = bad.len();
    bad[n - 1] ^= 0x55;
    let _ = decompress(&bad, 1);
}

#[test]
fn keys_built_from_enclosing_keys() {
    // AWS offer-file shape: map keys and ids that extend their parents' keys,
    // with more unique keys than the per-signature predictor group cap
    let mut s = String::from("{\n  \"terms\" : {\n    \"OnDemand\" : {");
    for i in 0..600 {
        let sku = format!("SKU{:05}X{}", i * 7919 % 100000, i % 3);
        let off = format!("{sku}.OFF{}", i % 4);
        let rate = format!("{off}.R{}", i % 2);
        s.push_str(if i > 0 { ",\n" } else { "\n" });
        s.push_str(&format!(
            "      \"{sku}\" : {{\n        \"{off}\" : {{ \"sku\" : \"{sku}\", \"offerTermCode\" : \"OFF{}\", \"priceDimensions\" : {{ \"{rate}\" : {{ \"rateCode\" : \"{rate}\", \"USD\" : \"{:.10}\", \"n\" : {} }} }} }}\n      }}",
            i % 4,
            0.0125 * (i % 13) as f64,
            i % 17
        ));
    }
    s.push_str("\n    }\n  }\n}");
    check(s.as_bytes());
}

#[test]
fn polygons_sharing_vertices() {
    // GeoJSON-style lots on a grid: neighbours share edges (in either direction),
    // enough points for the tuple dictionary's spatial index to switch on
    let pt = |x: i64, y: i64| format!("[ {:.15}, {:.15}, 0.0 ]", -122.4 + x as f64 * 1.37e-4 + (y % 3) as f64 * 1e-9, 37.8 + y as f64 * 1.21e-4);
    let mut s = String::from("{\n\"type\": \"FeatureCollection\",\n\"features\": [\n");
    let mut first = true;
    for y in 0..20 {
        for x in 0..20 {
            if !first {
                s.push_str("\n,\n");
            }
            first = false;
            let ring = if (x + y) % 2 == 0 {
                [pt(x, y), pt(x + 1, y), pt(x + 1, y + 1), pt(x, y + 1), pt(x, y)]
            } else {
                [pt(x, y), pt(x, y + 1), pt(x + 1, y + 1), pt(x + 1, y), pt(x, y)]
            };
            s.push_str(&format!(
                "{{ \"type\": \"Feature\", \"properties\": {{ \"LOT\": \"{x:03}{y:03}\", \"n\": {} }}, \"geometry\": {{ \"type\": \"Polygon\", \"coordinates\": [ [ {} ] ] }} }}",
                x * y,
                ring.join(", ")
            ));
        }
    }
    s.push_str("\n]\n}\n");
    check(s.as_bytes());
}

/// Feed `c` to a streaming decoder `step` bytes at a time. Returns the output and
/// how much of it had arrived while the last `held` input bytes were still out.
fn stream(c: &[u8], step: usize, held: usize, threads: usize) -> Result<(Vec<u8>, usize), String> {
    let mut d = jcomp::Decoder::new(threads);
    let mut out = Vec::new();
    let mut early = 0;
    for (k, piece) in c.chunks(step).enumerate() {
        d.push(piece);
        d.pull(&mut |p| out.extend_from_slice(&p))?;
        if (k + 1) * step <= c.len().saturating_sub(held) {
            early = out.len();
        }
    }
    d.finish(&mut |p| out.extend_from_slice(&p))?;
    Ok((out, early))
}

/// a record array big enough for several chunks and several output pieces
fn record_array(n: usize) -> Vec<u8> {
    let mut s = String::from("[");
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&format!(r#"{{"id": "r{i}", "n": {}, "price": "{:.4}", "note": "{}"}}"#, i % 97, 0.0125 * (i % 13) as f64, "x".repeat(i % 5)));
    }
    s.push(']');
    s.into_bytes()
}

#[test]
fn streaming_decode_matches_in_any_pieces() {
    let v = parse(std::str::from_utf8(&record_array(3000)).unwrap(), 100).unwrap();
    let mut docs: Vec<Vec<u8>> = styles().into_iter().take(3).map(|(st, tail)| [dumps(&v, &st), tail.as_bytes().to_vec()].concat()).collect();
    // no Python style: written through the layout
    docs.push(String::from_utf8(dumps(&v, &styles()[0].0)).unwrap().replace("\": ", "\" : ").into_bytes());
    // not a record array, and not JSON
    docs.push(br#"{"a": [1, 2, {"b": "c"}], "d": null}"#.to_vec());
    docs.push(b"plain text".to_vec());
    for raw in &docs {
        for o in modes() {
            let c = compress(raw, &o);
            for (step, threads) in [(1, 1), (7, 3), (4096, 4), (c.len(), 4)] {
                let (out, _) = stream(&c, step, 0, threads).unwrap_or_else(|e| panic!("{e} with {o:?}, step {step}"));
                assert!(out == *raw, "mismatch with {o:?}, step {step}");
            }
        }
    }
}

#[test]
fn record_arrays_stream_before_the_input_ends() {
    let raw = record_array(20000);
    let o = Opts { split: Split::Model, chunks: 8, threads: 4, verbose: false };
    let c = compress(&raw, &o);
    // with an eighth of the input still to come, most of the output is out
    let (out, early) = stream(&c, 1024, c.len() / 8, 4).unwrap();
    assert!(out == raw);
    assert!(early > raw.len() / 2 && early < raw.len(), "{early} of {} bytes before the input ended", raw.len());
    // and a single chunk streams record by record as it decodes
    let c1 = compress(&raw, &Opts { chunks: 1, ..o.clone() });
    let mut d = jcomp::Decoder::new(1);
    d.push(&c1);
    let mut pieces = 0;
    d.finish(&mut |_| pieces += 1).unwrap();
    assert!(pieces > 4, "{pieces} pieces");
}

#[test]
fn truncated_streams_fail_at_finish() {
    let raw = record_array(500);
    let c = compress(&raw, &Opts { split: Split::Model, chunks: 3, threads: 2, verbose: false });
    for cut in [0, 2, 4, 6, c.len() / 2, c.len() - 1] {
        assert!(stream(&c[..cut], 5, 0, 2).is_err(), "cut at {cut} of {}", c.len());
    }
    assert!(stream(b"not jcomp", 3, 0, 1).is_err());
}

#[test]
fn decompress_to_writes_the_original() {
    let raw = record_array(2000);
    let c = compress(&raw, &Opts::default());
    let mut out = Vec::new();
    jcomp::decompress_to(&c[..], &mut out, 3).unwrap();
    assert!(out == raw);
}
