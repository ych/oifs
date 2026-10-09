---
type: workflow
title: Filesystem Check Workflow
description: Step-by-step guide to running fsck, interpreting the consistency report, and understanding next steps when inconsistencies are detected.
tags: [fsck, integrity, verification, diagnostics]
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-ef0afc6eaf5c925c9975314d
    resource: repo://src/ipc.rs
  - id: openwiki-source-c1e8d5f8bb6497980a6b4166
    resource: repo://src/session.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-09T15:04:29.410Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-09T15:04:29.410Z
---

## Purpose

The OIFS filesystem check (`fsck`) workflow verifies structural consistency by comparing allocation bitmaps with directory reachability. It detects four types of metadata corruption: orphan inodes, leaked blocks, missing blocks, and cross-linked blocks. Unlike continuous runtime integrity guarantees, `fsck` treats the on-disk state as untrusted and reports discrepancies without automatic repair.

## Running fsck

### Command-line interface

Execute the `fsck` subcommand on a mounted or unmounted filesystem image:

```bash
oifs fsck --image /path/to/image.img
```

Options:
- `--image`: Path to the OIFS image file (required)
- `--password`: Provide password for encrypted filesystems (optional, prompts if omitted)
- `--json`: Output machine-readable JSON instead of human-readable text
- `--network`: Enable network mode for remote images (see [basic operations](../workflows/basic_operations.md))

### IPC interface

Clients can trigger a consistency check via the IPC layer by sending an `IpcRequest::Fsck` message. The server responds with an `IpcResponse::Fsck` containing the serialized `FsckReport`. Refer to [session.rs](repo://src/session.rs#L806-L815) for the server-side implementation.

## Understanding the fsck Report

The consistency check returns a [`FsckReport`](repo://src/disk.rs#L163-L174) structure with the following fields:

| Field | Type | Meaning |
|-------|------|---------|
| `is_clean` | `bool` | `true` if no inconsistencies detected; `false` otherwise |
| `orphan_inodes` | `Vec<u64>` | Allocated inodes not reachable from the root directory |
| `leaked_blocks` | `Vec<u64>` | Data blocks marked allocated in bitmaps but not referenced by any inode |
| `missing_blocks` | `Vec<u64>` | Data blocks referenced by inodes but marked free in bitmaps |
| `cross_linked_blocks` | `Vec<u64>` | Data blocks referenced by two or more distinct inodes |

### Sample Output

Human-readable format (default):

```
=== Filesystem Consistency Check (fsck) ===
Status:              ❌ CORRUPTED
Orphan Inodes:       2
  IDs: [42, 43]
Leaked Blocks:       0
Missing Blocks:      1
  IDs: [1057]
Cross-Linked Blocks: 0
===========================================
```

JSON format (`--json`):

```json
{
  "is_clean": false,
  "orphan_inodes": [42, 43],
  "leaked_blocks": [],
  "missing_blocks": [1057],
  "cross_linked_blocks": []
}
```

## Interpreting Results

Refer to the [Filesystem Integrity Checking (fsck)](../architecture/integrity_checking_fsck.md) page for detailed definitions and severity rankings:

- **Orphan Inodes** (Medium): Consume inode table space but are inaccessible. Safe to ignore or reclaim by clearing the inode bitmap bit.
- **Leaked Blocks** (Low): Wasted storage capacity. Safe to ignore or reclaim by clearing the corresponding data bitmap bit.
- **Missing Blocks** (**Critical**): Active file data points to blocks the allocator considers free. Subsequent allocations will overwrite live data, causing silent corruption.
- **Cross-Linked Blocks** (**Critical**): Multiple files share the same physical block. Writes to one file corrupt another.

A filesystem is consistent only when all four vectors are empty and `is_clean` is `true`.

## Next Steps After Detecting Inconsistencies

OIFS `fsck` is a diagnostic tool; it does **not** perform automatic repairs. If the report indicates corruption:

1. **Stop using the filesystem** to prevent further damage, especially if missing blocks or cross-linked blocks are present.
2. **Backup the image** immediately to preserve the current state for analysis.
3. **Determine the cause**: Consider recent crashes, power loss, or software bugs that may have produced the inconsistency.
4. **Manual intervention** (advanced): Experienced users may edit the raw image using a hex editor to adjust bitmap bits or clear orphan inodes, but this risks exacerbating corruption.
5. **Recreate the filesystem**: The safest recovery path is to backup user data, create a fresh OIFS image (`oifs create`), and restore data.

> **Note**: Future versions may implement repair options based on the reported inconsistencies. Monitor the project roadmap for updates.

## Relationship to Other Workflows

<!-- openwiki: broken internal link [../quickstart.md#mounting-an-image] heading anchor "mounting-an-image" does not exist in "../quickstart.md". Fix the href or restore the target, then delete this comment. -->
- Run `fsck` after an unclean shutdown before mounting the filesystem (see [basic operations](../quickstart.md#mounting-an-image)).
- Use `fsck` to validate the success of data recovery procedures.
- Combine with fragmentation analysis (`oifs analyze`) to assess overall filesystem health.

## Tests

See the diagnostic workflow verification in [`tests/fsck_test.rs`](repo://tests/fsck_test.rs#L6-L68) which:
1. Creates a clean filesystem and verifies a clean report.
2. Introduces bitmap corruption by clearing an allocated block's bit.
3. Confirms the corrupted filesystem reports the expected missing block.
