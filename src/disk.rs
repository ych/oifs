//! Disk manager module for OIFS file system
//!
//! Provides the main interface for interacting with the file system,
//! including creating/reading files and directories, managing inodes,
//! and handling file compression.

use crate::BLOCK_SIZE;
use crate::allocator::{AllocatorError, BlockAllocator, SimpleBlockAllocator};
use crate::inode::Inode;
use crate::superblock::SuperBlock;
use memmap2::{MmapMut, MmapOptions};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::{Arc, RwLock};
use thiserror::Error;

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

/// Internal disk manager state
///
/// Contains the file handle, memory-mapped region, and superblock.
/// Protected by a Mutex for thread-safe concurrent access.
struct DiskManagerInner {
    #[allow(dead_code)]
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
    pub inode_cache: RwLock<HashMap<u64, Inode>>,
    /// Per-directory name index: dir_inode_id -> DirIndex (P3.1)
    pub dir_cache: RwLock<HashMap<u64, DirIndex>>,
    /// Durability policy governing mmap msync behavior on mutations (P3.3)
    pub durability_mode: std::sync::atomic::AtomicU8,
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
        let offset =
            self.superblock.inode_table_block as usize * BLOCK_SIZE + inode_id as usize * 256;
        (offset, 256)
    }

    #[inline]
    pub fn block_byte_range(&self, block_id: u64) -> (usize, usize) {
        (block_id as usize * BLOCK_SIZE, BLOCK_SIZE)
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
}

impl DiskManager {
    /// Open an existing OIFS image or create a new one if it doesn't exist.
    /// `size`: Total size in bytes (only used when creating a new file).
    pub fn open<P: AsRef<Path>>(path: P, total_size: u64) -> Result<Self, DiskManagerError> {
        Self::init_or_open(path, total_size, None, false)
    }

    /// Open an encrypted OIFS image with a password
    pub fn open_with_password<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: Option<&str>,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(path, total_size, password, false)
    }

    /// Create a new encrypted filesystem
    pub fn create_encrypted<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: &str,
    ) -> Result<Self, DiskManagerError> {
        Self::init_or_open(path, total_size, Some(password), true)
    }

    fn init_or_open<P: AsRef<Path>>(
        path: P,
        total_size: u64,
        password: Option<&str>,
        create_encrypted: bool,
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
            let mut sb = SuperBlock::new(block_count);
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

        let free_block_hint = superblock.data_block_start;
        let inner = DiskManagerInner {
            file,
            mmap,
            superblock,
            encryption_key,
            free_block_hint,
            free_inode_hint: 0,
            inode_cache: RwLock::new(HashMap::with_capacity(1024)),
            dir_cache: RwLock::new(HashMap::new()),
            durability_mode: std::sync::atomic::AtomicU8::new(DurabilityMode::Lazy as u8),
        };

        let dm = Self {
            inner: Arc::new(RwLock::new(inner)),
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
        }

        Ok(dm)
    }

    // Accessor for SuperBlock (Copy)
    pub fn superblock(&self) -> SuperBlock {
        self.inner.read().unwrap().superblock
    }

    // Private helper for Inner
    fn get_block_mut_from_map(mmap: &mut MmapMut, block_id: u64) -> Option<&mut [u8]> {
        let start = block_id as usize * BLOCK_SIZE;
        let end = start + BLOCK_SIZE;
        if end > mmap.len() {
            None
        } else {
            Some(&mut mmap[start..end])
        }
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

    fn create_entry_internal(
        &self,
        parent_inode_id: u64,
        name: &str,
        file_type: crate::inode::FileType,
    ) -> Result<u64, DiskManagerError> {
        let mut guard = self.inner.write().unwrap();

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
        let mut read = 0;
        let mut blk_idx = 0;

        while read < physical_size {
            let rem = (physical_size - read) as usize;
            let to_read = std::cmp::min(rem, BLOCK_SIZE);

            // 1. Direct blocks (0..10)
            if blk_idx < 10 {
                let blk = inode.blocks[blk_idx];
                if blk != 0 {
                    if let Some(slice) = Self::get_block_from_map(&guard.mmap, blk) {
                        raw_data[read as usize..read as usize + to_read]
                            .copy_from_slice(&slice[..to_read]);
                    }
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
                if let Some(sib_slice) = Self::get_block_from_map(&guard.mmap, sib_id) {
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
                            if let Some(slice) = Self::get_block_from_map(&guard.mmap, blk) {
                                raw_data[read as usize..read as usize + to_read_curr]
                                    .copy_from_slice(&slice[..to_read_curr]);
                            }
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

                let sib_id = Self::read_block_ptr(&guard.mmap, dib_id, s_idx);
                if sib_id == 0 {
                    let skip_blocks = 512 - d_idx;
                    let skip_bytes =
                        (skip_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx += skip_blocks;
                    continue;
                }

                if let Some(sib_slice) = Self::get_block_from_map(&guard.mmap, sib_id) {
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
                            if let Some(slice) = Self::get_block_from_map(&guard.mmap, blk) {
                                raw_data[read as usize..read as usize + to_read_curr]
                                    .copy_from_slice(&slice[..to_read_curr]);
                            }
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

                let dib_id = Self::read_block_ptr(&guard.mmap, tib_id, t_idx);
                if dib_id == 0 {
                    let skip_blocks = (512 * 512) - rem_idx;
                    let skip_bytes =
                        (skip_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx += skip_blocks;
                    continue;
                }

                let sib_id = Self::read_block_ptr(&guard.mmap, dib_id, d_idx);
                if sib_id == 0 {
                    let skip_blocks = 512 - s_idx;
                    let skip_bytes =
                        (skip_blocks as u64 * BLOCK_SIZE as u64).min(physical_size - read);
                    read += skip_bytes;
                    blk_idx += skip_blocks;
                    continue;
                }

                let blk = Self::read_block_ptr(&guard.mmap, sib_id, s_idx);
                if blk != 0 {
                    if let Some(slice) = Self::get_block_from_map(&guard.mmap, blk) {
                        raw_data[read as usize..read as usize + to_read]
                            .copy_from_slice(&slice[..to_read]);
                    }
                }
                read += to_read as u64;
                blk_idx += 1;
                continue;
            }

            break;
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
    /// For uncompressed, unencrypted files, this directly copies the requested
    /// byte slice from the memory-mapped blocks into `buf` with **ZERO intermediate
    /// memory allocations**.
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
            let full_data = Self::read_data_internal(&guard, &inode)?;
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
            if blk_id != 0 {
                if let Some(slice) = Self::get_block_from_map(&guard.mmap, blk_id) {
                    buf[bytes_read..bytes_read + chunk_len]
                        .copy_from_slice(&slice[in_blk_offset..in_blk_offset + chunk_len]);
                } else {
                    buf[bytes_read..bytes_read + chunk_len].fill(0);
                }
            } else {
                buf[bytes_read..bytes_read + chunk_len].fill(0);
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

        // Case 1: Writing from offset 0 (initial write or complete overwrite)
        if file_offset == 0 {
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
    fn read_inode_internal(
        guard: &DiskManagerInner,
        inode_id: u64,
    ) -> Result<Inode, DiskManagerError> {
        // Fast path: check in-memory inode cache (P2.2)
        if let Some(cached) = guard.inode_cache.read().unwrap().get(&inode_id) {
            return Ok(*cached);
        }

        let inode_size = 256u64;
        let offset = guard.superblock.inode_table_block * BLOCK_SIZE as u64 + inode_id * inode_size;
        if offset + inode_size > guard.mmap.len() as u64 {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Bounds",
            )));
        }
        let slice = &guard.mmap[offset as usize..(offset + inode_size) as usize];
        let inode: Inode = bincode::deserialize(slice)?;

        let mut cache = guard.inode_cache.write().unwrap();
        if cache.len() >= 2048 {
            cache.clear();
        }
        cache.insert(inode_id, inode);
        Ok(inode)
    }

    fn write_inode_internal(
        guard: &mut DiskManagerInner,
        inode_id: u64,
        inode: &Inode,
    ) -> Result<(), DiskManagerError> {
        let inode_size = 256usize;
        let offset =
            guard.superblock.inode_table_block * BLOCK_SIZE as u64 + inode_id * inode_size as u64;
        let slice = &mut guard.mmap[offset as usize..(offset + inode_size as u64) as usize];
        let bytes = bincode::serialize(inode)?;
        if bytes.len() > inode_size {
            return Err(DiskManagerError::Serialization(Box::new(
                bincode::ErrorKind::SizeLimit,
            )));
        }
        slice[..bytes.len()].copy_from_slice(&bytes);

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
        let start = block_id as usize * BLOCK_SIZE;
        let end = start + BLOCK_SIZE;
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
        let guard = self.inner.write().unwrap();
        guard.mmap.flush().map_err(DiskManagerError::Io)
    }

    pub fn delete_file(&self, parent_inode_id: u64, name: &str) -> Result<(), DiskManagerError> {
        let mut guard = self.inner.write().unwrap();

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
        guard.inode_cache.write().unwrap().remove(&target_inode_id);

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
}
