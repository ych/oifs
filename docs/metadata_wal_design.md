# OIFS Metadata WAL (Write-Ahead Logging / Journaling) Specification

This document describes the on-disk layout, transaction model, durability modes, and crash-recovery guarantees of the opt-in **Metadata WAL** implemented in `src/journal.rs` and `src/disk.rs`.

---

## 1. Design Goals

Metadata mutations (`create_file`, `create_file_with_data`, `mkdir`, `delete_file`, `rename`, `write_data`, `truncate`, `defragment`) update multiple disjoint structures: inode bitmap, data bitmap, inode slots, directory blocks, and indirect pointer blocks. Without a journal, a crash mid-mutation can leave leaked blocks or orphan inodes requiring an offline `oifs fsck`.

The Metadata WAL provides:
- **Opt-in zero-overhead compatibility**: Enabled at image creation (`--journal` or `DiskManager::open_with_options(..., journaled = true, ...)`). Non-journaled images reserve zero WAL blocks and execute direct in-place updates.
- **All-or-nothing crash atomicity**: Every multi-block metadata mutation is staged via `AllocSim` without touching the live image, committed as a single CRC32C-checksummed transaction (`commit_journal_tx`), and then applied in place (`apply_journal_ops`).
- **Ordered-mode payload safety**: Payload data blocks are written and flushed *before* the metadata commit (except small `<= 8 KiB` payloads in `create_file_with_data` under `DurabilityMode::Strict`, which are data-journaled inline inside the WAL commit to reduce sync barriers from 3 to 1).
- **Fast idempotent redo recovery**: At mount time, committed transactions in the ring buffer are replayed idempotently in milliseconds; torn or CRC-invalid tail records are cleanly ignored.
- **Strict read-only safety**: `DiskManager::open_read_only*` verifies WAL cleanliness without writing a single byte and refuses unrecovered dirty journals rather than mutating the image.

---

## 2. On-Disk Layout (Format v3, 4096-Byte Blocks)

```text
+-------------------+--------------------+-------------------+---------------------+-------------------------+
| Block 0           | Blocks 1..I        | Blocks I+1..D     | [Optional WAL]      | Inode Table -> Data     |
| SuperBlock (4KB)  | Inode Bitmap       | Data Bitmap       | 33 Blocks (1 + 32)  | 256B Slots -> 4KB Data  |
+-------------------+--------------------+-------------------+---------------------+-------------------------+
```

- **Dynamic v3 bitmaps**: Inode and data bitmap sizes scale with image capacity (1 block tracks 32,768 entries).
- **WAL region (33 blocks = 132 KiB)** placed between the data bitmap and inode table when enabled:
  - **1 Header Block (4 KiB)**: Magic, version, `head_seq`, `tail_offset`, ring geometry, and CRC32C.
  - **32 Ring Blocks (128 KiB)**: Circular byte buffer of length-prefixed, CRC32C-verified `Transaction` frames containing coalesced `MetadataOp` records (`SetInodeBitmap`, `SetDataBitmap`, `SetDataBitmapRange`, `WriteBlockSlice`, `WriteInode`).
- **Write-through `FileIo` efficiency**: `JournalRegionMut` tracks exact dirty byte ranges and `pwrite`s only the modified header bytes and newly appended ring frame rather than rewriting all 132 KiB.

---

## 3. Transaction Execution & Checkpointing

Every journaled mutation follows a strict 3-phase protocol:
1. **Compute & Stage (no live mutation)**: Simulate any new allocations through `AllocSim`, build post-images of modified directory/indirect blocks and inodes, and coalesce contiguous bitmap ops (`coalesce_ops`).
2. **Commit Point (`commit_journal_tx`)**: Append the CRC32C-checksummed frame to the WAL ring and persist according to `DurabilityMode` (coalescing header + frame sync in `Strict` mode).
3. **Apply & Deferred Checkpoint (`apply_journal_ops` + `maybe_checkpoint`)**: Apply post-images to the live image; once ring utilization crosses the checkpoint watermark (JBD2-style deferred checkpointing), flush live metadata and advance the WAL head pointer.

**Core Invariant**: Every metadata-mutating path on a journaled image *must* go through the WAL. Bypassing the WAL on any path would allow a pending transaction in the ring to replay stale post-images over newer changes after a crash.

---

## 4. Durability Modes (`DurabilityMode`)

| Mode | Behavior | Trade-off |
| :--- | :--- | :--- |
| **`Lazy`** (default) | Writes to OS page cache / `mmap`; flushed on `flush()`, `Drop`, or OS writeback | Maximum throughput; process-crash safe |
| **`RangeAsync`** | Tracks dirty byte ranges and drains `msync(MS_ASYNC)` asynchronously outside write locks | Low latency with proactive background writeback |
| **`Strict`** | Synchronous `msync(MS_SYNC)` / `File::sync_data()` barrier on WAL commit (single-syscall coalesced header + frame; `<= 8 KiB` `create_file_with_data` inline data-journaled) | Immediate power-loss durability |
| **`LegacyWholeMmapAsync`** | Full-mapping async flush per mutation | Legacy compatibility |

---

## 5. Verification

- **Integration & Crash Tests**: `tests/journal_test.rs`, `tests/rename_test.rs`, `tests/read_only_mode_test.rs`, `tests/auto_defrag_test.rs` (torn-write injection, idempotent redo replay, checkpoint wrap-around, WAL-atomic in-place defragmentation and `rename`).
- **Formal Verification (Kani)**: Proofs in `src/journal.rs` verify CRC32C torn-write detection, ring-buffer wrap-around arithmetic, checkpoint invariants, and bitmap op coalescing soundness.
