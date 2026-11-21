---
type: "Reference"
title: "Requires nightly toolchain and clang with TSAN support"
openwiki_generated: true
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-886fbb2336c3635ef8adb3c3
    resource: repo://.github/workflows/rust.yml
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-466dba2e8868026185b26560
    resource: repo://src/auto_defrag.rs
  - id: openwiki-source-fd65460dcd62d8e13b82908b
    resource: repo://src/disk/caches.rs
  - id: openwiki-source-ef0afc6eaf5c925c9975314d
    resource: repo://src/ipc.rs
  - id: openwiki-source-ed8bf05e307c6278442542c2
    resource: repo://src/lib.rs
  - id: openwiki-source-c1e8d5f8bb6497980a6b4166
    resource: repo://src/session.rs
  - id: openwiki-source-120f0cc600d844dc98f88d46
    resource: repo://tests/concurrency_stress_test.rs
  - id: openwiki-source-89dfa6f9ae01afe2c6645cb0
    resource: repo://tests/rwlock_concurrency_test.rs
  - id: openwiki-source-4dd96a71b25353481dd09366
    resource: repo://tests/session_ipc_test.rs
  - id: openwiki-source-5b30823597738f668b07d33c
    resource: repo://tests/shuttle_concurrency_test.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---


ThreadSanitizer (TSan) is a dynamic data race detector that can be used with OIFS to identify memory safety issues arising from concurrent access. The project includes specialized stress tests designed to expose data races, atomicity violations, and other concurrency-related bugs when executed under TSan's instrumentation. **There is no `tsan` Cargo feature and no CI integration for ThreadSanitizer**; tests are run locally using the nightly toolchain with `-Zsanitizer=thread`.

## Running ThreadSanitizer Locally

ThreadSanitizer requires the nightly Rust toolchain and a compatible clang installation with TSAN support. Tests are executed with the `-Zsanitizer=thread` flag:

```bash
# Requires nightly toolchain and clang with TSAN support
RUSTFLAGS="-Z sanitizer=thread" cargo +nightly test --target x86_64-unknown-linux-gnu -Z build-std --features io_uring
```

Alternatively, for a specific test binary:

```bash
cargo +nightly test -Zsanitizer=thread --test concurrency_stress
cargo +nightly test -Zsanitizer=thread --test rwlock_concurrency_test
cargo +nightly test -Zsanitizer=thread --test session_ipc_test
```

The existing Shuttle model checking and Kani formal verification provide stronger guarantees than dynamic TSAN analysis for the verified code paths, as they systematically explore thread interleavings or prove properties over all inputs.

## Key Test Files for Data Race Detection

### concurrency_stress_test.rs

The primary high-contention stress suite (`tests/concurrency_stress_test.rs`) containing scenarios that maximize thread interleaving:

1. **Multithreaded Read/Write Integrity** (`test_stress_multithread_read_write_integrity`)
   - 6 concurrent threads performing 20 iterations each of 2KB writes
   - Periodic flushing and self-verification during write operations
   - Final structural integrity check via `verify_integrity()`

2. **Concurrent Mutations with Live Fsck** (`test_stress_concurrent_mutations_with_live_fsck`)
   - Background thread continuously running fsck while worker threads create/delete files
   - Validates filesystem structural integrity during active mutations

3. **Indirect Block Expansion Under Contention** (`test_stress_large_file_indirect_block_expansion`)
   - 4 threads expanding files across direct/indirect block boundary (40KB → 80KB)
   - Tests indirect table allocation safety, pointer initialization, and block indexing

4. **Disjoint Range Writes** (`test_stress_disjoint_range_writes`)
   - Multiple threads writing non-overlapping ranges concurrently
   - Validates no cross-range interference

### rwlock_concurrency_test.rs

The `rwlock_concurrency_test.rs` suite (`tests/rwlock_concurrency_test.rs`) stresses the `DiskManager`'s internal `RwLock<DiskManagerInner>` and the dedicated `sync_mutex` used for flush coordination:

| Test | Threads | Purpose |
|------|---------|---------|
| `test_concurrent_readers_scaling_and_safety` | 16 readers | Validates reader scaling and read-read concurrency under `RwLock` read guard |
| `test_mixed_readers_and_writers_safety` | 8 readers + 2 writers | Tests read-write and write-write exclusion on shared inodes |
| `test_concurrent_flush_and_readers` | 8 readers + 4 flushers | Intermixes `flush()` and `flush_async()` with concurrent readers to validate `sync_mutex` hierarchy |
| `test_out_of_lock_compression_and_encryption_concurrency` | 8 readers + 2 heavy writers | Exercises Zstd + Delta/Shuffle filters + ChaCha20 encryption pipeline while readers run |
| `test_reader_latency_under_heavy_writes` | 1 reader + 1 writer | Measures p50/p99 read latency during sustained compressed/encrypted writes |
| `test_sharded_inode_cache_sparse_readers_concurrency` | 16 readers × 1000 ops | Validates 32-shard inode cache (Second-Chance Clock) under sparse access patterns |
| `test_cache_miss_eviction_contention_benchmark` | 16 readers × 2000 ops | 3000 files > 2048 cache capacity forces continuous misses and evictions across shards |

These tests deliberately intermix synchronous (`flush()`) and asynchronous (`flush_async()`) flush operations with concurrent readers to validate correct locking hierarchy between the main `RwLock` and the `sync_mutex`.

### session_ipc_test.rs

The `session_ipc_test.rs` suite (`tests/session_ipc_test.rs`) stresses multi-process IPC concurrency and failover synchronization:

| Test | Focus |
|------|-------|
| `test_multi_process_transparent_proxy_and_concurrency` | 2 clients + master, concurrent writes from both clients, cross-client reads, peer count tracking |
| `test_event_notification_on_master` | `SessionEvent` channel delivery (`PeerConnected`, `RequestHandled`, `PeerDisconnected`) under concurrent client activity |
| `test_stale_socket_recovery` | Master election recovery when stale UDS socket exists |
| `test_encrypted_filesystem_with_session` / `test_encrypted_ipc_authentication_handshake_security` | Challenge-response authentication handshake with Argon2id/BLAKE2b-MAC under concurrent access |
| `test_network_mode_transparent_proxy_and_concurrency` | TCP transport + rendezvous file, active Ping/Pong probe, concurrent operations |
| `test_network_mode_stale_rendezvous_recovery_and_active_probe` | Stale rendezvous file cleanup via active probe |

The IPC layer uses a bounded connection pool (`MAX_CLIENT_POOL_STREAMS = 8`) with RAII `CheckedOutStream` guards. On connection failure, the transparent failover logic in `OifsSession::send_request` (`src/session.rs#L749-L848`) re-runs master election up to 5 times, using `lock_unpoisoned` on the `dm_fallback` mutex to pin a new local `DiskManager` if the client promotes to master.

### shuttle_concurrency_test.rs

Shuttle model checking (`tests/shuttle_concurrency_test.rs`) is **complementary to ThreadSanitizer**. While TSan detects races in the specific schedules exercised during test execution, Shuttle's randomized scheduler systematically explores thread interleavings across many runs (`shuttle::check_random(..., 50)`):

- Concurrent file creation, read/write, create/delete races
- Overlapping offset writes on the same file (3 threads, overlapping ranges)
- Concurrent compression/decompression race
- Nested directory creation and path resolution race
- Same-filename creation race (proves double-checked locking in directory index)
- Sharded `DirCache` promotion and indirect `read_at` under randomized schedules

Shuttle provides stronger guarantees for the explored state space but only for the modeled operations; TSan catches races in actual native execution including FFI, I/O, and OS interactions.

## lock_unpoisoned / read_unpoisoned Patterns

OIFS uses poison-recovered locking for caches, registries, and coordination primitives where a panic mid-critical-section **cannot leave the guarded data inconsistent**. The helpers are defined in `src/lib.rs#L42-L68`:

```rust
/// Lock a mutex, recovering from poisoning.
pub(crate) fn lock_unpoisoned<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Acquire a read lock, recovering from poisoning.
pub(crate) fn read_unpoisoned<T>(r: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    r.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Acquire a write lock, recovering from poisoning.
/// Only use where a panic cannot leave data inconsistent.
pub(crate) fn write_unpoisoned<T>(r: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    r.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}
```

### Rationale and TSAN Safety

**Why recover poison?**
- These locks guard **reconstructible in-memory state** (caches, registries, coordination flags)
- A panic in a cache operation leaves the cache data valid — it may be stale or partial, but not corrupted
- Propagating poison would permanently brick the subsystem (every future acquisition fails)
- One unrelated panic anywhere would cascade into total failure

**Where NOT used (poison propagates):**
- `DiskManagerInner` state behind the main `RwLock` — guards on-disk consistency
- `sync_mutex` — guards physical durability flush ordering
- I/O stream state — a panic mid-frame would desynchronize the protocol

**TSAN implications:**
- `lock_unpoisoned`/`read_unpoisoned` use `Mutex::lock().unwrap_or_else(...)` which **acquires the lock normally** before checking poison
- TSAN sees standard lock acquire/release semantics — happens-before edges are preserved
- No false positives from the recovery logic itself; the lock is held during the entire critical section
- The pattern is TSAN-safe because the underlying synchronization primitive is used correctly

### Call Sites

| Component | Lock Type | Guarded State | Why Recovery Safe |
|-----------|-----------|---------------|-------------------|
| `SESSSION_REGISTRY` (`src/session.rs#L77`) | `Mutex` | `HashMap<PathBuf, OifsSession>` | Registry entries are `Arc`-cloned; panic during insert leaves map valid |
| `IpcClient.pool` (`src/ipc.rs#L1407`) | `Mutex` | `VecDeque<Stream>`, `total_streams` | `CheckedOutStream` RAII ensures streams returned or closed on panic |
| `IpcServer.pool` (`src/ipc.rs#L1618`) | `Mutex` | Accepted stream pool | Same RAII pattern |
| `AutoDefragController.config/stats/worker/shutdown` (`src/auto_defrag.rs`) | `Mutex` | Configuration, statistics, worker handle | Purely in-memory coordination; panic leaves values readable |
| `ChunkReadCache`, `DecodedBlobCache`, `DirCache` shards (`src/disk.rs`) | `RwLock` | Cache entries | Caches are pure optimization; stale entries evicted naturally |
| `BoundedInodeCache` shards (`src/disk/caches.rs`) | `RwLock` | `FxHashMap<u64, CachedInodeEntry>` + `VecDeque` order | Second-chance bits are `AtomicBool`; order updated under lock |

## DirCache 32-Shard Locking and Contention Profile

The directory index cache (`ShardedDirCache` in `src/disk/caches.rs#L202-L274`) divides `dir_inode_id → DirIndex` across **32 independent `RwLock` shards** using a 64-bit Fibonacci hashing bijection:

```rust
const NUM_DIR_CACHE_SHARDS: usize = 32;

fn shard_index(&self, dir_inode_id: u64) -> usize {
    (dir_inode_id.wrapping_mul(0x517cc1b727220a95) as usize) & (self.num_shards - 1)
}
```

### Contention Characteristics

- **Read-heavy workloads** (`lookup`, `resolve_path`, `list_dir`): Acquire `read_shard` (shared `RwLockReadGuard`). Multiple readers on **different directories** never contend — they hit different shards. Readers on the **same directory** share the shard read lock (highly concurrent).
- **Write operations** (`create_file`, `delete_file`, `build_dir_index`, `list_dir` promotion): Acquire `write_shard` (exclusive `RwLockWriteGuard`). Only blocks other operations on directories mapping to the **same shard**.
- **Cache maintenance** (`clear`, `for_each_mut`): Iterates all 32 shards, taking write lock on each sequentially.

### TSAN Observations

Under ThreadSanitizer:
- No data races on `DirIndex` fields (`complete`, `plain_complete`, `cold_scans`, `may_have_holes`, `tail_offset`, `stored`, `plain`) — all accesses are protected by the shard's `RwLock`
- The Fibonacci hash ensures uniform shard distribution, minimizing false sharing
- `read_unpoisoned`/`write_unpoisoned` used for shard access — TSAN sees proper acquire/release

The companion `BoundedInodeCache` also uses 32 shards with the same hashing scheme, protecting `FxHashMap<u64, CachedInodeEntry>` + `VecDeque<u64>` order under `RwLock`. The `referenced` bit in `CachedInodeEntry` is an `AtomicBool` updated with `Relaxed` ordering under the **shared read lock** — this is intentional: hot inodes get a reference bit set without upgrading to write lock, and TSAN correctly sees the atomic operation as race-free.

## Auto-Defrag Daemon vs Writer Races

The background automatic defragmentation coordinator (`src/auto_defrag.rs`) runs as a daemon thread that periodically measures fragmentation and runs in-place defragmentation passes.

### Synchronization Design

- The worker owns only a `Weak<Arc<RwLock<DiskManagerInner>>>` — it cannot keep the manager alive past its last owner
- **Every defrag pass takes the inner `RwLock` write lock** (`DiskManager::defragment_inplace_on`), so it serializes against **all metadata mutations** exactly like any other writer
- No two defrag passes can overlap (single worker thread)
- The worker sleeps on a `Condvar` with the configured `check_interval` (minimum 10 ms, enforced by `MIN_CHECK_INTERVAL`)

### Race Scenarios and TSAN Coverage

| Scenario | Synchronization | TSAN Verification |
|----------|-----------------|-------------------|
| Defrag pass vs `write_data`/`create_file`/`delete_file` | Both take inner write lock | Mutual exclusion verified — no concurrent write/write |
| Defrag pass vs `read_at`/`read_data` | Defrag: write lock; Readers: read lock | RW exclusion verified — readers see consistent state |
| Defrag pass vs `flush`/`flush_async` | Defrag: inner write lock; Flush: `sync_mutex` + inner read lock | Lock hierarchy: `sync_mutex` acquired before inner read lock; defrag takes inner write lock directly — no deadlock, TSAN sees proper ordering |
| Config update during pass | `AutoDefragController.config` mutex (`lock_unpoisoned`) | Configuration read at start of each cycle; changes picked up next tick |

The `run_once` function (`src/auto_defrag.rs#L255-L299`) reads the config, checks fragmentation under read lock, then performs the defrag under write lock. The `skip_reason` check reads the superblock's `has_journal_layout` under `read_unpoisoned` — a read-only snapshot that cannot be left inconsistent by a panic.

## IPC Client Failover Synchronization

Transparent failover in `OifsSession::send_request` (`src/session.rs#L749-L848`) handles Master loss (crash, network partition) without caller intervention:

```rust
// Lock-free fast path: only acquire dm_fallback if previously failed over
if has_fallback.load(Acquire) {
    let local_dm = crate::lock_unpoisoned(dm_fallback).clone();
    if let Some(dm) = local_dm {
        return IpcServer::handle_request(&dm, req).map_err(...);
    }
}

// Retry loop: re-run master election up to 5 times
for attempt in 0..5 {
    match current_client.send(&req) {
        Ok(data) => return Ok(data),
        Err(io_err) if Self::is_connection_error(&io_err) => {
            // Re-elect: may become new Master (Direct) or connect to new Master (Remote)
            let retry_session = Self::open_with_mode(...)?;
            match retry_session {
                OifsSession::Direct { dm, .. } => {
                    // Pin local DiskManager as fallback
                    *crate::lock_unpoisoned(dm_fallback) = Some(dm.clone());
                    has_fallback.store(true, Release);
                    return IpcServer::handle_request(&dm, req).map_err(...);
                }
                OifsSession::Remote { client: new_client, .. } => {
                    current_client = new_client;
                }
            }
        }
        ...
    }
}
```

### Synchronization Primitives

| Primitive | Type | Purpose |
|-----------|------|---------|
| `dm_fallback` | `Arc<Mutex<Option<Arc<DiskManager>>>>` | Holds promoted local `DiskManager` after failover to Master |
| `has_fallback` | `Arc<AtomicBool>` | Lock-free flag to avoid mutex on steady-state Remote RPCs |
| `client.pool` | `Arc<Mutex<VecDeque<Stream>>>` | Connection pool with `CheckedOutStream` RAII guards |

### TSAN Considerations

- `has_fallback.load(Acquire)` / `store(Release)` establishes happens-before between failover detection and `dm_fallback` access
- `lock_unpoisoned(dm_fallback)` acquires the mutex normally — TSAN sees standard mutex semantics
- `CheckedOutStream` Drop impl closes socket on panic, preventing desynchronized socket reuse — no data race on stream state
- Connection pool mutex guards `VecDeque` + `total_streams` counter atomically

## Suppressing False Positives

When running under ThreadSanitizer, the following patterns may appear but are **not real data races**:

1. **`AtomicBool`/`AtomicUsize` with `Ordering::Relaxed`** for coordination flags (`stop_flag`, `has_fallback`, cache `referenced` bits) — intentionally race-free by design, TSAN correctly understands atomic operations
2. **Poison recovery in `lock_unpoisoned`/`read_unpoisoned`** — the lock is acquired before poison check; TSAN sees proper acquire/release
3. **`Weak::upgrade` in auto-defrag worker** — race between upgrade and last `Arc` drop is benign; worker exits cleanly on `None`
4. **`OnceLock` initialization in `SESSSION_REGISTRY`** — Rust's `OnceLock` is thread-safe; TSAN may report on internal initialization but it's a false positive

To suppress known benign reports, create a suppression file:

```bash
# tsan.suppr
race:^rust_begin_unwind
race:^std::sync::OnceLock
```

Run with:
```bash
TSAN_OPTIONS="suppressions=tsan.suppr" cargo +nightly test -Zsanitizer=thread ...
```

## Relationship to Other Verification Methods

| Method | Approach | Guarantees | Complementarity |
|--------|----------|------------|-----------------|
| **ThreadSanitizer** | Dynamic analysis of native execution | Empirical race freedom on exercised schedules | Catches races in FFI, I/O, OS interactions; real hardware memory model |
| **Shuttle** | Systematic randomized scheduling of `std::thread` | Exhaustive exploration of modeled interleavings (within bound) | Proves race freedom for pure-Rust logic under all explored schedules |
| **Kani** | Formal verification via CBMC | Mathematical proofs over all inputs for verified functions | Proves absence of specific bug classes (overflow, panic, memory safety) for modeled code |

**No `tsan` Cargo feature exists.** The project relies on Shuttle and Kani for concurrency verification in CI; ThreadSanitizer is a local development tool for additional dynamic analysis.
