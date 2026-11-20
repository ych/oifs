//! Thread-local pooling of Zstd contexts and a lightweight persistent scoped worker pool.
//!
//! Every `zstd::stream::encode_all` / `zstd::stream::Decoder::new` allocates
//! and initialises a fresh `CCtx` / `DCtx`, which costs hundreds of kilobytes
//! of internal tables. On the hot paths (one frame per append, one frame per
//! 64 KiB chunk) that allocation dominates the actual compression work.
//!
//! Each thread keeps one compressor per compression level and one decompressor
//! in thread-local storage. Persistent worker threads created by [`par_for_index`]
//! keep their thread-local contexts and scratch buffers warm across calls with
//! zero `pthread_create` or `ZSTD_createDCtx`/`CCtx` churn.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

const MAX_GLOBAL_DECOMPRESSORS: usize = 32;
const MAX_GLOBAL_COMPRESSORS_PER_LEVEL: usize = 32;

fn global_decompressors() -> &'static Mutex<Vec<zstd::bulk::Decompressor<'static>>> {
    static POOL: OnceLock<Mutex<Vec<zstd::bulk::Decompressor<'static>>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(Vec::new()))
}

fn global_compressors() -> &'static Mutex<HashMap<i32, Vec<zstd::bulk::Compressor<'static>>>> {
    static POOL: OnceLock<Mutex<HashMap<i32, Vec<zstd::bulk::Compressor<'static>>>>> =
        OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

struct LocalCompressors(HashMap<i32, zstd::bulk::Compressor<'static>>);

impl Drop for LocalCompressors {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }
        if let Ok(mut global) = global_compressors().lock() {
            for (level, ctx) in self.0.drain() {
                let bucket = global.entry(level).or_default();
                if bucket.len() < MAX_GLOBAL_COMPRESSORS_PER_LEVEL {
                    bucket.push(ctx);
                }
            }
        }
    }
}

struct LocalDecompressor(Option<zstd::bulk::Decompressor<'static>>);

impl Drop for LocalDecompressor {
    fn drop(&mut self) {
        if let Some(ctx) = self.0.take()
            && let Ok(mut global) = global_decompressors().lock()
            && global.len() < MAX_GLOBAL_DECOMPRESSORS
        {
            global.push(ctx);
        }
    }
}

thread_local! {
    /// Pooled single-frame compressors, keyed by compression level.
    static COMPRESSORS: RefCell<LocalCompressors> =
        RefCell::new(LocalCompressors(HashMap::new()));
    /// Pooled single-frame decompressor.
    static DECOMPRESSOR: RefCell<LocalDecompressor> =
        const { RefCell::new(LocalDecompressor(None)) };
}

fn acquire_compressor(level: i32) -> std::io::Result<zstd::bulk::Compressor<'static>> {
    if let Ok(mut global) = global_compressors().lock()
        && let Some(bucket) = global.get_mut(&level)
        && let Some(ctx) = bucket.pop()
    {
        return Ok(ctx);
    }
    zstd::bulk::Compressor::new(level)
}

fn acquire_decompressor() -> std::io::Result<zstd::bulk::Decompressor<'static>> {
    if let Ok(mut global) = global_decompressors().lock()
        && let Some(ctx) = global.pop()
    {
        return Ok(ctx);
    }
    zstd::bulk::Decompressor::new()
}

/// Compresses `data` into a single Zstd frame, reusing a pooled context.
///
/// The output is a valid Zstd frame, byte-compatible with what
/// `zstd::stream::encode_all` produces for the same level.
pub fn compress_frame(data: &[u8], level: i32) -> std::io::Result<Vec<u8>> {
    COMPRESSORS.with(|pool| {
        let mut pool = pool.borrow_mut();
        let compressor = match pool.0.get_mut(&level) {
            Some(c) => c,
            None => {
                let c = acquire_compressor(level)?;
                pool.0.insert(level, c);
                pool.0.get_mut(&level).expect("just inserted")
            }
        };
        compressor.compress(data)
    })
}

/// Decompresses a single Zstd frame into a buffer of exactly `capacity` bytes.
///
/// Returns the number of bytes written. The pooled decompressor avoids the
/// per-call `DCtx` allocation; the caller provides the destination buffer, so
/// the decompressed data is written in a single pass without reallocations.
pub fn decompress_frame_into(source: &[u8], destination: &mut [u8]) -> std::io::Result<usize> {
    DECOMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.0.is_none() {
            slot.0 = Some(acquire_decompressor()?);
        }
        let decompressor = slot.0.as_mut().expect("initialized above");
        decompressor.decompress_to_buffer(source, destination)
    })
}

/// Returns the decompressed size of a single Zstd frame, if it can be derived
/// from the frame header alone.
pub fn decompressed_size_hint(source: &[u8]) -> Option<usize> {
    zstd::bulk::Decompressor::upper_bound(source)
}

/// Releases the pooled contexts of the current thread.
///
/// The contexts are pooled per thread; a thread that processed a single huge
/// frame can release them explicitly.
pub fn release_pooled_contexts() {
    COMPRESSORS.with(|pool| pool.borrow_mut().0.clear());
    DECOMPRESSOR.with(|cell| {
        cell.borrow_mut().0 = None;
    });
}

// ---------------------------------------------------------------------------
// Persistent Scoped Worker Pool (zero external dependencies)
// ---------------------------------------------------------------------------

struct SharedTask {
    run: unsafe fn(*const (), usize),
    ctx: *const (),
    total: usize,
    next: AtomicUsize,
    remaining_workers: AtomicUsize,
    panicked: AtomicBool,
}

// SAFETY: `ctx` points to a `Sync` closure on the caller's stack, and `WaitGuard`
// guarantees all workers finish accessing `SharedTask` before `par_for_index` returns.
unsafe impl Sync for SharedTask {}

#[derive(Clone, Copy)]
struct DispatchState {
    generation: u64,
    task: usize, // `*const SharedTask as usize`, 0 when idle
}

struct WorkerPoolInner {
    state: Mutex<DispatchState>,
    wake_cv: Condvar,
    done_lock: Mutex<()>,
    done_cv: Condvar,
}

struct WorkerPool {
    dispatch_lock: Mutex<()>,
    num_workers: usize,
    inner: Arc<WorkerPoolInner>,
}

fn worker_pool() -> &'static WorkerPool {
    static POOL: OnceLock<WorkerPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let num_workers = cores.saturating_sub(1).clamp(1, 15);
        let inner = Arc::new(WorkerPoolInner {
            state: Mutex::new(DispatchState {
                generation: 0,
                task: 0,
            }),
            wake_cv: Condvar::new(),
            done_lock: Mutex::new(()),
            done_cv: Condvar::new(),
        });
        let mut spawned = 0usize;
        for idx in 0..num_workers {
            let shared = Arc::clone(&inner);
            if std::thread::Builder::new()
                .name(format!("oifs-chunk-w{idx}"))
                .spawn(move || {
                    let mut last_gen = 0u64;
                    loop {
                        let task_addr = {
                            let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                            while st.generation == last_gen {
                                st = shared.wake_cv.wait(st).unwrap_or_else(|e| e.into_inner());
                            }
                            last_gen = st.generation;
                            st.task
                        };
                        if task_addr == 0 {
                            continue;
                        }
                        // SAFETY: valid for the entire duration until `remaining_workers` reaches 0.
                        let task = unsafe { &*(task_addr as *const SharedTask) };
                        loop {
                            let i = task.next.fetch_add(1, Ordering::Relaxed);
                            if i >= task.total {
                                break;
                            }
                            let res =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    // SAFETY: `task.ctx` is a valid `&F` where `F: Fn(usize) + Sync`.
                                    unsafe { (task.run)(task.ctx, i) };
                                }));
                            if res.is_err() {
                                task.panicked.store(true, Ordering::Release);
                                task.next.store(task.total, Ordering::Relaxed);
                                break;
                            }
                        }
                        if task.remaining_workers.fetch_sub(1, Ordering::AcqRel) == 1 {
                            let _g = shared.done_lock.lock().unwrap_or_else(|e| e.into_inner());
                            shared.done_cv.notify_one();
                        }
                    }
                })
                .is_ok()
            {
                spawned += 1;
            }
        }
        WorkerPool {
            dispatch_lock: Mutex::new(()),
            num_workers: spawned,
            inner,
        }
    })
}

/// Executes `f(0..total)` in parallel using the persistent worker pool (with the
/// calling thread participating via dynamic work-stealing).
///
/// Falls back seamlessly to inline sequential execution when `total <= 1` or when
/// another thread is already driving the pool (avoiding oversubscription and
/// re-entrant deadlocks).
pub(crate) fn par_for_index<F>(total: usize, f: F)
where
    F: Fn(usize) + Sync,
{
    if total == 0 {
        return;
    }
    if total == 1 {
        f(0);
        return;
    }

    let pool = worker_pool();
    if pool.num_workers == 0 {
        for i in 0..total {
            f(i);
        }
        return;
    }
    let Ok(_dispatch_guard) = pool.dispatch_lock.try_lock() else {
        for i in 0..total {
            f(i);
        }
        return;
    };

    unsafe fn trampoline<F: Fn(usize) + Sync>(ctx: *const (), idx: usize) {
        // SAFETY: `ctx` is `&F` on the caller's stack, kept alive by `WaitGuard`.
        let func = unsafe { &*(ctx as *const F) };
        func(idx);
    }

    let task = SharedTask {
        run: trampoline::<F>,
        ctx: (&f as *const F).cast::<()>(),
        total,
        next: AtomicUsize::new(0),
        remaining_workers: AtomicUsize::new(pool.num_workers),
        panicked: AtomicBool::new(false),
    };

    struct WaitGuard<'a> {
        pool: &'a WorkerPool,
        task: &'a SharedTask,
    }

    impl Drop for WaitGuard<'_> {
        fn drop(&mut self) {
            if self.task.remaining_workers.load(Ordering::Acquire) > 0 {
                let mut g = self
                    .pool
                    .inner
                    .done_lock
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                while self.task.remaining_workers.load(Ordering::Acquire) > 0 {
                    g = self
                        .pool
                        .inner
                        .done_cv
                        .wait(g)
                        .unwrap_or_else(|e| e.into_inner());
                }
            }
            let mut st = self
                .pool
                .inner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            st.task = 0;
        }
    }

    {
        let mut st = pool.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        st.task = (&task as *const SharedTask) as usize;
        st.generation = st.generation.wrapping_add(1);
        pool.inner.wake_cv.notify_all();
    }
    let wait_guard = WaitGuard { pool, task: &task };

    // Calling thread participates immediately while workers wake up.
    loop {
        let i = task.next.fetch_add(1, Ordering::Relaxed);
        if i >= task.total {
            break;
        }
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f(i);
        }));
        if res.is_err() {
            task.panicked.store(true, Ordering::Release);
            task.next.store(task.total, Ordering::Relaxed);
            break;
        }
    }

    drop(wait_guard);
    if task.panicked.load(Ordering::Acquire) {
        panic!("worker panicked during parallel chunk execution");
    }
}
