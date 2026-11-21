# Changelog

All notable changes to the OIFS project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.3.0] - 2026-11-21

### Added
- **Pluggable `StorageMode::FileIo` (`pread`/`pwrite`, Zero `mmap`)**:
  - Added `src/storage.rs` implementing a unified `ImageBacking` abstraction over both `StorageMode::Mmap` (default) and `StorageMode::FileIo` (100% `mmap`-free positional `pread`/`pwrite` I/O in Safe Rust, eliminating `SIGBUS` truncation hazards).
  - Added `DiskManager::open_file_io`, `open_file_io_journaled`, `open_file_io_with_password`, `open_with_storage_mode`, `storage_mode()`, process-wide `set_default_storage_mode` / `OIFS_STORAGE_MODE=pread`, and the `--no-mmap` global CLI flag.
  - Added write-through in-memory bitmap and WAL ring mirrors plus a 16-shard `Arc`-backed 4 MiB `MetadataBlockCache` (deadlock-free across nested double/triple-indirect pointer traversals with a zero-`Arc`-clone `read_u64_le` fast path).
- **64-Shard User-Space Data Page Cache (`DataPageCache`)**:
  - Added a configurable 64-shard Second-Chance Clock user-space 4 KiB data page cache for `StorageMode::FileIo` (32 MiB default, configurable via `OIFS_FILE_IO_CACHE_MB` or `DiskManager::set_file_io_cache_bytes` up to 4 GiB, or `0` to disable).
  - Automatically bypasses streaming reads exceeding 64 KiB to prevent scan pollution and stays strictly coherent across all writes, truncations, block zeroes/copies, and WAL replays.
- **Single-Transaction File Creation (`create_file_with_data`)**:
  - Added `DiskManager::create_file_with_data` and `DiskManager::create_file_with_data_with_filters` to combine inode allocation, directory entry insertion, and initial payload write into a single write-lock acquisition and a single WAL transaction, with automatic rollback on `NoSpace` or I/O errors.
  - Under `DurabilityMode::Strict` on journaled images, small payloads (`<= 8 KiB` / 2 blocks) are zero-padded and data-journaled directly inside the WAL commit (`WriteBlockSlice`), cutting durable file creation from 3 sync barriers to 1 (`2.31x` higher `Strict` `FileIo+WAL` throughput).
- **Shared Read Sessions, Zero-Copy Mmap Views & Pinned File Handles (`ReadSession` / `FileReadHandle`)**:
  - Added `DiskManager::begin_read() -> ReadSession` (with `ReadSession::root_inode()`), `ReadSession::open_file() -> FileReadHandle`, and `FileReadHandle::mmap_slice()` (true zero-copy slice view on contiguous raw `StorageMode::Mmap` files with automatic `None` fallback on `FileIo`, compressed, encrypted, or sparse ranges) to amortize outer `RwLock` acquisitions and `BoundedInodeCache` lookups across batches of lookups, metadata queries, and positional `read_at` calls (`2.3x` faster 4-thread random 4 KiB reads on `Mmap`, `6.3x` faster on `FileIo`).
- **Rust Async Execution Engine (`OifsAsyncTask<T>`) & Group-Commit Write Pipeline**:
  - Added `OifsAsyncTask<T>` (`std::future::Future`, `is_ready`, `try_take`, `wait`, `wait_on_drop`), `read_at_async`, `read_data_async`, `read_at_batch_async`, `create_file_with_data_async`, `write_data_async`, and `flush_async_task`.
  - Reads use a non-blocking **Inline Fast-Path** (`try_read_at_inline_into`, `try_read_data_cached_only`, `try_read_block_ptr_cached_only`): hits on `Mmap`, `DataPageCache`, `ChunkReadCache`, and `DecodedBlobCache` return `Ready` immediately on the calling thread with zero thread hop, while cold reads and compressed/encrypted decodes dispatch to a persistent background worker pool.
  - Concurrent async writes (`create_file_with_data_async`, `write_data_async`) under `DurabilityMode::Strict` are harvested by a Group-Commit drainer thread and share a single physical `flush()` barrier per batch while preserving WAL-first durability and propagating flush errors to every affected task.
- **Header-Only Modern C++17/C++20 Native API (`include/oifs.hpp`) & Expanded C FFI (`include/oifs.h`)**:
  - Added `include/oifs.hpp` (`namespace oifs`): `Error`, `StorageMode`, `DurabilityMode`, `WritePolicy`, `OpenOptions`, `DirEntry`, `Span<T>`, `ByteView` (with alignment/size-checked `as_span<const T>()`), `MutableByteSpan`, RocksDB-style `PinnableSlice` (0-copy `mmap` pin with automatic 1-copy fallback on `FileIo`/compressed/encrypted/sparse files — `4.5–6.1 ns/op` vs `106–167 ns/op` legacy path-based C `oifs_read_at`, `23x–27x` speedup), move-only RAII `FileReadHandle`, `ReadSession`, `AsyncFuture`, and `FileSystem` (including `ls`, `truncate_file`, `read_at_batch`, `WritePolicy` async/sync overloads, async futures, and template lambda callback helpers).
  - Expanded `src/ffi.rs` & `include/oifs.h` with `oifs_open_with_options`, `oifs_set_durability_mode`, `oifs_set_file_io_cache_bytes`, `oifs_flush`, `oifs_resolve_path`, `oifs_create_file_with_data`, `oifs_read_at_inode`, `oifs_read_at_batch`, `oifs_begin_read`/`oifs_end_read`, `oifs_session_open_file`/`oifs_session_open_inode`/`oifs_file_close`, `oifs_file_size`/`oifs_file_inode_id`/`oifs_file_read_at`/`oifs_file_mmap_slice`, `oifs_read_at_async`, `oifs_create_file_with_data_async`, `oifs_write_file_async`, `oifs_flush_async`, `oifs_async_poll`/`oifs_async_wait`/`oifs_async_last_error`/`oifs_async_free`, and callback APIs (`oifs_read_at_async_cb`, `oifs_create_file_with_data_async_cb`).
  - Added `tests/async_and_ffi_test.rs` and `tests/cpp_api_test.rs` (compiling and running strict `-Wall -Wextra -Wpedantic -Werror -pthread` binaries in both `-std=c++17` and `-std=c++20`, verifying 0-copy/1-copy fallbacks, out-of-order destruction, UAF drop-barrier, RAII move semantics & `PinnableSlice` reuse, boundary/auth edge cases, 4-process × 4-thread `open_read_only` `F_RDLCK`/`F_WRLCK` multi-process stress testing across plain & encrypted images, 4-reader + 4-writer concurrent `DurabilityMode::Strict` Group-Commit workloads on `Mmap+WAL` & `FileIo+WAL`, and micro-benchmarks).
- **Read-Only Mode (`F_RDLCK` Shared Locks, `PROT_READ` `MmapReadOnly`, Zero-Write Mounts)**:
  - Added `ImageBacking::MmapReadOnly(memmap2::Mmap)` (`PROT_READ`, preserving true zero-copy `FileReadHandle::mmap_slice()` on `0444` images) and read-only `StorageMode::FileIo` (`O_RDONLY` `pread(2)`), with strict read-only policy enforcement rejecting every low-level write, bitmap mutation, journal write, and flush.
  - Added `DiskManager::open_read_only`, `open_read_only_with_password`, `open_read_only_with_storage_mode`, `is_read_only`, and `DiskManagerError::ReadOnly`, acquiring a non-blocking shared OS read lock (`fcntl(F_SETLK, F_RDLCK)`) so multiple independent read-only processes can mount the same image concurrently while mutually excluding exclusive read-write mounts (`F_WRLCK`).
  - Verified WAL cleanliness strictly read-only on mount (`is_journal_clean_read_only`), refusing unrecovered dirty journals until recovered by a read-write mount; gated every mutating sync/async `DiskManager` API with `DiskManagerError::ReadOnly`, suppressed auto-defrag and all `Drop` flushes, guaranteeing 100% byte-for-byte immutability.
  - Exposed read-only mounts across C FFI (`oifs_open_read_only`, `oifs_is_read_only` in `include/oifs.h` / `src/ffi.rs`), Modern C++ (`oifs::FileSystem::open_read_only`, `is_read_only` in `include/oifs.hpp`), and CLI (`--read-only` global flag plus `cat`, `info`, and `stat` read subcommands in `src/bin/oifs.rs`, with clean flushes on mutating CLI subcommands).
  - Added `tests/read_only_mode_test.rs` and expanded `tests/cpp_api_test.rs` covering the full read matrix on `0444` plain/journaled/encrypted images (`Mmap` & `FileIo`), exhaustive mutation gating with byte-for-byte immutability checks, multi-process `F_RDLCK`/`F_WRLCK` concurrency, dirty WAL rejection, C++/C FFI 0-copy `PinnableSlice`, and CLI `--read-only` / `--no-mmap` end-to-end verification.
- **Kani Formal Verification Suite Expanded to 99 Harnesses (+7)**:
  - Added 7 new `#[kani::proof]` harnesses (with `kani::cover!` where `kani::assume` is used) proving `BitmapSliceMut::set_dirty_bits` byte-range coverage and bounds, `MetadataBlockCache`/`DataPageCache` shard-index bounds and Fibonacci high-bit mixing, `DataPageCache::blocks_per_shard_for_bytes` clamping and overflow freedom, `FileReadHandle::mmap_slice` contiguous block/offset bounds soundness, `policy_to_compression_mode` total panic-free precedence, `proof_read_only_storage_access_invariants` (read-only operation gating and `MmapReadOnly` zero-copy availability), and `proof_read_only_mount_drop_and_open_mode_invariants` (dirty WAL rejection, drop flush suppression, and `OpenMode` write-requirement soundness).

### Performance
- **Precision WAL & Bitmap Writeback + Contiguous Fresh-File Allocation (`FileIo` & `Mmap`)**:
  - Narrowed `JournalRegionMut` writeback under `StorageMode::FileIo` from the full 132 KiB journal region to only the appended WAL frame (plus any wrap-around zeroed tail) and the 64-byte header, and narrowed `BitmapSliceMut` writeback to modified dirty byte ranges.
  - Added `DiskManager::try_write_contiguous_fresh_file` in `write_buffer_at_offset` to allocate payload and indirect pointer blocks in one contiguous bitmap span and write each indirect pointer block in a single write (`7.7x–9.5x` faster 16 MiB non-journaled `FileIo` sequential writes, `10.2–12.7 GB/s`).
- **Persistent Scoped Worker Pool & Global Zstd Context Recycling**: Added a zero-dependency persistent worker pool (`par_for_index`) and bounded global `CCtx`/`DCtx` recycling pool in `src/zstd_pool.rs`, keeping worker thread-local Zstd contexts warm across calls and eliminating per-call OS thread creation overhead (`2.70x` faster 1 MiB seekable `read_at`, `12.6 GB/s`).
- **Sequential 4-Chunk Parallel Read-Ahead**: Upgraded `ChunkReadCache` to an `RwLock` with zero-`Arc`-clone slice copying (`copy_slice_if_hit`) and automatic 4-chunk parallel read-ahead on sequential sub-chunk `read_at` scans (`1.47x` faster sequential 4 KiB seekable `read_at`).
- **Non-Journaled In-Place Chunk Reuse & Single-Lock Seekable Stage-1**: Reused existing contiguous chunk runs in place on non-journaled seekable overwrites when `blocks_needed <= old_cnt`, checked `ChunkReadCache` during partial-chunk RMW, and merged `write_data_with_filters` seekable routing into a single shared read lock.
- **AArch64 NEON SIMD + Short-Message Scalar XChaCha20 Fast Path**: Enabled `chacha20_force_neon` for `aarch64` targets in `.cargo/config.toml` (`+21.7%` encrypted 64 KiB append throughput, `701 MB/s`) alongside an RFC 8439-identical 1-block scalar XChaCha20-Poly1305 fast path for short filenames (`<= 192 B`), avoiding 4-block/8-block SIMD waste on both AArch64 NEON and x86_64 AVX2.

### Security
- **`FileIo` Writeback & WAL Apply Error Propagation**: Propagated `pwrite` I/O errors explicitly from `JournalRegionMut::flush_writeback` (rolling back the in-memory journal header cursor on failure) and `ImageBacking::apply_journal_op` (writing disk before updating in-memory mirrors/caches), preventing memory-vs-disk split-brain and preserving WAL-before-metadata ordering on host I/O failures.
- **Contiguous Fresh-File Allocation Rollback**: Ensured `try_write_contiguous_fresh_file` releases all newly allocated contiguous data bitmap blocks and resets `inode.blocks` if any mid-write `pwrite` fails.
- **Bounded `FileIo` Cache Configuration & Region Validation**: Clamped `OIFS_FILE_IO_CACHE_MB` / `set_file_io_cache_bytes` to `MAX_FILE_IO_CACHE_BYTES` (4 GiB), rejected out-of-bounds metadata regions in `FileBacking::configure_regions` with `UnexpectedEof`, and extended `ReadSession::open_file` encrypted-image checks to reject unencrypted inodes with `compressed_size > 0` or `INODE_FLAG_SEEKABLE_64K`.
- **FFI Lifetime, UAF Drop-Barrier & Callback Exception Safety**:
  - Shared `Arc<FfiSharedSession>` with strict top-to-bottom field drop ordering across `OIFSReadSession` and `OIFSFileHandle` so out-of-order `oifs_end_read` before `oifs_file_close` (or closing `FileReadHandle` while a pinned `oifs::PinnableSlice` is live) cannot trigger Use-After-Free, and eliminated recursive `RwLock` acquisition in `oifs_session_open_file`.
  - Enforced `wait_on_drop` with a panic-safe completion guard on `OIFSAsyncFuture` so dropping an in-flight `oifs_read_at_async` future blocks until the worker finishes writing to the caller's buffer and never deadlocks on unwind.
  - Enforced encrypted anti-downgrade and file-type validation before empty/EOF short-circuits in the Async Inline Fast-Path, and wrapped C++ `extern "C"` callback trampolines in `try { ... } catch (...) {}` to prevent exceptions from unwinding across the FFI boundary.

### Documentation & CI
- Added `docs/cpp_and_async_api_design.md` documenting the Modern C++ Zero-Copy API, Async Inline Fast-Path + Group-Commit design, and memory/crash-safety invariants.
- Updated `README.md`, `README_zh.md`, and `AGENTS.md` with current v1.2.0+ architecture, security hardening, and verified release benchmarks.
- Updated `.github/workflows/openwiki-update.yml` to use `openwiki@0.9.0`, `nvidia/nemotron-3-ultra-550b-a55b:free` (with manual `workflow_dispatch` `model_id` override), and Node 24-compatible `actions/checkout@v5` / `actions/setup-node@v5`.

## [1.2.0] - 2026-11-21

### Added
- **Modular Storage Engine**: Split `src/disk.rs` into focused submodules (`src/disk/caches.rs`, `src/disk/chunked.rs`, `src/disk/maintenance.rs`).
- **Hardened Cryptography**: Upgraded Argon2id KDF to OWASP / RFC 9106 parameters (`64 MiB`, `t=3`, `p=4`), added BLAKE2b-512 domain-separated subkeys (`payload_key`, `filename_siv_key`, `filename_cipher_key`, `ipc_auth_key`), and upgraded deterministic filename encryption to full 24-byte (192-bit) `XChaCha20-Poly1305` SIV with `parent_inode` bound in Poly1305 AAD.
- **Dependency Reduction**: Removed 9 external dependencies (`chrono`, `ctrlc`, `nix`, `rayon`, `rustc-hash`, `thiserror`, `anyhow`, moved `rand` to dev-dependencies, made `blosc2` optional), cutting total normal crates from 149 to 111 (64 with `--no-default-features`).
- **Background Auto-Defragmentation Coordinator**:
  - Added `src/auto_defrag.rs` with `AutoDefragConfig` / `AutoDefragStats` and a daemon worker thread that periodically measures data-area fragmentation and runs an in-place defragmentation pass once the configured threshold is exceeded.
  - Added `DiskManager::set_auto_defrag` / `auto_defrag_config` / `auto_defrag_stats` / `stop_auto_defrag` and `OifsSession::set_auto_defrag` / `auto_defrag_stats`.
  - Added CLI flags `--auto-defrag[=THRESHOLD]` and `--defrag-interval` (opt-in, default disabled).
  - The worker holds only a `Weak` reference to the inner state, so it exits as soon as the last manager is dropped; in-place defragmentation is fully WAL-atomic and supported on WAL-enabled images by default.
  - Added `tests/auto_defrag_test.rs` covering the default-off behaviour, threshold-triggered passes with fsck-verified data integrity, shutdown, journaled-image support, and the `min_free_blocks` guard.
- **IPC Frame Buffer Pooling & Bounded Frame Parsing**:
  - Added thread-local pooled buffers for the IPC framing layer (`FRAME_WRITE_BUF`, `FRAME_READ_BUF`, `FRAME_CHUNK_BUF`), eliminating per-message heap allocations in Master-Proxy mode.
  - Added the public `MAX_FRAME_BYTES` limit, validated on both the write and read paths *before* any allocation.
  - `read_framed` now streams payloads in bounded 64 KiB chunks, removing the blind `vec![0u8; len]` allocation a malicious peer could weaponize.
  - Added `tests/ipc_frame_test.rs` covering round-trips, chunked streaming, buffer reuse, oversized-frame rejection, and truncated frames.
- **Multi-Block Bitmaps (Layout v3)**: The inode and data bitmaps were previously hardcoded to a single 4 KiB block, capping the data area at 32,768 blocks (128 MiB). The v3 layout sizes bitmaps dynamically with the image; spans are derived from recorded offsets without breaking `bincode` superblock compatibility.
- **Kani Formal Verification Suite (92 Harnesses)**:
  - Expanded formal proofs across allocator wrap-around scanning, IPC frame length validation, auto-defrag configuration sanitization, symbolic `BlockPath` tier classification, usable inode table bounds, sharded directory cache Fibonacci hashing, indirect hole-skip conservation, contiguous mmap run coalescing, seekable truncate incremental size conservation, and encryption AAD domain separation.
  - Every proof that constrains its input with `kani::assume` carries `kani::cover!` assertions so no harness can pass vacuously.

### Security
- **Encrypted Image Integrity & Anti-Downgrade Hardening**:
  - Rejected unencrypted regular files (`!inode.encrypted`) on encrypted images across `read_data`, `read_at`, and seekable 64K reads, and removed plaintext filename fallbacks in `lookup`, `create`, `list_dir`, and `delete_file`.
  - Added `DiskManagerError::NotEncrypted` to refuse opening an unencrypted or downgraded image when a password or encryption key is provided.
  - Updated `open_with_password` / `open_with_key` on a new path to create an encrypted image instead of silently creating a plaintext image.
  - Added `MAX_READ_DATA_BYTES` (2 GiB), `MAX_DECOMPRESSED_STREAM_BYTES` (256 MiB), physical data-area bounds on `compressed_size`, and fallible `try_reserve_exact` allocations to prevent OOM aborts and Zstd decompression bombs from crafted `inode.size` values.
  - Enforced `SuperBlock::validate` allow-lists for `format_version`, `encryption_version`, `migration_cursor`, and `encrypted` flag consistency.
  - Created new `.img` files with owner-only `0600` permissions on Unix and `O_NOFOLLOW` on `create_encrypted` to prevent symlink truncation.
  - Added cycle detection and depth limits to recursive CLI `ls -r` and `rm -r` (also fixing `rm -r` for directories nested deeper than 2 levels).
- **Encryption Bypass on Non-Zero-Offset Writes**: Fixed the write planner so that a compressed, filtered, or encrypted payload is always rebuilt as a whole (or upgraded to context-bound 64 KiB AEAD chunks) instead of being patched in place, preventing plaintext leakage and Poly1305 tag corruption.
- **AEAD Context Binding**: Bound `XChaCha20-Poly1305` ciphertexts via Poly1305 associated data to `inode_id` (and `chunk_idx` + `flags` for seekable chunks) so ciphertexts cannot be swapped across files or reordered undetected. Image-wide `encryption_version` preserves read compatibility with older images.
- **Inode Table & Data Bitmap Bounds**: Derived usable inode capacity from the real 256-byte inode table span, enforced bounds across all inode entry points and both allocators, and rejected impossible layouts in `SuperBlock::validate`.
- **Data-Area Lower-Bound Checks**: Rejected block pointers outside the data area across read paths, write paths, and WAL replay.
- **Unzeroed Data Blocks**: Zeroed freshly allocated data blocks and extension slack bytes across both in-place and journaled paths so recycled blocks never expose deleted file plaintext.
- **Chunk-Aware Defragmentation**: Decoded packed `ChunkEntry` values for seekable 64K files during defragmentation so chunk runs relocate intact and safe defragmentation preserves the chunked format.
- **WAL-First Truncation & Non-Empty Directory Protection**: Staged indirect pointer table zeroing inside WAL transactions on journaled shrink paths and rejected deleting non-empty directories before touching parent entries.
- **IPC Deserialization & Transport Hardening**:
  - Enforced hard byte allocation limits during frame deserialization.
  - Replaced cleartext password transmission with keyed BLAKE2b challenge-response authentication over a direction-separated transcript with mutual key verification, and added random session secrets for unencrypted images.
  - Moved Unix sockets into per-user `0700` directories with `0600` socket/secret/rendezvous files, bounded concurrent peers (256) with a 10 s handshake budget, and defaulted network IPC to loopback unless `--allow-remote-ipsc` is explicitly passed.
- **FFI, MCP & Password Hygiene**:
  - Wrapped `CStr::from_ptr` conversions and `oifs_set_io_backend` / `oifs_get_io_backend` / `oifs_loaded_path` inside `catch_unwind` boundaries, and rejected null buffers with non-zero lengths in `oifs_write_file_with_policy`.
  - Built MCP JSON responses with `serde_json` and checked arithmetic.
  - Stored CLI and remote-session reconnection passwords in `zeroize::Zeroizing<String>` and wiped input scratch buffers.

### Performance
- **JBD2-Style Deferred Metadata Checkpointing & Durability-Aware COW Defragmentation**: Coalesced non-wrapping WAL header + frame `msync(MS_SYNC)` into a single syscall, deferred in-place metadata `msync` under `DurabilityMode::Strict` on journaled images to the 50% WAL watermark (`2.31x` higher `Strict` write throughput), and upgraded in-place COW defragmentation to compact word-level bitmaps, WAL op coalescing, and durability-aware barriers (`3.25x–4.53x` faster).
- **Incremental $O(\Delta \text{chunks})$ Seekable Truncate & Indirect Table Pruning**: Eliminated full-file chunk rescans during `truncate_chunked_64k` and pruned unused Single/Double/Triple indirect pointer tables on truncation.
- **Batch Indirect Reads & 32-Shard Directory Cache**: Coalesced contiguous `Mmap` extents and skipped sparse indirect subtrees in `read_at`, and sharded `ShardedDirCache` across 32 shards with out-of-lock index promotion (`9.97x` faster under contention).
- **Pooled Zstd Contexts and Single-Pass Decompression**: Added thread-local Zstd `CCtx`/`DCtx` pooling, single-pass chunk decompression into caller-provided buffers, and automatic frame consolidation when multi-frame overhead exceeds 25%.
- **IPC Payload Copies and Pooled Buffer Retention**: Released pooled frame buffers exceeding 1 MiB, passed requests by reference in `IpcClient::send`, and avoided materializing event names when no listener is attached.

### Fixed
- **Auto-Defrag Configuration Boundaries**: `AutoDefragConfig::sanitized` replaces `NaN` thresholds with the default and clamps zero intervals to `MIN_CHECK_INTERVAL`, preventing busy-spin loops; coordinator locks use unpoisoned acquisition helpers.
- **Kani Harness Maintenance**: Replaced symbolic loop unwinding in `SetDataBitmapRange` replay with span-wide bit updates, added `cover!` assertions to idempotency proofs, and updated `proof_crc_input_is_deterministic` to use `crc_frame_checksum`.
- **Release Verification Tooling**: Updated `scripts/release_check.sh` to run the full Kani suite in parallel (`cargo kani -j 8 --output-format=terse`) and added native LLVM coverage reporting.

## [1.1.0] - 2026-10-14

### Added
- **Seekable 64KB Chunked Compression & Out-of-Lock Parallel Pipeline**:
  - Implemented 3-stage pipeline: Stage 1 shared read-lock inspection, Stage 2 zero-lock multi-threaded parallel Zstd compression and XChaCha20 encryption, and Stage 3 atomic write-lock commit (`write_chunked_64k_pipeline`).
  - Added sub-file random rewind writes with fine-grained chunk dirtying, read-modify-write decompressed chunk preservation, and anti-inflation raw chunk fallback.
  - Added incremental $\Delta$ tracking for seekable chunk compressed size.
  - Added contiguous chunk block allocation (`allocate_contiguous`, `free_contiguous`).
- **Metadata WAL Journaling**:
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
  - Implemented sharded bounded inode cache (`BoundedInodeCache`) with interior lock striping to eliminate global cache miss lock contention.
  - Implemented directory cache acceleration (`DirIndex`) and zero-alloc path resolution.
  - Defaulted `OifsSession` to high-performance standalone mode with opt-in IPC.
- **Verification & Tooling**:
  - Expanded formal verification suite to 59 Kani proofs covering filter bijectivity, block addressing, directory lookups, CRC32C, journal ring buffer invariants, and version compatibility soundness.
  - Added TLA+ specification for Copy-on-Write seekable chunk replacement.

### Changed
- **Performance Optimizations**:
  - Replaced $O(N^2)$ block deduplication scan in `stage_payload_write` with fast $O(1)$ set lookup, doubling 32MB overwrite throughput from 8.8 GB/s to 18.5 GB/s (+110%).
  - Added mutation range coalescing to merge adjacent and overlapping block ranges before `msync` syscalls.
  - Enabled `TCP_NODELAY` on network IPC client and server sockets to eliminate Nagle's algorithm round-trip latency stalls.

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
