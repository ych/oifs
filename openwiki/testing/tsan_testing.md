---
type: "Reference"
title: "Thread Sanitizer (TSAN) Testing"
description: "Document ThreadSanitizer-based concurrency testing: data race detection in sharded caches, IPC, session management, and async task pipeline. No tsan Cargo feature exists; run locally with nightly toolchain."
tags: ["testing", "concurrency", "tsan", "data-races", "ipc", "session"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-886fbb2336c3635ef8adb3c3
    resource: repo://.github/workflows/rust.yml
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-466dba2e8868026185b26560
    resource: repo://src/auto_defrag.rs
  - id: openwiki-source-fd65460dcd62d8e13b82908b
    resource: repo://src/disk/caches.rs
  - id: openwiki-source-115024023ade9f243c6b4ec3
    resource: repo://src/disk/maintenance.rs
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
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

ThreadSanitizer (TSan) is a dynamic data race detector that can be used with OIFS to identify memory safety issues arising from concurrent access. The project includes specialized stress tests designed to expose data races, atomicity violations, and other concurrency-related bugs when executed under TSan's instrumentation. **There is no `tsan` Cargo feature and no CI integration for ThreadSanitizer**; tests are run locally using the nightly toolchain with `-Zsanitizer=thread`.

## Running ThreadSanitizer Locally

ThreadSanitizer requires the nightly Rust toolchain and a compatible clang installation with TSAN support. Tests are executed with the `-Zsanitizer=thread` flag. **Do not use `--features tsan`** — no such feature exists in `Cargo.toml`.

```bash
# Requires nightly toolchain and clang with TSAN support
RUSTFLAGS="-Z sanitizer=thread" cargo +nightly test --target x86_64-unknown-linux-gnu -Z build-std --features io_uring
```

Alternatively, for a specific test binary:

```bash
cargo +nightly test -Zsanitizer=thread --test concurrency_stress
cargo +nightly test -Zsanitizer=thread --test rwlock_concurrency_test
cargo +nightly test -Zsanitizer=thread --test session_ipc_test
cargo +nightly test -Zsanitizer=thread --test shuttle_concurrency_test
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

4. **Disjoint Range Writes** (`test_stress_concurrent_disjoint_slices_same_file`)
   - Multiple threads writing non-overlapping ranges to the same file concurrently
   - Validates no cross-range interference under TSan

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
| `test_multi_process_transparent_proxy_and_concurrency` | Multiple clients writing concurrently through Master; cross-client reads |
| `test_event_notification_on_master` | `SessionEvent` delivery ordering under concurrent client connect/disconnect |
| `test_stale_socket_recovery` | Master promotion when stale UDS socket is detected |
| `test_encrypted_filesystem_with_session` | Encrypted IPC session with authenticated clients |
| `test_encrypted_ipc_authentication_handshake_security` | Handshake protocol under rogue client attacks |
| `test_network_mode_transparent_proxy_and_concurrency` | TCP transport with rendezvous file + active ping probe |
| `test_network_mode_stale_rendezvous_recovery_and_active_probe` | Network failover with dead master detection |

### shuttle_concurrency_test.rs

Shuttle model checking (`tests/shuttle_concurrency_test.rs`) is complementary to ThreadSanitizer: Shuttle systematically explores thread interleavings of pure-Rust logic while TSAN detects races in actual native execution including FFI, I/O, and OS interactions.

## Tested Concurrency Hotspots

### 1. IPC Message Handling (`src/ipc.rs`)

**Thread interactions**: The `IpcClient` uses a connection pool (`MAX_CLIENT_POOL_STREAMS = 8`) protected by a `Mutex<ClientPoolState>`. Threads checkout streams via `checkout_stream()` which uses a `Condvar` for waiting. `CheckedOutStream` RAII guards ensure connection pool consistency on panic.

- **Lock hierarchy**: Pool mutex → stream checkout → RPC frame exchange → return to pool
- **TSAN coverage**: Pool contention under concurrent `send()` calls, secondary stream creation, authentication handshake race conditions
- **SessionMode interactions**: Both `SessionMode::Local` (UDS) and `SessionMode::Network` (TCP + rendezvous file) exercise the same client pool logic; network mode adds active ping probe failover

### 2. Sharded Caches (`src/disk/caches.rs`)

Three independent sharded caches with 32-way lock striping using Fibonacci hashing (`0x517cc1b727220a95`):

- **BoundedInodeCache**: 32 `RwLock<InodeCacheShard>` with Second-Chance (Clock) eviction; `AtomicBool` reference bits set under shared read lock
- **ShardedDirCache**: 32 `RwLock<FxHashMap<u64, DirIndex>>` for directory name indexes
- **ChunkReadCache** / **DecodedBlobCache**: Single `RwLock`/`Mutex` each (small bounded capacity)

**TSAN verification targets**:
- Concurrent `get()` (shared read lock) vs `insert()` (exclusive write lock) on different shards
- Reference bit `AtomicBool` updates under `Ordering::Relaxed` during cache hits
- Eviction queue manipulation under write lock without cross-shard interference
- `invalidate_inode()` clearing entries across all shards

### 3. Session Management (`src/session.rs`)

**SessionMode thread interactions**:

- **Standalone**: Direct `DiskManager` access, zero IPC overhead, no background threads
- **Local**: Master process spawns `IpcServer` with background thread; clients connect via UDS in `/tmp`
- **Network**: Master binds TCP + writes `.image.master` rendezvous file; active ping probe detects dead master; clients failover to local `DiskManager` via `dm_fallback` protected by `Mutex` + lock-free `has_fallback` `AtomicBool`

**TSAN verification targets**:
- Registry `OnceLock<Mutex<HashMap<PathBuf, OifsSession>>>` under concurrent `get_or_open_*` calls
- Master promotion race when stale socket/rendezvous file detected
- Failover: `has_fallback.load(Acquire)` → `dm_fallback.lock()` → local execution
- Event channel (`sync_channel`) delivery to Master under concurrent client requests

### 4. OifsAsyncTask Group-Commit Thread Safety (`src/disk.rs`)

The `OifsAsyncTask<T>` type implements an inline fast-path for cache hits and a background thread pool for slow paths:

- **Inline Fast-Path**: `OifsAsyncTask::ready()` returns immediately completed task (~50 ns) for:
  - `Mmap` raw file reads (mmap slice available)
  - Warm `ChunkReadCache` hits (seekable 64K chunks)
  - Warm `DecodedBlobCache` hits (stream-compressed whole-file)
  - Warm `FileIo` `DataPageCache` hits

- **Background Slow-Path**: `OifsAsyncTask::spawn()` submits to persistent worker pool (2–8 threads named `oifs-async-{idx}`)
  - Cache misses, cold reads, compressed/encrypted payload decoding
  - Uses `lock_unpoisoned` on queue mutex + `Condvar` for work distribution

**TSAN verification targets**:
- Fast-path `is_ready()` race with background completion
- `try_take()` / `wait()` / `poll()` concurrent access to shared `AsyncTaskState`
- Worker pool job submission and completion under contention
- `wait_on_drop` FFI guard preventing use-after-free

### 5. Group-Commit Worker Yield (`src/disk.rs`)

In the group-commit worker loop (`src/disk.rs:9736`), a `std::thread::yield_now()` is invoked when the work queue has exactly one pending operation. This gives other threads a scheduling opportunity and reduces contention on the queue mutex before the next batch drain:

```rust
if guard.len() == 1 {
    drop(guard);
    std::thread::yield_now();
    guard = crate::lock_unpoisoned(&q_worker.queue);
}
```

This `yield_now()` call is not gated behind any feature flag and executes in all builds.

### 6. DiskManager Lock Hierarchy

Two-tier locking strategy validated under TSAN:

1. **`RwLock<DiskManagerInner>`**: Primary guard for all metadata and inode operations
   - Read lock: `read_data`, `read_at`, `lookup`, `list_dir`, `begin_read()`
   - Write lock: `write_data`, `create_file`, `delete_file`, `flush`, `truncate`

2. **`sync_mutex: Arc<Mutex<()>>`**: Dedicated mutex serializing physical `msync`/`fdatasync` calls
   - Acquired **inside** write lock critical section for `DurabilityMode::Strict` and `RangeAsync` batch drains
   - Prevents readers from blocking on I/O syscalls

**TSAN verification targets**:
- No lock-order inversion: `sync_mutex` always acquired after `RwLock` write guard
- Concurrent readers proceed while `sync_mutex` held by flusher
- `RangeAsync` batch drain runs `flush_async_batch()` outside write lock (only `sync_mutex`)

## Poison Recovery Helpers (`src/lib.rs`)

```rust
pub(crate) fn lock_unpoisoned<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T>
pub(crate) fn read_unpoisoned<T>(r: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T>
pub(crate) fn write_unpoisoned<T>(r: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T>
```

These helpers recover from mutex/RwLock poisoning for **caches, registries, and coordination primitives** where a panic cannot leave data inconsistent. They are deliberately **not** used for:
- `DiskManagerInner` (guards on-disk consistency — propagates poison)
- `sync_mutex` (serializes physical flush — propagates poison)
- Any lock guarding stream state mid-frame (recovering would silently desynchronize protocol)

## Known False Positives and Suppression Strategies

### Expected TSAN Reports (Not Bugs)

1. **`AtomicBool` reference bit in `BoundedInodeCache`**
   - `referenced.store(true, Relaxed)` under shared read lock while eviction thread reads under write lock
   - Intentional relaxed-ordering Clock algorithm; no data race on inode payload

2. **`lock_unpoisoned` / `read_unpoisoned` recovery**
   - `PoisonError::into_inner()` accesses mutex internals after another thread panicked
   - By design: recovering thread observes consistent state because guarded data is reconstructible

3. **`sync_mutex` handoff during `RangeAsync` drain**
   - Background drain takes `sync_mutex` while writer holds `RwLock` write guard
   - Lock hierarchy validated: `RwLock` write → `sync_mutex` always; no inversion

4. **`OnceLock` session registry initialization**
   - First `get_or_open` call initializes `OnceLock`; concurrent callers wait
   - Standard library internal synchronization; not a user-data race

### Suppression Recommendations

If using TSAN suppressions file (`--suppressions=`), consider entries for:

```
# AtomicBool reference bit under Clock algorithm
race:^oifs::disk::caches::InodeCacheShard::get

# Poison recovery helpers
race:^oifs::lock_unpoisoned
race:^oifs::read_unpoisoned
race:^oifs::write_unpoisoned
```

**Do not suppress** reports involving:
- `DiskManagerInner` field accesses
- `sync_mutex` guard acquisition/release
- `IpcClient` pool state mutations
- `OifsAsyncTask` `AsyncTaskState` slot access
- Session registry `HashMap` operations

## Complementary Verification Tools

| Tool | Scope | Strength |
|------|-------|----------|
| **ThreadSanitizer** | Native execution with FFI, I/O, OS syscalls | Dynamic detection of actual data races in production-like runs |
| **Shuttle** (`shuttle_concurrency_test.rs`) | Pure-Rust logic with deterministic scheduler | Systematic interleaving exploration; finds races TSAN might miss due to scheduling |
| **Kani** (`kani_proofs` in `ipc.rs`, `disk.rs`) | Formal verification of frame validation, bounds checks | Mathematical proofs over all inputs for critical invariants |

Run all three locally for maximum concurrency confidence:

```bash
# TSAN (dynamic)
cargo +nightly test -Zsanitizer=thread --tests

# Shuttle (systematic interleaving)
cargo test --test shuttle_concurrency_test

# Kani (formal proofs)
cargo kani --test ipc_frame_test
cargo kani --test bolero_fuzz_test
```
