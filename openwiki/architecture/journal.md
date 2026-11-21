---
type: concept
title: Metadata WAL / Journaling
description: Describes the transactional metadata write-ahead log (WAL) that ensures crash resilience, including its circular buffer design, CRC32C protection, idempotent redo replay, JBD2-style deferred checkpointing under Strict durability, and DurabilityMode semantics.
tags: ["journal", "WAL", "write-ahead logging", "transaction", "recovery", "durability", "CRC32C", "checkpoint"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---

# Metadata WAL / Journaling

The journal subsystem implements a write-ahead log (WAL) for metadata mutations in OIFS. Before any metadata change (e.g., `create_file`, `delete_file`, `mkdir`, inode updates) is applied to the filesystem image, it is first recorded as a checksummed transaction in a fixed-size circular ring buffer residing in the journal region on disk. On crash recovery, the journal replays all durable-but-unapplied transactions to restore a consistent state, eliminating the need for a full `fsck` scan.

## Design Overview

* **Backward compatibility** – The on-disk `SuperBlock` layout is unchanged; a journaled image is identified by geometry (`inode_table_block >= 3 + JOURNAL_RESERVED_BLOCKS`) and a magic word in the journal header block. Non-journaled images behave exactly as before.
* **Idempotent redo** – Every `MetadataOp` encodes an absolute post-image write or an absolute bit-set operation. Replaying a transaction any number of times yields the same filesystem state, which makes the "write WAL first, then apply in place" approach safe even if a crash occurs during the in-place phase.
* **Torn-write detection** – Each transaction frame carries a CRC32C (Castagnoli) checksum over its header and payload, plus a trailing commit marker (`0xDEAD_BEEF`). A partially written frame fails verification and is ignored during recovery.
* **Zero new dependencies** – CRC32C is implemented in-crate with a compile-time generated table, keeping the code verifiable under model-checking tools like Kani/CBMC.
* **Mandatory journaling** – Every metadata mutation must go through the journal. Bypassing the journal leaves stale transactions in the ring; replay would overwrite newer changes with old post-images, causing silent data loss (observed when a zeroed inode post-image from a stalled `create` overwrote a later write's size and block pointers).

## On-Disk Layout

The journal region occupies the first `1 + JOURNAL_RING_BLOCKS` blocks of a journaled image:

| Block ID | Contents                          |
|----------|-----------------------------------|
| 3        | Journal header block (40 bytes of state + padding) |
| 4‑35     | Record ring (32 blocks = 128 KiB) |

* `JOURNAL_HEADER_MAGIC = 0x4A52_4E4C` ("JRNL" in ASCII)
* `JOURNAL_FORMAT_VERSION = 1`
* `JOURNAL_RING_BLOCKS = 32`
* `JOURNAL_RESERVED_BLOCKS = 33` (1 header + 32 ring blocks)
* `JOURNAL_INODE_TABLE_BLOCK = 36` (first inode-table block in a journaled image)

### Journal Header (40 bytes, little-endian)

| Offset | Size | Field               | Description |
|--------|------|---------------------|-------------|
| 0      | 4    | magic               | `JOURNAL_HEADER_MAGIC` |
| 4      | 4    | version             | `JOURNAL_FORMAT_VERSION` |
| 8      | 4    | ring_blocks         | `JOURNAL_RING_BLOCKS` |
| 12     | 8    | head                | Byte offset of next append position within the record ring |
| 20     | 8    | tail                | Byte offset of the oldest transaction still needing replay |
| 28     | 8    | tx_seq              | Monotonically increasing transaction sequence number |
| 36     | 1    | cleanly_unmounted   | `1` after a clean shutdown or successful recovery |
| 37‑39  | 3    | reserved            | Zero-filled |

### Transaction Frame Layout

Each transaction is stored as a contiguous frame inside the ring buffer:

| Field          | Size | Description |
|----------------|------|-------------|
| magic          | 4 B  | `TX_MAGIC = 0x5741_4C54` ("WALT") |
| tx_seq         | 8 B  | Transaction sequence number (matches header) |
| payload_len    | 4 B  | Length of the encoded metadata-operations payload |
| crc32c         | 4 B  | CRC32C over `tx_seq || payload_len || payload` |
| payload        | *    | Serialized `MetadataOp` sequence |
| commit_marker  | 4 B  | `TX_COMMIT_MARKER = 0xDEAD_BEEF` written **after** the payload |

The CRC covers everything except the leading magic and the marker itself, allowing the checksum to be stored inline while still detecting torn writes that corrupt any part of the protected region.

## Core Abstractions

### `JournalState`

Persisted in the journal header, tracks the ring buffer pointers and hygiene flag:

* `head` – next write offset (monotonic, wraps modulo ring size)
* `tail` – oldest un-checkpointed transaction offset
* `tx_seq` – latest transaction sequence number
* `cleanly_unmounted` – set after a clean unmount or successful recovery

### `MetadataOp`

Represents a single idempotent metadata mutation. Variants:

* `SetInodeBitmap` – allocate/free an inode
* `SetDataBitmap` – allocate/free a data block
* `SetDataBitmapRange` – allocate/free a contiguous range of data blocks
* `WriteInode` – overwrite a full 256-byte inode record (post-image)
* `WriteBlockSlice` – overwrite a byte range inside a data block (used for directory entry updates)

All encoding is little-endian, self-delimiting via a leading tag byte, and bounded in size (`encoded_size`).

### `JournalRing<'a>`

A mutable view over the journal region of a memory-mapped image. Provides the main operational interface:

* `open` – attach to an existing journal region, validating the header
* `create` – format a fresh empty journal
* `append` – encode and write a transaction, advancing `head` and handling wrap-around and overwrite-oldest semantics
* `persist` – flush the header block to stable storage (caller must `msync` the header plus the last written frame range)
* `recover` – replay every valid transaction between `tail` and `head`, then reset the ring
* `checkpoint` / `checkpoint_to` – advance `tail` past transactions that have been applied in place
* `pending_transactions` – count of transactions that would be replayed
* `used` – bytes currently occupied by un-checkpointed transactions

## CRC32C Implementation and Hardware Acceleration

CRC32C (Castagnoli polynomial `0x82F6_3B78`) is computed over `tx_seq || payload_len || payload`. The implementation:

1. **Compile-time table generation** – A 256-entry lookup table is built by a `const fn`, keeping the crate dependency-free and Kani-verifiable.
2. **Software fallback** – Byte-by-byte table-driven update used when no hardware support is detected or under Kani.
3. **AArch64 `crc` extension** – On ARMv8 with the CRC extension, uses `__crc32cb` (byte) and `__crc32cd` (64-bit) intrinsics.
4. **x86-64 SSE4.2** – Uses `_mm_crc32_u8` and `_mm_crc32_u64` intrinsics when available.
5. **Runtime feature detection** – `std::arch::is_aarch64_feature_detected!("crc")` and `is_x86_feature_detected!("sse4.2")` select the optimal path at first call.

```rust
// Usage
let crc = crate::journal::crc32c(data);
let crc = crate::journal::crc_frame_checksum(tx_seq, payload_len, payload);
```

The Kani proof `proof_crc32c_detects_single_bit_flip` guarantees that any single-bit corruption (the minimal torn-write case) changes the checksum, so `decode_frame` can never accept a torn frame.

## Write-Ahead Logging Protocol

The WAL-before-metadata ordering is enforced by `DiskManager::commit_journal_tx`:

1. **Stage metadata ops** – Build a `Vec<MetadataOp>` (inode bitmap, data bitmap, inode post-images, directory slice writes).
2. **Encode a transaction frame** – `encode_frame` coalesces adjacent ops (`coalesce_ops` merges `SetDataBitmap` into `SetDataBitmapRange`, merges contiguous `WriteBlockSlice` on the same block, deduplicates `WriteInode` for the same inode), computes CRC32C, appends commit marker.
3. **Append to ring and persist header+frame** – `JournalRing::append` writes the frame contiguously (wrapping to 0 if it would split), updates `head`, `tx_seq`, and `cleanly_unmounted=false`, then persists the header.
4. **Durability-mode gated msync** – If `DurabilityMode::Strict`, the caller flushes the header block and the frame range with `msync(MS_SYNC)`. For `Lazy`, `RangeAsync`, and `LegacyWholeMmapAsync`, the WAL remains in the page cache; a later `flush()`, `flush_async()`, or `Drop` writes it back. This ensures **WAL reaches stable storage before the metadata it describes**.
5. **Apply the ops in place** – `apply_journal_ops` invokes `apply_op_in_place` for each op. Because every op is an absolute post-image assignment, this step is idempotent and safe to redo.
6. **Checkpoint to advance the tail** – After in-place writes are durable, `JournalRing::checkpoint` (or `checkpoint_to` with a snapshot of `head` taken before the flush) advances `tail := head`, discarding the replayed transactions.

<!-- openwiki: mermaid parse failed and this diagram was converted to a text fence so it does not break rendering. Fix the diagram source and restore the mermaid fence. Parser error: Parse error on line 12: ...ed; header persisted alt DurabilityM Expecting '()', 'SOLID_OPEN_ARROW', 'DOTTED_OPEN_ARROW', 'SOLID_ARROW', 'SOLID_ARROW_TOP', 'SOLID_ARROW_BOTTOM', 'STICK_ARROW_TOP', 'STICK_ARROW_BOTTOM', 'SOLID_ARROW_TOP_DOTTED', 'SOLID_ARROW_BOTTOM_DOTTED', 'STICK_ARROW_TOP_DOTTED', 'STICK_ARROW_BOTTOM_DOTTED', 'SOLID_ARROW_TOP_REVERSE', 'SOLID_ARROW_BOTTOM_REVERSE', 'STICK_ARROW_TO -->
```text
sequenceDiagram
    autonumber
    participant Op as Operation (create_file)
    participant Sim as AllocSim (staged ops)
    participant Ring as JournalRing (WAL ring buffer)
    participant Mmap as Disk Image (in-place)
    participant Sync as msync / sync_mutex

    Op->>Sim: Simulate allocations (inode, data blocks, dir entry)
    Sim-->>Op: MetadataOp list
    Op->>Ring: Append transaction frame (encode_frame)
    Note over Ring: head, tx_seq updated; header persisted
    alt DurabilityMode::Strict
        Op->>Sync: msync(MS_SYNC) header + frame range
        Note over Sync: WAL durable before metadata
    else Lazy / RangeAsync / Legacy
        Op-->>Op: Return (WAL stays in page cache)
    end
    Op->>Mmap: apply_op_in_place (inode bitmap, data bitmap, inode, dir slice)
    Note over Mmap: Idempotent redo applied
    Op->>Ring: checkpoint() or checkpoint_to(pre-flush head)
    Note over Ring: tail advanced; ring space reclaimed
    Op-->>Op: Success
```

### Coalescing (`coalesce_ops`)

Before encoding, `JournalRing::append` calls `is_already_coalesced` and, if needed, `coalesce_ops` to shrink the payload:

* **Data bitmap** – Adjacent `SetDataBitmap` (or single-block `SetDataBitmapRange`) of the same polarity are merged into one `SetDataBitmapRange`. The sort key is `(allocated, start_block)`.
* **Block slice** – Overlapping or contiguous `WriteBlockSlice` on the same `block_id` are merged into larger intervals (latest write wins on overlap).
* **Inode post-images** – Multiple `WriteInode` for the same `inode_id` keep only the last.
* **Ordering** – Phases: 1) inode bitmap, 2) data bitmap, 3) block slice, 4) inode post-images.

The fast-path check `is_already_coalesced` avoids allocations when the op list is already canonical (O(N) scan, no `BTreeMap`).

## Crash Recovery

On mount (`DiskManager::open` → `init_or_open`), if the image has a journal layout and is not cleanly unmounted:

1. `JournalRing::open` validates the header.
2. `JournalRing::recover` walks from `tail` to `head`, decoding and validating each frame (magic, length, CRC32C, commit marker). The first frame that fails validation stops replay—everything before it is durable.
3. For each valid frame, `apply_op_in_place` is called for every op in order.
4. After replay, the ring is reset (`head = tail = 0`, `cleanly_unmounted = true`) and the header is persisted.

This **Redo-only** recovery restores consistency in milliseconds without a full-image scan.

### Idempotent In-Place Application (`apply_op_in_place`)

Each `MetadataOp` variant performs bounds checking and returns `JournalError::BlockOutOfRange` for invalid block/inode IDs:

* `SetInodeBitmap` / `SetDataBitmap` – Flips a single bit in the contiguous bitmap span.
* `SetDataBitmapRange` – Uses `Bitmap::set_range` / `clear_range` on the contiguous data bitmap slice.
* `WriteInode` – Copies the 256-byte post-image into the inode table slot.
* `WriteBlockSlice` – Copies `data` into `block_id` at `offset`, bounds-checked to the block size.

Kani proofs (`proof_apply_op_*_idempotent`) verify that re-applying any op leaves the image identical.

## JBD2-Style Deferred Checkpointing under `DurabilityMode::Strict`

`DurabilityMode` (in `src/disk.rs`) controls when `msync` is called:

| Mode | Behavior |
|------|----------|
| `Lazy` (default) | No per-mutation `msync`. Page cache survives process crash; power-loss safety via explicit `flush()`, `Drop`, or kernel writeback. |
| `RangeAsync` | Batches dirty ranges in `pending_async_ranges`, drains coalesced spans (journal first, then image) outside the write lock. `msync(MS_ASYNC)` per batch. |
| `Strict` | **Synchronous `msync(MS_SYNC)` on every transaction** for the WAL header+frame. In-place metadata (bitmaps, inode table, directory blocks) stays in page cache and is **deferred** until the WAL ring reaches its **50% watermark** (`ring.used() >= ring.ring_bytes() / 2`), or on `flush()`/`Drop`. |
| `LegacyWholeMmapAsync` | Async flush of entire mmap after every mutation. |

### Checkpoint Watermark (50% Ring Used)

In `DiskManager::maybe_checkpoint` (called after each journaled mutation under `Strict`):

```rust
let needs_ckpt = {
    let ring = JournalRing::open(region, bs)?;
    ring.used() >= ring.ring_bytes() / 2   // 50% watermark
};
if needs_ckpt {
    guard.mmap.flush()?;   // flush all in-place metadata
    Self::checkpoint_journal(&mut guard.mmap, &sb)?; // advance tail
}
```

This mirrors JBD2: the WAL is the commit log (always `MS_SYNC` per transaction); the metadata blocks are data that can be batched. The 50% threshold ensures the ring never fills completely between checkpoints, avoiding the "overwrite oldest" fallback.

### Manual Checkpoint: `checkpoint_journal_now()`

`DiskManager::checkpoint_journal_now()` forces an immediate checkpoint:

1. Opens the journal ring.
2. Counts `pending_transactions()`.
3. Calls `ring.checkpoint()` (sets `tail = head`).
4. Flushes the journal header block.
5. Returns the number of discarded transactions.

Use this after an explicit `DiskManager::flush()` or when the application needs to bound recovery time.

## DurabilityMode and WAL Ordering

The WAL-before-metadata ordering is **always** maintained; `DurabilityMode` only governs *when* the WAL reaches physical storage:

* **Strict** – WAL `msync(MS_SYNC)` before the mutation returns. In-place metadata deferred to 50% watermark.
* **RangeAsync** – WAL stays in page cache; drained in batches (journal first, then coalesced image spans) under `sync_mutex` + read lock.
* **Lazy / LegacyWholeMmapAsync** – WAL written back with the rest of the mapping by `flush()`/`Drop`.

Because the WAL is *always* written before the in-place metadata in program order, a power loss after step 3 but before step 5 leaves the WAL intact; recovery replays it and re-applies the metadata.

## Journal Region Helpers

* `journal_region_start(sb)` – Absolute block offset of the journal header (block 3).
* `journal_region_len(block_size)` – `(1 + JOURNAL_RING_BLOCKS) * block_size`.
* `journal_ring_bytes(block_size)` – `JOURNAL_RING_BLOCKS * block_size` (128 KiB for 4 KiB blocks).
* `is_journaled_layout(sb)` – `sb.inode_table_block >= JOURNAL_INODE_TABLE_BLOCK`.
* `DiskManager::journal_region(mmap, sb)` – Returns a mutable slice of the journal region for `JournalRing::open/create`.

## Formal Verification (Kani/CBMC)

The journal subsystem ships with Kani proofs covering:

* **Ring arithmetic** – `proof_ring_cursor_advance_stays_in_ring`, `proof_ring_used_invariants`, `proof_ring_append_bounds_guarantee`.
* **Checkpoint monotonicity** – `proof_checkpoint_empties_ring`, `proof_ring_checkpoint_to_monotonicity`.
* **CRC32C integrity** – `proof_crc32c_detects_single_bit_flip`, `proof_crc_input_is_deterministic`.
* **Frame decoding safety** – `proof_frame_length_overflow_rejected`.
* **Op bounds** – `proof_block_slice_offset_checked`.
* **Idempotent redo** – `proof_apply_op_set_inode_bitmap_idempotent`, `proof_apply_op_set_data_bitmap_idempotent`, `proof_apply_op_write_block_slice_idempotent`, `proof_apply_op_write_inode_idempotent`, `proof_apply_op_set_data_bitmap_range_idempotent`.
* **Coalescing equivalence** – `proof_apply_op_set_data_bitmap_range_equiv_single_ops`.
* **Encode/decode round-trips** – `proof_journal_state_encode_decode_roundtrip`, `proof_metadata_op_encode_decode_roundtrip`.
* **Push helpers** – `proof_push_data_bitmap_op_polarity_never_merges`, `proof_push_data_bitmap_op_coalesces_adjacent_same_polarity`.

These proofs guarantee absence of arithmetic overflow, out-of-bounds access, and logical errors in the recovery path.

## Integration Points

* **`DiskManager::create_entry_journaled`** – Builds ops for inode allocation, optional directory data block, directory entry insertion, and inode post-images; calls `commit_journal_tx` → `apply_journal_ops` → `maybe_checkpoint`.
* **`DiskManager::delete_file` / `write_data_with_filters`** – Stage through `AllocSim`, commit a transaction, apply, checkpoint.
* **`defragment_inplace_journaled_cow`** – Records every block move as a WAL transaction; checkpoints after each round (or at 50% watermark under `Strict`).
* **`PendingAsyncRanges`** – Accumulates dirty spans for `RangeAsync`; drained by `maybe_drain_async_ranges` (journal first, then coalesced spans with 1 MiB gap bridging).
* **`sync_mutation_ranges`** – Central `msync` dispatcher; under `Strict` coalesces with 256 KiB gap bridging to merge bitmap and inode-table flushes across the journal region.

## Error Variants (`JournalError`)

* `BadMagic` – Header magic mismatch (not a journaled image).
* `RegionTooSmall` – Journal region insufficient for the layout.
* `TransactionTooLarge` – Single transaction exceeds ring capacity.
* `CorruptFrame` – CRC/validation failure at a specific ring offset.
* `BlockOutOfRange` – Metadata op targets an out-of-range block or inode.

## Configuration Constants

| Constant | Value | Description |
|----------|-------|-------------|
| `JOURNAL_RING_BLOCKS` | 32 | Ring buffer blocks (128 KiB at 4 KiB block size) |
| `JOURNAL_HEADER_LEN` | 40 | Persistent header size in bytes |
| `TX_HEADER_LEN` | 20 | Transaction frame header size |
| `INODE_BYTES` | 256 | Inode record size |
| `RANGE_ASYNC_BATCH_OPS` | 64 | Mutations before `RangeAsync` batch drain |
| `RANGE_ASYNC_BATCH_BYTES` | 4 MiB | Dirty bytes before `RangeAsync` batch drain |
| `RANGE_ASYNC_GAP_BRIDGE_BYTES` | 1 MiB | Max clean-page gap bridged during `RangeAsync` coalescing |
| `STRICT_GAP_BRIDGE_BYTES` | 256 KiB | Max clean-page gap bridged during `Strict` metadata coalescing |
| Checkpoint watermark | 50% | `ring.used() >= ring.ring_bytes() / 2` triggers deferred checkpoint |

## Related Pages

* [Block Allocation](../architecture/block_allocation.md) – Bitmap operations recorded in the journal
* [Disk Manager and Persistence](../architecture/disk_manager_and_persistence.md) – DurabilityMode, msync, and the write pipeline
* [Testing and Verification](../operations/testing_and_verification.md) – Kani proofs and fault-injection tests
* [FSCK Workflow](../workflows/fsck_workflow.md) – Recovery vs. full fsck
