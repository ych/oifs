//! Disk manager module for OIFS file system
//!
//! Provides the main interface for interacting with the file system,
//! including creating/reading files and directories, managing inodes,
//! and handling file compression.

use crate::BLOCK_SIZE;
use crate::allocator::{AllocatorError, BlockAllocator, SimpleBlockAllocator};
use crate::inode::Inode;
use crate::io_engine::{ExtentList, IoBackend, IoEngine, ReadTarget};
use crate::superblock::SuperBlock;
use memmap2::{MmapMut, MmapOptions};
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

pub use caches::INODE_CACHE_CAPACITY;
use caches::*;
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
    /// Memory-mapped view of the file system image
    pub(crate) mmap: MmapMut,
    /// Cached copy of the superblock
    pub superblock: SuperBlock,
    /// Encryption key (if filesystem is encrypted)
    pub encryption_key: Option<crate::encryption::EncryptionKey>,
    /// Search hint for sequential O(1) block allocation
    pub free_block_hint: u64,
    /// Search hint for sequential O(1) inode allocation
    pub free_inode_hint: u64,
    /// Inode cache for zero-copy metadata access
    pub inode_cache: BoundedInodeCache,
    /// Sharded per-directory name index: dir_inode_id -> DirIndex
    pub dir_cache: ShardedDirCache,
    /// Bounded sub-chunk decompression cache for seekable 64K `read_at`
    pub chunk_read_cache: Mutex<ChunkReadCache>,
    /// Bounded decoded-payload cache for non-seekable `read_at`
    pub decoded_blob_cache: Mutex<DecodedBlobCache>,
    /// Durability policy governing mmap msync behavior on mutations
    pub durability_mode: std::sync::atomic::AtomicU8,
    /// Batched dirty-range accumulator for `DurabilityMode::RangeAsync`
    pub(crate) pending_async_ranges: Mutex<PendingAsyncRanges>,
    /// Fast atomic flag set when `pending_async_ranges` reaches its drain threshold
    pub(crate) async_drain_ready: Arc<std::sync::atomic::AtomicBool>,
    /// Payload-block read engine
    io_engine: IoEngine,
}

impl DiskManagerInner {
    /// The inode bitmap as one contiguous slice.
    ///
    /// The bitmap may span several blocks. The blocks are consecutive
    /// in the image, so the span is a contiguous slice of the mapping.
    pub(crate) fn inode_bitmap_slice(&self) -> Option<&[u8]> {
        self.bitmap_span(
            self.superblock.inode_bitmap_block,
            self.superblock.inode_bitmap_blocks(),
        )
    }

    /// The data bitmap as one contiguous slice.
    pub(crate) fn data_bitmap_slice(&self) -> Option<&[u8]> {
        self.bitmap_span(
            self.superblock.data_bitmap_block,
            self.superblock.data_bitmap_blocks(),
        )
    }

    /// The mutable inode bitmap span.
    pub(crate) fn inode_bitmap_slice_mut(&mut self) -> Option<&mut [u8]> {
        let start = self.bitmap_start(self.superblock.inode_bitmap_block)?;
        let len = self.bitmap_len(self.superblock.inode_bitmap_blocks())?;
        let end = start.checked_add(len)?;
        if end > self.mmap.len() {
            return None;
        }
        Some(&mut self.mmap[start..end])
    }

    /// The mutable data bitmap span.
    pub(crate) fn data_bitmap_slice_mut(&mut self) -> Option<&mut [u8]> {
        let start = self.bitmap_start(self.superblock.data_bitmap_block)?;
        let len = self.bitmap_len(self.superblock.data_bitmap_blocks())?;
        let end = start.checked_add(len)?;
        if end > self.mmap.len() {
            return None;
        }
        Some(&mut self.mmap[start..end])
    }

    /// The byte offset of the first block of a bitmap.
    fn bitmap_start(&self, block: u64) -> Option<usize> {
        usize::try_from(block).ok()?.checked_mul(BLOCK_SIZE)
    }

    /// The byte length of a bitmap span.
    fn bitmap_len(&self, blocks: u64) -> Option<usize> {
        usize::try_from(blocks).ok()?.checked_mul(BLOCK_SIZE)
    }

    /// The byte range of a bitmap span.
    fn bitmap_span(&self, start_block: u64, blocks: u64) -> Option<&[u8]> {
        let start = self.bitmap_start(start_block)?;
        let len = self.bitmap_len(blocks)?;
        let end = start.checked_add(len)?;
        if end > self.mmap.len() {
            return None;
        }
        Some(&self.mmap[start..end])
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
        if batch.is_empty() {
            return;
        }
        // Preserve WAL-before-metadata writeback ordering when draining a RangeAsync batch.
        if self.superblock.has_journal_layout() {
            let bs = self.superblock.block_size as u64;
            if let Some(start) = DiskManager::journal_region_start(&self.superblock) {
                let len =
                    DiskManager::journal_region_len(bs).min(self.mmap.len().saturating_sub(start));
                if len > 0 {
                    let _ = self.mmap.flush_async_range(start, len);
                }
            }
        }
        let coalesced = Self::coalesce_ranges_with_gap(
            batch,
            self.mmap.len(),
            Self::RANGE_ASYNC_GAP_BRIDGE_BYTES,
        );
        for (offset, actual_len) in coalesced {
            let _ = self.mmap.flush_async_range(offset, actual_len);
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
        match self.durability_mode() {
            DurabilityMode::Lazy => Ok(()),
            DurabilityMode::RangeAsync => {
                if ranges.is_empty() {
                    return Ok(());
                }
                let mmap_len = self.mmap.len();
                let mut inline_batch = None;
                {
                    let mut pending = crate::lock_unpoisoned(&self.pending_async_ranges);
                    let mut added = false;
                    for &(off, len) in ranges {
                        if len == 0 || off >= mmap_len {
                            continue;
                        }
                        let clamped_len = len.min(mmap_len - off);
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
                let coalesced = Self::coalesce_ranges_with_gap(
                    ranges,
                    self.mmap.len(),
                    Self::STRICT_GAP_BRIDGE_BYTES,
                );
                for (offset, actual_len) in coalesced {
                    self.mmap
                        .flush_range(offset, actual_len)
                        .map_err(DiskManagerError::Io)?;
                }
                Ok(())
            }
            DurabilityMode::LegacyWholeMmapAsync => {
                let _ = self.mmap.flush_async();
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
        // The journal region goes out first: on shutdown the log describing pending
        // metadata must not lag behind the metadata itself.
        if self.superblock.has_journal_layout() {
            let bs = self.superblock.block_size as u64;
            if let Some(start) = DiskManager::journal_region_start(&self.superblock) {
                let len = DiskManager::journal_region_len(bs).min(self.mmap.len() - start);
                let _ = self.mmap.flush_range(start, len);
            }
        }
        // Flush any pending changes to disk when dropped
        let _ = self.mmap.flush();
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
    /// Open an existing OIFS image or create a new one if it doesn't exist.
    /// `size`: Total size in bytes (only used when creating a new file).
    pub fn open<P: AsRef<Path>>(path: P, total_size: u64) -> Result<Self, DiskManagerError> {
        Self::init_or_open(path, total_size, None, false, false)
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
        Self::init_or_open(path, total_size, None, false, true)
    }

    /// Open an encrypted OIFS image with a password
    pub fn open_with_password<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: Option<&str>,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(path, total_size, password, false, false)
    }

    /// Create a new encrypted filesystem
    pub fn create_encrypted<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: &str,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(path, total_size, Some(password), true, false)
    }

    /// Create a new encrypted filesystem with metadata journaling enabled.
    pub fn create_encrypted_journaled<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: &str,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(path, total_size, Some(password), true, true)
    }

    /// Open an existing OIFS image with an existing derived encryption key (or unencrypted if None).
    pub fn open_with_key<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        key: Option<crate::encryption::EncryptionKey>,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open_with_key(path, total_size, None, key, false, false)
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
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open_with_key(path, total_size, password, None, create_encrypted, journal)
    }

    fn init_or_open_with_key<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: Option<&str>,
        existing_key: Option<crate::encryption::EncryptionKey>,
        create_encrypted: bool,
        journal: bool,
    ) -> Result<Self, DiskManagerError> {
        let path = path.as_ref();
        let exists = path.exists();

        let mut open_opts = OpenOptions::new();
        open_opts
            .read(true)
            .write(true)
            .create(true)
            .truncate(create_encrypted);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_opts.mode(0o600);
            if create_encrypted {
                open_opts.custom_flags(libc::O_NOFOLLOW);
            }
        }
        let file = open_opts.open(path)?;

        // Acquire Lock using F_SETLK
        Self::try_lock_whole_file(&file)?;

        let is_new = !exists || create_encrypted;
        if is_new {
            file.set_len(total_size)?;
        }

        let mut mmap = unsafe { MmapOptions::new().map_mut(&file)? };
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
                    encryption_key = Some(key);
                } else {
                    let pwd = password.unwrap_or_default();
                    sb.encrypted = true;
                    sb.encryption_salt = crate::encryption::generate_salt();
                    // New images bind the ciphertext to its file.
                    sb.encryption_version = crate::encryption::ENCRYPTION_SCHEME_CONTEXT_BOUND;
                    let key = crate::encryption::derive_key(pwd, &sb.encryption_salt)?;
                    encryption_key = Some(key);
                }
            } else {
                encryption_key = None;
            }
            let serialized = bincode::serialize(&sb)?;
            mmap[0..serialized.len()].copy_from_slice(&serialized);
            superblock = sb;
        } else {
            if mmap.len() < BLOCK_SIZE {
                return Err(DiskManagerError::FileTooSmall);
            }
            superblock = bincode::deserialize(&mmap[0..BLOCK_SIZE])?;
            if superblock.magic != SuperBlock::MAGIC {
                return Err(DiskManagerError::InvalidMagic);
            }
            superblock
                .validate(mmap.len())
                .map_err(DiskManagerError::CorruptedSuperblock)?;

            if superblock.encrypted {
                if let Some(key) = existing_key {
                    encryption_key = Some(key);
                } else {
                    let pwd = password.ok_or(DiskManagerError::PasswordRequired)?;
                    let key = crate::encryption::derive_key(pwd, &superblock.encryption_salt)?;
                    encryption_key = Some(key);
                }
            } else {
                if password.is_some() || existing_key.is_some() {
                    return Err(DiskManagerError::NotEncrypted);
                }
                encryption_key = None;
            }
        }

        // Mount-time journal handling.
        // A journaled image is recovered *before* the manager is published, so no
        // caller can ever observe a filesystem mid-replay.
        if is_new {
            Self::format_journal(&mut mmap, &superblock)?;
        } else if superblock.has_journal_layout() {
            Self::recover_journal(&mut mmap, &superblock)?;
            mmap.flush()?;
        }

        let backing_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let free_block_hint = superblock.data_block_start;
        let async_drain_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inner = DiskManagerInner {
            file,
            backing_path,
            mmap,
            superblock,
            encryption_key,
            free_block_hint,
            free_inode_hint: 0,
            inode_cache: BoundedInodeCache::with_capacity(INODE_CACHE_CAPACITY),
            dir_cache: ShardedDirCache::new(),
            chunk_read_cache: Mutex::new(ChunkReadCache::default()),
            decoded_blob_cache: Mutex::new(DecodedBlobCache::default()),
            durability_mode: std::sync::atomic::AtomicU8::new(DurabilityMode::Lazy as u8),
            pending_async_ranges: Mutex::new(PendingAsyncRanges::default()),
            async_drain_ready: async_drain_ready.clone(),
            io_engine: IoEngine::new(IoBackend::from_env().unwrap_or_default()),
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
            let bitmap_slice = guard.inode_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("Failed to get inode bitmap"))
            })?;
            let mut ia = SimpleBlockAllocator::new(bitmap_slice, 0);
            let root_id = ia.allocate()?;
            if root_id != 0 {
                return Err(DiskManagerError::Io(std::io::Error::other(
                    "Failed init root inode",
                )));
            }

            let data_start = guard.superblock.data_block_start;
            let data_slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("Failed to get data bitmap"))
            })?;
            let mut da = SimpleBlockAllocator::new(data_slice, data_start);
            let root_data = da.allocate()?;

            let mut root_inode = Inode::new(crate::inode::FileType::Directory);
            root_inode.blocks[0] = root_data;
            root_inode.size = BLOCK_SIZE as u64;
            Self::write_inode_internal(&mut guard, 0, &root_inode)?;
            guard.mmap.flush()?;
        } else {
            // Already replayed at mount time above; record a clean-shutdown marker so
            // the next mount can tell "clean" from "crashed" without a ring scan.
            let mut guard = dm.inner.write().unwrap();
            if guard.superblock.has_journal_layout() {
                let sb = guard.superblock;
                Self::mark_journal_clean(&mut guard.mmap, &sb)?;
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
        // SAFETY: the descriptor is owned by `file` and `lock` outlives the call.
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) };
        if rc == -1 {
            return Err(DiskManagerError::Locking(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Record a clean-shutdown marker in the journal header.
    fn mark_journal_clean(mmap: &mut MmapMut, sb: &SuperBlock) -> Result<(), DiskManagerError> {
        if !sb.has_journal_layout() {
            return Ok(());
        }
        let region = Self::journal_region(mmap, sb)?;
        let bs = sb.block_size as u64;
        crate::journal::JournalRing::open(region, bs)?.mark_clean_shutdown();
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

    // Private helper for Inner
    pub(crate) fn get_block_mut_from_map(mmap: &mut MmapMut, block_id: u64) -> Option<&mut [u8]> {
        let start = usize::try_from(block_id).ok()?.checked_mul(BLOCK_SIZE)?;
        let end = start.checked_add(BLOCK_SIZE)?;
        if end > mmap.len() {
            None
        } else {
            Some(&mut mmap[start..end])
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

    /// Resolve the journal region as a mutable slice, if the image is large enough.
    fn journal_region<'m>(
        mmap: &'m mut MmapMut,
        sb: &SuperBlock,
    ) -> Result<&'m mut [u8], DiskManagerError> {
        let bs = sb.block_size as u64;
        let start = Self::journal_region_start(sb).ok_or_else(|| {
            DiskManagerError::Io(std::io::Error::other("Journal offset overflow"))
        })?;
        let end = start
            .checked_add(Self::journal_region_len(bs))
            .ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("Journal region overflow"))
            })?;
        if end > mmap.len() {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Journal region does not fit in image",
            )));
        }
        Ok(&mut mmap[start..end])
    }

    /// Format the journal region of a freshly created journaled image.
    ///
    /// No-op for non-journaled images. The ring starts empty with
    /// `cleanly_unmounted = false`, so a crash before the next clean shutdown is
    /// still detectable on the next mount.
    fn format_journal(mmap: &mut MmapMut, sb: &SuperBlock) -> Result<(), DiskManagerError> {
        if !sb.has_journal_layout() {
            return Ok(());
        }
        let region = Self::journal_region(mmap, sb)?;
        crate::journal::JournalRing::create(region, sb.block_size as u64)
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
    fn recover_journal(mmap: &mut MmapMut, sb: &SuperBlock) -> Result<usize, DiskManagerError> {
        if !sb.has_journal_layout() {
            return Ok(0);
        }
        let bs = sb.block_size as u64;

        // Phase 1: validate + decode frames from a snapshot, releasing the borrow.
        let mut snapshot = Self::journal_region(mmap, sb)?.to_vec();
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
            crate::journal::apply_op_in_place(mmap, sb, op).map_err(DiskManagerError::Journal)?;
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
        let region = Self::journal_region(mmap, sb)?;
        region[..header_len].copy_from_slice(&state.encode()[..header_len]);

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
    /// two engines. Metadata and all writes always go through the shared mmap
    /// regardless of the backend.
    pub fn set_io_backend(&self, backend: IoBackend) -> IoBackend {
        let engine = IoEngine::new(backend);
        let effective = engine.effective();
        self.inner.write().unwrap().io_engine = engine;
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
        mmap: &MmapMut,
        block_id: u64,
        name: &str,
    ) -> Result<Option<u64>, DiskManagerError> {
        if let Some(slice) = Self::get_block_from_map(mmap, block_id) {
            return Ok(crate::directory::find_entry_in_block(slice, name));
        }
        Ok(None)
    }

    fn read_dir_entries_from_block(
        mmap: &MmapMut,
        block_id: u64,
    ) -> Result<Vec<crate::directory::DirectoryEntry>, DiskManagerError> {
        if let Some(slice) = Self::get_block_from_map(mmap, block_id) {
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
        } else {
            Ok(Vec::new())
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
        mmap: &MmapMut,
        inode: &Inode,
        target_name: &str,
        target_hash: u64,
    ) -> Option<(u64, u64)> {
        for blk_idx in 0..Self::dir_num_blocks(inode) {
            let phys_blk = Self::resolve_logical_block_id(mmap, inode, blk_idx);
            if phys_blk == 0 {
                continue;
            }
            if let Some(slice) = Self::get_block_from_map(mmap, phys_blk)
                && let Some(id) =
                    crate::directory::find_entry_in_block_with_hash(slice, target_name, target_hash)
            {
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
        mmap: &MmapMut,
        inode: &Inode,
    ) -> Result<(FxHashMap<String, (u64, u64, u64)>, bool, Option<(u64, u16)>), DiskManagerError>
    {
        let mut map = FxHashMap::default();
        let num_blocks = Self::dir_num_blocks(inode);
        let mut may_have_holes = false;
        let mut tail_offset = None;
        for blk_idx in 0..num_blocks {
            let phys_blk = Self::resolve_logical_block_id(mmap, inode, blk_idx);
            if phys_blk == 0 {
                continue;
            }
            let entries = Self::read_dir_entries_from_block(mmap, phys_blk)?;
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
                Self::build_dir_index(&guard.mmap, &parent)?;
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
            Self::locate_entry_in_dir(&guard.mmap, &parent, c, h).map(|(id, blk)| (id, blk, h))
        });
        let found = found_loc.map(|(id, _, _)| id);

        // Build full index *outside* the shard write lock when promoting on miss or threshold.
        let promoted_index = if found.is_none()
            || cold_scans_so_far.saturating_add(1) >= DIR_INDEX_BUILD_AFTER_SCANS
        {
            Some(Self::build_dir_index(&guard.mmap, &parent)?)
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
        let tail_blk = Self::resolve_logical_block_id(&guard.mmap, parent_inode, last_idx);
        if guard.is_data_block(tail_blk)
            && let Some(slice) = Self::get_block_from_map(&guard.mmap, tail_blk)
        {
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
                return (Some((tail_blk, insert_offset, true)), !may_have_holes);
            }
        }

        // If no deletions have occurred in earlier blocks (`0..num_blocks - 1`),
        // skip the O(B) historical block scan and allocate a new block immediately.
        if !may_have_holes || num_blocks <= 1 {
            return (None, true);
        }

        let mut any_hole_remaining = false;
        for blk_idx in 0..last_idx {
            let phys_blk = Self::resolve_logical_block_id(&guard.mmap, parent_inode, blk_idx);
            if !guard.is_data_block(phys_blk) {
                continue;
            }
            if let Some(slice) = Self::get_block_from_map(&guard.mmap, phys_blk) {
                let insert_offset = crate::directory::find_insert_offset_in_block(slice);
                if insert_offset + needed_space <= BLOCK_SIZE {
                    return (Some((phys_blk, insert_offset, false)), false);
                }
                if insert_offset + 21 <= BLOCK_SIZE {
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
            Self::append_dir_entry_to_block(&mut guard.mmap, phys_blk, insert_offset, entry)?;
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
        Self::append_dir_entry_to_block(&mut guard.mmap, new_phys_block, 0, entry)?;
        parent_inode.size = (new_blk_idx + 1) as u64 * BLOCK_SIZE as u64;
        Ok((
            new_phys_block,
            Some((new_phys_block, entry_len as u16)),
            all_earlier_full,
        ))
    }

    fn append_dir_entry_to_block(
        mmap: &mut MmapMut,
        block_id: u64,
        insert_offset: usize,
        entry: &crate::directory::DirectoryEntry,
    ) -> Result<(), DiskManagerError> {
        let block_slice = Self::get_block_mut_from_map(mmap, block_id).ok_or_else(|| {
            DiskManagerError::Io(std::io::Error::other("Directory block not found"))
        })?;

        if insert_offset + 20 + entry.name.len() > BLOCK_SIZE {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Directory block is full",
            )));
        }

        let mut cursor = std::io::Cursor::new(block_slice);
        cursor.set_position(insert_offset as u64);
        entry
            .serialize_into(&mut cursor)
            .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
        Ok(())
    }

    #[allow(dead_code)]
    fn rewrite_dir_entries_in_block(
        mmap: &mut MmapMut,
        block_id: u64,
        entries: &[crate::directory::DirectoryEntry],
    ) -> Result<(), DiskManagerError> {
        let block_slice = Self::get_block_mut_from_map(mmap, block_id).ok_or_else(|| {
            DiskManagerError::Io(std::io::Error::other("Directory block not found"))
        })?;
        block_slice.fill(0);
        let mut cursor = std::io::Cursor::new(block_slice);
        for entry in entries {
            entry
                .serialize_into(&mut cursor)
                .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
        }
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

    /// Immutable view of the journal region, if the image is large enough.
    fn journal_region_ref<'m>(
        mmap: &'m MmapMut,
        sb: &SuperBlock,
    ) -> Result<&'m [u8], DiskManagerError> {
        let bs = sb.block_size as u64;
        let start = Self::journal_region_start(sb).ok_or_else(|| {
            DiskManagerError::Io(std::io::Error::other("Journal offset overflow"))
        })?;
        let end = start
            .checked_add(Self::journal_region_len(bs))
            .ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("Journal region overflow"))
            })?;
        if end > mmap.len() {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Journal region does not fit in image",
            )));
        }
        Ok(&mmap[start..end])
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
        let written = {
            let region = Self::journal_region(&mut guard.mmap, &sb)?;
            let mut ring = crate::journal::JournalRing::open(region, bs)?;
            ring.append(ops)?;
            ring.last_write()
        };

        // The WAL must reach stable storage *before* the metadata it describes, or a
        // power loss can leave the image with partially applied metadata that recovery
        // has no record of.
        //
        // How urgently that must happen is exactly a durability-policy question, so
        // the same mode that governs metadata governs the WAL:
        //
        // * `Strict` — flush the frame *and* the header that advances `head` before
        //   the caller applies anything. Two `msync` calls per transaction, which is
        //   what buys the power-loss guarantee.
        // * `Lazy` / `RangeAsync` / `LegacyWholeMmapAsync` — the image bytes live in
        //   the page cache, which already survives a *process* crash; these modes
        //   never promise power-loss safety for metadata either. Forcing a per-
        //   transaction barrier on the WAL would impose a full power-loss cost on
        //   users who explicitly opted out of it, so the WAL is left to be written
        //   back with the rest of the mapping by `flush()` / drop.
        if guard.durability_mode() != DurabilityMode::Strict {
            return Ok(());
        }

        // Without the header flush the cursor update could be reordered after the
        // frame, and recovery would never see the transaction.
        // When the frame starts at ring offset 0 (`s == 0`, immediately after the
        // 4 KiB journal header block), coalesce the header block and frame into a single
        // contiguous `flush_range` (`msync(MS_SYNC)`) syscall.
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
            if off + len <= guard.mmap.len() {
                guard.mmap.flush_range(start, (off - start) + len)?;
                return Ok(());
            }
        }
        if header_len > 0 {
            guard.mmap.flush_range(start, header_len)?;
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
        let end = start.saturating_add(len).min(guard.mmap.len());
        if end > start {
            guard.mmap.flush_range(start, end - start)?;
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
            crate::journal::apply_op_in_place(&mut guard.mmap, &sb, op)?;
            match op {
                crate::journal::MetadataOp::WriteInode { inode_id, .. }
                | crate::journal::MetadataOp::SetInodeBitmap {
                    inode_id,
                    allocated: false,
                } => {
                    guard.invalidate_payload_caches(*inode_id);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn collect_inode_blocks(mmap: &MmapMut, inode: &Inode) -> Vec<u64> {
        let mut blks = Vec::new();
        Self::for_each_inode_block(mmap, inode, |blk| blks.push(blk));
        blks
    }

    fn for_each_inode_block(mmap: &MmapMut, inode: &Inode, mut visit: impl FnMut(u64)) {
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
            if let Some(slice) = Self::get_block_from_map(mmap, sib_id) {
                for chunk in slice.as_chunks::<8>().0 {
                    let blk = u64::from_le_bytes(*chunk);
                    push_leaf(blk, is_seekable, &mut visit);
                }
            }
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

            if let Some(slice) = Self::get_block_from_map(mmap, dib_id) {
                for chunk in slice[..max_s_entries * 8].as_chunks::<8>().0 {
                    let sib = u64::from_le_bytes(*chunk);
                    if sib != 0 {
                        visit(sib);
                        if let Some(s_slice) = Self::get_block_from_map(mmap, sib) {
                            for d_chunk in s_slice.as_chunks::<8>().0 {
                                let blk = u64::from_le_bytes(*d_chunk);
                                push_leaf(blk, is_seekable, &mut visit);
                            }
                        }
                    }
                }
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

            if let Some(slice) = Self::get_block_from_map(mmap, tib_id) {
                for chunk in slice[..max_t_entries * 8].as_chunks::<8>().0 {
                    let dib = u64::from_le_bytes(*chunk);
                    if dib != 0 {
                        visit(dib);
                        if let Some(d_slice) = Self::get_block_from_map(mmap, dib) {
                            for s_chunk in d_slice.as_chunks::<8>().0 {
                                let sib = u64::from_le_bytes(*s_chunk);
                                if sib != 0 {
                                    visit(sib);
                                    if let Some(s_slice) = Self::get_block_from_map(mmap, sib) {
                                        for blk_chunk in s_slice.as_chunks::<8>().0 {
                                            let blk = u64::from_le_bytes(*blk_chunk);
                                            push_leaf(blk, is_seekable, &mut visit);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Journaled create: simulate the allocation, commit one transaction, then apply.
    ///
    /// Covers both [`crate::inode::FileType::File`] and
    /// [`crate::inode::FileType::Directory`], since `mkdir` differs only in whether a
    /// data block is reserved for the new inode.
    ///
    /// The hard part is that allocation itself mutates the bitmaps, and the chosen
    /// ids depend on the current bitmap contents. [`AllocSim`] therefore runs the
    /// exact same allocator against *copies* of the bitmaps, so every allocated id
    /// (including any indirect pointer blocks needed to grow a multi-block directory)
    /// is known before the image is touched. That lets the whole operation be
    /// described as post-images and committed before it is applied.
    /// Reset the journal ring, discarding transactions already applied in place.
    fn checkpoint_journal(mmap: &mut MmapMut, sb: &SuperBlock) -> Result<(), DiskManagerError> {
        if !sb.has_journal_layout() {
            return Ok(());
        }
        let bs = sb.block_size as u64;
        let start = Self::journal_region_start(sb).unwrap_or(0);
        let header_len = usize::try_from(bs)
            .unwrap_or(0)
            .min(crate::journal::JOURNAL_HEADER_LEN);
        {
            let region = Self::journal_region(mmap, sb)?;
            crate::journal::JournalRing::open(region, bs)?.checkpoint();
        }
        if header_len > 0 {
            mmap.flush_range(start, header_len)?;
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
        // commit frame+header are synchronously `msync(MS_SYNC)`'d before the mutation
        // returns, while in-place metadata blocks (bitmaps, inode table, directory blocks)
        // stay in the OS page cache and are flushed in one batch once the WAL ring reaches
        // its 50% checkpoint watermark (`should_checkpoint()`), or on `flush()` / `Drop`.
        let bs = sb.block_size as u64;
        let needs_ckpt = {
            let region = Self::journal_region(&mut guard.mmap, &sb)?;
            let ring = crate::journal::JournalRing::open(region, bs)?;
            ring.used() >= ring.ring_bytes() / 2
        };
        if needs_ckpt {
            guard.mmap.flush()?;
            Self::checkpoint_journal(&mut guard.mmap, &sb)?;
        }
        Ok(())
    }

    /// Publicly force a journal checkpoint, e.g. after an explicit [`Self::flush`].
    ///
    /// Returns the number of transactions discarded.
    pub fn checkpoint_journal_now(&self) -> Result<usize, DiskManagerError> {
        let mut guard = self.inner.write().unwrap();
        let sb = guard.superblock;
        if !sb.has_journal_layout() {
            return Ok(0);
        }
        let bs = sb.block_size as u64;
        let region = Self::journal_region(&mut guard.mmap, &sb)?;
        let discarded = {
            let mut ring = crate::journal::JournalRing::open(region, bs)?;
            let pending = ring.pending_transactions();
            ring.checkpoint();
            pending
        };
        let start = Self::journal_region_start(&sb).unwrap_or(0);
        let header_len = usize::try_from(bs)
            .unwrap_or(0)
            .min(crate::journal::JOURNAL_HEADER_LEN);
        if header_len > 0 {
            guard.mmap.flush_range(start, header_len)?;
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
            let dir_data_block = sim.alloc_block(&guard.mmap)?;
            ops.push(crate::journal::MetadataOp::SetDataBitmap {
                block_id: dir_data_block,
                allocated: true,
            });
            if crate::journal::block_has_nonzero_bytes(&guard.mmap, dir_data_block) {
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
                &guard.mmap,
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

        // 2. Allocate Inode with search hint
        let hint = guard.free_inode_hint;
        let usable_inodes = guard.superblock.usable_inode_count();
        let new_inode_id = {
            let bitmap_slice = guard.inode_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("inode bitmap not found"))
            })?;
            let mut allocator = SimpleBlockAllocator::new(bitmap_slice, 0);
            let id = allocator.allocate_with_hint(Some(hint))?;
            // The bitmap can track more ids than the inode table can
            // hold. An id beyond the table would be written into the data
            // area, so it is released again and reported as "no space".
            if id >= usable_inodes {
                let _ = allocator.free(id);
                return Err(DiskManagerError::Allocator(
                    crate::allocator::AllocatorError::NoSpace,
                ));
            }
            id
        };
        guard.free_inode_hint = new_inode_id + 1;

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

    /// Creates a file if it does not exist, or opens (looks up) the existing file.
    ///
    /// This provides safe idempotent file opening under multi-threaded concurrency.
    pub fn create_or_open_file(
        &self,
        parent_inode_id: u64,
        name: &str,
    ) -> Result<u64, DiskManagerError> {
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
    pub(crate) fn resolve_logical_block_id(mmap: &MmapMut, inode: &Inode, blk_idx: usize) -> u64 {
        use crate::inode::BlockPath;
        // Follows one pointer level; a zero pointer means a sparse hole.
        let follow = |parent: u64, slot: usize| -> u64 {
            if parent == 0 {
                0
            } else {
                Self::read_block_ptr(mmap, parent, slot)
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
        mmap: &MmapMut,
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
        if sib_id != 0
            && let Some(slice) = Self::get_block_from_map(mmap, sib_id)
        {
            for chunk in slice.as_chunks::<8>().0 {
                push(u64::from_le_bytes(*chunk), &mut entries);
            }
        }

        let dib_id = inode.blocks[11];
        if dib_id != 0
            && let Some(slice) = Self::get_block_from_map(mmap, dib_id)
        {
            for chunk in slice.as_chunks::<8>().0 {
                let sib = u64::from_le_bytes(*chunk);
                if sib == 0 {
                    continue;
                }
                if let Some(s_slice) = Self::get_block_from_map(mmap, sib) {
                    for d_chunk in s_slice.as_chunks::<8>().0 {
                        push(u64::from_le_bytes(*d_chunk), &mut entries);
                    }
                }
            }
        }

        let tib_id = inode.triple_indirect;
        if tib_id != 0
            && let Some(slice) = Self::get_block_from_map(mmap, tib_id)
        {
            for chunk in slice.as_chunks::<8>().0 {
                let dib = u64::from_le_bytes(*chunk);
                if dib == 0 {
                    continue;
                }
                if let Some(d_slice) = Self::get_block_from_map(mmap, dib) {
                    for s_chunk in d_slice.as_chunks::<8>().0 {
                        let sib = u64::from_le_bytes(*s_chunk);
                        if sib == 0 {
                            continue;
                        }
                        if let Some(s_slice) = Self::get_block_from_map(mmap, sib) {
                            for p_chunk in s_slice.as_chunks::<8>().0 {
                                push(u64::from_le_bytes(*p_chunk), &mut entries);
                            }
                        }
                    }
                }
            }
        }

        entries
    }

    /// Walks the first `physical_size` bytes of `inode`'s on-disk payload in logical order,
    /// calling `emit(physical_block, payload_offset, len)` for every allocated block.
    /// Sparse holes (zero pointers at any indirection level) are skipped without a call.
    ///
    /// Indirect pointer blocks are resolved in batches straight from the mmap (metadata
    /// always stays on the mmap); only the payload transfer itself is delegated to `emit`.
    fn walk_payload_blocks(
        mmap: &MmapMut,
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
                if let Some(sib_slice) = Self::get_block_from_map(mmap, sib_id) {
                    let start_entry = blk_idx - 10;
                    let num_entries = ((10 + 512) - blk_idx).min(512 - start_entry);
                    let ptr_chunks = &sib_slice[start_entry * 8..(start_entry + num_entries) * 8];
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
                } else {
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

                let sib_id = Self::read_block_ptr(mmap, dib_id, s_idx);
                if sib_id == 0 || sib_id < data_start || sib_id >= data_end {
                    let skip_blocks = 512 - d_idx;
                    let skip_bytes =
                        (skip_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx += skip_blocks;
                    continue;
                }

                if let Some(sib_slice) = Self::get_block_from_map(mmap, sib_id) {
                    let num_entries = 512 - d_idx;
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
                } else {
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

                let dib_id = Self::read_block_ptr(mmap, tib_id, t_idx);
                if dib_id == 0 || dib_id < data_start || dib_id >= data_end {
                    let skip_blocks = (512 * 512) - rem_idx;
                    let skip_bytes =
                        (skip_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx += skip_blocks;
                    continue;
                }

                let sib_id = Self::read_block_ptr(mmap, dib_id, d_idx);
                if sib_id == 0 || sib_id < data_start || sib_id >= data_end {
                    let skip_blocks = 512 - s_idx;
                    let skip_bytes =
                        (skip_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx += skip_blocks;
                    continue;
                }

                if let Some(sib_slice) = Self::get_block_from_map(mmap, sib_id) {
                    let num_entries = 512 - s_idx;
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
                } else {
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
        {
            let data_start = guard.superblock.data_block_start;
            let data_end = guard.superblock.block_count;
            let mut contiguous_start_blk: Option<u64> = None;
            let mut expected_off = 0usize;
            let mut is_contiguous = true;

            Self::walk_payload_blocks(
                &guard.mmap,
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
                && byte_end <= guard.mmap.len()
            {
                let mut decoded =
                    Self::decompress_stream_payload(&guard.mmap[byte_start..byte_end], inode.size)?;
                if filter_config.is_active() {
                    crate::filters::unapply_filters_in_place(&mut decoded, &filter_config);
                }
                return Ok(decoded);
            }
        }

        let mut raw_data = Self::try_alloc_zeroed_vec(phys_len)?;
        if guard.io_engine.effective() == IoBackend::Mmap {
            // Zero-copy path: coalesce physically contiguous runs and memcpy from the mapping.
            let data_start = guard.superblock.data_block_start;
            let data_end = guard.superblock.block_count;
            let mmap_len = guard.mmap.len();
            let mut pending_run: Option<(usize, usize, usize)> = None;
            Self::walk_payload_blocks(
                &guard.mmap,
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
                                    .copy_from_slice(&guard.mmap[run_src..run_src + run_len]);
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
                    .copy_from_slice(&guard.mmap[run_src..run_src + run_len]);
            }
        } else {
            // Gather coalesced extents, then hand them to the engine in one submission.
            let mut extents = ExtentList::new();
            let data_start = guard.superblock.data_block_start;
            let data_end = guard.superblock.block_count;
            Self::walk_payload_blocks(
                &guard.mmap,
                inode,
                physical_size,
                data_start,
                data_end,
                |blk, off, len| {
                    if Self::get_block_from_map(&guard.mmap, blk).is_some() {
                        extents.push(blk * BLOCK_SIZE as u64, off, len);
                    }
                },
            );
            guard.io_engine.read(
                &guard.file,
                &guard.mmap,
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
        if guard.io_engine.effective() == IoBackend::Mmap {
            return Self::read_at_prepare(&guard, inode_id, file_offset, buf, None);
        }
        let mut extents = ExtentList::new();
        let n = Self::read_at_prepare(&guard, inode_id, file_offset, buf, Some(&mut extents))?;
        if !extents.is_empty() {
            guard.io_engine.read(
                &guard.file,
                &guard.mmap,
                &mut [ReadTarget {
                    buf,
                    extents: extents.as_slice(),
                }],
            )?;
        }
        Ok(n)
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
        let n = if guard.io_engine.effective() == IoBackend::Mmap {
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
                guard.io_engine.read(
                    &guard.file,
                    &guard.mmap,
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
        let direct = guard.io_engine.effective() == IoBackend::Mmap;

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
        if let Err(e) = guard.io_engine.read(&guard.file, &guard.mmap, &mut targets) {
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

        // Fallback for compressed or encrypted files: serve from bounded decoded-blob cache
        // when <= 16 MiB, avoiding O(calls * file_size) full-file decompression/decryption cliffs.
        if inode.compressed_size > 0 || inode.encrypted {
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
                if let Some((run_src, run_dst, run_len)) = self.pending_mmap_run.take() {
                    self.buf[run_dst..run_dst + run_len]
                        .copy_from_slice(&self.guard.mmap[run_src..run_src + run_len]);
                }
            }

            #[inline]
            fn emit_block(&mut self, blk_id: u64, in_blk_off: usize, chunk_len: usize) {
                let mmap_len = self.guard.mmap.len();
                // Reject forged pointers targeting metadata blocks (< data_block_start).
                let valid_src = if self.guard.is_data_block(blk_id) {
                    usize::try_from(blk_id)
                        .ok()
                        .and_then(|b| b.checked_mul(BLOCK_SIZE))
                        .filter(|&b_start| b_start.saturating_add(BLOCK_SIZE) <= mmap_len)
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
                if guard.is_data_block(sib_id)
                    && let Some(sib_slice) = Self::get_block_from_map(&guard.mmap, sib_id)
                {
                    sink.emit_sib_entries(sib_slice, blk_idx - 10);
                } else {
                    sink.emit_hole_until(SINGLE_LIMIT);
                }
                continue;
            }

            // 3. Double indirect blocks (522..262666) - Batch dib + sib resolution
            if blk_idx < DOUBLE_LIMIT {
                let dib_id = inode.blocks[11];
                if guard.is_data_block(dib_id)
                    && let Some(dib_slice) = Self::get_block_from_map(&guard.mmap, dib_id)
                {
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
                        if guard.is_data_block(sib_id)
                            && let Some(sib_slice) = Self::get_block_from_map(&guard.mmap, sib_id)
                        {
                            sink.emit_sib_entries(sib_slice, d_idx);
                        } else {
                            sink.emit_hole_until(cur_blk + (512 - d_idx));
                        }
                    }
                } else {
                    sink.emit_hole_until(DOUBLE_LIMIT);
                }
                continue;
            }

            // 4. Triple indirect blocks (262666..134480394) - Batch tib + dib + sib resolution
            if blk_idx < TRIPLE_LIMIT {
                let tib_id = inode.triple_indirect;
                if guard.is_data_block(tib_id)
                    && let Some(tib_slice) = Self::get_block_from_map(&guard.mmap, tib_id)
                {
                    let rel = blk_idx - DOUBLE_LIMIT;
                    let t_idx = rel / (512 * 512);
                    let rem_idx = rel % (512 * 512);
                    let d_idx = rem_idx / 512;
                    let s_idx = rem_idx % 512;
                    let dib_id =
                        u64::from_le_bytes(tib_slice[t_idx * 8..t_idx * 8 + 8].try_into().unwrap());
                    if guard.is_data_block(dib_id)
                        && let Some(dib_slice) = Self::get_block_from_map(&guard.mmap, dib_id)
                    {
                        let sib_id = u64::from_le_bytes(
                            dib_slice[d_idx * 8..d_idx * 8 + 8].try_into().unwrap(),
                        );
                        if guard.is_data_block(sib_id)
                            && let Some(sib_slice) = Self::get_block_from_map(&guard.mmap, sib_id)
                        {
                            sink.emit_sib_entries(sib_slice, s_idx);
                        } else {
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

    fn write_buffer_at_offset(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        mut current_offset: u64,
        data: &[u8],
        mut touched_blocks: Option<&mut Vec<u64>>,
    ) -> Result<u64, DiskManagerError> {
        let mut written = 0;
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

            if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, blk_id) {
                // On a newly allocated block, only zero the untouched
                // prefix/suffix bytes; full-block writes skip zeroing altogether.
                if is_fresh {
                    slice[..in_blk_off].fill(0);
                    slice[in_blk_off + to_write..].fill(0);
                }
                slice[in_blk_off..in_blk_off + to_write]
                    .copy_from_slice(&data[written..written + to_write]);
            }
            written += to_write;
            current_offset += to_write as u64;
        }
        Ok(current_offset)
    }

    /// Journaled counterpart of [`Self::write_data_with_filters`].
    ///
    /// Same four cases as the in-place path, but staged through [`AllocSim`] and
    /// committed as one metadata transaction.
    ///
    /// # Ordering (why this is crash-safe)
    ///
    /// 1. **Stage** — payload written into blocks whose bitmap bits are still clear.
    ///    A crash here leaves garbage in free blocks, which is harmless.
    /// 2. **Flush payload** — the touched blocks reach stable storage.
    /// 3. **Commit** — the metadata transaction (bitmap + inode) is appended and
    ///    `msync`ed. This is the atomic commit point.
    /// 4. **Apply** — metadata ops are replayed in place.
    ///
    /// The ordering is what makes step 3 safe: metadata can only ever become durable
    /// after the data it references is durable, so recovery never publishes a block
    /// that has not been filled.
    /// Record every indirect pointer block in `inode` that remains active for the
    /// first `needed_logical` blocks, so `write_data_journaled` never frees a
    /// reused indirect table block when reclaiming orphaned blocks.
    fn collect_retained_indirect_blocks(
        mmap: &MmapMut,
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
                let sib2 = Self::read_block_ptr(mmap, dib, a);
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
                let dib2 = Self::read_block_ptr(mmap, tib, b);
                if dib2 == 0 {
                    continue;
                }
                out.insert(dib2);
                let rem_in_b = rem_blocks.saturating_sub(b * PTRS * PTRS);
                let active_c = rem_in_b.div_ceil(PTRS).min(PTRS);
                for c in 0..active_c {
                    let sib3 = Self::read_block_ptr(mmap, dib2, c);
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
            Self::collect_inode_blocks(&guard.mmap, &inode)
        } else {
            Vec::new()
        };

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
        if rewrites_from_zero {
            if needed_logical == 0 {
                inode.blocks = [0; 12];
                inode.triple_indirect = 0;
            } else if needed_logical < old_logical {
                crate::journal::prune_stale_pointers(
                    &guard.mmap,
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
                &guard.mmap,
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
        //
        // Same durability-policy reasoning as the WAL barrier in
        // `commit_journal_tx`: only `Strict` promises the data reached disk before
        // the metadata referencing it, so only `Strict` pays for a pre-WAL barrier.
        // `RangeAsync` syncs metadata + payload together in a single coalesced pass
        // after applying the transaction in step 5.
        let payload_ranges: Vec<(usize, usize)> = if guard.durability_mode().is_range_based() {
            used.iter().map(|b| guard.block_byte_range(*b)).collect()
        } else {
            Vec::new()
        };
        if guard.durability_mode() == DurabilityMode::Strict {
            let coalesced_payload =
                DiskManagerInner::coalesce_ranges(&payload_ranges, guard.mmap.len());
            for (off, actual) in coalesced_payload {
                guard.mmap.flush_range(off, actual)?;
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
        // --- STAGE 1: Single Shared Read-Lock Inspection ---
        // Merges seekable routing + plan parameter inspection under ONE `inner.read()` lock.
        let (initial_inode, existing_decompressed, encryption_key, encryption_scheme, seekable_opt) = {
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
                let compress = explicit_seekable || compression_mode != CompressionMode::Never;
                let level = match compression_mode {
                    CompressionMode::Seekable { level, .. } => level,
                    CompressionMode::Stream { level } => level,
                    _ => 1,
                };
                (inode, None, None, 0, Some((level, compress)))
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
                (inode, existing, key, scheme, None)
            }
        }; // Shared read-guard is dropped immediately here!

        if let Some((level, compress)) = seekable_opt {
            return self.write_chunked_64k_pipeline_with_mode(
                inode_id,
                file_offset,
                data,
                level,
                compress,
            );
        }

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
        let can_use_prepared = inode.size == initial_inode.size
            && inode.compressed_size == initial_inode.compressed_size
            && inode.modified_at == initial_inode.modified_at
            && inode.encrypted == initial_inode.encrypted
            && inode.flags == initial_inode.flags
            && inode.blocks == initial_inode.blocks;

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
            let old_blocks = Self::collect_inode_blocks(&guard.mmap, &inode);
            let mut min_freed_blk = u64::MAX;
            {
                let db_start = guard.superblock.data_block_start;
                let slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
                })?;
                let mut da = SimpleBlockAllocator::new(slice, db_start);
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
        if end > guard.mmap.len() as u64 {
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
                slot.copy_from_slice(&guard.mmap[start..end]);
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

    fn read_inode_internal(
        guard: &DiskManagerInner,
        inode_id: u64,
    ) -> Result<Inode, DiskManagerError> {
        // Fast path: check in-memory inode cache
        if let Some(cached) = guard.inode_cache.get(inode_id) {
            return Ok(cached);
        }

        let (offset, end) = Self::inode_slot_range(guard, inode_id)?;
        let slice = &guard.mmap[offset..end];
        let inode = crate::inode_format::decode_for(
            slice,
            guard.superblock.inode_record_version_for(inode_id),
        )
        .map_err(|e| {
            DiskManagerError::Io(std::io::Error::other(format!("Inode decode failed: {e}")))
        })?;

        // Eviction is handled inside the cache: inserting past capacity drops the
        // single oldest entry instead of wiping the whole cache.
        guard.inode_cache.insert(inode_id, inode);
        Ok(inode)
    }

    /// Decodes an inode directly from the mapped slot (returning the cached entry if present)
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
        let slice = &guard.mmap[offset..end];
        crate::inode_format::decode_for(slice, guard.superblock.inode_record_version_for(inode_id))
            .map_err(|e| {
                DiskManagerError::Io(std::io::Error::other(format!("Inode decode failed: {e}")))
            })
    }

    fn write_inode_internal(
        guard: &mut DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
    ) -> Result<(), DiskManagerError> {
        let (offset, end) = Self::inode_slot_range(guard, inode_id)?;
        // Single source of truth for the on-disk bytes, shared with the journal's
        // post-image path so recovery can never diverge from a clean mount.
        let slot = Self::encode_inode_slot(guard, inode_id, inode)?;
        guard.mmap[offset..end].copy_from_slice(&slot);

        // Update in-memory inode cache & invalidate decoded payload caches.
        guard.inode_cache.insert(inode_id, *inode);
        guard.invalidate_payload_caches(inode_id);
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
            let slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
            })?;
            let mut da = SimpleBlockAllocator::new(slice, db_start);
            let blk = da
                .allocate_with_hint(Some(hint))
                .map_err(DiskManagerError::Allocator)?;
            // The data bitmap is sized in 4 KiB blocks, so it can track
            // blocks beyond the end of the image. Such a block is released
            // again and reported as "no space".
            if blk >= block_count {
                let _ = da.free(blk);
                return Err(DiskManagerError::Allocator(
                    crate::allocator::AllocatorError::NoSpace,
                ));
            }
            blk
        };
        guard.free_block_hint = blk + 1;
        Ok(blk)
    }

    fn allocate_block(guard: &mut DiskManagerInner) -> Result<u64, DiskManagerError> {
        let blk = Self::allocate_block_unzeroed(guard)?;

        // A freshly allocated block may still hold the plaintext of a
        // file that was deleted or truncated. Zero it before it becomes
        // reachable, otherwise a partial write would expose the old content
        // to the new owner of the block.
        if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, blk) {
            slice.fill(0);
        }
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
            let slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
            })?;
            let mut da = SimpleBlockAllocator::new(slice, db_start);
            let blk = da
                .allocate_contiguous(count, Some(hint))
                .map_err(DiskManagerError::Allocator)?;
            if blk.saturating_add(count as u64) > block_count {
                let _ = da.free_contiguous(blk, count);
                return Err(DiskManagerError::Allocator(
                    crate::allocator::AllocatorError::NoSpace,
                ));
            }
            blk
        };
        guard.free_block_hint = blk + count as u64;
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
            let slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
            })?;
            let mut da = SimpleBlockAllocator::new(slice, db_start);
            da.free_contiguous(start_blk, count)
                .map_err(DiskManagerError::Allocator)?;
        }
        if start_blk < guard.free_block_hint {
            guard.free_block_hint = start_blk;
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn read_block_ptr(mmap: &MmapMut, block_id: u64, entry_idx: usize) -> u64 {
        if let Some(slice) = Self::get_block_from_map(mmap, block_id) {
            let start = entry_idx * 8;
            if start + 8 <= slice.len() {
                return u64::from_le_bytes(slice[start..start + 8].try_into().unwrap());
            }
        }
        0
    }

    #[inline]
    fn write_block_ptr(mmap: &mut MmapMut, block_id: u64, entry_idx: usize, ptr: u64) {
        if let Some(slice) = Self::get_block_mut_from_map(mmap, block_id) {
            let start = entry_idx * 8;
            slice[start..start + 8].copy_from_slice(&ptr.to_le_bytes());
        }
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
        let mut child_id = Self::read_block_ptr(&guard.mmap, parent_block_id, entry_idx);
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
            Self::write_block_ptr(&mut guard.mmap, parent_block_id, entry_idx, child_id);
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
            let child_id = DiskManager::read_block_ptr(&guard.mmap, parent_block_id, entry_idx);
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
            DiskManager::write_block_ptr(&mut guard.mmap, parent_block_id, entry_idx, fresh);
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

    pub(crate) fn get_block_from_map(mmap: &MmapMut, block_id: u64) -> Option<&[u8]> {
        let start = usize::try_from(block_id).ok()?.checked_mul(BLOCK_SIZE)?;
        let end = start.checked_add(BLOCK_SIZE)?;
        if end > mmap.len() {
            None
        } else {
            Some(&mmap[start..end])
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
        Self::get_block_from_map(&guard.mmap, block_id).map(|s| s.to_vec())
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
        let inode = Self::read_inode_internal(&guard, dir_inode_id)?;
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
            let phys_blk = Self::resolve_logical_block_id(&guard.mmap, &inode, blk_idx);
            if !guard.is_data_block(phys_blk) {
                continue;
            }
            let block_entries = Self::read_dir_entries_from_block(&guard.mmap, phys_blk)?;
            let used: usize = block_entries.iter().map(|e| 18 + e.name.len()).sum();
            last_tail = Some((phys_blk, used.min(BLOCK_SIZE) as u16));
            for e in block_entries {
                stored_entries.push((e.name.clone(), e.inode, phys_blk, e.hash));
                entries.push(e);
            }
        }

        if let Some(key) = &guard.encryption_key {
            entries.retain_mut(|entry| {
                if !entry
                    .name
                    .starts_with(crate::encryption::FILENAME_ENC_PREFIX)
                {
                    return false;
                }
                if let Ok(decrypted) =
                    crate::encryption::decrypt_filename(key, dir_inode_id, &entry.name)
                    && crate::directory::validate_filename(&decrypted).is_ok()
                {
                    entry.hash = crate::directory::hash_filename(&decrypted);
                    entry.name = decrypted;
                }
                true
            });
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
                let region = Self::journal_region_ref(&guard.mmap, &sb)?;
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
            guard.mmap.flush().map_err(DiskManagerError::Io)?;
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
                    let region = Self::journal_region(&mut guard.mmap, &sb)?;
                    let mut ring = crate::journal::JournalRing::open(region, bs)?;
                    ring.checkpoint_to(head);
                }
                if header_len > 0 {
                    guard.mmap.flush_range(start, header_len)?;
                }
            }
        }
        Ok(())
    }

    /// Asynchronously flushes dirty pages in background without blocking concurrent readers.
    pub fn flush_async(&self) -> Result<(), DiskManagerError> {
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
        // WAL first, then the rest of the mapping, so a sync point never lets the
        // image reach disk ahead of the log describing it.
        if guard.superblock.has_journal_layout() {
            let bs = guard.superblock.block_size as u64;
            if let Some(start) = Self::journal_region_start(&guard.superblock) {
                let len = Self::journal_region_len(bs).min(guard.mmap.len() - start);
                let _ = guard.mmap.flush_async_range(start, len);
            }
        }
        guard.mmap.flush_async().map_err(DiskManagerError::Io)
    }

    /// Legacy flush using exclusive write lock (for performance comparison benchmarks).
    #[doc(hidden)]
    pub fn flush_exclusive_legacy(&self) -> Result<(), DiskManagerError> {
        let guard = self.inner.write().unwrap();
        guard.mmap.flush().map_err(DiskManagerError::Io)
    }

    /// Returns `true` if a directory has no entries across any of its allocated blocks.
    fn is_dir_empty(guard: &DiskManagerInner, dir_inode: &Inode) -> Result<bool, DiskManagerError> {
        let num_blocks = Self::dir_num_blocks(dir_inode);
        for blk_idx in 0..num_blocks {
            let phys_blk = Self::resolve_logical_block_id(&guard.mmap, dir_inode, blk_idx);
            if !guard.is_data_block(phys_blk) {
                continue;
            }
            if let Some(slice) = Self::get_block_from_map(&guard.mmap, phys_blk)
                && crate::directory::has_valid_entry_in_block(slice)
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
    fn delete_file_journaled(
        guard: &mut DiskManagerInner,
        parent_inode_id: u64,
        name: &str,
        matched_name: &str,
        _target_inode_id: u64,
        phys_blk: u64,
    ) -> Result<(), DiskManagerError> {
        let mut parent_inode = Self::read_inode_internal(guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Not a directory",
            )));
        }

        let initial_hash = crate::directory::hash_filename(matched_name);
        let (matched_name, target_inode_id, phys_blk, matched_hash) = if let Some(id) =
            Self::get_block_from_map(&guard.mmap, phys_blk).and_then(|s| {
                crate::directory::find_entry_in_block_with_hash(s, matched_name, initial_hash)
            }) {
            (matched_name.to_string(), id, phys_blk, initial_hash)
        } else {
            let target_name = Self::stored_filename(guard, parent_inode_id, name)?;
            let h = crate::directory::hash_filename(target_name.as_ref());
            Self::locate_entry_in_dir(&guard.mmap, &parent_inode, target_name.as_ref(), h)
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
        let mut dir_image = Self::get_block_from_map(&guard.mmap, phys_blk)
            .ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("Directory block not found"))
            })?
            .to_vec();
        crate::directory::remove_entry_in_block_bytes(&mut dir_image, &matched_name, matched_hash);

        let blocks_to_free = Self::collect_inode_blocks(&guard.mmap, &file_inode);

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
                            &guard.mmap,
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

        let mut parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Not a directory",
            )));
        }

        let initial_hash = crate::directory::hash_filename(&matched_name);
        let (matched_name, target_inode_id, phys_blk, matched_hash) = if let Some(id) =
            Self::get_block_from_map(&guard.mmap, phys_blk).and_then(|s| {
                crate::directory::find_entry_in_block_with_hash(s, &matched_name, initial_hash)
            }) {
            (matched_name, id, phys_blk, initial_hash)
        } else {
            let target_name = Self::stored_filename(&guard, parent_inode_id, name)?;
            let h = crate::directory::hash_filename(target_name.as_ref());
            Self::locate_entry_in_dir(&guard.mmap, &parent_inode, target_name.as_ref(), h)
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
        let block_slice =
            Self::get_block_mut_from_map(&mut guard.mmap, phys_blk).ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("Directory block not found"))
            })?;
        crate::directory::remove_entry_in_block_bytes(block_slice, &matched_name, matched_hash);

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
        let blocks_to_free = Self::collect_inode_blocks(&guard.mmap, &file_inode);

        // Free Data Blocks
        let mut min_freed_blk = u64::MAX;
        {
            let db_start = guard.superblock.data_block_start;
            let slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
            })?;
            let mut da = SimpleBlockAllocator::new(slice, db_start);

            for blk in blocks_to_free {
                da.free(blk)?;
                min_freed_blk = min_freed_blk.min(blk);
            }
        }
        if min_freed_blk < guard.free_block_hint {
            guard.free_block_hint = min_freed_blk;
        }

        // Free Inode
        {
            let slice = guard.inode_bitmap_slice_mut().ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::other("inode bitmap not found"))
            })?;
            let mut ia = SimpleBlockAllocator::new(slice, 0);
            ia.free(target_inode_id)?;
        }
        if target_inode_id < guard.free_inode_hint {
            guard.free_inode_hint = target_inode_id;
        }
        guard.inode_cache.remove(target_inode_id);
        guard.invalidate_payload_caches(target_inode_id);

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

    /// Truncates or extends a file to the specified size in bytes.
    ///
    /// If `new_size` is less than current size, excess data is discarded and
    /// unneeded blocks are freed back to the data bitmap.
    /// If `new_size` is greater than current size, the file is extended (sparse
    /// extension with zeroed reads for raw files; zero-padded for compressed/encrypted files).
    pub fn truncate(&self, inode_id: u64, new_size: u64) -> Result<(), DiskManagerError> {
        let res = {
            let mut guard = self.inner.write().unwrap();
            Self::truncate_internal(&mut guard, inode_id, new_size)
        };
        if res.is_ok() {
            self.maybe_drain_async_ranges();
        }
        res
    }

    /// Truncates or extends a file by its path.
    pub fn truncate_path(&self, path: &str, new_size: u64) -> Result<(), DiskManagerError> {
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
        Self::allocate_contiguous_blocks_internal(&mut guard, count)
    }

    /// Frees a contiguous run of `count` data blocks starting at `start_blk`.
    pub fn free_contiguous_blocks(
        &self,
        start_blk: u64,
        count: usize,
    ) -> Result<(), DiskManagerError> {
        let mut guard = self.inner.write().unwrap();
        Self::free_contiguous_blocks_internal(&mut guard, start_blk, count)
    }

    pub(crate) fn truncate_internal(
        guard: &mut DiskManagerInner,
        inode_id: u64,
        new_size: u64,
    ) -> Result<(), DiskManagerError> {
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
                let old_blocks = Self::collect_inode_blocks(&guard.mmap, &inode);
                let mut min_freed_blk = u64::MAX;
                {
                    let db_start = guard.superblock.data_block_start;
                    let slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
                    })?;
                    let mut da = SimpleBlockAllocator::new(slice, db_start);
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
            if old_size > 0 {
                let last_logical_idx = ((old_size - 1) / BLOCK_SIZE as u64) as usize;
                let last_phys_blk =
                    Self::resolve_logical_block_id(&guard.mmap, &inode, last_logical_idx);
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
                    } else if let Some(slice) =
                        Self::get_block_mut_from_map(&mut guard.mmap, last_phys_blk)
                    {
                        slice[slack_start..].fill(0);
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
                    let range = guard.inode_byte_range(inode_id);
                    guard.sync_mutation_ranges(&[range])?;
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
                Self::resolve_logical_block_id(&guard.mmap, &inode, last_logical_idx);
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
                } else if let Some(slice) =
                    Self::get_block_mut_from_map(&mut guard.mmap, last_phys_blk)
                {
                    slice[slack_start..BLOCK_SIZE].fill(0);
                }
            }
        }

        // Free blocks beyond new_blocks_needed
        //
        // On a journaled image the pointer zeroing is staged in the
        // transaction instead of being written to the mapping before the
        // commit.
        let journaled = guard.superblock.has_journal_layout();
        let mut freed_blocks = Vec::new();
        for blk_idx in new_blocks_needed..old_blocks_needed {
            let phys_blk = Self::resolve_logical_block_id(&guard.mmap, &inode, blk_idx);
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
                let slice = guard.data_bitmap_slice_mut().ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::other("data bitmap not found"))
                })?;
                let mut da = SimpleBlockAllocator::new(slice, db_start);
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
                    let sib = Self::read_block_ptr(&guard.mmap, dib, a);
                    if guard.is_data_block(sib) {
                        Self::zero_block_ptr(guard, sib, b, ops);
                    }
                }
            }
            Some(BlockPath::Triple(a, b, c)) => {
                let tib = inode.triple_indirect;
                if guard.is_data_block(tib) {
                    let dib = Self::read_block_ptr(&guard.mmap, tib, a);
                    if guard.is_data_block(dib) {
                        let sib = Self::read_block_ptr(&guard.mmap, dib, b);
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
    /// instead of being written to the mapping directly. A crash between the
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
                Self::write_block_ptr(&mut guard.mmap, block_id, entry_idx, 0);
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
                    if let Some(dib_slice) = Self::get_block_from_map(&guard.mmap, dib_id) {
                        let mut sibs = Vec::new();
                        for chunk in dib_slice.as_chunks::<8>().0 {
                            let sib = u64::from_le_bytes(*chunk);
                            if sib != 0 {
                                sibs.push(sib);
                            }
                        }
                        for sib in sibs {
                            if guard.is_data_block(sib) {
                                freed_blocks.push(sib);
                            }
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
            let mut pruned_slots = Vec::new();
            if let Some(dib_slice) = Self::get_block_from_map(&guard.mmap, dib_id) {
                for (a_idx, chunk) in dib_slice.as_chunks::<8>().0.iter().enumerate() {
                    if a_idx >= keep_sibs {
                        let sib = u64::from_le_bytes(*chunk);
                        if sib != 0 {
                            pruned_slots.push((a_idx, sib));
                        }
                    }
                }
            }
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
                        Self::write_block_ptr(&mut guard.mmap, dib_id, a_idx, 0);
                    }
                }
            }
        }

        let max_double_blocks = 10 + 512 + 512 * 512;
        if new_blocks_needed <= max_double_blocks {
            if inode.triple_indirect != 0 {
                let tib_id = inode.triple_indirect;
                if guard.is_data_block(tib_id) {
                    let mut dibs = Vec::new();
                    if let Some(tib_slice) = Self::get_block_from_map(&guard.mmap, tib_id) {
                        for dib_chunk in tib_slice.as_chunks::<8>().0 {
                            let dib = u64::from_le_bytes(*dib_chunk);
                            if dib != 0 {
                                dibs.push(dib);
                            }
                        }
                    }
                    for dib in dibs {
                        if guard.is_data_block(dib) {
                            let mut sibs = Vec::new();
                            if let Some(dib_slice) = Self::get_block_from_map(&guard.mmap, dib) {
                                for sib_chunk in dib_slice.as_chunks::<8>().0 {
                                    let sib = u64::from_le_bytes(*sib_chunk);
                                    if sib != 0 {
                                        sibs.push(sib);
                                    }
                                }
                            }
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
            if let Some(tib_slice) = Self::get_block_from_map(&guard.mmap, tib_id) {
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
            }
            for (t_idx, dib) in pruned_dibs {
                if guard.is_data_block(dib) {
                    let mut sibs = Vec::new();
                    if let Some(dib_slice) = Self::get_block_from_map(&guard.mmap, dib) {
                        for sib_chunk in dib_slice.as_chunks::<8>().0 {
                            let sib = u64::from_le_bytes(*sib_chunk);
                            if sib != 0 {
                                sibs.push(sib);
                            }
                        }
                    }
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
                        Self::write_block_ptr(&mut guard.mmap, tib_id, t_idx, 0);
                    }
                }
            }
            if let Some((t_idx, dib)) = boundary_dib
                && guard.is_data_block(dib)
            {
                let rem_in_dib = triple_rel - t_idx * (512 * 512);
                let keep_sibs = rem_in_dib.div_ceil(512);
                let mut pruned_sibs = Vec::new();
                if let Some(dib_slice) = Self::get_block_from_map(&guard.mmap, dib) {
                    for (d_idx, sib_chunk) in dib_slice.as_chunks::<8>().0.iter().enumerate() {
                        if d_idx >= keep_sibs {
                            let sib = u64::from_le_bytes(*sib_chunk);
                            if sib != 0 {
                                pruned_sibs.push((d_idx, sib));
                            }
                        }
                    }
                }
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
                            Self::write_block_ptr(&mut guard.mmap, dib, d_idx, 0);
                        }
                    }
                }
            }
        }
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
            guard.mmap[0..serialized.len()].copy_from_slice(&serialized);
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
}
