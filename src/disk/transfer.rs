//! Cross-image fast file transfer, sub-range splice/move, and whole-image CoW cloning.
//!
//! Implements the 3-tier automatic fallback pipeline for transferring file extents
//! between two independent OIFS `.img` containers (`A.img` -> `B.img`):
//!
//! 1. **Tier 1 (`Reflink`)**: Linux `ioctl(FICLONERANGE)` for zero-copy, zero-space CoW
//!    extent cloning on Btrfs, XFS (`reflink=1`), bcachefs, NFSv4.2, and Lustre Lreflink.
//! 2. **Tier 2 (`CopyFileRange`)**: Linux `copy_file_range(2)` for kernel/server-side copy
//!    without userspace bounce buffers (ext4, Lustre cross-OST).
//! 3. **Tier 3 (`RawBlockCopy`)**: Portable raw compressed-extent `pread`/`pwrite` transfer
//!    (macOS APFS, fallback kernels) that skips Zstd decompression, inverse SIMD filters,
//!    and re-compression.
//!
//! When either source or destination image is encrypted (independent Argon2id salts and
//! inode-bound AEAD AAD), automatically falls back to `LogicalReencode`.
//!
//! Also provides [`DiskManager::splice_range_to`] for cross-image sub-range copy,
//! `PunchHole`, and `CollapseRange` with Sandwich (`Head / Body / Tail`) alignment splitting
//! and two-image `Copy-Commit-Then-Trim` crash safety.

use super::*;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::OnceLock;

#[cfg(test)]
std::thread_local! {
    static TRANSFER_STAGE1_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static SPLICE_BEFORE_DST_COMMIT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static COLLAPSE_BEFORE_WRITE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
#[inline]
fn run_transfer_stage1_hook() {
    TRANSFER_STAGE1_HOOK.with(|cell| {
        if let Some(cb) = cell.borrow_mut().take() {
            cb();
        }
    });
}

#[cfg(test)]
#[inline]
fn run_splice_before_dst_commit_hook() {
    SPLICE_BEFORE_DST_COMMIT_HOOK.with(|cell| {
        if let Some(cb) = cell.borrow_mut().take() {
            cb();
        }
    });
}

#[cfg(test)]
#[inline]
fn run_collapse_before_write_hook() {
    COLLAPSE_BEFORE_WRITE_HOOK.with(|cell| {
        if let Some(cb) = cell.borrow_mut().take() {
            cb();
        }
    });
}

/// Method used by [`DiskManager::clone_image`] to fork a whole `.img` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneMethod {
    /// macOS APFS `clonefile(2)` O(1) CoW clone.
    ApfsClonefile,
    /// Linux `ioctl(FICLONE)` O(1) CoW clone (Btrfs / XFS / bcachefs / Lustre Lreflink).
    LinuxFiclone,
    /// Standard byte copy fallback.
    StandardCopy,
}

/// Transfer tier selected by the 3-tier cross-image transfer pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferMethod {
    /// Zero-copy CoW extent clone via Linux `ioctl(FICLONERANGE)`.
    Reflink,
    /// Kernel/server-side copy via Linux `copy_file_range(2)` (no decompression).
    CopyFileRange,
    /// Portable raw compressed-extent `pread`/`pwrite` copy (no decompression).
    RawBlockCopy,
    /// Logical decrypt/read + re-encrypt/write fallback (encrypted image boundary or unaligned RMW).
    LogicalReencode,
}

impl TransferMethod {
    /// Conservative tier rank (`0 = Reflink` fastest, `3 = LogicalReencode` fallback).
    #[inline]
    pub const fn tier_rank(self) -> u8 {
        match self {
            TransferMethod::Reflink => 0,
            TransferMethod::CopyFileRange => 1,
            TransferMethod::RawBlockCopy => 2,
            TransferMethod::LogicalReencode => 3,
        }
    }
}

/// Classification of an `ioctl(FICLONERANGE)` error code in the 3-tier transfer pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflinkFallback {
    /// Host device pair does not support reflink (`EOPNOTSUPP`, `ENOTTY`, `EXDEV`, `ENOSYS`):
    /// record `(src_dev, dst_dev)` in the negative cache and fall back to Tier 2/3.
    CacheUnsupportedDevicePair,
    /// Extent-local or transient rejection (`EINVAL` such as host FS block-size alignment
    /// mismatch, `EPERM`, `ETXTBSY`): fall back to Tier 2/3 for this extent only without
    /// poisoning the device-pair cache.
    FallbackThisExtentOnly,
    /// Unrecoverable I/O error (e.g. `ENOSPC`, `EIO`, `EBADF`): abort the transfer.
    Fatal,
}

/// A maximal contiguous run of logical file blocks mapped to contiguous physical blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicalRun {
    /// Starting logical block index within the file.
    pub logical_start: usize,
    /// Starting physical block ID in the source image (`!= 0`).
    pub phys_start: u64,
    /// Number of contiguous blocks in this run (`>= 1`).
    pub count: u64,
}

/// Options controlling [`DiskManager::transfer_file_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransferOptions {
    /// If `true`, deletes the source entry after the destination commits and flushes.
    pub move_file: bool,
    /// If `true`, replaces an existing regular file at the destination path.
    pub overwrite: bool,
}

/// Summary statistics returned by [`DiskManager::transfer_file_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferStats {
    /// Source inode ID in the source image.
    pub src_inode: u64,
    /// Newly created (or moved) inode ID in the destination image.
    pub dst_inode: u64,
    /// Logical file size in bytes.
    pub logical_bytes: u64,
    /// Physical payload bytes transferred across images (compressed/raw block extent total).
    pub physical_bytes: u64,
    /// Number of contiguous block extents transferred.
    pub extents_transferred: usize,
    /// Most conservative transfer tier used across the transferred extents.
    pub method: TransferMethod,
}

/// Source-range trim policy executed after [`DiskManager::splice_range_to`] commits and flushes
/// the destination image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpliceTrimMode {
    /// Leave the source file untouched (sub-range copy).
    #[default]
    Keep,
    /// Clear/zero `[src_off, src_off + len)` in the source file while keeping `src_inode.size`
    /// unchanged. Full aligned 4 KiB / 64 KiB units clear pointers to `0`, free OIFS blocks,
    /// and issue best-effort Linux `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)`.
    PunchHole,
    /// Remove `[src_off, src_off + len)` from the source file and shrink `src_inode.size` by
    /// `len`. When `src_off` and `len` are unit-aligned on an unencrypted file, shifts trailing
    /// block/chunk pointers left in O(remaining units) with zero payload copying.
    CollapseRange,
}

/// Options controlling [`DiskManager::splice_range_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpliceOptions {
    /// How the source range `[src_off, src_off + len)` is handled after `dst` commits & flushes.
    pub trim_mode: SpliceTrimMode,
}

/// Summary statistics returned by [`DiskManager::splice_range_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpliceStats {
    /// Source inode ID.
    pub src_inode: u64,
    /// Destination inode ID.
    pub dst_inode: u64,
    /// Total logical bytes spliced (clamped to available source bytes from `src_off`).
    pub bytes_spliced: u64,
    /// Logical bytes transferred via unaligned Head/Tail logical RMW.
    pub logical_rmw_bytes: u64,
    /// Aligned body logical bytes transferred via the 3-tier fast path.
    pub body_fast_path_bytes: u64,
    /// Physical payload bytes copied/reflinked across images in the body fast path.
    pub physical_bytes: u64,
    /// Number of contiguous physical extents transferred in the body fast path.
    pub extents_transferred: usize,
    /// Number of source OIFS blocks freed by `PunchHole` or `CollapseRange`.
    pub src_blocks_freed: usize,
    /// Transfer tier achieved by the body fast path (or `LogicalReencode` if pure logical).
    pub method: TransferMethod,
}

/// Sandwich (`Head / Body / Tail`) partition of `[src_off, src_off + len)` ->
/// `[dst_off, dst_off + len)` at alignment granularity `unit_size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplicePartition {
    /// Source offset of the unaligned head segment (`== src_off`).
    pub head_src_off: u64,
    /// Destination offset of the unaligned head segment (`== dst_off`).
    pub head_dst_off: u64,
    /// Byte length of the unaligned head segment (`0` if body starts immediately).
    pub head_len: u64,
    /// Source offset of the aligned body segment (`head_src_off + head_len`).
    pub body_src_off: u64,
    /// Destination offset of the aligned body segment (`head_dst_off + head_len`).
    pub body_dst_off: u64,
    /// Byte length of the aligned body segment (multiple of `unit_size`, or `0`).
    pub body_len: u64,
    /// Source offset of the unaligned tail segment (`body_src_off + body_len`).
    pub tail_src_off: u64,
    /// Destination offset of the unaligned tail segment (`body_dst_off + body_len`).
    pub tail_dst_off: u64,
    /// Byte length of the unaligned tail segment.
    pub tail_len: u64,
}

#[derive(Default)]
struct TierTracker {
    combined: Option<TransferMethod>,
    used_kernel_extent_op: bool,
}

impl TierTracker {
    #[inline]
    fn record(&mut self, m: TransferMethod) {
        if matches!(m, TransferMethod::Reflink | TransferMethod::CopyFileRange) {
            self.used_kernel_extent_op = true;
        }
        self.combined = Some(match self.combined {
            Some(prev) => combine_transfer_tier(prev, m),
            None => m,
        });
    }

    #[inline]
    fn finish(&self) -> TransferMethod {
        self.combined.unwrap_or(TransferMethod::RawBlockCopy)
    }
}

/// Returns `true` ONLY when both source and destination are unencrypted, permitting
/// the raw extent fast path (`FICLONERANGE` -> `copy_file_range` -> `RawBlockCopy`).
///
/// Whenever either side is encrypted, returns `false` so the caller forces
/// [`TransferMethod::LogicalReencode`] (decrypt + re-encrypt bound to the destination
/// inode ID and chunk index).
#[inline]
pub const fn should_use_raw_fast_path(src_encrypted: bool, dst_encrypted: bool) -> bool {
    !src_encrypted && !dst_encrypted
}

/// Returns the aligned fast-path unit size (`Some(4096)` for raw uncompressed files,
/// `Some(65536)` for Seekable 64K files with identical filter metadata) when both
/// source and destination inodes are eligible for sub-range block/chunk splicing.
///
/// Returns `None` (forcing logical `read_at` + `write_data`) if:
/// - Either side or either inode is encrypted;
/// - Either inode is stream-compressed (`compressed_size > 0` without `INODE_FLAG_SEEKABLE_64K`);
/// - Source and destination have mismatched storage layouts (e.g. Raw vs Seekable 64K);
/// - Seekable 64K inodes have different pre-compression filter metadata
///   (`filter_typesize`, `filter_delta`, `filter_shuffle`, `filter_bitshuffle`);
/// - Or either inode is not a regular file (`FileType::File`).
#[inline]
pub fn can_splice_fast_path(
    src_inode: &Inode,
    dst_inode: &Inode,
    src_encrypted: bool,
    dst_encrypted: bool,
) -> Option<u64> {
    if !should_use_raw_fast_path(
        src_encrypted || src_inode.encrypted,
        dst_encrypted || dst_inode.encrypted,
    ) {
        return None;
    }
    if src_inode.mode != crate::inode::FileType::File
        || dst_inode.mode != crate::inode::FileType::File
    {
        return None;
    }
    let src_seekable = (src_inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
    let dst_seekable = (dst_inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;

    if src_seekable && dst_seekable {
        let same_filters = src_inode.filter_typesize == dst_inode.filter_typesize
            && src_inode.filter_delta == dst_inode.filter_delta
            && src_inode.filter_shuffle == dst_inode.filter_shuffle
            && src_inode.filter_bitshuffle == dst_inode.filter_bitshuffle;
        if same_filters {
            return Some(crate::inode::CHUNK_SIZE_64K as u64);
        }
        return None;
    }
    let src_has_filters =
        src_inode.filter_delta || src_inode.filter_shuffle || src_inode.filter_bitshuffle;
    let dst_has_filters =
        dst_inode.filter_delta || dst_inode.filter_shuffle || dst_inode.filter_bitshuffle;
    if !src_seekable
        && !dst_seekable
        && src_inode.compressed_size == 0
        && dst_inode.compressed_size == 0
        && !src_has_filters
        && !dst_has_filters
    {
        return Some(BLOCK_SIZE as u64);
    }
    None
}

/// Returns `true` when raw block/chunk transfer between `src` and `dst` will produce
/// blocks readable by `dst` under its currently registered Zstd dictionaries.
///
/// Whenever `src` holds a compressed payload and registers dictionaries not present
/// (with identical content) in `dst`, raw transfer falls back to logical re-encoding so
/// `dst` never ends up with Zstd frames referencing an unknown `dict_id`.
#[inline]
pub fn dicts_compatible_for_raw_copy(
    src_is_compressed: bool,
    src_dicts: &crate::zstd_pool::ZstdDictRegistry,
    dst_dicts: &crate::zstd_pool::ZstdDictRegistry,
) -> bool {
    !src_is_compressed || !src_dicts.has_any() || src_dicts.is_subset_of(dst_dicts)
}

/// Plans a Sandwich (`Head / Body / Tail`) partition for splicing `len` bytes from
/// `src_off` to `dst_off` at alignment granularity `unit_size`.
///
/// Returns `None` if `src_off + len` or `dst_off + len` overflows `u64`.
/// When `unit_size == 0`, `len == 0`, `src_off % unit_size != dst_off % unit_size`, or
/// fewer than one full aligned unit fits inside the range, `body_len == 0` and the entire
/// range is placed in `head` for logical RMW.
pub fn plan_splice_partition(
    src_off: u64,
    dst_off: u64,
    len: u64,
    unit_size: u64,
) -> Option<SplicePartition> {
    let src_end = src_off.checked_add(len)?;
    let dst_end = dst_off.checked_add(len)?;

    let fallback = || SplicePartition {
        head_src_off: src_off,
        head_dst_off: dst_off,
        head_len: len,
        body_src_off: src_end,
        body_dst_off: dst_end,
        body_len: 0,
        tail_src_off: src_end,
        tail_dst_off: dst_end,
        tail_len: 0,
    };

    if len == 0 || unit_size == 0 {
        return Some(fallback());
    }
    let src_rem = src_off % unit_size;
    let dst_rem = dst_off % unit_size;
    if src_rem != dst_rem {
        return Some(fallback());
    }

    let head_len = if src_rem == 0 { 0 } else { unit_size - src_rem };
    if head_len >= len {
        return Some(fallback());
    }

    let rem_after_head = len - head_len;
    let body_len = (rem_after_head / unit_size) * unit_size;
    if body_len == 0 {
        return Some(fallback());
    }
    let tail_len = rem_after_head - body_len;

    let body_src_off = src_off + head_len;
    let body_dst_off = dst_off + head_len;
    let tail_src_off = body_src_off + body_len;
    let tail_dst_off = body_dst_off + body_len;

    Some(SplicePartition {
        head_src_off: src_off,
        head_dst_off: dst_off,
        head_len,
        body_src_off,
        body_dst_off,
        body_len,
        tail_src_off,
        tail_dst_off,
        tail_len,
    })
}

/// Combines two [`TransferMethod`] tiers, returning the more conservative (higher-rank) tier.
///
/// Forms a commutative, associative, idempotent, monotone join semilattice with
/// [`TransferMethod::Reflink`] as identity and [`TransferMethod::LogicalReencode`] as top.
#[inline]
pub const fn combine_transfer_tier(a: TransferMethod, b: TransferMethod) -> TransferMethod {
    if a.tier_rank() >= b.tier_rank() { a } else { b }
}

/// Classifies an OS `errno` returned by `ioctl(FICLONERANGE)`.
#[inline]
pub const fn classify_reflink_errno(errno: i32) -> ReflinkFallback {
    match errno {
        libc::EOPNOTSUPP | libc::ENOTTY | libc::EXDEV | libc::ENOSYS => {
            ReflinkFallback::CacheUnsupportedDevicePair
        }
        #[cfg(not(target_os = "linux"))]
        libc::ENOTSUP => ReflinkFallback::CacheUnsupportedDevicePair,
        libc::EINVAL | libc::EPERM | libc::ETXTBSY => ReflinkFallback::FallbackThisExtentOnly,
        _ => ReflinkFallback::Fatal,
    }
}

/// Rebases a [`crate::inode::ChunkEntry`] onto a newly allocated destination `start_block`,
/// preserving `block_count`, `flags`, and `compressed_len`.
///
/// Returns [`crate::inode::ChunkEntry::EMPTY`] when `entry.is_empty()`.
#[inline]
pub fn rebase_chunk_entry(
    entry: crate::inode::ChunkEntry,
    new_start_block: u32,
) -> crate::inode::ChunkEntry {
    if entry.is_empty() {
        return crate::inode::ChunkEntry::EMPTY;
    }
    crate::inode::ChunkEntry::new(
        new_start_block,
        entry.block_count(),
        entry.flags(),
        entry.compressed_len(),
    )
}

/// Zero-allocation slice core of [`shift_table_collapse`]: collapses
/// `[start_unit..start_unit + remove_units)` in `table`, shifting trailing entries left
/// and writing removed non-zero entries into `freed_out`.
///
/// Returns `(new_table_len, freed_len)`.
#[inline]
fn shift_table_collapse_slice(
    table: &mut [u64],
    start_unit: usize,
    remove_units: usize,
    freed_out: &mut [u64],
) -> (usize, usize) {
    let len = table.len();
    if remove_units == 0 || start_unit >= len {
        return (len, 0);
    }
    let end_unit = start_unit.saturating_add(remove_units).min(len);
    let mut freed_len = 0usize;
    let mut i = start_unit;
    while i < end_unit {
        let entry = table[i];
        if entry != 0 {
            freed_out[freed_len] = entry;
            freed_len += 1;
        }
        i += 1;
    }
    let tail_len = len - end_unit;
    let mut k = 0usize;
    while k < tail_len {
        table[start_unit + k] = table[end_unit + k];
        k += 1;
    }
    (start_unit + tail_len, freed_len)
}

/// Collapses the half-open unit range `[start_unit..start_unit + remove_units)` out of a
/// logical block/chunk pointer table, shifting all trailing entries left by `remove_units`
/// and returning all removed non-zero raw entries to be freed.
pub fn shift_table_collapse(
    table: &mut Vec<u64>,
    start_unit: usize,
    remove_units: usize,
) -> Vec<u64> {
    if remove_units == 0 || start_unit >= table.len() {
        return Vec::new();
    }
    let end_unit = start_unit.saturating_add(remove_units).min(table.len());
    let mut freed = vec![0u64; end_unit - start_unit];
    let (new_len, freed_len) =
        shift_table_collapse_slice(table.as_mut_slice(), start_unit, remove_units, &mut freed);
    table.truncate(new_len);
    freed.truncate(freed_len);
    freed
}

/// Clears a logical pointer/chunk slot to `0` (sparse hole) and returns the previous
/// non-zero raw entry (if any) whose physical block(s) must be freed.
#[inline]
pub fn punch_table_slot(slot: &mut u64) -> Option<u64> {
    let prev = *slot;
    *slot = 0;
    (prev != 0).then_some(prev)
}

/// Returns `true` if a raw pointer/chunk slot resolves to all-zero read semantics.
#[inline]
pub fn slot_reads_as_zero(raw_slot: u64, is_seekable: bool) -> bool {
    if is_seekable {
        crate::inode::ChunkEntry(raw_slot).is_empty()
    } else {
        raw_slot == 0
    }
}

/// Incrementally appends `(logical_idx, phys_blk)` to `runs`, skipping sparse holes
/// (`phys_blk == 0`) and coalescing contiguous logical+physical blocks into maximal runs.
#[inline]
pub fn push_logical_block_run(runs: &mut Vec<LogicalRun>, logical_idx: usize, phys_blk: u64) {
    if phys_blk == 0 {
        return;
    }
    if let Some(last) = runs.last_mut()
        && usize::try_from(last.count)
            .ok()
            .and_then(|c| last.logical_start.checked_add(c))
            == Some(logical_idx)
        && last.phys_start.checked_add(last.count) == Some(phys_blk)
    {
        last.count += 1;
    } else {
        runs.push(LogicalRun {
            logical_start: logical_idx,
            phys_start: phys_blk,
            count: 1,
        });
    }
}

/// Zero-allocation slice core of [`coalesce_logical_runs`]: coalesces `(logical_idx, phys_blk)`
/// mappings into `out` (which must have length `>= blocks.len()`), skipping sparse holes
/// (`phys_blk == 0`), and returns the number of runs written.
#[inline]
fn coalesce_logical_runs_into(blocks: &[(usize, u64)], out: &mut [LogicalRun]) -> usize {
    let mut len = 0usize;
    for &(logical_idx, phys_blk) in blocks {
        if phys_blk == 0 {
            continue;
        }
        if len > 0 {
            let last = &mut out[len - 1];
            if usize::try_from(last.count)
                .ok()
                .and_then(|c| last.logical_start.checked_add(c))
                == Some(logical_idx)
                && last.phys_start.checked_add(last.count) == Some(phys_blk)
            {
                last.count += 1;
                continue;
            }
        }
        out[len] = LogicalRun {
            logical_start: logical_idx,
            phys_start: phys_blk,
            count: 1,
        };
        len += 1;
    }
    len
}

/// Coalesces a sequence of `(logical_idx, phys_block_id)` mappings into maximal contiguous
/// [`LogicalRun`]s while skipping sparse holes (`phys_block_id == 0`).
pub fn coalesce_logical_runs(blocks: &[(usize, u64)]) -> Vec<LogicalRun> {
    let mut runs = vec![
        LogicalRun {
            logical_start: 0,
            phys_start: 0,
            count: 0,
        };
        blocks.len()
    ];
    let n = coalesce_logical_runs_into(blocks, &mut runs);
    runs.truncate(n);
    runs
}

/// Returns the canonical host OS file identity `(st_dev, st_ino)` for an open file
/// descriptor via `fstat(2)`.
///
/// Used for canonical total ordering of file pairs, same-image detection across
/// independent descriptors, and `(src_dev, dst_dev)` negative reflink caching.
pub fn canonical_file_id(fd: RawFd) -> std::io::Result<(u64, u64)> {
    // SAFETY: `libc::stat` is a plain C struct; `fstat` initializes it on success.
    let mut st = unsafe { std::mem::zeroed::<libc::stat>() };
    loop {
        // SAFETY: `fd` is an open file descriptor and `st` is a valid out-pointer.
        let rc = unsafe { libc::fstat(fd, &mut st) };
        if rc == 0 {
            return Ok((st.st_dev as u64, st.st_ino as u64));
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

/// Best-effort host-OS hole punch on freed physical block extents so CoW refcounts drop
/// and sparse host storage is reclaimed while preserving `.img` file size.
///
/// Silently ignores unsupported filesystems (`EOPNOTSUPP`, `ENOSYS`, `EINVAL`).
pub fn punch_host_file_blocks(fd: RawFd, start_block: u64, block_count: u64) {
    if block_count == 0 {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let Some(offset) = start_block.checked_mul(BLOCK_SIZE as u64) else {
            return;
        };
        let Some(len) = block_count.checked_mul(BLOCK_SIZE as u64) else {
            return;
        };
        loop {
            // SAFETY: `fd` is a valid open file descriptor; `FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE`
            // preserves the `.img` container's logical file size while releasing backing extents.
            let rc = unsafe {
                libc::fallocate(
                    fd,
                    libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                    offset as libc::off_t,
                    len as libc::off_t,
                )
            };
            if rc == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (fd, start_block, block_count);
    }
}

static UNSUPPORTED_REFLINK_DEVS: OnceLock<RwLock<FxHashSet<(u64, u64)>>> = OnceLock::new();

#[inline]
fn unsupported_reflink_devs() -> &'static RwLock<FxHashSet<(u64, u64)>> {
    UNSUPPORTED_REFLINK_DEVS.get_or_init(|| RwLock::new(FxHashSet::default()))
}

/// Returns `true` if `(src_dev, dst_dev)` has previously been cached as unsupported for
/// `FICLONERANGE`.
pub fn is_reflink_device_pair_unsupported(src_dev: u64, dst_dev: u64) -> bool {
    let guard = unsupported_reflink_devs()
        .read()
        .unwrap_or_else(|e| e.into_inner());
    guard.contains(&(src_dev, dst_dev))
}

/// Records `(src_dev, dst_dev)` in the negative reflink cache so subsequent extents on the
/// same device pair skip failing `FICLONERANGE` ioctls.
pub fn mark_reflink_device_pair_unsupported(src_dev: u64, dst_dev: u64) {
    let mut guard = unsupported_reflink_devs()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    guard.insert((src_dev, dst_dev));
}

/// Clears the global `(src_dev, dst_dev)` negative reflink cache.
pub fn clear_unsupported_reflink_cache() {
    let mut guard = unsupported_reflink_devs()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    guard.clear();
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct FileCloneRange {
    src_fd: i64,
    src_offset: u64,
    src_length: u64,
    dest_offset: u64,
}

#[cfg(target_os = "linux")]
const FICLONE: libc::Ioctl = 0x4004_9409 as libc::Ioctl;
#[cfg(target_os = "linux")]
const FICLONERANGE: libc::Ioctl = 0x4020_940d as libc::Ioctl;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn clonefile(src: *const libc::c_char, dst: *const libc::c_char, flags: u32) -> libc::c_int;
}

/// Transfers `len` bytes from `src_fd` at `src_offset` to `dst_fd` at `dst_offset` using
/// the 3-tier automatic fallback pipeline:
/// `FICLONERANGE` (Linux) -> `copy_file_range(2)` (Linux) -> raw `pread`/`pwrite`.
pub fn transfer_byte_range(
    src_fd: RawFd,
    src_offset: u64,
    dst_fd: RawFd,
    dst_offset: u64,
    len: u64,
) -> std::io::Result<TransferMethod> {
    let dev_pair = match (canonical_file_id(src_fd), canonical_file_id(dst_fd)) {
        (Ok((sd, _)), Ok((dd, _))) => Some((sd, dd)),
        _ => None,
    };
    transfer_byte_range_with_devs(src_fd, src_offset, dst_fd, dst_offset, len, dev_pair)
}

fn transfer_byte_range_with_devs(
    src_fd: RawFd,
    src_offset: u64,
    dst_fd: RawFd,
    dst_offset: u64,
    len: u64,
    dev_pair: Option<(u64, u64)>,
) -> std::io::Result<TransferMethod> {
    if len == 0 {
        return Ok(TransferMethod::RawBlockCopy);
    }

    #[allow(unused_mut)]
    let mut s_off = src_offset;
    #[allow(unused_mut)]
    let mut d_off = dst_offset;
    #[allow(unused_mut)]
    let mut remaining = len;

    #[cfg(not(target_os = "linux"))]
    let _ = dev_pair;

    #[cfg(target_os = "linux")]
    {
        // Tier 1: Linux `ioctl(FICLONERANGE)` zero-copy CoW extent clone.
        let skip_reflink =
            dev_pair.is_some_and(|(sd, dd)| is_reflink_device_pair_unsupported(sd, dd));
        if !skip_reflink {
            let range = FileCloneRange {
                src_fd: src_fd as i64,
                src_offset: s_off,
                src_length: remaining,
                dest_offset: d_off,
            };
            loop {
                // SAFETY: `range` is a valid `struct file_clone_range` pointer for `FICLONERANGE`.
                let rc = unsafe { libc::ioctl(dst_fd, FICLONERANGE, &range) };
                if rc == 0 {
                    return Ok(TransferMethod::Reflink);
                }
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                match classify_reflink_errno(err.raw_os_error().unwrap_or(0)) {
                    ReflinkFallback::CacheUnsupportedDevicePair => {
                        if let Some((sd, dd)) = dev_pair {
                            mark_reflink_device_pair_unsupported(sd, dd);
                        }
                        break;
                    }
                    ReflinkFallback::FallbackThisExtentOnly => {
                        break;
                    }
                    ReflinkFallback::Fatal => {
                        return Err(err);
                    }
                }
            }
        }

        // Tier 2: Linux `copy_file_range(2)` kernel/server-side copy.
        let mut cfr_src = s_off as libc::loff_t;
        let mut cfr_dst = d_off as libc::loff_t;
        let mut cfr_fell_back = false;
        while remaining > 0 {
            let chunk = remaining.min(usize::MAX as u64) as usize;
            // SAFETY: valid file descriptors and mutable offset pointers.
            let n = unsafe {
                libc::copy_file_range(src_fd, &mut cfr_src, dst_fd, &mut cfr_dst, chunk, 0)
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                if matches!(
                    err.raw_os_error(),
                    Some(libc::EXDEV)
                        | Some(libc::EOPNOTSUPP)
                        | Some(libc::EINVAL)
                        | Some(libc::ENOSYS)
                        | Some(libc::EPERM)
                        | Some(libc::ENOTTY)
                ) {
                    cfr_fell_back = true;
                    break;
                }
                return Err(err);
            }
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "copy_file_range hit unexpected EOF",
                ));
            }
            let advanced = n as u64;
            s_off += advanced;
            d_off += advanced;
            remaining -= advanced;
        }
        if !cfr_fell_back {
            return Ok(TransferMethod::CopyFileRange);
        }
    }

    // Tier 3: Portable chunked raw `pread`/`pwrite` for all remaining bytes
    // (skips decompression and re-compression).
    let buf_size = (remaining as usize).min(256 * 1024);
    let mut buf = vec![0u8; buf_size];
    let mut copied = 0u64;
    while copied < remaining {
        let to_copy = ((remaining - copied) as usize).min(buf.len());
        let mut r_done = 0usize;
        while r_done < to_copy {
            let off = (s_off + copied + r_done as u64) as libc::off_t;
            // SAFETY: `buf[r_done..to_copy]` is a valid writable slice.
            let n = unsafe {
                libc::pread(
                    src_fd,
                    buf[r_done..to_copy].as_mut_ptr().cast(),
                    to_copy - r_done,
                    off,
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "raw extent pread hit unexpected EOF",
                ));
            }
            r_done += n as usize;
        }
        let mut w_done = 0usize;
        while w_done < to_copy {
            let off = (d_off + copied + w_done as u64) as libc::off_t;
            // SAFETY: `buf[w_done..to_copy]` is a valid readable slice.
            let n = unsafe {
                libc::pwrite(
                    dst_fd,
                    buf[w_done..to_copy].as_ptr().cast(),
                    to_copy - w_done,
                    off,
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "raw extent pwrite wrote zero bytes",
                ));
            }
            w_done += n as usize;
        }
        copied += to_copy as u64;
    }
    Ok(TransferMethod::RawBlockCopy)
}

#[derive(Debug, Clone, Copy)]
struct ChunkExtent {
    chunk_idx: usize,
    entry: crate::inode::ChunkEntry,
}

/// Coalesces a list of `(start_block, count)` spans into sorted maximal non-overlapping runs.
fn coalesce_block_spans(spans: &mut Vec<(u64, u64)>) {
    if spans.len() <= 1 {
        spans.retain(|&(_, c)| c > 0);
        return;
    }
    spans.sort_unstable_by_key(|&(s, _)| s);
    let mut write = 0usize;
    for i in 0..spans.len() {
        let (s, c) = spans[i];
        if c == 0 {
            continue;
        }
        if write > 0 {
            let (ps, pc) = spans[write - 1];
            if ps.saturating_add(pc) >= s {
                let new_end = ps.saturating_add(pc).max(s.saturating_add(c));
                spans[write - 1].1 = new_end - ps;
                continue;
            }
        }
        spans[write] = (s, c);
        write += 1;
    }
    spans.truncate(write);
}

/// Extracts physical `(start_block, count)` span from a raw slot value.
#[inline]
fn raw_slot_block_span(
    guard: &DiskManagerInner,
    raw: u64,
    is_seekable: bool,
) -> Option<(u64, u64)> {
    if raw == 0 {
        return None;
    }
    if is_seekable {
        let e = crate::inode::ChunkEntry(raw);
        let start = e.start_block() as u64;
        let cnt = e.block_count() as u64;
        if !e.is_empty()
            && cnt > 0
            && guard.is_data_block(start)
            && guard.is_data_block(start.saturating_add(cnt - 1))
        {
            Some((start, cnt))
        } else {
            None
        }
    } else if guard.is_data_block(raw) {
        Some((raw, 1))
    } else {
        None
    }
}

#[inline]
fn inode_write_params(inode: &Inode) -> (CompressionMode, crate::filters::FilterConfig) {
    let is_seekable = (inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
    let mode = if is_seekable {
        CompressionMode::Seekable {
            chunk_size: crate::inode::CHUNK_SIZE_64K as u32,
            level: 1,
        }
    } else if inode.compressed_size > 0 {
        CompressionMode::Always
    } else {
        CompressionMode::Never
    };
    let filters = crate::filters::FilterConfig {
        typesize: inode.filter_typesize,
        delta: inode.filter_delta,
        shuffle: inode.filter_shuffle,
        bitshuffle: inode.filter_bitshuffle,
    };
    (mode, filters)
}

impl DiskManager {
    /// Clones a whole OIFS `.img` file using the host OS's fastest CoW fork primitive:
    ///
    /// - **macOS (APFS)**: `clonefile(2)` (O(1) instant CoW fork, 0 extra space).
    /// - **Linux (Btrfs / XFS / bcachefs / Lustre Lreflink)**: `ioctl(FICLONE)`.
    /// - **Fallback**: standard file copy with owner-only `0600` permissions.
    ///
    /// The destination path must not already exist.
    pub fn clone_image<P: AsRef<Path>, Q: AsRef<Path>>(
        src_path: P,
        dst_path: Q,
    ) -> Result<CloneMethod, DiskManagerError> {
        let src = src_path.as_ref();
        let dst = dst_path.as_ref();
        if dst.exists() {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("Destination image already exists: {}", dst.display()),
            )));
        }

        #[cfg(target_os = "macos")]
        {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;
            if let (Ok(c_src), Ok(c_dst)) = (
                CString::new(src.as_os_str().as_bytes()),
                CString::new(dst.as_os_str().as_bytes()),
            ) {
                loop {
                    // SAFETY: `c_src` and `c_dst` are valid NUL-terminated paths.
                    let rc = unsafe { clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
                    if rc == 0 {
                        return Ok(CloneMethod::ApfsClonefile);
                    }
                    if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                        break;
                    }
                }
            }
        }

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let src_file = File::open(src)?;
            let dst_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(dst)?;
            loop {
                // SAFETY: both descriptors are valid open regular files.
                let rc =
                    unsafe { libc::ioctl(dst_file.as_raw_fd(), FICLONE, src_file.as_raw_fd()) };
                if rc == 0 {
                    crate::retry_eintr(|| dst_file.sync_data())?;
                    return Ok(CloneMethod::LinuxFiclone);
                }
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    break;
                }
            }
            let len = src_file.metadata()?.len();
            if len > 0
                && let Err(e) =
                    transfer_byte_range(src_file.as_raw_fd(), 0, dst_file.as_raw_fd(), 0, len)
            {
                let _ = std::fs::remove_file(dst);
                return Err(e.into());
            }
            crate::retry_eintr(|| dst_file.sync_data())?;
            return Ok(CloneMethod::StandardCopy);
        }

        #[cfg(not(target_os = "linux"))]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut src_file = File::open(src)?;
            let mut dst_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(dst)?;
            if let Err(e) = std::io::copy(&mut src_file, &mut dst_file) {
                let _ = std::fs::remove_file(dst);
                return Err(e.into());
            }
            crate::retry_eintr(|| dst_file.sync_data())?;
            Ok(CloneMethod::StandardCopy)
        }
    }

    /// Walks all non-zero logical leaf pointers `(logical_idx, raw_value)` in ascending
    /// order within `[range_start..range_end)`, skipping sparse zero indirect subtrees in O(1).
    fn for_each_logical_leaf_in_range(
        guard: &DiskManagerInner,
        inode: &Inode,
        range_start: usize,
        range_end: usize,
        mut visit: impl FnMut(usize, u64),
    ) {
        use crate::inode::{DIRECT_BLOCKS, PTRS_PER_BLOCK};

        if range_start >= range_end {
            return;
        }

        // 1. Direct entries (0..10)
        if range_start < DIRECT_BLOCKS {
            let direct_end = range_end.min(DIRECT_BLOCKS);
            for i in range_start..direct_end {
                let val = inode.blocks[i];
                if val != 0 {
                    visit(i, val);
                }
            }
        }
        if range_end <= DIRECT_BLOCKS {
            return;
        }

        // 2. Single indirect (10..522)
        let single_base = DIRECT_BLOCKS;
        let double_base = single_base + PTRS_PER_BLOCK;
        if range_start < double_base && range_end > single_base {
            let sib_id = inode.blocks[10];
            if guard.is_data_block(sib_id) {
                let lo = range_start.saturating_sub(single_base);
                let hi = (range_end - single_base).min(PTRS_PER_BLOCK);
                let entries: Vec<(usize, u64)> = guard
                    .backing
                    .with_block(sib_id, |slice| {
                        slice[lo * 8..hi * 8]
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .enumerate()
                            .filter_map(|(rel, c)| {
                                let v = u64::from_le_bytes(*c);
                                (v != 0).then_some((single_base + lo + rel, v))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for (logical_idx, val) in entries {
                    visit(logical_idx, val);
                }
            }
        }
        if range_end <= double_base {
            return;
        }

        // 3. Double indirect (522..262_666): skips unallocated single-indirect subtrees.
        let per_dib = PTRS_PER_BLOCK * PTRS_PER_BLOCK;
        let triple_base = double_base + per_dib;
        if range_start < triple_base && range_end > double_base {
            let dib_id = inode.blocks[11];
            if guard.is_data_block(dib_id) {
                let d_lo = range_start.saturating_sub(double_base);
                let d_hi = (range_end - double_base).min(per_dib);
                let a_lo = d_lo / PTRS_PER_BLOCK;
                let a_hi = d_hi.div_ceil(PTRS_PER_BLOCK).min(PTRS_PER_BLOCK);
                let sibs: Vec<(usize, u64)> = guard
                    .backing
                    .with_block(dib_id, |slice| {
                        slice[a_lo * 8..a_hi * 8]
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .enumerate()
                            .filter_map(|(rel, c)| {
                                let sib = u64::from_le_bytes(*c);
                                guard.is_data_block(sib).then_some((a_lo + rel, sib))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for (a, sib) in sibs {
                    let sub_base = double_base + a * PTRS_PER_BLOCK;
                    let b_lo = range_start.saturating_sub(sub_base);
                    let b_hi = (range_end - sub_base).min(PTRS_PER_BLOCK);
                    let entries: Vec<(usize, u64)> = guard
                        .backing
                        .with_block(sib, |slice| {
                            slice[b_lo * 8..b_hi * 8]
                                .as_chunks::<8>()
                                .0
                                .iter()
                                .enumerate()
                                .filter_map(|(rel, c)| {
                                    let v = u64::from_le_bytes(*c);
                                    (v != 0).then_some((sub_base + b_lo + rel, v))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    for (logical_idx, val) in entries {
                        visit(logical_idx, val);
                    }
                }
            }
        }
        if range_end <= triple_base {
            return;
        }

        // 4. Triple indirect (262_666..MAX): skips unallocated double & single subtrees.
        let tib_id = inode.triple_indirect;
        if guard.is_data_block(tib_id) {
            let t_lo = range_start.saturating_sub(triple_base);
            let t_hi = range_end - triple_base;
            let a_lo = (t_lo / per_dib).min(PTRS_PER_BLOCK);
            let a_hi = t_hi.div_ceil(per_dib).min(PTRS_PER_BLOCK);
            let dibs: Vec<(usize, u64)> = guard
                .backing
                .with_block(tib_id, |slice| {
                    slice[a_lo * 8..a_hi * 8]
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .enumerate()
                        .filter_map(|(rel, c)| {
                            let dib = u64::from_le_bytes(*c);
                            guard.is_data_block(dib).then_some((a_lo + rel, dib))
                        })
                        .collect()
                })
                .unwrap_or_default();
            for (a, dib) in dibs {
                let dib_base = triple_base + a * per_dib;
                let d_lo = range_start.saturating_sub(dib_base);
                let d_hi = (range_end - dib_base).min(per_dib);
                let b_lo = d_lo / PTRS_PER_BLOCK;
                let b_hi = d_hi.div_ceil(PTRS_PER_BLOCK).min(PTRS_PER_BLOCK);
                let sibs: Vec<(usize, u64)> = guard
                    .backing
                    .with_block(dib, |slice| {
                        slice[b_lo * 8..b_hi * 8]
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .enumerate()
                            .filter_map(|(rel, c)| {
                                let sib = u64::from_le_bytes(*c);
                                guard.is_data_block(sib).then_some((b_lo + rel, sib))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for (b, sib) in sibs {
                    let sib_base = dib_base + b * PTRS_PER_BLOCK;
                    let c_lo = range_start.saturating_sub(sib_base);
                    let c_hi = (range_end - sib_base).min(PTRS_PER_BLOCK);
                    let entries: Vec<(usize, u64)> = guard
                        .backing
                        .with_block(sib, |slice| {
                            slice[c_lo * 8..c_hi * 8]
                                .as_chunks::<8>()
                                .0
                                .iter()
                                .enumerate()
                                .filter_map(|(rel, chunk)| {
                                    let v = u64::from_le_bytes(*chunk);
                                    (v != 0).then_some((sib_base + c_lo + rel, v))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    for (logical_idx, val) in entries {
                        visit(logical_idx, val);
                    }
                }
            }
        }
    }

    #[inline]
    fn for_each_logical_leaf(
        guard: &DiskManagerInner,
        inode: &Inode,
        max_entries: usize,
        visit: impl FnMut(usize, u64),
    ) {
        Self::for_each_logical_leaf_in_range(guard, inode, 0, max_entries, visit);
    }

    /// Transfers a regular file from `self` (`A.img`) to `dst` (`B.img`) using the 3-tier
    /// automatic fallback pipeline (`FICLONERANGE` -> `copy_file_range` -> raw compressed
    /// block copy), preserving compression, SIMD filters, sparse holes, and Seekable 64K
    /// chunks without decompressing or re-compressing.
    ///
    /// Automatically falls back to [`TransferMethod::LogicalReencode`] when either image is
    /// encrypted so ciphertext is re-encrypted with the destination's key and inode/chunk AAD.
    /// When `options.move_file` is `true`, the source entry is removed only after `dst` has
    /// committed and flushed to disk (At-Least-Once crash safety).
    ///
    /// Lock ordering invariant: never holds `self.inner` while acquiring `dst.inner`,
    /// preventing AB-BA deadlocks under concurrent bidirectional transfers.
    pub fn transfer_file_to(
        &self,
        src_parent: u64,
        src_name: &str,
        dst: &DiskManager,
        dst_parent: u64,
        dst_name: &str,
        options: TransferOptions,
    ) -> Result<TransferStats, DiskManagerError> {
        if dst.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        if options.move_file && self.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        for name in [src_name, dst_name] {
            crate::directory::validate_filename(name).map_err(|e| {
                DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    e.to_string(),
                ))
            })?;
        }

        // Canonical `(st_dev, st_ino)` identity check (acquired strictly one lock at a time).
        let src_file_id = {
            let src_guard = self.inner.read().unwrap();
            canonical_file_id(src_guard.file.as_raw_fd()).ok()
        };
        let dst_file_id = {
            let dst_guard = dst.inner.read().unwrap();
            canonical_file_id(dst_guard.file.as_raw_fd()).ok()
        };
        let same_backing_file = Arc::ptr_eq(&self.inner, &dst.inner)
            || (src_file_id.is_some() && src_file_id == dst_file_id);

        // Same-image special cases: self-target and O(1) metadata-only move via `rename`.
        if same_backing_file {
            let src_ino = self.lookup(src_parent, src_name)?;
            let src_inode = self.read_inode(src_ino)?;
            if src_inode.mode != crate::inode::FileType::File {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "transfer_file_to only supports regular files",
                )));
            }
            if src_parent == dst_parent && src_name == dst_name {
                if !options.move_file && !options.overwrite {
                    return Err(DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!("Destination '{dst_name}' already exists"),
                    )));
                }
                return Ok(TransferStats {
                    src_inode: src_ino,
                    dst_inode: src_ino,
                    logical_bytes: src_inode.size,
                    physical_bytes: 0,
                    extents_transferred: 0,
                    method: TransferMethod::Reflink,
                });
            }
            if options.move_file && Arc::ptr_eq(&self.inner, &dst.inner) {
                if let Ok(existing_ino) = dst.lookup(dst_parent, dst_name) {
                    if !options.overwrite {
                        return Err(DiskManagerError::Io(std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            format!("Destination '{dst_name}' already exists"),
                        )));
                    }
                    let existing = dst.read_inode(existing_ino)?;
                    if existing.mode == crate::inode::FileType::Directory {
                        return Err(DiskManagerError::Io(std::io::Error::other(
                            "Is a directory",
                        )));
                    }
                }
                self.rename(src_parent, src_name, dst_parent, dst_name)?;
                return Ok(TransferStats {
                    src_inode: src_ino,
                    dst_inode: src_ino,
                    logical_bytes: src_inode.size,
                    physical_bytes: 0,
                    extents_transferred: 0,
                    method: TransferMethod::Reflink,
                });
            }
        }

        let mut attempt = 0usize;
        let (src_ino, src_inode, dst_ino, physical_bytes, extents_transferred, method) = loop {
            // Flush source image before FD-level transfer so all dirty `MAP_SHARED` pages
            // are committed to the underlying host file.
            if !self.is_read_only() {
                self.flush()?;
            }

            // Stage 1: Inspect source inode and collect physical extents under `self.inner.read()`,
            // then strictly drop `src_guard` BEFORE touching `dst`.
            let (
                src_ino,
                src_inode,
                src_gen,
                src_encrypted,
                src_dicts,
                src_fd,
                seekable_chunks,
                standard_runs,
            ) = {
                let src_guard = self.inner.read().unwrap();
                let src_ino =
                    Self::dir_lookup(&src_guard, src_parent, src_name)?.ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "File not found",
                        ))
                    })?;
                let src_inode = Self::read_inode_internal(&src_guard, src_ino)?;
                if src_inode.mode != crate::inode::FileType::File {
                    return Err(DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "transfer_file_to only supports regular files",
                    )));
                }
                let src_gen = src_guard.inode_gen(src_ino);
                let src_encrypted = src_guard.superblock.encrypted
                    || src_guard.encryption_key.is_some()
                    || src_inode.encrypted;
                let src_dicts = Arc::clone(&src_guard.zstd_dicts);
                let src_fd = src_guard.file.as_raw_fd();
                let is_seekable = (src_inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;

                if src_encrypted {
                    (
                        src_ino,
                        src_inode,
                        src_gen,
                        true,
                        src_dicts,
                        src_fd,
                        Vec::new(),
                        Vec::new(),
                    )
                } else if is_seekable {
                    let num_chunks =
                        src_inode.size.div_ceil(crate::inode::CHUNK_SIZE_64K as u64) as usize;
                    let mut chunks = Vec::new();
                    Self::for_each_logical_leaf(
                        &src_guard,
                        &src_inode,
                        num_chunks,
                        |chunk_idx, val| {
                            let entry = crate::inode::ChunkEntry(val);
                            let start = entry.start_block() as u64;
                            let cnt = entry.block_count() as u64;
                            if !entry.is_empty()
                                && cnt > 0
                                && src_guard.is_data_block(start)
                                && src_guard.is_data_block(start.saturating_add(cnt - 1))
                            {
                                chunks.push(ChunkExtent { chunk_idx, entry });
                            }
                        },
                    );
                    (
                        src_ino,
                        src_inode,
                        src_gen,
                        false,
                        src_dicts,
                        src_fd,
                        chunks,
                        Vec::new(),
                    )
                } else {
                    let stored_bytes = if src_inode.compressed_size > 0 {
                        src_inode.compressed_size
                    } else {
                        src_inode.size
                    };
                    let num_blocks = stored_bytes.div_ceil(BLOCK_SIZE as u64) as usize;
                    let mut runs: Vec<LogicalRun> = Vec::new();
                    Self::for_each_logical_leaf(
                        &src_guard,
                        &src_inode,
                        num_blocks,
                        |logical_idx, phys_blk| {
                            if src_guard.is_data_block(phys_blk) {
                                push_logical_block_run(&mut runs, logical_idx, phys_blk);
                            }
                        },
                    );
                    (
                        src_ino,
                        src_inode,
                        src_gen,
                        false,
                        src_dicts,
                        src_fd,
                        Vec::new(),
                        runs,
                    )
                }
            };

            // Encryption gate: ONLY when both images are unencrypted may we enter Tier 1..3.
            #[cfg(test)]
            run_transfer_stage1_hook();

            let dst_encrypted = dst.is_encrypted();
            let src_is_compressed = src_inode.compressed_size > 0
                || (src_inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
            let dicts_ok = {
                let dst_guard = dst.inner.read().unwrap();
                dicts_compatible_for_raw_copy(src_is_compressed, &src_dicts, &dst_guard.zstd_dicts)
            };
            if !should_use_raw_fast_path(src_encrypted, dst_encrypted) || !dicts_ok {
                return self.transfer_logical_fallback(
                    src_parent, src_name, src_ino, &src_inode, dst, dst_parent, dst_name, options,
                );
            }

            // Flush destination before FD-level writes so no dirty `MAP_SHARED` pages race with
            // kernel `FICLONERANGE`/`copy_file_range`/`pwrite`.
            let est_dst_blks = {
                let b = if src_inode.compressed_size > 0 {
                    src_inode.compressed_size
                } else {
                    src_inode.size
                };
                b.div_ceil(BLOCK_SIZE as u64) + 8
            };
            dst.ensure_capacity_for(est_dst_blks, 1)?;
            dst.flush()?;

            let dev_pair = match (src_file_id, dst_file_id) {
                (Some((sd, _)), Some((dd, _))) => Some((sd, dd)),
                _ => None,
            };
            let is_seekable = (src_inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;

            // Stage 2: Allocate destination extents, execute 3-tier transfer, revalidate source
            // generation/link BEFORE committing, and atomically replace any existing target.
            // Zero Rust references into `dst_guard.backing` are held during FD-level transfers.
            enum Stage2Outcome {
                RetrySourceChanged,
                Committed(u64, u64, usize, TransferMethod),
            }

            let stage2_res = {
                let mut dst_guard = dst.inner.write().unwrap();
                if dst_guard.read_only {
                    return Err(DiskManagerError::ReadOnly);
                }
                if !should_use_raw_fast_path(
                    src_encrypted,
                    dst_guard.superblock.encrypted || dst_guard.encryption_key.is_some(),
                ) || !dicts_compatible_for_raw_copy(
                    src_is_compressed,
                    &src_dicts,
                    &dst_guard.zstd_dicts,
                ) {
                    drop(dst_guard);
                    return self.transfer_logical_fallback(
                        src_parent, src_name, src_ino, &src_inode, dst, dst_parent, dst_name,
                        options,
                    );
                }

                let dst_fd = dst_guard.file.as_raw_fd();
                let mut dst_parent_inode = Self::read_inode_internal(&dst_guard, dst_parent)?;
                if dst_parent_inode.mode != crate::inode::FileType::Directory {
                    return Err(DiskManagerError::Io(std::io::Error::other(
                        "Not a directory",
                    )));
                }

                let stored_name =
                    Self::stored_filename(&dst_guard, dst_parent, dst_name)?.into_owned();
                let stored_hash = crate::directory::hash_filename(&stored_name);

                // Inspect existing target without deleting it yet so ENOSPC or I/O failures
                // leave the original destination file intact.
                let replace_info = if let Some((old_ino, old_dir_blk)) = Self::locate_entry_in_dir(
                    &dst_guard.backing,
                    &dst_parent_inode,
                    &stored_name,
                    stored_hash,
                ) {
                    if !options.overwrite {
                        return Err(DiskManagerError::Io(std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            format!("Destination '{dst_name}' already exists"),
                        )));
                    }
                    let old_inode = Self::read_inode_internal(&dst_guard, old_ino)?;
                    if old_inode.mode == crate::inode::FileType::Directory {
                        return Err(DiskManagerError::Io(std::io::Error::other(
                            "Is a directory",
                        )));
                    }
                    let old_blocks = Self::collect_inode_blocks(&dst_guard.backing, &old_inode);
                    Some((old_ino, old_dir_blk, old_blocks))
                } else {
                    None
                };

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                let mut new_inode = Inode::new(crate::inode::FileType::File);
                new_inode.size = src_inode.size;
                new_inode.compressed_size = src_inode.compressed_size;
                new_inode.flags = src_inode.flags;
                new_inode.filter_typesize = src_inode.filter_typesize;
                new_inode.filter_delta = src_inode.filter_delta;
                new_inode.filter_shuffle = src_inode.filter_shuffle;
                new_inode.filter_bitshuffle = src_inode.filter_bitshuffle;
                new_inode.encrypted = false;
                new_inode.created_at = now;
                new_inode.modified_at = now;

                let journaled = dst_guard.superblock.has_journal_layout();
                let mut tracker = TierTracker::default();
                let mut physical_bytes = 0u64;
                let mut extents_transferred = 0usize;

                let source_still_valid = |dg: &DiskManagerInner| -> bool {
                    if Arc::ptr_eq(&self.inner, &dst.inner) {
                        dg.is_inode_allocated(src_ino)
                            && dg.inode_gen(src_ino) == src_gen
                            && Self::dir_lookup(dg, src_parent, src_name).ok().flatten()
                                == Some(src_ino)
                    } else if let Ok(sg) = self.inner.try_read() {
                        sg.is_inode_allocated(src_ino)
                            && sg.inode_gen(src_ino) == src_gen
                            && Self::dir_lookup(&sg, src_parent, src_name).ok().flatten()
                                == Some(src_ino)
                    } else {
                        false
                    }
                };

                if journaled {
                    let ib_slice = dst_guard.inode_bitmap_slice().ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::other("inode bitmap not found"))
                    })?;
                    let dst_ino = crate::bitmap::BitmapRef::new(ib_slice)
                        .find_next_free_bounded(
                            dst_guard.free_inode_hint as usize,
                            dst_guard.superblock.usable_inode_count() as usize,
                        )
                        .ok_or(DiskManagerError::Allocator(
                            crate::allocator::AllocatorError::NoSpace,
                        ))? as u64;
                    let next_free_inode_hint = dst_ino + 1;

                    let mut sim = crate::journal::AllocSim::new_data_only(
                        &dst_guard,
                        next_free_inode_hint,
                        dst_guard.free_block_hint,
                    )?;
                    let mut ops = Vec::new();
                    ops.push(crate::journal::MetadataOp::SetInodeBitmap {
                        inode_id: dst_ino,
                        allocated: true,
                    });
                    let mut payload_ranges: Vec<(usize, usize)> = Vec::new();
                    let track_ranges = dst_guard.durability_mode() == DurabilityMode::RangeAsync;

                    // Allocate destination extents & transfer payload bytes before WAL commit
                    // (ordered-mode crash safety: unreferenced blocks are populated first).
                    if is_seekable {
                        for ce in &seekable_chunks {
                            let count = ce.entry.block_count() as usize;
                            let dst_start = sim.alloc_contiguous(&dst_guard.backing, count)?;
                            let dst_start_u32 = u32::try_from(dst_start).map_err(|_| {
                                DiskManagerError::Io(std::io::Error::other(
                                    "Destination block exceeds 32-bit ChunkEntry range",
                                ))
                            })?;
                            let count_u32 = u32::try_from(count).map_err(|_| {
                                DiskManagerError::Io(std::io::Error::other("Extent count overflow"))
                            })?;
                            crate::journal::push_data_bitmap_op(
                                &mut ops, dst_start, count_u32, true,
                            );

                            let byte_len = count as u64 * BLOCK_SIZE as u64;
                            let m = transfer_byte_range_with_devs(
                                src_fd,
                                ce.entry.start_block() as u64 * BLOCK_SIZE as u64,
                                dst_fd,
                                dst_start * BLOCK_SIZE as u64,
                                byte_len,
                                dev_pair,
                            )?;
                            dst_guard
                                .backing
                                .invalidate_block_range_caches(dst_start, count as u64);
                            if track_ranges {
                                payload_ranges
                                    .push(((dst_start as usize) * BLOCK_SIZE, byte_len as usize));
                            }
                            tracker.record(m);
                            physical_bytes += byte_len;
                            extents_transferred += 1;

                            let new_entry = rebase_chunk_entry(ce.entry, dst_start_u32);
                            crate::journal::sim_set_logical_chunk_entry(
                                &dst_guard.backing,
                                &mut new_inode,
                                ce.chunk_idx,
                                new_entry,
                                &mut sim,
                                &mut ops,
                            )?;
                        }
                    } else {
                        for run in &standard_runs {
                            let mut rem = run.count as usize;
                            let mut src_blk = run.phys_start;
                            let mut log_idx = run.logical_start;
                            while rem > 0 {
                                let (dst_start, alloc_len) =
                                    Self::sim_alloc_run_greedy(&dst_guard.backing, &mut sim, rem)?;
                                let alloc_u32 = u32::try_from(alloc_len).map_err(|_| {
                                    DiskManagerError::Io(std::io::Error::other(
                                        "Extent count overflow",
                                    ))
                                })?;
                                crate::journal::push_data_bitmap_op(
                                    &mut ops, dst_start, alloc_u32, true,
                                );
                                let byte_len = alloc_len as u64 * BLOCK_SIZE as u64;
                                let m = transfer_byte_range_with_devs(
                                    src_fd,
                                    src_blk * BLOCK_SIZE as u64,
                                    dst_fd,
                                    dst_start * BLOCK_SIZE as u64,
                                    byte_len,
                                    dev_pair,
                                )?;
                                dst_guard
                                    .backing
                                    .invalidate_block_range_caches(dst_start, alloc_len as u64);
                                if track_ranges {
                                    payload_ranges.push((
                                        (dst_start as usize) * BLOCK_SIZE,
                                        byte_len as usize,
                                    ));
                                }
                                tracker.record(m);
                                physical_bytes += byte_len;
                                extents_transferred += 1;

                                for k in 0..alloc_len {
                                    crate::journal::sim_set_logical_block_ptr(
                                        &dst_guard.backing,
                                        &mut new_inode,
                                        log_idx + k,
                                        dst_start + k as u64,
                                        &mut sim,
                                        &mut ops,
                                    )?;
                                }
                                src_blk += alloc_len as u64;
                                log_idx += alloc_len;
                                rem -= alloc_len;
                            }
                        }
                    }

                    // Revalidate that `src_ino` was not concurrently deleted, overwritten,
                    // truncated, or defragmented between Stage 1 and completion of Stage 2 reads.
                    if !source_still_valid(&dst_guard) {
                        Stage2Outcome::RetrySourceChanged
                    } else {
                        // Ordered-mode crash consistency: persist host FS extent-tree changes
                        // (`FICLONERANGE` / `copy_file_range`) and strict-mode payloads via
                        // `fdatasync` BEFORE committing the OIFS WAL transaction.
                        if tracker.used_kernel_extent_op
                            || (physical_bytes > 0
                                && dst_guard.durability_mode() == DurabilityMode::Strict)
                        {
                            crate::retry_eintr(|| dst_guard.file.sync_data())?;
                        }

                        // Atomically link `dst_ino` (replacing any existing entry in `old_dir_blk`
                        // inside this same WAL transaction so overwrite failure never loses data).
                        let entry = crate::directory::DirectoryEntry {
                            inode: dst_ino,
                            hash: stored_hash,
                            name: stored_name.clone(),
                        };
                        let mut entry_bytes = Vec::with_capacity(20 + entry.name.len());
                        {
                            let mut cursor = std::io::Cursor::new(&mut entry_bytes);
                            entry.serialize_into(&mut cursor).map_err(|e| {
                                DiskManagerError::Io(std::io::Error::other(e.to_string()))
                            })?;
                        }

                        if let Some((old_ino, old_dir_blk, ref old_blocks)) = replace_info {
                            let mut blk_buf = dst_guard
                                .backing
                                .with_block(old_dir_blk, |s| s.to_vec())
                                .map_err(|_| {
                                    DiskManagerError::Io(std::io::Error::other(
                                        "Directory block not found",
                                    ))
                                })?;
                            crate::directory::remove_entry_in_block_bytes(
                                &mut blk_buf,
                                &stored_name,
                                stored_hash,
                            );
                            let ins = crate::directory::find_insert_offset_in_block(&blk_buf);
                            if ins + entry_bytes.len() > BLOCK_SIZE {
                                return Err(DiskManagerError::Io(std::io::Error::other(
                                    "Directory block overflow during atomic overwrite",
                                )));
                            }
                            blk_buf[ins..ins + entry_bytes.len()].copy_from_slice(&entry_bytes);
                            let tail = ins + entry_bytes.len();
                            if tail + 18 <= BLOCK_SIZE {
                                blk_buf[tail..tail + 18].fill(0);
                            }
                            ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                                block_id: old_dir_blk,
                                offset: 0,
                                data: blk_buf,
                            });

                            ops.push(crate::journal::MetadataOp::SetInodeBitmap {
                                inode_id: old_ino,
                                allocated: false,
                            });
                            let mut old_spans: Vec<(u64, u64)> =
                                old_blocks.iter().map(|&b| (b, 1)).collect();
                            coalesce_block_spans(&mut old_spans);
                            for &(os, oc) in &old_spans {
                                sim.free_contiguous(&dst_guard.backing, os, oc as usize)?;
                                let oc_u32 = u32::try_from(oc).map_err(|_| {
                                    DiskManagerError::Io(std::io::Error::other(
                                        "Freed extent overflow",
                                    ))
                                })?;
                                crate::journal::push_data_bitmap_op(&mut ops, os, oc_u32, false);
                            }
                        } else {
                            let needed_space = 20 + entry.name.len();
                            let (slot, _) = Self::find_dir_insert_slot(
                                &dst_guard,
                                dst_parent,
                                &dst_parent_inode,
                                needed_space,
                            );
                            if let Some((phys_blk, insert_offset, _)) = slot {
                                ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                                    block_id: phys_blk,
                                    offset: insert_offset as u32,
                                    data: entry_bytes,
                                });
                            } else {
                                let new_blk_idx = Self::dir_num_blocks(&dst_parent_inode);
                                let phys_blk = crate::journal::sim_get_or_alloc_block(
                                    &dst_guard.backing,
                                    &mut dst_parent_inode,
                                    new_blk_idx,
                                    &mut sim,
                                    &mut ops,
                                )?;
                                let mut block_image = vec![0u8; BLOCK_SIZE];
                                block_image[..entry_bytes.len()].copy_from_slice(&entry_bytes);
                                ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                                    block_id: phys_blk,
                                    offset: 0,
                                    data: block_image,
                                });
                                dst_parent_inode.size =
                                    (new_blk_idx + 1) as u64 * BLOCK_SIZE as u64;
                            }
                        }

                        dst_parent_inode.modified_at = now;
                        let new_inode_img =
                            Self::build_inode_post_image(&dst_guard, dst_ino, &new_inode)?;
                        let parent_img = Self::build_inode_post_image(
                            &dst_guard,
                            dst_parent,
                            &dst_parent_inode,
                        )?;
                        ops.push(crate::journal::MetadataOp::WriteInode {
                            inode_id: dst_ino,
                            inode_bytes: Box::new(new_inode_img),
                        });
                        ops.push(crate::journal::MetadataOp::WriteInode {
                            inode_id: dst_parent,
                            inode_bytes: Box::new(parent_img),
                        });

                        let ops = if crate::journal::is_already_coalesced(&ops) {
                            ops
                        } else {
                            crate::journal::coalesce_ops(ops)
                        };
                        Self::commit_journal_tx(&mut dst_guard, &ops)?;
                        Self::apply_journal_ops(&mut dst_guard, &ops)?;
                        dst_guard.free_inode_hint = sim.free_inode_hint;
                        dst_guard.free_block_hint = sim.free_block_hint;
                        if let Some((old_ino, _, ref old_blocks)) = replace_info {
                            if let Some(min_b) = old_blocks.iter().copied().min()
                                && min_b < dst_guard.free_block_hint
                            {
                                dst_guard.free_block_hint = min_b;
                            }
                            if old_ino < dst_guard.free_inode_hint {
                                dst_guard.free_inode_hint = old_ino;
                            }
                            dst_guard.inode_cache.remove(old_ino);
                            dst_guard.invalidate_payload_caches(old_ino);
                        }
                        dst_guard.cache_inode(dst_ino, new_inode);
                        dst_guard.cache_inode(dst_parent, dst_parent_inode);
                        dst_guard.dir_cache.remove(dst_parent);

                        if dst_guard.durability_mode() == DurabilityMode::RangeAsync {
                            let mut ranges = vec![
                                dst_guard.inode_bitmap_byte_range(),
                                dst_guard.data_bitmap_byte_range(),
                                dst_guard.inode_byte_range(dst_ino),
                                dst_guard.inode_byte_range(dst_parent),
                            ];
                            ranges.extend(payload_ranges);
                            for op in &ops {
                                if let crate::journal::MetadataOp::WriteBlockSlice {
                                    block_id,
                                    ..
                                } = op
                                {
                                    ranges.push(dst_guard.block_byte_range(*block_id));
                                }
                            }
                            dst_guard.sync_mutation_ranges(&ranges)?;
                        } else if dst_guard.durability_mode()
                            == DurabilityMode::LegacyWholeMmapAsync
                        {
                            dst_guard.sync_mutation_ranges(&[])?;
                        }
                        Self::maybe_checkpoint(&mut dst_guard)?;

                        Stage2Outcome::Committed(
                            dst_ino,
                            physical_bytes,
                            extents_transferred,
                            tracker.finish(),
                        )
                    }
                } else {
                    // Non-journaled destination path.
                    let hint = dst_guard.free_inode_hint;
                    let usable_inodes = dst_guard.superblock.usable_inode_count();
                    let dst_ino = {
                        let mut bitmap_slice =
                            dst_guard.inode_bitmap_slice_mut().ok_or_else(|| {
                                DiskManagerError::Io(std::io::Error::other(
                                    "inode bitmap not found",
                                ))
                            })?;
                        let mut allocator = SimpleBlockAllocator::new(&mut bitmap_slice, 0);
                        let id = allocator.allocate_bounded(Some(hint), usable_inodes)?;
                        bitmap_slice.set_dirty_bits(id as usize, 1);
                        id
                    };
                    dst_guard.free_inode_hint = if dst_ino + 1 >= usable_inodes {
                        0
                    } else {
                        dst_ino + 1
                    };

                    let mut touched_ranges: Vec<(usize, usize)> = Vec::new();
                    let track_ranges = dst_guard.durability_mode().is_range_based();

                    let mut stale_src = false;
                    let transfer_res: Result<(), DiskManagerError> = (|| {
                        if is_seekable {
                            for ce in &seekable_chunks {
                                let count = ce.entry.block_count() as usize;
                                let dst_start = Self::allocate_contiguous_blocks_internal(
                                    &mut dst_guard,
                                    count,
                                )?;
                                let dst_start_u32 = match u32::try_from(dst_start) {
                                    Ok(v) => v,
                                    Err(_) => {
                                        let _ = Self::free_contiguous_blocks_internal(
                                            &mut dst_guard,
                                            dst_start,
                                            count,
                                        );
                                        return Err(DiskManagerError::Io(std::io::Error::other(
                                            "Destination block exceeds 32-bit ChunkEntry range",
                                        )));
                                    }
                                };
                                let byte_len = count as u64 * BLOCK_SIZE as u64;
                                let m = match transfer_byte_range_with_devs(
                                    src_fd,
                                    ce.entry.start_block() as u64 * BLOCK_SIZE as u64,
                                    dst_fd,
                                    dst_start * BLOCK_SIZE as u64,
                                    byte_len,
                                    dev_pair,
                                ) {
                                    Ok(m) => m,
                                    Err(e) => {
                                        let _ = Self::free_contiguous_blocks_internal(
                                            &mut dst_guard,
                                            dst_start,
                                            count,
                                        );
                                        return Err(e.into());
                                    }
                                };
                                dst_guard
                                    .backing
                                    .invalidate_block_range_caches(dst_start, count as u64);
                                if track_ranges {
                                    touched_ranges.push((
                                        (dst_start as usize) * BLOCK_SIZE,
                                        byte_len as usize,
                                    ));
                                }
                                tracker.record(m);
                                physical_bytes += byte_len;
                                extents_transferred += 1;

                                let new_entry = rebase_chunk_entry(ce.entry, dst_start_u32);
                                if let Err(e) = Self::set_logical_chunk_entry(
                                    &mut dst_guard,
                                    &mut new_inode,
                                    ce.chunk_idx,
                                    new_entry,
                                    None,
                                ) {
                                    let _ = Self::free_contiguous_blocks_internal(
                                        &mut dst_guard,
                                        dst_start,
                                        count,
                                    );
                                    return Err(e);
                                }
                            }
                        } else {
                            for run in &standard_runs {
                                let mut rem = run.count as usize;
                                let mut src_blk = run.phys_start;
                                let mut log_idx = run.logical_start;
                                while rem > 0 {
                                    let (dst_start, alloc_len) =
                                        Self::alloc_run_greedy(&mut dst_guard, rem)?;
                                    let byte_len = alloc_len as u64 * BLOCK_SIZE as u64;
                                    let m = match transfer_byte_range_with_devs(
                                        src_fd,
                                        src_blk * BLOCK_SIZE as u64,
                                        dst_fd,
                                        dst_start * BLOCK_SIZE as u64,
                                        byte_len,
                                        dev_pair,
                                    ) {
                                        Ok(m) => m,
                                        Err(e) => {
                                            let _ = Self::free_contiguous_blocks_internal(
                                                &mut dst_guard,
                                                dst_start,
                                                alloc_len,
                                            );
                                            return Err(e.into());
                                        }
                                    };
                                    dst_guard
                                        .backing
                                        .invalidate_block_range_caches(dst_start, alloc_len as u64);
                                    if track_ranges {
                                        touched_ranges.push((
                                            (dst_start as usize) * BLOCK_SIZE,
                                            byte_len as usize,
                                        ));
                                    }
                                    tracker.record(m);
                                    physical_bytes += byte_len;
                                    extents_transferred += 1;

                                    for k in 0..alloc_len {
                                        if let Err(e) = Self::set_logical_block_ptr_in_place(
                                            &mut dst_guard,
                                            &mut new_inode,
                                            log_idx + k,
                                            dst_start + k as u64,
                                        ) {
                                            let remaining_unlinked = alloc_len - k;
                                            let _ = Self::free_contiguous_blocks_internal(
                                                &mut dst_guard,
                                                dst_start + k as u64,
                                                remaining_unlinked,
                                            );
                                            return Err(e);
                                        }
                                    }
                                    src_blk += alloc_len as u64;
                                    log_idx += alloc_len;
                                    rem -= alloc_len;
                                }
                            }
                        }

                        if !source_still_valid(&dst_guard) {
                            stale_src = true;
                            return Ok(());
                        }

                        if tracker.used_kernel_extent_op
                            || (physical_bytes > 0
                                && dst_guard.durability_mode() == DurabilityMode::Strict)
                        {
                            crate::retry_eintr(|| dst_guard.file.sync_data())?;
                        }

                        Self::write_inode_internal(&mut dst_guard, dst_ino, &new_inode)?;
                        let entry = crate::directory::DirectoryEntry {
                            inode: dst_ino,
                            hash: stored_hash,
                            name: stored_name.clone(),
                        };

                        let written_blk = if let Some((old_ino, old_dir_blk, ref old_blocks)) =
                            replace_info
                        {
                            let mut entry_bytes = Vec::with_capacity(20 + entry.name.len());
                            {
                                let mut cursor = std::io::Cursor::new(&mut entry_bytes);
                                entry.serialize_into(&mut cursor).map_err(|e| {
                                    DiskManagerError::Io(std::io::Error::other(e.to_string()))
                                })?;
                            }
                            dst_guard.backing.with_block_mut(old_dir_blk, |blk_buf| {
                                crate::directory::remove_entry_in_block_bytes(
                                    blk_buf,
                                    &stored_name,
                                    stored_hash,
                                );
                                let ins = crate::directory::find_insert_offset_in_block(blk_buf);
                                if ins + entry_bytes.len() <= BLOCK_SIZE {
                                    blk_buf[ins..ins + entry_bytes.len()]
                                        .copy_from_slice(&entry_bytes);
                                    let tail = ins + entry_bytes.len();
                                    if tail + 18 <= BLOCK_SIZE {
                                        blk_buf[tail..tail + 18].fill(0);
                                    }
                                }
                            })?;
                            dst_parent_inode.modified_at = now;
                            Self::write_inode_internal(
                                &mut dst_guard,
                                dst_parent,
                                &dst_parent_inode,
                            )?;
                            dst_guard.dir_cache.remove(dst_parent);

                            if !old_blocks.is_empty() {
                                let db_start = dst_guard.superblock.data_block_start;
                                let mut min_b = u64::MAX;
                                let mut max_b = 0u64;
                                {
                                    let mut slice =
                                        dst_guard.data_bitmap_slice_mut().ok_or_else(|| {
                                            DiskManagerError::Io(std::io::Error::other(
                                                "data bitmap not found",
                                            ))
                                        })?;
                                    let mut da = SimpleBlockAllocator::new(&mut slice, db_start);
                                    for &b in old_blocks {
                                        da.free(b)?;
                                        min_b = min_b.min(b);
                                        max_b = max_b.max(b);
                                    }
                                    if min_b <= max_b && min_b >= db_start {
                                        slice.set_dirty_bits(
                                            (min_b - db_start) as usize,
                                            (max_b - min_b + 1) as usize,
                                        );
                                    } else {
                                        slice.set_dirty_bits(0, 0);
                                    }
                                }
                                if min_b < dst_guard.free_block_hint {
                                    dst_guard.free_block_hint = min_b;
                                }
                            }
                            if let Some(mut ib) = dst_guard.inode_bitmap_slice_mut() {
                                let mut ia = SimpleBlockAllocator::new(&mut ib, 0);
                                ia.free(old_ino)?;
                                ib.set_dirty_bits(old_ino as usize, 1);
                            }
                            if old_ino < dst_guard.free_inode_hint {
                                dst_guard.free_inode_hint = old_ino;
                            }
                            dst_guard.bump_inode_gen(old_ino);
                            dst_guard.inode_cache.remove(old_ino);
                            dst_guard.invalidate_payload_caches(old_ino);
                            old_dir_blk
                        } else {
                            let (wb, _, _) = Self::append_dir_entry_to_dir(
                                &mut dst_guard,
                                dst_parent,
                                &mut dst_parent_inode,
                                &entry,
                            )?;
                            dst_parent_inode.modified_at = now;
                            Self::write_inode_internal(
                                &mut dst_guard,
                                dst_parent,
                                &dst_parent_inode,
                            )?;
                            dst_guard.dir_cache.remove(dst_parent);
                            wb
                        };

                        if track_ranges {
                            let mut ranges = vec![
                                dst_guard.inode_bitmap_byte_range(),
                                dst_guard.data_bitmap_byte_range(),
                                dst_guard.inode_byte_range(dst_ino),
                                dst_guard.inode_byte_range(dst_parent),
                                dst_guard.block_byte_range(written_blk),
                            ];
                            ranges.extend(touched_ranges);
                            dst_guard.sync_mutation_ranges(&ranges)?;
                        } else {
                            dst_guard.sync_mutation_ranges(&[])?;
                        }
                        Ok(())
                    })();

                    if let Err(e) = transfer_res {
                        Self::rollback_unlinked_file(&mut dst_guard, dst_ino, &new_inode);
                        return Err(e);
                    }
                    if stale_src {
                        Self::rollback_unlinked_file(&mut dst_guard, dst_ino, &new_inode);
                        Stage2Outcome::RetrySourceChanged
                    } else {
                        Stage2Outcome::Committed(
                            dst_ino,
                            physical_bytes,
                            extents_transferred,
                            tracker.finish(),
                        )
                    }
                }
            };

            match stage2_res {
                Stage2Outcome::Committed(dst_ino, physical_bytes, extents_transferred, method) => {
                    break (
                        src_ino,
                        src_inode,
                        dst_ino,
                        physical_bytes,
                        extents_transferred,
                        method,
                    );
                }
                Stage2Outcome::RetrySourceChanged => {
                    attempt += 1;
                    if attempt >= 5 {
                        let cur_src_ino = self.lookup(src_parent, src_name)?;
                        let cur_src_inode = self.read_inode(cur_src_ino)?;
                        return self.transfer_logical_fallback(
                            src_parent,
                            src_name,
                            cur_src_ino,
                            &cur_src_inode,
                            dst,
                            dst_parent,
                            dst_name,
                            options,
                        );
                    }
                }
            }
        };

        dst.maybe_drain_async_ranges();

        if options.move_file {
            dst.flush()?;
            self.delete_file(src_parent, src_name)?;
        }

        Ok(TransferStats {
            src_inode: src_ino,
            dst_inode: dst_ino,
            logical_bytes: src_inode.size,
            physical_bytes,
            extents_transferred,
            method,
        })
    }

    /// Splices a byte sub-range `[src_off, src_off + len)` from `src_inode_id` in `self`
    /// into `[dst_off, dst_off + len)` of `dst_inode_id` in `dst`, using Sandwich
    /// (`Head / Body / Tail`) alignment splitting and the 3-tier transfer pipeline on
    /// aligned body units (`4 KiB` for uncompressed files, `64 KiB` for Seekable 64K files
    /// with identical filter metadata).
    ///
    /// Two-image crash safety (`Copy-Commit-Then-Trim`):
    /// - Phase 1 & 2 populate `dst` (with `fdatasync` before WAL commit), commit `dst`
    ///   metadata, and flush `dst` before touching `self`.
    /// - Phase 3 executes `SpliceTrimMode::PunchHole` or `SpliceTrimMode::CollapseRange`
    ///   on `self` in its own atomic WAL transaction, followed by best-effort host-OS
    ///   `fallocate(FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE)` on freed source blocks.
    #[allow(clippy::too_many_arguments)]
    pub fn splice_range_to(
        &self,
        src_inode_id: u64,
        src_off: u64,
        dst: &DiskManager,
        dst_inode_id: u64,
        dst_off: u64,
        len: u64,
        options: SpliceOptions,
    ) -> Result<SpliceStats, DiskManagerError> {
        if dst.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        if options.trim_mode != SpliceTrimMode::Keep && self.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        if src_off.checked_add(len).is_none() || dst_off.checked_add(len).is_none() {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "splice_range_to offset + len overflow",
            )));
        }

        // Inspect source inode under `self.inner.read()`, then strictly drop `src_guard`.
        let (src_inode, src_encrypted, src_fd, src_file_id, src_dicts) = {
            let src_guard = self.inner.read().unwrap();
            let src_inode = Self::read_inode_internal(&src_guard, src_inode_id)?;
            if src_inode.mode != crate::inode::FileType::File {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "splice_range_to only supports regular files",
                )));
            }
            let src_encrypted = src_guard.superblock.encrypted
                || src_guard.encryption_key.is_some()
                || src_inode.encrypted;
            let src_fd = src_guard.file.as_raw_fd();
            let src_file_id = canonical_file_id(src_fd).ok();
            let src_dicts = Arc::clone(&src_guard.zstd_dicts);
            (src_inode, src_encrypted, src_fd, src_file_id, src_dicts)
        };

        // Inspect destination inode under `dst.inner.read()`, then strictly drop `dst_guard`.
        let (dst_inode, dst_encrypted, dst_file_id, dst_dicts) = {
            let dst_guard = dst.inner.read().unwrap();
            let dst_inode = Self::read_inode_internal(&dst_guard, dst_inode_id)?;
            if dst_inode.mode != crate::inode::FileType::File {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "splice_range_to only supports regular files",
                )));
            }
            let dst_encrypted = dst_guard.superblock.encrypted
                || dst_guard.encryption_key.is_some()
                || dst_inode.encrypted;
            let dst_file_id = canonical_file_id(dst_guard.file.as_raw_fd()).ok();
            let dst_dicts = Arc::clone(&dst_guard.zstd_dicts);
            (dst_inode, dst_encrypted, dst_file_id, dst_dicts)
        };

        // Clamp length to available bytes in source.
        let eff_len = if src_off >= src_inode.size || len == 0 {
            0
        } else {
            len.min(src_inode.size - src_off)
        };
        if eff_len == 0 {
            return Ok(SpliceStats {
                src_inode: src_inode_id,
                dst_inode: dst_inode_id,
                bytes_spliced: 0,
                logical_rmw_bytes: 0,
                body_fast_path_bytes: 0,
                physical_bytes: 0,
                extents_transferred: 0,
                src_blocks_freed: 0,
                method: TransferMethod::RawBlockCopy,
            });
        }
        if dst_off.checked_add(eff_len).is_none() {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "splice_range_to destination range overflow",
            )));
        }
        dst.ensure_capacity_for(eff_len.div_ceil(BLOCK_SIZE as u64) + 8, 0)?;

        let same_backing_file = Arc::ptr_eq(&self.inner, &dst.inner)
            || (src_file_id.is_some() && src_file_id == dst_file_id);
        let same_inode = same_backing_file && src_inode_id == dst_inode_id;

        let src_is_compressed = src_inode.compressed_size > 0
            || (src_inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
        let unit_opt = if same_inode
            || !dicts_compatible_for_raw_copy(src_is_compressed, &src_dicts, &dst_dicts)
        {
            None
        } else {
            can_splice_fast_path(&src_inode, &dst_inode, src_encrypted, dst_encrypted)
        };
        let partition = match unit_opt {
            Some(unit_size) => plan_splice_partition(src_off, dst_off, eff_len, unit_size)
                .ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "splice_range_to partition overflow",
                    ))
                })?,
            None => plan_splice_partition(src_off, dst_off, eff_len, 0).ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "splice_range_to partition overflow",
                ))
            })?,
        };

        let dev_pair = match (src_file_id, dst_file_id) {
            (Some((sd, _)), Some((dd, _))) => Some((sd, dd)),
            _ => None,
        };

        let mut logical_rmw_bytes = 0u64;
        let mut body_fast_path_bytes = 0u64;
        let mut physical_bytes = 0u64;
        let mut extents_transferred = 0usize;
        let mut body_method: Option<TransferMethod> = None;

        // Helper for logical RMW copy of a range (`Head` or `Tail`, or full fallback).
        let read_exact_logical = |off: u64, n: u64| -> Result<Vec<u8>, DiskManagerError> {
            let total = usize::try_from(n).map_err(|_| {
                DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "logical splice slice exceeds addressable memory",
                ))
            })?;
            let mut buf = vec![0u8; total];
            let mut got = 0usize;
            while got < total {
                let r = self.read_at(src_inode_id, off + got as u64, &mut buf[got..])?;
                if r == 0 {
                    buf.truncate(got);
                    break;
                }
                got += r;
            }
            Ok(buf)
        };

        // Helper for writing a logical splice slice without truncating the tail of a
        // stream-compressed / encrypted / filtered non-seekable destination file.
        let write_splice_logical =
            |dst_off_u64: u64, slice: &[u8]| -> Result<(), DiskManagerError> {
                if slice.is_empty() {
                    return Ok(());
                }
                let cur_dst = dst.read_inode(dst_inode_id)?;
                if cur_dst.mode != crate::inode::FileType::File {
                    return Err(DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "splice_range_to only supports regular files",
                    )));
                }
                let (cur_comp_mode, cur_filter_cfg) = inode_write_params(&cur_dst);
                let is_cur_seekable = (cur_dst.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
                let is_cur_transformed = cur_dst.compressed_size > 0
                    || cur_dst.encrypted
                    || dst.is_encrypted()
                    || cur_filter_cfg.is_active();
                if !is_cur_seekable
                    && is_cur_transformed
                    && dst_off_u64 == 0
                    && (slice.len() as u64) < cur_dst.size
                {
                    let mut full = dst.read_data(dst_inode_id)?;
                    if full.len() > slice.len() {
                        full[..slice.len()].copy_from_slice(slice);
                        return dst.write_data_with_filters(
                            dst_inode_id,
                            0,
                            &full,
                            cur_comp_mode,
                            cur_filter_cfg,
                        );
                    }
                }
                dst.write_data_with_filters(
                    dst_inode_id,
                    dst_off_u64,
                    slice,
                    cur_comp_mode,
                    cur_filter_cfg,
                )
            };

        // --- Phase 1 & 2: Populate Destination (`Head` -> `Body` -> `Tail`) ---
        if partition.head_len > 0 {
            let head_buf = read_exact_logical(partition.head_src_off, partition.head_len)?;
            write_splice_logical(partition.head_dst_off, &head_buf)?;
            logical_rmw_bytes += partition.head_len;
        }

        if partition.body_len > 0 {
            let unit_size = unit_opt.expect("body_len > 0 requires fast-path unit_size");
            let is_seekable = unit_size == crate::inode::CHUNK_SIZE_64K as u64;
            let src_start_unit = (partition.body_src_off / unit_size) as usize;
            let dst_start_unit = (partition.body_dst_off / unit_size) as usize;
            let num_units = (partition.body_len / unit_size) as usize;

            enum SpliceBodyOutcome {
                Retry,
                FallbackLogical,
                Done(u64, usize, TransferMethod),
            }

            let mut attempt = 0usize;
            let body_outcome = loop {
                if !self.is_read_only() {
                    self.flush()?;
                }

                // Collect source extents under `self.inner.read()`, then strictly drop `src_guard`.
                let (src_gen, cur_src_snap, src_chunks, src_runs, populated_rel_units) = {
                    let src_guard = self.inner.read().unwrap();
                    if !src_guard.is_inode_allocated(src_inode_id) {
                        return Err(DiskManagerError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "Source inode not found",
                        )));
                    }
                    let cur_src = Self::read_inode_internal(&src_guard, src_inode_id)?;
                    if cur_src.mode != crate::inode::FileType::File {
                        return Err(DiskManagerError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "splice_range_to only supports regular files",
                        )));
                    }
                    let cur_src_enc = src_guard.superblock.encrypted
                        || src_guard.encryption_key.is_some()
                        || cur_src.encrypted;
                    let cur_src_seek = (cur_src.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
                    let expected_still_ok = !cur_src_enc
                        && cur_src_seek == is_seekable
                        && (is_seekable || cur_src.compressed_size == 0);
                    if !expected_still_ok {
                        break SpliceBodyOutcome::FallbackLogical;
                    }
                    let s_gen = src_guard.inode_gen(src_inode_id);
                    let mut populated = vec![false; num_units];
                    if is_seekable {
                        let mut chunks = Vec::new();
                        Self::for_each_logical_leaf_in_range(
                            &src_guard,
                            &cur_src,
                            src_start_unit,
                            src_start_unit + num_units,
                            |chunk_idx, val| {
                                let entry = crate::inode::ChunkEntry(val);
                                let start = entry.start_block() as u64;
                                let cnt = entry.block_count() as u64;
                                if !entry.is_empty()
                                    && cnt > 0
                                    && src_guard.is_data_block(start)
                                    && src_guard.is_data_block(start.saturating_add(cnt - 1))
                                {
                                    let rel = chunk_idx - src_start_unit;
                                    populated[rel] = true;
                                    chunks.push(ChunkExtent {
                                        chunk_idx: rel,
                                        entry,
                                    });
                                }
                            },
                        );
                        (s_gen, cur_src, chunks, Vec::new(), populated)
                    } else {
                        let mut runs: Vec<LogicalRun> = Vec::new();
                        Self::for_each_logical_leaf_in_range(
                            &src_guard,
                            &cur_src,
                            src_start_unit,
                            src_start_unit + num_units,
                            |logical_idx, phys_blk| {
                                if src_guard.is_data_block(phys_blk) {
                                    let rel = logical_idx - src_start_unit;
                                    populated[rel] = true;
                                    push_logical_block_run(&mut runs, rel, phys_blk);
                                }
                            },
                        );
                        (s_gen, cur_src, Vec::new(), runs, populated)
                    }
                };

                // Flush destination before FD-level writes.
                dst.flush()?;

                #[cfg(test)]
                run_splice_before_dst_commit_hook();

                let step_outcome = {
                    let mut dst_guard = dst.inner.write().unwrap();
                    if dst_guard.read_only {
                        return Err(DiskManagerError::ReadOnly);
                    }
                    if !dst_guard.is_inode_allocated(dst_inode_id) {
                        return Err(DiskManagerError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "Destination inode not found",
                        )));
                    }
                    let dst_fd = dst_guard.file.as_raw_fd();
                    let mut cur_dst = Self::read_inode_internal(&dst_guard, dst_inode_id)?;
                    if cur_dst.mode != crate::inode::FileType::File {
                        return Err(DiskManagerError::Io(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "splice_range_to only supports regular files",
                        )));
                    }
                    let cur_dst_enc = dst_guard.superblock.encrypted
                        || dst_guard.encryption_key.is_some()
                        || cur_dst.encrypted;
                    let cur_src_is_comp = cur_src_snap.compressed_size > 0
                        || (cur_src_snap.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
                    if can_splice_fast_path(&cur_src_snap, &cur_dst, false, cur_dst_enc)
                        != Some(unit_size)
                        || !dicts_compatible_for_raw_copy(
                            cur_src_is_comp,
                            &src_dicts,
                            &dst_guard.zstd_dicts,
                        )
                    {
                        SpliceBodyOutcome::FallbackLogical
                    } else {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs();

                        let mut old_dst_spans: Vec<(u64, u64)> = Vec::new();
                        let mut dst_holes_to_clear: Vec<usize> = Vec::new();
                        Self::for_each_logical_leaf_in_range(
                            &dst_guard,
                            &cur_dst,
                            dst_start_unit,
                            dst_start_unit + num_units,
                            |dst_unit, raw| {
                                if let Some(span) =
                                    raw_slot_block_span(&dst_guard, raw, is_seekable)
                                {
                                    old_dst_spans.push(span);
                                }
                                let rel = dst_unit - dst_start_unit;
                                if !populated_rel_units[rel] {
                                    dst_holes_to_clear.push(dst_unit);
                                }
                            },
                        );
                        coalesce_block_spans(&mut old_dst_spans);

                        let src_still_valid = |dg: &DiskManagerInner| -> bool {
                            if Arc::ptr_eq(&self.inner, &dst.inner) {
                                dg.is_inode_allocated(src_inode_id)
                                    && dg.inode_gen(src_inode_id) == src_gen
                            } else if let Ok(sg) = self.inner.try_read() {
                                sg.is_inode_allocated(src_inode_id)
                                    && sg.inode_gen(src_inode_id) == src_gen
                            } else {
                                false
                            }
                        };

                        let journaled = dst_guard.superblock.has_journal_layout();
                        let mut tracker = TierTracker::default();
                        let mut b_phys = 0u64;
                        let mut b_ext = 0usize;

                        if journaled {
                            let mut sim = crate::journal::AllocSim::new_data_only(
                                &dst_guard,
                                dst_guard.free_inode_hint,
                                dst_guard.free_block_hint,
                            )?;
                            let mut ops = Vec::new();
                            let mut payload_ranges: Vec<(usize, usize)> = Vec::new();
                            let track_ranges =
                                dst_guard.durability_mode() == DurabilityMode::RangeAsync;

                            if is_seekable {
                                for ce in &src_chunks {
                                    let count = ce.entry.block_count() as usize;
                                    let dst_start =
                                        sim.alloc_contiguous(&dst_guard.backing, count)?;
                                    let dst_start_u32 = u32::try_from(dst_start).map_err(|_| {
                                        DiskManagerError::Io(std::io::Error::other(
                                            "Destination block exceeds 32-bit ChunkEntry range",
                                        ))
                                    })?;
                                    let count_u32 = u32::try_from(count).map_err(|_| {
                                        DiskManagerError::Io(std::io::Error::other(
                                            "Extent count overflow",
                                        ))
                                    })?;
                                    crate::journal::push_data_bitmap_op(
                                        &mut ops, dst_start, count_u32, true,
                                    );

                                    let byte_len = count as u64 * BLOCK_SIZE as u64;
                                    let m = transfer_byte_range_with_devs(
                                        src_fd,
                                        ce.entry.start_block() as u64 * BLOCK_SIZE as u64,
                                        dst_fd,
                                        dst_start * BLOCK_SIZE as u64,
                                        byte_len,
                                        dev_pair,
                                    )?;
                                    dst_guard
                                        .backing
                                        .invalidate_block_range_caches(dst_start, count as u64);
                                    if track_ranges {
                                        payload_ranges.push((
                                            (dst_start as usize) * BLOCK_SIZE,
                                            byte_len as usize,
                                        ));
                                    }
                                    tracker.record(m);
                                    b_phys += byte_len;
                                    b_ext += 1;

                                    let new_entry = rebase_chunk_entry(ce.entry, dst_start_u32);
                                    crate::journal::sim_set_logical_chunk_entry(
                                        &dst_guard.backing,
                                        &mut cur_dst,
                                        dst_start_unit + ce.chunk_idx,
                                        new_entry,
                                        &mut sim,
                                        &mut ops,
                                    )?;
                                }
                            } else {
                                for run in &src_runs {
                                    let mut rem = run.count as usize;
                                    let mut src_blk = run.phys_start;
                                    let mut rel_idx = run.logical_start;
                                    while rem > 0 {
                                        let (dst_start, alloc_len) = Self::sim_alloc_run_greedy(
                                            &dst_guard.backing,
                                            &mut sim,
                                            rem,
                                        )?;
                                        let alloc_u32 = u32::try_from(alloc_len).map_err(|_| {
                                            DiskManagerError::Io(std::io::Error::other(
                                                "Extent count overflow",
                                            ))
                                        })?;
                                        crate::journal::push_data_bitmap_op(
                                            &mut ops, dst_start, alloc_u32, true,
                                        );
                                        let byte_len = alloc_len as u64 * BLOCK_SIZE as u64;
                                        let m = transfer_byte_range_with_devs(
                                            src_fd,
                                            src_blk * BLOCK_SIZE as u64,
                                            dst_fd,
                                            dst_start * BLOCK_SIZE as u64,
                                            byte_len,
                                            dev_pair,
                                        )?;
                                        dst_guard.backing.invalidate_block_range_caches(
                                            dst_start,
                                            alloc_len as u64,
                                        );
                                        if track_ranges {
                                            payload_ranges.push((
                                                (dst_start as usize) * BLOCK_SIZE,
                                                byte_len as usize,
                                            ));
                                        }
                                        tracker.record(m);
                                        b_phys += byte_len;
                                        b_ext += 1;

                                        for k in 0..alloc_len {
                                            crate::journal::sim_set_logical_block_ptr(
                                                &dst_guard.backing,
                                                &mut cur_dst,
                                                dst_start_unit + rel_idx + k,
                                                dst_start + k as u64,
                                                &mut sim,
                                                &mut ops,
                                            )?;
                                        }
                                        src_blk += alloc_len as u64;
                                        rel_idx += alloc_len;
                                        rem -= alloc_len;
                                    }
                                }
                            }

                            if !src_still_valid(&dst_guard) {
                                SpliceBodyOutcome::Retry
                            } else {
                                for dst_unit in dst_holes_to_clear {
                                    crate::journal::sim_clear_logical_slot(
                                        &dst_guard.backing,
                                        &mut cur_dst,
                                        dst_unit,
                                        &mut sim,
                                        &mut ops,
                                    );
                                }
                                for &(old_start, old_cnt) in &old_dst_spans {
                                    sim.free_contiguous(
                                        &dst_guard.backing,
                                        old_start,
                                        old_cnt as usize,
                                    )?;
                                    let cnt_u32 = u32::try_from(old_cnt).map_err(|_| {
                                        DiskManagerError::Io(std::io::Error::other(
                                            "Freed extent overflow",
                                        ))
                                    })?;
                                    crate::journal::push_data_bitmap_op(
                                        &mut ops, old_start, cnt_u32, false,
                                    );
                                }

                                if tracker.used_kernel_extent_op
                                    || (b_phys > 0
                                        && dst_guard.durability_mode() == DurabilityMode::Strict)
                                {
                                    crate::retry_eintr(|| dst_guard.file.sync_data())?;
                                }

                                cur_dst.size = cur_dst
                                    .size
                                    .max(partition.body_dst_off + partition.body_len);
                                cur_dst.modified_at = now;
                                let dst_img = Self::build_inode_post_image(
                                    &dst_guard,
                                    dst_inode_id,
                                    &cur_dst,
                                )?;
                                ops.push(crate::journal::MetadataOp::WriteInode {
                                    inode_id: dst_inode_id,
                                    inode_bytes: Box::new(dst_img),
                                });

                                let ops = if crate::journal::is_already_coalesced(&ops) {
                                    ops
                                } else {
                                    crate::journal::coalesce_ops(ops)
                                };
                                Self::commit_journal_tx(&mut dst_guard, &ops)?;
                                Self::apply_journal_ops(&mut dst_guard, &ops)?;
                                dst_guard.free_block_hint = sim.free_block_hint;
                                dst_guard.invalidate_payload_caches(dst_inode_id);
                                dst_guard.cache_inode(dst_inode_id, cur_dst);

                                if dst_guard.durability_mode() == DurabilityMode::RangeAsync {
                                    let mut ranges = vec![
                                        dst_guard.data_bitmap_byte_range(),
                                        dst_guard.inode_byte_range(dst_inode_id),
                                    ];
                                    ranges.extend(payload_ranges);
                                    for op in &ops {
                                        if let crate::journal::MetadataOp::WriteBlockSlice {
                                            block_id,
                                            ..
                                        } = op
                                        {
                                            ranges.push(dst_guard.block_byte_range(*block_id));
                                        }
                                    }
                                    dst_guard.sync_mutation_ranges(&ranges)?;
                                } else if dst_guard.durability_mode()
                                    == DurabilityMode::LegacyWholeMmapAsync
                                {
                                    dst_guard.sync_mutation_ranges(&[])?;
                                }
                                Self::maybe_checkpoint(&mut dst_guard)?;

                                SpliceBodyOutcome::Done(b_phys, b_ext, tracker.finish())
                            }
                        } else {
                            let mut newly_alloc_spans: Vec<(u64, usize)> = Vec::new();
                            let mut pending_chunks: Vec<(usize, crate::inode::ChunkEntry)> =
                                Vec::new();
                            let mut pending_blocks: Vec<(usize, u64)> = Vec::new();
                            let mut touched_ranges: Vec<(usize, usize)> = Vec::new();
                            let track_ranges = dst_guard.durability_mode().is_range_based();

                            let stage_res: Result<(), DiskManagerError> = (|| {
                                if is_seekable {
                                    for ce in &src_chunks {
                                        let count = ce.entry.block_count() as usize;
                                        let dst_start = Self::allocate_contiguous_blocks_internal(
                                            &mut dst_guard,
                                            count,
                                        )?;
                                        newly_alloc_spans.push((dst_start, count));
                                        let dst_start_u32 = u32::try_from(dst_start).map_err(|_| {
                                            DiskManagerError::Io(std::io::Error::other(
                                                "Destination block exceeds 32-bit ChunkEntry range",
                                            ))
                                        })?;
                                        let byte_len = count as u64 * BLOCK_SIZE as u64;
                                        let m = transfer_byte_range_with_devs(
                                            src_fd,
                                            ce.entry.start_block() as u64 * BLOCK_SIZE as u64,
                                            dst_fd,
                                            dst_start * BLOCK_SIZE as u64,
                                            byte_len,
                                            dev_pair,
                                        )?;
                                        dst_guard
                                            .backing
                                            .invalidate_block_range_caches(dst_start, count as u64);
                                        if track_ranges {
                                            touched_ranges.push((
                                                (dst_start as usize) * BLOCK_SIZE,
                                                byte_len as usize,
                                            ));
                                        }
                                        tracker.record(m);
                                        b_phys += byte_len;
                                        b_ext += 1;
                                        pending_chunks.push((
                                            dst_start_unit + ce.chunk_idx,
                                            rebase_chunk_entry(ce.entry, dst_start_u32),
                                        ));
                                    }
                                } else {
                                    for run in &src_runs {
                                        let mut rem = run.count as usize;
                                        let mut src_blk = run.phys_start;
                                        let mut rel_idx = run.logical_start;
                                        while rem > 0 {
                                            let (dst_start, alloc_len) =
                                                Self::alloc_run_greedy(&mut dst_guard, rem)?;
                                            newly_alloc_spans.push((dst_start, alloc_len));
                                            let byte_len = alloc_len as u64 * BLOCK_SIZE as u64;
                                            let m = transfer_byte_range_with_devs(
                                                src_fd,
                                                src_blk * BLOCK_SIZE as u64,
                                                dst_fd,
                                                dst_start * BLOCK_SIZE as u64,
                                                byte_len,
                                                dev_pair,
                                            )?;
                                            dst_guard.backing.invalidate_block_range_caches(
                                                dst_start,
                                                alloc_len as u64,
                                            );
                                            if track_ranges {
                                                touched_ranges.push((
                                                    (dst_start as usize) * BLOCK_SIZE,
                                                    byte_len as usize,
                                                ));
                                            }
                                            tracker.record(m);
                                            b_phys += byte_len;
                                            b_ext += 1;
                                            for k in 0..alloc_len {
                                                pending_blocks.push((
                                                    dst_start_unit + rel_idx + k,
                                                    dst_start + k as u64,
                                                ));
                                            }
                                            src_blk += alloc_len as u64;
                                            rel_idx += alloc_len;
                                            rem -= alloc_len;
                                        }
                                    }
                                }
                                Ok(())
                            })(
                            );

                            if let Err(e) = stage_res {
                                for (s, c) in newly_alloc_spans {
                                    let _ =
                                        Self::free_contiguous_blocks_internal(&mut dst_guard, s, c);
                                }
                                return Err(e);
                            }

                            if !src_still_valid(&dst_guard) {
                                for (s, c) in newly_alloc_spans {
                                    let _ =
                                        Self::free_contiguous_blocks_internal(&mut dst_guard, s, c);
                                }
                                SpliceBodyOutcome::Retry
                            } else {
                                if tracker.used_kernel_extent_op
                                    || (b_phys > 0
                                        && dst_guard.durability_mode() == DurabilityMode::Strict)
                                {
                                    crate::retry_eintr(|| dst_guard.file.sync_data())?;
                                }

                                for (idx, entry) in pending_chunks {
                                    Self::set_logical_chunk_entry(
                                        &mut dst_guard,
                                        &mut cur_dst,
                                        idx,
                                        entry,
                                        None,
                                    )?;
                                }
                                for (idx, blk) in pending_blocks {
                                    Self::set_logical_block_ptr_in_place(
                                        &mut dst_guard,
                                        &mut cur_dst,
                                        idx,
                                        blk,
                                    )?;
                                }
                                for dst_unit in dst_holes_to_clear {
                                    Self::clear_logical_slot_in_place(
                                        &mut dst_guard,
                                        &mut cur_dst,
                                        dst_unit,
                                    );
                                }

                                cur_dst.size = cur_dst
                                    .size
                                    .max(partition.body_dst_off + partition.body_len);
                                cur_dst.modified_at = now;
                                Self::write_inode_internal(&mut dst_guard, dst_inode_id, &cur_dst)?;

                                for &(old_start, old_cnt) in &old_dst_spans {
                                    Self::free_contiguous_blocks_internal(
                                        &mut dst_guard,
                                        old_start,
                                        old_cnt as usize,
                                    )?;
                                }
                                dst_guard.invalidate_payload_caches(dst_inode_id);

                                if track_ranges {
                                    let mut ranges = vec![
                                        dst_guard.data_bitmap_byte_range(),
                                        dst_guard.inode_byte_range(dst_inode_id),
                                    ];
                                    ranges.extend(touched_ranges);
                                    dst_guard.sync_mutation_ranges(&ranges)?;
                                } else {
                                    dst_guard.sync_mutation_ranges(&[])?;
                                }
                                SpliceBodyOutcome::Done(b_phys, b_ext, tracker.finish())
                            }
                        }
                    }
                };

                match step_outcome {
                    SpliceBodyOutcome::Retry => {
                        attempt += 1;
                        if attempt >= 5 {
                            break SpliceBodyOutcome::FallbackLogical;
                        }
                    }
                    other => break other,
                }
            };

            match body_outcome {
                SpliceBodyOutcome::Done(b_phys, b_ext, b_method) => {
                    dst.maybe_drain_async_ranges();
                    body_fast_path_bytes = partition.body_len;
                    physical_bytes += b_phys;
                    extents_transferred += b_ext;
                    body_method = Some(b_method);
                }
                SpliceBodyOutcome::FallbackLogical | SpliceBodyOutcome::Retry => {
                    let body_buf = read_exact_logical(partition.body_src_off, partition.body_len)?;
                    write_splice_logical(partition.body_dst_off, &body_buf)?;
                    logical_rmw_bytes += partition.body_len;
                    body_method = Some(TransferMethod::LogicalReencode);
                }
            }
        }

        if partition.tail_len > 0 {
            let tail_buf = read_exact_logical(partition.tail_src_off, partition.tail_len)?;
            write_splice_logical(partition.tail_dst_off, &tail_buf)?;
            logical_rmw_bytes += partition.tail_len;
        }

        // --- Phase 2 -> Phase 3 Barrier: Copy-Commit-Then-Trim ---
        let src_blocks_freed = match options.trim_mode {
            SpliceTrimMode::Keep => 0,
            SpliceTrimMode::PunchHole => {
                dst.flush()?;
                self.trim_source_punch_hole(src_inode_id, src_off, eff_len)?
            }
            SpliceTrimMode::CollapseRange => {
                dst.flush()?;
                self.trim_source_collapse_range(src_inode_id, src_off, eff_len)?
            }
        };

        let method = match body_method {
            Some(m) => m,
            None if logical_rmw_bytes > 0 => TransferMethod::LogicalReencode,
            None => TransferMethod::RawBlockCopy,
        };

        Ok(SpliceStats {
            src_inode: src_inode_id,
            dst_inode: dst_inode_id,
            bytes_spliced: eff_len,
            logical_rmw_bytes,
            body_fast_path_bytes,
            physical_bytes,
            extents_transferred,
            src_blocks_freed,
            method,
        })
    }

    /// Executes `SpliceTrimMode::PunchHole` on `self`: clears `[src_off, src_off + len)`
    /// to zeros while preserving `src_inode.size`, freeing full aligned unit blocks in an
    /// atomic WAL transaction and punching host FS holes best-effort.
    fn trim_source_punch_hole(
        &self,
        src_inode_id: u64,
        src_off: u64,
        len: u64,
    ) -> Result<usize, DiskManagerError> {
        if len == 0 {
            return Ok(0);
        }
        let (src_inode, src_encrypted) = {
            let guard = self.inner.read().unwrap();
            let inode = Self::read_inode_internal(&guard, src_inode_id)?;
            let enc =
                guard.superblock.encrypted || guard.encryption_key.is_some() || inode.encrypted;
            (inode, enc)
        };
        let is_seekable = (src_inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
        let (src_comp_mode, src_filter_cfg) = inode_write_params(&src_inode);

        // Stream-compressed or non-seekable encrypted files cannot punch individual blocks;
        // zero the range logically while keeping `src_inode.size` unchanged.
        if !is_seekable && (src_inode.compressed_size > 0 || src_encrypted) {
            let zeros = vec![0u8; len as usize];
            self.write_data_with_filters(
                src_inode_id,
                src_off,
                &zeros,
                src_comp_mode,
                src_filter_cfg,
            )?;
            return Ok(0);
        }

        let unit_size = if is_seekable {
            crate::inode::CHUNK_SIZE_64K as u64
        } else {
            BLOCK_SIZE as u64
        };
        let part = plan_splice_partition(src_off, src_off, len, unit_size).ok_or_else(|| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "punch_hole range overflow",
            ))
        })?;

        if part.head_len > 0 {
            let zeros = vec![0u8; part.head_len as usize];
            self.write_data_with_filters(
                src_inode_id,
                part.head_src_off,
                &zeros,
                src_comp_mode,
                src_filter_cfg,
            )?;
        }

        let mut blocks_freed = 0usize;
        if part.body_len > 0 {
            let start_unit = (part.body_src_off / unit_size) as usize;
            let num_units = (part.body_len / unit_size) as usize;
            let mut fallback_params: Option<(CompressionMode, crate::filters::FilterConfig)> = None;
            let freed_spans = {
                let mut guard = self.inner.write().unwrap();
                if guard.read_only {
                    return Err(DiskManagerError::ReadOnly);
                }
                if !guard.is_inode_allocated(src_inode_id) {
                    return Err(DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "Source inode not found",
                    )));
                }
                let mut inode = Self::read_inode_internal(&guard, src_inode_id)?;
                if inode.mode != crate::inode::FileType::File {
                    return Err(DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "punch_hole only supports regular files",
                    )));
                }
                let cur_enc =
                    guard.superblock.encrypted || guard.encryption_key.is_some() || inode.encrypted;
                let cur_seek = (inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
                if cur_seek != is_seekable || cur_enc || (!cur_seek && inode.compressed_size > 0) {
                    fallback_params = Some(inode_write_params(&inode));
                    Vec::new()
                } else {
                    let mut slots_to_clear: Vec<usize> = Vec::new();
                    let mut spans: Vec<(u64, u64)> = Vec::new();
                    Self::for_each_logical_leaf_in_range(
                        &guard,
                        &inode,
                        start_unit,
                        start_unit + num_units,
                        |unit_idx, raw| {
                            let mut slot = raw;
                            if let Some(prev) = punch_table_slot(&mut slot) {
                                slots_to_clear.push(unit_idx);
                                if let Some(span) = raw_slot_block_span(&guard, prev, is_seekable) {
                                    spans.push(span);
                                }
                            }
                        },
                    );
                    coalesce_block_spans(&mut spans);

                    if !slots_to_clear.is_empty() {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        inode.modified_at = now;
                        if guard.superblock.has_journal_layout() {
                            let mut sim = crate::journal::AllocSim::new_data_only(
                                &guard,
                                guard.free_inode_hint,
                                guard.free_block_hint,
                            )?;
                            let mut ops = Vec::new();
                            for unit_idx in slots_to_clear {
                                crate::journal::sim_clear_logical_slot(
                                    &guard.backing,
                                    &mut inode,
                                    unit_idx,
                                    &mut sim,
                                    &mut ops,
                                );
                            }
                            for &(s, c) in &spans {
                                sim.free_contiguous(&guard.backing, s, c as usize)?;
                                let c_u32 = u32::try_from(c).map_err(|_| {
                                    DiskManagerError::Io(std::io::Error::other(
                                        "Freed extent overflow",
                                    ))
                                })?;
                                crate::journal::push_data_bitmap_op(&mut ops, s, c_u32, false);
                            }
                            let img = Self::build_inode_post_image(&guard, src_inode_id, &inode)?;
                            ops.push(crate::journal::MetadataOp::WriteInode {
                                inode_id: src_inode_id,
                                inode_bytes: Box::new(img),
                            });
                            let ops = if crate::journal::is_already_coalesced(&ops) {
                                ops
                            } else {
                                crate::journal::coalesce_ops(ops)
                            };
                            Self::commit_journal_tx(&mut guard, &ops)?;
                            Self::apply_journal_ops(&mut guard, &ops)?;
                            guard.free_block_hint = sim.free_block_hint;
                            guard.invalidate_payload_caches(src_inode_id);
                            guard.cache_inode(src_inode_id, inode);
                            Self::maybe_checkpoint(&mut guard)?;
                        } else {
                            for unit_idx in slots_to_clear {
                                Self::clear_logical_slot_in_place(&mut guard, &mut inode, unit_idx);
                            }
                            Self::write_inode_internal(&mut guard, src_inode_id, &inode)?;
                            for &(s, c) in &spans {
                                Self::free_contiguous_blocks_internal(&mut guard, s, c as usize)?;
                            }
                            guard.invalidate_payload_caches(src_inode_id);
                        }

                        // Best-effort host-OS hole punch AFTER source metadata commits so CoW
                        // refcounts drop from 2 -> 1 without touching referenced blocks.
                        let src_fd = guard.file.as_raw_fd();
                        for &(s, c) in &spans {
                            punch_host_file_blocks(src_fd, s, c);
                            guard.backing.invalidate_block_range_caches(s, c);
                        }
                    }
                    spans
                }
            };
            if let Some((fb_comp_mode, fb_filter_cfg)) = fallback_params {
                let zeros = vec![0u8; part.body_len as usize];
                self.write_data_with_filters(
                    src_inode_id,
                    part.body_src_off,
                    &zeros,
                    fb_comp_mode,
                    fb_filter_cfg,
                )?;
            } else {
                self.maybe_drain_async_ranges();
                blocks_freed = freed_spans.iter().map(|&(_, c)| c as usize).sum();
            }
        }

        if part.tail_len > 0 {
            let zeros = vec![0u8; part.tail_len as usize];
            self.write_data_with_filters(
                src_inode_id,
                part.tail_src_off,
                &zeros,
                src_comp_mode,
                src_filter_cfg,
            )?;
        }

        Ok(blocks_freed)
    }

    /// Executes `SpliceTrimMode::CollapseRange` on `self`: removes `[src_off, src_off + len)`
    /// and shrinks `src_inode.size` by `len`. When `src_off` and `len` (or EOF removal) are
    /// aligned to the file's unit granularity on an unencrypted Raw/Seekable file, performs
    /// O(remaining units) pointer/chunk-table shift left with zero payload copying.
    fn trim_source_collapse_range(
        &self,
        src_inode_id: u64,
        src_off: u64,
        len: u64,
    ) -> Result<usize, DiskManagerError> {
        if len == 0 {
            return Ok(0);
        }

        #[cfg(test)]
        run_collapse_before_write_hook();

        // Inspect layout and perform pointer-shift collapse under a single `self.inner.write()`
        // lock so concurrent appends/writes can never create a cross-lock TOCTOU mismatch.
        let fallback_info: (Inode, bool, bool) = {
            let mut guard = self.inner.write().unwrap();
            if guard.read_only {
                return Err(DiskManagerError::ReadOnly);
            }
            if !guard.is_inode_allocated(src_inode_id) {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Source inode not found",
                )));
            }
            let mut inode = Self::read_inode_internal(&guard, src_inode_id)?;
            if inode.mode != crate::inode::FileType::File {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "collapse_range only supports regular files",
                )));
            }
            let src_encrypted =
                guard.superblock.encrypted || guard.encryption_key.is_some() || inode.encrypted;
            let is_seekable = (inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
            let is_raw = !is_seekable && inode.compressed_size == 0;
            let unit_size_opt = if is_seekable {
                Some(crate::inode::CHUNK_SIZE_64K as u64)
            } else if is_raw {
                Some(BLOCK_SIZE as u64)
            } else {
                None
            };

            if src_off >= inode.size {
                return Ok(0);
            }
            let eff_len = len.min(inode.size - src_off);
            let removes_to_eof = src_off.saturating_add(eff_len) >= inode.size;
            // Encrypted seekable chunks bind AEAD AAD to `chunk_idx`, so shifting trailing chunks
            // left without re-encrypting is ONLY valid when unencrypted (or removing to EOF with
            // no shifted trailing chunks).
            let can_pointer_shift = unit_size_opt.is_some_and(|u| {
                (!src_encrypted || removes_to_eof)
                    && src_off.is_multiple_of(u)
                    && (eff_len.is_multiple_of(u) || removes_to_eof)
            });

            if can_pointer_shift {
                let unit_size = unit_size_opt.unwrap();
                let start_unit = (src_off / unit_size) as usize;
                let total_units = inode.size.div_ceil(unit_size) as usize;
                let remove_units = eff_len.div_ceil(unit_size) as usize;
                let new_size = inode.size.saturating_sub(eff_len);

                let mut table = vec![0u64; total_units];
                Self::for_each_logical_leaf(&guard, &inode, total_units, |idx, raw| {
                    if idx < total_units {
                        table[idx] = raw;
                    }
                });

                let removed_entries = shift_table_collapse(&mut table, start_unit, remove_units);
                let new_total_units = table.len();

                let mut spans: Vec<(u64, u64)> = Vec::new();
                for raw in removed_entries {
                    if let Some(span) = raw_slot_block_span(&guard, raw, is_seekable) {
                        spans.push(span);
                    }
                }

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                inode.size = new_size;
                inode.modified_at = now;

                if guard.superblock.has_journal_layout() {
                    let mut sim = crate::journal::AllocSim::new_data_only(
                        &guard,
                        guard.free_inode_hint,
                        guard.free_block_hint,
                    )?;
                    let mut ops = Vec::new();

                    // Write shifted slots `[start_unit..new_total_units)`.
                    for (idx, &raw) in table.iter().enumerate().skip(start_unit) {
                        if raw == 0 {
                            crate::journal::sim_clear_logical_slot(
                                &guard.backing,
                                &mut inode,
                                idx,
                                &mut sim,
                                &mut ops,
                            );
                        } else if is_seekable {
                            crate::journal::sim_set_logical_chunk_entry(
                                &guard.backing,
                                &mut inode,
                                idx,
                                crate::inode::ChunkEntry(raw),
                                &mut sim,
                                &mut ops,
                            )?;
                        } else {
                            crate::journal::sim_set_logical_block_ptr(
                                &guard.backing,
                                &mut inode,
                                idx,
                                raw,
                                &mut sim,
                                &mut ops,
                            )?;
                        }
                    }

                    // Clear vacated trailing slots `[new_total_units..total_units)`.
                    for idx in new_total_units..total_units {
                        crate::journal::sim_clear_logical_slot(
                            &guard.backing,
                            &mut inode,
                            idx,
                            &mut sim,
                            &mut ops,
                        );
                    }

                    // Prune indirect pointer blocks strictly beyond `new_total_units` so fsck
                    // never sees unreachable allocated indirect blocks.
                    let pruned_indirect = Self::prune_trailing_indirect_blocks_journaled(
                        &guard,
                        &mut inode,
                        new_total_units,
                        &mut sim,
                        &mut ops,
                    );
                    for blk in pruned_indirect {
                        spans.push((blk, 1));
                    }
                    coalesce_block_spans(&mut spans);

                    for &(s, c) in &spans {
                        sim.free_contiguous(&guard.backing, s, c as usize)?;
                        let c_u32 = u32::try_from(c).map_err(|_| {
                            DiskManagerError::Io(std::io::Error::other("Freed extent overflow"))
                        })?;
                        crate::journal::push_data_bitmap_op(&mut ops, s, c_u32, false);
                    }

                    let img = Self::build_inode_post_image(&guard, src_inode_id, &inode)?;
                    ops.push(crate::journal::MetadataOp::WriteInode {
                        inode_id: src_inode_id,
                        inode_bytes: Box::new(img),
                    });
                    let ops = if crate::journal::is_already_coalesced(&ops) {
                        ops
                    } else {
                        crate::journal::coalesce_ops(ops)
                    };
                    Self::commit_journal_tx(&mut guard, &ops)?;
                    Self::apply_journal_ops(&mut guard, &ops)?;
                    guard.free_block_hint = sim.free_block_hint;
                    guard.invalidate_payload_caches(src_inode_id);
                    guard.cache_inode(src_inode_id, inode);
                    Self::maybe_checkpoint(&mut guard)?;
                } else {
                    for (idx, &raw) in table.iter().enumerate().skip(start_unit) {
                        if raw == 0 {
                            Self::clear_logical_slot_in_place(&mut guard, &mut inode, idx);
                        } else if is_seekable {
                            Self::set_logical_chunk_entry(
                                &mut guard,
                                &mut inode,
                                idx,
                                crate::inode::ChunkEntry(raw),
                                None,
                            )?;
                        } else {
                            Self::set_logical_block_ptr_in_place(&mut guard, &mut inode, idx, raw)?;
                        }
                    }
                    for idx in new_total_units..total_units {
                        Self::clear_logical_slot_in_place(&mut guard, &mut inode, idx);
                    }
                    let pruned_indirect = Self::prune_trailing_indirect_blocks_in_place(
                        &mut guard,
                        &mut inode,
                        new_total_units,
                    );
                    for blk in pruned_indirect {
                        spans.push((blk, 1));
                    }
                    coalesce_block_spans(&mut spans);

                    Self::write_inode_internal(&mut guard, src_inode_id, &inode)?;
                    for &(s, c) in &spans {
                        Self::free_contiguous_blocks_internal(&mut guard, s, c as usize)?;
                    }
                    guard.invalidate_payload_caches(src_inode_id);
                }

                let src_fd = guard.file.as_raw_fd();
                for &(s, c) in &spans {
                    punch_host_file_blocks(src_fd, s, c);
                    guard.backing.invalidate_block_range_caches(s, c);
                }
                drop(guard);
                self.maybe_drain_async_ranges();
                return Ok(spans.iter().map(|&(_, c)| c as usize).sum());
            }

            (inode, src_encrypted, is_seekable)
        };

        let (src_inode, src_encrypted, is_seekable) = fallback_info;

        // Unaligned / stream-compressed / encrypted collapse fallback:
        // shift trailing bytes left and truncate to `src_inode.size - len`.
        let (src_comp_mode, src_filter_cfg) = inode_write_params(&src_inode);
        let tail_start = src_off + len;
        let new_size = src_inode.size.saturating_sub(len);
        if tail_start >= src_inode.size {
            self.truncate(src_inode_id, new_size)?;
            return Ok(0);
        }

        if !is_seekable && (src_inode.compressed_size > 0 || src_encrypted) {
            let mut full = self.read_data(src_inode_id)?;
            let s = (src_off as usize).min(full.len());
            let e = (tail_start as usize).min(full.len());
            full.drain(s..e);
            self.write_data_with_filters(src_inode_id, 0, &full, src_comp_mode, src_filter_cfg)?;
            if full.is_empty() {
                self.truncate(src_inode_id, 0)?;
            }
            return Ok(0);
        }

        // Stream-shift trailing bytes forward in 256 KiB chunks (safe: `dst_pos < src_pos`).
        let tail_total = src_inode.size - tail_start;
        let mut moved = 0u64;
        let mut buf = vec![0u8; 256 * 1024];
        while moved < tail_total {
            let step = ((tail_total - moved) as usize).min(buf.len());
            let mut got = 0usize;
            while got < step {
                let r = self.read_at(
                    src_inode_id,
                    tail_start + moved + got as u64,
                    &mut buf[got..step],
                )?;
                if r == 0 {
                    break;
                }
                got += r;
            }
            if got == 0 {
                break;
            }
            self.write_data_with_filters(
                src_inode_id,
                src_off + moved,
                &buf[..got],
                src_comp_mode,
                src_filter_cfg,
            )?;
            moved += got as u64;
        }
        self.truncate(src_inode_id, new_size)?;
        Ok(0)
    }

    /// Prunes indirect pointer blocks whose logical slot range lies strictly `>= new_total_units`
    /// in a journaled transaction, returning all freed indirect block IDs.
    fn prune_trailing_indirect_blocks_journaled(
        guard: &DiskManagerInner,
        inode: &mut Inode,
        new_total_units: usize,
        sim: &mut crate::journal::AllocSim,
        ops: &mut Vec<crate::journal::MetadataOp>,
    ) -> Vec<u64> {
        use crate::inode::{DIRECT_BLOCKS, PTRS_PER_BLOCK};

        let mut freed = Vec::new();
        let read_ptr = |parent: u64, slot: usize, sim: &crate::journal::AllocSim| -> u64 {
            if let Some(&v) = sim.pointers.get(&(parent, slot)) {
                v
            } else if sim.is_fresh(parent) {
                0
            } else {
                guard.backing.read_block_ptr(parent, slot)
            }
        };

        let single_base = DIRECT_BLOCKS;
        let double_base = single_base + PTRS_PER_BLOCK;
        let per_dib = PTRS_PER_BLOCK * PTRS_PER_BLOCK;
        let triple_base = double_base + per_dib;

        // Single indirect (10..522)
        if new_total_units <= single_base && guard.is_data_block(inode.blocks[10]) {
            freed.push(inode.blocks[10]);
            inode.blocks[10] = 0;
        }

        // Double indirect (522..262_666)
        let dib = inode.blocks[11];
        if guard.is_data_block(dib) {
            let keep_a = if new_total_units <= double_base {
                0
            } else {
                (new_total_units - double_base)
                    .div_ceil(PTRS_PER_BLOCK)
                    .min(PTRS_PER_BLOCK)
            };
            for a in keep_a..PTRS_PER_BLOCK {
                let sib = read_ptr(dib, a, sim);
                if guard.is_data_block(sib) {
                    freed.push(sib);
                    if keep_a > 0 {
                        sim.pointers.insert((dib, a), 0);
                        ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                            block_id: dib,
                            offset: (a * 8) as u32,
                            data: 0u64.to_le_bytes().to_vec(),
                        });
                    }
                }
            }
            if keep_a == 0 {
                freed.push(dib);
                inode.blocks[11] = 0;
            }
        }

        // Triple indirect (262_666..MAX)
        let tib = inode.triple_indirect;
        if guard.is_data_block(tib) {
            let keep_a = if new_total_units <= triple_base {
                0
            } else {
                (new_total_units - triple_base)
                    .div_ceil(per_dib)
                    .min(PTRS_PER_BLOCK)
            };
            for a in 0..PTRS_PER_BLOCK {
                let dib_blk = read_ptr(tib, a, sim);
                if !guard.is_data_block(dib_blk) {
                    continue;
                }
                if a >= keep_a {
                    for b in 0..PTRS_PER_BLOCK {
                        let sib = read_ptr(dib_blk, b, sim);
                        if guard.is_data_block(sib) {
                            freed.push(sib);
                        }
                    }
                    freed.push(dib_blk);
                    if keep_a > 0 {
                        sim.pointers.insert((tib, a), 0);
                        ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                            block_id: tib,
                            offset: (a * 8) as u32,
                            data: 0u64.to_le_bytes().to_vec(),
                        });
                    }
                } else if a + 1 == keep_a {
                    let dib_base = triple_base + a * per_dib;
                    let keep_b = new_total_units
                        .saturating_sub(dib_base)
                        .div_ceil(PTRS_PER_BLOCK)
                        .min(PTRS_PER_BLOCK);
                    for b in keep_b..PTRS_PER_BLOCK {
                        let sib = read_ptr(dib_blk, b, sim);
                        if guard.is_data_block(sib) {
                            freed.push(sib);
                            sim.pointers.insert((dib_blk, b), 0);
                            ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                                block_id: dib_blk,
                                offset: (b * 8) as u32,
                                data: 0u64.to_le_bytes().to_vec(),
                            });
                        }
                    }
                }
            }
            if keep_a == 0 {
                freed.push(tib);
                inode.triple_indirect = 0;
            }
        }

        freed
    }

    /// Prunes indirect pointer blocks whose logical slot range lies strictly `>= new_total_units`
    /// in place (non-journaled), returning all freed indirect block IDs.
    fn prune_trailing_indirect_blocks_in_place(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        new_total_units: usize,
    ) -> Vec<u64> {
        use crate::inode::{DIRECT_BLOCKS, PTRS_PER_BLOCK};

        let mut freed = Vec::new();
        let single_base = DIRECT_BLOCKS;
        let double_base = single_base + PTRS_PER_BLOCK;
        let per_dib = PTRS_PER_BLOCK * PTRS_PER_BLOCK;
        let triple_base = double_base + per_dib;

        if new_total_units <= single_base && guard.is_data_block(inode.blocks[10]) {
            freed.push(inode.blocks[10]);
            inode.blocks[10] = 0;
        }

        let dib = inode.blocks[11];
        if guard.is_data_block(dib) {
            let keep_a = if new_total_units <= double_base {
                0
            } else {
                (new_total_units - double_base)
                    .div_ceil(PTRS_PER_BLOCK)
                    .min(PTRS_PER_BLOCK)
            };
            for a in keep_a..PTRS_PER_BLOCK {
                let sib = guard.backing.read_block_ptr(dib, a);
                if guard.is_data_block(sib) {
                    freed.push(sib);
                    if keep_a > 0 {
                        guard.backing.write_block_ptr(dib, a, 0);
                    }
                }
            }
            if keep_a == 0 {
                freed.push(dib);
                inode.blocks[11] = 0;
            }
        }

        let tib = inode.triple_indirect;
        if guard.is_data_block(tib) {
            let keep_a = if new_total_units <= triple_base {
                0
            } else {
                (new_total_units - triple_base)
                    .div_ceil(per_dib)
                    .min(PTRS_PER_BLOCK)
            };
            for a in 0..PTRS_PER_BLOCK {
                let dib_blk = guard.backing.read_block_ptr(tib, a);
                if !guard.is_data_block(dib_blk) {
                    continue;
                }
                if a >= keep_a {
                    for b in 0..PTRS_PER_BLOCK {
                        let sib = guard.backing.read_block_ptr(dib_blk, b);
                        if guard.is_data_block(sib) {
                            freed.push(sib);
                        }
                    }
                    freed.push(dib_blk);
                    if keep_a > 0 {
                        guard.backing.write_block_ptr(tib, a, 0);
                    }
                } else if a + 1 == keep_a {
                    let dib_base = triple_base + a * per_dib;
                    let keep_b = new_total_units
                        .saturating_sub(dib_base)
                        .div_ceil(PTRS_PER_BLOCK)
                        .min(PTRS_PER_BLOCK);
                    for b in keep_b..PTRS_PER_BLOCK {
                        let sib = guard.backing.read_block_ptr(dib_blk, b);
                        if guard.is_data_block(sib) {
                            freed.push(sib);
                            guard.backing.write_block_ptr(dib_blk, b, 0);
                        }
                    }
                }
            }
            if keep_a == 0 {
                freed.push(tib);
                inode.triple_indirect = 0;
            }
        }

        freed
    }

    #[allow(clippy::too_many_arguments)]
    fn transfer_logical_fallback(
        &self,
        src_parent: u64,
        src_name: &str,
        src_ino: u64,
        src_inode: &Inode,
        dst: &DiskManager,
        dst_parent: u64,
        dst_name: &str,
        options: TransferOptions,
    ) -> Result<TransferStats, DiskManagerError> {
        static NEXT_XFER_TMP_ID: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);

        let data = self.read_data(src_ino)?;
        let existing_target = if let Ok(existing_ino) = dst.lookup(dst_parent, dst_name) {
            if !options.overwrite {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("Destination '{dst_name}' already exists"),
                )));
            }
            let existing = dst.read_inode(existing_ino)?;
            if existing.mode == crate::inode::FileType::Directory {
                return Err(DiskManagerError::Io(std::io::Error::other(
                    "Is a directory",
                )));
            }
            true
        } else {
            false
        };

        let is_seekable = (src_inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) != 0;
        let mode = if is_seekable {
            CompressionMode::Seekable {
                chunk_size: crate::inode::CHUNK_SIZE_64K as u32,
                level: 3,
            }
        } else if src_inode.compressed_size > 0 {
            CompressionMode::Always
        } else {
            CompressionMode::Never
        };
        let filter_config = crate::filters::FilterConfig {
            typesize: src_inode.filter_typesize,
            delta: src_inode.filter_delta,
            shuffle: src_inode.filter_shuffle,
            bitshuffle: src_inode.filter_bitshuffle,
        };

        // Stage overwrite into a temporary entry and atomically `rename` it into place so
        // `NoSpace` or write errors never destroy an existing destination file.
        let stage_via_tmp = |dst: &DiskManager| -> Result<u64, DiskManagerError> {
            let seq = NEXT_XFER_TMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp_name = format!(".__oifs_xf_{}_{}", std::process::id(), seq);
            let tmp_ino = dst.create_file_with_data_with_filters(
                dst_parent,
                &tmp_name,
                &data,
                mode,
                filter_config,
            )?;
            if let Err(e) = dst.rename(dst_parent, &tmp_name, dst_parent, dst_name) {
                let _ = dst.delete_file(dst_parent, &tmp_name);
                return Err(e);
            }
            Ok(tmp_ino)
        };

        let dst_ino = if existing_target {
            stage_via_tmp(dst)?
        } else {
            match dst.create_file_with_data_with_filters(
                dst_parent,
                dst_name,
                &data,
                mode,
                filter_config,
            ) {
                Ok(id) => id,
                Err(DiskManagerError::Io(ref io_err))
                    if io_err.kind() == std::io::ErrorKind::AlreadyExists && options.overwrite =>
                {
                    let existing_ino = dst.lookup(dst_parent, dst_name)?;
                    let existing = dst.read_inode(existing_ino)?;
                    if existing.mode == crate::inode::FileType::Directory {
                        return Err(DiskManagerError::Io(std::io::Error::other(
                            "Is a directory",
                        )));
                    }
                    stage_via_tmp(dst)?
                }
                Err(e) => return Err(e),
            }
        };
        if options.move_file {
            dst.flush()?;
            self.delete_file(src_parent, src_name)?;
        }
        Ok(TransferStats {
            src_inode: src_ino,
            dst_inode: dst_ino,
            logical_bytes: data.len() as u64,
            physical_bytes: data.len() as u64,
            extents_transferred: usize::from(!data.is_empty()),
            method: TransferMethod::LogicalReencode,
        })
    }

    /// Allocates as large a contiguous run as possible (up to `want` blocks) in `AllocSim`.
    fn sim_alloc_run_greedy(
        backing: &ImageBacking,
        sim: &mut crate::journal::AllocSim,
        want: usize,
    ) -> Result<(u64, usize), DiskManagerError> {
        let mut try_len = want.max(1);
        loop {
            match sim.alloc_contiguous(backing, try_len) {
                Ok(start) => return Ok((start, try_len)),
                Err(DiskManagerError::Allocator(AllocatorError::NoSpace)) if try_len > 1 => {
                    try_len = (try_len / 2).max(1);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Allocates as large a contiguous run as possible (up to `want` blocks) in place.
    fn alloc_run_greedy(
        guard: &mut DiskManagerInner,
        want: usize,
    ) -> Result<(u64, usize), DiskManagerError> {
        let mut try_len = want.max(1);
        loop {
            match Self::allocate_contiguous_blocks_internal(guard, try_len) {
                Ok(start) => return Ok((start, try_len)),
                Err(DiskManagerError::Allocator(AllocatorError::NoSpace)) if try_len > 1 => {
                    try_len = (try_len / 2).max(1);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Clears a single logical pointer/chunk slot to `0` in place without allocating any
    /// new indirect pointer blocks.
    fn clear_logical_slot_in_place(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        logical_idx: usize,
    ) {
        use crate::inode::BlockPath;
        match BlockPath::from_logical(logical_idx) {
            Some(BlockPath::Direct(i)) => {
                inode.blocks[i] = 0;
            }
            Some(BlockPath::Single(i)) => {
                let sib = inode.blocks[10];
                if guard.is_data_block(sib) {
                    guard.backing.write_block_ptr(sib, i, 0);
                }
            }
            Some(BlockPath::Double(a, b)) => {
                let dib = inode.blocks[11];
                if guard.is_data_block(dib) {
                    let sib = guard.backing.read_block_ptr(dib, a);
                    if guard.is_data_block(sib) {
                        guard.backing.write_block_ptr(sib, b, 0);
                    }
                }
            }
            Some(BlockPath::Triple(a, b, c)) => {
                let tib = inode.triple_indirect;
                if guard.is_data_block(tib) {
                    let dib = guard.backing.read_block_ptr(tib, a);
                    if guard.is_data_block(dib) {
                        let sib = guard.backing.read_block_ptr(dib, b);
                        if guard.is_data_block(sib) {
                            guard.backing.write_block_ptr(sib, c, 0);
                        }
                    }
                }
            }
            None => {}
        }
    }

    /// Writes a single physical block pointer into `inode`'s block tree in place,
    /// allocating and zeroing any missing indirect pointer blocks.
    fn set_logical_block_ptr_in_place(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        logical_idx: usize,
        phys_blk: u64,
    ) -> Result<(), DiskManagerError> {
        use crate::inode::BlockPath;
        if phys_blk == 0 {
            return Ok(());
        }
        let Some(path) = BlockPath::from_logical(logical_idx) else {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Logical block index exceeds maximum file size",
            )));
        };
        match path {
            BlockPath::Direct(i) => {
                inode.blocks[i] = phys_blk;
            }
            BlockPath::Single(i) => {
                if inode.blocks[10] == 0 {
                    inode.blocks[10] = Self::alloc_and_zero_block(guard)?;
                }
                guard.backing.write_block_ptr(inode.blocks[10], i, phys_blk);
            }
            BlockPath::Double(a, b) => {
                if inode.blocks[11] == 0 {
                    inode.blocks[11] = Self::alloc_and_zero_block(guard)?;
                }
                let mut sib = guard.backing.read_block_ptr(inode.blocks[11], a);
                if sib == 0 {
                    sib = Self::alloc_and_zero_block(guard)?;
                    guard.backing.write_block_ptr(inode.blocks[11], a, sib);
                }
                guard.backing.write_block_ptr(sib, b, phys_blk);
            }
            BlockPath::Triple(a, b, c) => {
                if inode.triple_indirect == 0 {
                    inode.triple_indirect = Self::alloc_and_zero_block(guard)?;
                }
                let mut dib = guard.backing.read_block_ptr(inode.triple_indirect, a);
                if dib == 0 {
                    dib = Self::alloc_and_zero_block(guard)?;
                    guard.backing.write_block_ptr(inode.triple_indirect, a, dib);
                }
                let mut sib = guard.backing.read_block_ptr(dib, b);
                if sib == 0 {
                    sib = Self::alloc_and_zero_block(guard)?;
                    guard.backing.write_block_ptr(dib, b, sib);
                }
                guard.backing.write_block_ptr(sib, c, phys_blk);
            }
        }
        Ok(())
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    fn any_transfer_method() -> TransferMethod {
        let tag: u8 = kani::any();
        kani::assume(tag < 4);
        match tag {
            0 => TransferMethod::Reflink,
            1 => TransferMethod::CopyFileRange,
            2 => TransferMethod::RawBlockCopy,
            _ => TransferMethod::LogicalReencode,
        }
    }

    /// Proves that the raw extent fast-path gate allows raw transfer if and only if
    /// both source and destination are unencrypted, strictly preventing ciphertext
    /// transfer across encryption boundaries.
    #[kani::proof]
    fn proof_transfer_fast_path_gate_soundness() {
        let src_enc: bool = kani::any();
        let dst_enc: bool = kani::any();
        let allow = should_use_raw_fast_path(src_enc, dst_enc);
        assert_eq!(allow, !src_enc && !dst_enc);
        if src_enc || dst_enc {
            assert!(!allow);
        } else {
            assert!(allow);
        }
    }

    /// Proves that `combine_transfer_tier` is commutative, associative, idempotent,
    /// monotone with respect to `tier_rank`, and has `Reflink` as identity and
    /// `LogicalReencode` as absorbing top element.
    #[kani::proof]
    fn proof_transfer_tier_combine_monotone() {
        let a = any_transfer_method();
        let b = any_transfer_method();
        let c = any_transfer_method();

        assert_eq!(combine_transfer_tier(a, b), combine_transfer_tier(b, a));
        assert_eq!(
            combine_transfer_tier(combine_transfer_tier(a, b), c),
            combine_transfer_tier(a, combine_transfer_tier(b, c))
        );
        assert_eq!(combine_transfer_tier(a, a), a);

        let ab = combine_transfer_tier(a, b);
        assert!(ab.tier_rank() >= a.tier_rank());
        assert!(ab.tier_rank() >= b.tier_rank());

        if a.tier_rank() <= c.tier_rank() {
            assert!(
                combine_transfer_tier(a, b).tier_rank() <= combine_transfer_tier(c, b).tier_rank()
            );
        }

        assert_eq!(combine_transfer_tier(TransferMethod::Reflink, a), a);
        assert_eq!(
            combine_transfer_tier(TransferMethod::LogicalReencode, a),
            TransferMethod::LogicalReencode
        );
    }

    /// Proves that `rebase_chunk_entry` preserves `block_count`, `flags`, and
    /// `compressed_len` while replacing only `start_block` (and keeping empty chunks
    /// empty), and that `classify_reflink_errno` partitions device-unsupported,
    /// extent-local, and fatal errnos accurately.
    #[kani::proof]
    fn proof_transfer_chunk_rebase_and_errno_classification() {
        let orig_start: u32 = kani::any();
        let block_count: u8 = kani::any();
        let flags: u8 = kani::any();
        let compressed_len: u16 = kani::any();
        let new_start: u32 = kani::any();

        let entry = crate::inode::ChunkEntry::new(orig_start, block_count, flags, compressed_len);
        let rebased = rebase_chunk_entry(entry, new_start);
        if entry.is_empty() {
            assert!(rebased.is_empty());
            assert_eq!(rebased, crate::inode::ChunkEntry::EMPTY);
        } else {
            assert!(!rebased.is_empty());
            assert_eq!(rebased.start_block(), new_start);
            assert_eq!(rebased.block_count(), block_count);
            assert_eq!(rebased.flags(), flags);
            assert_eq!(rebased.compressed_len(), compressed_len);
        }

        assert_eq!(
            classify_reflink_errno(libc::EOPNOTSUPP),
            ReflinkFallback::CacheUnsupportedDevicePair
        );
        assert_eq!(
            classify_reflink_errno(libc::ENOTTY),
            ReflinkFallback::CacheUnsupportedDevicePair
        );
        assert_eq!(
            classify_reflink_errno(libc::EXDEV),
            ReflinkFallback::CacheUnsupportedDevicePair
        );
        assert_eq!(
            classify_reflink_errno(libc::ENOSYS),
            ReflinkFallback::CacheUnsupportedDevicePair
        );
        assert_eq!(
            classify_reflink_errno(libc::EINVAL),
            ReflinkFallback::FallbackThisExtentOnly
        );
        assert_eq!(
            classify_reflink_errno(libc::EPERM),
            ReflinkFallback::FallbackThisExtentOnly
        );
        assert_eq!(
            classify_reflink_errno(libc::ETXTBSY),
            ReflinkFallback::FallbackThisExtentOnly
        );
        assert_eq!(classify_reflink_errno(libc::ENOSPC), ReflinkFallback::Fatal);
        assert_eq!(classify_reflink_errno(libc::EIO), ReflinkFallback::Fatal);
        assert_eq!(classify_reflink_errno(libc::EBADF), ReflinkFallback::Fatal);

        let errno: i32 = kani::any();
        let cls = classify_reflink_errno(errno);
        if errno == libc::EINVAL || errno == libc::EPERM || errno == libc::ETXTBSY {
            assert_eq!(cls, ReflinkFallback::FallbackThisExtentOnly);
        }
    }

    /// Proves that `coalesce_logical_runs` skips all sparse holes (`phys_blk == 0`),
    /// conserves the exact count and ordered `(logical_idx, phys_blk)` bijection of
    /// non-zero blocks, and produces strictly maximal runs.
    #[kani::proof]
    #[kani::unwind(6)]
    fn proof_transfer_extent_coalescing_conservation() {
        let pairs: [(usize, u64); 4] = [
            (kani::any::<u8>() as usize, kani::any::<u8>() as u64),
            (kani::any::<u8>() as usize, kani::any::<u8>() as u64),
            (kani::any::<u8>() as usize, kani::any::<u8>() as u64),
            (kani::any::<u8>() as usize, kani::any::<u8>() as u64),
        ];
        for &(l, p) in &pairs {
            kani::assume(l < 16);
            kani::assume(p < 16);
        }

        let mut runs_buf = [LogicalRun {
            logical_start: 0,
            phys_start: 0,
            count: 0,
        }; 4];
        let run_len = coalesce_logical_runs_into(&pairs, &mut runs_buf);
        let runs = &runs_buf[..run_len];

        let mut total_blocks: u64 = 0;
        for run in runs {
            assert!(run.phys_start != 0);
            assert!(run.count >= 1);
            total_blocks += run.count;
        }

        let mut expected_non_zero: usize = 0;
        let mut expected_pairs = [(0usize, 0u64); 4];
        for &(l, p) in &pairs {
            if p != 0 {
                expected_pairs[expected_non_zero] = (l, p);
                expected_non_zero += 1;
            }
        }
        assert_eq!(total_blocks, expected_non_zero as u64);

        let mut cursor = 0usize;
        for run in runs {
            let mut k = 0u64;
            while k < run.count {
                assert_eq!(
                    expected_pairs[cursor],
                    (run.logical_start + k as usize, run.phys_start + k)
                );
                cursor += 1;
                k += 1;
            }
        }
        assert_eq!(cursor, expected_non_zero);

        let mut i = 0usize;
        while i + 1 < runs.len() {
            let cur = runs[i];
            let next = runs[i + 1];
            let contiguous_logical = cur.logical_start + cur.count as usize == next.logical_start;
            let contiguous_phys = cur.phys_start + cur.count == next.phys_start;
            assert!(!(contiguous_logical && contiguous_phys));
            i += 1;
        }
    }

    /// Proves Sandwich (`Head / Body / Tail`) alignment partition conservation
    /// (`head + body + tail == len`), exact contiguity, alignment on both `src` and `dst`,
    /// and absence of `u64` overflow/overlap across all offsets and lengths.
    #[kani::proof]
    fn proof_splice_alignment_partition() {
        let src_off: u64 = kani::any();
        let dst_off: u64 = kani::any();
        let len: u64 = kani::any();
        let unit_tag: u8 = kani::any();
        kani::assume(unit_tag < 3);
        let unit_size = match unit_tag {
            0 => 0u64,
            1 => BLOCK_SIZE as u64,
            _ => crate::inode::CHUNK_SIZE_64K as u64,
        };

        match plan_splice_partition(src_off, dst_off, len, unit_size) {
            None => {
                assert!(src_off.checked_add(len).is_none() || dst_off.checked_add(len).is_none());
            }
            Some(p) => {
                let src_end = src_off.checked_add(len).expect("src no overflow");
                let dst_end = dst_off.checked_add(len).expect("dst no overflow");

                assert_eq!(p.head_len + p.body_len + p.tail_len, len);
                assert_eq!(p.head_src_off, src_off);
                assert_eq!(p.head_dst_off, dst_off);
                assert_eq!(p.body_src_off, p.head_src_off + p.head_len);
                assert_eq!(p.body_dst_off, p.head_dst_off + p.head_len);
                assert_eq!(p.tail_src_off, p.body_src_off + p.body_len);
                assert_eq!(p.tail_dst_off, p.body_dst_off + p.body_len);
                assert_eq!(p.tail_src_off + p.tail_len, src_end);
                assert_eq!(p.tail_dst_off + p.tail_len, dst_end);

                if p.body_len > 0 {
                    assert!(unit_size > 0);
                    assert_eq!(src_off % unit_size, dst_off % unit_size);
                    assert_eq!(p.body_src_off % unit_size, 0);
                    assert_eq!(p.body_dst_off % unit_size, 0);
                    assert_eq!(p.body_len % unit_size, 0);
                    assert!(p.head_len < unit_size);
                    assert!(p.tail_len < unit_size);
                }
                if unit_size == 0 || len == 0 || src_off % unit_size != dst_off % unit_size {
                    assert_eq!(p.body_len, 0);
                }
            }
        }
    }

    /// Proves `shift_table_collapse` is a strict bijection between original entries and
    /// `(remaining_table, freed_entries)` with zero leaked blocks and zero cross-linked
    /// (duplicate) block pointers.
    #[kani::proof]
    #[kani::unwind(6)]
    fn proof_chunk_table_shift_bijectivity() {
        let len: usize = kani::any();
        kani::assume(len <= 4);
        let start_unit: usize = kani::any();
        kani::assume(start_unit <= 5);
        let remove_units: usize = kani::any();
        kani::assume(remove_units <= 5);

        let mut orig = [0u64; 4];
        let mut table_buf = [0u64; 4];
        for i in 0..len {
            let v = kani::any::<u8>() as u64;
            kani::assume(v < 16);
            orig[i] = v;
            table_buf[i] = v;
        }
        // Assume all non-zero entries in the source table are pairwise distinct
        // (no pre-existing cross-links).
        for i in 0..len {
            if orig[i] != 0 {
                for j in (i + 1)..len {
                    if orig[j] != 0 {
                        kani::assume(orig[i] != orig[j]);
                    }
                }
            }
        }

        let mut freed_buf = [0u64; 4];
        let (new_len, freed_len) = shift_table_collapse_slice(
            &mut table_buf[..len],
            start_unit,
            remove_units,
            &mut freed_buf,
        );
        let table = &table_buf[..new_len];
        let freed = &freed_buf[..freed_len];

        let actual_removed = if start_unit < len {
            (len - start_unit).min(remove_units)
        } else {
            0
        };
        assert_eq!(table.len(), len - actual_removed);

        // Prefix preserved verbatim.
        let prefix = start_unit.min(len);
        for i in 0..prefix {
            assert_eq!(table[i], orig[i]);
        }
        // Suffix shifted left bijectively.
        for i in (start_unit.saturating_add(actual_removed))..len {
            assert_eq!(table[i - actual_removed], orig[i]);
        }

        // Exact non-zero conservation (zero leaks).
        let mut orig_nz = 0usize;
        for i in 0..len {
            if orig[i] != 0 {
                orig_nz += 1;
            }
        }
        let mut rem_nz = 0usize;
        for &v in table {
            if v != 0 {
                rem_nz += 1;
            }
        }
        assert_eq!(rem_nz + freed.len(), orig_nz);

        // Zero cross-linking: no freed entry remains in `table`, and no duplicates exist.
        for &f in freed {
            assert!(f != 0);
            for &r in table {
                assert!(r != f);
            }
        }
        for i in 0..table.len() {
            if table[i] != 0 {
                for j in (i + 1)..table.len() {
                    assert!(table[i] != table[j]);
                }
            }
        }
    }

    /// Proves that punching a slot (`punch_table_slot`) unconditionally yields zeroed
    /// read semantics for both raw blocks and Seekable 64K chunks, never leaks non-zero
    /// entries, and never double-frees holes. Also verifies `can_splice_fast_path` gate
    /// soundness against encryption, stream compression, and filter mismatches.
    #[kani::proof]
    fn proof_punch_hole_zeroed_reuse_equivalence() {
        let raw_slot: u64 = kani::any();
        let is_seekable: bool = kani::any();

        let mut slot = raw_slot;
        let freed = punch_table_slot(&mut slot);
        assert_eq!(slot, 0);
        assert!(slot_reads_as_zero(slot, is_seekable));
        if is_seekable {
            let e = crate::inode::ChunkEntry(slot);
            assert!(e.is_empty());
            assert_eq!(e.start_block(), 0);
            assert_eq!(e.block_count(), 0);
            assert_eq!(e.compressed_len(), 0);
        }
        if raw_slot != 0 {
            assert_eq!(freed, Some(raw_slot));
        } else {
            assert_eq!(freed, None);
        }

        // Verify `can_splice_fast_path` gate rejects encryption, stream compression, and
        // Seekable 64K filter mismatches.
        let mut src = Inode::new(crate::inode::FileType::File);
        let mut dst = Inode::new(crate::inode::FileType::File);
        let src_enc: bool = kani::any();
        let dst_enc: bool = kani::any();
        src.encrypted = kani::any();
        dst.encrypted = kani::any();
        let src_seek: bool = kani::any();
        let dst_seek: bool = kani::any();
        src.flags = if src_seek {
            crate::inode::INODE_FLAG_SEEKABLE_64K
        } else {
            0
        };
        dst.flags = if dst_seek {
            crate::inode::INODE_FLAG_SEEKABLE_64K
        } else {
            0
        };
        src.compressed_size = kani::any();
        dst.compressed_size = kani::any();
        src.filter_typesize = kani::any();
        dst.filter_typesize = kani::any();
        src.filter_delta = kani::any();
        dst.filter_delta = kani::any();
        src.filter_shuffle = kani::any();
        dst.filter_shuffle = kani::any();
        src.filter_bitshuffle = kani::any();
        dst.filter_bitshuffle = kani::any();

        let unit = can_splice_fast_path(&src, &dst, src_enc, dst_enc);
        let src_has_filters = src.filter_delta || src.filter_shuffle || src.filter_bitshuffle;
        let dst_has_filters = dst.filter_delta || dst.filter_shuffle || dst.filter_bitshuffle;
        if src_enc || dst_enc || src.encrypted || dst.encrypted {
            assert_eq!(unit, None);
        } else if src_seek && dst_seek {
            let same_filters = src.filter_typesize == dst.filter_typesize
                && src.filter_delta == dst.filter_delta
                && src.filter_shuffle == dst.filter_shuffle
                && src.filter_bitshuffle == dst.filter_bitshuffle;
            if same_filters {
                assert_eq!(unit, Some(crate::inode::CHUNK_SIZE_64K as u64));
            } else {
                assert_eq!(unit, None);
            }
        } else if !src_seek
            && !dst_seek
            && src.compressed_size == 0
            && dst.compressed_size == 0
            && !src_has_filters
            && !dst_has_filters
        {
            assert_eq!(unit, Some(BLOCK_SIZE as u64));
        } else {
            assert_eq!(unit, None);
        }
    }
}

#[cfg(test)]
mod bugfix_repro_tests {
    use super::*;

    #[test]
    fn repro_bug5_transfer_stage1_to_stage2_concurrent_source_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.img");
        let dst_path = dir.path().join("dst.img");

        // Sub-case 1: Concurrent delete + block reuse between Stage 1 and Stage 2.
        {
            let src = DiskManager::open_journaled(&src_path, 4 * 1024 * 1024).unwrap();
            let dst = DiskManager::open_journaled(&dst_path, 4 * 1024 * 1024).unwrap();
            src.create_file_with_data(0, "a.bin", &vec![0xAA; 32 * 1024], CompressionMode::Never)
                .unwrap();
            src.flush().unwrap();

            let src_clone = src.clone();
            TRANSFER_STAGE1_HOOK.with(|cell| {
                *cell.borrow_mut() = Some(Box::new(move || {
                    src_clone.delete_file(0, "a.bin").unwrap();
                    src_clone
                        .create_file_with_data(
                            0,
                            "secret.bin",
                            &vec![0xCC; 32 * 1024],
                            CompressionMode::Never,
                        )
                        .unwrap();
                    src_clone.flush().unwrap();
                }));
            });

            let res =
                src.transfer_file_to(0, "a.bin", &dst, 0, "out.bin", TransferOptions::default());
            assert!(
                res.is_err(),
                "Transfer of concurrently deleted source file must fail instead of copying reused secret blocks"
            );
            assert!(
                dst.lookup(0, "out.bin").is_err(),
                "out.bin must not exist in dst after concurrent source delete"
            );
        }

        // Sub-case 2: Concurrent overwrite + freed-block reuse between Stage 1 and Stage 2.
        let src2_path = dir.path().join("src2.img");
        let dst2_path = dir.path().join("dst2.img");
        {
            let src = DiskManager::open_journaled(&src2_path, 4 * 1024 * 1024).unwrap();
            let dst = DiskManager::open_journaled(&dst2_path, 4 * 1024 * 1024).unwrap();
            let a_ino = src
                .create_file_with_data(0, "a.bin", &vec![0xAA; 32 * 1024], CompressionMode::Never)
                .unwrap();
            src.flush().unwrap();

            let src_clone = src.clone();
            TRANSFER_STAGE1_HOOK.with(|cell| {
                *cell.borrow_mut() = Some(Box::new(move || {
                    src_clone
                        .write_data(a_ino, 0, &vec![0xBB; 32 * 1024], CompressionMode::Never)
                        .unwrap();
                    src_clone
                        .create_file_with_data(
                            0,
                            "secret.bin",
                            &vec![0xCC; 32 * 1024],
                            CompressionMode::Never,
                        )
                        .unwrap();
                    src_clone.flush().unwrap();
                }));
            });

            let stats = src
                .transfer_file_to(0, "a.bin", &dst, 0, "out.bin", TransferOptions::default())
                .unwrap();
            let got = dst.read_data(stats.dst_inode).unwrap();
            assert_eq!(
                got,
                vec![0xBB; 32 * 1024],
                "Transferred data must match updated 0xBB content, never stale/reused 0xCC secret blocks"
            );
            assert!(dst.verify_integrity().unwrap().is_clean);
        }
    }

    #[test]
    fn repro_bug6_splice_range_dst_toctou_layout_and_delete() {
        let dir = tempfile::tempdir().unwrap();

        // Sub-case 1: dst converts from Raw -> Stream-compressed between Stage 1 and Stage 2.
        {
            let src_path = dir.path().join("splice_src1.img");
            let dst_path = dir.path().join("splice_dst1.img");
            let src = DiskManager::open_journaled(&src_path, 4 * 1024 * 1024).unwrap();
            let dst = DiskManager::open_journaled(&dst_path, 4 * 1024 * 1024).unwrap();

            let src_ino = src
                .create_file_with_data(0, "src.bin", &vec![0xAA; 32 * 1024], CompressionMode::Never)
                .unwrap();
            let dst_ino = dst
                .create_file_with_data(0, "dst.bin", &vec![0x11; 32 * 1024], CompressionMode::Never)
                .unwrap();

            let dst_clone = dst.clone();
            SPLICE_BEFORE_DST_COMMIT_HOOK.with(|cell| {
                *cell.borrow_mut() = Some(Box::new(move || {
                    dst_clone
                        .write_data(dst_ino, 0, &vec![b'Z'; 32 * 1024], CompressionMode::Always)
                        .unwrap();
                }));
            });

            src.splice_range_to(
                src_ino,
                0,
                &dst,
                dst_ino,
                0,
                16 * 1024,
                SpliceOptions::default(),
            )
            .unwrap();
            let mut expected = vec![0xAAu8; 16 * 1024];
            expected.extend_from_slice(&vec![b'Z'; 16 * 1024]);
            assert_eq!(dst.read_data(dst_ino).unwrap(), expected);
            assert!(dst.verify_integrity().unwrap().is_clean);
        }

        // Sub-case 2: dst inode deleted before Stage 2 write lock.
        {
            let src_path = dir.path().join("splice_src2.img");
            let dst_path = dir.path().join("splice_dst2.img");
            let src = DiskManager::open_journaled(&src_path, 4 * 1024 * 1024).unwrap();
            let dst = DiskManager::open_journaled(&dst_path, 4 * 1024 * 1024).unwrap();

            let src_ino = src
                .create_file_with_data(0, "src.bin", &vec![0xAA; 32 * 1024], CompressionMode::Never)
                .unwrap();
            let dst_ino = dst
                .create_file_with_data(0, "dst.bin", &vec![0x11; 32 * 1024], CompressionMode::Never)
                .unwrap();

            let dst_clone = dst.clone();
            SPLICE_BEFORE_DST_COMMIT_HOOK.with(|cell| {
                *cell.borrow_mut() = Some(Box::new(move || {
                    dst_clone.delete_file(0, "dst.bin").unwrap();
                }));
            });

            let res = src.splice_range_to(
                src_ino,
                0,
                &dst,
                dst_ino,
                0,
                16 * 1024,
                SpliceOptions::default(),
            );
            assert!(
                res.is_err(),
                "splice_range_to must reject concurrently deleted dst inode"
            );
            let report = dst.verify_integrity().unwrap();
            assert!(
                report.is_clean,
                "dst fsck must remain clean after rejected splice: {report:?}"
            );
        }
    }

    #[test]
    fn repro_bug6_collapse_range_src_toctou_concurrent_append() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("collapse_src.img");
        let dst_path = dir.path().join("collapse_dst.img");
        let src = DiskManager::open_journaled(&src_path, 4 * 1024 * 1024).unwrap();
        let dst = DiskManager::open_journaled(&dst_path, 4 * 1024 * 1024).unwrap();

        let mut initial = Vec::with_capacity(16 * 1024);
        for pat in [0x11u8, 0x22, 0x33, 0x44] {
            initial.extend_from_slice(&[pat; 4096]);
        }
        let src_ino = src
            .create_file_with_data(0, "src.bin", &initial, CompressionMode::Never)
            .unwrap();
        let dst_ino = dst
            .create_file_with_data(0, "dst.bin", &[], CompressionMode::Never)
            .unwrap();

        let src_clone = src.clone();
        COLLAPSE_BEFORE_WRITE_HOOK.with(|cell| {
            *cell.borrow_mut() = Some(Box::new(move || {
                let mut appended = vec![0x55u8; 4096];
                appended.extend_from_slice(&[0x66u8; 4096]);
                src_clone
                    .write_data(src_ino, 16 * 1024, &appended, CompressionMode::Never)
                    .unwrap();
            }));
        });

        src.splice_range_to(
            src_ino,
            0,
            &dst,
            dst_ino,
            0,
            4096,
            SpliceOptions {
                trim_mode: SpliceTrimMode::CollapseRange,
            },
        )
        .unwrap();

        let mut expected = Vec::with_capacity(20 * 1024);
        for pat in [0x22u8, 0x33, 0x44, 0x55, 0x66] {
            expected.extend_from_slice(&[pat; 4096]);
        }
        assert_eq!(src.read_data(src_ino).unwrap(), expected);
        let report = src.verify_integrity().unwrap();
        assert!(
            report.is_clean,
            "fsck after concurrent append + CollapseRange must be clean: {report:?}"
        );
    }
}
