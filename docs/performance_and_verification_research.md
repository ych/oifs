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

### P4.2: Seekable 64KB Chunked Compression for `read_at` & Random COW Writes [RESOLVED]
* **File & Lines**: [`src/inode.rs:30-90`](file:///Users/ych/oifs/src/inode.rs#L30-L90), [`src/inode_format.rs:43-150`](file:///Users/ych/oifs/src/inode_format.rs#L43-L150), [`src/disk.rs:3480-4170`](file:///Users/ych/oifs/src/disk.rs#L3480-L4170), [`src/ffi.rs:180-240`](file:///Users/ych/oifs/src/ffi.rs#L180-L240), [`include/oifs.h:60-75`](file:///Users/ych/oifs/include/oifs.h#L60-L75)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - **Fixed 64KB Logical Chunks**: Fixed chunk size (`CHUNK_SIZE_64K = 64 * 1024`) mapping any file offset to `chunk_idx = offset / 65536`.
  - **Zero-Disruption Isomorphic Block Tree**: Direct and multi-level indirect block pointers are reused directly as 64-bit transparent `ChunkEntry(pub u64)` values:
    - `bits [0..32]`: `start_block: u32` (physical block allocation)
    - `bits [32..40]`: `block_count: u8` (1..16 contiguous 4KB blocks)
    - `bits [40..48]`: `flags: u8` (0 = Empty/Hole, 1 = Compressed Zstd, 2 = Raw Fallback)
    - `bits [48..64]`: `compressed_len: u16` (exact compressed size $\le 65535$)
  - **Format v2 Inode Flag**: `INODE_FLAG_SEEKABLE_64K: u32 = 0x0001` serialized in Format v2 reserved padding at bytes 167..171, ensuring 100% backwards compatibility with v1 and standard stream-compressed files.
  - **$O(1)$ Random Writes (COW Read-Modify-Recompress)**: Rewriting arbitrary bytes at random offsets only decompresses and recompresses the touched 64KB chunks instead of the entire file. Previous chunk blocks are cleanly freed.
  - **$O(1)$ Sliced Reads (`read_at`)**: `read_at_chunked_64k` directly resolves affected chunk entries and decompresses only the required 64KB chunk frames, avoiding memory blow-up on multi-hundred megabyte files.
  - **Anti-Inflation Fallback**: If compressed frame size $\ge \text{chunk\_valid\_len}$ or $\ge 65536$, automatically falls back to uncompressed `FLAG_RAW` storage, avoiding negative compression ratio and CPU overhead.
  - **Allocation & FSCK Consistency**: `collect_inode_blocks` properly extracts physical extents `start_block..start_block + block_count` for seekable inodes, ensuring zero leaked blocks on file deletion and complete FSCK and defragmenter consistency.
  - **Full C/C++ FFI Integration**: Exposed `oifs_write_file_with_policy` and `OIFS_WRITE_POLICY_SEEKABLE_64K (0x08)` for native consumers.
  - **CLI Integration**: Added `--chunked` flag to `oifs put` for seamless shell and multi-process usage.
* **Impact & Verification Results** ([`tests/seekable_chunked_test.rs`](file:///Users/ych/oifs/tests/seekable_chunked_test.rs), [`tests/seekable_chunked_concurrency_test.rs`](file:///Users/ych/oifs/tests/seekable_chunked_concurrency_test.rs)):
  - 16 comprehensive integration tests covering end-to-end roundtrip, random rewind overwriting, anti-inflation fallback, shrink/expand truncation, filesystem remount persistence, zero block leakage validation, and C FFI API.
  - **Multithreaded Concurrency on Rewindable (Seekable) Files**:
    - 4 writer threads concurrently performing unaligned random rewind writes (`offset < size`) while 4 reader threads continuously read via `read_at`.
    - Mutator threads dynamically triggering anti-inflation fallback (RAW vs Compressed transitions) under concurrent reader access.
    - 16 concurrent reader threads performing random aligned/unaligned sliced `read_at` on shared seekable files.
    - 16 parallel threads simultaneously writing non-overlapping 64KB partitioned chunks into a single shared file.
    - Continuous reader threads concurrent with rapid writer chunk replacements (verifying COW atomic replacement prevents torn/corrupted Zstd frames).
    - Concurrent dynamic `truncate` (shrinking and expanding) stress race against active `read_at` readers.
  - **Multiprocess Concurrency**:
    - Real OS processes concurrently invoking CLI `oifs put --chunked`, `oifs get`, and `oifs fsck`.
    - Multi-client Remote IPC proxy session concurrent sliced `read_at` and partitioned chunk writes over Unix domain sockets.
    - Multi-node TCP network session concurrent partitioned 64KB chunk writes and sliced reads on a shared matrix file.
  - Formal Kani proof `proof_chunk_entry_roundtrip_all` exhaustively verifies bitfield pack/unpack invariants for all possible 64-bit inputs.

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

### P4.4: Contiguous Extent Block Allocation (Multi-Block Bit Scanning) [RESOLVED]
* **File & Lines**: [`src/bitmap.rs:105-185`](file:///Users/ych/oifs/src/bitmap.rs#L105-L185), [`src/allocator.rs:40-75`](file:///Users/ych/oifs/src/allocator.rs#L40-L75), [`src/disk.rs:4340-4400`](file:///Users/ych/oifs/src/disk.rs#L4340-L4400)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - Implemented `find_contiguous_free`, `find_contiguous_free_from`, and `find_contiguous_free_wrapped` in `BitmapRef` and `Bitmap`. Scans 64-bit words with fast jump skipping when insufficient free bits remain in a word.
  - Implemented word- and byte-aligned `set_range` and `clear_range` on `Bitmap` for batched bit modification without repetitive individual bit manipulation.
  - Extended `SimpleBlockAllocator` with `allocate_contiguous` and `free_contiguous`.
  - Added `DiskManager::allocate_contiguous_blocks` and `free_contiguous_blocks` (with journaled WAL transaction support via `MetadataOp::AllocContiguousBlocks` / `FreeContiguousBlocks` and legacy in-place updates).
* **Impact & Verification Results** (`tests/contiguous_and_truncate_test.rs::test_contiguous_allocation_and_reuse`):
  - Verified multi-block contiguous run discovery, allocation, and reuse.
  - Formally proved bit searching correctness under Kani (`proof_find_contiguous_free_correctness`).

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

### P4.8: IPC Buffer Pooling & Bounded Frame Parsing [RESOLVED]
* **File & Lines**: [`src/ipc.rs`](file:///Users/ych/oifs/src/ipc.rs) (`write_framed` / `read_framed`), [`tests/ipc_frame_test.rs`](file:///Users/ych/oifs/tests/ipc_frame_test.rs)
* **Status**: **Completed & Verified**
* **Previous Behavior**:
  - `write_framed` allocated a new `Vec` per message.
  - `read_framed` accepted frames up to 128MB, immediately allocating `vec![0u8; len]` before receiving payload bytes (memory exhaustion risk).
* **Implementation Details**:
  - Added three thread-local pooled buffers (`FRAME_WRITE_BUF`, `FRAME_READ_BUF`, `FRAME_CHUNK_BUF`): steady-state framing performs no per-message heap allocation, and buffer capacity is reused across frames.
  - Introduced the public `MAX_FRAME_BYTES` limit, validated on **both** the write and the read path *before* any allocation occurs.
  - `read_framed` now streams the payload in bounded 64 KiB chunks into the pooled buffer, so a peer that announces a huge frame but never sends it cannot force a huge blind allocation.
  - The write path no longer builds a fresh prefix buffer: the 4-byte length prefix is serialized in place in the pooled buffer.
* **Impact**: **Medium** (Eliminates allocator pressure in Master-Proxy mode; removes the blind-allocation DoS vector on the frame length prefix).

---

### P4.9: In-place Filter & Decryption Pipelines [RESOLVED]
* **File & Lines**: [`src/encryption.rs:120-133`](file:///Users/ych/oifs/src/encryption.rs#L120-L133), [`src/filters.rs:67-76`](file:///Users/ych/oifs/src/filters.rs#L67-L76), [`src/disk.rs:2863-2905`](file:///Users/ych/oifs/src/disk.rs#L2863-L2905), [`src/disk.rs:3865-3983`](file:///Users/ych/oifs/src/disk.rs#L3865-L3983)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - Added [`decrypt_data_in_place`](file:///Users/ych/oifs/src/encryption.rs#L120-L133) using `chacha20poly1305::aead::AeadInPlace::decrypt_in_place` and [`unapply_filters_in_place`](file:///Users/ych/oifs/src/filters.rs#L666-L671) so `read_data_internal` decrypts and delta-decodes in-place without intermediate full-payload allocations.
  - Updated [`read_single_chunk_decompressed`](file:///Users/ych/oifs/src/disk.rs#L3865-L3983) to slice contiguous 64K chunk extents directly from `&guard.mmap`, eliminating intermediate block-by-block `Vec<u8>` copies before decryption/decompression.
* **Impact**: **Medium** (Cuts read memory allocations by 50~75% and eliminates intermediate copies on seekable 64K chunk reads).

---

### P4.10: Quadratic $O(N^2)$ Checks in `write_data_journaled`, `stage_payload_write` & `AllocSim` [RESOLVED]
* **File & Lines**: [`src/disk.rs:3240-3250`](file:///Users/ych/oifs/src/disk.rs#L3240-L3250), [`src/journal.rs:2480-2645`](file:///Users/ych/oifs/src/journal.rs#L2480-L2645)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - Replaced `used.contains` in `write_data_journaled` and `AllocSim::is_fresh` with `rustc_hash::FxHashSet<u64>`, and removed the redundant `used.contains(&phys)` linear scan in `stage_payload_write` (where `blk_idx` is strictly monotonic).
* **Impact**: **High** (32 MB journaled initial writes reach **6,077 MB/s** and 32 MB overwrites reach **26,038 MB/s** with zero super-linear degradation).

---

### P4.11: WAL Ring Overflow on Large Writes & Extent Bitmap Op Coalescing [RESOLVED]
* **File & Lines**: [`src/journal.rs`](file:///Users/ych/oifs/src/journal.rs)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - Introduced `MetadataOp::SetDataBitmapRange { start_block: u64, count: u32, allocated: bool }` (tag 5).
  - Implemented `coalesce_ops` grouping bitmap allocations, adjacent indirect pointer slices (`WriteBlockSlice`), and deduplicating inode writes, condensing thousands of contiguous allocations into 14 bytes and shrinking WAL frames by up to 88%.
  - Added simulated indirect pointer write caching in `AllocSim` (`pointers: FxHashMap<(u64, usize), u64>`), eliminating redundant block allocations on large double/triple indirect files.
* **Impact**: **High** (Enables arbitrary multi-megabyte writes under journaling; 32MB writes commit in ~5.3ms at 6,077 MB/s).

---

### P4.12: Avoid Full Inode Block Tree Traversal on Pure Appends [RESOLVED]
* **File & Lines**: [`src/disk.rs:3125-3132`](file:///Users/ych/oifs/src/disk.rs#L3125-L3132)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - Gated `collect_inode_blocks` on `may_free_blocks = old_size > 0 && (case.frees_previous_blocks() || (file_offset == 0 && phys_off == 0 && new_physical < old_size))`.
  - On pure appends and non-shrinking writes, skips traversing pointer trees entirely.
* **Impact**: **High** (Turns append writes from $O(\text{filesize})$ to $O(\Delta N)$).

---

### P4.13: Hardware CRC32C Acceleration & Zero-Copy WAL Frame Encoding [RESOLVED]
* **File & Lines**: [`src/journal.rs:147-212`](file:///Users/ych/oifs/src/journal.rs#L147-L212), [`src/journal.rs:653-795`](file:///Users/ych/oifs/src/journal.rs#L653-L795), [`src/disk.rs:301-395`](file:///Users/ych/oifs/src/disk.rs#L301-L395)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - Implemented hardware-accelerated `crc32c_update` using ARMv8 `__crc32cd`/`__crc32cb` on `aarch64` and SSE4.2 `_mm_crc32_u64`/`_mm_crc32_u8` on `x86_64`, preserving `crc32c_update_sw` for Kani formal verification.
  - Replaced `crc_input` temporary buffer allocations with incremental `crc_frame_checksum`, added `is_already_coalesced` fast path and single-buffer `encode_frame` encoding, and avoided unnecessary 4 KB bitmap copies via `AllocSim::new_data_only` and read-only `BitmapRef::find_next_free_wrapped` in `create_entry_journaled`.
  - Added `DiskManagerInner::coalesce_ranges_with_gap` (1 MB gap bridging for `DurabilityMode::RangeAsync`) and removed duplicate post-apply payload `msync` barriers in `Strict` mode.
* **Impact**: **High** (Journaled `create_file` reaches **1.49M ops/s** — **1.0x–1.1x** of unjournaled legacy; small journaled writes reach **1.75M ops/s** — **1.3x** of legacy; `RangeAsync` 1 MB writes reach **50% of `Lazy`**).

---

### P4.14: Multi-Block Directory Listing Truncation in FFI and MCP [RESOLVED]
* **File & Lines**: [`src/ffi.rs`](file:///Users/ych/oifs/src/ffi.rs), [`src/bin/oifs_mcp.rs`](file:///Users/ych/oifs/src/bin/oifs_mcp.rs)
* **Status**: **Completed & Verified**
* **Implementation Details**:
  - Replaced legacy `inode.blocks[0]` direct scans with `dm.list_dir(dir_inode_id)`.
  - Directory listings with > 110 entries spanning multiple blocks now return complete results across all blocks.
* **Impact**: **Medium** (Correctness & completeness for multi-block directory queries).

---

### P4.15: Eliminate Redundant Full-Block Zeroing Before Overwrite (`SEC-27` Double-Write Bandwidth Waste) [RESOLVED]
* **File & Lines**: [`src/journal.rs:3039-3065`](file:///Users/ych/oifs/src/journal.rs#L3039-L3065) (`stage_payload_write`), [`src/disk.rs:5260-5385`](file:///Users/ych/oifs/src/disk.rs#L5260-L5385) (`allocate_block_unzeroed`, `allocate_block`, `alloc_and_zero_block`, `get_or_alloc_block_for_write`), [`src/disk.rs:1809-1812`](file:///Users/ych/oifs/src/disk.rs#L1809-L1812) (`append_dir_entry_to_dir`), [`src/disk.rs:3366-3395`](file:///Users/ych/oifs/src/disk.rs#L3366-L3395) (`write_buffer_at_offset`)
* **Status**: **Completed & Verified** (see §6.20)
* **Implementation Details**:
  - In `stage_payload_write`, `AllocSim::take_unzeroed_fresh(phys)` tracks whether a freshly allocated block still requires slack zeroing and only zeroes the untouched prefix `slice[..in_blk_off].fill(0)` and suffix `slice[in_blk_off + n..].fill(0)` on the first write. For full-block writes (`in_blk_off == 0 && n == BLOCK_SIZE`), zeroing cost is **0 bytes**.
  - Added `allocate_block_unzeroed` and `get_or_alloc_block_for_write` (returning `(u64, bool)`) so `write_buffer_at_offset` only zeroes untouched prefix/suffix bytes on fresh blocks, and removed the duplicate `slice.fill(0)` calls in `alloc_and_zero_block` and `append_dir_entry_to_dir`.
* **Impact**: **High** (~50% reduction in `mmap` write bandwidth on initial file writes for both journaled and legacy paths while preserving `SEC-27` zero-leak guarantees).

---

### P4.16: Directory Insert Duplicate Offset Scan & $O(B^2)$ Historical Block Probing [RESOLVED]
* **File & Lines**: [`src/disk.rs:423-440`](file:///Users/ych/oifs/src/disk.rs#L423-L440) (`DirIndex`), [`src/disk.rs:1780-1895`](file:///Users/ych/oifs/src/disk.rs#L1780-L1895) (`find_dir_insert_slot`, `append_dir_entry_to_dir`, `append_dir_entry_to_block`), [`src/disk.rs:2435-2510`](file:///Users/ych/oifs/src/disk.rs#L2435-L2510) (`create_entry_journaled`)
* **Status**: **Completed & Verified** (see §6.21)
* **Implementation Details**:
  - `append_dir_entry_to_block` now accepts the pre-computed `insert_offset: usize`, eliminating the duplicate intra-block scan on every directory insertion.
  - `DirIndex` tracks `may_have_holes: bool` and `tail_offset: Option<(u64, u16)>` (`(num_blocks, used_bytes_in_last_block)`), populated during `build_dir_index` and `list_dir` and updated in place on `create_entry_*` and `delete_file*`.
  - Both `append_dir_entry_to_dir` and `create_entry_journaled` resolve the target insertion slot via `find_dir_insert_slot`, using the cached tail offset in $O(1)$ and immediately allocating `new_blk_idx = num_blocks` in $O(1)$ when the tail block fills and `!may_have_holes`.
* **Impact**: **High** (Eliminates quadratic $O(B^2)$ historical block probing during large directory creation and halves per-insert block scanning).

---

### P4.17: Seekable 64K Chunked Read/Write Zero-Copy Borrowing, Parallel Decompression, Sub-Chunk Cache & In-Place AEAD [RESOLVED]
* **File & Lines**: [`src/disk.rs`](file:///Users/ych/oifs/src/disk.rs) (`write_chunked_64k_pipeline_with_mode`, `prepare_chunk_payload`, `read_single_chunk_into`, `read_chunked_64k_internal`, `read_at_chunked_64k`, `ChunkReadCache`), [`src/encryption.rs`](file:///Users/ych/oifs/src/encryption.rs) (`encrypt_with_nonce_prefix`, `decrypt_into_slice`)
* **Status**: **Completed & Verified** (see §6.22 — **5.9x** faster 16 MB `read_data`, **12.0x** faster 4 KB sequential `read_at`, **1.49x** faster 64 MB seekable write)
* **Bottleneck Analysis**:
  1. **Unnecessary 64 KB Heap Allocation + Copy Under Read Lock (Write Stage 1)**: In `write_chunked_64k_pipeline`, when writing a full 64 KB chunk (`chunk_offset_start == 0 && chunk_offset_end >= chunk_valid_len`), Stage 1 allocates `vec![0u8; chunk_valid_len]` and copies `&data[data_start..data_end]` into it while holding `inner.read()`, instead of borrowing `&data[data_start..data_end]` via `Cow::Borrowed`.
  2. **Single-Threaded Sequential Decompression & Per-Chunk `Vec` Allocation**: While `write_chunked_64k_pipeline` parallelizes chunk compression across CPU cores via Rayon (`3.82 ms` for 16 MB), `read_chunked_64k_internal` decompresses chunks sequentially on a single thread (`8.23 ms` for 16 MB — **2.15x slower than compression!**), allocating an intermediate `vec![0u8; 65536]` per 64 KB chunk before copying into `out`.
  3. **16x Repeated Decompression on Sub-Chunk Sequential `read_at`**: `read_at_chunked_64k` has no last-decoded-chunk cache, so 4 KB sequential `read_at` calls over a 16 MB seekable file decompress each 64 KB Zstd frame **16 times** (`99.58 ms` / `160.7 MB/s` vs `7.22 ms` / `2,216 MB/s` for 1 MB `read_at` — **13.8x slower**).
  4. **Out-of-Place Chunk AEAD Encryption/Decryption**: `prepare_chunk_payload` and `read_single_chunk_decompressed` invoke `encrypt_data` / `decrypt_data` (allocating new buffers) rather than `AeadInPlace`.
* **Implementation**:
  - Stage 1 borrows full chunks via `Cow::Borrowed` under `inner.read()` with zero allocations.
  - `read_single_chunk_into` decodes directly into caller destination slices using thread-local AEAD/Zstd scratch buffers, and `read_chunked_64k_internal` / aligned multi-chunk `read_at_chunked_64k` parallelize decoding via Rayon `par_chunks_mut(CHUNK_SIZE_64K)`.
  - Added `ChunkReadCache` (8-slot FIFO cache of `Arc<[u8]>` chunks) invalidated automatically on every inode/file mutation.
  - `prepare_chunk_payload` checks compressed length before AEAD and encrypts directly with `encrypt_with_nonce_prefix`.

---

### P4.18: Lazy `AllocSim` Bitmap Copy & On-the-Fly `MetadataOp` Coalescing in Journaled Writes [OPEN]
* **File & Lines**: [`src/journal.rs:2592-2614, 2774-2803`](file:///Users/ych/oifs/src/journal.rs#L2592-L2614) (`AllocSim::new_data_only`, `sim_child`, `sim_get_or_alloc_block`), [`src/disk.rs:3437-3504, 5815-5834`](file:///Users/ych/oifs/src/disk.rs#L3437-L3504) (`write_data_journaled`, `delete_file_journaled`)
* **Status**: **Proposed (Medium-High Priority)**
* **Bottleneck Analysis**:
  1. **Unconditional 4 KB `data_bitmap` Copy on Overwrites**: `write_data_journaled` always calls `AllocSim::new_data_only` (cloning the 4 KB `data_bitmap` block into a `Vec<u8>`) even when overwriting already-allocated blocks where `AllocSim` never allocates a single block.
  2. **Fragmented `SetDataBitmap` & 8-Byte `WriteBlockSlice` Emission**: `sim_get_or_alloc_block`, `write_data_journaled` (block freeing), and `delete_file_journaled` push one `MetadataOp::SetDataBitmap` per block and allocate a separate 8-byte `Vec<u8>` (`blk.to_le_bytes().to_vec()`) for each indirect pointer entry in `sim_child`. On multi-megabyte writes/deletes, `ops` accumulates thousands of entries, causing `is_already_coalesced(ops)` in `encode_frame` to return `false` and forcing a full `ops.to_vec()` clone plus `BTreeMap` sorting/merging.
* **Proposed Remedy**:
  - Make `AllocSim::data_bitmap` copy-on-write (`Cow<[u8]>`), cloning the 4 KB bitmap only on the first actual block allocation or deallocation.
  - Coalesce contiguous `SetDataBitmap` into `SetDataBitmapRange` and adjacent indirect pointer writes into the tail `MetadataOp::WriteBlockSlice` at push time (`push_bitmap_op` / `push_block_slice_op`), keeping `ops` compact and already coalesced without `BTreeMap` passes.
* **Expected Impact**: **Medium-High** (Eliminates 4 KB bitmap copies on in-place overwrites and removes thousands of small heap allocations on large journaled writes/deletes).

---

### P4.19: `FxHashMap` for `dir_cache` & Encrypted Negative-Lookup Fast Path [RESOLVED]
* **File & Lines**: [`src/disk.rs`](file:///Users/ych/oifs/src/disk.rs) (`dir_cache`, `DirIndex`, `dir_lookup`, `list_dir`)
* **Status**: **Completed & Verified** (see §6.22 — **7.3x** faster encrypted negative lookups, **2.8–3.4x** faster 10k `create_file`; outer lock sharding deferred as single `RwLock<FxHashMap>` read-only path suffices)
* **Bottleneck Analysis**:
  1. **Global `RwLock<std::collections::HashMap>`**: Unlike `inode_cache` (32-shard `FxHashMap`), `dir_cache` uses a single global `RwLock` and standard SipHash `HashMap` for both outer `u64` keys and inner `stored`/`plain` string maps.
  2. **Encrypted Negative Lookups Acquire Write Lock & Re-Run Cipher**: In `dir_lookup` ([`src/disk.rs:1711`](file:///Users/ych/oifs/src/disk.rs#L1711)), `if ix.complete && !encrypted { return Ok(None); }` skips the early return on encrypted images. Every negative lookup on an encrypted directory (including the existence check in every `create_file` call) therefore reads the parent inode, computes `encrypt_filename` (BLAKE2b + XChaCha20-Poly1305), re-acquires `dir_cache.read()`, and then unconditionally acquires `dir_cache.write()` at line 1757 even when `ix.complete` is already `true` and `found.is_none()`.
  3. **Redundant `hash_filename` on Cached `list_dir`**: [`src/disk.rs:5539, 5550`](file:///Users/ych/oifs/src/disk.rs#L5539) recomputes `hash_filename(name)` for every entry on every cached `list_dir` call.
* **Implementation**:
  - Migrated outer `dir_cache` and inner `DirIndex::stored` / `DirIndex::plain` to `FxHashMap`.
  - Added `plain_complete: bool` to `DirIndex`: when `plain_complete` is `true`, negative lookups on encrypted directories return `Ok(None)` under a single read lock with zero SIV encryption and zero write-lock acquisitions; when `ix.complete && found.is_none()`, `dir_lookup` skips `dir_cache.write()`.

---

### P4.20: `defragment_inplace_on` Zero-Fragmentation Short-Circuit, Cache Preservation, `Bitmap::set_range` & Redundant `fsck` Removal [RESOLVED]
* **File & Lines**: [`src/disk.rs`](file:///Users/ych/oifs/src/disk.rs) (`defragment_inplace_on`)
* **Status**: **Completed & Verified** (see §6.22 — **301.8x** faster no-op `defragment(InPlace)`: `6.64 ms` $\rightarrow$ `0.022 ms`, `0` bytes moved)
* **Bottleneck Analysis**:
  1. **Unnecessary Full-Image Relocation When `fragmentation_ratio == 0.0` (`free_runs <= 1`)**: Because `defragment_inplace_on` places all `dir_inodes` blocks ahead of `file_inodes` blocks, calling `defragment(DefragMode::InPlace)` on a brand-new 5,000-file image with zero deletions (`free_runs == 1`, `fragmentation_ratio == 0.0`) needlessly relocates **4,881 blocks (~20 MB)** in `6.64 ms` just to shift file blocks past multi-block directory growth.
  2. **Unconditional Full-Filesystem `verify_integrity_on` (`fsck`)**: Runs a full `fsck` across the entire image after every in-place defragmentation pass—even when `moves.is_empty()` (`0` blocks moved) and during background `auto_defrag` ticks.
  3. **Bit-by-Bit Bitmap Loop**: Executes `for bit in 0..total_used { bitmap.set(bit); }` instead of the word-aligned `bitmap.set_range(0, total_used)`.
  4. **Unnecessary Full Wipe of `inode_cache` & `dir_cache`**: Wipes `guard.inode_cache` (even though Step 5 already updated every modified inode via `write_inode_internal`) and `guard.dir_cache` (updating `tail_offset` only when relocated).
  5. **SipHash `HashMap` & `moves.clone()`**: Relocation tables (`old_to_new`, `moves`, `occupied`, `incoming`) use `std::collections::{HashMap, HashSet}` instead of `FxHashMap`/`FxHashSet`.
* **Implementation**:
  - Exact compaction short-circuit (`already_compacted`: `stats_before.used_blocks == 0 || (stats_before.free_runs <= 1 && bit 0 is allocated)`, so front-hole `[0..0][1..1]` images still compact to `data_block_start`).
  - Single-pass `read_inode_uncached` classification, `FxHashMap`/`FxHashSet` relocation tables, `Bitmap::set_range(0, total_used)`, and in-place `dir_cache` physical-block remapping (`stored` + `tail_offset`) preserving both `inode_cache` and `dir_cache`.

---

### P4.21: In-Place Whole-File Encryption on the Write Path (`encrypt_data_in_place`) [RESOLVED]
* **File & Lines**: [`src/disk.rs`](file:///Users/ych/oifs/src/disk.rs) (`build_full_overwrite_buffer_pure`), [`src/encryption.rs`](file:///Users/ych/oifs/src/encryption.rs) (`encrypt_data_in_place`)
* **Status**: **Completed & Verified** (see §6.22)
* **Bottleneck Analysis**:
  - While the read path uses `decrypt_data_in_place` (P4.9), `build_full_overwrite_buffer_pure` still calls `encrypt_data(final_data.as_ref(), key, &nonce)`, which allocates a new `Vec<u8>` and copies the entire buffer even when `final_data` is already `Cow::Owned(Vec<u8>)` (e.g. after Zstd compression or byte/bit-shuffle filters).
* **Implementation**:
  - Added `encrypt_data_in_place(buf: &mut Vec<u8>, key, nonce, aad)` using `AeadInPlace::encrypt_in_place_detached`, consuming `final_data.into_owned()` directly without allocating a second full-file ciphertext vector.

---

### P4.22: Second-Chance (Clock) Eviction in `BoundedInodeCache` Shards [OPEN]
* **File & Lines**: [`src/disk.rs:487-532`](file:///Users/ych/oifs/src/disk.rs#L487-L532) (`InodeCacheShard`)
* **Status**: **Proposed (Low Priority)**
* **Bottleneck Analysis**:
  - `InodeCacheShard` uses strict FIFO (`order: VecDeque<u64>`). Because `get()` does not record reference activity, hot directory inodes (such as root inode `0` or active parent directories) inserted early are evicted first whenever a workload scans or creates >64 files mapping to the same shard.
* **Proposed Remedy**:
  - Track a lightweight reference bit per cached inode (Second-Chance / Clock policy) so frequently read parent directory and hot file inodes survive sequential scans without requiring a write lock on `get()`.
* **Expected Impact**: **Low** (Improves cache hit rate under mixed directory-lookup + large-scan workloads).

---

### P4.23: $O(N^2)$ Whole-File Rebuild on Sequential Appends/Partial Writes to Encrypted Files [RESOLVED]
* **File & Lines**: [`src/disk.rs`](file:///Users/ych/oifs/src/disk.rs) (`write_data_with_filters`, `write_chunked_64k_pipeline_with_mode`, `commit_prepared_chunked_write`)
* **Status**: **Completed & Verified** (see §6.22 — **108.8x** faster encrypted `CompressionMode::Never` appends: `1,453.41 ms` $\rightarrow$ `13.36 ms`; **6.5x** faster encrypted `CompressionMode::Auto` appends: `89.46 ms` $\rightarrow$ `13.70 ms`)
* **Bottleneck Analysis**:
  - Whenever `encryption_key.is_some()`, `payload_is_transformed` returns `true` and `fast_append` is disabled (`encryption_key.is_none()`). After `SEC-16`, every `file_offset > 0` write (including sequential streaming appends via FFI/IPC/CLI) triggers `WriteCase::Recompress`, which reads, decrypts, splices, and re-encrypts the **entire file from offset 0**. Streaming a 100 MB file in 64 KB writes therefore performs $O(N^2)$ total cryptographic and I/O work.
* **Implementation**:
  - Transparently upgrades non-zero-offset (`file_offset > 0`) writes on encrypted Format-v2+ non-filtered files to `INODE_FLAG_SEEKABLE_64K` (preserving raw encrypted chunks when `CompressionMode::Never` and compressed encrypted chunks otherwise), reclaiming prior whole-file blocks in `commit_prepared_chunked_write` and reducing all subsequent appends and random overwrites to $O(\text{write\_len})$.

---

### P4.24: $O(N^2)$ Full-File Decode on Positional `read_at` for Non-Seekable Compressed/Encrypted Files [RESOLVED]
* **File & Lines**: [`src/disk.rs`](file:///Users/ych/oifs/src/disk.rs) (`DecodedBlobCache`, `read_at_prepare`)
* **Status**: **Completed & Verified** (see §6.22 — **996.7x** faster sequential 4 KB `read_at` on 4 MB stream-compressed file: `1,933.57 ms` $\rightarrow$ `1.94 ms`, `2.1 MB/s` $\rightarrow$ `2,059.8 MB/s`)
* **Bottleneck Analysis**:
  - For non-seekable compressed or encrypted files (`inode.compressed_size > 0 || inode.encrypted`), every `read_at` call invokes `Self::read_data_internal(guard, &inode)` to decrypt and decompress the **entire file** into a freshly allocated `Vec<u8>`, copies out `buf.len()` bytes, and drops the decoded buffer. Sequential 4 KB `read_at` calls over a file of size $N$ incur $O(N^2)$ total decompression/decryption work.
* **Implementation**:
  - Added bounded `DecodedBlobCache` (max 4 entries, 16 MiB per entry, 32 MiB total budget) in `DiskManagerInner`, keyed by `(inode_id, modified_at, size, compressed_size, encryption_nonce)` and invalidated explicitly on every write/truncate/delete/migrate/defrag/reload path (guaranteeing coherence even across same-second overwrites).

---

### P4.25: Single-Block Data & Inode Bitmap Capacity Ceiling (~128 MiB Data Area Limit) [RESOLVED]
* **File & Lines**: [`src/superblock.rs:159-183, 223-247`](file:///Users/ych/oifs/src/superblock.rs#L159-L183) (`SuperBlock::new_with_layout`, `MAX_TRACKED_INODES`)
* **Status**: **Completed & Verified** (see §6.16)
* **Bottleneck Analysis**:
  - Both `inode_bitmap` and `data_bitmap` are hardcoded to a single 4,096-byte block (32,768 bits). Although `Inode` triple-indirect pointers can address up to ~513 GB, the data bitmap can only track 32,768 data blocks ($\approx 128\text{ MiB}$ of usable data), capping the effective capacity of any OIFS image.
* **Implementation**:
  - The v3 layout sizes the bitmaps with the image: the data bitmap covers the whole data area, the inode bitmap the inode table. The spans are *derived* from the recorded offsets, so no new superblock field is needed (the superblock is `bincode`-encoded, and a new field would break every existing image).
  - Every bitmap access now goes through the span helpers (`inode_bitmap_slice`, `data_bitmap_slice`, and their mutable variants); the WAL replay addresses the bits of the whole span instead of a single block.
* **Expected Impact**: **High (Scalability)** (Unlocks multi-gigabyte images matching the 513 GB inode addressing capability).

---

### P4.26: Thread-Local Zstd `CCtx`/`DCtx` Pooling & Single-Pass `decompress_to_buffer` [RESOLVED]
* **File & Lines**: [`src/disk.rs:984, 3129-3137, 3585, 4063, 4215-4222`](file:///Users/ych/oifs/src/disk.rs#L3129-L3137), [`src/filters.rs:761, 839`](file:///Users/ych/oifs/src/filters.rs#L761)
* **Status**: **Completed & Verified** (see §6.13)
* **Bottleneck Analysis**:
  1. **Per-Call Zstd Context Allocation**: Every call to `zstd::stream::encode_all` or `zstd::stream::Decoder::new` allocates and initializes a new Zstd `CCtx` / `DCtx` (hundreds of kilobytes of internal tables) and frees it immediately after a single frame or 64 KB chunk.
  2. **Zero-Initialized `raw_data` & Capped `decoded` Capacity in `read_data_internal`**: At [`src/disk.rs:3065`](file:///Users/ych/oifs/src/disk.rs#L3065), `vec![0u8; physical_size as usize]` zero-fills the entire compressed/raw buffer right before overwriting it; at [`src/disk.rs:3131`](file:///Users/ych/oifs/src/disk.rs#L3131), `Vec::with_capacity((inode.size as usize).min(1024 * 1024))` caps pre-allocation at 1 MiB, forcing repeated geometric reallocations and copies when decompressing files larger than 1 MiB.
  3. **Uncompacted Multi-Frame Zstd Appends**: [`WriteCase::CompressedAppend`](file:///Users/ych/oifs/src/disk.rs#L977-L996) appends a new Zstd frame on every small write without ever compacting, degrading compression ratio and read speed over thousands of appends.
* **Proposed Remedy**:
  - Pool `zstd::bulk::Compressor` and `zstd::bulk::Decompressor` in `thread_local!` storage for single-frame/64K-chunk operations, and use `decompress_to_buffer` directly into a buffer sized to `inode.size` (bounded by `max_decompressed_bytes`).
  - Trigger automatic whole-file recompression when multi-frame append count or physical/logical overhead exceeds a threshold.
* **Expected Impact**: **High** (Eliminates per-chunk `CCtx`/`DCtx` allocation overhead and multi-megabyte reallocation churn during decompression).

---

### P4.27: Bypass `inode_cache` on Bulk Maintenance Scans (`fsck`, `defragment`, `migrate`) & Bitmap-Based `fsck` [RESOLVED]
* **File & Lines**: [`src/disk.rs`](file:///Users/ych/oifs/src/disk.rs) (`read_inode_uncached`, `for_each_inode_block`, `verify_integrity_on`), [`src/directory.rs`](file:///Users/ych/oifs/src/directory.rs) (`for_each_valid_inode_in_block`)
* **Status**: **Completed & Verified** (see §6.22 — **3.5–6.2x** faster `verify_integrity` (`fsck`) on 10k-file images: `2.61 ms` $\rightarrow$ `0.42 ms` with zero `inode_cache` pollution)
* **Bottleneck Analysis**:
  1. **Cache Pollution & Shard Write-Lock Churn on Full-Table Scans**: `verify_integrity_on` (`fsck`), `defragment_safe`, and `defragment_inplace_on` call `read_inode_internal` on every allocated inode in the filesystem, acquiring shard write locks to insert cold inodes and evicting hot working-set inodes from `BoundedInodeCache`.
  2. **Heavy `HashMap<u64, Vec<u64>>` Allocations & Duplicate Directory Reads in `verify_integrity_on`**:
     - `referenced_data_blocks: HashMap<u64, Vec<u64>>` ([`src/disk.rs:7331-7383`](file:///Users/ych/oifs/src/disk.rs#L7331-L7383)) allocates a separate heap `Vec<u64>` for every single referenced data block in the image just to detect `entries.len() > 1` (cross-linked blocks).
     - `allocated_inodes`, `allocated_data_blocks`, `referenced_inodes`, and `visited` all use `std::collections::HashSet<u64>`.
     - During directory traversal ([`src/disk.rs:7347, 7362`](file:///Users/ych/oifs/src/disk.rs#L7347-L7366)), every child inode is decoded inside the parent loop to check `child_inode.mode == FileType::Directory`, and then decoded a second time when popped from `queue` and a third time in the block-collection loop at line 7374.
* **Implementation**:
  - Added `read_inode_uncached`, `for_each_inode_block`, and zero-allocation `for_each_valid_inode_in_block` (skipping UTF-8 `String` allocation during `fsck`).
  - Replaced all `HashMap`/`HashSet`/`Vec<u64>` structures in `verify_integrity_on` with compact `u64`-word bitsets (`ref_inodes`, `seen_data`, `cross_linked_data`) and a single-pass reachable tree walk that decodes each inode once.

---

### P4.28: IPC & Remote Session Payload Copies, Pooled Buffer Shrinkage & Connection Multiplexing [RESOLVED]
* **File & Lines**: [`src/session.rs:741-752, 980-986`](file:///Users/ych/oifs/src/session.rs#L741-L752), [`src/ipc.rs:589-629, 1098, 1242-1258, 1448-1462`](file:///Users/ych/oifs/src/ipc.rs#L589-L629)
* **Status**: **Partially completed** (see §6.14: the pooled buffer retention, the per-request payload clone and the per-request string allocation are fixed; the connection multiplexing and the duplicate inode lookup remain)
* **Bottleneck Analysis**:
  1. **4–5x Payload Copies on Remote `write_data`**:
     - `OifsSession::write_data_with_filters` ([`src/session.rs:983`](file:///Users/ych/oifs/src/session.rs#L983)) copies `data: &[u8]` into `data.to_vec()`.
     - `OifsSession::send_request` ([`src/session.rs:751`](file:///Users/ych/oifs/src/session.rs#L751)) calls `current_client.send(req.clone())` on **attempt 0**, cloning the entire `IpcRequest::WriteData` `Vec<u8>` payload before even sending it once!
     - `read_framed` ([`src/ipc.rs:609-620`](file:///Users/ych/oifs/src/ipc.rs#L609-L620)) reads into a 64 KB `FRAME_CHUNK_BUF` stage buffer and copies (`buf.extend_from_slice`) into `FRAME_READ_BUF`, and `bincode::deserialize` copies the payload out into `IpcRequest::WriteData.data`.
  2. **Unbounded Thread-Local Frame Buffer Retention**: `FRAME_WRITE_BUF` and `FRAME_READ_BUF` never call `shrink_to` after handling a large frame (up to `MAX_FRAME_BYTES = 128 MiB`). Across `MAX_CONCURRENT_PEERS = 256` worker threads, peak RSS can remain permanently pinned at multi-gigabyte levels.
  3. **Single-Stream Mutex Serialization in `IpcClient`**: `IpcClient` wraps a single `Mutex<IpcStream>` ([`src/ipc.rs:1360, 1454`](file:///Users/ych/oifs/src/ipc.rs#L1454)), serializing all concurrent threads in a Proxy process into 1-in-flight request at a time, and `send_request` locks `dm_fallback: Arc<Mutex<Option<Arc<DiskManager>>>>` on every request.
  4. **Per-Request Minor Overheads in Master Worker**:
     - [`src/ipc.rs:1098`](file:///Users/ych/oifs/src/ipc.rs#L1098) allocates `let short_name = req.name().to_string();` on every request even when `tx_worker.is_none()` (and `req.name()` is already `&'static str`).
     - [`IpcServer::handle_request` (`ReadAt`)](file:///Users/ych/oifs/src/ipc.rs#L1242-L1258) calls `dm.read_inode(inode_id)` and then `dm.read_at(inode_id, ...)`, acquiring the read lock and looking up the inode twice.
* **Proposed Remedy**:
  - Pass `&IpcRequest` to `IpcClient::send(&self, req: &IpcRequest)` so `send_request` never clones `req` across the retry loop.
  - Read directly into `FRAME_READ_BUF` spare capacity in 64 KB chunks without the intermediate `FRAME_CHUNK_BUF` copy, and `shrink_to(1024 * 1024)` when thread-local buffer capacity exceeds 1 MiB.
  - Change `SessionEvent::RequestHandled.req_type` (or compute `short_name` only inside `if let Some(tx) = &tx_worker`), and avoid double inode lookup in `IpcRequest::ReadAt`.
* **Expected Impact**: **Medium** (Cuts IPC write memory copies by >50%, bounds idle worker RSS, and reduces per-RPC latency).

---

### P4.29: Micro-Optimizations in `write_data_with_filters`, `list_dir`, `dir_lookup` & `encrypt_filename` [RESOLVED]
* **File & Lines**: [`src/disk.rs`](file:///Users/ych/oifs/src/disk.rs) (`write_data_with_filters`, `list_dir`), [`src/encryption.rs`](file:///Users/ych/oifs/src/encryption.rs) (`encrypt_filename`, `decrypt_filename`, `file_aad_stack`, `chunk_aad_stack`)
* **Status**: **Completed & Verified** (see §6.22)
* **Bottleneck Analysis**:
  1. **Double Read-Lock & Double Inode Lookup in `write_data_with_filters`**: Lines 3687–3692 acquire `self.inner.read()` and call `read_inode_internal` just to check `INODE_FLAG_SEEKABLE_64K`, drop the lock, and immediately re-acquire `self.inner.read()` and call `read_inode_internal` again in Stage 1 (line 3705).
  2. **Unnecessary `raw_entries.clone()` on Unencrypted `list_dir` Slow Path**: At [`src/disk.rs:5569`](file:///Users/ych/oifs/src/disk.rs#L5569), `let mut entries = raw_entries.clone();` clones every `DirectoryEntry` and its `String` filename even when `guard.encryption_key.is_none()`.
  3. **Double Full-Directory Scan on First Cold Negative `dir_lookup`**: At [`src/disk.rs:1746-1764`](file:///Users/ych/oifs/src/disk.rs#L1746-L1764), a cold miss scans all directory blocks in `locate_entry_in_dir`, and then immediately scans all directory blocks a second time in `build_dir_index`.
  4. **Repeated Key Schedule & Multi-Allocation Staging in `encrypt_filename` / `decrypt_filename`**: [`src/encryption.rs:175-186`](file:///Users/ych/oifs/src/encryption.rs#L175-L186) allocates multiple intermediate `Vec<u8>`/`String` buffers per filename and heap-allocates `Vec<u8>` AAD slices on every file/chunk AEAD operation.
* **Implementation**:
  - Merged the `INODE_FLAG_SEEKABLE_64K` dispatch check into Stage 1 of `write_data_with_filters` (eliminating the extra outer read lock and duplicate inode lookup).
  - Eliminated `raw_entries.clone()` on unencrypted `list_dir` by building `DirIndex` directly before moving `raw_entries`.
  - Replaced multi-buffer `encrypt_filename`/`decrypt_filename` with stack-staged `[u8; 512]` in-place AEAD + single-allocation base64 encoding, and replaced per-call `Vec<u8>` AAD builders with zero-allocation stack helpers (`file_aad_stack`, `chunk_aad_stack`).

---

### P4.30: `delete_file` Quadratic $O(N \cdot B)$ Empty-Block Walk & $O(K^2)$ Per-Block String Deserialization Storm [RESOLVED]
* **File & Lines**: [`src/disk.rs`](file:///Users/ych/oifs/src/disk.rs) (`DirIndex`, `delete_file`, `delete_file_journaled`, `is_dir_empty`), [`src/directory.rs`](file:///Users/ych/oifs/src/directory.rs) (`remove_entry_in_block_bytes`, `has_valid_entry_in_block`)
* **Status**: **Completed & Verified** (see §6.22 — **14.4x** faster plain 10k `delete_file`: `52.00 ms` $\rightarrow$ `3.60 ms`; **4.2x** faster encrypted 10k `delete_file`: `23.50 ms` $\rightarrow$ `5.63 ms`)
* **Bottleneck Analysis**:
  - Measured on 10,000 files in a single directory: `delete_file` takes **52.00 ms** (`enc=false`, 124 entries/block) vs **8.58 ms** for `create_file` (**6.1x slower**), and is paradoxically 2.2x slower than `enc=true` (`23.50 ms`, 42 entries/block):
  1. **Missing Physical Block ID in `DirIndex`**: `DirIndex::stored` only maps `stored_name -> inode_id` without recording `phys_blk: u64`. Consequently, `delete_file` ignores `dir_cache` and calls `locate_entry_in_dir` from `blk_idx = 0` on **every delete**. Because emptied leading directory blocks (`0..k`) remain linked, deleting `N` files scans $\sum_{b=0}^{B-1} b \cdot K = O(N \cdot B)$ empty blocks.
  2. **$O(K^2)$ Heap `String` Allocations & Re-Validation Per Block**: Every single `delete_file` calls `read_dir_entries_from_block`, allocating `K` heap `String`s (`~124` per block) and running `validate_filename` `K` times, filters out 1 entry, and calls `rewrite_dir_entries_in_block` to zero the 4 KB block and re-serialize + re-validate `K - 1` entries (~627,750 heap `String` allocations and >1.25M `validate_filename` calls across 10,000 deletes).
  3. **Duplicate `read_inode_internal` Call**: `delete_file` reads `target_inode_id` twice under the write lock (once for `SEC-29` directory emptiness check, and again to collect blocks to free).
* **Implementation**:
  - Stored `(inode_id: u64, phys_blk: u64)` in `DirIndex::stored` so both `delete_file` and `delete_file_journaled` resolve `(target_inode_id, phys_blk)` in **$O(1)$** from `dir_cache`.
  - Added zero-allocation `remove_entry_in_block_bytes` (`copy_within` + trailing zero fill on raw 4 KB block bytes) and zero-allocation `has_valid_entry_in_block` for `is_dir_empty`, and deduplicated the `target_inode` read.

---

### P4.31: Stream-Compressed `read_data_internal` Pooled Zstd Fast Path & Contiguous Mmap Zero-Copy Slice [RESOLVED]
* **File & Lines**: [`src/disk.rs`](file:///Users/ych/oifs/src/disk.rs) (`read_data_internal`, `decompress_stream_payload`)
* **Status**: **Completed & Verified** (see §6.22 — **1.35x** faster small stream-compressed `read_data`: `8.14 ms` $\rightarrow$ `6.05 ms`, `2,584.0 MB/s`)
* **Bottleneck Analysis**:
  1. **Unpooled `zstd::stream::Decoder::new` on Stream-Compressed Files**: `P4.26` wired `zstd_pool::decompress_frame_into` only into seekable 64K chunks; `read_data_internal` still constructs a brand-new `zstd::stream::Decoder` (allocating a fresh `DCtx` and internal streaming buffer) on every `read_data` call for `CompressionMode::Auto`, `Always`, and `Stream`.
  2. **Unnecessary `raw_data` Allocation & Copy on Contiguous Unencrypted Mmap Extents**: When `!inode.encrypted && inode.compressed_size > 0 && guard.io_engine.effective() == IoBackend::Mmap` and the compressed blocks are physically contiguous (the common case), `read_data_internal` allocates and zeroes `vec![0u8; physical_size]` and copies out of `&guard.mmap` before decompressing.
* **Implementation**:
  - Added contiguous physical-run detection in `read_data_internal` so unencrypted `IoBackend::Mmap` compressed files slice `&guard.mmap[start..start + physical_size]` directly with zero intermediate allocation/copy.
  - Added `decompress_stream_payload` using thread-local pooled `zstd_pool::decompress_frame_into` directly into an exact-capacity output `Vec<u8>`, falling back to `zstd::stream::Decoder` only for concatenated multi-frame Zstd streams.

---

### P4.32: Filter Pipeline Single-Allocation In-Place `bitshuffle` & SIMD Prefix-Sum `delta_decode_inplace` [RESOLVED]
* **File & Lines**: [`src/filters.rs`](file:///Users/ych/oifs/src/filters.rs) (`delta_decode_inplace`, `transpose_bit_blocks_inplace`, `bitshuffle_encode`, `bitshuffle_decode`, `FilterPipeline::apply`, `FilterPipeline::unapply_in_place`)
* **Status**: **Completed & Verified** (see §6.22 — **1.63x** faster `bitshuffle_encode`: `4.01 ms` $\rightarrow$ `2.46 ms`, `6,506.8 MB/s`)
* **Bottleneck Analysis**:
  1. **Double Full-Buffer Allocation in `bitshuffle_encode` / `bitshuffle_decode`**: `bitshuffle_encode` (`4.01 ms` / `3.99 GB/s` on 16 MB) allocates `byte_shuffled = shuffle_encode(...)` (`vec![0; len]`) and then allocates a second `out = vec![0u8; len]` for `transpose_8x8_u64`, even though `transpose_8x8_u64` transforms each independent 8-byte word into an 8-byte word **in place**. `bitshuffle_decode` similarly allocates both `unbit` and `shuffle_decode` buffers.
  2. **3.8x Encode/Decode Asymmetry in `delta_decode_inplace`**: On 16 MB (`typesize = 4`), `delta_encode_inplace` reaches **47.8 GB/s (`0.33 ms`)** via auto-vectorized backward differences, whereas `delta_decode_inplace` drops to **12.7 GB/s (`1.26 ms`)** due to the 1-element loop-carried prefix-sum dependency (`slice[i] = slice[i - 1] + slice[i]`).
* **Implementation**:
  - Added `transpose_bit_blocks_inplace` so `bitshuffle_encode`, `FilterPipeline::apply`, and `FilterPipeline::unapply_in_place` perform 8×8 bit-matrix transpositions in place, cutting heap allocations by 50%.
  - Added `#[cfg(not(kani))]` AArch64 NEON and x86_64 SSE2 2x-unrolled Hillis-Steele block prefix-sum kernels in `delta_decode_inplace` for `typesize == 1, 2, 4` while preserving the scalar Kani-verified implementation under `#[cfg(kani)]`.

---

### P4.33: Batched / Deferred Dirty-Range Coalescing for `DurabilityMode::RangeAsync` [OPEN]
* **File & Lines**: [`src/disk.rs:448-485`](file:///Users/ych/oifs/src/disk.rs#L448-L485) (`sync_mutation_ranges`)
* **Status**: **Proposed (Medium Priority)**
* **Bottleneck Analysis**:
  - Measured on 200 MiB journaled images: `RangeAsync` achieves **25,782 ops/s** on 512 B writes vs **373,246 ops/s** in `Lazy` (**6.9%**). Because metadata (`4 KB..4.3 MB`) and freshly allocated data blocks often exceed the 1 MB gap-bridging threshold, every mutation executes multiple synchronous `msync(MS_ASYNC)` syscalls (~15–35 µs kernel VM lock + page-table walk each) while holding `inner.write()`.
* **Proposed Remedy**:
  - Buffer dirty `(start, end)` intervals in an interval set and issue coalesced `flush_async_range` calls outside `inner.write()` or every $M$ mutations / periodic tick (preserving asynchronous OS writeback semantics without paying multiple VM syscalls inside every critical section).
* **Expected Impact**: **Medium-High** (5–10x throughput boost for `DurabilityMode::RangeAsync`).

---

## 3. Critical Arithmetic & Safety Vulnerabilities Discovered

The following safety and correctness issues were uncovered during the subagent code audits:

| Bug ID | Location | Vulnerability Description | Severity | Kani Target | Status |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **SEC-01** | `src/disk.rs:3511` (`get_block_from_map`) | `block_id as usize * BLOCK_SIZE` can **overflow/wrap around** on release builds if `block_id` is large (e.g. from a corrupted indirect table). It wraps to 0, returning **Block 0 (SuperBlock)** as a valid data block, allowing user writes to **overwrite and corrupt the SuperBlock**! | **Critical** | `proof_get_block_checked_arithmetic_prevents_wrap_around` | **Resolved** (`checked_mul + checked_add`) |
| **PANIC-01** | `src/filters.rs:192` (`delta_encode_inplace`) | Computes `data.len() / typesize` without checking `typesize > 0`. Public callers passing `typesize = 0` trigger an immediate `attempt to divide by zero` panic. | **Medium** | `proof_delta_encode_zero_typesize_safety` | **Resolved** (`typesize == 0` guard) |
| **CORR-01** | `src/disk.rs:1599` (`write_data_from_start_internal`), `src/disk.rs:4400` (`truncate_internal`) | When overwriting a file at offset 0 with smaller data: `inode.size = std::cmp::max(inode.size, len)`. The file size was **not truncated**, leaving stale data blocks and leaked space. | **High** | `proof_write_from_start_size_invariant` | **Resolved** (`truncate`, `truncate_path`, `write_data_truncated`) |
| **PANIC-02** | `src/disk.rs:2654` (`read_at_prepare`) | If an image is corrupted and `inode.size > decompressed.len()`, a read offset $\ge \text{len}$ causes `buf.copy_from_slice(&full_data[start..end])` where `end < start` to panic. | **Medium** | `proof_clamp_slice_range_soundness` | **Resolved** (`start >= full_data.len() -> Ok(0)`) |
| **OVF-01** | `src/io_engine.rs:248` (`ExtentList::push`) | Adjacent extent merging uses `last.len += len` without `checked_add`, risking integer overflow on extreme read batches. | **Low** | `proof_extent_push_no_overflow` | **Resolved** (`last.len.checked_add(len)`) |
| **SEC-02** | `src/disk.rs:3889-3938`, `3720-3765` | Seekable 64KB chunked compression completely bypasses disk encryption when `guard.encryption_key.is_some()`. Chunks are written in plaintext directly to physical blocks. | **Critical** | `proof_chunk_encryption_soundness` | **Resolved** (Chunk-level XChaCha20-Poly1305 with SIV deterministic nonces) |
| **SEC-03** | `src/session.rs`, `src/ipc.rs` | Remote IPC Proxy connects to Master over UDS or TCP without password authentication handshake, exposing plaintext reads/writes to unauthorized local or network peers. | **Critical** | - | **Resolved** (`IpcRequest::Authenticate` handshake, Argon2id salt verification, constant-time compare, auto-reject unauthenticated requests) |
| **SEC-04** | `src/ipc.rs`, `src/disk.rs` | `IpcRequest::Defragment` accepts arbitrary `source_path` and `output_path`, allowing malicious clients to copy, rename, and overwrite arbitrary host files. | **High** | - | **Resolved** (Strict canonical backing path match validation, output path parent directory confinement, path traversal rejection) |
| **SEC-05** | `src/disk.rs`, `src/superblock.rs` | Mounting an image only validates `magic == SuperBlock::MAGIC`, omitting checks on `block_size`, `block_count <= mmap.len()`, and table boundaries. | **High** | `proof_superblock_validate_soundness` | **Resolved** (`SuperBlock::validate` verifying block size, count bounds, bitmap & inode table layout geometry) |
| **SEC-06** | `src/journal.rs:836-857` | WAL recovery loop divides by `ring_bytes` (panics if `block_size == 0`) and lacks cycle/progress guards, permitting infinite replay loop DoS from crafted WAL. | **High** | `proof_recover_termination` | **Resolved** (`ring == 0` early exit, `total == 0` break, slice bounds check, `bytes_traversed < ring` termination limit) |
| **SEC-07** | `src/disk.rs` | Unbounded `zstd::stream::decode_all` allows malicious compressed streams to expand into gigabytes, triggering host OOM DoS (decompression bomb). | **Medium** | - | **Resolved** (Bounded streaming decompression via `Decoder::take(max_bytes)`) |
| **SEC-08** | `src/directory.rs` | Directory `dir_block_count` calculates `size.div_ceil(4096)`. Extremely large directory `inode.size` triggers billions of block lookups (CPU starvation DoS). | **Medium** | - | **Resolved** (`MAX_DIR_BLOCKS = 65536` upper bound, `block_size == 0` safety guard) |
| **SEC-09** | `src/session.rs`, `src/ipc.rs` | Deterministic Unix Domain Socket path in shared `/tmp` (`/tmp/oifs_<hash>.sock`) derived with fixed-key SipHash allows local attackers on multi-user systems to pre-create or squat the socket before the master starts, intercepting client proxy connections. | **Medium** | - | **Mitigated** (SessionMode defaults to Standalone; IPC is explicit opt-in via `--ipc`) |
| **SEC-10** | `src/ipc.rs`, `src/session.rs` | Network IPC transport transmits file data and session payloads over unencrypted raw TCP without TLS/mTLS, exposing all operations to network eavesdropping and tampering. | **High** | - | **Mitigated** (see §6.8: the bind address defaults to loopback, a non-loopback bind has to be requested explicitly with `--allow-remote-ipsc` and prints a warning, the password never crosses the wire and the client verifies the master. Transport encryption itself remains an architecture decision: tunnel it over WireGuard/IPsec/SSH) |
| **SEC-11** | `src/ffi.rs` | Several exported C FFI functions (e.g. `oifs_set_io_backend`, `oifs_get_io_backend`) lack `std::panic::catch_unwind`. Unwinding panics across the `extern "C"` boundary invokes Undefined Behavior. | **Medium** | - | **Resolved** (`catch_unwind` wraps on `oifs_set_io_backend`, `oifs_get_io_backend` and `oifs_loaded_path`; `oifs_check_version` is panic-free by construction and kept modelable for Kani) |
| **SEC-12** | `src/encryption.rs`, `src/session.rs` | Encryption keys (`EncryptionKey`) and passwords are kept in standard unzeroized `String`/`Vec<u8>` heap memory, leaving key material exposed in core dumps or memory forensics. | **Low** | - | **Resolved** (`EncryptionKey` derives `Zeroize`/`ZeroizeOnDrop`; CLI password handling now uses `zeroize::Zeroizing<String>` and wipes the raw `read_line` scratch buffer) |
| **SEC-13** | `src/bin/oifs.rs` | Passing `--password <SECRET>` on CLI arguments exposes encryption passwords in plain text in `/proc/<pid>/cmdline` and `ps` process tables to other local users. | **Low** | - | **User Advisory** (`OIFS_PASSWORD` environment variable supported, interactive prompt recommended) |
| **SEC-14** | `src/directory.rs`, `src/disk.rs`, `src/bin/oifs.rs` | Untrusted disk images or callers with directory entry names containing path traversal components (`/`, `\`, `..`, `.`, `\0`, or empty) can escape target export folders or corrupt directory trees. | **Medium** | - | **Resolved** (`validate_filename` in `serialize_into`/`deserialize_from`, `create_entry_internal`/`journaled`, `resolve_parent`, `resolve_path_iter`; sanitized export in `Commands::Get`) |
| **SEC-15** | `src/disk.rs:4410-4450` | Seekable 64K Chunked writes previously allocated contiguous blocks and updated on-disk indirect pointer tables in-place before committing the WAL transaction frame, corrupting replay on crash. | **High** | `test_seekable_chunked_journaled_crash_recovery_and_atomicity` | **Resolved in 1.1.0** (Staged through `AllocSim` + ordered payload flush before `commit_journal_tx`) |
| **SEC-16** | `src/disk.rs:881-1047, 3524-3545` (`plan_write_prepared`, `plan_write`, `write_data_with_filters`) | On encrypted images (`encryption_key.is_some()`), non-zero-offset writes (`file_offset > 0`) to uncompressed or newly created empty files (`old_compressed_size == 0`) fall through to `WriteCase::Raw`, writing **unencrypted plaintext** directly to disk (`inode.encrypted == false`) or overwriting XChaCha20-Poly1305 ciphertext in place and corrupting its Poly1305 tag. Also, shrinking overwrites at `file_offset == 0` on uncompressed encrypted files keep `old_size` via `max(old_size, payload.buffer.len())`, breaking subsequent AEAD decryption. | **Critical** | - | **Resolved** (see §6.1) |
| **SEC-17** | `src/ipc.rs:406` (`read_framed`) | `bincode::deserialize(buf.as_slice())` is called with default unlimited options. A tiny 16-byte frame containing a crafted `u64` `Vec`/`String` length prefix forces `bincode` to pre-allocate gigabytes before reading from `buf`, crashing the Master via OOM. | **High** | - | **Resolved** (`frame_codec` limits the deserializer to the frame size) |
| **SEC-18** | `src/ipc.rs:124-158, 691-760`, `src/session.rs:265-355, 580-607` | (1) `IpcRequest::Authenticate` sends the raw password in cleartext; an attacker who writes a crafted `.{image}.master` rendezvous file can redirect connecting clients and harvest their encryption password. (2) Unencrypted images have zero IPC authentication (`authenticated = !is_encrypted`). (3) `/tmp` Unix sockets lack `0600` permission enforcement. (4) `spawn_client_worker` spawns unbounded OS threads per connection without socket read/write timeouts (Slowloris / thread exhaustion DoS). | **High** | - | **Resolved** (see §6.3) |
| **SEC-19** | `src/encryption.rs:83-137`, `src/disk.rs:3872-3900` | XChaCha20-Poly1305 AEAD passes empty associated data (`b""`). Because all files and 64K seekable chunks share the same master key without binding `(inode_id, chunk_idx, flags)` in AAD, an attacker with write access to `.img` can swap/reorder/rollback encrypted chunks across files or toggle unauthenticated inode filter flags undetected. | **Medium** | - | **Resolved** (see §6.15: the associated data binds the inode id, the chunk index and the payload flags; the image-wide `encryption_version` selects the scheme, so images written before SEC-19 stay readable) |
| **SEC-20** | `src/disk.rs:2743-2798`, `src/journal.rs:924-980` (`resolve_logical_block_id`, `get_or_alloc_block`, `apply_op_in_place`) | Resolving data block pointers from inodes/indirect tables and replaying `MetadataOp::WriteBlockSlice` only check `block_id * BLOCK_SIZE <= mmap.len()` without verifying `block_id >= sb.data_block_start` (and `WriteInode` does not check `inode_id < sb.inode_count`), allowing crafted/corrupted pointers to overwrite SuperBlock, bitmaps, or inode tables. | **Medium** | - | **Resolved** (see §6.5) |
| **SEC-21** | `src/ffi.rs:251, 315, 344, 417, 488, 590` | Several FFI entry points execute `unsafe { CStr::from_ptr(...) }` and `std::slice::from_raw_parts(...)` outside `std::panic::catch_unwind`, and `oifs_write_file_with_policy` silently substitutes `&[]` when `buf.is_null() && buf_size > 0` instead of returning `-1`. | **Medium** | - | **Resolved** (see §6.6) |
| **SEC-22** | `src/bin/oifs_mcp.rs:154, 188-191, 285, 367` | MCP server constructs JSON responses via `format!` without escaping quotes, backslashes, or control characters in filenames/paths/error messages (JSON injection), and computes `size_mb * 1024 * 1024` without overflow checks. | **Medium** | - | **Resolved** (see §6.7) |
| **SEC-23** | `src/auto_defrag.rs:66-82, 165-171, 240-318` | `AutoDefragConfig` does not sanitize `NaN` thresholds (`f64::NAN.clamp(0.0, 1.0)` remains `NaN`) or `check_interval == Duration::ZERO` (100% CPU busy-spin loop), and uses `.lock().unwrap()` / `.read().unwrap()` instead of `crate::lock_unpoisoned`. | **Low** | - | **Resolved** (`AutoDefragConfig::sanitized`, `MIN_CHECK_INTERVAL`, `crate::lock_unpoisoned` / `read_unpoisoned`) |
| **SEC-24** | `src/session.rs:106, 612` | `OifsSession::Remote` retains `password: Option<String>` in plain unzeroized heap memory across the session lifetime for failover reconnection, instead of wrapping it in `zeroize::Zeroizing<String>`. | **Low** | - | **Resolved** (`Option<zeroize::Zeroizing<String>>`) |
| **SEC-25** | `src/disk.rs:5025-5051, 2379-2384, 2627-2634, 5131-5143`, `src/journal.rs:2601-2620`, `src/superblock.rs:224-266` | (1) `inode_slot_range` only checks `end <= guard.mmap.len()` without verifying `inode_id < sb.inode_count`; an out-of-range `inode_id` resolves into the **Data Area**, enabling fake-inode type confusion where user file payload bytes are parsed/mutated as `Inode` structs to read, overwrite, or free arbitrary blocks. (2) Inode/data bitmap allocations scan all 32,768 bits without capping at `sb.inode_count` or `sb.block_count`, allowing allocation past `inode_count` to overwrite `data_block_start` (root directory block). (3) `SuperBlock::validate` does not verify `inode_table_block + inode_count.div_ceil(16) <= data_block_start`. | **Critical** | `proof_usable_inode_count_fits_the_table` | **Resolved** (see §6.9) |
| **SEC-26** | `src/disk.rs:6499-6578, 6695-6756, 6864-7020` (`collect_inode_block_layout`, `defragment_inplace_on`, `defragment_safe`) | `collect_inode_block_layout` ignores `INODE_FLAG_SEEKABLE_64K` and treats 64-bit packed `ChunkEntry` values (`(compressed_len << 48) | (flags << 40) | (block_count << 32) | start_block`) as raw block IDs. Running `defragment_inplace` (or background `auto_defrag`) on an image with seekable-compressed files panics out-of-bounds (`src as usize * BLOCK_SIZE`) or corrupts multi-block chunk extents and packed `ChunkEntry` metadata; `defragment_safe` also checks only `inode.compressed_size > 0` (which is `0` for seekable files). | **High** | - | **Resolved** (see §6.10) |
| **SEC-27** | `src/disk.rs:5131-5143, 5240-5287, 6079-6101`, `src/journal.rs:2895-2945` (`allocate_block`, `get_or_alloc_block`, `stage_payload_write`, `truncate_internal`) | Freed data blocks are not zeroed, and newly allocated data blocks are not zero-initialized before partial writes (`file_offset > 0` within a fresh block) or size-extending `truncate(inode_id, new_size)` (Case 2), leaking up to 4,095 bytes per block of previously deleted files' residual plaintext. | **High** | - | **Resolved** (see §6.11) |
| **SEC-28** | `src/disk.rs:3172-3184, 4031-4096, 2758-2777, 1741-1764, 6113-6129` (`read_at_prepare`, `read_single_chunk_decompressed`, `resolve_logical_block_id`, `append_dir_entry_to_dir`, `truncate_internal`) | Incomplete `SEC-20` `is_data_block` coverage: `read_at_prepare` and `read_single_chunk_decompressed` only check `< block_count` (not `>= data_block_start`), allowing `read_at` or seekable chunk reads on a corrupted inode to leak the SuperBlock, bitmaps, or Inode Table; non-journaled `append_dir_entry_to_dir`, `truncate_internal` slack zeroing, and indirect block pointers (`sib_id`/`dib_id`/`tib_id`) also bypass `is_data_block`. | **Medium** | - | **Resolved** (see §6.18) |
| **SEC-29** | `src/disk.rs:6134-6161, 6214-6255, 5677-5705, 5875-5905` (`truncate_internal`, `clear_logical_block_ptr`, `delete_file_journaled`, `delete_file`) | (1) `truncate_internal` Case 3 calls `clear_logical_block_ptr` and `prune_unused_indirect_blocks` before `commit_journal_tx`, mutating indirect pointer blocks directly in `guard.mmap` without `MetadataOp::WriteBlockSlice` WAL entries (violating the WAL-first invariant). (2) `delete_file` / `delete_file_journaled` delete non-empty directories without checking for child entries, permanently orphaning all child inodes and data blocks in the bitmaps. | **Medium** | - | **Resolved** (see §6.12) |
| **SEC-30** | `src/session.rs:571`, `src/ipc.rs:775-785, 807-814, 1177` | (1) `OifsSession::Direct` always creates an unbounded `mpsc::channel()` and stores `Some(rx)` in `event_rx`; if `take_event_receiver()` is never called, every IPC request handled by the Master appends a `SessionEvent` forever (unbounded memory growth). (2) In `SessionMode::Local`, `bind_unix_listener(&socket_path)` runs before `write_token_file(&token_path, &auth_secret)`, leaving a brief race window where a connecting client reads a stale `.tok` from a previously crashed Master. | **Low / Medium** | - | **Resolved** (see §6.19) |
| **SEC-31** | `src/disk.rs` (`journal_region_start`), `src/superblock.rs` (`new_v3`) | `DiskManager::journal_region_start` hardcoded `JOURNAL_HEADER_BLOCK = 3` even on v3 images (`> 128 MiB` data area) where the data bitmap spans multiple blocks (`blocks 2..2+M`), causing WAL writes and bitmap allocations past block 32,768 to overwrite each other (`Journal(BadMagic)` / cross-linked blocks). | **Critical** | - | **Resolved** (`SuperBlock::journal_header_block`, see §6.22) |
| **SEC-32** | `src/disk.rs` (`write_data_journaled`) | (1) `write_data_journaled` computed `new_physical = max(new_compressed_size, new_size)` instead of `if new_compressed_size > 0 { new_compressed_size } else { new_size }` and gated `prune_stale_pointers` on `file_offset == 0`, leaving stale pointers to freed blocks on shrinking compressed writes/truncates. (2) Reused indirect pointer blocks (`sib`/`dib`/`tib`) were absent from `used`, so `FullOverwrite` / `Recompress` freed still-active indirect tables in the data bitmap. | **High** | - | **Resolved** (`collect_retained_indirect_blocks` + exact `new_physical`, see §6.22) |
| **SEC-33** | `src/journal.rs` (`sim_ensure_root`, `sim_child`, `coalesce_ops`), `src/disk.rs` (`create_entry_journaled`) | Journaled indirect pointer table allocations (`sim_ensure_root`, intermediate `sim_child`) and new directory root blocks (`mkdir`) did not emit a 4 KiB zero-init `WriteBlockSlice`, causing recycled freed blocks to expose residual file/ciphertext bytes as forged block pointers or directory entries (`BlockOutOfRange`). | **High** | - | **Resolved** (WAL 4 KiB zero-init + chronological overlap splicing in `coalesce_ops`, see §6.22) |

### 3.1 Detailed Threat Model & Residual Security Evaluation

1. **Local Multi-User Isolation (SEC-09 & SEC-13)**:
   - *Threat*: In a shared Linux host with multiple untrusted users, `/tmp` is world-writable with sticky bit permissions.
   - *Analysis*: Because `resolve_ipc_socket_path` computes a predictable hash from the canonical image path, an unprivileged local user who discovers the target path could pre-create `/tmp/oifs_<hash>.sock`. When a victim user opens the image with `--ipc`, the client attempts to connect to the attacker's pre-created socket.
   - *Resolution*: In release 1.1.0, `OifsSession` defaults to `Standalone` mode (zero socket files created). When `--ipc` is enabled, the recommended defense is binding sockets inside `$XDG_RUNTIME_DIR` (permissions `0700`) rather than world-writable `/tmp`.

2. **Network Transport Security (SEC-10)**:
   - *Threat*: Remote Master-Proxy synchronization over TCP.
   - *Analysis*: `SEC-03` resolved authentication spoofing by introducing Argon2id salt hashing and constant-time password verification. However, the subsequent TCP communication stream is raw plaintext without TLS. An attacker on the local network segment (or Wi-Fi) can observe file read/write payloads or inject TCP packets.
   - *Remediation*: Restrict default TCP bindings to `127.0.0.1`. Production multi-node OIFS deployments across machines must be tunnelled over WireGuard, IPsec, or SSH port forwarding.

3. **FFI Boundary Panic Safety (SEC-11)**:
   - *Threat*: Panic unwinding across C ABI (`extern "C"`).
   - *Analysis*: In Rust 1.85+, a panic escaping an `extern "C"` function without `catch_unwind` is defined to abort the process, but prior toolchains consider it undefined behavior (UB). Critical file I/O operations (`oifs_read_file`, `oifs_write_file`, `oifs_create_file`) wrap logic in `catch_unwind`, but auxiliary utility functions (`oifs_set_io_backend`, `oifs_get_io_backend`) directly call Rust methods.
   - *Remediation*: Wrap all remaining FFI entry points with `catch_unwind` to return `-1` on error. **Status**: Applied to `oifs_set_io_backend`, `oifs_get_io_backend` and `oifs_loaded_path`; `oifs_check_version` and the version accessors are panic-free by construction (pure integer arithmetic / constants) and therefore need no wrapper.

4. **Cryptographic Material Lifetime (SEC-12)**:
   - *Threat*: Cold-boot attacks, core dump extraction, or process memory scrapers.
   - *Analysis*: `XChaCha20Poly1305` keys are generated via Argon2id and stored in `EncryptionKey`. Rust's standard allocator does not wipe memory upon `drop()`.
   - *Remediation*: Adopt the `zeroize` crate to implement `Zeroize` and `ZeroizeOnDrop` on `EncryptionKey` buffers. **Status**: `EncryptionKey` derives `Zeroize`/`ZeroizeOnDrop`; the CLI now wraps every resolved password (flag, `OIFS_PASSWORD` env, interactive prompt) in `zeroize::Zeroizing<String>` and wipes the raw `read_line` scratch string before returning.

5. **Untrusted Image Defense & Journal Replay Invariants (SEC-15)**:
   - *Threat*: A hostile or corrupted `.img` image file opened by OIFS.
   - *Analysis*: In 1.1.0, Seekable 64K Chunked compression is fully integrated into the WAL journaling subsystem. All block allocations are simulated via `AllocSim`, and pointer table writes are recorded in memory before being committed to the journal ring. Crucially, raw payload blocks are flushed to physical backing pages *before* the WAL frame commits (Ordered Mode), ensuring that torn transactions never point to uninitialized or garbage data.

### 3.2 Second-Round Security Audit Findings (SEC-16 – SEC-24)

1. **Encryption Bypass & AEAD Corruption on Non-Zero-Offset Writes (SEC-16 — Critical)**:
   - *Target*: [`src/disk.rs:881-1047`](file:///Users/ych/oifs/src/disk.rs#L881-L1047) (`plan_write_prepared`, `plan_write`), [`src/disk.rs:3524-3545`](file:///Users/ych/oifs/src/disk.rs#L3524-L3545) (`write_data_with_filters`).
   - *Mechanism*: `is_full_overwrite` only holds when `file_offset == 0`, and the read-modify-rewrite branch (`WriteCase::Recompress`) is gated solely on `if old_compressed_size > 0`. Consequently, on an encrypted filesystem (`encryption_key.is_some()`), any write with `file_offset > 0` to a newly created file (`old_size == 0`) or to an uncompressed encrypted file (`old_compressed_size == 0`, e.g. `CompressionMode::Never` or small `Auto` files) falls through to `WriteCase::Raw`. This writes raw plaintext directly to the image (with `inode.encrypted == false` on new files) or overwrites existing XChaCha20-Poly1305 ciphertext bytes in place, leaking the new plaintext and invalidating the 16-byte Poly1305 authentication tag. Furthermore, in `FullOverwrite` (`file_offset == 0`), `new_size` for uncompressed files uses `std::cmp::max(old_size, payload.buffer.len() as u64)`, so shrinking an encrypted uncompressed file retains trailing stale bytes past the new AEAD tag and causes subsequent reads to fail with `DecryptionFailed`.
   - *Remediation*: Route any non-zero-offset write where `old_compressed_size > 0 || inode.encrypted || encryption_key.is_some() || filter_config.is_active() || inode.filter_typesize > 0` through the read-decrypt-splice-reencrypt (`WriteCase::Recompress`) path, and use exact `payload.buffer.len() as u64` as `new_size` in `FullOverwrite` whenever encryption or filtering is active.

2. **Bincode Deserialization Allocation Bomb Inside Framed IPC (SEC-17 — High)**:
   - *Target*: [`src/ipc.rs:406`](file:///Users/ych/oifs/src/ipc.rs#L406) (`read_framed`).
   - *Mechanism*: While `read_framed` caps the outer frame at `MAX_FRAME_BYTES` (128 MiB) and reads in 64 KiB chunks, calling `bincode::deserialize(buf.as_slice())` without `.with_limit(...)` allows a small frame containing a forged `u64` sequence length prefix to trigger a multi-gigabyte `Vec`/`String` allocation before `bincode` checks remaining buffer bytes.
   - *Remediation*: Deserialize via `bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes().with_limit(buf.len() as u64).deserialize(buf.as_slice())`.

3. **IPC / Network Cleartext Password Phishing, Unauthenticated Local/Network Access & Worker Thread DoS (SEC-18 — High)**:
   - *Target*: [`src/ipc.rs:124-158, 691-760`](file:///Users/ych/oifs/src/ipc.rs#L691-L760), [`src/session.rs:265-355, 580-607`](file:///Users/ych/oifs/src/session.rs#L580-L607).
   - *Mechanism*:
     - Clients connecting via `.{image}.master` send `IpcRequest::Authenticate { password }` in cleartext before verifying the server's identity, allowing a forged `.master` file to harvest passwords.
     - Unencrypted images set `authenticated = !is_encrypted` (`true`), granting full read/write/defragment access without a session token.
     - Unix sockets in `/tmp` are created with default `umask` rather than `0600`.
     - `spawn_client_worker` spawns an unbounded OS thread per connection with no socket read/write timeouts.
   - *Remediation*: Replace plaintext password transmission with a keyed challenge-response (BLAKE2b/HMAC over a server nonce using `EncryptionKey`), enforce `0600` socket permissions, and add connection concurrency limits plus read/write timeouts.

4. **Missing AEAD Associated Data (AAD) Binding for Inodes and Chunks (SEC-19 — Medium)**:
   - *Target*: [`src/encryption.rs:83-137`](file:///Users/ych/oifs/src/encryption.rs#L83-L137), [`src/disk.rs:3872-3900`](file:///Users/ych/oifs/src/disk.rs#L3872-L3900).
   - *Mechanism*: XChaCha20-Poly1305 encrypts file payloads and 64K seekable chunks with empty AAD (`b""`), allowing cross-file or cross-chunk ciphertext swapping/rollback and unauthenticated modification of inode filter flags on disk.
   - *Remediation*: Bind `(inode_id, chunk_idx, flags, filter_config)` into AEAD AAD in the next on-disk format revision.

5. **Missing Data-Area Lower-Bound Check (`block_id >= sb.data_block_start`) on Block Pointers & WAL Replay (SEC-20 — Medium)**:
   - *Target*: [`src/disk.rs:2743-2798`](file:///Users/ych/oifs/src/disk.rs#L2743-L2798), [`src/journal.rs:924-980`](file:///Users/ych/oifs/src/journal.rs#L924-L980).
   - *Mechanism*: `resolve_logical_block_id`, `get_or_alloc_block`, and `apply_op_in_place` (`MetadataOp::WriteBlockSlice`) do not verify `block_id >= sb.data_block_start` (nor does `MetadataOp::WriteInode` verify `inode_id < sb.inode_count`), allowing corrupted or malicious block pointers/WAL frames to overwrite SuperBlock, bitmaps, or inode table blocks.
   - *Remediation*: Enforce `block_id >= sb.data_block_start && block_id < sb.block_count` on data/indirect block accesses and `inode_id < sb.inode_count` on `WriteInode`.

6. **FFI Raw Pointer Dereferences Outside `catch_unwind` & Null Buffer Inconsistency (SEC-21 — Medium)**:
   - *Target*: [`src/ffi.rs:251, 315, 344, 417, 488, 590`](file:///Users/ych/oifs/src/ffi.rs#L251-L598).
   - *Mechanism*: `CStr::from_ptr` and `slice::from_raw_parts` are called before `std::panic::catch_unwind` in several FFI functions, and `oifs_write_file_with_policy` treats `buf == NULL && buf_size > 0` as a 0-byte write instead of returning `-1`.
   - *Remediation*: Move all pointer conversions inside `catch_unwind`, validate `buf_size <= isize::MAX as u64`, and reject `buf.is_null() && buf_size > 0` in `oifs_write_file_with_policy`.

7. **MCP Server Manual JSON String Interpolation & Unchecked Size Multiplication (SEC-22 — Medium)**:
   - *Target*: [`src/bin/oifs_mcp.rs:154, 188-191, 285, 367`](file:///Users/ych/oifs/src/bin/oifs_mcp.rs#L154-L367).
   - *Mechanism*: Filenames, image paths, and error messages are interpolated into JSON via `format!` without escaping `"` or `\` or control characters; `size_mb * 1024 * 1024` can overflow `u64`.
   - *Remediation*: Escape all dynamic strings before JSON formatting and use `saturating_mul` / `checked_mul`.

8. **Auto-Defragmentation `NaN` Threshold, Zero-Interval Busy Spin & Lock Unpoisoning (SEC-23 — Low)**:
   - *Target*: [`src/auto_defrag.rs:66-82, 165-171, 240-318`](file:///Users/ych/oifs/src/auto_defrag.rs#L66-L82).
   - *Mechanism*: `f64::NAN.clamp(0.0, 1.0)` remains `NaN`, `check_interval == Duration::ZERO` causes a 100% CPU spin loop, and internal locks use `.unwrap()` rather than `crate::lock_unpoisoned`.
   - *Remediation*: Guard `threshold.is_nan()`, enforce a minimum `check_interval`, and use `lock_unpoisoned`/`read_unpoisoned`/`write_unpoisoned`.

9. **Unzeroized Session Password Copy in `OifsSession::Remote` (SEC-24 — Low)**:
   - *Target*: [`src/session.rs:106, 612`](file:///Users/ych/oifs/src/session.rs#L612).
   - *Mechanism*: `OifsSession::Remote` stores `password: Option<String>` in heap memory for failover retries without `ZeroizeOnDrop`.
   - *Remediation*: Store `Option<zeroize::Zeroizing<String>>` in `OifsSession::Remote`.

### 3.3 Third-Round Security Audit Findings (SEC-25 – SEC-30)

1. **Inode Table & Data Bitmap Bounds Confusion / Fake Inode Type Confusion (SEC-25 — Critical)**:
   - *Target*: [`src/disk.rs:5025-5051`](file:///Users/ych/oifs/src/disk.rs#L5025-L5051) (`inode_slot_range`), [`src/disk.rs:2379-2384`](file:///Users/ych/oifs/src/disk.rs#L2379-L2384) (`create_entry_journaled`), [`src/disk.rs:2627-2634`](file:///Users/ych/oifs/src/disk.rs#L2627-L2634) (`create_entry_internal`), [`src/disk.rs:5131-5143`](file:///Users/ych/oifs/src/disk.rs#L5131-L5143) (`allocate_block`), [`src/journal.rs:2601-2620`](file:///Users/ych/oifs/src/journal.rs#L2601-L2620) (`AllocSim::alloc_inode`, `AllocSim::alloc_block`), [`src/superblock.rs:224-266`](file:///Users/ych/oifs/src/superblock.rs#L224-L266) (`SuperBlock::validate`).
   - *Mechanism*:
     - **Fake Inode Type Confusion (`inode_id >= sb.inode_count`)**: `inode_slot_range(guard, inode_id)` computes `base + inode_id * 256` and only checks `end <= guard.mmap.len() as u64`, omitting `inode_id < guard.superblock.inode_count`. Because the Data Area immediately follows the Inode Table on disk, passing `inode_id >= sb.inode_count` (via `DiskManager`, `OifsSession`, or `IpcRequest::ReadData`/`WriteData`/`Truncate`/`Stat`) resolves into a **data block**. An attacker or unprivileged IPC peer who writes a crafted 256-byte `Inode` record into their own file's data block can then invoke `read_data(fake_inode_id)`, `write_data(fake_inode_id, ...)`, or `truncate(fake_inode_id, 0)` to read, overwrite, or free arbitrary data blocks belonging to other files.
     - **Inode & Data Bitmap Over-Allocation Past SuperBlock Geometry**: The inode bitmap and data bitmap each occupy a full 4,096-byte block (32,768 bits), regardless of `sb.inode_count` or `sb.block_count`. In `create_entry_internal`, `create_entry_journaled`, `allocate_block`, and `AllocSim::alloc_block`, the allocator scans the entire 4,096-byte bitmap without checking `new_inode_id < sb.inode_count` or `blk < sb.block_count`. On any image where `inode_count < 32,768` (e.g. a 10 MB image with `inode_count = 640`), creating the 641st file allocates `inode_id = 640`, whose 256-byte slot lands at `data_block_start` (Block 43, the root directory's data block), immediately corrupting the root directory. Similarly, allocating past `sb.block_count` returns out-of-bounds block IDs instead of `StorageError::NoSpace`.
     - **SuperBlock Geometry Overlap**: `SuperBlock::validate` checks `self.inode_table_block <= self.data_block_start` rather than verifying that the entire inode table fits before `data_block_start` (`inode_table_block + inode_count.div_ceil(16) <= data_block_start`).
   - *Remediation*:
     - Enforce `if inode_id >= guard.superblock.inode_count { return Err(StorageError::InodeNotFound(inode_id)); }` at the start of `inode_slot_range`.
     - Check `new_inode_id < sb.inode_count` and `blk < sb.block_count` in all allocator paths (`create_entry_internal`, `create_entry_journaled`, `allocate_block`, `allocate_contiguous`, `AllocSim`), returning `StorageError::NoSpace` when exceeded.
     - Verify `self.inode_table_block.checked_add(self.inode_count.div_ceil(16)) <= Some(self.data_block_start)` in `SuperBlock::validate`.
   - *Status*: **Resolved** — see §6.9. The remediation was implemented with a
     stricter bound than proposed: the usable inode range is derived from the
     *real* table capacity (`inode_table_capacity()`), because the superblock's
     `inode_count` is twice the number of 256-byte slots that fit in the table
     (it was sized for the historical 128-byte records).

2. **In-Place & Safe Defragmentation Panic or Corrupt Seekable 64K Chunked Files (SEC-26 — High)**:
   - *Target*: [`src/disk.rs:6499-6578`](file:///Users/ych/oifs/src/disk.rs#L6499-L6578) (`collect_inode_block_layout`), [`src/disk.rs:6864-7020`](file:///Users/ych/oifs/src/disk.rs#L6864-L7020) (`defragment_inplace_on`), [`src/disk.rs:6695-6756`](file:///Users/ych/oifs/src/disk.rs#L6695-L6756) (`defragment_safe`).
   - *Mechanism*:
     - For files with `inode.flags & INODE_FLAG_SEEKABLE_64K != 0`, direct and indirect pointer slots do not store raw block numbers; they store 64-bit packed `ChunkEntry` values (`(compressed_len << 48) | (flags << 40) | (block_count << 32) | start_block`), where each entry spans `block_count` contiguous physical blocks (`start_block .. start_block + block_count`).
     - `collect_inode_block_layout` is completely unaware of `INODE_FLAG_SEEKABLE_64K` and treats every non-zero 64-bit slot value as a single physical block ID `b`. In `defragment_inplace_on` (and background `auto_defrag`), `src as usize * BLOCK_SIZE` overflows or panics out-of-bounds in `guard.mmap.copy_within(s..s + BLOCK_SIZE, d)`, moves only 1 block instead of `block_count` contiguous blocks, and overwrites the packed `ChunkEntry` with a bare block ID (destroying `compressed_len`, `flags`, and `block_count`).
     - In `defragment_safe`, `let is_compressed = inode.compressed_size > 0;` misses seekable files (which set `inode.compressed_size = 0` and use `INODE_FLAG_SEEKABLE_64K`), causing `defragment_safe` to rewrite them with `CompressionMode::Never`.
   - *Remediation*: Make `collect_inode_block_layout` and `defragment_inplace_on` decode `ChunkEntry::decode(raw)`, relocate the entire contiguous `entry.block_count` run together, and write back `ChunkEntry { start_block: new_start, ..entry }.encode()`; in `defragment_safe`, preserve `CompressionMode::Seekable64K` when `inode.flags & INODE_FLAG_SEEKABLE_64K != 0`.

3. **Cross-File Information Disclosure via Unzeroed Data Block Allocation & Sparse Writes / Truncate (SEC-27 — High)**:
   - *Target*: [`src/disk.rs:5131-5143, 5240-5287`](file:///Users/ych/oifs/src/disk.rs#L5240-L5287) (`allocate_block`, `get_or_alloc_block`), [`src/journal.rs:2895-2945`](file:///Users/ych/oifs/src/journal.rs#L2895-L2945) (`stage_payload_write`), [`src/disk.rs:6079-6101`](file:///Users/ych/oifs/src/disk.rs#L6079-L6101) (`truncate_internal` Case 2).
   - *Mechanism*:
     - When files are deleted or truncated, their data blocks are only unmarked in the data bitmap; residual plaintext bytes remain on disk.
     - Neither `allocate_block` (non-journaled) nor `stage_payload_write` (journaled) zeroes newly allocated data blocks when writing only a sub-slice (`offset_in_block > 0` or `write_len < BLOCK_SIZE`). Thus, creating a new uncompressed file and writing 1 byte at `file_offset = 4095` allocates a recycled block and leaves bytes `0..4095` containing the previous file's contents.
     - Similarly, `truncate(inode_id, new_size)` with `new_size > inode.size` (Case 2) updates `inode.size = new_size` without zeroing trailing slack bytes in the last allocated block (`old_size % 4096 .. 4096`), exposing residual data in that block on subsequent reads.
   - *Remediation*: Zero-fill newly allocated data blocks in `allocate_block` and `stage_payload_write` (whenever `offset_in_block > 0 || to_copy < BLOCK_SIZE`), and zero the tail of the final allocated block (`old_size..min(new_size, block_end)`) when extending `inode.size` in `truncate_internal`.

4. **Incomplete `SEC-20` Data-Area Bounds Checks on Read, Seekable Chunk, Directory & Indirect Paths (SEC-28 — Medium)**:
   - *Target*: [`src/disk.rs:3172-3184`](file:///Users/ych/oifs/src/disk.rs#L3172-L3184) (`read_at_prepare`), [`src/disk.rs:4031-4096`](file:///Users/ych/oifs/src/disk.rs#L4031-L4096) (`read_single_chunk_decompressed`), [`src/disk.rs:2758-2777`](file:///Users/ych/oifs/src/disk.rs#L2758-L2777) (`resolve_logical_block_id`), [`src/disk.rs:1741-1764`](file:///Users/ych/oifs/src/disk.rs#L1741-L1764) (`append_dir_entry_to_dir`), [`src/disk.rs:6113-6129`](file:///Users/ych/oifs/src/disk.rs#L6113-L6129) (`truncate_internal` Case 3).
   - *Mechanism*:
     - `read_at_prepare` (used by `read_at` across `Mmap`, `Pread`, and `IoUring` backends) and `read_single_chunk_decompressed` only check against `block_count` / `mmap.len()` without checking `is_data_block(blk)` (`blk >= data_block_start`). A corrupted inode can still leak Block 0 (SuperBlock), bitmaps, or the Inode Table via `read_at` or seekable reads.
     - `resolve_logical_block_id` returns raw `Ok(Some(id))` without checking `is_data_block(id)` (nor checking `sib_id`/`dib_id`/`tib_id` before dereferencing indirect blocks). Callers of `resolve_logical_block_id` on write paths—specifically non-journaled `append_dir_entry_to_dir` and `truncate_internal` slack-zeroing—can therefore overwrite metadata blocks if a pointer is corrupted.
   - *Remediation*: Enforce `is_data_block` inside `resolve_logical_block_id` (for both indirect table blocks and the resolved target block), `walk_payload_blocks` (for indirect table blocks), and `read_single_chunk_decompressed` (`entry.start_block >= data_block_start`).

5. **WAL-First Invariant Violation in `truncate_internal` & Orphan Leak on Deleting Non-Empty Directories (SEC-29 — Medium)**:
   - *Target*: [`src/disk.rs:6134-6161, 6214-6255`](file:///Users/ych/oifs/src/disk.rs#L6134-L6161) (`truncate_internal` Case 3, `clear_logical_block_ptr`), [`src/disk.rs:5677-5705, 5875-5905`](file:///Users/ych/oifs/src/disk.rs#L5677-L5705) (`delete_file_journaled`, `delete_file`).
   - *Mechanism*:
     - In `truncate_internal` Case 3 (`new_size < inode.size` with `journaled == true`), `clear_logical_block_ptr` and `prune_unused_indirect_blocks` mutate indirect pointer blocks directly in `guard.mmap` *before* `commit_journal_tx(&mut guard, &ops)` and without recording `MetadataOp::WriteBlockSlice` in `ops`, violating the WAL-first invariant.
     - Neither `delete_file` nor `delete_file_journaled` verifies whether a target `FileType::Directory` inode is empty before unlinking and freeing it, permanently orphaning all child files and subdirectories in the inode and data bitmaps (`fsck` failure / permanent space leak).
   - *Remediation*: Stage indirect pointer zeroing as `MetadataOp::WriteBlockSlice` operations in `truncate_internal` when `journaled == true`, and check that a directory has zero valid entries before allowing `delete_file` / `delete_file_journaled` to unlink it.

6. **Master Unbounded `SessionEvent` Channel Memory Growth & Local IPC `.tok` Creation Ordering Race (SEC-30 — Low / Medium)**:
   - *Target*: [`src/session.rs:571`](file:///Users/ych/oifs/src/session.rs#L571) (`open_with_mode_retry`), [`src/ipc.rs:775-785, 807-814, 1177`](file:///Users/ych/oifs/src/ipc.rs#L775-L785) (`IpcServer::bind_or_connect`, `emit_event`).
   - *Mechanism*:
     - `OifsSession::Direct` always creates an unbounded `std::sync::mpsc::channel()` (`let (tx, rx) = mpsc::channel();`) and stores `Some(rx)` inside `OifsSession::Direct.event_rx`. Whenever the application does not call `session.take_event_receiver()`, `rx` remains alive for the lifetime of the Master session while `IpcServer::emit_event` pushes a `SessionEvent` on every IPC request (`ReadFile`, `WriteFile`, `CreateFile`, `DeleteFile`, `Truncate`), causing unbounded heap growth proportional to total IPC traffic.
     - In `IpcServer::bind_or_connect` (`SessionMode::Local`), `Self::bind_unix_listener(&socket_path)` is called *before* `Self::write_token_file(&token_path, &auth_secret)`. If a previous Master crashed and left a stale `.tok` file, a concurrent client connecting in the microsecond window between `bind_unix_listener` and `write_token_file` will read the stale token and fail authentication with `PermissionDenied`.
   - *Remediation*: Create the `SessionEvent` channel lazily or use a bounded `sync_channel` with `try_send` drop-oldest semantics, and call `write_token_file(&token_path, &auth_secret)` *before* `bind_unix_listener(&socket_path)`.

### 3.4 Status of the Third-Round Findings

| Finding | Severity | Status |
| :--- | :--- | :--- |
| **SEC-25** — inode table / data bitmap bounds, fake inode type confusion | Critical | **Resolved** (§6.9) |
| **SEC-26** — defragmentation mishandles seekable 64K chunked files (`ChunkEntry` decoding) | High | **Resolved** (§6.10) |
| **SEC-27** — freed / partially written data blocks are not zeroed (cross-file disclosure) | High | **Resolved** (§6.11) |
| **SEC-28** — incomplete data-area bounds checks on the read, chunk, directory and indirect paths | Medium | **Resolved** (§6.18) |
| **SEC-29** — WAL-first violation in `truncate_internal`; orphan leak when deleting a non-empty directory | Medium | **Resolved** (§6.12) |
| **SEC-30** — unbounded `SessionEvent` channel; `.tok` creation ordering race | Low / Medium | **Resolved** (§6.19) |

---

## 4. Kani Formal Verification Expansion Roadmap

### 4.1 Audit of Existing 44 Harnesses
1. **Concrete vs. Symbolic Inputs**:
   - `src/inode.rs::proof_block_path_tier_boundaries` tests only concrete constants (`9`, `10`, `521`, etc.).
   - `src/directory.rs::proof_find_insert_offset_in_block` tests only `name_len = 4`.
   - *Fix*: Replace concrete values with `kani::any()` constrained by realistic domain invariants.
   - **Status**: `proof_block_path_tier_boundaries_symbolic` proves the tier
     classification for *every* logical block index (the tier, the slot bounds
     and the index formula), so a boundary can no longer drift unnoticed.
2. **Missing `kani::cover!` Checks**:
   - Proofs using `kani::assume` lack `cover!` assertions, risking vacuous validity.
   - **Status**: every proof that constrains its input with `kani::assume` now
     carries `kani::cover!` assertions for the interesting branches, so the
     harness cannot pass vacuously.
3. **Missing Module Verification**:
   - `src/encryption.rs`, `src/ipc.rs`, `src/ffi.rs`, and `src/session.rs` currently have **zero Kani proofs**.
   - **Status**: `src/ipc.rs` now has harnesses for the frame-length invariant.
     `src/encryption.rs` and `src/session.rs` still have none: their core
     logic is Argon2id / XChaCha20-Poly1305 / BLAKE2b, which CBMC cannot
     model, so a proof there would be vacuous rather than informative. The
     modelable parts (the SIV transcript layout, the zeroizing key buffer)
     are covered by unit tests instead.

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

**Status: integrated into `scripts/release_check.sh` (step 7/8).** The step
builds the instrumented test binaries, runs the suite, merges the raw profiles
and prints the report. It prefers the `llvm-tools-preview` component of the
active toolchain (the system copy is often too old for the raw profile format
of a current nightly) and asks cargo for the test executable paths instead of
hard-coding `target/release/deps`. The current suite reaches **~77 % region
coverage**.

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

---

## 6. Remediations of the Second-Round Audit

### 6.1 SEC-16 — Encryption Bypass & AEAD Corruption on Non-Zero-Offset Writes

**Root cause.** `plan_write_prepared` decided how to write by looking at the
*compression* state only. A payload that is compressed, filtered or encrypted
is a single blob covering the whole file and can therefore never be patched in
place; only the compressed case was recognised, so every other transformed
payload fell through to `WriteCase::Raw` and was written **untransformed**:

* a new file written at `file_offset > 0` on an encrypted image stored
  **plaintext** on disk (`inode.encrypted` stayed `false`),
* a partial write into an encrypted file overwrote the ciphertext with
  plaintext, destroying the Poly1305 tag (`DecryptionFailed` afterwards),
* a shrinking overwrite at offset 0 kept `max(old_size, payload.len())` as the
  file size, so the reader walked past the stored ciphertext and the AEAD
  authentication failed.

**Fix** (`src/disk.rs`):

* New helper `payload_is_transformed(inode, filter_config, encryption_key)`:
  a payload is transformed when it is compressed, filtered, encrypted, or when
  the *image* carries an encryption key.
* `plan_write_prepared` now routes every transformed payload through the
  read-modify-rebuild path: the file's decoded content is read, the new bytes
  are spliced in, and the whole payload is re-filtered / re-compressed /
  re-encrypted with a fresh nonce. An empty file grows from zero instead of
  failing.
* `plan_write` and `write_data_with_filters` read the existing decoded content
  whenever the payload is transformed, so the plan can rebuild it.
* The size bookkeeping of a transformed payload now uses the length of the
  stored payload instead of `max(old_size, ...)`, so a shrinking overwrite no
  longer leaves the reader past the end of the ciphertext.
* The filter metadata written back to the inode now describes the *effective*
  filter pipeline (the caller's config, or the file's stored one), so the
  payload and the inode always agree on whether the payload is filtered.

**Verification**: `tests/encrypted_partial_write_test.rs` reproduces the three scenarios
(plaintext on disk, partial write corruption, shrinking overwrite) and fails
without the fix.

### 6.2 SEC-17 — Bincode Deserialization Allocation Bomb

`bincode::deserialize` was called without an allocation limit, so a 28-byte
frame announcing a `Vec<u8>` of `u64::MAX` elements made the receiver request
gigabytes before reading a single payload byte.

**Fix** (`src/ipc.rs`): both framing directions now use
`frame_codec(limit)`, which reproduces the legacy wire format exactly (fixint
encoding, trailing bytes allowed) and adds `with_limit`. The read path uses
the frame size as the limit, so a payload can never claim more bytes than the
frame carries.

**Verification**: `tests/ipc_frame_test.rs::test_crafted_length_prefix_cannot_trigger_huge_allocation`.

### 6.3 SEC-18 — IPC Transport & Session Authentication

| Sub-issue | Fix |
| :--- | :--- |
| Cleartext password on the wire | The handshake is now a challenge-response: the client sends a random nonce, the master answers with its own nonce, the image salt and whether the image is encrypted, and both sides prove their key with a keyed BLAKE2b MAC over a direction-separated transcript. The password never leaves the client, and the client verifies the master's MAC, so a redirected client detects a fake master. |
| Unencrypted images had no authentication | A master publishes a random 32-byte session secret; clients must present it (encrypted images still require the filesystem password). |
| `/tmp` socket without permissions | The socket now lives in a per-user private directory (`$XDG_RUNTIME_DIR/oifs` or `<tmp>/oifs-<uid>/oifs`, mode `0700`) and the socket itself is `0600`. The secret is stored in a `0600` token file next to it; the TCP rendezvous file is created with `0600` as well. |
| Unbounded threads / no timeouts | `MAX_CONCURRENT_PEERS` (256) bounds the number of concurrent peers, and every connection gets a 10 s handshake budget; an idle but authenticated session may stay open. |

**Verification**: `tests/ipc_security_test.rs` (6 tests) covers the socket,
token and rendezvous permissions, the rejection of a master that cannot prove
its key, the rejection of unauthenticated peers, and the successful
authentication with the published secret.

### 6.5 SEC-20 — Data-Area Lower-Bound Checks

**Fix** (`src/disk.rs`, `src/journal.rs`):

* `DiskManagerInner::is_data_block(block_id)` is the single predicate for "this
  block id addresses the data area" (`data_block_start <= id < block_count`).
* `walk_payload_blocks` (the read path) ignores a pointer that leaves the data
  area instead of exposing metadata as file content.
* `get_or_alloc_block`, `get_or_alloc_indirect_child` and the indirect-root
  helper reject a forged pointer with an error, so a write can never land in
  the superblock, the bitmaps or the inode table.
* `apply_op_in_place` validates every replayed operation: `WriteBlockSlice` and
  `SetDataBitmap`/`SetDataBitmapRange` must stay inside the data area, and
  `WriteInode`/`SetInodeBitmap` must address a real inode.

**Verification**: `tests/block_pointer_bounds_test.rs` (3 tests) — a file whose
pointer addresses the superblock does not expose it as content, a write
through a forged pointer is rejected while the filesystem stays usable, and
the WAL replay refuses every operation that leaves the data area.

### 6.6 SEC-21 — FFI Boundary Hardening

**Fix** (`src/ffi.rs`):

* New `cstr_to_str()` helper; every `CStr::from_ptr` conversion of a
  caller-provided pointer now happens *inside* the `catch_unwind` closure, so
  a fault while converting the string can never unwind across the
  `extern "C"` boundary.
* `oifs_write_file_with_policy` no longer substitutes an empty slice for a
  null buffer with a non-zero size: it reports the caller error instead of
  silently writing nothing.

### 6.7 SEC-22 — MCP Response Encoding

**Fix** (`src/bin/oifs_mcp.rs`):

* All tool responses are built with `serde_json` (`json_object` / `json_error`
  helpers) instead of `format!`, so quotes, backslashes and control characters
  in file names or error messages are escaped correctly and cannot inject
  JSON.
* The image size is computed with `checked_mul`, so a huge `--size` is
  rejected instead of wrapping around into a tiny image.

**Verification**: `tests/mcp_server_test.rs::test_mcp_tool_response_escapes_hostile_names`
drives the server over stdio with a file name that would break a hand-built
JSON string and asserts that every emitted line (and every embedded tool
payload) parses as JSON.

### 6.8 SEC-10 — Plain-TCP Network Transport (Mitigated)

The IPC transport itself is still plain TCP, so a full fix (TLS/mTLS inside
the protocol) is an architecture decision that the project defers to the
deployment (WireGuard, IPsec or SSH port forwarding). The parts that can be
hardened inside the project are now in place:

* the master binds `127.0.0.1` by default, so the transport is not reachable
  from other machines out of the box;
* a non-loopback bind address is **refused** by the CLI unless the operator
  passes `--allow-remote-ipsc`, and the library prints a warning when such a
  bind is requested;
* the password never crosses the wire (challenge-response, see §6.3) and the
  client verifies that the master holds the same key, so a redirected client
  neither leaks its credentials nor talks to an impostor.

**Verification**: `tests/ipc_security_test.rs::test_non_loopback_bind_requires_an_explicit_opt_in`.

### 6.9 SEC-25 — Inode Table and Data Bitmap Bounds

**Root cause.** The layout code sized the inode table with the *historical*
128-byte `bincode` records (`inodes_per_block = 4096 / 128 = 32`), while the
records have been 256 bytes wide since the v2 inode format. `inode_count` was
therefore twice the number of slots that actually fit between
`inode_table_block` and `data_block_start`, and nothing checked the ids
against the table:

* `inode_slot_range` only compared the end offset with the mapped length, so
  an inode id beyond the table addressed the **data area**. A file containing
  a crafted 256-byte record could be used as a "fake inode" to read or
  overwrite another file, and `write_inode_internal` would write a real inode
  record into a foreign file.
* The inode allocator handed out ids from the bitmap, which tracks 32,768
  ids, so creating more files than the table could hold overwrote the root
  directory's data blocks.
* The data allocator handed out blocks from the data bitmap, which is a full
  4 KiB block: on an image smaller than 128 MB it could return a block beyond
  the end of the image.

**Fix**:

* `SuperBlock::inode_table_capacity()` and `SuperBlock::usable_inode_count()`
  express the real capacity (the table, the bitmap and the claimed count,
  whichever is smallest). The historical `inode_count` is left untouched, so
  existing images keep their layout.
* `inode_slot_range` rejects an inode id outside the usable range, so every
  entry point that takes an inode id (`read_inode`, `read_data`, `write_data`,
  `truncate`, the IPC requests) refuses it.
* The inode allocator (in-place and journaled) never hands out an id beyond
  the table: the in-place path releases the bit and reports "no space", the
  journaled path searches the bitmap with the new bounded
  `BitmapRef::find_next_free_bounded`.
* The data allocator (in-place and `AllocSim`) never hands out a block beyond
  the end of the image, and a contiguous run must fit completely.
* `SuperBlock::validate` rejects an inode count the bitmap cannot track, a
  root inode outside the usable range and an inode table that does not fit
  before the data area.

**Deliberate deviation.** The review also asked for
`block_count - data_block_start <= 32768` in `validate`. That check would
reject every image larger than 128 MB, because the data bitmap only covers
32,768 blocks. Such images are valid today (the space beyond the bitmap is
simply unusable), so rejecting them at mount time would be a breaking change.
The bound is therefore enforced where it matters — the allocator refuses to
hand out a block outside the image — and the bitmap capacity is documented
instead.

**Verification**: `tests/inode_table_bounds_test.rs` (4 tests) — a fake inode
id is refused by every entry point and the victim file stays intact, the
inode allocation stops exactly at the table boundary with the root directory
and the fsck report intact, the data allocation stops at the image boundary,
and `SuperBlock::validate` rejects the impossible layouts. The Kani harness
`proof_usable_inode_count_fits_the_table` proves that the usable inode ids
always fit in the inode table.

### 6.10 SEC-26 — Defragmentation of Seekable 64K Chunked Files

**Root cause.** A seekable 64K file does not store raw block ids in its pointer
slots but packed `ChunkEntry` values
(`(compressed_len << 48) | (flags << 40) | (block_count << 32) | start_block`),
whose payload is a *run* of contiguous blocks. The defragmentation treated
every non-zero slot as a single block id:

* `collect_inode_block_layout` pushed the packed value itself as a payload
  block, so the relocation moved the wrong blocks (or none at all) and the
  rebuilt data bitmap freed the file's real blocks — the file's content was
  lost.
* The pointer rewrite looked the packed value up in the block map, found
  nothing and left the old value in place, so the file kept pointing at the
  blocks that had been moved away.
* `defragment_safe` decided "compressed" from `inode.compressed_size > 0`,
  which is `0` for a seekable file, so it copied the content as a plain file
  while the seekable flag stayed set: the reader then interpreted raw bytes as
  chunk entries.

**Fix** (`src/disk.rs`):

* `collect_inode_block_layout` decodes the chunk entries of a seekable file
  and expands every entry into the run of blocks it addresses.
* New `collect_chunk_entries` returns the packed entries of a seekable file.
* `defragment_inplace_on` builds a map from a packed entry to its relocated
  counterpart (the new start block, the same count, flags and length) and uses
  it in the pointer rewrite. The two value ranges cannot overlap: a non-empty
  entry has `block_count >= 1` and is therefore at least `1 << 32`, far above
  any block id.
* `defragment_safe` copies a seekable file with `CompressionMode::Seekable`,
  so the defragmented image keeps the chunked format (and the flag stays
  consistent with the payload).

**Verification**: `tests/defrag_seekable_chunk_test.rs` (2 tests) — the
in-place and the safe defragmentation both preserve the content of a seekable
file and keep the filesystem consistent. Both tests fail without the fix.

### 6.11 SEC-27 — Zeroed Data Blocks

**Root cause.** A data block that was freed (file deleted, file truncated) kept
its plaintext, and a block that was allocated again was handed to the new owner
without being cleared. A short write into such a block therefore exposed the
content of the previously deleted file, and extending a file left the slack of
its last block untouched.

**Fix** (`src/disk.rs`, `src/journal.rs`):

* `allocate_block` zeroes the block it hands out, so a recycled block never
  exposes the content of its previous owner.
* `stage_payload_write` (the journaled path) zeroes every block that the
  simulation has just allocated (`AllocSim::is_fresh`) before the payload is
  staged into it.
* `truncate_internal` zeroes the slack between the old and the new size when a
  file is extended, so the region that becomes readable is zeroed.

**Verification**: `tests/zeroed_block_reuse_test.rs` (3 tests) — the raw
content of a recycled block never contains the plaintext of the deleted file
(in-place and journaled), and the extended region of a truncated file is
zeroed. The first two tests fail without the fix.

### 6.12 SEC-29 — WAL-First Ordering and Orphaned Directory Children

**Root cause (a) — WAL-first violation.** In the journaled shrink path of
`truncate_internal`, the indirect pointer tables were zeroed directly in the
mapping *before* the transaction was committed. A crash between the mutation
and the commit therefore left the pointer tables cleared while the journal
knew nothing about it: the tail of the file was silently lost and the
recovered image no longer matched the WAL.

**Fix**: `clear_logical_block_ptr` and `prune_unused_indirect_blocks` now
take the transaction's operation list. On a journaled image the zeroing is
staged as `MetadataOp::WriteBlockSlice` operations and applied by the
transaction; on a legacy image the mapping is still written directly. The
metadata therefore changes exactly at the commit point.

**Root cause (b) — orphaned children.** `delete_file` and
`delete_file_journaled` removed a directory without checking whether it still
had entries, so the children (and their blocks) stayed allocated but became
unreachable.

**Fix**: both delete paths now refuse to remove a non-empty directory. The
check runs *before* the parent entry is rewritten, so a refused delete leaves
the filesystem untouched. The CLI already deletes the children first for
`rm --recursive`, so the user-facing behaviour is unchanged.

### 6.13 P4.26 — Pooled Zstd Contexts and Single-Pass Decompression

* New module `src/zstd_pool` keeps one `zstd::bulk::Compressor` per
  compression level and one `zstd::bulk::Decompressor` in thread-local
  storage. A `CCtx`/`DCtx` allocates and initialises hundreds of kilobytes of
  internal tables; allocating one per frame (or per 64 KiB chunk) dominated
  the actual compression work.
* The write paths (the appended frame, the full-overwrite buffer and the
  64 KiB chunk) use the pooled compressor.
* A seekable chunk is a single Zstd frame, so it is now decompressed in one
  pass into a caller-provided buffer with the pooled decompressor instead of
  growing a `Vec` through a streaming reader. The buffer holds a whole chunk,
  because after a truncation the stored frame can still carry the previous
  (longer) content.
* The whole-file read path allocates the expected decompressed size once
  instead of capping the pre-allocation at 1 MiB and growing the buffer
  repeatedly.
* A stream of small appended frames costs a frame header each and compresses
  poorly. The append fast path now compares the physical size against the
  logical one and rewrites the file as a single frame once the overhead
  exceeds a quarter, which keeps the amortised cost of an append constant.

**Verification**: `tests/zstd_context_pool_test.rs` (3 tests) — the pooled
compressor round-trips, the seekable chunks survive the pooled decompressor,
and the frame overhead of a thousand small appends stays bounded.

### 6.14 P4.28 — IPC Payload Copies and Pooled Buffer Retention

* The pooled frame buffers are released again when they grew beyond 1 MiB. A
  single 128 MiB frame previously pinned its buffer for the lifetime of the
  worker thread; with `MAX_CONCURRENT_PEERS` workers the resident memory
  could stay in the multi-gigabyte range.
* `IpcClient::send` takes the request by reference, so the retry loop of
  `OifsSession::send_request` no longer clones the whole `WriteData` payload
  for every attempt.
* The per-request event name is only materialized when a listener exists
  (`req.name()` is a `&'static str`).

The remaining part of P4.28 — multiplexing several connections per proxy
process and the duplicate inode lookup of the `ReadAt` handler — is not
addressed here.

### 6.15 SEC-19 — AEAD Associated Data (Context Binding)

**Root cause.** XChaCha20-Poly1305 was used without associated data. Every file
of an image shares the same master key, and the nonce is the only thing that
distinguishes two ciphertexts, so an attacker with write access to the image
could move a ciphertext to another file, reorder the chunks of a file or roll
a file back to an older version — the Poly1305 tag still verified, because
nothing bound the ciphertext to its context.

**Fix** (`src/encryption.rs`, `src/disk.rs`):

* `encrypt_data` / `decrypt_data` / `decrypt_data_in_place` take the
  associated data and pass it to the AEAD as a `Payload`.
* `file_aad(inode_id)` binds a whole-file ciphertext to its inode;
  `chunk_aad(inode_id, chunk_idx, flags)` binds a seekable chunk to its file,
  its index and its flags.
* The scheme is selected by the image-wide `encryption_version`:
  `ENCRYPTION_SCHEME_LEGACY` (1) keeps the empty associated data, so images
  written before SEC-19 stay readable, and `ENCRYPTION_SCHEME_CONTEXT_BOUND`
  (2) is used by every new image.

**Verification**: `tests/encrypted_context_binding_test.rs` swaps the
ciphertexts of two encrypted files on disk — the read must fail — and
`src/disk.rs::encryption_scheme_tests` proves that a legacy image stays
readable and that new images use the context-bound scheme.

### 6.16 P4.25 — Multi-Block Bitmaps (the v3 layout)

**Root cause.** The inode bitmap and the data bitmap were hardcoded to a
single 4 KiB block, so the data area was capped at 32,768 blocks (128 MiB) and
the inode count at 32,768, no matter how large the image was.

**Fix** (`src/superblock.rs`, `src/disk.rs`, `src/journal.rs`):

* New `SuperBlock::new_v3` lays the image out as
  `[superblock][inode bitmap: N][data bitmap: M][journal: J][inode table: T][data area]`,
  with `N` and `M` derived from the image size.
* The spans are *derived* from the recorded offsets
  (`inode_bitmap_blocks()`, `data_bitmap_blocks()`), so the `bincode`-encoded
  superblock needs no new field — adding one would make every existing image
  unmountable. The layout version is carried by `format_version >= 3`, which
  still means "the v2 (256-byte) inode records".
* The inode table keeps its historical size (1024 blocks), but it now reports
  the 16,384 inodes that actually fit into it instead of the historical 32,768.
* All bitmap accesses (including `allocate_block_unzeroed`, `allocate_contiguous_blocks_internal`, `free_contiguous_blocks_internal`, `delete_file`, `truncate_internal`, `defragment_safe`, `AllocSim::new`/`new_data_only`/`apply_to_image`, and `inode_bitmap_byte_range`/`data_bitmap_byte_range`) go through the span helpers, `SuperBlock::journal_header_block()` places the journal region dynamically after `data_bitmap_block + data_bitmap_blocks()` (preventing overlap with the second data bitmap block on >128 MiB journaled images), and the WAL replay sets the bits of the whole span (a range may cross a block boundary).

**Verification**: `tests/large_image_bitmap_test.rs` checks the layout of a
200 MiB image (a multi-block data bitmap), that a small image keeps a single
bitmap block, that a multi-block image stays consistent through the
allocator, the fsck and the defragmentation, that allocations and frees
beyond the first 32,768 data blocks (`test_p425_allocate_and_free_beyond_first_bitmap_block`)
correctly set and clear bits in the second bitmap block, and that a 512 MiB
journaled image (`test_journal_does_not_overlap_a_multi_block_data_bitmap`)
writes >50,000 blocks and reopens cleanly without corrupting the WAL header.

### 6.17 Format Review — What Else Can Be Improved

While the format was being revised, the remaining opportunities were
reviewed. They are **not** implemented, because each of them needs a decision
that changes the on-disk contract:

| Opportunity | Size | Blocker |
| :--- | :--- | :--- |
| The inode record reserves 85 bytes per slot (`[171..256)`) | 85 B × inode count (2.7 MB for 32,768 inodes) | The `Inode` struct is also the `bincode` payload of the **v1** inode format, so a new field changes the v1 encoding and breaks the legacy images. A separate "v1 mirror" struct (or a custom decoder) is needed first. The space is the natural home for a per-file encryption scheme, a Zstd frame count (which would make the append compaction exact instead of the current ratio heuristic) and a chunk-table pointer. |
| The superblock block is 4 KiB but the struct uses ~100 bytes | ~4 KB per image | Same reason: the superblock is `bincode`-encoded, so a new field breaks every existing image. A fixed 4 KiB superblock record (version + length + payload) would allow a checksum, a feature bitmap and the layout parameters. |
| The v3 layout still caps the inode table at 1024 blocks (16,384 inodes) | — | A larger table trades data area for inodes; the ratio should be a configurable image property, which needs a superblock field (see above). |
| The 64 KiB chunk size is fixed in the inode flags | — | A per-image chunk size would need a new flag bit (the reserved inode tail has room) and a format-version gate. |

The two changes that are implemented (the context-bound AEAD and the
multi-block bitmaps) are the ones that could be made **backward compatible**:
the AEAD scheme is selected by the existing `encryption_version` field, and the
v3 layout derives its bitmap spans from the recorded offsets, so an image
written by an older version keeps its layout and stays readable.

### 6.18 SEC-28 — Complete Data-Area Bounds Enforcement

**Fix** (`src/disk.rs`, `src/journal.rs`):

* `read_at_prepare` now filters physical block IDs through `guard.is_data_block(blk)` instead of `< block_count`, so positional reads (`Mmap`, `Pread`, `IoUring`) never expose metadata blocks (`0..data_block_start`) when an inode pointer is corrupted.
* `read_single_chunk_decompressed` validates `entry.start_block >= data_block_start` and `end_block <= block_count` before reading a seekable 64K chunk extent, and `set_logical_chunk_entry` validates indirect table pointers (`sib`, `dib`, `tib`) with `guard.is_data_block(...)` before writing.
* `walk_payload_blocks` checks `guard.is_data_block` on `sib_id`, `dib_id`, and `tib_id` before reading indirect pointer tables.
* `find_dir_insert_slot`, `append_dir_entry_to_dir`, `create_entry_journaled`, `truncate_internal` (both extension slack-zeroing and shrink block collection), `clear_logical_block_ptr`, `zero_block_ptr`, and `prune_unused_indirect_blocks` enforce `guard.is_data_block(...)` on all resolved data and indirect blocks.
* `AllocSim::is_data_block` enforces `blk >= data_block_start && blk < block_count` in `sim_ensure_root`, `sim_child`, and `sim_get_or_alloc_block`.

**Verification**: `tests/block_pointer_bounds_test.rs::test_sec28_read_at_chunked_and_truncate_never_touch_metadata_blocks`.

### 6.19 SEC-30 — Bounded `SessionEvent` Channel & Atomic Token File Publication

**Fix** (`src/session.rs`, `src/ipc.rs`):

* Replaced the unbounded `std::sync::mpsc::channel()` in `OifsSession::Direct` with `mpsc::sync_channel(MAX_PENDING_SESSION_EVENTS)` (`1024`), and switched `IpcServer::emit_event` to non-blocking `try_send`. If the caller never consumes `take_event_receiver()`, queue memory is strictly bounded to 1,024 events and never stalls IPC workers.
* `IpcServer::write_token_file` writes the 32-byte session secret to a mode-`0600` process-scoped temporary file (`.tok.tmp.<pid>`) and atomically renames it over `token_path` (`std::fs::rename`), eliminating any partial-write or stale-token race window.

**Verification**: `tests/ipc_security_test.rs::test_sec30_session_event_channel_is_bounded`.

### 6.20 P4.15 — Prefix/Suffix-Only Fresh-Block Zeroing

**Fix** (`src/journal.rs`, `src/disk.rs`):

* Added `AllocSim::take_unzeroed_fresh(phys)` in `src/journal.rs`: `stage_payload_write` only zeroes the untouched prefix (`slice[..in_blk_off].fill(0)`) and suffix (`slice[in_blk_off + n..].fill(0)`) on the first write to a freshly allocated block. Full-block writes (`in_blk_off == 0 && n == BLOCK_SIZE`) write **0 zero bytes**, eliminating the 2x memory-bus bandwidth penalty while maintaining `SEC-27` zero-disclosure guarantees.
* Added `allocate_block_unzeroed` and `get_or_alloc_block_for_write` (returning `(u64, bool)`) in `src/disk.rs` so `write_buffer_at_offset` only zeroes untouched prefix/suffix bytes on fresh blocks, and removed redundant `slice.fill(0)` calls in `alloc_and_zero_block` and `append_dir_entry_to_dir`.

### 6.21 P4.16 — $O(1)$ Tail-Cached Directory Insertion & Hole-Gated Probing

**Fix** (`src/disk.rs`):

* `append_dir_entry_to_block` accepts the pre-computed `insert_offset: usize`, eliminating the duplicate intra-block entry scan on every directory insertion.
* `DirIndex` tracks `may_have_holes: bool` (set to `true` when `delete_file` or `delete_file_journaled` removes an entry) and `tail_offset: Option<(u64, u16)>` (`(num_blocks, used_bytes_in_last_block)`).
* `find_dir_insert_slot` checks `tail_offset` in $O(1)$ and only probes earlier directory blocks `0..num_blocks - 1` when `may_have_holes` is `true`, turning multi-block directory population from $O(B^2)$ block scans into $O(1)$ per insert.

### 6.22 P4.17 / P4.19–P4.21 / P4.23–P4.24 / P4.27 / P4.29–P4.32 & SEC-31..SEC-33 — Algorithmic, Parallelism, Cache & SIMD Optimization Wave

**Fixes** (`src/disk.rs`, `src/directory.rs`, `src/encryption.rs`, `src/filters.rs`, `src/journal.rs`, `src/superblock.rs`):

1. **Positional `read_at` & Stream Decompression (`P4.24`, `P4.31`)**:
   * Added bounded `DecodedBlobCache` (4 slots, 16 MiB/entry, 32 MiB cap) in `DiskManagerInner` with explicit mutation invalidation, turning sequential `read_at` on non-seekable compressed/encrypted files from $O(N^2)$ into $O(N)$.
   * `read_data_internal` slices physically contiguous unencrypted `Mmap` extents directly (`&guard.mmap[start..end]`) and decodes single-frame Zstd payloads via `zstd_pool::decompress_frame_into` (`decompress_stream_payload`) with zero per-call `DCtx` setup.
2. **Encrypted Streaming Writes & In-Place AEAD (`P4.23`, `P4.21`, `P4.29`)**:
   * Non-zero-offset (`file_offset > 0`) writes to encrypted Format-v2+ non-filtered files transparently upgrade to `INODE_FLAG_SEEKABLE_64K` (preserving raw encrypted chunks under `CompressionMode::Never` and compressed encrypted chunks otherwise), eliminating $O(N^2)$ whole-file rebuilds while maintaining `SEC-16` / `SEC-19` security invariants.
   * Added `encrypt_data_in_place`, `encrypt_with_nonce_prefix`, `decrypt_into_slice`, stack-allocated AAD builders (`file_aad_stack`, `chunk_aad_stack`), and single-allocation stack-staged `encrypt_filename` / `decrypt_filename`.
3. **Seekable 64K Parallel Read, Zero-Copy Write Stage 1 & Sub-Chunk Cache (`P4.17`)**:
   * Stage 1 borrows full chunks via `Cow::Borrowed` under `inner.read()`.
   * `read_single_chunk_into` decodes directly into destination slices using thread-local scratch buffers; `read_chunked_64k_internal` and multi-chunk aligned `read_at_chunked_64k` parallelize decoding across cores via Rayon `par_chunks_mut(CHUNK_SIZE_64K)`; `ChunkReadCache` (8 slots) serves sub-chunk sequential `read_at` calls without re-decoding 64 KiB Zstd/AEAD frames.
4. **Directory Bulk Unlink, `FxHashMap`, Negative-Lookup Fast Path & Bitset `fsck` (`P4.30`, `P4.19`, `P4.27`)**:
   * `DirIndex` uses `FxHashMap`, tracks `plain_complete`, and stores `(inode_id, phys_blk)` so `delete_file` / `delete_file_journaled` resolve the target block in $O(1)$ and compact raw block bytes in place via zero-allocation `remove_entry_in_block_bytes`.
   * `verify_integrity_on` (`fsck`) uses `read_inode_uncached`, `for_each_valid_inode_in_block`, `for_each_inode_block`, and compact `u64` bitsets (`ref_inodes`, `seen_data`, `cross_linked_data`).
   * `defragment_inplace_on` (`P4.20`) short-circuits in $O(1)$ when `already_compacted`, uses `Bitmap::set_range`, and remaps `dir_cache` block IDs in place without cache stampedes.
5. **Filter Pipeline Single-Alloc `bitshuffle` & SIMD `delta_decode_inplace` (`P4.32`)**:
   * Added `transpose_bit_blocks_inplace` (halving `bitshuffle` heap allocations) and `#[cfg(not(kani))]` AArch64 NEON + x86_64 SSE2 2x-unrolled Hillis-Steele prefix-sum kernels for `delta_decode_inplace`.
6. **WAL & Multi-Block Bitmap Integrity Fixes (`SEC-31`, `SEC-32`, `SEC-33`)**:
   * `SuperBlock::journal_header_block` places the v3 WAL after multi-block data bitmaps (`> 128 MiB` journaled images).
   * `write_data_journaled` uses exact physical byte sizes for `prune_stale_pointers` and retains active indirect pointer tables via `collect_retained_indirect_blocks`.
   * `sim_ensure_root`, intermediate `sim_child`, and `mkdir` (`dir_data_block`) emit 4 KiB zero-init `WriteBlockSlice` ops coalesced in chronological order by `coalesce_ops`.

**Measured Before / After Benchmarks (`--release`, Apple Silicon)**:

| Workload | Before | After | Speedup |
| :--- | ---: | ---: | ---: |
| Stream-compressed 4 MB seq `4 KB read_at` (`P4.24` + `P4.31`) | `1,933.57 ms` (`2.1 MB/s`) | **`1.94 ms`** (`2,059.8 MB/s`) | **996.7x** |
| Clean 5k-file image `defragment(InPlace)` (`P4.20`) | `6.64 ms` (`19.99 MB` moved) | **`0.022 ms`** (`0 B` moved) | **301.8x** |
| Encrypted `128 × 64 KB` append (`Never`, 8 MB) (`P4.23`) | `1,453.41 ms` (`5.5 MB/s`) | **`13.36 ms`** (`598.6 MB/s`) | **108.8x** |
| Encrypted `128 × 64 KB` append (`Auto`, 8 MB) (`P4.23`) | `89.46 ms` (`89.4 MB/s`) | **`13.70 ms`** (`583.8 MB/s`) | **6.5x** |
| Plain 10k-file `delete_file` in 1 dir (`P4.30`) | `52.00 ms` | **`3.60 ms`** | **14.4x** |
| Encrypted 10k-file `delete_file` in 1 dir (`P4.30`) | `23.50 ms` | **`5.63 ms`** | **4.2x** |
| Seekable 16 MB seq `4 KB read_at` (`P4.17`) | `99.58 ms` (`160.7 MB/s`) | **`8.28 ms`** (`1,931.9 MB/s`) | **12.0x** |
| Encrypted 10k negative `lookup` (`P4.19`) | `4.59 ms` | **`0.63 ms`** | **7.3x** |
| `verify_integrity` (`fsck`, 10k files, plain) (`P4.27`) | `2.61 ms` | **`0.42 ms`** | **6.2x** |
| Seekable 16 MB `read_data` (`P4.17` Rayon parallel) | `8.23 ms` (`1,944 MB/s`) | **`1.39 ms`** (`11,511 MB/s`) | **5.9x** |
| Seekable 16 MB seq `1 MB read_at` (`P4.17`) | `7.22 ms` (`2,216 MB/s`) | **`1.76 ms`** (`9,091 MB/s`) | **4.1x** |
| Plain / Encrypted 10k `create_file` (`P4.19` + `P4.29`) | `8.58 ms` / `15.62 ms` | **`2.53 ms` / `5.59 ms`** | **3.4x / 2.8x** |
| `bitshuffle_encode` 16 MB (`typesize=4`) (`P4.32`) | `4.01 ms` (`3,987.2 MB/s`) | **`2.46 ms`** (`6,506.8 MB/s`) | **1.63x** |
| Seekable 64 MB initial `write_data` (`P4.17`) | `12.38 ms` | **`8.30 ms`** | **1.49x** |
| `1,000 × 16 KB` stream `read_data` (`P4.31`) | `8.14 ms` (`1,918.8 MB/s`) | **`6.05 ms`** (`2,584.0 MB/s`) | **1.35x** |

**Verification**: `tests/p4_perf_optimizations_test.rs`, `tests/large_image_bitmap_test.rs::test_journal_does_not_overlap_a_multi_block_data_bitmap`.
