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
| **SEC-19** | `src/encryption.rs:83-137`, `src/disk.rs:3872-3900` | XChaCha20-Poly1305 AEAD passes empty associated data (`b""`). Because all files and 64K seekable chunks share the same master key without binding `(inode_id, chunk_idx, flags)` in AAD, an attacker with write access to `.img` can swap/reorder/rollback encrypted chunks across files or toggle unauthenticated inode filter flags undetected. | **Medium** | - | **Open / Format v3** (requires an on-disk format change: the AAD binds the inode id, the chunk index and the payload flags, so old ciphertexts become unreadable and a migration is needed) |
| **SEC-20** | `src/disk.rs:2743-2798`, `src/journal.rs:924-980` (`resolve_logical_block_id`, `get_or_alloc_block`, `apply_op_in_place`) | Resolving data block pointers from inodes/indirect tables and replaying `MetadataOp::WriteBlockSlice` only check `block_id * BLOCK_SIZE <= mmap.len()` without verifying `block_id >= sb.data_block_start` (and `WriteInode` does not check `inode_id < sb.inode_count`), allowing crafted/corrupted pointers to overwrite SuperBlock, bitmaps, or inode tables. | **Medium** | - | **Resolved** (see §6.5) |
| **SEC-21** | `src/ffi.rs:251, 315, 344, 417, 488, 590` | Several FFI entry points execute `unsafe { CStr::from_ptr(...) }` and `std::slice::from_raw_parts(...)` outside `std::panic::catch_unwind`, and `oifs_write_file_with_policy` silently substitutes `&[]` when `buf.is_null() && buf_size > 0` instead of returning `-1`. | **Medium** | - | **Resolved** (see §6.6) |
| **SEC-22** | `src/bin/oifs_mcp.rs:154, 188-191, 285, 367` | MCP server constructs JSON responses via `format!` without escaping quotes, backslashes, or control characters in filenames/paths/error messages (JSON injection), and computes `size_mb * 1024 * 1024` without overflow checks. | **Medium** | - | **Resolved** (see §6.7) |
| **SEC-23** | `src/auto_defrag.rs:66-82, 165-171, 240-318` | `AutoDefragConfig` does not sanitize `NaN` thresholds (`f64::NAN.clamp(0.0, 1.0)` remains `NaN`) or `check_interval == Duration::ZERO` (100% CPU busy-spin loop), and uses `.lock().unwrap()` / `.read().unwrap()` instead of `crate::lock_unpoisoned`. | **Low** | - | **Resolved** (`AutoDefragConfig::sanitized`, `MIN_CHECK_INTERVAL`, `crate::lock_unpoisoned` / `read_unpoisoned`) |
| **SEC-24** | `src/session.rs:106, 612` | `OifsSession::Remote` retains `password: Option<String>` in plain unzeroized heap memory across the session lifetime for failover reconnection, instead of wrapping it in `zeroize::Zeroizing<String>`. | **Low** | - | **Resolved** (`Option<zeroize::Zeroizing<String>>`) |
| **SEC-25** | `src/disk.rs:5025-5051, 2379-2384, 2627-2634, 5131-5143`, `src/journal.rs:2601-2620`, `src/superblock.rs:224-266` | (1) `inode_slot_range` only checks `end <= guard.mmap.len()` without verifying `inode_id < sb.inode_count`; an out-of-range `inode_id` resolves into the **Data Area**, enabling fake-inode type confusion where user file payload bytes are parsed/mutated as `Inode` structs to read, overwrite, or free arbitrary blocks. (2) Inode/data bitmap allocations scan all 32,768 bits without capping at `sb.inode_count` or `sb.block_count`, allowing allocation past `inode_count` to overwrite `data_block_start` (root directory block). (3) `SuperBlock::validate` does not verify `inode_table_block + inode_count.div_ceil(16) <= data_block_start`. | **Critical** | `proof_usable_inode_count_fits_the_table` | **Resolved** (see §6.9) |
| **SEC-26** | `src/disk.rs:6499-6578, 6695-6756, 6864-7020` (`collect_inode_block_layout`, `defragment_inplace_on`, `defragment_safe`) | `collect_inode_block_layout` ignores `INODE_FLAG_SEEKABLE_64K` and treats 64-bit packed `ChunkEntry` values (`(compressed_len << 48) | (flags << 40) | (block_count << 32) | start_block`) as raw block IDs. Running `defragment_inplace` (or background `auto_defrag`) on an image with seekable-compressed files panics out-of-bounds (`src as usize * BLOCK_SIZE`) or corrupts multi-block chunk extents and packed `ChunkEntry` metadata; `defragment_safe` also checks only `inode.compressed_size > 0` (which is `0` for seekable files). | **High** | - | **Resolved** (see §6.10) |
| **SEC-27** | `src/disk.rs:5131-5143, 5240-5287, 6079-6101`, `src/journal.rs:2895-2945` (`allocate_block`, `get_or_alloc_block`, `stage_payload_write`, `truncate_internal`) | Freed data blocks are not zeroed, and newly allocated data blocks are not zero-initialized before partial writes (`file_offset > 0` within a fresh block) or size-extending `truncate(inode_id, new_size)` (Case 2), leaking up to 4,095 bytes per block of previously deleted files' residual plaintext. | **High** | - | **Resolved** (see §6.11) |
| **SEC-28** | `src/disk.rs:3172-3184, 4031-4096, 2758-2777, 1741-1764, 6113-6129` (`read_at_prepare`, `read_single_chunk_decompressed`, `resolve_logical_block_id`, `append_dir_entry_to_dir`, `truncate_internal`) | Incomplete `SEC-20` `is_data_block` coverage: `read_at_prepare` and `read_single_chunk_decompressed` only check `< block_count` (not `>= data_block_start`), allowing `read_at` or seekable chunk reads on a corrupted inode to leak the SuperBlock, bitmaps, or Inode Table; non-journaled `append_dir_entry_to_dir`, `truncate_internal` slack zeroing, and indirect block pointers (`sib_id`/`dib_id`/`tib_id`) also bypass `is_data_block`. | **Medium** | - | **Open** (see §3.3) |
| **SEC-29** | `src/disk.rs:6134-6161, 6214-6255, 5677-5705, 5875-5905` (`truncate_internal`, `clear_logical_block_ptr`, `delete_file_journaled`, `delete_file`) | (1) `truncate_internal` Case 3 calls `clear_logical_block_ptr` and `prune_unused_indirect_blocks` before `commit_journal_tx`, mutating indirect pointer blocks directly in `guard.mmap` without `MetadataOp::WriteBlockSlice` WAL entries (violating the WAL-first invariant). (2) `delete_file` / `delete_file_journaled` delete non-empty directories without checking for child entries, permanently orphaning all child inodes and data blocks in the bitmaps. | **Medium** | - | **Open** (see §3.3) |
| **SEC-30** | `src/session.rs:571`, `src/ipc.rs:775-785, 807-814, 1177` | (1) `OifsSession::Direct` always creates an unbounded `mpsc::channel()` and stores `Some(rx)` in `event_rx`; if `take_event_receiver()` is never called, every IPC request handled by the Master appends a `SessionEvent` forever (unbounded memory growth). (2) In `SessionMode::Local`, `bind_unix_listener(&socket_path)` runs before `write_token_file(&token_path, &auth_secret)`, leaving a brief race window where a connecting client reads a stale `.tok` from a previously crashed Master. | **Low / Medium** | - | **Open** (see §3.3) |

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
| **SEC-28** — incomplete data-area bounds checks on the read, chunk, directory and indirect paths | Medium | Open |
| **SEC-29** — WAL-first violation in `truncate_internal`; orphan leak when deleting a non-empty directory | Medium | Open |
| **SEC-30** — unbounded `SessionEvent` channel; `.tok` creation ordering race | Low / Medium | Open |

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

**Verification**: `tests/sec16_repro_test.rs` reproduces the three scenarios
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

**Verification**: `tests/sec20_block_pointer_test.rs` (3 tests) — a file whose
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

**Verification**: `tests/sec25_inode_bounds_test.rs` (4 tests) — a fake inode
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

**Verification**: `tests/sec26_defrag_seekable_test.rs` (2 tests) — the
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

**Verification**: `tests/sec27_zeroed_blocks_test.rs` (3 tests) — the raw
content of a recycled block never contains the plaintext of the deleted file
(in-place and journaled), and the extended region of a truncated file is
zeroed. The first two tests fail without the fix.
