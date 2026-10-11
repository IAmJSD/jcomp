// Host worker for jcomp-wasm/decompress.
import { serve, type Stream, type WasmModule } from './host.js';

interface M extends WasmModule {
  decompress(data: Uint8Array, threads: number): Uint8Array;
  StreamDecoder: new (threads: number) => Stream;
}

serve<M>(
  (sharedMemory) => (sharedMemory ? import('../wasm/decompress-threads/jcomp_wasm.js') : import('../wasm/decompress/jcomp_wasm.js')) as Promise<M>,
  (m, [data], threads) => m.decompress(data as Uint8Array, threads),
  (m, threads) => new m.StreamDecoder(threads),
);
