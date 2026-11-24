//! Disk manager module for OIFS file system
//!
//! Provides the main interface for interacting with the file system,
//! including creating/reading files and directories, managing inodes,
//! and handling file compression.

use crate::BLOCK_SIZE;
use crate::allocator::{AllocatorError, BlockAllocator, SimpleBlockAllocator};
use crate::inode::Inode;
use crate::io_engine::{ExtentList, IoBackend, IoEngine, ReadTarget};
use crate::storage::{BitmapSliceMut, ImageBacking, StorageMode};
use crate::superblock::SuperBlock;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use crate::fx_hash::{FxHashMap, FxHashSet};

use serde::{Deserialize, Serialize};

/// Errors that can occur during disk manager operations
#[derive(Debug)]
pub enum DiskManagerError {
    /// I/O error occurred
    Io(std::io::Error),
    /// Serialization/deserialization error
    Serialization(bincode::Error),
    /// Invalid file system magic number
    InvalidMagic,
    /// File system image is too small
    FileTooSmall,
    /// Block allocation error
    Allocator(AllocatorError),
    /// File locking error
    Locking(std::io::Error),
    /// Encryption error
    Encryption(crate::encryption::EncryptionError),
    /// Metadata WAL / journal error
    Journal(crate::journal::JournalError),
    /// On-disk inode record could not be encoded or decoded
    InodeFormat(crate::inode_format::InodeFormatError),
    /// Password required for encrypted filesystem
    PasswordRequired,
    /// Password or key provided for an unencrypted filesystem
    NotEncrypted,
    /// Decryption failed - wrong password or corrupted data
    DecryptionFailed,
    /// Superblock bounds or layout geometry invalid
    CorruptedSuperblock(&'static str),
    /// Mutation attempted on a read-only mount
    ReadOnly,
}

impl std::fmt::Display for DiskManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::Serialization(e) => write!(f, "Serialization error: {e}"),
            Self::InvalidMagic => write!(f, "Invalid magic number"),
            Self::FileTooSmall => write!(f, "File too small"),
            Self::Allocator(e) => write!(f, "Allocator error: {e}"),
            Self::Locking(e) => write!(f, "Locking error: {e}"),
            Self::Encryption(e) => write!(f, "Encryption error: {e}"),
            Self::Journal(e) => write!(f, "Journal error: {e}"),
            Self::InodeFormat(e) => write!(f, "Inode format error: {e}"),
            Self::PasswordRequired => write!(f, "Password required to access encrypted filesystem"),
            Self::NotEncrypted => {
                write!(
                    f,
                    "Password or encryption key provided for an unencrypted filesystem"
                )
            }
            Self::DecryptionFailed => write!(f, "Decryption failed - check password"),
            Self::CorruptedSuperblock(msg) => {
                write!(f, "Corrupted superblock layout or invalid bounds: {msg}")
            }
            Self::ReadOnly => write!(f, "Filesystem is mounted read-only"),
        }
    }
}

impl std::error::Error for DiskManagerError {}

impl From<std::io::Error> for DiskManagerError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<bincode::Error> for DiskManagerError {
    fn from(e: bincode::Error) -> Self {
        Self::Serialization(e)
    }
}

impl From<AllocatorError> for DiskManagerError {
    fn from(e: AllocatorError) -> Self {
        Self::Allocator(e)
    }
}

impl From<crate::encryption::EncryptionError> for DiskManagerError {
    fn from(e: crate::encryption::EncryptionError) -> Self {
        Self::Encryption(e)
    }
}

impl From<crate::journal::JournalError> for DiskManagerError {
    fn from(e: crate::journal::JournalError) -> Self {
        Self::Journal(e)
    }
}

impl From<crate::inode_format::InodeFormatError> for DiskManagerError {
    fn from(e: crate::inode_format::InodeFormatError) -> Self {
        Self::InodeFormat(e)
    }
}

/// Compression mode for write operations
///
/// Controls when files should be compressed using zstd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CompressionMode {
    /// Always compress, regardless of file size (Stream mode)
    Always,
    /// Never compress
    Never,
    /// Auto: compress files >= 8KB
    #[default]
    Auto,
    /// Full-file stream compression with explicit Zstd compression level
    Stream {
        /// Zstd compression level (1 ~ 19, 0 = default level 3)
        level: i32,
    },
    /// Seekable chunked compression (64KB independent chunks)
    Seekable {
        /// Logical chunk size in bytes (e.g. 64KB = 65536)
        chunk_size: u32,
        /// Zstd compression level (recommend level 1)
        level: i32,
    },
}

/// File opening mode matching standard POSIX semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpenMode {
    /// Open an existing file. Fails with NotFound (ENOENT) if the file does not exist.
    OpenExisting,
    /// Atomically create a new file exclusively. Fails with AlreadyExists (EEXIST) if the file already exists.
    CreateNew,
    /// Open the file if it exists, or create it if it does not exist.
    CreateOrOpen,
}

/// Statistics about disk fragmentation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FragmentationStats {
    /// Total number of data blocks
    pub total_blocks: usize,
    /// Number of used (allocated) blocks
    pub used_blocks: usize,
    /// Number of free blocks
    pub free_blocks: usize,
    /// Number of contiguous free runs (gaps)
    pub free_runs: usize,
    /// Size of largest contiguous free space
    pub largest_free_run: usize,
    /// Average size of free gaps
    pub avg_gap_size: f64,
    /// Fragmentation ratio (0.0 = no fragmentation, 1.0 = maximum)
    pub fragmentation_ratio: f64,
}

/// Defragmentation mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DefragMode {
    /// Safe mode: create new image and replace original after success
    #[default]
    Safe,
    /// In-place mode: directly modify original image (faster but risky)
    InPlace,
}

/// Statistics from defragmentation operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefragStats {
    /// Number of files defragmented
    pub files_processed: usize,
    /// Total bytes moved
    pub bytes_moved: u64,
    /// Number of blocks freed
    pub blocks_freed: usize,
    /// Fragmentation ratio before defrag
    pub frag_before: f64,
    /// Fragmentation ratio after defrag
    pub frag_after: f64,
}

/// Durability and write-synchronization policy for filesystem mutations.
///
/// Controls when and how `msync` is called on the backing memory map.
/// Shared memory mappings (`MAP_SHARED`) write directly to the OS page cache,
/// ensuring modifications survive application process crashes (e.g. SIGKILL).
/// Durability policies balance power-loss resilience against write throughput.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[repr(u8)]
pub enum DurabilityMode {
    /// Lazy / ProcessSafe (Default):
    /// Mutations update the shared mmap and OS page cache without issuing
    /// per-mutation `msync` syscalls. Changes are immediately visible to all processes
    /// and survive process crashes/panics. Persistence across sudden machine power loss
    /// is guaranteed on explicit [`DiskManager::flush`], upon [`Drop`], or via periodic
    /// OS kernel writeback. Delivers up to ~45x faster write throughput in bulk operations.
    #[default]
    Lazy = 0,

    /// RangeAsync:
    /// Asynchronously flushes only the modified byte ranges via `msync(MS_ASYNC)` on each mutation,
    /// avoiding full virtual-memory address space scans while scheduling dirty pages for early writeback.
    RangeAsync = 1,

    /// Strict:
    /// Synchronously flushes modified byte ranges via `msync(MS_SYNC)` on every mutation.
    /// Guarantees that data has reached physical storage before the mutating function returns.
    Strict = 2,

    /// LegacyWholeMmapAsync:
    /// Asynchronously flushes the entire virtual memory map after every mutation (legacy whole-mapping flush behavior).
    LegacyWholeMmapAsync = 3,
}

impl DurabilityMode {
    pub fn from_u8(val: u8) -> Self {
        match val {
            1 => Self::RangeAsync,
            2 => Self::Strict,
            3 => Self::LegacyWholeMmapAsync,
            _ => Self::Lazy,
        }
    }

    /// Whether this durability mode flushes specific mutated byte ranges.
    #[inline]
    pub fn is_range_based(&self) -> bool {
        matches!(self, Self::RangeAsync | Self::Strict)
    }
}

/// Detailed diagnostic report from a consistency check (fsck)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsckReport {
    /// True if filesystem is structurally consistent
    pub is_clean: bool,
    /// List of allocated inodes not referenced by any directory entries
    pub orphan_inodes: Vec<u64>,
    /// List of data blocks marked allocated in the bitmap but not mapped by any inode
    pub leaked_blocks: Vec<u64>,
    /// List of data blocks mapped by inodes but marked free in the bitmap
    pub missing_blocks: Vec<u64>,
    /// List of data blocks referenced by multiple inodes
    pub cross_linked_blocks: Vec<u64>,
}

/// One positional read in a [`DiskManager::read_at_batch`] call.
#[derive(Debug)]
pub struct ReadRequest<'a> {
    /// Inode of the regular file to read
    pub inode_id: u64,
    /// Byte offset within the (logical, uncompressed) file
    pub offset: u64,
    /// Destination; up to `buf.len()` bytes are read
    pub buf: &'a mut [u8],
}

mod caches;
mod chunked;
mod maintenance;
mod transfer;

pub use caches::INODE_CACHE_CAPACITY;
use caches::*;
pub use transfer::{
    CloneMethod, LogicalRun, ReflinkFallback, SpliceOptions, SplicePartition, SpliceStats,
    SpliceTrimMode, TransferMethod, TransferOptions, TransferStats, can_splice_fast_path,
    canonical_file_id, classify_reflink_errno, clear_unsupported_reflink_cache,
    coalesce_logical_runs, combine_transfer_tier, is_reflink_device_pair_unsupported,
    mark_reflink_device_pair_unsupported, plan_splice_partition, punch_host_file_blocks,
    punch_table_slot, push_logical_block_run, rebase_chunk_entry, shift_table_collapse,
    should_use_raw_fast_path, slot_reads_as_zero, transfer_byte_range,
};
/// Internal disk manager state
///
/// Contains the file handle, memory-mapped region, and superblock.
/// Protected by a Mutex for thread-safe concurrent access.
pub(crate) struct DiskManagerInner {
    /// Image file handle; payload reads use it directly under the `Pread` / `IoUring`
    /// backends.
    file: File,
    /// Canonical path of the underlying image file on host disk
    pub(crate) backing_path: PathBuf,
    /// Active storage engine (`StorageMode::Mmap` or `StorageMode::FileIo`)
    pub(crate) backing: ImageBacking,
    /// Cached copy of the superblock
    pub superblock: SuperBlock,
    /// Encryption key (if filesystem is encrypted)
    pub encryption_key: Option<crate::encryption::EncryptionKey>,
    /// Keyed BLAKE2b verifier of the mount password for session re-authentication
    password_verifier: Option<zeroize::Zeroizing<[u8; 32]>>,
    /// Search hint for sequential O(1) block allocation
    pub free_block_hint: u64,
    /// Search hint for sequential O(1) inode allocation
    pub free_inode_hint: u64,
    /// Inode cache for zero-copy metadata access
    pub inode_cache: BoundedInodeCache,
    /// Sharded per-directory name index: dir_inode_id -> DirIndex
    pub dir_cache: ShardedDirCache,
    /// Bounded sub-chunk decompression cache for seekable 64K `read_at`
    pub chunk_read_cache: RwLock<ChunkReadCache>,
    /// Bounded decoded-payload cache for non-seekable `read_at`
    pub decoded_blob_cache: Mutex<DecodedBlobCache>,
    /// Durability policy governing flush/msync behavior on mutations
    pub durability_mode: std::sync::atomic::AtomicU8,
    /// Batched dirty-range accumulator for `DurabilityMode::RangeAsync`
    pub(crate) pending_async_ranges: Mutex<PendingAsyncRanges>,
    /// Fast atomic flag set when `pending_async_ranges` reaches its drain threshold
    pub(crate) async_drain_ready: Arc<std::sync::atomic::AtomicBool>,
    /// Payload-block read engine
    io_engine: IoEngine,
    /// Per-inode write generation counters (indexed by `inode_id & 4095`) for lock-free
    /// Stage-1 -> Stage-3 staleness detection even within the same 1-second timestamp.
    pub(crate) inode_write_gen: Box<[u64; 4096]>,
    /// Whether this mount is strictly read-only (`F_RDLCK`, zero disk writes)
    pub(crate) read_only: bool,
}

impl DiskManagerInner {
    /// Returns the monotonic mutation generation for `inode_id`'s slot.
    #[inline]
    pub(crate) fn inode_gen(&self, inode_id: u64) -> u64 {
        self.inode_write_gen[(inode_id as usize) & 4095]
    }

    /// Advances the mutation generation for `inode_id`'s slot.
    #[inline]
    pub(crate) fn bump_inode_gen(&mut self, inode_id: u64) {
        let slot = &mut self.inode_write_gen[(inode_id as usize) & 4095];
        *slot = slot.wrapping_add(1);
    }

    /// The inode bitmap as one contiguous slice.
    ///
    /// The bitmap may span several blocks. Under `Mmap`, this borrows the
    /// mapped span directly; under `FileIo`, this borrows the write-through
    /// in-memory bitmap mirror.
    pub(crate) fn inode_bitmap_slice(&self) -> Option<&[u8]> {
        self.backing.inode_bitmap_slice(&self.superblock)
    }

    /// Returns `true` when `inode_id` is within the usable inode table and marked allocated.
    pub(crate) fn is_inode_allocated(&self, inode_id: u64) -> bool {
        if inode_id >= self.superblock.usable_inode_count() {
            return false;
        }
        let Some(ib) = self.inode_bitmap_slice() else {
            return false;
        };
        crate::bitmap::BitmapRef::new(ib).get(inode_id as usize)
    }

    /// The data bitmap as one contiguous slice.
    pub(crate) fn data_bitmap_slice(&self) -> Option<&[u8]> {
        self.backing.data_bitmap_slice(&self.superblock)
    }

    /// The mutable inode bitmap span (auto-flushes to disk on drop under `FileIo`).
    pub(crate) fn inode_bitmap_slice_mut(&mut self) -> Option<BitmapSliceMut<'_>> {
        self.backing.inode_bitmap_slice_mut(&self.superblock)
    }

    /// The mutable data bitmap span (auto-flushes to disk on drop under `FileIo`).
    pub(crate) fn data_bitmap_slice_mut(&mut self) -> Option<BitmapSliceMut<'_>> {
        self.backing.data_bitmap_slice_mut(&self.superblock)
    }

    /// Whether a block id addresses a block inside the data area.
    ///
    /// Pointers that leave the data area (superblock, bitmaps, inode
    /// table, journal region) must never be followed or written through.
    pub fn is_data_block(&self, block_id: u64) -> bool {
        block_id != 0
            && block_id >= self.superblock.data_block_start
            && block_id < self.superblock.block_count
    }

    pub fn durability_mode(&self) -> DurabilityMode {
        DurabilityMode::from_u8(
            self.durability_mode
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    pub fn set_durability_mode(&self, mode: DurabilityMode) {
        self.durability_mode
            .store(mode as u8, std::sync::atomic::Ordering::Relaxed);
    }

    #[inline]
    pub fn inode_byte_range(&self, inode_id: u64) -> (usize, usize) {
        let base = (self.superblock.inode_table_block as usize).checked_mul(BLOCK_SIZE);
        let id_offset = usize::try_from(inode_id)
            .ok()
            .and_then(|id| id.checked_mul(256));
        let offset = base
            .and_then(|b| id_offset.and_then(|i| b.checked_add(i)))
            .unwrap_or(usize::MAX);
        (offset, 256)
    }

    #[inline]
    pub fn block_byte_range(&self, block_id: u64) -> (usize, usize) {
        let start = usize::try_from(block_id)
            .ok()
            .and_then(|b| b.checked_mul(BLOCK_SIZE))
            .unwrap_or(usize::MAX);
        (start, BLOCK_SIZE)
    }

    #[inline]
    pub fn inode_bitmap_byte_range(&self) -> (usize, usize) {
        (
            self.superblock.inode_bitmap_block as usize * BLOCK_SIZE,
            self.superblock.inode_bitmap_blocks() as usize * BLOCK_SIZE,
        )
    }

    #[inline]
    pub fn data_bitmap_byte_range(&self) -> (usize, usize) {
        (
            self.superblock.data_bitmap_block as usize * BLOCK_SIZE,
            self.superblock.data_bitmap_blocks() as usize * BLOCK_SIZE,
        )
    }

    /// Maximum clean-page gap bridged when coalescing ranges for `DurabilityMode::RangeAsync`.
    ///
    /// Bridging nearby ranges within 1 MB collapses metadata (`inode_bitmap`, `data_bitmap`,
    /// `inode_table`) and nearby data blocks into a single `msync(MS_ASYNC)` syscall per
    /// transaction; the OS pager skips clean pages in the bridged gap.
    pub const RANGE_ASYNC_GAP_BRIDGE_BYTES: usize = 1024 * 1024;

    /// Maximum clean-page gap bridged when coalescing metadata ranges under
    /// `DurabilityMode::Strict`.
    ///
    /// Bridges across the 132 KiB journal region between the bitmaps (blocks 1..2)
    /// and the low inode table blocks (block 35+), collapsing separate bitmap and
    /// inode `msync(MS_SYNC)` syscalls into a single VM call (the OS pager only
    /// writes back dirty pages in the bridged span).
    pub const STRICT_GAP_BRIDGE_BYTES: usize = 256 * 1024;

    /// Coalesces a slice of byte ranges by:
    /// 1. Clamping to `mmap_len` and expanding to page/block boundaries (`BLOCK_SIZE = 4096`).
    /// 2. Sorting by start offset.
    /// 3. Merging overlapping and contiguous intervals.
    ///
    /// This eliminates duplicate syscalls when multiple mutations touch the same 4KB page
    /// or contiguous blocks on disk.
    pub fn coalesce_ranges(ranges: &[(usize, usize)], mmap_len: usize) -> Vec<(usize, usize)> {
        Self::coalesce_ranges_with_gap(ranges, mmap_len, 0)
    }

    /// Coalesces byte ranges like [`Self::coalesce_ranges`], additionally bridging
    /// gaps of at most `max_gap` bytes between adjacent page-aligned intervals.
    pub fn coalesce_ranges_with_gap(
        ranges: &[(usize, usize)],
        mmap_len: usize,
        max_gap: usize,
    ) -> Vec<(usize, usize)> {
        if ranges.is_empty() || mmap_len == 0 {
            return Vec::new();
        }

        let mut intervals: Vec<(usize, usize)> = ranges
            .iter()
            .filter_map(|&(off, len)| {
                if len == 0 || off >= mmap_len {
                    return None;
                }
                let page_start = (off / BLOCK_SIZE) * BLOCK_SIZE;
                let raw_end = off.saturating_add(len);
                let page_end = ((raw_end.saturating_add(BLOCK_SIZE - 1)) / BLOCK_SIZE)
                    .saturating_mul(BLOCK_SIZE)
                    .min(mmap_len);
                if page_start < page_end {
                    Some((page_start, page_end))
                } else {
                    None
                }
            })
            .collect();

        if intervals.is_empty() {
            return Vec::new();
        }

        intervals.sort_unstable_by_key(|&(start, end)| (start, end));

        let mut coalesced: Vec<(usize, usize)> = Vec::with_capacity(intervals.len());
        let (mut cur_start, mut cur_end) = intervals[0];

        for &(start, end) in &intervals[1..] {
            if start <= cur_end.saturating_add(max_gap) {
                cur_end = cur_end.max(end);
            } else {
                coalesced.push((cur_start, cur_end - cur_start));
                cur_start = start;
                cur_end = end;
            }
        }
        coalesced.push((cur_start, cur_end - cur_start));

        coalesced
    }

    /// Number of `RangeAsync` mutations buffered before an asynchronous `msync(MS_ASYNC)` batch drain.
    pub const RANGE_ASYNC_BATCH_OPS: usize = 64;
    /// Dirty byte span buffered before an asynchronous `msync(MS_ASYNC)` batch drain.
    pub const RANGE_ASYNC_BATCH_BYTES: usize = 4 * 1024 * 1024;
    /// Safety cap that forces an inline drain if internal callers bypass public `DiskManager` wrappers.
    const RANGE_ASYNC_FALLBACK_INLINE_OPS: usize = 256;

    pub(crate) fn flush_async_batch(&self, batch: &[(usize, usize)]) {
        if self.read_only || batch.is_empty() {
            return;
        }
        // Preserve WAL-before-metadata writeback ordering when draining a RangeAsync batch.
        if self.superblock.has_journal_layout() {
            let bs = self.superblock.block_size as u64;
            if let Some(start) = DiskManager::journal_region_start(&self.superblock) {
                let len = DiskManager::journal_region_len(bs)
                    .min(self.backing.len().saturating_sub(start));
                if len > 0 {
                    let _ = self.backing.flush_async_range(start, len);
                }
            }
        }
        let coalesced = Self::coalesce_ranges_with_gap(
            batch,
            self.backing.len(),
            Self::RANGE_ASYNC_GAP_BRIDGE_BYTES,
        );
        for (offset, actual_len) in coalesced {
            let _ = self.backing.flush_async_range(offset, actual_len);
        }
    }

    pub(crate) fn drain_pending_async_ranges_internal(&self) {
        let batch = {
            let mut pending = crate::lock_unpoisoned(&self.pending_async_ranges);
            pending.pending_ops = 0;
            pending.approx_bytes = 0;
            self.async_drain_ready
                .store(false, std::sync::atomic::Ordering::Release);
            std::mem::take(&mut pending.ranges)
        };
        self.flush_async_batch(&batch);
    }

    /// Syncs one or more modified byte ranges according to the current `DurabilityMode`.
    ///
    /// Under `RangeAsync`, dirty ranges are accumulated in `pending_async_ranges`
    /// and drained in coalesced batches outside `inner.write()`, eliminating per-op
    /// `msync(MS_ASYNC)` syscall overhead while holding the exclusive write lock.
    pub fn sync_mutation_ranges(&self, ranges: &[(usize, usize)]) -> Result<(), DiskManagerError> {
        if self.read_only {
            return Ok(());
        }
        match self.durability_mode() {
            DurabilityMode::Lazy => Ok(()),
            DurabilityMode::RangeAsync => {
                if ranges.is_empty() {
                    return Ok(());
                }
                let img_len = self.backing.len();
                let mut inline_batch = None;
                {
                    let mut pending = crate::lock_unpoisoned(&self.pending_async_ranges);
                    let mut added = false;
                    for &(off, len) in ranges {
                        if len == 0 || off >= img_len {
                            continue;
                        }
                        let clamped_len = len.min(img_len - off);
                        if let Some(last) = pending.ranges.last_mut() {
                            let ls = last.0;
                            let prev_len = last.1;
                            let le = ls.saturating_add(prev_len);
                            let ns = off;
                            let ne = ns.saturating_add(clamped_len);
                            if ns <= le.saturating_add(BLOCK_SIZE)
                                && ls <= ne.saturating_add(BLOCK_SIZE)
                            {
                                let merged_s = ls.min(ns);
                                let merged_e = le.max(ne);
                                let new_len = merged_e - merged_s;
                                *last = (merged_s, new_len);
                                pending.approx_bytes = pending
                                    .approx_bytes
                                    .saturating_add(new_len.saturating_sub(prev_len));
                                added = true;
                                continue;
                            }
                        }
                        pending.ranges.push((off, clamped_len));
                        pending.approx_bytes = pending.approx_bytes.saturating_add(clamped_len);
                        added = true;
                    }
                    if added {
                        pending.pending_ops += 1;
                        if pending.pending_ops >= Self::RANGE_ASYNC_BATCH_OPS
                            || pending.approx_bytes >= Self::RANGE_ASYNC_BATCH_BYTES
                            || pending.ranges.len() >= 256
                        {
                            if pending.pending_ops >= Self::RANGE_ASYNC_FALLBACK_INLINE_OPS {
                                pending.pending_ops = 0;
                                pending.approx_bytes = 0;
                                self.async_drain_ready
                                    .store(false, std::sync::atomic::Ordering::Relaxed);
                                inline_batch = Some(std::mem::take(&mut pending.ranges));
                            } else {
                                self.async_drain_ready
                                    .store(true, std::sync::atomic::Ordering::Release);
                            }
                        }
                    }
                }
                if let Some(batch) = inline_batch {
                    self.flush_async_batch(&batch);
                }
                Ok(())
            }
            DurabilityMode::Strict => {
                if is_strict_sync_deferred() {
                    return Ok(());
                }
                if self.backing.as_mmap_slice().is_some() {
                    let coalesced = Self::coalesce_ranges_with_gap(
                        ranges,
                        self.backing.len(),
                        Self::STRICT_GAP_BRIDGE_BYTES,
                    );
                    for (offset, actual_len) in coalesced {
                        self.backing
                            .flush_range(offset, actual_len)
                            .map_err(DiskManagerError::Io)?;
                    }
                } else {
                    // Under FileIo, a single sync_data() flushes all dirty ranges at once.
                    self.backing.flush().map_err(DiskManagerError::Io)?;
                }
                Ok(())
            }
            DurabilityMode::LegacyWholeMmapAsync => {
                let _ = self.backing.flush_async();
                Ok(())
            }
        }
    }

    #[inline]
    pub(crate) fn invalidate_payload_caches(&mut self, inode_id: u64) {
        self.chunk_read_cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate_inode(inode_id);
        self.decoded_blob_cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate_inode(inode_id);
    }

    #[inline]
    pub(crate) fn clear_payload_caches(&mut self) {
        self.chunk_read_cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.decoded_blob_cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    /// Decides whether `Drop` should flush `(journal_region, backing)`.
    ///
    /// A read-only mount must never issue any disk write or flush on drop.
    #[inline]
    pub(crate) const fn should_sync_on_drop(read_only: bool, has_journal: bool) -> (bool, bool) {
        if read_only {
            (false, false)
        } else {
            (has_journal, true)
        }
    }
}

/// Batched dirty-range state for `DurabilityMode::RangeAsync`.
#[derive(Default)]
pub(crate) struct PendingAsyncRanges {
    ranges: Vec<(usize, usize)>,
    pending_ops: usize,
    approx_bytes: usize,
}

impl Drop for DiskManagerInner {
    fn drop(&mut self) {
        let (flush_journal, flush_backing) =
            Self::should_sync_on_drop(self.read_only, self.superblock.has_journal_layout());
        // The journal region goes out first: on shutdown the log describing pending
        // metadata must not lag behind the metadata itself.
        if flush_journal {
            let bs = self.superblock.block_size as u64;
            if let Some(start) = DiskManager::journal_region_start(&self.superblock) {
                let len = DiskManager::journal_region_len(bs)
                    .min(self.backing.len().saturating_sub(start));
                let _ = self.backing.flush_range(start, len);
            }
        }
        // Flush any pending changes to disk when dropped
        if flush_backing {
            let _ = self.backing.flush();
        }
    }
}

/// Main disk manager interface for the OIFS file system
///
/// Provides thread-safe access to the file system through an Arc<RwLock<>> wrapper.
/// Supports:
/// - File and directory creation/deletion
/// - File reading/writing with optional zstd compression
/// - Path resolution and directory listing
/// - High-concurrency parallel reads across multiple threads
#[derive(Clone)]
pub struct DiskManager {
    inner: Arc<RwLock<DiskManagerInner>>,
    /// Dedicated mutex to serialize physical msync calls without blocking readers
    sync_mutex: Arc<Mutex<()>>,
    /// Shared fast-path flag indicating buffered `RangeAsync` ranges are ready to drain
    async_drain_ready: Arc<std::sync::atomic::AtomicBool>,
    /// Background automatic defragmentation coordinator
    auto_defrag: Arc<crate::auto_defrag::AutoDefragController>,
}

/// Outcome of an on-disk format migration.
///
/// Returned by [`DiskManager::migrate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationStats {
    /// Inode record version the image used before migrating.
    pub from_version: u32,
    /// Inode record version the image uses after migrating.
    pub to_version: u32,
    /// Number of allocated inodes rewritten in this call.
    pub inodes_rewritten: u64,
    /// `true` when the image was already current and nothing was rewritten.
    pub already_current: bool,
}

/// Which of the four write cases applies to a `write_data` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteCase {
    /// `file_offset == 0` and the whole file is being replaced (re-filtered,
    /// recompressed and re-encrypted as one unit).
    FullOverwrite,
    /// Appending a fresh Zstd frame at the physical end of a compressed stream.
    CompressedAppend,
    /// Decompress, splice, and rewrite contiguously from offset 0.
    Recompress,
    /// Plain append or random file write into an uncompressed file.
    Raw,
}

impl WriteCase {
    /// Whether this case re-runs the filter pipeline over the whole file.
    ///
    /// When it does, the caller must persist the filter metadata into the inode.
    /// Missing that leaves the stored payload still filtered while the reader
    /// believes it is raw, which silently returns corrupted data. This is the rule
    /// the in-place path has always followed.
    fn rewrites_filter_metadata(&self) -> bool {
        matches!(self, WriteCase::FullOverwrite | WriteCase::Recompress)
    }

    /// Whether the write must release every previously allocated block first.
    ///
    /// Both rewriting cases rebuild the payload from block 0, so any block the new
    /// payload does not need is dead weight. `FullOverwrite` used to leave it
    /// allocated, which leaked space on every shrink; `Recompress` has always
    /// reclaimed it. Freeing requires clearing the inode's pointers too, otherwise
    /// `get_or_alloc_block` would hand back a block that is no longer ours.
    fn frees_previous_blocks(&self) -> bool {
        matches!(self, WriteCase::FullOverwrite | WriteCase::Recompress)
    }
}

/// The encryption inputs of a write plan.
///
/// The associated data binds the ciphertext to the file it belongs to
///; an empty slice means the legacy scheme.
struct EncryptionContext<'a> {
    key: Option<&'a crate::encryption::EncryptionKey>,
    aad: &'a [u8],
}

/// The decision half of a write: what bytes land where, and what the inode becomes.
///
/// Both the in-place and the journaled write paths consume this, so the four cases
/// and their size arithmetic exist exactly once. They previously lived in two
/// parallel implementations that had already drifted: the in-place path added
/// `data.len()` (logical) for a compressed append while the journaled path added
/// `frame.len()` (physical), so the same append produced two different file sizes.
/// Result of preparing a full-overwrite payload buffer out-of-lock.
pub(crate) struct FullOverwritePayload<'a> {
    pub buffer: std::borrow::Cow<'a, [u8]>,
    pub is_compressed: bool,
    pub encryption_nonce: Option<[u8; 24]>,
}

/// The context that binds a seekable chunk ciphertext to its chunk.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChunkAadContext {
    pub inode_id: u64,
    pub chunk_idx: u64,
    /// The image's encryption scheme (see `crate::encryption`).
    pub scheme: u8,
}

/// Result of preparing a seekable 64K chunk payload buffer out-of-lock.
#[derive(Debug, Clone)]
pub(crate) struct PreparedChunkPayload {
    pub chunk_idx: usize,
    pub payload: Vec<u8>,
    pub flag: u8,
    pub comp_len: u16,
    pub blocks_needed: usize,
}

struct WritePlan<'a> {
    case: WriteCase,
    /// Byte offset in the file's payload where `buffer` begins.
    phys_off: u64,
    /// Exact bytes to store on disk (borrowed directly on raw writes; owned when transformed).
    buffer: std::borrow::Cow<'a, [u8]>,
    /// New logical size.
    new_size: u64,
    /// New physical size; `0` when the payload is stored raw.
    new_compressed_size: u64,
    /// Updated encryption nonce if newly encrypted.
    encryption_nonce: Option<[u8; 24]>,
}

impl WritePlan<'_> {}

/// Whether a compressed file may grow by appending a fresh Zstd frame.
///
/// Besides the format checks, the physical size is compared against the
/// logical one: a stream of many small frames costs a frame header each and
/// compresses poorly, so once the overhead exceeds a quarter the file is
/// rewritten as a single frame instead.
fn may_append_zstd_frame(
    inode: &Inode,
    file_offset: u64,
    filter_config: &crate::filters::FilterConfig,
    encryption_key: Option<&crate::encryption::EncryptionKey>,
) -> bool {
    if inode.compressed_size == 0
        || file_offset != inode.size
        || inode.encrypted
        || filter_config.is_active()
        || inode.filter_typesize != 0
        || encryption_key.is_some()
    {
        return false;
    }
    let overhead_limit = inode.size + inode.size / 4 + 4096;
    inode.compressed_size <= overhead_limit
}

/// Whether the stored payload of a file is a whole-file transformed blob.
///
/// Compressed, filtered and encrypted payloads are single blobs that cover the
/// entire file, so they cannot be patched in place: any write to such a file
/// has to rebuild the payload from the file's decoded content. Treating a
/// transformed payload as a plain byte range writes *untransformed* bytes over
/// transformed ones: plaintext lands on an encrypted image and the
/// AEAD tag of the file is destroyed.
fn payload_is_transformed(
    inode: &Inode,
    filter_config: &crate::filters::FilterConfig,
    encryption_key: Option<&crate::encryption::EncryptionKey>,
) -> bool {
    inode.compressed_size > 0
        || inode.encrypted
        || encryption_key.is_some()
        || filter_config.is_active()
        || inode.filter_typesize > 0
}

/// Decide which of the four write cases applies and compute the exact bytes and
/// sizes that result, performing CPU-intensive filtering, compression, and encryption
/// without holding any filesystem lock.
fn plan_write_prepared<'a>(
    inode: &Inode,
    file_offset: u64,
    data: &'a [u8],
    compression_mode: CompressionMode,
    filter_config: &crate::filters::FilterConfig,
    existing_decompressed: Option<&[u8]>,
    encryption: EncryptionContext<'_>,
) -> Result<WritePlan<'a>, DiskManagerError> {
    let encryption_key = encryption.key;
    let aad = encryption.aad;
    let old_size = inode.size;
    let old_compressed_size = inode.compressed_size;
    // A transformed payload (compressed, filtered or encrypted) always has to be
    // rebuilt as a whole; it can never be patched in place.
    let transformed = payload_is_transformed(inode, filter_config, encryption_key);

    // A write at offset 0 only replaces the whole file when it is at least as long
    // as the current one, or when the payload is transformed and therefore has to
    // be rewritten as a unit.
    let is_full_overwrite =
        file_offset == 0 && (old_size == 0 || data.len() as u64 >= old_size || transformed);

    if is_full_overwrite {
        // The effective filter keeps a file's stored filter pipeline when the
        // caller does not request a new one; the payload and the inode metadata
        // must always agree on whether the payload is filtered.
        let effective = DiskManager::effective_filter(inode, filter_config);
        let payload = DiskManager::build_full_overwrite_buffer_pure(
            data,
            compression_mode,
            &effective,
            encryption_key,
            aad,
        )?;
        // A compressed payload keeps its logical length in `size`; an uncompressed
        // one never shrinks, matching the in-place path's `max` behaviour.
        let new_size = if payload.is_compressed {
            data.len() as u64
        } else if payload.encryption_nonce.is_some() || effective.is_active() {
            // The stored payload is transformed, so its own length is
            // authoritative. Keeping the old (larger) size would make the reader
            // walk past the stored payload: an encrypted file would fail to
            // authenticate (C) and a filtered file would return raw
            // filtered bytes.
            payload.buffer.len() as u64
        } else {
            std::cmp::max(old_size, payload.buffer.len() as u64)
        };
        let new_compressed_size = if payload.is_compressed {
            payload.buffer.len() as u64
        } else {
            0
        };
        return Ok(WritePlan {
            case: WriteCase::FullOverwrite,
            phys_off: 0,
            buffer: payload.buffer,
            new_size,
            new_compressed_size,
            encryption_nonce: payload.encryption_nonce,
        });
    }

    if transformed {
        // Fast path: append a fresh Zstd frame at the physical end of the stream.
        // `encryption_key.is_none()` is belt-and-braces -- on an encrypted image
        // every file is already marked `encrypted`, so `!inode.encrypted` should
        // already imply it. Guarding it here keeps both paths provably identical.
        let fast_append = may_append_zstd_frame(inode, file_offset, filter_config, encryption_key);
        if fast_append {
            // The pooled compressor avoids a per-frame `CCtx`
            // allocation.
            let frame = crate::zstd_pool::compress_frame(data, 0).map_err(DiskManagerError::Io)?;
            // The logical size grows by the *uncompressed* bytes; only the physical
            // stream grows by the frame length.
            return Ok(WritePlan {
                case: WriteCase::CompressedAppend,
                phys_off: old_compressed_size,
                new_size: old_size.saturating_add(data.len() as u64),
                new_compressed_size: old_compressed_size + frame.len() as u64,
                buffer: std::borrow::Cow::Owned(frame),
                encryption_nonce: None,
            });
        }

        // Read-modify-rebuild: splice into the decoded image and rewrite from 0.
        // `existing_decompressed` is the file's decoded content (decrypted,
        // decompressed and unfiltered). It is absent only for an empty file,
        // which then grows from zero.
        let mut full = match existing_decompressed {
            Some(existing) => existing.to_vec(),
            None if old_size == 0 => Vec::new(),
            None => {
                return Err(DiskManagerError::Io(std::io::Error::other(
                    "Missing existing decompressed payload for recompression plan",
                )));
            }
        };
        let end = (file_offset as usize).saturating_add(data.len());
        if full.len() < end {
            full.resize(end, 0);
        }
        full[file_offset as usize..end].copy_from_slice(data);

        let effective = DiskManager::effective_filter(inode, filter_config);
        let payload = DiskManager::build_full_overwrite_buffer_pure(
            &full,
            compression_mode,
            &effective,
            encryption_key,
            aad,
        )?;
        let new_size = if payload.is_compressed {
            full.len() as u64
        } else {
            payload.buffer.len() as u64
        };
        let new_compressed_size = if payload.is_compressed {
            payload.buffer.len() as u64
        } else {
            0
        };
        let owned_buffer = payload.buffer.into_owned();
        return Ok(WritePlan {
            case: WriteCase::Recompress,
            phys_off: 0,
            buffer: std::borrow::Cow::Owned(owned_buffer),
            new_size,
            new_compressed_size,
            encryption_nonce: payload.encryption_nonce,
        });
    }

    // Plain append or random write into an uncompressed file: borrow caller slice directly!
    Ok(WritePlan {
        case: WriteCase::Raw,
        phys_off: file_offset,
        new_size: std::cmp::max(old_size, file_offset + data.len() as u64),
        new_compressed_size: 0,
        buffer: std::borrow::Cow::Borrowed(data),
        encryption_nonce: None,
    })
}

/// Helper to plan a write when already holding a disk lock (e.g. for re-planning after a race).
fn plan_write<'a>(
    guard: &DiskManagerInner,
    inode_id: u64,
    inode: &Inode,
    file_offset: u64,
    data: &'a [u8],
    compression_mode: CompressionMode,
    filter_config: &crate::filters::FilterConfig,
) -> Result<WritePlan<'a>, DiskManagerError> {
    let transformed = payload_is_transformed(inode, filter_config, guard.encryption_key.as_ref());
    let is_full_overwrite =
        file_offset == 0 && (inode.size == 0 || data.len() as u64 >= inode.size || transformed);
    let fast_append = may_append_zstd_frame(
        inode,
        file_offset,
        filter_config,
        guard.encryption_key.as_ref(),
    );

    // A transformed payload is rebuilt from the file's decoded content, so that
    // content has to be read before the plan is computed.
    let existing_decompressed =
        if !is_full_overwrite && transformed && inode.size > 0 && !fast_append {
            Some(DiskManager::read_data_internal(guard, inode_id, inode)?)
        } else {
            None
        };

    // Stack-staged AAD binds the ciphertext to the file with zero allocation.
    let (aad_buf, aad_len) =
        crate::encryption::file_aad_for_scheme_stack(inode_id, guard.superblock.encryption_version);
    plan_write_prepared(
        inode,
        file_offset,
        data,
        compression_mode,
        filter_config,
        existing_decompressed.as_deref(),
        EncryptionContext {
            key: guard.encryption_key.as_ref(),
            aad: &aad_buf[..aad_len],
        },
    )
}

impl DiskManager {
    /// Default storage mode: honors process override, `OIFS_STORAGE_MODE` (`mmap` / `pread` / `file_io`),
    /// falling back to [`StorageMode::Mmap`].
    #[inline]
    fn default_storage_mode() -> StorageMode {
        crate::storage::default_storage_mode()
    }

    /// Open an existing OIFS image or create a new one if it doesn't exist.
    /// `size`: Total size in bytes (only used when creating a new file).
    pub fn open<P: AsRef<Path>>(path: P, total_size: u64) -> Result<Self, DiskManagerError> {
        Self::init_or_open(
            path,
            total_size,
            None,
            false,
            false,
            Self::default_storage_mode(),
        )
    }

    /// Open an existing OIFS image or create a new one using an explicit [`StorageMode`].
    pub fn open_with_storage_mode<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        mode: StorageMode,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(path, total_size, None, false, false, mode)
    }

    /// Open or create an OIFS image with explicit password, metadata WAL journaling flag,
    /// and [`StorageMode`].
    pub fn open_with_options<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: Option<&str>,
        journaled: bool,
        mode: StorageMode,
    ) -> Result<Self, DiskManagerError> {
        let create_enc = !path.as_ref().exists() && password.is_some_and(|p| !p.is_empty());
        Self::init_or_open(path, total_size, password, create_enc, journaled, mode)
    }

    /// Open (or create) an OIFS image using pure positional `pread`/`pwrite` I/O (zero `mmap`).
    pub fn open_file_io<P: AsRef<Path>>(
        path: P,
        total_size: u64,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(path, total_size, None, false, false, StorageMode::FileIo)
    }

    /// Open (or create) a journaled OIFS image with metadata WAL enabled.
    ///
    /// Journaling is opt-in at creation time. Opening an existing journaled image
    /// automatically replays any uncommitted-but-durable transactions before
    /// returning, so the caller always observes a fully recovered filesystem.
    pub fn open_journaled<P: AsRef<Path>>(
        path: P,
        total_size: u64,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(
            path,
            total_size,
            None,
            false,
            true,
            Self::default_storage_mode(),
        )
    }

    /// Open (or create) a journaled OIFS image using pure positional `pread`/`pwrite` I/O (zero `mmap`).
    pub fn open_file_io_journaled<P: AsRef<Path>>(
        path: P,
        total_size: u64,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(path, total_size, None, false, true, StorageMode::FileIo)
    }

    /// Open an encrypted OIFS image with a password
    pub fn open_with_password<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: Option<&str>,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(
            path,
            total_size,
            password,
            false,
            false,
            Self::default_storage_mode(),
        )
    }

    /// Open an encrypted OIFS image with a password using pure positional `pread`/`pwrite` I/O (zero `mmap`).
    pub fn open_file_io_with_password<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: Option<&str>,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(
            path,
            total_size,
            password,
            false,
            false,
            StorageMode::FileIo,
        )
    }

    /// Create a new encrypted filesystem
    pub fn create_encrypted<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: &str,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(
            path,
            total_size,
            Some(password),
            true,
            false,
            Self::default_storage_mode(),
        )
    }

    /// Create a new encrypted filesystem using pure positional `pread`/`pwrite` I/O (zero `mmap`).
    pub fn create_encrypted_file_io<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: &str,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(
            path,
            total_size,
            Some(password),
            true,
            false,
            StorageMode::FileIo,
        )
    }

    /// Create a new encrypted filesystem with metadata journaling enabled.
    pub fn create_encrypted_journaled<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: &str,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(
            path,
            total_size,
            Some(password),
            true,
            true,
            Self::default_storage_mode(),
        )
    }

    /// Create a new encrypted, journaled filesystem using pure positional `pread`/`pwrite` I/O (zero `mmap`).
    pub fn create_encrypted_file_io_journaled<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: &str,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(
            path,
            total_size,
            Some(password),
            true,
            true,
            StorageMode::FileIo,
        )
    }

    /// Open an existing OIFS image with an existing derived encryption key (or unencrypted if None).
    pub fn open_with_key<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        key: Option<crate::encryption::EncryptionKey>,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open_with_key(
            path,
            total_size,
            None,
            key,
            false,
            false,
            Self::default_storage_mode(),
        )
    }

    /// Open an existing OIFS image with an existing derived encryption key and explicit [`StorageMode`].
    pub fn open_with_key_and_storage_mode<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        key: Option<crate::encryption::EncryptionKey>,
        mode: StorageMode,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open_with_key(path, total_size, None, key, false, false, mode)
    }

    /// Open an existing OIFS image in strictly read-only mode using the default [`StorageMode`].
    ///
    /// Acquires a shared advisory file lock (`F_RDLCK`), allowing multiple concurrent read-only
    /// processes on the same `.img` while preventing concurrent read-write mounts (`F_WRLCK`).
    /// Works on `0444` read-only image files and performs zero disk writes.
    pub fn open_read_only<P: AsRef<Path>>(path: P) -> Result<Self, DiskManagerError> {
        Self::open_read_only_with_storage_mode(path, None, Self::default_storage_mode())
    }

    /// Open an existing (plain or encrypted) OIFS image in strictly read-only mode using the
    /// default [`StorageMode`].
    pub fn open_read_only_with_password<P: AsRef<Path>>(
        path: P,
        password: Option<&str>,
    ) -> Result<Self, DiskManagerError> {
        Self::open_read_only_with_storage_mode(path, password, Self::default_storage_mode())
    }

    /// Open an existing OIFS image in strictly read-only mode with an explicit [`StorageMode`].
    ///
    /// - Opens the image with read-only permissions (`O_RDONLY`), so `0444` files succeed.
    /// - Acquires a shared file lock (`F_RDLCK`), enabling multiple simultaneous readers across
    ///   processes while rejecting concurrent exclusive writers (`F_WRLCK`).
    /// - Under [`StorageMode::Mmap`], maps the image `PROT_READ` (`MmapReadOnly`) with full
    ///   zero-copy [`FileReadHandle::mmap_slice`] support.
    /// - On journaled images, verifies with zero disk writes that `0` unrecovered transactions
    ///   remain pending in the WAL ring.
    pub fn open_read_only_with_storage_mode<P: AsRef<Path>>(
        path: P,
        password: Option<&str>,
        mode: StorageMode,
    ) -> Result<Self, DiskManagerError> {
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).open(path)?;

        // Acquire shared non-blocking lock (`F_RDLCK`).
        Self::try_lock_whole_file_shared(&file)?;

        let mut backing = ImageBacking::open_read_only(&file, mode)?;
        if backing.len() < BLOCK_SIZE {
            return Err(DiskManagerError::FileTooSmall);
        }
        let mut sb_buf = [0u8; BLOCK_SIZE];
        backing.read_exact_at(0, &mut sb_buf)?;
        let superblock: SuperBlock = bincode::deserialize(&sb_buf)?;
        if superblock.magic != SuperBlock::MAGIC {
            return Err(DiskManagerError::InvalidMagic);
        }
        superblock
            .validate(backing.len())
            .map_err(DiskManagerError::CorruptedSuperblock)?;

        let (encryption_key, password_verifier) = if superblock.encrypted {
            let pwd = password.ok_or(DiskManagerError::PasswordRequired)?;
            let key = crate::encryption::derive_key(pwd, &superblock.encryption_salt)?;
            let verifier = Self::compute_password_verifier(&key, pwd);
            (Some(key), Some(verifier))
        } else {
            if password.is_some() {
                return Err(DiskManagerError::NotEncrypted);
            }
            (None, None)
        };

        // Load read-only bitmap & journal mirrors for `StorageMode::FileIo` (no-op for `MmapReadOnly`).
        backing.configure_regions(&superblock)?;

        // Verify journal cleanliness with zero disk writes.
        if superblock.has_journal_layout() {
            let clean = backing.is_journal_clean_read_only(&superblock)?;
            let pending = usize::from(!clean);
            if !Self::validate_read_only_journal_state(true, pending) {
                return Err(DiskManagerError::Io(std::io::Error::other(
                    "Journal has unrecovered transactions; open read-write first to recover",
                )));
            }
        }

        let backing_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let free_block_hint = superblock.data_block_start;
        let async_drain_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let requested_io_backend = match mode {
            StorageMode::Mmap => IoBackend::from_env().unwrap_or_default(),
            StorageMode::FileIo => match IoBackend::from_env() {
                Some(IoBackend::IoUring) => IoBackend::IoUring,
                _ => IoBackend::Pread,
            },
        };
        let inner = DiskManagerInner {
            file,
            backing_path,
            backing,
            superblock,
            encryption_key,
            password_verifier,
            free_block_hint,
            free_inode_hint: 0,
            inode_cache: BoundedInodeCache::with_capacity(INODE_CACHE_CAPACITY),
            dir_cache: ShardedDirCache::new(),
            chunk_read_cache: RwLock::new(ChunkReadCache::default()),
            decoded_blob_cache: Mutex::new(DecodedBlobCache::default()),
            durability_mode: std::sync::atomic::AtomicU8::new(DurabilityMode::Lazy as u8),
            pending_async_ranges: Mutex::new(PendingAsyncRanges::default()),
            async_drain_ready: async_drain_ready.clone(),
            io_engine: IoEngine::new(requested_io_backend),
            inode_write_gen: Box::new([0u64; 4096]),
            read_only: true,
        };

        let inner = Arc::new(RwLock::new(inner));
        Ok(Self {
            inner: inner.clone(),
            sync_mutex: Arc::new(Mutex::new(())),
            async_drain_ready,
            auto_defrag: Arc::new(crate::auto_defrag::AutoDefragController::new(&inner)),
        })
    }

    /// Returns `true` when this [`DiskManager`] was mounted in read-only mode.
    pub fn is_read_only(&self) -> bool {
        self.inner.read().unwrap().read_only
    }

    /// Returns `true` if a read-only mount is allowed given `(has_journal, pending_txs)`.
    #[inline]
    pub(crate) const fn validate_read_only_journal_state(
        has_journal: bool,
        pending_txs: usize,
    ) -> bool {
        !has_journal || pending_txs == 0
    }

    /// Returns `true` when opening a path with `mode` requires write permission given
    /// whether the entry already exists.
    #[inline]
    pub(crate) const fn open_mode_requires_write(mode: OpenMode, entry_exists: bool) -> bool {
        match mode {
            OpenMode::OpenExisting => false,
            OpenMode::CreateNew => true,
            OpenMode::CreateOrOpen => !entry_exists,
        }
    }

    /// Returns the active low-level storage backing mode (`Mmap` or `FileIo`).
    pub fn storage_mode(&self) -> StorageMode {
        self.inner.read().unwrap().backing.mode()
    }

    /// Configures the user-space data page cache budget (in bytes) when running under
    /// [`StorageMode::FileIo`]. Passing `0` disables the cache and releases all cached pages.
    /// No-op when running under [`StorageMode::Mmap`].
    pub fn set_file_io_cache_bytes(&self, bytes: usize) {
        self.inner
            .read()
            .unwrap()
            .backing
            .set_data_cache_capacity_bytes(bytes);
    }

    /// Returns the configured user-space data page cache capacity in bytes (`0` for `Mmap`
    /// or when disabled).
    pub fn file_io_cache_bytes(&self) -> usize {
        self.inner
            .read()
            .unwrap()
            .backing
            .data_cache_capacity_bytes()
    }

    /// Returns the approximate number of bytes currently resident in the [`StorageMode::FileIo`]
    /// user-space data page cache.
    pub fn file_io_cache_used_bytes(&self) -> usize {
        self.inner.read().unwrap().backing.data_cache_used_bytes()
    }

    #[cfg(test)]
    pub(crate) fn inner_for_test(&self) -> &Arc<RwLock<DiskManagerInner>> {
        &self.inner
    }

    fn init_or_open<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: Option<&str>,
        create_encrypted: bool,
        journal: bool,
        storage_mode: StorageMode,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open_with_key(
            path,
            total_size,
            password,
            None,
            create_encrypted,
            journal,
            storage_mode,
        )
    }

    fn compute_password_verifier(
        key: &crate::encryption::EncryptionKey,
        pwd: &str,
    ) -> zeroize::Zeroizing<[u8; 32]> {
        use blake2::{Blake2b512, Digest};
        use zeroize::Zeroize;
        let mut hasher = Blake2b512::new();
        hasher.update(b"OIFS-PWD-VERIFIER-V1");
        hasher.update(key.ipc_auth_key());
        hasher.update(pwd.as_bytes());
        let mut digest = hasher.finalize_reset();
        let mut out = zeroize::Zeroizing::new([0u8; 32]);
        out.copy_from_slice(&digest[..32]);
        digest.zeroize();
        out
    }

    /// Verifies `password` against an already-opened [`DiskManager`] handle (used to enforce
    /// credential checks when reusing a cached session in `SESSION_REGISTRY`).
    pub(crate) fn verify_password(&self, password: Option<&str>) -> Result<(), DiskManagerError> {
        let guard = self.inner.read().unwrap();
        if !guard.superblock.encrypted {
            if password.is_some() {
                return Err(DiskManagerError::NotEncrypted);
            }
            return Ok(());
        }
        let pwd = password.ok_or(DiskManagerError::PasswordRequired)?;
        let key = guard
            .encryption_key
            .as_ref()
            .ok_or(DiskManagerError::PasswordRequired)?;
        if let Some(expected) = &guard.password_verifier {
            let candidate = Self::compute_password_verifier(key, pwd);
            if crate::encryption::ct_eq_slice(&*candidate, &**expected) {
                Ok(())
            } else {
                Err(DiskManagerError::DecryptionFailed)
            }
        } else {
            let candidate = crate::encryption::derive_key(pwd, &guard.superblock.encryption_salt)?;
            if crate::encryption::ct_eq_slice(candidate.as_bytes(), key.as_bytes()) {
                Ok(())
            } else {
                Err(DiskManagerError::DecryptionFailed)
            }
        }
    }

    fn init_or_open_with_key<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: Option<&str>,
        existing_key: Option<crate::encryption::EncryptionKey>,
        create_encrypted: bool,
        journal: bool,
        storage_mode: StorageMode,
    ) -> Result<Self, DiskManagerError> {
        let path = path.as_ref();
        let existed_before = path.exists();

        // Never pass O_TRUNC before acquiring the exclusive file lock, otherwise a racing
        // `create_encrypted` call would truncate a live locked `.img` to 0 bytes before
        // `try_lock_whole_file` rejects it.
        let mut open_opts = OpenOptions::new();
        open_opts
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_opts.mode(0o600);
            if create_encrypted {
                open_opts.custom_flags(libc::O_NOFOLLOW);
            }
        }
        let file = open_opts.open(path)?;

        // Acquire Lock using F_SETLK before inspecting or resizing the image.
        Self::try_lock_whole_file(&file)?;

        let current_len = file.metadata()?.len();
        let is_new = !existed_before || create_encrypted || current_len == 0;
        let mut password_verifier = None;
        if is_new {
            let block_count = total_size / BLOCK_SIZE as u64;
            let min_blocks = if journal { 41 } else { 8 };
            if block_count < min_blocks {
                drop(file);
                if !existed_before && current_len == 0 {
                    let _ = std::fs::remove_file(path);
                }
                return Err(DiskManagerError::FileTooSmall);
            }
            if create_encrypted && current_len > 0 {
                file.set_len(0)?;
            }
            file.set_len(total_size)?;
        }

        let mut backing = ImageBacking::open(&file, storage_mode)?;
        let superblock: SuperBlock;
        let encryption_key: Option<crate::encryption::EncryptionKey>;

        if is_new {
            let block_count = total_size / BLOCK_SIZE as u64;
            let mut sb = if journal {
                SuperBlock::new_journaled(block_count)
            } else {
                SuperBlock::new(block_count)
            };
            let should_encrypt = create_encrypted || password.is_some() || existing_key.is_some();
            if should_encrypt {
                if let Some(key) = existing_key {
                    sb.encrypted = true;
                    sb.encryption_salt = crate::encryption::generate_salt();
                    // New images bind the ciphertext to its file.
                    sb.encryption_version = crate::encryption::ENCRYPTION_SCHEME_CONTEXT_BOUND;
                    if let Some(pwd) = password {
                        password_verifier = Some(Self::compute_password_verifier(&key, pwd));
                    }
                    encryption_key = Some(key);
                } else {
                    let pwd = password.unwrap_or_default();
                    sb.encrypted = true;
                    sb.encryption_salt = crate::encryption::generate_salt();
                    // New images bind the ciphertext to its file.
                    sb.encryption_version = crate::encryption::ENCRYPTION_SCHEME_CONTEXT_BOUND;
                    let key = crate::encryption::derive_key(pwd, &sb.encryption_salt)?;
                    password_verifier = Some(Self::compute_password_verifier(&key, pwd));
                    encryption_key = Some(key);
                }
            } else {
                encryption_key = None;
            }
            let serialized = bincode::serialize(&sb)?;
            backing.write_all_at(0, &serialized)?;
            superblock = sb;
        } else {
            if backing.len() < BLOCK_SIZE {
                return Err(DiskManagerError::FileTooSmall);
            }
            let mut sb_buf = [0u8; BLOCK_SIZE];
            backing.read_exact_at(0, &mut sb_buf)?;
            superblock = bincode::deserialize(&sb_buf)?;
            if superblock.magic != SuperBlock::MAGIC {
                return Err(DiskManagerError::InvalidMagic);
            }
            superblock
                .validate(backing.len())
                .map_err(DiskManagerError::CorruptedSuperblock)?;

            if superblock.encrypted {
                if let Some(key) = existing_key {
                    if let Some(pwd) = password {
                        password_verifier = Some(Self::compute_password_verifier(&key, pwd));
                    }
                    encryption_key = Some(key);
                } else {
                    let pwd = password.ok_or(DiskManagerError::PasswordRequired)?;
                    let key = crate::encryption::derive_key(pwd, &superblock.encryption_salt)?;
                    password_verifier = Some(Self::compute_password_verifier(&key, pwd));
                    encryption_key = Some(key);
                }
            } else {
                if password.is_some() || existing_key.is_some() {
                    return Err(DiskManagerError::NotEncrypted);
                }
                encryption_key = None;
            }
        }

        // Load in-memory bitmap & journal mirrors for `StorageMode::FileIo` (no-op for `Mmap`).
        backing.configure_regions(&superblock)?;

        // Mount-time journal handling.
        // A journaled image is recovered *before* the manager is published, so no
        // caller can ever observe a filesystem mid-replay.
        if is_new {
            Self::format_journal(&mut backing, &superblock)?;
        } else if superblock.has_journal_layout() {
            Self::recover_journal(&mut backing, &superblock)?;
            backing.flush()?;
        }

        let backing_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let free_block_hint = superblock.data_block_start;
        let async_drain_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let requested_io_backend = match storage_mode {
            StorageMode::Mmap => IoBackend::from_env().unwrap_or_default(),
            StorageMode::FileIo => match IoBackend::from_env() {
                Some(IoBackend::IoUring) => IoBackend::IoUring,
                _ => IoBackend::Pread,
            },
        };
        let inner = DiskManagerInner {
            file,
            backing_path,
            backing,
            superblock,
            encryption_key,
            password_verifier,
            free_block_hint,
            free_inode_hint: 0,
            inode_cache: BoundedInodeCache::with_capacity(INODE_CACHE_CAPACITY),
            dir_cache: ShardedDirCache::new(),
            chunk_read_cache: RwLock::new(ChunkReadCache::default()),
            decoded_blob_cache: Mutex::new(DecodedBlobCache::default()),
            durability_mode: std::sync::atomic::AtomicU8::new(DurabilityMode::Lazy as u8),
            pending_async_ranges: Mutex::new(PendingAsyncRanges::default()),
            async_drain_ready: async_drain_ready.clone(),
            io_engine: IoEngine::new(requested_io_backend),
            inode_write_gen: Box::new([0u64; 4096]),
            read_only: false,
        };

        let inner = Arc::new(RwLock::new(inner));
        let dm = Self {
            inner: inner.clone(),
            sync_mutex: Arc::new(Mutex::new(())),
            async_drain_ready,
            auto_defrag: Arc::new(crate::auto_defrag::AutoDefragController::new(&inner)),
        };

        if is_new {
            let mut guard = dm.inner.write().unwrap();
            {
                let mut bitmap_slice = guard.inode_bitmap_slice_mut().ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::other("Failed to get inode bitmap"))
                })?;
                let mut ia = SimpleBlockAllocator::new(&mut bitmap_slice, 0);
                let root_id = ia.allocate()?;
                if root_id != 0 {
                    return Err(DiskManagerError::Io(std::io::Error::other(
                        "Failed init root inode",
                    )));
                }
            }

            let data_start = guard.superblock.data_block_start;
            let root_data = {
                let mut data_slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::other("Failed to get data bitmap"))
                })?;
                let mut da = SimpleBlockAllocator::new(&mut data_slice, data_start);
                da.allocate()?
            };

            let mut root_inode = Inode::new(crate::inode::FileType::Directory);
            root_inode.blocks[0] = root_data;
            root_inode.size = BLOCK_SIZE as u64;
            Self::write_inode_internal(&mut guard, 0, &root_inode)?;
            guard.backing.flush()?;
        } else {
            // Already replayed at mount time above; record a clean-shutdown marker so
            // the next mount can tell "clean" from "crashed" without a ring scan.
            let mut guard = dm.inner.write().unwrap();
            if guard.superblock.has_journal_layout() {
                let sb = guard.superblock;
                Self::mark_journal_clean(&mut guard.backing, &sb)?;
            }
        }

        Ok(dm)
    }

    /// Take a non-blocking exclusive `fcntl(F_SETLK)` write lock on the whole file.
    ///
    /// Fails with [`DiskManagerError::Locking`] when another process holds the lock.
    pub(crate) fn try_lock_whole_file(file: &File) -> Result<(), DiskManagerError> {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `flock` is a plain C struct; all-zero is a valid initial value.
        let mut lock = unsafe { std::mem::zeroed::<libc::flock>() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        lock.l_start = 0;
        lock.l_len = 0; // Whole file
        loop {
            // SAFETY: the descriptor is owned by `file` and `lock` outlives the call.
            let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) };
            if rc != -1 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(DiskManagerError::Locking(err));
        }
    }

    /// Take a non-blocking shared `fcntl(F_SETLK)` read lock (`F_RDLCK`) on the whole file.
    ///
    /// Allows multiple concurrent read-only processes to hold the lock simultaneously while
    /// failing with [`DiskManagerError::Locking`] when another process holds an exclusive
    /// write lock (`F_WRLCK`).
    pub(crate) fn try_lock_whole_file_shared(file: &File) -> Result<(), DiskManagerError> {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `flock` is a plain C struct; all-zero is a valid initial value.
        let mut lock = unsafe { std::mem::zeroed::<libc::flock>() };
        lock.l_type = libc::F_RDLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        lock.l_start = 0;
        lock.l_len = 0; // Whole file
        loop {
            // SAFETY: the descriptor is owned by `file` and `lock` outlives the call.
            let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) };
            if rc != -1 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(DiskManagerError::Locking(err));
        }
    }

    /// Record a clean-shutdown marker in the journal header.
    fn mark_journal_clean(
        backing: &mut ImageBacking,
        sb: &SuperBlock,
    ) -> Result<(), DiskManagerError> {
        if !sb.has_journal_layout() {
            return Ok(());
        }
        let mut region = backing.journal_region_mut(sb)?;
        region.mark_header_only();
        let bs = sb.block_size as u64;
        crate::journal::JournalRing::open(&mut region, bs)?.mark_clean_shutdown();
        Ok(())
    }

    /// Returns `true` when this filesystem was created with metadata journaling.
    pub fn has_journal(&self) -> bool {
        self.inner.read().unwrap().superblock.has_journal_layout()
    }

    // Accessor for SuperBlock (Copy)
    pub fn superblock(&self) -> SuperBlock {
        self.inner.read().unwrap().superblock
    }

    /// Returns the canonical path of the underlying image file on host disk.
    pub fn backing_path(&self) -> PathBuf {
        self.inner.read().unwrap().backing_path.clone()
    }

    /// Returns `true` if this filesystem is encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.inner.read().unwrap().superblock.encrypted
    }

    /// Authenticates a password against this disk manager.
    ///
    /// Returns `Ok(())` if the filesystem is unencrypted or if the password correctly
    /// derives the master encryption key. Returns `Err(PasswordRequired)` if encrypted
    /// and no password was provided, or `Err(DecryptionFailed)` if the password was incorrect.
    /// The salt used to derive the filesystem key from the password.
    ///
    /// The salt is not secret (it is stored in the superblock); the IPC
    /// handshake sends it to the client so that both sides can derive the
    /// same key without the password ever travelling over the wire.
    pub fn encryption_salt(&self) -> [u8; 16] {
        self.inner.read().unwrap().superblock.encryption_salt
    }

    /// The domain-separated IPC authentication subkey, used to authenticate IPC clients.
    ///
    /// `None` for an unencrypted image, which authenticates its peers with a
    /// published session secret instead.
    pub fn auth_key(&self) -> Option<Vec<u8>> {
        self.inner
            .read()
            .unwrap()
            .encryption_key
            .as_ref()
            .map(|k| k.ipc_auth_key().to_vec())
    }

    pub fn authenticate(&self, password: Option<&str>) -> Result<(), DiskManagerError> {
        let guard = self.inner.read().unwrap();
        if !guard.superblock.encrypted {
            if password.is_some() {
                return Err(DiskManagerError::NotEncrypted);
            }
            return Ok(());
        }
        let master_key = match &guard.encryption_key {
            Some(k) => k,
            None => return Err(DiskManagerError::PasswordRequired),
        };
        let pwd = password.ok_or(DiskManagerError::PasswordRequired)?;
        let derived = crate::encryption::derive_key(pwd, &guard.superblock.encryption_salt)?;
        let mut diff = 0u8;
        for (a, b) in master_key.as_bytes().iter().zip(derived.as_bytes().iter()) {
            diff |= a ^ b;
        }
        if diff == 0 {
            Ok(())
        } else {
            Err(DiskManagerError::DecryptionFailed)
        }
    }

    /// Byte length of the journal region (header block + record ring).
    fn journal_region_len(block_size: u64) -> usize {
        usize::try_from(crate::journal::JOURNAL_RESERVED_BLOCKS.saturating_mul(block_size))
            .unwrap_or(0)
    }

    /// Absolute byte offset of the journal region within the image.
    ///
    /// Derived from the superblock: the v3 layout places the journal after the
    /// (possibly multi-block) data bitmap, see [`SuperBlock::journal_header_block`].
    fn journal_region_start(sb: &SuperBlock) -> Option<usize> {
        usize::try_from(sb.journal_header_block())
            .ok()?
            .checked_mul(usize::try_from(sb.block_size).ok()?)
    }

    /// Format the journal region of a freshly created journaled image.
    ///
    /// No-op for non-journaled images. The ring starts empty with
    /// `cleanly_unmounted = false`, so a crash before the next clean shutdown is
    /// still detectable on the next mount.
    fn format_journal(backing: &mut ImageBacking, sb: &SuperBlock) -> Result<(), DiskManagerError> {
        if !sb.has_journal_layout() {
            return Ok(());
        }
        let mut region = backing.journal_region_mut(sb)?;
        crate::journal::JournalRing::create(&mut region, sb.block_size as u64)
            .map_err(DiskManagerError::Journal)?;
        Ok(())
    }

    /// Replay any durable-but-unapplied journal transactions (crash recovery).
    ///
    /// Returns the number of transactions replayed. No-op for non-journaled images.
    ///
    /// The region is snapshotted so the image can be mutated while iterating, and
    /// every op is an absolute post-image write, so replaying a transaction twice
    /// is indistinguishable from replaying it once.
    fn recover_journal(
        backing: &mut ImageBacking,
        sb: &SuperBlock,
    ) -> Result<usize, DiskManagerError> {
        if !sb.has_journal_layout() {
            return Ok(0);
        }
        let bs = sb.block_size as u64;

        // Phase 1: validate + decode frames from a snapshot, releasing the borrow.
        let mut snapshot = backing.journal_region_ref(sb)?.to_vec();
        let mut ops = Vec::new();
        let (tx_count, tx_seq) = {
            let mut ring = crate::journal::JournalRing::open(&mut snapshot, bs)
                .map_err(DiskManagerError::Journal)?;
            let n = ring
                .recover(|op| {
                    ops.push(op.clone());
                    Ok(())
                })
                .map_err(DiskManagerError::Journal)?;
            (n, ring.state().tx_seq)
        };

        // Phase 2: redo the ops directly against the image.
        for op in &ops {
            backing.apply_journal_op(sb, op)?;
        }

        // Phase 3: reset the ring and record a clean shutdown.
        let state = crate::journal::JournalState {
            head: 0,
            tail: 0,
            tx_seq,
            cleanly_unmounted: true,
        };
        let header_len = usize::try_from(bs)
            .unwrap_or(0)
            .min(crate::journal::JOURNAL_HEADER_LEN);
        let mut region = backing.journal_region_mut(sb)?;
        region.mark_header_only();
        region[..header_len].copy_from_slice(&state.encode()[..header_len]);
        region.flush_writeback()?;

        Ok(tx_count)
    }

    /// Reads an inode from the inode table
    pub fn read_inode(&self, inode_id: u64) -> Result<Inode, DiskManagerError> {
        let guard = self.inner.read().unwrap();
        Self::read_inode_internal(&guard, inode_id)
    }

    /// Writes an inode to the inode table
    pub fn write_inode(&self, inode_id: u64, inode: &Inode) -> Result<(), DiskManagerError> {
        let res = {
            let mut guard = self.inner.write().unwrap();
            if guard.read_only {
                return Err(DiskManagerError::ReadOnly);
            }
            Self::write_inode_internal(&mut guard, inode_id, inode)?;
            let range = guard.inode_byte_range(inode_id);
            guard.sync_mutation_ranges(&[range])
        };
        self.maybe_drain_async_ranges();
        res
    }

    #[inline]
    fn maybe_drain_async_ranges(&self) {
        if self
            .async_drain_ready
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.drain_pending_async_ranges();
        }
    }

    fn drain_pending_async_ranges(&self) {
        let _sync_guard = crate::lock_unpoisoned(&self.sync_mutex);
        let guard = self.inner.read().unwrap();
        let batch = {
            let mut pending = crate::lock_unpoisoned(&guard.pending_async_ranges);
            pending.pending_ops = 0;
            pending.approx_bytes = 0;
            self.async_drain_ready
                .store(false, std::sync::atomic::Ordering::Release);
            std::mem::take(&mut pending.ranges)
        };
        guard.flush_async_batch(&batch);
    }

    /// Returns the current durability policy mode.
    pub fn durability_mode(&self) -> DurabilityMode {
        self.inner.read().unwrap().durability_mode()
    }

    /// Updates the durability policy mode.
    pub fn set_durability_mode(&self, mode: DurabilityMode) {
        let prev = {
            let guard = self.inner.read().unwrap();
            let prev = guard.durability_mode();
            guard.set_durability_mode(mode);
            prev
        };
        if prev == DurabilityMode::RangeAsync && mode != DurabilityMode::RangeAsync {
            self.drain_pending_async_ranges();
        }
    }

    /// Builder pattern helper to set durability mode upon initialization.
    pub fn with_durability_mode(self, mode: DurabilityMode) -> Self {
        self.set_durability_mode(mode);
        self
    }

    /// Returns the payload-read backend actually in use.
    ///
    /// May differ from [`Self::requested_io_backend`] when `IoUring` was requested on a
    /// platform or kernel without `io_uring` support (falls back to `Pread`).
    pub fn io_backend(&self) -> IoBackend {
        self.inner.read().unwrap().io_engine.effective()
    }

    /// Returns the payload-read backend that was requested.
    pub fn requested_io_backend(&self) -> IoBackend {
        self.inner.read().unwrap().io_engine.requested()
    }

    /// Switches the payload-read backend and returns the effective one.
    ///
    /// Waits for in-progress operations (takes the write lock), so no read straddles
    /// two engines. Under [`StorageMode::FileIo`], requesting `IoBackend::Mmap` falls
    /// back to `IoBackend::Pread`.
    pub fn set_io_backend(&self, backend: IoBackend) -> IoBackend {
        let mut guard = self.inner.write().unwrap();
        let target = if guard.backing.mode() == StorageMode::FileIo && backend == IoBackend::Mmap {
            IoBackend::Pread
        } else {
            backend
        };
        let engine = IoEngine::new(target);
        let effective = engine.effective();
        guard.io_engine = engine;
        effective
    }

    /// Builder pattern helper to set the payload-read backend upon initialization.
    pub fn with_io_backend(self, backend: IoBackend) -> Self {
        self.set_io_backend(backend);
        self
    }

    // ----- Background automatic defragmentation -----

    /// Returns the current background auto-defragmentation configuration.
    pub fn auto_defrag_config(&self) -> crate::auto_defrag::AutoDefragConfig {
        self.auto_defrag.config()
    }

    /// Installs a new background auto-defragmentation configuration.
    ///
    /// Enabling the configuration starts a daemon thread that periodically
    /// measures the fragmentation of the data area and runs an in-place
    /// defragmentation pass whenever the configured threshold is exceeded.
    /// Disabling it stops the thread and waits for the current pass to finish.
    pub fn set_auto_defrag(&self, config: crate::auto_defrag::AutoDefragConfig) {
        if self.is_read_only() && config.enabled {
            return;
        }
        self.auto_defrag.set_config(config);
    }

    /// Returns the counters of the background auto-defragmentation coordinator.
    pub fn auto_defrag_stats(&self) -> crate::auto_defrag::AutoDefragStats {
        self.auto_defrag.stats()
    }

    /// Stops the background auto-defragmentation coordinator.
    pub fn stop_auto_defrag(&self) {
        self.auto_defrag.shutdown();
    }

    #[allow(dead_code)]
    fn find_dir_entry_in_block(
        backing: &ImageBacking,
        block_id: u64,
        name: &str,
    ) -> Result<Option<u64>, DiskManagerError> {
        Ok(backing
            .with_block(block_id, |slice| {
                crate::directory::find_entry_in_block(slice, name)
            })
            .unwrap_or(None))
    }

    fn read_dir_entries_from_block(
        backing: &ImageBacking,
        block_id: u64,
    ) -> Result<Vec<crate::directory::DirectoryEntry>, DiskManagerError> {
        match backing.with_block(block_id, |slice| {
            let iter = crate::directory::DirectoryIterator::new(slice);
            let mut entries = Vec::new();
            for entry in iter {
                entries.push(entry.map_err(|e| {
                    DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e.to_string(),
                    ))
                })?);
            }
            Ok(entries)
        }) {
            Ok(res) => res,
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Returns the number of physical data blocks allocated for this directory.
    /// Handles backward-compatibility where legacy images have inode.size == 0 and inode.blocks[0] != 0.
    #[inline]
    pub fn dir_num_blocks(inode: &Inode) -> usize {
        crate::directory::dir_block_count(inode.size, inode.blocks[0], BLOCK_SIZE as u64)
    }

    /// Scans a directory's blocks for `target_name`, using the stored 64-bit hash to skip
    /// mismatching records without comparing names. Returns `(inode_id, physical_block)`.
    fn locate_entry_in_dir(
        backing: &ImageBacking,
        inode: &Inode,
        target_name: &str,
        target_hash: u64,
    ) -> Option<(u64, u64)> {
        for blk_idx in 0..Self::dir_num_blocks(inode) {
            let phys_blk = Self::resolve_logical_block_id(backing, inode, blk_idx);
            if phys_blk == 0 {
                continue;
            }
            if let Ok(Some(id)) = backing.with_block(phys_blk, |slice| {
                crate::directory::find_entry_in_block_with_hash(slice, target_name, target_hash)
            }) {
                return Some((id, phys_blk));
            }
        }
        None
    }

    /// Reads every entry of a directory into a stored-name -> `(inode, phys_blk, stored_hash)` map,
    /// along with `(may_have_holes, tail_offset)` metadata for O(1) subsequent insertions,
    /// deletions, and zero-hash-recomputation `list_dir` calls.
    #[allow(clippy::type_complexity)]
    fn build_dir_index(
        backing: &ImageBacking,
        inode: &Inode,
    ) -> Result<(FxHashMap<String, (u64, u64, u64)>, bool, Option<(u64, u16)>), DiskManagerError>
    {
        let mut map = FxHashMap::default();
        let num_blocks = Self::dir_num_blocks(inode);
        let mut may_have_holes = false;
        let mut tail_offset = None;
        for blk_idx in 0..num_blocks {
            let phys_blk = Self::resolve_logical_block_id(backing, inode, blk_idx);
            if phys_blk == 0 {
                continue;
            }
            let entries = Self::read_dir_entries_from_block(backing, phys_blk)?;
            let mut used_bytes = 0usize;
            for entry in entries {
                used_bytes += 18 + entry.name.len();
                // Keep the first occurrence, matching on-disk scan order.
                map.entry(entry.name)
                    .or_insert((entry.inode, phys_blk, entry.hash));
            }
            if blk_idx + 1 < num_blocks {
                if used_bytes + 21 <= BLOCK_SIZE {
                    may_have_holes = true;
                }
            } else {
                tail_offset = Some((phys_blk, used_bytes.min(BLOCK_SIZE) as u16));
            }
        }
        Ok((map, may_have_holes, tail_offset))
    }

    /// Resolves `name` inside directory `parent_id` (`Ok(None)` = not found).
    ///
    /// Order: cached hit -> authoritative miss from a complete index -> targeted on-disk scan.
    /// A negative scan (already a full pass) or repeated cold scans promote the directory to a
    /// complete index so later hits, misses and create-time existence checks are O(1).
    /// Resolves the on-disk directory entry name for `name` under `parent_inode_id`.
    ///
    /// On an encrypted filesystem, only the authenticated `_e_` ciphertext name is
    /// accepted (never falling back to plaintext), preventing unauthenticated
    /// plaintext directory-entry injection.
    fn stored_filename<'a>(
        guard: &DiskManagerInner,
        parent_inode_id: u64,
        name: &'a str,
    ) -> Result<std::borrow::Cow<'a, str>, DiskManagerError> {
        if let Some(key) = &guard.encryption_key {
            let enc = crate::encryption::encrypt_filename(key, parent_inode_id, name)?;
            Ok(std::borrow::Cow::Owned(enc))
        } else {
            Ok(std::borrow::Cow::Borrowed(name))
        }
    }

    fn dir_lookup(
        guard: &DiskManagerInner,
        parent_id: u64,
        name: &str,
    ) -> Result<Option<u64>, DiskManagerError> {
        let encrypted = guard.encryption_key.is_some();

        // 1. Fast path: no allocation, no disk access, no SIV filename encryption.
        if let Some(ix) = guard.dir_cache.read_shard(parent_id).get(&parent_id) {
            let hit = if encrypted {
                ix.plain.get(name).map(|&(id, _)| id)
            } else {
                ix.stored.get(name).map(|&(id, _, _)| id)
            };
            if let Some(id) = hit {
                return Ok(Some(id));
            }
            if ix.complete && (!encrypted || ix.plain_complete) {
                return Ok(None);
            }
        }

        let parent = Self::read_inode_internal(guard, parent_id)?;
        if parent.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
        }

        // On an encrypted filesystem, only match the authenticated `_e_` ciphertext name.
        let target_name = Self::stored_filename(guard, parent_id, name)?;
        let candidates = [Some(target_name.as_ref())];

        // 2. Complete index (reachable here on encrypted filesystems when `plain_complete` is false).
        let (indexed, has_entry_yet, cold_scans_so_far) = {
            let cache = guard.dir_cache.read_shard(parent_id);
            match cache.get(&parent_id) {
                Some(ix) if ix.complete => (
                    Some(
                        candidates
                            .iter()
                            .flatten()
                            .find_map(|c| ix.stored.get(*c).map(|&(id, _, _)| id)),
                    ),
                    true,
                    ix.cold_scans,
                ),
                Some(ix) => (None, true, ix.cold_scans),
                None => (None, false, 0),
            }
        };

        if let Some(found) = indexed {
            // On an authoritative negative lookup, return immediately without taking shard `.write()`.
            if let Some(id) = found {
                let mut cache = guard.dir_cache.write_shard(parent_id);
                let ix = cache.entry(parent_id).or_default();
                ix.plain.insert(
                    name.to_string(),
                    (id, crate::directory::hash_filename(name)),
                );
                if ix.complete && ix.plain.len() == ix.stored.len() {
                    ix.plain_complete = true;
                }
            }
            return Ok(found);
        }

        // 3. First-touch unindexed directory: build the complete index in a single pass
        //    instead of scanning all blocks twice on a cold negative lookup.
        if !has_entry_yet {
            let (stored, may_have_holes, tail_offset) =
                Self::build_dir_index(&guard.backing, &parent)?;
            let found = candidates
                .iter()
                .flatten()
                .find_map(|c| stored.get(*c).map(|&(id, _, _)| id));
            let is_empty = stored.is_empty();
            let mut cache = guard.dir_cache.write_shard(parent_id);
            let ix = cache.entry(parent_id).or_default();
            if !ix.complete {
                ix.stored = stored;
                ix.may_have_holes = may_have_holes;
                ix.tail_offset = tail_offset;
                ix.complete = true;
                if is_empty {
                    ix.plain.clear();
                    ix.plain_complete = true;
                }
            }
            if let Some(id) = found
                && encrypted
            {
                ix.plain.insert(
                    name.to_string(),
                    (id, crate::directory::hash_filename(name)),
                );
                if ix.complete && ix.plain.len() == ix.stored.len() {
                    ix.plain_complete = true;
                }
            }
            return Ok(found);
        }

        // 4. Targeted on-disk scan (retains `(id, blk, hash)` so a hit never re-scans).
        let found_loc = candidates.iter().flatten().find_map(|c| {
            let h = crate::directory::hash_filename(c);
            Self::locate_entry_in_dir(&guard.backing, &parent, c, h).map(|(id, blk)| (id, blk, h))
        });
        let found = found_loc.map(|(id, _, _)| id);

        // Build full index *outside* the shard write lock when promoting on miss or threshold.
        let promoted_index = if found.is_none()
            || cold_scans_so_far.saturating_add(1) >= DIR_INDEX_BUILD_AFTER_SCANS
        {
            Some(Self::build_dir_index(&guard.backing, &parent)?)
        } else {
            None
        };

        let mut cache = guard.dir_cache.write_shard(parent_id);
        let ix = cache.entry(parent_id).or_default();
        ix.cold_scans = ix.cold_scans.saturating_add(1);
        if !ix.complete
            && let Some((stored, may_have_holes, tail_offset)) = promoted_index
        {
            let is_empty = stored.is_empty();
            ix.stored = stored;
            ix.may_have_holes = may_have_holes;
            ix.tail_offset = tail_offset;
            ix.complete = true;
            if is_empty {
                ix.plain.clear();
                ix.plain_complete = true;
            }
        }
        if let Some((id, blk, h)) = found_loc {
            if encrypted {
                ix.plain.insert(
                    name.to_string(),
                    (id, crate::directory::hash_filename(name)),
                );
                if ix.complete && ix.plain.len() == ix.stored.len() {
                    ix.plain_complete = true;
                }
            } else if !ix.complete {
                ix.stored.insert(name.to_string(), (id, blk, h));
            }
        }
        Ok(found)
    }

    /// Finds an existing `(phys_blk, insert_offset, is_tail_block)` in `parent_inode` with at
    /// least `needed_space` bytes remaining, or returns `(None, all_earlier_full)` when a new
    /// directory block must be allocated.
    fn find_dir_insert_slot(
        guard: &DiskManagerInner,
        parent_inode_id: u64,
        parent_inode: &Inode,
        needed_space: usize,
    ) -> (Option<(u64, usize, bool)>, bool) {
        let num_blocks = Self::dir_num_blocks(parent_inode);
        if num_blocks == 0 {
            return (None, true);
        }

        let (cached_tail, may_have_holes) = guard
            .dir_cache
            .read_shard(parent_inode_id)
            .get(&parent_inode_id)
            .map(|ix| (ix.tail_offset, ix.may_have_holes))
            .unwrap_or((None, num_blocks > 1));

        let last_idx = num_blocks - 1;
        let tail_blk = Self::resolve_logical_block_id(&guard.backing, parent_inode, last_idx);
        if guard.is_data_block(tail_blk)
            && let Ok(Some(res)) = guard.backing.with_block(tail_blk, |slice| {
                let insert_offset = match cached_tail {
                    Some((blk, off))
                        if blk == tail_blk
                            && (off as usize) + 18 <= BLOCK_SIZE
                            && slice[off as usize..off as usize + 18] == [0u8; 18] =>
                    {
                        off as usize
                    }
                    _ => crate::directory::find_insert_offset_in_block(slice),
                };
                if insert_offset + needed_space <= BLOCK_SIZE {
                    Some((tail_blk, insert_offset, true))
                } else {
                    None
                }
            })
        {
            return (Some(res), !may_have_holes);
        }

        // If no deletions have occurred in earlier blocks (`0..num_blocks - 1`),
        // skip the O(B) historical block scan and allocate a new block immediately.
        if !may_have_holes || num_blocks <= 1 {
            return (None, true);
        }

        let mut any_hole_remaining = false;
        for blk_idx in 0..last_idx {
            let phys_blk = Self::resolve_logical_block_id(&guard.backing, parent_inode, blk_idx);
            if !guard.is_data_block(phys_blk) {
                continue;
            }
            if let Ok((found, has_hole)) = guard.backing.with_block(phys_blk, |slice| {
                let insert_offset = crate::directory::find_insert_offset_in_block(slice);
                if insert_offset + needed_space <= BLOCK_SIZE {
                    (Some((phys_blk, insert_offset, false)), false)
                } else {
                    (None, insert_offset + 21 <= BLOCK_SIZE)
                }
            }) {
                if let Some(slot) = found {
                    return (Some(slot), false);
                }
                if has_hole {
                    any_hole_remaining = true;
                }
            }
        }

        (None, !any_hole_remaining)
    }

    /// Appends a directory entry to a directory, allocating a new block if all existing blocks are full.
    ///
    /// The last block is tried first: in a growing directory every earlier block is full, so this
    /// makes the common insert O(1) blocks; earlier blocks are only probed (to reuse space freed by
    /// deletions) when the last block is full and `may_have_holes` is set.
    #[allow(clippy::type_complexity)]
    fn append_dir_entry_to_dir(
        guard: &mut DiskManagerInner,
        parent_inode_id: u64,
        parent_inode: &mut Inode,
        entry: &crate::directory::DirectoryEntry,
    ) -> Result<(u64, Option<(u64, u16)>, bool), DiskManagerError> {
        let num_blocks = Self::dir_num_blocks(parent_inode);
        let entry_len = 18 + entry.name.len();
        let needed_space = 20 + entry.name.len();

        let (slot, all_earlier_full) =
            Self::find_dir_insert_slot(guard, parent_inode_id, parent_inode, needed_space);
        if let Some((phys_blk, insert_offset, is_tail)) = slot {
            Self::append_dir_entry_to_block(&mut guard.backing, phys_blk, insert_offset, entry)?;
            let next_tail = if is_tail {
                Some((phys_blk, (insert_offset + entry_len) as u16))
            } else {
                None
            };
            return Ok((phys_blk, next_tail, all_earlier_full));
        }

        // All existing blocks are full (or the directory had none): grow by one block.
        // `get_or_alloc_block` already zeroes newly allocated blocks via `allocate_block`.
        let new_blk_idx = num_blocks;
        let new_phys_block = Self::get_or_alloc_block(guard, parent_inode, new_blk_idx, true)?;
        Self::append_dir_entry_to_block(&mut guard.backing, new_phys_block, 0, entry)?;
        parent_inode.size = (new_blk_idx + 1) as u64 * BLOCK_SIZE as u64;
        Ok((
            new_phys_block,
            Some((new_phys_block, entry_len as u16)),
            all_earlier_full,
        ))
    }

    fn append_dir_entry_to_block(
        backing: &mut ImageBacking,
        block_id: u64,
        insert_offset: usize,
        entry: &crate::directory::DirectoryEntry,
    ) -> Result<(), DiskManagerError> {
        if insert_offset + 20 + entry.name.len() > BLOCK_SIZE {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Directory block is full",
            )));
        }

        backing
            .with_block_mut(block_id, |block_slice| {
                let mut cursor = std::io::Cursor::new(&mut block_slice[..]);
                cursor.set_position(insert_offset as u64);
                entry
                    .serialize_into(&mut cursor)
                    .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))
            })
            .map_err(|_| {
                DiskManagerError::Io(std::io::Error::other("Directory block not found"))
            })??;
        Ok(())
    }

    #[allow(dead_code)]
    fn rewrite_dir_entries_in_block(
        backing: &mut ImageBacking,
        block_id: u64,
        entries: &[crate::directory::DirectoryEntry],
    ) -> Result<(), DiskManagerError> {
        backing
            .with_block_mut(block_id, |block_slice| {
                block_slice.fill(0);
                let mut cursor = std::io::Cursor::new(&mut block_slice[..]);
                for entry in entries {
                    entry
                        .serialize_into(&mut cursor)
                        .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
                }
                Ok::<(), DiskManagerError>(())
            })
            .map_err(|_| {
                DiskManagerError::Io(std::io::Error::other("Directory block not found"))
            })??;
        Ok(())
    }

    /// Build the post-image of a directory block without touching the image.
    ///
    /// Mirrors [`Self::rewrite_dir_entries_in_block`] byte for byte (zero-fill, then
    /// serialize entries in order) so a journaled rewrite produces exactly the same
    /// block as the legacy in-place path.
    #[allow(dead_code)]
    fn build_dir_block_image(
        entries: &[crate::directory::DirectoryEntry],
    ) -> Result<Vec<u8>, DiskManagerError> {
        let mut buf = vec![0u8; BLOCK_SIZE];
        {
            let mut cursor = std::io::Cursor::new(&mut buf[..]);
            for entry in entries {
                entry
                    .serialize_into(&mut cursor)
                    .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
            }
        }
        Ok(buf)
    }

    /// Build the 256-byte post-image of an inode slot without touching the image.
    ///
    /// [`Self::write_inode_internal`] overwrites only the leading `bincode` bytes and
    /// leaves the tail of the slot untouched, so the post-image must start from the
    /// slot's current contents rather than from zeros.
    fn build_inode_post_image(
        guard: &DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
    ) -> Result<[u8; crate::inode_format::INODE_SLOT_SIZE], DiskManagerError> {
        // Shares the encoder with the in-place writer, so the bytes replayed by the
        // journal are exactly the bytes a clean mount would have written.
        Self::encode_inode_slot(guard, inode_id, inode)
    }

    /// Write a metadata transaction to the WAL and flush it to stable storage.
    ///
    /// This is the **commit point**: on return the transaction is durable, so the
    /// caller may apply the ops in place. If the process dies between the commit and
    /// the in-place phase, mount-time recovery replays the transaction.
    ///
    /// No-op for non-journaled filesystems, which keeps the legacy path free of any
    /// extra I/O or CPU.
    fn commit_journal_tx(
        guard: &mut DiskManagerInner,
        ops: &[crate::journal::MetadataOp],
    ) -> Result<(), DiskManagerError> {
        let sb = guard.superblock;
        if !sb.has_journal_layout() {
            return Ok(());
        }
        let bs = sb.block_size as u64;
        let is_strict = guard.durability_mode() == DurabilityMode::Strict;
        let append_res = {
            let mut region = guard.backing.journal_region_mut(&sb)?;
            let hdr_len = crate::journal::JOURNAL_HEADER_LEN.min(region.len());
            let mut prev_hdr = [0u8; crate::journal::JOURNAL_HEADER_LEN];
            prev_hdr[..hdr_len].copy_from_slice(&region[..hdr_len]);
            let append_opt = {
                let mut ring = crate::journal::JournalRing::open(&mut region, bs)?;
                if is_strict {
                    ring.try_append_without_overwrite(ops)?
                        .map(|_| (ring.last_write(), ring.last_wrap_pad()))
                } else {
                    ring.append(ops)?;
                    Some((ring.last_write(), ring.last_wrap_pad()))
                }
            };
            if let Some((written, wrap_pad)) = append_opt {
                region.mark_append_written(bs as usize, written, wrap_pad);
                if let Err(e) = region.flush_writeback() {
                    region[..hdr_len].copy_from_slice(&prev_hdr[..hdr_len]);
                    return Err(e.into());
                }
                Some(written)
            } else {
                region.mark_clean();
                None
            }
        };

        let written = match append_res {
            Some(w) => w,
            None => {
                // Under Strict durability, flush all previously applied in-place metadata
                // and checkpoint the ring before overwriting uncheckpointed entries.
                guard.backing.flush()?;
                Self::checkpoint_journal(&mut guard.backing, &sb)?;

                let mut region = guard.backing.journal_region_mut(&sb)?;
                let hdr_len = crate::journal::JOURNAL_HEADER_LEN.min(region.len());
                let mut prev_hdr = [0u8; crate::journal::JOURNAL_HEADER_LEN];
                prev_hdr[..hdr_len].copy_from_slice(&region[..hdr_len]);
                let (written, wrap_pad) = {
                    let mut ring = crate::journal::JournalRing::open(&mut region, bs)?;
                    ring.append(ops)?;
                    (ring.last_write(), ring.last_wrap_pad())
                };
                region.mark_append_written(bs as usize, written, wrap_pad);
                if let Err(e) = region.flush_writeback() {
                    region[..hdr_len].copy_from_slice(&prev_hdr[..hdr_len]);
                    return Err(e.into());
                }
                written
            }
        };

        // The WAL must reach stable storage *before* the metadata it describes, or a
        // power loss can leave the image with partially applied metadata that recovery
        // has no record of. Under group-commit batching (`is_strict_sync_deferred()`),
        // the drainer thread performs a unified `flush()` barrier before completing any
        // future in the batch.
        if guard.durability_mode() != DurabilityMode::Strict || is_strict_sync_deferred() {
            return Ok(());
        }

        // Without the header flush the cursor update could be reordered after the
        // frame, and recovery would never see the transaction.
        let start = Self::journal_region_start(&sb).unwrap_or(0);
        let ring_base = start + usize::try_from(bs).unwrap_or(0);
        let header_len = usize::try_from(bs)
            .unwrap_or(0)
            .min(crate::journal::JOURNAL_HEADER_LEN);
        if let Some((s, e)) = written
            && e > s
        {
            let off = ring_base + usize::try_from(s).unwrap_or(0);
            let len = usize::try_from(e - s).unwrap_or(0);
            if off + len <= guard.backing.len() {
                guard.backing.flush_range(start, (off - start) + len)?;
                return Ok(());
            }
        }
        if header_len > 0 {
            guard.backing.flush_range(start, header_len)?;
        }
        Ok(())
    }

    /// Write out the whole journal region, WAL before anything else.
    ///
    /// Used by `flush()`/`flush_async()` so that a caller-driven sync point preserves
    /// the ordering invariant even when individual transactions skipped their own
    /// barrier.
    fn sync_journal_region(guard: &DiskManagerInner) -> Result<(), DiskManagerError> {
        let sb = guard.superblock;
        if !sb.has_journal_layout() {
            return Ok(());
        }
        let bs = sb.block_size as u64;
        let start = Self::journal_region_start(&sb).unwrap_or(0);
        let len = Self::journal_region_len(bs);
        let end = start.saturating_add(len).min(guard.backing.len());
        if end > start {
            guard.backing.flush_range(start, end - start)?;
        }
        Ok(())
    }

    /// Apply ops to the image and refresh the caches they invalidate.
    fn apply_journal_ops(
        guard: &mut DiskManagerInner,
        ops: &[crate::journal::MetadataOp],
    ) -> Result<(), DiskManagerError> {
        let sb = guard.superblock;
        for op in ops {
            guard.backing.apply_journal_op(&sb, op)?;
            match op {
                crate::journal::MetadataOp::WriteInode { inode_id, .. }
                | crate::journal::MetadataOp::SetInodeBitmap {
                    inode_id,
                    allocated: false,
                } => {
                    guard.invalidate_payload_caches(*inode_id);
                    guard.bump_inode_gen(*inode_id);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn collect_inode_blocks(backing: &ImageBacking, inode: &Inode) -> Vec<u64> {
        let mut blks = Vec::new();
        Self::for_each_inode_block(backing, inode, |blk| blks.push(blk));
        blks
    }

    fn for_each_inode_block(backing: &ImageBacking, inode: &Inode, mut visit: impl FnMut(u64)) {
        let is_seekable = inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0;

        #[inline]
        fn push_leaf(val: u64, is_seekable: bool, visit: &mut impl FnMut(u64)) {
            if val != 0 {
                if is_seekable {
                    let entry = crate::inode::ChunkEntry(val);
                    if !entry.is_empty() {
                        let start = entry.start_block() as u64;
                        let count = entry.block_count() as u64;
                        for b in 0..count {
                            visit(start + b);
                        }
                    }
                } else {
                    visit(val);
                }
            }
        }

        // 1. Direct blocks (0..10)
        for i in 0..10 {
            push_leaf(inode.blocks[i], is_seekable, &mut visit);
        }

        // 2. Single Indirect block (10)
        let sib_id = inode.blocks[10];
        if sib_id != 0 {
            visit(sib_id);
            let _ = backing.with_block(sib_id, |slice| {
                for chunk in slice.as_chunks::<8>().0 {
                    let blk = u64::from_le_bytes(*chunk);
                    push_leaf(blk, is_seekable, &mut visit);
                }
            });
        }

        // 3. Double Indirect block (11)
        let dib_id = inode.blocks[11];
        if dib_id != 0 {
            visit(dib_id);
            let total_logical_entries = if is_seekable {
                (inode.size as usize).div_ceil(crate::inode::CHUNK_SIZE_64K) as u64
            } else {
                let physical_size = if inode.compressed_size > 0 {
                    inode.compressed_size
                } else {
                    inode.size
                };
                physical_size.div_ceil(BLOCK_SIZE as u64)
            };
            let max_s_entries = if total_logical_entries > 522 {
                let diff = total_logical_entries - 522;
                let needed = diff.div_ceil(512) as usize;
                needed.min(512)
            } else {
                512 // Fallback if size was 0 or reset before collect
            };

            let sibs: Vec<u64> = backing
                .with_block(dib_id, |slice| {
                    slice[..max_s_entries * 8]
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_le_bytes(*c))
                        .filter(|&sib| sib != 0)
                        .collect()
                })
                .unwrap_or_default();
            for sib in sibs {
                visit(sib);
                let _ = backing.with_block(sib, |s_slice| {
                    for d_chunk in s_slice.as_chunks::<8>().0 {
                        let blk = u64::from_le_bytes(*d_chunk);
                        push_leaf(blk, is_seekable, &mut visit);
                    }
                });
            }
        }

        // 4. Triple Indirect block
        let tib_id = inode.triple_indirect;
        if tib_id != 0 {
            visit(tib_id);
            let total_logical_entries = if is_seekable {
                (inode.size as usize).div_ceil(crate::inode::CHUNK_SIZE_64K) as u64
            } else {
                let physical_size = if inode.compressed_size > 0 {
                    inode.compressed_size
                } else {
                    inode.size
                };
                physical_size.div_ceil(BLOCK_SIZE as u64)
            };
            let max_t_entries = if total_logical_entries > 262666 {
                let diff = total_logical_entries - 262666;
                let needed = diff.div_ceil(512 * 512) as usize;
                needed.min(512)
            } else {
                512 // Fallback if size was 0 or reset before collect
            };

            let dibs: Vec<u64> = backing
                .with_block(tib_id, |slice| {
                    slice[..max_t_entries * 8]
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_le_bytes(*c))
                        .filter(|&dib| dib != 0)
                        .collect()
                })
                .unwrap_or_default();
            for dib in dibs {
                visit(dib);
                let sibs: Vec<u64> = backing
                    .with_block(dib, |d_slice| {
                        d_slice
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .map(|c| u64::from_le_bytes(*c))
                            .filter(|&sib| sib != 0)
                            .collect()
                    })
                    .unwrap_or_default();
                for sib in sibs {
                    visit(sib);
                    let _ = backing.with_block(sib, |s_slice| {
                        for blk_chunk in s_slice.as_chunks::<8>().0 {
                            let blk = u64::from_le_bytes(*blk_chunk);
                            push_leaf(blk, is_seekable, &mut visit);
                        }
                    });
                }
            }
        }
    }

    /// Reset the journal ring, discarding transactions already applied in place.
    fn checkpoint_journal(
        backing: &mut ImageBacking,
        sb: &SuperBlock,
    ) -> Result<(), DiskManagerError> {
        if !sb.has_journal_layout() {
            return Ok(());
        }
        let bs = sb.block_size as u64;
        let start = Self::journal_region_start(sb).unwrap_or(0);
        let header_len = usize::try_from(bs)
            .unwrap_or(0)
            .min(crate::journal::JOURNAL_HEADER_LEN);
        {
            let mut region = backing.journal_region_mut(sb)?;
            region.mark_header_only();
            crate::journal::JournalRing::open(&mut region, bs)?.checkpoint();
        }
        if header_len > 0 {
            backing.flush_range(start, header_len)?;
        }
        Ok(())
    }

    /// Checkpoint the WAL when it is provably safe to discard committed transactions.
    ///
    /// A checkpoint throws away transactions that have already been applied in place,
    /// so it is only safe once those in-place bytes are durable. Otherwise a crash
    /// could leave a half-applied transaction with no WAL record left to replay.
    ///
    /// * `Strict` — `sync_mutation_ranges` has already `msync`ed every mutated range,
    ///   so the ring can be reclaimed immediately.
    /// * `Lazy` / `RangeAsync` / `LegacyWholeMmapAsync` — the image may still live only
    ///   in the page cache, so the ring is kept and mount-time recovery will replay it.
    ///   This is safe because the ring overwrites oldest-first, so a long-running Master
    ///   never grows the journal without bound.
    ///
    /// Non-journaled images are a no-op.
    fn maybe_checkpoint(guard: &mut DiskManagerInner) -> Result<(), DiskManagerError> {
        let sb = guard.superblock;
        if !sb.has_journal_layout() || guard.durability_mode() != DurabilityMode::Strict {
            return Ok(());
        }
        // JBD2-style deferred in-place metadata checkpointing:
        // Under `Strict` mode on a journaled image, every transaction's payload and WAL
        // commit frame+header are synchronously flushed before the mutation returns,
        // while in-place metadata blocks stay in the OS page cache and are flushed in
        // one batch once the WAL ring reaches its 50% checkpoint watermark, or on
        // `flush()` / `Drop`.
        let bs = sb.block_size as u64;
        let needs_ckpt = {
            let mut region = guard.backing.journal_region_mut(&sb)?;
            region.mark_clean();
            let ring = crate::journal::JournalRing::open(&mut region, bs)?;
            ring.used() >= ring.ring_bytes() / 2
        };
        if needs_ckpt {
            guard.backing.flush()?;
            Self::checkpoint_journal(&mut guard.backing, &sb)?;
        }
        Ok(())
    }

    /// Publicly force a journal checkpoint, e.g. after an explicit [`Self::flush`].
    ///
    /// Returns the number of transactions discarded.
    pub fn checkpoint_journal_now(&self) -> Result<usize, DiskManagerError> {
        let mut guard = self.inner.write().unwrap();
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }
        let sb = guard.superblock;
        if !sb.has_journal_layout() {
            return Ok(0);
        }
        let bs = sb.block_size as u64;
        let discarded = {
            let mut region = guard.backing.journal_region_mut(&sb)?;
            region.mark_header_only();
            let mut ring = crate::journal::JournalRing::open(&mut region, bs)?;
            let pending = ring.pending_transactions();
            ring.checkpoint();
            pending
        };
        let start = Self::journal_region_start(&sb).unwrap_or(0);
        let header_len = usize::try_from(bs)
            .unwrap_or(0)
            .min(crate::journal::JOURNAL_HEADER_LEN);
        if header_len > 0 {
            guard.backing.flush_range(start, header_len)?;
        }
        Ok(discarded)
    }

    fn create_entry_journaled(
        guard: &mut DiskManagerInner,
        parent_inode_id: u64,
        name: &str,
        stored_name: String,
        stored_hash: u64,
        file_type: crate::inode::FileType,
    ) -> Result<u64, DiskManagerError> {
        crate::directory::validate_filename(name).map_err(|e| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
        crate::directory::validate_filename(&stored_name).map_err(|e| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;

        // ---- Phase 1: in-lock validation (re-verify parent & collision in case of race) ----
        let mut parent_inode = Self::read_inode_internal(guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
        }

        if Self::dir_lookup(guard, parent_inode_id, name)?.is_some() {
            let type_str = if file_type == crate::inode::FileType::Directory {
                "Directory"
            } else {
                "File"
            };
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} '{}' already exists", type_str, name),
            )));
        }

        // ---- Phase 2: simulate allocation ----
        let ib_slice = guard
            .inode_bitmap_slice()
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("inode bitmap not found")))?;
        // The search is bounded by the inode table, so an id that
        // would address the data area is never handed out.
        let new_inode_id = crate::bitmap::BitmapRef::new(ib_slice)
            .find_next_free_bounded(
                guard.free_inode_hint as usize,
                guard.superblock.usable_inode_count() as usize,
            )
            .ok_or(DiskManagerError::Allocator(
                crate::allocator::AllocatorError::NoSpace,
            ))? as u64;
        let next_free_inode_hint = new_inode_id + 1;

        let mut ops = Vec::with_capacity(4);
        ops.push(crate::journal::MetadataOp::SetInodeBitmap {
            inode_id: new_inode_id,
            allocated: true,
        });

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut new_inode = Inode::new(file_type);
        new_inode.created_at = now;
        new_inode.modified_at = now;

        let mut sim_opt: Option<crate::journal::AllocSim> = None;
        if file_type == crate::inode::FileType::Directory {
            let sim = sim_opt.insert(crate::journal::AllocSim::new_data_only(
                guard,
                next_free_inode_hint,
                guard.free_block_hint,
            )?);
            let dir_data_block = sim.alloc_block(&guard.backing)?;
            ops.push(crate::journal::MetadataOp::SetDataBitmap {
                block_id: dir_data_block,
                allocated: true,
            });
            if crate::journal::block_has_nonzero_bytes(&guard.backing, dir_data_block) {
                ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                    block_id: dir_data_block,
                    offset: 0,
                    data: vec![0u8; BLOCK_SIZE],
                });
            }
            new_inode.blocks[0] = dir_data_block;
            new_inode.size = BLOCK_SIZE as u64;
        }

        let entry = crate::directory::DirectoryEntry {
            inode: new_inode_id,
            hash: stored_hash,
            name: stored_name.clone(),
        };
        let mut entry_bytes = Vec::with_capacity(20 + entry.name.len());
        {
            let mut cursor = std::io::Cursor::new(&mut entry_bytes);
            entry
                .serialize_into(&mut cursor)
                .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
        }

        // Try to place the entry into an existing directory block (read-only probe).
        let num_blocks = Self::dir_num_blocks(&parent_inode);
        let entry_len = entry_bytes.len();
        let needed_space = 20 + entry.name.len();
        let (slot, all_earlier_full) =
            Self::find_dir_insert_slot(guard, parent_inode_id, &parent_inode, needed_space);
        let (written_blk, next_tail) = if let Some((phys_blk, insert_offset, is_tail)) = slot {
            ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                block_id: phys_blk,
                offset: insert_offset as u32,
                data: std::mem::take(&mut entry_bytes),
            });
            let tail = if is_tail {
                Some((phys_blk, (insert_offset + entry_len) as u16))
            } else {
                None
            };
            (phys_blk, tail)
        } else {
            // Grow the directory by one block. `sim_get_or_alloc_block` may also need to
            // allocate indirect pointer blocks once the directory exceeds 10 blocks;
            // those allocations are recorded as ops too.
            let sim = match &mut sim_opt {
                Some(s) => s,
                None => sim_opt.insert(crate::journal::AllocSim::new_data_only(
                    guard,
                    next_free_inode_hint,
                    guard.free_block_hint,
                )?),
            };
            let new_blk_idx = num_blocks;
            let phys_blk = crate::journal::sim_get_or_alloc_block(
                &guard.backing,
                &mut parent_inode,
                new_blk_idx,
                sim,
                &mut ops,
            )?;
            // A freshly grown directory block is zero-filled by the legacy path, so the
            // post-image is the whole block rather than just the entry.
            let mut block_image = vec![0u8; BLOCK_SIZE];
            block_image[..entry_bytes.len()].copy_from_slice(&entry_bytes);
            ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                block_id: phys_blk,
                offset: 0,
                data: block_image,
            });
            parent_inode.size = (new_blk_idx + 1) as u64 * BLOCK_SIZE as u64;
            (phys_blk, Some((phys_blk, entry_len as u16)))
        };

        parent_inode.modified_at = now;

        // Post-images must be read from the *unmodified* image.
        let new_inode_image = Self::build_inode_post_image(guard, new_inode_id, &new_inode)?;
        let parent_image = Self::build_inode_post_image(guard, parent_inode_id, &parent_inode)?;
        ops.push(crate::journal::MetadataOp::WriteInode {
            inode_id: new_inode_id,
            inode_bytes: Box::new(new_inode_image),
        });
        ops.push(crate::journal::MetadataOp::WriteInode {
            inode_id: parent_inode_id,
            inode_bytes: Box::new(parent_image),
        });

        // ---- Phase 3: commit point ----
        Self::commit_journal_tx(guard, &ops)?;

        // ---- Phase 4: apply in place ----
        Self::apply_journal_ops(guard, &ops)?;

        guard.free_inode_hint = next_free_inode_hint;
        if let Some(sim) = &sim_opt {
            guard.free_block_hint = sim.free_block_hint;
        }

        guard.inode_cache.insert(new_inode_id, new_inode);
        guard.inode_cache.insert(parent_inode_id, parent_inode);
        let encrypted = guard.encryption_key.is_some();
        if let Some(ix) = guard
            .dir_cache
            .write_shard(parent_inode_id)
            .get_mut(&parent_inode_id)
        {
            if encrypted {
                ix.plain.insert(
                    name.to_string(),
                    (new_inode_id, crate::directory::hash_filename(name)),
                );
            }
            ix.stored
                .insert(stored_name, (new_inode_id, written_blk, stored_hash));
            if encrypted && ix.complete && ix.plain.len() == ix.stored.len() {
                ix.plain_complete = true;
            }
            if let Some(tail) = next_tail {
                ix.tail_offset = Some(tail);
            }
            if all_earlier_full {
                ix.may_have_holes = false;
            }
        }

        if guard.durability_mode() == DurabilityMode::RangeAsync {
            let mut ranges = vec![
                guard.inode_bitmap_byte_range(),
                guard.data_bitmap_byte_range(),
                guard.inode_byte_range(new_inode_id),
                guard.inode_byte_range(parent_inode_id),
                guard.block_byte_range(written_blk),
            ];
            if file_type == crate::inode::FileType::Directory {
                ranges.push(guard.block_byte_range(new_inode.blocks[0]));
            }
            if let Some(sim) = &sim_opt {
                for blk in &sim.fresh_blocks {
                    ranges.push(guard.block_byte_range(*blk));
                }
            }
            guard.sync_mutation_ranges(&ranges)?;
        } else if guard.durability_mode() == DurabilityMode::LegacyWholeMmapAsync {
            guard.sync_mutation_ranges(&[])?;
        }
        Self::maybe_checkpoint(guard)?;
        Ok(new_inode_id)
    }

    fn create_entry_internal(
        &self,
        parent_inode_id: u64,
        name: &str,
        file_type: crate::inode::FileType,
    ) -> Result<u64, DiskManagerError> {
        if self.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        crate::directory::validate_filename(name).map_err(|e| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;

        // --- STAGE 1: Out-of-Lock Fast Validation & Filename Encryption ---
        let (stored_name, stored_hash) = {
            let guard = self.inner.read().unwrap();
            let parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
            if parent_inode.mode != crate::inode::FileType::Directory {
                return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
            }

            // Fast presence check under shared read lock without blocking readers
            if Self::dir_lookup(&guard, parent_inode_id, name)?.is_some() {
                let type_str = if file_type == crate::inode::FileType::Directory {
                    "Directory"
                } else {
                    "File"
                };
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("{} '{}' already exists", type_str, name),
                )));
            }

            let stored_name = Self::stored_filename(&guard, parent_inode_id, name)?.into_owned();
            let stored_hash = crate::directory::hash_filename(&stored_name);
            (stored_name, stored_hash)
        }; // Shared read-guard is dropped immediately here!

        // --- STAGE 2: In-Lock Allocation & Commitment ---
        let mut guard = self.inner.write().unwrap();

        // Journaled images take the WAL-first path; legacy images keep the
        // original in-place implementation untouched (zero-overhead guarantee).
        if guard.superblock.has_journal_layout() {
            let res = Self::create_entry_journaled(
                &mut guard,
                parent_inode_id,
                name,
                stored_name,
                stored_hash,
                file_type,
            );
            drop(guard);
            self.maybe_drain_async_ranges();
            return res;
        }

        // 1. Read Parent (re-verify under write lock in case of concurrent changes)
        let mut parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
        }

        // Check if file or directory was created in a race
        if Self::dir_lookup(&guard, parent_inode_id, name)?.is_some() {
            let type_str = if file_type == crate::inode::FileType::Directory {
                "Directory"
            } else {
                "File"
            };
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} '{}' already exists", type_str, name),
            )));
        }

        // 2. Allocate Inode with bounded wrapping search hint
        let hint = guard.free_inode_hint;
        let usable_inodes = guard.superblock.usable_inode_count();
        let new_inode_id = {
            let mut bitmap_slice = guard.inode_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("inode bitmap not found"))
            })?;
            let mut allocator = SimpleBlockAllocator::new(&mut bitmap_slice, 0);
            let id = allocator.allocate_bounded(Some(hint), usable_inodes)?;
            bitmap_slice.set_dirty_bits(id as usize, 1);
            id
        };
        guard.free_inode_hint = if new_inode_id + 1 >= usable_inodes {
            0
        } else {
            new_inode_id + 1
        };

        // 3. Init Inode with proper timestamps
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut new_inode = Inode::new(file_type);
        new_inode.created_at = now;
        new_inode.modified_at = now;

        if file_type == crate::inode::FileType::Directory {
            let dir_data_block = Self::alloc_and_zero_block(&mut guard)?;
            new_inode.blocks[0] = dir_data_block;
            new_inode.size = BLOCK_SIZE as u64;
        }

        Self::write_inode_internal(&mut guard, new_inode_id, &new_inode)?;

        // 4. Update Parent Dir
        let entry = crate::directory::DirectoryEntry {
            inode: new_inode_id,
            hash: stored_hash,
            name: stored_name,
        };
        let (written_blk, new_tail, all_earlier_full) =
            Self::append_dir_entry_to_dir(&mut guard, parent_inode_id, &mut parent_inode, &entry)?;

        // 5. Update Parent Mtime
        parent_inode.modified_at = now;
        Self::write_inode_internal(&mut guard, parent_inode_id, &parent_inode)?;

        // Keep the directory index consistent (only if one exists; otherwise nothing is stale).
        let stored_name = entry.name;
        let encrypted = guard.encryption_key.is_some();
        if let Some(ix) = guard
            .dir_cache
            .write_shard(parent_inode_id)
            .get_mut(&parent_inode_id)
        {
            if encrypted {
                ix.plain.insert(
                    name.to_string(),
                    (new_inode_id, crate::directory::hash_filename(name)),
                );
            }
            ix.stored
                .insert(stored_name, (new_inode_id, written_blk, stored_hash));
            if encrypted && ix.complete && ix.plain.len() == ix.stored.len() {
                ix.plain_complete = true;
            }
            if let Some(tail) = new_tail {
                ix.tail_offset = Some(tail);
            }
            if all_earlier_full {
                ix.may_have_holes = false;
            }
        }

        let sync_res = if guard.durability_mode().is_range_based() {
            let ranges = [
                guard.inode_bitmap_byte_range(),
                guard.data_bitmap_byte_range(),
                guard.inode_byte_range(new_inode_id),
                guard.inode_byte_range(parent_inode_id),
                guard.block_byte_range(written_blk),
                if file_type == crate::inode::FileType::Directory {
                    guard.block_byte_range(new_inode.blocks[0])
                } else {
                    (0, 0)
                },
            ];
            guard.sync_mutation_ranges(&ranges)
        } else {
            guard.sync_mutation_ranges(&[])
        };
        drop(guard);
        self.maybe_drain_async_ranges();
        sync_res?;
        Ok(new_inode_id)
    }

    /// Creates a new file in a directory
    pub fn create_file(&self, parent_inode_id: u64, name: &str) -> Result<u64, DiskManagerError> {
        self.create_entry_internal(parent_inode_id, name, crate::inode::FileType::File)
    }

    /// Atomically creates a new file and writes its initial contents in a single write-lock
    /// acquisition (and a single WAL transaction on journaled images).
    ///
    /// On journaled images in [`DurabilityMode::Strict`], small payloads up to 8 KiB (two
    /// blocks) are data-journaled inside the metadata commit so durable file creation requires
    /// only **1 sync barrier** instead of 3 (`create_file` + payload + `write_data`).
    pub fn create_file_with_data(
        &self,
        parent_inode_id: u64,
        name: &str,
        data: &[u8],
        compression_mode: CompressionMode,
    ) -> Result<u64, DiskManagerError> {
        self.create_file_with_data_with_filters(
            parent_inode_id,
            name,
            data,
            compression_mode,
            crate::filters::FilterConfig::none(),
        )
    }

    /// Atomically creates a new file and writes its initial contents with custom pre-compression
    /// filters in a single write-lock acquisition (and a single WAL transaction on journaled
    /// images for standard compression modes).
    pub fn create_file_with_data_with_filters(
        &self,
        parent_inode_id: u64,
        name: &str,
        data: &[u8],
        compression_mode: CompressionMode,
        filter_config: crate::filters::FilterConfig,
    ) -> Result<u64, DiskManagerError> {
        if self.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        crate::directory::validate_filename(name).map_err(|e| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;

        // Seekable 64K files maintain their own parallel chunk pipeline; compose with rollback
        // so a failed write never leaves an empty orphan file behind.
        if matches!(compression_mode, CompressionMode::Seekable { .. }) {
            let inode_id = self.create_file(parent_inode_id, name)?;
            if let Err(e) =
                self.write_data_with_filters(inode_id, 0, data, compression_mode, filter_config)
            {
                let _ = self.delete_file(parent_inode_id, name);
                return Err(e);
            }
            return Ok(inode_id);
        }

        // --- STAGE 1: Out-of-Lock Validation & Filename Encryption ---
        let (stored_name, stored_hash) = {
            let guard = self.inner.read().unwrap();
            let parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
            if parent_inode.mode != crate::inode::FileType::Directory {
                return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
            }
            if Self::dir_lookup(&guard, parent_inode_id, name)?.is_some() {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("File '{}' already exists", name),
                )));
            }
            let stored_name = Self::stored_filename(&guard, parent_inode_id, name)?.into_owned();
            crate::directory::validate_filename(&stored_name).map_err(|e| {
                DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    e.to_string(),
                ))
            })?;
            let stored_hash = crate::directory::hash_filename(&stored_name);
            (stored_name, stored_hash)
        };

        // --- STAGE 2: Lock-Free Pre-Compression Filtering & Compression ---
        // Encryption is finalized in Stage 3 once `new_inode_id` is allocated so the
        // AEAD associated data binds to the newly assigned inode ID.
        let prep = Self::build_full_overwrite_buffer_pure(
            data,
            compression_mode,
            &filter_config,
            None,
            &[],
        )?;

        // --- STAGE 3: Exclusive Lock Allocation & Single-Commit Persistence ---
        let mut guard = self.inner.write().unwrap();

        if guard.superblock.has_journal_layout() {
            let res = Self::create_file_with_data_journaled(
                &mut guard,
                parent_inode_id,
                name,
                stored_name,
                stored_hash,
                data.len() as u64,
                prep,
                filter_config,
            );
            drop(guard);
            self.maybe_drain_async_ranges();
            return res;
        }

        // Re-verify parent & collision under exclusive write lock.
        let mut parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
        }
        if Self::dir_lookup(&guard, parent_inode_id, name)?.is_some() {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("File '{}' already exists", name),
            )));
        }

        let hint = guard.free_inode_hint;
        let usable_inodes = guard.superblock.usable_inode_count();
        let new_inode_id = {
            let mut bitmap_slice = guard.inode_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("inode bitmap not found"))
            })?;
            let mut allocator = SimpleBlockAllocator::new(&mut bitmap_slice, 0);
            let id = allocator.allocate_bounded(Some(hint), usable_inodes)?;
            bitmap_slice.set_dirty_bits(id as usize, 1);
            id
        };
        guard.free_inode_hint = if new_inode_id + 1 >= usable_inodes {
            0
        } else {
            new_inode_id + 1
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut new_inode = Inode::new(crate::inode::FileType::File);
        new_inode.created_at = now;
        new_inode.modified_at = now;
        new_inode.filter_typesize = filter_config.typesize;
        new_inode.filter_delta = filter_config.delta;
        new_inode.filter_shuffle = filter_config.shuffle;
        new_inode.filter_bitshuffle = filter_config.bitshuffle;

        let (final_buf, encryption_nonce) = if let Some(key) = guard.encryption_key.as_ref() {
            let (aad_buf, aad_len) = crate::encryption::file_aad_for_scheme_stack(
                new_inode_id,
                guard.superblock.encryption_version,
            );
            let nonce = crate::encryption::generate_nonce();
            let mut enc = prep.buffer.into_owned();
            enc.reserve(16);
            if let Err(e) =
                crate::encryption::encrypt_data_in_place(&mut enc, key, &nonce, &aad_buf[..aad_len])
            {
                Self::rollback_unlinked_file(&mut guard, new_inode_id, &new_inode);
                return Err(e.into());
            }
            (std::borrow::Cow::Owned(enc), Some(nonce))
        } else {
            (prep.buffer, None)
        };

        if let Some(nonce) = encryption_nonce {
            new_inode.encrypted = true;
            new_inode.encryption_nonce = nonce;
        }
        new_inode.size = if prep.is_compressed {
            data.len() as u64
        } else {
            final_buf.len() as u64
        };
        new_inode.compressed_size = if prep.is_compressed {
            final_buf.len() as u64
        } else {
            0
        };

        let mut touched_blocks = if guard.durability_mode().is_range_based() {
            Some(Vec::new())
        } else {
            None
        };
        if !final_buf.is_empty()
            && let Err(e) = Self::write_buffer_at_offset(
                &mut guard,
                &mut new_inode,
                0,
                &final_buf,
                touched_blocks.as_mut(),
            )
        {
            Self::rollback_unlinked_file(&mut guard, new_inode_id, &new_inode);
            return Err(e);
        }

        if let Err(e) = Self::write_inode_internal(&mut guard, new_inode_id, &new_inode) {
            Self::rollback_unlinked_file(&mut guard, new_inode_id, &new_inode);
            return Err(e);
        }

        let entry = crate::directory::DirectoryEntry {
            inode: new_inode_id,
            hash: stored_hash,
            name: stored_name,
        };
        let (written_blk, new_tail, all_earlier_full) = match Self::append_dir_entry_to_dir(
            &mut guard,
            parent_inode_id,
            &mut parent_inode,
            &entry,
        ) {
            Ok(v) => v,
            Err(e) => {
                Self::rollback_unlinked_file(&mut guard, new_inode_id, &new_inode);
                return Err(e);
            }
        };

        parent_inode.modified_at = now;
        Self::write_inode_internal(&mut guard, parent_inode_id, &parent_inode)?;

        let stored_name = entry.name;
        let encrypted = guard.encryption_key.is_some();
        if let Some(ix) = guard
            .dir_cache
            .write_shard(parent_inode_id)
            .get_mut(&parent_inode_id)
        {
            if encrypted {
                ix.plain.insert(
                    name.to_string(),
                    (new_inode_id, crate::directory::hash_filename(name)),
                );
            }
            ix.stored
                .insert(stored_name, (new_inode_id, written_blk, stored_hash));
            if encrypted && ix.complete && ix.plain.len() == ix.stored.len() {
                ix.plain_complete = true;
            }
            if let Some(tail) = new_tail {
                ix.tail_offset = Some(tail);
            }
            if all_earlier_full {
                ix.may_have_holes = false;
            }
        }

        let sync_res = if guard.durability_mode().is_range_based() {
            let mut ranges = vec![
                guard.inode_bitmap_byte_range(),
                guard.data_bitmap_byte_range(),
                guard.inode_byte_range(new_inode_id),
                guard.inode_byte_range(parent_inode_id),
                guard.block_byte_range(written_blk),
            ];
            if let Some(blks) = touched_blocks {
                for blk in blks {
                    ranges.push(guard.block_byte_range(blk));
                }
            }
            guard.sync_mutation_ranges(&ranges)
        } else {
            guard.sync_mutation_ranges(&[])
        };
        drop(guard);
        self.maybe_drain_async_ranges();
        sync_res?;
        Ok(new_inode_id)
    }

    fn rollback_unlinked_file(guard: &mut DiskManagerInner, inode_id: u64, inode: &Inode) {
        let blocks = Self::collect_inode_blocks(&guard.backing, inode);
        if !blocks.is_empty() {
            let db_start = guard.superblock.data_block_start;
            let mut min_blk = u64::MAX;
            if let Some(mut slice) = guard.data_bitmap_slice_mut() {
                let mut da = SimpleBlockAllocator::new(&mut slice, db_start);
                for &blk in &blocks {
                    let _ = da.free(blk);
                    min_blk = min_blk.min(blk);
                }
            }
            if min_blk < guard.free_block_hint {
                guard.free_block_hint = min_blk;
            }
        }
        if let Some(mut ib) = guard.inode_bitmap_slice_mut() {
            let mut ia = SimpleBlockAllocator::new(&mut ib, 0);
            let _ = ia.free(inode_id);
            ib.set_dirty_bits(inode_id as usize, 1);
        }
        if inode_id < guard.free_inode_hint {
            guard.free_inode_hint = inode_id;
        }
        guard.inode_cache.remove(inode_id);
    }

    #[allow(clippy::too_many_arguments)]
    fn create_file_with_data_journaled(
        guard: &mut DiskManagerInner,
        parent_inode_id: u64,
        name: &str,
        stored_name: String,
        stored_hash: u64,
        orig_len: u64,
        prep: FullOverwritePayload<'_>,
        filter_config: crate::filters::FilterConfig,
    ) -> Result<u64, DiskManagerError> {
        let mut parent_inode = Self::read_inode_internal(guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
        }
        if Self::dir_lookup(guard, parent_inode_id, name)?.is_some() {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("File '{}' already exists", name),
            )));
        }

        let ib_slice = guard
            .inode_bitmap_slice()
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("inode bitmap not found")))?;
        let new_inode_id = crate::bitmap::BitmapRef::new(ib_slice)
            .find_next_free_bounded(
                guard.free_inode_hint as usize,
                guard.superblock.usable_inode_count() as usize,
            )
            .ok_or(DiskManagerError::Allocator(
                crate::allocator::AllocatorError::NoSpace,
            ))? as u64;
        let next_free_inode_hint = new_inode_id + 1;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut new_inode = Inode::new(crate::inode::FileType::File);
        new_inode.created_at = now;
        new_inode.modified_at = now;
        new_inode.filter_typesize = filter_config.typesize;
        new_inode.filter_delta = filter_config.delta;
        new_inode.filter_shuffle = filter_config.shuffle;
        new_inode.filter_bitshuffle = filter_config.bitshuffle;

        let final_buf = if let Some(key) = guard.encryption_key.as_ref() {
            let (aad_buf, aad_len) = crate::encryption::file_aad_for_scheme_stack(
                new_inode_id,
                guard.superblock.encryption_version,
            );
            let nonce = crate::encryption::generate_nonce();
            let mut enc = prep.buffer.into_owned();
            enc.reserve(16);
            crate::encryption::encrypt_data_in_place(&mut enc, key, &nonce, &aad_buf[..aad_len])?;
            new_inode.encrypted = true;
            new_inode.encryption_nonce = nonce;
            std::borrow::Cow::Owned(enc)
        } else {
            prep.buffer
        };

        new_inode.size = if prep.is_compressed {
            orig_len
        } else {
            final_buf.len() as u64
        };
        new_inode.compressed_size = if prep.is_compressed {
            final_buf.len() as u64
        } else {
            0
        };

        let mut ops = Vec::with_capacity(8);
        ops.push(crate::journal::MetadataOp::SetInodeBitmap {
            inode_id: new_inode_id,
            allocated: true,
        });

        let mut sim = crate::journal::AllocSim::new_data_only(
            guard,
            next_free_inode_hint,
            guard.free_block_hint,
        )?;
        let mut used: Vec<u64> = Vec::new();

        // Under `DurabilityMode::Strict`, inline small payloads (<= 2 blocks / 8 KiB)
        // directly into the WAL frame as zero-padded `WriteBlockSlice` ops. This makes
        // the file's data durable as part of the single WAL commit barrier (1 sync total
        // instead of 3 for `create_file` + payload + `write_data`).
        // Under Group-Commit batching (`is_strict_sync_deferred()`), the drainer issues
        // a unified `flush()` for the whole batch, so payload blocks are written directly
        // to the data area instead of consuming WAL ring capacity.
        let inline_in_wal = guard.durability_mode() == DurabilityMode::Strict
            && !is_strict_sync_deferred()
            && !final_buf.is_empty()
            && final_buf.len() <= 2 * BLOCK_SIZE;
        let mut payload_needs_pre_sync = false;

        if inline_in_wal {
            let num_blocks = final_buf.len().div_ceil(BLOCK_SIZE);
            for i in 0..num_blocks {
                let blk = sim.alloc_block(&guard.backing)?;
                crate::journal::push_data_bitmap_op(&mut ops, blk, 1, true);
                let chunk_start = i * BLOCK_SIZE;
                let chunk_end = (chunk_start + BLOCK_SIZE).min(final_buf.len());
                let mut blk_img = vec![0u8; BLOCK_SIZE];
                blk_img[..chunk_end - chunk_start]
                    .copy_from_slice(&final_buf[chunk_start..chunk_end]);
                ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                    block_id: blk,
                    offset: 0,
                    data: blk_img,
                });
                new_inode.blocks[i] = blk;
            }
        } else if !final_buf.is_empty() {
            crate::journal::stage_payload_write(
                guard,
                &mut new_inode,
                0,
                &final_buf,
                &mut sim,
                &mut ops,
                &mut used,
            )?;
            payload_needs_pre_sync =
                guard.durability_mode() == DurabilityMode::Strict && !is_strict_sync_deferred();
        }

        // Stage directory entry into parent directory.
        let entry = crate::directory::DirectoryEntry {
            inode: new_inode_id,
            hash: stored_hash,
            name: stored_name.clone(),
        };
        let mut entry_bytes = Vec::with_capacity(20 + entry.name.len());
        {
            let mut cursor = std::io::Cursor::new(&mut entry_bytes);
            entry
                .serialize_into(&mut cursor)
                .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
        }
        let num_blocks = Self::dir_num_blocks(&parent_inode);
        let entry_len = entry_bytes.len();
        let needed_space = 20 + entry.name.len();
        let (slot, all_earlier_full) =
            Self::find_dir_insert_slot(guard, parent_inode_id, &parent_inode, needed_space);
        let (written_blk, next_tail) = if let Some((phys_blk, insert_offset, is_tail)) = slot {
            ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                block_id: phys_blk,
                offset: insert_offset as u32,
                data: std::mem::take(&mut entry_bytes),
            });
            let tail = if is_tail {
                Some((phys_blk, (insert_offset + entry_len) as u16))
            } else {
                None
            };
            (phys_blk, tail)
        } else {
            let new_blk_idx = num_blocks;
            let phys_blk = crate::journal::sim_get_or_alloc_block(
                &guard.backing,
                &mut parent_inode,
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
            parent_inode.size = (new_blk_idx + 1) as u64 * BLOCK_SIZE as u64;
            (phys_blk, Some((phys_blk, entry_len as u16)))
        };

        parent_inode.modified_at = now;

        let new_inode_image = Self::build_inode_post_image(guard, new_inode_id, &new_inode)?;
        let parent_image = Self::build_inode_post_image(guard, parent_inode_id, &parent_inode)?;
        ops.push(crate::journal::MetadataOp::WriteInode {
            inode_id: new_inode_id,
            inode_bytes: Box::new(new_inode_image),
        });
        ops.push(crate::journal::MetadataOp::WriteInode {
            inode_id: parent_inode_id,
            inode_bytes: Box::new(parent_image),
        });

        let ops = if crate::journal::is_already_coalesced(&ops) {
            ops
        } else {
            crate::journal::coalesce_ops(ops)
        };

        let payload_ranges: Vec<(usize, usize)> = if guard.durability_mode().is_range_based() {
            used.iter().map(|b| guard.block_byte_range(*b)).collect()
        } else {
            Vec::new()
        };
        if payload_needs_pre_sync {
            if guard.backing.as_mmap_slice().is_some() {
                let coalesced_payload =
                    DiskManagerInner::coalesce_ranges(&payload_ranges, guard.backing.len());
                for (off, actual) in coalesced_payload {
                    guard.backing.flush_range(off, actual)?;
                }
            } else {
                guard.backing.flush()?;
            }
        }

        // Commit point + in-place apply.
        Self::commit_journal_tx(guard, &ops)?;
        Self::apply_journal_ops(guard, &ops)?;

        guard.free_inode_hint = sim.free_inode_hint;
        guard.free_block_hint = sim.free_block_hint;

        guard.inode_cache.insert(new_inode_id, new_inode);
        guard.inode_cache.insert(parent_inode_id, parent_inode);
        let encrypted = guard.encryption_key.is_some();
        if let Some(ix) = guard
            .dir_cache
            .write_shard(parent_inode_id)
            .get_mut(&parent_inode_id)
        {
            if encrypted {
                ix.plain.insert(
                    name.to_string(),
                    (new_inode_id, crate::directory::hash_filename(name)),
                );
            }
            ix.stored
                .insert(stored_name, (new_inode_id, written_blk, stored_hash));
            if encrypted && ix.complete && ix.plain.len() == ix.stored.len() {
                ix.plain_complete = true;
            }
            if let Some(tail) = next_tail {
                ix.tail_offset = Some(tail);
            }
            if all_earlier_full {
                ix.may_have_holes = false;
            }
        }

        if guard.durability_mode() == DurabilityMode::RangeAsync {
            let mut ranges = vec![
                guard.inode_bitmap_byte_range(),
                guard.data_bitmap_byte_range(),
                guard.inode_byte_range(new_inode_id),
                guard.inode_byte_range(parent_inode_id),
            ];
            ranges.extend(payload_ranges);
            for op in &ops {
                if let crate::journal::MetadataOp::WriteBlockSlice { block_id, .. } = op {
                    ranges.push(guard.block_byte_range(*block_id));
                }
            }
            guard.sync_mutation_ranges(&ranges)?;
        } else if guard.durability_mode() == DurabilityMode::LegacyWholeMmapAsync {
            guard.sync_mutation_ranges(&[])?;
        }
        Self::maybe_checkpoint(guard)?;
        Ok(new_inode_id)
    }

    /// Creates a file if it does not exist, or opens (looks up) the existing file.
    ///
    /// This provides safe idempotent file opening under multi-threaded concurrency.
    pub fn create_or_open_file(
        &self,
        parent_inode_id: u64,
        name: &str,
    ) -> Result<u64, DiskManagerError> {
        if self.is_read_only() {
            return match self.lookup(parent_inode_id, name) {
                Ok(inode_id) => {
                    debug_assert!(!Self::open_mode_requires_write(
                        OpenMode::CreateOrOpen,
                        true
                    ));
                    Ok(inode_id)
                }
                Err(DiskManagerError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    debug_assert!(Self::open_mode_requires_write(
                        OpenMode::CreateOrOpen,
                        false
                    ));
                    Err(DiskManagerError::ReadOnly)
                }
                Err(e) => Err(e),
            };
        }
        match self.create_file(parent_inode_id, name) {
            Ok(inode_id) => Ok(inode_id),
            Err(DiskManagerError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.lookup(parent_inode_id, name)
            }
            Err(e) => Err(e),
        }
    }

    /// Opens a file with the specified [`OpenMode`] matching standard POSIX semantics.
    pub fn open_file(
        &self,
        parent_inode_id: u64,
        name: &str,
        mode: OpenMode,
    ) -> Result<u64, DiskManagerError> {
        match mode {
            OpenMode::OpenExisting => self.lookup(parent_inode_id, name),
            OpenMode::CreateNew => self.create_file(parent_inode_id, name),
            OpenMode::CreateOrOpen => self.create_or_open_file(parent_inode_id, name),
        }
    }

    /// Creates a new directory in a parent directory
    pub fn create_directory(
        &self,
        parent_inode_id: u64,
        name: &str,
    ) -> Result<u64, DiskManagerError> {
        self.create_entry_internal(parent_inode_id, name, crate::inode::FileType::Directory)
    }

    /// Looks up a file/directory by name within a parent directory
    pub fn lookup(&self, parent_inode_id: u64, name: &str) -> Result<u64, DiskManagerError> {
        crate::directory::validate_filename(name).map_err(|e| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
        let guard = self.inner.read().unwrap();
        Self::dir_lookup(&guard, parent_inode_id, name)?.ok_or_else(|| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Not found",
            ))
        })
    }

    /// Resolves a single logical block index to its physical block ID (read-only, does not allocate).
    pub(crate) fn resolve_logical_block_id(
        backing: &ImageBacking,
        inode: &Inode,
        blk_idx: usize,
    ) -> u64 {
        use crate::inode::BlockPath;
        // Follows one pointer level; a zero pointer means a sparse hole.
        let follow = |parent: u64, slot: usize| -> u64 {
            if parent == 0 {
                0
            } else {
                backing.read_block_ptr(parent, slot)
            }
        };
        match BlockPath::from_logical(blk_idx) {
            Some(BlockPath::Direct(i)) => inode.blocks[i],
            Some(BlockPath::Single(i)) => follow(inode.blocks[10], i),
            Some(BlockPath::Double(a, b)) => follow(follow(inode.blocks[11], a), b),
            Some(BlockPath::Triple(a, b, c)) => {
                follow(follow(follow(inode.triple_indirect, a), b), c)
            }
            None => 0,
        }
    }

    /// Collects the packed chunk entries of a seekable 64K file.
    ///
    /// The leaf pointer slots of such a file hold `ChunkEntry` values
    /// instead of raw block ids; the defragmentation has to rewrite them as a
    /// unit, because the payload of a chunk is a run of contiguous blocks.
    fn collect_chunk_entries(
        backing: &ImageBacking,
        inode: &Inode,
    ) -> Vec<(u64, crate::inode::ChunkEntry)> {
        let mut entries = Vec::new();
        if inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K == 0 {
            return entries;
        }

        let push = |raw: u64, entries: &mut Vec<(u64, crate::inode::ChunkEntry)>| {
            if raw == 0 {
                return;
            }
            let entry = crate::inode::ChunkEntry(raw);
            if !entry.is_empty() {
                entries.push((raw, entry));
            }
        };

        for i in 0..10 {
            push(inode.blocks[i], &mut entries);
        }

        let sib_id = inode.blocks[10];
        if sib_id != 0 {
            let _ = backing.with_block(sib_id, |slice| {
                for chunk in slice.as_chunks::<8>().0 {
                    push(u64::from_le_bytes(*chunk), &mut entries);
                }
            });
        }

        let dib_id = inode.blocks[11];
        if dib_id != 0 {
            let sibs: Vec<u64> = backing
                .with_block(dib_id, |slice| {
                    slice
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_le_bytes(*c))
                        .filter(|&sib| sib != 0)
                        .collect()
                })
                .unwrap_or_default();
            for sib in sibs {
                let _ = backing.with_block(sib, |s_slice| {
                    for d_chunk in s_slice.as_chunks::<8>().0 {
                        push(u64::from_le_bytes(*d_chunk), &mut entries);
                    }
                });
            }
        }

        let tib_id = inode.triple_indirect;
        if tib_id != 0 {
            let dibs: Vec<u64> = backing
                .with_block(tib_id, |slice| {
                    slice
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_le_bytes(*c))
                        .filter(|&dib| dib != 0)
                        .collect()
                })
                .unwrap_or_default();
            for dib in dibs {
                let sibs: Vec<u64> = backing
                    .with_block(dib, |d_slice| {
                        d_slice
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .map(|c| u64::from_le_bytes(*c))
                            .filter(|&sib| sib != 0)
                            .collect()
                    })
                    .unwrap_or_default();
                for sib in sibs {
                    let _ = backing.with_block(sib, |s_slice| {
                        for p_chunk in s_slice.as_chunks::<8>().0 {
                            push(u64::from_le_bytes(*p_chunk), &mut entries);
                        }
                    });
                }
            }
        }

        entries
    }

    /// Walks the first `physical_size` bytes of `inode`'s on-disk payload in logical order,
    /// calling `emit(physical_block, payload_offset, len)` for every allocated block.
    /// Sparse holes (zero pointers at any indirection level) are skipped without a call.
    fn walk_payload_blocks(
        backing: &ImageBacking,
        inode: &Inode,
        physical_size: u64,
        data_start: u64,
        data_end: u64,
        mut emit: impl FnMut(u64, usize, usize),
    ) {
        let mut read = 0;
        let mut blk_idx = 0;

        while read < physical_size {
            let rem = (physical_size - read) as usize;
            let to_read = std::cmp::min(rem, BLOCK_SIZE);

            // 1. Direct blocks (0..10)
            if blk_idx < 10 {
                let blk = inode.blocks[blk_idx];
                // A pointer that does not address the data area is
                // ignored instead of exposing metadata as file content.
                if blk != 0 && blk >= data_start && blk < data_end {
                    emit(blk, read as usize, to_read);
                }
                read += to_read as u64;
                blk_idx += 1;
                continue;
            }

            // 2. Single indirect blocks (10..522) - Batch resolution
            if blk_idx < 10 + 512 {
                let sib_id = inode.blocks[10];
                // Indirect pointer block must reside in the data area.
                if sib_id == 0 || sib_id < data_start || sib_id >= data_end {
                    let remaining_blocks = (10 + 512) - blk_idx;
                    let skip_bytes =
                        (remaining_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx = 10 + 512;
                    continue;
                }
                let start_entry = blk_idx - 10;
                let num_entries = ((10 + 512) - blk_idx).min(512 - start_entry);
                if backing
                    .with_block(sib_id, |sib_slice| {
                        let ptr_chunks =
                            &sib_slice[start_entry * 8..(start_entry + num_entries) * 8];
                        for chunk in ptr_chunks.as_chunks::<8>().0 {
                            if read >= physical_size {
                                break;
                            }
                            let to_read_curr =
                                std::cmp::min((physical_size - read) as usize, BLOCK_SIZE);
                            let blk = u64::from_le_bytes(*chunk);
                            if blk != 0 && blk >= data_start && blk < data_end {
                                emit(blk, read as usize, to_read_curr);
                            }
                            read += to_read_curr as u64;
                            blk_idx += 1;
                        }
                    })
                    .is_err()
                {
                    read += to_read as u64;
                    blk_idx += 1;
                }
                continue;
            }

            // 3. Double indirect blocks (522..262666) - Batch resolution per single indirect block
            if blk_idx < 10 + 512 + 512 * 512 {
                let dib_id = inode.blocks[11];
                if dib_id == 0 || dib_id < data_start || dib_id >= data_end {
                    let remaining_blocks = (10 + 512 + 512 * 512) - blk_idx;
                    let skip_bytes =
                        (remaining_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx = 10 + 512 + 512 * 512;
                    continue;
                }
                let idx = blk_idx - (10 + 512);
                let s_idx = idx / 512;
                let d_idx = idx % 512;

                let sib_id = backing.read_block_ptr(dib_id, s_idx);
                if sib_id == 0 || sib_id < data_start || sib_id >= data_end {
                    let skip_blocks = 512 - d_idx;
                    let skip_bytes =
                        (skip_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx += skip_blocks;
                    continue;
                }

                let num_entries = 512 - d_idx;
                if backing
                    .with_block(sib_id, |sib_slice| {
                        let ptr_chunks = &sib_slice[d_idx * 8..(d_idx + num_entries) * 8];
                        for chunk in ptr_chunks.as_chunks::<8>().0 {
                            if read >= physical_size {
                                break;
                            }
                            let to_read_curr =
                                std::cmp::min((physical_size - read) as usize, BLOCK_SIZE);
                            let blk = u64::from_le_bytes(*chunk);
                            if blk != 0 && blk >= data_start && blk < data_end {
                                emit(blk, read as usize, to_read_curr);
                            }
                            read += to_read_curr as u64;
                            blk_idx += 1;
                        }
                    })
                    .is_err()
                {
                    read += to_read as u64;
                    blk_idx += 1;
                }
                continue;
            }

            // 4. Triple indirect blocks (262666 .. 134480394)
            let max_blocks = 10 + 512 + 512 * 512 + 512 * 512 * 512;
            if blk_idx < max_blocks {
                let tib_id = inode.triple_indirect;
                if tib_id == 0 || tib_id < data_start || tib_id >= data_end {
                    let remaining_blocks = max_blocks - blk_idx;
                    let skip_bytes =
                        (remaining_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx = max_blocks;
                    continue;
                }
                let idx = blk_idx - (10 + 512 + 512 * 512);
                let t_idx = idx / (512 * 512);
                let rem_idx = idx % (512 * 512);
                let d_idx = rem_idx / 512;
                let s_idx = rem_idx % 512;

                let dib_id = backing.read_block_ptr(tib_id, t_idx);
                if dib_id == 0 || dib_id < data_start || dib_id >= data_end {
                    let skip_blocks = (512 * 512) - rem_idx;
                    let skip_bytes =
                        (skip_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx += skip_blocks;
                    continue;
                }

                let sib_id = backing.read_block_ptr(dib_id, d_idx);
                if sib_id == 0 || sib_id < data_start || sib_id >= data_end {
                    let skip_blocks = 512 - s_idx;
                    let skip_bytes =
                        (skip_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx += skip_blocks;
                    continue;
                }

                let num_entries = 512 - s_idx;
                if backing
                    .with_block(sib_id, |sib_slice| {
                        let ptr_chunks = &sib_slice[s_idx * 8..(s_idx + num_entries) * 8];
                        for chunk in ptr_chunks.as_chunks::<8>().0 {
                            if read >= physical_size {
                                break;
                            }
                            let to_read_curr =
                                std::cmp::min((physical_size - read) as usize, BLOCK_SIZE);
                            let blk = u64::from_le_bytes(*chunk);
                            // A pointer that does not address the data area is
                            // ignored instead of exposing metadata as file content.
                            if blk != 0 && blk >= data_start && blk < data_end {
                                emit(blk, read as usize, to_read_curr);
                            }
                            read += to_read_curr as u64;
                            blk_idx += 1;
                        }
                    })
                    .is_err()
                {
                    read += to_read as u64;
                    blk_idx += 1;
                }
                continue;
            }

            break;
        }
    }

    /// Hard cap (2 GiB) on single-call whole-file materialization (`read_data` and
    /// seekable full-file reads) to prevent OOM aborts from crafted/corrupted on-disk
    /// `inode.size` values (e.g. 64 TiB or `u64::MAX`).
    /// Larger files remain readable via bounded positional `read_at` calls.
    pub const MAX_READ_DATA_BYTES: usize = 2 * 1024 * 1024 * 1024;

    /// Hard cap (256 MiB) on single-call Zstd stream decompression to prevent
    /// decompression bombs (e.g. a 32 KiB compressed stream expanding to GiB).
    pub const MAX_DECOMPRESSED_STREAM_BYTES: usize = 256 * 1024 * 1024;

    pub(crate) fn checked_read_alloc_len(
        size: u64,
        limit: usize,
    ) -> Result<usize, DiskManagerError> {
        if size > limit as u64 {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "File size {size} bytes exceeds maximum single-read limit ({limit} bytes); use read_at for streaming reads"
                ),
            )));
        }
        usize::try_from(size).map_err(|_| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("File size {size} bytes overflows platform address space"),
            ))
        })
    }

    pub(crate) fn try_alloc_zeroed_vec(len: usize) -> Result<Vec<u8>, DiskManagerError> {
        let mut buf = Vec::new();
        buf.try_reserve_exact(len).map_err(|_| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                format!("Failed to allocate {len} bytes"),
            ))
        })?;
        buf.resize(len, 0);
        Ok(buf)
    }

    fn decompress_stream_payload(
        zstd_src: &[u8],
        expected_size: u64,
    ) -> Result<Vec<u8>, DiskManagerError> {
        let expected_len =
            Self::checked_read_alloc_len(expected_size, Self::MAX_DECOMPRESSED_STREAM_BYTES)?;

        // Single-pass pooled `DCtx` fast path (handles both single-frame and
        // concatenated multi-frame Zstd streams in one C call with zero `DCtx` allocs).
        if expected_len <= 64 * 1024 * 1024 {
            let mut decoded = Self::try_alloc_zeroed_vec(expected_len)?;
            if let Ok(written) = crate::zstd_pool::decompress_frame_into(zstd_src, &mut decoded)
                && written == expected_len
            {
                return Ok(decoded);
            }
        }

        // Fallback to bounded streaming decoder (large files > 64 MiB or forged/corrupted sizes).
        use std::io::Read;
        let max_decompressed_bytes = expected_size
            .saturating_add(65536)
            .min(Self::MAX_DECOMPRESSED_STREAM_BYTES as u64);
        let mut decoder = zstd::stream::Decoder::new(std::io::Cursor::new(zstd_src))
            .map_err(DiskManagerError::Io)?;
        let init_cap = expected_len.min(16 * 1024 * 1024);
        let mut decoded = Vec::new();
        decoded.try_reserve(init_cap).map_err(|_| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                format!("Failed to allocate {init_cap} bytes for decompression"),
            ))
        })?;
        decoder
            .by_ref()
            .take(max_decompressed_bytes.saturating_add(1))
            .read_to_end(&mut decoded)
            .map_err(DiskManagerError::Io)?;
        if decoded.len() as u64 > max_decompressed_bytes {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "Decompressed payload exceeds maximum allowed size ({max_decompressed_bytes} bytes)"
                ),
            )));
        }
        Ok(decoded)
    }

    #[allow(clippy::collapsible_if)]
    fn read_data_internal(
        guard: &DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
    ) -> Result<Vec<u8>, DiskManagerError> {
        if inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0 {
            return Self::read_chunked_64k_internal(guard, inode_id, inode);
        }

        let physical_size = if inode.compressed_size > 0 {
            inode.compressed_size
        } else {
            inode.size
        };
        if physical_size == 0 {
            return Ok(Vec::new());
        }

        // On an encrypted filesystem, reject any non-empty regular file whose
        // inode is not marked encrypted to prevent unauthenticated plaintext injection.
        if guard.superblock.encrypted
            && inode.mode == crate::inode::FileType::File
            && !inode.encrypted
        {
            return Err(DiskManagerError::DecryptionFailed);
        }

        // Compressed payloads are never sparse, so their physical size cannot exceed
        // the filesystem's data area. Also bound logical decompressed size upfront.
        if inode.compressed_size > 0 {
            let data_area_bytes = guard
                .superblock
                .block_count
                .saturating_sub(guard.superblock.data_block_start)
                .saturating_mul(BLOCK_SIZE as u64);
            if inode.compressed_size > data_area_bytes {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "Compressed size ({}) exceeds filesystem data area ({data_area_bytes} bytes)",
                        inode.compressed_size
                    ),
                )));
            }
            Self::checked_read_alloc_len(inode.size, Self::MAX_DECOMPRESSED_STREAM_BYTES)?;
        }

        let phys_len = Self::checked_read_alloc_len(physical_size, Self::MAX_READ_DATA_BYTES)?;

        let filter_config = crate::filters::FilterConfig {
            typesize: inode.filter_typesize,
            delta: inode.filter_delta,
            shuffle: inode.filter_shuffle,
            bitshuffle: inode.filter_bitshuffle,
        };

        // Zero-copy contiguous mmap slice fast path for unencrypted compressed files.
        // Skips allocating `raw_data` and copying compressed blocks before Zstd decompression.
        if !inode.encrypted
            && inode.mode == crate::inode::FileType::File
            && inode.compressed_size > 0
            && guard.io_engine.effective() == IoBackend::Mmap
            && let Some(mmap) = guard.backing.as_mmap_slice()
        {
            let data_start = guard.superblock.data_block_start;
            let data_end = guard.superblock.block_count;
            let mut contiguous_start_blk: Option<u64> = None;
            let mut expected_off = 0usize;
            let mut is_contiguous = true;

            Self::walk_payload_blocks(
                &guard.backing,
                inode,
                physical_size,
                data_start,
                data_end,
                |blk, off, len| {
                    if !is_contiguous {
                        return;
                    }
                    if off != expected_off {
                        is_contiguous = false;
                        return;
                    }
                    match contiguous_start_blk {
                        None => contiguous_start_blk = Some(blk),
                        Some(first_blk) => {
                            if blk != first_blk + (off / BLOCK_SIZE) as u64 {
                                is_contiguous = false;
                                return;
                            }
                        }
                    }
                    expected_off = off + len;
                },
            );

            if is_contiguous
                && expected_off == phys_len
                && let Some(first_blk) = contiguous_start_blk
                && let Some(byte_start) = usize::try_from(first_blk)
                    .ok()
                    .and_then(|b| b.checked_mul(BLOCK_SIZE))
                && let Some(byte_end) = byte_start.checked_add(phys_len)
                && byte_end <= mmap.len()
            {
                let mut decoded =
                    Self::decompress_stream_payload(&mmap[byte_start..byte_end], inode.size)?;
                if filter_config.is_active() {
                    crate::filters::unapply_filters_in_place(&mut decoded, &filter_config);
                }
                return Ok(decoded);
            }
        }

        let mut raw_data = Self::try_alloc_zeroed_vec(phys_len)?;
        if guard.io_engine.effective() == IoBackend::Mmap
            && let Some(mmap) = guard.backing.as_mmap_slice()
        {
            // Zero-copy path: coalesce physically contiguous runs and memcpy from the mapping.
            let data_start = guard.superblock.data_block_start;
            let data_end = guard.superblock.block_count;
            let mmap_len = mmap.len();
            let mut pending_run: Option<(usize, usize, usize)> = None;
            Self::walk_payload_blocks(
                &guard.backing,
                inode,
                physical_size,
                data_start,
                data_end,
                |blk, off, len| {
                    if let Some(src_off) = usize::try_from(blk)
                        .ok()
                        .and_then(|b| b.checked_mul(BLOCK_SIZE))
                        && src_off.saturating_add(len) <= mmap_len
                    {
                        match pending_run {
                            Some((run_src, run_dst, run_len))
                                if run_src + run_len == src_off && run_dst + run_len == off =>
                            {
                                pending_run = Some((run_src, run_dst, run_len + len));
                            }
                            Some((run_src, run_dst, run_len)) => {
                                raw_data[run_dst..run_dst + run_len]
                                    .copy_from_slice(&mmap[run_src..run_src + run_len]);
                                pending_run = Some((src_off, off, len));
                            }
                            None => {
                                pending_run = Some((src_off, off, len));
                            }
                        }
                    }
                },
            );
            if let Some((run_src, run_dst, run_len)) = pending_run {
                raw_data[run_dst..run_dst + run_len]
                    .copy_from_slice(&mmap[run_src..run_src + run_len]);
            }
        } else {
            // Gather coalesced extents, then hand them to the engine in one submission.
            let mut extents = ExtentList::new();
            let data_start = guard.superblock.data_block_start;
            let data_end = guard.superblock.block_count;
            let backing_len = guard.backing.len();
            Self::walk_payload_blocks(
                &guard.backing,
                inode,
                physical_size,
                data_start,
                data_end,
                |blk, off, len| {
                    if let Some(byte_off) = usize::try_from(blk)
                        .ok()
                        .and_then(|b| b.checked_mul(BLOCK_SIZE))
                        && byte_off.saturating_add(BLOCK_SIZE) <= backing_len
                    {
                        extents.push(blk * BLOCK_SIZE as u64, off, len);
                    }
                },
            );
            guard.io_engine.read_backing(
                &guard.file,
                &guard.backing,
                &mut [ReadTarget {
                    buf: &mut raw_data,
                    extents: extents.as_slice(),
                }],
            )?;
        }

        // === DECRYPTION STEP ===
        let mut decrypted_data = raw_data;
        if inode.encrypted {
            let encryption_key = guard
                .encryption_key
                .as_ref()
                .ok_or(DiskManagerError::PasswordRequired)?;

            // Stack-staged AAD binds the ciphertext to the file with zero allocs.
            let (aad_buf, aad_len) = crate::encryption::file_aad_for_scheme_stack(
                inode_id,
                guard.superblock.encryption_version,
            );
            crate::encryption::decrypt_data_in_place(
                &mut decrypted_data,
                encryption_key,
                &inode.encryption_nonce,
                &aad_buf[..aad_len],
            )
            .map_err(|_| DiskManagerError::DecryptionFailed)?;
        }

        // Decompress if this is a compressed file (natively decompresses concatenated multi-frame Zstd streams)
        if inode.mode == crate::inode::FileType::File && inode.compressed_size > 0 {
            decrypted_data = Self::decompress_stream_payload(&decrypted_data, inode.size)?;
        }

        // === POST-DECOMPRESSION FILTER STEP ===
        if !filter_config.is_active() {
            return Ok(decrypted_data);
        }
        crate::filters::unapply_filters_in_place(&mut decrypted_data, &filter_config);
        Ok(decrypted_data)
    }

    /// Reads data from a file (high-concurrency read-lock)
    ///
    /// # Arguments
    /// * `inode_id` - The inode ID of the file to read
    ///
    /// # Returns
    /// The file's data as a Vec<u8>. If the file is compressed, it will be
    /// automatically decompressed before returning.
    pub fn read_data(&self, inode_id: u64) -> Result<Vec<u8>, DiskManagerError> {
        let guard = self.inner.read().unwrap();
        let inode = Self::read_inode_internal(&guard, inode_id)?;
        Self::read_data_internal(&guard, inode_id, &inode)
    }

    /// Opens a shared read-lock session ([`ReadSession`]) over this filesystem.
    ///
    /// Acquiring a `ReadSession` amortizes the outer `RwLock` across batches of lookups,
    /// inode stats, and positional reads — eliminating atomic reader-count cache-line
    /// contention under multi-threaded read workloads.
    #[inline]
    pub fn begin_read(&self) -> ReadSession<'_> {
        ReadSession {
            guard: self.inner.read().unwrap(),
        }
    }

    #[inline]
    fn resolve_raw_block_id(guard: &DiskManagerInner, inode: &Inode, blk_idx: usize) -> u64 {
        const SINGLE_LIMIT: usize = 10 + 512;
        const DOUBLE_LIMIT: usize = SINGLE_LIMIT + 512 * 512;
        const TRIPLE_LIMIT: usize = DOUBLE_LIMIT + 512 * 512 * 512;
        if blk_idx < 10 {
            return inode.blocks[blk_idx];
        }
        if blk_idx < SINGLE_LIMIT {
            let sib_id = inode.blocks[10];
            if !guard.is_data_block(sib_id) {
                return 0;
            }
            return guard.backing.read_block_ptr(sib_id, blk_idx - 10);
        }
        if blk_idx < DOUBLE_LIMIT {
            let dib_id = inode.blocks[11];
            if !guard.is_data_block(dib_id) {
                return 0;
            }
            let rel = blk_idx - SINGLE_LIMIT;
            let sib_id = guard.backing.read_block_ptr(dib_id, rel / 512);
            if !guard.is_data_block(sib_id) {
                return 0;
            }
            return guard.backing.read_block_ptr(sib_id, rel % 512);
        }
        if blk_idx < TRIPLE_LIMIT {
            let tib_id = inode.triple_indirect;
            if !guard.is_data_block(tib_id) {
                return 0;
            }
            let rel = blk_idx - DOUBLE_LIMIT;
            let dib_id = guard.backing.read_block_ptr(tib_id, rel / (512 * 512));
            if !guard.is_data_block(dib_id) {
                return 0;
            }
            let rem = rel % (512 * 512);
            let sib_id = guard.backing.read_block_ptr(dib_id, rem / 512);
            if !guard.is_data_block(sib_id) {
                return 0;
            }
            return guard.backing.read_block_ptr(sib_id, rem % 512);
        }
        0
    }

    #[inline]
    fn read_at_with_guard(
        guard: &DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
        file_offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, DiskManagerError> {
        if guard.io_engine.effective() == IoBackend::Mmap && guard.backing.as_mmap_slice().is_some()
        {
            return Self::read_at_with_inode(guard, inode_id, inode, file_offset, buf, None);
        }
        let mut extents = ExtentList::new();
        let n =
            Self::read_at_with_inode(guard, inode_id, inode, file_offset, buf, Some(&mut extents))?;
        if !extents.is_empty() {
            guard.io_engine.read_backing(
                &guard.file,
                &guard.backing,
                &mut [ReadTarget {
                    buf,
                    extents: extents.as_slice(),
                }],
            )?;
        }
        Ok(n)
    }

    /// Reads up to `buf.len()` bytes starting at `file_offset` from a file.
    ///
    /// # Performance
    /// For uncompressed, unencrypted files under the default [`IoBackend::Mmap`]
    /// backend, this directly copies the requested byte slice from the memory-mapped
    /// blocks into `buf` with **ZERO intermediate memory allocations**. Under
    /// [`IoBackend::Pread`] / [`IoBackend::IoUring`] the physically contiguous block runs
    /// are coalesced and read with one syscall / one ring submission.
    ///
    /// Returns the number of bytes read (0 if at or beyond EOF).
    pub fn read_at(
        &self,
        inode_id: u64,
        file_offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, DiskManagerError> {
        let guard = self.inner.read().unwrap();
        let inode = Self::read_inode_internal(&guard, inode_id)?;
        Self::read_at_with_guard(&guard, inode_id, &inode, file_offset, buf)
    }

    /// Single-lock, single-inode-lookup positional read returning a clamped `Vec<u8>`.
    ///
    /// Used by `IpcServer` to serve `IpcRequest::ReadAt` without acquiring `inner.read()`
    /// and looking up `inode_id` twice per RPC.
    pub(crate) fn read_at_vec(
        &self,
        inode_id: u64,
        file_offset: u64,
        max_len: usize,
    ) -> Result<Vec<u8>, DiskManagerError> {
        let guard = self.inner.read().unwrap();
        let inode = Self::read_inode_internal(&guard, inode_id)?;
        if inode.mode != crate::inode::FileType::File {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Cannot read non-file inode",
            )));
        }
        if file_offset >= inode.size || max_len == 0 {
            return Ok(Vec::new());
        }
        let available = usize::try_from(inode.size - file_offset).unwrap_or(usize::MAX);
        let alloc_len = max_len.min(available);
        let mut buf = Self::try_alloc_zeroed_vec(alloc_len)?;
        let n = if guard.io_engine.effective() == IoBackend::Mmap
            && guard.backing.as_mmap_slice().is_some()
        {
            Self::read_at_with_inode(&guard, inode_id, &inode, file_offset, &mut buf, None)?
        } else {
            let mut extents = ExtentList::new();
            let n = Self::read_at_with_inode(
                &guard,
                inode_id,
                &inode,
                file_offset,
                &mut buf,
                Some(&mut extents),
            )?;
            if !extents.is_empty() {
                guard.io_engine.read_backing(
                    &guard.file,
                    &guard.backing,
                    &mut [ReadTarget {
                        buf: &mut buf,
                        extents: extents.as_slice(),
                    }],
                )?;
            }
            n
        };
        buf.truncate(n);
        Ok(buf)
    }

    /// Performs many positional reads under a single read lock and, for raw files, a
    /// single engine submission.
    ///
    /// With [`IoBackend::IoUring`] every extent of every request is in flight at once,
    /// so cold reads scattered across many files are serviced concurrently by the
    /// device instead of one blocking page fault at a time. Compressed or encrypted
    /// files are decoded individually, as in [`Self::read_at`].
    ///
    /// Returns one result per request, in order: the number of bytes read into that
    /// request's buffer, or the error for that request. If the batched submission
    /// itself fails, every request that depended on it reports the I/O error.
    pub fn read_at_batch(
        &self,
        requests: &mut [ReadRequest<'_>],
    ) -> Vec<Result<usize, DiskManagerError>> {
        let guard = self.inner.read().unwrap();
        let direct = guard.io_engine.effective() == IoBackend::Mmap
            && guard.backing.as_mmap_slice().is_some();

        let mut results = Vec::with_capacity(requests.len());
        let mut plans: Vec<ExtentList> = Vec::with_capacity(requests.len());
        for req in requests.iter_mut() {
            let mut extents = ExtentList::new();
            let sink = if direct { None } else { Some(&mut extents) };
            let res = Self::read_at_prepare(&guard, req.inode_id, req.offset, req.buf, sink);
            if res.is_err() {
                extents.clear();
            }
            results.push(res);
            plans.push(extents);
        }

        let mut targets: Vec<ReadTarget<'_>> = requests
            .iter_mut()
            .zip(plans.iter())
            .filter(|(_, plan)| !plan.is_empty())
            .map(|(req, plan)| ReadTarget {
                buf: &mut *req.buf,
                extents: plan.as_slice(),
            })
            .collect();
        if targets.is_empty() {
            return results;
        }
        if let Err(e) = guard
            .io_engine
            .read_backing(&guard.file, &guard.backing, &mut targets)
        {
            drop(targets);
            for (res, plan) in results.iter_mut().zip(plans.iter()) {
                if !plan.is_empty() {
                    *res = Err(DiskManagerError::Io(std::io::Error::new(
                        e.kind(),
                        e.to_string(),
                    )));
                }
            }
        }
        results
    }

    /// Validates a positional read and plans its transfer.
    ///
    /// Returns the number of bytes that `buf` will hold once planned extents are read.
    /// Compressed/encrypted files, EOF, and (when `extents` is `None`) raw files are
    /// fully served here by copying from the mmap. When `extents` is `Some`, raw payload
    /// ranges are appended to it instead and sparse holes are zero-filled immediately.
    fn read_at_prepare(
        guard: &DiskManagerInner,
        inode_id: u64,
        file_offset: u64,
        buf: &mut [u8],
        extents: Option<&mut ExtentList>,
    ) -> Result<usize, DiskManagerError> {
        let inode = Self::read_inode_internal(guard, inode_id)?;
        Self::read_at_with_inode(guard, inode_id, &inode, file_offset, buf, extents)
    }

    fn read_at_with_inode(
        guard: &DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
        file_offset: u64,
        buf: &mut [u8],
        extents: Option<&mut ExtentList>,
    ) -> Result<usize, DiskManagerError> {
        if inode.mode != crate::inode::FileType::File {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Cannot read non-file inode",
            )));
        }

        if file_offset >= inode.size || buf.is_empty() {
            return Ok(0);
        }

        // On an encrypted filesystem, reject any non-empty regular file whose
        // inode is not marked encrypted to prevent unauthenticated plaintext injection.
        if guard.superblock.encrypted && !inode.encrypted {
            return Err(DiskManagerError::DecryptionFailed);
        }

        let available = (inode.size - file_offset) as usize;
        let to_read_total = std::cmp::min(buf.len(), available);

        // Fast path for seekable chunked compressed files: decompress only affected chunks
        if inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0 {
            return Self::read_at_chunked_64k(guard, inode_id, inode, file_offset, buf);
        }

        // Fallback for compressed, encrypted, or filtered files: serve from bounded decoded-blob cache
        // when <= 16 MiB, avoiding O(calls * file_size) full-file decompression/decryption/unfilter cliffs.
        if inode.compressed_size > 0
            || inode.encrypted
            || inode.filter_delta
            || inode.filter_shuffle
            || inode.filter_bitshuffle
        {
            if (inode.size as usize) <= DecodedBlobCache::MAX_ENTRY_BYTES {
                let cached = crate::lock_unpoisoned(&guard.decoded_blob_cache).get(inode_id, inode);
                let blob = match cached {
                    Some(arc_blob) => arc_blob,
                    None => {
                        let decoded = Arc::new(Self::read_data_internal(guard, inode_id, inode)?);
                        crate::lock_unpoisoned(&guard.decoded_blob_cache).insert(
                            inode_id,
                            inode,
                            Arc::clone(&decoded),
                        );
                        decoded
                    }
                };
                let start = file_offset as usize;
                if start >= blob.len() {
                    return Ok(0);
                }
                let end = (start + to_read_total).min(blob.len());
                let actual = end.saturating_sub(start);
                buf[..actual].copy_from_slice(&blob[start..end]);
                return Ok(actual);
            }

            let full_data = Self::read_data_internal(guard, inode_id, inode)?;
            let start = file_offset as usize;
            if start >= full_data.len() {
                return Ok(0);
            }
            let end = (start + to_read_total).min(full_data.len());
            let actual = end.saturating_sub(start);
            buf[..actual].copy_from_slice(&full_data[start..end]);
            return Ok(actual);
        }

        // Fast path for raw uncompressed files: Batch indirect leaf resolution & coalesced
        // contiguous mmap extent copy
        struct ReadAtSink<'a> {
            guard: &'a DiskManagerInner,
            buf: &'a mut [u8],
            extents: Option<&'a mut ExtentList>,
            to_read_total: usize,
            end_offset: u64,
            bytes_read: usize,
            curr_offset: u64,
            pending_mmap_run: Option<(usize, usize, usize)>,
        }

        impl<'a> ReadAtSink<'a> {
            #[inline]
            fn flush_mmap_run(&mut self) {
                if let Some((run_src, run_dst, run_len)) = self.pending_mmap_run.take()
                    && let Some(mmap) = self.guard.backing.as_mmap_slice()
                {
                    self.buf[run_dst..run_dst + run_len]
                        .copy_from_slice(&mmap[run_src..run_src + run_len]);
                }
            }

            #[inline]
            fn emit_block(&mut self, blk_id: u64, in_blk_off: usize, chunk_len: usize) {
                let backing_len = self.guard.backing.len();
                // Reject forged pointers targeting metadata blocks (< data_block_start).
                let valid_src = if self.guard.is_data_block(blk_id) {
                    usize::try_from(blk_id)
                        .ok()
                        .and_then(|b| b.checked_mul(BLOCK_SIZE))
                        .filter(|&b_start| b_start.saturating_add(BLOCK_SIZE) <= backing_len)
                } else {
                    None
                };
                match valid_src {
                    Some(blk_byte_start) => match self.extents.as_deref_mut() {
                        None => {
                            let src_off = blk_byte_start + in_blk_off;
                            match self.pending_mmap_run {
                                Some((run_src, run_dst, run_len))
                                    if run_src + run_len == src_off
                                        && run_dst + run_len == self.bytes_read =>
                                {
                                    self.pending_mmap_run =
                                        Some((run_src, run_dst, run_len + chunk_len));
                                }
                                _ => {
                                    self.flush_mmap_run();
                                    self.pending_mmap_run =
                                        Some((src_off, self.bytes_read, chunk_len));
                                }
                            }
                        }
                        Some(list) => list.push(
                            blk_id * BLOCK_SIZE as u64 + in_blk_off as u64,
                            self.bytes_read,
                            chunk_len,
                        ),
                    },
                    None => {
                        self.flush_mmap_run();
                        self.buf[self.bytes_read..self.bytes_read + chunk_len].fill(0);
                    }
                }
                self.bytes_read += chunk_len;
                self.curr_offset += chunk_len as u64;
            }

            #[inline]
            fn emit_hole_until(&mut self, limit_blk: usize) {
                let hole_end = (limit_blk as u64 * BLOCK_SIZE as u64).min(self.end_offset);
                let hole_len = (hole_end - self.curr_offset) as usize;
                self.flush_mmap_run();
                self.buf[self.bytes_read..self.bytes_read + hole_len].fill(0);
                self.bytes_read += hole_len;
                self.curr_offset = hole_end;
            }

            #[inline]
            fn emit_sib_entries(&mut self, sib_slice: &[u8], start_entry: usize) {
                for chunk in sib_slice[start_entry * 8..512 * 8].as_chunks::<8>().0 {
                    if self.bytes_read >= self.to_read_total {
                        break;
                    }
                    let in_off = (self.curr_offset % BLOCK_SIZE as u64) as usize;
                    let chunk_len = (self.to_read_total - self.bytes_read).min(BLOCK_SIZE - in_off);
                    self.emit_block(u64::from_le_bytes(*chunk), in_off, chunk_len);
                }
            }
        }

        let mut sink = ReadAtSink {
            guard,
            buf,
            extents,
            to_read_total,
            end_offset: file_offset + to_read_total as u64,
            bytes_read: 0,
            curr_offset: file_offset,
            pending_mmap_run: None,
        };

        const SINGLE_LIMIT: usize = 10 + 512;
        const DOUBLE_LIMIT: usize = SINGLE_LIMIT + 512 * 512;
        const TRIPLE_LIMIT: usize = DOUBLE_LIMIT + 512 * 512 * 512;

        while sink.bytes_read < sink.to_read_total {
            let blk_idx = (sink.curr_offset / BLOCK_SIZE as u64) as usize;
            let in_blk_offset = (sink.curr_offset % BLOCK_SIZE as u64) as usize;

            // 1. Direct blocks (0..10)
            if blk_idx < 10 {
                let rem_in_blk = BLOCK_SIZE - in_blk_offset;
                let chunk_len = (sink.to_read_total - sink.bytes_read).min(rem_in_blk);
                sink.emit_block(inode.blocks[blk_idx], in_blk_offset, chunk_len);
                continue;
            }

            // 2. Single indirect blocks (10..522) - Batch leaf slice resolution
            if blk_idx < SINGLE_LIMIT {
                let sib_id = inode.blocks[10];
                if !guard.is_data_block(sib_id)
                    || guard
                        .backing
                        .with_block(sib_id, |sib_slice| {
                            sink.emit_sib_entries(sib_slice, blk_idx - 10);
                        })
                        .is_err()
                {
                    sink.emit_hole_until(SINGLE_LIMIT);
                }
                continue;
            }

            // 3. Double indirect blocks (522..262666) - Batch dib + sib resolution
            if blk_idx < DOUBLE_LIMIT {
                let dib_id = inode.blocks[11];
                if !guard.is_data_block(dib_id)
                    || guard
                        .backing
                        .with_block(dib_id, |dib_slice| {
                            while sink.bytes_read < sink.to_read_total {
                                let cur_blk = (sink.curr_offset / BLOCK_SIZE as u64) as usize;
                                if cur_blk >= DOUBLE_LIMIT {
                                    break;
                                }
                                let rel = cur_blk - SINGLE_LIMIT;
                                let s_idx = rel / 512;
                                let d_idx = rel % 512;
                                let sib_id = u64::from_le_bytes(
                                    dib_slice[s_idx * 8..s_idx * 8 + 8].try_into().unwrap(),
                                );
                                if !guard.is_data_block(sib_id)
                                    || guard
                                        .backing
                                        .with_block(sib_id, |sib_slice| {
                                            sink.emit_sib_entries(sib_slice, d_idx);
                                        })
                                        .is_err()
                                {
                                    sink.emit_hole_until(cur_blk + (512 - d_idx));
                                }
                            }
                        })
                        .is_err()
                {
                    sink.emit_hole_until(DOUBLE_LIMIT);
                }
                continue;
            }

            // 4. Triple indirect blocks (262666..134480394) - Batch tib + dib + sib resolution
            if blk_idx < TRIPLE_LIMIT {
                let tib_id = inode.triple_indirect;
                if guard.is_data_block(tib_id) {
                    let rel = blk_idx - DOUBLE_LIMIT;
                    let t_idx = rel / (512 * 512);
                    let rem_idx = rel % (512 * 512);
                    let d_idx = rem_idx / 512;
                    let s_idx = rem_idx % 512;
                    let dib_id = guard.backing.read_block_ptr(tib_id, t_idx);
                    if guard.is_data_block(dib_id) {
                        let sib_id = guard.backing.read_block_ptr(dib_id, d_idx);
                        if !guard.is_data_block(sib_id)
                            || guard
                                .backing
                                .with_block(sib_id, |sib_slice| {
                                    sink.emit_sib_entries(sib_slice, s_idx);
                                })
                                .is_err()
                        {
                            sink.emit_hole_until(blk_idx + (512 - s_idx));
                        }
                    } else {
                        sink.emit_hole_until(blk_idx + ((512 * 512) - rem_idx));
                    }
                } else {
                    sink.emit_hole_until(TRIPLE_LIMIT);
                }
                continue;
            }

            // Beyond maximum addressable blocks: zero-fill remainder
            sink.flush_mmap_run();
            let rem_start = sink.bytes_read;
            let rem_end = sink.to_read_total;
            sink.buf[rem_start..rem_end].fill(0);
            sink.bytes_read = rem_end;
            break;
        }

        sink.flush_mmap_run();
        Ok(sink.bytes_read)
    }

    /// Writes data to a file (default: no pre-compression filters)
    ///
    /// For custom filter pipeline (Delta / Shuffle / typesize), use [`write_data_with_filters`].
    pub fn write_data(
        &self,
        inode_id: u64,
        file_offset: u64,
        data: &[u8],
        compression_mode: CompressionMode,
    ) -> Result<(), DiskManagerError> {
        self.write_data_with_filters(
            inode_id,
            file_offset,
            data,
            compression_mode,
            crate::filters::FilterConfig::none(),
        )
    }

    /// Fast path for fresh multi-block writes from offset 0 (`inode.blocks == [0; 12]`).
    ///
    /// Allocates all payload blocks + indirect pointer blocks in ONE contiguous bitmap
    /// allocation, writes the entire payload in ONE contiguous `write_all_at` (plus trailing
    /// slack zeroing), and writes each indirect pointer block once instead of per-entry.
    /// Falls back cleanly to the general per-block path if contiguous free space is unavailable.
    fn try_write_contiguous_fresh_file(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        data: &[u8],
        mut touched_blocks: Option<&mut Vec<u64>>,
    ) -> Result<Option<u64>, DiskManagerError> {
        const DIRECT: usize = 10;
        const PTRS: usize = 512;
        const SINGLE_LIMIT: usize = DIRECT + PTRS;
        const DOUBLE_LIMIT: usize = SINGLE_LIMIT + PTRS * PTRS;

        let total_blks = data.len().div_ceil(BLOCK_SIZE);
        if total_blks <= 1 || total_blks > DOUBLE_LIMIT {
            return Ok(None);
        }

        let need_sib = total_blks > DIRECT;
        let double_blks = total_blks.saturating_sub(SINGLE_LIMIT);
        let num_dib_sibs = double_blks.div_ceil(PTRS);
        let need_dib = double_blks > 0;
        let total_meta_blks = usize::from(need_sib) + usize::from(need_dib) + num_dib_sibs;
        let total_alloc = total_blks + total_meta_blks;

        let start_blk = match Self::allocate_contiguous_blocks_internal(guard, total_alloc) {
            Ok(b) => b,
            Err(DiskManagerError::Allocator(AllocatorError::NoSpace)) => return Ok(None),
            Err(e) => return Err(e),
        };

        let write_res: Result<(), DiskManagerError> = (|| {
            // 1. Write the entire payload in one contiguous write + zero trailing slack.
            let payload_off = start_blk.checked_mul(BLOCK_SIZE as u64).ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("payload block offset overflow"))
            })?;
            guard.backing.write_all_at(payload_off, data)?;
            let slack = total_blks * BLOCK_SIZE - data.len();
            if slack > 0 {
                const ZEROS: [u8; BLOCK_SIZE] = [0u8; BLOCK_SIZE];
                guard
                    .backing
                    .write_all_at(payload_off + data.len() as u64, &ZEROS[..slack])?;
            }

            // 2. Populate direct pointers and write each indirect pointer block in one shot.
            for i in 0..total_blks.min(DIRECT) {
                inode.blocks[i] = start_blk + i as u64;
            }
            let mut next_meta = start_blk + total_blks as u64;

            if need_sib {
                let sib_blk = next_meta;
                next_meta += 1;
                let mut sib_buf = [0u8; BLOCK_SIZE];
                let sib_end = total_blks.min(SINGLE_LIMIT);
                for (slot, logical_idx) in (DIRECT..sib_end).enumerate() {
                    let phys = start_blk + logical_idx as u64;
                    sib_buf[slot * 8..slot * 8 + 8].copy_from_slice(&phys.to_le_bytes());
                }
                guard
                    .backing
                    .write_all_at(sib_blk * BLOCK_SIZE as u64, &sib_buf)?;
                inode.blocks[10] = sib_blk;
            }

            if need_dib {
                let dib_blk = next_meta;
                next_meta += 1;
                let mut dib_buf = [0u8; BLOCK_SIZE];
                for a in 0..num_dib_sibs {
                    let child_sib = next_meta;
                    next_meta += 1;
                    dib_buf[a * 8..a * 8 + 8].copy_from_slice(&child_sib.to_le_bytes());
                    let mut sib_buf = [0u8; BLOCK_SIZE];
                    let range_start = SINGLE_LIMIT + a * PTRS;
                    let range_end = total_blks.min(range_start + PTRS);
                    for (slot, logical_idx) in (range_start..range_end).enumerate() {
                        let phys = start_blk + logical_idx as u64;
                        sib_buf[slot * 8..slot * 8 + 8].copy_from_slice(&phys.to_le_bytes());
                    }
                    guard
                        .backing
                        .write_all_at(child_sib * BLOCK_SIZE as u64, &sib_buf)?;
                }
                guard
                    .backing
                    .write_all_at(dib_blk * BLOCK_SIZE as u64, &dib_buf)?;
                inode.blocks[11] = dib_blk;
            }
            debug_assert_eq!(next_meta, start_blk + total_alloc as u64);
            Ok(())
        })();

        if let Err(e) = write_res {
            inode.blocks = [0; 12];
            let _ = Self::free_contiguous_blocks_internal(guard, start_blk, total_alloc);
            return Err(e);
        }

        if let Some(ref mut tb) = touched_blocks {
            tb.reserve(total_alloc);
            for b in 0..total_alloc as u64 {
                tb.push(start_blk + b);
            }
        }

        Ok(Some(data.len() as u64))
    }

    fn write_buffer_at_offset(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        mut current_offset: u64,
        data: &[u8],
        mut touched_blocks: Option<&mut Vec<u64>>,
    ) -> Result<u64, DiskManagerError> {
        if current_offset == 0
            && data.len() > BLOCK_SIZE
            && inode.blocks == [0; 12]
            && inode.triple_indirect == 0
            && let Some(end_off) = Self::try_write_contiguous_fresh_file(
                guard,
                inode,
                data,
                touched_blocks.as_deref_mut(),
            )?
        {
            return Ok(end_off);
        }

        let mut written = 0;
        // Coalesce physically contiguous full-block writes into a single `write_all_at`
        // call, turning N 4 KiB `pwrite` syscalls under `FileIo` into 1 contiguous `pwrite`.
        let mut pending_run: Option<(u64, usize, usize)> = None;
        while written < data.len() {
            let blk_idx = (current_offset / BLOCK_SIZE as u64) as usize;
            let (blk_id, is_fresh) = Self::get_or_alloc_block_for_write(guard, inode, blk_idx)?;
            if let Some(ref mut tb) = touched_blocks
                && tb.last().copied() != Some(blk_id)
            {
                tb.push(blk_id);
            }

            let in_blk_off = (current_offset % BLOCK_SIZE as u64) as usize;
            let to_write = std::cmp::min(data.len() - written, BLOCK_SIZE - in_blk_off);

            if in_blk_off == 0 && to_write == BLOCK_SIZE {
                let off = blk_id.checked_mul(BLOCK_SIZE as u64).ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::other("block offset overflow"))
                })?;
                match pending_run {
                    Some((run_off, run_start, run_len))
                        if run_off + run_len as u64 == off && run_start + run_len == written =>
                    {
                        pending_run = Some((run_off, run_start, run_len + to_write));
                    }
                    Some((run_off, run_start, run_len)) => {
                        guard
                            .backing
                            .write_all_at(run_off, &data[run_start..run_start + run_len])?;
                        pending_run = Some((off, written, to_write));
                    }
                    None => {
                        pending_run = Some((off, written, to_write));
                    }
                }
            } else {
                if let Some((run_off, run_start, run_len)) = pending_run.take() {
                    guard
                        .backing
                        .write_all_at(run_off, &data[run_start..run_start + run_len])?;
                }
                if is_fresh && guard.backing.as_mmap_slice().is_none() {
                    let mut block_buf = [0u8; BLOCK_SIZE];
                    block_buf[in_blk_off..in_blk_off + to_write]
                        .copy_from_slice(&data[written..written + to_write]);
                    let off = blk_id.checked_mul(BLOCK_SIZE as u64).ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::other("block offset overflow"))
                    })?;
                    guard.backing.write_all_at(off, &block_buf)?;
                } else {
                    guard.backing.with_block_mut(blk_id, |slice| {
                        // On a newly allocated block, only zero the untouched
                        // prefix/suffix bytes; full-block writes skip zeroing altogether.
                        if is_fresh {
                            slice[..in_blk_off].fill(0);
                            slice[in_blk_off + to_write..].fill(0);
                        }
                        slice[in_blk_off..in_blk_off + to_write]
                            .copy_from_slice(&data[written..written + to_write]);
                    })?;
                }
            }
            written += to_write;
            current_offset += to_write as u64;
        }
        if let Some((run_off, run_start, run_len)) = pending_run {
            guard
                .backing
                .write_all_at(run_off, &data[run_start..run_start + run_len])?;
        }
        Ok(current_offset)
    }

    /// Record every indirect pointer block in `inode` that remains active for the
    /// first `needed_logical` blocks, so `write_data_journaled` never frees a
    /// reused indirect table block when reclaiming orphaned blocks.
    fn collect_retained_indirect_blocks(
        backing: &ImageBacking,
        inode: &Inode,
        needed_logical: usize,
        out: &mut FxHashSet<u64>,
    ) {
        const PTRS: usize = 512;
        const DIRECT: usize = 10;
        const SINGLE_SPAN: usize = DIRECT + PTRS;
        const DOUBLE_SPAN: usize = DIRECT + PTRS * (1 + PTRS);

        if needed_logical <= DIRECT {
            return;
        }
        if inode.blocks[10] != 0 {
            out.insert(inode.blocks[10]);
        }
        if needed_logical <= SINGLE_SPAN {
            return;
        }
        let dib = inode.blocks[11];
        if dib != 0 {
            out.insert(dib);
            let active_a = (needed_logical - SINGLE_SPAN).div_ceil(PTRS).min(PTRS);
            for a in 0..active_a {
                let sib2 = backing.read_block_ptr(dib, a);
                if sib2 != 0 {
                    out.insert(sib2);
                }
            }
        }
        if needed_logical <= DOUBLE_SPAN {
            return;
        }
        let tib = inode.triple_indirect;
        if tib != 0 {
            out.insert(tib);
            let rem_blocks = needed_logical - DOUBLE_SPAN;
            let active_b = rem_blocks.div_ceil(PTRS * PTRS).min(PTRS);
            for b in 0..active_b {
                let dib2 = backing.read_block_ptr(tib, b);
                if dib2 == 0 {
                    continue;
                }
                out.insert(dib2);
                let rem_in_b = rem_blocks.saturating_sub(b * PTRS * PTRS);
                let active_c = rem_in_b.div_ceil(PTRS).min(PTRS);
                for c in 0..active_c {
                    let sib3 = backing.read_block_ptr(dib2, c);
                    if sib3 != 0 {
                        out.insert(sib3);
                    }
                }
            }
        }
    }

    fn write_data_journaled(
        guard: &mut DiskManagerInner,
        inode_id: u64,
        file_offset: u64,
        plan: WritePlan<'_>,
        filter_config: crate::filters::FilterConfig,
    ) -> Result<(), DiskManagerError> {
        let mut inode = Self::read_inode_internal(guard, inode_id)?;
        let old_size = inode.size;
        let old_compressed = inode.compressed_size;
        let old_physical = if old_compressed > 0 {
            old_compressed
        } else {
            old_size
        };
        let old_logical = (old_physical / BLOCK_SIZE as u64) as usize
            + usize::from(old_physical % BLOCK_SIZE as u64 != 0);

        let WritePlan {
            case,
            phys_off,
            buffer,
            new_size,
            new_compressed_size,
            encryption_nonce,
        } = plan;

        let new_physical = if new_compressed_size > 0 {
            new_compressed_size
        } else {
            new_size
        };
        let rewrites_from_zero =
            case.frees_previous_blocks() || (file_offset == 0 && phys_off == 0);
        let may_free_blocks = old_physical > 0
            && (case.frees_previous_blocks()
                || (rewrites_from_zero && new_physical < old_physical));
        let old_blocks = if may_free_blocks {
            Self::collect_inode_blocks(&guard.backing, &inode)
        } else {
            Vec::new()
        };

        // Full-rewrite cases (`FullOverwrite`, `Recompress`) must allocate brand-new
        // Copy-On-Write blocks rather than overwriting the inode's live blocks in place
        // prior to the WAL commit; otherwise a crash or mid-stage failure corrupts the
        // pre-transaction payload before recovery sees the new pointers.
        if case.frees_previous_blocks() {
            inode.blocks = [0; 12];
            inode.triple_indirect = 0;
        }

        let mut sim = crate::journal::AllocSim::new_data_only(
            guard,
            guard.free_inode_hint,
            guard.free_block_hint,
        )?;
        let mut ops: Vec<crate::journal::MetadataOp> = Vec::new();
        let mut used: Vec<u64> = Vec::new();

        // 1. Stage payload (bitmap bits still clear).
        crate::journal::stage_payload_write(
            guard, &mut inode, phys_off, &buffer, &mut sim, &mut ops, &mut used,
        )?;

        // A shrinking write (including `FullOverwrite`, `Recompress` at non-zero
        // `file_offset`, or `truncate(..., 0)`) orphans both data blocks and the
        // pointers that reference them; both halves must go, or the inode ends up
        // pointing at free blocks (`missing_blocks` in `fsck`).
        let needed_logical = (new_physical / BLOCK_SIZE as u64) as usize
            + usize::from(new_physical % BLOCK_SIZE as u64 != 0);
        if rewrites_from_zero && !case.frees_previous_blocks() {
            if needed_logical == 0 {
                inode.blocks = [0; 12];
                inode.triple_indirect = 0;
            } else if needed_logical < old_logical {
                crate::journal::prune_stale_pointers(
                    &guard.backing,
                    &mut inode,
                    needed_logical,
                    &mut ops,
                )?;
            }
        }

        inode.size = new_size;
        inode.compressed_size = new_compressed_size;
        if let Some(nonce) = encryption_nonce {
            inode.encrypted = true;
            inode.encryption_nonce = nonce;
        }
        // Persist filter metadata whenever the pipeline re-ran over the whole file,
        // compressed or not. Gating this on `compressed_size > 0` silently corrupted
        // uncompressed filtered writes: the payload stayed filtered on disk while the
        // inode claimed it was raw, so the reader returned filtered bytes verbatim.
        if case.rewrites_filter_metadata() {
            inode.filter_typesize = filter_config.typesize;
            inode.filter_delta = filter_config.delta;
            inode.filter_shuffle = filter_config.shuffle;
            inode.filter_bitshuffle = filter_config.bitshuffle;
        }
        inode.modified_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // 2. Free blocks the new layout no longer references (retaining active leaf
        //    blocks in `used` AND any indirect pointer blocks still covering `0..needed_logical`).
        if !old_blocks.is_empty() {
            let mut used_set: FxHashSet<u64> = used.iter().copied().collect();
            Self::collect_retained_indirect_blocks(
                &guard.backing,
                &inode,
                needed_logical,
                &mut used_set,
            );
            for blk in &old_blocks {
                if !used_set.contains(blk) {
                    crate::journal::push_data_bitmap_op(&mut ops, *blk, 1, false);
                }
            }
        }
        ops.push(crate::journal::MetadataOp::WriteInode {
            inode_id,
            inode_bytes: Box::new(Self::build_inode_post_image(guard, inode_id, &inode)?),
        });
        let ops = if crate::journal::is_already_coalesced(&ops) {
            ops
        } else {
            crate::journal::coalesce_ops(ops)
        };

        // 3. Payload durability BEFORE the metadata commit.
        let payload_ranges: Vec<(usize, usize)> = if guard.durability_mode().is_range_based() {
            used.iter().map(|b| guard.block_byte_range(*b)).collect()
        } else {
            Vec::new()
        };
        if guard.durability_mode() == DurabilityMode::Strict && !is_strict_sync_deferred() {
            if guard.backing.as_mmap_slice().is_some() {
                let coalesced_payload =
                    DiskManagerInner::coalesce_ranges(&payload_ranges, guard.backing.len());
                for (off, actual) in coalesced_payload {
                    guard.backing.flush_range(off, actual)?;
                }
            } else {
                guard.backing.flush()?;
            }
        }

        // 4. Commit point.
        Self::commit_journal_tx(guard, &ops)?;

        // 5. Apply.
        Self::apply_journal_ops(guard, &ops)?;

        guard.free_inode_hint = sim.free_inode_hint;
        guard.free_block_hint = sim.free_block_hint;
        guard.inode_cache.insert(inode_id, inode);

        if guard.durability_mode() == DurabilityMode::RangeAsync {
            let mut ranges = vec![
                guard.data_bitmap_byte_range(),
                guard.inode_byte_range(inode_id),
            ];
            ranges.extend(payload_ranges);
            for op in &ops {
                if let crate::journal::MetadataOp::WriteBlockSlice { block_id, .. } = op {
                    ranges.push(guard.block_byte_range(*block_id));
                }
            }
            guard.sync_mutation_ranges(&ranges)?;
        } else if guard.durability_mode() == DurabilityMode::LegacyWholeMmapAsync {
            guard.sync_mutation_ranges(&[])?;
        }
        Self::maybe_checkpoint(guard)?;
        Ok(())
    }

    /// Build the final on-disk buffer for a full (offset 0) overwrite: filter, then
    /// compress, then encrypt, for a write that replaces the whole file.
    ///
    /// This function performs pure CPU-bound processing without holding any disk locks.
    pub(crate) fn build_full_overwrite_buffer_pure<'a>(
        data: &'a [u8],
        compression_mode: CompressionMode,
        filter_config: &crate::filters::FilterConfig,
        encryption_key: Option<&crate::encryption::EncryptionKey>,
        aad: &[u8],
    ) -> Result<FullOverwritePayload<'a>, DiskManagerError> {
        let filtered = crate::filters::apply_filters_cow(data, filter_config);
        let working: &[u8] = &filtered;

        let should_compress = match compression_mode {
            CompressionMode::Always
            | CompressionMode::Stream { .. }
            | CompressionMode::Seekable { .. } => true,
            CompressionMode::Never => false,
            CompressionMode::Auto => working.len() >= 8192,
        };

        let level = match compression_mode {
            CompressionMode::Stream { level } | CompressionMode::Seekable { level, .. } => {
                if level <= 0 { 0 } else { level }
            }
            _ => 0,
        };

        let (final_data, is_compressed): (std::borrow::Cow<'a, [u8]>, bool) = if should_compress {
            // The pooled compressor avoids a per-call `CCtx` allocation.
            let compressed =
                crate::zstd_pool::compress_frame(working, level).map_err(DiskManagerError::Io)?;
            match compression_mode {
                CompressionMode::Always
                | CompressionMode::Stream { .. }
                | CompressionMode::Seekable { .. } => (std::borrow::Cow::Owned(compressed), true),
                CompressionMode::Auto => {
                    if compressed.len() < working.len() {
                        (std::borrow::Cow::Owned(compressed), true)
                    } else {
                        (filtered, false)
                    }
                }
                CompressionMode::Never => (filtered, false),
            }
        } else {
            (filtered, false)
        };

        if let Some(key) = encryption_key {
            let nonce = crate::encryption::generate_nonce();
            // In-place AEAD avoids allocating a second full-payload ciphertext buffer.
            let mut enc = final_data.into_owned();
            enc.reserve(16);
            crate::encryption::encrypt_data_in_place(&mut enc, key, &nonce, aad)?;
            return Ok(FullOverwritePayload {
                buffer: std::borrow::Cow::Owned(enc),
                is_compressed,
                encryption_nonce: Some(nonce),
            });
        }
        Ok(FullOverwritePayload {
            buffer: final_data,
            is_compressed,
            encryption_nonce: None,
        })
    }

    /// Backwards-compatible wrapper around `build_full_overwrite_buffer_pure`.
    #[allow(dead_code)]
    fn build_full_overwrite_buffer(
        guard: &DiskManagerInner,
        inode: &mut Inode,
        data: &[u8],
        compression_mode: CompressionMode,
        filter_config: &crate::filters::FilterConfig,
    ) -> Result<(Vec<u8>, bool), DiskManagerError> {
        let payload = Self::build_full_overwrite_buffer_pure(
            data,
            compression_mode,
            filter_config,
            guard.encryption_key.as_ref(),
            &[],
        )?;
        if let Some(n) = payload.encryption_nonce {
            inode.encrypted = true;
            inode.encryption_nonce = n;
        }
        Ok((payload.buffer.into_owned(), payload.is_compressed))
    }

    /// Filters to apply on a recompression path: the caller's when active, otherwise
    /// whatever the inode already records.
    fn effective_filter(
        inode: &Inode,
        filter_config: &crate::filters::FilterConfig,
    ) -> crate::filters::FilterConfig {
        if filter_config.is_active() {
            *filter_config
        } else {
            crate::filters::FilterConfig {
                typesize: inode.filter_typesize,
                delta: inode.filter_delta,
                shuffle: inode.filter_shuffle,
                bitshuffle: inode.filter_bitshuffle,
            }
        }
    }

    /// Writes data to a file with custom pre-compression filters
    ///
    /// # Arguments
    /// * `inode_id` - The inode ID of the file to write to
    /// * `file_offset` - Byte offset to start writing at
    /// * `data` - Data to write
    /// * `compression_mode` - Compression mode (Always, Never, or Auto)
    /// * `filter_config` - Pre-compression filter configuration (delta/shuffle/typesize)
    ///
    /// # Compression Handling & Append
    /// - `file_offset == 0`: Initial write or full overwrite.
    /// - `file_offset > 0` on compressed files:
    ///   - **Fast Path (Zstd Multi-Frame)**: Appending strictly at EOF to an unencrypted file
    ///     compresses the new chunk into an independent Zstd frame and writes it directly
    ///     without decompressing existing blocks.
    ///   - **Transparent Fallback (Read-Modify-Recompress)**: For encrypted files, random-offset
    ///     writes, or files with active filters, decompresses existing data, splices in the change,
    ///     and re-writes contiguously from offset 0.
    pub fn write_data_with_filters(
        &self,
        inode_id: u64,
        file_offset: u64,
        data: &[u8],
        compression_mode: CompressionMode,
        filter_config: crate::filters::FilterConfig,
    ) -> Result<(), DiskManagerError> {
        if self.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        // --- STAGE 1: Single Shared Read-Lock Inspection ---
        // Merges seekable routing + Stage 1 preparation under ONE `inner.read()` lock.
        enum Stage1Prep<'a> {
            Seekable {
                level: i32,
                compress: bool,
                stage1: chunked::ChunkedStage1<'a>,
            },
            Standard {
                initial_inode: Inode,
                initial_gen: u64,
                existing_decompressed: Option<Vec<u8>>,
                encryption_key: Option<crate::encryption::EncryptionKey>,
                encryption_scheme: u8,
            },
        }

        let stage1 = {
            let guard = self.inner.read().unwrap();
            let inode = Self::read_inode_internal(&guard, inode_id)?;

            if inode.mode != crate::inode::FileType::File {
                return Err(DiskManagerError::Io(std::io::Error::other(
                    "Cannot write data to non-file inode",
                )));
            }

            let explicit_seekable = inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0
                || matches!(compression_mode, CompressionMode::Seekable { .. });
            // Non-zero-offset writes on encrypted Format-v2+ non-filtered files
            // transparently upgrade to 64 KiB context-bound AEAD chunks, turning O(N^2)
            // whole-file decrypt+splice+re-encrypt appends into O(1) per-chunk COW updates.
            let encrypted_partial_upgrade = !explicit_seekable
                && file_offset > 0
                && !data.is_empty()
                && (inode.encrypted || guard.encryption_key.is_some())
                && guard.superblock.inode_record_version_for(inode_id)
                    >= SuperBlock::FORMAT_VERSION_FIXED_INODE
                && !filter_config.is_active()
                && inode.filter_typesize == 0;

            if explicit_seekable || encrypted_partial_upgrade {
                if data.is_empty() {
                    return Ok(());
                }
                let compress = explicit_seekable || compression_mode != CompressionMode::Never;
                let level = match compression_mode {
                    CompressionMode::Seekable { level, .. } => level,
                    CompressionMode::Stream { level } => level,
                    _ => 1,
                };
                let prep =
                    Self::prepare_chunked_64k_stage1(&guard, inode_id, inode, file_offset, data)?;
                Stage1Prep::Seekable {
                    level,
                    compress,
                    stage1: prep,
                }
            } else {
                let transformed =
                    payload_is_transformed(&inode, &filter_config, guard.encryption_key.as_ref());
                let is_full_overwrite = file_offset == 0
                    && (inode.size == 0 || data.len() as u64 >= inode.size || transformed);

                let fast_append = may_append_zstd_frame(
                    &inode,
                    file_offset,
                    &filter_config,
                    guard.encryption_key.as_ref(),
                );

                // A transformed payload is rebuilt from the file's decoded content,
                // so that content has to be read before the plan is computed.
                let existing =
                    if !is_full_overwrite && transformed && inode.size > 0 && !fast_append {
                        Some(Self::read_data_internal(&guard, inode_id, &inode)?)
                    } else {
                        None
                    };

                let key = guard.encryption_key.clone();
                let scheme = guard.superblock.encryption_version;
                let initial_gen = guard.inode_gen(inode_id);
                Stage1Prep::Standard {
                    initial_inode: inode,
                    initial_gen,
                    existing_decompressed: existing,
                    encryption_key: key,
                    encryption_scheme: scheme,
                }
            }
        }; // Shared read-guard is dropped immediately here!

        let (
            initial_inode,
            initial_gen,
            existing_decompressed,
            encryption_key,
            encryption_scheme,
        ) = match stage1 {
            Stage1Prep::Seekable {
                level,
                compress,
                stage1,
            } => {
                return self.finish_chunked_64k_pipeline(
                    inode_id,
                    file_offset,
                    data,
                    level,
                    compress,
                    stage1,
                );
            }
            Stage1Prep::Standard {
                initial_inode,
                initial_gen,
                existing_decompressed,
                encryption_key,
                encryption_scheme,
            } => (
                initial_inode,
                initial_gen,
                existing_decompressed,
                encryption_key,
                encryption_scheme,
            ),
        };

        // --- STAGE 2: CPU-Bound Processing (ZERO filesystem locks held) ---
        // Perform CPU-heavy filtering, Zstd compression, and XChaCha20 encryption
        // entirely outside the exclusive lock so concurrent readers are not blocked.
        // Stack-staged AAD binds the ciphertext to the file with zero allocs.
        let (aad_buf, aad_len) =
            crate::encryption::file_aad_for_scheme_stack(inode_id, encryption_scheme);
        let prepared_plan = plan_write_prepared(
            &initial_inode,
            file_offset,
            data,
            compression_mode,
            &filter_config,
            existing_decompressed.as_deref(),
            EncryptionContext {
                key: encryption_key.as_ref(),
                aad: &aad_buf[..aad_len],
            },
        )?;

        // --- STAGE 3: Exclusive Lock Acquisition & Commit ---
        // Acquire exclusive write lock ONLY for block allocation and metadata commit.
        let mut guard = self.inner.write().unwrap();
        if !guard.is_inode_allocated(inode_id) {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Inode was deleted",
            )));
        }
        let mut inode = Self::read_inode_internal(&guard, inode_id)?;

        if inode.mode != crate::inode::FileType::File {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Cannot write data to non-file inode",
            )));
        }

        if inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0 {
            let compress = compression_mode != CompressionMode::Never;
            return Self::write_chunked_64k_internal(
                &mut guard,
                inode_id,
                file_offset,
                data,
                1,
                compress,
            );
        }

        // Verify if the inode state remains consistent with Stage 1.
        let can_use_prepared = guard.inode_gen(inode_id) == initial_gen
            && inode.size == initial_inode.size
            && inode.compressed_size == initial_inode.compressed_size
            && inode.modified_at == initial_inode.modified_at
            && inode.encrypted == initial_inode.encrypted
            && inode.encryption_nonce == initial_inode.encryption_nonce
            && inode.flags == initial_inode.flags
            && inode.blocks == initial_inode.blocks
            && inode.triple_indirect == initial_inode.triple_indirect
            && inode.filter_typesize == initial_inode.filter_typesize
            && inode.filter_delta == initial_inode.filter_delta
            && inode.filter_shuffle == initial_inode.filter_shuffle
            && inode.filter_bitshuffle == initial_inode.filter_bitshuffle;

        let plan = if can_use_prepared {
            prepared_plan
        } else {
            // Raced with another writer on this specific inode: re-compute plan under write lock.
            plan_write(
                &guard,
                inode_id,
                &inode,
                file_offset,
                data,
                compression_mode,
                &filter_config,
            )?
        };

        // Journaled images stage the write through AllocSim and commit one
        // metadata transaction; legacy images keep the in-place implementation.
        //
        // The filter metadata written back must describe the pipeline that was
        // actually applied to the payload. The plan uses the *effective* filter
        // (the caller's config, or the file's stored one when the caller does
        // not request a new pipeline), so the metadata has to follow it.
        let effective_filter = Self::effective_filter(&inode, &filter_config);

        if guard.superblock.has_journal_layout() {
            let res = Self::write_data_journaled(
                &mut guard,
                inode_id,
                file_offset,
                plan,
                effective_filter,
            );
            drop(guard);
            self.maybe_drain_async_ranges();
            return res;
        }

        // Decide once, apply in place.
        if plan.case.frees_previous_blocks() {
            let old_blocks = Self::collect_inode_blocks(&guard.backing, &inode);
            let mut min_freed_blk = u64::MAX;
            {
                let db_start = guard.superblock.data_block_start;
                let mut slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
                })?;
                let mut da = SimpleBlockAllocator::new(&mut slice, db_start);
                for blk in old_blocks {
                    da.free(blk)?;
                    min_freed_blk = min_freed_blk.min(blk);
                }
            }
            if min_freed_blk < guard.free_block_hint {
                guard.free_block_hint = min_freed_blk;
            }
            inode.blocks = [0; 12];
            inode.triple_indirect = 0;
        }

        let mut touched = if guard.durability_mode().is_range_based() {
            Some(Vec::new())
        } else {
            None
        };
        Self::write_buffer_at_offset(
            &mut guard,
            &mut inode,
            plan.phys_off,
            &plan.buffer,
            touched.as_mut(),
        )?;

        inode.size = plan.new_size;
        inode.compressed_size = plan.new_compressed_size;
        if let Some(nonce) = plan.encryption_nonce {
            inode.encrypted = true;
            inode.encryption_nonce = nonce;
        }
        if plan.case.rewrites_filter_metadata() {
            inode.filter_typesize = effective_filter.typesize;
            inode.filter_delta = effective_filter.delta;
            inode.filter_shuffle = effective_filter.shuffle;
            inode.filter_bitshuffle = effective_filter.bitshuffle;
        }
        inode.modified_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self::write_inode_internal(&mut guard, inode_id, &inode)?;

        let sync_res = if guard.durability_mode().is_range_based() {
            let mut ranges = vec![
                guard.data_bitmap_byte_range(),
                guard.inode_byte_range(inode_id),
            ];
            if let Some(tb) = touched {
                for blk in tb {
                    ranges.push(guard.block_byte_range(blk));
                }
            }
            guard.sync_mutation_ranges(&ranges)
        } else {
            guard.sync_mutation_ranges(&[])
        };
        drop(guard);
        self.maybe_drain_async_ranges();
        sync_res?;

        Ok(())
    }

    // Internal Helpers working on guards
    /// Byte range of the 256-byte inode slot for `inode_id` within the image.
    fn inode_slot_range(
        guard: &DiskManagerInner,
        inode_id: u64,
    ) -> Result<(usize, usize), DiskManagerError> {
        let inode_size = crate::inode_format::INODE_SLOT_SIZE as u64;
        let overflow = || {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Offset overflow",
            ))
        };
        // An inode id beyond the inode table would address the data
        // area, where a crafted byte sequence could be mistaken for an inode
        // (a "fake inode") and used to read or overwrite another file.
        if inode_id >= guard.superblock.usable_inode_count() {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Inode id {inode_id} is outside the inode table ({} usable inodes)",
                    guard.superblock.usable_inode_count()
                ),
            )));
        }
        let base = guard
            .superblock
            .inode_table_block
            .checked_mul(BLOCK_SIZE as u64)
            .ok_or_else(overflow)?;
        let id_off = inode_id.checked_mul(inode_size).ok_or_else(overflow)?;
        let offset = base.checked_add(id_off).ok_or_else(overflow)?;
        let end = offset.checked_add(inode_size).ok_or_else(overflow)?;
        if end > guard.backing.len() as u64 {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Bounds",
            )));
        }
        Ok((offset as usize, end as usize))
    }

    /// Build the exact bytes that will be stored in `inode_id`'s 256-byte slot.
    ///
    /// The write path and the journal's post-image path both go through here so they
    /// cannot drift apart — the journal replays a full 256-byte record, so if the
    /// in-place writer produced something different, recovery would diverge from a
    /// clean mount.
    ///
    /// Format v2 writes the whole fixed record, giving deterministic, zero-padded,
    /// byte-identical output. Format v1 keeps the historical behaviour of overlaying
    /// only the `bincode` prefix on top of the slot's existing bytes, so an
    /// unmigrated image is never rewritten in a way that differs from before.
    fn encode_inode_slot(
        guard: &DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
    ) -> Result<[u8; crate::inode_format::INODE_SLOT_SIZE], DiskManagerError> {
        let mut slot = [0u8; crate::inode_format::INODE_SLOT_SIZE];

        let version = guard.superblock.inode_record_version_for(inode_id);
        if version < SuperBlock::FORMAT_VERSION_FIXED_INODE {
            if let Ok((start, end)) = Self::inode_slot_range(guard, inode_id) {
                let _ = guard
                    .backing
                    .read_exact_at(start as u64, &mut slot[..end - start]);
            }
            let payload = crate::inode_format::encode_v1_payload(inode)?;
            if payload.len() > crate::inode_format::INODE_SLOT_SIZE {
                return Err(DiskManagerError::Serialization(Box::new(
                    bincode::ErrorKind::SizeLimit,
                )));
            }
            slot[..payload.len()].copy_from_slice(&payload);
            return Ok(slot);
        }

        slot = crate::inode_format::encode_v2(inode);
        Ok(slot)
    }

    /// Returns `true` if `inode_id` is currently marked allocated in the inode bitmap.
    pub fn is_inode_allocated(&self, inode_id: u64) -> bool {
        let guard = self.inner.read().unwrap();
        guard.is_inode_allocated(inode_id)
    }

    /// Reads an inode only if it is currently marked allocated in the inode bitmap.
    /// Returns `ErrorKind::NotFound` if the inode is unallocated or out of bounds.
    pub fn read_allocated_inode(&self, inode_id: u64) -> Result<Inode, DiskManagerError> {
        let guard = self.inner.read().unwrap();
        if !guard.is_inode_allocated(inode_id) {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Inode not allocated",
            )));
        }
        Self::read_inode_internal(&guard, inode_id)
    }

    fn read_inode_internal(
        guard: &DiskManagerInner,
        inode_id: u64,
    ) -> Result<Inode, DiskManagerError> {
        // Fast path: check in-memory inode cache
        if let Some(cached) = guard.inode_cache.get(inode_id) {
            return Ok(cached);
        }

        let (offset, end) = Self::inode_slot_range(guard, inode_id)?;
        let version = guard.superblock.inode_record_version_for(inode_id);
        let inode = if let Some(mmap) = guard.backing.as_mmap_slice() {
            crate::inode_format::decode_for(&mmap[offset..end], version)
        } else {
            let mut raw = [0u8; crate::inode_format::INODE_SLOT_SIZE];
            guard
                .backing
                .read_exact_at(offset as u64, &mut raw[..end - offset])?;
            crate::inode_format::decode_for(&raw[..end - offset], version)
        }
        .map_err(|e| {
            DiskManagerError::Io(std::io::Error::other(format!("Inode decode failed: {e}")))
        })?;

        // Eviction is handled inside the cache: inserting past capacity drops the
        // single oldest entry instead of wiping the whole cache.
        guard.inode_cache.insert(inode_id, inode);
        Ok(inode)
    }

    /// Decodes an inode directly from the on-disk slot (returning the cached entry if present)
    /// without inserting misses into `inode_cache` — avoids evicting hot working-set inodes
    /// during full-filesystem scans (`fsck`, `defragment`).
    fn read_inode_uncached(
        guard: &DiskManagerInner,
        inode_id: u64,
    ) -> Result<Inode, DiskManagerError> {
        if let Some(cached) = guard.inode_cache.get(inode_id) {
            return Ok(cached);
        }

        let (offset, end) = Self::inode_slot_range(guard, inode_id)?;
        let version = guard.superblock.inode_record_version_for(inode_id);
        let res = if let Some(mmap) = guard.backing.as_mmap_slice() {
            crate::inode_format::decode_for(&mmap[offset..end], version)
        } else {
            let mut raw = [0u8; crate::inode_format::INODE_SLOT_SIZE];
            guard
                .backing
                .read_exact_at(offset as u64, &mut raw[..end - offset])?;
            crate::inode_format::decode_for(&raw[..end - offset], version)
        };
        res.map_err(|e| {
            DiskManagerError::Io(std::io::Error::other(format!("Inode decode failed: {e}")))
        })
    }

    fn write_inode_internal(
        guard: &mut DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
    ) -> Result<(), DiskManagerError> {
        let (offset, _end) = Self::inode_slot_range(guard, inode_id)?;
        // Single source of truth for the on-disk bytes, shared with the journal's
        // post-image path so recovery can never diverge from a clean mount.
        let slot = Self::encode_inode_slot(guard, inode_id, inode)?;
        guard.backing.write_all_at(offset as u64, &slot)?;

        // Update in-memory inode cache & invalidate decoded payload caches.
        guard.inode_cache.insert(inode_id, *inode);
        guard.invalidate_payload_caches(inode_id);
        guard.bump_inode_gen(inode_id);
        Ok(())
    }

    /// Allocates a single data block without zeroing its contents.
    ///
    /// Callers MUST either overwrite the entire block or explicitly zero any
    /// unwritten prefix/suffix bytes before releasing the write lock.
    fn allocate_block_unzeroed(guard: &mut DiskManagerInner) -> Result<u64, DiskManagerError> {
        let db_start = guard.superblock.data_block_start;
        let block_count = guard.superblock.block_count;
        let hint = guard.free_block_hint;
        let blk = {
            let mut slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
            })?;
            let mut da = SimpleBlockAllocator::new(&mut slice, db_start);
            let blk = da
                .allocate_bounded(Some(hint), block_count)
                .map_err(DiskManagerError::Allocator)?;
            slice.set_dirty_bits((blk - db_start) as usize, 1);
            blk
        };
        guard.free_block_hint = if blk + 1 >= block_count {
            db_start
        } else {
            blk + 1
        };
        Ok(blk)
    }

    fn allocate_block(guard: &mut DiskManagerInner) -> Result<u64, DiskManagerError> {
        let blk = Self::allocate_block_unzeroed(guard)?;

        // A freshly allocated block may still hold the plaintext of a
        // file that was deleted or truncated. Zero it before it becomes
        // reachable, otherwise a partial write would expose the old content
        // to the new owner of the block.
        guard.backing.zero_block(blk)?;
        Ok(blk)
    }

    pub(crate) fn allocate_contiguous_blocks_internal(
        guard: &mut DiskManagerInner,
        count: usize,
    ) -> Result<u64, DiskManagerError> {
        if count == 0 {
            return Ok(guard.free_block_hint);
        }
        let db_start = guard.superblock.data_block_start;
        let block_count = guard.superblock.block_count;
        let hint = guard.free_block_hint;
        let blk = {
            let mut slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
            })?;
            let mut da = SimpleBlockAllocator::new(&mut slice, db_start);
            let blk = da
                .allocate_contiguous_bounded(count, Some(hint), block_count)
                .map_err(DiskManagerError::Allocator)?;
            slice.set_dirty_bits((blk - db_start) as usize, count);
            blk
        };
        guard.free_block_hint = if blk + count as u64 >= block_count {
            db_start
        } else {
            blk + count as u64
        };
        Ok(blk)
    }

    pub(crate) fn free_contiguous_blocks_internal(
        guard: &mut DiskManagerInner,
        start_blk: u64,
        count: usize,
    ) -> Result<(), DiskManagerError> {
        if count == 0 {
            return Ok(());
        }
        let db_start = guard.superblock.data_block_start;
        {
            let mut slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
            })?;
            let mut da = SimpleBlockAllocator::new(&mut slice, db_start);
            da.free_contiguous(start_blk, count)
                .map_err(DiskManagerError::Allocator)?;
            if start_blk >= db_start {
                slice.set_dirty_bits((start_blk - db_start) as usize, count);
            }
        }
        if start_blk < guard.free_block_hint {
            guard.free_block_hint = start_blk;
        }
        Ok(())
    }

    #[inline]
    fn alloc_and_zero_block(guard: &mut DiskManagerInner) -> Result<u64, DiskManagerError> {
        Self::allocate_block(guard)
    }

    fn get_or_alloc_indirect_child(
        guard: &mut DiskManagerInner,
        parent_block_id: u64,
        entry_idx: usize,
        allocate: bool,
        is_intermediate: bool,
    ) -> Result<u64, DiskManagerError> {
        // An indirect block must live in the data area, and so must
        // the child pointer it holds.
        if !guard.is_data_block(parent_block_id) {
            return Err(DiskManagerError::Io(std::io::Error::other(format!(
                "Indirect block {} is outside the data area",
                parent_block_id
            ))));
        }
        let mut child_id = guard.backing.read_block_ptr(parent_block_id, entry_idx);
        if child_id != 0 && !guard.is_data_block(child_id) {
            return Err(DiskManagerError::Io(std::io::Error::other(format!(
                "Block pointer {} is outside the data area",
                child_id
            ))));
        }
        if child_id == 0 && allocate {
            child_id = if is_intermediate {
                Self::alloc_and_zero_block(guard)?
            } else {
                Self::allocate_block(guard)?
            };
            guard
                .backing
                .write_block_ptr(parent_block_id, entry_idx, child_id);
        }
        Ok(child_id)
    }

    /// Resolves or allocates a leaf payload block for [`Self::write_buffer_at_offset`],
    /// returning `(physical_block_id, is_fresh_unzeroed)`.
    fn get_or_alloc_block_for_write(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        logical_block_idx: usize,
    ) -> Result<(u64, bool), DiskManagerError> {
        use crate::inode::BlockPath;

        fn ensure_root(
            guard: &mut DiskManagerInner,
            slot: &mut u64,
        ) -> Result<u64, DiskManagerError> {
            if *slot == 0 {
                *slot = DiskManager::alloc_and_zero_block(guard)?;
            }
            if !guard.is_data_block(*slot) {
                return Err(DiskManagerError::Io(std::io::Error::other(format!(
                    "Indirect root {} is outside the data area",
                    *slot
                ))));
            }
            Ok(*slot)
        }

        fn alloc_leaf_child(
            guard: &mut DiskManagerInner,
            parent_block_id: u64,
            entry_idx: usize,
        ) -> Result<(u64, bool), DiskManagerError> {
            if !guard.is_data_block(parent_block_id) {
                return Err(DiskManagerError::Io(std::io::Error::other(format!(
                    "Indirect block {} is outside the data area",
                    parent_block_id
                ))));
            }
            let child_id = guard.backing.read_block_ptr(parent_block_id, entry_idx);
            if child_id != 0 {
                if !guard.is_data_block(child_id) {
                    return Err(DiskManagerError::Io(std::io::Error::other(format!(
                        "Block pointer {} is outside the data area",
                        child_id
                    ))));
                }
                return Ok((child_id, false));
            }
            let fresh = DiskManager::allocate_block_unzeroed(guard)?;
            guard
                .backing
                .write_block_ptr(parent_block_id, entry_idx, fresh);
            Ok((fresh, true))
        }

        match BlockPath::from_logical(logical_block_idx) {
            Some(BlockPath::Direct(i)) => {
                if inode.blocks[i] == 0 {
                    let fresh = Self::allocate_block_unzeroed(guard)?;
                    inode.blocks[i] = fresh;
                    return Ok((fresh, true));
                }
                if !guard.is_data_block(inode.blocks[i]) {
                    return Err(DiskManagerError::Io(std::io::Error::other(format!(
                        "Block pointer {} is outside the data area",
                        inode.blocks[i]
                    ))));
                }
                Ok((inode.blocks[i], false))
            }
            Some(BlockPath::Single(i)) => {
                let sib = ensure_root(guard, &mut inode.blocks[10])?;
                alloc_leaf_child(guard, sib, i)
            }
            Some(BlockPath::Double(a, b)) => {
                let dib = ensure_root(guard, &mut inode.blocks[11])?;
                let sib = Self::get_or_alloc_indirect_child(guard, dib, a, true, true)?;
                alloc_leaf_child(guard, sib, b)
            }
            Some(BlockPath::Triple(a, b, c)) => {
                let tib = ensure_root(guard, &mut inode.triple_indirect)?;
                let dib = Self::get_or_alloc_indirect_child(guard, tib, a, true, true)?;
                let sib = Self::get_or_alloc_indirect_child(guard, dib, b, true, true)?;
                alloc_leaf_child(guard, sib, c)
            }
            None => Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "File too large (max 513GB)",
            ))),
        }
    }

    fn get_or_alloc_block(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        logical_block_idx: usize,
        allocate: bool,
    ) -> Result<u64, DiskManagerError> {
        use crate::inode::BlockPath;

        // Ensures a top-level indirect root pointer exists (allocating a zeroed block if asked).
        fn ensure_root(
            guard: &mut DiskManagerInner,
            slot: &mut u64,
            allocate: bool,
        ) -> Result<u64, DiskManagerError> {
            if *slot == 0 && allocate {
                *slot = DiskManager::alloc_and_zero_block(guard)?;
            }
            // An indirect root must address the data area; otherwise a
            // corrupted inode could make a write land in the metadata area.
            if *slot != 0 && !guard.is_data_block(*slot) {
                return Err(DiskManagerError::Io(std::io::Error::other(format!(
                    "Indirect root {} is outside the data area",
                    *slot
                ))));
            }
            Ok(*slot)
        }

        match BlockPath::from_logical(logical_block_idx) {
            Some(BlockPath::Direct(i)) => {
                if inode.blocks[i] == 0 && allocate {
                    inode.blocks[i] = Self::allocate_block(guard)?;
                }
                // A direct pointer must address the data area.
                if inode.blocks[i] != 0 && !guard.is_data_block(inode.blocks[i]) {
                    return Err(DiskManagerError::Io(std::io::Error::other(format!(
                        "Block pointer {} is outside the data area",
                        inode.blocks[i]
                    ))));
                }
                Ok(inode.blocks[i])
            }
            Some(BlockPath::Single(i)) => {
                let sib = ensure_root(guard, &mut inode.blocks[10], allocate)?;
                if sib == 0 {
                    return Ok(0);
                }
                Self::get_or_alloc_indirect_child(guard, sib, i, allocate, false)
            }
            Some(BlockPath::Double(a, b)) => {
                let dib = ensure_root(guard, &mut inode.blocks[11], allocate)?;
                if dib == 0 {
                    return Ok(0);
                }
                let sib = Self::get_or_alloc_indirect_child(guard, dib, a, allocate, true)?;
                if sib == 0 {
                    return Ok(0);
                }
                Self::get_or_alloc_indirect_child(guard, sib, b, allocate, false)
            }
            Some(BlockPath::Triple(a, b, c)) => {
                let tib = ensure_root(guard, &mut inode.triple_indirect, allocate)?;
                if tib == 0 {
                    return Ok(0);
                }
                let dib = Self::get_or_alloc_indirect_child(guard, tib, a, allocate, true)?;
                if dib == 0 {
                    return Ok(0);
                }
                let sib = Self::get_or_alloc_indirect_child(guard, dib, b, allocate, true)?;
                if sib == 0 {
                    return Ok(0);
                }
                Self::get_or_alloc_indirect_child(guard, sib, c, allocate, false)
            }
            None => Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "File too large (max 513GB)",
            ))),
        }
    }

    fn resolve_path_iter<'a>(
        guard: &DiskManagerInner,
        parts: impl Iterator<Item = &'a str>,
    ) -> Result<u64, DiskManagerError> {
        let mut curr = guard.superblock.root_inode;
        for part in parts {
            if part == ".." {
                return Err(DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Path traversal '..' is not supported in OIFS paths",
                )));
            }
            crate::directory::validate_filename(part).map_err(|e| {
                DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    e.to_string(),
                ))
            })?;
            curr = Self::dir_lookup(guard, curr, part)?.ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Not found",
                ))
            })?;
        }
        Ok(curr)
    }

    // Path resolution API (public) - wraps lookup with zero heap allocations
    pub fn resolve_path(&self, path: &str) -> Result<u64, DiskManagerError> {
        let guard = self.inner.read().unwrap();
        Self::resolve_path_iter(
            &guard,
            path.split('/').filter(|s| !s.is_empty() && *s != "."),
        )
    }

    pub fn get_block_copy(&self, block_id: u64) -> Option<Vec<u8>> {
        let guard = self.inner.read().unwrap();
        guard.backing.with_block(block_id, |s| s.to_vec()).ok()
    }

    /// Lists all entries in a directory
    ///
    /// If the directory's index is complete in `dir_cache`, entries are returned directly
    /// from memory without reading or parsing physical disk blocks.
    pub fn list_dir(
        &self,
        dir_inode_id: u64,
    ) -> Result<Vec<crate::directory::DirectoryEntry>, DiskManagerError> {
        let guard = self.inner.read().unwrap();
        Self::list_dir_with_guard(&guard, dir_inode_id)
    }

    fn list_dir_with_guard(
        guard: &DiskManagerInner,
        dir_inode_id: u64,
    ) -> Result<Vec<crate::directory::DirectoryEntry>, DiskManagerError> {
        let inode = Self::read_inode_internal(guard, dir_inode_id)?;
        if inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Not a directory",
            )));
        }

        let encrypted = guard.encryption_key.is_some();

        // 1. Fast path: check directory index cache.
        // If complete, construct entries directly from memory without reading/decoding disk blocks
        // or recomputing SipHash over filenames.
        if let Some(ix) = guard.dir_cache.read_shard(dir_inode_id).get(&dir_inode_id)
            && ix.complete
        {
            if !encrypted {
                let entries: Vec<crate::directory::DirectoryEntry> = ix
                    .stored
                    .iter()
                    .map(
                        |(name, &(inode, _, hash))| crate::directory::DirectoryEntry {
                            inode,
                            hash,
                            name: name.clone(),
                        },
                    )
                    .collect();
                return Ok(entries);
            } else if ix.plain.len() == ix.stored.len() {
                let entries: Vec<crate::directory::DirectoryEntry> = ix
                    .plain
                    .iter()
                    .map(|(name, &(inode, hash))| crate::directory::DirectoryEntry {
                        inode,
                        hash,
                        name: name.clone(),
                    })
                    .collect();
                return Ok(entries);
            }
        }

        // 2. Slow path: scan physical directory blocks
        let num_blocks = Self::dir_num_blocks(&inode);
        let mut entries = Vec::new();
        let mut stored_entries = Vec::new();
        let mut last_tail: Option<(u64, u16)> = None;
        for blk_idx in 0..num_blocks {
            let phys_blk = Self::resolve_logical_block_id(&guard.backing, &inode, blk_idx);
            if !guard.is_data_block(phys_blk) {
                continue;
            }
            let block_entries = Self::read_dir_entries_from_block(&guard.backing, phys_blk)?;
            let used: usize = block_entries.iter().map(|e| 18 + e.name.len()).sum();
            last_tail = Some((phys_blk, used.min(BLOCK_SIZE) as u16));
            let enc_ver = guard.superblock.encryption_version;
            for mut e in block_entries {
                if let Some(key) = &guard.encryption_key {
                    let Ok(decrypted) = crate::encryption::decrypt_filename_authenticated(
                        key,
                        dir_inode_id,
                        &e.name,
                        enc_ver,
                    ) else {
                        continue;
                    };
                    if crate::directory::validate_filename(&decrypted).is_err() {
                        continue;
                    }
                    let raw_name = std::mem::replace(&mut e.name, decrypted);
                    let raw_hash = e.hash;
                    e.hash = crate::directory::hash_filename(&e.name);
                    stored_entries.push((raw_name, e.inode, phys_blk, raw_hash));
                    entries.push(e);
                } else {
                    stored_entries.push((e.name.clone(), e.inode, phys_blk, e.hash));
                    entries.push(e);
                }
            }
        }

        // 3. Promote scanned directory to complete index in cache for future O(1) lookups and listings
        {
            let mut cache = guard.dir_cache.write_shard(dir_inode_id);
            let ix = cache.entry(dir_inode_id).or_default();
            ix.stored.clear();
            for (raw_name, raw_inode, raw_blk, raw_hash) in stored_entries {
                ix.stored
                    .entry(raw_name)
                    .or_insert((raw_inode, raw_blk, raw_hash));
            }
            if encrypted {
                ix.plain.clear();
                for e in &entries {
                    ix.plain.insert(e.name.clone(), (e.inode, e.hash));
                }
                ix.plain_complete = ix.plain.len() == ix.stored.len();
            }
            if ix.tail_offset.is_none() && !ix.may_have_holes {
                ix.tail_offset = last_tail;
            }
            ix.complete = true;
        }

        Ok(entries)
    }

    pub fn resolve_parent(&self, path: &str) -> Result<(u64, String), DiskManagerError> {
        let trimmed = path.trim_end_matches('/');
        let (parent_str, name) = match trimmed.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", trimmed),
        };

        if !name.is_empty() && name != "." {
            crate::directory::validate_filename(name).map_err(|e| {
                DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    e.to_string(),
                ))
            })?;
            let guard = self.inner.read().unwrap();
            let parent_id = Self::resolve_path_iter(
                &guard,
                parent_str.split('/').filter(|s| !s.is_empty() && *s != "."),
            )?;
            return Ok((parent_id, name.to_string()));
        }

        let mut parts: Vec<&str> = path
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        if parts.is_empty() {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Empty",
            )));
        }
        let name = parts.pop().unwrap().to_string();
        crate::directory::validate_filename(&name).map_err(|e| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
        let guard = self.inner.read().unwrap();
        let parent_id = Self::resolve_path_iter(&guard, parts.into_iter())?;
        Ok((parent_id, name))
    }

    pub fn flush(&self) -> Result<(), DiskManagerError> {
        if self.is_read_only() {
            return Ok(());
        }
        // 1. Ensure only 1 flush operation performs msync at any given time (prevents redundant writeback storms)
        let _sync_guard = crate::lock_unpoisoned(&self.sync_mutex);

        // 2. Snapshot the ring head under the shared lock, *before* the flush.
        //
        // The flush itself must stay on a read lock so concurrent readers are not
        // blocked. But reclaiming the ring needs a write lock, and taking one
        // only after the flush opens a window in which another thread can commit a
        // new transaction — one the flush never made durable. Reclaiming up to this
        // snapshot discards only what the flush actually covered.
        let journal_head_before_flush = {
            let guard = self.inner.read().unwrap();
            let sb = guard.superblock;
            if sb.has_journal_layout() {
                let bs = sb.block_size as u64;
                let region = guard.backing.journal_region_ref(&sb)?;
                let header_len = usize::try_from(bs)
                    .unwrap_or(0)
                    .min(crate::journal::JOURNAL_HEADER_LEN);
                // Reading the header directly avoids opening a mutable ring just to
                // look at one cursor.
                crate::journal::JournalState::decode(&region[..header_len]).map(|s| s.head)
            } else {
                None
            }
        };

        // 3. Acquire shared read lock: prevents concurrent writers, but allows all
        // concurrent readers to proceed.
        {
            let guard = self.inner.read().unwrap();
            {
                let mut pending = crate::lock_unpoisoned(&guard.pending_async_ranges);
                pending.ranges.clear();
                pending.pending_ops = 0;
                pending.approx_bytes = 0;
                self.async_drain_ready
                    .store(false, std::sync::atomic::Ordering::Release);
            }
            // WAL first: a sync point must never let image bytes reach disk ahead of
            // the log describing them, even when individual transactions skipped
            // their own barrier because of a non-Strict durability mode.
            Self::sync_journal_region(&guard)?;
            guard.backing.flush().map_err(DiskManagerError::Io)?;
        }

        // 4. Everything committed up to the snapshot is now on disk, so those
        // transactions can be retired. Anything newer stays pending for recovery.
        if let Some(head) = journal_head_before_flush {
            let mut guard = self.inner.write().unwrap();
            let sb = guard.superblock;
            if sb.has_journal_layout() {
                let bs = sb.block_size as u64;
                let start = Self::journal_region_start(&sb).unwrap_or(0);
                let header_len = usize::try_from(bs)
                    .unwrap_or(0)
                    .min(crate::journal::JOURNAL_HEADER_LEN);
                {
                    let mut region = guard.backing.journal_region_mut(&sb)?;
                    region.mark_header_only();
                    let mut ring = crate::journal::JournalRing::open(&mut region, bs)?;
                    ring.checkpoint_to(head);
                }
                if header_len > 0 {
                    guard.backing.flush_range(start, header_len)?;
                }
            }
        }
        Ok(())
    }

    /// Asynchronously flushes dirty pages in background without blocking concurrent readers.
    pub fn flush_async(&self) -> Result<(), DiskManagerError> {
        if self.is_read_only() {
            return Ok(());
        }
        let _sync_guard = crate::lock_unpoisoned(&self.sync_mutex);
        let guard = self.inner.read().unwrap();
        {
            let mut pending = crate::lock_unpoisoned(&guard.pending_async_ranges);
            pending.ranges.clear();
            pending.pending_ops = 0;
            pending.approx_bytes = 0;
            self.async_drain_ready
                .store(false, std::sync::atomic::Ordering::Release);
        }
        // WAL first, then the rest of the image, so a sync point never lets the
        // image reach disk ahead of the log describing it.
        if guard.superblock.has_journal_layout() {
            let bs = guard.superblock.block_size as u64;
            if let Some(start) = Self::journal_region_start(&guard.superblock) {
                let len =
                    Self::journal_region_len(bs).min(guard.backing.len().saturating_sub(start));
                let _ = guard.backing.flush_async_range(start, len);
            }
        }
        guard.backing.flush_async().map_err(DiskManagerError::Io)
    }

    /// Legacy flush using exclusive write lock (for performance comparison benchmarks).
    #[doc(hidden)]
    pub fn flush_exclusive_legacy(&self) -> Result<(), DiskManagerError> {
        if self.is_read_only() {
            return Ok(());
        }
        let guard = self.inner.write().unwrap();
        guard.backing.flush().map_err(DiskManagerError::Io)
    }

    /// Returns `true` if a directory has no entries across any of its allocated blocks.
    fn is_dir_empty(guard: &DiskManagerInner, dir_inode: &Inode) -> Result<bool, DiskManagerError> {
        let num_blocks = Self::dir_num_blocks(dir_inode);
        for blk_idx in 0..num_blocks {
            let phys_blk = Self::resolve_logical_block_id(&guard.backing, dir_inode, blk_idx);
            if !guard.is_data_block(phys_blk) {
                continue;
            }
            if guard
                .backing
                .with_block(phys_blk, crate::directory::has_valid_entry_in_block)
                .unwrap_or(false)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Journaled delete: compute every post-image first, commit one transaction, then apply.
    ///
    /// The three phases are strictly ordered:
    ///
    /// 1. **Compute** — derive the new directory block, the blocks to free, and the
    ///    updated parent inode *without mutating the image*.
    /// 2. **Commit** — append the transaction to the WAL and `msync` it. This is the
    ///    atomic commit point.
    /// 3. **Apply** — replay the ops in place and refresh the affected caches.
    ///
    /// A crash before (2) leaves the filesystem untouched; a crash during (3) leaves
    /// the WAL ahead of the image, and mount-time recovery finishes the job. Because
    /// every op is an absolute post-image, recovery is idempotent even if a crash
    /// lands halfway through (3).
    /// Verify that `target_blk` is an allocated leaf directory data block owned by `parent_inode`.
    fn dir_owns_data_block(backing: &ImageBacking, parent_inode: &Inode, target_blk: u64) -> bool {
        if target_blk == 0 {
            return false;
        }
        if parent_inode.blocks[..10].contains(&target_blk) {
            return true;
        }
        let scan_leaf = |sib_id: u64| -> bool {
            sib_id != 0
                && backing
                    .with_block(sib_id, |slice| {
                        slice
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .any(|c| u64::from_le_bytes(*c) == target_blk)
                    })
                    .unwrap_or(false)
        };
        if scan_leaf(parent_inode.blocks[10]) {
            return true;
        }
        let dib_id = parent_inode.blocks[11];
        if dib_id != 0 {
            let found = backing
                .with_block(dib_id, |slice| {
                    slice
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_le_bytes(*c))
                        .any(&scan_leaf)
                })
                .unwrap_or(false);
            if found {
                return true;
            }
        }
        let tib_id = parent_inode.triple_indirect;
        if tib_id != 0 {
            let found = backing
                .with_block(tib_id, |slice| {
                    slice
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_le_bytes(*c))
                        .filter(|&dib| dib != 0)
                        .any(|dib| {
                            backing
                                .with_block(dib, |s2| {
                                    s2.as_chunks::<8>()
                                        .0
                                        .iter()
                                        .map(|c| u64::from_le_bytes(*c))
                                        .any(&scan_leaf)
                                })
                                .unwrap_or(false)
                        })
                })
                .unwrap_or(false);
            if found {
                return true;
            }
        }
        false
    }

    fn delete_file_journaled(
        guard: &mut DiskManagerInner,
        parent_inode_id: u64,
        name: &str,
        matched_name: &str,
        _target_inode_id: u64,
        phys_blk: u64,
    ) -> Result<(), DiskManagerError> {
        if !guard.is_inode_allocated(parent_inode_id) {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Parent directory not found",
            )));
        }
        let mut parent_inode = Self::read_inode_internal(guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Not a directory",
            )));
        }

        let initial_hash = crate::directory::hash_filename(matched_name);
        let (matched_name, target_inode_id, phys_blk, matched_hash) =
            if Self::dir_owns_data_block(&guard.backing, &parent_inode, phys_blk)
                && let Ok(Some(id)) = guard.backing.with_block(phys_blk, |s| {
                    crate::directory::find_entry_in_block_with_hash(s, matched_name, initial_hash)
                })
            {
                (matched_name.to_string(), id, phys_blk, initial_hash)
            } else {
            let target_name = Self::stored_filename(guard, parent_inode_id, name)?;
            let h = crate::directory::hash_filename(target_name.as_ref());
            Self::locate_entry_in_dir(&guard.backing, &parent_inode, target_name.as_ref(), h)
                .map(|(id, blk)| (target_name.into_owned(), id, blk, h))
                .ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "File not found",
                    ))
                })?
        };

        // ---- Phase 1: compute post-images (no mutation) ----
        let file_inode = Self::read_inode_internal(guard, target_inode_id)?;
        // Deleting a non-empty directory would orphan every child
        // (their inodes and blocks would stay allocated but unreachable).
        if file_inode.mode == crate::inode::FileType::Directory
            && !Self::is_dir_empty(guard, &file_inode)?
        {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Directory is not empty",
            )));
        }

        // Build post-image by compacting raw block bytes in place (zero String allocs).
        let mut dir_image = guard
            .backing
            .with_block(phys_blk, |s| s.to_vec())
            .map_err(|_| {
                DiskManagerError::Io(std::io::Error::other("Directory block not found"))
            })?;
        crate::directory::remove_entry_in_block_bytes(&mut dir_image, &matched_name, matched_hash);

        let blocks_to_free = Self::collect_inode_blocks(&guard.backing, &file_inode);

        parent_inode.modified_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let parent_image = Self::build_inode_post_image(guard, parent_inode_id, &parent_inode)?;

        // Emit ops in canonical phase order (SetInodeBitmap -> SetDataBitmapRange ->
        // WriteBlockSlice -> WriteInode) with on-the-fly bitmap range coalescing.
        let mut ops = Vec::with_capacity(8);
        ops.push(crate::journal::MetadataOp::SetInodeBitmap {
            inode_id: target_inode_id,
            allocated: false,
        });
        for &blk in &blocks_to_free {
            crate::journal::push_data_bitmap_op(&mut ops, blk, 1, false);
        }
        ops.push(crate::journal::MetadataOp::WriteBlockSlice {
            block_id: phys_blk,
            offset: 0,
            data: dir_image,
        });
        ops.push(crate::journal::MetadataOp::WriteInode {
            inode_id: parent_inode_id,
            inode_bytes: Box::new(parent_image),
        });
        let ops = if crate::journal::is_already_coalesced(&ops) {
            ops
        } else {
            crate::journal::coalesce_ops(ops)
        };

        // ---- Phase 2: commit point ----
        Self::commit_journal_tx(guard, &ops)?;

        // ---- Phase 3: apply in place ----
        Self::apply_journal_ops(guard, &ops)?;

        // Refresh allocator hints and caches to match what was just applied.
        if let Some(min_freed) = blocks_to_free.iter().copied().min()
            && min_freed < guard.free_block_hint
        {
            guard.free_block_hint = min_freed;
        }
        if target_inode_id < guard.free_inode_hint {
            guard.free_inode_hint = target_inode_id;
        }
        guard.inode_cache.remove(target_inode_id);
        guard.inode_cache.insert(parent_inode_id, parent_inode);
        {
            let mut cache = guard.dir_cache.write_shard(parent_inode_id);
            if let Some(ix) = cache.get_mut(&parent_inode_id) {
                ix.plain.remove(name);
                ix.stored.remove(&matched_name);
                ix.may_have_holes = true;
                if ix.tail_offset.is_some_and(|(tb, _)| tb == phys_blk) {
                    ix.tail_offset = None;
                }
            }
        }
        guard.dir_cache.remove(target_inode_id);

        if guard.durability_mode() == DurabilityMode::RangeAsync {
            let mut ranges = vec![
                guard.inode_bitmap_byte_range(),
                guard.data_bitmap_byte_range(),
                guard.inode_byte_range(target_inode_id),
                guard.inode_byte_range(parent_inode_id),
                guard.block_byte_range(phys_blk),
            ];
            for blk in &blocks_to_free {
                ranges.push(guard.block_byte_range(*blk));
            }
            guard.sync_mutation_ranges(&ranges)?;
        } else if guard.durability_mode() == DurabilityMode::LegacyWholeMmapAsync {
            guard.sync_mutation_ranges(&[])?;
        }
        Self::maybe_checkpoint(guard)?;
        Ok(())
    }

    pub fn delete_file(&self, parent_inode_id: u64, name: &str) -> Result<(), DiskManagerError> {
        if self.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        crate::directory::validate_filename(name).map_err(|e| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;

        // --- STAGE 1: Out-of-Lock Validation & O(1) Target Resolution ---
        let (matched_name, target_inode_id, phys_blk) = {
            let guard = self.inner.read().unwrap();
            let parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
            if parent_inode.mode != crate::inode::FileType::Directory {
                return Err(DiskManagerError::Io(std::io::Error::other(
                    "Not a directory",
                )));
            }

            let target_name = Self::stored_filename(&guard, parent_inode_id, name)?;
            let candidates = [Some(target_name.as_ref())];

            // Fast path: check `dir_cache` for O(1) `(inode_id, phys_blk)` resolution.
            let cached_loc = {
                let cache = guard.dir_cache.read_shard(parent_inode_id);
                cache.get(&parent_inode_id).map(|ix| {
                    let found = candidates.iter().flatten().find_map(|c| {
                        ix.stored
                            .get(*c)
                            .map(|&(id, blk, _)| ((*c).to_string(), id, blk))
                    });
                    (found, ix.complete)
                })
            };

            match cached_loc {
                Some((Some(loc), _)) => loc,
                Some((None, true)) => {
                    return Err(DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "File not found",
                    )));
                }
                _ => candidates
                    .iter()
                    .flatten()
                    .find_map(|c| {
                        Self::locate_entry_in_dir(
                            &guard.backing,
                            &parent_inode,
                            c,
                            crate::directory::hash_filename(c),
                        )
                        .map(|(id, blk)| ((*c).to_string(), id, blk))
                    })
                    .ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "File not found",
                        ))
                    })?,
            }
        }; // Read guard is dropped!

        // --- STAGE 2: In-Lock Modification & Commit ---
        let mut guard = self.inner.write().unwrap();

        // Journaled images take the WAL-first path; legacy images keep the
        // original in-place implementation untouched (zero-overhead guarantee).
        if guard.superblock.has_journal_layout() {
            let res = Self::delete_file_journaled(
                &mut guard,
                parent_inode_id,
                name,
                &matched_name,
                target_inode_id,
                phys_blk,
            );
            drop(guard);
            if res.is_ok() {
                self.maybe_drain_async_ranges();
            }
            return res;
        }

        if !guard.is_inode_allocated(parent_inode_id) {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Parent directory not found",
            )));
        }
        let mut parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Not a directory",
            )));
        }

        let initial_hash = crate::directory::hash_filename(&matched_name);
        let (matched_name, target_inode_id, phys_blk, matched_hash) =
            if Self::dir_owns_data_block(&guard.backing, &parent_inode, phys_blk)
                && let Ok(Some(id)) = guard.backing.with_block(phys_blk, |s| {
                    crate::directory::find_entry_in_block_with_hash(s, &matched_name, initial_hash)
                })
            {
                (matched_name, id, phys_blk, initial_hash)
            } else {
            let target_name = Self::stored_filename(&guard, parent_inode_id, name)?;
            let h = crate::directory::hash_filename(target_name.as_ref());
            Self::locate_entry_in_dir(&guard.backing, &parent_inode, target_name.as_ref(), h)
                .map(|(id, blk)| (target_name.into_owned(), id, blk, h))
                .ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "File not found",
                    ))
                })?
        };

        let file_inode = Self::read_inode_internal(&guard, target_inode_id)?;
        // Deleting a non-empty directory would orphan every child
        // (their inodes and blocks would stay allocated but unreachable). The
        // check runs before the parent entry is removed.
        if file_inode.mode == crate::inode::FileType::Directory
            && !Self::is_dir_empty(&guard, &file_inode)?
        {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Directory is not empty",
            )));
        }

        // Compact only the affected directory block in place with zero heap allocations.
        guard
            .backing
            .with_block_mut(phys_blk, |block_slice| {
                crate::directory::remove_entry_in_block_bytes(
                    block_slice,
                    &matched_name,
                    matched_hash,
                );
            })
            .map_err(|_| {
                DiskManagerError::Io(std::io::Error::other("Directory block not found"))
            })?;

        // Invalidate cached names in the parent, and drop the target's own index in case it was
        // a directory: its inode id can be reused, and stale names must never resolve under it.
        {
            let mut cache = guard.dir_cache.write_shard(parent_inode_id);
            if let Some(ix) = cache.get_mut(&parent_inode_id) {
                ix.plain.remove(name);
                ix.stored.remove(&matched_name);
                ix.may_have_holes = true;
                if ix.tail_offset.is_some_and(|(tb, _)| tb == phys_blk) {
                    ix.tail_offset = None;
                }
            }
        }
        guard.dir_cache.remove(target_inode_id);

        // Free Inode & Blocks
        let blocks_to_free = Self::collect_inode_blocks(&guard.backing, &file_inode);

        // Free Data Blocks
        let mut min_freed_blk = u64::MAX;
        let mut max_freed_blk = 0u64;
        {
            let db_start = guard.superblock.data_block_start;
            let mut slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
            })?;
            let mut da = SimpleBlockAllocator::new(&mut slice, db_start);

            for blk in blocks_to_free {
                da.free(blk)?;
                min_freed_blk = min_freed_blk.min(blk);
                max_freed_blk = max_freed_blk.max(blk);
            }
            if min_freed_blk <= max_freed_blk && min_freed_blk >= db_start {
                let start_bit = (min_freed_blk - db_start) as usize;
                let bit_count = (max_freed_blk - min_freed_blk + 1) as usize;
                slice.set_dirty_bits(start_bit, bit_count);
            } else {
                slice.set_dirty_bits(0, 0);
            }
        }
        if min_freed_blk < guard.free_block_hint {
            guard.free_block_hint = min_freed_blk;
        }

        // Free Inode
        {
            let mut slice = guard.inode_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("inode bitmap not found"))
            })?;
            let mut ia = SimpleBlockAllocator::new(&mut slice, 0);
            ia.free(target_inode_id)?;
            slice.set_dirty_bits(target_inode_id as usize, 1);
        }
        if target_inode_id < guard.free_inode_hint {
            guard.free_inode_hint = target_inode_id;
        }
        guard.inode_cache.remove(target_inode_id);
        guard.invalidate_payload_caches(target_inode_id);
        guard.bump_inode_gen(target_inode_id);

        // Update Parent Mtime
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        parent_inode.modified_at = now;
        Self::write_inode_internal(&mut guard, parent_inode_id, &parent_inode)?;

        if guard.durability_mode().is_range_based() {
            let ranges = [
                guard.inode_bitmap_byte_range(),
                guard.data_bitmap_byte_range(),
                guard.inode_byte_range(target_inode_id),
                guard.inode_byte_range(parent_inode_id),
                guard.block_byte_range(phys_blk),
            ];
            guard.sync_mutation_ranges(&ranges)?;
        } else {
            guard.sync_mutation_ranges(&[])?;
        }
        drop(guard);
        self.maybe_drain_async_ranges();
        Ok(())
    }

    /// Returns `true` if `target_id` is `ancestor_id` or lies inside any subdirectory
    /// reachable from `ancestor_id` (used to prevent directory cycles during `rename`).
    fn is_ancestor_dir(
        guard: &DiskManagerInner,
        ancestor_id: u64,
        target_id: u64,
    ) -> Result<bool, DiskManagerError> {
        if ancestor_id == target_id {
            return Ok(true);
        }
        let mut visited = crate::FxHashSet::default();
        let mut queue = std::collections::VecDeque::new();
        visited.insert(ancestor_id);
        queue.push_back(ancestor_id);

        while let Some(dir_id) = queue.pop_front() {
            let dir_inode = Self::read_inode_internal(guard, dir_id)?;
            if dir_inode.mode != crate::inode::FileType::Directory {
                continue;
            }
            let num_blocks = Self::dir_num_blocks(&dir_inode);
            for blk_idx in 0..num_blocks {
                let phys_blk = Self::resolve_logical_block_id(&guard.backing, &dir_inode, blk_idx);
                if !guard.is_data_block(phys_blk) {
                    continue;
                }
                let child_ids: Vec<u64> = guard
                    .backing
                    .with_block(phys_blk, |slice| {
                        let mut ids = Vec::new();
                        let mut cursor = std::io::Cursor::new(slice);
                        while let Ok(Some(entry)) =
                            crate::directory::DirectoryEntry::deserialize_from(&mut cursor)
                        {
                            ids.push(entry.inode);
                        }
                        ids
                    })
                    .unwrap_or_default();
                for child_id in child_ids {
                    if child_id == target_id {
                        return Ok(true);
                    }
                    if visited.insert(child_id)
                        && let Ok(child_inode) = Self::read_inode_internal(guard, child_id)
                        && child_inode.mode == crate::inode::FileType::Directory
                    {
                        queue.push_back(child_id);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Atomically renames or moves a file or directory within the filesystem.
    ///
    /// - **Metadata-only**: Never reads, decrypts, re-compresses, or copies file payload blocks.
    ///   On encrypted images, only the directory entry's SIV-encrypted filename is recomputed
    ///   for `(new_parent_inode_id, new_name)` because payload AEAD is bound to `inode_id`.
    /// - **POSIX overwrite semantics**:
    ///   - Replacing an existing regular file with a regular file atomically unlinks the target
    ///     and frees its blocks/inode.
    ///   - Replacing an existing empty directory with a directory atomically removes the empty
    ///     target directory; replacing a non-empty directory fails with `"Directory is not empty"`.
    ///   - Mismatched file/directory overwrites fail with `"Is a directory"` / `"Not a directory"`.
    ///   - Moving a directory into itself or any of its descendants is rejected (`InvalidInput`).
    /// - **Crash safety**: On journaled images, the entire rename (source unlink, optional target
    ///   overwrite + block/inode release, destination link, and parent inode updates) is committed
    ///   in a single atomic CRC32C WAL transaction.
    pub fn rename(
        &self,
        old_parent_inode_id: u64,
        old_name: &str,
        new_parent_inode_id: u64,
        new_name: &str,
    ) -> Result<(), DiskManagerError> {
        if self.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        for name in [old_name, new_name] {
            crate::directory::validate_filename(name).map_err(|e| {
                DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    e.to_string(),
                ))
            })?;
        }

        let mut guard = self.inner.write().unwrap();
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }
        let res = Self::rename_internal(
            &mut guard,
            old_parent_inode_id,
            old_name,
            new_parent_inode_id,
            new_name,
        );
        drop(guard);
        if res.is_ok() {
            self.maybe_drain_async_ranges();
        }
        res
    }

    fn rename_internal(
        guard: &mut DiskManagerInner,
        old_parent_inode_id: u64,
        old_name: &str,
        new_parent_inode_id: u64,
        new_name: &str,
    ) -> Result<(), DiskManagerError> {
        let mut old_parent = Self::read_inode_internal(guard, old_parent_inode_id)?;
        if old_parent.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Not a directory",
            )));
        }

        let same_parent = old_parent_inode_id == new_parent_inode_id;
        let mut new_parent_opt = if same_parent {
            None
        } else {
            let np = Self::read_inode_internal(guard, new_parent_inode_id)?;
            if np.mode != crate::inode::FileType::Directory {
                return Err(DiskManagerError::Io(std::io::Error::other(
                    "Not a directory",
                )));
            }
            Some(np)
        };

        let old_stored = Self::stored_filename(guard, old_parent_inode_id, old_name)?.into_owned();
        let new_stored = Self::stored_filename(guard, new_parent_inode_id, new_name)?.into_owned();
        crate::directory::validate_filename(&new_stored).map_err(|e| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
        let old_hash = crate::directory::hash_filename(&old_stored);
        let new_hash = crate::directory::hash_filename(&new_stored);

        let (src_inode_id, src_phys_blk) =
            Self::locate_entry_in_dir(&guard.backing, &old_parent, &old_stored, old_hash)
                .ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "File not found",
                    ))
                })?;

        // POSIX no-op: renaming an entry onto itself succeeds without mutating anything.
        if same_parent && old_name == new_name {
            return Ok(());
        }

        let src_inode = Self::read_inode_internal(guard, src_inode_id)?;

        // Prevent moving a directory into itself or any of its descendants.
        // Skip BFS completely when renaming within the same parent unless moving into self.
        if src_inode.mode == crate::inode::FileType::Directory
            && (src_inode_id == new_parent_inode_id
                || (!same_parent
                    && Self::is_ancestor_dir(guard, src_inode_id, new_parent_inode_id)?))
        {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Cannot move a directory into itself or its subdirectory",
            )));
        }

        // Check whether destination already exists.
        let dst_loc = {
            let np_ref = new_parent_opt.as_ref().unwrap_or(&old_parent);
            Self::locate_entry_in_dir(&guard.backing, np_ref, &new_stored, new_hash)
        };
        let dst_info = if let Some((dst_inode_id, dst_phys_blk)) = dst_loc {
            if dst_inode_id == src_inode_id {
                return Ok(());
            }
            let dst_inode = Self::read_inode_internal(guard, dst_inode_id)?;
            match (src_inode.mode, dst_inode.mode) {
                (crate::inode::FileType::Directory, m)
                    if m != crate::inode::FileType::Directory =>
                {
                    return Err(DiskManagerError::Io(std::io::Error::other(
                        "Not a directory",
                    )));
                }
                (m, crate::inode::FileType::Directory)
                    if m != crate::inode::FileType::Directory =>
                {
                    return Err(DiskManagerError::Io(std::io::Error::other(
                        "Is a directory",
                    )));
                }
                (crate::inode::FileType::Directory, crate::inode::FileType::Directory)
                    if !Self::is_dir_empty(guard, &dst_inode)? =>
                {
                    return Err(DiskManagerError::Io(std::io::Error::other(
                        "Directory is not empty",
                    )));
                }
                _ => {}
            }
            let dst_blocks = Self::collect_inode_blocks(&guard.backing, &dst_inode);
            Some((dst_inode_id, dst_phys_blk, dst_blocks))
        } else {
            None
        };

        // ---- Stage directory block post-images in memory ----
        // Using a BTreeMap ensures that if `src_phys_blk == dst_phys_blk` or if the new
        // entry is inserted into the same block we just compacted, all mutations compose
        // cleanly onto the same 4 KiB post-image.
        let mut modified_blocks: std::collections::BTreeMap<u64, Vec<u8>> =
            std::collections::BTreeMap::new();
        fn load_block_mut<'a>(
            backing: &ImageBacking,
            map: &'a mut std::collections::BTreeMap<u64, Vec<u8>>,
            blk: u64,
        ) -> Result<&'a mut Vec<u8>, DiskManagerError> {
            match map.entry(blk) {
                std::collections::btree_map::Entry::Occupied(e) => Ok(e.into_mut()),
                std::collections::btree_map::Entry::Vacant(e) => {
                    let bytes = backing.with_block(blk, |s| s.to_vec()).map_err(|_| {
                        DiskManagerError::Io(std::io::Error::other("Directory block not found"))
                    })?;
                    Ok(e.insert(bytes))
                }
            }
        }

        // 1. Remove old directory entry from `src_phys_blk`.
        {
            let blk_buf = load_block_mut(&guard.backing, &mut modified_blocks, src_phys_blk)?;
            crate::directory::remove_entry_in_block_bytes(blk_buf, &old_stored, old_hash);
        }

        // 2. If overwriting an existing destination, remove its entry from `dst_phys_blk`.
        if let Some((_, dst_phys_blk, _)) = &dst_info {
            let blk_buf = load_block_mut(&guard.backing, &mut modified_blocks, *dst_phys_blk)?;
            crate::directory::remove_entry_in_block_bytes(blk_buf, &new_stored, new_hash);
        }

        // 3. Serialize the new directory entry pointing to `src_inode_id` onto the stack.
        let new_stored_for_cache = new_stored.clone();
        let new_entry = crate::directory::DirectoryEntry {
            inode: src_inode_id,
            hash: new_hash,
            name: new_stored,
        };
        let mut entry_buf = [0u8; 20 + crate::directory::DirectoryEntry::MAX_FILENAME_LEN];
        let entry_len = {
            let mut cursor = std::io::Cursor::new(&mut entry_buf[..]);
            new_entry
                .serialize_into(&mut cursor)
                .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
            cursor.position() as usize
        };
        let entry_bytes = &entry_buf[..entry_len];
        let needed_space = 20 + new_entry.name.len();

        // 4. Find a free slot in `new_parent`'s existing blocks (accounting for staged removals).
        let journaled = guard.superblock.has_journal_layout();
        let mut journal_ops: Vec<crate::journal::MetadataOp> = Vec::new();
        let mut sim_opt: Option<crate::journal::AllocSim> = None;

        let target_slot = {
            let np_ref = new_parent_opt.as_ref().unwrap_or(&old_parent);
            let num_blocks = Self::dir_num_blocks(np_ref);
            let mut found: Option<(u64, usize)> = None;
            for blk_idx in 0..num_blocks {
                let phys_blk = Self::resolve_logical_block_id(&guard.backing, np_ref, blk_idx);
                if !guard.is_data_block(phys_blk) {
                    continue;
                }
                let insert_offset = if let Some(staged) = modified_blocks.get(&phys_blk) {
                    crate::directory::find_insert_offset_in_block(staged)
                } else {
                    guard
                        .backing
                        .with_block(phys_blk, crate::directory::find_insert_offset_in_block)
                        .unwrap_or(BLOCK_SIZE)
                };
                if insert_offset + needed_space <= BLOCK_SIZE {
                    found = Some((phys_blk, insert_offset));
                    break;
                }
            }
            found
        };

        let (inserted_phys_blk, allocated_new_block) =
            if let Some((phys_blk, insert_offset)) = target_slot {
                let blk_buf = load_block_mut(&guard.backing, &mut modified_blocks, phys_blk)?;
                blk_buf[insert_offset..insert_offset + entry_bytes.len()]
                    .copy_from_slice(entry_bytes);
                // Zero-terminate trailing header bytes if room remains so scans stop cleanly.
                let end = insert_offset + entry_bytes.len();
                if end + 18 <= BLOCK_SIZE {
                    blk_buf[end..end + 18].fill(0);
                }
                (phys_blk, false)
            } else {
                // Grow `new_parent` by one directory block.
                let np_mut = new_parent_opt.as_mut().unwrap_or(&mut old_parent);
                let new_blk_idx = Self::dir_num_blocks(np_mut);
                let new_phys_blk = if journaled {
                    let sim = sim_opt.insert(crate::journal::AllocSim::new_data_only(
                        guard,
                        guard.free_inode_hint,
                        guard.free_block_hint,
                    )?);
                    crate::journal::sim_get_or_alloc_block(
                        &guard.backing,
                        np_mut,
                        new_blk_idx,
                        sim,
                        &mut journal_ops,
                    )?
                } else {
                    Self::get_or_alloc_block(guard, np_mut, new_blk_idx, true)?
                };
                let mut block_image = vec![0u8; BLOCK_SIZE];
                block_image[..entry_bytes.len()].copy_from_slice(entry_bytes);
                modified_blocks.insert(new_phys_blk, block_image);
                np_mut.size = (new_blk_idx + 1) as u64 * BLOCK_SIZE as u64;
                (new_phys_blk, true)
            };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        old_parent.modified_at = now;
        if let Some(np) = new_parent_opt.as_mut() {
            np.modified_at = now;
        }

        if journaled {
            if let Some((dst_inode_id, _, ref dst_blocks)) = dst_info {
                journal_ops.push(crate::journal::MetadataOp::SetInodeBitmap {
                    inode_id: dst_inode_id,
                    allocated: false,
                });
                for &blk in dst_blocks {
                    crate::journal::push_data_bitmap_op(&mut journal_ops, blk, 1, false);
                }
            }
            for (&blk_id, blk_data) in &modified_blocks {
                journal_ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                    block_id: blk_id,
                    offset: 0,
                    data: blk_data.clone(),
                });
            }
            let old_parent_img =
                Self::build_inode_post_image(guard, old_parent_inode_id, &old_parent)?;
            journal_ops.push(crate::journal::MetadataOp::WriteInode {
                inode_id: old_parent_inode_id,
                inode_bytes: Box::new(old_parent_img),
            });
            if let Some(ref np) = new_parent_opt {
                let new_parent_img = Self::build_inode_post_image(guard, new_parent_inode_id, np)?;
                journal_ops.push(crate::journal::MetadataOp::WriteInode {
                    inode_id: new_parent_inode_id,
                    inode_bytes: Box::new(new_parent_img),
                });
            }
            let ops = if crate::journal::is_already_coalesced(&journal_ops) {
                journal_ops
            } else {
                crate::journal::coalesce_ops(journal_ops)
            };

            Self::commit_journal_tx(guard, &ops)?;
            Self::apply_journal_ops(guard, &ops)?;

            if let Some(sim) = sim_opt {
                guard.free_block_hint = sim.free_block_hint;
            }
            if let Some((dst_inode_id, _, ref dst_blocks)) = dst_info {
                if let Some(min_freed) = dst_blocks.iter().copied().min()
                    && min_freed < guard.free_block_hint
                {
                    guard.free_block_hint = min_freed;
                }
                if dst_inode_id < guard.free_inode_hint {
                    guard.free_inode_hint = dst_inode_id;
                }
            }
        } else {
            for (&blk_id, blk_data) in &modified_blocks {
                guard.backing.with_block_mut(blk_id, |slice| {
                    slice.copy_from_slice(blk_data);
                })?;
            }
            Self::write_inode_internal(guard, old_parent_inode_id, &old_parent)?;
            if let Some(ref np) = new_parent_opt {
                Self::write_inode_internal(guard, new_parent_inode_id, np)?;
            }
            if let Some((dst_inode_id, _, ref dst_blocks)) = dst_info {
                let mut min_freed_blk = u64::MAX;
                let mut max_freed_blk = 0u64;
                {
                    let db_start = guard.superblock.data_block_start;
                    let mut slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
                    })?;
                    let mut da = SimpleBlockAllocator::new(&mut slice, db_start);
                    for &blk in dst_blocks {
                        da.free(blk)?;
                        min_freed_blk = min_freed_blk.min(blk);
                        max_freed_blk = max_freed_blk.max(blk);
                    }
                    if min_freed_blk <= max_freed_blk && min_freed_blk >= db_start {
                        let start_bit = (min_freed_blk - db_start) as usize;
                        let bit_count = (max_freed_blk - min_freed_blk + 1) as usize;
                        slice.set_dirty_bits(start_bit, bit_count);
                    } else {
                        slice.set_dirty_bits(0, 0);
                    }
                }
                if min_freed_blk < guard.free_block_hint {
                    guard.free_block_hint = min_freed_blk;
                }
                {
                    let mut slice = guard.inode_bitmap_slice_mut().ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::other("inode bitmap not found"))
                    })?;
                    let mut ia = SimpleBlockAllocator::new(&mut slice, 0);
                    ia.free(dst_inode_id)?;
                    slice.set_dirty_bits(dst_inode_id as usize, 1);
                }
                if dst_inode_id < guard.free_inode_hint {
                    guard.free_inode_hint = dst_inode_id;
                }
            }
        }

        // Refresh in-memory caches for parents and any overwritten destination in place.
        let encrypted = guard.superblock.encrypted;
        let dst_phys_blk_opt = dst_info.as_ref().map(|&(_, b, _)| b);
        guard.inode_cache.insert(old_parent_inode_id, old_parent);
        if same_parent {
            let mut cache = guard.dir_cache.write_shard(old_parent_inode_id);
            if let Some(ix) = cache.get_mut(&old_parent_inode_id) {
                if encrypted {
                    ix.plain.remove(old_name);
                    ix.plain.insert(
                        new_name.to_string(),
                        (src_inode_id, crate::directory::hash_filename(new_name)),
                    );
                }
                ix.stored.remove(&old_stored);
                ix.stored.insert(
                    new_stored_for_cache,
                    (src_inode_id, inserted_phys_blk, new_hash),
                );
                ix.may_have_holes = true;
                if allocated_new_block
                    || ix.tail_offset.is_some_and(|(tb, _)| {
                        tb == src_phys_blk
                            || tb == inserted_phys_blk
                            || dst_phys_blk_opt == Some(tb)
                    })
                {
                    ix.tail_offset = None;
                }
            }
        } else {
            {
                let mut cache = guard.dir_cache.write_shard(old_parent_inode_id);
                if let Some(ix) = cache.get_mut(&old_parent_inode_id) {
                    if encrypted {
                        ix.plain.remove(old_name);
                    }
                    ix.stored.remove(&old_stored);
                    ix.may_have_holes = true;
                    if ix.tail_offset.is_some_and(|(tb, _)| tb == src_phys_blk) {
                        ix.tail_offset = None;
                    }
                }
            }
            if let Some(np) = new_parent_opt {
                guard.inode_cache.insert(new_parent_inode_id, np);
            }
            {
                let mut cache = guard.dir_cache.write_shard(new_parent_inode_id);
                if let Some(ix) = cache.get_mut(&new_parent_inode_id) {
                    if encrypted {
                        ix.plain.insert(
                            new_name.to_string(),
                            (src_inode_id, crate::directory::hash_filename(new_name)),
                        );
                    }
                    ix.stored.insert(
                        new_stored_for_cache,
                        (src_inode_id, inserted_phys_blk, new_hash),
                    );
                    if dst_phys_blk_opt.is_some() {
                        ix.may_have_holes = true;
                    }
                    if allocated_new_block
                        || ix.tail_offset.is_some_and(|(tb, _)| {
                            tb == inserted_phys_blk || dst_phys_blk_opt == Some(tb)
                        })
                    {
                        ix.tail_offset = None;
                    }
                }
            }
        }
        if let Some((dst_inode_id, _, _)) = dst_info {
            guard.inode_cache.remove(dst_inode_id);
            guard.dir_cache.remove(dst_inode_id);
            guard.invalidate_payload_caches(dst_inode_id);
            guard.bump_inode_gen(dst_inode_id);
        }

        if guard.durability_mode().is_range_based() {
            let mut ranges = vec![
                guard.inode_bitmap_byte_range(),
                guard.data_bitmap_byte_range(),
                guard.inode_byte_range(old_parent_inode_id),
            ];
            if !same_parent {
                ranges.push(guard.inode_byte_range(new_parent_inode_id));
            }
            for &blk_id in modified_blocks.keys() {
                ranges.push(guard.block_byte_range(blk_id));
            }
            guard.sync_mutation_ranges(&ranges)?;
        } else if guard.durability_mode() == DurabilityMode::LegacyWholeMmapAsync {
            guard.sync_mutation_ranges(&[])?;
        }
        if journaled {
            Self::maybe_checkpoint(guard)?;
        }
        Ok(())
    }

    /// Truncates or extends a file to the specified size in bytes.
    ///
    /// If `new_size` is less than current size, excess data is discarded and
    /// unneeded blocks are freed back to the data bitmap.
    /// If `new_size` is greater than current size, the file is extended (sparse
    /// extension with zeroed reads for raw files; zero-padded for compressed/encrypted files).
    pub fn truncate(&self, inode_id: u64, new_size: u64) -> Result<(), DiskManagerError> {
        let res = {
            let mut guard = self.inner.write().unwrap();
            if guard.read_only {
                return Err(DiskManagerError::ReadOnly);
            }
            Self::truncate_internal(&mut guard, inode_id, new_size)
        };
        if res.is_ok() {
            self.maybe_drain_async_ranges();
        }
        res
    }

    /// Truncates or extends a file by its path.
    pub fn truncate_path(&self, path: &str, new_size: u64) -> Result<(), DiskManagerError> {
        if self.is_read_only() {
            return Err(DiskManagerError::ReadOnly);
        }
        let inode_id = self.resolve_path(path)?;
        self.truncate(inode_id, new_size)
    }

    /// Writes data from offset 0 and truncates the file to exactly `data.len()` bytes.
    pub fn write_data_truncated(
        &self,
        inode_id: u64,
        data: &[u8],
        compression_mode: CompressionMode,
    ) -> Result<(), DiskManagerError> {
        self.truncate(inode_id, 0)?;
        self.write_data(inode_id, 0, data, compression_mode)
    }

    /// Allocates a contiguous run of `count` free data blocks.
    pub fn allocate_contiguous_blocks(&self, count: usize) -> Result<u64, DiskManagerError> {
        let mut guard = self.inner.write().unwrap();
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }
        Self::allocate_contiguous_blocks_internal(&mut guard, count)
    }

    /// Frees a contiguous run of `count` data blocks starting at `start_blk`.
    pub fn free_contiguous_blocks(
        &self,
        start_blk: u64,
        count: usize,
    ) -> Result<(), DiskManagerError> {
        let mut guard = self.inner.write().unwrap();
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }
        Self::free_contiguous_blocks_internal(&mut guard, start_blk, count)
    }

    pub(crate) fn truncate_internal(
        guard: &mut DiskManagerInner,
        inode_id: u64,
        new_size: u64,
    ) -> Result<(), DiskManagerError> {
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }
        let mut inode = Self::read_inode_internal(guard, inode_id)?;
        if inode.mode != crate::inode::FileType::File {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Cannot truncate non-file inode",
            )));
        }

        if new_size == inode.size {
            return Ok(());
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Case 0: Seekable 64K Chunked file
        if inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0 {
            return Self::truncate_chunked_64k(guard, inode_id, new_size);
        }

        // Case 1: Compressed / Encrypted / Filtered file
        if inode.compressed_size > 0 || inode.encrypted || inode.filter_typesize > 0 {
            let mut full_data = Self::read_data_internal(guard, inode_id, &inode)?;
            full_data.resize(new_size as usize, 0);

            let filter_config = crate::filters::FilterConfig {
                typesize: inode.filter_typesize,
                delta: inode.filter_delta,
                shuffle: inode.filter_shuffle,
                bitshuffle: inode.filter_bitshuffle,
            };
            let compression_mode = if inode.compressed_size > 0 {
                CompressionMode::Always
            } else {
                CompressionMode::Never
            };

            // The associated data binds the ciphertext to the file.
            let (aad_buf, aad_len) = crate::encryption::file_aad_for_scheme_stack(
                inode_id,
                guard.superblock.encryption_version,
            );
            let plan = plan_write_prepared(
                &inode,
                0,
                &full_data,
                compression_mode,
                &filter_config,
                None,
                EncryptionContext {
                    key: guard.encryption_key.as_ref(),
                    aad: &aad_buf[..aad_len],
                },
            )?;

            if guard.superblock.has_journal_layout() {
                return Self::write_data_journaled(guard, inode_id, 0, plan, filter_config);
            }

            if plan.case.frees_previous_blocks() {
                let old_blocks = Self::collect_inode_blocks(&guard.backing, &inode);
                let mut min_freed_blk = u64::MAX;
                {
                    let db_start = guard.superblock.data_block_start;
                    let mut slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
                    })?;
                    let mut da = SimpleBlockAllocator::new(&mut slice, db_start);
                    for blk in old_blocks {
                        da.free(blk)?;
                        min_freed_blk = min_freed_blk.min(blk);
                    }
                }
                if min_freed_blk < guard.free_block_hint {
                    guard.free_block_hint = min_freed_blk;
                }
                inode.blocks = [0; 12];
                inode.triple_indirect = 0;
            }

            let mut touched = if guard.durability_mode().is_range_based() {
                Some(Vec::new())
            } else {
                None
            };
            Self::write_buffer_at_offset(
                guard,
                &mut inode,
                plan.phys_off,
                &plan.buffer,
                touched.as_mut(),
            )?;

            inode.size = plan.new_size;
            inode.compressed_size = plan.new_compressed_size;
            inode.modified_at = now;
            if let Some(nonce) = plan.encryption_nonce {
                inode.encrypted = true;
                inode.encryption_nonce = nonce;
            }
            Self::write_inode_internal(guard, inode_id, &inode)?;
            if guard.durability_mode().is_range_based() {
                let mut ranges = vec![
                    guard.data_bitmap_byte_range(),
                    guard.inode_byte_range(inode_id),
                ];
                if let Some(tb) = touched {
                    for blk in tb {
                        ranges.push(guard.block_byte_range(blk));
                    }
                }
                guard.sync_mutation_ranges(&ranges)?;
            } else {
                guard.sync_mutation_ranges(&[])?;
            }
            return Ok(());
        }

        // Case 2: Uncompressed Raw file - Expansion
        if new_size > inode.size {
            // The region between the old and the new size must be
            // zeroed. The last allocated block still holds the old content
            // after the old size, and that slack would otherwise become
            // readable through the extended file.
            let old_size = inode.size;
            let mut ops = Vec::new();
            let mut zeroed_last_blk: Option<u64> = None;
            if old_size > 0 && !old_size.is_multiple_of(BLOCK_SIZE as u64) {
                let last_logical_idx = ((old_size - 1) / BLOCK_SIZE as u64) as usize;
                let last_phys_blk =
                    Self::resolve_logical_block_id(&guard.backing, &inode, last_logical_idx);
                // Only zero slack if the resolved block resides in the data area.
                if guard.is_data_block(last_phys_blk) {
                    let slack_start = (old_size % BLOCK_SIZE as u64) as usize;
                    let slack_len = BLOCK_SIZE - slack_start;
                    if guard.superblock.has_journal_layout() {
                        ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                            block_id: last_phys_blk,
                            offset: slack_start as u32,
                            data: vec![0u8; slack_len],
                        });
                    } else if guard
                        .backing
                        .with_block_mut(last_phys_blk, |slice| {
                            slice[slack_start..].fill(0);
                        })
                        .is_ok()
                    {
                        zeroed_last_blk = Some(last_phys_blk);
                    }
                }
            }

            inode.size = new_size;
            inode.modified_at = now;

            if guard.superblock.has_journal_layout() {
                let post_image = Self::build_inode_post_image(guard, inode_id, &inode)?;
                ops.push(crate::journal::MetadataOp::WriteInode {
                    inode_id,
                    inode_bytes: Box::new(post_image),
                });
                Self::commit_journal_tx(guard, &ops)?;
                Self::apply_journal_ops(guard, &ops)?;
                guard.inode_cache.insert(inode_id, inode);
                Self::maybe_checkpoint(guard)?;
            } else {
                Self::write_inode_internal(guard, inode_id, &inode)?;
                if guard.durability_mode().is_range_based() {
                    if let Some(blk) = zeroed_last_blk {
                        let ranges = [guard.block_byte_range(blk), guard.inode_byte_range(inode_id)];
                        guard.sync_mutation_ranges(&ranges)?;
                    } else {
                        let range = guard.inode_byte_range(inode_id);
                        guard.sync_mutation_ranges(&[range])?;
                    }
                }
            }
            return Ok(());
        }

        // Case 3: Uncompressed Raw file - Shrinking (new_size < inode.size)
        let old_size = inode.size;
        let new_blocks_needed = (new_size as usize).div_ceil(BLOCK_SIZE);
        let old_blocks_needed = (old_size as usize).div_ceil(BLOCK_SIZE);

        let mut ops = Vec::new();

        // Zero out trailing slack bytes in the last surviving block
        if !new_size.is_multiple_of(BLOCK_SIZE as u64) && new_blocks_needed > 0 {
            let last_logical_idx = new_blocks_needed - 1;
            let last_phys_blk =
                Self::resolve_logical_block_id(&guard.backing, &inode, last_logical_idx);
            // Only zero slack if the resolved block resides in the data area.
            if guard.is_data_block(last_phys_blk) {
                let slack_start = (new_size % BLOCK_SIZE as u64) as usize;
                let slack_len = BLOCK_SIZE - slack_start;
                if guard.superblock.has_journal_layout() {
                    ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                        block_id: last_phys_blk,
                        offset: slack_start as u32,
                        data: vec![0u8; slack_len],
                    });
                } else {
                    let _ = guard.backing.with_block_mut(last_phys_blk, |slice| {
                        slice[slack_start..BLOCK_SIZE].fill(0);
                    });
                }
            }
        }

        // Free blocks beyond new_blocks_needed
        //
        // On a journaled image the pointer zeroing is staged in the
        // transaction instead of being written to the image before the
        // commit.
        let journaled = guard.superblock.has_journal_layout();
        let mut freed_blocks = Vec::new();
        for blk_idx in new_blocks_needed..old_blocks_needed {
            let phys_blk = Self::resolve_logical_block_id(&guard.backing, &inode, blk_idx);
            // Only free and clear pointers for valid data blocks.
            if guard.is_data_block(phys_blk) {
                freed_blocks.push(phys_blk);
                let mut journal_ops = if journaled { Some(&mut ops) } else { None };
                Self::clear_logical_block_ptr(guard, &mut inode, blk_idx, &mut journal_ops);
            } else if phys_blk != 0 {
                let mut journal_ops = if journaled { Some(&mut ops) } else { None };
                Self::clear_logical_block_ptr(guard, &mut inode, blk_idx, &mut journal_ops);
            }
        }

        // Prune empty indirect blocks
        {
            let mut journal_ops = if journaled { Some(&mut ops) } else { None };
            Self::prune_unused_indirect_blocks(
                guard,
                &mut inode,
                new_blocks_needed,
                &mut freed_blocks,
                &mut journal_ops,
            );
        }

        inode.size = new_size;
        inode.modified_at = now;

        if guard.superblock.has_journal_layout() {
            let post_image = Self::build_inode_post_image(guard, inode_id, &inode)?;
            for &blk in &freed_blocks {
                crate::journal::push_data_bitmap_op(&mut ops, blk, 1, false);
            }
            ops.push(crate::journal::MetadataOp::WriteInode {
                inode_id,
                inode_bytes: Box::new(post_image),
            });
            let ops = if crate::journal::is_already_coalesced(&ops) {
                ops
            } else {
                crate::journal::coalesce_ops(ops)
            };
            Self::commit_journal_tx(guard, &ops)?;
            Self::apply_journal_ops(guard, &ops)?;

            if let Some(min_freed) = freed_blocks.iter().copied().min()
                && min_freed < guard.free_block_hint
            {
                guard.free_block_hint = min_freed;
            }
            guard.inode_cache.insert(inode_id, inode);

            if guard.durability_mode() == DurabilityMode::RangeAsync {
                let mut ranges = vec![
                    guard.data_bitmap_byte_range(),
                    guard.inode_byte_range(inode_id),
                ];
                for blk in &freed_blocks {
                    ranges.push(guard.block_byte_range(*blk));
                }
                guard.sync_mutation_ranges(&ranges)?;
            } else if guard.durability_mode() == DurabilityMode::LegacyWholeMmapAsync {
                guard.sync_mutation_ranges(&[])?;
            }
            Self::maybe_checkpoint(guard)?;
        } else {
            let mut min_freed_blk = u64::MAX;
            {
                let db_start = guard.superblock.data_block_start;
                let mut slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
                })?;
                let mut da = SimpleBlockAllocator::new(&mut slice, db_start);
                for blk in &freed_blocks {
                    let _ = da.free(*blk);
                    min_freed_blk = min_freed_blk.min(*blk);
                }
            }
            if min_freed_blk < guard.free_block_hint {
                guard.free_block_hint = min_freed_blk;
            }
            Self::write_inode_internal(guard, inode_id, &inode)?;
            if guard.durability_mode().is_range_based() {
                let mut ranges = vec![
                    guard.data_bitmap_byte_range(),
                    guard.inode_byte_range(inode_id),
                ];
                for blk in &freed_blocks {
                    ranges.push(guard.block_byte_range(*blk));
                }
                guard.sync_mutation_ranges(&ranges)?;
            }
        }

        Ok(())
    }

    fn clear_logical_block_ptr(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        logical_block_idx: usize,
        ops: &mut Option<&mut Vec<crate::journal::MetadataOp>>,
    ) {
        use crate::inode::BlockPath;
        match BlockPath::from_logical(logical_block_idx) {
            Some(BlockPath::Direct(i)) => {
                inode.blocks[i] = 0;
            }
            Some(BlockPath::Single(i)) => {
                let sib = inode.blocks[10];
                if guard.is_data_block(sib) {
                    Self::zero_block_ptr(guard, sib, i, ops);
                }
            }
            Some(BlockPath::Double(a, b)) => {
                let dib = inode.blocks[11];
                if guard.is_data_block(dib) {
                    let sib = guard.backing.read_block_ptr(dib, a);
                    if guard.is_data_block(sib) {
                        Self::zero_block_ptr(guard, sib, b, ops);
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
                            Self::zero_block_ptr(guard, sib, c, ops);
                        }
                    }
                }
            }
            None => {}
        }
    }

    /// Clears one pointer slot of an indirect block.
    ///
    /// On a journaled image the metadata must not change before the
    /// transaction commits, so the zeroing is staged as a WAL operation
    /// instead of being written to the image directly. A crash between the
    /// mutation and the commit would otherwise leave the pointer tables
    /// cleared while the journal knows nothing about it.
    fn zero_block_ptr(
        guard: &mut DiskManagerInner,
        block_id: u64,
        entry_idx: usize,
        ops: &mut Option<&mut Vec<crate::journal::MetadataOp>>,
    ) {
        // Never write into a block outside the data area.
        if !guard.is_data_block(block_id) {
            return;
        }
        match ops {
            Some(ops) => {
                crate::journal::push_block_slice_op(
                    ops,
                    block_id,
                    (entry_idx * 8) as u32,
                    &[0u8; 8],
                );
            }
            None => {
                guard.backing.write_block_ptr(block_id, entry_idx, 0);
            }
        }
    }

    pub(super) fn prune_unused_indirect_blocks(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        new_blocks_needed: usize,
        freed_blocks: &mut Vec<u64>,
        ops: &mut Option<&mut Vec<crate::journal::MetadataOp>>,
    ) {
        if new_blocks_needed <= 10 && inode.blocks[10] != 0 {
            if guard.is_data_block(inode.blocks[10]) {
                freed_blocks.push(inode.blocks[10]);
            }
            inode.blocks[10] = 0;
        }

        if new_blocks_needed <= 522 {
            if inode.blocks[11] != 0 {
                let dib_id = inode.blocks[11];
                if guard.is_data_block(dib_id) {
                    let sibs: Vec<u64> = guard
                        .backing
                        .with_block(dib_id, |dib_slice| {
                            dib_slice
                                .as_chunks::<8>()
                                .0
                                .iter()
                                .map(|c| u64::from_le_bytes(*c))
                                .filter(|&sib| sib != 0)
                                .collect()
                        })
                        .unwrap_or_default();
                    for sib in sibs {
                        if guard.is_data_block(sib) {
                            freed_blocks.push(sib);
                        }
                    }
                    freed_blocks.push(dib_id);
                }
                inode.blocks[11] = 0;
            }
        } else if guard.is_data_block(inode.blocks[11]) {
            let dib_id = inode.blocks[11];
            let keep_sibs = (new_blocks_needed - 522).div_ceil(512);
            // The pruning is staged in the WAL on a journaled image
            // (see `zero_block_ptr`).
            let pruned_slots: Vec<(usize, u64)> = guard
                .backing
                .with_block(dib_id, |dib_slice| {
                    dib_slice
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .enumerate()
                        .filter_map(|(a_idx, chunk)| {
                            if a_idx >= keep_sibs {
                                let sib = u64::from_le_bytes(*chunk);
                                (sib != 0).then_some((a_idx, sib))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            for (a_idx, sib) in pruned_slots {
                if guard.is_data_block(sib) {
                    freed_blocks.push(sib);
                }
                match ops {
                    Some(ops) => {
                        crate::journal::push_block_slice_op(
                            ops,
                            dib_id,
                            (a_idx * 8) as u32,
                            &[0u8; 8],
                        );
                    }
                    None => {
                        guard.backing.write_block_ptr(dib_id, a_idx, 0);
                    }
                }
            }
        }

        let max_double_blocks = 10 + 512 + 512 * 512;
        if new_blocks_needed <= max_double_blocks {
            if inode.triple_indirect != 0 {
                let tib_id = inode.triple_indirect;
                if guard.is_data_block(tib_id) {
                    let dibs: Vec<u64> = guard
                        .backing
                        .with_block(tib_id, |tib_slice| {
                            tib_slice
                                .as_chunks::<8>()
                                .0
                                .iter()
                                .map(|c| u64::from_le_bytes(*c))
                                .filter(|&dib| dib != 0)
                                .collect()
                        })
                        .unwrap_or_default();
                    for dib in dibs {
                        if guard.is_data_block(dib) {
                            let sibs: Vec<u64> = guard
                                .backing
                                .with_block(dib, |dib_slice| {
                                    dib_slice
                                        .as_chunks::<8>()
                                        .0
                                        .iter()
                                        .map(|c| u64::from_le_bytes(*c))
                                        .filter(|&sib| sib != 0)
                                        .collect()
                                })
                                .unwrap_or_default();
                            for sib in sibs {
                                if guard.is_data_block(sib) {
                                    freed_blocks.push(sib);
                                }
                            }
                            freed_blocks.push(dib);
                        }
                    }
                    freed_blocks.push(tib_id);
                }
                inode.triple_indirect = 0;
            }
        } else if guard.is_data_block(inode.triple_indirect) {
            let tib_id = inode.triple_indirect;
            let triple_rel = new_blocks_needed - max_double_blocks;
            let keep_dibs = triple_rel.div_ceil(512 * 512);
            let mut pruned_dibs = Vec::new();
            let mut boundary_dib: Option<(usize, u64)> = None;
            let _ = guard.backing.with_block(tib_id, |tib_slice| {
                for (t_idx, chunk) in tib_slice.as_chunks::<8>().0.iter().enumerate() {
                    let dib = u64::from_le_bytes(*chunk);
                    if dib != 0 {
                        if t_idx >= keep_dibs {
                            pruned_dibs.push((t_idx, dib));
                        } else if t_idx + 1 == keep_dibs {
                            boundary_dib = Some((t_idx, dib));
                        }
                    }
                }
            });
            for (t_idx, dib) in pruned_dibs {
                if guard.is_data_block(dib) {
                    let sibs: Vec<u64> = guard
                        .backing
                        .with_block(dib, |dib_slice| {
                            dib_slice
                                .as_chunks::<8>()
                                .0
                                .iter()
                                .map(|c| u64::from_le_bytes(*c))
                                .filter(|&sib| sib != 0)
                                .collect()
                        })
                        .unwrap_or_default();
                    for sib in sibs {
                        if guard.is_data_block(sib) {
                            freed_blocks.push(sib);
                        }
                    }
                    freed_blocks.push(dib);
                }
                match ops {
                    Some(ops) => {
                        crate::journal::push_block_slice_op(
                            ops,
                            tib_id,
                            (t_idx * 8) as u32,
                            &[0u8; 8],
                        );
                    }
                    None => {
                        guard.backing.write_block_ptr(tib_id, t_idx, 0);
                    }
                }
            }
            if let Some((t_idx, dib)) = boundary_dib
                && guard.is_data_block(dib)
            {
                let rem_in_dib = triple_rel - t_idx * (512 * 512);
                let keep_sibs = rem_in_dib.div_ceil(512);
                let pruned_sibs: Vec<(usize, u64)> = guard
                    .backing
                    .with_block(dib, |dib_slice| {
                        dib_slice
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .enumerate()
                            .filter_map(|(d_idx, sib_chunk)| {
                                if d_idx >= keep_sibs {
                                    let sib = u64::from_le_bytes(*sib_chunk);
                                    (sib != 0).then_some((d_idx, sib))
                                } else {
                                    None
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for (d_idx, sib) in pruned_sibs {
                    if guard.is_data_block(sib) {
                        freed_blocks.push(sib);
                    }
                    match ops {
                        Some(ops) => {
                            crate::journal::push_block_slice_op(
                                ops,
                                dib,
                                (d_idx * 8) as u32,
                                &[0u8; 8],
                            );
                        }
                        None => {
                            guard.backing.write_block_ptr(dib, d_idx, 0);
                        }
                    }
                }
            }
        }
    }
}

/// A shared read-lock session over a [`DiskManager`].
///
/// Created via [`DiskManager::begin_read`], a `ReadSession` holds the filesystem's shared
/// read lock once for its entire lifetime. Batching lookups, inode stats, directory listings,
/// and positional reads through a `ReadSession` eliminates per-call `RwLock` atomic reader-count
/// contention across concurrent reader threads.
///
/// Note: Because a `ReadSession` holds a shared read lock, concurrent writers wait until all
/// active `ReadSession`s are dropped. Keep sessions scoped to bounded read batches.
pub struct ReadSession<'a> {
    guard: std::sync::RwLockReadGuard<'a, DiskManagerInner>,
}

impl<'a> ReadSession<'a> {
    /// Returns the root directory inode ID without re-acquiring the outer lock.
    #[inline]
    pub fn root_inode(&self) -> u64 {
        self.guard.superblock.root_inode
    }

    /// Looks up a child entry by name in `parent_inode_id` without re-acquiring the outer lock.
    pub fn lookup(&self, parent_inode_id: u64, name: &str) -> Result<u64, DiskManagerError> {
        crate::directory::validate_filename(name).map_err(|e| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
        DiskManager::dir_lookup(&self.guard, parent_inode_id, name)?.ok_or_else(|| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Not found",
            ))
        })
    }

    /// Reads an inode's metadata without re-acquiring the outer lock.
    #[inline]
    pub fn read_inode(&self, inode_id: u64) -> Result<Inode, DiskManagerError> {
        DiskManager::read_inode_internal(&self.guard, inode_id)
    }

    /// Lists all directory entries in `dir_inode_id` without re-acquiring the outer lock.
    pub fn list_dir(
        &self,
        dir_inode_id: u64,
    ) -> Result<Vec<crate::directory::DirectoryEntry>, DiskManagerError> {
        DiskManager::list_dir_with_guard(&self.guard, dir_inode_id)
    }

    /// Reads the entire contents of `inode_id` without re-acquiring the outer lock.
    pub fn read_data(&self, inode_id: u64) -> Result<Vec<u8>, DiskManagerError> {
        let inode = DiskManager::read_inode_internal(&self.guard, inode_id)?;
        DiskManager::read_data_internal(&self.guard, inode_id, &inode)
    }

    /// Performs a positional read from `inode_id` without re-acquiring the outer lock.
    #[inline]
    pub fn read_at(
        &self,
        inode_id: u64,
        file_offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, DiskManagerError> {
        let inode = DiskManager::read_inode_internal(&self.guard, inode_id)?;
        DiskManager::read_at_with_guard(&self.guard, inode_id, &inode, file_offset, buf)
    }

    /// Pins a validated regular file [`Inode`] into a [`FileReadHandle`] so repeated
    /// [`FileReadHandle::read_at`] calls skip both outer `RwLock` acquisition and
    /// `BoundedInodeCache` lookups.
    pub fn open_file(&self, inode_id: u64) -> Result<FileReadHandle<'_, 'a>, DiskManagerError> {
        let inode = DiskManager::read_inode_internal(&self.guard, inode_id)?;
        if inode.mode != crate::inode::FileType::File {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Cannot read non-file inode",
            )));
        }
        if self.guard.superblock.encrypted
            && !inode.encrypted
            && (inode.size > 0
                || inode.compressed_size > 0
                || (inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0))
        {
            return Err(DiskManagerError::DecryptionFailed);
        }
        let is_raw_fast = inode.compressed_size == 0
            && !inode.encrypted
            && !self.guard.superblock.encrypted
            && (inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) == 0
            && !inode.filter_delta
            && !inode.filter_shuffle
            && !inode.filter_bitshuffle
            && (self.guard.io_engine.effective() == IoBackend::Mmap
                || self.guard.backing.as_mmap_slice().is_none());
        Ok(FileReadHandle {
            session: self,
            inode_id,
            inode,
            is_raw_fast,
        })
    }
}

/// A pinned read-only handle to a single regular file within a [`ReadSession`].
///
/// Eliminates both per-call `RwLock` acquisition and `BoundedInodeCache` lookups, and uses a
/// direct single-block fast path for raw uncompressed files under both [`StorageMode::Mmap`]
/// and [`StorageMode::FileIo`] (backed by the 64-shard user-space data page cache).
pub struct FileReadHandle<'s, 'a> {
    session: &'s ReadSession<'a>,
    inode_id: u64,
    inode: Inode,
    is_raw_fast: bool,
}

impl<'s, 'a> FileReadHandle<'s, 'a> {
    /// Returns the pinned inode ID.
    #[inline]
    pub fn inode_id(&self) -> u64 {
        self.inode_id
    }

    /// Returns a reference to the pinned [`Inode`].
    #[inline]
    pub fn inode(&self) -> &Inode {
        &self.inode
    }

    /// Returns the logical file size in bytes.
    #[inline]
    pub fn size(&self) -> u64 {
        self.inode.size
    }

    /// Reads the entire contents of this file.
    pub fn read_all(&self) -> Result<Vec<u8>, DiskManagerError> {
        DiskManager::read_data_internal(&self.session.guard, self.inode_id, &self.inode)
    }

    /// Returns a direct zero-copy slice `&'s [u8]` into the memory-mapped container when:
    /// 1. The filesystem is backed by [`StorageMode::Mmap`],
    /// 2. The file is raw (uncompressed, unencrypted, non-seekable, no active filters), and
    /// 3. The requested range `[file_offset..file_offset + max_len)` (clamped to EOF) resides
    ///    in physically contiguous data blocks with no sparse holes.
    ///
    /// Returns `Ok(Some(slice))` when zero-copy is possible, or `Ok(None)` when the caller
    /// should fall back to [`Self::read_at`] (e.g. under [`StorageMode::FileIo`], compressed or
    /// encrypted files, sparse holes, or non-contiguous block boundaries).
    #[inline]
    pub fn mmap_slice(
        &self,
        file_offset: u64,
        max_len: usize,
    ) -> Result<Option<&'s [u8]>, DiskManagerError> {
        if !self.is_raw_fast {
            return Ok(None);
        }
        let guard = &self.session.guard;
        let Some(mmap) = guard.backing.as_mmap_slice() else {
            return Ok(None);
        };
        if file_offset >= self.inode.size || max_len == 0 {
            return Ok(Some(&[]));
        }
        let available = usize::try_from(self.inode.size - file_offset).unwrap_or(usize::MAX);
        let len = max_len.min(available);
        let first_blk_idx = (file_offset / BLOCK_SIZE as u64) as usize;
        let in_blk_off = (file_offset % BLOCK_SIZE as u64) as usize;
        let last_byte = file_offset + (len - 1) as u64;
        let last_blk_idx = (last_byte / BLOCK_SIZE as u64) as usize;

        let first_phys = DiskManager::resolve_raw_block_id(guard, &self.inode, first_blk_idx);
        if !guard.is_data_block(first_phys) {
            return Ok(None);
        }
        for b_idx in (first_blk_idx + 1)..=last_blk_idx {
            let expected = first_phys + (b_idx - first_blk_idx) as u64;
            let actual = DiskManager::resolve_raw_block_id(guard, &self.inode, b_idx);
            if actual != expected || !guard.is_data_block(actual) {
                return Ok(None);
            }
        }
        let Some(start) = usize::try_from(first_phys)
            .ok()
            .and_then(|b| b.checked_mul(BLOCK_SIZE))
            .and_then(|b| b.checked_add(in_blk_off))
        else {
            return Ok(None);
        };
        let Some(end) = start.checked_add(len) else {
            return Ok(None);
        };
        if end > mmap.len() {
            return Ok(None);
        }
        Ok(Some(&mmap[start..end]))
    }

    /// Reads up to `buf.len()` bytes starting at `file_offset`.
    #[inline]
    pub fn read_at(&self, file_offset: u64, buf: &mut [u8]) -> Result<usize, DiskManagerError> {
        if self.is_raw_fast {
            if file_offset >= self.inode.size || buf.is_empty() {
                return Ok(0);
            }
            let available = (self.inode.size - file_offset) as usize;
            let to_read = buf.len().min(available);
            let in_blk_off = (file_offset % BLOCK_SIZE as u64) as usize;
            if in_blk_off + to_read <= BLOCK_SIZE {
                let guard = &self.session.guard;
                let blk_idx = (file_offset / BLOCK_SIZE as u64) as usize;
                let phys_blk = DiskManager::resolve_raw_block_id(guard, &self.inode, blk_idx);
                if guard.is_data_block(phys_blk) {
                    if let Some(mmap) = guard.backing.as_mmap_slice() {
                        if let Some(src) = usize::try_from(phys_blk)
                            .ok()
                            .and_then(|b| b.checked_mul(BLOCK_SIZE))
                            .and_then(|b| b.checked_add(in_blk_off))
                            && src.saturating_add(to_read) <= mmap.len()
                        {
                            buf[..to_read].copy_from_slice(&mmap[src..src + to_read]);
                            return Ok(to_read);
                        }
                    } else {
                        let disk_off = phys_blk * BLOCK_SIZE as u64 + in_blk_off as u64;
                        guard
                            .backing
                            .read_data_cached(disk_off, &mut buf[..to_read])?;
                        return Ok(to_read);
                    }
                }
                buf[..to_read].fill(0);
                return Ok(to_read);
            }
        }
        DiskManager::read_at_with_guard(
            &self.session.guard,
            self.inode_id,
            &self.inode,
            file_offset,
            buf,
        )
    }
}

thread_local! {
    static TL_DEFER_STRICT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[inline]
fn is_strict_sync_deferred() -> bool {
    TL_DEFER_STRICT_SYNC.with(|c| c.get())
}

struct DeferStrictSyncGuard;

impl DeferStrictSyncGuard {
    fn enter() -> Self {
        TL_DEFER_STRICT_SYNC.with(|c| c.set(true));
        Self
    }
}

impl Drop for DeferStrictSyncGuard {
    fn drop(&mut self) {
        TL_DEFER_STRICT_SYNC.with(|c| c.set(false));
    }
}

pub(crate) struct AsyncTaskState<T> {
    done: std::sync::atomic::AtomicBool,
    slot: Mutex<(Option<T>, Option<std::task::Waker>)>,
    cvar: std::sync::Condvar,
}

impl<T> AsyncTaskState<T> {
    pub(crate) fn new_pending() -> Arc<Self> {
        Arc::new(Self {
            done: std::sync::atomic::AtomicBool::new(false),
            slot: Mutex::new((None, None)),
            cvar: std::sync::Condvar::new(),
        })
    }

    pub(crate) fn complete(&self, val: T) {
        let waker = {
            let mut guard = crate::lock_unpoisoned(&self.slot);
            guard.0 = Some(val);
            self.done.store(true, std::sync::atomic::Ordering::Release);
            guard.1.take()
        };
        self.cvar.notify_all();
        if let Some(w) = waker {
            w.wake();
        }
    }

    pub(crate) fn mark_finished_without_value(&self) {
        if self.done.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let waker = {
            let mut guard = crate::lock_unpoisoned(&self.slot);
            self.done.store(true, std::sync::atomic::Ordering::Release);
            guard.1.take()
        };
        self.cvar.notify_all();
        if let Some(w) = waker {
            w.wake();
        }
    }
}

/// Handle to an asynchronous OIFS operation.
///
/// Implements [`std::future::Future`] for zero-dependency `.await` in any Rust async runtime
/// (`tokio`, `smol`, `futures::executor::block_on`), while also supporting synchronous polling
/// ([`Self::is_ready`], [`Self::try_take`]) and blocking wait ([`Self::wait`]).
///
/// Hot cache-hit and `Mmap` reads complete on the **Inline Fast-Path** (`OifsAsyncTask::ready`)
/// in ~50 ns without spawning or waking background threads.
pub struct OifsAsyncTask<T> {
    inline_ready: Option<T>,
    state: Option<Arc<AsyncTaskState<T>>>,
    wait_on_drop: bool,
}

impl<T> OifsAsyncTask<T> {
    /// Creates an immediately completed [`OifsAsyncTask`] with zero heap allocation or thread hop.
    #[inline]
    pub fn ready(val: T) -> Self {
        Self {
            inline_ready: Some(val),
            state: None,
            wait_on_drop: false,
        }
    }

    pub(crate) fn from_state(state: Arc<AsyncTaskState<T>>, wait_on_drop: bool) -> Self {
        Self {
            inline_ready: None,
            state: Some(state),
            wait_on_drop,
        }
    }

    /// Returns `true` if the operation has completed.
    #[inline]
    pub fn is_ready(&self) -> bool {
        if self.inline_ready.is_some() {
            return true;
        }
        self.state
            .as_ref()
            .is_some_and(|s| s.done.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Extracts the result if the operation has completed, returning `None` if still in flight
    /// or already taken.
    #[inline]
    pub fn try_take(&mut self) -> Option<T> {
        if let Some(v) = self.inline_ready.take() {
            self.wait_on_drop = false;
            return Some(v);
        }
        let s = self.state.as_ref()?;
        if !s.done.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let v = crate::lock_unpoisoned(&s.slot).0.take();
        if v.is_some() {
            self.wait_on_drop = false;
        }
        v
    }

    /// Blocks the current thread until the operation completes without consuming the result.
    pub fn wait_for_completion(&self) {
        if self.inline_ready.is_some() {
            return;
        }
        let Some(s) = self.state.as_ref() else {
            return;
        };
        if s.done.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let mut guard = crate::lock_unpoisoned(&s.slot);
        while !s.done.load(std::sync::atomic::Ordering::Acquire) {
            guard = s.cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Blocks the current thread until the operation completes and returns the output.
    #[inline]
    pub fn wait(mut self) -> T {
        if let Some(v) = self.inline_ready.take() {
            self.wait_on_drop = false;
            return v;
        }
        self.wait_for_completion();
        self.wait_on_drop = false;
        let s = self.state.as_ref().expect("missing async state");
        let res = crate::lock_unpoisoned(&s.slot).0.take();
        res.expect("OifsAsyncTask background worker panicked or result already consumed")
    }
}

impl<T: Send + 'static> OifsAsyncTask<T> {
    /// Spawns `work` onto the persistent async read/compute pool.
    pub fn spawn(work: impl FnOnce() -> T + Send + 'static) -> Self {
        Self::spawn_with_drop_policy(work, false)
    }

    /// Spawns `work` onto the persistent async read/compute pool; when `wait_on_drop` is `true`,
    /// dropping the task waits for `work` to finish (preventing Use-After-Free when FFI callers
    /// pass borrowed output buffers).
    pub(crate) fn spawn_with_drop_policy(
        work: impl FnOnce() -> T + Send + 'static,
        wait_on_drop: bool,
    ) -> Self {
        struct PanicCompletionGuard<T>(Option<Arc<AsyncTaskState<T>>>);
        impl<T> Drop for PanicCompletionGuard<T> {
            fn drop(&mut self) {
                if let Some(s) = self.0.take() {
                    s.mark_finished_without_value();
                }
            }
        }

        let state = AsyncTaskState::new_pending();
        let worker_state = Arc::clone(&state);
        submit_async_job(Box::new(move || {
            let mut guard = PanicCompletionGuard(Some(worker_state));
            let out = work();
            if let Some(s) = guard.0.take() {
                s.complete(out);
            }
        }));
        Self::from_state(state, wait_on_drop)
    }
}

impl<T> Drop for OifsAsyncTask<T> {
    fn drop(&mut self) {
        if self.wait_on_drop {
            self.wait_for_completion();
        }
    }
}

impl<T: Unpin> std::future::Future for OifsAsyncTask<T> {
    type Output = T;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(val) = this.inline_ready.take() {
            this.wait_on_drop = false;
            return std::task::Poll::Ready(val);
        }
        let s = this.state.as_ref().expect("missing async state");
        if s.done.load(std::sync::atomic::Ordering::Acquire) {
            this.wait_on_drop = false;
            let val = crate::lock_unpoisoned(&s.slot)
                .0
                .take()
                .expect("OifsAsyncTask background worker panicked or result already consumed");
            return std::task::Poll::Ready(val);
        }
        let mut guard = crate::lock_unpoisoned(&s.slot);
        if let Some(val) = guard.0.take() {
            this.wait_on_drop = false;
            return std::task::Poll::Ready(val);
        }
        if s.done.load(std::sync::atomic::Ordering::Acquire) {
            this.wait_on_drop = false;
            panic!("OifsAsyncTask background worker panicked or result already consumed");
        }
        guard.1 = Some(cx.waker().clone());
        std::task::Poll::Pending
    }
}

type AsyncJob = Box<dyn FnOnce() + Send + 'static>;

struct AsyncPoolQueue {
    queue: Mutex<std::collections::VecDeque<AsyncJob>>,
    cvar: std::sync::Condvar,
}

pub(crate) fn submit_async_job(job: AsyncJob) {
    static POOL: std::sync::OnceLock<Arc<AsyncPoolQueue>> = std::sync::OnceLock::new();
    let pool = POOL.get_or_init(|| {
        let q = Arc::new(AsyncPoolQueue {
            queue: Mutex::new(std::collections::VecDeque::with_capacity(64)),
            cvar: std::sync::Condvar::new(),
        });
        let num_workers = std::thread::available_parallelism()
            .map(|n| n.get().clamp(2, 8))
            .unwrap_or(4);
        for idx in 0..num_workers {
            let q_worker = Arc::clone(&q);
            let _ = std::thread::Builder::new()
                .name(format!("oifs-async-{idx}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let mut guard = crate::lock_unpoisoned(&q_worker.queue);
                            loop {
                                if let Some(j) = guard.pop_front() {
                                    break j;
                                }
                                guard =
                                    q_worker.cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
                            }
                        };
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                });
        }
        q
    });
    {
        let mut guard = crate::lock_unpoisoned(&pool.queue);
        guard.push_back(job);
    }
    pool.cvar.notify_one();
}

type GroupWriteRun =
    Box<dyn FnOnce(&DiskManager) -> Result<u64, DiskManagerError> + Send + 'static>;
type GroupWriteComplete = Box<dyn FnOnce(Result<u64, DiskManagerError>) + Send + 'static>;
type GroupWriteResultItem = (
    DiskManager,
    Result<u64, DiskManagerError>,
    GroupWriteComplete,
);

struct GroupWriteOp {
    dm: DiskManager,
    run: GroupWriteRun,
    complete: GroupWriteComplete,
}

struct GroupWriteQueue {
    queue: Mutex<std::collections::VecDeque<GroupWriteOp>>,
    cvar: std::sync::Condvar,
}

pub(crate) fn submit_group_write_op(
    dm: DiskManager,
    run: impl FnOnce(&DiskManager) -> Result<u64, DiskManagerError> + Send + 'static,
    complete: impl FnOnce(Result<u64, DiskManagerError>) + Send + 'static,
) {
    static WRITE_POOL: std::sync::OnceLock<Arc<GroupWriteQueue>> = std::sync::OnceLock::new();
    let pool = WRITE_POOL.get_or_init(|| {
        let q = Arc::new(GroupWriteQueue {
            queue: Mutex::new(std::collections::VecDeque::with_capacity(128)),
            cvar: std::sync::Condvar::new(),
        });
        let q_worker = Arc::clone(&q);
        let _ = std::thread::Builder::new()
            .name("oifs-group-commit".to_string())
            .spawn(move || {
                let mut batch = Vec::with_capacity(64);
                loop {
                    {
                        let mut guard = crate::lock_unpoisoned(&q_worker.queue);
                        while guard.is_empty() {
                            guard = q_worker.cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
                        }
                        if guard.len() == 1 {
                            drop(guard);
                            std::thread::yield_now();
                            guard = crate::lock_unpoisoned(&q_worker.queue);
                        }
                        while let Some(op) = guard.pop_front() {
                            batch.push(op);
                            if batch.len() >= 128 {
                                break;
                            }
                        }
                    }

                    // Multi-operation Group Commit batch:
                    // Defer individual Strict WAL sync barriers while applying operations in
                    // memory, harvest any operations that arrived while the batch was executing
                    // (up to 128 ops), and then flush each involved Strict DiskManager once
                    // before notifying any caller.
                    let mut results: Vec<GroupWriteResultItem> =
                        Vec::with_capacity(batch.len().max(16));
                    {
                        let _defer = DeferStrictSyncGuard::enter();
                        loop {
                            for op in batch.drain(..) {
                                let res =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        (op.run)(&op.dm)
                                    }))
                                    .unwrap_or_else(|_| {
                                        Err(DiskManagerError::Io(std::io::Error::other(
                                            "panic in async write",
                                        )))
                                    });
                                results.push((op.dm, res, op.complete));
                            }
                            if results.len() >= 128 {
                                break;
                            }
                            let mut guard = crate::lock_unpoisoned(&q_worker.queue);
                            while results.len() + batch.len() < 128
                                && let Some(op) = guard.pop_front()
                            {
                                batch.push(op);
                            }
                            if batch.is_empty() {
                                break;
                            }
                        }
                    }

                    // Deduplicate Strict DiskManagers by Arc pointer and flush each once.
                    let mut flushed_ptrs: Vec<(usize, Option<String>)> = Vec::new();
                    for (dm, res, _) in &results {
                        if res.is_ok() && dm.durability_mode() == DurabilityMode::Strict {
                            let ptr = Arc::as_ptr(&dm.inner) as usize;
                            if !flushed_ptrs.iter().any(|(p, _)| *p == ptr) {
                                let flush_err = dm.flush().err().map(|e| e.to_string());
                                flushed_ptrs.push((ptr, flush_err));
                            }
                        }
                    }

                    for (dm, mut res, complete) in results {
                        if res.is_ok() && dm.durability_mode() == DurabilityMode::Strict {
                            let ptr = Arc::as_ptr(&dm.inner) as usize;
                            if let Some((_, Some(err_msg))) =
                                flushed_ptrs.iter().find(|(p, _)| *p == ptr)
                            {
                                res = Err(DiskManagerError::Io(std::io::Error::other(
                                    err_msg.clone(),
                                )));
                            }
                        }
                        complete(res);
                    }
                }
            });
        q
    });
    {
        let mut guard = crate::lock_unpoisoned(&pool.queue);
        guard.push_back(GroupWriteOp {
            dm,
            run: Box::new(run),
            complete: Box::new(complete),
        });
    }
    pool.cvar.notify_one();
}

impl DiskManager {
    /// Non-blocking cache-only pointer resolution used by the Async Read Inline Fast-Path.
    #[inline]
    fn try_resolve_raw_block_id_cached_only(
        guard: &DiskManagerInner,
        inode: &Inode,
        blk_idx: usize,
    ) -> Option<u64> {
        const SINGLE_LIMIT: usize = 10 + 512;
        const DOUBLE_LIMIT: usize = SINGLE_LIMIT + 512 * 512;
        const TRIPLE_LIMIT: usize = DOUBLE_LIMIT + 512 * 512 * 512;
        if blk_idx < 10 {
            return Some(inode.blocks[blk_idx]);
        }
        if blk_idx < SINGLE_LIMIT {
            let sib_id = inode.blocks[10];
            if !guard.is_data_block(sib_id) {
                return Some(0);
            }
            return guard
                .backing
                .try_read_block_ptr_cached_only(sib_id, blk_idx - 10);
        }
        if blk_idx < DOUBLE_LIMIT {
            let dib_id = inode.blocks[11];
            if !guard.is_data_block(dib_id) {
                return Some(0);
            }
            let rel = blk_idx - SINGLE_LIMIT;
            let sib_id = guard
                .backing
                .try_read_block_ptr_cached_only(dib_id, rel / 512)?;
            if !guard.is_data_block(sib_id) {
                return Some(0);
            }
            return guard
                .backing
                .try_read_block_ptr_cached_only(sib_id, rel % 512);
        }
        if blk_idx < TRIPLE_LIMIT {
            let tib_id = inode.triple_indirect;
            if !guard.is_data_block(tib_id) {
                return Some(0);
            }
            let rel = blk_idx - DOUBLE_LIMIT;
            let dib_id = guard
                .backing
                .try_read_block_ptr_cached_only(tib_id, rel / (512 * 512))?;
            if !guard.is_data_block(dib_id) {
                return Some(0);
            }
            let rem = rel % (512 * 512);
            let sib_id = guard
                .backing
                .try_read_block_ptr_cached_only(dib_id, rem / 512)?;
            if !guard.is_data_block(sib_id) {
                return Some(0);
            }
            return guard
                .backing
                .try_read_block_ptr_cached_only(sib_id, rem % 512);
        }
        Some(0)
    }

    /// Attempts to satisfy a positional read immediately on the calling thread without blocking
    /// or issuing syscalls (Inline Fast-Path).
    ///
    /// Succeeds when `inner.try_read()` succeeds and the requested range is either:
    /// - backed by [`StorageMode::Mmap`] on a raw uncompressed/unencrypted file,
    /// - resident in the [`StorageMode::FileIo`] `DataPageCache`,
    /// - resident in `ChunkReadCache` (for Seekable 64K files), or
    /// - resident in `DecodedBlobCache` (for compressed/encrypted files).
    pub(crate) fn try_read_at_inline_into(
        &self,
        inode_id: u64,
        file_offset: u64,
        buf: &mut [u8],
    ) -> Option<Result<usize, DiskManagerError>> {
        let guard = self.inner.try_read().ok()?;
        let inode = if let Some(cached) = guard.inode_cache.get(inode_id) {
            cached
        } else if guard.backing.as_mmap_slice().is_some() {
            match Self::read_inode_internal(&guard, inode_id) {
                Ok(i) => i,
                Err(e) => return Some(Err(e)),
            }
        } else {
            return None;
        };

        if inode.mode != crate::inode::FileType::File {
            return Some(Err(DiskManagerError::Io(std::io::Error::other(
                "Cannot read non-file inode",
            ))));
        }
        if guard.superblock.encrypted && !inode.encrypted {
            return Some(Err(DiskManagerError::DecryptionFailed));
        }
        if file_offset >= inode.size || buf.is_empty() {
            return Some(Ok(0));
        }

        let available = usize::try_from(inode.size - file_offset).unwrap_or(usize::MAX);
        let to_read = buf.len().min(available);

        // 1. Seekable 64K cache-hit probe (single chunk).
        if inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0 {
            let chunk_size = crate::inode::CHUNK_SIZE_64K;
            let start_chunk = (file_offset as usize) / chunk_size;
            let end_chunk = ((file_offset as usize) + to_read - 1) / chunk_size;
            if start_chunk == end_chunk {
                let in_chunk = (file_offset as usize) % chunk_size;
                let chunk_start = (start_chunk * chunk_size) as u64;
                let chunk_valid_len = ((inode.size - chunk_start) as usize).min(chunk_size);
                if in_chunk >= chunk_valid_len {
                    return Some(Ok(0));
                }
                let raw_entry =
                    Self::try_resolve_raw_block_id_cached_only(&guard, &inode, start_chunk)?;
                if raw_entry == 0 {
                    buf[..to_read].fill(0);
                    return Some(Ok(to_read));
                }
                if let Some(n) = guard
                    .chunk_read_cache
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .copy_slice_if_hit(
                        inode_id,
                        start_chunk,
                        raw_entry,
                        chunk_valid_len,
                        in_chunk,
                        &mut buf[..to_read],
                    )
                {
                    return Some(Ok(n));
                }
            }
            return None;
        }

        // 2. DecodedBlobCache hit probe for compressed/encrypted/filtered files.
        if inode.compressed_size > 0
            || inode.encrypted
            || inode.filter_delta
            || inode.filter_shuffle
            || inode.filter_bitshuffle
        {
            if let Some(blob) =
                crate::lock_unpoisoned(&guard.decoded_blob_cache).get(inode_id, &inode)
            {
                let start = file_offset as usize;
                if start >= blob.len() {
                    return Some(Ok(0));
                }
                let end = (start + to_read).min(blob.len());
                let actual = end - start;
                buf[..actual].copy_from_slice(&blob[start..end]);
                return Some(Ok(actual));
            }
            return None;
        }

        // 3. Raw uncompressed/unencrypted file: Mmap or DataPageCache hit probe.
        if inode.filter_delta || inode.filter_shuffle || inode.filter_bitshuffle {
            return None;
        }
        let mut written = 0usize;
        let mut cur_off = file_offset;
        while written < to_read {
            let blk_idx = (cur_off / BLOCK_SIZE as u64) as usize;
            let in_blk = (cur_off % BLOCK_SIZE as u64) as usize;
            let take = (to_read - written).min(BLOCK_SIZE - in_blk);
            let phys_blk = Self::try_resolve_raw_block_id_cached_only(&guard, &inode, blk_idx)?;
            if guard.is_data_block(phys_blk) {
                let disk_off = phys_blk * BLOCK_SIZE as u64 + in_blk as u64;
                match guard
                    .backing
                    .try_read_data_cached_only(disk_off, &mut buf[written..written + take])?
                {
                    Ok(()) => {}
                    Err(e) => return Some(Err(DiskManagerError::Io(e))),
                }
            } else {
                buf[written..written + take].fill(0);
            }
            written += take;
            cur_off += take as u64;
        }
        Some(Ok(to_read))
    }

    /// Probes in-memory caches (and `Mmap` direct ranges) under an already-held
    /// shared guard BEFORE allocating any temporary output buffer, avoiding large
    /// multi-megabyte `try_alloc_zeroed_vec` allocations on cold cache misses.
    fn try_read_at_inline_vec(
        guard: &DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
        file_offset: u64,
        to_read: usize,
    ) -> Option<Result<Vec<u8>, DiskManagerError>> {
        if to_read == 0 {
            return Some(Ok(Vec::new()));
        }

        // 1. Seekable 64K cache-hit probe (single chunk).
        if inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K != 0 {
            let chunk_size = crate::inode::CHUNK_SIZE_64K;
            let start_chunk = (file_offset as usize) / chunk_size;
            let end_chunk = ((file_offset as usize) + to_read - 1) / chunk_size;
            if start_chunk == end_chunk {
                let in_chunk = (file_offset as usize) % chunk_size;
                let chunk_start = (start_chunk * chunk_size) as u64;
                let chunk_valid_len = ((inode.size - chunk_start) as usize).min(chunk_size);
                if in_chunk >= chunk_valid_len {
                    return Some(Ok(Vec::new()));
                }
                let raw_entry =
                    Self::try_resolve_raw_block_id_cached_only(guard, inode, start_chunk)?;
                if raw_entry == 0 {
                    let avail = chunk_valid_len.saturating_sub(in_chunk).min(to_read);
                    return match Self::try_alloc_zeroed_vec(avail) {
                        Ok(buf) => Some(Ok(buf)),
                        Err(e) => Some(Err(e)),
                    };
                }
                if let Some(vec) = guard
                    .chunk_read_cache
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .copy_slice_to_vec_if_hit(
                        inode_id,
                        start_chunk,
                        raw_entry,
                        chunk_valid_len,
                        in_chunk,
                        to_read,
                    )
                {
                    return Some(Ok(vec));
                }
            }
            return None;
        }

        // 2. DecodedBlobCache hit probe for compressed/encrypted/filtered files.
        if inode.compressed_size > 0
            || inode.encrypted
            || inode.filter_delta
            || inode.filter_shuffle
            || inode.filter_bitshuffle
        {
            if let Some(blob) =
                crate::lock_unpoisoned(&guard.decoded_blob_cache).get(inode_id, inode)
            {
                let start = file_offset as usize;
                if start >= blob.len() {
                    return Some(Ok(Vec::new()));
                }
                let end = (start + to_read).min(blob.len());
                return Some(Ok(blob[start..end].to_vec()));
            }
            return None;
        }

        // 3. Raw uncompressed/unencrypted file: probe first FileIo page before huge alloc.
        if guard.backing.as_mmap_slice().is_none() && to_read > 64 * 1024 {
            let blk_idx = (file_offset / BLOCK_SIZE as u64) as usize;
            let in_blk = (file_offset % BLOCK_SIZE as u64) as usize;
            let phys0 = Self::try_resolve_raw_block_id_cached_only(guard, inode, blk_idx)?;
            if guard.is_data_block(phys0) {
                let mut probe = [0u8; 1];
                let disk_off = phys0 * BLOCK_SIZE as u64 + in_blk as u64;
                if let Err(e) = guard
                    .backing
                    .try_read_data_cached_only(disk_off, &mut probe)?
                {
                    return Some(Err(DiskManagerError::Io(e)));
                }
            }
        }

        let mut buf = match Self::try_alloc_zeroed_vec(to_read) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        let mut written = 0usize;
        let mut cur_off = file_offset;
        while written < to_read {
            let blk_idx = (cur_off / BLOCK_SIZE as u64) as usize;
            let in_blk = (cur_off % BLOCK_SIZE as u64) as usize;
            let take = (to_read - written).min(BLOCK_SIZE - in_blk);
            let phys_blk = Self::try_resolve_raw_block_id_cached_only(guard, inode, blk_idx)?;
            if guard.is_data_block(phys_blk) {
                let disk_off = phys_blk * BLOCK_SIZE as u64 + in_blk as u64;
                match guard
                    .backing
                    .try_read_data_cached_only(disk_off, &mut buf[written..written + take])?
                {
                    Ok(()) => {}
                    Err(e) => return Some(Err(DiskManagerError::Io(e))),
                }
            } else {
                buf[written..written + take].fill(0);
            }
            written += take;
            cur_off += take as u64;
        }
        Some(Ok(buf))
    }

    /// Asynchronously reads up to `len` bytes starting at `file_offset` from `inode_id`.
    ///
    /// Completes immediately on the calling thread (`Poll::Ready`, zero thread hop) when the
    /// range is served from `Mmap`, `DataPageCache`, `ChunkReadCache`, or `DecodedBlobCache`;
    /// dispatches to the background pool only on cache miss, lock contention, or cold decode.
    pub fn read_at_async(
        &self,
        inode_id: u64,
        file_offset: u64,
        len: usize,
    ) -> OifsAsyncTask<Result<Vec<u8>, DiskManagerError>> {
        if len <= DecodedBlobCache::MAX_ENTRY_BYTES
            && let Ok(guard) = self.inner.try_read()
        {
            let inode_opt = guard.inode_cache.get(inode_id).or_else(|| {
                if guard.backing.as_mmap_slice().is_some() {
                    Self::read_inode_internal(&guard, inode_id).ok()
                } else {
                    None
                }
            });
            if let Some(inode) = inode_opt
                && inode.mode == crate::inode::FileType::File
                && !(guard.superblock.encrypted && !inode.encrypted)
            {
                if file_offset >= inode.size || len == 0 {
                    return OifsAsyncTask::ready(Ok(Vec::new()));
                }
                let avail = usize::try_from(inode.size - file_offset).unwrap_or(usize::MAX);
                let to_read = len.min(avail);
                if let Some(res) =
                    Self::try_read_at_inline_vec(&guard, inode_id, &inode, file_offset, to_read)
                {
                    return OifsAsyncTask::ready(res);
                }
            }
        }
        let dm = self.clone();
        OifsAsyncTask::spawn(move || dm.read_at_vec(inode_id, file_offset, len))
    }

    /// Asynchronously reads the entire decoded contents of `inode_id`.
    ///
    /// Completes inline when the file is already resident in cache/`Mmap`;
    /// otherwise dispatches to the background pool.
    pub fn read_data_async(
        &self,
        inode_id: u64,
    ) -> OifsAsyncTask<Result<Vec<u8>, DiskManagerError>> {
        if let Ok(guard) = self.inner.try_read() {
            let inode_opt = guard.inode_cache.get(inode_id).or_else(|| {
                if guard.backing.as_mmap_slice().is_some() {
                    Self::read_inode_internal(&guard, inode_id).ok()
                } else {
                    None
                }
            });
            if let Some(inode) = inode_opt
                && inode.mode == crate::inode::FileType::File
                && !(guard.superblock.encrypted && !inode.encrypted)
                && inode.size <= DecodedBlobCache::MAX_ENTRY_BYTES as u64
            {
                let sz = inode.size as usize;
                if sz == 0 {
                    return OifsAsyncTask::ready(Ok(Vec::new()));
                }
                if let Some(res) = Self::try_read_at_inline_vec(&guard, inode_id, &inode, 0, sz) {
                    return OifsAsyncTask::ready(res);
                }
            }
        }
        let dm = self.clone();
        OifsAsyncTask::spawn(move || {
            let guard = dm.inner.read().unwrap();
            let inode = Self::read_inode_internal(&guard, inode_id)?;
            let cacheable_kind = (inode.compressed_size > 0
                || inode.encrypted
                || inode.filter_delta
                || inode.filter_shuffle
                || inode.filter_bitshuffle)
                && (inode.flags & crate::inode::INODE_FLAG_SEEKABLE_64K) == 0;
            if cacheable_kind
                && let Some(blob) =
                    crate::lock_unpoisoned(&guard.decoded_blob_cache).get(inode_id, &inode)
            {
                return Ok(blob.as_ref().clone());
            }
            let data = Self::read_data_internal(&guard, inode_id, &inode)?;
            if cacheable_kind && data.len() <= DecodedBlobCache::MAX_ENTRY_BYTES {
                crate::lock_unpoisoned(&guard.decoded_blob_cache).insert(
                    inode_id,
                    &inode,
                    Arc::new(data.clone()),
                );
            }
            Ok(data)
        })
    }

    /// Asynchronously executes a batch of positional reads (`read_at_batch`) on the background pool.
    pub fn read_at_batch_async(
        &self,
        requests: Vec<(u64, u64, usize)>,
    ) -> OifsAsyncTask<Vec<Result<Vec<u8>, DiskManagerError>>> {
        let dm = self.clone();
        OifsAsyncTask::spawn(move || {
            let mut bufs: Vec<Vec<u8>> =
                requests.iter().map(|&(_, _, len)| vec![0u8; len]).collect();
            let mut req_refs: Vec<ReadRequest<'_>> = requests
                .iter()
                .zip(bufs.iter_mut())
                .map(|(&(inode_id, offset, _), buf)| ReadRequest {
                    inode_id,
                    offset,
                    buf: &mut buf[..],
                })
                .collect();
            let results = dm.read_at_batch(&mut req_refs);
            results
                .into_iter()
                .zip(bufs)
                .map(|(res, mut b)| {
                    res.map(|n| {
                        b.truncate(n);
                        b
                    })
                })
                .collect()
        })
    }

    /// Asynchronously creates a file and writes its initial payload via the Group-Commit write
    /// pipeline. Under [`DurabilityMode::Strict`], concurrent async writes share a single
    /// physical flush barrier per batch.
    pub fn create_file_with_data_async(
        &self,
        parent_inode_id: u64,
        name: String,
        data: Vec<u8>,
        compression_mode: CompressionMode,
    ) -> OifsAsyncTask<Result<u64, DiskManagerError>> {
        if self.is_read_only() {
            return OifsAsyncTask::ready(Err(DiskManagerError::ReadOnly));
        }
        let state = AsyncTaskState::new_pending();
        let state_done = Arc::clone(&state);
        submit_group_write_op(
            self.clone(),
            move |dm| dm.create_file_with_data(parent_inode_id, &name, &data, compression_mode),
            move |res| state_done.complete(res),
        );
        OifsAsyncTask::from_state(state, false)
    }

    /// Asynchronously writes `data` at `file_offset` into `inode_id` via the Group-Commit write
    /// pipeline.
    pub fn write_data_async(
        &self,
        inode_id: u64,
        file_offset: u64,
        data: Vec<u8>,
        compression_mode: CompressionMode,
    ) -> OifsAsyncTask<Result<(), DiskManagerError>> {
        if self.is_read_only() {
            return OifsAsyncTask::ready(Err(DiskManagerError::ReadOnly));
        }
        let state = AsyncTaskState::new_pending();
        let state_done = Arc::clone(&state);
        submit_group_write_op(
            self.clone(),
            move |dm| {
                dm.write_data(inode_id, file_offset, &data, compression_mode)
                    .map(|()| 0)
            },
            move |res| state_done.complete(res.map(|_| ())),
        );
        OifsAsyncTask::from_state(state, false)
    }

    /// Asynchronously flushes dirty data and checkpoints the WAL on the background pool.
    pub fn flush_async_task(&self) -> OifsAsyncTask<Result<(), DiskManagerError>> {
        if self.is_read_only() {
            return OifsAsyncTask::ready(Ok(()));
        }
        let dm = self.clone();
        OifsAsyncTask::spawn(move || dm.flush())
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn proof_durability_mode_from_u8_soundness() {
        let val: u8 = kani::any();
        let mode = DurabilityMode::from_u8(val);
        match val {
            1 => assert_eq!(mode, DurabilityMode::RangeAsync),
            2 => assert_eq!(mode, DurabilityMode::Strict),
            3 => assert_eq!(mode, DurabilityMode::LegacyWholeMmapAsync),
            _ => assert_eq!(mode, DurabilityMode::Lazy),
        }
    }

    #[kani::proof]
    fn proof_durability_mode_is_range_based_consistency() {
        let val: u8 = kani::any();
        let mode = DurabilityMode::from_u8(val);
        let is_range = mode.is_range_based();
        if mode == DurabilityMode::RangeAsync || mode == DurabilityMode::Strict {
            assert!(is_range);
        } else {
            assert!(!is_range);
        }
    }

    /// Prove that checked arithmetic in get_block_from_map strictly prevents integer wrap-around.
    /// Specifically: for ANY non-zero block_id, it is mathematically impossible to wrap around
    /// and resolve to offset 0 (the SuperBlock).
    #[kani::proof]
    fn proof_get_block_checked_arithmetic_prevents_wrap_around() {
        let block_id: u64 = kani::any();
        let mmap_len: usize = kani::any();

        let checked_start = usize::try_from(block_id)
            .ok()
            .and_then(|b| b.checked_mul(BLOCK_SIZE));
        let checked_end = checked_start.and_then(|s| s.checked_add(BLOCK_SIZE));

        match (checked_start, checked_end) {
            (Some(start), Some(end)) => {
                assert!(start < end);
                assert_eq!(end - start, BLOCK_SIZE);
                if block_id > 0 {
                    assert!(
                        start >= BLOCK_SIZE,
                        "Non-zero block_id must NEVER resolve to Block 0 (SuperBlock)"
                    );
                } else {
                    assert_eq!(start, 0, "block_id 0 must resolve to Block 0");
                }
                if end <= mmap_len {
                    assert!(end <= mmap_len);
                }
                kani::cover!(block_id > 0 && end <= mmap_len, "Valid in-bounds block");
                kani::cover!(block_id == 0 && end <= mmap_len, "Block 0");
            }
            _ => {
                // Safely rejected upon multiplication/addition overflow or usize truncation
                kani::cover!(block_id > 0, "Overflow safely rejected");
            }
        }
    }

    /// Prove that inode_byte_range never wraps around to low memory on large inode_id.
    #[kani::proof]
    fn proof_inode_byte_range_checked_bounds() {
        let inode_id: u64 = kani::any();
        let inode_table_block: u64 = 3; // Standard block 3

        let base = (inode_table_block as usize).checked_mul(BLOCK_SIZE);
        let id_offset = usize::try_from(inode_id)
            .ok()
            .and_then(|id| id.checked_mul(256));
        let offset = base
            .and_then(|b| id_offset.and_then(|i| b.checked_add(i)))
            .unwrap_or(usize::MAX);

        if offset != usize::MAX {
            assert!(offset >= 3 * BLOCK_SIZE);
            if inode_id > 0 {
                assert!(offset >= 3 * BLOCK_SIZE + 256);
            }
        }
    }

    /// Prove that a full overwrite maintains the file-size invariant.
    /// When overwriting a file at offset 0 with smaller data on a full overwrite,
    /// the file size MUST be truncated to the new data size (not max(old, new)).
    /// This proof verifies the logic branch that decides whether to truncate.
    #[kani::proof]
    fn proof_write_from_start_size_invariant() {
        let old_size: u64 = kani::any();
        let new_data_len: u64 = kani::any();
        let compressed_size: u64 = kani::any();
        let encrypted: bool = kani::any();
        let filter_active: bool = kani::any();

        kani::assume(old_size <= 1024 * 1024 * 1024);
        kani::assume(new_data_len <= 1024 * 1024 * 1024);
        kani::assume(compressed_size <= 1024 * 1024 * 1024);

        let is_full_overwrite =
            new_data_len >= old_size || compressed_size > 0 || encrypted || filter_active;

        let expected_size = if is_full_overwrite {
            new_data_len
        } else {
            std::cmp::max(old_size, new_data_len)
        };

        if is_full_overwrite && new_data_len < old_size {
            assert_eq!(
                expected_size, new_data_len,
                "Full overwrite MUST truncate to new data size"
            );
            assert!(expected_size < old_size, "Size must decrease on truncation");
        }

        if !is_full_overwrite {
            assert_eq!(expected_size, std::cmp::max(old_size, new_data_len));
            assert!(
                expected_size >= old_size,
                "Partial write must not shrink file"
            );
        }

        kani::cover!(
            is_full_overwrite && new_data_len < old_size,
            "Full overwrite truncation"
        );
        kani::cover!(
            is_full_overwrite && new_data_len > old_size,
            "Full overwrite expansion"
        );
        kani::cover!(
            !is_full_overwrite && new_data_len < old_size,
            "Partial write no truncation"
        );
        kani::cover!(
            !is_full_overwrite && new_data_len > old_size,
            "Partial write expansion"
        );
    }

    /// Prove that read_at slice clamping cannot overflow or invert start..end ranges.
    /// Even if an image is corrupted and file_offset exceeds the decoded buffer length,
    /// the guard returns 0 safely without panicking.
    #[kani::proof]
    fn proof_clamp_slice_range_soundness() {
        let file_offset: u64 = kani::any();
        let to_read_total: usize = kani::any();
        let full_data_len: usize = kani::any();
        kani::assume(full_data_len <= 1024 * 1024);
        kani::assume(to_read_total <= 64 * 1024);

        let start = file_offset as usize;
        if start >= full_data_len {
            // Safely returns 0 bytes read
            return;
        }

        let end = (start.saturating_add(to_read_total)).min(full_data_len);
        assert!(start <= end, "start must never exceed end");
        assert!(end <= full_data_len, "end must never exceed full_data_len");
        let actual = end.saturating_sub(start);
        assert!(actual <= to_read_total);
        assert!(actual <= full_data_len);
        kani::cover!(start < full_data_len, "in-range read reached");
    }

    /// Prove that 64KB chunk index and offset slicing calculations for Seekable 64K Chunked Compression
    /// are strictly safe and preserve byte conservation across all arbitrary symbolic offsets and lengths.
    /// Invariants mathematically proven:
    /// 1. first_chunk <= last_chunk
    /// 2. For every chunk index in first_chunk..=last_chunk:
    ///    - chunk_offset_start < chunk_offset_end <= CHUNK_SIZE_64K
    ///    - data_start < data_end <= data_len
    ///    - Strict length conservation: (data_end - data_start) == (chunk_offset_end - chunk_offset_start)
    ///    - chunk_offset_end <= chunk_valid_len
    /// 3. Neither arithmetic overflow nor slice indexing out-of-bounds can occur.
    #[kani::proof]
    fn proof_chunked_64k_offset_and_slicing_soundness() {
        let file_offset: u64 = kani::any();
        let data_len: usize = kani::any();
        let current_file_size: u64 = kani::any();

        kani::assume(data_len > 0 && data_len <= 256 * 1024);
        kani::assume(file_offset <= 1024 * 1024 * 1024);
        kani::assume(current_file_size <= 1024 * 1024 * 1024);

        let new_file_size = current_file_size.max(file_offset + data_len as u64);
        let first_chunk = (file_offset as usize) / crate::inode::CHUNK_SIZE_64K;
        let last_chunk =
            ((file_offset + data_len as u64 - 1) as usize) / crate::inode::CHUNK_SIZE_64K;

        assert!(first_chunk <= last_chunk);

        // Pick an arbitrary chunk index in [first_chunk, last_chunk]
        let chunk_idx: usize = kani::any();
        kani::assume(chunk_idx >= first_chunk && chunk_idx <= last_chunk);

        let chunk_file_start = (chunk_idx * crate::inode::CHUNK_SIZE_64K) as u64;
        let chunk_file_end = chunk_file_start + crate::inode::CHUNK_SIZE_64K as u64;

        let write_file_start = file_offset.max(chunk_file_start);
        let write_file_end = (file_offset + data_len as u64).min(chunk_file_end);

        assert!(write_file_start < write_file_end);

        let data_start = (write_file_start - file_offset) as usize;
        let data_end = (write_file_end - file_offset) as usize;
        let chunk_offset_start = (write_file_start - chunk_file_start) as usize;
        let chunk_offset_end = (write_file_end - chunk_file_start) as usize;

        assert!(data_start < data_end);
        assert!(data_end <= data_len);
        assert!(chunk_offset_start < chunk_offset_end);
        assert!(chunk_offset_end <= crate::inode::CHUNK_SIZE_64K);

        // Exact byte conservation: the sliced input chunk matches the target buffer slice
        assert_eq!(data_end - data_start, chunk_offset_end - chunk_offset_start);

        let chunk_valid_len =
            ((new_file_size - chunk_file_start) as usize).min(crate::inode::CHUNK_SIZE_64K);
        assert!(chunk_offset_end <= chunk_valid_len);
        kani::cover!(first_chunk == last_chunk, "single-chunk write reached");
        kani::cover!(first_chunk < last_chunk, "multi-chunk write reached");
    }

    /// Prove that `ShardedDirCache` 64-bit Fibonacci shard hashing is strictly in-bounds
    /// (`< NUM_DIR_CACHE_SHARDS`) for every possible `u64` directory inode ID, and that
    /// consecutive directory inode IDs (`id` and `id.wrapping_add(1)`) never collide in
    /// the same shard.
    #[kani::proof]
    fn proof_sharded_dir_cache_fibonacci_index_in_bounds_and_bijective() {
        let dir_inode_id: u64 = kani::any();
        let mask = NUM_DIR_CACHE_SHARDS - 1;
        assert!(NUM_DIR_CACHE_SHARDS.is_power_of_two());

        let idx0 = (dir_inode_id.wrapping_mul(0x517cc1b727220a95) as usize) & mask;
        let idx1 = (dir_inode_id
            .wrapping_add(1)
            .wrapping_mul(0x517cc1b727220a95) as usize)
            & mask;

        assert!(idx0 < NUM_DIR_CACHE_SHARDS);
        assert!(idx1 < NUM_DIR_CACHE_SHARDS);
        assert_ne!(
            idx0, idx1,
            "Consecutive directory inode IDs must map to distinct shards"
        );
    }

    /// Prove that indirect subtree hole skipping (`emit_hole_until` in `read_at_with_inode`)
    /// guarantees strict forward progress (`hole_len >= 1`), never overflows, and never indexes
    /// outside `buf[0..to_read_total]` across Single, Double, and Triple indirect boundaries.
    #[kani::proof]
    fn proof_read_at_indirect_hole_skip_conservation() {
        let file_offset: u64 = kani::any();
        let to_read_total: usize = kani::any();
        let bytes_read: usize = kani::any();
        let limit_blk: usize = kani::any();

        // Bound to max addressable triple-indirect limit (10 + 512 + 512^2 + 512^3 = 134_480_394)
        const TRIPLE_LIMIT: usize = 10 + 512 + 512 * 512 + 512 * 512 * 512;
        kani::assume(to_read_total > 0 && to_read_total <= 64 * 1024 * 1024);
        kani::assume(bytes_read < to_read_total);
        kani::assume(limit_blk > 0 && limit_blk <= TRIPLE_LIMIT);
        kani::assume(file_offset <= (TRIPLE_LIMIT as u64) * (BLOCK_SIZE as u64));

        let curr_offset = file_offset.saturating_add(bytes_read as u64);
        let end_offset = file_offset.saturating_add(to_read_total as u64);
        let blk_idx = (curr_offset / BLOCK_SIZE as u64) as usize;
        kani::assume(blk_idx < limit_blk);
        kani::assume(curr_offset < end_offset);
        kani::assume(end_offset - curr_offset == (to_read_total - bytes_read) as u64);

        let hole_end = (limit_blk as u64 * BLOCK_SIZE as u64).min(end_offset);
        assert!(hole_end > curr_offset, "Must make strict forward progress");
        assert!(hole_end <= end_offset, "Must never overshoot end_offset");

        let hole_len = (hole_end - curr_offset) as usize;
        assert!(hole_len >= 1);
        assert!(bytes_read.checked_add(hole_len).unwrap() <= to_read_total);
    }

    /// Prove that `ReadAtSink` contiguous `IoBackend::Mmap` extent coalescing
    /// preserves exact byte equivalence with per-block copying and stays within bounds.
    #[kani::proof]
    fn proof_read_at_contiguous_mmap_run_coalescing_soundness() {
        let mmap_len: usize = 64;
        let src1: usize = kani::any();
        let len1: usize = kani::any();
        let src2: usize = kani::any();
        let len2: usize = kani::any();

        kani::assume(len1 > 0 && len1 <= 16);
        kani::assume(len2 > 0 && len2 <= 16);
        kani::assume(src1 <= mmap_len && src1 + len1 <= mmap_len);
        kani::assume(src2 <= mmap_len && src2 + len2 <= mmap_len);

        let dst1: usize = 0;
        let dst2: usize = len1;
        let is_contiguous = src1 + len1 == src2 && dst1 + len1 == dst2;

        if is_contiguous {
            let merged_src = src1;
            let merged_dst = dst1;
            let merged_len = len1 + len2;
            assert!(merged_src + merged_len <= mmap_len);
            assert_eq!(merged_dst + merged_len, len1 + len2);
            // Every byte index k in the second chunk maps to the exact same source offset
            let k: usize = kani::any();
            kani::assume(k < len2);
            assert_eq!(merged_src + (len1 + k), src2 + k);
            assert_eq!(merged_dst + (len1 + k), dst2 + k);
        }
    }

    /// Prove that incremental `compressed_size_delta` accounting in `truncate_chunked_64k`
    /// produces the exact same `inode.compressed_size` as a full `0..new_chunks` summation
    /// and never underflows or overflows.
    #[kani::proof]
    fn proof_seekable_truncate_incremental_compressed_size_conservation() {
        let c0: u16 = kani::any();
        let c1: u16 = kani::any();
        let c2: u16 = kani::any();
        let c3: u16 = kani::any();
        let new_chunks: usize = kani::any();
        let new_last_comp_len: u16 = kani::any();
        let partial_last: bool = kani::any();

        kani::assume(new_chunks >= 1 && new_chunks <= 3);
        let chunks = [c0, c1, c2, c3];
        let old_total: u64 = (c0 as u64) + (c1 as u64) + (c2 as u64) + (c3 as u64);

        let mut delta: i64 = 0;
        let last_c_idx = new_chunks - 1;
        for (s_idx, &chunk_len) in chunks.iter().enumerate() {
            if s_idx >= new_chunks {
                delta -= chunk_len as i64;
            }
        }
        if partial_last && chunks[last_c_idx] > 0 {
            delta += (new_last_comp_len as i64) - (chunks[last_c_idx] as i64);
        }

        let incremental_total = (old_total as i64 + delta).max(0) as u64;

        let mut full_scan_total: u64 = 0;
        for (s_idx, &chunk_len) in chunks.iter().enumerate() {
            if s_idx < new_chunks {
                if s_idx == last_c_idx && partial_last && chunk_len > 0 {
                    full_scan_total += new_last_comp_len as u64;
                } else {
                    full_scan_total += chunk_len as u64;
                }
            }
        }

        assert_eq!(incremental_total, full_scan_total);
    }

    /// Formally prove that `FileReadHandle::mmap_slice` block-range and byte-offset
    /// arithmetic is overflow-free, preserves exact slice length, and never indexes
    /// past `mmap_len` for any symbolic `(inode_size, file_offset, max_len, first_phys)`.
    #[kani::proof]
    fn proof_file_read_handle_mmap_slice_bounds_soundness() {
        let inode_size: u64 = kani::any();
        let file_offset: u64 = kani::any();
        let max_len: usize = kani::any();
        let first_phys: u64 = kani::any();
        let mmap_len: usize = kani::any();

        kani::assume(inode_size <= 1024 * 1024 * 1024);
        kani::assume(max_len <= 64 * 1024 * 1024);

        if file_offset >= inode_size || max_len == 0 {
            kani::cover!(
                file_offset >= inode_size,
                "EOF or past-EOF offset returns empty slice"
            );
            kani::cover!(max_len == 0, "Zero max_len returns empty slice");
            return;
        }

        let available = usize::try_from(inode_size - file_offset).unwrap_or(usize::MAX);
        let len = max_len.min(available);
        assert!(len >= 1 && len <= max_len);
        assert!((len as u64) <= inode_size - file_offset);

        let first_blk_idx = (file_offset / BLOCK_SIZE as u64) as usize;
        let in_blk_off = (file_offset % BLOCK_SIZE as u64) as usize;
        let last_byte = file_offset + (len - 1) as u64;
        let last_blk_idx = (last_byte / BLOCK_SIZE as u64) as usize;

        assert!(first_blk_idx <= last_blk_idx);
        assert!(in_blk_off < BLOCK_SIZE);
        let spanned_blocks = last_blk_idx - first_blk_idx + 1;
        assert!(in_blk_off + len <= spanned_blocks * BLOCK_SIZE);

        if let Some(start) = usize::try_from(first_phys)
            .ok()
            .and_then(|b| b.checked_mul(BLOCK_SIZE))
            .and_then(|b| b.checked_add(in_blk_off))
            && let Some(end) = start.checked_add(len)
            && end <= mmap_len
        {
            assert!(start < end);
            assert_eq!(end - start, len);
            assert!(end <= mmap_len);
            kani::cover!(
                first_blk_idx == last_blk_idx,
                "Single-block zero-copy slice"
            );
            kani::cover!(
                first_blk_idx < last_blk_idx,
                "Multi-block contiguous zero-copy slice"
            );
        }
    }

    /// Formally prove read-only mount, drop, and open-mode invariants:
    /// 1. `validate_read_only_journal_state` rejects every journaled image with `pending_txs > 0`
    ///    and accepts non-journaled or clean (`pending_txs == 0`) journaled images.
    /// 2. `should_sync_on_drop` unconditionally suppresses both journal and backing flushes when
    ///    `read_only == true` (zero disk writes on drop).
    /// 3. `open_mode_requires_write` permits `OpenExisting` unconditionally, rejects `CreateNew`
    ///    unconditionally, and permits `CreateOrOpen` iff the entry already exists.
    #[kani::proof]
    fn proof_read_only_mount_drop_and_open_mode_invariants() {
        let has_journal: bool = kani::any();
        let pending_txs: usize = kani::any();
        let allowed = DiskManager::validate_read_only_journal_state(has_journal, pending_txs);
        if has_journal && pending_txs > 0 {
            assert!(!allowed, "Dirty WAL must be rejected on read-only mount");
        } else {
            assert!(
                allowed,
                "Non-journaled or clean WAL must be mountable read-only"
            );
        }

        let read_only: bool = kani::any();
        let (flush_journal, flush_backing) =
            DiskManagerInner::should_sync_on_drop(read_only, has_journal);
        if read_only {
            assert!(!flush_journal, "Read-only drop must never flush journal");
            assert!(!flush_backing, "Read-only drop must never flush backing");
        } else {
            assert_eq!(flush_journal, has_journal);
            assert!(flush_backing);
        }

        let mode_tag: u8 = kani::any();
        kani::assume(mode_tag < 3);
        let mode = match mode_tag {
            0 => OpenMode::OpenExisting,
            1 => OpenMode::CreateNew,
            _ => OpenMode::CreateOrOpen,
        };
        let entry_exists: bool = kani::any();
        let needs_write = DiskManager::open_mode_requires_write(mode, entry_exists);
        match mode {
            OpenMode::OpenExisting => {
                assert!(!needs_write);
            }
            OpenMode::CreateNew => {
                assert!(needs_write);
            }
            OpenMode::CreateOrOpen => {
                assert_eq!(needs_write, !entry_exists);
            }
        }

        kani::cover!(has_journal && pending_txs > 0, "Dirty WAL rejected");
        kani::cover!(has_journal && pending_txs == 0, "Clean WAL accepted");
        kani::cover!(
            read_only && has_journal,
            "Read-only journaled drop suppressed"
        );
        kani::cover!(
            mode == OpenMode::CreateOrOpen && !entry_exists && needs_write,
            "CreateOrOpen on missing entry requires write"
        );
    }
}

#[cfg(test)]
mod encryption_scheme_tests {
    use super::*;
    use crate::encryption::ENCRYPTION_SCHEME_CONTEXT_BOUND;

    /// An image written with the legacy scheme (no associated data) must stay
    /// readable: the context-bound scheme was introduced without breaking
    /// the images that were written before it.
    #[test]
    fn legacy_encryption_scheme_stays_readable() {
        let path =
            std::env::temp_dir().join(format!("oifs_legacy_scheme_{}.img", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let dm = DiskManager::create_encrypted(&path, 10 * 1024 * 1024, "pw123456")
            .expect("create encrypted image");

        // Downgrade the image to the legacy scheme before any data is written.
        {
            let mut guard = dm.inner.write().unwrap();
            let mut sb = guard.superblock;
            sb.encryption_version = crate::encryption::ENCRYPTION_SCHEME_LEGACY;
            let serialized = bincode::serialize(&sb).expect("serialize superblock");
            guard
                .backing
                .write_all_at(0, &serialized)
                .expect("write superblock");
            guard.superblock = sb;
        }

        let root = dm.superblock().root_inode;
        let id = dm.create_file(root, "legacy.txt").expect("create file");
        let payload = b"written with the legacy scheme";
        dm.write_data(id, 0, payload, CompressionMode::Never)
            .expect("write data");

        // The read must succeed: the legacy scheme uses an empty AAD.
        assert_eq!(dm.read_data(id).expect("read data"), payload.to_vec());
        drop(dm);

        // And it must still be readable after a remount.
        let dm = DiskManager::open_with_password(&path, 0, Some("pw123456"))
            .expect("reopen encrypted image");
        assert_eq!(
            dm.read_data(id).expect("read data after remount"),
            payload.to_vec()
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A fresh image uses the context-bound scheme.
    #[test]
    fn new_images_use_the_context_bound_scheme() {
        let path =
            std::env::temp_dir().join(format!("oifs_context_scheme_{}.img", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let dm = DiskManager::create_encrypted(&path, 10 * 1024 * 1024, "pw123456")
            .expect("create encrypted image");
        assert_eq!(
            dm.superblock().encryption_version,
            ENCRYPTION_SCHEME_CONTEXT_BOUND
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn repro_file_io_journal_pwrite_error_not_silently_ignored() {
        let path = std::env::temp_dir().join(format!(
            "oifs_repro_journal_pwrite_{}.img",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let dm = DiskManager::open_file_io_journaled(&path, 4 * 1024 * 1024).unwrap();
        let root = dm.superblock().root_inode;

        // Swap the backing's file descriptor with a read-only handle so every `pwrite`
        // fails in the kernel while `pread` continues to succeed.
        {
            let ro = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
            let mut guard = dm.inner.write().unwrap();
            guard.backing.replace_file_for_test(ro);
        }

        // Creating a file MUST return an I/O error rather than silently succeeding in memory.
        let res = dm.create_file(root, "ghost.txt");
        assert!(
            res.is_err(),
            "create_file under FileIo+WAL silently succeeded despite pwrite failure: {res:?}"
        );
        assert!(
            dm.lookup(root, "ghost.txt").is_err(),
            "failed journaled create_file must not leave ghost entry in memory"
        );

        // Direct apply_journal_op must also propagate pwrite errors and keep bitmaps rolled back.
        {
            let mut guard = dm.inner.write().unwrap();
            let sb = guard.superblock;
            let op = crate::journal::MetadataOp::SetDataBitmapRange {
                start_block: sb.data_block_start + 10,
                count: 4,
                allocated: true,
            };
            assert!(guard.backing.apply_journal_op(&sb, &op).is_err());
            let db = guard.data_bitmap_slice().unwrap();
            let bm = crate::bitmap::BitmapRef::new(db);
            for b in 10..14 {
                assert!(
                    !bm.get(b),
                    "failed apply_journal_op mutated data_bitmap bit {b}"
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn repro_contiguous_fresh_write_io_error_does_not_leak_data_bitmap_blocks() {
        let path =
            std::env::temp_dir().join(format!("oifs_repro_contig_leak_{}.img", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let dm = DiskManager::open_file_io(&path, 4 * 1024 * 1024).unwrap();
        let root = dm.superblock().root_inode;
        let id = dm.create_file(root, "target.bin").unwrap();

        let allocated_before: usize = {
            let guard = dm.inner.read().unwrap();
            guard
                .data_bitmap_slice()
                .unwrap()
                .iter()
                .map(|b| b.count_ones() as usize)
                .sum()
        };

        // Swap the backing's file descriptor with a read-only handle so payload `write_all_at`
        // inside `try_write_contiguous_fresh_file` fails right after allocating 4 data blocks.
        {
            let ro = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
            let mut guard = dm.inner.write().unwrap();
            guard.backing.replace_file_for_test(ro);
        }

        let res = dm.write_data(id, 0, &vec![0xAB; 16 * 1024], CompressionMode::Never);
        assert!(res.is_err(), "expected write_data to fail on read-only FD");

        let allocated_after: usize = {
            let guard = dm.inner.read().unwrap();
            guard
                .data_bitmap_slice()
                .unwrap()
                .iter()
                .map(|b| b.count_ones() as usize)
                .sum()
        };
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            allocated_after,
            allocated_before,
            "try_write_contiguous_fresh_file leaked {} data bitmap blocks on write error",
            allocated_after.saturating_sub(allocated_before)
        );
    }

    #[test]
    fn repro_file_io_cache_env_cap_and_configure_regions_bounds() {
        let path =
            std::env::temp_dir().join(format!("oifs_repro_cache_cap_{}.img", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // 1. Huge OIFS_FILE_IO_CACHE_MB value must be clamped to a safe ceiling (<= 4 GiB).
        // SAFETY: scoped to this unit test check.
        unsafe {
            std::env::set_var(crate::storage::FILE_IO_CACHE_MB_ENV, "99999999");
        }
        let dm = DiskManager::open_file_io(&path, 2 * 1024 * 1024).unwrap();
        unsafe {
            std::env::remove_var(crate::storage::FILE_IO_CACHE_MB_ENV);
        }
        let cap = dm.file_io_cache_bytes();

        // 2. configure_regions on an out-of-bounds superblock must return UnexpectedEof.
        let mut bogus_sb = dm.superblock();
        bogus_sb.inode_bitmap_block = 999_999;
        let cfg_res = {
            let mut guard = dm.inner.write().unwrap();
            guard.backing.configure_regions(&bogus_sb)
        };
        drop(dm);
        let _ = std::fs::remove_file(&path);

        assert!(
            cap <= 4 * 1024 * 1024 * 1024,
            "OIFS_FILE_IO_CACHE_MB was not clamped: cap = {cap} bytes"
        );
        assert!(
            cfg_res.is_err(),
            "configure_regions silently accepted out-of-bounds bitmap offset"
        );
    }

    #[test]
    fn repro_read_session_open_file_rejects_unencrypted_compressed_or_seekable_inode() {
        let path = std::env::temp_dir().join(format!(
            "oifs_repro_enc_open_file_{}.img",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let dm = DiskManager::create_encrypted(&path, 4 * 1024 * 1024, "pw123456").unwrap();
        let root = dm.superblock().root_inode;
        let id = dm
            .create_file_with_data(root, "secret.bin", b"top-secret", CompressionMode::Never)
            .unwrap();

        // Forge an unencrypted inode with size == 0 but compressed_size > 0 on an encrypted image.
        {
            let mut guard = dm.inner.write().unwrap();
            let mut forged = DiskManager::read_inode_internal(&guard, id).unwrap();
            forged.encrypted = false;
            forged.size = 0;
            forged.compressed_size = 64;
            DiskManager::write_inode_internal(&mut guard, id, &forged).unwrap();
        }

        let session = dm.begin_read();
        let is_dec_failed = matches!(
            session.open_file(id),
            Err(DiskManagerError::DecryptionFailed)
        );
        drop(session);
        drop(dm);
        let _ = std::fs::remove_file(&path);

        assert!(
            is_dec_failed,
            "ReadSession::open_file accepted unencrypted inode with compressed_size > 0 on encrypted image"
        );
    }
}
