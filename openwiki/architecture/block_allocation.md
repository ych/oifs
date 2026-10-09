---
type: architecture
title: Block Allocation and Bitmap Indexing
description: How OIFS allocates and tracks data blocks and inodes with 64-bit bitmap scanning, allocation hints for amortized O(1) sequential allocation, and the single/double/triple indirect block addressing scheme that expands files up to ~513GB.
tags: [allocation, bitmap, indirect-blocks, hint, performance]
sources:
  - id: openwiki-source-57692e9ab78d05d0aeba3e7c
    resource: repo://src/allocator.rs
  - id: openwiki-source-69dc9c7ca45b38a30db2f06f
    resource: repo://src/bitmap.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-bc305a37042018e1ebd6d860
    resource: repo://src/inode.rs
verified:
  - by: openwiki/0.6.1
    at: 2026-10-09T15:04:29.410Z
generated: { by: "openwiki/0.6.1", at: "2026-10-09T15:04:29.410Z" }
---

## Responsibility and ownership

Block allocation is the responsibility of the bitmap subsystem
[`BitmapRef`](../../src/bitmap.rs) combined with the 
[`SimpleBlockAllocator`](../../src/allocator.rs#L36-L88). The 
[`DiskManager`](../../src/disk.rs) owns the actual bitmap slices in the image and drives
allocation through cached search hints stored in
`DiskManagerInner` (`free_block_hint`, `free_inode_hint`). Every allocation,
deallocation, directory write, and indirect-block expansion on the write path
goes through this machinery, so it is the single authority over which physical
blocks are free or in use.

## On-disk bitmap representation

A bitmap is a flat bit array stored in one 4KB block. Bit `0` means free and bit
`1` means allocated. `SuperBlock` (`../../src/superblock.rs`) records where each
bitmap lives: the inode bitmap at `inode_bitmap_block` (block 1, tracking inodes
from block 0), the data bitmap at `data_bitmap_block` (block 2, tracking data
blocks starting at `data_block_start`).

Two views wrap the raw bytes:

- [`BitmapRef`](../../src/bitmap.rs#L2-L111) — read-only view used for analysis,
  scanning, and fsck.
- [`Bitmap`](../../src/bitmap.rs#L186-L245) — mutable view that forwards `set` /
  `clear` and delegates search helpers to a `BitmapRef`.

## 64-bit word scanning and hardware population

The bitmap deliberately processes 64 bits (8 bytes) per CPU word instead of bit
by bit. The key operations in `BitmapRef` (`../../src/bitmap.rs`) all use
`chunks_exact(8)`:

- `find_first_free_from` masks out bits before the search start with
  `(1u64 << bit_in_chunk) - 1`, then locates the first zero bit using
  `(!masked_word).trailing_zeros()` — a single hardware `tzcnt` instruction.
- `for_each_set_bit` (used by fsck, defragmentation, and `analyze_fragmentation`)
  iterates set bits with `word &= word - 1` to clear the lowest set bit each
  cycle, yielding only as many iterations as there are allocated blocks.
- `analyze_fragmentation` (`../../src/disk.rs`) additionally special-cases whole words
  equal to `0` (all free) or `u64::MAX` (all used) so densely packed regions are
  scanned in constant time.

These optimizations are what the README reports as 11x–13.4x faster free-block
scans and a ~7.4x fsck speedup over bit-by-bit scanning.

## Allocation hints for amortized O(1) sequential allocation

Before the hint feature, allocating many blocks sequentially re-scanned the
bitmap from bit 0 every time, producing $O(N^2)$ behavior. 
(`../../src/allocator.rs#L48-L70`) accepts an optional hint block ID and translates it
to a bit index (`hint - start_block_offset`) before calling
`find_next_free_wrapped`.

The `DiskManager` keeps two hints in `DiskManagerInner` and advances them after
each allocation:

- `allocate_block` (`../../src/disk.rs`) reads `free_block_hint`, allocates, then sets
  `free_block_hint = blk + 1`.
- Inode allocation in `create_entry_internal` reads `free_inode_hint`, allocates,
  then sets `free_inode_hint = new_inode_id + 1`.

When a hint points past the end of the bitmap, `find_next_free_wrapped`
(`../../src/bitmap.rs#L113-L125`) wraps around and searches from bit 0 for a bit
below the hint, so allocation is guaranteed to find space whenever one exists.
Freeing a block updates the hint downward (`min_freed_blk < free_block_hint`),
keeping sequential allocation compact.

## Indirect block addressing scheme

[`DiskManager`](../../src/disk.rs) maps logical file blocks to physical blocks using
a hybrid direct + indirect index tree. Each block pointer entry is an 8-byte
little-endian `u64`, so a 4KB block holds 512 pointers. The layout
(`../../src/disk.rs#L197-L325` for allocation, `collect_inode_blocks` for collection):

| Level | Pointer field | Capacity | Logical block index range |
| --- | --- | --- | --- |
| Direct | `blocks[0..10]` | 10 blocks | 0 – 9 |
| Single indirect | `blocks[10]` | 512 blocks | 10 – 521 |
| Double indirect | `blocks[11]` | 262,144 blocks | 522 – 262,665 |
| Triple indirect | `triple_indirect` | 134,217,728 blocks | 262,666 – 134,480,393 |

The total is 134,480,394 blocks × 4KB ≈ 513GB. Allocation walks the levels via
`get_or_alloc_block` (`../../src/disk.rs#L786-L866`), lazily allocating intermediate
sub-indirect blocks with `alloc_and_zero_block`. Reads and collection use
hierarchical skipping: when a sub-tree pointer is zero, the code jumps past the
entire subtree instead of visiting each empty block, which collapsed sparse
large-file read latency by >690x. The traversals are also capped by the file's
logical/compressed size via `total_logical_blocks`, eliminating millions of
unnecessary empty-block checks on huge files.

## Invariant: collision-free, monotone allocation

Two consecutive allocations never return the same block: `allocate_with_hint`
sets a bit immediately after finding it, and the hint advances past it, so the
next allocation starts strictly ahead. This is a Kani-verified property
(`../../src/allocator.rs#L120-L135`) and is what makes the sequential hint path safe
under concurrent access serialized by the DiskManager mutex.

## Extension seams

Allocation is centralized behind the [`BlockAllocator`](../../src/allocator.rs#L13-L26)
trait (`allocate`/`free`), so alternative allocation strategies could be added
without touching the write path. The hint machinery, bitmap word scanning, and
the indirect addressing tree are independent seams — each was optimized without
altering the others.
