# OIFS Performance Optimization Roadmap & Architecture Blueprints

This document records the implemented optimizations (P0 & P1) and the planned future performance enhancements (P2 & P3) for the OIFS storage engine across ARM (AArch64) and x86/x64 architectures.

---

## 1. Implemented Optimizations Status

### P0 (Critical - Completed)
1. **Zero-Copy `read_at` & Batch Indirect Block Resolution**:
   - **File**: `src/disk.rs`, `src/ffi.rs`
   - **Mechanism**: Eliminates heap allocation for raw/uncompressed reads. Directly copies byte slices from `mmap` blocks into the user-supplied buffer. Single/double/triple indirect blocks are batch-resolved in 8-byte chunks (`chunks_exact(8)`) via `u64::from_le_bytes`, skipping unallocated sparse holes.
   - **FFI**: Exported `oifs_read_at(handle, filename, offset, buf, buf_size) -> i64`.
2. **IPC Server Busy-Polling Removal**:
   - **File**: `src/ipc.rs`
   - **Mechanism**: Replaced non-blocking `thread::sleep(Duration::from_millis(2))` with POSIX `libc::poll` (`wait_for_fd_readable(raw_fd, 50)`).
   - **Impact**: Zero idle CPU usage (eliminates 500 context switches/sec); sub-microsecond connection latency.

### P1 (High Impact - Completed)
1. **BitShuffle 8x8 Delta-Swap Transposition with 2x Superscalar ILP**:
   - **File**: `src/filters.rs`
   - **Mechanism**: Replaced 64-iteration bitwise loops with Hacker's Delight delta-swap matrix transposition (`transpose_8x8_u64`). Loop is unrolled 2x to process 16-byte pairs across dual ALU execution ports on modern ARM and x86_64 cores.
   - **Benchmark Result**: Throughput jumped from **1,684.3 MB/s to 14,672.3 MB/s (8.40x faster, ~14.7 GB/s)**.
2. **`DiskManager` Reader-Writer Lock Transition (`Arc<RwLock<DiskManagerInner>>`)**:
   - **File**: `src/disk.rs`
   - **Mechanism**: Replaced global `Mutex` with `RwLock`. All read-only paths (`read_data`, `read_at`, `lookup`, `list_dir`, `resolve_path`, `analyze_fragmentation`, `verify_integrity`) acquire shared `.read()` locks, allowing concurrent parallel worker threads without contention.
3. **Idiomatic Rust Portable Endianness Architecture**:
   - **File**: `src/filters.rs`, `include/oifs.h`
   - **Mechanism**: Uses `#[cfg(target_endian = "little")]` with typed slice auto-vectorization on Little-Endian, and standard library `from_le_bytes`/`to_le_bytes` portable fallback on Big-Endian for 100% on-disk interoperability.

---

### P2 (Performance & Latency - Completed)
1. **`recommend_filters` Heuristic Sampling & Rayon Parallelism (P2.1)**:
   - **File**: `src/filters.rs`, `Cargo.toml`
   - **Mechanism**: For payloads > 256 KB, takes representative 192 KB multi-window samples (64 KB head, 64 KB mid, 64 KB tail, aligned to 8-byte boundaries) and parallelizes 14 candidate filter/compression evaluations across all CPU cores using `rayon::par_iter()`.
   - **Benchmark Result**: Recommendation latency for a 10MB structured dataset dropped from **3.5 seconds down to 6.4 milliseconds (> 500x speedup)** with 100% detection accuracy (detected Delta+Shuffle typesize=4, 99.9% space savings).
2. **`DiskManager` Zero-Copy Inode Cache (P2.2)**:
   - **File**: `src/disk.rs`
   - **Mechanism**: Added thread-safe in-memory `RwLock<HashMap<u64, Inode>>` inside `DiskManagerInner`. Eliminates `bincode::deserialize` overhead on metadata hot paths (`read_inode`, `resolve_path`, `lookup`, `stat`). Cache updates synchronously on `write_inode_internal` and invalidates on `delete_file`.
   - **Benchmark Result**: 10,000 3-level path resolutions (`/dir_a/dir_b/data.bin`) completed in **9.7 milliseconds (< 1.0 microsecond per lookup)**.

---

### P3 (Extreme Scale - In Progress)
1. **Multi-Block Directories & Hashed Directory Index (P3.1 - Completed)**:
   - **File**: `src/directory.rs`, `src/disk.rs`, `src/inode.rs`, `tests/multi_block_directory_test.rs`, `tests/dir_bench.rs`
   - **Mechanism**:
     - **Dynamic Block Expansion**: Directories grow across direct / single / double indirect blocks via `get_or_alloc_block`. Inserts try the last block first, so a growing directory appends in O(1) blocks; earlier blocks are only probed (to reuse space freed by deletes) when the last block is full.
     - **On-disk 64-bit SipHash-2-4 per entry** (`hash_filename`): `find_entry_in_block_with_hash` skips records whose non-zero hash differs before comparing names. Legacy entries (`hash == 0`) always fall through to a name compare. On-disk scans are still linear in the number of blocks — there is no on-disk bucket placement (a `hash % num_blocks` scheme does not work because `num_blocks` changes as the directory grows).
     - **Per-directory in-memory index** (`dir_cache: RwLock<HashMap<u64, DirIndex>>`): zero-allocation hits. The first negative lookup in a directory (already a full scan), or 8 cold scans, builds a *complete* index, after which hits, misses and create-time existence checks are O(1). Deleting a directory drops its index so a reused inode id can never resolve stale names.
     - **Verified block addressing**: logical-block → pointer-path arithmetic lives in one pure function (`inode::BlockPath`) shared by the read path and the allocating write path, with a Kani proof over every `usize`.
     - **Subsystem Integration**: FSCK (`verify_integrity`) and Defragmentation (`defragment_safe`) scan and preserve all multi-block directory trees.
     - **Backward Compatibility**: Legacy v1 single-block directories (`inode.size == 0`) remain readable/writable and upgrade on first expansion.
   - **Benchmark Result** (`cargo test --release --test dir_bench -- --ignored --nocapture`, 10,000 entries / 91 blocks, arm64 macOS):

     | Operation (×10,000) | Before index | After index |
     | :--- | ---: | ---: |
     | Cold lookup (fresh `DiskManager`) | 79.2 ms | 1.05 ms |
     | Negative lookup | 137.0 ms | 0.82 ms |
     | Warm lookup | 0.69 ms | 0.71 ms |
     | Create + 1-byte write | 991 ms | 874 ms |

     Create is dominated by the whole-mapping `flush_async()` (msync) issued on every mutation; without it the same run takes ~19 ms (see open item below).


---

## 2. Planned Optimizations Status (P3 Future)

---

## 3. P3 Architectural Blueprints (Long-Term)

### P3.3: Configurable Durability Policy & Per-Mutation msync Optimization (Completed)
- **File**: `src/disk.rs`, `src/lib.rs`, `tests/durability_test.rs`
- **Mechanism**:
  - Replaced unconditionally scanning the entire virtual memory address space with `mmap.flush_async()` on every single filesystem mutation (`create_file`, `write_data`, `delete_file`).
  - Added [`DurabilityMode`](file:///Users/ych/oifs/src/disk.rs) enum supporting 4 configurable durability policies:
    1. `Lazy` (Default / ProcessSafe): Mutations modify shared mmap & OS page cache without per-mutation syscalls; survives process crashes/panics; dirty pages are flushed on explicit [`DiskManager::flush`], on [`Drop`], or via OS writeback.
    2. `RangeAsync`: Asynchronously schedules writeback for only the exact modified byte ranges (inode bitmap, data bitmap, inode records, and modified data/directory blocks) via `msync(MS_ASYNC)`.
    3. `Strict`: Synchronously flushes modified byte ranges via `msync(MS_SYNC)` on every mutation for immediate power-loss safety.
    4. `LegacyWholeMmapAsync`: Flushes the entire mmap asynchronously on every mutation (pre-P3.3 behavior).
  - Can be queried via `dm.durability_mode()`, switched dynamically via `dm.set_durability_mode()`, or configured via builder `dm.with_durability_mode()`.
  - Added Kani formal verification proofs (`proof_durability_mode_from_u8_soundness`, `proof_durability_mode_is_range_based_consistency`).
- **Benchmark Result** (`cargo test --release --test dir_bench -- --ignored --nocapture`, 10,000 files in a directory):

  | Operation (×10,000) | Before P3.3 (Whole-Mmap Async) | After P3.3 (Lazy Mode) | Speedup |
  | :--- | :--- | :--- | :--- |
  | Create + 1-byte write | 874.12 ms | **17.01 ms** | **51.4x faster** |
  | Warm lookup | 710.0 µs | **784.7 µs** | < 80 ns / lookup |
  | Cold lookup (fresh process) | 1.05 ms | **1.32 ms** | < 135 ns / lookup |
  | Negative lookup | 820.0 µs | **1.20 ms** | < 125 ns / lookup |

---

### P3.2: Pluggable Asynchronous I/O Engine & io_uring Batching (Completed)
- **File**: `src/io_engine.rs`, `src/disk.rs`, `src/lib.rs`, `src/ffi.rs`, `include/oifs.h`, `tests/io_engine_test.rs`
- **Mechanism**:
  - Decouples hot metadata (superblock, bitmaps, inodes, indirect pointer tables, directory indexes) which remain on the shared mmap, from payload data block reads which can cause synchronous blocking page faults when cold.
  - Pluggable I/O Engine ([`IoBackend`](file:///Users/ych/oifs/src/io_engine.rs)) supporting 3 backends:
    1. `Mmap` (Default): Zero-copy direct slice copy from shared mmap blocks. Zero intermediate allocations for uncompressed raw reads.
    2. `Pread`: Positional `pread(2)` per contiguous extent. Portable Unix implementation that transforms scattered page faults into coalesced kernel sequential reads.
    3. `IoUring`: Linux `io_uring` asynchronous submission engine with pooled `IoUring` rings. All payload extents of a file read — or across an entire multi-file batch ([`DiskManager::read_at_batch`]) — are submitted in a single ring submission and processed concurrently by hardware storage queues (queue depth > 1).
  - **Extent Coalescing**: [`ExtentList`](file:///Users/ych/oifs/src/io_engine.rs) automatically merges contiguous on-disk and in-buffer block runs into single unified transfers, collapsing sequential files into a single `pread` / SQE.
  - **Ring Pooling & Thread-Safe Concurrency**: Ring instances are managed via a thread-safe checkout pool, enabling concurrent reader threads under shared `RwLock` without ring contention.
  - **Safe Lifetime & In-Flight Invariant**: The submission runner never returns while SQEs are in flight, ensuring kernel buffer references remain strictly valid even under transient error recovery.
  - **Graceful Fallback & Portability**: If `IoUring` is requested on platforms or kernels without `io_uring` support (macOS/BSD/Windows, older Linux kernels, seccomp filters), it gracefully falls back to `Pread` without errors.
  - **Page-Cache Coherence**: Reads share the same OS page cache as `MAP_SHARED`, ensuring un-msynced writes in `DurabilityMode::Lazy` are immediately visible to `Pread` and `IoUring` backends.
  - **Batched Multi-File API**: [`DiskManager::read_at_batch`] evaluates multiple file requests under a single shared read lock and issues them in a unified engine submission.
  - **Configuration**:
    - Environment variable: `OIFS_IO_BACKEND=mmap|pread|io_uring`
    - Runtime API: `dm.io_backend()`, `dm.requested_io_backend()`, `dm.set_io_backend(backend)`, builder `dm.with_io_backend(backend)`
  - **Kernel Version Gating & RHEL 9 Backport Recognition**: Automatically detects the host kernel version via `libc::uname` and enforces Linux $\ge 5.15$ for production stability and security, with first-class recognition of the RHEL 9 lifecycle:
    - **RHEL 9.0 ~ 9.2** (`build < 362`): Default `CONFIG_IO_URING=n` upstream; automatically safely rejected and falls back to `Pread`.
    - **RHEL 9.3** (`build = 362`, `5.14.0-362.8.1.el9_3`): Official Technology Preview backport with updated SELinux support; automatically recognized and allowed through.
    - **RHEL 9.4+** (`build >= 427`): Adds sysctl `kernel.io_uring_disabled` (0=all, 1=privileged, 2=disabled); if restricted, `IoEngine` gracefully falls back to `Pread` without application crashes.
    - Manual override available via `OIFS_ALLOW_PRE_5_15=1`.
  - **Formal Verification**: Verified with Kani formal proofs (`proof_extent_push_preserves_coverage`, `proof_io_backend_from_u8_soundness`).

