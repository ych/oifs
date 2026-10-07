---
type: workflow
title: Defragmentation Workflow
description: Step-by-step guide to analyzing filesystem fragmentation, executing safe defragmentation, and verifying results while maintaining data integrity.
tags: [defragmentation, fragmentation-analysis, safe-mode, atomic-rename, workflow]
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-bb40fa17bd09d24ebba1a72a
    resource: repo://tests/defrag_test.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-04T10:12:53.730Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-07T12:20:29.772Z
---

## Fragmentation Analysis

Before defragmentation, analyze the current layout to quantify fragmentation and determine if action is needed.

### Running Analysis

Analysis is performed via the `analyze_fragmentation` method on a `DiskManager` instance. This method scans the data bitmap in 64-bit word chunks to compute free-block statistics without modifying the filesystem.

**Programmatic (Rust):**
```rust
let stats = disk_manager.analyze_fragmentation()?;
println!(
    "Free runs: {}, Largest free run: {}, Fragmentation ratio: {:.2}",
    stats.free_runs, stats.largest_free_run, stats.fragmentation_ratio
);
```

**Command-line (via `oifs`):**
```bash
oifs --image /path/to/image.img stats
```
The `stats` subcommand outputs fragmentation metrics including the fragmentation ratio (0.0 = optimal, 1.0 = maximally fragmented).

### Interpreting Results

The `FragmentationStats` structure provides:
- `free_runs`: Number of contiguous free block sequences. Higher values indicate more scattered free space.
- `largest_free_run`: Size of the largest contiguous free block chunk (in blocks).
- `fragmentation_ratio`: Computed as `(free_runs - 1) / max(free_blocks - 1, 1)`. A ratio > 0.3 typically warrants defragmentation.

## Safe Defragmentation Execution

OIFS performs defragmentation via an out-of-place rebuild to guarantee safety against crashes or power loss. The default `DefragMode::Safe` creates a temporary copy, rewrites files contiguously, and uses an atomic rename to swap images.

### Command-line Execution

Initiate safe defragmentation with the `defrag` subcommand:
```bash
oifs --image /path/to/image.img defrag
```
This runs with default `DefragMode::Safe`. Progress and results are printed to stdout, including:
- Files processed
- Fragmentation ratio before/after
- Bytes moved

### Programmatic Execution

```rust
use oifs::disk::{DefragMode, DiskManager};

let mut dm = DiskManager::open("/path/to/image.img", 0)?;
let stats = dm.defragment("/path/to/image.img", DefragMode::Safe, None)?;
println!(
    "Defragged {} files. Fragmentation: {:.2} -> {:.2}",
    stats.files_processed, stats.frag_before, stats.frag_after
);
```

The `defragment` method returns a `DefragStats` struct containing:
- `files_processed`: Number of regular files defragmented
- `frag_before`: Fragmentation ratio prior to defragmentation
- `frag_after`: Fragmentation ratio after defragmentation

### In-Place Mode (Not Recommended)

`DefragMode::InPlace` attempts direct modification but is currently unimplemented and returns an error. Only `DefragMode::Safe` is functional.

## Safety Guarantees

Safe defragmentation ensures data integrity through three key mechanisms:

### Out-of-Place Rebuild

The process begins by copying the entire image to a temporary file (`{image}.defrag.tmp`). All metadata scanning, block allocation, and file rewriting occur on this copy, leaving the original untouched until verification.

### Contiguous Allocation with Attribute Preservation

During rewrite:
1. Allocated inodes are scanned and directory block locations preserved (to avoid breaking parent pointers).
2. File data, compression status, and filter configurations are extracted.
3. The temporary image's data bitmap is cleared, excluding preserved directory blocks.
4. Files are reallocated contiguously starting from the first data block, using sequential hint-based allocation.
5. Original compression mode and filter settings are reapplied exactly.

### Transactional 3-Step Atomic Rename

After flushing the temporary image to disk, an atomic rename sequence swaps images with automatic rollback on failure:
1. Rename original image → `{image}.old` (backup)
2. Rename temporary image → original image (`{image}.defrag.tmp` → `{image}`)
3. **Verification**: Reopen the new image and validate critical metadata (superblock, root inode).
   - On success: Delete backup (`{image}.old`)
   - On failure: Rename backup back to original image, restoring pre-defragmentation state

This guarantees either a fully defragmented filesystem or a complete rollback to the original state, even if power loss occurs mid-operation.

## Verification

Post-defragmentation, verify success by:
1. Checking the command-line or programmatic output for `DefragStats` showing reduced fragmentation ratio.
2. Re-running fragmentation analysis:
   ```bash
   oifs --image /path/to/image.img stats
   ```
3. Confirming no `.old` or `.defrag.tmp` files remain (indicating successful cleanup).
4. Validating file accessibility and data integrity through read operations.

## Related Workflows

<!-- openwiki: broken internal link [../basic_operations.md] file "../basic_operations.md" does not exist. Fix the href or restore the target, then delete this comment. -->
See [Basic Operations Workflow](../basic_operations.md) for creating images, file I/O, and other fundamental tasks that interoperate with defragmentation.
