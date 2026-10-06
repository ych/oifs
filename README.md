# OIFS (O's Inode File System)

[English](README.md) | [繁體中文](README_zh.md) | [📖 Online Documentation](https://ych.github.io/oifs/)

> 🌐 **Documentation & Architecture Website**: [https://ych.github.io/oifs/](https://ych.github.io/oifs/)  
> Browse interactive architecture diagrams, module call trees, verified design claims, and comprehensive technical specifications.

OIFS is a high-performance, embedded, multi-process inode filesystem engine implemented in Rust. It delivers crash-resilient storage, fine-grained concurrency, military-grade AEAD encryption, scientific data pre-compression filters, pluggable asynchronous I/O engines, a native Model Context Protocol (MCP) server for AI agents, and a stable C/C++ FFI interface.

---

## Capabilities & Architecture

### 1. Storage & Scaling
*   **Unix-like Inode Architecture**: Standard inode hierarchy managing regular files, directories, permissions, and nanosecond timestamps.
*   **Large File Support (up to 513 GB)**: Single, double, and triple indirect block indexing (134,480,394 blocks of 4KB), maintaining 100% backward compatibility with legacy disk formats.
*   **Dynamic Multi-Block Directories**: Seamlessly expands directories across dynamically allocated 4KB extent blocks, scaling to tens of thousands of entries per directory with in-memory accelerated caching.
*   **Online Zero-Downtime Defragmentation 🧹**: `analyze_fragmentation` computes fragmentation ratios; online defragmentation uses a safe 3-step atomic rename sequence with `.old` backup protection, relocating files contiguously while 100% preserving metadata, filter parameters, and encryption.
*   **FSCK Integrity Diagnostics 🛠️**: Full structural consistency scanner detecting orphan inodes, leaked blocks, missing blocks, and cross-linked references with human-readable and structured JSON outputs.

### 2. Transactional Crash Resilience & Durability
*   **Transactional Metadata WAL (Write-Ahead Log)**: Circular ring buffer write-ahead logging for directory mutations and file allocations. Every structural mutation is logged before physical blocks are modified.
*   **Instant Crash Recovery**: Automatic replay upon mount restores uncheckpointed transactions and discards torn transactions without leaking blocks or leaving orphaned inodes.
*   **Configurable Durability Policies**:
    *   `Strict`: Synchronous `msync(MS_SYNC)` per transaction commit for maximum power-loss resilience.
    *   `RangeAsync`: Asynchronous page writeback with automatic 4KB page range-coalescing, reducing system call overhead by up to 99.6%.
    *   `Lazy`: Memory-buffered writeback for maximum in-memory throughput.
*   **Non-Blocking Flush Concurrency**: `flush()` and `flush_async()` serialize physical `msync` calls through a dedicated synchronization mutex while holding a shared read-lock, guaranteeing that background flushes never stall concurrent reader threads.

### 3. High-Concurrency Multi-Threaded Engine
*   **32-Shard Lock-Striped Inode Cache**: Decouples the in-memory bounded inode cache into 32 independent shards, each guarded by its own `RwLock`. Inode IDs are uniformly distributed using 64-bit Fibonacci hashing bijections, eliminating lock contention on cache misses and evictions (**4,000,000+ ops/sec**).
*   **Out-of-Lock Processing Pipeline**: CPU-intensive Delta/Shuffle filtering, Zstandard compression, and XChaCha20 encryption execute completely outside filesystem locks. Reader threads experience **zero starvation** during heavy multi-megabyte writes, maintaining 0 µs p50/p99 read latencies.
*   **Decoupled Directory Mutations**: File and directory creation (`create_file`) and deletion (`delete_file`) perform path resolution, collision checks, and cryptographic filename encryption under shared read-locks or out-of-lock, acquiring exclusive write-locks only for the brief final block commit.
*   **Zero-Allocation Path Splitting**: Zero-heap-allocation path component iteration (`resolve_path_iter`) and $O(1)$ parent splitting (`resolve_parent`), delivering **7.7M+ path lookups/sec**.

### 4. End-to-End Cryptographic Security 🔒
*   **XChaCha20-Poly1305 AEAD**: Authenticated Encryption with Associated Data providing confidentiality and cryptographic tamper-proofing.
*   **Argon2id Key Derivation**: Memory-hard password hashing with random salt stored in the SuperBlock, resistant to GPU/ASIC brute-force attacks.
*   **Synthetic IV (SIV) Filename Encryption**: Deterministic authenticated filename encryption using ChaCha20-Poly1305 + Blake2b-512 PRF with parent inode tweak and Base64URL encoding. Raw disk inspections (`strings`, `hexdump`) reveal zero filenames or directory structures.
*   **Zeroization**: Sensitive cryptographic key material is automatically zeroed on drop using the `zeroize` crate.
*   **CLI Password Masking**: Automatically suppresses echo in terminal prompts to prevent shoulder-surfing.

### 5. Numerical Data Filters & Blosc2 Compression ⚡
*   **Why Pre-compression Filters**: General-purpose compressors (Zstandard, LZ4) rely on sliding-window byte matching (LZ77), which struggles on binary numerical arrays (IEEE 754 floats, timeseries integers, coordinate vectors). Pre-compression filters reorganize bytes to collapse Shannon Entropy, boosting compression ratios from 1.95x up to **390x (99.7% space savings)**.
*   **Supported Filters**: First-order Delta (`wrapping_sub`), Byte Shuffle (AoS to SoA transposition), BitShuffle ($8 \times 8$ bit matrix transposition), and TruncPrecision (mantissa bit truncation).
*   **Composite Filter Pipelines**: Arbitrary stacking and chaining of multiple filters.
*   **Intelligent Filter Recommender**: Automatically computes Shannon entropy across candidate pipelines in parallel to suggest optimal parameters.

### 6. Transparent Multi-Process & Network IPC 🔄
*   **Dynamic Master-Proxy Coordination**: The first process opening an image acquires an OS-level exclusive file lock (`flock`) and becomes the **Master**. Subsequent processes automatically run as **Proxies**, transparently dispatching operations over IPC.
*   **Dual Transport Backends**: Ultra-low-latency local Unix Domain Sockets (UDS) and cross-host Network TCP (`--network`).
*   **Atomic Block-Level Merge Policy**: Disjoint byte ranges in the same 4KB block merge in-place; overlapping ranges adhere to atomic Last-Writer-Wins (POSIX `pwrite` semantics).

### 7. Pluggable Async I/O Engine ⚡
*   Decouples payload block reads into pluggable engines selectable at runtime via API or `OIFS_IO_BACKEND`:
    *   `IoBackend::Mmap`: Direct zero-copy memory mapping.
    *   `IoBackend::Pread`: Positional system calls per coalesced extent run.
    *   `IoBackend::IoUring`: Linux asynchronous submission queue with kernel polling.

### 8. Mathematical Verification & Tooling 🛡️
*   **50+ Kani Formal Proofs**: Mathematically proven with AWS **Kani Rust Verifier (CBMC/CaDiCaL)** across 9 modules, proving integer overflow safety, filter bijectivity, ring buffer wrap invariants, and file overwrite bounds.
*   **Shuttle & TSan Concurrency Verification**: Exhaustive randomized thread-schedule permutation testing via Shuttle and ThreadSanitizer multi-threaded stress tests.
*   **Model Context Protocol (MCP) Server 🤖**: Dedicated `oifs_mcp` binary allowing AI coding agents (Claude Desktop, Cursor, Antigravity) to inspect and manage OIFS filesystems via standard JSON-RPC tools.
*   **Stable C/C++ ABI (FFI) 🔌**: Shared library (`liboifs.so`) with a 3-state version handshake verifying runtime compatibility.

---

## Performance & Concurrency Benchmarks

The following benchmarks were measured on release builds under multi-threaded stress workloads:

| Benchmark Scenario | Workload / Configuration | Result / Throughput | Baseline Comparison |
| :--- | :--- | :--- | :--- |
| **Inode Cache Throughput** | 16 reader threads, 32,000 operations across 3,000 files (continuous misses & evictions) | **3,996,081 ops/sec** (8.05 ms total) | **2.70x faster** (+170% throughput vs. global lock 1.48M ops/sec) |
| **Concurrent Reader Latency Under Heavy Writes** | 8 readers reading 64KB file while 2 writers continuously compress/encrypt multi-MB files | **p50 = 0 µs, p99 = 0 µs** (341,902 reads completed) | **Zero reader starvation** (down from 30~50 ms stalls per write) |
| **Path Resolution** | 10,000 lookups traversing multi-tier directory paths | **7,729,979 lookups/sec** (129.37 ns/op) | **Zero heap allocations** |
| **Large Directory Listing** | Listing directory with 5,000 files | **4,712 listings/sec** (212.2 µs per listing) | **2.34x faster** (+134% throughput via in-memory index) |
| **Multi-Block Durability Sync** | 1 MB sequential write (256 payload blocks) under `RangeAsync` mode | **1,885 writes/sec** (81% of Lazy speed) | **7.03x faster** (+603% throughput, 99.6% syscall reduction) |
| **Numerical Data Compression** | 4-byte structured integer / telemetry dataset | **390.1x compression ratio** (99.7% space savings) | **200x better** than raw Zstd (1.95x) |

---

## Build & Test

```bash
# Build the project (release mode)
cargo build --release

# Run all test suites
cargo test --all-targets

# Run the comprehensive concurrency benchmarks
cargo test --test rwlock_concurrency_test -- --nocapture
```

---

## CLI Usage

The compiled `oifs` binary provides a comprehensive CLI for managing disk images.

### 1. Create Image
Create a standard 10MB image:
```bash
cargo run --bin oifs -- -i disk.img create --size 10
```

Create an encrypted image (prompts for password securely with masked input):
```bash
cargo run --bin oifs -- -i encrypted.img create --size 10 --encrypt
```

### 2. File Import & Export
Import a host file:
```bash
cargo run --bin oifs -- -i disk.img put dataset.bin
```

Extract a file back to the host:
```bash
cargo run --bin oifs -- -i disk.img get dataset.bin extracted.bin
```

### 3. Directories & Listings
```bash
# Create directory
cargo run --bin oifs -- -i disk.img mkdir logs

# Recursive listing
cargo run --bin oifs -- -i disk.img ls -r
```

### 4. Blosc2 Numerical Compression & Recommendation ⚡
Analyze data and obtain automated filter recommendations:
```bash
cargo run --bin oifs -- filter-analyze dataset.bin
```

Example output:
```text
=== OIFS Filter Recommendation Report for "dataset.bin" ===
Original Size:        8192 bytes
Baseline Zstd Size:   4199 bytes (Entropy: 4.024 bits/byte)
--------------------------------------------------------------------------------
Filter Pipeline                     Entropy  Zstd Size      Ratio    Savings
--------------------------------------------------------------------------------
None (Raw Zstd)                       4.024       4199      1.95x      48.7%
Delta (typesize=4, u32/f32)           0.811         21    390.10x      99.7% [*RECOMMENDED*]
BitShuffle (typesize=4, u32/f32)      1.122        147     55.73x      98.2%
Shuffle (typesize=4, u32/f32)         4.024        309     26.51x      96.2%
--------------------------------------------------------------------------------
Recommended Blosc2 Filter(s): ["blosc2::Filter::Delta"]
```

Import with automatic filter selection:
```bash
cargo run --bin oifs -- -i disk.img put dataset.bin --filter auto
```

Or manually specify filters (`delta`, `shuffle`, `bitshuffle`, `both`):
```bash
cargo run --bin oifs -- -i disk.img put dataset.bin --filter delta --typesize 4
```

### 5. Integrity Check (FSCK) 🛠️
```bash
cargo run --bin oifs -- -i disk.img fsck
cargo run --bin oifs -- -i disk.img fsck --json
```

### 6. Online Defragmentation 🧹
```bash
cargo run --bin oifs -- -i disk.img defrag
```

### 7. Multi-Process & Network Cluster Access 🌐
```bash
# Node 1 starts as Master listening on TCP port 8989
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 ls

# Node 2 connects as Proxy, transparently dispatching writes
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 put data.bin
```

### 8. Model Context Protocol (MCP) Server 🤖
Run the native MCP server for AI coding agents (Claude, Cursor, Antigravity):
```bash
cargo run --bin oifs_mcp --features mcp
```

---

## Rust API Example

```rust
use oifs::disk::{CompressionMode, DiskManager};

// Open existing image (size 0 opens without truncation)
let dm = DiskManager::open("disk.img", 0).unwrap();

// Resolve root directory
let root_id = dm.resolve_path(".").unwrap();

// Create a file (returns Inode ID)
let file_id = dm.create_file(root_id, "telemetry.bin").unwrap();

// Write data with offset
let data = b"High-throughput concurrent payload";
dm.write_data(file_id, 0, data, CompressionMode::Auto).unwrap();

// Read data back
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
