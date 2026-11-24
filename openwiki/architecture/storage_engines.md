---
type: "Reference"
title: "Storage Engines"
description: "OIFS dual storage backends—Mmap for zero-copy memory-mapped I/O and FileIo for pure positional pread/pwrite I/O—operating over the exact same on-disk format."
tags: ["storage", "mmap", "fileio", "backend", "architecture"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-24T19:09:23.676Z
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-62eef0b8799186dd9e2faae9
    resource: repo://src/storage.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-24T19:09:23.676Z" }
---

# Dual Storage Engine: Mmap and FileIo

OIFS provides two distinct storage backends that operate over the **exact same on-disk format**. The choice of backend is made at mount time and affects performance characteristics, failure modes, and operational constraints—but not the on-disk layout.

## Storage Mode Selection

### CLI and Environment

| Method | Flag / Variable | Values | Default |
|--------|-----------------|--------|---------|
| CLI (global) | `--no-mmap` | boolean | `false` |
| Environment | `OIFS_STORAGE_MODE` | `mmap`, `pread`, `file`, `file_io`, `no-mmap` | `mmap` |
| API | `DiskManager::open_with_storage_mode()` | `StorageMode::Mmap` / `StorageMode::FileIo` | — |

The `--no-mmap` flag sets a process-wide default to `StorageMode::FileIo`:

```rust
// src/bin/oifs.rs:190
#[arg(long = "no-mmap", global = true)]
no_mmap: bool,

// src/bin/oifs.rs:547
if cli.no_mmap {
    oifs::storage::set_default_storage_mode(oifs::StorageMode::FileIo);
}
```

### Read-Only Auto-Selection

When opening an image read-only (`open_read_only*` APIs), the backing is **always** memory-mapped with `PROT_READ` (`MmapReadOnly`) when `StorageMode::Mmap` is selected, regardless of the file's permissions. This works on `0444` images and provides true zero-copy slicing.

```rust
// src/storage.rs:1003-1012
StorageMode::Mmap => {
    // SAFETY: caller holds shared advisory lock (F_RDLCK) on file
    let mmap = unsafe { MmapOptions::new().map(file)? };
    Ok(Self::MmapReadOnly(mmap))
}
```

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                        DiskManager (public API)                     │
└─────────────────────────────────┬───────────────────────────────────┘
                                  │
                    ┌─────────────┴─────────────┐
                    ▼                           ▼
            ┌───────────────┐             ┌───────────────┐
            │  Mmap Backend │             │  FileIo Backend│
            │ (StorageMode::│             │ (StorageMode::│
            │    Mmap)      │             │    FileIo)    │
            └───────┬───────┘             └───────┬───────┘
                    │                           │
        ┌───────────┼───────────┐       ┌───────┼───────┐
        ▼           ▼           ▼       ▼       ▼       ▼
    MmapMut    MmapReadOnly   -   FileBacking  BlockCache DataCache
    (RW)       (RO)                (pwrite)   (16 shard) (64 shard)
        │           │                │         4 MiB      Config
        └───────────┴────────────────┴────────────────────┘
                              │
                    ┌─────────┴─────────┐
                    ▼                   ▼
            ┌───────────┐         ┌───────────┐
            │  Metadata │         │  Payload  │
            │ (bitmaps, │         │  (file    │
            │  inodes,  │         │  blocks)  │
            │  journal) │         │           │
            └───────────┘         └───────────┘
```

---

## ImageBacking Trait and Implementations

The unified abstraction is `ImageBacking` (src/storage.rs:964), an enum with three variants:

```rust
pub(crate) enum ImageBacking {
    Mmap(MmapMut),           // read-write, MAP_SHARED
    MmapReadOnly(Mmap),      // read-only, PROT_READ
    File(Box<FileBacking>),  // pure pread/pwrite I/O
}
```

### Mmap Backend (`StorageMode::Mmap`)

- **Mechanism**: Maps the entire `.img` container with `memmap2::MmapMut` (read-write) or `Mmap` (read-only `PROT_READ`).
- **Performance**: Zero-copy memory access (`40–58 GB/s` on resident pages).
- **Read-write**: `MmapMut` with `MAP_SHARED`; modifications propagate to disk via page cache.
- **Read-only**: `MmapReadOnly` with `PROT_READ`; works on `0444` files.
- **Hazards**: Vulnerable to `SIGBUS` on truncation or I/O error; uses `unsafe` for mapping.
- **Flush**: `msync(MS_SYNC)` / `flush_async` via `mmap` methods.

### FileIo Backend (`StorageMode::FileIo`)

- **Mechanism**: 100% `mmap`-free positional `pread(2)` / `pwrite(2)` I/O.
- **Mirrors**: In-memory write-through mirrors for inode bitmap, data bitmap, and journal (WAL).
- **Metadata block cache**: 16-shard bounded 4 KiB block cache (max 1,024 blocks = 4 MiB) for indirect pointer and directory blocks.
- **Data page cache**: 64-shard bounded Second-Chance Clock cache (default 32 MiB, configurable via `OIFS_FILE_IO_CACHE_MB`, max 4 GiB), scan-resistant with `MAX_CACHED_EXTENT = 64 KiB`.
- **Safety**: All I/O in Safe Rust; eliminates `SIGBUS` crashes and `unsafe` memory-mapping hazards.
- **Flush**: `fd.sync_data()`; `flush_async` is a no-op (no dirty page tracking).

---

## FileBacking — FileIo Internals

### Write-Through Mirrors

`FileBacking` maintains three in-memory mirrors loaded at `configure_regions()`:

1. **Inode bitmap** (`inode_bitmap: Vec<u8>`)
2. **Data bitmap** (`data_bitmap: Vec<u8>`)
3. **Journal ring** (`journal_buf: Vec<u8>`) — only when `superblock.has_journal_layout()`

Every `pwrite` via `write_all_at()` calls `sync_mirrors_on_write()`, which updates any overlapping mirror bytes and invalidates/updates the two caches.

### Metadata Block Cache (16-shard, 4 MiB)

```rust
// src/storage.rs:115-180
struct MetadataBlockCache {
    shards: [RwLock<FxHashMap<u64, Arc<[u8; BLOCK_SIZE]>>>; 16],
}
```

- **Sharding**: Fibonacci hash `((block_id * 0x9E37_79B9_7F4A_7C15) >> 60) as usize` for uniform distribution.
- **Capacity**: 64 blocks per shard = 16 × 64 × 4 KiB = 4 MiB total.
- **Eviction**: Arbitrary single-entry drop when a shard is full and a new block arrives.
- **API**: `get()`, `read_u64_le()` (lock-free u64 read under shard read lock), `insert()`, `update_if_present()`, `invalidate()`, `invalidate_block_range()`, `clear()`.
- **Deadlock-free**: `get()` and `read_u64_le()` release the shard read lock before returning `Arc`/value, allowing nested `with_block` calls (e.g., double-indirect traversal) even when two blocks hash to the same shard.

### Data Page Cache (64-shard, Second-Chance Clock)

```rust
// src/storage.rs:182-370
struct DataPageCache {
    shards: [RwLock<DataCacheShard>; 64],
    max_per_shard: AtomicUsize,
    count: AtomicUsize,
}
```

- **Sharding**: Fibonacci hash `((block_id * 0x9E37_79B9_7F4A_7C15) >> 58) as usize` → 64 shards.
- **Default capacity**: 32 MiB (`DEFAULT_FILE_IO_CACHE_BYTES`).
- **Configurable**: `OIFS_FILE_IO_CACHE_MB` environment variable (0 = disabled, max 4 GiB).
- **Scan resistance**: Extents `> MAX_CACHED_EXTENT` (64 KiB) bypass the cache entirely—single contiguous `pread` preserves sequential throughput and prevents large scans from evicting hot random-access pages.
- **Eviction**: Second-Chance Clock (CLOCK with reference bit). Each shard maintains a ring buffer of block IDs and a clock hand. On insertion when full, the hand scans up to `2 * ring.len() + 1` steps, clearing reference bits, until finding an unreferenced entry to evict.
- **Reference bit**: Set on cache hit in `try_copy_into()` (lock-free `AtomicBool::store(true, Relaxed)` under shared shard read lock) and on `insert()`.
- **Coherency**: `update_if_present()` (for partial-block writes), `invalidate()`, `invalidate_block_range()` (for large streaming writes), `clear()` (on capacity resize to 0 or `configure_regions()`).
- **Read path** (`read_data_cached`): For each 4 KiB block, `try_copy_into()` under shared lock; on miss, `pread` full block, copy slice, `insert()` full block.

---

## DiskManager Public Cache Control API

`DiskManager` exposes three methods to inspect and configure the FileIo data page cache at runtime:

| Method | Description |
|--------|-------------|
| `set_file_io_cache_bytes(bytes: usize)` | Sets the cache budget in bytes. `0` disables the cache and releases all cached pages. No-op under `Mmap`. |
| `file_io_cache_bytes() -> usize` | Returns the configured capacity in bytes (`0` for `Mmap` or when disabled). |
| `file_io_cache_used_bytes() -> usize` | Returns the approximate number of bytes currently resident in the cache. |

```rust
// src/disk.rs:1440-1460
pub fn set_file_io_cache_bytes(&self, bytes: usize) { ... }
pub fn file_io_cache_bytes(&self) -> usize { ... }
pub fn file_io_cache_used_bytes(&self) -> usize { ... }
```

---

## BitmapSliceMut and JournalRegionMut — Auto-Flush Guards

Both backends use RAII guards for mutable bitmap and journal access:

- **`BitmapSliceMut`** — `DerefMut` to `[u8]`; on `Drop`, flushes dirty byte range via `pwrite` (FileIo) or no-op (Mmap). `set_dirty_bits(start_bit, bit_count)` narrows the flush range.
- **`JournalRegionMut`** — `DerefMut` to `[u8]`; `mark_append_written()`, `mark_header_only()`, `mark_clean()` control which sub-range is flushed on `Drop`. Under FileIo, writes the WAL frame (and optional wrap pad) then the 64-byte header atomically.

---

## Read-Only Mount Semantics

| Operation | Mmap (RO) | FileIo (RO) |
|-----------|-----------|-------------|
| `read_exact_at` | ✅ zero-copy | ✅ `pread` |
| `read_data_cached` | ✅ zero-copy | ✅ cache + `pread` |
| `with_block` | ✅ zero-copy slice | ✅ cache + `pread` |
| `try_read_data_cached_only` | ✅ | ✅ cache-only |
| `write_all_at` | ❌ `PermissionDenied` | ❌ `PermissionDenied` |
| `zero_block` | ❌ | ❌ |
| `copy_block` | ❌ | ❌ |
| `with_block_mut` | ❌ | ❌ |
| `inode_bitmap_slice_mut` | `None` | `None` |
| `data_bitmap_slice_mut` | `None` | `None` |
| `journal_region_mut` | `Err` | `Err` |
| `apply_journal_op` | ❌ | ❌ |
| `flush*` | ✅ no-op / `msync` | ✅ no-op / `sync_data` |

Read-only mounts are rejected if the image has a journal with unrecovered transactions (`pending_txs > 0`).

---

## Journal Replay Under FileIo

`apply_journal_op()` (src/storage.rs:2000-2150) applies committed `MetadataOp` entries directly to the backing store:

- **SetInodeBitmap / SetDataBitmap / SetDataBitmapRange**: Updates mirror byte(s) + `pwrite` single byte or range.
- **WriteInode**: `pwrite` inode slot + `sync_mirrors_on_write()` for cache coherency.
- **WriteBlockSlice**: `pwrite` data slice + updates both `block_cache` and `data_cache` via `update_if_present()`.

Bounds checks mirror the Mmap in-place applicator exactly.

---

## Cache Invalidation for External Writes

`invalidate_block_range_caches(start_block, count)` (src/storage.rs:2280) invalidates both `data_cache` and `block_cache` for a block range. Use after external FD-level writes (`FICLONERANGE`, `copy_file_range`, raw `pwrite`) bypass `ImageBacking`.

---

## Configuration Constants

| Constant | Value | Description |
|----------|-------|-------------|
| `BLOCK_SIZE` | 4096 | Fundamental block size |
| `DEFAULT_FILE_IO_CACHE_BYTES` | 32 MiB | Default data page cache budget |
| `MAX_FILE_IO_CACHE_BYTES` | 4 GiB | Maximum data page cache budget |
| `MAX_CACHED_EXTENT` | 64 KiB | Maximum read extent eligible for data cache |
| `MetadataBlockCache::NUM_SHARDS` | 16 | Metadata block cache shards |
| `MetadataBlockCache::MAX_PER_SHARD` | 64 | Blocks per metadata shard (4 MiB total) |
| `DataPageCache::NUM_SHARDS` | 64 | Data page cache shards |

---

## Formal Verification Invariants (Kani)

The codebase includes Kani proof harnesses verifying:

1. **`BitmapSliceMut::set_dirty_bits`** — Computes a valid byte range `[byte_start..byte_end)` never exceeding `slice.len()` and covering every bit in the requested range.
2. **Shard index bounds & mixing** — `MetadataBlockCache::shard_idx < 16` and `DataPageCache::shard_idx < 64` for all `u64` block IDs; Fibonacci step never collides in top 4/6 bits for consecutive IDs.
3. **`DataPageCache::blocks_per_shard_for_bytes`** — Clamps against `MAX_FILE_IO_CACHE_BYTES`, no overflow in total capacity computation.
4. **Read-only access policy** — Every mutating operation (`Write`, `BitmapWrite`, `JournalWrite`, `Flush`) forbidden under `read_only=true`; all reads permitted. Zero-copy mmap slicing enabled exactly for `StorageMode::Mmap` (including `MmapReadOnly`).

See `src/storage.rs` `#[cfg(kani)] mod verification` for the complete proofs.
