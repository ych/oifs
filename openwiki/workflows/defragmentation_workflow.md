---
type: workflow
title: Defragmentation Workflow
description: Step-by-step guide to analyzing filesystem fragmentation, executing safe defragmentation, running in-place WAL-atomic defragmentation, and configuring the background auto-defrag coordinator while maintaining data integrity.
tags: [defragmentation, fragmentation-analysis, safe-mode, in-place-mode, atomic-rename, wal, auto-defrag, workflow]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
  - id: openwiki-source-115024023ade9f243c6b4ec3
    resource: repo://src/disk/maintenance.rs
  - id: openwiki-source-bb40fa17bd09d24ebba1a72a
    resource: repo://tests/defrag_test.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---

## Fragmentation Analysis

Before defragmentation, analyze the current layout to quantify fragmentation and determine if action is needed.

### Running Analysis

Analysis is performed via the `analyze_fragmentation` method on a `DiskManager` instance. This method scans the data bitmap in 64-bit word chunks to compute free-block statistics without modifying the filesystem.

**Programmatic (Rust):**
```rust
use oifs::disk::DiskManager;

let dm = DiskManager::open("/path/to/image.img", 0)?;
let stats = dm.analyze_fragmentation()?;
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
- `total_blocks`: Total data blocks in the filesystem
- `used_blocks`: Number of allocated data blocks
- `free_blocks`: Number of free data blocks
- `free_runs`: Number of contiguous free block sequences. Higher values indicate more scattered free space.
- `largest_free_run`: Size of the largest contiguous free block chunk (in blocks).
- `avg_gap_size`: Average size of free runs (total gap size / free_runs).
- `fragmentation_ratio`: Computed as `(free_runs - 1) / max(free_blocks - 1, 1)`. A ratio > 0.25 typically warrants defragmentation (this is the default auto-defrag threshold).

**Evidence:** [`analyze_fragmentation_internal`](repo://src/disk/maintenance.rs#L150-L255) performs the 64-bit word scanning and ratio calculation.

---

## Safe Defragmentation Execution (Out-of-Place Rebuild)

OIFS performs defragmentation via an out-of-place rebuild to guarantee safety against crashes or power loss. The default `DefragMode::Safe` creates a temporary copy, rewrites files contiguously, and uses an atomic rename to swap images.

### Command-line Execution

Initiate safe defragmentation with the `defrag` subcommand:
```bash
oifs --image /path/to/image.img defrag
```
To explicitly specify safe mode:
```bash
oifs --image /path/to/image.img defrag --mode safe
```
Progress and results are printed to stdout, including:
- Files processed
- Fragmentation ratio before/after
- Bytes moved

### Programmatic Execution

```rust
use oifs::disk::{DefragMode, DiskManager};

let dm = DiskManager::open("/path/to/image.img", 0)?;
let stats = dm.defragment("/path/to/image.img", DefragMode::Safe, None)?;
println!(
    "Defragged {} files. Fragmentation: {:.2} -> {:.2}",
    stats.files_processed, stats.frag_before, stats.frag_after
);
```

The `defragment` method returns a `DefragStats` struct containing:
- `files_processed`: Number of regular files defragmented
- `bytes_moved`: Total bytes rewritten
- `blocks_freed`: Approximate free runs eliminated (`frag_before.free_runs - frag_after.free_runs`)
- `frag_before`: Fragmentation ratio prior to defragmentation
- `frag_after`: Fragmentation ratio after defragmentation

**Evidence:** [`DefragStats`](repo://src/disk.rs#L157-L174) and [`defragment`](repo://src/disk/maintenance.rs#L460-L480).

---

## In-Place Defragmentation (WAL-Atomic, Copy-on-Write)

OIFS also supports in-place defragmentation via `DefragMode::InPlace`. This mode directly reorganizes physical data blocks within the existing image without allocating duplicate temporary files. It is fully WAL-atomic on journaled images: payload moves are ordered before metadata commits, and metadata updates are recorded in the WAL with batched checkpointing between rounds.

### Command-line Execution

```bash
oifs --image /path/to/image.img defrag --mode inplace
```
The CLI prints a 3-second warning before proceeding, as in-place modification directly modifies the image file.

### Programmatic Execution

```rust
use oifs::disk::{DefragMode, DiskManager};

let dm = DiskManager::open("/path/to/image.img", 0)?;
let stats = dm.defragment("/path/to/image.img", DefragMode::InPlace, None)?;
println!(
    "In-place defragged {} files. Bytes moved: {}. Frag: {:.2} -> {:.2}",
    stats.files_processed, stats.bytes_moved, stats.frag_before, stats.frag_after
);
```

### When to Use In-Place vs Safe Mode

| Aspect | Safe Mode | In-Place Mode |
|--------|-----------|---------------|
| **Crash safety** | Original image untouched until verification; automatic rollback on failure | WAL-atomic; mid-pass crash leaves image consistent (journal replay or no-op) |
| **Disk space** | Requires 2x image size temporarily (original + temp copy) | No extra space needed |
| **Performance** | Full image copy + rewrite | Round-based COW moves only fragmented blocks; generally faster on large images |
| **Encryption** | Fully supported (key preserved) | Fully supported (uses WAL on journaled images) |
| **Journaled images** | Uses legacy path (copy + rename) | **Default on journaled images** — leverages WAL for atomicity |
| **Legacy (non-journaled) images** | Safe path always works | Uses legacy cycle-aware permutation (no WAL) |

**Evidence:** [`defragment_inplace_on`](repo://src/disk/maintenance.rs#L644-L720) and [`defragment_inplace_journaled_cow`](repo://src/disk/maintenance.rs#L792-L1553).

---

## In-Place Defragmentation Internals (Journaled Images)

On WAL-enabled (journaled) images, in-place defragmentation uses a **round-based copy-on-write (COW) algorithm** with strict ordering guarantees:

### Algorithm Overview

1. **Pre-defrag quiesce**: Drain pending async ranges, flush dirty mmap pages, checkpoint WAL to head (clean ring).
2. **Collect inodes**: Scan inode bitmap, decode all active inodes in a single pass (no `inode_cache` thrashing). Separate directories and regular files.
3. **Plan target locations**: Assign contiguous target block ranges starting from `data_block_start`. Directories placed first (to preserve parent pointers), then regular files.
4. **Build relocation units**: Each unit represents a contiguous run of blocks (or a `ChunkEntry` for seekable 64K files) with its owner (inode slot, indirect block slot, etc.).
5. **Round-based COW passes**:
   - **Step A**: Select a batch of units whose target ranges are currently free and whose WAL transaction fits within half the journal ring.
   - **Step B**: Copy payload blocks to free destinations; flush destination blocks **before WAL commit** in Strict durability mode.
   - **Step C**: Stage WAL metadata ops (inode pointer updates, indirect block pointer updates, data bitmap alloc/free) without modifying live metadata.
   - **Step D**: Commit single bounded WAL transaction.
   - **Step E**: Apply metadata updates via `apply_journal_ops` (same path as normal writes).
   - **Step F**: Sync metadata ranges per `DurabilityMode`, retire WAL ring (checkpoint), repeat until all units relocated.
6. **Post-pass**: Update free block hint, remap `dir_cache` entries, clear payload caches, run `verify_integrity`.

### Crash Recovery Guarantee

If the process crashes mid-round:
- The WAL transaction is either fully committed (and applied) or not committed at all.
- A crash **after commit but before apply** is resolved by journal replay on next mount.
- A crash **during apply** is also resolved by journal replay.
- **Payload moves are ordered before metadata commits**: destination blocks are flushed before the WAL transaction commits (in Strict mode) or are covered by the WAL payload (in RangeAsync mode), so a crash never leaves metadata pointing to uninitialized blocks.
- Freed source blocks are only marked free in the WAL after the payload is safely at its new location.

**Evidence:** [`defragment_inplace_journaled_cow`](repo://src/disk/maintenance.rs#L792-L1553), particularly the round loop and the `is_strict` flush before commit.

---

## Safe Defragmentation Pipeline (Detailed Stages)

The safe defragmentation process executes across six distinct stages:

### 1. Snapshot Creation

The entire active image file is copied via `std::fs::copy(source_path, &temp_path)`. The destination defaults to `{source_path}.defrag.tmp` when no output path is provided. This guarantees that if the defragmentation process is killed mid-stream, the original image file remains untouched and uncorrupted.

### 2. Inode Scan and Metadata Preservation

The temporary image is opened preserving the encryption key from the original image. All allocated inode indices are collected from the inode bitmap:

- **Directories**: Directory block addresses are harvested via `collect_inode_blocks` and staged in `directory_blocks`. These cannot simply be moved without updating parent pointer graphs.
- **Regular Files**: For each file with `size > 0`, the file's exact compression status (`is_compressed = inode.compressed_size > 0`) and pre-compression filter configuration (`FilterConfig`: `typesize`, `delta`, `shuffle`, `bitshuffle`) are recorded. **Seekable 64K chunked files** are detected by `INODE_FLAG_SEEKABLE_64K` and their `CompressionMode::Seekable` config is preserved.
- **Inode Reset**: The inode on disk is zeroed out (`size = 0`, `compressed_size = 0`, `blocks = [0; 12]`, `triple_indirect = 0`) via `write_inode`, freeing the blocks for contiguous reassignment.

### 3. Bitmap Reset

The entire data bitmap of the temporary image is wiped using `bitmap_slice.fill(0)`. Then, only the preserved directory block IDs (`directory_blocks`) are re-marked as allocated. All other data blocks become completely free and unallocated.

### 4. Contiguous File Rewrite

For each regular file, `write_data_with_filters` is invoked:
- Because the data bitmap was cleared and allocation starts from `data_block_start` using sequential hint-based allocation, each file receives a strictly contiguous sequence of data blocks.
- The original compression setting (`CompressionMode::Always` if previously compressed, `CompressionMode::Seekable` for chunked files, else `CompressionMode::Never`) and filter configuration are reapplied, preserving space efficiency without altering compression/filter semantics.
- Bytes moved and files processed counters are updated for final reporting.

### 5. Durability Flush

`temp_dm.flush()` commits all dirty pages and metadata to disk, and the temporary `DiskManager` handle is explicitly dropped to release memory maps and file descriptors.

### 6. Transactional 3-Step Atomic Rename

1. **Rename original → backup**: `std::fs::rename(source_path, "{source_path}.old")`
2. **Rename temp → original**: `std::fs::rename("{source_path}.defrag.tmp", source_path)`
3. **Verify**: Open the new image with the encryption key, run `analyze_fragmentation` and `verify_integrity`.
   - **On success**: Delete backup file, `reload_from_disk()` to synchronize the active manager handle with the new file.
   - **On failure**: Rollback by renaming backup → original, return error.

**Evidence:** [`defragment_safe`](repo://src/disk/maintenance.rs#L484-L640).

---

## Background Auto-Defragmentation Coordinator

OIFS includes a background coordinator that periodically measures fragmentation and runs in-place defragmentation passes when the configured threshold is exceeded.

### Configuration (`AutoDefragConfig`)

| Parameter | Default | Description |
|-----------|---------|-------------|
| `enabled` | `false` | Whether the coordinator is running |
| `fragmentation_threshold` | `0.25` | Fragmentation ratio above which a pass is triggered. Clamped to `0.0..=1.0`. `NaN` falls back to default. |
| `check_interval` | `300s` | How often to measure fragmentation. Minimum `10ms` (prevents hot loop). |
| `min_free_blocks` | `0` | Minimum free data blocks required before a pass is allowed (guards against compacting a nearly-full image). |
| `allow_journaled` | `true` | Whether the coordinator may run on WAL-enabled images. In-place defrag is fully journaled and atomic. |

### Enabling Auto-Defrag

**Programmatic:**
```rust
use oifs::auto_defrag::AutoDefragConfig;
use std::time::Duration;

let dm = DiskManager::open("/path/to/image.img", 0)?;
let config = AutoDefragConfig::new(0.25)           // trigger at 25% fragmentation
    .with_check_interval(Duration::from_secs(600)) // check every 10 minutes
    .with_min_free_blocks(1000)                    // need at least 1000 free blocks
    .with_allow_journaled(true);                   // allow on WAL-enabled images
dm.set_auto_defrag(config);
```

**Command-line (via `oifs`):**
```bash
# The CLI doesn't have a direct auto-defrag subcommand; configure via API or session
```

### Monitoring Auto-Defrag Stats

```rust
let stats = dm.auto_defrag_stats();
println!(
    "Checks: {}, Passes: {}, Skipped: {}, Errors: {}, Last ratio: {:.4}",
    stats.checks, stats.passes, stats.skipped, stats.errors, stats.last_fragmentation_ratio
);
if let Some(last_pass) = stats.last_pass {
    println!("Last pass: {} files, {} bytes moved, frag {:.2} -> {:.2}",
        last_pass.files_processed, last_pass.bytes_moved,
        last_pass.frag_before, last_pass.frag_after);
}
```

The coordinator runs as a daemon thread named `oifs-auto-defrag` that exits when the last `DiskManager` clone is dropped.

### Pass Skip Reasons

A pass is skipped (and counted in `skipped`) when:
- Fragmentation ratio is below `fragmentation_threshold`
- Free blocks < `min_free_blocks`
- Image is WAL-enabled and `allow_journaled = false`
- Any error during measurement (counted in `errors`)

**Evidence:** [`AutoDefragConfig`](repo://src/auto_defrag.rs#L1-L100), [`run_worker`](repo://src/auto_defrag.rs#L296-L360), [`skip_reason`](repo://src/auto_defrag.rs#L268-L294).

---

## DefragStats Output Interpretation

| Field | Meaning | Typical Values |
|-------|---------|----------------|
| `files_processed` | Number of regular files rewritten | Matches number of non-empty files |
| `bytes_moved` | Total payload bytes copied | Sum of file sizes processed |
| `blocks_freed` | Reduction in free runs (`frag_before.free_runs - frag_after.free_runs`) | ≥ 0; larger = more fragmentation eliminated |
| `frag_before` | Fragmentation ratio before pass | 0.0 (optimal) → 1.0 (max fragmented) |
| `frag_after` | Fragmentation ratio after pass | Should be ≤ `frag_before`; ideally near 0.0 |

**Note:** `frag_before` and `frag_after` are the **fragmentation ratio** values, not the raw `free_runs` count. A ratio of `0.0` means all free space is in one contiguous run; `1.0` means every free block is isolated.

---

## Safe vs In-Place Mode Trade-offs

| Concern | Safe Mode (`DefragMode::Safe`) | In-Place Mode (`DefragMode::InPlace`) |
|---------|-------------------------------|--------------------------------------|
| **Data safety** | Maximum — original image never modified until verification passes | WAL-atomic on journaled images; legacy permutation on non-journaled |
| **Disk space** | Needs 2× image size (temporarily) | No extra space needed |
| **Speed** | Full copy + full rewrite | Selective COW moves; faster on large, lightly fragmented images |
| **Encryption** | Fully supported | Fully supported |
| **Journaled images** | Uses copy + rename path | **Preferred** — leverages WAL for true atomicity |
| **Interruption** | Clean rollback via rename | Mid-pass crash → journal replay or no-op; image stays consistent |
| **Recommendation** | Default choice for most users; required for non-journaled images if safety is paramount | Use on large journaled images where 2× space is unavailable |

---

## Auto-Defrag Configuration Parameters and When to Enable

Enable auto-defrag when:
- The filesystem experiences heavy create/delete churn (fragmenting free space)
- You need large contiguous free runs for the contiguous block allocator or seekable 64K chunked compressor
- The image is on a WAL-enabled (journaled) filesystem and you set `allow_journaled = true`

Tune parameters based on workload:
- **Lower `fragmentation_threshold`** (e.g., 0.15) → more aggressive defragmentation, more I/O
- **Higher `check_interval`** (e.g., 1 hour) → less frequent checks, less overhead
- **`min_free_blocks`** → set to a meaningful fraction of total blocks (e.g., 5-10%) to avoid stalling on nearly-full images
- **`allow_journaled = false`** → disable on WAL images if you prefer to run defrag manually during maintenance windows

---

## Allow Journaled Flag and WAL-Atomic Guarantees

The `allow_journaled` flag in `AutoDefragConfig` (and the equivalent decision when calling `defragment` with `DefragMode::InPlace` on a journaled image) controls whether in-place defragmentation runs on WAL-enabled images.

When **enabled** (default):
- In-place defrag uses the **round-based COW algorithm** with WAL transactions
- Each round commits a single bounded WAL transaction (never overflowing the ring)
- Payload blocks are copied to free destinations **before** metadata commits
- Metadata updates (inode pointers, indirect blocks, data bitmap) are staged in the WAL and applied atomically via `apply_journal_ops`
- The WAL is checkpointed after each round, freeing ring space for the next round
- A crash at any point leaves the image consistent: journal replay either completes the round or rolls it back

When **disabled**:
- The coordinator skips in-place passes on journaled images
- Safe mode defragmentation still works (uses copy + rename, not WAL)

**Evidence:** [`allow_journaled`](repo://src/auto_defrag.rs#L38-L40), [`defragment_inplace_journaled_cow`](repo://src/disk/maintenance.rs#L792-L1553), [`skip_reason`](repo://src/auto_defrag.rs#L283-L291).

---

## Verification and Testing

After any defragmentation pass (safe or in-place), the system runs `verify_integrity` (fsck) which checks:
- No orphan inodes (allocated but unreferenced)
- No leaked blocks (allocated in bitmap but not referenced by any inode)
- No missing blocks (referenced by inode but not allocated in bitmap)
- No cross-linked blocks (referenced by multiple inodes)

If verification fails:
- **Safe mode**: Automatic rollback via rename (backup → original)
- **In-place mode**: Error returned; image state is whatever the WAL last committed (consistent by construction)

**Evidence:** [`verify_integrity_on`](repo://src/disk/maintenance.rs#L1555-L1711), [`defragment_safe`](repo://src/disk/maintenance.rs#L600-L640), [`defragment_inplace_on`](repo://src/disk/maintenance.rs#L700-L720).

---

## Related Pages

- [Block Allocation Architecture](../architecture/block_allocation.md) — contiguous allocation and hint-based allocator
- [Defragmentation Architecture](../architecture/defragmentation.md) — deeper architectural details
- [Journal Architecture](../architecture/journal.md) — WAL design and atomicity guarantees
- [Testing and Verification](../operations/testing_and_verification.md) — verification procedures
