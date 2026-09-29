---
type: architecture
title: OIFS Architecture Overview
description: High-level architectural overview of OIFS (O's Inode File System), detailing its single-file on-disk image model, module layout, multi-target crate compilation, and subsystem interactions across CLI, C FFI, and MCP entry points.
tags: [architecture, overview, modules, crate-type, mcp-feature, subsystems, block-size]
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-23775c3de52f3ab95a13cb8b
    resource: repo://README.md
  - id: openwiki-source-ed8bf05e307c6278442542c2
    resource: repo://src/lib.rs
generated: { by: "pi", at: "2026-09-29T16:14:34.721Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-09-29T18:26:58.313Z
---

## What is OIFS?

<!-- openwiki: broken internal link [src/lib.rs] file "src/lib.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
**OIFS (O's Inode File System)** is a high-performance, single-file containerized Unix-like inode filesystem implemented in Rust ([`src/lib.rs`](src/lib.rs)).

It packages an entire structured filesystem into a standalone `.img` container file, combining traditional filesystem durability with modern data processing and AI capabilities:
- **Single-Image Container Model**: Files, directories, inodes, and bitmaps are stored contiguously in a single host file mapped directly into memory via `memmap2`.
<!-- openwiki: broken internal link [src/lib.rs#L17] file "src/lib.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **Large Dataset Scalability**: Multi-tier block indexing (direct, single indirect, double indirect, and triple indirect) scales individual files up to **513 GB** while retaining 4KB block granularity ([`BLOCK_SIZE = 4096`](src/lib.rs#L17)).
- **Extreme Compression Ratios**: Combines Blosc2-style pre-compression data filters (Delta, ByteShuffle, BitShuffle) with Zstandard to collapse Shannon entropy on numerical and tabular data before compression.
- **At-Rest Authenticated Cryptography**: End-to-end encryption with XChaCha20-Poly1305 AEAD, Argon2id password key derivation, and deterministic SIV filename encryption.
- **Multi-Process Concurrency**: Dynamic Master-Proxy IPC architecture that coordinates concurrent processes sharing a single image over local Unix domain sockets or network TCP.
- **Three Unified Consumer Surfaces**: The engine is exposed via a full-featured CLI binary, a standard C dynamic library (`liboifs.so`), and a Model Context Protocol (MCP) server for AI agents.

## Top-level module layout

<!-- openwiki: broken internal link [src/lib.rs#L1-L11] file "src/lib.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The codebase is organized into 11 specialized modules declared in [`src/lib.rs#L1-L11`](src/lib.rs#L1-L11):

```
                                  ┌───────────────────────────┐
                                  │      Client Surfaces      │
                                  │  CLI  │  C FFI  │ MCP Ser │
                                  └─────┬───────┬───────┬─────┘
                                        │       │       │
                                        ▼       ▼       ▼
┌───────────────────────────┐     ┌───────────────────────────┐
│     Multi-Process IPC     │<───>│      Session Registry     │
│       (src/ipc.rs)        │     │     (src/session.rs)      │
└───────────────────────────┘     └─────────────┬─────────────┘
                                                │
                                                ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                         DiskManager (src/disk.rs)                           │
│              Protected by Arc<RwLock<DiskManagerInner>> / mmap              │
└───────┬──────────────┬──────────────┬──────────────┬──────────────┬─────────┘
        │              │              │              │              │
        ▼              ▼              ▼              ▼              ▼
┌──────────────┐┌──────────────┐┌──────────────┐┌──────────────┐┌─────────────┐
│  Superblock  ││  Allocators  ││ Inode & Dirs ││ Data Filters ││ Encryption  │
│(superblock.rs││ (allocator.rs││ (inode.rs    ││ (filters.rs) ││(encryption. │
│              ││  bitmap.rs)  ││  directory.rs││              ││   rs)       │
└──────────────┘└──────────────┘└──────────────┘└──────────────┘└─────────────┘
```

<!-- openwiki: broken internal link [src/allocator.rs] file "src/allocator.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/allocator.rs] file "src/allocator.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
1. [`allocator`](src/allocator.rs): [`SimpleBlockAllocator`](src/allocator.rs) providing sequential hint-based $O(1)$ block and inode allocation.
<!-- openwiki: broken internal link [src/bitmap.rs] file "src/bitmap.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/bitmap.rs] file "src/bitmap.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/bitmap.rs] file "src/bitmap.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
2. [`bitmap`](src/bitmap.rs): Mutable [`Bitmap`](src/bitmap.rs) and zero-allocation [`BitmapRef`](src/bitmap.rs) with 64-bit hardware `tzcnt` word scanning.
<!-- openwiki: broken internal link [src/disk.rs] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
3. [`disk`](src/disk.rs): Core [`DiskManager`](src/disk.rs) storage coordinator, mmap lifecycle, `RwLock` concurrency, durability flushes, and defragmentation.
<!-- openwiki: broken internal link [src/encryption.rs] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
4. [`encryption`](src/encryption.rs): Authenticated AEAD payload encryption, Argon2id KDF, memory zeroization (`ZeroizeOnDrop`), and deterministic SIV filename privacy.
<!-- openwiki: broken internal link [src/filters.rs] file "src/filters.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
5. [`filters`](src/filters.rs): Pre-compression transformations (Delta, ByteShuffle, BitShuffle, TruncPrecision), Shannon entropy analysis, and C-Blosc2 bindings.
<!-- openwiki: broken internal link [src/inode.rs] file "src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/inode.rs] file "src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/inode.rs#L9-L15] file "src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
6. [`inode`](src/inode.rs): Fixed 256-byte [`Inode`](src/inode.rs) metadata layout, [`FileType`](src/inode.rs#L9-L15), block pointers up to 513GB, and Kani formal proofs.
<!-- openwiki: broken internal link [src/superblock.rs] file "src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/superblock.rs] file "src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
7. [`superblock`](src/superblock.rs): [`SuperBlock`](src/superblock.rs) magic validation (`OIFS`), geometry definitions, and backward compatibility.
<!-- openwiki: broken internal link [src/directory.rs] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/directory.rs] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/directory.rs] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
8. [`directory`](src/directory.rs): Variable-length [`DirectoryEntry`](src/directory.rs), [`DirectoryIterator`](src/directory.rs), and zero-heap-allocation lookup algorithms.
<!-- openwiki: broken internal link [src/ipc.rs] file "src/ipc.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
9. [`ipc`](src/ipc.rs): Master-Proxy IPC protocol, UDS / TCP rendezvous discovery, worker threads, and length-prefixed framing.
<!-- openwiki: broken internal link [src/session.rs] file "src/session.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/session.rs] file "src/session.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
10. [`session`](src/session.rs): Process-wide [`OifsSession`](src/session.rs) registry, reference-counted session reuse, and automatic client-to-master failover.
<!-- openwiki: broken internal link [src/ffi.rs] file "src/ffi.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/ffi.rs] file "src/ffi.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
11. [`ffi`](src/ffi.rs): C-compatible ABI surface, boxed [`OIFSHandle`](src/ffi.rs), callback-based listing, and thread-isolated error reporting.

## Crate configuration and target outputs

<!-- openwiki: broken internal link [Cargo.toml#L10-L11] file "Cargo.toml" does not exist. Fix the href or restore the target, then delete this comment. -->
In [`Cargo.toml#L10-L11`](Cargo.toml#L10-L11), OIFS configures dual compilation targets:

```toml
[lib]
crate-type = ["cdylib", "rlib"]
```

- **`cdylib` (C Dynamic Library)**: Compiles `liboifs.so` (Linux) or `liboifs.dylib` (macOS). Strips Rust-specific metadata and exports only `#[unsafe(no_mangle)] pub extern "C"` symbols, allowing direct linking from C, C++, Python (via `ctypes`/`cffi`), or Go.
- **`rlib` (Rust Library)**: Produces standard Rust library artifacts consumed by the binary targets (`src/bin/oifs.rs`, `src/bin/oifs_mcp.rs`) and integration test suites.

### The `mcp` optional feature flag

<!-- openwiki: broken internal link [Cargo.toml#L35-L55] file "Cargo.toml" does not exist. Fix the href or restore the target, then delete this comment. -->
To maintain lightweight compilation for CLI-only or embedded environments, the Model Context Protocol server dependencies are gated behind an optional feature flag in [`Cargo.toml#L35-L55`](Cargo.toml#L35-L55):

```toml
[features]
default = []
mcp = ["dep:rmcp", "dep:tokio", "dep:anyhow", "dep:schemars"]

[[bin]]
name = "oifs_mcp"
path = "src/bin/oifs_mcp.rs"
required-features = ["mcp"]
```

- Building standard `oifs` CLI avoids compiling Tokio and the async web stack.
- Passing `--features mcp` pulls in `rmcp`, `tokio`, and `schemars` to build the `oifs_mcp` executable.

## Core concepts and storage model

<!-- openwiki: broken internal link [src/lib.rs#L17] file "src/lib.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
1. **4KB Fixed Block Sizing**: The filesystem uses a uniform block size of 4096 bytes ([`BLOCK_SIZE = 4096`](src/lib.rs#L17)). All bitmaps, inodes, directory tables, and indirect pointer tables align to 4KB boundaries, matching CPU virtual memory page sizes for optimal mmap page faults.
2. **On-Disk Image Model**: An OIFS image consists of four contiguous physical regions:
   - Block 0: `SuperBlock` header
   - Block 1: Inode Bitmap
   - Block 2: Data Block Bitmap
   - Blocks $3 \dots K$: Inode Table (packed 256-byte Inode slots)
   - Blocks $K+1 \dots N$: Data Blocks
3. **Three Interaction Surfaces**:
   - **CLI (`src/bin/oifs.rs`)**: Command-line binary providing subcommands (`create`, `put`, `get`, `ls`, `rm`, `mkdir`, `fsck`, `defrag`, `analyze`).
   - **C FFI (`src/ffi.rs`)**: Shared library interface exporting `oifs_open`, `oifs_read_at`, `oifs_write_file`, and handle management.
   - **MCP Server (`src/bin/oifs_mcp.rs`)**: AI agent stdio server offering structured tools (`read_file`, `write_file`, `list_dir`, `append_file`, `status`).

## Subsystem interactions

```
[Write Request: path, offset=0, data]
  │
  ├── 1. Path Resolution: Resolve parent directory and lookup/create Inode in Inode Table.
  ├── 2. Pre-compression Filter: Inode filter flags determine Delta/Shuffle pipeline.
  │      apply_filters_cow() reorganizes byte streams with 0-copy Cow semantics.
  ├── 3. Zstd Compression: Auto-mode evaluates filtered payload size vs compressed size.
  ├── 4. Authenticated Encryption: If enabled, encrypts compressed data with XChaCha20-Poly1305.
  ├── 5. Block Allocation: SimpleBlockAllocator consults Data Bitmap using 64-bit tzcnt scans.
  └── 6. Durability Flush: Writes blocks to mmap and triggers mmap.flush_async().
```

- **Durability**: Sync-on-write semantics guarantee that metadata and data are queued to OS page writeback immediately.
- **Multiprocessing**: The first process opening an image acquires an advisory file lock (`F_SETLK`) and spawns an `IpcServer`; secondary processes detect the lock and become proxies, routing requests over length-prefixed IPC frames.
