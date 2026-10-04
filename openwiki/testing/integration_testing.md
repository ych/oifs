---
type: guide
title: Integration Testing Guide
description: A guide to the end-to-end integration tests for the OIFS file system, covering disk persistence, crash safety, and network synchronization.
tags: [testing, integration, crash-safety, network]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-04T10:12:53.730Z
sources:
  - id: openwiki-source-9aac3242153060207ee25af4
    resource: repo://tests/crash_safety_test.rs
  - id: openwiki-source-8ab3a63a56b627706c5b2737
    resource: repo://tests/integration_test.rs
  - id: openwiki-source-cb9a877a2a968da6a562a2e9
    resource: repo://tests/network_single_file_sync_test.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-04T10:12:53.730Z" }
---

# Integration Testing Guide

This guide describes the end-to-end integration tests that validate the OIFS file system's behavior in multi-process scenarios, network synchronization, and crash safety. These tests exercise the system as a whole, ensuring that components work together correctly under realistic conditions.

## Test Categories

The integration tests fall into three main categories:

1. **Disk Initialization and Persistence** - Verifies that the file system can be created, closed, and reopened while maintaining consistency.
2. **Crash Safety and Flush Behavior** - Ensures data survives intentional and unintentional shutdowns through proper flushing mechanisms.
3. **Network Synchronization and Concurrent Writes** - Validates that multiple network clients can safely coordinate access to shared files.

## Disk Initialization and Persistence

Located in `tests/integration_test.rs`, this test suite validates basic file system lifecycle operations.

### Test: Disk Initialization and Persistence

**Purpose**: Confirms that a file system image can be created, closed, and reopened without losing superblock integrity.

**Steps**:
1. Create a new 10MB disk image using `DiskManager::open()`.
2. Verify the superblock magic number matches the expected constant.
3. Close the disk manager (dropping the scope).
4. Reopen the same image path.
5. Verify the superblock magic number again.

**Verification**: The superblock magic must persist across open/close cycles, confirming that metadata is correctly written to disk and readable after reopening.

**Evidence**: See `tests/integration_test.rs` lines 6-28.

### Test: Allocation Persistence (Disabled)

**Note**: This test is currently disabled because the low-level block allocator API is no longer public. The comment indicates that concurrency and functional tests cover persistence of metadata and data allocation instead.

**Evidence**: See `tests/integration_test.rs` lines 30-65.

## Crash Safety and Flush Behavior

Located in `tests/crash_safety_test.rs`, this test suite validates that data survives system crashes and proper resource cleanup.

### Test: Flush Persistence

**Purpose**: Ensures that explicit flushing of data to disk persists after a simulated crash (process exit).

**Steps**:
1. Create a new disk image via the CLI (`oifs create`).
2. Open the image and create a file named "important.txt".
3. Write the string "Critical Data" to the file.
4. Explicitly call `flush()` to force data to disk.
5. Close the disk manager.
6. Reopen the image and verify the file contains the expected data.

**Verification**: Data written before an explicit flush must be readable after reopening the image, confirming that flush operations correctly transfer data from volatile caches to persistent storage.

**Evidence**: See `tests/crash_safety_test.rs` lines 6-50.

### Test: Drop Flush

**Purpose**: Ensures that relying on the `Drop` implementation to flush data (without an explicit flush call) still persists data correctly.

**Steps**:
1. Create a new disk image via the CLI.
2. Open the image and create a file named "drop.txt".
3. Write the string "Drop Data" to the file.
4. Allow the disk manager to go out of scope (triggering `Drop`).
5. Reopen the image and verify the file contains the expected data.

**Verification**: Data written before disk manager destruction must be readable after reopening, confirming that the `Drop` implementation properly flushes all pending data.

**Evidence**: See `tests/crash_safety_test.rs` lines 52-85.

### Test: Block Overflow Safety

**Purpose**: Prevents a specific class of allocation errors where the block allocator might return reserved blocks (like the superblock) under exhaustion conditions.

**Steps**:
1. Create a new disk image via the CLI.
2. Open the image.
3. Repeatedly allocate data blocks until exhaustion (or a large number of iterations).
4. Verify that no allocated block ID matches the superblock block ID.

**Verification**: The block allocator must never return reserved blocks (such as those containing the superblock) even under pressure, maintaining file system integrity.

**Evidence**: See `tests/crash_safety_test.rs` lines 87-114.

## Network Synchronization and Concurrent Writes

Located in `tests/network_single_file_sync_test.rs`, this test validates network-enabled concurrent access to shared files.

### Test: Multi-Node Concurrent Slice Writes to Single File

**Purpose**: Ensures that multiple network clients can safely write to non-overlapping regions of a single file without data corruption.

**Steps**:
1. Set up a master node that initializes a 20MB filesystem over TCP (listening on a dynamic port).
2. Create a shared file named "shared_matrix.dat" in the root directory.
3. Spawn 8 client threads (simulating network nodes), each:
   - Connecting to the filesystem via TCP.
   - Generating a unique byte pattern.
   - Writing a 4096-byte slice to a predetermined offset in the shared file.
4. Wait for all client threads to complete.
5. Open a verification client connection.
6. Read the entire shared file and verify:
   - Total length equals 8 × 4096 bytes.
   - Each slice contains the expected unique byte pattern.

**Verification**: Concurrent writes to distinct file regions by multiple network clients must not interfere, and the final file must contain a coherent concatenation of all slices.

**Evidence**: See `tests/network_single_file_sync_test.rs` lines 39-154.

## Running Integration Tests

Integration tests are located in the `tests` directory and can be executed using Cargo:

```bash
cargo test --test integration_test
cargo test --test crash_safety_test
cargo test --test network_single_file_sync_test
```

Or to run all tests:

```bash
cargo test
```

These tests require a working Rust toolchain and the `oifs` binary to be built (via `cargo build --release` or similar). They create temporary disk image files in the current working directory and clean up after themselves.

## Relationship to Other Test Suites

- **Unit Tests**: Focus on individual components in isolation (not covered in this guide).
- **Integration Tests**: Covered here; test multi-component interactions and end-to-end scenarios.
- **Concurrency Tests**: Validate thread-safe behavior within a single process (see `concurrency_test.rs` and related files).
- **Crash Safety Tests**: A subset of integration tests specifically targeting power-failure safety (see also `/openwiki/testing/crash_safety_testing.md`).

These integration tests provide confidence that the OIFS file system behaves correctly when deployed in networked environments and subjected to real-world shutdown scenarios.
