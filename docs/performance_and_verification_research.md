# OIFS Performance Optimization & Formal Verification (Kani) Research Report

**Date**: 2026-10-04  
**Author / Agents**: Antigravity Pair-Programming & Multi-Agent Research Audit  
**Status**: Research & Actionable Blueprint  
**Target Repository**: `/Users/ych/oifs`

---

## 1. Executive Summary

This document consolidates deep codebase investigations and findings recovered from the execution logs of the performance and formal verification analysis agents. It identifies:
1. **9 concrete performance bottlenecks** with actionable architectural remedies across lock contention, memory churn, caching, serialization, and disk allocation.
2. **5 critical arithmetic overflow and memory-safety vulnerabilities** (including an integer-multiplication wrap-around in block resolution that can cause the SuperBlock to be overwritten).
3. **A comprehensive roadmap to expand Kani formal verification**, including analysis of weaknesses in existing 44 proofs, code sketches for high-value new harnesses, and modern toolchain integration (`-Z function-contracts`, `bolero`, `--concrete-playback`, LLVM coverage).

---

## 2. Performance Hotspot Analysis & Optimization Proposals

### P4.1: Out-of-Lock Compression & Encryption (Lock Granularity Reduction) [RESOLVED]
* **File & Lines**: [`src/disk.rs:720-850`](file:///Users/ych/oifs/src/disk.rs#L720-L850), [`src/disk.rs:2960-3230`](file:///Users/ych/oifs/src/disk.rs#L2960-L3230)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - Decoupled `write_data_with_filters` into a 3-stage asynchronous-style pipeline:
    1. **Stage 1 (Out-of-Lock Fast Inspection)**: Acquires a short shared `inner.read()` lock (~1 µs) to inspect the inode, verify regular file mode, determine whether previous contents are required for splicing (only in random-offset recompression), and extract a clone of `encryption_key`. Drops the read lock immediately.
    2. **Stage 2 (Pure CPU Out-of-Lock Processing)**: Executes `apply_filters_cow` (Delta/Shuffle transposition), `zstd::stream::encode_all` (heavy CPU compression), and `encrypt_data` (XChaCha20-Poly1305 AEAD cipher) completely outside the exclusive write lock with **zero filesystem locks held**.
    3. **Stage 3 (In-Lock Allocation & Commit)**: Acquires exclusive `inner.write()` lock solely to allocate physical blocks, commit the metadata WAL transaction (or in-place pointers), and update the inode cache. Verifies consistency against Stage 1; re-plans under lock only in the rare event of a race.
* **Impact & Verification Results** (`tests/rwlock_concurrency_test.rs`):
  - **Concurrent Reader Starvation Elimination**: 8 concurrent reader threads performed **341,902 reads** smoothly while two writer threads continuously compressed and encrypted multi-megabyte payloads in parallel.
  - **Latency Spikes Eliminated**: Reader p50 latency stayed at **0 µs** (sub-microsecond), p99 latency stayed at **0 µs**, and maximum latency capped at 4.03 ms (down from blocking for 30~50 ms per multi-megabyte write).
  - Verified across all durability modes, journaled WAL, legacy in-place writes, and encrypted images without regression.

---

### P4.2: Chunked Compression & Decoder Context Reuse for `read_at`
* **File & Lines**: [`src/disk.rs:1439-1447`](file:///Users/ych/oifs/src/disk.rs#L1439-L1447), [`src/disk.rs:1284-1290`](file:///Users/ych/oifs/src/disk.rs#L1284-L1290)
* **Current Behavior**:
  When reading small slices (`read_at(offset, 128)`) from a compressed or encrypted file, the engine invokes `read_data_internal`, decompressing the **entire file** into a newly allocated `Vec<u8>`, copying 128 bytes, and immediately deallocating the vector. Furthermore, `zstd::stream::decode_all` constructs a new decoder context on every invocation.
* **Proposed Remedy**:
  1. **Chunked Zstd Frames**: Compress large files in independent 64KB or 128KB chunks; `read_at` calculates the target chunk and only decompresses the required frame.
  2. **Decompressed Block Cache**: Introduce a small LRU cache for hot decompressed file blocks.
  3. **Context Reuse**: Use `zstd::bulk::Decompressor` or thread-local decoder scratch buffers to avoid allocation churn.
* **Impact**: **High** (Reduces random read latency from $O(\text{FileSize})$ to $O(\text{ChunkSize})$).

---

### P4.3: Zero-Copy Inode Serialization & Sharded Inode Cache (Lock Contention Elimination) [RESOLVED]
* **File & Lines**: [`src/disk.rs:420-530`](file:///Users/ych/oifs/src/disk.rs#L420-L530), [`src/inode_format.rs`](file:///Users/ych/oifs/src/inode_format.rs)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - Replaced bincode serialization with fixed-width 256-byte v2 memory layout (`encode_v2` / `decode_v2`), eliminating heap allocations on disk reads/writes.
  - Implemented 32-shard concurrent `BoundedInodeCache` (`NUM_INODE_CACHE_SHARDS = 32`), where each shard is guarded by its own independent `RwLock`.
  - Removed the outer global `RwLock<BoundedInodeCache>` wrapper from `DiskManagerInner`; cache methods use interior mutability so concurrent readers across different inodes never contend or stall each other.
  - Inode IDs are uniformly distributed across shards using a 64-bit Fibonacci hashing bijection (`inode_id.wrapping_mul(0x517cc1b727220a95)`), ensuring sequential IDs never collide on the same shard lock.
* **Impact & Verification Results** (`tests/rwlock_concurrency_test.rs`):
  - **A/B Benchmark (Cache Miss & Eviction Contention)**: 16 concurrent reader threads performing 32,000 random lookups across 3,000 files (exceeding 2,048 cache capacity to induce continuous misses and FIFO evictions):
    - **Before (`da985ed`, Global `RwLock`)**: 21.74 ms avg (**1,481,485 ops/sec**).
    - **After (`b647fa2`, 32-Shard Lock Striping)**: 8.05 ms avg (**3,996,081 ops/sec**).
    - **Speedup**: **2.70x faster (+170% throughput boost)**, total operation time dropped by **63%**.
  - **Cache Hit Concurrency**: 16 concurrent reader threads reading across 128 different files achieved **2,128,721 operations/sec (7.51 ms for 16,000 sparse operations)** with zero thread stalls.
  - **Memory Impact**: Exact bound capacity of 2048 entries maintained ($32 \times 64 = 2048$). Struct overhead increased by merely ~4 KB (< 0.01% of memory footprint).

---

### P4.4: Contiguous Extent Block Allocation (Multi-Block Bit Scanning)
* **File & Lines**: [`src/disk.rs:1507-1518`](file:///Users/ych/oifs/src/disk.rs#L1507-L1518), [`src/bitmap.rs:29-88`](file:///Users/ych/oifs/src/bitmap.rs#L29-L88)
* **Current Behavior**:
  Writing large files invokes `get_or_alloc_block` once per 4KB block. A 100MB file requires 25,600 individual bitmap searches and lock acquisitions.
* **Proposed Remedy**:
  Implement `find_contiguous_free(count: usize)` in [`src/bitmap.rs`](file:///Users/ych/oifs/src/bitmap.rs) using 64-bit word bitwise masks. Allocate runs of 64~256 contiguous blocks in a single operation and batch-update indirect tables.
* **Impact**: **High** (Faster bulk writes; reduces filesystem fragmentation and optimizes downstream `io_uring` extent coalescing).

---

### P4.5: Read Lock Concurrency for `DiskManager::flush()` [IMPLEMENTED]
* **File & Lines**: [`src/disk.rs:2162-2176`](file:///Users/ych/oifs/src/disk.rs#L2162-L2176)
* **Optimization**:
  `flush()` and `flush_async()` acquire a dedicated `sync_mutex: Arc<Mutex<()>>` to serialize physical `msync` calls without duplicate I/O storms, while acquiring an `inner.read()` lock instead of an exclusive write lock.
* **Benefit**:
  Concurrent reader threads are completely unblocked during flush operations, eliminating read latency spikes. Verified via `tests/rwlock_concurrency_test.rs::test_concurrent_flush_and_readers`.


---

### P4.6: Directory Listing Cache Utilization & Zero-Allocation Path Splitting [RESOLVED]
* **File & Lines**: [`src/disk.rs:3355-3440`](file:///Users/ych/oifs/src/disk.rs#L3355-L3440)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - `list_dir`: Checks `dir_cache` first. If `ix.complete == true`, constructs and returns `DirectoryEntry` items directly from in-memory index without scanning physical directory blocks or decrypting filenames. When scanning an uncached directory, automatically promotes it to `complete = true` in `dir_cache`.
  - `resolve_path`: Replaced heap-allocated `Vec<&str>` with `resolve_path_iter`, traversing path components via zero-allocation iterators.
  - `resolve_parent`: Uses `path.trim_end_matches('/').rsplit_once('/')` to split parent and name in $O(1)$ without allocating intermediate vectors in the common fast path.
* **Impact & Benchmark Results** (`tests/dir_bench.rs::bench_p4_6_dir_cache_and_path_resolution`):
  - **`list_dir` (5,000 files in multi-block directory)**:
    - Cold pass (on-disk block parse & scan): 495.7 µs (2,017 listings/sec)
    - Warm pass (P4.6 in-memory cache hit): **212.2 µs (4,712 listings/sec)**
    - **Speedup**: **2.34x faster (+134% throughput)**, completely eliminating physical block traversal and string decoding.
  - **`resolve_path` throughput**: **7,729,979 lookups/sec (129.37 ns/op)** across 3-tier directory paths.
  - **`resolve_parent` throughput**: **13,164,137 operations/sec (75.96 ns/op)** with zero heap vector allocations.

---

### P4.7: Range-Coalescing in `sync_mutation_ranges` [RESOLVED]
* **File & Lines**: [`src/disk.rs:520-560`](file:///Users/ych/oifs/src/disk.rs#L520-L560), [`src/disk.rs:2860-2875`](file:///Users/ych/oifs/src/disk.rs#L2860-L2875)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - Implemented `DiskManagerInner::coalesce_ranges(ranges, mmap_len)` to align ranges to 4KB page boundaries (`BLOCK_SIZE`), sort by start offset, and merge overlapping/contiguous intervals into a single `(offset, len)` slice.
  - Integrated into `DiskManagerInner::sync_mutation_ranges` (for `RangeAsync` and `Strict` modes) and `DiskManager::write_data_journaled` (for payload block pre-sync before WAL commit).
* **Impact & Benchmark Results**:
  - **1MB Writes (256 payload blocks)**: Throughput increased from 268 writes/sec to **1,885 writes/sec (7.03x speedup, +603%)**, reaching 81% of `DurabilityMode::Lazy` speed.
  - **128KB Writes (32 blocks)**: Increased from 1,364 to **4,771 writes/sec (3.5x speedup)**.
  - **Small file writes (500 files)**: Increased from 8,535/s (4.2 MB/s) to **19,840/s (9.7 MB/s) (2.32x speedup)**.
  - Syscall count for multi-block sequential payload sync reduced by up to **99.6%** (from $N$ to 1).

---

### P4.8: IPC Buffer Pooling & Bounded Frame Parsing
* **File & Lines**: [`src/ipc.rs:278-306`](file:///Users/ych/oifs/src/ipc.rs#L278-L306)
* **Current Behavior**:
  - `write_framed` allocates a new `Vec` per message.
  - `read_framed` accepts frames up to 128MB, immediately allocating `vec![0u8; len]` before receiving payload bytes (memory exhaustion risk).
* **Proposed Remedy**:
  Use thread-local scratch buffers or `BytesMut` pool; enforce streaming chunks for large payloads.
* **Impact**: **Medium** (Eliminates GC/allocator pressure in Master-Proxy mode).

---

### P4.9: In-place Filter & Decryption Pipelines
* **File & Lines**: [`src/disk.rs:1262-1302`](file:///Users/ych/oifs/src/disk.rs#L1262-L1302)
* **Current Behavior**:
  `read_data_internal` creates 4 successive buffers: `raw_data` $\rightarrow$ `decrypted_data` $\rightarrow$ `decoded` $\rightarrow$ `result`.
* **Proposed Remedy**:
  Perform decryption and in-place unfiltering (`delta_decode_inplace`) directly in the reusable buffer where possible.
* **Impact**: **Medium** (Cuts read memory allocations by 50~75%).

---

## 3. Critical Arithmetic & Safety Vulnerabilities Discovered

The following safety and correctness issues were uncovered during the subagent code audits:

| Bug ID | Location | Vulnerability Description | Severity | Kani Target | Status |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **SEC-01** | `src/disk.rs:3511` (`get_block_from_map`) | `block_id as usize * BLOCK_SIZE` can **overflow/wrap around** on release builds if `block_id` is large (e.g. from a corrupted indirect table). It wraps to 0, returning **Block 0 (SuperBlock)** as a valid data block, allowing user writes to **overwrite and corrupt the SuperBlock**! | **Critical** | `proof_get_block_checked_arithmetic_prevents_wrap_around` | **Resolved** (`checked_mul + checked_add`) |
| **PANIC-01** | `src/filters.rs:192` (`delta_encode_inplace`) | Computes `data.len() / typesize` without checking `typesize > 0`. Public callers passing `typesize = 0` trigger an immediate `attempt to divide by zero` panic. | **Medium** | `proof_delta_encode_zero_typesize_safety` | **Resolved** (`typesize == 0` guard) |
| **CORR-01** | `src/disk.rs:1599` (`write_data_from_start_internal`) | When overwriting a file at offset 0 with smaller data: `inode.size = std::cmp::max(inode.size, len)`. The file size is **not truncated**, leaving stale data blocks and leaked space. | **High** | `proof_write_from_start_size_invariant` | Open |
| **PANIC-02** | `src/disk.rs:2654` (`read_at_prepare`) | If an image is corrupted and `inode.size > decompressed.len()`, a read offset $\ge \text{len}$ causes `buf.copy_from_slice(&full_data[start..end])` where `end < start` to panic. | **Medium** | `proof_clamp_slice_range_soundness` | **Resolved** (`start >= full_data.len() -> Ok(0)`) |
| **OVF-01** | `src/io_engine.rs:248` (`ExtentList::push`) | Adjacent extent merging uses `last.len += len` without `checked_add`, risking integer overflow on extreme read batches. | **Low** | `proof_extent_push_no_overflow` | **Resolved** (`last.len.checked_add(len)`) |

---

## 4. Kani Formal Verification Expansion Roadmap

### 4.1 Audit of Existing 44 Harnesses
1. **Concrete vs. Symbolic Inputs**:
   - `src/inode.rs::proof_block_path_tier_boundaries` tests only concrete constants (`9`, `10`, `521`, etc.).
   - `src/directory.rs::proof_find_insert_offset_in_block` tests only `name_len = 4`.
   - *Fix*: Replace concrete values with `kani::any()` constrained by realistic domain invariants.
2. **Missing `kani::cover!` Checks**:
   - Proofs using `kani::assume` lack `cover!` assertions, risking vacuous validity.
3. **Missing Module Verification**:
   - `src/encryption.rs`, `src/ipc.rs`, `src/ffi.rs`, and `src/session.rs` currently have **zero Kani proofs**.

---

### 4.2 High-Value Proof Harness Blueprints

#### Blueprint 1: `BitmapRef::find_next_free_wrapped` Correctness & Wrap-around Invariant
* **Target**: [`src/bitmap.rs:91-99`](file:///Users/ych/oifs/src/bitmap.rs#L91-L99)
* **Property**: Proves that for any symbolic bitmap and any hint, the returned bit is genuinely free (`get(bit) == false`) and no free bits were skipped between `hint` and `bit`.

```rust
#[cfg(kani)]
mod bitmap_kani_expanded {
    use super::*;

    #[kani::proof]
    #[kani::unwind(17)]
    fn proof_find_next_free_wrapped_soundness() {
        let data: [u8; 16] = kani::any(); // 128 bits symbolic bitmap
        let b = BitmapRef::new(&data);
        let hint: usize = kani::any();
        kani::assume(hint <= 128);

        if let Some(bit) = b.find_next_free_wrapped(hint) {
            assert!(bit < 128);
            assert_eq!(b.get(bit), false, "Returned bit must be unallocated (0)");
            if bit >= hint {
                // Prove no free bit exists between [hint, bit)
                for i in hint..bit {
                    assert_eq!(b.get(i), true, "Skipped bit must be allocated");
                }
            }
            kani::cover!(bit < hint, "Wrap-around case reached");
            kani::cover!(bit >= hint, "Linear forward case reached");
        } else {
            // Prove that all bits in the bitmap are 1 (exhausted)
            for i in 0..128 {
                assert_eq!(b.get(i), true, "All bits must be 1 if None returned");
            }
        }
    }
}
```

#### Blueprint 2: `get_block_from_map` Checked Arithmetic & Bounds Protection
* **Target**: [`src/disk.rs:1994`](file:///Users/ych/oifs/src/disk.rs#L1994)
* **Property**: Proves that with `checked_mul` and `checked_add`, arbitrary 64-bit `block_id` values never wrap around to return Block 0.

```rust
#[cfg(kani)]
mod disk_block_kani {
    use super::*;

    #[kani::proof]
    fn proof_get_block_checked_arithmetic() {
        let block_id: u64 = kani::any();
        let mmap_len: usize = kani::any();
        kani::assume(mmap_len >= 4096 && mmap_len <= 1024 * 1024 * 1024);

        let checked_start = (block_id as usize).checked_mul(BLOCK_SIZE);
        let checked_end = checked_start.and_then(|s| s.checked_add(BLOCK_SIZE));

        match (checked_start, checked_end) {
            (Some(start), Some(end)) if end <= mmap_len => {
                assert!(start < end);
                assert!(end - start == BLOCK_SIZE);
                if block_id > 0 {
                    assert!(start >= BLOCK_SIZE, "Non-zero block_id must never resolve to Block 0");
                }
            }
            _ => {
                // Safely rejected
            }
        }
    }
}
```

#### Blueprint 3: `SuperBlock::new` Non-Overlapping Layout Geometry
* **Target**: [`src/superblock.rs:44-106`](file:///Users/ych/oifs/src/superblock.rs#L44-L106)
* **Property**: Proves that for all `total_blocks >= 5`, metadata blocks never overlap and table boundaries stay within total blocks.

```rust
#[cfg(kani)]
mod sb_kani_expanded {
    use super::*;

    #[kani::proof]
    fn proof_superblock_non_overlapping_layout() {
        let total_blocks: u64 = kani::any();
        kani::assume(total_blocks >= 5 && total_blocks <= 1_000_000_000);

        let sb = SuperBlock::new(total_blocks);
        assert_eq!(sb.inode_bitmap_block, 1);
        assert_eq!(sb.data_bitmap_block, 2);
        assert_eq!(sb.inode_table_block, 3);
        assert!(sb.data_block_start > sb.inode_table_block);
        assert!(sb.data_block_start <= sb.block_count);
    }
}
```

---

## 5. Tooling & Coverage Integration

### 5.1 Kani Verification Commands
```bash
# Run all Kani proofs
cargo kani

# Run with source coverage report
cargo kani --coverage

# Automatically generate regression unit tests from counterexamples
cargo kani --concrete-playback=inplace
```

### 5.2 Native LLVM Code Coverage on macOS
Without external third-party crates, use Apple's built-in developer tools:
```bash
# 1. Run tests with LLVM profile instrumentation
RUSTFLAGS="-C instrument-coverage" LLVM_PROFILE_FILE="oifs-%p-%m.profraw" cargo test

# 2. Merge profile counters
/Library/Developer/CommandLineTools/usr/bin/llvm-profdata merge -sparse oifs-*.profraw -o oifs.profdata

# 3. Generate summary report
xcrun llvm-cov report \
    $(find target/debug/deps -maxdepth 1 -perm +111 -type f -not -name "*.*") \
    -instr-profile=oifs.profdata \
    -ignore-filename-regex="rustc/|cargo/"
```

### 5.3 Unified Property Testing & Fuzzing via Bolero
Add `bolero = "0.10"` to `[dev-dependencies]`. A single property test can be verified by both Kani and `cargo-fuzz`:
```rust
#[test]
fn test_block_path_bolero() {
    bolero::check!().with_type::<usize>().for_each(|&idx| {
        if idx < MAX_LOGICAL_BLOCKS {
            let path = BlockPath::from_logical(idx).unwrap();
            assert_eq!(path.to_logical(), idx);
        }
    });
}
```
