# OpenWiki Instructions for OIFS

Follow these guidelines when planning, updating, or writing OpenWiki pages for **OIFS (O's Inode File System)**.

## 1. Request Efficiency & Tool Batching (Critical)

Every LLM turn consumes limited API quota. Minimize low-value exploratory turns:

- **Batch independent tool calls in parallel**: When inspecting multiple files or searching multiple symbols, emit all independent `read_file` / `grep` calls in a **single turn** instead of one tool call per turn.
- **Read large contiguous slices**: When reading a source file, request substantial line ranges (400–800 lines or the whole file if small) rather than tiny incremental peeks.
- **Jump straight to owning modules**: Use the Module Map below instead of exploratory `glob` / directory walks at the start of each page.
- **Avoid low-signal files**: Do not read `tests/`, `Cargo.lock`, `CHANGELOG.md`, `README_zh.md`, `scripts/`, or `docs/openwiki-visualizer/` unless explicitly required. Formal Kani proofs live directly inside `src/*.rs` under `#[cfg(kani)]`.

## 2. Authoritative Module Map

Use this map to go directly to the source of truth for any subsystem:

| Subsystem | Owning Source & Design Files |
| :--- | :--- |
| Public crate API & constants | `src/lib.rs`, `Cargo.toml` |
| Dual storage engine (`Mmap`, `PROT_READ` `MmapReadOnly`, `FileIo` + caches) | `src/storage.rs` |
| SuperBlock, v1/v2/v3 layout geometry, validation | `src/superblock.rs` |
| Inode layout (256B `#[repr(C)]`), `ChunkEntry`, `BlockPath` addressing | `src/inode.rs`, `src/inode_format.rs` |
| Core `DiskManager`, read-only mounts, atomic `rename`, `ReadSession`, `FileReadHandle`, `OifsAsyncTask` (inline fast-path & group-commit), durability modes | `src/disk.rs`, `docs/cpp_and_async_api_design.md` |
| In-memory sharded caches (`ChunkReadCache`, `DecodedBlobCache`, `ShardedDirCache`, `BoundedInodeCache`) | `src/disk/caches.rs` |
| Seekable 64 KiB chunked compression & encryption | `src/disk/chunked.rs` |
| Online maintenance (`migrate`, `verify_integrity`/fsck, `analyze_fragmentation`, `defragment`) | `src/disk/maintenance.rs`, `src/auto_defrag.rs` |
| Cross-image CoW clone & 3-tier transfer / sub-range splice (`FICLONERANGE` -> `copy_file_range` -> raw block copy, `PunchHole` / `CollapseRange`) | `src/disk/transfer.rs`, `docs/cross_image_reflink_design.md` |
| Multi-block directories & SipHash indexing | `src/directory.rs` |
| Block & inode bitmaps and allocator | `src/bitmap.rs`, `src/allocator.rs` |
| Pluggable I/O engine (`Mmap`, `Pread`, `IoUring`) & extent coalescing | `src/io_engine.rs` |
| Optional Metadata WAL (33-block journal, CRC32C, redo recovery, checkpointing) | `src/journal.rs`, `docs/metadata_wal_design.md` |
| Pre-compression SIMD filters (Delta, Shuffle, BitShuffle, TruncPrecision) & Zstd worker/context pool | `src/filters.rs`, `src/zstd_pool.rs` |
| Encryption (Argon2id, BLAKE2b-512 subkeys, XChaCha20-Poly1305 AEAD, 192-bit SIV filenames) | `src/encryption.rs` |
| Pure-Rust cross-platform FUSE ABI v7.28 engine (Linux & macOS macFUSE) | `src/fuse.rs` |
| Pure-Rust local loopback NFSv3 Server (zero-kext macOS/Linux mounting, ONC RPC v2/XDR, MOUNT v3 + NFS v3) | `src/nfs.rs`, `docs/nfs_mount_design.md` |
| Multi-process Master-Proxy sessions & hardened IPC | `src/session.rs`, `src/ipc.rs` |
| C FFI & Modern C++17/C++20 header-only API | `src/ffi.rs`, `include/oifs.h`, `include/oifs.hpp` |
| CLI & MCP server | `src/bin/oifs.rs`, `src/bin/oifs_mcp.rs` |

## 3. Incremental Planning & Page Scope Rules

- **Fast incremental planning (`submit_plan` early)**: During `--update`, focus strictly on the files changed since the last documented commit. Do **not** read or reorganize unmodified existing `openwiki/**/*.md` pages during planning—identify only the affected pages (plus at most 1 new page if a brand-new module like `src/nfs.rs` was added) and call `submit_plan` within 5–10 turns.
- **Stay strictly grounded in current code**: Document symbols, structs, CLI flags, C/C++ APIs, and invariants verified directly in `src/` and `include/`.
- **Include canonical repository links**: When updating `quickstart.md` or `architecture/overview.md`, include links to the official GitHub repository (`https://github.com/ych/oifs`) and online documentation site (`https://ych.github.io/oifs/`).
- **Clean terminology**: Never include internal tracking IDs or ticket codes in documentation.
- **Keep diagrams compact**: Use valid GitHub-flavored Mermaid diagrams where they genuinely clarify data flow or on-disk layout.
