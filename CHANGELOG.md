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
  - Added `proof_find_next_free_wrapped_soundness` (wrap-around allocator scan), the first `ipc` module harnesses `proof_frame_len_validation_soundness` / `proof_zero_length_frame_is_valid` and `proof_sanitized_config_is_always_valid` for the auto-defrag configuration. The suite now holds 77 harnesses.
  - Added `proof_block_path_tier_boundaries_symbolic`, which proves the pointer-tree tier classification for *every* logical block index instead of a handful of constants.
  - Added `proof_usable_inode_count_fits_the_table`, which proves that the usable inode ids always fit in the inode table (SEC-25).
  - Every proof that constrains its input with `kani::assume` now carries `kani::cover!` assertions, so no harness can pass vacuously.

### Security
- **Encryption Bypass on Non-Zero-Offset Writes (SEC-16, Critical)**: Fixed the write planner so that a compressed, filtered or encrypted payload is always rebuilt as a whole instead of being patched in place. Previously a write at `file_offset > 0` on an encrypted image could store **plaintext** on disk, overwrite the XChaCha20-Poly1305 ciphertext of an existing file (destroying its Poly1305 tag), and a shrinking overwrite kept a stale file size that broke AEAD authentication. The inode's filter metadata now also describes the filter pipeline that was actually applied.
- **IPC Deserialization Allocation Bomb (SEC-17, High)**: The framing layer now deserializes with a hard byte limit (the size of the frame), so a tiny frame announcing a huge `Vec`/`String` length can no longer make the receiver allocate gigabytes. The wire format is unchanged.
- **IPC Transport Hardening (SEC-18, High)**:
  - The cleartext password no longer travels over the wire: authentication is a challenge-response over a direction-separated transcript, authenticated with keyed BLAKE2b, and the client verifies that the master holds the same key (so a client redirected to a fake master aborts).
  - Unencrypted images are no longer unauthenticated: a master publishes a random session secret that clients must present.
  - Unix sockets now live in a per-user private directory (`0700`) with `0600` socket and secret files, and the TCP rendezvous file is created with `0600`.
  - The number of concurrent peers is bounded (256) and every connection gets a 10 s handshake budget, which removes the Slowloris / thread-exhaustion vector.

### Security
- **Data-Area Lower-Bound Checks (SEC-20, Medium)**: Block pointers that leave the data area are now rejected everywhere: the read path ignores them, the write path fails with an error instead of landing in the superblock, the bitmaps or the inode table, and the WAL replay refuses every operation that addresses metadata.
- **FFI Boundary Hardening (SEC-21, Medium)**: Every `CStr::from_ptr` conversion of a caller-provided pointer now happens inside the `catch_unwind` closure, and `oifs_write_file_with_policy` reports a null buffer with a non-zero size instead of silently writing nothing.
- **MCP Response Encoding (SEC-22, Medium)**: The MCP tool responses are built with `serde_json` instead of `format!`, so file names and error messages can no longer inject JSON, and the image size is computed with checked arithmetic.
- **Release Verification Tooling**: `scripts/release_check.sh` now runs the *complete* Kani suite in parallel (`cargo kani -j 8 --output-format=terse`) instead of a handful of harnesses, and gained a native LLVM coverage step (step 7/8) that reports ~77% region coverage. The step prefers the `llvm-tools-preview` component of the active toolchain and asks cargo for the test executable paths.
- **Session Password Hygiene (SEC-24, Low)**: The password kept by a remote session for the automatic reconnection now lives in a `zeroize::Zeroizing<String>` and is wiped when the session is dropped.

### Security
- **Plain-TCP Network Transport (SEC-10, Mitigated)**: The IPC master binds loopback by default, a non-loopback bind address is refused unless `--allow-remote-ipsc` is given, and the library warns when such a bind is requested. The transport itself remains plain TCP and is expected to be tunnelled.

### Security
- **Inode Table and Data Bitmap Bounds (SEC-25, Critical)**: The superblock's `inode_count` was sized with the historical 128-byte inode records and is therefore twice the number of 256-byte slots that actually fit in the inode table, while `inode_slot_range` only checked the mapped length. An inode id beyond the table addressed the **data area**: a crafted "fake inode" inside a file could be used to read or overwrite another file, creating a file beyond the table capacity overwrote the root directory's data blocks, and the data allocator could hand out blocks beyond the end of a small image. The usable inode range is now derived from the real table capacity, every entry point that takes an inode id refuses an id outside it, both allocators stop at their region boundary, and `SuperBlock::validate` rejects the impossible layouts.

### Security
- **Defragmentation of Seekable 64K Chunked Files (SEC-26, High)**: A seekable file stores packed `ChunkEntry` values (a run of contiguous blocks) in its pointer slots, but the defragmentation treated them as raw block ids: the in-place pass relocated the wrong blocks, freed the file's real blocks in the rebuilt bitmap and left invalid pointers behind, and the safe pass copied the content as a plain file while the seekable flag stayed set. The layout collector now decodes the chunk entries, the in-place pass rewrites them with their new start block, and the safe pass copies such files with `CompressionMode::Seekable`.
- **Unzeroed Data Blocks (SEC-27, High)**: A data block that was freed kept its plaintext, and a block allocated again was handed to the new owner uncleared, so a short write could expose the content of a deleted file. Newly allocated blocks are now zeroed (in-place and journaled paths), and extending a file zeroes the slack of its last block.

### Security
- **WAL-First Ordering and Orphaned Directory Children (SEC-29, Medium)**: The journaled shrink path of `truncate_internal` zeroed the indirect pointer tables directly in the mapping before the transaction was committed, so a crash in between lost the tail of the file without the journal knowing. The zeroing is now staged as a transaction operation. In addition, both delete paths refused nothing: removing a non-empty directory orphaned all its children. A non-empty directory is now rejected before the parent entry is touched.

### Performance
- **Pooled Zstd Contexts and Single-Pass Decompression (P4.26)**: Added a thread-local pool of Zstd contexts, so a frame (or a 64 KiB chunk) no longer allocates and initialises a fresh `CCtx`/`DCtx`. Seekable chunks are decompressed in one pass into a caller-provided buffer, the whole-file read path allocates the expected size once, and a stream of small appended frames is rewritten as a single frame once the frame overhead exceeds a quarter.
- **IPC Payload Copies and Pooled Buffer Retention (P4.28)**: The pooled frame buffers are released again when they grew beyond 1 MiB (a single 128 MiB frame used to pin its buffer per worker thread), `IpcClient::send` takes the request by reference so the retry loop no longer clones the whole payload, and the per-request event name is only materialized when a listener exists.

### Fixed
- **Broken Kani Build**: `proof_crc_input_is_deterministic` still referenced the `crc_input` helper removed by the P4.13 CRC refactor, so `cargo kani` failed to compile. The harness now uses `crc_frame_checksum` and additionally proves that the sequence number and payload length are covered by the checksum.
- **Auto-Defrag Configuration Boundaries (SEC-23)**: `AutoDefragConfig::sanitized` now replaces a `NaN` fragmentation threshold with the default (a `NaN` threshold silently disabled the "not fragmented enough" check and defragmented on every tick) and raises a zero check interval to `MIN_CHECK_INTERVAL`, which removes the 100% CPU busy-spin loop. The coordinator's locks now use `crate::lock_unpoisoned` / `crate::read_unpoisoned`.
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
