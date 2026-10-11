// Browser tests: build fixtures with the native CLI, then run test/page.js in
// headless Chrome, once cross-origin isolated (shared-memory thread pool) and
// once not (separate single-threaded workers).
//
//   node test/run.mjs     (after ./build.sh and `cargo build --release` at the repo root)
import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';
import { serve } from './server.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');
const jcomp = join(root, '..', 'target', 'release', 'jcomp');
const fx = join(here, '.fixtures');
mkdirSync(fx, { recursive: true });

// deterministic pseudo-random numbers
let seed = 1;
const rnd = () => ((seed = (seed * 1103515245 + 12345) % 2 ** 31) / 2 ** 31);

const regions = ['us-east-1', 'us-west-2', 'eu-west-1', 'ap-south-1'];
const records = Array.from({ length: 20000 }, (_, i) => ({
  instance_type: `m${i % 9}.${['large', 'xlarge', '2xlarge'][i % 3]}${i}`,
  vCPU: 2 ** (i % 6),
  memory: (2 ** (i % 6)) * 4,
  pricing: Object.fromEntries(regions.map((r, k) => [r, { linux: { ondemand: ((i % 50 + 1) * (1 + k / 10) * 0.0125).toFixed(4), reserved: { yrTerm1: ((i % 50 + 1) * 0.0125 * 8760 / 8760).toString() } } }])),
}));
const tree = {
  type: 'FeatureCollection',
  features: Array.from({ length: 4000 }, (_, i) => ({
    type: 'Feature',
    properties: { id: i, name: `lot ${i}`, area: Math.round(rnd() * 1e6) / 100 },
    geometry: { type: 'Polygon', coordinates: [Array.from({ length: 6 }, (_, k) => [-122.4 + ((i + k) % 97) / 1e3, 37.7 + ((i * 3 + k) % 89) / 1e3])] },
  })),
};
// Jackson-style spacing, which matches no Python json.dumps style
const jackson = JSON.stringify(records.slice(0, 3000), null, 2).replace(/": /g, '" : ');

const docs = {
  records: JSON.stringify(records, null, 1),
  tree: JSON.stringify(tree),
  jackson,
  invalid: 'not json, just text '.repeat(500),
};
const fixtures = [
  { name: 'records', tag: 'auto', options: {}, cli: [] },
  { name: 'records', tag: 'model3', options: { split: 'model', chunks: 3 }, cli: ['--split', 'model', '--chunks', '3'] },
  { name: 'records', tag: 'cols', options: { split: { xz: ['pricing'] }, chunks: 2 }, cli: ['--split', 'xz=pricing', '--chunks', '2'] },
  { name: 'records', tag: 'chunks8', options: { split: 'model', chunks: 8 }, cli: ['--split', 'model', '--chunks', '8'] },
  { name: 'records', tag: 'model1', options: { split: 'model', chunks: 1 }, cli: ['--split', 'model', '--chunks', '1'] },
  { name: 'tree', tag: 'chunks4', options: { chunks: 4 }, cli: ['--chunks', '4'] },
  { name: 'jackson', tag: 'auto', options: {}, cli: [] },
  { name: 'invalid', tag: 'auto', options: {}, cli: [] },
];
for (const [name, text] of Object.entries(docs)) writeFileSync(join(fx, `${name}.json`), text);
for (const f of fixtures) execFileSync(jcomp, ['c', ...f.cli, join(fx, `${f.name}.json`), join(fx, `${f.name}.${f.tag}.jc`)]);
writeFileSync(join(fx, 'deep.json'), '['.repeat(5000) + ']'.repeat(5000));
execFileSync(jcomp, ['c', '--split', 'model', join(fx, 'deep.json'), join(fx, 'deep.native.jc')]);

const browser = await chromium.launch({ channel: 'chrome' });
let failed = 0;
for (const isolated of [true, false]) {
  const server = await serve(root, isolated);
  const page = await browser.newPage();
  page.on('pageerror', (e) => console.log('  page error:', e.message));
  if (process.env.VERBOSE) page.on('console', (m) => console.log('  console:', m.text()));
  await page.goto(`http://127.0.0.1:${server.address().port}/test/page.html`);
  await page.waitForFunction(() => window.pageReady === true);
  console.log(isolated ? 'cross-origin isolated (shared-memory threads):' : 'not isolated (separate workers):');
  const log = await page.evaluate((f) => window.runTests(f), fixtures.map(({ name, tag, options }) => ({ name, tag, options })));
  for (const r of log) {
    if (!r.ok) failed++;
    console.log(`  ${r.ok ? 'ok  ' : 'FAIL'} ${r.name}${r.detail ? `  (${r.detail})` : ''}`);
  }
  await page.close();
  server.close();
}
await browser.close();
if (failed) {
  console.log(`${failed} failed`);
  process.exit(1);
}
