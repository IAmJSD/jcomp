// Messages between a Pool (page side) and a host worker (worker side).

export type ToWorker =
  | { type: 'init'; threads: number }
  | { type: 'run'; id: number; args: unknown[] }
  // a stream: open, then push input pieces (each acknowledged), then end
  | { type: 'open'; id: number }
  | { type: 'push'; id: number; data: Uint8Array }
  | { type: 'end'; id: number };

export type FromWorker =
  | { type: 'ready'; sharedMemory: boolean }
  | { type: 'done'; id: number; result: Uint8Array }
  // stream output, in order
  | { type: 'data'; id: number; piece: Uint8Array }
  | { type: 'pushed'; id: number }
  | { type: 'ended'; id: number }
  // fatal: the wasm instance trapped (stack overflow, out of memory) and the
  // worker must be replaced
  | { type: 'error'; id: number | null; message: string; fatal: boolean };
