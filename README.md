# OIFS (O's Inode File System)

[English](README.md) | [繁體中文](README_zh.md) | [📖 Online Documentation](https://ych.github.io/oifs/)

> 🌐 **Documentation & Architecture Website**: [https://ych.github.io/oifs/](https://ych.github.io/oifs/)  
> Browse interactive architecture diagrams, module call trees, verified design claims, and comprehensive technical specifications.

OIFS is a high-performance, embedded, single-file multi-process inode filesystem engine written in Rust (Edition 2024). It delivers crash-resilient WAL storage, fine-grained sharded concurrency, hardened AEAD encryption, scientific numerical pre-compression filters, seekable 64 KiB parallel chunked I/O, pluggable asynchronous I/O engines, a native Model Context Protocol (MCP) server for AI agents, and a stable C/C++ FFI interface.

---

## Capabilities & Architecture

### 1. Storage & Scaling
*   **Unix-like Inode Architecture & Format v2/v3**: Deterministic fixed 256-byte little-endian inode records (zeroed reserved space, leak-free) and multi-block v3 bitmaps that scale beyond 128 MiB images, with 100% backward compatibility and in-place `oifs migrate` from legacy v1 images.
*   **Large File Support (up to 513 GB)**: Single, double, and triple indirect block indexing (134,480,394 blocks of 4 KiB) with batch indirect leaf resolution and sparse subtree hole-skipping.
*   **Seekable 64 KiB Parallel Chunked Engine**: Independent 64 KiB compressed/encrypted chunks backed by a zero-dependency persistent scoped worker pool, pooled Zstd `CCtx`/`DCtx` contexts, sequential 4-chunk parallel read-ahead, incremental $O(\Delta \text{chunks})$ truncation with indirect table pruning, and non-journaled in-place chunk block reuse.
*   **Dynamic Multi-Block Directories**: Seamlessly expands directories across dynamically allocated 4 KiB extent blocks with 64-bit SipHash filename indexing and 32-shard in-memory acceleration.
*   **Online & Background Defragmentation 🧹**: Supports both Safe mode (atomic `.old` backup swap) and WAL-atomic In-Place COW defragmentation, plus an opt-in background auto-defragmentation coordinator (`--auto-defrag[=THRESHOLD]`).
*   **FSCK Integrity Diagnostics 🛠️**: Full structural consistency scanner detecting orphan inodes, leaked blocks, missing blocks, and cross-linked references with human-readable and structured JSON outputs.

### 2. Transactional Crash Resilience & Durability
*   **Transactional Metadata WAL (Write-Ahead Log)**: Opt-in CRC32C-checksummed circular ring buffer (`--journal`) using ordered-mode payload flush before metadata commit, idempotent redo replay, torn-write rejection, and automatic mount-time crash recovery.
*   **Configurable Durability Policies**:
    *   `Strict`: Synchronous `msync(MS_SYNC)` with single-syscall WAL header+frame coalescing and JBD2-style deferred in-place metadata checkpointing (`10.9k+ ops/sec`, `2.3x` faster than per-tx metadata sync).
    *   `RangeAsync`: Batched dirty-range coalescing drained asynchronously outside exclusive write locks (`234k+ ops/sec`).
    *   `Lazy`: Shared mmap / OS page-cache writeback for maximum throughput (`626k+ ops/sec`), surviving process crashes and flushing on `flush()` / `Drop`.
*   **Non-Blocking Flush Concurrency**: `flush()` and `flush_async()` serialize physical `msync` calls through a dedicated synchronization mutex while holding only a shared read lock, so flushes never stall concurrent readers.

### 3. High-Concurrency Multi-Threaded Engine
*   **32-Shard Lock-Striped Inode & Directory Caches**: Both `BoundedInodeCache` (Second-Chance Clock eviction) and `ShardedDirCache` (with out-of-lock directory index promotion) are partitioned across 32 independent `RwLock` shards using 64-bit Fibonacci hashing (**8.6M+ directory lookups/sec** across 16 threads, **9.97x** faster under contention).
*   **Out-of-Lock Processing Pipeline**: Delta/Shuffle/BitShuffle filtering, Zstandard compression, and XChaCha20-Poly1305 encryption execute completely outside exclusive filesystem locks, eliminating reader starvation during heavy multi-MB writes.
*   **Zero-Allocation Path Splitting**: Zero-heap-allocation path component iteration (`resolve_path_iter`) and $O(1)$ parent splitting (`resolve_parent`), delivering **11.9M+ path lookups/sec (83.9 ns/op)**.

### 4. End-to-End Cryptographic & Format Security 🔒
*   **OWASP / RFC 9106 Hardened Argon2id KDF**: Memory-hard key derivation (`64 MiB`, `t=3`, `p=4`) with per-image random salt and **BLAKE2b-512 domain-separated subkeys** (`payload_key`, `filename_siv_key`, `filename_cipher_key`, `ipc_auth_key`), automatically zeroized on drop (`zeroize`).
*   **Context-Bound XChaCha20-Poly1305 AEAD**: Binds ciphertexts to `inode_id` (and `chunk_idx` + `flags` for seekable chunks) via Poly1305 AAD to prevent cross-file swaps and chunk reordering. Uses AArch64 NEON / x86_64 AVX2 SIMD for file & chunk payloads and an RFC 8439-identical 1-block scalar fast path for short filenames (`<= 192 B`).
*   **192-Bit SIV Filename Encryption**: Deterministic `XChaCha20-Poly1305` filename encryption (`_e_...`) with 24-byte synthetic nonces and `parent_inode` bound in Poly1305 AAD; raw image inspections (`strings`, `hexdump`) reveal zero filenames or contents.
*   **Defense-in-Depth On-Disk Hardening**:
    *   Rejects unencrypted inodes and plaintext filename injection on encrypted images, and refuses opening unencrypted/downgraded images with a password (`NotEncrypted`).
    *   Enforces `SuperBlock::validate` version allow-lists, data-area block pointer bounds, zeroed block reuse, owner-only `0600` image creation (+ `O_NOFOLLOW` on `create_encrypted`), cycle-safe CLI `ls -r`/`rm -r`, and fallible bounded allocations (`MAX_READ_DATA_BYTES`, `MAX_DECOMPRESSED_STREAM_BYTES`) against forged-size OOM and Zstd decompression bombs.
    *   Hardened IPC transport with challenge-response authentication (passwords never cross the wire), owner-only `0700`/`0600` sockets and rendezvous files, loopback-only default TCP (`--allow-remote-ipsc`), and bounded frame deserialization.

### 5. Numerical Data Filters & Compression ⚡
*   **Pre-Compression Filters**: First-order Delta (with NEON/SSE2 SIMD), Byte Shuffle (AoS $\rightarrow$ SoA), BitShuffle (delta-swap ILP 8×8 transposition), and TruncPrecision, boosting compression on binary numerical telemetry from 1.95x up to **390x (99.7% space savings)**. Optional `blosc2` feature flag supported.
*   **Intelligent Filter Recommender**: Evaluates candidate pipelines by Shannon entropy and Zstd ratio to suggest optimal parameters.

### 6. Transparent Multi-Process & Network IPC 🔄
*   **Dynamic Master-Proxy Coordination**: The first process acquires an exclusive OS lock (`fcntl`) and runs as **Master**; concurrent secondary processes transparently connect as **Proxies** over local Unix Domain Sockets (UDS) or Network TCP (`--network`) with pooled authenticated streams.

### 7. Pluggable Async I/O Engine ⚡
*   Runtime-selectable payload read backend via API or `OIFS_IO_BACKEND`:
    *   `IoBackend::Mmap`: Zero-copy memory mapping with contiguous extent coalescing.
    *   `IoBackend::Pread`: Positional syscalls per coalesced extent run.
    *   `IoBackend::IoUring`: Linux asynchronous submission queue (optional `io-uring` feature).

### 8. Mathematical Verification & Tooling 🛡️
*   **92 Kani Formal Proofs**: Formally verified with AWS **Kani Rust Verifier (CBMC)** covering block pointer resolution, superblock geometry, bitmap scanning, filter bijectivity, CRC32C torn-write detection, WAL ring/idempotency invariants, chunk entry bit-isolation, incremental seekable truncation, AAD domain separation, and IPC frame bounds.
*   **Bolero Fuzzing & Shuttle Concurrency Testing**: Property/fuzz testing via Bolero, randomized schedule permutation testing via Shuttle, and 68 integration test suites.
*   **Model Context Protocol (MCP) Server 🤖**: Dedicated `oifs_mcp` binary (`--features mcp`) exposing OIFS operations to AI coding agents over JSON-RPC.
*   **Stable C/C++ ABI (FFI) 🔌**: Shared library (`liboifs.so` / `liboifs.dylib`) with a 3-state runtime version handshake (`oifs_check_version`, `oifs_loaded_path`).

---

## Performance & Concurrency Benchmarks

Measured on release builds (Apple Silicon `aarch64`, `--release`):

| Benchmark Scenario | Workload / Configuration | Result / Throughput | Highlights / Comparison |
| :--- | :--- | :--- | :--- |
| **Dense Indirect `read_at`** | 1,000 × 1 MB double-indirect reads | **58.2 GB/s** (`16.8 ms`) | **2.63x faster** via batch leaf resolution + extent coalescing |
| **Sparse Subtree Hole-Skip** | 200 × 4 MB unallocated indirect subtrees | **232.5 GB/s** (`3.36 ms`) | **4.55x faster** vs 4 KiB block stepping |
| **Seekable 64K `read_at`** | 16 MB seekable file (`1 MiB` / `4 KiB` seq reads) | **12.6 GB/s** (`1.27 ms`) / **3.32 GB/s** (`4.82 ms`) | **2.70x / 1.47x faster** via persistent worker pool + 4-chunk read-ahead |
| **Large File Journaled Write** | 32 MB initial write / overwrite (WAL enabled) | **9.78 GB/s** / **46.1 GB/s** | Linear scaling across 1 MB–32 MB |
| **Encrypted 64K Appends** | 128 × 64 KiB encrypted appends (8 MB) | **701 MB/s** (`11.41 ms`) | **+21.7%** via AArch64 NEON + zero-regression short-filename scalar path |
| **32-Shard Directory Cache** | 16 threads × 10k ops (97% lookup + 3% build) | **8.60M lookups/sec** (`3.24 ms`) | **9.97x faster** vs single `RwLock` |
| **Path Resolution & `list_dir`** | 200k 3-tier `resolve_path` / 5,000-entry `list_dir` | **11.9M lookups/sec** (`83.9 ns`) / **9,523 listings/sec** (`105 µs`) | Zero-allocation path traversal |
| **Journal Durability Modes** | 500 × 512 B `create` + `write` (WAL enabled) | Lazy **626k/s** \| RangeAsync **234k/s** \| Strict **10.9k/s** | `Strict` **2.31x faster** via JBD2-style deferred metadata checkpointing |
| **In-Place COW Defragment** | 500 fragmented 16 KB files (WAL enabled) | **3.28–3.65 ms** (No-op: `18.9 µs`) | **4.4x faster** via word-level bitmap + WAL op coalescing |
| **Numerical Data Compression** | 4-byte structured telemetry (`Delta` + Zstd) | **390.1x ratio** (99.7% savings) | **200x better** than raw Zstd (`1.95x`) |

---

## Build & Test

```bash
# Build in release mode (lean default dependencies; optional: --features blosc2,mcp,io-uring)
cargo build --release

# Run all 68 unit and integration test suites
cargo test --release

# Run Kani formal verification proofs (all 92 harnesses in parallel)
cargo kani -j 8 --output-format=terse

# Run full pre-release verification script (fmt, clippy, build, test, shuttle, kani, coverage)
./scripts/release_check.sh --quick
```

---

## CLI Usage

The compiled `oifs` binary provides a comprehensive CLI for managing disk images.

### 1. Create Image
```bash
# Create a 10MB image (with optional metadata WAL journaling via --journal)
cargo run --bin oifs -- -i disk.img create --size 10 --journal

# Create an encrypted image (prompts securely with masked input)
cargo run --bin oifs -- -i encrypted.img create --size 10 --encrypt
```

### 2. File Import, Export & Directories
```bash
# Import / export host files
cargo run --bin oifs -- -i disk.img put dataset.bin
cargo run --bin oifs -- -i disk.img get dataset.bin extracted.bin

# Create directories, list recursively, remove recursively
cargo run --bin oifs -- -i disk.img mkdir logs
cargo run --bin oifs -- -i disk.img ls -r
cargo run --bin oifs -- -i disk.img rm -r logs
```

### 3. Numerical Compression & Filter Recommendation ⚡
```bash
# Analyze data and get automated filter recommendations
cargo run --bin oifs -- filter-analyze dataset.bin

# Import with auto-recommended or explicit filters (delta, shuffle, bitshuffle, both)
cargo run --bin oifs -- -i disk.img put dataset.bin --filter auto
cargo run --bin oifs -- -i disk.img put dataset.bin --filter delta --typesize 4
```

### 4. Integrity Check (FSCK), Online Defrag & Format Migration 🛠️
```bash
# Verify filesystem consistency
cargo run --bin oifs -- -i disk.img fsck
cargo run --bin oifs -- -i disk.img fsck --json

# Online defragmentation (or enable background auto-defrag via --auto-defrag=0.3)
cargo run --bin oifs -- -i disk.img defrag

# Upgrade legacy v1 images in place to format v2
cargo run --bin oifs -- -i legacy.img migrate
```

### 5. Multi-Process, Network IPC & MCP Server 🌐🤖
```bash
# Node 1 starts as Master listening on loopback TCP 8989
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 ls

# Node 2 connects as Proxy, transparently dispatching operations
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 put data.bin

# Launch the native MCP server for AI coding agents
cargo run --bin oifs_mcp --features mcp
```

---

## Rust API Example

```rust
use oifs::disk::{CompressionMode, DiskManager};

// Open existing image (size 0 opens without truncation)
let dm = DiskManager::open("disk.img", 0).unwrap();

// Resolve root directory and create a file
let root_id = dm.resolve_path(".").unwrap();
let file_id = dm.create_file(root_id, "telemetry.bin").unwrap();

// Write data (supports Never, Auto, Always, Stream { level }, and Seekable { chunk_size, level })
let data = b"High-throughput concurrent payload";
dm.write_data(file_id, 0, data, CompressionMode::Auto).unwrap();

// Read full file or positional slice
let content = dm.read_data(file_id).unwrap();
assert_eq!(content, data);
```

---

## Documentation Website & Interactive Visualizer 🌐

Explore the complete system specifications, module dependencies, and verified design claims:  
👉 **[https://ych.github.io/oifs/](https://ych.github.io/oifs/)**

---

## License

Copyright (c) 2026 Yu-Chun Huang <ych@ychuang.org>

Licensed under the Apache License, Version 2.0 (the "License"); you may not use this file except in compliance with the License. You may obtain a copy of the License at [http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0).
