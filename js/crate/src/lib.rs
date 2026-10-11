//! wasm-bindgen exports for the jcomp TypeScript package (`js/`).
use wasm_bindgen::prelude::*;

#[cfg(feature = "threads")]
pub use wasm_bindgen_rayon::init_thread_pool;

#[cfg(feature = "threads")]
mod alloc;

#[cfg(feature = "threads")]
#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

/// `split`: "auto", "model", "xz", or "xz=COL,COL"; `chunks`: 0 = auto.
#[cfg(feature = "compress")]
#[wasm_bindgen]
pub fn compress(data: &[u8], split: &str, chunks: u32, threads: u32) -> Result<Vec<u8>, JsError> {
    use jcomp::{Opts, Split};
    let split = match split {
        "auto" => Split::Auto,
        "model" => Split::Model,
        "xz" => Split::Xz,
        s if s.starts_with("xz=") => Split::Cols(s[3..].split(',').filter(|x| !x.is_empty()).map(String::from).collect()),
        _ => return Err(JsError::new(&format!("bad split {split:?}"))),
    };
    let o = Opts { split, chunks: chunks as usize, threads: threads.max(1) as usize, verbose: false };
    Ok(jcomp::compress(data, &o))
}

#[cfg(feature = "decompress")]
#[wasm_bindgen]
pub fn decompress(data: &[u8], threads: u32) -> Result<Vec<u8>, JsError> {
    jcomp::decompress(data, threads.max(1) as usize).map_err(|e| JsError::new(&e))
}

/// Streaming decompression: output pieces go to `emit` as soon as they are decoded.
#[cfg(feature = "decompress")]
#[wasm_bindgen]
pub struct StreamDecoder(jcomp::Decoder);

#[cfg(feature = "decompress")]
#[wasm_bindgen]
impl StreamDecoder {
    #[wasm_bindgen(constructor)]
    pub fn new(threads: u32) -> StreamDecoder {
        StreamDecoder(jcomp::Decoder::new(threads.max(1) as usize))
    }

    /// Add input and decode everything it allows.
    pub fn push(&mut self, data: &[u8], emit: &js_sys::Function) -> Result<(), JsError> {
        self.0.push(data);
        self.0.pull(&mut |p| call(emit, &p)).map_err(|e| JsError::new(&e))
    }

    /// The input is complete: decode the rest, or fail if it was truncated.
    pub fn finish(&mut self, emit: &js_sys::Function) -> Result<(), JsError> {
        self.0.finish(&mut |p| call(emit, &p)).map_err(|e| JsError::new(&e))
    }
}

/// hand `p` to JavaScript as a fresh (transferable) Uint8Array
#[cfg(feature = "decompress")]
fn call(emit: &js_sys::Function, p: &[u8]) {
    let _ = emit.call1(&JsValue::NULL, &js_sys::Uint8Array::from(p));
}
