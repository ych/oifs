---
type: concept
title: Journal and Write-Ahead Logging
description: Describes the transactional metadata WAL (Write-Ahead Log) that ensures crash resilience, including its circular buffer design, replay mechanism, and durability modes.
tags: ["journal", "WAL", "write-ahead logging", "transaction", "recovery", "durability"]
sources:
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-14T13:26:03.589Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-19T15:22:28.053Z
---

# Journal and Write-Ahead Logging

The journal subsystem implements a write-ahead log (WAL) for metadata mutations in OIFS. Before any metadata change (e.g., `create_file`, `delete_file`, `mkdir`, inode updates) is applied to the filesystem image, it is first recorded as a checksummed transaction in a fixed-size circular ring buffer residing in the journal region on disk. On crash recovery, the journal replays all durable-but-unapplied transactions to restore a consistent state, eliminating the need for a full `fsck` scan.

## Design Overview

* **Backward compatibility** – The on-disk `SuperBlock` layout is unchanged; a journaled image is identified by geometry (`inode_table_block >= 3 + JOURNAL_RESERVED_BLOCKS`) and a magic word in the journal header block.
* **Idempotent redo** – Every `MetadataOp` encodes an absolute post‑image write or an absolute bit‑set operation. Replaying a transaction any number of times yields the same filesystem state, which makes the “write WAL first, then apply in place” approach safe even if a crash occurs during the in‑place phase.
* **Torn‑write detection** – Each transaction frame carries a CRC32C (Castagnoli) checksum over its header and payload, plus a trailing commit marker (`0xDEAD_BEEF`). A partially written frame fails verification and is ignored during recovery.
* **Zero new dependencies** – CRC32C is implemented in‑cube with a compile‑time generated table, keeping the code verifiable under model‑checking tools like Kani/CBMC.
* **Mandatory journaling** – Every metadata mutation must go through the journal. Bypassing the journal leaves stale transactions in the ring; replay would overwrite newer changes with old post‑images, causing silent data loss (observed when a zeroed inode post‑image from a stalled `create` overwrote a later write’s size and block pointers).

## On‑Disk Layout

The journal region occupies the first `1 + JOURNAL_RING_BLOCKS` blocks of a journaled image:

| Block ID | Contents                          |
|----------|-----------------------------------|
| 3        | Journal header block (40 bytes of state + padding) |
| 4‑35     | Record ring (32 blocks)           |

* `JOURNAL_HEADER_MAGIC = 0x4A52_4E4C` (“JRNL” in ASCII)
* `JOURNAL_FORMAT_VERSION = 1`
* `JOURNAL_RING_BLOCKS = 32`
* `JOURNAL_RESERVED_BLOCKS = 33` (1 header + 32 ring blocks)
* `JOURNAL_INODE_TABLE_BLOCK = 36` (first inode‑table block in a journaled image)

### Journal Header (40 bytes, little‑endian)

| Offset | Size | Field               | Description |
|--------|------|---------------------|-------------|
| 0      | 4    | magic               | `JOURNAL_HEADER_MAGIC` |
| 4      | 4    | version             | `JOURNAL_FORMAT_VERSION` |
| 8      | 4    | ring_blocks         | `JOURNAL_RING_BLOCKS` |
| 12     | 8    | head                | Byte offset of next append position within the record ring |
| 20     | 8    | tail                | Byte offset of the oldest transaction still needing replay |
| 28     | 8    | tx_seq              | Monotonically increasing transaction sequence number |
| 36     | 1    | cleanly_unmounted   | `1` after a clean shutdown or successful recovery |
| 37‑39  | 3    | reserved            | Zero‑filled |

### Transaction Frame Layout

Each transaction is stored as a contiguous frame inside the ring buffer:

| Field          | Size | Description |
|----------------|------|-------------|
| magic          | 4 B  | `TX_MAGIC = 0x5741_4C54` (“WALT”) |
| tx_seq         | 8 B  | Transaction sequence number (matches header) |
| payload_len    | 4 B  | Length of the encoded metadata‑operations payload |
| crc32c         | 4 B  | CRC32C over `tx_seq || payload_len || payload` |
| payload        | *    | Serialized `MetadataOp` sequence |
| commit_marker  | 4 B  | `TX_COMMIT_MARKER = 0xDEAD_BEEF` written **after** the payload |

The CRC covers everything except the leading magic and the marker itself, allowing the checksum to be stored inline while still detecting torn writes that corrupt any part of the protected region.

## Core Abstractions

### `JournalState`

Persisted in the journal header, tracks the ring buffer pointers and hygiene flag:

* `head` – next write offset (monotonic, wraps modulo ring size)
* `tail` – oldest un‑checkpointed transaction offset
* `tx_seq` – latest transaction sequence number
* `cleanly_unmounted` – set after a clean unmount or successful recovery

### `MetadataOp`

Represents a single idempotent metadata mutation. Variants:

* `SetInodeBitmap` – allocate/free an inode
* `SetDataBitmap` – allocate/free a data block
* `SetDataBitmapRange` – allocate/free a contiguous range of data blocks
* `WriteInode` – overwrite a full 256‑byte inode record (post‑image)
* `WriteBlockSlice` – overwrite a byte range inside a data block (used for directory entry updates)

All encoding is little‑endian, self‑delimiting via a leading tag byte, and bounded in size (`encoded_size`).

### `JournalRing<'a>`

A mutable view over the journal region of a memory‑mapped image. Provides the main operational interface:

* `open` – attach to an existing journal region, validating the header
* `create` – format a fresh empty journal
* `append` – encode and write a transaction, advancing `head` and handling wrap‑around and overwrite‑oldest semantics
* `persist` – flush the header block to stable storage (caller must `msync` the header plus the last written frame range)
* `recover` – replay every valid transaction from `tail` to `head`, applying each `MetadataOp` via a caller‑supplied function; stops at the first frame that fails validation; resets the ring and marks the image cleanly unmounted
* `checkpoint` / `checkpoint_to` – advance `tail` to discard transactions that have been safely applied in place; must be called only after the in‑place bytes are durable

## Write‑Ahead Logging Protocol

1. **Stage** – Collect metadata mutations into a `Vec<MetadataOp>`.
2. **Encode** – Build a transaction frame (`encode_frame`) comprising magic, `tx_seq`, payload length, CRC32C, payload, and commit marker.
3. **Append** – `JournalRing::append` writes the frame to the record ring at `head`, wrapping if necessary, and advances `head`. If the new frame would overwrite un‑checkpointed transactions, `tail` is moved forward (overwrite‑oldest). The ring state (`head`, `tx_seq`, `cleanly_unmounted=false`) is persisted to the header block.
4. **Durability** – The durability layer (outside the journal) must `msync`:
   * the journal header block, and
   * the exact byte range returned by `JournalRing::last_write` (the frame just appended).
   Only after this flush is the transaction considered committed.
5. **Apply** – Perform the metadata mutations in place on the filesystem image.
6. **Checkpoint** – Once the in‑place writes are durable, call `JournalRing::checkpoint` (or `checkpoint_to` with a pre‑flush snapshot of `head`) to advance `tail` and free the ring space.
7. **Clean unmount** – Before yielding the disk, flush the header with `cleanly_unmounted=true`.

## Crash Recovery

On mount, if `journal_state.cleanly_unmounted == false`, the recovery procedure runs:

1. Instantiate a `JournalRing` over the journal region.
2. Call `JournalRing::recover` with a closure that applies each `MetadataOp` via `apply_op_in_place`.
3. Replay proceeds forward from `tail` to `head`, stopping at the first frame that fails magic, length, CRC, or commit‑marker validation (torn write or corruption). All frames before that point are considered durable and are reapplied.
4. After replay, the ring is reset (`head = tail = 0`) and the header is updated with `cleanly_unmounted = true`.
5. Normal mount continues; the filesystem is now in a consistent state.

Because each `MetadataOp` is an absolute post‑image operation, replay is safe and idempotent: applying the same transaction twice produces the same result as applying it once.

## In‑Place Application (`apply_op_in_place`)

The redo primitive performs the actual metadata updates:

* **Bitmap ops** – locate the relevant bitmap block (inode or data) and flip the bit for the target ID.
* **WriteInode** – locate the inode record in the inode table and copy the full 256‑byte post‑image.
* **WriteBlockSlice** – locate the data block and copy the provided byte slice at the given offset.

All operations validate that the target block lies within the image bounds; otherwise they return `JournalError::BlockOutOfRange`.

## Error Handling

* `JournalError::BadMagic` – journal header missing or malformed (non‑journaled image).
* `JournalError::RegionTooSmall` – journal region smaller than required (header + ring).
* `JournalError::TransactionTooLarge` – a single transaction exceeds the ring buffer capacity.
* `JournalError::CorruptFrame` – CRC32C or structural validation failure (torn write or corruption).
* `JournalError::BlockOutOfRange` – metadata operation targets a block outside the image.

## Relationships with Other Components

* **SuperBlock** – provides geometry constants (`block_size`, `inode_table_block`, bitmap block IDs, data block start) used by the journal during encoding, decoding, and in‑place application.
* **Disk manager / persistence layer** – responsible for issuing the actual `msync`/`fdatasync` calls that make the header and the last‑written frame range durable. The journal itself only manages in‑memory ring state and persists the header block.
* **Concurrency and session layer** – holds the write lock while a transaction is appended, applied, and checkpointed, ensuring that only one thread mutates the journal at a time.
* **Integrity checking (`fsck`)** – becomes unnecessary for metadata consistency because the journal guarantees recoverability; a background `fsck` may still run to verify structural invariants or scan for orphaned extents, but it does not need to replay the journal.

## Configuration and Operation

* Journaling is enabled automatically when the filesystem is formatted with `inode_table_block >= 3 + JOURNAL_RESERVED_BLOCKS`. No mount‑time flag is required.
* The ring size is fixed at 32 blocks (typically 128 KiB with 4 KiB blocks). This bounds the maximum metadata burst that can be checkpointed before older transactions must be discarded.
* There are no runtime tunables; the design prioritizes simplicity and deterministic behavior.

## Testing

Unit tests in `src/journal.rs` exercise:

* Header encoding/decoding and validation of magic, version, and geometry.
* CRC32C correctness against known test vectors.
* Transaction frame encoding/decoding, including wrap‑around, overwrite‑oldest, and torn‑write detection.
* Ring buffer accounting (`used`, `pending_transactions`).
* Full append → apply → checkpoint → recover cycles for each `MetadataOp` variant.
* Edge cases: maximal‑sized transaction, zero‑length payload, sequential wraps, and power‑loss simulation during frame write.

These tests validate that the journal satisfies its crash‑recovery guarantee under the stated assumptions.
