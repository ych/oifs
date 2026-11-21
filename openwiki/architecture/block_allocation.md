---
type: architecture
title: Block & Inode Allocation
description: Describes the two-bitmap allocator (inode bitmap, data bitmap), the search-hint optimization (free_block_hint, free_inode_hint), the AllocSim used by journaled operations to simulate allocations against bitmap copies, and the contiguous-run allocator for seekable chunked files.
tags: [allocation, bitmap, indirect-blocks, hint, alloc-sim, contiguous, journaling]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-57692e9ab78d05d0aeba3e7c
    resource: repo://src/allocator.rs
  - id: openwiki-source-69dc9c7ca45b38a30db2f06f
    resource: repo://src/bitmap.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-01ceb1b2a92015c2306721dc
    resource: repo://src/disk/chunked.rs
  - id: openwiki-source-bc305a37042018e1ebd6d860
    resource: repo://src/inode.rs
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---

## Responsibility and ownership

Block and inode allocation is centralized in `SimpleBlockAllocator` (`src/allocator.rs`) operating on 4 KB bitmap slices. The `DiskManagerInner` (`src/disk.rs`) owns the actual bitmap slices in the mmapped image and drives allocation through cached search hints stored in `free_block_hint` and `free_inode_hint`. Every allocation, deallocation, directory write, and indirect-block expansion on the write path goes through this machinery, so it is the single authority over which physical blocks are free or in use.

For journaled images, allocation is first simulated against in-memory bitmap copies via `AllocSim` (`src/journal.rs`) before any on-disk metadata is mutated. `AllocSim` mirrors `SimpleBlockAllocator` semantics exactly (same hint handling, same "first free bit at or after hint" rule), so the IDs it predicts are the IDs the real allocator will hand out when the transaction is applied.

## On-disk bitmap representation

A bitmap is a flat bit array stored in one or more 4 KB blocks. Bit `0` means free and bit `1` means allocated. `SuperBlock` (`src/superblock.rs`) records where each bitmap lives:

- Inode bitmap at `inode_bitmap_block` (block 1 in legacy layout; spans `inode_bitmap_blocks` blocks in v3 layout), tracking inodes from block 0.
- Data bitmap at `data_bitmap_block` (block 2 in legacy; spans `data_bitmap_blocks` blocks in v3), tracking data blocks starting at `data_block_start`.

Bit index mapping: for the data bitmap, the bit index for a given `block_id` is `block_id - data_block_start`. The inode bitmap uses a direct 0-based index (inode ID == bit index).

Two views wrap the raw bytes:

- `BitmapRef` (`src/bitmap.rs`) — read-only view used for analysis, scanning, and fsck.
- `Bitmap` (`src/bitmap.rs`) — mutable view that forwards `set`/`clear` and delegates search helpers to a `BitmapRef`.

## 64-bit word scanning and hardware population count

The bitmap deliberately processes 64 bits (8 bytes) per CPU word instead of bit by bit. The key operations in `BitmapRef` all use `chunks_exact(8)`:

- `find_first_free_from` masks out bits before the search start with `(1u64 << bit_in_chunk) - 1`, then locates the first zero bit using `(!masked_word).trailing_zeros()` — a single hardware `tzcnt` instruction.
- `for_each_set_bit` (used by fsck, defragmentation, and `analyze_fragmentation`) iterates set bits with `word &= word - 1` to clear the lowest set bit each cycle, yielding only as many iterations as there are allocated blocks.
- `analyze_fragmentation` (`src/disk.rs`) additionally special-cases whole words equal to `0` (all free) or `u64::MAX` (all used) so densely packed regions are scanned in constant time.

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

1. For each chunk, `sim.alloc_contiguous(mmap, prepared.blocks_needed)` reserves a contiguous run in the simulator's data-bitmap copy.
2. The run is recorded in the WAL via `SetDataBitmapRange { start_block, count, allocated: true }` (coalesced from individual `SetDataBitmap` ops).
3. Payload bytes are written into the newly allocated blocks in the mmap.
4. Old chunk blocks (if any) are freed via `sim.free_contiguous` and `SetDataBitmapRange { allocated: false }`.
5. The inode's chunk-entry pointer is updated to point at the new run (`sim_set_logical_chunk_entry` emits a `WriteBlockSlice` for the pointer table entry).

This guarantees that every seekable chunk maps to a single contiguous physical extent, eliminating intra-chunk seeks.

## AllocSim: WAL-first allocation simulation

`AllocSim` (`src/journal.rs`) is an in-memory allocation simulator used exclusively by journaled code paths (`write_data_journaled`, `commit_prepared_chunked_write`, `truncate_chunked_64k`, `create_entry_journaled`). It mirrors `SimpleBlockAllocator` behavior exactly so that block IDs assigned during simulation are the same IDs the real allocator will produce when the transaction is applied.

Key design points:

- **Lazy data-bitmap copy**: `new_data_only` avoids copying the data bitmap until the first allocation or free actually happens, saving 4–64 KB memcpy on pure in-place overwrites.
- **Fresh-block tracking**: `fresh_blocks`, `fresh_set`, and `unzeroed_fresh` record blocks allocated during this transaction. When a fresh block is first written, `take_unzeroed_fresh` returns true so the caller can zero the slack bytes before writing payload.
- **Pointer simulation**: `pointers` map `(parent_block, entry_index) → child_block` records indirect-block pointer writes without touching the mmap. `sim_get_or_alloc_block` and `sim_child` use this to build the indirect tree in the WAL.
- **Hint synchronization**: On commit, `guard.free_block_hint = sim.free_block_hint` and `guard.free_inode_hint = sim.free_inode_hint` advance the real hints to match the simulated state.

Allocation operations against `AllocSim` emit `MetadataOp` entries that are later coalesced and written to the journal:

- Single block: `SetDataBitmap { block_id, allocated: true }`
- Contiguous run: `SetDataBitmapRange { start_block, count, allocated: true }` (after coalescing)
- Free: same ops with `allocated: false`

## Journal interaction: SetDataBitmap and SetDataBitmapRange

The journal encodes bitmap mutations as two operation types:

- `SetDataBitmap { block_id, allocated }` — single-bit set/clear.
- `SetDataBitmapRange { start_block, count, allocated }` — contiguous run set/clear.

`coalesce_ops` (`src/journal.rs`) merges adjacent `SetDataBitmap` ops of the same polarity into `SetDataBitmapRange`, shrinking WAL frame sizes for multi-block operations by orders of magnitude. The canonical phase order in a committed transaction is: `SetInodeBitmap` → `SetDataBitmapRange` → `WriteBlockSlice` → `WriteInode`. During replay, applying a `SetDataBitmap` or `SetDataBitmapRange` is strictly idempotent (Kani-verified), so redo is safe.

## Indirect block addressing scheme

`DiskManager` maps logical file blocks to physical blocks using a hybrid direct + indirect index tree. Each block pointer entry is an 8-byte little-endian `u64`, so a 4 KB block holds 512 pointers.

| Level | Pointer field | Capacity | Logical block index range |
| --- | --- | --- | --- |
| Direct | `blocks[0..10]` | 10 blocks | 0 – 9 |
| Single indirect | `blocks[10]` | 512 blocks | 10 – 521 |
| Double indirect | `blocks[11]` | 262,144 blocks | 522 – 262,665 |
| Triple indirect | `triple_indirect` | 134,217,728 blocks | 262,666 – 134,480,393 |

Total: 134,480,394 blocks × 4 KB ≈ 513 GB. Allocation walks the levels via `get_or_alloc_block` (`src/disk.rs`), lazily allocating intermediate sub-indirect blocks with `alloc_and_zero_block`. Reads and collection use hierarchical skipping: when a sub-tree pointer is zero, the code jumps past the entire subtree instead of visiting each empty block. The traversals are also capped by the file's logical/compressed size via `total_logical_blocks`, eliminating millions of unnecessary empty-block checks on huge files.

## Invariant: collision-free, monotone allocation

Two consecutive allocations never return the same block: `allocate_with_hint` sets a bit immediately after finding it, and the hint advances past it, so the next allocation starts strictly ahead. This is a Kani-verified property (`src/allocator.rs`) and is what makes the sequential hint path safe under concurrent access serialized by the `DiskManager` mutex.

## Extension seams

Allocation is centralized behind the `BlockAllocator` trait (`src/allocator.rs`), so alternative allocation strategies could be added without touching the write path. The hint machinery, bitmap word scanning, the indirect addressing tree, `AllocSim`, and the contiguous-run allocator are independent seams — each was optimized without altering the others.
