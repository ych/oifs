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
    /// Dedicated thread-local `CCtx` for dictionary-based compression so dictionary
    /// state never leaks into non-dict `compress_frame` calls.
    static DICT_COMPRESSOR: RefCell<Option<zstd::zstd_safe::CCtx<'static>>> =
        const { RefCell::new(None) };
    /// Dedicated thread-local `DCtx` for dictionary-based decompression.
    static DICT_DECOMPRESSOR: RefCell<Option<zstd::zstd_safe::DCtx<'static>>> =
        const { RefCell::new(None) };
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
    DICT_COMPRESSOR.with(|cell| {
        *cell.borrow_mut() = None;
    });
    DICT_DECOMPRESSOR.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

// ---------------------------------------------------------------------------
// Custom Zstd Dictionary Registry & Pooled CDict/DDict Compression
// ---------------------------------------------------------------------------

/// Maximum accepted raw dictionary size (16 MiB) to prevent unbounded memory use.
pub const MAX_ZSTD_DICT_BYTES: usize = 16 * 1024 * 1024;
const MAX_CACHED_DICTS: usize = 64;
const MAX_CACHED_CDICTS: usize = 256;

/// Returns the non-zero Zstd `dict_id` encoded in a trained Zstd dictionary buffer.
#[inline]
pub fn dict_id_from_bytes(dict: &[u8]) -> Option<u32> {
    zstd::zstd_safe::get_dict_id_from_dict(dict).map(|id| id.get())
}

/// Returns the non-zero Zstd `dict_id` required by a compressed frame header, if any.
#[inline]
pub fn frame_dict_id(source: &[u8]) -> Option<u32> {
    zstd::zstd_safe::get_dict_id_from_frame(source).map(|id| id.get())
}

/// Trains a Zstd dictionary from sample byte slices (bounded by `MAX_ZSTD_DICT_BYTES`).
pub fn train_dictionary<T: AsRef<[u8]>>(
    samples: &[T],
    max_dict_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    if samples.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Cannot train a Zstd dictionary from an empty sample list",
        ));
    }
    if !(256..=MAX_ZSTD_DICT_BYTES).contains(&max_dict_bytes) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Dictionary size must be between 256 and {MAX_ZSTD_DICT_BYTES} bytes"),
        ));
    }
    zstd::dict::from_samples(samples, max_dict_bytes)
}

fn hash_dict_bytes(dict: &[u8]) -> [u8; 32] {
    use blake2::{Blake2b512, Digest};
    let digest = Blake2b512::digest(dict);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest[..32]);
    out
}

#[derive(Default)]
struct DictRegistryState {
    raw: HashMap<u32, (Arc<Vec<u8>>, [u8; 32])>,
    ddicts: HashMap<u32, Arc<zstd::zstd_safe::DDict<'static>>>,
    cdicts: HashMap<(u32, i32), Arc<zstd::zstd_safe::CDict<'static>>>,
    default_dict_id: Option<u32>,
}

/// Thread-safe registry of Zstd compression/decompression dictionaries (`CDict` / `DDict`).
///
/// Keyed by 32-bit `dict_id` (plus level for `CDict`) so multiple dictionary revisions can
/// coexist safely for decompression while one optional active `default_dict_id` governs
/// new writes.
#[derive(Default)]
pub struct ZstdDictRegistry {
    inner: std::sync::RwLock<DictRegistryState>,
}

impl ZstdDictRegistry {
    /// Creates an empty dictionary registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates and registers a trained Zstd dictionary, returning its `dict_id`.
    pub fn register(&self, dict_bytes: &[u8]) -> std::io::Result<u32> {
        if dict_bytes.is_empty() || dict_bytes.len() > MAX_ZSTD_DICT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Zstd dictionary size must be 1..={MAX_ZSTD_DICT_BYTES} bytes (got {})",
                    dict_bytes.len()
                ),
            ));
        }
        let dict_id = dict_id_from_bytes(dict_bytes).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Zstd dictionary lacks a valid non-zero dict_id header",
            )
        })?;
        let hash = hash_dict_bytes(dict_bytes);

        // Fast check under shared lock if this exact `(dict_id, hash)` is already loaded.
        {
            let state = self.inner.read().unwrap();
            if let Some((_, existing_hash)) = state.raw.get(&dict_id)
                && *existing_hash == hash
            {
                return Ok(dict_id);
            }
        }

        let ddict = zstd::zstd_safe::DDict::try_create(dict_bytes).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Failed to construct Zstd DDict from dictionary bytes",
            )
        })?;

        let mut state = self.inner.write().unwrap();
        if !state.raw.contains_key(&dict_id) && state.raw.len() >= MAX_CACHED_DICTS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Dictionary registry full (max {MAX_CACHED_DICTS} dictionaries)"),
            ));
        }
        state.cdicts.retain(|&(id, _), _| id != dict_id);
        state
            .raw
            .insert(dict_id, (Arc::new(dict_bytes.to_vec()), hash));
        state.ddicts.insert(dict_id, Arc::new(ddict));
        Ok(dict_id)
    }

    /// Sets (or clears with `None`) the active default compression dictionary ID.
    pub fn set_default(&self, dict_id: Option<u32>) -> std::io::Result<()> {
        let mut state = self.inner.write().unwrap();
        if let Some(id) = dict_id
            && !state.raw.contains_key(&id)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Zstd dictionary {id} is not registered"),
            ));
        }
        state.default_dict_id = dict_id;
        Ok(())
    }

    /// Returns the currently active default compression dictionary ID, if any.
    pub fn default_dict_id(&self) -> Option<u32> {
        self.inner.read().unwrap().default_dict_id
    }

    /// Returns a pooled `CDict` for the active default dictionary at `level` (or `None`).
    pub fn default_cdict(&self, level: i32) -> Option<Arc<zstd::zstd_safe::CDict<'static>>> {
        let level = if level <= 0 { 3 } else { level };
        let raw = {
            let state = self.inner.read().unwrap();
            let id = state.default_dict_id?;
            if let Some(cached) = state.cdicts.get(&(id, level)) {
                return Some(Arc::clone(cached));
            }
            Arc::clone(&state.raw.get(&id)?.0)
        };

        let dict_id = dict_id_from_bytes(&raw)?;
        let cdict = Arc::new(zstd::zstd_safe::CDict::try_create(&raw, level)?);
        let mut state = self.inner.write().unwrap();
        if let Some(existing) = state.cdicts.get(&(dict_id, level)) {
            return Some(Arc::clone(existing));
        }
        if state.cdicts.len() < MAX_CACHED_CDICTS {
            state.cdicts.insert((dict_id, level), Arc::clone(&cdict));
        }
        Some(cdict)
    }

    /// Looks up the `DDict` for `dict_id`.
    pub fn get_ddict(&self, dict_id: u32) -> Option<Arc<zstd::zstd_safe::DDict<'static>>> {
        self.inner.read().unwrap().ddicts.get(&dict_id).cloned()
    }

    /// Looks up the raw dictionary bytes for `dict_id` (used for streaming decoder fallback).
    pub fn get_raw(&self, dict_id: u32) -> Option<Arc<Vec<u8>>> {
        self.inner
            .read()
            .unwrap()
            .raw
            .get(&dict_id)
            .map(|(raw, _)| Arc::clone(raw))
    }

    /// Removes `dict_id` from the registry (clears `default_dict_id` if it pointed to it).
    pub fn unregister(&self, dict_id: u32) -> bool {
        let mut state = self.inner.write().unwrap();
        let existed = state.raw.remove(&dict_id).is_some();
        state.ddicts.remove(&dict_id);
        state.cdicts.retain(|&(id, _), _| id != dict_id);
        if state.default_dict_id == Some(dict_id) {
            state.default_dict_id = None;
        }
        existed
    }

    /// Clears all registered dictionaries and cached prepared contexts.
    pub fn clear(&self) {
        let mut state = self.inner.write().unwrap();
        state.raw.clear();
        state.ddicts.clear();
        state.cdicts.clear();
        state.default_dict_id = None;
    }

    /// Returns `true` if at least one dictionary is registered.
    pub fn has_any(&self) -> bool {
        !self.inner.read().unwrap().raw.is_empty()
    }

    /// Returns a sorted list of all registered `dict_id`s.
    pub fn registered_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.inner.read().unwrap().raw.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Returns `true` if every `(dict_id, content_hash)` in `self` is also present in `other`.
    ///
    /// Snapshots `self` before locking `other` to eliminate cross-registry lock ordering hazards.
    pub fn is_subset_of(&self, other: &ZstdDictRegistry) -> bool {
        if std::ptr::eq(self, other) {
            return true;
        }
        let snapshot: Vec<(u32, [u8; 32])> = {
            let state = self.inner.read().unwrap();
            if state.raw.is_empty() {
                return true;
            }
            state
                .raw
                .iter()
                .map(|(&id, (_, hash))| (id, *hash))
                .collect()
        };
        let other_state = other.inner.read().unwrap();
        snapshot.into_iter().all(|(id, hash)| {
            other_state
                .raw
                .get(&id)
                .is_some_and(|(_, other_hash)| *other_hash == hash)
        })
    }
}

/// Compresses `data` into a single Zstd frame using `cdict` and a dedicated TLS `CCtx`.
pub fn compress_frame_with_cdict(
    data: &[u8],
    cdict: &zstd::zstd_safe::CDict<'_>,
) -> std::io::Result<Vec<u8>> {
    let bound = zstd::zstd_safe::compress_bound(data.len());
    let mut out = vec![0u8; bound];
    let written = DICT_COMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = zstd::zstd_safe::CCtx::try_create();
        }
        let cctx = slot
            .as_mut()
            .ok_or_else(|| std::io::Error::other("Failed to allocate Zstd CCtx"))?;
        cctx.compress_using_cdict(&mut out, data, cdict)
            .map_err(|code| {
                std::io::Error::other(format!(
                    "Zstd CDict compression failed: {}",
                    zstd::zstd_safe::get_error_name(code)
                ))
            })
    })?;
    out.truncate(written);
    Ok(out)
}

/// Decompresses a single Zstd frame using `ddict` and a dedicated TLS `DCtx`.
pub fn decompress_frame_into_with_ddict(
    source: &[u8],
    destination: &mut [u8],
    ddict: &zstd::zstd_safe::DDict<'_>,
) -> std::io::Result<usize> {
    DICT_DECOMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = zstd::zstd_safe::DCtx::try_create();
        }
        let dctx = slot
            .as_mut()
            .ok_or_else(|| std::io::Error::other("Failed to allocate Zstd DCtx"))?;
        dctx.decompress_using_ddict(destination, source, ddict)
            .map_err(|code| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "Zstd DDict decompression failed: {}",
                        zstd::zstd_safe::get_error_name(code)
                    ),
                )
            })
    })
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
    workers_to_claim: usize,
}

/// Computes the exact number of background pool workers to wake for `total` work items,
/// given `num_workers` available pool threads.
///
/// The calling thread always participates inline, so at most `total - 1` additional
/// workers can ever do useful work. Avoiding redundant wakeups eliminates thundering-herd
/// synchronization overhead on small multi-chunk writes and 4-chunk read-ahead batches.
#[inline]
pub(crate) const fn workers_to_wake(total: usize, num_workers: usize) -> usize {
    let needed = total.saturating_sub(1);
    if needed < num_workers {
        needed
    } else {
        num_workers
    }
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
                workers_to_claim: 0,
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
                            while st.generation == last_gen || st.workers_to_claim == 0 {
                                st = shared.wake_cv.wait(st).unwrap_or_else(|e| e.into_inner());
                            }
                            last_gen = st.generation;
                            st.workers_to_claim -= 1;
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
    let active_workers = workers_to_wake(total, pool.num_workers);
    if active_workers == 0 {
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
        remaining_workers: AtomicUsize::new(active_workers),
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
            st.workers_to_claim = 0;
        }
    }

    {
        let mut st = pool.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        st.task = (&task as *const SharedTask) as usize;
        st.workers_to_claim = active_workers;
        st.generation = st.generation.wrapping_add(1);
        if active_workers >= pool.num_workers {
            pool.inner.wake_cv.notify_all();
        } else {
            for _ in 0..active_workers {
                pool.inner.wake_cv.notify_one();
            }
        }
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

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Proves that `workers_to_wake` never exceeds either `num_workers` or `total - 1`,
    /// skips waking any background threads when `total <= 1`, and guarantees sufficient
    /// worker reachability (`min(sleeping, active) + returning >= active`) so exact-count
    /// `notify_one()` wakeups can never deadlock regardless of how many workers are asleep.
    #[kani::proof]
    fn kani_worker_pool_exact_wakeup_liveness() {
        let total: usize = kani::any();
        let num_workers: usize = kani::any();
        kani::assume(num_workers <= 64);

        let active = workers_to_wake(total, num_workers);
        assert!(active <= num_workers);
        if total <= 1 {
            assert_eq!(active, 0);
        } else {
            assert!(active <= total - 1);
            if num_workers > 0 {
                assert!(active >= 1);
            }
        }

        // Let `sleeping` be the number of workers currently blocked in `wake_cv.wait`,
        // and `returning = num_workers - sleeping` be workers about to re-lock `state`.
        let sleeping: usize = kani::any();
        kani::assume(sleeping <= num_workers);
        let returning = num_workers - sleeping;
        let woken = if sleeping < active { sleeping } else { active };
        assert!(
            woken + returning >= active,
            "at least `active` distinct workers must reach state.lock() to claim slots"
        );
    }
}
