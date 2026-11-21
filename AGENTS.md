<!-- OPENWIKI:START -->

## OpenWiki

This repository has a generated `openwiki/` evidence index. It is optional just-in-time context, not required startup reading.

- Do not enumerate, preload, or search wikis at task start. Use retrieval when the user asks for it, when unfamiliar architecture or dependency behavior materially affects the task, or when source inspection leaves an important uncertainty. Stop once the question is grounded.
- When those conditions apply and OpenWiki retrieval tools are available, use `openwiki_search` for just-in-time context and `openwiki_read` for the relevant complete sections. If search returns `workspace_required`, ask which listed workspace to use and retry with its ID.
- Use `openwiki_list_workspaces` or `openwiki_list_wikis` when workspace membership itself needs to be discovered.
- If the retrieval tools are unavailable, read `openwiki/quickstart.md` and follow its links to the relevant pages.
- Treat source code and tests as authoritative. A brief's unknowns and review items are verification gaps, not automatic requirements.
- Prefer the narrowest quiet validation that proves the changed behavior. Preserve complete failure output.

Use the repository's documented maintenance process to refresh generated pages after source changes. Check its configuration and completed runs before relying on automatic updates. Do not hand-edit generated OpenWiki pages unless explicitly asked; prefer updating source code/docs and letting OpenWiki regenerate.

<!-- OPENWIKI:END -->

# OIFS (O's Inode File System) — Agent Quickstart & Architecture Guide

Welcome, Agent! This guide provides the architectural mental model, codebase map, core invariants, and workflows needed to work productively on OIFS without regressions.

---

## 1. What is OIFS?

**OIFS** is a high-performance, single-file containerized Unix-like inode filesystem written in Rust (Edition 2024 / Rust 1.85+). It packs an entire filesystem—inodes, multi-block directories, raw/compressed data blocks, encryption, and pre-compression filter pipelines—into a single `.img` container file backed by either memory mapping (`StorageMode::Mmap`, default) or pure positional `pread`/`pwrite` file I/O (`StorageMode::FileIo`, zero `mmap`).

### Key Highlights
- **Dual Image Storage Engine (`StorageMode::Mmap` & `StorageMode::FileIo`)**: Defaults to zero-copy `memmap2::MmapMut` (`48–51 GB/s` cached reads), with an opt-in 100% `mmap`-free positional `pread`/`pwrite` engine (`DiskManager::open_file_io*`, `DiskManager::open_with_storage_mode`, `--no-mmap`, `OIFS_STORAGE_MODE=pread`) backed by write-through bitmap/WAL mirrors, a 16-shard `Arc`-backed 4 MiB metadata block cache, and a configurable 64-shard 32 MiB Second-Chance Clock user-space data page cache (`OIFS_FILE_IO_CACHE_MB`, `DiskManager::set_file_io_cache_bytes`, capped at 4 GiB with `> 64 KiB` streaming scan bypass).
- **Read-Only Mode (`F_RDLCK` Shared Locks & Zero-Write `PROT_READ` Mounts)**: `DiskManager::open_read_only`, `open_read_only_with_password`, and `open_read_only_with_storage_mode` open `0444` images with `O_RDONLY`, acquire a shared `fcntl(F_SETLK, F_RDLCK)` lock (allowing concurrent multi-process read-only mounts while mutually excluding `F_WRLCK` writers), map `PROT_READ` (`ImageBacking::MmapReadOnly`) with full 0-copy `FileReadHandle::mmap_slice()` support or read-only `FileIo`, verify WAL cleanliness with zero disk writes (refusing unrecovered dirty journals), reject every mutating API with `DiskManagerError::ReadOnly`, and suppress flushes on `Drop` for 100% byte-for-byte immutability. Exposed via C FFI (`oifs_open_read_only`, `oifs_is_read_only`), Modern C++ (`oifs::FileSystem::open_read_only`, `is_read_only`), and CLI (`--read-only`, with `cat`/`info`/`stat` read subcommands).
- **Multi-Process Concurrency**: Master-Proxy architecture using OS-level file locking (`fcntl`). The first process is Master; concurrent secondary processes connect as Proxies over Unix Domain Sockets or Network TCP.
- **Pluggable I/O Engine, Shared Read Sessions & Zero-Copy / Async API**: `Mmap` (zero-copy direct slice with contiguous extent coalescing), `Pread` (contiguous extent batching), and `IoUring` (Linux async submission rings with thread-safe pooling, optional `io-uring` feature). `DiskManager::begin_read` returns a `ReadSession` (and `ReadSession::open_file` returns a pinned `FileReadHandle` with `mmap_slice()` true zero-copy views on contiguous raw `Mmap` files). `OifsAsyncTask<T>` implements `std::future::Future` with a non-blocking Inline Fast-Path (`Mmap`, `DataPageCache`, `ChunkReadCache`, `DecodedBlobCache` hits return `Ready` immediately with zero thread hop), a persistent background read/decode worker pool, and a Group-Commit async write pipeline coalescing concurrent `DurabilityMode::Strict` writes into a single `flush()` barrier.
- **Configurable Durability & Single-Transaction File Creation**: `Lazy` (fastest, OS writeback / process crash safe), `RangeAsync` (batched dirty-range coalescing drained asynchronously outside write locks), `Strict` (synchronous `msync`/`sync_data` with single-syscall WAL header+frame coalescing and JBD2-style deferred in-place metadata checkpointing), and `LegacyWholeMmapAsync`. `DiskManager::create_file_with_data` (and `create_file_with_data_with_filters`) combines file creation and initial payload write into a single lock acquisition and single WAL transaction, data-journaling small payloads (`<= 8 KiB`) directly inside the WAL commit under `Strict` mode (1 sync barrier instead of 3).
- **Pre-compression Filters**: Built-in SIMD-accelerated Delta, Byte Shuffle, BitShuffle (delta-swap ILP 8x8 transposition), and TruncPrecision followed by Zstandard (with optional `blosc2` feature).
- **Encryption**: XChaCha20-Poly1305 with OWASP / RFC 9106 hardened Argon2id KDF (`64 MiB`, `t=3`, `p=4`), BLAKE2b-512 domain-separated subkeys (`payload_key`, `filename_siv_key`, `filename_cipher_key`, `ipc_auth_key`), and 24-byte (192-bit) `XChaCha20-Poly1305` deterministic SIV filename encryption (`_e_...`) with `parent_inode` bound in Poly1305 AAD. Payloads leverage AArch64 NEON (via `.cargo/config.toml`) and x86_64 AVX2 SIMD, while short filenames (`<= 192 B`) use an RFC 8439-identical 1-block scalar fast path to avoid multi-block SIMD overhead.
- **Encrypted Image Integrity & Anti-Downgrade**: Encrypted images reject unencrypted regular files (`!inode.encrypted`) on all read paths (`read_data`, `read_at`, seekable 64K) and reject plaintext directory entries in `lookup`, `create`, `list_dir`, and `delete_file`. Providing a password or key for an unencrypted/downgraded image returns `DiskManagerError::NotEncrypted`; `open_with_password` / `open_with_key` on a new path creates an encrypted image.
- **C FFI & Header-Only Modern C++17/C++20 Native API (`include/oifs.h`, `include/oifs.hpp`)**: 3-state runtime version & path verification (`oifs_check_version`, `oifs_loaded_path`: 0=OK, 1=WARN, -1=ERR), Arc-pinned `OIFSReadSession`/`OIFSFileHandle` (out-of-order destruction safe), `oifs_file_mmap_slice`, `oifs_read_at_batch`, `OIFSAsyncFuture` (with `wait_on_drop` UAF drop barrier), and callback APIs; wrapped by `include/oifs.hpp` (`oifs::FileSystem`, `oifs::ReadSession`, `oifs::FileReadHandle`, RocksDB-style 0-copy/1-copy `oifs::PinnableSlice`, `oifs::AsyncFuture`, and exception-safe template lambda callbacks).
- **Formal Verification**: 99 Kani proofs verifying filter bijectivity, block addressing, directory lookups, checked arithmetic, CRC32C torn-write detection, journal ring/checkpoint invariants, version policy soundness, chunk entry bit-isolation/state predicates, sharded directory cache Fibonacci hashing, indirect hole-skip conservation, contiguous mmap run coalescing, seekable truncate incremental size conservation, encryption AAD domain separation, `BitmapSliceMut::set_dirty_bits` byte-range coverage, `MetadataBlockCache`/`DataPageCache` shard-index bounds & capacity clamping, `FileReadHandle::mmap_slice` bounds soundness, `policy_to_compression_mode` total coverage, read-only storage access & mount/drop/open-mode invariants, and bounds safety.
- **On-Disk Format v2 & v3**: Fixed 256-byte inode records with explicit little-endian fields, a 1-byte file-type tag, and zeroed reserved space. The v3 layout sizes inode and data bitmaps dynamically with the image so images scale beyond 128 MiB. Legacy v1 images stay fully readable and writable; `oifs migrate` upgrades them in place.
- **Metadata WAL (Journaling, opt-in)**: `--journal` reserves 33 blocks (1 header + 128 KB ring) and records `create_file`/`mkdir`/`delete_file`/`write_data`/`truncate`/`defragment` as CRC32C-checksummed transactions. WAL-first, idempotent redo, torn-write rejection, and checkpointing. Payload data is *not* journaled — it is flushed before the metadata commit (ordered mode), so WAL traffic stays proportional to metadata, not file size. Recovery runs automatically at mount.
- **Encrypted Writes Never Bypass the Cipher**: A compressed, filtered, or encrypted payload is rebuilt as a whole (or transparently upgraded to context-bound 64 KiB AEAD chunks for non-zero-offset writes on v2+ non-filtered encrypted files), so a write at a non-zero offset can never store plaintext on an encrypted image.
- **Zeroed Data Blocks**: A freshly allocated data block (and file-extension slack) is zeroed before it is handed to a new owner, so a recycled block can never expose deleted or truncated plaintext.
- **Context-Bound Encryption**: The AEAD associated data binds a ciphertext to its `inode_id` (and a seekable chunk to its `chunk_idx` and `flags`), so a ciphertext cannot be moved to another file or reordered undetected. Image-wide `encryption_version` selects the scheme so older images stay readable.
- **Pooled Zstd Contexts & Persistent Scoped Worker Pool**: `src/zstd_pool.rs` provides thread-local `CCtx`/`DCtx` pools backed by a bounded global recycling pool plus a zero-dependency persistent scoped worker pool (`par_for_index`) for parallel seekable 64K chunk compression, decompression, and sequential 4-chunk read-ahead.
- **Chunk-Aware & Background Auto-Defragmentation**: Defragmentation decodes packed `ChunkEntry` values for seekable 64K files so chunk runs relocate intact. In-place COW defragmentation is fully WAL-atomic and supported by the opt-in background coordinator (`--auto-defrag[=THRESHOLD]`, `DiskManager::set_auto_defrag`) whose worker holds only a `Weak` reference.
- **Bounded Format, Memory & File Security**: `SuperBlock::validate` enforces version allow-lists (`format_version`, `encryption_version`, `migration_cursor`, `encrypted` flag consistency) and table/region bounds; allocators and block pointers are bounded to their respective regions; whole-file reads and stream decompression enforce `MAX_READ_DATA_BYTES` (2 GiB), `MAX_DECOMPRESSED_STREAM_BYTES` (256 MiB), physical `compressed_size <= data_area_bytes`, and fallible `try_reserve_exact` allocations against forged-size OOM and Zstd bombs; new images are created with owner-only `0600` permissions on Unix (`O_NOFOLLOW` on `create_encrypted`); CLI `ls -r` and `rm -r` enforce cycle detection and depth limits.
- **Hardened IPC Transport**: Frames are deserialized with a hard allocation limit, authentication uses keyed BLAKE2b challenge-response (passwords never cross the wire), sockets and rendezvous files are owner-only (`0700`/`0600`), concurrent peers are bounded (256), and TCP binds loopback by default unless `--allow-remote-ipsc` is explicitly passed.

---

## 2. On-Disk Layout (4096-byte Blocks)

```text
+-------------------+--------------------+-------------------+---------------------+-------------------------+
| Block 0           | Blocks 1..I        | Blocks I+1..D     | [Optional WAL]      | Inode Table -> Data     |
| SuperBlock (4KB)  | Inode Bitmap       | Data Bitmap       | 33 Blocks (1+32)    | 256B Slots -> 4KB Data  |
+-------------------+--------------------+-------------------+---------------------+-------------------------+
```

1. **Block 0 - SuperBlock**: Magic (`0x4F494653`), block counts, bitmap offsets, root inode (0), encryption salt/version, format version, and migration cursor.
2. **Inode Bitmap**: Dynamically sized in v3 (1 block tracks up to 32,768 inodes). Bit=1 means allocated.
3. **Data Bitmap**: Dynamically sized in v3 (1 block tracks up to 32,768 data blocks).
4. **Optional Metadata WAL**: 33 reserved blocks (1 header + 128 KB circular ring) placed between the data bitmap and inode table when `--journal` is enabled.
5. **Inode Table**: Contiguous table of 256-byte `Inode` structures (16 inodes per 4KB block).
6. **Data Blocks**: Stores directory entry lists, file payloads, seekable chunk runs, and indirect pointer tables.

### Inode Block Addressing (Max ~513 GB)
An inode stores:
- **10 Direct Blocks**: Blocks 0..9 (up to 40 KB)
- **1 Single Indirect**: Blocks 10..521 (512 pointers, up to 2 MB)
- **1 Double Indirect**: Blocks 522..262,665 (512² pointers, up to 1 GB)
- **1 Triple Indirect**: Blocks 262,666..MAX (512³ pointers, up to 513 GB)

*Note: All pointer resolution logic is centralized and formally proven in `src/inode.rs::BlockPath`.*

---

## 3. Codebase File Map

| Path | Purpose & Key Types |
| :--- | :--- |
| `src/lib.rs` | Public re-exports, module tree, custom `FxHashMap`/`FxHashSet`, and `BLOCK_SIZE = 4096`. |
| `src/storage.rs` | Pluggable image storage engine (`ImageBacking`: `Mmap`, `PROT_READ` `MmapReadOnly`, & `FileIo`, `FileBacking`, `BitmapSliceMut`, `JournalRegionMut`, 16-shard `MetadataBlockCache`, 64-shard `DataPageCache`). |
| `src/superblock.rs` | `SuperBlock` definition, v3 layout geometry calculation, strict `validate`, Kani proofs. |
| `src/inode.rs` | `Inode` (256B `#[repr(C)]`), `FileType`, `ChunkEntry`, `BlockPath` tier resolver. |
| `src/inode_format.rs` | On-disk inode encoding: fixed 256B v2 record, legacy `bincode` v1 decoder, version dispatch. |
| `src/disk.rs` | Core storage engine: `DiskManager` (`open_read_only*`, `DiskManagerError::ReadOnly`), `DiskManagerInner`, `DurabilityMode`, `ReadSession`, `FileReadHandle` (`mmap_slice`), `OifsAsyncTask` (Inline Fast-Path & Group-Commit write pipeline), `create_file_with_data`, open/mount/WAL recovery, directory & standard file read/write/truncate paths. |
| `src/disk/caches.rs` | In-memory caches: `RwLock`-backed `ChunkReadCache`, `DecodedBlobCache`, 32-shard `ShardedDirCache`, and 32-shard Second-Chance `BoundedInodeCache`. |
| `src/disk/chunked.rs` | Seekable 64K chunked compression/encryption engine (`write_chunked_64k`, `read_at_chunked_64k` with 4-chunk parallel read-ahead, non-journaled in-place reuse, `truncate_chunked_64k`). |
| `src/disk/maintenance.rs` | Online maintenance: `migrate`, `analyze_fragmentation`, `defragment` (`Safe` / `InPlace` / `Journaled COW`), and `verify_integrity` (`fsck`). |
| `src/directory.rs` | `DirectoryEntry`, multi-block directory iteration, 64-bit SipHash filename hashing. |
| `src/io_engine.rs` | `IoEngine`, `IoBackend` (`Mmap`, `Pread`, `IoUring`), `ExtentList`, extent merging. |
| `src/journal.rs` | Optional Metadata WAL: CRC32C, transaction frames, `MetadataOp`, `JournalRing`, `AllocSim`, idempotent redo. |
| `src/auto_defrag.rs` | Background automatic defragmentation coordinator: `AutoDefragConfig`, `AutoDefragStats`, worker thread. |
| `src/zstd_pool.rs` | Thread-local + global-recycled Zstd `CCtx`/`DCtx` pool and zero-dependency persistent scoped worker pool (`par_for_index`). |
| `src/bitmap.rs` | `BitmapRef` / `Bitmap`, fast 64-bit word `tzcnt` free-bit scanner, range set/clear, set-bit iterator. |
| `src/allocator.rs` | `SimpleBlockAllocator`, sequential block/inode allocation with hint and contiguous allocation. |
| `src/filters.rs` | Numeric pre-compression filters: Delta (NEON/SSE2 SIMD), Shuffle, BitShuffle (delta-swap ILP), TruncPrecision. |
| `src/encryption.rs` | Argon2id KDF (OWASP/RFC 9106), BLAKE2b-512 subkeys, XChaCha20-Poly1305 AEAD (NEON/AVX2 payloads + 1-block scalar short-filename fast path), 192-bit SIV filename cipher. |
| `src/session.rs` | `OifsSession`: Process-level session registry (`get_or_open`), Master/Proxy dispatch. |
| `src/ipc.rs` | `IpcServer`, `IpcClient`, length-prefixed binary framing, challenge-response auth, connection pooling. |
| `src/ffi.rs` | C-compatible dynamic library bindings (`include/oifs.h`), `oifs_open_read_only`/`oifs_is_read_only`, Arc-pinned sessions, zero-copy mmap views, `OIFSAsyncFuture` drop-barrier, and callback APIs. |
| `include/oifs.hpp` | Header-only Modern C++17/C++20 native wrapper (`namespace oifs`: `FileSystem` including `open_read_only`/`is_read_only`, `ReadSession`, `FileReadHandle`, `PinnableSlice`, `AsyncFuture`, `ByteView`, `MutableByteSpan`, `Error`). |
| `src/bin/oifs.rs` | CLI application entry point (`clap`-based), `--no-mmap` & `--read-only` flags, `cat`/`info`/`stat` commands, cycle-safe recursive `ls -r` / `rm -r`. |
| `src/bin/oifs_mcp.rs` | AI Model Context Protocol (MCP) server for IDE/agent tooling (optional `mcp` feature). |
| `tests/` | 69 integration test files (73 total test targets): Bolero property/fuzz suites, stress, crash-safety, concurrency, journal, auto-defrag, `FileIo` storage & data page cache, `read_only_mode_test`, `ReadSession` / `create_file_with_data`, `async_and_ffi_test`, `cpp_api_test`, IPC-security, block-pointer & size/bomb bounds, inode-bounds, defrag-seekable, zeroed-block, zstd-pool, encrypted-context-binding, large-image-bitmap, and CLI suites. |
| `docs/cpp_and_async_api_design.md` | Architecture, trade-offs, and safety invariants for `include/oifs.hpp`, C FFI zero-copy sessions, Async Inline Fast-Path, and Group-Commit writes. |
| `docs/optimization_roadmap.md` | Detailed optimization changelog and benchmarks. |
| `docs/metadata_wal_design.md` | Metadata WAL specification, durability modes, and recovery guarantees. |
| `docs/performance_and_verification_research.md` | Audit of performance bottlenecks, safety properties, and formal verification blueprints. |

---

## 4. Key Architectural Patterns & Invariants

1. **Lock Hierarchy & Concurrency**:
   - `DiskManager` wraps `Arc<RwLock<DiskManagerInner>>` and a dedicated `sync_mutex: Arc<Mutex<()>>`.
   - Read operations (`read_data`, `read_at`, `lookup`, `stat`, `list_dir`, `resolve_path`, `flush`, `flush_async`) acquire `.read()` locks.
   - Flush operations (`flush`, `flush_async`) hold `sync_mutex` to serialize physical `msync` calls, but only hold `inner.read()`, allowing all concurrent readers to proceed without latency spikes.
   - Write operations (`create_file`, `mkdir`, `delete_file`, `write_data`) hold `inner.write()` only during Stage 3 commit after Stage 1 read inspection and Stage 2 lock-free CPU processing.
   - **Every metadata mutation must be journaled on a journaled image.** A transaction left pending in the ring becomes stale the moment an unjournaled path touches the same metadata, and recovery will replay the old post-image over the newer change. All journaled paths stage through `AllocSim` so every allocated id is known before the image is touched.
   - Inside `DiskManagerInner`, `inode_cache`, `dir_cache`, and `chunk_read_cache` have their own fine-grained `RwLock`s.
   - **Rule**: Never hold an inner write lock while requesting an outer lock (prevent deadlocks).
   - **Lock poisoning**: use `crate::lock_unpoisoned(&m)` for locks guarding reconstructible in-memory state (caches, registries, serialization-only mutexes). Do **not** return a stateful resource to a shared pool if a panic mid-critical-section could leave it inconsistent: `IpcClient` checks sockets out via `CheckedOutStream` so a panic or I/O error between `write_framed` and `read_framed` closes and discards the desynchronized stream instead of returning it to `idle`.
2. **Zero-Copy Inode & Directory Caching**:
   - `inode_cache`: 32-shard bounded `BoundedInodeCache` (`FxHashMap` + Second-Chance Clock order, capacity 2048). Eviction drops **one** unreferenced entry; it never clears the whole cache.
   - `dir_cache`: 32-shard `ShardedDirCache` (`FxHashMap<u64, DirIndex>` per shard) storing per-directory entry name-to-inode mappings. Negative lookups trigger out-of-lock complete indexing without stalling other directories.
   - **Note**: inodes are persisted through `src::inode_format`, not by casting `#[repr(C)]` bytes. Format v2 writes an explicit deterministic 256-byte little-endian layout with zeroed reserved space.
3. **Master-Proxy Session Architecture**:
   - Always prefer `OifsSession::get_or_open(...)` in multi-threaded code. It canonicalizes paths, coordinates file locking, and shares Master instances within the same process.
4. **Backward Compatibility**:
   - OIFS images created under v1 (single-block directories where `inode.size == 0`) remain transparently readable and upgrade on the fly to multi-block format.
   - **Inode format v1** (packed `bincode` records) images remain readable *and writable*; they keep writing v1 until `oifs migrate` upgrades them, so an image never holds a mix of encodings by accident.
   - `SuperBlock.format_version` selects the inode decoder family. `migration_cursor` makes a partially-applied migration an explicit, resumable state rather than an ambiguous one.

---

## 5. Development & Testing Commands

```bash
# 0. One-click comprehensive pre-release verification (fmt, clippy, build, test,
#    shuttle, the full parallel Kani suite and the LLVM coverage report)
./scripts/release_check.sh
./scripts/release_check.sh --quick   # Fast run (skips Kani, Shuttle & coverage)

# 1. Compile in debug and release modes
cargo build
cargo build --release

# 2. Run all standard unit and integration tests (73 test targets)
cargo test --release

# 2b. Run integration suites under pure pread/pwrite StorageMode::FileIo (no mmap)
OIFS_STORAGE_MODE=pread cargo test --release --test read_only_mode_test --test async_and_ffi_test --test file_io_storage_test --test session_and_cache_test --test journal_test --test integration_test

# 3. Run multi-block directory performance benchmark
cargo test --release --test dir_bench -- --ignored --nocapture

# 3b. Run the metadata WAL benchmarks (journaled vs legacy, durability-mode cost, flush cost)
cargo test --release --test journal_bench -- --ignored --nocapture -- --test-threads=1

# 3c. Run the seekable, cache coherence, and SIMD filter benchmarks
cargo test --release --test cache_coherence_and_simd_filter_test -- --ignored --nocapture --test-threads=1

# 3d. Run the background auto-defragmentation coordinator tests
cargo test --release --test auto_defrag_test

# 3e. Run the IPC framing / transport security tests
cargo test --release --test ipc_frame_test --test ipc_security_test

# 3f. Run Bolero property & fuzz test suite
cargo test --release --test bolero_fuzz_test

# 4. Run Shuttle randomized concurrency permutation tests
cargo test --test shuttle_concurrency_test

# 5. Run Kani formal verification proofs (all 99 proofs)
cargo kani

# 5b. Run the Kani proofs in parallel (one CBMC process per harness).
#     NOTE: -j REQUIRES --output-format=terse, otherwise cargo-kani errors out.
#     Tune -j to available cores *and* RAM: each harness runs its own CBMC process.
cargo kani -j 8 --output-format=terse

# 6. Run a specific Kani proof harness
cargo kani --harness proof_get_block_checked_arithmetic_prevents_wrap_around

# 7. Check formatting and clippy
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

---

## 6. Guidelines for Making Changes

- **Preserve Formal Proofs**: If you alter `BlockPath`, bitmap scanning, directory block headers, or superblock layout, update and run the corresponding Kani proof harnesses (`cargo kani`).
- **Endianness Portability**: All on-disk numbers are strictly Little-Endian (`to_le_bytes()` / `from_le_bytes()`). Never use native-endian binary casts for on-disk data.
- **No Internal Task Codes in Code/Docs**: Keep comments, tests, `CHANGELOG.md`, and `AGENTS.md` free of internal phase/ticket/milestone labels; use clear technical descriptions instead.
- **Verification Rule (`.agent/rules/cargo_test_verification.md`)**:
  - Always verify that the exit code of `cargo test` is 0.
  - Check for `FAILED` or `test result: FAILED` in stdout.
  - If you touch `DiskManager` or `OifsSession` APIs, check `tests/` for any stale signatures and fix them immediately.
