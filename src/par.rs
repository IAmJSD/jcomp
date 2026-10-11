//! Parallel helpers. Native builds use scoped threads with large stacks. Wasm
//! builds use rayon on Web Workers (feature `wasm-threads`) or run sequentially.

/// worker stack size: the model recurses once per JSON nesting level
#[cfg(not(target_arch = "wasm32"))]
pub const STACK: usize = 512 << 20;

/// Map `f` over `jobs` on up to `threads` threads; results keep the job order.
#[cfg(not(target_arch = "wasm32"))]
pub fn par_map<T: Send, R: Send>(jobs: Vec<T>, threads: usize, f: impl Fn(T) -> R + Sync + Send) -> Vec<R> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let n = jobs.len();
    if threads <= 1 || n <= 1 {
        return jobs.into_iter().map(f).collect();
    }
    let jobs: Vec<Mutex<Option<T>>> = jobs.into_iter().map(|j| Mutex::new(Some(j))).collect();
    let results: Vec<Mutex<Option<R>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.min(n) {
            let b = std::thread::Builder::new().stack_size(STACK);
            b.spawn_scoped(s, || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let job = jobs[i].lock().unwrap().take().unwrap();
                let r = f(job);
                *results[i].lock().unwrap() = Some(r);
            })
            .unwrap();
        }
    });
    results.into_iter().map(|r| r.into_inner().unwrap().unwrap()).collect()
}

/// Like `par_map`, but hands each result to `emit` on the calling thread, in job
/// order, as soon as it and every earlier result are done.
#[cfg(not(target_arch = "wasm32"))]
pub fn map_in_order<T: Send, R: Send>(jobs: Vec<T>, threads: usize, f: impl Fn(T) -> R + Sync + Send, mut emit: impl FnMut(R)) {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let n = jobs.len();
    if threads <= 1 || n <= 1 {
        return jobs.into_iter().for_each(|j| emit(f(j)));
    }
    let jobs: Vec<Mutex<Option<T>>> = jobs.into_iter().map(|j| Mutex::new(Some(j))).collect();
    let next = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::channel::<(usize, R)>();
    std::thread::scope(|s| {
        for _ in 0..threads.min(n) {
            let tx = tx.clone();
            let b = std::thread::Builder::new().stack_size(STACK);
            let (jobs, next, f) = (&jobs, &next, &f);
            b.spawn_scoped(s, move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let job = jobs[i].lock().unwrap().take().unwrap();
                if tx.send((i, f(job))).is_err() {
                    break;
                }
            })
            .unwrap();
        }
        drop(tx);
        in_order(n, rx, &mut emit);
    });
}

/// receive (index, result) pairs and emit them in index order
#[cfg(any(not(target_arch = "wasm32"), feature = "wasm-threads"))]
fn in_order<R>(n: usize, rx: std::sync::mpsc::Receiver<(usize, R)>, emit: &mut impl FnMut(R)) {
    let mut pending: Vec<Option<R>> = (0..n).map(|_| None).collect();
    let mut head = 0;
    for (i, r) in rx {
        pending[i] = Some(r);
        while let Some(r) = pending.get_mut(head).and_then(Option::take) {
            emit(r);
            head += 1;
        }
    }
}

/// Run `a` and `b` concurrently: `a` on a new large-stack thread, `b` on this one.
#[cfg(not(target_arch = "wasm32"))]
pub fn join<A: Send, B: Send>(a: impl FnOnce() -> A + Send, b: impl FnOnce() -> B + Send) -> (A, B) {
    std::thread::scope(|s| {
        let h = std::thread::Builder::new().stack_size(STACK).spawn_scoped(s, a).unwrap();
        let rb = b();
        (h.join().unwrap(), rb)
    })
}

/// Run `f` on a thread with a large stack.
#[cfg(not(target_arch = "wasm32"))]
pub fn on_big_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|s| std::thread::Builder::new().stack_size(STACK).spawn_scoped(s, f).unwrap().join().unwrap())
}

// Wasm: stack sizes are fixed at link time (see js/build.sh), so `on_big_stack`
// runs in place; the rayon pool is started from JavaScript.
#[cfg(target_arch = "wasm32")]
pub fn par_map<T: Send, R: Send>(jobs: Vec<T>, threads: usize, f: impl Fn(T) -> R + Sync + Send) -> Vec<R> {
    #[cfg(feature = "wasm-threads")]
    if threads > 1 && jobs.len() > 1 {
        use rayon::prelude::*;
        return jobs.into_par_iter().with_max_len(1).map(f).collect();
    }
    let _ = threads;
    jobs.into_iter().map(f).collect()
}

#[cfg(target_arch = "wasm32")]
pub fn map_in_order<T: Send, R: Send>(jobs: Vec<T>, threads: usize, f: impl Fn(T) -> R + Sync + Send, mut emit: impl FnMut(R)) {
    #[cfg(feature = "wasm-threads")]
    if threads > 1 && jobs.len() > 1 {
        // the scope body runs on this thread, so `emit` may call into JavaScript
        let n = jobs.len();
        let (tx, rx) = std::sync::mpsc::channel::<(usize, R)>();
        let f = &f;
        rayon::in_place_scope(|s| {
            for (i, job) in jobs.into_iter().enumerate() {
                let tx = tx.clone();
                s.spawn(move |_| {
                    let _ = tx.send((i, f(job)));
                });
            }
            drop(tx);
            in_order(n, rx, &mut emit);
        });
        return;
    }
    let _ = threads;
    jobs.into_iter().for_each(|j| emit(f(j)));
}

#[cfg(target_arch = "wasm32")]
pub fn join<A: Send, B: Send>(a: impl FnOnce() -> A + Send, b: impl FnOnce() -> B + Send) -> (A, B) {
    #[cfg(feature = "wasm-threads")]
    return rayon::join(a, b);
    #[cfg(not(feature = "wasm-threads"))]
    (a(), b())
}

#[cfg(target_arch = "wasm32")]
pub fn on_big_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    f()
}
