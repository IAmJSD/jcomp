// jcomp-wasm/compress: compress JSON (or any bytes) in Web Workers.
import { Pool, type PoolInfo, type PoolOptions } from './pool.js';

export type { PoolInfo, PoolOptions };

export interface CompressOptions {
  /**
   * How record columns are divided between the structural model and xz.
   * `"auto"` (default) measures each column under both and keeps the cheaper;
   * `{ xz: [...] }` sends exactly those columns to xz.
   */
  split?: 'auto' | 'model' | 'xz' | { xz: string[] };
  /**
   * Chunks that decompress in parallel. `"auto"` (default) keeps the most
   * parallel layout within 0.5% of the smallest; 1 gives the best ratio.
   */
  chunks?: 'auto' | number;
}

export class Compressor {
  readonly #pool: Pool;

  constructor(options: PoolOptions = {}) {
    this.#pool = new Pool(() => new Worker(new URL('./compress.worker.js', import.meta.url), { type: 'module' }), options);
  }

  get info(): PoolInfo {
    return this.#pool.info;
  }

  /** Compress `data`. The input is copied, never transferred. */
  compress(data: Uint8Array, options: CompressOptions = {}): Promise<Uint8Array> {
    const split = options.split ?? 'auto';
    const s = typeof split === 'string' ? split : `xz=${split.xz.join(',')}`;
    const chunks = options.chunks ?? 'auto';
    if (chunks !== 'auto' && !(Number.isInteger(chunks) && chunks >= 1)) {
      return Promise.reject(new RangeError(`jcomp: chunks must be "auto" or a positive integer, got ${chunks}`));
    }
    return this.#pool.run([data, s, chunks === 'auto' ? 0 : chunks]);
  }

  /** Stop all workers; pending jobs are rejected. */
  terminate(): void {
    this.#pool.terminate();
  }
}

let shared: Compressor | undefined;

/** Compress with a shared `Compressor` that uses every core. */
export function compress(data: Uint8Array, options?: CompressOptions): Promise<Uint8Array> {
  shared ??= new Compressor();
  return shared.compress(data, options);
}
