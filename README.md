# OIFS (O's Inode File System)

[English](README.md) | [繁體中文](README_zh.md)

OIFS is an inode-based file system implemented in Rust. It provides robust file operations, directory management, multi-process thread-safe concurrency, online safe defragmentation, pre-compression data filters, AEAD encryption, Model Context Protocol (MCP) server integration, and a C FFI interface for C/C++ integration.

---

## Features

*   **Inode-based Architecture**: Standard Unix-like inode design managing files, directories, permissions, and timestamps.
*   **Large File Support**: Single indirect and double indirect block indexing, expanding maximum file size up to **1GB** (removing the legacy 48KB direct-block ceiling).
*   **Encryption Support 🔒**:
    *   **XChaCha20-Poly1305 AEAD**: Authenticated Encryption with Associated Data providing confidentiality and cryptographic tamper-proofing.
    *   **Argon2id Key Derivation**: Memory-hard password hashing resistant to GPU/ASIC brute-force attacks.
    *   **Per-File Unique Nonces**: Cryptographically secure 192-bit CSPRNG nonces per file.
    *   **Integrated Compression & Encryption**: Compresses before encrypting to maximize compression ratio while preserving ciphertext entropy.
    *   **CLI Password Masking**: Automatically suppresses echo in terminal prompts to prevent shoulder-surfing.
*   **Integrity & Diagnostics (fsck) 🛠️**:
    *   Full structural integrity scanner detecting Orphan Inodes, Leaked Blocks, Missing Blocks, and Cross-Linked Blocks.
    *   Supports human-readable and structured JSON output formats.
*   **Crash Safety**:
    *   Metadata mutations (`create`, `mkdir`, `delete`) enforce sync-on-write semantics.
    *   Leverages `mmap` kernel flush mechanisms to guarantee durability against sudden application or system crashes.
*   **Concurrency & Multi-Process Session (Master-Proxy IPC) 🔄**:
    *   **Dynamic Master-Proxy Architecture**: The first process opening an image acquires an OS-level exclusive file lock (`flock`) and becomes the **Master**. Subsequent processes automatically run in **Proxy** mode, transparently forwarding all file system requests over IPC.
    *   **Dual-Mode IPC Coordination**: Seamlessly supports ultra-low-latency local Unix Domain Sockets (UDS) and cross-host Network TCP (`--network`).
    *   **Process-Level Session Registry**: Provides `OifsSession::get_or_open` for thread-safe session caching and reference counting, with multi-hop symbolic link (symlink) recursive canonicalization.
    *   **Block-Level Merge Policy**: Disjoint byte ranges in the same 4KB block automatically merge in-place; overlapping ranges adhere to atomic Last-Writer-Wins (POSIX `pwrite` semantics).
*   **Online Safe Defragmentation 🧹**:
    *   **Fragmentation Analysis**: `analyze_fragmentation` computes fragmentation percentage and unallocated gap distributions.
    *   **3-Step Atomic Replacement**: Employs a fail-safe rename sequence with an automated `.old` backup to protect against power outages or crashes during defragmentation.
    *   **100% Metadata Preservation**: Contiguously reallocates blocks while strictly preserving directory structures, Blosc2 filter parameters, compression, and encryption.
*   **Model Context Protocol (MCP) Server 🤖**:
    *   Dedicated `oifs_mcp` server binary allowing AI agents and IDEs (Claude Desktop, Cursor, Antigravity) to manage and inspect OIFS images via standard JSON-RPC tools.
*   **Extreme Zero-Allocation Optimization 🚀**:
    *   **Bitmap Allocation Scanner**: 64-bit word chunking with hardware `tzcnt` instruction, speeding up free-block scans by **11x ~ 13.4x**.
    *   **Zero-Allocation Directory Lookup**: In-place byte-slice parsing of entry headers and names, accelerating lookups by **12x ~ 26.8x** with **0 heap allocations**.
    *   **Step-Skipping Insertion Point Scan**: Fast-forwards directory insertion offsets via `18 + len` byte offsets, speeding up appends by **32.5x**.
    *   **Zero-Copy Read/Write Filter Pipeline**: Utilizes `Cow<'a, [u8]>` to eliminate memory clones on uncompressed/unfiltered paths, speeding up reads/writes by **10x ~ 3,162x**.
*   **Rigorous Concurrency Verification 🧪**:
    *   Integrated **Shuttle** randomized thread-schedule permutation tests to exhaustively explore race conditions and deadlocks.
    *   Integrated **ThreadSanitizer (TSan)** multi-threaded stress tests verifying data integrity under extreme concurrency.
*   **Blosc2 Pre-compression Data Filters & Extreme Compression ⚡**:
    *   **Why Blosc2**: General-purpose compressors (Zstandard, LZ4) rely on sliding-window byte matching (LZ77), which underperforms on numerical streams (Float32/64, Int32/64), time series, and Array of Structures (AoS). Pre-compression filters reorganize bytes and compute differences to collapse Shannon Entropy, boosting compression ratios from 1.95x to **390x** (**99.7% space savings**).
    *   **Supported Filters**: First-order Delta, Byte Shuffle, BitShuffle, TruncPrecision.
    *   **Composite Filter Pipeline**: Arbitrary stacking and chaining of multiple filters.
    *   **Filter Recommendation Tool**: Automatically measures Shannon entropy and evaluates 14 candidate pipelines in parallel to suggest optimal parameters.
    *   **Native C-Blosc2 Integration**: Direct bindings to the compiled C-Blosc2 chunk codec.
*   **Formal Verification Guarantee 🛡️**:
    *   25 mathematical proofs verified with AWS **Kani Rust Verifier (CBMC/CaDiCaL)**, proving filter bijectivity, two's-complement overflow safety, superblock bounds, and collision-free block allocation.
*   **C API (FFI) 🔌**: Comprehensive C shared library (`liboifs.so`) supporting encrypted access, I/O, directory management, `oifs_get_or_open` session reuse, and rich diagnostics.

---

## Build

```bash
# Build the project (release mode)
cargo build --release

# Run the test suite
cargo test
```

---

## CLI Usage

The compiled `oifs` binary provides a comprehensive command-line interface for disk image management.

### 1. Create Image
Create a 10MB file system image:
```bash
cargo run --bin oifs -- -i disk.img create --size 10
```

#### Create Encrypted Image 🔒
Create an encrypted file system (prompts for password securely with masked input):
```bash
cargo run --bin oifs -- -i encrypted.img create --size 10 --encrypt
```

Specify password via command-line argument (not recommended for production):
```bash
cargo run --bin oifs -- -i encrypted.img --password mypassword create --size 10 --encrypt
```

### 2. Import File
Import a local file into the file system image:
```bash
touch hello.txt && echo "Hello World" > hello.txt
cargo run --bin oifs -- -i disk.img put hello.txt
```

Encrypted images will automatically prompt for password when accessed:
```bash
cargo run --bin oifs -- -i encrypted.img put hello.txt
# 🔒 Encrypted filesystem detected. Enter password: 
```

### 3. Make Directory
Create a new directory inside the image:
```bash
cargo run --bin oifs -- -i disk.img mkdir documents
```

### 4. List Files
List files and directories (supports recursive listing `-r`):
```bash
cargo run --bin oifs -- -i disk.img ls -r
```

### 5. Export File
Extract a file from the image back to the host system:
```bash
cargo run --bin oifs -- -i disk.img get hello.txt downloaded.txt
```

### 6. Filesystem Consistency Check (FSCK) 🛠️
Scan and verify file system structural integrity:
```bash
cargo run --bin oifs -- -i disk.img fsck
```

Output in JSON format for automated tooling:
```bash
cargo run --bin oifs -- -i disk.img fsck --json
```

### 7. Blosc2 Filter Recommendation & Numerical Compression ⚡

#### Analyze file and obtain filter recommendations:
```bash
cargo run --bin oifs -- filter-analyze dataset.bin
```

Example Output:
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
Recommendation Rationale: Data displays strong linear/temporal correlation; first-order delta collapses dynamic range, shrinking entropy.
Recommended Blosc2 Filter(s): ["blosc2::Filter::Delta"]
Command to import with recommended filter:
  oifs -i disk.img put "dataset.bin" --filter delta --typesize 4
```

#### Automatically apply the recommended filter on import:
```bash
cargo run --bin oifs -- -i disk.img put dataset.bin --filter auto
```

#### Manually specify filter and element typesize (1, 2, 4, 8 bytes):
```bash
# First-order Delta
cargo run --bin oifs -- -i disk.img put dataset.bin --filter delta --typesize 4

# Byte Shuffle
cargo run --bin oifs -- -i disk.img put dataset.bin --filter shuffle --typesize 4

# BitShuffle
cargo run --bin oifs -- -i disk.img put dataset.bin --filter bitshuffle --typesize 4

# Composite Filter (Delta + ByteShuffle)
cargo run --bin oifs -- -i disk.img put dataset.bin --filter both --typesize 4
```

### 8. Online Defragmentation 🧹
Analyze and eliminate disk fragmentation by relocating blocks contiguously:
```bash
cargo run --bin oifs -- -i disk.img defrag
```
The command outputs before-and-after fragmentation ratios (e.g. `Fragmentation: 45.2% -> 0.0%`) and guarantees atomic cutover with backup protection.

### 9. Multi-Process & Network IPC Mode 🌐
OIFS supports concurrent multi-process access and cross-host networking:
```bash
# First process launches as Master listening on TCP port 8989
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 ls

# Secondary processes connect as Proxies, transparently dispatching operations
cargo run --bin oifs -- -i disk.img --network 127.0.0.1:8989 put data.bin
```

### 10. Model Context Protocol (MCP) Server 🤖
Launch the native MCP server to interface with AI coding agents and IDEs:
```bash
cargo run --bin oifs_mcp --features mcp
```
Provides standard MCP tools: `create_image`, `list_files`, `read_file`, `write_file`, `delete_file`, `fsck`, `defrag`.

### 11. Performance Microbenchmark 📊
Run the dedicated release-mode benchmark to measure speedups across key hot paths:
```bash
cargo test --test perf_comparison --release -- --nocapture
```

---

## Why Blosc2 & Pre-compression Filters?

### 1. The Core Problem
In HPC, machine learning, and scientific computing, datasets predominantly consist of binary numerical arrays (e.g., IEEE 754 floats, time-series integers, spatial coordinates).

Standard dictionary compressors (Zstandard, LZ4) match repeating byte sequences. In numerical data, exponent and mantissa bits interleave, meaning even smooth gradients or close values rarely produce repeating substrings. Raw Zstd typically achieves only a 1.2x ~ 2.0x ratio.

**The Role of Pre-compression Filters**:
> Transform binary layouts losslessly before compression to collapse Shannon Entropy and group repeating bytes, unlocking 10x ~ 300x higher compression ratios.

### 2. Filter Principles
*   **Delta**: Computes difference between adjacent elements: $\Delta[i] = x[i] \mathbin{\text{wrapping\_sub}} x[i-1]$. Continuous sequences collapse to constant streams of `1`s or small integers.
*   **Byte Shuffle**: Transposes Array of Structures (AoS) into Structure of Arrays (SoA), clustering identical high-order bytes together into long zero-byte runs.
*   **BitShuffle**: Performs an $8 \times 8$ bit matrix transposition, highly effective for sparse matrices and boolean masks.
*   **TruncPrecision**: Zeros out lower mantissa noise bits in Float32/Float64 to boost compressibility while maintaining specified precision.

### 3. Composite Filter Pipeline
```rust
use oifs::filters::{FilterPipeline, FilterType};

let pipeline = FilterPipeline::new(4)
    .then(FilterType::TruncPrecision { prec_bits: 14 })
    .then(FilterType::Delta)
    .then(FilterType::ByteShuffle);

let filtered = pipeline.apply(&data);
let restored = pipeline.unapply(&filtered);
```

---

## Rust API Example

```rust
use oifs::disk::DiskManager;

// Open image (size 0 opens existing file)
let dm = DiskManager::open("disk.img", 0).unwrap();

// Resolve root directory
let root_id = dm.resolve_path(".").unwrap();

// Create a file (returns Inode ID)
let file_id = dm.create_file(root_id, "test.txt").unwrap();

// Write data (supports offset)
let data = b"Hello OIFS";
dm.write_data(file_id, 0, data).unwrap();

// Read data back
let content = dm.read_data(file_id).unwrap();
assert_eq!(content, data);
```

---

## Architecture & Layout

*   **SuperBlock**: Stores file system metadata (magic number, total blocks, block size, bitmap locations).
*   **Inode Bitmap & Data Bitmap**: Bitmaps tracking allocation status of inodes and data blocks.
*   **Inode Table**: Array of 256-byte Inode structures (mode, size, timestamps, block pointers).
*   **Data Blocks**: 4KB blocks storing file payloads, directory entries, or indirect pointer tables.
*   **Directory Entry**: 18-byte header (`inode`, `hash`, `name_len`) followed by variable-length name bytes.
*   **Dynamic Master-Proxy IPC**: Coordinates multi-process and multi-node concurrent access.

---

## Concurrency Model & Block Merge Policy ⚖️

When multiple processes or threads concurrently access the same file system image, OIFS guarantees data integrity and POSIX compliance through a two-tiered model:

### 1. Master-Proxy Architecture
* **Exclusive Coordination**: The first process opening the image becomes the **Master**, acquiring the OS-level exclusive file lock (`flock`) and exclusive `mmap` write control.
* **Transparent Proxy**: Subsequent processes operate as **Proxies**, forwarding file system operations over IPC to the Master.
* **Serialization via Mutex**: Inside the Master process, incoming requests acquire an internal `Mutex<DiskManagerInner>`, serializing operations and preventing low-level data races.

### 2. Block-Level Merge Policy
When multiple processes write to the **same file and the same 4KB block**, OIFS applies the following merge policy:

| Conflict Scenario | Merge Policy | Behavior & Resulting State |
| :--- | :--- | :--- |
| **Same Block, Disjoint Offsets** | **In-place Byte Merging** | For example, Process A writes `0..100` and Process B writes `200..300`. Each writes only to its designated offset range in the 4KB block slice. Untouched bytes remain intact, and **both writes coexist and merge seamlessly**. |
| **Same Block, Overlapping Offsets** | **Last-Writer-Wins (Atomic)** | Overlapping byte ranges are overwritten by whichever write acquires the Master Mutex later. The mutex ensures atomicity, **guaranteeing no torn writes**. Conforms to standard POSIX `pwrite()` semantics. |
| **Same Block, Compressed File** | **Zstd Multi-Frame Append / Read-Modify-Recompress Fallback** | For sequential EOF appends (`file_offset == size`), OIFS natively writes independent Zstd frames via Multi-Frame concatenation without decompressing previous blocks. For middle-offset random writes or encrypted files, OIFS transparently performs Read-Modify-Recompress to ensure stream consistency. |

---

## Testing Suite

OIFS is backed by over 50 automated tests and formal verification harnesses:

*   **Unit Tests**: Core module functionality (Superblock, Inode, Directory, Allocator).
*   **Integration Tests**: End-to-end file system operations and persistence across re-openings.
*   **Large File Tests**: Validates boundary limits across single and double indirect blocks (up to 1GB).
*   **FSCK Extended Tests**: Verifies detection of orphan inodes, leaked blocks, missing blocks, and cross-linked references.
*   **Online Defrag Tests**: Verifies fragmentation analysis, 3-step atomic rename, and metadata preservation.
*   **Shuttle Concurrency Tests**: Uses **Shuttle** randomized schedule permutation testing to exhaustively explore race conditions and deadlock freedom.
*   **ThreadSanitizer (TSan) Stress Tests**: Validates high-concurrency multi-threaded read/write integrity.
*   **Session IPC & Edge Cases**: Tests dynamic master-proxy promotion, stale socket recovery, zero-byte files, and high-concurrency bursts.
*   **Network Multi-Node Sync Tests**: Verifies multi-node TCP concurrent slice writes and synchronization on a single shared file.
*   **MCP Server Tests**: Validates JSON-RPC tool invocations adhering to the Model Context Protocol.
*   **Performance Microbenchmark**: Empirically proves zero-allocation and algorithmic speedup ratios.
*   **Kani Formal Proofs**: 25 formal proofs verifying arithmetic overflow safety, bijectivity, and allocation correctness.

Run all standard tests:
```bash
cargo test
```

Run performance comparison benchmark:
```bash
cargo test --test perf_comparison --release -- --nocapture
```

---

## Encryption Security

### Cryptographic Primitives
*   **AEAD Cipher**: XChaCha20-Poly1305 (256-bit key, 192-bit CSPRNG nonce per file).
*   **Key Derivation**: Argon2id with 128-bit random salt stored in the SuperBlock.

### Security Notes
1. **Password Recovery**: Passwords are never stored on disk. **Lost passwords result in permanent data loss**.
2. **Directory Metadata Scope**: `--encrypt` encrypts file contents. Directory entries (filenames, sizes, timestamps) remain in plaintext in directory blocks. Avoid identifiable file names if threat models require metadata privacy.
3. **Memory Zeroing**: Key material is cleared on drop using the `zeroize` crate.
4. **Ordering**: Data is compressed before encryption, maintaining high compression efficiency without degrading ciphertext entropy.
