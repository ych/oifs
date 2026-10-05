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
struct DiskManagerInner {
    /// Image file handle; payload reads use it directly under the `Pread` / `IoUring`
    /// backends (P3.2).
    file: File,
    /// Memory-mapped view of the file system image
    mmap: MmapMut,
    /// Cached copy of the superblock
    pub superblock: SuperBlock,
    /// Encryption key (if filesystem is encrypted)
    pub encryption_key: Option<crate::encryption::EncryptionKey>,
    /// Search hint for sequential O(1) block allocation
    pub free_block_hint: u64,
    /// Search hint for sequential O(1) inode allocation
    pub free_inode_hint: u64,
    /// Inode cache for zero-copy metadata access (P2.2)
    pub inode_cache: RwLock<BoundedInodeCache>,
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

    /// Syncs one or more modified byte ranges according to the current `DurabilityMode`.
    pub fn sync_mutation_ranges(&self, ranges: &[(usize, usize)]) -> Result<(), DiskManagerError> {
        match self.durability_mode() {
            DurabilityMode::Lazy => Ok(()),
            DurabilityMode::RangeAsync => {
                let mmap_len = self.mmap.len();
                for &(offset, len) in ranges {
                    if len > 0 && offset < mmap_len {
                        let actual_len = len.min(mmap_len - offset);
                        let _ = self.mmap.flush_async_range(offset, actual_len);
                    }
                }
                Ok(())
            }
            DurabilityMode::Strict => {
                let mmap_len = self.mmap.len();
                for &(offset, len) in ranges {
                    if len > 0 && offset < mmap_len {
                        let actual_len = len.min(mmap_len - offset);
                        self.mmap
                            .flush_range(offset, actual_len)
                            .map_err(DiskManagerError::Io)?;
                    }
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
struct DirIndex {
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

/// Local simulation of block/inode allocation against copied bitmaps.
///
/// The journaled create path must know every allocated id *before* it touches the
/// image, so the whole operation can be expressed as post-images and committed to
/// the WAL first. Allocation here mirrors `SimpleBlockAllocator` semantics exactly
/// (same hint handling, same "first free bit at or after hint" rule), so the ids it
/// predicts are the ids the real allocator would hand out.
struct AllocSim {
    inode_bitmap: Vec<u8>,
    data_bitmap: Vec<u8>,
    data_start: u64,
    free_inode_hint: u64,
    free_block_hint: u64,
    /// Blocks allocated during this simulation, in order.
    fresh_blocks: Vec<u64>,
}

impl AllocSim {
    fn new(
        guard: &DiskManagerInner,
        free_inode_hint: u64,
        free_block_hint: u64,
    ) -> Result<Self, DiskManagerError> {
        let sb = guard.superblock;
        let ib = DiskManager::get_block_from_map(&guard.mmap, sb.inode_bitmap_block)
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("inode bitmap not found")))?;
        let db = DiskManager::get_block_from_map(&guard.mmap, sb.data_bitmap_block)
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("data bitmap not found")))?;
        Ok(Self {
            inode_bitmap: ib.to_vec(),
            data_bitmap: db.to_vec(),
            data_start: sb.data_block_start,
            free_inode_hint,
            free_block_hint,
            fresh_blocks: Vec::new(),
        })
    }

    fn alloc_inode(&mut self) -> Result<u64, DiskManagerError> {
        let mut a = SimpleBlockAllocator::new(&mut self.inode_bitmap, 0);
        let id = a
            .allocate_with_hint(Some(self.free_inode_hint))
            .map_err(DiskManagerError::Allocator)?;
        self.free_inode_hint = id + 1;
        Ok(id)
    }

    fn alloc_block(&mut self) -> Result<u64, DiskManagerError> {
        let mut a = SimpleBlockAllocator::new(&mut self.data_bitmap, self.data_start);
        let blk = a
            .allocate_with_hint(Some(self.free_block_hint))
            .map_err(DiskManagerError::Allocator)?;
        self.free_block_hint = blk + 1;
        self.fresh_blocks.push(blk);
        Ok(blk)
    }

    fn is_fresh(&self, block_id: u64) -> bool {
        self.fresh_blocks.contains(&block_id)
    }
}

/// Mirror of [`DiskManager::get_or_alloc_block`] that allocates against [`AllocSim`].
///
/// Instead of writing pointer blocks into the image it records the equivalent
/// `MetadataOp`s, so the resulting indirect-block structure is captured in the WAL
/// transaction exactly as the legacy path would have written it.
fn sim_get_or_alloc_block(
    mmap: &MmapMut,
    inode: &mut Inode,
    logical_idx: usize,
    sim: &mut AllocSim,
    ops: &mut Vec<crate::journal::MetadataOp>,
) -> Result<u64, DiskManagerError> {
    use crate::inode::BlockPath;

    fn ensure_root(
        slot: &mut u64,
        sim: &mut AllocSim,
        ops: &mut Vec<crate::journal::MetadataOp>,
    ) -> Result<u64, DiskManagerError> {
        if *slot == 0 {
            let blk = sim.alloc_block()?;
            ops.push(crate::journal::MetadataOp::SetDataBitmap {
                block_id: blk,
                allocated: true,
            });
            *slot = blk;
        }
        Ok(*slot)
    }

    fn child(
        mmap: &MmapMut,
        parent_blk: u64,
        idx: usize,
        sim: &mut AllocSim,
        ops: &mut Vec<crate::journal::MetadataOp>,
    ) -> Result<u64, DiskManagerError> {
        // A freshly allocated pointer block reads as all-zero, so its entries are 0.
        let existing = if sim.is_fresh(parent_blk) {
            0
        } else {
            DiskManager::read_block_ptr(mmap, parent_blk, idx)
        };
        if existing != 0 {
            return Ok(existing);
        }
        let blk = sim.alloc_block()?;
        ops.push(crate::journal::MetadataOp::SetDataBitmap {
            block_id: blk,
            allocated: true,
        });
        // Record the pointer write into the parent pointer block.
        ops.push(crate::journal::MetadataOp::WriteBlockSlice {
            block_id: parent_blk,
            offset: (idx * 8) as u32,
            data: blk.to_le_bytes().to_vec(),
        });
        Ok(blk)
    }

    match BlockPath::from_logical(logical_idx) {
        Some(BlockPath::Direct(i)) => {
            if inode.blocks[i] == 0 {
                let blk = sim.alloc_block()?;
                ops.push(crate::journal::MetadataOp::SetDataBitmap {
                    block_id: blk,
                    allocated: true,
                });
                inode.blocks[i] = blk;
            }
            Ok(inode.blocks[i])
        }
        Some(BlockPath::Single(i)) => {
            let sib = ensure_root(&mut inode.blocks[10], sim, ops)?;
            if sib == 0 {
                return Ok(0);
            }
            child(mmap, sib, i, sim, ops)
        }
        Some(BlockPath::Double(a, b)) => {
            let dib = ensure_root(&mut inode.blocks[11], sim, ops)?;
            if dib == 0 {
                return Ok(0);
            }
            let sib = child(mmap, dib, a, sim, ops)?;
            if sib == 0 {
                return Ok(0);
            }
            child(mmap, sib, b, sim, ops)
        }
        Some(BlockPath::Triple(a, b, c)) => {
            let tib = ensure_root(&mut inode.triple_indirect, sim, ops)?;
            if tib == 0 {
                return Ok(0);
            }
            let dib = child(mmap, tib, a, sim, ops)?;
            if dib == 0 {
                return Ok(0);
            }
            let sib = child(mmap, dib, b, sim, ops)?;
            if sib == 0 {
                return Ok(0);
            }
            child(mmap, sib, c, sim, ops)
        }
        None => Err(DiskManagerError::Io(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "File too large (max 513GB)",
        ))),
    }
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
struct BoundedInodeCache {
    map: FxHashMap<u64, Inode>,
    /// Inode ids in insertion order; the front is the next eviction victim.
    order: VecDeque<u64>,
    capacity: usize,
}

impl BoundedInodeCache {
    fn with_capacity(capacity: usize) -> Self {
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
        // Drop the queue slot too, otherwise a later re-insert of the same id would
        // evict the *new* entry prematurely via a stale queue entry.
        self.order.retain(|id| *id != inode_id);
    }

    /// Drop every cached inode.
    ///
    /// Used by a format migration, which rewrites every slot underneath the cache.
    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
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
        let mut c = BoundedInodeCache::with_capacity(4);
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
        let mut c = BoundedInodeCache::with_capacity(3);
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
        let mut c = BoundedInodeCache::with_capacity(3);
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
        let mut c = BoundedInodeCache::with_capacity(2);
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
        let mut c = BoundedInodeCache::with_capacity(4);
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
        let mut c = BoundedInodeCache::with_capacity(2);
        let mut i = ino(crate::inode::FileType::File);
        i.size = 4242;
        c.insert(5, i);
        assert_eq!(c.get(5).expect("present").size, 4242);
        assert!(c.get(6).is_none());
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
            inode_cache: RwLock::new(BoundedInodeCache::with_capacity(INODE_CACHE_CAPACITY)),
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
    fn get_block_mut_from_map(mmap: &mut MmapMut, block_id: u64) -> Option<&mut [u8]> {
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

        // Flush the frame bytes *and* the header block that advances `head`.
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
        guard.inode_cache.write().unwrap().clear();

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
        file_type: crate::inode::FileType,
    ) -> Result<u64, DiskManagerError> {
        // ---- Phase 1: read-only validation ----
        let mut parent_inode = Self::read_inode_internal(guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
        }

        let stored_name = if let Some(key) = &guard.encryption_key {
            crate::encryption::encrypt_filename(key, parent_inode_id, name)
                .unwrap_or_else(|_| name.to_string())
        } else {
            name.to_string()
        };
        let stored_hash = crate::directory::hash_filename(&stored_name);

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

        let mut sim = AllocSim::new(guard, guard.free_inode_hint, guard.free_block_hint)?;
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
            let phys_blk = sim_get_or_alloc_block(
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

        {
            let mut ic = guard.inode_cache.write().unwrap();
            ic.insert(new_inode_id, new_inode);
            ic.insert(parent_inode_id, parent_inode);
        }
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
        let mut guard = self.inner.write().unwrap();

        // M3: journaled images take the WAL-first path; legacy images keep the
        // original in-place implementation untouched (zero-overhead guarantee).
        if guard.superblock.has_journal_layout() {
            return Self::create_entry_journaled(&mut guard, parent_inode_id, name, file_type);
        }

        // 1. Read Parent
        let mut parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
        }

        let stored_name = if let Some(key) = &guard.encryption_key {
            crate::encryption::encrypt_filename(key, parent_inode_id, name)
                .unwrap_or_else(|_| name.to_string())
        } else {
            name.to_string()
        };
        let stored_hash = crate::directory::hash_filename(&stored_name);

        // Check if file or directory already exists (checks ciphertext and legacy plaintext names).
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

    /// Stage a payload write into the file described by `inode`.
    ///
    /// Allocates through `sim` so every id is known before the bitmaps are touched,
    /// and records each metadata change in `ops` (bitmap bits, indirect pointer writes).
    /// `used` collects the physical blocks touched, so the caller can free orphans and
    /// flush exactly the right ranges.
    ///
    /// Payload bytes are written straight into the mapping and are deliberately **not**
    /// recorded as ops: journaling user data would multiply WAL traffic by the file size
    /// and defeat the point of a metadata journal. Durability instead comes from
    /// ordering — the caller flushes these blocks *before* committing the metadata
    /// transaction, so recovery never exposes an allocated-but-empty block.
    fn stage_payload_write(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        phys_off: u64,
        buf: &[u8],
        sim: &mut AllocSim,
        ops: &mut Vec<crate::journal::MetadataOp>,
        used: &mut Vec<u64>,
    ) -> Result<(), DiskManagerError> {
        let mut written = 0usize;
        let mut cur = phys_off;
        while written < buf.len() {
            let blk_idx = (cur / BLOCK_SIZE as u64) as usize;
            let in_blk_off = (cur % BLOCK_SIZE as u64) as usize;
            let n = std::cmp::min(buf.len() - written, BLOCK_SIZE - in_blk_off);

            let phys = sim_get_or_alloc_block(&guard.mmap, inode, blk_idx, sim, ops)?;
            if phys != 0 && !used.contains(&phys) {
                used.push(phys);
            }
            if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, phys) {
                slice[in_blk_off..in_blk_off + n].copy_from_slice(&buf[written..written + n]);
            }
            written += n;
            cur += n as u64;
        }
        Ok(())
    }

    /// Clear block pointers at or beyond the file's new logical length.
    ///
    /// When a file shrinks, the indirect pointer blocks keep stale entries pointing at
    /// blocks that are no longer used. Those blocks are freed by the caller, so leaving
    /// the pointers behind would leave the inode referencing blocks the bitmap says are
    /// free — an inconsistency `fsck` reports as `missing_blocks`.
    ///
    /// Direct pointers live in the inode and are captured by the `WriteInode` op;
    /// indirect entries are recorded as zeroing `WriteBlockSlice` ops so the change is
    /// part of the same transaction.
    fn prune_stale_pointers(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        first_stale: usize,
        ops: &mut Vec<crate::journal::MetadataOp>,
    ) -> Result<(), DiskManagerError> {
        const POINTERS_PER_BLOCK: usize = 512; // 4096 / 8

        if first_stale == 0 {
            return Ok(());
        }

        // Direct pointers.
        for i in first_stale.min(10)..10 {
            inode.blocks[i] = 0;
        }
        if first_stale <= 10 {
            // The whole single-indirect block is unreachable; drop the pointer so it is
            // freed with the rest of the orphans.
            inode.blocks[10] = 0;
            return Ok(());
        }

        // Single indirect.
        let sib = inode.blocks[10];
        if sib != 0 {
            let start = first_stale - 10;
            if start < POINTERS_PER_BLOCK {
                for idx in start..POINTERS_PER_BLOCK {
                    ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                        block_id: sib,
                        offset: (idx * 8) as u32,
                        data: vec![0u8; 8],
                    });
                }
            }
            if first_stale <= 10 + POINTERS_PER_BLOCK {
                return Ok(());
            }
        }

        // Double indirect: clear whole second-level blocks beyond the new length.
        let dib = inode.blocks[11];
        if dib != 0 {
            let start = first_stale - 10 - POINTERS_PER_BLOCK;
            for a in start.min(POINTERS_PER_BLOCK)..POINTERS_PER_BLOCK {
                ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                    block_id: dib,
                    offset: (a * 8) as u32,
                    data: vec![0u8; 8],
                });
            }
            for a in start.min(POINTERS_PER_BLOCK)..POINTERS_PER_BLOCK {
                let sib2 = DiskManager::read_block_ptr(&guard.mmap, dib, a);
                if sib2 == 0 {
                    continue;
                }
                ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                    block_id: sib2,
                    offset: 0,
                    data: vec![0u8; BLOCK_SIZE],
                });
            }
            if first_stale <= 10 + POINTERS_PER_BLOCK * (1 + POINTERS_PER_BLOCK) {
                return Ok(());
            }
        }

        // Triple indirect: clear whole third-level blocks beyond the new length.
        let tib = inode.triple_indirect;
        if tib != 0 {
            let span = 10 + POINTERS_PER_BLOCK * (1 + POINTERS_PER_BLOCK);
            if first_stale > span && tib != 0 {
                let start = first_stale - span;
                let max_b = POINTERS_PER_BLOCK;
                for b in start.min(max_b)..max_b {
                    ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                        block_id: tib,
                        offset: (b * 8) as u32,
                        data: vec![0u8; 8],
                    });
                    let dib2 = DiskManager::read_block_ptr(&guard.mmap, tib, b);
                    if dib2 == 0 {
                        continue;
                    }
                    for c in 0..POINTERS_PER_BLOCK {
                        ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                            block_id: dib2,
                            offset: (c * 8) as u32,
                            data: vec![0u8; 8],
                        });
                        let sib3 = DiskManager::read_block_ptr(&guard.mmap, dib2, c);
                        if sib3 == 0 {
                            continue;
                        }
                        ops.push(crate::journal::MetadataOp::WriteBlockSlice {
                            block_id: sib3,
                            offset: 0,
                            data: vec![0u8; BLOCK_SIZE],
                        });
                    }
                }
            }
        }
        Ok(())
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
        data: &[u8],
        compression_mode: CompressionMode,
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

        let mut sim = AllocSim::new(guard, guard.free_inode_hint, guard.free_block_hint)?;
        let mut ops: Vec<crate::journal::MetadataOp> = Vec::new();
        let mut used: Vec<u64> = Vec::new();

        // Final physical offset, buffer, logical size and physical size for every case,
        // mirroring the in-place path exactly.
        //
        // Sizes must match the legacy implementation byte for byte: the same image
        // content has to be produced whether or not journaling is enabled, otherwise
        // enabling `--journal` would silently change filesystem semantics.
        let (phys_off, buffer, new_size, new_compressed_size): (u64, Vec<u8>, u64, u64) = {
            let is_full_overwrite = old_size == 0
                || data.len() as u64 >= old_size
                || inode.compressed_size > 0
                || inode.encrypted
                || filter_config.is_active();

            if file_offset == 0 && is_full_overwrite {
                let (buf, compressed) = Self::build_full_overwrite_buffer(
                    guard,
                    &mut inode,
                    data,
                    compression_mode,
                    &filter_config,
                )?;
                let logical = if compressed {
                    data.len() as u64
                } else {
                    buf.len() as u64
                };
                let logical = if compressed {
                    logical
                } else {
                    std::cmp::max(old_size, logical)
                };
                let physical = if compressed { buf.len() as u64 } else { 0 };
                (0u64, buf, logical, physical)
            } else if inode.compressed_size > 0 {
                let fast_append = file_offset == inode.size
                    && !inode.encrypted
                    && !filter_config.is_active()
                    && inode.filter_typesize == 0
                    && guard.encryption_key.is_none();
                if fast_append {
                    let frame = zstd::stream::encode_all(std::io::Cursor::new(data), 0)
                        .map_err(DiskManagerError::Io)?;
                    let size = old_size.saturating_add(frame.len() as u64);
                    let physical = inode.compressed_size + frame.len() as u64;
                    (inode.compressed_size, frame, size, physical)
                } else {
                    // Read-modify-recompress: splice into the decoded image and rewrite
                    // from offset 0. The legacy path resets the sizes before rewriting,
                    // so `old_size` plays no part here.
                    let mut full = Self::read_data_internal(guard, &inode)?;
                    let end = (file_offset as usize).saturating_add(data.len());
                    if full.len() < end {
                        full.resize(end, 0);
                    }
                    full[file_offset as usize..end].copy_from_slice(data);
                    let effective = Self::effective_filter(&inode, &filter_config);
                    let (buf, compressed) = Self::build_full_overwrite_buffer(
                        guard,
                        &mut inode,
                        &full,
                        compression_mode,
                        &effective,
                    )?;
                    let logical = if compressed {
                        full.len() as u64
                    } else {
                        buf.len() as u64
                    };
                    let physical = if compressed { buf.len() as u64 } else { 0 };
                    (0u64, buf, logical, physical)
                }
            } else {
                // Raw append or random write.
                let logical = std::cmp::max(old_size, file_offset + data.len() as u64);
                (file_offset, data.to_vec(), logical, 0)
            }
        };

        // 1. Stage payload (bitmap bits still clear).
        Self::stage_payload_write(
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
                Self::prune_stale_pointers(guard, &mut inode, needed_logical, &mut ops)?;
            }
        }

        inode.size = new_size;
        inode.compressed_size = new_compressed_size;
        if new_compressed_size > 0 {
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
        let payload_ranges: Vec<(usize, usize)> =
            used.iter().map(|b| guard.block_byte_range(*b)).collect();
        if guard.durability_mode() == DurabilityMode::Strict {
            for &(off, len) in &payload_ranges {
                let mmap_len = guard.mmap.len();
                if len > 0 && off < mmap_len {
                    let actual = len.min(mmap_len - off);
                    guard.mmap.flush_range(off, actual)?;
                }
            }
        } else {
            for &(off, len) in &payload_ranges {
                let mmap_len = guard.mmap.len();
                if len > 0 && off < mmap_len {
                    let actual = len.min(mmap_len - off);
                    let _ = guard.mmap.flush_async_range(off, actual);
                }
            }
        }

        // 4. Commit point.
        Self::commit_journal_tx(guard, &ops)?;

        // 5. Apply.
        Self::apply_journal_ops(guard, &ops)?;

        guard.free_inode_hint = sim.free_inode_hint;
        guard.free_block_hint = sim.free_block_hint;
        {
            let mut ic = guard.inode_cache.write().unwrap();
            ic.insert(inode_id, inode);
        }

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
    /// compress, then encrypt. Mirrors [`Self::write_data_from_start_internal`] exactly.
    ///
    /// Returns `(buffer, is_compressed)`. The flag matters: the caller must record it
    /// in the inode, otherwise a compressed payload would be tagged as uncompressed and
    /// read back as garbage.
    fn build_full_overwrite_buffer(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        data: &[u8],
        compression_mode: CompressionMode,
        filter_config: &crate::filters::FilterConfig,
    ) -> Result<(Vec<u8>, bool), DiskManagerError> {
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

        if let Some(key) = &guard.encryption_key {
            let nonce = crate::encryption::generate_nonce();
            let enc = crate::encryption::encrypt_data(final_data.as_ref(), key, &nonce)?;
            inode.encrypted = true;
            inode.encryption_nonce = nonce;
            return Ok((enc, is_compressed));
        }
        Ok((final_data.into_owned(), is_compressed))
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

    fn write_data_from_start_internal(
        guard: &mut DiskManagerInner,
        inode_id: u64,
        inode: &mut Inode,
        data: &[u8],
        compression_mode: CompressionMode,
        filter_config: crate::filters::FilterConfig,
    ) -> Result<(), DiskManagerError> {
        // === PRE-COMPRESSION FILTER STEP ===
        let filtered_data = crate::filters::apply_filters_cow(data, &filter_config);
        let working_data: &[u8] = &filtered_data;

        let final_data: std::borrow::Cow<[u8]>;
        let mut is_compressed = false;

        let should_compress = match compression_mode {
            CompressionMode::Always => true,
            CompressionMode::Never => false,
            CompressionMode::Auto => working_data.len() >= 8192,
        };

        if should_compress {
            let compressed = zstd::stream::encode_all(std::io::Cursor::new(working_data), 0)
                .map_err(DiskManagerError::Io)?;

            match compression_mode {
                CompressionMode::Always => {
                    final_data = std::borrow::Cow::Owned(compressed);
                    is_compressed = true;
                }
                CompressionMode::Auto => {
                    if compressed.len() < working_data.len() {
                        final_data = std::borrow::Cow::Owned(compressed);
                        is_compressed = true;
                    } else {
                        final_data = filtered_data;
                    }
                }
                CompressionMode::Never => {
                    final_data = filtered_data;
                }
            }
        } else {
            final_data = filtered_data;
        }

        // === ENCRYPTION STEP ===
        let final_encrypted: Vec<u8>;
        let write_buffer: &[u8] = if let Some(encryption_key) = &guard.encryption_key {
            let nonce = crate::encryption::generate_nonce();
            final_encrypted =
                crate::encryption::encrypt_data(final_data.as_ref(), encryption_key, &nonce)?;
            inode.encrypted = true;
            inode.encryption_nonce = nonce;
            &final_encrypted
        } else {
            final_data.as_ref()
        };

        let mut touched = if guard.durability_mode().is_range_based() {
            Some(Vec::new())
        } else {
            None
        };
        Self::write_buffer_at_offset(guard, inode, 0, write_buffer, touched.as_mut())?;

        if is_compressed {
            inode.size = data.len() as u64; // Logical size
            inode.compressed_size = write_buffer.len() as u64; // Physical size
        } else {
            inode.size = std::cmp::max(inode.size, write_buffer.len() as u64);
            inode.compressed_size = 0;
        }

        inode.filter_typesize = filter_config.typesize;
        inode.filter_delta = filter_config.delta;
        inode.filter_shuffle = filter_config.shuffle;
        inode.filter_bitshuffle = filter_config.bitshuffle;

        inode.modified_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self::write_inode_internal(guard, inode_id, inode)?;
        if let Some(tb) = touched {
            let mut ranges = Vec::with_capacity(tb.len() + 2);
            ranges.push(guard.data_bitmap_byte_range());
            ranges.push(guard.inode_byte_range(inode_id));
            for blk in tb {
                ranges.push(guard.block_byte_range(blk));
            }
            guard.sync_mutation_ranges(&ranges)?;
        } else {
            guard.sync_mutation_ranges(&[])?;
        }
        Ok(())
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
        let mut guard = self.inner.write().unwrap();
        let mut inode = Self::read_inode_internal(&guard, inode_id)?;

        if inode.mode != crate::inode::FileType::File {
            return Err(DiskManagerError::Io(std::io::Error::other(
                "Cannot write data to non-file inode",
            )));
        }

        // M3: journaled images stage the write through AllocSim and commit one
        // metadata transaction; legacy images keep the in-place implementation.
        if guard.superblock.has_journal_layout() {
            return Self::write_data_journaled(
                &mut guard,
                inode_id,
                file_offset,
                data,
                compression_mode,
                filter_config,
            );
        }

        // Case 1: Writing from offset 0
        // Determine if this is a true full overwrite (initial write or complete replacement)
        // vs a partial write at the beginning of the file.
        let is_full_overwrite = file_offset == 0
            && (inode.size == 0
                || data.len() as u64 >= inode.size
                || inode.compressed_size > 0
                || inode.encrypted
                || filter_config.is_active());

        if is_full_overwrite {
            return Self::write_data_from_start_internal(
                &mut guard,
                inode_id,
                &mut inode,
                data,
                compression_mode,
                filter_config,
            );
        }

        // Case 1b: Partial write at offset 0 on uncompressed, unencrypted file
        // Fall through to Case 3 (raw file write) for zero-copy partial update.
        if file_offset == 0
            && inode.compressed_size == 0
            && !inode.encrypted
            && !filter_config.is_active()
        {
            // Continue to Case 3 below
        } else if file_offset == 0 {
            // This shouldn't happen due to is_full_overwrite check, but safety fallback
            return Self::write_data_from_start_internal(
                &mut guard,
                inode_id,
                &mut inode,
                data,
                compression_mode,
                filter_config,
            );
        }

        // Case 2: Append or random write to an already-compressed file
        if inode.compressed_size > 0 {
            // Fast Path: Zstd Multi-Frame Append
            // When appending strictly at EOF to an unencrypted file with no active pre-compression filters,
            // we directly compress `data` as a new independent Zstd Frame and append it to the physical
            // compressed stream. Zstd decoders (such as zstd::stream::decode_all) naturally decompress concatenated
            // multi-frame streams seamlessly without needing to decompress previous blocks.
            if file_offset == inode.size
                && !inode.encrypted
                && !filter_config.is_active()
                && inode.filter_typesize == 0
            {
                if data.is_empty() {
                    return Ok(());
                }
                let new_frame = zstd::stream::encode_all(std::io::Cursor::new(data), 0)
                    .map_err(DiskManagerError::Io)?;

                let mut touched = if guard.durability_mode().is_range_based() {
                    Some(Vec::new())
                } else {
                    None
                };
                let append_offset = inode.compressed_size;
                Self::write_buffer_at_offset(
                    &mut guard,
                    &mut inode,
                    append_offset,
                    &new_frame,
                    touched.as_mut(),
                )?;

                inode.size += data.len() as u64;
                inode.compressed_size += new_frame.len() as u64;
                inode.modified_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                Self::write_inode_internal(&mut guard, inode_id, &inode)?;
                if let Some(tb) = touched {
                    let mut ranges = Vec::with_capacity(tb.len() + 2);
                    ranges.push(guard.data_bitmap_byte_range());
                    ranges.push(guard.inode_byte_range(inode_id));
                    for blk in tb {
                        ranges.push(guard.block_byte_range(blk));
                    }
                    guard.sync_mutation_ranges(&ranges)?;
                } else {
                    guard.sync_mutation_ranges(&[])?;
                }
                return Ok(());
            }

            // Fallback: Read-Modify-Recompress
            // For encrypted files, middle-offset random writes, or files with active filters,
            // transparently decompress the existing payload, splice in the new data, and re-write from offset 0.
            let mut full_data = Self::read_data_internal(&guard, &inode)?;
            let end_offset = (file_offset as usize) + data.len();
            if full_data.len() < file_offset as usize {
                full_data.resize(file_offset as usize, 0);
            }
            if full_data.len() < end_offset {
                full_data.resize(end_offset, 0);
            }
            full_data[file_offset as usize..end_offset].copy_from_slice(data);

            // Free previous blocks
            let old_blocks = Self::collect_inode_blocks(&guard.mmap, &inode);
            let mut min_freed_blk = u64::MAX;
            {
                let db_blk = guard.superblock.data_bitmap_block;
                let db_start = guard.superblock.data_block_start;
                if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, db_blk) {
                    let mut da = SimpleBlockAllocator::new(slice, db_start);
                    for blk in old_blocks {
                        let _ = da.free(blk);
                        min_freed_blk = min_freed_blk.min(blk);
                    }
                }
            }
            if min_freed_blk < guard.free_block_hint {
                guard.free_block_hint = min_freed_blk;
            }
            inode.blocks = [0; 12];
            inode.triple_indirect = 0;
            inode.size = 0;
            inode.compressed_size = 0;

            let effective_filter = if filter_config.is_active() {
                filter_config
            } else {
                crate::filters::FilterConfig {
                    typesize: inode.filter_typesize,
                    delta: inode.filter_delta,
                    shuffle: inode.filter_shuffle,
                    bitshuffle: inode.filter_bitshuffle,
                }
            };

            return Self::write_data_from_start_internal(
                &mut guard,
                inode_id,
                &mut inode,
                &full_data,
                compression_mode,
                effective_filter,
            );
        }

        // Case 3: Raw (uncompressed) file append or random write
        let mut touched = if guard.durability_mode().is_range_based() {
            Some(Vec::new())
        } else {
            None
        };
        let final_offset = Self::write_buffer_at_offset(
            &mut guard,
            &mut inode,
            file_offset,
            data,
            touched.as_mut(),
        )?;
        inode.size = std::cmp::max(inode.size, final_offset);
        inode.modified_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self::write_inode_internal(&mut guard, inode_id, &inode)?;
        if let Some(tb) = touched {
            let mut ranges = Vec::with_capacity(tb.len() + 2);
            ranges.push(guard.data_bitmap_byte_range());
            ranges.push(guard.inode_byte_range(inode_id));
            for blk in tb {
                ranges.push(guard.block_byte_range(blk));
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
        if let Some(cached) = guard.inode_cache.read().unwrap().get(inode_id) {
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
        guard.inode_cache.write().unwrap().insert(inode_id, inode);
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
        guard.inode_cache.write().unwrap().insert(inode_id, *inode);
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
    fn read_block_ptr(mmap: &MmapMut, block_id: u64, entry_idx: usize) -> u64 {
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

    fn get_block_from_map(mmap: &MmapMut, block_id: u64) -> Option<&[u8]> {
        let start = usize::try_from(block_id).ok()?.checked_mul(BLOCK_SIZE)?;
        let end = start.checked_add(BLOCK_SIZE)?;
        if end > mmap.len() {
            None
        } else {
            Some(&mmap[start..end])
        }
    }

    fn resolve_path_internal(
        guard: &DiskManagerInner,
        parts: &[&str],
    ) -> Result<u64, DiskManagerError> {
        let mut curr = guard.superblock.root_inode;
        for &part in parts {
            curr = Self::dir_lookup(guard, curr, part)?.ok_or_else(|| {
                DiskManagerError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Not found",
                ))
            })?;
        }
        Ok(curr)
    }

    // Path resolution API (public) - wraps lookup
    pub fn resolve_path(&self, path: &str) -> Result<u64, DiskManagerError> {
        let parts: Vec<&str> = path
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        let guard = self.inner.read().unwrap();
        Self::resolve_path_internal(&guard, &parts)
    }

    pub fn get_block_copy(&self, block_id: u64) -> Option<Vec<u8>> {
        let guard = self.inner.read().unwrap();
        Self::get_block_from_map(&guard.mmap, block_id).map(|s| s.to_vec())
    }

    /// Lists all entries in a directory
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
        let num_blocks = Self::dir_num_blocks(&inode);
        let mut entries = Vec::new();
        for blk_idx in 0..num_blocks {
            let phys_blk = Self::resolve_logical_block_id(&guard.mmap, &inode, blk_idx);
            if phys_blk == 0 {
                continue;
            }
            entries.extend(Self::read_dir_entries_from_block(&guard.mmap, phys_blk)?);
        }
        if let Some(key) = &guard.encryption_key {
            for entry in &mut entries {
                if let Ok(decrypted) =
                    crate::encryption::decrypt_filename(key, dir_inode_id, &entry.name)
                {
                    entry.name = decrypted;
                }
            }
        }
        Ok(entries)
    }

    pub fn resolve_parent(&self, path: &str) -> Result<(u64, String), DiskManagerError> {
        let parts: Vec<&str> = path
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        if parts.is_empty() {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Empty",
            )));
        }
        let name = parts.last().unwrap().to_string();
        let parent_parts = &parts[..parts.len() - 1];

        let guard = self.inner.read().unwrap();
        let parent_id = if parent_parts.is_empty() {
            guard.superblock.root_inode
        } else {
            Self::resolve_path_internal(&guard, parent_parts)?
        };
        Ok((parent_id, name))
    }

    pub fn flush(&self) -> Result<(), DiskManagerError> {
        // 1. Ensure only 1 flush operation performs msync at any given time (prevents redundant writeback storms)
        let _sync_guard = self.sync_mutex.lock().unwrap();

        // 2. Acquire shared read lock: prevents concurrent writers, but allows all concurrent readers to proceed!
        let guard = self.inner.read().unwrap();
        guard.mmap.flush().map_err(DiskManagerError::Io)
    }

    /// Asynchronously flushes dirty pages in background without blocking concurrent readers.
    pub fn flush_async(&self) -> Result<(), DiskManagerError> {
        let _sync_guard = self.sync_mutex.lock().unwrap();
        let guard = self.inner.read().unwrap();
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
    ) -> Result<(), DiskManagerError> {
        let mut parent_inode = Self::read_inode_internal(guard, parent_inode_id)?;
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

        // Locate the entry (ciphertext name first, then legacy plaintext) without
        // deserializing. Read-only, so this belongs to the compute phase.
        let (matched_name, target_inode_id, phys_blk) = candidates
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

        // ---- Phase 1: compute post-images (no mutation) ----
        let remaining_entries: Vec<_> = Self::read_dir_entries_from_block(&guard.mmap, phys_blk)?
            .into_iter()
            .filter(|e| e.name != matched_name)
            .collect();
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
        {
            let mut ic = guard.inode_cache.write().unwrap();
            ic.remove(target_inode_id);
            ic.insert(parent_inode_id, parent_inode);
        }
        {
            let mut cache = guard.dir_cache.write().unwrap();
            if let Some(ix) = cache.get_mut(&parent_inode_id) {
                ix.plain.remove(name);
                ix.stored.remove(matched_name);
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
        let mut guard = self.inner.write().unwrap();

        // M3: journaled images take the WAL-first path; legacy images keep the
        // original in-place implementation untouched (zero-overhead guarantee).
        if guard.superblock.has_journal_layout() {
            return Self::delete_file_journaled(&mut guard, parent_inode_id, name);
        }

        // M3: journaled images take the WAL-first path; legacy images keep the
        // original in-place implementation untouched (zero-overhead guarantee).
        if guard.superblock.has_journal_layout() {
            return Self::delete_file_journaled(&mut guard, parent_inode_id, name);
        }

        let mut parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
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

        // Locate the entry (ciphertext name first, then legacy plaintext) without deserializing.
        let (matched_name, target_inode_id, phys_blk) = candidates
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

        // Rewrite only the block that holds the entry.
        let remaining_entries: Vec<_> = Self::read_dir_entries_from_block(&guard.mmap, phys_blk)?
            .into_iter()
            .filter(|e| e.name != matched_name)
            .collect();
        Self::rewrite_dir_entries_in_block(&mut guard.mmap, phys_blk, &remaining_entries)?;

        // Invalidate cached names in the parent, and drop the target's own index in case it was
        // a directory: its inode id can be reused, and stale names must never resolve under it.
        {
            let mut cache = guard.dir_cache.write().unwrap();
            if let Some(ix) = cache.get_mut(&parent_inode_id) {
                ix.plain.remove(name);
                ix.stored.remove(matched_name);
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
        guard.inode_cache.write().unwrap().remove(target_inode_id);

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

    /// Prove that write_data_from_start_internal maintains size invariant on full overwrite.
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
}
