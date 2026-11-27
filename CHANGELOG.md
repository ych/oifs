# Changelog

All notable changes to the OIFS project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.7.0] - 2026-11-28

### Added
- **Default `v3` Hardware-Accelerated Authenticated Encryption (`src/encryption.rs`, `src/disk.rs`, `src/disk/chunked.rs`)**:
  - Added `EncryptionCipher` (`Aes256Gcm` default `v3`, or `XChaCha20Poly1305` `v2` via `--cipher` / FFI / C++ / MCP). Default **v3** uses single-pass hardware-accelerated **C2SP XAES-256-GCM** (24-byte nonces, AArch64 `aes_armv8` + `polyval_armv8` / x86_64 AES-NI + PCLMULQDQ) for payloads and **RFC 8452 AES-256-GCM-SIV** for deterministic `_e_...` filenames (`parent_inode` bound in nonce and AAD).
  - Added keyed BLAKE2b-256 `SuperBlock::header_mac` verified at mount time for `v3` and new `v2` encrypted images to prevent cipher-version, salt, and format downgrades (`legacy v1` remains readable and mounts read-only automatically against AAD downgrade).
- **`oifs_mcp` Anti-Token-Drain Pagination & Machine-Readable Agent Discovery Manifests**:
  - Added bounded pagination to `oifs_mcp` (`src/bin/oifs_mcp.rs`): `read_file` supports `offset` / `max_bytes` (256 KiB default cap, 4 MiB hard cap, UTF-8 boundary-safe trimming, and continuation hints), and `list_dir` supports `offset` / `limit` (500-entry default page, 5,000 hard cap, JSONL pagination footer).
  - Added standard agent/registry discovery files: `server.json`, `glama.json`, `llms.txt`, `SKILL.md`, `SECURITY.md`, `.well-known/mcp.json`, `.well-known/agent.json`, and GitHub Pages mirrors in `docs/`.
- **Kani Formal Verification Suite Expanded to 135 Harnesses (+4 since v1.6.1)**:
  - Added `proof_coalesce_data_bitmap_intervals_equivalence`, `proof_encryption_cipher_roundtrip_and_v3_filename_nonce_injective`, and additional filter/codec formal harnesses (`135` total `#[kani::proof]` harnesses in `src/`).

### Performance
- **Authenticated Encryption Hot Path (`src/encryption.rs`, `src/disk.rs`, `src/disk/chunked.rs`)**: Pre-expanded `Aes256` & `Aes256GcmSiv` key schedules in `EncryptionKey`, stack Base64url short-filename encoding, capacity-reserved `encrypt_data_for_scheme`, per-batch `PrecomputedBatchAead` counter nonces for Seekable 64K writes, and out-of-lock optimistic AEAD encryption on `create_file_with_data_with_filters`.
- **`StorageMode::FileIo` Syscall Batching & Cache Fast Paths (`src/storage.rs`, `src/disk.rs`, `src/disk/chunked.rs`)**: Added lock-free `count == 0` cache bypasses and read-probe fast paths in `MetadataBlockCache` / `DataPageCache`, coalesced contiguous cold-miss `pread` runs in `read_data_cached`, combined chunk block writes via `write_chunk_blocks_owned`, completed `set_dirty_bits` bitmap tracking, and eliminated redundant `fdatasync` barriers in `DiskManager::flush()`.
- **Metadata WAL Interval Coalescing & Compact Diff Slices (`src/journal.rs`, `src/disk.rs`)**: Replaced quadratic bitmap coalescing in `coalesce_ops` with $O(N \log N)$ `OrderedIntervalMap` merging, added cache-pollution-free `block_has_nonzero_bytes` scanning, and emitted minimal sub-block WAL diff slices (`minimal_fresh_block_slice_op` / `minimal_diff_block_slice_op`) for directory and small-file commits.
- **Lazy `WriteBatch` Namespace Validation (`src/disk/batch.rs`)**: Replaced full parent-directory scans during `WriteBatch` validation with lazy per-entry `dir_lookup` probes and cached `(stored_name, stored_hash)` in `PreparedBatchOp`.
- **Zero-Copy Decode, `ChunkReadCache` & Pooled Multi-Frame Zstd (`src/disk/caches.rs`, `src/disk/chunked.rs`, `src/zstd_pool.rs`, `src/disk.rs`)**: Stored `Arc<Vec<u8>>` directly in `ChunkReadCache` (`MAX_PER_SHARD = 4`), added zero-`memset` `WriteBuf` Zstd frame decompression (`decompress_frame_to_vec*`) and pooled TLS `StreamDecompressorSlot` reuse across multi-frame Zstd streams, routed `FileIo` chunk reads through `read_data_cached`, skipped redundant zero-fills on hole-free `Mmap` reads, and enabled parallel multi-chunk Seekable-64K `read_at` (`>= 192 KiB`) on unaligned offsets via `TL_BOUNDARY_SCRATCH`.
- **NEON / SSE2 `ByteShuffle` & 128-Byte Tree-Scan `Delta` Filters (`src/filters.rs`)**: Vectorized `ByteShuffle` (`typesize = 2, 4, 8`) and `delta_encode_inplace` (`typesize = 1, 2, 4, 8`), and upgraded `delta_decode_inplace` (`typesize = 1, 2, 4, 8`) to a 128-Byte (8-vector) decoupled binary-tree prefix sum.

### Documentation
- Updated `README.md`, `README_zh.md`, `docs/index.html`, `docs/formal_verification/README.md`, `CHANGELOG.md`, `include/oifs.h`, and BaaaTON SVG certification badge for `v1.7.0`.

## [1.6.1] - 2026-11-26

### Fixed
- **Reduced `DiskManager` / `FileBacking` Stack Footprint (`src/disk.rs`, `src/disk/caches.rs`, `src/storage.rs`)**:
  - Heap-boxed the 128-byte `CachePadded` shard arrays in `ChunkReadCache`, `DecodedBlobCache`, `MetadataBlockCache`, and `DataPageCache`, and replaced `Box::new([0u64; 4096])` with direct heap allocation (`DiskManagerInner::alloc_zeroed_inode_write_gen`), shrinking `DiskManager::init_or_open_with_key`'s unoptimized debug stack frame below 64 KiB coroutine stacks (fixing debug `SIGBUS` in `shuttle_concurrency_test`) while preserving full 128-byte cache-line isolation.
- **Inode v3 CRC32C Tampering Test Alignment (`tests/encryption_test.rs`)**:
  - Updated `test_encrypted_image_rejects_unencrypted_inode_injection` to recompute the unkeyed Inode v3 CRC32C checksum (`[171..175)`) after raw byte injection so reads exercise the `!inode.encrypted` -> `DiskManagerError::DecryptionFailed` guard instead of stopping earlier at `InodeFormatError::ChecksumMismatch`.

## [1.6.0] - 2026-11-26

### Added
- **Dynamic Auto-Grow, Online Resize & Vacuum (`src/disk/resize.rs`)**:
  - Added `DiskManager::grow`, `DiskManager::resize`, `DiskManager::vacuum`, `DiskManager::set_auto_grow`, `DiskManager::auto_grow_config`, and `DiskManager::image_size_bytes` (with matching `OifsSession` and CLI `--auto-grow[=MAX_MB]`, `oifs resize --size|--bytes`, and `oifs vacuum` commands).
  - Supports zero-relocation O(1) in-place grow/tail shrink within bitmap boundaries (`SuperBlock::in_place_block_count_range`) and inode-preserving streaming atomic re-layout (`AtomicRelayout`, using `0600` temp files, exclusive file lock transfer, and atomic `rename`) across the 128 MiB bitmap boundary and inode-table expansions (`SuperBlock::new_v3_with_min_inodes`), preserving plain, encrypted, Seekable 64K, and custom Zstd-dictionary files.
  - Wired transparent Auto-Grow (`ensure_capacity_for` + bounded `NoSpace` retry, disabled by default) across `create_entry_internal`, `create_file_with_data_with_filters`, `write_data_with_filters`, `rename`, `write_chunked_64k`, `WriteBatch::commit`, `transfer_file_to`, and `splice_range_to`, with `tests/resize_vacuum_autogrow_test.rs`.
- **Custom Zstd Dictionary Support (`src/zstd_pool.rs`, `src/disk.rs`, `src/disk/chunked.rs`, `src/disk/transfer.rs`)**:
  - Added `ZstdDictRegistry` (16 MiB per-dict / 64-dict cache bounds) with lock-free shared `Arc<CDict>`/`Arc<DDict>` contexts pooled per thread (`compress_frame_with_cdict` / `decompress_frame_with_ddict`), automatic small-file compression bypass in `CompressionMode::Auto` (with anti-inflation fallback), Seekable 64K / Stream / Encrypted / `FileIo` coverage, `transfer_file_to`/`splice_range_to` dictionary compatibility fallback, `train_zstd_dict` / `train_zstd_dict_from_dir`, and CLI `--dict` + `oifs dict train`.
- **Explicit Multi-Op Write Batch API (`src/disk/batch.rs`)**:
  - Added `DiskManager::begin_write_batch()` (`WriteBatch`, `BatchOp`, `BatchCommitResult`) coalescing mixed `CreateFileWithData`, `Mkdir`, `Delete`, and `Rename` operations into chunked WAL transactions (96 KiB per-chunk budget for the 128 KiB circular WAL ring) and single write-lock / coalesced range-sync commits.
- **Inode v3 Hardware-Accelerated CRC32C Checksums (`src/inode_format.rs`)**:
  - Added 256-byte Inode `record_version == 3` CRC32C checksums covering `[0..171)` while maintaining full read compatibility with legacy v2 and v1 images, integrated with `DiskManager::verify_integrity()` (`FsckReport::corrupted_inodes`) and `tests/inode_crc_dict_batch_test.rs`.
- **Kani Formal Verification Suite Expanded to 131 Harnesses (+18 since v1.5.0)**:
  - Added 2 proofs in `src/superblock.rs`, 5 proofs in `src/disk/resize.rs`, 3 proofs in `src/inode_format.rs`, 2 proofs in `src/disk/batch.rs`, 1 proof in `src/disk/caches.rs`, 2 proofs in `src/disk.rs`, 2 proofs in `src/disk/transfer.rs`, and 1 proof in `src/encryption.rs`.

### Security & Reliability
- Fixed `Strict` WAL ring wrap padding persistence, WAL crash-recovery header reset ordering prior to flush, C++20 coroutine `Awaiter` lifetime UAF hazards, Stage-1→Stage-2 concurrent source-mutation TOCTOU in `transfer_file_to`, destination layout/deletion TOCTOU races in `splice_range_to`, and non-journaled Seekable 64K `ENOSPC` dangling block-pointer cleanup.

### Performance
- **128-Byte Cache-Line False-Sharing Isolation (`CachePadded<T>`)**: Promoted `#[repr(align(128))]` `CachePadded<T>` to `src/lib.rs` (covering both Apple Silicon 128B and x86_64 64B cache lines), isolated all shard locks and atomic counters across `ChunkReadCache`, `DecodedBlobCache`, `ShardedDirCache`, `BoundedInodeCache`, `MetadataBlockCache`, `DataPageCache`, and `zstd_pool::SharedTask`, and added a cache-line-isolated lock-free `DiskManager::auto_grow_enabled` fast bypass for steady-state mutations.
- Added a zero-sort stack fast path for 1–2 intervals in `DiskManagerInner::coalesce_ranges_with_gap`.

### Documentation
- Updated `README.md`, `README_zh.md`, `docs/index.html`, `docs/formal_verification/README.md`, and `AGENTS.md` for `v1.6.0` and 131 Kani formal verification proofs.

## [1.5.0] - 2026-11-25

### Added
- **Pure-Rust Local Loopback NFSv3 Server (`src/nfs.rs`)**:
  - Added a zero-dependency, kext-free, reboot-free local NFSv3 + MOUNT v3 server multiplexed over a single loopback TCP port (`127.0.0.1:0`) with stateless 16-byte file handles, bounded ONC RPC v2 record framing, all 18 NFS v3 procedures (including atomic `RENAME`, paginated `READDIR`/`READDIRPLUS`, UTF-8 NFD→NFC normalization, macOS Finder metadata filtering, and strict `NFS3ERR_ROFS` read-only enforcement).
  - Wired into `oifs mount <mountpoint> [--backend auto|nfs|fuse] [--port <u16>]` (making zero-kext NFSv3 the default `--backend auto` on macOS), with `tests/nfs_test.rs`, `tests/nfs_live_mount_test.rs`, and 3 Kani formal proofs.
- **Kani Formal Verification Suite Expanded to 113 Harnesses (+4 since v1.4.0)**:
  - Added 3 NFSv3 proofs in `src/nfs.rs` and 1 exact-count worker wakeup liveness proof in `src/zstd_pool.rs`.
- **Regression Test Suites**:
  - Added `tests/signal_storm_test.rs`, `tests/data_integrity_regressions_test.rs`, `tests/security_auth_regressions_test.rs`, `tests/wal_and_concurrency_regressions_test.rs`, `tests/protocol_ffi_stability_regressions_test.rs`, and `tests/perf_regressions_test.rs`.

### Security & Reliability
- **Upgraded Zstd Stack (`zstd 0.14.5`, `zstd-safe 8.4.0`, `zstd-sys 2.2.0+zstd.1.6.0`)**:
  - Bundled upstream C `zstd 1.6.0` hardening fixes and `zstd-safe 8.4.0` addressing `RUSTSEC-2026-0401` and `RUSTSEC-2026-0402`.
- **Reduced Default Dependency Surface by 50% (112 → 56 crates; 65 → 49 crates `--no-default-features`)**:
  - Moved the optional MCP stack (`rmcp`/`tokio`/`schemars`) out of default features (`--features mcp`), trimmed unused `clap` color/suggestions crates behind `cli`, replaced `zeroize_derive` proc-macro on `EncryptionKey` with explicit `Zeroize`/`Drop`/`ZeroizeOnDrop` impls, and limited `argon2`'s `password-hash` feature to dev-dependencies.
- **Crypto, Auth, WAL & Protocol Hardening**:
  - Authenticated `_e_` filename SIV decryption (`decrypt_filename_authenticated`), zeroized Argon2id/subkey/ChaCha20 key material, enforced cached-session password verification in `OifsSession`, removed pre-lock file truncation, and rejected stale/deleted handles in FUSE (`-ESTALE`) and NFS (`NFS3ERR_STALE`).
  - Hardened `write_data_journaled` with Copy-On-Write before WAL commit, pre-checkpointed `Strict` WAL ring wraps instead of dropping frames, persisted ordered `migration_cursor` progress, added `inode_write_gen` Stage-3 staleness detection, fixed `OifsAsyncTask` worker-panic wakeup, and hardened syscalls against `EINTR` signal storms and race-free IPC shutdown.
  - Enforced pre-auth 4 KiB IPC frame limits, handshake timeouts, atomic `0o600` `.master` files, NFS fragment/connection/socket bounds, FUSE `SCM_RIGHTS` alignment/truncation checks, C++20 `Awaiter` lifetime safety, and FFI batch buffer overlap/`isize::MAX` guards.

### Performance
- **Lock Contention & Hot Read Paths**:
  - Cache-line padded (`128 B`) `DiskManagerInner` behind `InnerLock` to eliminate false sharing with `RwLock` reader-count atomics, sharded `ChunkReadCache` and `DecodedBlobCache` 16 ways with lock-free empty bypasses, decoded v2 `Mmap` inodes directly from the mapping, and added an atomic fast path for FFI `last_error` (`1.6x–2.4x` faster 8-thread reads).
  - Added lock-free monotonic SeqLock fast slots in `FuseHandleTable`, wait-free `AtomicU64` parent lookup tables in FUSE/NFS, and generation-keyed sorted directory snapshot caching for multi-page FUSE/NFS `readdir`/`readdirplus` (`~26x–40x` faster multi-page listings).
  - Added exact-count worker wakeups in `zstd_pool::par_for_index`, in-place `dir_cache` updates on `rename`, `O_RDONLY` FUSE flush bypass, zero-alloc cold probes in `read_at_async`/`read_data_async`, non-blocking MCP tool execution with `O(1)` EOF append, and instant `IpcServer::drop` wakeup.

### Documentation
- Added crawlable GitHub Pages landing page (`docs/index.html`), `sitemap.xml`, `robots.txt`, and JSON-LD metadata.
- Cleaned up resolved items and completed roadmap entries in `docs/similar_projects_comparison.md`.

## [1.4.0] - 2026-11-22

### Added
- **Pure-Rust Cross-Platform FUSE Kernel Protocol (ABI v7.28) & VFS Engine (`src/fuse.rs`)**:
  - Added a zero-external-C-dependency FUSE ABI v7.28 wire parser, panic-free binary frame dispatcher (`OifsFuseFs::dispatch_frame`, `dispatch_frame_for_platform`), high-level VFS API (`lookup`, `getattr`, `setattr_size`, `mkdir`, `unlink`, `rmdir`, `rename`, `open`, `create`, `read`, `write`, `flush`, `fsync`, `release`, `opendir`, `readdir`, `releasedir`, `statfs`, `access`, xattr handlers), and stateful `FuseHandleTable` multiplexing thousands of open FUSE handles (`fh`) over the single underlying `.img` OS file descriptor.
  - Added cross-platform Linux and macOS kernel mount & transport support: atomic packet device I/O (`serve_fuse_fd`), Unix stream runner (`serve_stream`), Linux `fusermount3`/`fusermount` `_FUSE_COMMFD` + `SCM_RIGHTS` FD passing (`send_fd_scm_rights`/`recv_fd_scm_rights`) with direct `/dev/fuse` fallback, macOS `FUSE-T` / `macFUSE` / `osxfuse` discovery (`discover_macos_fuse_backend`) with `macFUSE 5.x` `SCM_RIGHTS` handshake and non-blocking helper reap, dynamic `max_write` negotiation (`1 MiB` default, Linux `FUSE_MAX_PAGES` + `/sys/module/fuse/parameters/max_pages_limit` + `sysconf(_SC_PAGESIZE)` detection, macOS `-o iosize=<bytes>`), dual ABI wire layouts (Linux 88-byte `FuseAttr` & macOS 104-byte `FuseAttr` with `crtime`/`crtimensec`/`flags`, Linux 8-byte & macOS 16-byte `FUSE_RENAME` / xattr headers), OS-filtered mount options (`build_mount_options_for_os`), and host `unmount` helper.
  - Added macOS/cross-platform desktop compatibility: Finder metadata storm fast-rejection (`is_macos_metadata_filename` for `.DS_Store`, `._*`, `.Spotlight-V100`, `.Trashes`, `.fseventsd`, `.TemporaryItems`), `-ENOTSUP` extended-attribute replies (avoiding `-ENOSYS`/`-EINVAL` Finder copy aborts), pure-Rust UTF-8 NFD→NFC filename normalization (`normalize_filename_nfc` covering ASCII fast path, Latin diacritics, Japanese Kana dakuten/handakuten, and Korean Hangul Jamo composition), `geteuid()`/`getegid()` defaults, real non-zero `FUSE_STATFS` free/available space (`bavail = 0` when read-only), and strict `-EROFS` enforcement on read-only mounts.
  - Exposed FUSE mounting and frame dispatch across CLI (`oifs mount <mountpoint> [--foreground] [--allow-other]` honoring `--read-only`, `--no-mmap`, and `--password` in `src/bin/oifs.rs`), C FFI (`oifs_fuse_dispatch_frame` with lazy persistent `OifsFuseState` per `OIFSHandle` in `src/ffi.rs` & `include/oifs.h`), and Modern C++ (`oifs::FileSystem::fuse_dispatch_frame` in `include/oifs.hpp`).
  - Added `tests/fuse_test.rs` (10 protocol/ABI integration tests) and `tests/fuse_live_mount_test.rs` (`#[ignore]` live kernel mount E2E suite & APFS A/B benchmark verified on macOS + macFUSE 5.5.0).
- **Atomic POSIX `rename` (`DiskManager::rename`, `FUSE_RENAME` / `FUSE_RENAME2`)**:
  - Added metadata-only `DiskManager::rename(old_parent, old_name, new_parent, new_name)` supporting same-directory rename, cross-directory move, atomic regular-file / empty-directory overwrite (freeing victim blocks & inode in the same WAL transaction), directory cycle prevention (`is_ancestor_dir`), encrypted SIV filename re-encryption for `(new_parent, new_name)` without touching payload blocks, and read-only rejection.
  - Wired `FUSE_RENAME` (opcode 12, handling both Linux 8-byte and macOS 16-byte headers) and `FUSE_RENAME2` (opcode 45, including `FUSE_RENAME_NOREPLACE`) in `src/fuse.rs`.
  - Added `tests/rename_test.rs` and live macFUSE `std::fs::rename` E2E verification.
- **3-Tier Cross-Image Fast Transfer, Sub-Range Splice & Whole-Image CoW Clone (`src/disk/transfer.rs`)**:
  - Added `DiskManager::clone_image` (macOS APFS `clonefile(2)`, Linux `ioctl(FICLONE)`, portable fallback) for instant O(1) whole-image CoW forks (`2.19x` faster on 64 MiB APFS).
  - Added `DiskManager::transfer_file_to` implementing the 3-tier automatic fallback pipeline (`ioctl(FICLONERANGE)` → `copy_file_range(2)` → raw compressed block `pread`/`pwrite`) with encryption/AAD fast-path gating (`LogicalReencode` whenever either image is encrypted), `(src_dev, dst_dev)` negative reflink caching, per-extent `EINVAL` fallback, `Mmap`/`FileIo` page-cache coherence (`invalidate_block_range_caches`), WAL ordered-mode `fdatasync` before commit, Seekable 64K `ChunkEntry` rebasing, sparse zero-subtree skipping, and AB-BA deadlock-free lock discipline (`16.3x` faster 16 MiB Zstd stream, `137.7x` faster 16 MiB Delta+Shuffle+Zstd, `104.0x` faster 64 MiB sparse transfer).
  - Added `DiskManager::splice_range_to` supporting cross-image sub-range copy/move with compression & Inode filter compatibility gating, `Head / Body / Tail` sandwich partitioning (`plan_splice_partition`), `SpliceTrimMode::{Keep, PunchHole, CollapseRange}` (Copy-Commit-Then-Trim two-image crash safety, best-effort Linux `FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE`, and O(remaining) aligned pointer/chunk table collapse shifting — `2.81x` faster on 8 MiB Seekable 64K splice+collapse).
  - Added CLI `oifs clone <src> <dst>` and `oifs transfer --to <dst.img> <src_path> [--dest ...] [--move] [--overwrite] [--offset/--len/--dst-offset/--trim-mode]` (with safe IPC logical fallback in Proxy mode), plus `tests/transfer_test.rs` (16 integration, unit, AB-BA concurrency, CLI tests + before/after benchmark).
- **Expanded C FFI (`include/oifs.h`, `src/ffi.rs`) & Modern C++17/C++20 API (`include/oifs.hpp`)**:
  - Added 12 C FFI functions and 7 `#[repr(C)]` structs: `oifs_rename`, `oifs_clone_image`, `oifs_transfer_file_to`, `oifs_splice_range_to`, `oifs_ls_dir`, `oifs_stat`, `oifs_exists`, `oifs_create_file_with_data_filtered`, `oifs_write_file_with_filters`, `oifs_verify_integrity`, `oifs_analyze_fragmentation`, and `oifs_defragment`.
  - Expanded `include/oifs.hpp` (`namespace oifs`) with all new operations (`rename`, `clone_image`, `transfer_file_to`, `splice_range_to`, `ls(path)`, `stat`, `exists`, SIMD `FilterConfig`, `verify_integrity`, `analyze_fragmentation`, `defragment`), missing C FFI wrappers (`get_or_open*`, `IoBackend` `set_io_backend`/`get_io_backend`, `oifs::check_version`/`version_string`/`loaded_path`, `std::string_view` `open_read_only`), ergonomic typed helpers (`read_file`, `create_file_with_span<T>`, `write_span<T>`), and C++20 `co_await` coroutine support on `AsyncFuture` (Inline Fast-Path zero-suspend `await_ready()`).
  - Expanded `tests/cpp_api_test.rs` verifying all additions under both `-std=c++17` and `-std=c++20` with `-Wall -Wextra -Wpedantic -Werror -pthread` across `Mmap` and `OIFS_STORAGE_MODE=pread`.
- **Kani Formal Verification Suite Expanded to 109 Harnesses (+10 since v1.3.0)**:
  - Added 3 `#[kani::proof]` harnesses in `src/fuse.rs` (`proof_fuse_nodeid_inode_bijection_and_root_mapping`, `proof_fuse_dirent_align_and_record_bounds`, `proof_fuse_frame_header_parse_no_panic_and_bounds`).
  - Added 7 `#[kani::proof]` harnesses in `src/disk/transfer.rs` (`proof_transfer_fast_path_gate_soundness`, `proof_transfer_tier_combine_monotone`, `proof_transfer_chunk_rebase_and_errno_classification`, `proof_transfer_extent_coalescing_conservation`, `proof_splice_alignment_partition`, `proof_chunk_table_shift_bijectivity`, `proof_punch_hole_zeroed_reuse_equivalence`).

### Documentation & CI
- Added `docs/nfs_mount_design.md` (Pure-Rust local loopback NFSv3 Server design) and `docs/cross_image_reflink_design.md` (3-tier cross-image reflink/transfer design).
- Removed obsolete pre-implementation drafts (`docs/optimization_roadmap.md`, `docs/performance_and_verification_research.md`, `docs/seekable_chunked_compression_design.md`) and refreshed `docs/metadata_wal_design.md` and `AGENTS.md`.
- Capped OpenRouter 429 retry wait in `.github/scripts/openrouter-rate-limit.cjs` to `120s` so daily RPD quota exhaustion fails gracefully and preserves completed OpenWiki pages in a PR instead of hitting the 6-hour CI cancellation timeout.

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
