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

## 2. P2 Optimization Blueprints (Next Phase)

### P2.1: `recommend_filters` Heuristic Sampling & Rayon Parallelism
- **Problem**: Currently, `recommend_filters` runs Shannon entropy calculations and full Zstd trials across 14 filter combinations sequentially over the entire file payload. On 100MB+ files, this takes several seconds.
- **Proposed Solution**:
  1. **Prefix / Stride Sampling**: For large files (> 256 KB), evaluate Shannon entropy on representative sample windows (e.g. 64 KB from start, middle, and end) rather than compressing the full multi-megabyte stream.
  2. **Rayon Parallel Evaluation**: Parallelize the 14 candidate pipeline simulations using `rayon::par_iter()`, evaluating candidate compressions simultaneously across all CPU cores.
- **Expected Impact**: Reduces recommendation latency from seconds down to sub-5 milliseconds.

### P2.2: Inode Fixed-Memory Mapping & Zero-Copy Inode Cache
- **Problem**: Inode slots are fixed at 256 bytes on disk (`#[repr(C)]`), but `read_inode_internal` and `write_inode_internal` currently invoke `bincode::deserialize` and `bincode::serialize` on every metadata access.
- **Proposed Solution**:
  1. **Direct Memory View**: Since `Inode` is `#[repr(C)]` with fixed-size types, implement safe in-place casting (or zero-copy transmutation via `bytemuck` / direct field access) to avoid serde overhead.
  2. **LRU Inode Cache**: Maintain a lightweight LRU cache (e.g. 1024 entries) in `DiskManagerInner` for frequently accessed directories and active file inodes.
- **Expected Impact**: Speeds up high-frequency path resolutions, `stat`, and `lookup` by 2x to 5x.

---

## 3. P3 Architectural Blueprints (Long-Term)

### P3.1: Multi-Block Directories & Hash-Indexed Buckets
- **Problem**: Currently, directories reside within a single 4KB block (`inode.blocks[0]`), limiting directories to a few hundred entries.
- **Proposed Solution**: Support directory expansion across multiple blocks and introduce hash-bucket indexing (e.g. 64-bit SipHash) for $O(1) \sim O(\log N)$ lookups in directories containing 10,000+ files.

### P3.2: io_uring Asynchronous I/O Engine (Linux)
- **Problem**: Synchronous mmap page faults block threads when data is not cached in RAM.
- **Proposed Solution**: Optional `io_uring` backend on modern Linux kernels for batched asynchronous disk submission.
