---
type: architecture
title: DiskManager and Persistence Model
description: How OIFS encapsulates memory-mapped I/O, RwLock concurrency, pluggable async I/O engines (Mmap, Pread, io_uring), configurable durability policies, zero-copy reads, and the integrated write pipeline inside DiskManager.
tags: [disk-manager, mmap, persistence, durability, io_engine, io_uring, rwlock, read-at, pipeline, path-resolution]
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
generated: { by: "antigravity", at: "2026-10-03T11:29:24.571Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-04T06:56:29.774Z
---

# DiskManager and Persistence Model

<!-- openwiki: broken internal link [src/disk.rs#L324-L327] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`DiskManager`](src/disk.rs#L324-L327) is the central coordinator of the OIFS storage engine. It provides the single authoritative interface between raw on-disk bytes and high-level filesystem operations (file and directory creation, reads, writes, deletion, path resolution, defragmentation, and integrity checks).

Internally, state is maintained by `DiskManagerInner` (`src/disk.rs#L191-L213`), wrapped in an `Arc<RwLock<DiskManagerInner>>` (`src/disk.rs#L324-L327`):
- `file`: The underlying host image file handle with an exclusive POSIX advisory lock (`F_SETLK`).
- `mmap`: The mutable memory-mapped view (`MmapMut`) spanning the entire filesystem image.
- `superblock`: In-memory cached copy of the deserialized `SuperBlock` (`src/superblock.rs`).
- `encryption_key`: Derived cryptographic key (`Option<EncryptionKey>`) used for authenticated AEAD encryption.
- `free_block_hint` and `free_inode_hint`: Allocation watermarks that convert sequential bitmap scans into amortized $O(1)$ operations.
- `inode_cache`: In-memory zero-copy cache (`RwLock<HashMap<u64, Inode>>`) avoiding redundant bincode deserialization (P2.2).
- `dir_cache`: In-memory directory lookup indices (`RwLock<HashMap<u64, DirIndex>>`) providing $O(1)$ path queries (P3.1).
- `durability_mode`: Atomic durability policy governing `msync` flush behavior on writes (P3.3).
- `io_engine`: Pluggable data-block read engine supporting Mmap, Pread, and Linux `io_uring` backends (P3.2).

## Lifecycle, Image Locking, and Initialization

`DiskManager` instances are constructed via `DiskManager::open` (`src/disk.rs#L331-L333`), `open_with_password` (`src/disk.rs#L336-L342`), or `create_encrypted` (`src/disk.rs#L345-L351`), all delegating to the unified `init_or_open` routine (`src/disk.rs#L353-L471`):

1. **File Opening and Locking**: Opens the image with read/write permissions. Immediately attempts an exclusive whole-file advisory lock using `libc::F_SETLK` (`src/disk.rs#L369-L378`). This prevents independent host processes from concurrently accessing the same raw image without going through the Master-Proxy IPC protocol.
2. **Sizing and Memory Mapping**: New images are sized to `total_size` via `file.set_len(total_size)`. The full image is mapped into process address space using `MmapOptions::new().map_mut(&file)` (`src/disk.rs#L384`).
3. **Superblock Verification**:
   - For new files: Constructs a fresh `SuperBlock`, configures encryption salt/flags if requested, writes the serialized block to offset 0, and initializes root directory inode 0 (`src/disk.rs#L388-L403`, `L439-L468`).
   - For existing files: Reads and deserializes `SuperBlock` from block 0. Verifies magic bytes (`OIFS`) against `SuperBlock::MAGIC` (`src/disk.rs#L404-L411`).
4. **Key Derivation**: If `superblock.encrypted` is true, derives the 256-bit AEAD key using Argon2id with `superblock.encryption_salt` (`src/disk.rs#L413-L417`).

## Durability Policies and Flush Behavior (P3.3)

OIFS provides configurable durability policies through the `DurabilityMode` enum (`src/disk.rs#L118-L141`), balancing machine power-loss resilience against bulk write throughput:

- **`DurabilityMode::Lazy` (Default, value `0`)**:
  Mutations update the shared mmap and OS page cache without issuing per-mutation `msync` syscalls. Changes are immediately visible to all processes and survive application crashes. Persistence across machine power loss is ensured via explicit `flush()`, upon `Drop`, or by periodic OS kernel writeback. Delivers up to ~45x–51x faster write throughput in bulk operations.
- **`DurabilityMode::RangeAsync` (Value `1`)**:
  Asynchronously flushes only the modified byte ranges via `msync(MS_ASYNC)` on each mutation (`sync_mutation_ranges`, `src/disk.rs#L241-L272`), scheduling dirty pages for early writeback without scanning the entire virtual memory address space.
- **`DurabilityMode::Strict` (Value `2`)**:
  Synchronously flushes modified byte ranges via `msync(MS_SYNC)` on every mutation. Guarantees physical media persistence before mutating functions return.
- **`DurabilityMode::LegacyWholeMmapAsync` (Value `3`)**:
  Asynchronously flushes the entire virtual memory map after every mutation (`mmap.flush_async()`).

Mutating operations (`create_entry_internal`, `write_data_with_filters`, `delete_file_internal`) compute modified ranges and invoke `sync_mutation_ranges` (`src/disk.rs#L241-L272`). The `Drop` implementation for `DiskManagerInner` (`src/disk.rs#L308-L313`) executes `self.mmap.flush()`, while explicit synchronous flushing is exposed via `DiskManager::flush` (`src/disk.rs#L2090-L2093`).

## Concurrency Model: Reader-Writer Lock

`DiskManager` wraps its inner state in `Arc<RwLock<DiskManagerInner>>` (`src/disk.rs#L324-L327`). This enables high-concurrency workloads:
- **Parallel Read Path**: All read-only methods (`read_data`, `read_at`, `read_at_batch`, `lookup`, `list_dir`, `resolve_path`, `analyze_fragmentation`, and `verify_integrity`) acquire shared `.read()` locks. Arbitrary numbers of threads can read files and traverse directories concurrently without contention.
- **Exclusive Write Path**: Mutating methods (`write_data_with_filters`, `create_file`, `create_directory`, `delete_file`, and `flush`) acquire exclusive `.write()` locks, ensuring ACID isolation for block allocation, inode updates, and directory modifications.

## Pluggable Payload Read Engines and Vector Reads (P3.2)

In P3.2, `DiskManager` decouples file payload block reads from the memory map via `src/io_engine.rs`:
- **Backend Selection**: `dm.set_io_backend(backend)` (`src/disk.rs#L538-L543`) allows applications to switch between `IoBackend::Mmap` (default), `IoBackend::Pread`, and `IoBackend::IoUring`.
- **Positional Reads (`read_at`)**: `DiskManager::read_at` (`src/disk.rs#L1329-L1352`) directly copies bytes from mmap blocks without heap allocation under `IoBackend::Mmap` for raw files. Under `Pread` and `IoUring`, it coalesces contiguous blocks via `ExtentList` and executes one syscall or one ring submission.
- **Batched Concurrent Reads (`read_at_batch`)**: `DiskManager::read_at_batch` (`src/disk.rs#L1365-L1411`) processes multiple `ReadRequest` entries under a single read-lock acquisition, submitting all extents simultaneously to the underlying `IoEngine` to maximize storage queue depth.

## The Read Pipeline and Decryption/Decompression

Complete file reads via `read_data_internal` (`src/disk.rs#L1229-L1303`) execute in two phases:
1. **Physical Block Assembly**: `walk_payload_blocks` (`src/disk.rs#L1070-L1226`) traverses direct (0..10), single indirect (10..522), double indirect (522..262666), and triple indirect tiers, skipping sparse zero sub-trees lazily. Payload blocks are fetched via `memcpy` or `io_engine.read`.
2. **Reverse Pipeline Execution** (`src/disk.rs#L1268-L1303`):
   - **Decryption**: If `inode.encrypted`, decrypts with XChaCha20-Poly1305 using `guard.encryption_key` and `inode.encryption_nonce`.
   - **Decompression**: If `inode.compressed_size > 0`, decompresses with `zstd::stream::decode_all` (supporting multi-frame streams).
   - **Filter Unapplication**: If pre-compression filters are active, reverses transformations via `unapply_filters`.

## The Write Pipeline (`write_data_with_filters`)

File mutation via `write_data_with_filters` (`src/disk.rs#L1645-L1814`) branches across three paths:
1. **Offset 0 Write**: Calls `write_data_from_start_internal` (`src/disk.rs#L1529-L1625`), applying pre-compression filters, optional Zstd compression, and AEAD encryption before block allocation.
2. **Compressed Stream Appends & Edits**:
   - **Fast Path (Zstd Multi-Frame Append)**: When appending strictly at EOF to an unencrypted file with no active filters, compresses the new chunk as an independent Zstd frame and writes it directly at `inode.compressed_size`.
   - **Fallback (Read-Modify-Recompress)**: When performing random writes, modifying encrypted files, or altering filtered files, decompresses existing content, splices changes, frees previous blocks, and re-compresses from offset 0.
3. **Raw Uncompressed Writes**: Calls `write_buffer_at_offset` (`src/disk.rs#L1479-L1527`) to directly write or allocate sequential blocks.
