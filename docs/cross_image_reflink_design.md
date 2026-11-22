# OIFS Cross-Image Fast Transfer & CoW Reflink Design

## 1. Motivation & Goals

Within a single OIFS `.img` container, `DiskManager::rename` moves files and directories in $O(1)$ metadata-only time. However, workflows such as splitting a 100 GB dataset image into `train.img` / `val.img`, merging per-worker output images, or cloning an image for isolated sandboxes require transferring files **across two independent `.img` containers (`A.img` → `B.img`)**.

This document designs a **3-tier zero-decompression / zero-copy cross-image transfer pipeline** that leverages host OS CoW reflink primitives whenever supported, while preserving **100% self-contained single-file portability** for both `A.img` and `B.img`.

---

## 2. Host OS & Filesystem Support Matrix

Because OIFS aligns all data blocks to `4096` bytes (`BLOCK_SIZE`), its physical extents naturally satisfy host filesystem reflink alignment requirements.

| Host OS / Filesystem | Whole-Image Fork (`A.img` → `B.img`) | Sub-Image Range Copy/Move (`A.img[off]` → `B.img[off]`) | Notes |
| :--- | :--- | :--- | :--- |
| **Linux: Btrfs / XFS (`reflink=1`) / bcachefs / NFSv4.2** | ✅ `ioctl(FICLONE)` (0 copy, 0 extra space) | ✅ `ioctl(FICLONERANGE)` (0 copy, 0 extra space) | Both images remain 100% independent; deleting `A.img` never affects `B.img`. |
| **Linux: LustreFS (with Lreflink)** | ✅ `ioctl(FICLONE)` (preserves OST stripe layout) | ⚠️ `ioctl(FICLONERANGE)` on **same OST**; falls back across OSTs | Lreflink intercepts standard `FICLONE`/`FICLONERANGE` VFS ioctls, but enforces **No Cross-OST Cloning**. Unstriped (`stripe_count=1`) images on the same OST hit zero-copy; cross-OST ranges return `EXDEV`/`EOPNOTSUPP` and fall back cleanly. |
| **Linux: ext4 / Lustre (without Lreflink)** | ❌ (standard copy) | ⚠️ `copy_file_range(2)` (kernel/server-side copy, 0 decompress/recompress) | Skips Zstd decompression, inverse SIMD filters, and re-compression. |
| **macOS: APFS** | ✅ `clonefile(2)` (`< 1 ms`, 0 extra space) | ⚠️ Raw compressed block `pread`/`pwrite` | APFS supports whole-file `clonefile(2)` only (no public sub-range `FICLONERANGE` syscall). Still skips all decompression/re-compression CPU work. |

---

## 3. Three-Tier Automatic Fallback Pipeline

For each contiguous physical block extent `(src_blk, dst_blk, block_count)` of a transferred file (`src_off = src_blk * 4096`, `dst_off = dst_blk * 4096`, `len = block_count * 4096`):

1. **Tier 1 — Zero-Copy CoW Reflink (`ioctl(dst_fd, FICLONERANGE, &range)`)**:
   - Available on Linux Btrfs, XFS (`reflink=1`), bcachefs, NFSv4.2, and LustreFS with Lreflink (same-OST extents).
   - Updates only host filesystem CoW extent metadata: **0 bytes disk I/O, 0 extra disk space**.
2. **Tier 2 — Kernel-Side Copy (`copy_file_range(2)`)**:
   - Triggered automatically when Tier 1 returns `EOPNOTSUPP`, `EXDEV`, or `EINVAL`.
   - Avoids copying compressed bytes into userspace buffers; on Lustre/NFSv4.2 can leverage server-side copy.
3. **Tier 3 — Raw Compressed Extent Transfer (`pread` + `pwrite`)**:
   - Portable fallback (macOS APFS, older kernels).
   - Transfers raw on-disk 4 KiB blocks directly without decompressing Zstd streams or reversing SIMD numeric filters (**90%+ CPU savings** vs `read_data` → `write_data`).

---

## 4. Core Algorithm & Technical Invariants

### 4.1 Transfer Steps (`A.img` → `B.img`)
1. **Flush & Inspect Source (`A.img`)**:
   - Flush `A.img` so all dirty pages are committed to the underlying OS file before `FICLONERANGE` / `copy_file_range` reads from `src_fd`.
   - Read source `Inode` metadata (`size`, `compressed_size`, `compression`, `filters`, `flags`, `seekable`).
   - Collect the file's physical data extents via `ExtentList` (for standard/stream files) or decoded `ChunkEntry` runs (for Seekable 64K files).
2. **Encrypted Image Boundary Check**:
   - Because each encrypted `.img` has a unique Argon2id salt/master key and binds AEAD ciphertext AAD to `inode_id`, raw ciphertext blocks cannot be reflinked across differently-keyed encrypted images.
   - When either `A.img` or `B.img` is encrypted, automatically fall back to logical decrypt (`A.img`) → re-encrypt (`B.img`).
   - *(Note: Whole-image `clonefile`/`FICLONE` works for encrypted images because the cloned image retains the exact same salt, key, and `inode_id`s.)*
3. **Allocate & Transfer Extents into Destination (`B.img`)**:
   - Allocate matching contiguous block runs in `B.img` (using `AllocSim` on journaled images).
   - Run the 3-tier pipeline (`FICLONERANGE` → `copy_file_range` → raw `pread`/`pwrite`) for each `(src_extent, dst_extent)` pair.
   - For Seekable 64K files, remap every `ChunkEntry`'s physical start block to its new `dst_blk` in `B.img` (reusing the `ChunkEntry` remapping logic from `src/disk/maintenance.rs`).
4. **Page-Cache & Storage-Mode Coherence**:
   - `StorageMode::FileIo`: Invalidate `B.img`'s `DataPageCache` entries for all newly populated `dst_blk`s after `FICLONERANGE` / `copy_file_range`.
   - `StorageMode::Mmap`: Ensure `A.img` is `msync`'d beforehand and `B.img`'s target range is coherent (`msync` / `madvise(MADV_DONTNEED)` where needed) so memory-mapped reads immediately observe the cloned extents.
5. **Commit Destination Metadata (`B.img`)**:
   - On journaled `B.img`, commit the new Inode, indirect/chunk-table blocks, bitmap updates, and directory entry in a **single atomic WAL transaction**, then `flush()`.
6. **Optional Source Deletion (`move` Mode) & Space Reclamation**:
   - Cross-image `move` follows **At-Least-Once crash safety**: only after `B.img` has committed and `flush()`'d successfully does OIFS call `delete_file` on `A.img`. A crash mid-move never loses data (at worst both images temporarily retain the file).
   - Optional host-space reclamation on `A.img`: contiguous freed data extents can optionally be released back to the host OS via `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)` on Linux.

---

## 5. Proposed API & CLI Surface

### Rust API
- `DiskManager::clone_image(src_path, dst_path) -> Result<CloneMethod, DiskManagerError>`
  - Whole-image CoW fork (`clonefile` on macOS APFS, `FICLONE` on Linux Btrfs/XFS/Lustre Lreflink, standard copy fallback).
- `DiskManager::transfer_file_to(src_parent, src_name, dst: &DiskManager, dst_parent, dst_name, opts: TransferOptions) -> Result<TransferStats, DiskManagerError>`
  - Supports `copy` and `move_file: bool`, reporting bytes transferred and tier used (`Reflink`, `CopyFileRange`, `RawBlockCopy`, or `LogicalReencode`).

### CLI
```bash
# Instant whole-image CoW fork (macOS APFS clonefile / Linux FICLONE)
oifs clone source.img fork.img

# Fast partial copy or move from A.img to B.img (3-tier reflink -> copy_file_range -> raw)
oifs transfer --from A.img --to B.img /datasets/shard_01.bin --dest /train/shard_01.bin
oifs transfer --from A.img --to B.img /datasets/val --dest /val --recursive --move
```

---

## 6. Verification Plan
1. **Cross-image unit & integration tests (`tests/transfer_test.rs`)**:
   - Plain, Zstd stream, Seekable 64K, SIMD-filtered, and encrypted images across `Mmap` and `FileIo`.
   - Verify bit-for-bit readback, `fsck` (`verify_integrity`) clean on both `A.img` and `B.img`, and `TransferStats` tier reporting.
2. **Host-FS reflink E2E test (`#[ignore]`)**:
   - On Linux Btrfs/XFS/Lustre-Lreflink: assert `TransferMethod::Reflink` is chosen and `B.img` shares extents with `A.img` (e.g., via `FIEMAP` or block usage) while remaining readable after deleting `A.img`.
   - On macOS APFS: verify `oifs clone` succeeds via `clonefile(2)` and produces an independent writable fork.

---

## 7. Reference Implementations

The following production open-source codebases serve as concrete references for errno handling, kernel quirks, and extent-sharing verification:

- **QEMU (`block/file-posix.c` — `raw_co_copy_range_from` / `raw_co_copy_range_to`)**: Closest architectural match to OIFS—performs 4 KiB-aligned sub-range copies between single-file disk images using `copy_file_range(2)` with short-copy loops and fallback.
- **Rust `uutils/coreutils` (`src/uu/cp/src/platform/linux.rs` & `macos.rs`)**: Idiomatic Rust `libc::ioctl(FICLONE/FICLONERANGE)` → `copy_file_range(2)` → userspace fallback, including Linux errno handling (`EXDEV`, `EOPNOTSUPP`, `EINVAL`, `ENOSYS`, `ENOTTY`, `ETXTBSY`) and macOS `clonefile(2)`.
- **Rust Standard Library (`library/std/src/sys/fs/unix.rs` — `kernel_copy`)**: Battle-tested `copy_file_range(2)` wrapper with `ENOSYS` capability caching and partial-copy retry handling.
- **`nicokoch/reflink-copy` (Rust crate)**: Compact cross-platform whole-file reflink reference (`FICLONE` on Linux, `clonefile` on macOS, `FSCTL_DUPLICATE_EXTENTS_TO_FILE` on Windows).
- **GNU `coreutils` (`src/copy.c`)**: Gold standard for sparse-file hole detection (`SEEK_DATA`/`SEEK_HOLE`) combined with `FICLONE` and `copy_file_range` fallback.
- **`duperemove` / `xfs_io -c fiemap`**: Reference for Linux `FS_IOC_FIEMAP` ioctl to verify in automated tests that `A.img` and `B.img` genuinely share physical disk extents after `FICLONERANGE`.
- **Linux Kernel (`fs/read_write.c` — `vfs_copy_file_range`)**: Note that Linux 5.3+ `copy_file_range(2)` internally attempts `remap_file_range()` on Btrfs/XFS; however, OIFS explicitly invokes `FICLONERANGE` first so `TransferStats` accurately distinguishes true zero-copy CoW reflink from full data copies.

