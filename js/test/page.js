// Runs in the browser; test/run.mjs calls window.runTests and checks the report.
import { Compressor } from '../dist/compress.js';
import { Decompressor } from '../dist/decompress.js';

const fetchBytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
const equal = (a, b) => a.length === b.length && a.every((x, i) => x === b[i]);

window.runTests = async (fixtures) => {
  const log = [];
  const check = (name, ok, detail = '') => {
    log.push({ name, ok, detail });
    console.log(`progress: ${ok ? 'ok' : 'FAIL'} ${name}`);
  };
  const c = new Compressor();
  const d = new Decompressor();
  check('shared memory matches crossOriginIsolated', c.info.sharedMemory === crossOriginIsolated && d.info.sharedMemory === crossOriginIsolated, JSON.stringify(c.info));

  for (const f of fixtures) {
    const raw = await fetchBytes(`./.fixtures/${f.name}.json`);
    const native = await fetchBytes(`./.fixtures/${f.name}.${f.tag}.jc`);
    const t0 = performance.now();
    const ours = await c.compress(raw, f.options);
    const t1 = performance.now();
    check(`${f.name} ${f.tag}: wasm output matches native`, equal(ours, native), `${raw.length} -> ${ours.length} (native ${native.length})`);
    check(`${f.name} ${f.tag}: round trip`, equal(await d.decompress(ours), raw));
    const t2 = performance.now();
    check(`${f.name} ${f.tag}: decodes native output`, equal(await d.decompress(native), raw), `compress ${(t1 - t0).toFixed(0)} ms, decompress ${(performance.now() - t2).toFixed(0)} ms`);
  }

  // streaming: input in small pieces, as a network body would arrive
  const drain = async (stream, onPiece = () => {}) => {
    const parts = [];
    for (const reader = stream.getReader(); ; ) {
      const { done, value } = await reader.read();
      if (done) break;
      parts.push(value);
      onPiece(value);
    }
    const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    parts.reduce((at, p) => (out.set(p, at), at + p.length), 0);
    return { out, pieces: parts.length };
  };
  const trickle = (bytes, step) => new ReadableStream({
    start(c) {
      for (let i = 0; i < bytes.length; i += step) c.enqueue(bytes.slice(i, i + step));
      c.close();
    },
  });
  for (const f of fixtures) {
    const raw = await fetchBytes(`./.fixtures/${f.name}.json`);
    const native = await fetchBytes(`./.fixtures/${f.name}.${f.tag}.jc`);
    const { out, pieces } = await drain(d.decompressStream(trickle(native, 4093)));
    check(`${f.name} ${f.tag}: streamed in pieces`, equal(out, raw), `${pieces} output pieces`);
  }

  // output arrives while input is still coming in
  {
    const raw = await fetchBytes('./.fixtures/records.json');
    const native = await fetchBytes('./.fixtures/records.chunks8.jc');
    let received = 0;
    let beforeEnd = -1;
    async function* slow() {
      const step = 8192;
      for (let i = 0; i < native.length; i += step) {
        if (i + step >= native.length - native.length / 8 && beforeEnd < 0) {
          await new Promise((r) => setTimeout(r, 50));
          beforeEnd = received;
        }
        yield native.slice(i, i + step);
      }
    }
    const { out } = await drain(d.decompressStream(slow()), (p) => (received += p.length));
    check('output streams before the input ends', equal(out, raw) && beforeEnd > raw.length / 2 && beforeEnd < raw.length, `${beforeEnd} of ${raw.length} bytes with an eighth of the input to come`);
    const single = await drain(d.decompressStream(await fetchBytes('./.fixtures/records.model1.jc')));
    check('one chunk streams record by record', equal(single.out, raw) && single.pieces > 10, `${single.pieces} pieces`);
  }

  // a truncated stream errors; a cancelled one stops; the pool carries on
  {
    const native = await fetchBytes('./.fixtures/records.chunks8.jc');
    try {
      await drain(d.decompressStream(native.subarray(0, native.length - 100)));
      check('truncated stream errors', false);
    } catch (e) {
      check('truncated stream errors', /truncated/.test(e.message), e.message);
    }
    const reader = d.decompressStream(trickle(native, 1000)).getReader();
    await reader.read();
    await reader.cancel();
    const small = new TextEncoder().encode('[{"a": 1}, {"a": 2}]');
    const again = await drain(d.decompressStream(await c.compress(small)));
    check('pool works after a cancelled stream', equal(again.out, small));
  }

  // many jobs at once: queued (shared memory) or spread over workers (otherwise)
  const docs = Array.from({ length: 12 }, (_, k) => new TextEncoder().encode(JSON.stringify(Array.from({ length: 2000 }, (_, i) => ({ id: i * k, price: ((i * 7 + k) / 100).toFixed(2), tags: ['a', 'b', String(k)] })))));
  const packed = await Promise.all(docs.map((x) => c.compress(x)));
  const back = await Promise.all(packed.map((x) => d.decompress(x)));
  check('12 concurrent jobs round-trip', back.every((x, i) => equal(x, docs[i])));

  // jcomp's own errors leave the worker usable
  try {
    await d.decompress(new Uint8Array([1, 2, 3]));
    check('corrupt input rejects', false);
  } catch (e) {
    check('corrupt input rejects', /not a jcomp v1 file/.test(e.message), e.message);
  }

  // deeper than wasm's nesting limit: compressed as raw xz, still exact
  const deep = new TextEncoder().encode('['.repeat(600) + ']'.repeat(600));
  check('600-deep document round-trips', equal(await d.decompress(await c.compress(deep)), deep));

  // natively compressed and too deep for the engine's stack: the worker is replaced
  try {
    await d.decompress(await fetchBytes('./.fixtures/deep.native.jc'));
    check('too-deep native file rejects', false);
  } catch (e) {
    check('too-deep native file rejects', true, e.message);
  }
  const small = new TextEncoder().encode('[{"a": 1}, {"a": 2}]');
  check('pool recovers after a fatal error', equal(await d.decompress(await c.compress(small)), small));

  c.terminate();
  d.terminate();
  return log;
};
window.pageReady = true;
