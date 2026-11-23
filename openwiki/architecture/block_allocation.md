---
type: architecture
title: Block & Inode Allocation
description: Documents the bitmap-based block allocator (SimpleBlockAllocator), inode allocator, free space tracking, allocation strategies (hints, contiguous runs), multi-block bitmap layout (v3), hardware-accelerated tzcnt scanning, BitmapSliceMut write-through mirrors for FileIo mode, and allocation simulation for WAL transactions.
tags: [allocation, bitmap, indirect-blocks, hint, alloc-sim, contiguous, journaling, v3-layout]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-23T20:55:55.458Z
sources:
  - id: openwiki-source-57692e9ab78d05d0aeba3e7c
    resource: repo://src/allocator.rs
  - id: openwiki-source-69dc9c7ca45b38a30db2f06f
    resource: repo://src/bitmap.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-01ceb1b2a92015c2306721dc
    resource: repo://src/disk/chunked.rs
  - id: openwiki-source-115024023ade9f243c6b4ec3
    resource: repo://src/disk/maintenance.rs
  - id: openwiki-source-bc305a37042018e1ebd6d860
    resource: repo://src/inode.rs
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
  - id: openwiki-source-62eef0b8799186dd9e2faae9
    resource: repo://src/storage.rs
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-23T20:55:55.458Z" }
---

## Responsibility and ownership

Block and inode allocation is centralized in `SimpleBlockAllocator` (`src/allocator.rs`) operating on 4 KB bitmap slices. The `DiskManagerInner` (`src/disk.rs`) owns the actual bitmap slices in the mmapped image (or in-memory mirrors under `FileIo` mode) and drives allocation through cached search hints stored in `free_block_hint` and `free_inode_hint`. Every allocation, deallocation, directory write, and indirect-block expansion on the write path goes through this machinery, so it is the single authority over which physical blocks are free or in use.

For journaled images, allocation is first simulated against in-memory bitmap copies via `AllocSim` (`src/journal.rs`) before any on-disk metadata is mutated. `AllocSim` mirrors `SimpleBlockAllocator` semantics exactly (same hint handling, same "first free bit at or after hint" rule), so the IDs it predicts are the IDs the real allocator will hand out when the transaction is applied.

## On-disk bitmap representation (v3 multi-block layout)

A bitmap is a flat bit array stored in one or more 4 KB blocks. Bit `0` means free and bit `1` means allocated. `SuperBlock` (`src/superblock.rs`) records where each bitmap lives and how many blocks each spans:

- Inode bitmap starts at `inode_bitmap_block` (block 1 in legacy layout; spans `inode_bitmap_blocks()` blocks in v3), tracking inodes from ID 0.
- Data bitmap starts at `data_bitmap_block` (block 2 in legacy; spans `data_bitmap_blocks()` blocks in v3), tracking data blocks starting at `data_block_start`.

The v3 layout computes `inode_bitmap_blocks = inode_count.div_ceil(BITMAP_BLOCK_BITS).max(1)` and `data_bitmap_blocks` derived from the recorded `data_block_start` and `inode_table_block` offsets. Bit index mapping: for the data bitmap, the bit index for a given `block_id` is `block_id - data_block_start`. The inode bitmap uses a direct 0-based index (inode ID == bit index).

Two views wrap the raw bytes:

- `BitmapRef` (`src/bitmap.rs`) — read-only view used for analysis, scanning, and fsck.
- `Bitmap` (`src/bitmap.rs`) — mutable view that forwards `set`/`clear` and delegates search helpers to a `BitmapRef`.

Under `StorageMode::FileIo`, the on-disk bitmaps are mirrored in memory (`FileBacking.inode_bitmap` / `data_bitmap` in `src/storage.rs`). Mutable access is provided through `BitmapSliceMut` (see below).

## 64-bit word scanning and hardware `tzcnt`

The bitmap deliberately processes 64 bits (8 bytes) per CPU word instead of bit by bit. The key operations in `BitmapRef` all use `chunks_exact(8)`:

- `find_first_free_from` masks out bits before the search start with `(1u64 << bit_in_chunk) - 1`, then locates the first zero bit using `(!masked_word).trailing_zeros()` — a single hardware `tzcnt` instruction on x86/ARM.
- `for_each_set_bit` (used by fsck, defragmentation, and `analyze_fragmentation`) iterates set bits with `word &= word - 1` to clear the lowest set bit each cycle, yielding only as many iterations as there are allocated blocks.
- `analyze_fragmentation` (`src/disk/maintenance.rs`) additionally special-cases whole words equal to `0` (all free) or `u64::MAX` (all used) so densely packed regions are scanned in constant time.

These optimizations deliver 11×–13.4× faster free-block scans and ~7.4× fsck speedup over bit-by-bit scanning.

## Allocation hints for amortized O(1) sequential allocation

Before hints, allocating many blocks sequentially re-scanned the bitmap from bit 0 every time, producing O(N²) behavior. `allocate_with_hint` (`src/allocator.rs`) accepts an optional hint block ID and translates it to a bit index (`hint - start_block_offset`) before calling `find_next_free_wrapped`.

`DiskManagerInner` keeps two hints and advances them after each allocation:

- `allocate_block` reads `free_block_hint`, allocates, then sets `free_block_hint = blk + 1`.
- Inode allocation in `create_entry_internal` reads `free_inode_hint`, allocates, then sets `free_inode_hint = new_inode_id + 1`.

When a hint points past the end of the bitmap, `find_next_free_wrapped` wraps around and searches from bit 0 for a bit below the hint, so allocation is guaranteed to find space whenever one exists. Freeing a block updates the hint downward (`min_freed_blk < free_block_hint`), keeping sequential allocation compact.

## Contiguous-run allocation for seekable 64K chunks

Seekable 64 KiB chunked files (`INODE_FLAG_SEEKABLE_64K`) require each chunk to occupy a contiguous run of physical blocks so that a chunk can be read with a single I/O operation. `SimpleBlockAllocator::allocate_contiguous` (`src/allocator.rs`) finds the first run of `count` consecutive free bits starting from a hint, using `BitmapRef::find_contiguous_free_wrapped`. The allocator scans 64-bit words and skips entirely used words in O(1), then checks candidate runs bit-by-bit only when a mixed word is encountered.

The write path (`commit_prepared_chunked_write` in `src/disk/chunked.rs`) stages through `AllocSim`:

1. For each chunk, `sim.alloc_contiguous(backing, prepared.blocks_needed)` reserves a contiguous run in the simulator's data-bitmap copy.
2. The run is recorded in the WAL via `SetDataBitmapRange { start_block, count, allocated: true }` (coalesced from individual `SetDataBitmap` ops).
3. Payload bytes are written into the newly allocated blocks in the mmap.
4. Old chunk blocks (if any) are freed via `sim.free_contiguous` and `SetDataBitmapRange { allocated: false }`.
5. The inode's chunk-entry pointer is updated to point at the new run (`sim_set_logical_chunk_entry` emits a `WriteBlockSlice` for the pointer table entry).

This guarantees that every seekable chunk maps to a single contiguous physical extent, eliminating intra-chunk seeks.

## BitmapSliceMut: write-through mirrors for FileIo mode

Under `StorageMode::FileIo` there is no mmap; the filesystem image is accessed via `pread`/`pwrite`. To keep bitmap mutations coherent, `DiskManagerInner` provides mutable access through `BitmapSliceMut` (`src/storage.rs`):

```rust
pub(crate) struct BitmapSliceMut<'a> {
    slice: &'a mut [u8],
    file_writeback: Option<(&'a File, u64)>,  // (file handle, file offset of bitmap start)
    dirty_range: Option<(usize, usize)>,     // narrowed byte range [start..end)
}
```

- `inode_bitmap_slice_mut()` and `data_bitmap_slice_mut()` on `ImageBacking` return a `BitmapSliceMut` borrowing the in-memory mirror.
- `DerefMut` allows direct byte mutation (`slice[byte] |= mask`).
- `set_dirty_range(byte_start, byte_end)` and `set_dirty_bits(start_bit, bit_count)` narrow the flush range.
- On `Drop`, `flush_writeback()` is called automatically: under `FileIo` it issues a `pwrite` of the dirty range to the bitmap's file offset; under `Mmap` it is a no-op (the mmap is already coherent).
- Explicit `flush_writeback()` is also available for callers that need synchronous persistence before the guard drops.

## Allocation simulation (`AllocSim`) for WAL transactions

Journaled images must never mutate on-disk metadata before the transaction is logged. `AllocSim` (`src/journal.rs`) provides a copy-on-write simulator:

- It clones the inode and data bitmap slices on first use (`ensure_data_bitmap` is lazy for data-only transactions).
- It wraps `SimpleBlockAllocator` to perform `alloc_block`, `alloc_contiguous`, `free_contiguous` against the copies.
- It tracks `fresh_blocks` (blocks allocated during the transaction) and `pointers` (simulated indirect pointer updates).
- On commit, the simulator's `free_block_hint` and `free_inode_hint` are copied back to `DiskManagerInner`, so subsequent allocations resume from the predicted state.

This design ensures that the WAL can roll back a failed transaction by simply discarding the simulator; no on-disk state is touched until `apply_journal_ops` runs.

## Inode allocation and free inode tracking

Inode allocation uses the same `SimpleBlockAllocator` machinery with a zero `start_block_offset` (inode ID == bit index). The inode bitmap may track more IDs than the inode table can hold (`usable_inode_count`); `find_next_free_bounded` limits the search to valid table capacity. Allocation proceeds as:

1. Read `free_inode_hint`.
2. `BitmapRef::find_next_free_bounded(hint, usable_inode_count)` → `new_inode_id`.
3. If `new_inode_id >= usable_inode_count`, release the bit and return `NoSpace`.
4. Set bit, update `free_inode_hint = new_inode_id + 1`.
5. Under `FileIo`, `BitmapSliceMut::set_dirty_bits(id, 1)` narrows the pwrite to the affected byte.

Freeing an inode clears the bit and rewinds `free_inode_hint = min(free_inode_hint, inode_id)` so newly freed IDs are reused quickly.

## Free space tracking and fragmentation analysis

`analyze_fragmentation` (`src/disk/maintenance.rs`) walks the data bitmap once using the same 64-bit word fast path:

- Counts `used_blocks`, `free_blocks`, `free_runs`, `largest_free_run`, `total_gap_size`.
- Special-cases `word == 0` (64 free blocks in O(1)) and `word == u64::MAX` (64 used blocks in O(1)).
- Computes `avg_gap_size = total_gap_size / free_runs`.
- Computes `fragmentation_ratio = (free_runs - 1) / (free_blocks - 1)` (0.0 = perfectly contiguous free space, 1.0 = maximum fragmentation).

This analysis is used by the defragmenter and exposed via CLI/FUSE/NFS/IPC for monitoring.

## Block deallocation and hint rewind

`free` and `free_contiguous` clear the corresponding bits and update `free_block_hint` downward if the freed range starts before the current hint. This keeps the allocator's search window compact after truncation or file deletion, avoiding unnecessary scans past large freed regions.

## Indirect block allocation

Indirect blocks are allocated via the same `allocate_block` / `allocate_contiguous` path. `get_or_alloc_indirect_child` and `get_or_alloc_block_for_write` resolve or allocate pointer entries on demand, always verifying that both the parent indirect block and any child block live inside the data area (`is_data_block` check). This ensures the allocator never hands out metadata-region blocks for payload data.
