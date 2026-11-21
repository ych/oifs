---
type: "Reference"
title: "Crash safety testing"
openwiki_generated: true
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-115024023ade9f243c6b4ec3
    resource: repo://src/disk/maintenance.rs
  - id: openwiki-source-89aec3d3635aab8ea2684408
    resource: repo://tests/batch_read_sharded_dir_and_durability_test.rs
  - id: openwiki-source-6b4f56115285539fa470e8e9
    resource: repo://tests/contiguous_and_truncate_test.rs
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
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---


OIFS ensures crash safety through a combination of memory-mapped I/O, explicit flushing mechanisms, automatic flushing on object drop, journal-based write-ahead logging (WAL) for metadata, and structural consistency checking (fsck). This page documents the test scenarios and mechanisms that verify data persistence across system crashes and power loss events.

## Durability Policies

OIFS provides configurable durability modes that control when and how modifications are synchronized to physical storage ([src/disk.rs#L207-L241](repo://src/disk.rs#L207-L241)):

- **Lazy (Default)**: Updates reside in the OS page cache and survive process crashes but require explicit flush or drop for power-loss safety. Provides highest write throughput.
- **RangeAsync**: Asynchronously flushes only modified byte ranges per mutation via `msync(MS_ASYNC)`, balancing safety and performance.
- **Strict**: Synchronously flushes modified byte ranges via `msync(MS_SYNC)` on every mutation, guaranteeing physical storage persistence before returning.
- **LegacyWholeMmapAsync**: Asynchronously flushes the entire memory map after each mutation (pre-P3.3 behavior).

The current durability mode is accessible via [`DiskManager::durability_mode`] and can be modified at runtime ([src/disk.rs#L1574-L1592](repo://src/disk.rs#L1574-L1592)).

## Crash Safety Mechanisms

### Explicit Flush

The [`DiskManager::flush`] method forces synchronization of all pending changes to disk. Tests verify that data written before an explicit flush persists after reopening the filesystem image ([tests/crash_safety_test.rs#L7-L50](repo://tests/crash_safety_test.rs#L7-L50)).

### Implicit Flush on Drop

The [`DiskManagerInner::Drop`] implementation calls [`MmapMut::flush`] when the DiskManager is dropped, ensuring data persistence even without an explicit flush. It also flushes the journal region first to ensure the log describing pending metadata does not lag behind the metadata itself ([src/disk.rs#L680-L700](repo://src/disk.rs#L680-L700)). This is validated by the drop_flush test ([tests/crash_safety_test.rs#L53-L85](repo://tests/crash_safety_test.rs#L53-L85)).

### Range-Based Synchronization

When using RangeAsync or Strict durability modes, mutations trigger synchronization of only the specific byte ranges that were modified ([src/disk.rs#L265-L296](repo://src/disk.rs#L265-L296)), including:
- Inode bitmap updates
- Data bitmap updates
- Modified inode ranges
- Allocated data blocks
- Directory data blocks (for new directories)

This minimizes unnecessary I/O while ensuring crash consistency.

### Journal-Based Write-Ahead Logging (WAL)

When a filesystem is created with journaling enabled, metadata mutations (`create_file`, `delete_file`, `mkdir`, `write_data`, `truncate`, defragmentation) are first recorded as checksummed transactions in a fixed-size circular ring buffer, flushed to stable storage, and only then applied in place. A crash therefore always leaves the filesystem in a recoverable state: on the next mount, durable-but-unapplied transactions are replayed (Redo) automatically without a full-image `fsck` scan ([src/journal.rs#L1-L50](repo://src/journal.rs#L1-L50)).

## Test Scenarios

### Flush Persistence (`test_flush_persistence`)
1. Creates a new filesystem image
2. Opens the image and creates a file
3. Writes test data ("Critical Data")
4. Calls explicit [`flush`]
5. Reopens the image and verifies data integrity

Validates that explicit flush guarantees persistence across file closures and reopens.

### Drop Flush (`test_drop_flush`)
1. Creates a new filesystem image
2. Opens the image and creates a file
3. Writes test data ("Drop Data")
4. Allows the DiskManager to drop (triggering implicit flush via Drop)
5. Reopens the image and verifies data integrity

Confirms that the Drop implementation provides sufficient crash safety without explicit flush calls.

### Block Overflow Safety (`test_block_overflow_safety_does_not_return_superblock`)
1. Creates a filesystem image
2. Requests blocks far beyond the filesystem capacity
3. Verifies that:
   - Requests return `None` rather than valid block IDs
   - Specifically do not return block 0 (superblock)
   - Superblock magic number remains intact

Ensures boundary checks prevent metadata corruption during error conditions.

### Journal Recovery: Crash Between Commit and Apply (`test_crash_recovery_replays_durable_transaction`)
Simulates a crash after the WAL transaction is durable but before the metadata ops are applied in place ([tests/journal_test.rs#L153-L180](repo://tests/journal_test.rs#L153-L180)):
1. Creates a journaled filesystem
2. Appends a transaction marking a data block allocated directly to the journal ring **without** applying it to the image
3. Mounts the image
4. Asserts that `recover_journal` replayed the transaction and the data bitmap bit is set

This validates that `recover_journal` (called during mount in [src/disk.rs#L1245](repo://src/disk.rs#L1245) and implemented in [src/disk.rs#L1489](repo://src/disk.rs#L1489)) replays durable-but-unapplied transactions automatically.

### Torn Transaction Discard (`test_torn_transaction_is_discarded_on_mount`)
1. Appends a valid transaction to the journal
2. Corrupts a single bit in the frame payload (simulating a torn write / partial sector write)
3. Mounts the image
2. Verifies the mount succeeds, the corrupt transaction is ignored, and the filesystem remains usable

Validates CRC32C-based torn-write detection.

### Journaled Defragmentation Crash Midway Tests

OIFS in-place defragmentation is implemented as a series of round-based COW (copy-on-write) moves, each governed by a single bounded WAL transaction ([src/disk/maintenance.rs#L1430-L1490](repo://src/disk/maintenance.rs#L1430-L1490)). The defrag crash tests simulate power loss at specific phases within a round.

#### Pre-Commit Crash (Data Copied, Metadata + WAL Still Pre-Defrag) ([tests/journal_test.rs#L1863-L1920](repo://tests/journal_test.rs#L1863-L1920))
1. Runs in-place defragmentation on a journaled filesystem
2. Captures the **pre-defrag** image bytes and **post-defrag** image bytes
3. Constructs a hybrid "crashed" image: metadata + WAL regions from pre-defrag, **data area from post-defrag**
4. Mounts the crashed image
5. Verifies fsck reports clean and surviving files are intact

Since the WAL ring still contains pre-defrag state, `recover_journal` replays nothing; the post-defrag payload in free blocks is simply unused and the filesystem is consistent.

#### Post-Commit Crash (WAL Committed, In-Place Metadata Not Applied) ([tests/journal_test.rs#L1920-L1960](repo://tests/journal_test.rs#L1920-L1960))
1. Same pre/post capture
2. Constructs crashed image: **metadata from pre-defrag, data area from post-defrag, WAL region from post-defrag (committed defrag transaction), but journal header rewound (tail=0, cleanly_unmounted=false)**
3. Mount triggers recovery: replays the committed defrag transactions from tail=0 to head
4. Verifies fsck is completely clean and all surviving files have 100% correct content

This proves the WAL-atomic round design: the redo phase reconstructs the exact post-defrag metadata state from the WAL, while the data blocks were already copied to their destinations before the WAL commit.

#### Large Defrag Exceeding WAL Ring (`test_journaled_defragment_large_exceeding_wal_ring`) ([tests/journal_test.rs#L1962-L2004](repo://tests/journal_test.rs#L1962-L2004))
1. Creates 1000 files, deletes 500, runs in-place defragmentation on a journaled filesystem
2. The resulting ~500 surviving inodes × 269B per inode exceeds the 128 KiB WAL ring capacity
3. Verifies defragmentation completes in **multiple checkpointed COW rounds** without WAL overflow
4. Reopens and verifies fsck clean and all data intact

This validates the multi-round checkpointing logic where each round commits its WAL transaction, checkpoints the ring, and retires it before the next round begins.

### Truncate WAL-First Ordering Tests

The `truncate` operation on a journaled filesystem stages all metadata changes (zeroing slack bytes, clearing block pointers in direct/indirect tables, freeing data bitmap bits, pruning empty indirect blocks, updating inode size) into a single WAL transaction before committing ([src/disk.rs#L5918-L6280](repo://src/disk.rs#L5918-L6280)).

#### Journaled Truncate (`test_truncate_journaled_filesystem`) ([tests/contiguous_and_truncate_test.rs#L167-L205](repo://tests/contiguous_and_truncate_test.rs#L167-L205))
1. Creates a journaled filesystem
2. Writes a 4-block file
3. Truncates down to 1 block
4. Closes and reopens the filesystem
5. Verifies inode size, block pointers, content, and fsck cleanliness

This validates that the WAL transaction encompassing all truncate side-effects is durable before the in-place metadata is modified.

#### Journaled Coalesced Large Write + Truncate + Delete (`test_journaled_coalesced_large_write_truncate_and_delete`) ([tests/batch_read_sharded_dir_and_durability_test.rs#L200-L250](repo://tests/batch_read_sharded_dir_and_durability_test.rs#L200-L250))
1. Creates a journaled filesystem in RangeAsync mode
2. Performs mixed create, write, overwrite, truncate (shrink + expand cycles), and delete operations
3. Verifies data integrity and fsck cleanliness after reopen

This exercises the journal coalescing path where multiple operations share a single WAL transaction.

### FSCK Validation After Simulated Power Loss

The structural consistency checker (`verify_integrity`) performs a single-pass directory and block-tree traversal, validating bitmap consistency and detecting orphans, leaked blocks, missing blocks, and cross-linked blocks ([src/disk/maintenance.rs#L1570-L1710](repo://src/disk/maintenance.rs#L1570-L1710)).

#### Clean Filesystem (`test_fsck_clean_filesystem`) ([tests/fsck_extended_test.rs#L23-L48](repo://tests/fsck_extended_test.rs#L23-L48))
Creates files and directories, verifies fsck reports clean with all error categories empty.

#### Corrupted Data Bitmap Detection (`test_fsck_diagnostic_flow`) ([tests/fsck_test.rs#L1-L69](repo://tests/fsck_test.rs#L1-L69))
1. Creates a healthy filesystem with data
2. Directly corrupts the data bitmap on disk (clears an allocated block's bit)
3. Reopens and verifies fsck detects the missing block

#### Orphan Inode Detection (`test_fsck_detect_orphan_inode`) ([tests/fsck_extended_test.rs#L50-L75](repo://tests/fsck_extended_test.rs#L50-L75))
1. Directly sets an inode bitmap bit without a directory entry
2. Verifies fsck reports the orphan inode

#### Leaked Data Block Detection (`test_fsck_detect_leaked_data_block`) ([tests/fsck_extended_test.rs#L77-L105](repo://tests/fsck_extended_test.rs#L77-L105))
1. Directly sets a data bitmap bit without an inode referencing it
2. Verifies fsck reports the leaked block

#### Missing Data Block Detection (`test_fsck_detect_missing_data_block`) ([tests/fsck_extended_test.rs#L107-L140](repo://tests/fsck_extended_test.rs#L107-L140))
1. Creates a file with allocated blocks
2. Directly clears the corresponding data bitmap bits
3. Verifies fsck reports the missing blocks

#### Cross-Linked Block Detection (`test_fsck_detect_cross_linked_blocks`) ([tests/fsck_extended_test.rs#L142-L177](repo://tests/fsck_extended_test.rs#L142-L177))
1. Creates two files with distinct data blocks
2. Corrupts the second inode to point to the first file's block
3. Verifies fsck reports the cross-linked block

#### Defragmentation FSCK After Crash Simulation
Both defrag crash tests ([tests/journal_test.rs#L1863](repo://tests/journal_test.rs#L1863)) and the seekable-chunked defrag tests ([tests/defrag_seekable_chunk_test.rs#L1-L135](repo://tests/defrag_seekable_chunk_test.rs#L1-L135)) verify fsck cleanliness after mounting crashed images, ensuring that the round-based COW WAL-atomic design leaves the filesystem structurally consistent regardless of when power is lost.

## Crash Injection Pattern

While the existing tests do not use a fork/kill harness, they simulate crashes by **directly manipulating the on-disk image bytes** between well-defined phases (pre-commit, post-commit, post-apply) and then mounting the resulting image. This pattern:
- Precisely controls the crash point at syscall/phase boundaries
- Avoids flakiness from timing-dependent signal delivery
- Enables deterministic reproduction of specific failure modes (torn writes, partial metadata apply, WAL commit without apply)

The test helpers in [tests/journal_test.rs](repo://tests/journal_test.rs) provide the `Img` fixture and byte-level image surgery utilities for this pattern.

## Running Crash Safety Tests

Execute the crash safety test suite with:
```bash
cargo test --test crash_safety
```

Execute journal recovery and defrag crash tests:
```bash
cargo test --test journal_test
```

Execute fsck tests:
```bash
cargo test --test fsck_test --test fsck_extended_test
```

Execute truncate and defragmentation tests:
```bash
cargo test --test contiguous_and_truncate_test --test defrag_test --test defrag_seekable_chunk_test
```

These tests create temporary filesystem images in the current directory or `target/` and clean them up upon completion.

## Relation to Other Testing

Crash safety testing complements:

- [Integration Testing](integration_testing.md): Validates full filesystem operations under normal conditions
- [Testing and Verification](../operations/testing_and_verification.md): Broader verification strategies including performance and correctness
- [Journal Architecture](../architecture/journal.md): Journaling subsystem design
- [Defragmentation Architecture](../architecture/defragmentation.md): Round-based COW in-place defragmentation
- [Integrity Checking / FSCK](../architecture/integrity_checking_fsck.md): Structural consistency verification

Together, these test layers ensure OIFS maintains both functionality and reliability under various failure conditions.
