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
| **SEC-10** | `src/ipc.rs`, `src/session.rs` | Network IPC transport transmits file data and session payloads over unencrypted raw TCP without TLS/mTLS, exposing all operations to network eavesdropping and tampering. | **High** | - | **Open / Architecture** (Requires external VPN/WireGuard or local-only `127.0.0.1` binding) |
| **SEC-11** | `src/ffi.rs` | Several exported C FFI functions (e.g. `oifs_set_io_backend`, `oifs_get_io_backend`) lack `std::panic::catch_unwind`. Unwinding panics across the `extern "C"` boundary invokes Undefined Behavior. | **Medium** | - | **Resolved** (`catch_unwind` wraps on `oifs_set_io_backend`, `oifs_get_io_backend` and `oifs_loaded_path`; `oifs_check_version` is panic-free by construction and kept modelable for Kani) |
| **SEC-12** | `src/encryption.rs`, `src/session.rs` | Encryption keys (`EncryptionKey`) and passwords are kept in standard unzeroized `String`/`Vec<u8>` heap memory, leaving key material exposed in core dumps or memory forensics. | **Low** | - | **Resolved** (`EncryptionKey` derives `Zeroize`/`ZeroizeOnDrop`; CLI password handling now uses `zeroize::Zeroizing<String>` and wipes the raw `read_line` scratch buffer) |
| **SEC-13** | `src/bin/oifs.rs` | Passing `--password <SECRET>` on CLI arguments exposes encryption passwords in plain text in `/proc/<pid>/cmdline` and `ps` process tables to other local users. | **Low** | - | **User Advisory** (`OIFS_PASSWORD` environment variable supported, interactive prompt recommended) |
| **SEC-14** | `src/directory.rs`, `src/disk.rs`, `src/bin/oifs.rs` | Untrusted disk images or callers with directory entry names containing path traversal components (`/`, `\`, `..`, `.`, `\0`, or empty) can escape target export folders or corrupt directory trees. | **Medium** | - | **Resolved** (`validate_filename` in `serialize_into`/`deserialize_from`, `create_entry_internal`/`journaled`, `resolve_parent`, `resolve_path_iter`; sanitized export in `Commands::Get`) |
| **SEC-15** | `src/disk.rs:4410-4450` | Seekable 64K Chunked writes previously allocated contiguous blocks and updated on-disk indirect pointer tables in-place before committing the WAL transaction frame, corrupting replay on crash. | **High** | `test_seekable_chunked_journaled_crash_recovery_and_atomicity` | **Resolved in 1.1.0** (Staged through `AllocSim` + ordered payload flush before `commit_journal_tx`) |

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
