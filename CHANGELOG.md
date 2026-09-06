# Changelog

All notable changes to the OIFS project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **`BitmapRef` Read-Only View**: Zero-allocation read view over bitmap slices (`&[u8]`) eliminating 16KB of redundant heap copies in fragmentation analysis, defragmentation, and fsck integrity checks.
- **In-Place Delta Encoding & Decoding**: Added `delta_encode_inplace` and `delta_decode_inplace` to eliminate full-buffer allocations in pre-compression filter pipelines.
- **In-Place Mantissa Truncation**: Added `trunc_precision_encode_inplace` for floating-point precision truncation without intermediate buffer cloning.
- **`IpcRequest::name()`**: Added static string identifier method for all IPC request variants, eliminating multi-megabyte debug formatting allocations on file write payloads.
- **Comprehensive Refactoring & Performance Test Suite**: Added `tests/refactoring_and_perf_test.rs` covering bitmap parity, in-place filters, IPC naming, and timestamp initialization.

### Changed
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
