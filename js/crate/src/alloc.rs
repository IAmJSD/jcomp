//! Global allocator for the shared-memory build. std's wasm allocator takes one
//! global spin lock on every call, which serializes the whole thread pool (10
//! threads ran 5x slower than one). Here each thread keeps free lists of small
//! blocks and takes the lock once per batch.
use dlmalloc::Dlmalloc;
use std::alloc::{GlobalAlloc, Layout};
use std::cell::{Cell, UnsafeCell};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};

/// small blocks: 16, 32, ..., 512 bytes, 16-aligned
const STEP: usize = 16;
const CLASSES: usize = 32;
/// blocks moved between a thread and the heap per lock
const BATCH: usize = 32;
/// cached blocks per class before half are returned
const MAX: usize = 4 * BATCH;

struct Heap {
    lock: AtomicBool,
    dl: UnsafeCell<Dlmalloc>,
}

// SAFETY: `dl` is only touched while `lock` is held
unsafe impl Sync for Heap {}

static HEAP: Heap = Heap { lock: AtomicBool::new(false), dl: UnsafeCell::new(Dlmalloc::new()) };

impl Heap {
    fn with<R>(&self, f: impl FnOnce(&mut Dlmalloc) -> R) -> R {
        while self.lock.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::hint::spin_loop();
        }
        // SAFETY: the lock gives exclusive access
        let r = f(unsafe { &mut *self.dl.get() });
        self.lock.store(false, Ordering::Release);
        r
    }
}

/// per-thread free lists, linked through the first word of each block
struct Cache {
    head: [Cell<*mut u8>; CLASSES],
    len: [Cell<usize>; CLASSES],
}

thread_local! {
    static CACHE: Cache = const { Cache { head: [const { Cell::new(null_mut()) }; CLASSES], len: [const { Cell::new(0) }; CLASSES] } };
}

fn class(l: &Layout) -> Option<usize> {
    (l.size() <= CLASSES * STEP && l.align() <= STEP).then(|| l.size().max(1).div_ceil(STEP) - 1)
}

impl Cache {
    unsafe fn push(&self, c: usize, p: *mut u8) {
        unsafe { *(p as *mut *mut u8) = self.head[c].get() };
        self.head[c].set(p);
        self.len[c].set(self.len[c].get() + 1);
    }

    unsafe fn pop(&self, c: usize) -> *mut u8 {
        let p = self.head[c].get();
        if !p.is_null() {
            self.head[c].set(unsafe { *(p as *mut *mut u8) });
            self.len[c].set(self.len[c].get() - 1);
        }
        p
    }
}

pub struct Alloc;

unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let Some(c) = class(&l) else { return HEAP.with(|h| unsafe { h.malloc(l.size(), l.align()) }) };
        CACHE.with(|t| unsafe {
            if t.head[c].get().is_null() {
                HEAP.with(|h| {
                    for _ in 0..BATCH {
                        let p = h.malloc((c + 1) * STEP, STEP);
                        if p.is_null() {
                            break;
                        }
                        t.push(c, p);
                    }
                });
            }
            t.pop(c)
        })
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        let Some(c) = class(&l) else { return HEAP.with(|h| unsafe { h.free(p, l.size(), l.align()) }) };
        CACHE.with(|t| unsafe {
            t.push(c, p);
            if t.len[c].get() > MAX {
                HEAP.with(|h| {
                    for _ in 0..MAX / 2 {
                        h.free(t.pop(c), (c + 1) * STEP, STEP);
                    }
                });
            }
        })
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if class(&l).is_none() {
            return HEAP.with(|h| unsafe { h.calloc(l.size(), l.align()) });
        }
        let p = unsafe { self.alloc(l) };
        if !p.is_null() {
            unsafe { p.write_bytes(0, l.size()) };
        }
        p
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { Layout::from_size_align_unchecked(new_size, l.align()) };
        if class(&l).is_none() && class(&new).is_none() {
            return HEAP.with(|h| unsafe { h.realloc(p, l.size(), l.align(), new_size) });
        }
        let q = unsafe { self.alloc(new) };
        if !q.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(p, q, l.size().min(new_size));
                self.dealloc(p, l);
            }
        }
        q
    }
}
