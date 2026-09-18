# Changelog

All notable changes to the OIFS project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
