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
- **Formal Verification**: 48 Kani proofs verifying filter bijectivity, block addressing, directory lookups, checked arithmetic, and bounds safety.

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
| `src/disk.rs` | Core storage engine: `DiskManager`, `DiskManagerInner`, `DurabilityMode`, caches. |
| `src/directory.rs` | `DirectoryEntry`, multi-block directory iteration, 64-bit SipHash filename hashing. |
| `src/io_engine.rs` | `IoEngine`, `IoBackend` (`Mmap`, `Pread`, `IoUring`), `ExtentList`, extent merging. |
| `src/bitmap.rs` | `BitmapRef`, fast 64-bit chunk `tzcnt` free-bit scanner, set-bit iterator. |
| `src/allocator.rs` | `SimpleBlockAllocator`, sequential block/inode allocation with hint. |
| `src/filters.rs` | Blosc2 filters: Delta, Shuffle, BitShuffle (delta-swap ILP), TruncPrecision. |
| `src/encryption.rs` | Argon2id KDF, XChaCha20-Poly1305 encryption, SIV deterministic filename cipher. |
| `src/session.rs` | `OifsSession`: Process-level session registry (`get_or_open`), Master/Proxy dispatch. |
| `src/ipc.rs` | `IpcServer`, `IpcClient`, length-prefixed binary framing, polling via `libc::poll`. |
| `src/ffi.rs` | C-compatible dynamic library bindings (`include/oifs.h`). |
| `src/bin/oifs.rs` | CLI application entry point (`clap`-based). |
| `src/bin/oifs_mcp.rs` | AI Model Context Protocol (MCP) server for IDE/agent tooling. |
| `tests/` | 38 comprehensive integration, stress, crash-safety, and concurrency tests. |
| `docs/optimization_roadmap.md` | Detailed changelog and benchmarks for P0, P1, P2, and P3 optimizations. |
| `docs/performance_and_verification_research.md` | In-depth audit of performance bottlenecks, safety vulnerabilities, and Kani blueprints. |

---

## 4. Key Architectural Patterns & Invariants

1. **Lock Hierarchy & Concurrency**:
   - `DiskManager` wraps `Arc<RwLock<DiskManagerInner>>`.
   - Read operations (`read_data`, `read_at`, `lookup`, `stat`, `list_dir`, `resolve_path`) acquire `.read()` locks.
   - Write operations (`create_file`, `write_data`, `delete_file`) acquire `.write()` locks.
   - Inside `DiskManagerInner`, `inode_cache` and `dir_cache` have their own fine-grained `RwLock`s.
   - **Rule**: Never hold an inner write lock while requesting an outer lock (prevent deadlocks).
2. **Zero-Copy Inode & Directory Caching**:
   - `inode_cache`: Caches `Inode` values in memory to avoid parsing disk blocks during path traversal.
   - `dir_cache`: `DirIndex` stores per-directory entry name-to-inode mappings. Negative lookups trigger complete indexing.
3. **Master-Proxy Session Architecture**:
   - Always prefer `OifsSession::get_or_open(...)` in multi-threaded code. It canonicalizes paths, coordinates flocking, and shares Master instances within the same process.
4. **Backward Compatibility**:
   - OIFS images created under v1 (single-block directories where `inode.size == 0`) must remain transparently readable and upgrade on the fly to multi-block format.

---

## 5. Development & Testing Commands

```bash
# 0. One-click comprehensive pre-release verification (fmt, clippy, build, test, kani, shuttle)
./scripts/release_check.sh
./scripts/release_check.sh --quick   # Fast run (skips Kani & Shuttle)

# 1. Compile in debug and release modes
cargo build
cargo build --release

# 2. Run all standard unit and integration tests (38 test suites)
cargo test --release

# 3. Run multi-block directory performance benchmark
cargo test --release --test dir_bench -- --ignored --nocapture

# 4. Run Shuttle randomized concurrency permutation tests
cargo test --test shuttle_concurrency_test

# 5. Run Kani formal verification proofs (all 48 proofs)
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
