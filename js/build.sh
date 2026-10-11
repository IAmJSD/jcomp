#!/usr/bin/env bash
# Build the four wasm variants ({compress, decompress} x {single, threads}) into
# js/wasm/, then the TypeScript into js/dist/.
#
# Needs: rustup with the wasm32-unknown-unknown target, a nightly toolchain with
# rust-src (the threaded variant rebuilds std with atomics), wasm-bindgen
# matching crate/Cargo.toml, and a clang that targets wasm32 (Homebrew llvm on
# macOS; Apple's clang does not) for liblzma's C sources.
set -euo pipefail
cd "$(dirname "$0")"

# Wasm stack of the thread that calls into jcomp. The model uses about 440
# bytes per JSON nesting level here; the engine's own call stack (about 1 MB,
# roughly 1300 levels) runs out first, so 2 MB is never the limit.
STACK=${JCOMP_WASM_STACK:-$((2 << 20))}

if [[ -z "${CC_wasm32_unknown_unknown:-}" ]]; then
  for cc in "$(brew --prefix llvm 2>/dev/null)/bin/clang" clang; do
    if "$cc" --print-targets 2>/dev/null | grep -q wasm32; then
      export CC_wasm32_unknown_unknown="$cc"
      export AR_wasm32_unknown_unknown="$(dirname "$(command -v "$cc")")/llvm-ar"
      break
    fi
  done
fi
[[ -n "${CC_wasm32_unknown_unknown:-}" ]] || { echo "no clang with a wasm32 target found; set CC_wasm32_unknown_unknown" >&2; exit 1; }

rm -rf wasm
for part in compress decompress; do
  echo "== $part (single-threaded)"
  RUSTFLAGS="-C link-arg=-zstack-size=$STACK" \
    cargo build --manifest-path crate/Cargo.toml --release --target wasm32-unknown-unknown \
      --features "$part" --target-dir "crate/target/$part"
  wasm-bindgen --target web --out-dir "wasm/$part" \
    "crate/target/$part/wasm32-unknown-unknown/release/jcomp_wasm.wasm"

  echo "== $part (threads)"
  CFLAGS_wasm32_unknown_unknown="-matomics -mbulk-memory" \
  RUSTFLAGS="-C target-feature=+atomics,+bulk-memory -C link-arg=-zstack-size=$STACK \
    -C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--max-memory=4294967296 \
    -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size \
    -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base" \
    cargo +nightly build --manifest-path crate/Cargo.toml --release --target wasm32-unknown-unknown \
      --features "$part,threads" --target-dir "crate/target/$part-threads" -Z build-std=panic_abort,std
  wasm-bindgen --target web --out-dir "wasm/$part-threads" \
    "crate/target/$part-threads/wasm32-unknown-unknown/release/jcomp_wasm.wasm"
  # wasm-bindgen-rayon starts each pool worker with init's deprecated positional
  # form (a console warning per worker) and the default thread stack; pass the
  # options object instead, with the same stack as the calling thread
  helper=$(echo wasm/$part-threads/snippets/wasm-bindgen-rayon-*/src/workerHelpers.no-bundler.js)
  grep -q 'pkg.default(data.module, data.memory)' "$helper"
  sed -i.bak "s/pkg.default(data.module, data.memory)/pkg.default({ module_or_path: data.module, memory: data.memory, thread_stack_size: $STACK })/" "$helper"
  rm "$helper.bak"
done

if command -v wasm-opt >/dev/null; then
  for f in wasm/*/jcomp_wasm_bg.wasm; do
    wasm-opt -O3 --enable-threads --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext "$f" -o "$f"
  done
fi

npx tsc -p .
