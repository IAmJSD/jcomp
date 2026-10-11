# jcomp-wasm

jcomp compiled to WebAssembly, running in Web Workers. It has two entry
points, so a page that only decompresses never downloads the compressor:

```ts
import { compress } from 'jcomp-wasm/compress';
import { decompress } from 'jcomp-wasm/decompress';

const packed = await compress(new TextEncoder().encode(json));
const bytes = await decompress(packed); // byte-identical to the input
```

The output is the same file format as the `jcomp` CLI, byte for byte: either
side can decompress what the other compressed.

## Streaming

`decompressStream` takes the compressed bytes as a `ReadableStream` (a fetch
body, say), an async iterable or a `Uint8Array`. It returns a
`ReadableStream` of the original bytes:

```ts
import { decompressStream } from 'jcomp-wasm/decompress';

const res = await fetch('/data/instances.jc');
await decompressStream(res.body!)
  .pipeThrough(new TextDecoderStream())
  .pipeTo(new WritableStream({ write: (text) => render(text) }));
```

For a JSON array of objects, output starts as soon as the header and the
first chunk have arrived. The first chunk is written record by record as it
decodes, in pieces of about 64 KB. Each later chunk is decoded once all of its
bytes are in, in parallel with any others already available, and written in
order. Other documents come out in one piece once fully decoded. More input
is read only as the output is consumed, so a slow reader slows the download
rather than filling memory.

A stream keeps one worker until it ends. Errors, including truncated input,
error the stream; cancelling it replaces the worker.

## Options

```ts
import { Compressor } from 'jcomp-wasm/compress';

const c = new Compressor({ threads: 4 });           // default: navigator.hardwareConcurrency
await c.compress(data, { split: 'auto', chunks: 'auto' });
await c.compress(data, { split: { xz: ['pricing'] }, chunks: 1 });
c.info;                                              // { sharedMemory, threads }
c.terminate();
```

`split` and `chunks` mean the same as the CLI's `--split` and `--chunks`.
More chunks also mean more streaming: with `chunks: 1`, a stream can't start
until all of its input is in (output still arrives record by record).
`Decompressor` has the same constructor, `info` and `terminate`, plus
`decompressStream`. The `compress`, `decompress` and `decompressStream`
functions use a shared instance with default options. Calls never block the
page: every job runs in a worker.

## Parallelism

How the workers are used depends on whether the page is
[cross-origin isolated](https://developer.mozilla.org/en-US/docs/Web/API/Window/crossOriginIsolated),
which it is when served with:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

- **Isolated:** a shared-memory build runs each job on a pool of `threads`
  workers, the same way the CLI uses threads. On a 77 MB file in 8 chunks, 10
  threads compress 1.8× and decompress 2.4× faster than one.
- **Not isolated:** shared memory is unavailable, so up to `threads`
  single-threaded workers run separate jobs side by side. Each job runs at
  single-thread speed.

## Limits

- **Nesting depth.** Browsers cap the native call stack at about 1 MB, and
  jcomp's model recurses once per nesting level. The wasm compressor
  therefore handles documents nested up to 512 levels deep with the
  structural model, and stores deeper ones as plain xz (still exact).
  Decompressing a CLI-made file nested beyond about 1,300 levels fails with a
  stack overflow; the worker is replaced and later jobs work normally.
- **Memory.** WebAssembly memory is limited to 4 GB per instance, which is
  enough for inputs of a few hundred MB.
- **Errors.** Corrupt input rejects with an `Error`. If a worker crashes
  (stack overflow, out of memory), its job rejects and the worker is replaced.

## Building

```
./build.sh       # four wasm variants into wasm/, TypeScript into dist/
npm test         # browser tests in headless Chrome, isolated and not
```

The build needs the `wasm32-unknown-unknown` Rust target, a nightly toolchain
with `rust-src` (the shared-memory build rebuilds std with atomics),
`wasm-bindgen` 0.2.127, and a clang that can target wasm32 for liblzma's C
sources (Homebrew `llvm` on macOS; Apple's clang can't). The tests also need
`cargo build --release` at the repository root, Google Chrome, and `npm
install`.

`crate/` holds the wasm bindings. Its shared-memory build replaces Rust's
wasm allocator, which takes one global lock per call, with per-thread caches
(`crate/src/alloc.rs`); with the stock allocator, 10 threads ran 5× slower
than one.

## License

MIT. See [LICENSE](LICENSE).
