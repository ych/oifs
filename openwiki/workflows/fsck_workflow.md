---
type: workflow
title: Fsck Workflow
description: Step-by-step guide to running OIFS fsck (CLI and API), interpreting the consistency report (orphan inodes, leaked blocks, missing blocks, cross-linked blocks), when manual fsck is needed versus journaled auto-recovery on mount, and the meaning of the is_clean flag.
tags: [fsck, integrity, verification, diagnostics, journal, recovery]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-115024023ade9f243c6b4ec3
    resource: repo://src/disk/maintenance.rs
  - id: openwiki-source-ef0afc6eaf5c925c9975314d
    resource: repo://src/ipc.rs
  - id: openwiki-source-c1e8d5f8bb6497980a6b4166
    resource: repo://src/session.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---

## Purpose

The OIFS filesystem check (`fsck`) verifies structural consistency by comparing allocation bitmaps with directory reachability. It detects four types of metadata corruption: orphan inodes, leaked blocks, missing blocks, and cross-linked blocks. Unlike continuous runtime integrity guarantees, `fsck` treats the on-disk state as untrusted and reports discrepancies without automatic repair.

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
- `--network`: Enable network mode for remote images

### IPC / API interface

Clients can trigger a consistency check via the IPC layer by sending an `IpcRequest::VerifyIntegrity` message. The server responds with an `IpcResponse::Success(IpcResponseData::Fsck)` containing the serialized `FsckReport`. See `session.rs` for the server-side implementation and `IpcClient::send` for the client entrypoint.

Programmatic access through `OifsSession::verify_integrity()`:

```rust
let report = session.verify_integrity()?;
// report.is_clean, report.orphan_inodes, etc.
```

## Understanding the fsck Report

The consistency check returns a [`FsckReport`](repo://src/disk.rs#L251-L260) structure with the following fields:

| Field | Type | Meaning |
|-------|------|---------|
| `is_clean` | `bool` | `true` if no inconsistencies detected (all four vectors empty); `false` otherwise |
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

## Interpreting Results — Field-by-Field Remediation

The `is_clean` flag is `true` only when **all four vectors are empty**. Any non-empty vector means the filesystem has structural corruption.

| Field | Severity | Definition | Remediation |
|-------|----------|------------|-------------|
| `orphan_inodes` | Medium | Inodes with allocation bit set but no directory entry points to them. They occupy inode-table space but contain no user-accessible data. | Safe to clear the inode bitmap bit for each ID (`Bitmap::clear(idx)`). Orphan inodes carry no reachable user data. |
| `leaked_blocks` | Low | Data blocks with allocation bit set but not mapped by any reachable inode. Wasted capacity. | Safe to clear the corresponding data bitmap bits. These blocks contain no live file data. |
| `missing_blocks` | **Critical** | Data blocks referenced by reachable inodes but marked free in the bitmap. Subsequent allocations will overwrite live file data, causing silent corruption. | **Requires manual investigation**. Marking them allocated in the bitmap restores accounting but does not guarantee file contents are intact if blocks were subsequently reused. |
| `cross_linked_blocks` | **Critical** | A single physical block referenced by ≥2 distinct inodes. Writes to one file corrupt the other. | **Requires manual investigation**. Copy the block data to a fresh block for all but one inode, update that inode's pointer, then clear the cross-link. |

> **Note**: OIFS `fsck` is a diagnostic tool; it does **not** perform automatic repairs. Future versions may implement repair options based on the reported inconsistencies.

## Cross-Linked Block Resolution Strategy

Cross-linked blocks are detected during the single-pass BFS traversal of the directory tree (`verify_integrity_on`). The algorithm maintains two bitsets over the data block range:

- `seen_data`: blocks referenced by ≥1 reachable inode
- `cross_linked_data`: blocks referenced by ≥2 reachable inodes

For each block pointer visited via `for_each_inode_block` (which flattens direct, single-indirect, double-indirect, and triple-indirect pointers, and expands contiguous runs for 64K-chunked files):

```text
if block in valid range:
    idx = block - data_block_start
    if seen_data[idx] already set:
        cross_linked_data[idx] = 1   // second or later reference
    else:
        seen_data[idx] = 1           // first reference
```

After traversal, `cross_linked_data` is iterated to emit the sorted list in `FsckReport.cross_linked_blocks`.

**Resolution**: Because multiple inodes share the same physical block, the only safe fix is:
1. Read the shared block's contents.
2. Allocate a fresh block for each inode except one (e.g., the first encountered).
3. Copy the data to each fresh block.
4. Update the inode's block pointer to point to its new private block.
5. Clear the cross-link from the report and mark the original block allocated to the remaining inode.

This requires manual intervention or a future repair tool; `fsck` itself does not modify the image.

## When fsck Is Needed vs. Journaled Auto-Recovery

### Non-journaled images (default)

After a crash or power loss, a non-journaled image may have inconsistent metadata because in-place updates can be torn. **Run `fsck` before mounting** to diagnose the damage. The test in `tests/fsck_test.rs` demonstrates this by corrupting the data bitmap and confirming `verify_integrity` detects the missing block.

### Journaled images (`create_encrypted_journaled` / `open_journaled`)

On every mount, `DiskManager::open` calls `recover_journal` **before** publishing the manager:

1. **Phase 1**: Snapshot the journal region, decode all valid transaction frames via `JournalRing::recover`, collecting `MetadataOp` list.
2. **Phase 2**: Redo every op directly against the mmap via `apply_op_in_place` (idempotent post-image writes: `SetInodeBitmap`, `SetDataBitmap`/`SetDataBitmapRange`, `WriteInode`, `WriteBlockSlice`).
3. **Phase 3**: Reset the ring (`head = tail = 0`, `cleanly_unmounted = true`) and persist the header.

Because every `MetadataOp` is an **absolute post-image** (not a delta), replay is idempotent: replaying a transaction any number of times yields the same filesystem state. Torn writes are detected by the per-frame CRC32C and trailing commit marker (`0xDEAD_BEEF`); incomplete frames are ignored.

**Result**: On a correctly functioning journaled image, `fsck` will consistently report `is_clean = true` even after unclean shutdowns. The journal replaces the traditional "fsck on boot" with a guaranteed-bounded, deterministic replay proportional to the number of uncommitted transactions (typically a handful), not to the filesystem size.

### Summary

| Scenario | Action |
|----------|--------|
| Non-journaled image, unclean shutdown | Run `oifs fsck --image <path>` before mount |
| Journaled image, any shutdown | Mount directly; replay is automatic; `fsck` optional as diagnostic |
| Suspected bit rot / hardware fault / manual hex-edit | Run `fsck` on either image type to detect non-journal corruptions |

## Next Steps After Detecting Inconsistencies

If the report indicates corruption:

1. **Stop using the filesystem** to prevent further damage, especially if missing blocks or cross-linked blocks are present.
2. **Backup the image** immediately to preserve the current state for analysis.
3. **Determine the cause**: Consider recent crashes, power loss, or software bugs that may have produced the inconsistency.
4. **Manual intervention** (advanced): Experienced users may edit the raw image using a hex editor to adjust bitmap bits or clear orphan inodes, but this risks exacerbating corruption.
5. **Recreate the filesystem**: The safest recovery path is to backup user data, create a fresh OIFS image (`oifs create`), and restore data.

## Relationship to Other Workflows

- Run `fsck` after an unclean shutdown before mounting the filesystem (non-journaled images).
- Use `fsck` to validate the success of data recovery procedures.
- Combine with fragmentation analysis (`oifs analyze`) to assess overall filesystem health.

## Tests

See the diagnostic workflow verification in [`tests/fsck_test.rs`](repo://tests/fsck_test.rs#L6-L68) which:
1. Creates a clean filesystem and verifies a clean report.
2. Introduces bitmap corruption by clearing an allocated block's bit.
3. Confirms the corrupted filesystem reports the expected missing block.
