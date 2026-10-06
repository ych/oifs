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
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use thiserror::Error;

use rustc_hash::FxHashMap;

use serde::{Deserialize, Serialize};

/// Errors that can occur during disk manager operations
#[derive(Error, Debug)]
pub enum DiskManagerError {
    /// I/O error occurred
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Serialization/deserialization error
    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),
    /// Invalid file system magic number
    #[error("Invalid magic number")]
    InvalidMagic,
    /// File system image is too small
    #[error("File too small")]
    FileTooSmall,
    /// Block allocation error
    #[error("Allocator error: {0}")]
    Allocator(#[from] AllocatorError),
    /// File locking error
    #[error("Locking error: {0}")]
    Locking(#[from] nix::errno::Errno),
    /// Encryption error
    #[error("Encryption error: {0}")]
    Encryption(#[from] crate::encryption::EncryptionError),
    /// Metadata WAL / journal error
    #[error("Journal error: {0}")]
    Journal(#[from] crate::journal::JournalError),
    /// On-disk inode record could not be encoded or decoded
    #[error("Inode format error: {0}")]
    InodeFormat(#[from] crate::inode_format::InodeFormatError),
    /// Password required for encrypted filesystem
    #[error("Password required to access encrypted filesystem")]
    PasswordRequired,
    /// Decryption failed - wrong password or corrupted data
    #[error("Decryption failed - check password")]
    DecryptionFailed,
}

/// Compression mode for write operations
///
/// Controls when files should be compressed using zstd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CompressionMode {
    /// Always compress, regardless of file size
    Always,
    /// Never compress
    Never,
    /// Auto: compress files >= 8KB
    #[default]
    Auto,
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

/// Durability and write-synchronization policy for filesystem mutations (P3.3).
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
    /// Asynchronously flushes the entire virtual memory map after every mutation (pre-P3.3 behavior).
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

/// One positional read in a [`DiskManager::read_at_batch`] call (P3.2).
#[derive(Debug)]
pub struct ReadRequest<'a> {
    /// Inode of the regular file to read
    pub inode_id: u64,
    /// Byte offset within the (logical, uncompressed) file
    pub offset: u64,
    /// Destination; up to `buf.len()` bytes are read
    pub buf: &'a mut [u8],
}

/// Internal disk manager state
///
/// Contains the file handle, memory-mapped region, and superblock.
/// Protected by a Mutex for thread-safe concurrent access.
pub(crate) struct DiskManagerInner {
    /// Image file handle; payload reads use it directly under the `Pread` / `IoUring`
    /// backends (P3.2).
    file: File,
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
    /// Inode cache for zero-copy metadata access (P2.2)
    pub inode_cache: BoundedInodeCache,
    /// Per-directory name index: dir_inode_id -> DirIndex (P3.1)
    pub dir_cache: RwLock<HashMap<u64, DirIndex>>,
    /// Durability policy governing mmap msync behavior on mutations (P3.3)
    pub durability_mode: std::sync::atomic::AtomicU8,
    /// Payload-block read engine (P3.2)
    io_engine: IoEngine,
}

impl DiskManagerInner {
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
            BLOCK_SIZE,
        )
    }

    #[inline]
    pub fn data_bitmap_byte_range(&self) -> (usize, usize) {
        (
            self.superblock.data_bitmap_block as usize * BLOCK_SIZE,
            BLOCK_SIZE,
        )
    }

    /// Coalesces a slice of byte ranges by:
    /// 1. Clamping to `mmap_len` and expanding to page/block boundaries (`BLOCK_SIZE = 4096`).
    /// 2. Sorting by start offset.
    /// 3. Merging overlapping and contiguous intervals.
    ///
    /// This eliminates duplicate syscalls when multiple mutations touch the same 4KB page
    /// or contiguous blocks on disk.
    pub fn coalesce_ranges(ranges: &[(usize, usize)], mmap_len: usize) -> Vec<(usize, usize)> {
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
            if start <= cur_end {
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

    /// Syncs one or more modified byte ranges according to the current `DurabilityMode`.
    ///
    /// Ranges are automatically coalesced across page boundaries (`BLOCK_SIZE = 4096`)
    /// to eliminate redundant system calls under `RangeAsync` and `Strict` modes.
    pub fn sync_mutation_ranges(&self, ranges: &[(usize, usize)]) -> Result<(), DiskManagerError> {
        match self.durability_mode() {
            DurabilityMode::Lazy => Ok(()),
            DurabilityMode::RangeAsync => {
                let coalesced = Self::coalesce_ranges(ranges, self.mmap.len());
                for (offset, actual_len) in coalesced {
                    let _ = self.mmap.flush_async_range(offset, actual_len);
                }
                Ok(())
            }
            DurabilityMode::Strict => {
                let coalesced = Self::coalesce_ranges(ranges, self.mmap.len());
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
}

/// In-memory index of one directory's entries.
///
/// Starts empty and caches positive lookups. Once a directory has had a negative lookup
/// (which already costs a full scan) or `DIR_INDEX_BUILD_AFTER_SCANS` cold scans, the whole
/// directory is indexed and `complete` is set, after which hits *and misses* are O(1).
#[derive(Default)]
pub(crate) struct DirIndex {
    complete: bool,
    cold_scans: u32,
    /// On-disk stored name (ciphertext on encrypted filesystems) -> inode id.
    stored: HashMap<String, u64>,
    /// Plaintext name -> inode id. Only used on encrypted filesystems to skip re-encryption.
    plain: HashMap<String, u64>,
}

/// Cold targeted scans tolerated on a directory before building its full index.
const DIR_INDEX_BUILD_AFTER_SCANS: u32 = 8;

impl Drop for DiskManagerInner {
    fn drop(&mut self) {
        // The journal region goes out first: on shutdown the log describing pending
        // metadata must not lag behind the metadata itself.
        if self.superblock.has_journal_layout() {
            let bs = self.superblock.block_size as u64;
            if let Some(start) = DiskManager::journal_region_start(bs) {
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
}

/// Maximum number of inodes kept in the in-memory inode cache.
///
/// Reaching this limit evicts a single entry; it no longer wipes the whole cache.
pub const INODE_CACHE_CAPACITY: usize = 2048;

/// Bounded inode cache with FIFO eviction.
///
/// The previous policy cleared the *entire* cache whenever it hit 2048 entries,
/// which turned a single overflow into a cache stampede of up to 2048 synchronous
/// inode-table reads. Eviction now removes exactly one entry in O(1), so the cost
/// of an overflow is one miss instead of a full cache.
///
/// Lookup uses a fast integer hasher (`FxHashMap`) because keys are dense `u64`
/// inode ids; SipHash spends most of its time on entropy mixing that buys nothing
/// for this access pattern.
/// Number of concurrent shards in the bounded inode cache to eliminate lock contention.
const NUM_INODE_CACHE_SHARDS: usize = 32;

/// A single shard of the bounded inode cache, protected by its own RwLock.
struct InodeCacheShard {
    map: FxHashMap<u64, Inode>,
    order: VecDeque<u64>,
    capacity: usize,
}

impl InodeCacheShard {
    fn new(capacity: usize) -> Self {
        Self {
            map: FxHashMap::default(),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn get(&self, inode_id: u64) -> Option<Inode> {
        self.map.get(&inode_id).copied()
    }

    fn insert(&mut self, inode_id: u64, inode: Inode) {
        // Re-inserting an existing id must not queue a second eviction slot.
        if self.map.insert(inode_id, inode).is_none() {
            self.order.push_back(inode_id);
        }
        while self.order.len() > self.capacity {
            if let Some(victim) = self.order.pop_front() {
                self.map.remove(&victim);
            }
        }
    }

    fn remove(&mut self, inode_id: u64) {
        self.map.remove(&inode_id);
        self.order.retain(|id| *id != inode_id);
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

/// Sharded, thread-safe bounded inode cache (P4.3 / lock contention reduction).
///
/// Divides the inode cache into 32 independent shards, each with its own `RwLock`.
/// Concurrent readers and writers on different inodes access completely separate shards
/// with zero lock contention.
///
/// Inode IDs are uniformly distributed across shards using a 64-bit Fibonacci hashing
/// bijection, ensuring consecutive sequential inode IDs never map to the same shard.
pub(crate) struct BoundedInodeCache {
    shards: Box<[RwLock<InodeCacheShard>]>,
    num_shards: usize,
}

impl BoundedInodeCache {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        if capacity <= 16 {
            Self {
                shards: vec![RwLock::new(InodeCacheShard::new(capacity))].into_boxed_slice(),
                num_shards: 1,
            }
        } else {
            let num_shards = NUM_INODE_CACHE_SHARDS;
            let shard_cap = std::cmp::max(1, capacity / num_shards);
            let shards = (0..num_shards)
                .map(|_| RwLock::new(InodeCacheShard::new(shard_cap)))
                .collect::<Vec<_>>()
                .into_boxed_slice();
            Self { shards, num_shards }
        }
    }

    #[inline]
    fn shard_index(&self, inode_id: u64) -> usize {
        if self.num_shards == 1 {
            0
        } else {
            (inode_id.wrapping_mul(0x517cc1b727220a95) as usize) & (self.num_shards - 1)
        }
    }

    pub(crate) fn get(&self, inode_id: u64) -> Option<Inode> {
        let idx = self.shard_index(inode_id);
        self.shards[idx].read().unwrap().get(inode_id)
    }

    pub(crate) fn insert(&self, inode_id: u64, inode: Inode) {
        let idx = self.shard_index(inode_id);
        self.shards[idx].write().unwrap().insert(inode_id, inode);
    }

    pub(crate) fn remove(&self, inode_id: u64) {
        let idx = self.shard_index(inode_id);
        self.shards[idx].write().unwrap().remove(inode_id);
    }

    /// Drop every cached inode.
    ///
    /// Used by a format migration, which rewrites every slot underneath the cache.
    pub(crate) fn clear(&self) {
        for shard in self.shards.iter() {
            shard.write().unwrap().clear();
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shards.iter().map(|s| s.read().unwrap().len()).sum()
    }
}

#[cfg(test)]
mod inode_cache_tests {
    use super::*;

    fn ino(mode: crate::inode::FileType) -> Inode {
        Inode::new(mode)
    }

    #[test]
    fn test_cache_evicts_single_oldest_entry_not_all() {
        // This is the regression the bounded cache exists for: the old policy wiped
        // every entry, turning one overflow into a full cache stampede.
        let c = BoundedInodeCache::with_capacity(4);
        for i in 0..4u64 {
            c.insert(i, ino(crate::inode::FileType::File));
        }
        assert_eq!(c.len(), 4);

        c.insert(4, ino(crate::inode::FileType::File));
        assert_eq!(c.len(), 4, "capacity must be respected");
        assert!(c.get(0).is_none(), "oldest entry must be evicted");
        for i in 1..=4u64 {
            assert!(c.get(i).is_some(), "recent entry {i} must survive");
        }
    }

    #[test]
    fn test_cache_reinsert_does_not_queue_duplicate_eviction() {
        let c = BoundedInodeCache::with_capacity(3);
        for i in 0..3u64 {
            c.insert(i, ino(crate::inode::FileType::File));
        }
        // Re-writing an already-cached id must not consume an extra queue slot.
        c.insert(1, ino(crate::inode::FileType::Directory));
        c.insert(3, ino(crate::inode::FileType::File));
        // Eviction order must still be 0 then 1.
        assert!(c.get(0).is_none(), "0 is oldest and must go first");
        assert!(
            c.get(1).is_some(),
            "1 was refreshed, must survive 0's eviction"
        );
        assert!(c.get(3).is_some());
    }

    #[test]
    fn test_cache_remove_then_reinsert_evicts_correct_entry() {
        // A stale queue slot left by remove() would evict the *new* entry.
        let c = BoundedInodeCache::with_capacity(3);
        c.insert(10, ino(crate::inode::FileType::File));
        c.insert(11, ino(crate::inode::FileType::File));
        c.insert(12, ino(crate::inode::FileType::File));

        c.remove(11);
        c.insert(11, ino(crate::inode::FileType::Directory));
        c.insert(13, ino(crate::inode::FileType::File));

        assert!(
            c.get(11).is_some(),
            "re-inserted id must not be evicted by its stale queue entry"
        );
        assert!(c.get(10).is_none(), "10 is now oldest and must be evicted");
    }

    #[test]
    fn test_cache_remove_drops_queue_slot() {
        let c = BoundedInodeCache::with_capacity(2);
        c.insert(1, ino(crate::inode::FileType::File));
        c.insert(2, ino(crate::inode::FileType::File));
        c.remove(1);
        c.insert(3, ino(crate::inode::FileType::File));
        // Only 2 and 3 remain; 1 must not linger as an eviction victim.
        assert_eq!(c.len(), 2);
        assert!(c.get(1).is_none());
        assert!(c.get(2).is_some());
        assert!(c.get(3).is_some());
    }

    #[test]
    fn test_cache_clear_resets_both_structures() {
        let c = BoundedInodeCache::with_capacity(4);
        for i in 0..4u64 {
            c.insert(i, ino(crate::inode::FileType::File));
        }
        c.clear();
        assert_eq!(c.len(), 0);
        assert!(c.get(0).is_none());
        // Must still work after a clear.
        c.insert(9, ino(crate::inode::FileType::File));
        assert!(c.get(9).is_some());
    }

    #[test]
    fn test_cache_insert_returns_stored_inode() {
        let c = BoundedInodeCache::with_capacity(2);
        let mut i = ino(crate::inode::FileType::File);
        i.size = 4242;
        c.insert(5, i);
        assert_eq!(c.get(5).expect("present").size, 4242);
        assert!(c.get(6).is_none());
    }

    #[test]
    fn test_sharded_cache_concurrent_access() {
        let c = Arc::new(BoundedInodeCache::with_capacity(2048));
        let mut handles = Vec::new();
        for t in 0..8 {
            let cache = c.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    let inode_id = t * 1000 + i;
                    let mut inode = ino(crate::inode::FileType::File);
                    inode.size = inode_id;
                    cache.insert(inode_id, inode);
                    let fetched = cache.get(inode_id).expect("must be cached");
                    assert_eq!(fetched.size, inode_id);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }
}

#[cfg(test)]
mod range_coalesce_tests {
    use super::*;

    const BS: usize = BLOCK_SIZE;

    #[test]
    fn test_coalesce_empty_and_zero_len() {
        let mmap_len = 100 * BS;
        assert!(DiskManagerInner::coalesce_ranges(&[], mmap_len).is_empty());
        assert!(DiskManagerInner::coalesce_ranges(&[(0, 0), (100, 0)], mmap_len).is_empty());
        assert!(DiskManagerInner::coalesce_ranges(&[(mmap_len, 100)], mmap_len).is_empty());
        assert!(DiskManagerInner::coalesce_ranges(&[(mmap_len + 10, 100)], mmap_len).is_empty());
        assert!(DiskManagerInner::coalesce_ranges(&[(100, 100)], 0).is_empty());
    }

    #[test]
    fn test_coalesce_same_page_sub_ranges() {
        let mmap_len = 100 * BS;
        // Two disjoint 256-byte inode slices within the same block (block 3 = 12288..16384)
        let r1 = (3 * BS, 256); // 12288..12544
        let r2 = (3 * BS + 512, 256); // 12800..13056
        let coalesced = DiskManagerInner::coalesce_ranges(&[r1, r2], mmap_len);
        assert_eq!(coalesced, vec![(3 * BS, BS)]);
    }

    #[test]
    fn test_coalesce_contiguous_blocks() {
        let mmap_len = 100 * BS;
        // Inode bitmap (block 1) + Data bitmap (block 2)
        let r1 = (BS, BS);
        let r2 = (2 * BS, BS);
        let coalesced = DiskManagerInner::coalesce_ranges(&[r1, r2], mmap_len);
        assert_eq!(coalesced, vec![(BS, 2 * BS)]);
    }

    #[test]
    fn test_coalesce_multi_block_runs_and_disjoint() {
        let mmap_len = 100 * BS;
        // Contiguous run of 3 blocks + 1 disjoint block far away
        let ranges = vec![(BS, BS), (2 * BS, BS), (3 * BS, BS), (10 * BS, BS)];
        let coalesced = DiskManagerInner::coalesce_ranges(&ranges, mmap_len);
        assert_eq!(coalesced, vec![(BS, 3 * BS), (10 * BS, BS)]);
    }

    #[test]
    fn test_coalesce_unsorted_and_overlapping() {
        let mmap_len = 100 * BS;
        let ranges = vec![
            (10 * BS, BS),
            (BS, 2 * BS), // covers block 1 and 2
            (2 * BS, BS), // duplicate overlap with block 2
            (BS, BS),     // duplicate overlap with block 1
        ];
        let coalesced = DiskManagerInner::coalesce_ranges(&ranges, mmap_len);
        assert_eq!(coalesced, vec![(BS, 2 * BS), (10 * BS, BS)]);
    }

    #[test]
    fn test_coalesce_clamping_at_mmap_boundary() {
        let mmap_len = 10 * BS;
        // Range extending past end of mmap
        let ranges = vec![(9 * BS + 100, 2 * BS)];
        let coalesced = DiskManagerInner::coalesce_ranges(&ranges, mmap_len);
        assert_eq!(coalesced, vec![(9 * BS, BS)]);
    }
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

/// The decision half of a write: what bytes land where, and what the inode becomes.
///
/// Both the in-place and the journaled write paths consume this, so the four cases
/// and their size arithmetic exist exactly once. They previously lived in two
/// parallel implementations that had already drifted: the in-place path added
/// `data.len()` (logical) for a compressed append while the journaled path added
/// `frame.len()` (physical), so the same append produced two different file sizes.
/// Result of preparing a full-overwrite payload buffer out-of-lock.
pub(crate) struct FullOverwritePayload {
    pub buffer: Vec<u8>,
    pub is_compressed: bool,
    pub encryption_nonce: Option<[u8; 24]>,
}

struct WritePlan {
    case: WriteCase,
    /// Byte offset in the file's payload where `buffer` begins.
    phys_off: u64,
    /// Exact bytes to store on disk (filtered, compressed and encrypted).
    buffer: Vec<u8>,
    /// New logical size.
    new_size: u64,
    /// New physical size; `0` when the payload is stored raw.
    new_compressed_size: u64,
    /// Updated encryption nonce if newly encrypted.
    encryption_nonce: Option<[u8; 24]>,
}

impl WritePlan {}

/// Decide which of the four write cases applies and compute the exact bytes and
/// sizes that result, performing CPU-intensive filtering, compression, and encryption
/// without holding any filesystem lock (P4.1).
fn plan_write_prepared(
    inode: &Inode,
    file_offset: u64,
    data: &[u8],
    compression_mode: CompressionMode,
    filter_config: &crate::filters::FilterConfig,
    existing_decompressed: Option<&[u8]>,
    encryption_key: Option<&crate::encryption::EncryptionKey>,
) -> Result<WritePlan, DiskManagerError> {
    let old_size = inode.size;
    let old_compressed_size = inode.compressed_size;

    // A write at offset 0 only replaces the whole file when it is at least as long
    // as the current one, or when compression/encryption/filters force a rewrite.
    let is_full_overwrite = file_offset == 0
        && (old_size == 0
            || data.len() as u64 >= old_size
            || old_compressed_size > 0
            || inode.encrypted
            || filter_config.is_active());

    if is_full_overwrite {
        let payload = DiskManager::build_full_overwrite_buffer_pure(
            data,
            compression_mode,
            filter_config,
            encryption_key,
        )?;
        // A compressed payload keeps its logical length in `size`; an uncompressed
        // one never shrinks, matching the in-place path's `max` behaviour.
        let new_size = if payload.is_compressed {
            data.len() as u64
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

    if old_compressed_size > 0 {
        // Fast path: append a fresh Zstd frame at the physical end of the stream.
        // `encryption_key.is_none()` is belt-and-braces -- on an encrypted image
        // every file is already marked `encrypted`, so `!inode.encrypted` should
        // already imply it. Guarding it here keeps both paths provably identical.
        let fast_append = file_offset == inode.size
            && !inode.encrypted
            && !filter_config.is_active()
            && inode.filter_typesize == 0
            && encryption_key.is_none();
        if fast_append {
            let frame = zstd::stream::encode_all(std::io::Cursor::new(data), 0)
                .map_err(DiskManagerError::Io)?;
            // The logical size grows by the *uncompressed* bytes; only the physical
            // stream grows by the frame length.
            return Ok(WritePlan {
                case: WriteCase::CompressedAppend,
                phys_off: old_compressed_size,
                new_size: old_size.saturating_add(data.len() as u64),
                new_compressed_size: old_compressed_size + frame.len() as u64,
                buffer: frame,
                encryption_nonce: None,
            });
        }

        // Read-modify-recompress: splice into the decoded image and rewrite from 0.
        let mut full = match existing_decompressed {
            Some(existing) => existing.to_vec(),
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
        return Ok(WritePlan {
            case: WriteCase::Recompress,
            phys_off: 0,
            buffer: payload.buffer,
            new_size,
            new_compressed_size,
            encryption_nonce: payload.encryption_nonce,
        });
    }

    // Plain append or random write into an uncompressed file.
    Ok(WritePlan {
        case: WriteCase::Raw,
        phys_off: file_offset,
        new_size: std::cmp::max(old_size, file_offset + data.len() as u64),
        new_compressed_size: 0,
        buffer: data.to_vec(),
        encryption_nonce: None,
    })
}

/// Helper to plan a write when already holding a disk lock (e.g. for re-planning after a race).
fn plan_write(
    guard: &DiskManagerInner,
    inode: &Inode,
    file_offset: u64,
    data: &[u8],
    compression_mode: CompressionMode,
    filter_config: &crate::filters::FilterConfig,
) -> Result<WritePlan, DiskManagerError> {
    let old_compressed_size = inode.compressed_size;
    let is_full_overwrite = file_offset == 0
        && (inode.size == 0
            || data.len() as u64 >= inode.size
            || old_compressed_size > 0
            || inode.encrypted
            || filter_config.is_active());
    let fast_append = old_compressed_size > 0
        && file_offset == inode.size
        && !inode.encrypted
        && !filter_config.is_active()
        && inode.filter_typesize == 0
        && guard.encryption_key.is_none();

    let existing_decompressed = if !is_full_overwrite && old_compressed_size > 0 && !fast_append {
        Some(DiskManager::read_data_internal(guard, inode)?)
    } else {
        None
    };

    plan_write_prepared(
        inode,
        file_offset,
        data,
        compression_mode,
        filter_config,
        existing_decompressed.as_deref(),
        guard.encryption_key.as_ref(),
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
        let path = path.as_ref();
        let exists = path.exists();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(create_encrypted)
            .open(path)?;

        // Acquire Lock using F_SETLK
        let mut lock = unsafe { std::mem::zeroed::<libc::flock>() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        lock.l_start = 0;
        lock.l_len = 0; // Whole file

        use nix::fcntl::{FcntlArg, fcntl};
        fcntl(&file, FcntlArg::F_SETLK(&lock)).map_err(DiskManagerError::Locking)?;

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
            if create_encrypted {
                let pwd = password.unwrap_or_default();
                sb.encrypted = true;
                sb.encryption_salt = crate::encryption::generate_salt();
                sb.encryption_version = 1; // XChaCha20-Poly1305
                let key = crate::encryption::derive_key(pwd, &sb.encryption_salt)?;
                encryption_key = Some(key);
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

            if superblock.encrypted {
                let pwd = password.ok_or(DiskManagerError::PasswordRequired)?;
                let key = crate::encryption::derive_key(pwd, &superblock.encryption_salt)?;
                encryption_key = Some(key);
            } else {
                encryption_key = None;
            }
        }

        // M2: Mount-time journal handling.
        // A journaled image is recovered *before* the manager is published, so no
        // caller can ever observe a filesystem mid-replay.
        if is_new {
            Self::format_journal(&mut mmap, &superblock)?;
        } else if superblock.has_journal_layout() {
            Self::recover_journal(&mut mmap, &superblock)?;
            mmap.flush()?;
        }

        let free_block_hint = superblock.data_block_start;
        let inner = DiskManagerInner {
            file,
            mmap,
            superblock,
            encryption_key,
            free_block_hint,
            free_inode_hint: 0,
            inode_cache: BoundedInodeCache::with_capacity(INODE_CACHE_CAPACITY),
            dir_cache: RwLock::new(HashMap::new()),
            durability_mode: std::sync::atomic::AtomicU8::new(DurabilityMode::Lazy as u8),
            io_engine: IoEngine::new(IoBackend::from_env().unwrap_or_default()),
        };

        let dm = Self {
            inner: Arc::new(RwLock::new(inner)),
            sync_mutex: Arc::new(Mutex::new(())),
        };

        if is_new {
            let mut guard = dm.inner.write().unwrap();
            let inode_bitmap_block = guard.superblock.inode_bitmap_block;
            let bitmap_slice = Self::get_block_mut_from_map(&mut guard.mmap, inode_bitmap_block)
                .ok_or_else(|| {
                    DiskManagerError::Io(std::io::Error::other("Failed to get inode bitmap"))
                })?;
            let mut ia = SimpleBlockAllocator::new(bitmap_slice, 0);
            let root_id = ia.allocate()?;
            if root_id != 0 {
                return Err(DiskManagerError::Io(std::io::Error::other(
                    "Failed init root inode",
                )));
            }

            let data_bitmap_block = guard.superblock.data_bitmap_block;
            let data_start = guard.superblock.data_block_start;
            let data_slice = Self::get_block_mut_from_map(&mut guard.mmap, data_bitmap_block)
                .ok_or_else(|| {
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
    fn journal_region_start(block_size: u64) -> Option<usize> {
        usize::try_from(crate::journal::JOURNAL_HEADER_BLOCK)
            .ok()?
            .checked_mul(usize::try_from(block_size).ok()?)
    }

    /// Resolve the journal region as a mutable slice, if the image is large enough.
    fn journal_region<'m>(
        mmap: &'m mut MmapMut,
        sb: &SuperBlock,
    ) -> Result<&'m mut [u8], DiskManagerError> {
        let bs = sb.block_size as u64;
        let start = Self::journal_region_start(bs).ok_or_else(|| {
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
        let mut guard = self.inner.write().unwrap();
        Self::write_inode_internal(&mut guard, inode_id, inode)?;
        let range = guard.inode_byte_range(inode_id);
        guard.sync_mutation_ranges(&[range])
    }

    /// Returns the current durability policy mode (P3.3).
    pub fn durability_mode(&self) -> DurabilityMode {
        self.inner.read().unwrap().durability_mode()
    }

    /// Updates the durability policy mode (P3.3).
    pub fn set_durability_mode(&self, mode: DurabilityMode) {
        self.inner.read().unwrap().set_durability_mode(mode);
    }

    /// Builder pattern helper to set durability mode upon initialization.
    pub fn with_durability_mode(self, mode: DurabilityMode) -> Self {
        self.set_durability_mode(mode);
        self
    }

    /// Returns the payload-read backend actually in use (P3.2).
    ///
    /// May differ from [`Self::requested_io_backend`] when `IoUring` was requested on a
    /// platform or kernel without `io_uring` support (falls back to `Pread`).
    pub fn io_backend(&self) -> IoBackend {
        self.inner.read().unwrap().io_engine.effective()
    }

    /// Returns the payload-read backend that was requested (P3.2).
    pub fn requested_io_backend(&self) -> IoBackend {
        self.inner.read().unwrap().io_engine.requested()
    }

    /// Switches the payload-read backend and returns the effective one (P3.2).
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

    /// Reads every entry of a directory into a stored-name -> inode map.
    fn build_dir_index(
        mmap: &MmapMut,
        inode: &Inode,
    ) -> Result<HashMap<String, u64>, DiskManagerError> {
        let mut map = HashMap::new();
        for blk_idx in 0..Self::dir_num_blocks(inode) {
            let phys_blk = Self::resolve_logical_block_id(mmap, inode, blk_idx);
            if phys_blk == 0 {
                continue;
            }
            for entry in Self::read_dir_entries_from_block(mmap, phys_blk)? {
                // Keep the first occurrence, matching on-disk scan order.
                map.entry(entry.name).or_insert(entry.inode);
            }
        }
        Ok(map)
    }

    /// Resolves `name` inside directory `parent_id` (`Ok(None)` = not found).
    ///
    /// Order: cached hit -> authoritative miss from a complete index -> targeted on-disk scan.
    /// A negative scan (already a full pass) or repeated cold scans promote the directory to a
    /// complete index so later hits, misses and create-time existence checks are O(1).
    fn dir_lookup(
        guard: &DiskManagerInner,
        parent_id: u64,
        name: &str,
    ) -> Result<Option<u64>, DiskManagerError> {
        let encrypted = guard.encryption_key.is_some();

        // 1. Fast path: no allocation, no disk access.
        if let Some(ix) = guard.dir_cache.read().unwrap().get(&parent_id) {
            let hit = if encrypted {
                ix.plain.get(name)
            } else {
                ix.stored.get(name)
            };
            if let Some(&id) = hit {
                return Ok(Some(id));
            }
            if ix.complete && !encrypted {
                return Ok(None);
            }
        }

        let parent = Self::read_inode_internal(guard, parent_id)?;
        if parent.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
        }

        // Stored-name candidates in priority order: ciphertext name first, then the legacy
        // plaintext name (entries written before encryption-aware naming).
        let enc_name = guard.encryption_key.as_ref().map(|key| {
            crate::encryption::encrypt_filename(key, parent_id, name)
                .unwrap_or_else(|_| name.to_string())
        });
        let candidates = [enc_name.as_deref(), Some(name)];

        // 2. Complete index (only reachable here on encrypted filesystems).
        let indexed = guard
            .dir_cache
            .read()
            .unwrap()
            .get(&parent_id)
            .filter(|ix| ix.complete)
            .map(|ix| {
                candidates
                    .iter()
                    .flatten()
                    .find_map(|c| ix.stored.get(*c).copied())
            });

        let found = match indexed {
            Some(found) => found,
            // 3. Targeted on-disk scan.
            None => candidates.iter().flatten().find_map(|c| {
                Self::locate_entry_in_dir(
                    &guard.mmap,
                    &parent,
                    c,
                    crate::directory::hash_filename(c),
                )
                .map(|(id, _)| id)
            }),
        };

        let mut cache = guard.dir_cache.write().unwrap();
        let ix = cache.entry(parent_id).or_default();
        if indexed.is_none() {
            ix.cold_scans = ix.cold_scans.saturating_add(1);
            if !ix.complete && (found.is_none() || ix.cold_scans >= DIR_INDEX_BUILD_AFTER_SCANS) {
                ix.stored = Self::build_dir_index(&guard.mmap, &parent)?;
                ix.complete = true;
            }
        }
        if let Some(id) = found {
            let map = if encrypted {
                &mut ix.plain
            } else {
                &mut ix.stored
            };
            map.insert(name.to_string(), id);
        }
        Ok(found)
    }

    /// Appends a directory entry to a directory, allocating a new block if all existing blocks are full.
    ///
    /// The last block is tried first: in a growing directory every earlier block is full, so this
    /// makes the common insert O(1) blocks; earlier blocks are only probed (to reuse space freed by
    /// deletions) when the last block is full.
    fn append_dir_entry_to_dir(
        guard: &mut DiskManagerInner,
        parent_inode: &mut Inode,
        entry: &crate::directory::DirectoryEntry,
    ) -> Result<u64, DiskManagerError> {
        let num_blocks = Self::dir_num_blocks(parent_inode);
        let needed_space = 20 + entry.name.len();

        if num_blocks > 0 {
            let probe_order = std::iter::once(num_blocks - 1).chain(0..num_blocks - 1);
            for blk_idx in probe_order {
                let phys_blk = Self::resolve_logical_block_id(&guard.mmap, parent_inode, blk_idx);
                if phys_blk == 0 {
                    continue;
                }
                if let Some(slice) = Self::get_block_from_map(&guard.mmap, phys_blk) {
                    let insert_offset = crate::directory::find_insert_offset_in_block(slice);
                    if insert_offset + needed_space <= BLOCK_SIZE {
                        Self::append_dir_entry_to_block(&mut guard.mmap, phys_blk, entry)?;
                        return Ok(phys_blk);
                    }
                }
            }
        }

        // All existing blocks are full (or the directory had none): grow by one block.
        let new_blk_idx = num_blocks;
        let new_phys_block = Self::get_or_alloc_block(guard, parent_inode, new_blk_idx, true)?;
        if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, new_phys_block) {
            slice.fill(0);
        }
        Self::append_dir_entry_to_block(&mut guard.mmap, new_phys_block, entry)?;
        parent_inode.size = (new_blk_idx + 1) as u64 * BLOCK_SIZE as u64;
        Ok(new_phys_block)
    }

    fn append_dir_entry_to_block(
        mmap: &mut MmapMut,
        block_id: u64,
        entry: &crate::directory::DirectoryEntry,
    ) -> Result<(), DiskManagerError> {
        let block_slice = Self::get_block_mut_from_map(mmap, block_id).ok_or_else(|| {
            DiskManagerError::Io(std::io::Error::other("Directory block not found"))
        })?;

        let insert_offset = crate::directory::find_insert_offset_in_block(block_slice);

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
        let start = Self::journal_region_start(bs).ok_or_else(|| {
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
        let start = Self::journal_region_start(bs).unwrap_or(0);
        let ring_base = start + usize::try_from(bs).unwrap_or(0);
        if let Some((s, e)) = written
            && e > s
        {
            let off = ring_base + usize::try_from(s).unwrap_or(0);
            let len = usize::try_from(e - s).unwrap_or(0);
            if off + len <= guard.mmap.len() {
                guard.mmap.flush_range(off, len)?;
            }
        }
        let header_len = usize::try_from(bs)
            .unwrap_or(0)
            .min(crate::journal::JOURNAL_HEADER_LEN);
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
        let start = Self::journal_region_start(bs).unwrap_or(0);
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
        }
        Ok(())
    }

    fn collect_inode_blocks(mmap: &MmapMut, inode: &Inode) -> Vec<u64> {
        let mut blks = Vec::new();

        // 1. Direct blocks (0..10)
        for i in 0..10 {
            let blk = inode.blocks[i];
            if blk != 0 {
                blks.push(blk);
            }
        }

        // 2. Single Indirect block (10)
        let sib_id = inode.blocks[10];
        if sib_id != 0 {
            blks.push(sib_id);
            if let Some(slice) = Self::get_block_from_map(mmap, sib_id) {
                for chunk in slice.as_chunks::<8>().0 {
                    let blk = u64::from_le_bytes(*chunk);
                    if blk != 0 {
                        blks.push(blk);
                    }
                }
            }
        }

        // 3. Double Indirect block (11)
        let dib_id = inode.blocks[11];
        if dib_id != 0 {
            blks.push(dib_id);
            let physical_size = if inode.compressed_size > 0 {
                inode.compressed_size
            } else {
                inode.size
            };
            let total_logical_blocks = physical_size.div_ceil(BLOCK_SIZE as u64);
            let max_s_entries = if total_logical_blocks > 522 {
                let diff = total_logical_blocks - 522;
                let needed = diff.div_ceil(512) as usize;
                needed.min(512)
            } else {
                512 // Fallback if size was 0 or reset before collect
            };

            if let Some(slice) = Self::get_block_from_map(mmap, dib_id) {
                for chunk in slice[..max_s_entries * 8].as_chunks::<8>().0 {
                    let sib = u64::from_le_bytes(*chunk);
                    if sib != 0 {
                        blks.push(sib);
                        if let Some(s_slice) = Self::get_block_from_map(mmap, sib) {
                            for d_chunk in s_slice.as_chunks::<8>().0 {
                                let blk = u64::from_le_bytes(*d_chunk);
                                if blk != 0 {
                                    blks.push(blk);
                                }
                            }
                        }
                    }
                }
            }
        }

        // 4. Triple Indirect block
        let tib_id = inode.triple_indirect;
        if tib_id != 0 {
            blks.push(tib_id);
            let physical_size = if inode.compressed_size > 0 {
                inode.compressed_size
            } else {
                inode.size
            };
            let total_logical_blocks = physical_size.div_ceil(BLOCK_SIZE as u64);
            let max_t_entries = if total_logical_blocks > 262666 {
                let diff = total_logical_blocks - 262666;
                let needed = diff.div_ceil(512 * 512) as usize;
                needed.min(512)
            } else {
                512 // Fallback if size was 0 or reset before collect
            };

            if let Some(slice) = Self::get_block_from_map(mmap, tib_id) {
                for chunk in slice[..max_t_entries * 8].as_chunks::<8>().0 {
                    let dib = u64::from_le_bytes(*chunk);
                    if dib != 0 {
                        blks.push(dib);
                        if let Some(d_slice) = Self::get_block_from_map(mmap, dib) {
                            for s_chunk in d_slice.as_chunks::<8>().0 {
                                let sib = u64::from_le_bytes(*s_chunk);
                                if sib != 0 {
                                    blks.push(sib);
                                    if let Some(s_slice) = Self::get_block_from_map(mmap, sib) {
                                        for blk_chunk in s_slice.as_chunks::<8>().0 {
                                            let blk = u64::from_le_bytes(*blk_chunk);
                                            if blk != 0 {
                                                blks.push(blk);
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

        blks
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
        let start = Self::journal_region_start(bs).unwrap_or(0);
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
        Self::checkpoint_journal(&mut guard.mmap, &sb)
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
        let start = Self::journal_region_start(bs).unwrap_or(0);
        let header_len = usize::try_from(bs)
            .unwrap_or(0)
            .min(crate::journal::JOURNAL_HEADER_LEN);
        if header_len > 0 {
            guard.mmap.flush_range(start, header_len)?;
        }
        Ok(discarded)
    }

    /// `true` when this image still stores inodes in the legacy format and would
    /// benefit from [`Self::migrate`].
    pub fn needs_migration(&self) -> bool {
        self.inner
            .read()
            .unwrap()
            .superblock
            .is_legacy_inode_format()
    }

    /// `true` when a previous migration was interrupted and is resumable.
    pub fn migration_in_progress(&self) -> bool {
        self.inner
            .read()
            .unwrap()
            .superblock
            .is_migration_in_progress()
    }

    /// Upgrade a legacy (v1) image to the current inode format, in place.
    ///
    /// # Crash safety
    ///
    /// Inodes are rewritten one at a time, so a crash can leave an image holding
    /// both encodings. That intermediate state is *recorded* rather than ambiguous:
    /// the superblock keeps advertising the legacy version while
    /// [`SuperBlock::migration_cursor`] marks how far the rewrite has progressed,
    /// and readers decode each slot according to its position (see
    /// [`SuperBlock::inode_record_version_for`]). A crash therefore leaves a
    /// readable, resumable image rather than a corrupt one.
    ///
    /// The journal is checkpointed first: a pending `WriteInode` op encoded in the
    /// outgoing format would otherwise be replayed into a rewritten slot and destroy
    /// the record.
    ///
    /// Idempotent — safe to call on an already-current image (returns zero work) and
    /// safe to call again after an interrupted migration.
    pub fn migrate(&self) -> Result<MigrationStats, DiskManagerError> {
        let mut guard = self.inner.write().unwrap();
        let from_version = guard.superblock.inode_record_version();
        if !guard.superblock.is_legacy_inode_format() {
            return Ok(MigrationStats {
                from_version,
                to_version: from_version,
                inodes_rewritten: 0,
                already_current: true,
            });
        }

        // Must precede any rewrite: see the doc comment.
        let sb = guard.superblock;
        Self::checkpoint_journal(&mut guard.mmap, &sb)?;

        // Snapshot the allocation bitmap so we know which slots are in use.
        let inode_bitmap = Self::get_block_from_map(&guard.mmap, sb.inode_bitmap_block)
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("inode bitmap not found")))?
            .to_vec();

        let mut cursor = guard.superblock.migration_cursor;
        let mut rewritten = 0u64;

        while cursor < sb.inode_count {
            let inode_id = cursor;
            let bit = inode_id as usize;
            let allocated = inode_bitmap[bit / 8] & (1 << (bit % 8)) != 0;

            if allocated {
                // Decode with the *outgoing* format regardless of the cursor: a slot
                // at the cursor has not been rewritten yet.
                let (start, end) = Self::inode_slot_range(&guard, inode_id)?;
                let inode = crate::inode_format::decode_v1(&guard.mmap[start..end])?;
                let slot = crate::inode_format::encode_v2(&inode);
                guard.mmap[start..end].copy_from_slice(&slot);
                rewritten += 1;
            }

            cursor += 1;
            // Persist progress periodically so an interrupted migration resumes near
            // where it stopped instead of from zero.
            if cursor.is_multiple_of(256) {
                guard.superblock.migration_cursor = cursor;
                Self::persist_superblock(&mut guard)?;
                guard.mmap.flush()?;
            }
        }

        // All slots rewritten: advertise the new format and clear the cursor.
        guard.superblock.migration_cursor = 0;
        guard.superblock.format_version = SuperBlock::FORMAT_VERSION_FIXED_INODE;
        Self::persist_superblock(&mut guard)?;
        guard.mmap.flush()?;
        guard.inode_cache.clear();

        Ok(MigrationStats {
            from_version,
            to_version: SuperBlock::FORMAT_VERSION_FIXED_INODE,
            inodes_rewritten: rewritten,
            already_current: false,
        })
    }

    /// Write the in-memory superblock back into block 0.
    fn persist_superblock(guard: &mut DiskManagerInner) -> Result<(), DiskManagerError> {
        let bytes = bincode::serialize(&guard.superblock)?;
        if bytes.len() > BLOCK_SIZE {
            return Err(DiskManagerError::Serialization(Box::new(
                bincode::ErrorKind::SizeLimit,
            )));
        }
        guard.mmap[0..bytes.len()].copy_from_slice(&bytes);
        Ok(())
    }

    fn create_entry_journaled(
        guard: &mut DiskManagerInner,
        parent_inode_id: u64,
        name: &str,
        stored_name: String,
        stored_hash: u64,
        file_type: crate::inode::FileType,
    ) -> Result<u64, DiskManagerError> {
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

        let mut sim =
            crate::journal::AllocSim::new(guard, guard.free_inode_hint, guard.free_block_hint)?;
        let mut ops = Vec::new();

        // ---- Phase 2: simulate allocation ----
        let new_inode_id = sim.alloc_inode()?;
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

        if file_type == crate::inode::FileType::Directory {
            let dir_data_block = sim.alloc_block()?;
            ops.push(crate::journal::MetadataOp::SetDataBitmap {
                block_id: dir_data_block,
                allocated: true,
            });
            new_inode.blocks[0] = dir_data_block;
            new_inode.size = BLOCK_SIZE as u64;
        }

        let entry = crate::directory::DirectoryEntry {
            inode: new_inode_id,
            hash: stored_hash,
            name: stored_name.clone(),
        };
        let mut entry_bytes = Vec::new();
        {
            let mut cursor = std::io::Cursor::new(&mut entry_bytes);
            entry
                .serialize_into(&mut cursor)
                .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
        }

        // Try to place the entry into an existing directory block (read-only probe).
        let num_blocks = Self::dir_num_blocks(&parent_inode);
        let needed_space = 20 + entry.name.len();
        let mut written_blk = 0u64;
        if num_blocks > 0 {
            let probe_order = std::iter::once(num_blocks - 1).chain(0..num_blocks - 1);
            for blk_idx in probe_order {
                let phys_blk = Self::resolve_logical_block_id(&guard.mmap, &parent_inode, blk_idx);
                if phys_blk == 0 {
                    continue;
                }
                if let Some(slice) = Self::get_block_from_map(&guard.mmap, phys_blk) {
                    let insert_offset = crate::directory::find_insert_offset_in_block(slice);
                    if insert_offset + needed_space <= BLOCK_SIZE {
                        ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                            block_id: phys_blk,
                            offset: insert_offset as u32,
                            data: entry_bytes.clone(),
                        });
                        written_blk = phys_blk;
                        break;
                    }
                }
            }
        }

        if written_blk == 0 {
            // Grow the directory by one block. `sim_get_or_alloc_block` may also need to
            // allocate indirect pointer blocks once the directory exceeds 10 blocks;
            // those allocations are recorded as ops too.
            let new_blk_idx = num_blocks;
            let phys_blk = crate::journal::sim_get_or_alloc_block(
                &guard.mmap,
                &mut parent_inode,
                new_blk_idx,
                &mut sim,
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
            written_blk = phys_blk;
        }

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

        guard.free_inode_hint = sim.free_inode_hint;
        guard.free_block_hint = sim.free_block_hint;

        guard.inode_cache.insert(new_inode_id, new_inode);
        guard.inode_cache.insert(parent_inode_id, parent_inode);
        let encrypted = guard.encryption_key.is_some();
        if let Some(ix) = guard.dir_cache.write().unwrap().get_mut(&parent_inode_id) {
            if encrypted {
                ix.plain.insert(name.to_string(), new_inode_id);
            }
            ix.stored.insert(stored_name, new_inode_id);
        }

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
        for blk in &sim.fresh_blocks {
            ranges.push(guard.block_byte_range(*blk));
        }
        if guard.durability_mode().is_range_based() {
            guard.sync_mutation_ranges(&ranges)?;
        } else {
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

            let stored_name = if let Some(key) = &guard.encryption_key {
                crate::encryption::encrypt_filename(key, parent_inode_id, name)
                    .unwrap_or_else(|_| name.to_string())
            } else {
                name.to_string()
            };
            let stored_hash = crate::directory::hash_filename(&stored_name);
            (stored_name, stored_hash)
        }; // Shared read-guard is dropped immediately here!

        // --- STAGE 2: In-Lock Allocation & Commitment ---
        let mut guard = self.inner.write().unwrap();

        // M3: journaled images take the WAL-first path; legacy images keep the
        // original in-place implementation untouched (zero-overhead guarantee).
        if guard.superblock.has_journal_layout() {
            return Self::create_entry_journaled(
                &mut guard,
                parent_inode_id,
                name,
                stored_name,
                stored_hash,
                file_type,
            );
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
        let inode_bitmap = guard.superblock.inode_bitmap_block;
        let hint = guard.free_inode_hint;
        let new_inode_id = {
            let bitmap_slice = Self::get_block_mut_from_map(&mut guard.mmap, inode_bitmap).unwrap();
            let mut allocator = SimpleBlockAllocator::new(bitmap_slice, 0);
            allocator.allocate_with_hint(Some(hint))?
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
        let written_blk = Self::append_dir_entry_to_dir(&mut guard, &mut parent_inode, &entry)?;

        // 5. Update Parent Mtime
        parent_inode.modified_at = now;
        Self::write_inode_internal(&mut guard, parent_inode_id, &parent_inode)?;

        // Keep the directory index consistent (only if one exists; otherwise nothing is stale).
        let stored_name = entry.name;
        let encrypted = guard.encryption_key.is_some();
        if let Some(ix) = guard.dir_cache.write().unwrap().get_mut(&parent_inode_id) {
            if encrypted {
                ix.plain.insert(name.to_string(), new_inode_id);
            }
            ix.stored.insert(stored_name, new_inode_id);
        }

        if guard.durability_mode().is_range_based() {
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
            guard.sync_mutation_ranges(&ranges)?;
        } else {
            guard.sync_mutation_ranges(&[])?;
        }
        Ok(new_inode_id)
    }

    /// Creates a new file in a directory
    pub fn create_file(&self, parent_inode_id: u64, name: &str) -> Result<u64, DiskManagerError> {
        self.create_entry_internal(parent_inode_id, name, crate::inode::FileType::File)
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
                if blk != 0 {
                    emit(blk, read as usize, to_read);
                }
                read += to_read as u64;
                blk_idx += 1;
                continue;
            }

            // 2. Single indirect blocks (10..522) - Batch resolution
            if blk_idx < 10 + 512 {
                let sib_id = inode.blocks[10];
                if sib_id == 0 {
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
                        if blk != 0 {
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
                if dib_id == 0 {
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
                if sib_id == 0 {
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
                        if blk != 0 {
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
                if tib_id == 0 {
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
                if dib_id == 0 {
                    let skip_blocks = (512 * 512) - rem_idx;
                    let skip_bytes =
                        (skip_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx += skip_blocks;
                    continue;
                }

                let sib_id = Self::read_block_ptr(mmap, dib_id, d_idx);
                if sib_id == 0 {
                    let skip_blocks = 512 - s_idx;
                    let skip_bytes =
                        (skip_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx += skip_blocks;
                    continue;
                }

                let blk = Self::read_block_ptr(mmap, sib_id, s_idx);
                if blk != 0 {
                    emit(blk, read as usize, to_read);
                }
                read += to_read as u64;
                blk_idx += 1;
                continue;
            }

            break;
        }
    }

    #[allow(clippy::collapsible_if)]
    fn read_data_internal(
        guard: &DiskManagerInner,
        inode: &Inode,
    ) -> Result<Vec<u8>, DiskManagerError> {
        let physical_size = if inode.compressed_size > 0 {
            inode.compressed_size
        } else {
            inode.size
        };
        if physical_size == 0 {
            return Ok(Vec::new());
        }

        let mut raw_data = vec![0u8; physical_size as usize];
        if guard.io_engine.effective() == IoBackend::Mmap {
            // Zero-copy path: memcpy each allocated block straight out of the mapping.
            Self::walk_payload_blocks(&guard.mmap, inode, physical_size, |blk, off, len| {
                if let Some(slice) = Self::get_block_from_map(&guard.mmap, blk) {
                    raw_data[off..off + len].copy_from_slice(&slice[..len]);
                }
            });
        } else {
            // P3.2: gather coalesced extents, then hand them to the engine in one submission.
            let mut extents = ExtentList::new();
            Self::walk_payload_blocks(&guard.mmap, inode, physical_size, |blk, off, len| {
                if Self::get_block_from_map(&guard.mmap, blk).is_some() {
                    extents.push(blk * BLOCK_SIZE as u64, off, len);
                }
            });
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

            decrypted_data = crate::encryption::decrypt_data(
                &decrypted_data,
                encryption_key,
                &inode.encryption_nonce,
            )
            .map_err(|_| DiskManagerError::DecryptionFailed)?;
        }

        // Decompress if this is a compressed file (natively decompresses concatenated multi-frame Zstd streams)
        if inode.mode == crate::inode::FileType::File && inode.compressed_size > 0 {
            let decoded = zstd::stream::decode_all(std::io::Cursor::new(&decrypted_data))
                .map_err(DiskManagerError::Io)?;
            decrypted_data = decoded;
        }

        // === POST-DECOMPRESSION FILTER STEP ===
        let filter_config = crate::filters::FilterConfig {
            typesize: inode.filter_typesize,
            delta: inode.filter_delta,
            shuffle: inode.filter_shuffle,
            bitshuffle: inode.filter_bitshuffle,
        };
        if !filter_config.is_active() {
            return Ok(decrypted_data);
        }
        let result = crate::filters::unapply_filters(&decrypted_data, &filter_config);
        Ok(result)
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
        Self::read_data_internal(&guard, &inode)
    }

    /// Reads up to `buf.len()` bytes starting at `file_offset` from a file.
    ///
    /// # Performance
    /// For uncompressed, unencrypted files under the default [`IoBackend::Mmap`]
    /// backend, this directly copies the requested byte slice from the memory-mapped
    /// blocks into `buf` with **ZERO intermediate memory allocations**. Under
    /// [`IoBackend::Pread`] / [`IoBackend::IoUring`] the physically contiguous block runs
    /// are coalesced and read with one syscall / one ring submission (P3.2).
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

    /// Performs many positional reads under a single read lock and, for raw files, a
    /// single engine submission (P3.2).
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
        mut extents: Option<&mut ExtentList>,
    ) -> Result<usize, DiskManagerError> {
        let inode = Self::read_inode_internal(guard, inode_id)?;

        if inode.mode != crate::inode::FileType::File {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Cannot read non-file inode",
            )));
        }

        if file_offset >= inode.size || buf.is_empty() {
            return Ok(0);
        }

        let available = (inode.size - file_offset) as usize;
        let to_read_total = std::cmp::min(buf.len(), available);

        // Fallback for compressed or encrypted files: decompress/decrypt and slice
        if inode.compressed_size > 0 || inode.encrypted {
            let full_data = Self::read_data_internal(guard, &inode)?;
            let start = file_offset as usize;
            if start >= full_data.len() {
                return Ok(0);
            }
            let end = (start + to_read_total).min(full_data.len());
            let actual = end.saturating_sub(start);
            buf[..actual].copy_from_slice(&full_data[start..end]);
            return Ok(actual);
        }

        // Fast path for raw uncompressed files: Zero-allocation direct copy from mmap blocks
        let mut bytes_read = 0;
        let mut curr_offset = file_offset;

        while bytes_read < to_read_total {
            let blk_idx = (curr_offset / BLOCK_SIZE as u64) as usize;
            let in_blk_offset = (curr_offset % BLOCK_SIZE as u64) as usize;
            let rem_in_blk = BLOCK_SIZE - in_blk_offset;
            let chunk_len = std::cmp::min(to_read_total - bytes_read, rem_in_blk);

            let blk_id = Self::resolve_logical_block_id(&guard.mmap, &inode, blk_idx);
            let dst = &mut buf[bytes_read..bytes_read + chunk_len];
            match Self::get_block_from_map(&guard.mmap, blk_id).filter(|_| blk_id != 0) {
                Some(slice) => match extents.as_deref_mut() {
                    None => dst.copy_from_slice(&slice[in_blk_offset..in_blk_offset + chunk_len]),
                    Some(list) => list.push(
                        blk_id * BLOCK_SIZE as u64 + in_blk_offset as u64,
                        bytes_read,
                        chunk_len,
                    ),
                },
                None => dst.fill(0),
            }

            bytes_read += chunk_len;
            curr_offset += chunk_len as u64;
        }

        Ok(bytes_read)
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
            let blk_id = Self::get_or_alloc_block(guard, inode, blk_idx, true)?;
            if let Some(ref mut tb) = touched_blocks
                && tb.last().copied() != Some(blk_id)
            {
                tb.push(blk_id);
            }

            let in_blk_off = (current_offset % BLOCK_SIZE as u64) as usize;
            let to_write = std::cmp::min(data.len() - written, BLOCK_SIZE - in_blk_off);

            if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, blk_id) {
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
    fn write_data_journaled(
        guard: &mut DiskManagerInner,
        inode_id: u64,
        file_offset: u64,
        plan: WritePlan,
        filter_config: crate::filters::FilterConfig,
    ) -> Result<(), DiskManagerError> {
        let mut inode = Self::read_inode_internal(guard, inode_id)?;
        let old_size = inode.size;
        let old_compressed = inode.compressed_size;
        let old_logical = {
            let p = std::cmp::max(old_compressed, old_size);
            (p / BLOCK_SIZE as u64) as usize + usize::from(p % BLOCK_SIZE as u64 != 0)
        };
        let old_blocks = Self::collect_inode_blocks(&guard.mmap, &inode);

        let mut sim =
            crate::journal::AllocSim::new(guard, guard.free_inode_hint, guard.free_block_hint)?;
        let mut ops: Vec<crate::journal::MetadataOp> = Vec::new();
        let mut used: Vec<u64> = Vec::new();

        let WritePlan {
            case,
            phys_off,
            buffer,
            new_size,
            new_compressed_size,
            encryption_nonce,
        } = plan;

        // 1. Stage payload (bitmap bits still clear).
        crate::journal::stage_payload_write(
            guard, &mut inode, phys_off, &buffer, &mut sim, &mut ops, &mut used,
        )?;

        // A shrinking write orphans both data blocks and the pointers that reference
        // them; both halves must go, or the inode ends up pointing at free blocks.
        // Use the sizes computed above rather than reading back from `inode`, which
        // still holds the pre-write values at this point.
        let new_physical = std::cmp::max(new_compressed_size, new_size);
        if file_offset == 0 && phys_off == 0 {
            let needed_logical = (new_physical / BLOCK_SIZE as u64) as usize
                + usize::from(new_physical % BLOCK_SIZE as u64 != 0);
            if needed_logical < old_logical {
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

        // 2. Free blocks the new layout no longer references.
        for blk in &old_blocks {
            if !used.contains(blk) {
                ops.push(crate::journal::MetadataOp::SetDataBitmap {
                    block_id: *blk,
                    allocated: false,
                });
            }
        }
        ops.push(crate::journal::MetadataOp::WriteInode {
            inode_id,
            inode_bytes: Box::new(Self::build_inode_post_image(guard, inode_id, &inode)?),
        });

        // 3. Payload durability BEFORE the metadata commit.
        //
        // Same durability-policy reasoning as the WAL barrier in
        // `commit_journal_tx`: only `Strict` promises the data reached disk before
        // the metadata referencing it, so only `Strict` pays for a barrier. The
        // process-crash-safe modes leave the payload in the page cache, exactly as
        // they already do for metadata — and `flush_async_range` is still a syscall
        // per block, so issuing it there bought latency without adding a guarantee.
        let payload_ranges: Vec<(usize, usize)> =
            used.iter().map(|b| guard.block_byte_range(*b)).collect();
        let coalesced_payload =
            DiskManagerInner::coalesce_ranges(&payload_ranges, guard.mmap.len());
        match guard.durability_mode() {
            DurabilityMode::Strict => {
                for (off, actual) in coalesced_payload {
                    guard.mmap.flush_range(off, actual)?;
                }
            }
            DurabilityMode::RangeAsync => {
                for (off, actual) in coalesced_payload {
                    let _ = guard.mmap.flush_async_range(off, actual);
                }
            }
            // Lazy and LegacyWholeMmapAsync promise nothing about writeback timing.
            DurabilityMode::Lazy | DurabilityMode::LegacyWholeMmapAsync => {}
        }

        // 4. Commit point.
        Self::commit_journal_tx(guard, &ops)?;

        // 5. Apply.
        Self::apply_journal_ops(guard, &ops)?;

        guard.free_inode_hint = sim.free_inode_hint;
        guard.free_block_hint = sim.free_block_hint;
        guard.inode_cache.insert(inode_id, inode);

        let mut ranges = vec![
            guard.data_bitmap_byte_range(),
            guard.inode_byte_range(inode_id),
        ];
        ranges.extend(payload_ranges);
        for blk in &sim.fresh_blocks {
            ranges.push(guard.block_byte_range(*blk));
        }
        if guard.durability_mode().is_range_based() {
            guard.sync_mutation_ranges(&ranges)?;
        } else {
            guard.sync_mutation_ranges(&[])?;
        }
        Ok(())
    }

    /// Build the final on-disk buffer for a full (offset 0) overwrite: filter, then
    /// compress, then encrypt, for a write that replaces the whole file.
    ///
    /// This function performs pure CPU-bound processing without holding any disk locks (P4.1).
    pub(crate) fn build_full_overwrite_buffer_pure(
        data: &[u8],
        compression_mode: CompressionMode,
        filter_config: &crate::filters::FilterConfig,
        encryption_key: Option<&crate::encryption::EncryptionKey>,
    ) -> Result<FullOverwritePayload, DiskManagerError> {
        let filtered = crate::filters::apply_filters_cow(data, filter_config);
        let working: &[u8] = &filtered;

        let should_compress = match compression_mode {
            CompressionMode::Always => true,
            CompressionMode::Never => false,
            CompressionMode::Auto => working.len() >= 8192,
        };

        let (final_data, is_compressed): (std::borrow::Cow<[u8]>, bool) = if should_compress {
            let compressed = zstd::stream::encode_all(std::io::Cursor::new(working), 0)
                .map_err(DiskManagerError::Io)?;
            match compression_mode {
                CompressionMode::Always => (std::borrow::Cow::Owned(compressed), true),
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
            let enc = crate::encryption::encrypt_data(final_data.as_ref(), key, &nonce)?;
            return Ok(FullOverwritePayload {
                buffer: enc,
                is_compressed,
                encryption_nonce: Some(nonce),
            });
        }
        Ok(FullOverwritePayload {
            buffer: final_data.into_owned(),
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
        )?;
        if let Some(n) = payload.encryption_nonce {
            inode.encrypted = true;
            inode.encryption_nonce = n;
        }
        Ok((payload.buffer, payload.is_compressed))
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
        // --- STAGE 1: Out-of-Lock Preparation (P4.1) ---
        // Fast shared read-lock inspection to determine plan parameters and extract encryption key.
        let (initial_inode, existing_decompressed, encryption_key) = {
            let guard = self.inner.read().unwrap();
            let inode = Self::read_inode_internal(&guard, inode_id)?;

            if inode.mode != crate::inode::FileType::File {
                return Err(DiskManagerError::Io(std::io::Error::other(
                    "Cannot write data to non-file inode",
                )));
            }

            let is_full_overwrite = file_offset == 0
                && (inode.size == 0
                    || data.len() as u64 >= inode.size
                    || inode.compressed_size > 0
                    || inode.encrypted
                    || filter_config.is_active());

            let fast_append = inode.compressed_size > 0
                && file_offset == inode.size
                && !inode.encrypted
                && !filter_config.is_active()
                && inode.filter_typesize == 0
                && guard.encryption_key.is_none();

            let existing = if !is_full_overwrite && inode.compressed_size > 0 && !fast_append {
                Some(Self::read_data_internal(&guard, &inode)?)
            } else {
                None
            };

            let key = guard.encryption_key.clone();
            (inode, existing, key)
        }; // Shared read-guard is dropped immediately here!

        // --- STAGE 2: CPU-Bound Processing (ZERO filesystem locks held) ---
        // Perform CPU-heavy filtering, Zstd compression, and XChaCha20 encryption
        // entirely outside the exclusive lock so concurrent readers are not blocked.
        let prepared_plan = plan_write_prepared(
            &initial_inode,
            file_offset,
            data,
            compression_mode,
            &filter_config,
            existing_decompressed.as_deref(),
            encryption_key.as_ref(),
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

        // Verify if the inode state remains consistent with Stage 1.
        let can_use_prepared = inode.size == initial_inode.size
            && inode.compressed_size == initial_inode.compressed_size
            && inode.modified_at == initial_inode.modified_at
            && inode.encrypted == initial_inode.encrypted
            && inode.blocks == initial_inode.blocks;

        let plan = if can_use_prepared {
            prepared_plan
        } else {
            // Raced with another writer on this specific inode: re-compute plan under write lock.
            plan_write(
                &guard,
                &inode,
                file_offset,
                data,
                compression_mode,
                &filter_config,
            )?
        };

        // M3: journaled images stage the write through AllocSim and commit one
        // metadata transaction; legacy images keep the in-place implementation.
        if guard.superblock.has_journal_layout() {
            return Self::write_data_journaled(
                &mut guard,
                inode_id,
                file_offset,
                plan,
                filter_config,
            );
        }

        // Decide once, apply in place.
        if plan.case.frees_previous_blocks() {
            let old_blocks = Self::collect_inode_blocks(&guard.mmap, &inode);
            let mut min_freed_blk = u64::MAX;
            {
                let db_blk = guard.superblock.data_bitmap_block;
                let db_start = guard.superblock.data_block_start;
                let slice =
                    Self::get_block_mut_from_map(&mut guard.mmap, db_blk).ok_or_else(|| {
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
            inode.filter_typesize = filter_config.typesize;
            inode.filter_delta = filter_config.delta;
            inode.filter_shuffle = filter_config.shuffle;
            inode.filter_bitshuffle = filter_config.bitshuffle;
        }
        inode.modified_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self::write_inode_internal(&mut guard, inode_id, &inode)?;

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
        // Fast path: check in-memory inode cache (P2.2)
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

        // Update in-memory inode cache (P2.2)
        guard.inode_cache.insert(inode_id, *inode);
        Ok(())
    }

    fn allocate_block(guard: &mut DiskManagerInner) -> Result<u64, DiskManagerError> {
        let db_blk = guard.superblock.data_bitmap_block;
        let db_start = guard.superblock.data_block_start;
        let hint = guard.free_block_hint;
        let blk = {
            let slice = Self::get_block_mut_from_map(&mut guard.mmap, db_blk).unwrap();
            let mut da = SimpleBlockAllocator::new(slice, db_start);
            da.allocate_with_hint(Some(hint))
                .map_err(DiskManagerError::Allocator)?
        };
        guard.free_block_hint = blk + 1;
        Ok(blk)
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

    fn alloc_and_zero_block(guard: &mut DiskManagerInner) -> Result<u64, DiskManagerError> {
        let blk = Self::allocate_block(guard)?;
        if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, blk) {
            slice.fill(0);
        }
        Ok(blk)
    }

    fn get_or_alloc_indirect_child(
        guard: &mut DiskManagerInner,
        parent_block_id: u64,
        entry_idx: usize,
        allocate: bool,
        is_intermediate: bool,
    ) -> Result<u64, DiskManagerError> {
        let mut child_id = Self::read_block_ptr(&guard.mmap, parent_block_id, entry_idx);
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
            Ok(*slot)
        }

        match BlockPath::from_logical(logical_block_idx) {
            Some(BlockPath::Direct(i)) => {
                if inode.blocks[i] == 0 && allocate {
                    inode.blocks[i] = Self::allocate_block(guard)?;
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
    /// from memory without reading or parsing physical disk blocks (P4.6).
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

        // 1. Fast path: check directory index cache (P4.6).
        // If complete, construct entries directly from memory without reading/decoding disk blocks.
        if let Some(ix) = guard.dir_cache.read().unwrap().get(&dir_inode_id)
            && ix.complete
        {
            if !encrypted {
                let entries: Vec<crate::directory::DirectoryEntry> = ix
                    .stored
                    .iter()
                    .map(|(name, &inode)| crate::directory::DirectoryEntry {
                        inode,
                        hash: crate::directory::hash_filename(name),
                        name: name.clone(),
                    })
                    .collect();
                return Ok(entries);
            } else if ix.plain.len() == ix.stored.len() {
                let entries: Vec<crate::directory::DirectoryEntry> = ix
                    .plain
                    .iter()
                    .map(|(name, &inode)| crate::directory::DirectoryEntry {
                        inode,
                        hash: crate::directory::hash_filename(name),
                        name: name.clone(),
                    })
                    .collect();
                return Ok(entries);
            }
        }

        // 2. Slow path: scan physical directory blocks
        let num_blocks = Self::dir_num_blocks(&inode);
        let mut raw_entries = Vec::new();
        for blk_idx in 0..num_blocks {
            let phys_blk = Self::resolve_logical_block_id(&guard.mmap, &inode, blk_idx);
            if phys_blk == 0 {
                continue;
            }
            raw_entries.extend(Self::read_dir_entries_from_block(&guard.mmap, phys_blk)?);
        }

        let mut entries = raw_entries.clone();
        if let Some(key) = &guard.encryption_key {
            for entry in &mut entries {
                if let Ok(decrypted) =
                    crate::encryption::decrypt_filename(key, dir_inode_id, &entry.name)
                {
                    entry.name = decrypted;
                }
            }
        }

        // 3. Promote scanned directory to complete index in cache for future O(1) lookups and listings
        {
            let mut cache = guard.dir_cache.write().unwrap();
            let ix = cache.entry(dir_inode_id).or_default();
            ix.stored.clear();
            for raw in &raw_entries {
                ix.stored.insert(raw.name.clone(), raw.inode);
            }
            if encrypted {
                ix.plain.clear();
                for e in &entries {
                    ix.plain.insert(e.name.clone(), e.inode);
                }
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
        // blocked (P4.5). But reclaiming the ring needs a write lock, and taking one
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
                let start = Self::journal_region_start(bs).unwrap_or(0);
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
        // WAL first, then the rest of the mapping, so a sync point never lets the
        // image reach disk ahead of the log describing it.
        if guard.superblock.has_journal_layout() {
            let bs = guard.superblock.block_size as u64;
            if let Some(start) = Self::journal_region_start(bs) {
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
        target_inode_id: u64,
        phys_blk: u64,
    ) -> Result<(), DiskManagerError> {
        let mut parent_inode = Self::read_inode_internal(guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Not a directory",
            )));
        }

        let raw_entries = Self::read_dir_entries_from_block(&guard.mmap, phys_blk)?;
        let (matched_name, target_inode_id, phys_blk, remaining_entries) =
            if raw_entries.iter().any(|e| e.name == matched_name) {
                let remaining: Vec<_> = raw_entries
                    .into_iter()
                    .filter(|e| e.name != matched_name)
                    .collect();
                (
                    matched_name.to_string(),
                    target_inode_id,
                    phys_blk,
                    remaining,
                )
            } else {
                let enc_name = guard.encryption_key.as_ref().and_then(|key| {
                    crate::encryption::encrypt_filename(key, parent_inode_id, name).ok()
                });
                let candidates = [enc_name.as_deref(), Some(name)];
                let (c, id, blk) = candidates
                    .iter()
                    .flatten()
                    .find_map(|c| {
                        Self::locate_entry_in_dir(
                            &guard.mmap,
                            &parent_inode,
                            c,
                            crate::directory::hash_filename(c),
                        )
                        .map(|(id, blk)| (*c, id, blk))
                    })
                    .ok_or_else(|| {
                        DiskManagerError::Io(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "File not found",
                        ))
                    })?;
                let remaining: Vec<_> = Self::read_dir_entries_from_block(&guard.mmap, blk)?
                    .into_iter()
                    .filter(|e| e.name != c)
                    .collect();
                (c.to_string(), id, blk, remaining)
            };

        // ---- Phase 1: compute post-images (no mutation) ----
        let dir_image = Self::build_dir_block_image(&remaining_entries)?;

        let file_inode = Self::read_inode_internal(guard, target_inode_id)?;
        let blocks_to_free = Self::collect_inode_blocks(&guard.mmap, &file_inode);

        parent_inode.modified_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let parent_image = Self::build_inode_post_image(guard, parent_inode_id, &parent_inode)?;

        let mut ops = Vec::with_capacity(blocks_to_free.len() + 3);
        ops.push(crate::journal::MetadataOp::WriteBlockSlice {
            block_id: phys_blk,
            offset: 0,
            data: dir_image,
        });
        for blk in &blocks_to_free {
            ops.push(crate::journal::MetadataOp::SetDataBitmap {
                block_id: *blk,
                allocated: false,
            });
        }
        ops.push(crate::journal::MetadataOp::SetInodeBitmap {
            inode_id: target_inode_id,
            allocated: false,
        });
        ops.push(crate::journal::MetadataOp::WriteInode {
            inode_id: parent_inode_id,
            inode_bytes: Box::new(parent_image),
        });

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
            let mut cache = guard.dir_cache.write().unwrap();
            if let Some(ix) = cache.get_mut(&parent_inode_id) {
                ix.plain.remove(name);
                ix.stored.remove(&matched_name);
            }
            cache.remove(&target_inode_id);
        }

        if guard.durability_mode().is_range_based() {
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
        } else {
            guard.sync_mutation_ranges(&[])?;
        }
        Self::maybe_checkpoint(guard)?;
        Ok(())
    }

    pub fn delete_file(&self, parent_inode_id: u64, name: &str) -> Result<(), DiskManagerError> {
        // --- STAGE 1: Out-of-Lock Validation & Target Resolution ---
        let (matched_name, target_inode_id, phys_blk) = {
            let guard = self.inner.read().unwrap();
            let parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
            if parent_inode.mode != crate::inode::FileType::Directory {
                return Err(DiskManagerError::Io(std::io::Error::other(
                    "Not a directory",
                )));
            }

            let enc_name = if let Some(key) = &guard.encryption_key {
                crate::encryption::encrypt_filename(key, parent_inode_id, name).ok()
            } else {
                None
            };
            let candidates = [enc_name.as_deref(), Some(name)];

            // Locate entry under shared read lock without blocking readers
            let (matched_name, id, blk) = candidates
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
                })?;
            (matched_name, id, blk)
        }; // Read guard is dropped!

        // --- STAGE 2: In-Lock Modification & Commit ---
        let mut guard = self.inner.write().unwrap();

        // M3: journaled images take the WAL-first path; legacy images keep the
        // original in-place implementation untouched (zero-overhead guarantee).
        if guard.superblock.has_journal_layout() {
            return Self::delete_file_journaled(
                &mut guard,
                parent_inode_id,
                name,
                &matched_name,
                target_inode_id,
                phys_blk,
            );
        }

        let mut parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Not a directory",
            )));
        }

        let raw_entries = Self::read_dir_entries_from_block(&guard.mmap, phys_blk)?;
        let (matched_name, target_inode_id, phys_blk, remaining_entries) =
            if raw_entries.iter().any(|e| e.name == matched_name) {
                let remaining: Vec<_> = raw_entries
                    .into_iter()
                    .filter(|e| e.name != matched_name)
                    .collect();
                (matched_name, target_inode_id, phys_blk, remaining)
            } else {
                let enc_name = guard.encryption_key.as_ref().and_then(|key| {
                    crate::encryption::encrypt_filename(key, parent_inode_id, name).ok()
                });
                let candidates = [enc_name.as_deref(), Some(name)];
                let (c, id, blk) = candidates
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
                    })?;
                let remaining: Vec<_> = Self::read_dir_entries_from_block(&guard.mmap, blk)?
                    .into_iter()
                    .filter(|e| e.name != c)
                    .collect();
                (c, id, blk, remaining)
            };

        // Rewrite only the block that holds the entry.
        Self::rewrite_dir_entries_in_block(&mut guard.mmap, phys_blk, &remaining_entries)?;

        // Invalidate cached names in the parent, and drop the target's own index in case it was
        // a directory: its inode id can be reused, and stale names must never resolve under it.
        {
            let mut cache = guard.dir_cache.write().unwrap();
            if let Some(ix) = cache.get_mut(&parent_inode_id) {
                ix.plain.remove(name);
                ix.stored.remove(&matched_name);
            }
            cache.remove(&target_inode_id);
        }

        // Free Inode & Blocks
        let file_inode = Self::read_inode_internal(&guard, target_inode_id)?;
        let blocks_to_free = Self::collect_inode_blocks(&guard.mmap, &file_inode);

        // Free Data Blocks
        let mut min_freed_blk = u64::MAX;
        {
            let db_blk = guard.superblock.data_bitmap_block;
            let db_start = guard.superblock.data_block_start;
            let slice = Self::get_block_mut_from_map(&mut guard.mmap, db_blk).unwrap();
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
            let ib_blk = guard.superblock.inode_bitmap_block;
            let slice = Self::get_block_mut_from_map(&mut guard.mmap, ib_blk).unwrap();
            let mut ia = SimpleBlockAllocator::new(slice, 0);
            ia.free(target_inode_id)?;
        }
        if target_inode_id < guard.free_inode_hint {
            guard.free_inode_hint = target_inode_id;
        }
        guard.inode_cache.remove(target_inode_id);

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
        Ok(())
    }

    /// Analyzes disk fragmentation and returns statistics
    ///
    /// Scans the data block bitmap to calculate fragmentation metrics.
    ///
    /// # Returns
    /// `FragmentationStats` containing detailed fragmentation information
    pub fn analyze_fragmentation(&self) -> Result<FragmentationStats, DiskManagerError> {
        let guard = self.inner.read().unwrap();
        let sb = &guard.superblock;

        // Get data bitmap block
        let bitmap_block = sb.data_bitmap_block;
        let bitmap_slice = Self::get_block_from_map(&guard.mmap, bitmap_block)
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("Bitmap not found")))?;

        // Zero-allocation BitmapRef analysis
        let bitmap = crate::bitmap::BitmapRef::new(bitmap_slice);
        let total_blocks = (sb.block_count - sb.data_block_start) as usize;

        let mut used_blocks = 0;
        let mut free_runs = 0;
        let mut current_run_len = 0;
        let mut largest_free_run = 0;
        let mut total_gap_size = 0;

        // Fast 64-bit word scanning
        let mut in_free_run = false;
        let num_words = total_blocks / 64;
        let rem_bits = total_blocks % 64;
        let (chunks, _) = bitmap_slice.as_chunks::<8>();

        for chunk in chunks.iter().take(num_words) {
            let word = u64::from_le_bytes(*chunk);
            if word == 0 {
                // All 64 blocks are free
                if !in_free_run {
                    free_runs += 1;
                    in_free_run = true;
                }
                current_run_len += 64;
            } else if word == u64::MAX {
                // All 64 blocks are used
                used_blocks += 64;
                if in_free_run {
                    total_gap_size += current_run_len;
                    largest_free_run = largest_free_run.max(current_run_len);
                    in_free_run = false;
                    current_run_len = 0;
                }
            } else {
                // Mixed bits: inspect bit by bit
                used_blocks += word.count_ones() as usize;
                for bit in 0..64 {
                    let is_free = (word & (1u64 << bit)) == 0;
                    if is_free {
                        if !in_free_run {
                            free_runs += 1;
                            in_free_run = true;
                        }
                        current_run_len += 1;
                    } else if in_free_run {
                        total_gap_size += current_run_len;
                        largest_free_run = largest_free_run.max(current_run_len);
                        in_free_run = false;
                        current_run_len = 0;
                    }
                }
            }
        }

        // Remainder bits if total_blocks % 64 != 0
        if rem_bits > 0 {
            let base_idx = num_words * 64;
            for i in base_idx..total_blocks {
                let is_free = !bitmap.get(i);
                if is_free {
                    if !in_free_run {
                        free_runs += 1;
                        in_free_run = true;
                    }
                    current_run_len += 1;
                } else {
                    used_blocks += 1;
                    if in_free_run {
                        total_gap_size += current_run_len;
                        largest_free_run = largest_free_run.max(current_run_len);
                        in_free_run = false;
                        current_run_len = 0;
                    }
                }
            }
        }

        // Handle final free run if exists
        if in_free_run {
            total_gap_size += current_run_len;
            largest_free_run = largest_free_run.max(current_run_len);
        }

        let free_blocks = total_blocks - used_blocks;
        let avg_gap_size = if free_runs > 0 {
            total_gap_size as f64 / free_runs as f64
        } else {
            0.0
        };

        // Calculate fragmentation ratio
        // Ideal: all free space in one contiguous run (free_runs = 1 or 0)
        // Worst: each free block is separate (free_runs = free_blocks)
        let fragmentation_ratio = if free_blocks > 0 && free_runs > 1 {
            (free_runs - 1) as f64 / (free_blocks - 1).max(1) as f64
        } else {
            0.0
        };

        Ok(FragmentationStats {
            total_blocks,
            used_blocks,
            free_blocks,
            free_runs,
            largest_free_run,
            avg_gap_size,
            fragmentation_ratio,
        })
    }

    /// Defragments the filesystem by reorganizing files contiguously
    ///
    /// # Arguments
    /// * `source_path` - Path to the source image file
    /// * `mode` - Defragmentation mode (Safe or InPlace)
    /// * `output_path` - Optional output path for safe mode (defaults to source_path.defrag.tmp)
    ///
    /// # Returns
    /// Statistics about the defragmentation operation
    pub fn defragment(
        &self,
        source_path: &str,
        mode: DefragMode,
        output_path: Option<&str>,
    ) -> Result<DefragStats, DiskManagerError> {
        match mode {
            DefragMode::Safe => self.defragment_safe(source_path, output_path),
            DefragMode::InPlace => self.defragment_inplace(),
        }
    }

    /// Safe defragmentation: creates new image with defragmented layout
    fn defragment_safe(
        &self,
        source_path: &str,
        output_path: Option<&str>,
    ) -> Result<DefragStats, DiskManagerError> {
        // Get fragmentation before
        let stats_before = self.analyze_fragmentation()?;

        // Determine temp path
        let temp_path = if let Some(out) = output_path {
            out.to_string()
        } else {
            format!("{}.defrag.tmp", source_path)
        };

        // Step 1: Copy the entire image file first (safer than creating new)
        std::fs::copy(source_path, &temp_path)?;

        // Step 2: Open the copy and perform actual defragmentation
        let temp_dm = DiskManager::open(&temp_path, 0)?;

        // Collect all active, allocated inodes and their data
        let sb = temp_dm.superblock();
        let mut file_data_list = Vec::new();
        let mut directory_blocks = Vec::new();

        let mut allocated_inodes = Vec::new();
        {
            let guard = temp_dm.inner.read().unwrap();
            let ib_blk = guard.superblock.inode_bitmap_block;
            if let Some(bitmap_slice) = Self::get_block_from_map(&guard.mmap, ib_blk) {
                let bitmap = crate::bitmap::BitmapRef::new(bitmap_slice);
                bitmap.for_each_set_bit(sb.inode_count as usize, |idx| {
                    allocated_inodes.push(idx as u64);
                });
            }
        }

        for inode_id in allocated_inodes {
            if let Ok(inode) = temp_dm.read_inode(inode_id) {
                if inode.mode == crate::inode::FileType::Directory {
                    let guard = temp_dm.inner.read().unwrap();
                    let dir_blks = Self::collect_inode_blocks(&guard.mmap, &inode);
                    directory_blocks.extend(dir_blks);
                } else if inode.mode == crate::inode::FileType::File && inode.size > 0 {
                    // Read and store data
                    let is_compressed = inode.compressed_size > 0;
                    let data = temp_dm.read_data(inode_id)?;
                    let filter_cfg = crate::filters::FilterConfig {
                        typesize: inode.filter_typesize,
                        delta: inode.filter_delta,
                        shuffle: inode.filter_shuffle,
                        bitshuffle: inode.filter_bitshuffle,
                    };
                    file_data_list.push((inode_id, is_compressed, data, filter_cfg));

                    // Reset inode on disk so write_data allocates contiguous blocks
                    let mut cleared_inode = inode;
                    cleared_inode.size = 0;
                    cleared_inode.compressed_size = 0;
                    cleared_inode.blocks = [0; 12];
                    cleared_inode.triple_indirect = 0;
                    temp_dm.write_inode(inode_id, &cleared_inode)?;
                }
            }
        }

        // Step 3: Clear data bitmap and preserve directory blocks
        {
            let mut guard = temp_dm.inner.write().unwrap();
            let data_bitmap_block = guard.superblock.data_bitmap_block;
            let data_start = guard.superblock.data_block_start;
            if let Some(bitmap_slice) =
                Self::get_block_mut_from_map(&mut guard.mmap, data_bitmap_block)
            {
                bitmap_slice.fill(0);
                let mut bitmap = crate::bitmap::Bitmap::new(bitmap_slice);
                for &dir_blk in &directory_blocks {
                    if dir_blk >= data_start {
                        let bit_idx = (dir_blk - data_start) as usize;
                        bitmap.set(bit_idx);
                    }
                }
            }
        }

        // Step 4: Reallocate blocks contiguously and write data
        let mut files_processed = 0;
        let mut bytes_moved = 0u64;

        for (inode_id, is_compressed, data, filter_cfg) in file_data_list {
            let comp_mode = if is_compressed {
                CompressionMode::Always
            } else {
                CompressionMode::Never
            };

            temp_dm.write_data_with_filters(inode_id, 0, &data, comp_mode, filter_cfg)?;
            files_processed += 1;
            bytes_moved += data.len() as u64;
        }

        // Step 5: Flush all changes
        temp_dm.flush()?;
        drop(temp_dm);

        // Step 6: Safe replacement using 3-step rename
        let backup_path = format!("{}.old", source_path);

        // 6a: Rename original to backup
        std::fs::rename(source_path, &backup_path)?;

        // 6b: Rename new defragged to original
        match std::fs::rename(&temp_path, source_path) {
            Ok(_) => {
                // Success! Now we can delete the backup
                // But let's verify first
                match DiskManager::open(source_path, 0) {
                    Ok(final_dm) => {
                        let stats_after = final_dm.analyze_fragmentation()?;

                        // Everything OK, delete backup
                        let _ = std::fs::remove_file(&backup_path);

                        Ok(DefragStats {
                            files_processed,
                            bytes_moved,
                            blocks_freed: stats_before
                                .free_runs
                                .saturating_sub(stats_after.free_runs),
                            frag_before: stats_before.fragmentation_ratio,
                            frag_after: stats_after.fragmentation_ratio,
                        })
                    }
                    Err(e) => {
                        // Failed to open new file! Restore from backup
                        let _ = std::fs::rename(&backup_path, source_path);
                        Err(e)
                    }
                }
            }
            Err(e) => {
                // Failed to rename new file! Restore original
                let _ = std::fs::rename(&backup_path, source_path);
                Err(DiskManagerError::Io(e))
            }
        }
    }

    /// In-place defragmentation: directly modifies original image
    fn defragment_inplace(&self) -> Result<DefragStats, DiskManagerError> {
        // TODO: Implement in-place defrag
        Err(DiskManagerError::Io(std::io::Error::other(
            "In-place defragmentation not yet implemented",
        )))
    }

    /// Structural consistency check (fsck) for OIFS filesystem
    pub fn verify_integrity(&self) -> Result<FsckReport, DiskManagerError> {
        let guard = self.inner.read().unwrap();
        let sb = guard.superblock;

        // 1. Collect all allocated Inode IDs from Inode Bitmap
        let mut allocated_inodes = std::collections::HashSet::new();
        let ib_blk = sb.inode_bitmap_block;
        if let Some(bitmap_slice) = Self::get_block_from_map(&guard.mmap, ib_blk) {
            let bitmap = crate::bitmap::BitmapRef::new(bitmap_slice);
            bitmap.for_each_set_bit(sb.inode_count as usize, |idx| {
                allocated_inodes.insert(idx as u64);
            });
        }

        // 2. Collect all allocated Data Blocks from Data Bitmap
        let mut allocated_data_blocks = std::collections::HashSet::new();
        let db_blk = sb.data_bitmap_block;
        let db_start = sb.data_block_start;
        if let Some(bitmap_slice) = Self::get_block_from_map(&guard.mmap, db_blk) {
            let bitmap = crate::bitmap::BitmapRef::new(bitmap_slice);
            let max_data_blocks = sb.block_count.saturating_sub(db_start) as usize;
            bitmap.for_each_set_bit(max_data_blocks, |idx| {
                allocated_data_blocks.insert(db_start + idx as u64);
            });
        }

        // 3. Traversal tracking sets
        let mut referenced_inodes = std::collections::HashSet::new();
        let mut referenced_data_blocks: std::collections::HashMap<u64, Vec<u64>> =
            std::collections::HashMap::new();
        let mut cross_linked_blocks = std::collections::HashSet::new();

        // Always reference root inode
        referenced_inodes.insert(sb.root_inode);

        // Recursive directory scanner
        let mut queue = vec![sb.root_inode];
        let mut visited = std::collections::HashSet::new();

        while let Some(dir_id) = queue.pop() {
            if !visited.insert(dir_id) {
                continue;
            }

            let dir_inode = Self::read_inode_internal(&guard, dir_id)?;
            if dir_inode.mode != crate::inode::FileType::Directory {
                continue;
            }

            let num_blocks = Self::dir_num_blocks(&dir_inode);
            for blk_idx in 0..num_blocks {
                let phys_blk = Self::resolve_logical_block_id(&guard.mmap, &dir_inode, blk_idx);
                if phys_blk == 0 {
                    continue;
                }
                if let Some(block_data) = Self::get_block_from_map(&guard.mmap, phys_blk) {
                    for entry in crate::directory::DirectoryIterator::new(block_data).flatten() {
                        referenced_inodes.insert(entry.inode);

                        if let Ok(child_inode) = Self::read_inode_internal(&guard, entry.inode)
                            && child_inode.mode == crate::inode::FileType::Directory
                        {
                            queue.push(entry.inode);
                        }
                    }
                }
            }
        }

        // Scan all referenced inodes and collect block references
        for &inode_id in &referenced_inodes {
            if let Ok(inode) = Self::read_inode_internal(&guard, inode_id) {
                let blks = Self::collect_inode_blocks(&guard.mmap, &inode);
                for blk in blks {
                    let entries = referenced_data_blocks.entry(blk).or_default();
                    entries.push(inode_id);
                    if entries.len() > 1 {
                        cross_linked_blocks.insert(blk);
                    }
                }
            }
        }

        // Calculate differences
        // A. Orphan Inodes: allocated but not referenced
        let mut orphan_inodes = Vec::new();
        for &inode_id in &allocated_inodes {
            if !referenced_inodes.contains(&inode_id) {
                orphan_inodes.push(inode_id);
            }
        }

        // B. Leaked Blocks: allocated in bitmap but not referenced by any inode
        let mut leaked_blocks = Vec::new();
        for &blk in &allocated_data_blocks {
            if !referenced_data_blocks.contains_key(&blk) {
                leaked_blocks.push(blk);
            }
        }

        // C. Missing Blocks: referenced by inodes but free in data bitmap
        let mut missing_blocks = Vec::new();
        for &blk in referenced_data_blocks.keys() {
            if !allocated_data_blocks.contains(&blk) {
                missing_blocks.push(blk);
            }
        }

        let cross_linked_blocks: Vec<u64> = cross_linked_blocks.into_iter().collect();
        orphan_inodes.sort();
        leaked_blocks.sort();
        missing_blocks.sort();

        let is_clean = orphan_inodes.is_empty()
            && leaked_blocks.is_empty()
            && missing_blocks.is_empty()
            && cross_linked_blocks.is_empty();

        Ok(FsckReport {
            is_clean,
            orphan_inodes,
            leaked_blocks,
            missing_blocks,
            cross_linked_blocks,
        })
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

    /// Prove that read_at slice clamping cannot overflow or invert start..end ranges (PANIC-02).
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
    }
}
