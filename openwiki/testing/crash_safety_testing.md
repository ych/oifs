---
type: "Reference"
title: "Crash Safety Testing"
description: "Documents crash safety validation including power-loss simulation, WAL replay verification, fsck after unclean shutdown, durability mode guarantees, and test scenarios for concurrent operations during crash."
tags: [crash-safety, testing, journal, wal, durability, fsck, power-loss, recovery]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-115024023ade9f243c6b4ec3
    resource: repo://src/disk/maintenance.rs
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
  - id: openwiki-source-9aac3242153060207ee25af4
    resource: repo://tests/crash_safety_test.rs
  - id: openwiki-source-a249d539653d0c5ebcb1b76c
    resource: repo://tests/defrag_seekable_chunk_test.rs
  - id: openwiki-source-d20cd9a331c33e77e0221049
    resource: repo://tests/fsck_extended_test.rs
  - id: openwiki-source-3c96e510942a9d6c6179a866
    resource: repo://tests/fsck_test.rs
  - id: openwiki-source-1b7d3f3dde74154b9d297ef3
    resource: repo://tests/journal_test.rs
  - id: openwiki-source-15b95b45be81584668eab400
    resource: repo://tests/rename_test.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

OIFS ensures crash safety through a combination of memory-mapped I/O, explicit flushing mechanisms, automatic flushing on object drop, journal-based write-ahead logging (WAL) for metadata, and structural consistency checking (fsck). This page documents the test scenarios and mechanisms that verify data persistence across system crashes and power loss events.

## Durability Policies

OIFS provides configurable durability modes that control when and how modifications are synchronized to physical storage ([repo://src/disk.rs#L207-L241](repo://src/disk.rs#L207-L241)):

- **Lazy (Default)**: Updates reside in the OS page cache and survive process crashes but require explicit flush or drop for power-loss safety. Provides highest write throughput.
- **RangeAsync**: Asynchronously flushes only modified byte ranges per mutation via `msync(MS_ASYNC)`, balancing safety and performance.
- **Strict**: Synchronously flushes modified byte ranges via `msync(MS_SYNC)` on every mutation, guaranteeing physical storage persistence before returning.
- **LegacyWholeMmapAsync**: Asynchronously flushes the entire memory map after each mutation (pre-P3.3 behavior).

The current durability mode is accessible via [`DiskManager::durability_mode`] and can be modified at runtime ([repo://src/disk.rs#L1937-L1955](repo://src/disk.rs#L1937-L1955)).

### Crash Guarantees by Durability Mode

| Mode | Process Crash Safety | Power-Loss Safety | Ordering Guarantees |
|------|---------------------|-------------------|---------------------|
| **Lazy** | ✅ (OS page cache) | ❌ Requires explicit `flush()` or `Drop` | None |
| **RangeAsync** | ✅ | ⚠️ Async `msync` — may lose recent mutations | Per-mutation range ordering |
| **Strict** | ✅ | ✅ Sync `msync` — all mutations durable on return | Full per-mutation ordering |
| **LegacyWholeMmapAsync** | ✅ | ⚠️ Async full-map flush | Coarse-grained |

## Crash Safety Mechanisms

### Explicit Flush

The [`DiskManager::flush`] method forces synchronization of all pending changes to disk. Tests verify that data written before an explicit flush persists after reopening the filesystem image ([repo://tests/crash_safety_test.rs#L7-L50](repo://tests/crash_safety_test.rs#L7-L50)).

### Implicit Flush on Drop

The [`DiskManagerInner::Drop`] implementation flushes the journal region first (if journaled), then calls `MmapMut::flush` on the backing storage ([repo://src/disk.rs#L774-L794](repo://src/disk.rs#L774-L794)). This ensures data persistence even without an explicit flush, and the journal region is flushed first so the log describing pending metadata does not lag behind the metadata itself. This is validated by the `test_drop_flush` test ([repo://tests/crash_safety_test.rs#L53-L85](repo://tests/crash_safety_test.rs#L53-L85)).

```rust
impl Drop for DiskManagerInner {
    fn drop(&mut self) {
        let (flush_journal, flush_backing) =
            Self::should_sync_on_drop(self.read_only, self.superblock.has_journal_layout());
        // The journal region goes out first: on shutdown the log describing pending
        // metadata must not lag behind the metadata itself.
        if flush_journal {
            let bs = self.superblock.block_size as u64;
            if let Some(start) = DiskManager::journal_region_start(&self.superblock) {
                let len = DiskManager::journal_region_len(bs)
                    .min(self.backing.len().saturating_sub(start));
                let _ = self.backing.flush_range(start, len);
            }
        }
        // Flush any pending changes to disk when dropped
        if flush_backing {
            let _ = self.backing.flush();
        }
    }
}
```

### Range-Based Synchronization

When using `RangeAsync` or `Strict` durability modes, mutations trigger synchronization of only the specific byte ranges that were modified via `sync_mutation_ranges` ([repo://src/disk.rs#L547-L690](repo://src/disk.rs#L547-L690)), including:
- Inode bitmap updates
- Data bitmap updates
- Modified inode ranges
- Allocated data blocks
- Directory data blocks (for new directories)

This minimizes unnecessary I/O while ensuring crash consistency.

```rust
pub fn sync_mutation_ranges(&self, ranges: &[(usize, usize)]) -> Result<(), DiskManagerError> {
    if self.read_only {
        return Ok(());
    }
    match self.durability_mode() {
        DurabilityMode::Lazy => Ok(()),
        DurabilityMode::RangeAsync => {
            // Accumulate ranges, coalesce adjacent/overlapping ranges,
            // drain in batches outside the write lock
        }
        DurabilityMode::Strict => {
            // Synchronously msync(MS_SYNC) each coalesced range
        }
        DurabilityMode::LegacyWholeMmapAsync => {
            // Async flush entire mmap
        }
    }
}
```

The `sync_mutation_ranges` implementation:
- **RangeAsync**: Accumulates dirty ranges in `pending_async_ranges`, coalesces adjacent/overlapping ranges with a 4 KiB gap bridge, and drains in batches of up to 64 operations or 4 MiB. The journal region is flushed first (when journaled) before data ranges to preserve WAL-before-metadata ordering.
- **Strict**: Synchronously `msync(MS_SYNC)` each coalesced range. Under `FileIo` storage mode, a single `sync_data()` flushes all dirty ranges at once.
- **LegacyWholeMmapAsync**: Asynchronously flushes the entire memory map.

### Journal-Based Write-Ahead Logging (WAL)

When a filesystem is created with journaling enabled, metadata mutations (`create_file`, `delete_file`, `mkdir`, `write_data`, `truncate`, defragmentation) are first recorded as checksummed transactions in a fixed-size circular ring buffer, flushed to stable storage, and only then applied in place. A crash therefore always leaves the filesystem in a recoverable state: on the next mount, durable-but-unapplied transactions are replayed (Redo) automatically without a full-image `fsck` scan ([repo://src/journal.rs#L1-L50](repo://src/journal.rs#L1-L50)).

## Journal Recovery Mechanism

The `recover_journal` function is called automatically on mount ([repo://src/disk.rs#L1912](repo://src/disk.rs#L1912), [repo://src/disk.rs#L2176-L2218](repo://src/disk.rs#L2176-L2218)). It operates in three phases:

<!-- openwiki: mermaid parse failed and this diagram was converted to a text fence so it does not break rendering. Fix the diagram source and restore the mermaid fence. Parser error: Parse error on line 10: ...r/>Post-Image Write (Idempotent)] E2 Expecting 'SQE', 'DOUBLECIRCLEEND', 'PE', '-)', 'STADIUMEND', 'SUBROUTINEEND', 'PIPE', 'CYLINDEREND', 'DIAMOND_STOP', 'TAGEND', 'TRAPEND', 'INVTRAPEND', 'UNICODE_TEXT', 'TEXT', 'TAGSTART', got 'PS' -->
```text
flowchart TD
    A[Mount Journaled Image] --> B{Journal Region Exists?}
    B -- No --> C[Skip Recovery]
    B -- Yes --> D[Phase 1: Validation]
    D --> D1[Snapshot Journal Region]
    D1 --> D2[Decode & Validate All Frames<br/>between tail and head using CRC32C]
    D2 --> D3[Collect Operations from Valid Frames]
    D3 --> E[Phase 2: Redo]
    E --> E1[Apply Each Operation Directly<br/>Against Image Backing Storage]
    E1 --> E2[Every MetadataOp is Absolute<br/>Post-Image Write (Idempotent)]
    E2 --> F[Phase 3: Reset & Flush]
    F --> F1[Flush Replayed Metadata to Disk]
    F1 --> F2[Reset Ring: head=0, tail=0,<br/>cleanly_unmounted=true]
    F2 --> G[Mount Complete - Consistent State]
```

1. **Phase 1 (Validation)**: Snapshot the journal region, decode and validate all frames between `tail` and `head` using CRC32C. Collect all operations from valid frames. Torn or corrupt frames fail validation and are ignored — everything before the first invalid frame is durable and replayable.

2. **Phase 2 (Redo)**: Apply each operation directly against the image backing storage. Every `MetadataOp` is an absolute post-image write, making replay idempotent (replaying a transaction twice yields identical state).

3. **Phase 3 (Reset)**: Flush replayed metadata to disk, then reset the ring to a clean state (`head=0`, `tail=0`, `cleanly_unmounted=true`). The header is updated and flushed with `flush_writeback()` to ensure durability before the ring is marked clean.

This guarantees that a crash between WAL commit and in-place apply is recovered by redo on the next mount.

## Test Scenarios

### Flush Persistence (`test_flush_persistence`)
1. Creates a new filesystem image
2. Opens the image and creates a file
3. Writes test data ("Critical Data")
4. Calls explicit [`flush`]
5. Reopens the image and verifies data integrity

Validates that explicit flush guarantees persistence across file closures and reopens ([repo://tests/crash_safety_test.rs#L7-L50](repo://tests/crash_safety_test.rs#L7-L50)).

### Drop Flush (`test_drop_flush`)
1. Creates a new filesystem image
2. Opens the image and creates a file
3. Writes test data ("Drop Data")
4. Allows the DiskManager to drop (triggering implicit flush via Drop)
5. Reopens the image and verifies data integrity

Confirms that the Drop implementation provides sufficient crash safety without explicit flush calls ([repo://tests/crash_safety_test.rs#L53-L85](repo://tests/crash_safety_test.rs#L53-L85)).

### Block Overflow Safety (`test_block_overflow_safety_does_not_return_superblock`)
1. Creates a filesystem image
2. Requests blocks far beyond the filesystem capacity
3. Verifies that:
   - Requests return `None` rather than valid block IDs
   - Specifically do not return block 0 (superblock)
   - Superblock magic number remains intact

Ensures boundary checks prevent metadata corruption during error conditions ([repo://tests/crash_safety_test.rs#L87-L114](repo://tests/crash_safety_test.rs#L87-L114)).

### Journal Recovery: Crash Between Commit and Apply (`test_crash_recovery_replays_durable_transaction`)
Simulates a crash after the WAL transaction is durable but before the metadata ops are applied in place ([repo://tests/journal_test.rs#L153-L180](repo://tests/journal_test.rs#L153-L180)):
1. Creates a journaled image
2. Directly appends a transaction (e.g., `SetDataBitmap`) to the journal ring **without** applying it to the image
3. Writes the "crashed" image to disk
4. Mounts the image — `recover_journal` must replay the transaction
5. Verifies the data bitmap bit is set, confirming redo took effect

### Torn Transaction Discard (`test_torn_transaction_is_discarded_on_mount`)
1. Creates a journaled image and appends a valid transaction
2. Corrupts the transaction frame by flipping a bit in the payload
3. Mounts the image — the torn frame fails CRC32C validation and is ignored
4. Verifies the filesystem mounts successfully and remains usable ([repo://tests/journal_test.rs#L182-L210](repo://tests/journal_test.rs#L182-L210))

### Idempotent Redo (`test_apply_op_idempotent_in_place`)
Directly asserts that applying the same `MetadataOp` twice yields identical on-disk state, proving redo is idempotent and safe to replay any number of times ([repo://tests/journal_test.rs#L212-L225](repo://tests/journal_test.rs#L212-L225)).

### Journaled Truncate Crash Safety (`test_truncate_journaled_filesystem`)
Verifies that truncate on a journaled filesystem stages all metadata changes (zeroing slack, clearing block pointers, freeing data bitmap bits, pruning indirect blocks, updating inode size) into a single WAL transaction before committing ([repo://tests/contiguous_and_truncate_test.rs#L167-L205](repo://tests/contiguous_and_truncate_test.rs#L167-L205)):

1. Opens journaled image, creates file, writes 16 KB (4 blocks)
2. Truncates to 4 KB (1 block) — zeroing slack, freeing 3 blocks, pruning indirects, updating inode
3. Drops DiskManager, reopens image
4. Verifies inode size = 4 KB, only block 0 allocated, content intact
5. Runs `verify_integrity` — filesystem must be 100% clean

The `truncate_internal` implementation ([repo://src/disk.rs#L8085-L8480](repo://src/disk.rs#L8085-L8480)) builds a vector of `MetadataOp` including `WriteBlockSlice` for slack zeroing, `WriteBlockSlice` for indirect block pointer clearing, `SetDataBitmap` for freeing data blocks, and `WriteInode` for the size update — all committed as one transaction.

### Defragmentation Crash Safety (`test_journaled_defragment_crash_midway_with_stale_disk_metadata`)
Simulates power loss at two precise mid-round phases ([repo://tests/journal_test.rs#L1862-L1960](repo://tests/journal_test.rs#L1862-L1960)):
- **Pre-commit crash (Case 1)**: Data copied to new blocks, WAL still contains pre-defrag state — after mount, original layout must be intact
- **Post-commit crash (Case 2)**: WAL committed, metadata not yet applied — after mount, redo must apply the defrag transaction

The test constructs crash images by:
1. Running defragmentation and capturing pre-defrag and post-defrag image bytes
2. **Case 1**: Combining pre-defrag metadata + WAL with post-defrag data area (simulates crash during payload copy, before journal sync)
3. **Case 2**: Same combined image but with post-defrag WAL header (`tail=0`, `cleanly_unmounted=false`) to force recovery replay

Both scenarios verified with `verify_integrity` (clean) and data integrity checks.

### Large Defragmentation WAL Overflow (`test_journaled_defragment_large_exceeding_wal_ring`)
Exercises defragmentation exceeding the WAL ring capacity, completing in multiple checkpointed COW rounds without WAL overflow, with fsck verification after reopen ([repo://tests/journal_test.rs#L1961-L2004](repo://tests/journal_test.rs#L1961-L2004)).

### Seekable Chunked Journaled Crash Recovery (`test_seekable_chunked_journaled_crash_recovery_and_atomicity`)
Tests crash safety for seekable (chunked) compressed files on journaled filesystem ([repo://tests/seekable_chunked_test.rs#L467-L535](repo://tests/seekable_chunked_test.rs#L467-L535)):
1. Creates journaled image with `Strict` durability
2. Writes 256 KB seekable-compressed payload (4×64 KB chunks)
3. Performs partial overwrite across chunk boundaries, truncate shrink, truncate expand
4. Drops filesystem without explicit flush
5. Reopens — verifies data integrity and `verify_integrity` is clean

## Power-Loss Simulation Methodology

Crash injection uses deterministic on-disk image byte manipulation between well-defined phases rather than fork/kill harnesses ([repo://tests/journal_test.rs#L153-L180](repo://tests/journal_test.rs#L153-L180), [repo://tests/journal_test.rs#L1862-L1960](repo://tests/journal_test.rs#L1862-L1960), [repo://tests/wal_and_concurrency_regressions_test.rs#L86-L93](repo://tests/wal_and_concurrency_regressions_test.rs#L86-L93)):

| Phase | Description | Simulated Crash Point |
|-------|-------------|----------------------|
| **Pre-commit** | Write image state before WAL commit | During data copy, before journal `msync` |
| **Post-commit** | Write image state after WAL commit but before in-place apply | After journal `msync`, before metadata `msync` |
| **Post-apply** | Write image state after metadata applied | Clean shutdown equivalent |

This enables precise control of crash points at syscall/phase boundaries without non-deterministic process termination. The `test_strict_journal_ring_wrap_checkpoints_instead_of_dropping` test ([repo://tests/wal_and_concurrency_regressions_test.rs#L67-L105](repo://tests/wal_and_concurrency_regressions_test.rs#L67-L105)) snapshots raw bytes while the filesystem is open (simulating abrupt power loss before Drop), then remounts to verify `Strict` durability checkpoints the WAL ring correctly.

## FSCK Validation After Simulated Crashes

Structural consistency checking validates crash recovery by injecting direct on-disk corruption and verifying detection:

- **Orphan inodes**: Inode bitmap bit set, no directory reference ([repo://tests/fsck_extended_test.rs#L47-L80](repo://tests/fsck_extended_test.rs#L47-L80))
- **Leaked blocks**: Data bitmap bit set, no inode pointer ([repo://tests/fsck_extended_test.rs#L82-L115](repo://tests/fsck_extended_test.rs#L82-L115))
- **Missing blocks**: Inode references block, bitmap says free ([repo://tests/fsck_extended_test.rs#L117-L155](repo://tests/fsck_extended_test.rs#L117-L155))
- **Cross-linked blocks**: Multiple inodes reference same block ([repo://tests/fsck_extended_test.rs#L157-L177](repo://tests/fsck_extended_test.rs#L157-L177))

Also validated after defragmentation crash simulations ([repo://tests/journal_test.rs#L1862-L1960](repo://tests/journal_test.rs#L1862-L1960)) and seekable chunked tests ([repo://tests/seekable_chunked_test.rs#L528-L535](repo://tests/seekable_chunked_test.rs#L528-L535)).

The `verify_integrity` implementation ([repo://src/disk/maintenance.rs#L1599-L1795](repo://src/disk/maintenance.rs#L1599-L1795)) operates under a shared read lock, scanning the inode and data bitmaps directly from disk, performing a single-pass reachable tree walk from the root inode, and computing set differences to identify the five corruption categories.

## Concurrent Operations + Crash Scenarios

| Scenario | Test | Crash Point | Verification |
|----------|------|-------------|--------------|
| **Concurrent write + crash** | Implicit via multi-threaded tests | Between `msync` of data and inode update | Reopen, verify data integrity |
| **Rename + crash** | `test_rename_matrix_plain_journaled_and_file_io` ([repo://tests/rename_test.rs#L1-L100](repo://tests/rename_test.rs#L1-L100)) | After directory entry moved, before source entry cleared | Mount recovery, verify single directory entry, inode preserved |
| **Journal checkpoint + crash** | Defragmentation tests ([repo://tests/journal_test.rs#L1862-L2004](repo://tests/journal_test.rs#L1862-L2004)) | Between WAL checkpoint and ring retire | Mount recovery, verify no stale transactions |

The rename test covers four storage mode combinations (Mmap/FileIo × journaled/legacy) and validates:
- Same-directory rename preserves inode ID and payload blocks
- Cross-directory move preserves inode and data
- Overwrite of existing file frees victim atomically
- All variants reopen cleanly after simulated crash

### WAL Replay Verification

Transaction atomicity and ordering are verified by:
1. **Atomicity**: A transaction either fully replays or is discarded (torn frame detection via CRC32C)
2. **Ordering**: Operations within a transaction replay in append order; transactions replay in sequence number order
3. **Idempotency**: Replaying a transaction multiple times yields identical state (post-image writes)

The `test_crash_recovery_replays_durable_transaction` test directly exercises the pre-commit crash scenario, and `test_apply_op_idempotent_in_place` validates the idempotent redo primitive.

### Data Integrity Under COW Failure (`test_write_data_journaled_cow_preserves_old_data_on_enospc`)
Verifies that copy-on-write (COW) on journaled filesystems preserves original data when ENOSPC occurs mid-overwrite ([repo://tests/wal_and_concurrency_regressions_test.rs#L10-L52](repo://tests/wal_and_concurrency_regressions_test.rs#L10-L52)):
1. Creates small journaled image (~256 KiB) with an 80 KiB file
2. Attempts 160 KiB incompressible overwrite (insufficient free blocks for COW)
3. Asserts overwrite fails with ENOSPC
4. Verifies original data intact on reopen + `verify_integrity` clean

### Strict Durability WAL Ring Wrap (`test_strict_journal_ring_wrap_checkpoints_instead_of_dropping`)
Ensures `Strict` durability auto-checkpoints before overwriting uncheckpointed WAL frames, even without explicit `flush()` calls ([repo://tests/wal_and_concurrency_regressions_test.rs#L55-L105](repo://tests/wal_and_concurrency_regressions_test.rs#L55-L105)):
1. Creates journaled image with `Strict` mode
2. Creates 450 small files (generates enough transactions to wrap the 128 KiB ring)
3. Snapshots raw bytes while open (simulates abrupt power loss before Drop)
4. Remounts snapshot — all 450 files present, data intact, `verify_integrity` clean

## Summary of Crash Safety Guarantees

| Operation | Non-Journaled | Journaled |
|-----------|---------------|-----------|
| **File create** | `Lazy`: Lost on power loss<br>`Strict`: Durable | Always durable (WAL + redo) |
| **File delete** | `Lazy`: Lost on power loss<br>`Strict`: Durable | Always durable (WAL + redo) |
| **Write data** | `Lazy`: Lost on power loss<br>`Strict`: Durable | Data blocks written first; metadata via WAL |
| **Truncate** | `Lazy`: Lost on power loss<br>`Strict`: Durable | Single WAL transaction |
| **Rename** | `Lazy`: Lost on power loss<br>`Strict`: Durable | WAL transaction covering both dir entries |
| **Defragment** | N/A | Multi-round WAL with checkpoints |

For journaled filesystems, any crash leaves the on-disk state as either:
- **Pre-crash state**: Transaction not yet durable (torn or absent in WAL)
- **Post-crash state**: Transaction durable, redo applied on next mount

There is no intermediate corrupt state — fsck is never required for journal recovery.

## Related Pages

- [Journal Architecture](../architecture/journal.md) — WAL design, transaction format, recovery algorithm
- [Integrity Checking (fsck)](../architecture/integrity_checking_fsck.md) — Structural consistency verification
- [Integration Testing](../testing/integration_testing.md) — End-to-end test scenarios
- [Fsck Workflow](../workflows/fsck_workflow.md) — Operational fsck procedures
