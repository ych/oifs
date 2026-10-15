---
type: architecture
title: Online Defragmentation
description: How OIFS analyzes filesystem fragmentation using 64-bit bitmap scanning and performs safe defragmentation via an out-of-place rebuild with preserved compression, filters, and a transactional 3-step atomic rename with rollback.
tags: [defragmentation, fragmentation, atomic-rename, contiguous-allocation, filters, safe-mode]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-15T13:11:08.406Z
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-15T13:11:08.406Z" }
---

## Responsibility and ownership

<!-- openwiki: broken internal link [src/disk.rs#L6377-L6602] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L6126-L6330] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L6377-L6393] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Defragmentation and space reorganization logic reside entirely within [`DiskManager`](src/disk.rs#L6377-L6602). It evaluates disk layout health through [`analyze_fragmentation`](src/disk.rs#L6126-L6330), which measures free-block scattering across physical data space. Defragmentation execution is exposed via [`DiskManager::defragment`](src/disk.rs#L6377-L6393), supporting two operational modes:
- `DefragMode::Safe` (default): An out-of-place reorganization in a temporary image followed by transactional verification and rename.
- `DefragMode::InPlace`: Direct modification of the existing image (implemented).

## Fragmentation analysis

<!-- openwiki: broken internal link [src/disk.rs#L6126-L6330] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/bitmap.rs#L2-L111] file "src/bitmap.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Before and after defragmentation, [`DiskManager::analyze_fragmentation`](src/disk.rs#L6126-L6330) inspects the filesystem's `data_bitmap_block` using a read-only [`BitmapRef`](src/bitmap.rs#L2-L111).

### 64-bit word scanning

Instead of checking bit by bit, the analyzer slices the bitmap into 8-byte chunks (`chunks_exact(8)`) and decodes them as little-endian 64-bit words via `u64::from_le_bytes` (`src/disk.rs#L6148-L6187`):
- `word == 0`: All 64 data blocks are completely free. If not already tracking a free run, increments `free_runs` and extends `current_run_len` by 64.
- `word == u64::MAX`: All 64 blocks are fully allocated. Closes any open free run, recording `largest_free_run` and accumulating `total_gap_size`.
- Mixed word: Uses `word.count_ones()` to quickly tabulate allocated blocks, then iterates through each bit (`0..64`) with bitwise masks to track individual block boundaries and transitions.
- Trailing bits: Any leftover bits (`total_blocks % 64`) are checked at the end using `bitmap.get(i)` (`src/disk.rs#L6190-L6210`).

### Metrics and fragmentation ratio

<!-- openwiki: broken internal link [src/disk.rs#L103-L118] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The resulting [`FragmentationStats`](src/disk.rs#L103-L118) tracks:
- `total_blocks`, `used_blocks`, `free_blocks`.
- `free_runs`: Number of separate contiguous sequences of free blocks.
- `largest_free_run`: The size (in blocks) of the largest contiguous free chunk.
- `avg_gap_size`: `total_gap_size / free_runs` (average size of free runs).
- `fragmentation_ratio`: Calculated as:
  $$\text{fragmentation\_ratio} = \frac{\text{free\_runs} - 1}{\max(\text{free\_blocks} - 1, 1)}$$
  If all free space forms a single contiguous run (`free_runs <= 1`), the ratio is `0.0` (optimal). If every free block is completely isolated (`free_runs == free_blocks`), the ratio approaches `1.0` (maximally fragmented).

## Safe defragmentation pipeline

<!-- openwiki: broken internal link [src/disk.rs#L6444-L6602] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`defragment_safe`](src/disk.rs#L6444-L6602) reorganizes the filesystem out-of-place to protect against crashes, power failures, or corruption. The process executes across six distinct stages:

```
[Original Image] ──(fs::copy)──> [Temp Copy (.defrag.tmp)]
                                         │
                 ┌───────────────────────┴───────────────────────┐
                 │ 1. Scan allocated inodes & preserve dir blocks│
                 │ 2. Extract file data + compression/filter cfg │
                 │ 3. Clear data bitmap (restore only dir blocks)│
                 │ 4. Reallocate & rewrite files contiguously    │
                 │ 5. Flush temp image                           │
                 └───────────────────────┬───────────────────────┘
                                         │
[Original] ──rename──> [Backup (.old)]   │
                       [Original] <──rename── [Temp Copy]
                            │
               Verification OK?
               ├── Yes ──> Delete Backup (.old)
               └── No  ──> Rollback: Restore Backup (.old) -> [Original]
```

### 1. Snapshot creation

The entire active image file is copied via `std::fs::copy(source_path, &temp_path)` (`src/disk.rs#L6460`). The destination defaults to `{source_path}.defrag.tmp` when no output path is provided. This guarantees that if the defragmentation process is killed mid-stream, the original image file remains untouched and uncorrupted.

### 2. Inode scan and metadata preservation

<!-- openwiki: broken internal link [src/bitmap.rs#L94-L108] file "src/bitmap.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The temporary image is opened via `DiskManager::open_with_key(&temp_path, 0, enc_key)` (`src/disk.rs#L6464`), preserving the encryption key from the original image. Using [`BitmapRef::for_each_set_bit`](src/bitmap.rs#L94-L108), all allocated inode indices are collected from `inode_bitmap_block` (`src/disk.rs#L6475-L6480`):
- **Directories**: Directory blocks cannot simply be moved without updating parent pointer graphs, so directory block addresses are harvested via `collect_inode_blocks` and staged in `directory_blocks` (`src/disk.rs#L6486-L6489`).
- **Regular Files**: For each file with `size > 0`, the file's exact compression status (`is_compressed = inode.compressed_size > 0`) and pre-compression filter configuration ([`FilterConfig`]: `typesize`, `delta`, `shuffle`, `bitshuffle`) are recorded (`src/disk.rs#L6491-L6497`).
- **Inode Reset**: The inode on disk is zeroed out (`size = 0`, `compressed_size = 0`, `blocks = [0; 12]`, `triple_indirect = 0`) via `write_inode` (`src/disk.rs#L6500-L6506`), freeing the blocks for contiguous reassignment.

### 3. Bitmap reset

The entire data bitmap of the temporary image is wiped using `bitmap_slice.fill(0)` (`src/disk.rs#L6518`). Then, only the preserved directory block IDs (`directory_blocks`) are re-marked as allocated (`src/disk.rs#L6520-L6526`). All other data blocks become completely free and unallocated.

### 4. Contiguous file rewrite

For each regular file, `write_data_with_filters` is invoked (`src/disk.rs#L6542`):
- Because the data bitmap was cleared and allocation starts from `data_block_start` using sequential hint-based allocation, each file receives a strictly contiguous sequence of data blocks.
- The original compression setting (`CompressionMode::Always` if previously compressed, else `CompressionMode::Never`) and filter configuration are reapplied, preserving space efficiency without altering compression/filter semantics.
- Bytes moved and files processed counters are updated for final reporting.

### 5. Durability flush

`temp_dm.flush()` commits all dirty pages and metadata to disk, and the temporary `DiskManager` handle is explicitly dropped to release memory maps and file descriptors (`src/disk.rs#L6547-L6548`).

## Transactional 3-step atomic rename and rollback

To swap the defragmented image in place without risking downtime or data loss, `defragment_safe` utilizes a 3-step atomic rename with verification and rollback (`src/disk.rs#L6550-L6602`):

1. **Rename to backup**: `std::fs::rename(source_path, &backup_path)` renames the original image to `{source_path}.old`. On POSIX filesystems, this is an atomic directory entry operation (`src/disk.rs#L6554`).
2. **Promote defragmented image**: `std::fs::rename(&temp_path, source_path)` atomically renames the temporary defragmented file to the original path (`src/disk.rs#L6557`).
3. **Verification and cleanup**:
   - The new image is opened with `DiskManager::open_with_key` to verify it's valid (`src/disk.rs#L6560`).
<!-- openwiki: broken internal link [src/disk.rs#L6126-L6330] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/disk.rs#L6882-L6920] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
   - [`analyze_fragmentation`](src/disk.rs#L6126-L6330) and [`verify_integrity`](src/disk.rs#L6882-L6920) are run to ensure correctness (`src/disk.rs#L6562-L6564`).
   - If verification passes: the backup is deleted (`src/disk.rs#L6574`) and the active manager is reloaded via `reload_from_disk` to synchronize with the new file (`src/disk.rs#L6577`).
   - If verification fails at any point: the backup is renamed back to the original path to restore the pre-defragmentation state (`src/disk.rs#L5665,L6591,L6598`).
