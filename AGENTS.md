<!-- OPENWIKI:START -->

## OpenWiki

This repository has a generated `openwiki/` evidence index. It is optional just-in-time context, not required startup reading.

- Do not enumerate, preload, or search wikis at task start. Use retrieval when the user asks for it, when unfamiliar architecture or dependency behavior materially affects the task, or when source inspection leaves an important uncertainty. Stop once the question is grounded.
- When those conditions apply and OpenWiki retrieval tools are available, use `openwiki_search` for just-in-time context and `openwiki_read` for the relevant complete sections. If search returns `workspace_required`, ask which listed workspace to use and retry with its ID.
- Use `openwiki_list_workspaces` or `openwiki_list_wikis` when workspace membership itself needs to be discovered.
- If the retrieval tools are unavailable, read `openwiki/quickstart.md` and follow its links to the relevant pages.
- Treat source code and tests as authoritative. A brief's unknowns and review items are verification gaps, not automatic requirements.
- Prefer the narrowest quiet validation that proves the changed behavior. Preserve complete failure output.

The scheduled OpenWiki GitHub Actions workflow refreshes the repository wiki. Do not hand-edit generated OpenWiki pages unless explicitly asked; prefer updating source code/docs and letting OpenWiki regenerate.

<!-- OPENWIKI:END -->

# OIFS (O's Inode File System) — Agent Quickstart & Architecture Guide

Welcome, Agent! This guide provides the architectural mental model, codebase map, core invariants, and workflows needed to work productively on OIFS without regressions.

---

## 1. What is OIFS?

**OIFS** is a high-performance, single-file containerized Unix-like inode filesystem written in Rust (Edition 2024 / Rust 1.85+). It packs an entire filesystem—inodes, multi-block directories, raw/compressed data blocks, encryption, and pre-compression filter pipelines—into a single `.img` container file backed by memory mapping (`memmap2::MmapMut`).

### Key Highlights
- **Multi-Process Concurrency**: Master-Proxy architecture using OS-level file locking (`flock`). The first process is Master; concurrent secondary processes connect as Proxies over Unix Domain Sockets or Network TCP.
- **Pluggable I/O Engine (P3.2)**: `Mmap` (zero-copy direct slice), `Pread` (contiguous extent batching), and `IoUring` (Linux async submission rings with thread-safe pooling).
- **Configurable Durability (P3.3)**: `Lazy` (fastest, OS writeback / process crash safe), `RangeAsync` (msync modified byte ranges), `Strict` (msync synchronous), and `LegacyWholeMmapAsync`.
- **Pre-compression Filters**: Integrated Blosc2 filters (Delta, Byte Shuffle, BitShuffle with Hacker's Delight 8x8 transposition, TruncPrecision) followed by Zstandard.
- **Encryption**: XChaCha20-Poly1305 with Argon2id KDF and parent-inode tweaked deterministic SIV filename encryption (`_e_...`).
- **FFI Dynamic Library Handshake**: 3-state runtime version & path verification (`oifs_check_version`, `oifs_loaded_path`): 0=OK, 1=WARN (newer), -1=ERR (older).
- **Formal Verification**: 59 Kani proofs verifying filter bijectivity, block addressing, directory lookups, checked arithmetic, CRC32C torn-write detection, journal ring/checkpoint invariants, version policy soundness, and bounds safety.
- **On-Disk Format v2**: Fixed 256-byte inode records with explicit little-endian fields, a 1-byte file-type tag, and zeroed reserved space. Deterministic, reproducible, leak-free. Legacy v1 images stay fully readable and writable; `oifs migrate` upgrades them in place.
- **Metadata WAL (Journaling, opt-in)**: `--journal` reserves 33 blocks (1 header + 128 KB ring) and records `create_file`/`mkdir`/`delete_file`/`write_data` as CRC32C-checksummed transactions. WAL-first, idempotent redo, torn-write rejection, and checkpointing. Payload data is *not* journaled — it is flushed before the metadata commit (ordered mode), so WAL traffic stays proportional to metadata, not file size. Recovery runs automatically at mount, so a power loss no longer requires a full-image `fsck`.
- **Durability governs the journal too**: only `DurabilityMode::Strict` pays the per-transaction `msync` barriers (WAL + payload); the process-crash-safe modes rely on the page cache and sync at `flush()`/`Drop`, with the WAL always written before the image.
- **Known perf issue**: `DurabilityMode::RangeAsync` issues one `msync(MS_ASYNC)` per mutated range with no coalescing, leaving it ~6-8% as fast as `Lazy`. Root cause, measurements, and candidate fixes are recorded in `docs/metadata_wal_design.md` §9.

---

## 2. On-Disk Layout (4096-byte Blocks)

```text
+-------------------+--------------------+-------------------+---------------------+-------------------------+
| Block 0           | Block 1            | Block 2           | Blocks 3 .. N       | Blocks N+1 .. End       |
| SuperBlock (4KB)  | Inode Bitmap (4KB) | Data Bitmap (4KB) | Inode Table (4KB*K) | Data Blocks (4KB each)  |
+-------------------+--------------------+-------------------+---------------------+-------------------------+
```

1. **Block 0 - SuperBlock**: Magic (`0x4F494653`), block counts, bitmap offsets, root inode (0), and encryption salt.
2. **Block 1 - Inode Bitmap**: 1 block tracks up to 32,768 inodes. Bit=1 means allocated.
3. **Block 2 - Data Bitmap**: 1 block tracks up to 32,768 data blocks.
4. **Block 3+ - Inode Table**: Contiguous table of 256-byte `Inode` structures (16 inodes per 4KB block).
5. **Remaining Blocks - Data Blocks**: Stores directory entry lists, file payloads, and indirect pointer tables.

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
| `src/lib.rs` | Public re-exports, module tree, and `BLOCK_SIZE = 4096`. |
| `src/superblock.rs` | `SuperBlock` definition, layout geometry calculation, Kani proofs. |
| `src/inode.rs` | `Inode` (256B `#[repr(C)]`), `FileType`, `BlockPath` tier resolver. |
| `src/inode_format.rs` | On-disk inode encoding: fixed 256B v2 record, legacy `bincode` v1 decoder, version dispatch. |
| `src/disk.rs` | Core storage engine: `DiskManager`, `DiskManagerInner`, `DurabilityMode`, caches, bounded inode cache, journaled write paths. |
| `src/directory.rs` | `DirectoryEntry`, multi-block directory iteration, 64-bit SipHash filename hashing. |
| `src/io_engine.rs` | `IoEngine`, `IoBackend` (`Mmap`, `Pread`, `IoUring`), `ExtentList`, extent merging. |
| `src/journal.rs` | Optional Metadata WAL: CRC32C, transaction frames, `MetadataOp`, `JournalRing`, idempotent redo. |
| `src/bitmap.rs` | `BitmapRef`, fast 64-bit chunk `tzcnt` free-bit scanner, set-bit iterator. |
| `src/allocator.rs` | `SimpleBlockAllocator`, sequential block/inode allocation with hint. |
| `src/filters.rs` | Blosc2 filters: Delta, Shuffle, BitShuffle (delta-swap ILP), TruncPrecision. |
| `src/encryption.rs` | Argon2id KDF, XChaCha20-Poly1305 encryption, SIV deterministic filename cipher. |
| `src/session.rs` | `OifsSession`: Process-level session registry (`get_or_open`), Master/Proxy dispatch. |
| `src/ipc.rs` | `IpcServer`, `IpcClient`, length-prefixed binary framing, polling via `libc::poll`. |
| `src/ffi.rs` | C-compatible dynamic library bindings (`include/oifs.h`). |
| `src/bin/oifs.rs` | CLI application entry point (`clap`-based). |
| `src/bin/oifs_mcp.rs` | AI Model Context Protocol (MCP) server for IDE/agent tooling. |
| `tests/` | 46 integration, stress, crash-safety, concurrency, journal, and CLI test suites. |
| `docs/optimization_roadmap.md` | Detailed changelog and benchmarks for P0, P1, P2, and P3 optimizations. |
| `docs/metadata_wal_design.md` | Metadata WAL specification and implementation status (M1–M5 complete). |
| `docs/performance_and_verification_research.md` | In-depth audit of performance bottlenecks, safety vulnerabilities, and Kani blueprints. |

---

## 4. Key Architectural Patterns & Invariants

1. **Lock Hierarchy & Concurrency**:
   - `DiskManager` wraps `Arc<RwLock<DiskManagerInner>>` and a dedicated `sync_mutex: Arc<Mutex<()>>`.
   - Read operations (`read_data`, `read_at`, `lookup`, `stat`, `list_dir`, `resolve_path`, `flush`, `flush_async`) acquire `.read()` locks.
   - Flush operations (`flush`, `flush_async`) hold `sync_mutex` to serialize physical `msync` calls, but only hold `inner.read()`, allowing all concurrent readers to proceed without latency spikes.
   - Write operations (`create_file`, `mkdir`, `delete_file`, `write_data`) hold `inner.write()`.
   - **Every metadata mutation must be journaled on a journaled image.** A transaction left pending in the ring becomes stale the moment an unjournaled path touches the same metadata, and recovery will replay the old post-image over the newer change. All four paths stage through `AllocSim` (run the real allocator against copied bitmaps) so every allocated id is known before the image is touched.
   - Inside `DiskManagerInner`, `inode_cache` and `dir_cache` have their own fine-grained `RwLock`s.
   - **Rule**: Never hold an inner write lock while requesting an outer lock (prevent deadlocks).
2. **Zero-Copy Inode & Directory Caching**:
   - `inode_cache`: Bounded `BoundedInodeCache` (`FxHashMap` + FIFO order, capacity 2048). Eviction drops **one** oldest entry; it never clears the whole cache, which would cause a cache stampede.
   - `dir_cache`: `DirIndex` stores per-directory entry name-to-inode mappings. Negative lookups trigger complete indexing.
   - **Note**: inodes are persisted through `src::inode_format`, not by casting `#[repr(C)]` bytes. `bincode`'s 169-byte packed output does **not** match the 176-byte in-memory layout (which contains uninitialized padding), so raw casting would misparse every existing image and leak stack memory to disk. Format v2 fixes this with an explicit layout.
3. **Master-Proxy Session Architecture**:
   - Always prefer `OifsSession::get_or_open(...)` in multi-threaded code. It canonicalizes paths, coordinates flocking, and shares Master instances within the same process.
4. **Backward Compatibility**:
   - OIFS images created under v1 (single-block directories where `inode.size == 0`) must remain transparently readable and upgrade on the fly to multi-block format.
   - **Inode format v1** (packed `bincode` records) images remain readable *and writable*; they keep writing v1 until `oifs migrate` upgrades them, so an image never holds a mix of encodings by accident.
   - `SuperBlock.format_version` selects the inode decoder family. `migration_cursor` makes a partially-applied migration an explicit, resumable state rather than an ambiguous one.

---

## 5. Development & Testing Commands

```bash
# 0. One-click comprehensive pre-release verification (fmt, clippy, build, test, kani, shuttle)
./scripts/release_check.sh
./scripts/release_check.sh --quick   # Fast run (skips Kani & Shuttle)

# 1. Compile in debug and release modes
cargo build
cargo build --release

# 2. Run all standard unit and integration tests (46 test suites)
cargo test --release

# 3. Run multi-block directory performance benchmark
cargo test --release --test dir_bench -- --ignored --nocapture

# 3b. Run the metadata WAL benchmarks (journaled vs legacy, durability-mode cost, flush cost)
cargo test --release --test journal_bench -- --ignored --nocapture -- --test-threads=1

# 4. Run Shuttle randomized concurrency permutation tests
cargo test --test shuttle_concurrency_test

# 5. Run Kani formal verification proofs (all 59 proofs)
cargo kani

# 6. Run a specific Kani proof harness
cargo kani --harness proof_get_block_checked_arithmetic_prevents_wrap_around

# 7. Check formatting and clippy
cargo fmt --check
cargo clippy --lib --bins -- -D warnings
```

---

## 6. Guidelines for Making Changes

- **Preserve Formal Proofs**: If you alter `BlockPath`, bitmap scanning, directory block headers, or superblock layout, update and run the corresponding Kani proof harnesses (`cargo kani`).
- **Endianness Portability**: All on-disk numbers are strictly Little-Endian (`to_le_bytes()` / `from_le_bytes()`). Never use native-endian binary casts for on-disk data.
- **Verification Rule (`.agent/rules/cargo_test_verification.md`)**:
  - Always verify that the exit code of `cargo test` is 0.
  - Check for `FAILED` or `test result: FAILED` in stdout.
  - If you touch `DiskManager` or `OifsSession` APIs, check `tests/` for any stale signatures and fix them immediately.
