// Worker side: load one wasm variant and run jobs posted by a Pool.
import type { FromWorker, ToWorker } from './protocol.js';

export interface WasmModule {
  default(): Promise<unknown>;
  initThreadPool?(threads: number): Promise<unknown>;
}

/** A streaming job: input pieces in, output pieces out through `emit`. */
export interface Stream {
  push(data: Uint8Array, emit: (piece: Uint8Array) => void): void;
  finish(emit: (piece: Uint8Array) => void): void;
  free(): void;
}

type Scope = {
  onmessage: ((e: MessageEvent<ToWorker>) => void) | null;
  postMessage(msg: FromWorker, transfer?: Transferable[]): void;
  crossOriginIsolated?: boolean;
};

/**
 * `load(true)` must return the shared-memory build, `load(false)` the
 * single-threaded one. `call` runs a job and `open` starts a stream: `threads`
 * is the size of the pool they may use (1 without shared memory).
 */
export function serve<M extends WasmModule>(
  load: (sharedMemory: boolean) => Promise<M>,
  call: (m: M, args: unknown[], threads: number) => Uint8Array,
  open?: (m: M, threads: number) => Stream,
): void {
  const scope = globalThis as unknown as Scope;
  let wasm: M | undefined;
  let threads = 1;
  let stream: Stream | undefined;
  const fail = (id: number, e: unknown) => {
    stream?.free();
    stream = undefined;
    // jcomp's own errors arrive as plain Errors; anything else (RangeError on
    // stack overflow, WebAssembly.RuntimeError on a trap) leaves the
    // instance in an unknown state
    const fatal = !(e instanceof Error) || e.constructor !== Error;
    scope.postMessage({ type: 'error', id, message: e instanceof Error ? e.message : String(e), fatal });
  };
  scope.onmessage = async ({ data: msg }) => {
    if (msg.type === 'init') {
      try {
        const shared = msg.threads > 1 && scope.crossOriginIsolated === true;
        wasm = await load(shared);
        await wasm.default();
        if (shared) {
          await wasm.initThreadPool!(msg.threads);
          threads = msg.threads;
        }
        scope.postMessage({ type: 'ready', sharedMemory: shared });
      } catch (e) {
        scope.postMessage({ type: 'error', id: null, message: String(e), fatal: true });
      }
      return;
    }
    const id = msg.id;
    const emit = (piece: Uint8Array) => scope.postMessage({ type: 'data', id, piece }, [piece.buffer]);
    try {
      switch (msg.type) {
        case 'run': {
          const result = call(wasm!, msg.args, threads);
          scope.postMessage({ type: 'done', id, result }, [result.buffer]);
          break;
        }
        case 'open':
          // a stream the page abandoned (its input failed) is still open
          stream?.free();
          stream = open!(wasm!, threads);
          break;
        case 'push':
          stream!.push(msg.data, emit);
          scope.postMessage({ type: 'pushed', id });
          break;
        case 'end':
          stream!.finish(emit);
          stream!.free();
          stream = undefined;
          scope.postMessage({ type: 'ended', id });
          break;
      }
    } catch (e) {
      fail(id, e);
    }
  };
}
