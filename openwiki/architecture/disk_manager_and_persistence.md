---
type: architecture
title: DiskManager and Persistence Model
description: How OIFS encapsulates memory-mapped I/O, RwLock concurrency, pluggable async I/O engines (Mmap, Pread, io_uring), configurable durability policies, zero-copy reads, and the integrated write pipeline inside DiskManager.
tags: [disk-manager, mmap, persistence, durability, io_engine, io_uring, rwlock, read-at, pipeline, path-resolution]
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
verified:
  - by: openwiki/0.6.1
    at: 2026-10-07T12:20:29.772Z
generated: { by: "openwiki/0.6.1", at: "2026-10-07T12:20:29.772Z" }
---

# DiskManager and Persistence Model

[`DiskManager`](../../src/disk.rs#L324-L327) is the central coordinator of the OIFS storage engine. It provides the single authoritative interface between raw on-disk bytes and high-level filesystem operations (file and directory creation, reads, writes, deletion, path resolution, defragmentation, and integrity checks).

Internally, state is maintained by `DiskManagerInner` (`src/disk.rs#L191-L213`), wrapped in an `Arc<RwLock<DiskManagerInner>>` (`src/disk.rs#L324-L327`):
- `file`: The underlying host image file handle with an exclusive POSIX advisory lock (`F_SETLK`).
- `mmap`: The mutable memory-mapped view (`MmapMut`) spanning the entire filesystem image.
- `superblock`: In-memory cached copy of the deserialized `SuperBlock` (`src/superblock.rs`).
- `encryption_key`: Derived cryptographic key (`Option<EncryptionKey>`) used for authenticated AEAD encryption.
- `free_block_hint` and `free_inode_hint`: Allocation watermarks that convert sequential bitmap scans into amortized $O(1)$ operations.
- `inode_cache`: In-memory zero-copy cache (`RwLock<BoundedInodeCache>`) avoiding redundant bincode deserialization (P2.2).
- `dir_cache`: In-memory directory lookup indices (`RwLock<HashMap<u64, DirIndex>>`) providing $O(1)$ path queries (P3.1).
- `durability_mode`: Atomic durability policy governing `msync` flush behavior on writes (P3.3).
- `io_engine`: Pluggable data-block read engine supporting Mmap, Pread, and Linux `io_uring` backends (P3.2).

## Lifecycle, Image Locking, and Initialization

`DiskManager` instances are constructed via `DiskManager::open` (`src/disk.rs#L331-L333`), `open_with_password` (`src/disk.rs#L336-L342`), or `create_encrypted` (`src/disk.rs#L345-L351`), all delegating to the unified `init_or_open` routine (`src/disk.rs#L1048-L1213`):

1. **File Opening and Locking**: Opens the image with read/write permissions. Immediately attempts an exclusive whole-file advisory lock using `libc::F_SETLK` (`src/disk.rs#L1076-L1084`). This prevents independent host processes from concurrently accessing the same raw image without going through the Master-Proxy IPC protocol.

2. **Sizing and Memory Mapping**: New images are sized to `total_size` via `file.set_len(total_size)`. The full image is mapped into process address space using `MmapOptions::new().map_mut(&file)` (`src/disk.rs#L1091`).

3. **Superblock Verification**:
   - For new files: Constructs a fresh `SuperBlock`, configures encryption salt/flags if requested, writes the serialized block to offset 0, and initializes root directory inode 0 (`src/disk.rs#L1095-L1121`, `L1193-L1202`).
   - For existing files: Reads and deserializes `SuperBlock` from block 0. Verifies magic bytes (`OIFS`) against `SuperBlock::MAGIC` (`src/disk.rs#L1122-L1141`).

4. **Key Derivation**: If `superblock.encrypted` is true, derives the 256-bit AEAD key using Argon2id with `superblock.encryption_salt` (`src/disk.rs#L1131-L1141`).

5. **Journal Handling**: For journaled images, formats journal if new or recovers journal if existing before publishing the manager (`src/disk.rs#L1144-L1210`).

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

Mutating operations (`create_entry_internal`, `write_data_with_filters`, `delete_file`) compute modified ranges and invoke `sync_mutation_ranges` (`src/disk.rs#L241-L272`). The `Drop` implementation for `DiskManagerInner` (`src/disk.rs#L308-L313`) executes `self.mmap.flush()`, while explicit synchronous flushing is exposed via `DiskManager::flush` (`src/disk.rs#L2090-L2093`).

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

File mutation via `write_data_with_filters` (`src/disk.rs#L3218-L3486`) uses a three-stage approach to minimize lock contention:
1. **Out-of-Lock Preparation** (shared read lock): Inspects inode state, reads existing data if needed, and prepares a write plan via `plan_write_prepared` (`src/disk.rs#L850-L946`).
2. **CPU-Bound Processing**: Performs filtering, compression, and encryption outside any filesystem lock.
3. **Exclusive Lock Acquisition & Commit**: Acquires exclusive write lock only to verify preconditions, potentially replan via `plan_write` (`src/disk.rs#L948-L986`) if raced, then commit the transaction.

The write plan selects among four cases:
1. **FullOverwrite**: Offset-0 rewrite that runs the full filter-compression-encryption staging when rewriting the entire file.
2. **CompressedAppend**: Fast Zstd multi-frame append at EOF for eligible compressed, unencrypted, unfiltered files.
3. **Recompress**: Read-modify-recompress fallback for compressed files when the fast path is unavailable (random writes, encrypted files, or active filters).
4. **Raw**: Direct block writes for uncompressed files, either extending or overwriting in place.

## Inode Caching (P2.2)

`DiskManagerInner` maintains an in-memory inode cache (`RwLock<BoundedInodeCache>`) that:
- Serves as a fast path for `read_inode_internal` hits, avoiding bincode deserialization.
- Is populated on cache misses during inode reads (`src/disk.rs#L3463-L3479`).
- Is updated whenever inodes are modified via `write_inode_internal` (`src/disk.rs#L3494-L3495`).
- Does not require explicit invalidation on file deletion; cache entries for deleted inodes are naturally overwritten when those inode IDs are reused for new files.
