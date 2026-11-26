---
type: "Reference"
title: "Metadata WAL / Journaling"
openwiki_generated: true
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
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
| 4‑35     | Record ring (32 blocks = 128 KiB at 4 KiB block size) |

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
* `append` – write a transaction frame, wrapping the ring cursor and optionally overwriting the oldest uncheckpointed transactions
* `try_append_without_overwrite` – append only if it would not overwrite uncheckpointed entries; used by `Strict` durability
* `persist` – flush the updated `JournalState` to the header block
* `recover` – replay all valid frames from `tail` to `head`, stopping at the first invalid frame
* `checkpoint` / `checkpoint_to` – advance `tail` to discard transactions already applied in place
* `mark_clean_shutdown` – reset ring and set `cleanly_unmounted`

## CRC32C (Castagnoli) Protection

The CRC32C implementation uses a compile-time generated 256-entry lookup table with the Castagnoli polynomial (`0x82F6_3B78`). Hardware acceleration is used when available:

* **AArch64**: `crc32c` instruction via `__crc32cb` / `__crc32cd`
* **x86_64**: SSE4.2 `crc32` instruction via `_mm_crc32_u8` / `_mm_crc32_u64`
* **Software fallback**: pure-Rust table-driven implementation

The checksum is computed over the concatenation of `tx_seq` (8 bytes LE), `payload_len` (4 bytes LE), and the payload bytes. The frame magic and the CRC field itself are excluded so the checksum can be stored inline.

`decode_frame` validates:
1. Frame magic equals `TX_MAGIC`
2. Declared payload length does not overrun the buffer
3. CRC32C matches the computed value
4. Trailing commit marker equals `TX_COMMIT_MARKER`

Any failure rejects the frame, making torn writes impossible to replay.

## Write Path: Staging, Commit, Apply, Checkpoint

The journaled mutation protocol follows five phases:

1. **Stage metadata ops** – Callers (e.g., `create_entry_journaled`, `delete_file_journaled`, `write_data_journaled`) build a vector of `MetadataOp` using `AllocSim` to simulate allocations against copied bitmaps. This produces deterministic post-images before any on-disk modification.
2. **Encode transaction frame** – `encode_frame` coalesces adjacent operations (`SetDataBitmap` → `SetDataBitmapRange`, contiguous `WriteBlockSlice` merges, duplicate `WriteInode` dedup), computes CRC32C, and appends the commit marker.
3. **Append to ring and persist header+frame** – `JournalRing::append` (or `try_append_without_overwrite` under `Strict`) writes the frame contiguously, wraps the cursor with zero-padding if needed, updates `head` and `tx_seq`, persists the header, and flushes the written range plus header to stable storage. Under `Strict` durability, if the ring is full, the caller flushes all prior in-place metadata and checkpoints before retrying.
4. **Apply ops in place** – `apply_journal_ops` iterates the ops and calls `apply_op_in_place` for each, which performs the absolute post-image write (bitmap bit set/clear, inode slot overwrite, block slice copy). Caches for affected inodes are invalidated and generations bumped.
5. **Checkpoint** – `maybe_checkpoint` advances `tail` to `head` once the in-place writes are durable. Under `Strict` mode this happens eagerly when the ring reaches 50% capacity (JBD2-style deferred checkpointing). Under async modes the ring is retained for mount-time recovery.

## Crash Recovery Flow

On mount, `DiskManager::open` invokes `recover_journal`:

1. **Snapshot the journal region** – Read the entire journal region (header + ring) into a local buffer to avoid holding the write lock during decode.
2. **Decode and validate frames** – `JournalRing::recover` walks from `tail` to `head`, calling `decode_frame` on each. Decoding stops at the first frame that fails validation (bad magic, length overflow, CRC mismatch, or missing commit marker). All frames before the failure are valid and durable.
3. **Redo phase** – Collected ops are replayed against the live image via `ImageBacking::apply_journal_op`, which mirrors `apply_op_in_place` but operates on the actual memory mapping.
4. **Flush replayed metadata** – If any transactions were replayed, a full `flush()` ensures the redone metadata reaches stable storage **before** the WAL header is reset.
5. **Reset ring** – Write a clean `JournalState` (`head=0, tail=0, cleanly_unmounted=true`) to the header block and flush it. Subsequent mounts see a clean journal.

Recovery is idempotent: replaying a transaction twice is indistinguishable from once because every op is an absolute post-image.

## Durability Modes and Group Commit

`DurabilityMode` controls the persistence guarantees:

| Mode | Journal Flush | In-Place Flush | Checkpointing |
|------|---------------|----------------|---------------|
| `Strict` | Synchronous `msync` per transaction (or batched via drainer) | Deferred; batched at 50% ring watermark or explicit `flush` | Eager at 50% ring usage |
| `Lazy` | No per-transaction barrier; relies on OS writeback | No explicit flush; relies on OS writeback | Deferred to mount-time recovery |
| `RangeAsync` | Batched async `msync` of mutation ranges | Batched async with journal | Deferred to mount-time recovery |
| `LegacyWholeMmapAsync` | Whole-file async `msync` | Whole-file async `msync` | Deferred to mount-time recovery |

### Group Commit under `Strict`

When `is_strict_sync_deferred()` returns true (configured drainer thread active), individual `commit_journal_tx` calls skip the final `flush_range` for their frame. Instead, a background drainer collects pending futures and performs a single unified `flush()` barrier before completing the batch. This amortizes the `msync` cost across multiple transactions while preserving the WAL-before-data ordering.

## Deferred Checkpointing (JBD2-Style)

Under `Strict` durability, the WAL ring acts as the durability frontier. In-place metadata blocks (inodes, bitmaps, directory blocks) stay in the OS page cache after `apply_journal_ops` and are flushed in one batch when:

* The ring reaches 50% capacity (`used() >= ring_bytes() / 2`), or
* The user calls `flush()` / `flush_async()`, or
* The filesystem is unmounted cleanly.

The checkpoint operation (`checkpoint_journal`) performs:
1. Full `flush()` of the backing store (ensures all in-place metadata since last checkpoint is durable)
2. `JournalRing::checkpoint()` which sets `tail = head`
3. Header flush to persist the new `tail`

This is safe because the WAL still contains every transaction since the last checkpoint; if a crash occurs after step 1 but before step 3, mount-time recovery will replay from the old `tail` and re-apply the already-durable in-place writes (idempotent). The ring never grows without bound because `append` overwrites oldest entries when full, and checkpointing reclaims space.

## Allocation Staging with `AllocSim`

Journaled create/delete/write paths must know every allocated block/inode **before** committing to the WAL. `AllocSim` provides a local simulation of the block and inode bitmaps:

* Copies the inode bitmap and (lazily) the data bitmap from the live image
* Mirrors `SimpleBlockAllocator` semantics exactly (same hint handling, same first-free-bit rule)
* Tracks `fresh_blocks` allocated during the transaction for later zeroing and sync ranges
* Supports `alloc_inode`, `free_inode`, `alloc_block`, `alloc_contiguous`, and pointer simulation for indirect blocks

`AllocSim::new_data_only` avoids the inode bitmap copy for operations that only allocate/free data blocks (`write_data`, `truncate`, etc.).

## Integration with DiskManager

* **`DiskManagerInner::commit_journal_tx`** – central commit point; handles `Strict` overwrite logic, header persistence, and flush ordering.
* **`DiskManagerInner::apply_journal_ops`** – redo path used by both commit-time apply and mount-time recovery.
* **`DiskManagerInner::recover_journal`** – invoked during `DiskManager::open` before publishing the manager.
* **`DiskManagerInner::maybe_checkpoint`** – called after each journaled mutation under `Strict`; drives deferred checkpointing.
* **`DiskManager::checkpoint_journal_now`** – public API to force a checkpoint and return the number of discarded transactions.
* **`sync_journal_region`** – writes the entire journal region (header + ring) in order, used by `flush`/`flush_async` to preserve WAL-before-data ordering for transactions that skipped their own barrier.

## Invariants and Failure Semantics

* **Ring cursor arithmetic** – `head` and `tail` are always modulo `ring_bytes`; `used() = (head + ring - tail) % ring` is strictly less than `ring_bytes`.
* **Frame contiguity** – Frames are never split across the wrap point; `append` zero-pads the tail and wraps `head` to 0.
* **Checkpoint monotonicity** – `checkpoint_to` only advances `tail` forward toward `head` (measured by ring distance), never backwards.
* **Recovery atomicity** – Header reset occurs **after** replayed metadata flush, so a crash during recovery never loses replayed transactions.
* **Stale transaction hazard** – Any unjournaled metadata mutation on a journaled image invalidates the WAL; the next recovery would replay stale post-images over newer state. All metadata paths must use the journal or drain it first.

## Testing and Formal Verification

The journal subsystem includes extensive Kani proofs covering:

* CRC32C detects every single-bit flip
* Frame length overflow is rejected before out-of-bounds reads
* `apply_op_in_place` is strictly idempotent for all `MetadataOp` variants
* `JournalState` encode/decode round-trip and corruption rejection
* `MetadataOp` encode/decode round-trip and size agreement
* Ring cursor arithmetic invariants (no overflow, wrap correctness)
* `checkpoint`/`checkpoint_to` monotonicity and ring-emptying
* `push_data_bitmap_op` and `push_block_slice_op` coalescing correctness
* Coalesced `SetDataBitmapRange` equivalence to individual `SetDataBitmap` ops

Property-based tests exercise wrap-around survival, torn-frame detection, checkpoint/recovery interactions, and frame ordering guarantees.
