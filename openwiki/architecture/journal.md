---
type: concept
title: Journaling and Write-Ahead Log
description: Document the transactional metadata Write-Ahead Log (WAL) that provides crash resilience and durability, including journal handling, recovery, and durability policies.
tags: [filesystem, journaling, wal, crash-recovery, durability]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-09T15:04:29.410Z
sources:
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-09T15:04:29.410Z" }
---

# Journaling and Write-Ahead Log

The OIFS filesystem uses a metadata Write-Ahead Log (WAL) to ensure crash consistency and durability for metadata updates. When journaling is enabled, every metadata mutation (such as file creation, deletion, or directory updates) is first recorded as a checksummed transaction in a fixed-size circular ring buffer, flushed to stable storage, and only then applied in place. On the next mount after a crash, durable-but-unapplied transactions are automatically replayed (Redo) to restore a consistent state without requiring a full filesystem check.

## Design notes

The journaling subsystem is designed with the following properties:

* **100% backward compatible**: The [`SuperBlock`] structure remains unchanged. A journaled image is distinguished solely by its geometry (`inode_table_block >= 3 + JOURNAL_RESERVED_BLOCKS`) and a magic word in the journal header block. Non-journaled images behave exactly as before.
* **Idempotent redo**: Every [`MetadataOp`] is a *post-image* write or an absolute bit set, never a relative delta. Replaying a transaction any number of times yields the same filesystem state, making "write WAL first, then apply in place" safe even if a crash occurs during the in-place phase.
* **Torn-write detection**: Each frame carries a CRC32C (Castagnoli) checksum over its header body and payload, plus a trailing commit marker. A partially written frame fails verification and is discarded during recovery.
* **No new dependencies**: CRC32C is implemented in-crate with a `const fn` generated table, ensuring verifiability under Kani/CBMC.
* **Every metadata mutation must be journaled**: Paths that modify metadata (like `create_file`, `mkdir`, `delete_file`, and `write_data`) stage changes through an allocation simulator and commit a transaction. Any future metadata-mutating path must follow the same pattern or drain the journal ring first to avoid stale transactions being replayed over newer changes.

## Journal geometry

A journaled image reserves blocks for the journal region at the beginning of the filesystem:

| Component               | Block ID (absolute) | Size (blocks) |
|-------------------------|---------------------|---------------|
| Journal header block    | `JOURNAL_HEADER_BLOCK = 3` | 1             |
| Journal record ring     | `4` through `3 + JOURNAL_RING_BLOCKS - 1` | `JOURNAL_RING_BLOCKS = 32` |
| First inode-table block | `JOURNAL_INODE_TABLE_BLOCK = 3 + JOURNAL_RESERVED_BLOCKS` | — |

The journal header block contains a persistent [`JournalState`] structure. The record ring follows immediately after and occupies `JOURNAL_RING_BLOCKS` blocks. The total space reserved for journaling is `JOURNAL_RESERVED_BLOCKS = 1 + JOURNAL_RING_BLOCKS` blocks.

## Persistent journal state

The [`JournalState`] structure, stored in the journal header block, tracks the ring buffer's state:

* `head`: Byte offset of the next append position within the record ring.
* `tail`: Byte offset of the oldest transaction still needing replay.
* `tx_seq`: Monotonically increasing transaction sequence number.
* `cleanly_unmounted`: Flag set to `true` after a clean shutdown or successful recovery.

The state is persisted to disk via the [`JournalRing::persist`] method, which writes the encoded state to the journal header block.

## Metadata operations

Each metadata mutation is represented as a [`MetadataOp`] variant, designed to be idempotent:

* [`SetInodeBitmap`]: Allocate or free an inode by setting/clearing its bit in the inode bitmap.
* [`SetDataBitmap`]: Allocate or free a data block by setting/clearing its bit in the data bitmap.
* [`WriteInode`]: Overwrite an entire 256-byte inode record with a post-image.
* [`WriteBlockSlice`]: Overwrite a byte range inside a data block (used for directory entry updates).

Operations are encoded in a self-delimiting, little-endian format with a leading tag byte. The encoded size of each operation is precomputed for buffer sizing.

## Transaction frame format

A transaction frame consists of:

| Field           | Size (bytes) | Description                                                                 |
|-----------------|--------------|-----------------------------------------------------------------------------|
| Magic (`WALT`)  | 4            | Fixed value `0x5741_4C54` identifying a transaction frame start.           |
| Transaction seq | 8            | Monotonic sequence number from [`JournalState::tx_seq`].                   |
| Payload length  | 4            | Length of the encoded metadata operations payload in bytes (little-endian). |
| CRC32C          | 4            | Castagnoli CRC of `tx_seq || payload_len || payload`.                      |
| Payload         | *variable*   | Concatenated encoded [`MetadataOp`] instances.                              |
| Commit marker   | 4            | Fixed value `0xDEAD_BEEF` written only after the payload is fully stored.  |

The CRC32C covers the transaction sequence, payload length, and payload, allowing detection of torn writes. The commit marker ensures the frame is complete.

## Journal ring buffer

The [`JournalRing`] structure manages the circular record ring:

* **Appending a transaction** ([`JournalRing::append`]):
  1. Increments the transaction sequence number.
  2. Encodes the operations into a frame.
  3. Checks that the frame fits within the ring buffer; returns [`JournalError::TransactionTooLarge`] if not.
  4. Wraps the head pointer if the frame would exceed the ring's end.
  5. Advances the tail pointer to drop older transactions if the ring would overwrite un-checkpointed data (safe due to idempotent replay).
  6. Copies the frame into the ring buffer at the current head position.
  7. Updates the head pointer and persists the journal state to disk.

* **Persisting state** ([`JournalRing::persist`]):
  Writes the current [`JournalState`] to the journal header block, ensuring durability of head, tail, sequence number, and clean unmount flag.

* **Recovery** ([`JournalRing::recover`]):
  Replays transactions from `tail` to `head`:
  1. Iterates over frames in the ring buffer, decoding and validating each.
  2. For each valid frame, applies its metadata operations in order via the provided `apply` callback.
  3. Stops at the first frame that fails validation (torn write or corruption); all prior frames are considered durable and are replayed.
  4. After replay, resets the ring buffer (`head = tail = 0`) and marks the filesystem as cleanly unmounted.

* **Checkpointing** ([`JournalRing::checkpoint`] and [`JournalRing::checkpoint_to`]):
  Advances the `tail` pointer to the current `head`, discarding all transactions that have been applied in place. This frees space in the ring buffer for new transactions. Checkpointing should only be called after the in-place bytes are durable (e.g., after an `msync`).

* **Durability tracking** ([`JournalRing::last_write`] and [`JournalRing::journal_byte_ranges`]):
  The durability layer uses these to determine which ranges to flush via `msync`:
  * The journal header block (always flushed on state change).
  * The exact byte range of the most recently appended transaction (from [`JournalRing::last_write`]).

## Relationship with the disk manager

Metadata operations in the disk manager (e.g., [`DiskManager::create_file`], [`DiskManager::delete_file`], [`DiskManager::create_directory`]) are staged through an allocation simulator and committed as a journal transaction before being applied in place. This ensures that:

1. The transaction is durable on disk before any in-place modification occurs.
2. If a crash occurs before the in-place update, the transaction will be replayed on remount.
3. If a crash occurs during the in-place update, the transaction is idempotent and safe to replay.

Any new metadata-mutating operation must follow the same pattern: record the intended changes as [`MetadataOp`] instances, encode them into a transaction, append to the journal, persist the journal state, then apply the changes in place.

## Configuration and enabling

Journaling is enabled implicitly by the filesystem geometry at mkfs time. The superblock's `inode_table_block` field must be at least `JOURNAL_INODE_TABLE_BLOCK` (typically block 35 for 4KiB blocks: 1 header + 32 ring blocks + 2 superblock + descriptor blocks). Non-journaled images use `inode_table_block = 3`.

The journal subsystem does not require runtime configuration; it is activated automatically when the layout indicates a journaled image (see [`is_journaled_layout`]).

## Failure modes and invariants

* **Transaction too large**: A single transaction exceeding the ring buffer size (`JOURNAL_RING_BLOCKS * block_size`) is rejected at append time.
* **Torn write detection**: A frame with invalid CRC32C or missing commit marker is treated as corrupt and stops recovery; prior frames are replayed.
* **Ring buffer overwrite**: When the ring buffer is full, appending a new transaction advances the tail to drop the oldest transaction. This is safe only if the oldest transaction has been checkpointed (i.e., applied in place and made durable).
* **Clean unmount flag**: The `cleanly_unmounted` flag in [`JournalState`] is set to `true` only after a successful checkpoint or recovery. On mount, if the flag is `false`, recovery is performed.

## Testing and verification

The journal subsystem includes:
* Unit tests for encoding/decoding of [`MetadataOp`] and transaction frames.
* Property-based tests for ring buffer append, wrap, and checkpoint logic.
* Kani proofs for critical invariants (e.g., ring cursor advances stay within bounds, checkpointing empties the ring).
* Integration tests that simulate power loss during various operations and verify recovery consistency.

## Related components

<!-- openwiki: broken internal link [../disk_manager_and_persistence.md#superblock] file "../disk_manager_and_persistence.md" does not exist. Fix the href or restore the target, then delete this comment. -->
* [**SuperBlock**](../disk_manager_and_persistence.md#superblock): Contains filesystem geometry; journaled layout detected by `inode_table_block` position.
<!-- openwiki: broken internal link [../disk_manager_and_persistence.md] file "../disk_manager_and_persistence.md" does not exist. Fix the href or restore the target, then delete this comment. -->
* [**Disk manager**](../disk_manager_and_persistence.md): Coordinates metadata operations and journaling via the [`JournalRing`].
<!-- openwiki: broken internal link [../concurrency_and_session.md] file "../concurrency_and_session.md" does not exist. Fix the href or restore the target, then delete this comment. -->
* [**Concurrency and session**](../concurrency_and_session.md): Journaling interacts with locking; metadata mutations require holding appropriate locks during the journal append and in-place apply phases.
* [**Fsck workflow**](../workflows/fsck_workflow.md): Journaling reduces the need for fsck; recovery handles most crash consistency cases automatically.
