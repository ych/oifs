---
type: architecture
title: DiskManager and Persistence Model
description: How OIFS encapsulates memory-mapped I/O, RwLock concurrency, sync-on-write durability, zero-copy read paths, and the integrated filter-compression-encryption pipeline inside DiskManager.
tags: [disk-manager, mmap, persistence, sync-on-write, rwlock, read-at, pipeline, path-resolution]
verified:
  - by: openwiki/0.6.1
    at: 2026-09-29T16:14:34.721Z
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
generated: { by: "pi", at: "2026-09-29T16:14:34.721Z" }
---

## Responsibility and ownership

<!-- openwiki: broken internal link [src/disk.rs#L163-L166] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`DiskManager`](src/disk.rs#L163-L166) is the central coordinator of the OIFS storage engine. It provides the single authoritative interface between raw on-disk bytes and high-level filesystem operations (file/directory creation, reads, writes, deletion, path resolution, integrity checks, and defragmentation).

<!-- openwiki: broken internal link [src/disk.rs#L132-L145] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Internally, state is maintained by [`DiskManagerInner`](src/disk.rs#L132-L145), protected by an `Arc<RwLock<DiskManagerInner>>`:
- `file`: The underlying host image file handle with POSIX advisory write lock (`F_SETLK`).
<!-- openwiki: broken internal link [src/disk.rs#L136] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- `mmap`: The mutable memory-mapped view ([`MmapMut`](src/disk.rs#L136)) spanning the entire filesystem image.
<!-- openwiki: broken internal link [src/superblock.rs] file "src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- `superblock`: In-memory cached copy of the deserialized [`SuperBlock`](src/superblock.rs).
- `encryption_key`: Derived cryptographic key (`Option<EncryptionKey>`) used for authenticated data and filename encryption.
- `free_block_hint` and `free_inode_hint`: Allocation watermarks that convert bitmap scans into amortized $O(1)$ operations.

## Lifecycle, mmap mapping, and initialization

<!-- openwiki: broken internal link [src/disk.rs#L170] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L175] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L184] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L192-L274] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
DiskManager instances are constructed via [`DiskManager::open`](src/disk.rs#L170), [`open_with_password`](src/disk.rs#L175), or [`create_encrypted`](src/disk.rs#L184), all delegating to the unified [`init_or_open`](src/disk.rs#L192-L274) routine:

1. **File Opening and Locking**: Opens the image with read/write permissions. Immediately attempts an exclusive whole-file advisory lock using `libc::F_SETLK` (`src/disk.rs#L208-L217`). This prevents multiple independent host processes from concurrently writing to the same raw image without going through the Master-Proxy IPC protocol.
2. **Sizing and Memory Mapping**: New images are sized to `total_size` via `file.set_len(total_size)`. The full image is mapped into process address space using `MmapOptions::new().map_mut(&file)` (`src/disk.rs#L223`).
3. **Superblock Verification**:
   - For new files: Constructs a fresh `SuperBlock`, configures encryption salt/flags if requested, writes the serialized block to byte offset 0, and allocates root directory inode 0 (`src/disk.rs#L227-L242`).
   - For existing files: Reads and deserializes `SuperBlock` from block 0. Verifies magic bytes (`OIFS`) against `SuperBlock::MAGIC` (`src/disk.rs#L248-L250`).
4. **Key Derivation**: If `superblock.encrypted` is true, derives the 256-bit AEAD key using Argon2id with `superblock.encryption_salt` (`src/disk.rs#L254`).

## Durability and sync-on-write model

OIFS enforces a sync-on-write durability guarantee across all mutating operations while minimizing I/O stalls:

<!-- openwiki: broken internal link [src/disk.rs#L550] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L1088, src/disk.rs#L1146] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L1435] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **Asynchronous Pipeline Flushes**: Every mutating operation — [`create_entry_internal`](src/disk.rs#L550), [`write_data_with_filters`](src/disk.rs#L1088, src/disk.rs#L1146), and [`delete_file`](src/disk.rs#L1435) — concludes with `guard.mmap.flush_async()`. This notifies the operating system page cache to immediately queue dirty memory-mapped pages for background writeback without blocking the caller.
<!-- openwiki: broken internal link [src/disk.rs#L1387-L1390] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **Explicit Synchronization**: The public [`DiskManager::flush`](src/disk.rs#L1387-L1390) method acquires an exclusive write lock and invokes synchronous `guard.mmap.flush()`, blocking until all dirty pages are durably committed to physical media (equivalent to `msync(MS_SYNC)`).
<!-- openwiki: broken internal link [src/disk.rs#L147-L152] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **Graceful Destruction**: The [`Drop` implementation for `DiskManagerInner`](src/disk.rs#L147-L152) automatically executes `self.mmap.flush()`. When the last `Arc` reference drops or a process exits cleanly, all pending changes are guaranteed to be flushed to disk before the file descriptor closes.

## Concurrency model: reader-writer lock

`DiskManager` wraps its inner state in `Arc<RwLock<DiskManagerInner>>` (`src/disk.rs#L164`). This enables high-concurrency workloads:
<!-- openwiki: broken internal link [src/disk.rs#L855] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L869] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L565] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L1349] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L1337] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L1475] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L1762] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **Parallel Read Path**: All read-only methods ([`read_data`](src/disk.rs#L855), [`read_at`](src/disk.rs#L869), [`lookup`](src/disk.rs#L565), [`list_dir`](src/disk.rs#L1349), [`resolve_path`](src/disk.rs#L1337), [`analyze_fragmentation`](src/disk.rs#L1475), and [`verify_integrity`](src/disk.rs#L1762)) acquire shared `.read()` locks. Arbitrary numbers of threads can read files and traverse directories concurrently without thread contention.
<!-- openwiki: broken internal link [src/disk.rs#L1054] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L555] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L560] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L1392] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L1387] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **Exclusive Write Path**: Mutating methods ([`write_data_with_filters`](src/disk.rs#L1054), [`create_file`](src/disk.rs#L555), [`create_directory`](src/disk.rs#L560), [`delete_file`](src/disk.rs#L1392), and [`flush`](src/disk.rs#L1387)) acquire exclusive `.write()` locks, ensuring ACID isolation for block allocation, inode updates, and directory modifications.

## The read pipeline and zero-copy `read_at`

### Complete file read (`read_data_internal`)

<!-- openwiki: broken internal link [src/disk.rs#L655-L845] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Reading an entire file via [`read_data_internal`](src/disk.rs#L655-L845) executes in two phases: physical assembly followed by pipeline reversal:

1. **Physical Block Assembly** (`src/disk.rs#L665-L810`):
   - Direct blocks (`0..10`): Copies up to 4KB per block directly from mmap.
   - Single indirect blocks (`10..522`): Batch-resolves pointer blocks in 8-byte chunks (`chunks_exact(8)`) via `u64::from_le_bytes`, skipping unallocated holes in sparse files.
   - Double indirect blocks (`522..262666`): Resolves single indirect pointer arrays in batches.
   - Triple indirect blocks (`262666..134480394`): Traverses 3-level indirect tree up to 513 GB.
2. **Reverse Pipeline Execution** (`src/disk.rs#L813-L845`):
   - **Step 1 (Decryption)**: If `inode.encrypted`, invokes XChaCha20-Poly1305 with `guard.encryption_key` and `inode.encryption_nonce`.
   - **Step 2 (Decompression)**: If `inode.compressed_size > 0`, feeds the stream into `zstd::stream::decode_all`. This transparently decompresses concatenated multi-frame streams.
   - **Step 3 (Filter Unapply)**: If pre-compression filters were configured on the inode, runs `unapply_filters` in reverse order (un-bitshuffle / un-shuffle $\to$ un-delta).

### Zero-copy slice reads (`read_at`)

<!-- openwiki: broken internal link [src/disk.rs#L869-L950] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
For high-throughput random read patterns, [`DiskManager::read_at`](src/disk.rs#L869-L950) provides zero-copy chunk reads:
- For raw (uncompressed and unencrypted) files, it bypasses heap allocation entirely.
- It translates `file_offset` into starting logical block index and in-block offset, resolves the physical block through direct or indirect pointers, and directly executes `buf[...].copy_from_slice(&slice[...])` from memory-mapped blocks into the user-provided destination buffer.

## The write pipeline (`write_data_with_filters`)

<!-- openwiki: broken internal link [src/disk.rs#L1054-L1148] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
File mutation via [`write_data_with_filters`](src/disk.rs#L1054-L1148) supports three distinct operational cases:

```
write_data_with_filters(inode, offset, data)
  │
  ├── offset == 0
  │     └── write_data_from_start_internal (filter → compress → encrypt)
  │
  ├── compressed_size > 0
  │     ├── EOF append, unencrypted, no active filters
  │     │     └── Zstd Multi-Frame Append (compress chunk as standalone frame)
  │     └── Random offset, encrypted, or filtered
  │           └── Read-Modify-Recompress (decompress, splice, re-write from 0)
  │
  └── Raw uncompressed file
        └── write_buffer_at_offset (direct in-place or extending block writes)
```

1. **Initial Write / Overwrite from Offset 0** (`src/disk.rs#L1063-L1065`):
   Delegates to `write_data_from_start_internal` (`src/disk.rs#L953-L1040`). Applies zero-copy filter staging via `apply_filters_cow`, conditional Zstd compression (Auto/Always/Never), and AEAD encryption with a freshly generated 24-byte nonce.
2. **Compressed Stream Appends & Edits** (`src/disk.rs#L1067-L1139`):
   - **Fast-Path (Zstd Multi-Frame Append)**: When appending strictly at EOF (`file_offset == inode.size`) to an unencrypted file with no active filters, the new chunk is compressed as an independent Zstd frame and appended directly to the end of physical data blocks (`src/disk.rs#L1074-L1090`).
   - **Fallback (Read-Modify-Recompress)**: When performing random/overwrite writes, writing to encrypted files, or modifying files with active filters, the existing content is decompressed into memory, modified in-place, previous physical blocks are returned to the block allocator, and the combined payload is recompressed and rewritten (`src/disk.rs#L1092-L1139`).
3. **Raw Uncompressed Writes** (`src/disk.rs#L1141-L1148`):
   Invokes `write_buffer_at_offset` to directly write into existing data blocks or allocate new sequential blocks via `allocate_block`.

## Path resolution and directory operations

Path navigation and directory lookups map human-readable hierarchy to physical inodes:

<!-- openwiki: broken internal link [src/disk.rs#L1337-L1341] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L1370-L1385] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L565-L587] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **Path Resolution**: [`resolve_path`](src/disk.rs#L1337-L1341) and [`resolve_parent`](src/disk.rs#L1370-L1385) split paths by `/`, filtering out empty segments and `.`. Traversal starts from `superblock.root_inode` (inode 0) and repeatedly calls [`lookup`](src/disk.rs#L565-L587).
- **Encrypted Filename Support**: When encryption is active, `lookup` and `list_dir` automatically encrypt lookup keys or decrypt directory entries using deterministic directory-tweak SIV encryption (`src/disk.rs#L577-L583`, `src/disk.rs#L1360-L1366`).
<!-- openwiki: broken internal link [src/disk.rs#L1392-L1450] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **File Deletion**: [`delete_file`](src/disk.rs#L1392-L1450) locates the entry in the parent directory block, rewrites remaining entries via `rewrite_dir_entries_in_block`, collects and frees all direct, indirect, and double-indirect blocks in the data bitmap, updates `free_block_hint`, and frees the inode in the inode bitmap.
