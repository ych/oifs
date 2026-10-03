---
type: architecture
title: OIFS Architecture Overview
description: High-level architectural overview of OIFS (O's Inode File System), detailing its single-file on-disk image model, module layout, multi-target crate compilation, and subsystem interactions across CLI, C FFI, and MCP entry points.
tags: [architecture, overview, modules, crate-type, mcp-feature, subsystems, block-size, io_engine]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-03T11:29:24.571Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-23775c3de52f3ab95a13cb8b
    resource: repo://README.md
  - id: openwiki-source-ed8bf05e307c6278442542c2
    resource: repo://src/lib.rs
generated: { by: "antigravity", at: "2026-10-03T11:29:24.571Z" }
---

# OIFS Architecture Overview

<!-- openwiki: broken internal link [src/lib.rs] file "src/lib.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
**OIFS (O's Inode File System)** is a high-performance, single-file containerized Unix-like inode filesystem implemented in Rust ([`src/lib.rs`](src/lib.rs)).

It packages an entire structured filesystem into a standalone `.img` container file, combining traditional filesystem durability with modern data processing and AI capabilities:
- **Single-Image Container Model**: Files, directories, inodes, and bitmaps are stored contiguously in a single host file mapped directly into memory via `memmap2`.
<!-- openwiki: broken internal link [src/lib.rs#L20] file "src/lib.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **Large Dataset Scalability**: Multi-tier block indexing (direct, single indirect, double indirect, and triple indirect) scales individual files up to **513 GB** while retaining 4KB block granularity ([`BLOCK_SIZE = 4096`](src/lib.rs#L20)).
- **Pluggable Async I/O Engine**: Decouples data block reading from memory-mapped page faults via `Mmap`, `Pread`, and Linux `io_uring` backends (P3.2).
- **Extreme Compression Ratios**: Combines Blosc2-style pre-compression data filters (Delta, ByteShuffle, BitShuffle) with Zstandard to collapse Shannon entropy on numerical and tabular data before compression.
- **At-Rest Authenticated Cryptography**: End-to-end encryption with XChaCha20-Poly1305 AEAD, Argon2id password key derivation, and deterministic SIV filename encryption.
- **Multi-Process Concurrency**: Dynamic Master-Proxy IPC architecture that coordinates concurrent processes sharing a single image over local Unix domain sockets or network TCP.
- **Three Unified Consumer Surfaces**: The engine is exposed via a full-featured CLI binary, a standard C dynamic library (`liboifs.so`), and a Model Context Protocol (MCP) server for AI agents.

## Top-Level Module Layout

<!-- openwiki: broken internal link [src/lib.rs#L1-L12] file "src/lib.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The codebase is organized into 12 specialized modules declared in [`src/lib.rs#L1-L12`](src/lib.rs#L1-L12):

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
└──────────────┘└──────────────┘└──────┬───────┘└──────────────┘└─────────────┘
                                       │
                                       ▼
                        ┌──────────────────────────────┐
                        │      Pluggable I/O Engine    │
                        │      (src/io_engine.rs)      │
                        │ Mmap │ Pread │ Linux io_uring│
                        └──────────────────────────────┘
```

<!-- openwiki: broken internal link [src/allocator.rs] file "src/allocator.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
1. [`allocator`](src/allocator.rs): `SimpleBlockAllocator` providing sequential hint-based $O(1)$ block and inode allocation.
<!-- openwiki: broken internal link [src/bitmap.rs] file "src/bitmap.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
2. [`bitmap`](src/bitmap.rs): Mutable `Bitmap` and zero-allocation `BitmapRef` with 64-bit hardware `tzcnt` word scanning.
<!-- openwiki: broken internal link [src/directory.rs] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
3. [`directory`](src/directory.rs): Variable-length `DirectoryEntry`, `DirectoryIterator`, and memory-indexed directory lookups (`DirIndex`).
<!-- openwiki: broken internal link [src/disk.rs] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
4. [`disk`](src/disk.rs): Core `DiskManager` storage coordinator, mmap lifecycle, `RwLock` concurrency, durability flushes, and defragmentation.
<!-- openwiki: broken internal link [src/encryption.rs] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
5. [`encryption`](src/encryption.rs): Authenticated AEAD payload encryption, Argon2id KDF, memory zeroization (`ZeroizeOnDrop`), and deterministic SIV filename privacy.
<!-- openwiki: broken internal link [src/ffi.rs] file "src/ffi.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
6. [`ffi`](src/ffi.rs): C-compatible ABI surface, boxed `OIFSHandle`, callback-based listing, I/O backend selection, and thread-isolated error reporting.
<!-- openwiki: broken internal link [src/filters.rs] file "src/filters.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
7. [`filters`](src/filters.rs): Pre-compression transformations (Delta, ByteShuffle, BitShuffle, TruncPrecision), Shannon entropy analysis, and pipeline recommendations.
<!-- openwiki: broken internal link [src/inode.rs] file "src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
8. [`inode`](src/inode.rs): Fixed 256-byte `Inode` metadata layout, `FileType`, block pointers up to 513GB, and Kani formal proofs.
<!-- openwiki: broken internal link [src/io_engine.rs] file "src/io_engine.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
9. [`io_engine`](src/io_engine.rs): Pluggable data-block read engine supporting Mmap, Pread, and Linux `io_uring` with ExtentList coalescing and kernel gating (P3.2).
<!-- openwiki: broken internal link [src/ipc.rs] file "src/ipc.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
10. [`ipc`](src/ipc.rs): Master-Proxy IPC protocol, UDS / TCP rendezvous discovery, worker threads, and length-prefixed framing.
<!-- openwiki: broken internal link [src/session.rs] file "src/session.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
11. [`session`](src/session.rs): Process-wide `OifsSession` registry, reference-counted session reuse, and automatic client-to-master failover.
<!-- openwiki: broken internal link [src/superblock.rs] file "src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
12. [`superblock`](src/superblock.rs): `SuperBlock` magic validation (`OIFS`), geometry definitions, and backward compatibility.

## Crate Configuration and Target Outputs

In `Cargo.toml`, OIFS configures dual compilation targets:

```toml
[lib]
crate-type = ["cdylib", "rlib"]
```

- **`cdylib` (C Dynamic Library)**: Compiles `liboifs.so` (Linux) or `liboifs.dylib` (macOS). Strips Rust-specific metadata and exports only `#[unsafe(no_mangle)] pub extern "C"` symbols, allowing direct linking from C, C++, Python, or Go.
- **`rlib` (Rust Library)**: Produces standard Rust library artifacts consumed by the binary targets (`src/bin/oifs.rs`, `src/bin/oifs_mcp.rs`) and integration test suites.

### The `mcp` Optional Feature Flag

To maintain lightweight compilation for CLI-only or embedded environments, the Model Context Protocol server dependencies are gated behind an optional feature flag in `Cargo.toml`:

```toml
[features]
default = []
mcp = ["dep:rmcp", "dep:tokio", "dep:anyhow", "dep:schemars"]
```

- Building standard `oifs` CLI avoids compiling Tokio and the async web stack.
- Passing `--features mcp` pulls in `rmcp`, `tokio`, and `schemars` to build the `oifs_mcp` executable.
