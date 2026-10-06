---
type: testing methodology
title: Crash Safety Testing
description: Guide to crash safety test scenarios that verify durability against power loss and system crashes in the OIFS file system.
tags: [testing, crash safety, durability, flush]
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-9aac3242153060207ee25af4
    resource: repo://tests/crash_safety_test.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-05T16:55:45.523Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-05T16:55:45.523Z
---

OIFS ensures crash safety through a combination of memory-mapped I/O, explicit flushing mechanisms, and automatic flushing on object drop. This page documents the test scenarios and mechanisms that verify data persistence across system crashes and power loss events.

## Durability Policies

<!-- openwiki: broken internal link [#src/disk.rs#L111-L142] heading anchor "src/disk.rs#L111-L142" does not exist in /openwiki/testing/crash_safety_testing.md. Fix the href or restore the target, then delete this comment. -->
OIFS provides configurable durability modes that control when and how modifications are synchronized to physical storage ([src/disk.rs#L111-L142](#src/disk.rs#L111-L142)):

- **Lazy (Default)**: Updates reside in the OS page cache and survive process crashes but require explicit flush or drop for power-loss safety. Provides highest write throughput.
- **RangeAsync**: Asynchronously flushes only modified byte ranges per mutation, balancing safety and performance.
- **Strict**: Synchronously flushes modified byte ranges on every mutation, guaranteeing physical storage persistence before returning.
- **LegacyWholeMmapAsync**: Asynchronously flushes the entire memory map after each mutation (pre-P3.3 behavior).

The current durability mode is accessible via [`DiskManager::durability_mode`] and can be modified at runtime.

## Crash Safety Mechanisms

### Explicit Flush

The [`DiskManager::flush`] method forces synchronization of all pending changes to disk. Tests verify that data written before an explicit flush persists after reopening the filesystem image ([tests/crash_safety_test.rs#L7-L50](../../tests/crash_safety_test.rs#L7-L50)).

### Implicit Flush on Drop

<!-- openwiki: broken internal link [../src/disk.rs#L325-L338] file "../src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The [`DiskManagerInner::Drop`] implementation calls [`MmapMut::flush`] when the DiskManager is dropped, ensuring data persistence even without an explicit flush ([src/disk.rs#L325-L338](../src/disk.rs#L325-L338)). This is validated by the drop_flush test which writes data and relies solely on the drop-triggered flush ([tests/crash_safety_test.rs#L53-L85](../../tests/crash_safety_test.rs#L53-L85)).

### Range-Based Synchronization
<!-- openwiki: broken internal link [../src/disk.rs#L265-L296] file "../src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
When using RangeAsync or Strict durability modes, mutations trigger synchronization of only the specific byte ranges that were modified ([src/disk.rs#L265-L296](../src/disk.rs#L265-L296)), including:
- Inode bitmap updates
- Data bitmap updates  
- Modified inode ranges
- Allocated data blocks
- Directory data blocks (for new directories)

This minimizes unnecessary I/O while ensuring crash consistency.

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

## Implementation Details

Crash safety relies on several key implementation aspects:

- **Memory-Mapped I/O**: All metadata and file data modifications occur through a shared memory map ([`MmapMut`]), which automatically updates the OS page cache.
- **Controlled Synchronization**: The [`sync_mutation_ranges`] method applies the selected durability policy to specific byte ranges rather than the entire map when appropriate.
- **Atomic Operations**: Critical metadata updates (inode allocations, bitmap modifications) are followed by appropriate synchronization calls.
- **Error Handling**: Disk operations propagate errors upward, allowing tests to verify failure scenarios.

## Running Crash Safety Tests

Execute the crash safety test suite with:
```bash
cargo test --test crash_safety
```

These tests create temporary filesystem images in the current directory and clean them up upon completion.

## Relation to Other Testing

Crash safety testing complements:
<!-- openwiki: broken internal link [../integration_testing.md] file "../integration_testing.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Integration Testing](../integration_testing.md): Validates full filesystem operations under normal conditions
- [Testing and Verification](../operations/testing_and_verification.md): Broader verification strategies including performance and correctness

Together, these test layers ensure OIFS maintains both functionality and reliability under various failure conditions.
