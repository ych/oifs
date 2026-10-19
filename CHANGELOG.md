# Changelog

All notable changes to the OIFS project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Background Auto-Defragmentation Coordinator (Blueprint item 7)**:
  - Added `src/auto_defrag.rs` with `AutoDefragConfig` / `AutoDefragStats` and a daemon worker thread that periodically measures data-area fragmentation and runs an in-place defragmentation pass once the configured threshold is exceeded.
  - Added `DiskManager::set_auto_defrag` / `auto_defrag_config` / `auto_defrag_stats` / `stop_auto_defrag` and `OifsSession::set_auto_defrag` / `auto_defrag_stats`.
  - Added CLI flags `--auto-defrag[=THRESHOLD]` and `--defrag-interval` (opt-in, default disabled).
  - The worker holds only a `Weak` reference to the inner state, so it exits as soon as the last manager is dropped; it skips WAL-enabled images unless `allow_journaled` is set, because in-place defragmentation is not journalled.
  - Added `tests/auto_defrag_test.rs` (6 tests) covering the default-off behaviour, threshold-triggered passes with fsck-verified data integrity, shutdown, the journaled-image skip, and the `min_free_blocks` guard.
- **IPC Frame Buffer Pooling & Bounded Frame Parsing (P4.8)**:
  - Added thread-local pooled buffers for the IPC framing layer (`FRAME_WRITE_BUF`, `FRAME_READ_BUF`, `FRAME_CHUNK_BUF`), eliminating per-message heap allocations in Master-Proxy mode.
  - Added the public `MAX_FRAME_BYTES` limit, validated on both the write and read paths *before* any allocation.
  - `read_framed` now streams payloads in bounded 64 KiB chunks, removing the blind `vec![0u8; len]` allocation a malicious peer could weaponize.
  - Added `tests/ipc_frame_test.rs` covering round-trips, chunked streaming, buffer reuse, oversized-frame rejection and truncated frames.
- **Kani Proofs**:
  - Added `proof_find_next_free_wrapped_soundness` (wrap-around allocator scan) and the first `ipc` module harnesses `proof_frame_len_validation_soundness` / `proof_zero_length_frame_is_valid`. The suite now holds 74 harnesses.

### Fixed
- **Broken Kani Build**: `proof_crc_input_is_deterministic` still referenced the `crc_input` helper removed by the P4.13 CRC refactor, so `cargo kani` failed to compile. The harness now uses `crc_frame_checksum` and additionally proves that the sequence number and payload length are covered by the checksum.

### Fixed
- **FFI Panic Boundary (SEC-11)**: Wrapped `oifs_set_io_backend`, `oifs_get_io_backend` and `oifs_loaded_path` with `std::panic::catch_unwind` so a panic can never unwind across the `extern "C"` boundary; they now record the error and return `-1`.
- **Password Hygiene (SEC-12)**: CLI password handling (flag, `OIFS_PASSWORD` env var, interactive prompt) now uses `zeroize::Zeroizing<String>`, and the raw `read_line` scratch buffer is wiped before the password is handed to the session layer.

## [1.1.0] - 2026-10-14

### Added
- **Seekable 64KB Chunked Compression & Out-of-Lock Parallel Pipeline (P4.2 / H2)**:
  - Implemented 3-stage pipeline: Stage 1 shared read-lock inspection, Stage 2 zero-lock multi-threaded parallel Zstd compression and XChaCha20 encryption via Rayon, and Stage 3 atomic write-lock commit (`write_chunked_64k_pipeline`).
  - Added sub-file random rewind writes with fine-grained chunk dirtying, read-modify-write decompressed chunk preservation, and anti-inflation raw chunk fallback.
  - Added incremental $\Delta$ tracking for seekable chunk compressed size (H3).
  - Added contiguous chunk block allocation (`allocate_contiguous`, `free_contiguous`, P4.4).
- **Metadata WAL Journaling (M1–M5)**:
  - Added opt-in 128KB journal ring buffer (`--journal` / `DiskManager::open_journaled`) with CRC32C-checksummed frames and torn-write detection.
  - Implemented WAL-first atomic metadata transactions for `create_file`, `mkdir`, `delete_file`, `write_data`, and `truncate`.
  - Ordered-mode payload data flush ensuring payload blocks reach disk before metadata commit.
  - Automatic idempotent redo recovery on mount.
  - Staged seekable 64K chunked writes and truncations through `AllocSim`, guaranteeing WAL-first ordering.
- **On-Disk Inode Format v2**:
  - Implemented fixed 256-byte deterministic inode records with explicit little-endian fields, file-type tags, and zeroed reserved space.
  - Preserved backward compatibility with legacy v1 images, supported by in-place idempotent `oifs migrate`.
- **POSIX-Style OpenMode API & Concurrency**:
  - Added `OpenMode` (`Read`, `Write`, `Create`, `CreateNew`, `Truncate`, `Append`) supporting POSIX `O_CREAT | O_EXCL` atomic file creation.
  - Implemented sharded bounded inode cache (`BoundedInodeCache`) with interior lock striping to eliminate global cache miss lock contention (P4.3).
  - Implemented directory cache acceleration (`DirIndex`) and zero-alloc path resolution (P4.6).
  - Defaulted `OifsSession` to high-performance standalone mode with opt-in IPC.
- **Verification & Tooling**:
  - Expanded formal verification suite to 59 Kani proofs covering filter bijectivity, block addressing, directory lookups, CRC32C, journal ring buffer invariants, and version compatibility soundness.
  - Added TLA+ specification for Copy-on-Write seekable chunk replacement.

### Changed
- **Performance Optimizations**:
  - Replaced $O(N^2)$ block deduplication scan in `stage_payload_write` with fast $O(1)$ set lookup (H1), doubling 32MB overwrite throughput from 8.8 GB/s to 18.5 GB/s (+110%).
  - Added mutation range coalescing to merge adjacent and overlapping block ranges before `msync` syscalls.
  - Enabled `TCP_NODELAY` on network IPC client and server sockets to eliminate Nagle's algorithm round-trip latency stalls (M5).

### Fixed
- **WAL Ordering on Seekable Writes**: Fixed issue where seekable 64K chunked writes and truncations mutated bitmaps and pointer tables prior to transaction commit; mutations are now fully staged via `AllocSim` and payload flushed before WAL frame commit.
- **IPC Memory Safety**: Added memory boundary clamping for `IpcRequest::ReadAt` against file size and reasonable buffer limits to prevent excessive memory allocations.
- **Durability Range Syncing**: Ensured modified bitmap and payload ranges are synchronized under range-based durability modes (`Strict`, `RangeAsync`) for seekable chunked writes.

## [1.0.0] - 2026-10-04

### Added
- **Hierarchical Indirect Block Skipping**:
  - Implemented subtree skipping in `DiskManager::read_data_internal` that skips up to 262,144 blocks in a single check when single, double, or triple indirect block pointers are zero, reducing sparse large-file read latency from 6.91s to 0.01s (>690x speedup).
- **Sequential Allocation Hints**:
  - Added `find_first_free_from` and `find_next_free_wrapped` to `Bitmap` and `BitmapRef`.
  - Added `allocate_with_hint` to `SimpleBlockAllocator`.
  - Added `free_block_hint` and `free_inode_hint` to `DiskManagerInner`, converting sequential multi-block allocations from $O(N^2)$ bit scans into amortized $O(1)$ lookups (7.32x speedup on 2,000-block allocations).
- **64-Bit Word Traversal for Fsck & Defragmentation**:
  - Added `BitmapRef::for_each_set_bit` using `trailing_zeros()` and `word &= word - 1` bit-clearing to process 64 blocks per cycle.
  - Optimized `DiskManager::verify_integrity`, `defragment_safe`, and `analyze_fragmentation` to eliminate bit-by-bit checking (7.4x fsck speedup).
- **Zero-Copy Write Path with `Cow` & BitShuffle Fast Path**:
  - Added `apply_filters_cow` returning `Cow::Borrowed` when filters are disabled, eliminating 100% of intermediate buffer copies on standard writes (up to 28,500x speedup on 1MB payloads).
  - Added zero-block bypass in `bitshuffle_encode` and `bitshuffle_decode` (4.62x throughput increase on sparse data).
- **IPC Syscall Consolidation**:
  - Optimized `write_framed` in `src/ipc.rs` to serialize into a pre-reserved 4-byte prefixed buffer, consolidating length framing and payload into a single system call.
- **Extended Quantitative Performance Benchmark Suite**:
  - Expanded `tests/perf_comparison.rs` with 8 reproducible benchmark cases verifying speedups across bitmap scanning, directory search, filter pipelines, sequential allocation, and fsck operations.

### Changed
- **Directory Operations & Creation Deduplication**:
  - Eliminated redundant secondary directory block lookups in `DiskManager::create_entry_internal` when the filesystem is unencrypted.
  - Replaced blocking synchronous `mmap.flush()` (`msync(MS_SYNC)`) on high-frequency write paths with `mmap.flush_async()`.
- **Code Deduplication in `DiskManager`**:
  - Unified indirect block pointer read/write/zeroing operations into `read_block_ptr`, `write_block_ptr`, `alloc_and_zero_block`, and `get_or_alloc_indirect_child`, eliminating ~170 lines of boilerplate.
  - Consolidated `create_file` and `create_directory` into `create_entry_internal`, ensuring consistent validation, locking, and error reporting.
  - Consolidated 3 separate chunk write loops across initial write, Zstd multi-frame append, and raw uncompressed write paths into `write_buffer_at_offset`.
- **Early Termination in `collect_inode_blocks`**:
  - Capped double and triple indirect block tree traversals by the maximum blocks mapped by the file's logical/compressed size, eliminating up to 134 million unnecessary empty block checks.

### Fixed
- **IPC Write Stalls**: Fixed critical performance bug where `format!("{:?}", req)` was invoked on every incoming IPC request, preventing 40MB+ debug string allocations on 10MB write requests.
- **Timestamp Initialization**: Fixed bug where `created_at` remained 0 upon file/directory creation; now correctly initialized to the current Unix epoch timestamp.
- **Documentation Accuracy**: Corrected outdated inode table slot size comments and updated maximum file size documentation to 513GB.

## [0.1.0] - 2026-09-06

### Added
- Triple indirect block addressing supporting files up to 513GB.
- fscrypt-style filename encryption with Synthetic IV (SIV) and parent inode tweaking.
- Zstd multi-frame append support for fast EOF appending without full decompression.
- Optimized release profile with Thin-LTO, single codegen unit, and symbol stripping.
- Master-Proxy IPC with dual-mode Local (UDS) and Network (TCP + Active Ping Probe) transports.
- Blosc2 pre-compression numeric filter pipelines (Delta, Shuffle, BitShuffle, Precision Truncation).
- Defragmentation (Safe & In-place modes) and fsck integrity verification.
