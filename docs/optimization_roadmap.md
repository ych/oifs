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

### P3.3: Per-mutation msync policy (open decision)
- **Finding**: `create_entry_internal`, `write_data_with_filters` and `delete_file` call `mmap.flush_async()` (MS_ASYNC) on the *entire* mapping after every mutation. In the 10k-create benchmark this costs ~855 of 874 ms (~19 ms without it).
- **Note**: MS_ASYNC only schedules writeback and never waited for durability; data in the shared mapping already survives a process crash. Explicit `flush()` and `Drop` still `msync(MS_SYNC)`.
- **Options**: (a) drop per-op `flush_async`, (b) flush only the touched byte ranges, (c) make it a configurable durability mode.

### P3.2: io_uring Asynchronous I/O Engine (Linux)
- **Problem**: Synchronous mmap page faults block threads when data is not cached in RAM.
- **Proposed Solution**: Optional `io_uring` backend on modern Linux kernels for batched asynchronous disk submission.

