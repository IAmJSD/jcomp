// jcomp-wasm/decompress: decompress jcomp files in Web Workers.
import { Pool, type PoolInfo, type PoolOptions, type StreamInput } from './pool.js';

export type { PoolInfo, PoolOptions, StreamInput };

export class Decompressor {
  readonly #pool: Pool;

  constructor(options: PoolOptions = {}) {
    this.#pool = new Pool(() => new Worker(new URL('./decompress.worker.js', import.meta.url), { type: 'module' }), options);
  }

  get info(): PoolInfo {
    return this.#pool.info;
  }

  /** Decompress `data` to exactly the original bytes. The input is copied, never transferred. */
  decompress(data: Uint8Array): Promise<Uint8Array> {
    return this.#pool.run([data]);
  }

  /**
   * Decompress as a stream. Input can arrive over time (for example a fetch
   * body); output pieces are enqueued as soon as they are decoded, and more
   * input is pushed only as the output is read. A record array (a JSON array
   * of objects) streams record by record; other documents arrive in one
   * piece once decoded. The pieces concatenate to exactly the original bytes.
   */
  decompressStream(input: StreamInput): ReadableStream<Uint8Array> {
    return this.#pool.stream(input);
  }

  /** Stop all workers; pending jobs are rejected. */
  terminate(): void {
    this.#pool.terminate();
  }
}

let shared: Decompressor | undefined;

/** Decompress with a shared `Decompressor` that uses every core. */
export function decompress(data: Uint8Array): Promise<Uint8Array> {
  shared ??= new Decompressor();
  return shared.decompress(data);
}

/** `decompressStream` with the shared `Decompressor`. */
export function decompressStream(input: StreamInput): ReadableStream<Uint8Array> {
  shared ??= new Decompressor();
  return shared.decompressStream(input);
}
