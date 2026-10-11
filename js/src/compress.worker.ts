// Host worker for jcomp-wasm/compress.
import { serve, type WasmModule } from './host.js';

interface M extends WasmModule {
  compress(data: Uint8Array, split: string, chunks: number, threads: number): Uint8Array;
}

serve<M>(
  (sharedMemory) => (sharedMemory ? import('../wasm/compress-threads/jcomp_wasm.js') : import('../wasm/compress/jcomp_wasm.js')) as Promise<M>,
  (m, [data, split, chunks], threads) => m.compress(data as Uint8Array, split as string, chunks as number, threads),
);
