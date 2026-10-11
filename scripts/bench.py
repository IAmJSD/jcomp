#!/usr/bin/env python3
"""Download the benchmark files and regenerate the results table in README.md.

    python3 scripts/bench.py [--dir DIR] [--print]

Each file is compressed with `jcomp t` (which also checks the round trip is
byte-exact), and its minified form (Python `json.dumps` with compact
separators) with `xz -9e` and `brotli -q 11`. Downloads go to DIR (default: a
temporary directory, removed afterwards); files already in DIR are reused.
--print shows the table instead of writing it into README.md.
"""
import argparse, gzip, json, os, re, shutil, subprocess, sys, tempfile, urllib.request
from concurrent.futures import ThreadPoolExecutor

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
VANTAGE = "https://instances.vantage.sh/"

# (label, suffix after the link, url)
FILES = [
    ("AWS RDS price list", " (Jackson style)", "https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonRDS/current/index.json"),
    ("EC2 instances", "", VANTAGE + "instances.json"),
    ("NVD CVE 2024", " (text heavy)", "https://nvd.nist.gov/feeds/json/cve/2.0/nvdcve-2.0-2024.json.gz"),
    ("SF city lots", " (GeoJSON)", "https://raw.githubusercontent.com/zemirco/sf-city-lots-json/master/citylots.json"),
    ("Azure VMs", "", VANTAGE + "azure/instances.json"),
    ("RDS instances", "", VANTAGE + "rds/instances.json"),
    ("EC2 instances, China", "", VANTAGE + "instances-cn.json"),
    ("GCP instances", "", VANTAGE + "gcp/instances.json"),
    ("OpenSearch instances", "", VANTAGE + "opensearch/instances.json"),
    ("ElastiCache nodes", "", VANTAGE + "cache/instances.json"),
    ("RDS instances, China", "", VANTAGE + "rds/instances-cn.json"),
    ("ElastiCache nodes, China", "", VANTAGE + "cache/instances-cn.json"),
    ("OpenSearch instances, China", "", VANTAGE + "opensearch/instances-cn.json"),
    ("Redshift nodes", "", VANTAGE + "redshift/instances.json"),
    ("Redshift nodes, China", "", VANTAGE + "redshift/instances-cn.json"),
]

HEADER = "| file | original | xz -9e | brotli -11 | **jcomp** | saving | ratio | vs best |"


def local_name(url):
    name = re.sub(r"^https?://", "", url).replace("/", "_")
    return name[:-3] if name.endswith(".gz") else name


def download(url, path):
    if os.path.exists(path):
        return
    print(f"downloading {url}", file=sys.stderr)
    req = urllib.request.Request(url, headers={"User-Agent": "jcomp-bench"})
    with urllib.request.urlopen(req) as r, open(path + ".part", "wb") as f:
        shutil.copyfileobj(gzip.GzipFile(fileobj=r) if url.endswith(".gz") else r, f, 1 << 20)
    os.rename(path + ".part", path)


def compressed_size(cmd, path):
    with open(path, "rb") as f:
        out = subprocess.run(cmd, stdin=f, stdout=subprocess.PIPE, check=True).stdout
    return len(out)


def jcomp_size(binary, path):
    out = subprocess.run([binary, "t", path], stdout=subprocess.PIPE, text=True, check=True).stdout
    m = re.search(r"original (\d+)\s+compressed (\d+).*\b(EXACT|MISMATCH)\b", out)
    if not m or m[3] != "EXACT":
        sys.exit(f"jcomp round trip failed on {path}: {out.strip()}")
    return int(m[2])


def row(label, suffix, url, orig, xz, br, jc):
    best = min(xz, br)
    vs = best / jc
    j = f"**{jc:,}**" if jc < best else f"{jc:,}"
    return (f"| [{label}]({url}){suffix} | {orig:,} | {xz:,} | {br:,} | {j} | "
            f"{100 * (1 - jc / orig):.1f}% | {orig / jc:.1f}× | {vs:.{2 if vs < 1.5 else 1}f}× |")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dir", help="download directory to use and keep")
    ap.add_argument("--print", action="store_true", help="print the table instead of updating README.md")
    args = ap.parse_args()

    for tool in ("xz", "brotli", "cargo"):
        if not shutil.which(tool):
            sys.exit(f"{tool} not found on PATH")
    subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=ROOT, check=True)
    binary = os.path.join(ROOT, "target", "release", "jcomp")

    work = args.dir or tempfile.mkdtemp(prefix="jcomp-bench-")
    os.makedirs(work, exist_ok=True)
    try:
        paths = [os.path.join(work, local_name(url)) for _, _, url in FILES]
        for (_, _, url), path in zip(FILES, paths):
            download(url, path)

        # Minify one file at a time (the large ones take several GB to parse)
        # and let xz and brotli run in the background meanwhile.
        pool = ThreadPoolExecutor(max_workers=os.cpu_count())
        baseline = []
        for path in paths:
            mini = path + ".min"
            with open(path, encoding="utf-8") as f:
                doc = json.load(f)
            with open(mini, "w", encoding="utf-8") as f:
                json.dump(doc, f, separators=(",", ":"))
            del doc
            baseline.append((pool.submit(compressed_size, ["xz", "-9e", "-c"], mini),
                             pool.submit(compressed_size, ["brotli", "-q", "11", "-c"], mini)))

        jc = []
        for path in paths:
            print(f"jcomp {os.path.basename(path)}", file=sys.stderr)
            jc.append(jcomp_size(binary, path))

        rows = []
        for (label, suffix, url), path, (xz, br), j in zip(FILES, paths, baseline, jc):
            rows.append(row(label, suffix, url, os.path.getsize(path), xz.result(), br.result(), j))
        pool.shutdown()
    finally:
        if not args.dir:
            shutil.rmtree(work, ignore_errors=True)

    table = "\n".join([HEADER, "|---|---:|---:|---:|---:|---:|---:|---:|"] + rows)
    if args.print:
        print(table)
        return
    readme = os.path.join(ROOT, "README.md")
    text = open(readme, encoding="utf-8").read()
    start = text.index(HEADER)
    end = text.index("\n\n", start)
    with open(readme, "w", encoding="utf-8") as f:
        f.write(text[:start] + table + text[end:])
    print("updated README.md", file=sys.stderr)


if __name__ == "__main__":
    main()
