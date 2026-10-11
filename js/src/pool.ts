// Page side: a set of host workers and a queue of jobs waiting for one.
//
// With shared memory (a cross-origin isolated page), one host worker runs each
// job on a rayon pool of `threads` Web Workers, so a single job uses every
// thread. Without it, up to `threads` single-threaded host workers run
// separate jobs side by side.
import type { FromWorker, ToWorker } from './protocol.js';

export interface PoolOptions {
  /**
   * Web Workers to use. Default: `navigator.hardwareConcurrency`.
   */
  threads?: number;
}

export interface PoolInfo {
  /** Jobs run on a shared-memory thread pool (the page is cross-origin isolated). */
  sharedMemory: boolean;
  threads: number;
}

/** Input for a stream: a byte stream, an (async) iterable of pieces, or all of it at once. */
export type StreamInput = ReadableStream<Uint8Array> | AsyncIterable<Uint8Array> | Iterable<Uint8Array> | Uint8Array;

interface Host {
  worker: Worker;
  ready: Promise<void>;
  busy: boolean;
}

/** pieces this large or smaller are posted to the worker one at a time */
const PUSH = 1 << 20;

const terminated = () => new Error('jcomp: pool was terminated');

export class Pool {
  readonly #spawn: () => Worker;
  readonly #threads: number;
  readonly #shared: boolean;
  #hosts: Host[] = [];
  #waiting: { resolve(h: Host): void; reject(e: Error): void }[] = [];
  /** rejecters of requests awaiting a reply, for `terminate` */
  #inflight = new Set<(e: Error) => void>();
  #nextId = 0;
  #closed = false;

  constructor(spawn: () => Worker, options: PoolOptions = {}) {
    this.#spawn = spawn;
    const n = options.threads ?? globalThis.navigator?.hardwareConcurrency ?? 4;
    this.#threads = Math.max(1, Math.floor(n));
    this.#shared = this.#threads > 1 && globalThis.crossOriginIsolated === true;
  }

  get info(): PoolInfo {
    return { sharedMemory: this.#shared, threads: this.#threads };
  }

  async run(args: unknown[]): Promise<Uint8Array> {
    const host = await this.#acquire();
    const id = this.#nextId++;
    const msg = await this.#request(host, id, { type: 'run', id, args }, 'done');
    this.#release(host);
    return (msg as Extract<FromWorker, { type: 'done' }>).result;
  }

  /**
   * Run a streaming job: input pieces are pushed to one worker as the output
   * stream is read, and output pieces are enqueued as the worker emits them.
   */
  stream(input: StreamInput): ReadableStream<Uint8Array> {
    let host: Host | undefined;
    let id = -1;
    let source: AsyncIterator<Uint8Array> | Iterator<Uint8Array>;
    let output: ReadableStreamDefaultController<Uint8Array>;
    let onData: ((e: MessageEvent<FromWorker>) => void) | undefined;
    let enqueued = 0;
    const stop = (e: Error) => {
      if (host) {
        if (onData) host.worker.removeEventListener('message', onData);
        // a fatal error already replaced the worker; otherwise it is reusable
        if (this.#hosts.includes(host)) this.#release(host);
        host = undefined;
      }
      void source?.return?.();
      output.error(e);
    };
    return new ReadableStream<Uint8Array>({
      start: async (controller) => {
        output = controller;
        source = pieces(input);
        try {
          host = await this.#acquire();
        } catch (e) {
          controller.error(e);
          return;
        }
        id = this.#nextId++;
        onData = ({ data }) => {
          if (data.type === 'data' && data.id === id) {
            controller.enqueue(data.piece);
            enqueued++;
          }
        };
        host.worker.addEventListener('message', onData);
        host.worker.postMessage({ type: 'open', id } satisfies ToWorker);
      },
      // the stream calls pull again only after something is enqueued, so keep
      // pushing input until the worker emits output or the input ends
      pull: async (controller) => {
        const before = enqueued;
        try {
          while (host && enqueued === before) {
            const next = await source.next();
            if (next.done) {
              await this.#request(host, id, { type: 'end', id }, 'ended');
              host.worker.removeEventListener('message', onData!);
              this.#release(host);
              host = undefined;
              controller.close();
              return;
            }
            const data = next.value.slice();
            await this.#request(host, id, { type: 'push', id, data }, 'pushed', [data.buffer]);
          }
        } catch (e) {
          stop(e as Error);
        }
      },
      cancel: () => {
        // the worker may be mid-decode; replacing it is the only way to stop it
        if (host) this.#drop(host);
        host = undefined;
        void source?.return?.();
      },
    });
  }

  terminate(): void {
    this.#closed = true;
    for (const h of this.#hosts) h.worker.terminate();
    for (const w of this.#waiting) w.reject(terminated());
    for (const r of this.#inflight) r(terminated());
    this.#hosts = [];
    this.#waiting = [];
    this.#inflight.clear();
  }

  /** an idle, started host; waits for one when all are busy */
  async #acquire(): Promise<Host> {
    if (this.#closed) throw terminated();
    let host = this.#hosts.find((h) => !h.busy);
    if (!host && this.#hosts.length < (this.#shared ? 1 : this.#threads)) host = this.#start();
    if (!host) host = await new Promise<Host>((resolve, reject) => this.#waiting.push({ resolve, reject }));
    host.busy = true;
    try {
      await host.ready;
    } catch (e) {
      this.#drop(host);
      throw e;
    }
    return host;
  }

  #release(host: Host): void {
    // handed straight to the next waiter, so no other caller can take it first
    const w = this.#waiting.shift();
    if (w) w.resolve(host);
    else host.busy = false;
  }

  /** replace a host that crashed, trapped, or is busy with abandoned work */
  #drop(host: Host): void {
    host.worker.terminate();
    this.#hosts = this.#hosts.filter((h) => h !== host);
    const w = this.#waiting.shift();
    if (w && !this.#closed) {
      const h = this.#start();
      h.busy = true;
      w.resolve(h);
    }
  }

  #start(): Host {
    const worker = this.#spawn();
    const host: Host = { worker, busy: false, ready: Promise.resolve() };
    host.ready = new Promise((resolve, reject) => {
      const onReady = ({ data }: MessageEvent<FromWorker>) => {
        worker.removeEventListener('message', onReady);
        if (data.type === 'ready') resolve();
        else reject(new Error(`jcomp: worker failed to start: ${data.type === 'error' ? data.message : data.type}`));
      };
      worker.addEventListener('message', onReady);
      worker.addEventListener('error', (e) => reject(new Error(`jcomp: worker failed to start: ${e.message}`)), { once: true });
    });
    worker.postMessage({ type: 'init', threads: this.#shared ? this.#threads : 1 } satisfies ToWorker);
    this.#hosts.push(host);
    return host;
  }

  /**
   * Post `msg` and wait for the reply of type `reply` (or an error) for `id`.
   * Errors reject; a fatal one, or a crashed worker, also replaces the host.
   */
  #request(host: Host, id: number, msg: ToWorker, reply: FromWorker['type'], transfer: Transferable[] = []): Promise<FromWorker> {
    return new Promise((resolve, reject) => {
      const finish = (data: FromWorker) => {
        this.#inflight.delete(reject);
        host.worker.removeEventListener('message', onMsg);
        host.worker.removeEventListener('error', onError);
        if (data.type !== 'error') return resolve(data);
        if (data.fatal) this.#drop(host);
        else if (msg.type === 'run') this.#release(host);
        reject(new Error(`jcomp: ${data.message}`));
      };
      const onMsg = ({ data }: MessageEvent<FromWorker>) => {
        if ((data.type === reply || data.type === 'error') && 'id' in data && data.id === id) finish(data);
      };
      // the worker itself died (for example, out of memory)
      const onError = (e: ErrorEvent) => finish({ type: 'error', id, message: e.message || 'worker crashed', fatal: true });
      this.#inflight.add(reject);
      host.worker.addEventListener('message', onMsg);
      host.worker.addEventListener('error', onError);
      host.worker.postMessage(msg, transfer);
    });
  }
}

/** iterate over the pieces of any stream input */
function pieces(input: StreamInput): AsyncIterator<Uint8Array> | Iterator<Uint8Array> {
  if (input instanceof Uint8Array) {
    // split, so output can be read (and backpressure apply) between pieces
    const parts: Uint8Array[] = [];
    for (let i = 0; i < input.length; i += PUSH) parts.push(input.subarray(i, i + PUSH));
    return parts[Symbol.iterator]();
  }
  if (input instanceof ReadableStream) {
    const reader = input.getReader();
    return {
      next: () => reader.read() as Promise<IteratorResult<Uint8Array>>,
      return: async () => {
        await reader.cancel();
        return { done: true, value: undefined };
      },
    };
  }
  if (Symbol.asyncIterator in input) return input[Symbol.asyncIterator]();
  return input[Symbol.iterator]();
}
