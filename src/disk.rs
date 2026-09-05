//! Disk manager module for OIFS file system
//!
//! Provides the main interface for interacting with the file system,
//! including creating/reading files and directories, managing inodes,
//! and handling file compression.

use std::fs::{File, OpenOptions};
use std::path::Path;
use memmap2::{MmapMut, MmapOptions};
use thiserror::Error;
use crate::superblock::SuperBlock;
use crate::BLOCK_SIZE;
use crate::allocator::{SimpleBlockAllocator, BlockAllocator, AllocatorError};
use crate::inode::Inode;
use std::sync::{Arc, Mutex};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[derive(Default)]
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
}

impl Drop for DiskManagerInner {
    fn drop(&mut self) {
        // Flush any pending changes to disk when dropped
        let _ = self.mmap.flush();
    }
}

/// Main disk manager interface for the OIFS file system
///
/// Provides thread-safe access to the file system through an Arc<Mutex<>> wrapper.
/// Supports:
/// - File and directory creation/deletion
/// - File reading/writing with optional zstd compression
/// - Path resolution and directory listing
/// - Concurrent access from multiple threads
#[derive(Clone)]
pub struct DiskManager {
    inner: Arc<Mutex<DiskManagerInner>>,
}

// Ensure Send + Sync (Mutex provides this if contents are Send)
// File is Send+Sync. SuperBlock is Send+Sync. MmapMut is Send+Sync on linux (usually). 
// Actually MmapMut is Send but !Sync.
// Mutex<T> is Sync if T is Send. MmapMut is Send. So Mutex<MmapMut> is Sync.
// So Arc<Mutex<DiskManagerInner>> is Send + Sync. Correct.

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

        use nix::fcntl::{fcntl, FcntlArg};
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

        let inner = DiskManagerInner {
            file,
            mmap,
            superblock,
            encryption_key,
        };

        let dm = Self {
            inner: Arc::new(Mutex::new(inner)),
        };

        if is_new {
            let mut guard = dm.inner.lock().unwrap();
            let inode_bitmap_block = guard.superblock.inode_bitmap_block;
            let bitmap_slice = Self::get_block_mut_from_map(&mut guard.mmap, inode_bitmap_block)
                .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("Failed to get inode bitmap")))?;
            let mut ia = SimpleBlockAllocator::new(bitmap_slice, 0);
            let root_id = ia.allocate()?;
            if root_id != 0 {
                return Err(DiskManagerError::Io(std::io::Error::other("Failed init root inode")));
            }

            let data_bitmap_block = guard.superblock.data_bitmap_block;
            let data_start = guard.superblock.data_block_start;
            let data_slice = Self::get_block_mut_from_map(&mut guard.mmap, data_bitmap_block)
                .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("Failed to get data bitmap")))?;
            let mut da = SimpleBlockAllocator::new(data_slice, data_start);
            let root_data = da.allocate()?;

            let mut root_inode = Inode::new(crate::inode::FileType::Directory);
            root_inode.blocks[0] = root_data;
            Self::write_inode_internal(&mut guard, 0, &root_inode)?;
            guard.mmap.flush()?;
        }

        Ok(dm)
    }

    // Accessor for SuperBlock (Copy)
    pub fn superblock(&self) -> SuperBlock {
        self.inner.lock().unwrap().superblock
    }

    // Private helper for Inner
    fn get_block_mut_from_map(mmap: &mut MmapMut, block_id: u64) -> Option<&mut [u8]> {
        let start = block_id as usize * BLOCK_SIZE;
        let end = start + BLOCK_SIZE;
        if end > mmap.len() { None } else { Some(&mut mmap[start..end]) }
    }
    
    // We can't expose MmapMut directly.
    // We can't expose Allocator that holds ref to mmap directly outside of a closure or short life.
    // The previous design `dm.inode_allocator()` returned a struct borrowing `dm`. 
    // Now `dm` is `Arc<Mutex<>>`. `inode_allocator` would need to lock it.
    // `SimpleBlockAllocator` borrows slice. Slice borrows `MutexGuard`?
    // `SimpleBlockAllocator<'a>` where 'a is lifetime of Guard.
    
    // So:
    // pub fn with_inode_allocator<F>(&self, f: F) -> Result<(), Error> where F: FnOnce(&mut Allocator)
    // Or just keep internal logic hidden.
    
    // Let's implement high level ops directly on DiskManager using internal locking.

    /// Reads an inode from the inode table
    pub fn read_inode(&self, inode_id: u64) -> Result<Inode, DiskManagerError> {
        let guard = self.inner.lock().unwrap();
        Self::read_inode_internal(&guard, inode_id)
    }

    /// Writes an inode to the inode table
    pub fn write_inode(&self, inode_id: u64, inode: &Inode) -> Result<(), DiskManagerError> {
        let mut guard = self.inner.lock().unwrap();
        Self::write_inode_internal(&mut guard, inode_id, inode)
    }

    fn find_dir_entry_in_block(mmap: &MmapMut, block_id: u64, name: &str) -> Result<Option<u64>, DiskManagerError> {
        if let Some(slice) = Self::get_block_from_map(mmap, block_id) {
            return Ok(crate::directory::find_entry_in_block(slice, name));
        }
        Ok(None)
    }

    fn read_dir_entries_from_block(mmap: &MmapMut, block_id: u64) -> Result<Vec<crate::directory::DirectoryEntry>, DiskManagerError> {
        if let Some(slice) = Self::get_block_from_map(mmap, block_id) {
            let iter = crate::directory::DirectoryIterator::new(slice);
            let mut entries = Vec::new();
            for entry in iter {
                entries.push(entry.map_err(|e| DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))?);
            }
            Ok(entries)
        } else {
            Ok(Vec::new())
        }
    }

    fn append_dir_entry_to_block(mmap: &mut MmapMut, block_id: u64, entry: &crate::directory::DirectoryEntry) -> Result<(), DiskManagerError> {
        let block_slice = Self::get_block_mut_from_map(mmap, block_id)
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("Directory block not found")))?;

        let insert_offset = crate::directory::find_insert_offset_in_block(block_slice);

        if insert_offset + 20 + entry.name.len() > BLOCK_SIZE {
            return Err(DiskManagerError::Io(std::io::Error::other("Directory block is full")));
        }

        let mut cursor = std::io::Cursor::new(block_slice);
        cursor.set_position(insert_offset as u64);
        entry.serialize_into(&mut cursor).map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
        Ok(())
    }

    fn rewrite_dir_entries_in_block(mmap: &mut MmapMut, block_id: u64, entries: &[crate::directory::DirectoryEntry]) -> Result<(), DiskManagerError> {
        let block_slice = Self::get_block_mut_from_map(mmap, block_id)
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("Directory block not found")))?;
        block_slice.fill(0);
        let mut cursor = std::io::Cursor::new(block_slice);
        for entry in entries {
            entry.serialize_into(&mut cursor).map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
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
                for idx in 0..512 {
                    let start = idx * 8;
                    let mut blk_bytes = [0u8; 8];
                    blk_bytes.copy_from_slice(&slice[start..start + 8]);
                    let blk = u64::from_le_bytes(blk_bytes);
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
            if let Some(slice) = Self::get_block_from_map(mmap, dib_id) {
                for s_idx in 0..512 {
                    let s_start = s_idx * 8;
                    let mut sib_bytes = [0u8; 8];
                    sib_bytes.copy_from_slice(&slice[s_start..s_start + 8]);
                    let sib = u64::from_le_bytes(sib_bytes);
                    if sib != 0 {
                        blks.push(sib);
                        if let Some(s_slice) = Self::get_block_from_map(mmap, sib) {
                            for d_idx in 0..512 {
                                let d_start = d_idx * 8;
                                let mut blk_bytes = [0u8; 8];
                                blk_bytes.copy_from_slice(&s_slice[d_start..d_start + 8]);
                                let blk = u64::from_le_bytes(blk_bytes);
                                if blk != 0 {
                                    blks.push(blk);
                                }
                            }
                        }
                    }
                }
            }
        }

        blks
    }

    /// Creates a new file in a directory
    pub fn create_file(&self, parent_inode_id: u64, name: &str) -> Result<u64, DiskManagerError> {
        let mut guard = self.inner.lock().unwrap();

        // 1. Read Parent
        let parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
        }

        let dir_block_id = parent_inode.blocks[0];
        if dir_block_id == 0 {
            return Err(DiskManagerError::Io(std::io::Error::other("No block")));
        }

        // Check if file already exists
        if let Some(_existing) = Self::find_dir_entry_in_block(&guard.mmap, dir_block_id, name)? {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("File '{}' already exists", name),
            )));
        }

        // 2. Allocate Inode
        let inode_bitmap = guard.superblock.inode_bitmap_block;
        let bitmap_slice = Self::get_block_mut_from_map(&mut guard.mmap, inode_bitmap).unwrap();
        let mut allocator = SimpleBlockAllocator::new(bitmap_slice, 0);
        let file_inode_id = allocator.allocate()?;

        // 3. Init Inode
        let mut file_inode = Inode::new(crate::inode::FileType::File);
        file_inode.modified_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        Self::write_inode_internal(&mut guard, file_inode_id, &file_inode)?;

        // 4. Update Parent Dir
        let entry = crate::directory::DirectoryEntry {
            inode: file_inode_id,
            hash: 0,
            name: name.to_string(),
        };
        Self::append_dir_entry_to_block(&mut guard.mmap, dir_block_id, &entry)?;

        // 5. Update Parent Mtime
        let mut parent_inode = parent_inode;
        parent_inode.modified_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        Self::write_inode_internal(&mut guard, parent_inode_id, &parent_inode)?;

        guard.mmap.flush()?;
        Ok(file_inode_id)
    }

    /// Creates a new directory in a parent directory
    pub fn create_directory(&self, parent_inode_id: u64, name: &str) -> Result<u64, DiskManagerError> {
        let mut guard = self.inner.lock().unwrap();

        let parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
        }

        let dir_block_id = parent_inode.blocks[0];
        if dir_block_id == 0 {
            return Err(DiskManagerError::Io(std::io::Error::other("No block")));
        }

        if let Some(_existing) = Self::find_dir_entry_in_block(&guard.mmap, dir_block_id, name)? {
            return Err(DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("Directory '{}' already exists", name),
            )));
        }

        // Alloc Inode
        let inode_bitmap = guard.superblock.inode_bitmap_block;
        let bitmap_slice = Self::get_block_mut_from_map(&mut guard.mmap, inode_bitmap).unwrap();
        let mut ia = SimpleBlockAllocator::new(bitmap_slice, 0);
        let dir_inode_id = ia.allocate()?;

        // Alloc Data
        let data_bitmap = guard.superblock.data_bitmap_block;
        let data_start = guard.superblock.data_block_start;
        let data_slice = Self::get_block_mut_from_map(&mut guard.mmap, data_bitmap).unwrap();
        let mut da = SimpleBlockAllocator::new(data_slice, data_start);
        let dir_data_block = da.allocate()?;

        // Init Inode
        let mut dir_inode = Inode::new(crate::inode::FileType::Directory);
        dir_inode.modified_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        dir_inode.blocks[0] = dir_data_block;
        Self::write_inode_internal(&mut guard, dir_inode_id, &dir_inode)?;

        // Add to Parent
        let entry = crate::directory::DirectoryEntry {
            inode: dir_inode_id,
            hash: 0,
            name: name.to_string(),
        };
        Self::append_dir_entry_to_block(&mut guard.mmap, dir_block_id, &entry)?;

        let mut parent_inode = parent_inode;
        parent_inode.modified_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        Self::write_inode_internal(&mut guard, parent_inode_id, &parent_inode)?;

        guard.mmap.flush()?;
        Ok(dir_inode_id)
    }

    /// Looks up a file/directory by name within a parent directory
    pub fn lookup(&self, parent_inode_id: u64, name: &str) -> Result<u64, DiskManagerError> {
        let guard = self.inner.lock().unwrap();
        let parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not dir")));
        }

        let dir_block_id = parent_inode.blocks[0];
        if dir_block_id == 0 {
            return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "Not found")));
        }

        Self::find_dir_entry_in_block(&guard.mmap, dir_block_id, name)?
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "Not found")))
    }

    /// Reads data from a file
    ///
    /// # Arguments
    /// * `inode_id` - The inode ID of the file to read
    ///
    /// # Returns
    /// The file's data as a Vec<u8>. If the file is compressed, it will be
    /// automatically decompressed before returning.
    ///
    /// # Compression Handling
    /// - If `compressed_size > 0`: Read compressed data and decompress using zstd
    /// - If `compressed_size == 0`: Read and return raw data
    pub fn read_data(&self, inode_id: u64) -> Result<Vec<u8>, DiskManagerError> {
        let mut guard = self.inner.lock().unwrap();
        let inode = Self::read_inode_internal(&guard, inode_id)?;
        
        // Determine physical size on disk
        // If file is compressed, use compressed_size; otherwise use logical size
        let physical_size = if inode.compressed_size > 0 { inode.compressed_size } else { inode.size };
        let mut raw_data = Vec::with_capacity(physical_size as usize);
        let mut read = 0;
        
        let mut inode_clone = inode;
        let mut blk_idx = 0;
        while read < physical_size {
            let blk = Self::get_or_alloc_block(&mut guard, &mut inode_clone, blk_idx, false)?;
            let rem = (physical_size - read) as usize;
            let to_read = std::cmp::min(rem, BLOCK_SIZE);
            if blk == 0 {
                raw_data.extend(std::iter::repeat_n(0u8, to_read));
                read += to_read as u64;
            } else if let Some(slice) = Self::get_block_from_map(&guard.mmap, blk) {
                raw_data.extend_from_slice(&slice[..to_read]);
                read += to_read as u64;
            } else {
                break;
            }
            blk_idx += 1;
        }
        
        // === DECRYPTION STEP ===
        // Decrypt before decompression (if file is encrypted)
        let mut decrypted_data = raw_data;
        if inode.encrypted {
            let encryption_key = guard.encryption_key.as_ref()
                .ok_or(DiskManagerError::PasswordRequired)?;
            
            // Decrypt using stored nonce
            decrypted_data = crate::encryption::decrypt_data(
                &decrypted_data,
                encryption_key,
                &inode.encryption_nonce
            ).map_err(|_| DiskManagerError::DecryptionFailed)?;
        }
        
        // Decompress if this is a compressed file
        if inode.mode == crate::inode::FileType::File && inode.compressed_size > 0 {
             let decoded = zstd::stream::decode_all(std::io::Cursor::new(&decrypted_data))
                 .map_err(DiskManagerError::Io)?;
             decrypted_data = decoded;
        }

        // === POST-DECOMPRESSION FILTER STEP ===
        // Reverse the filter pipeline: Unshuffle -> Undelta
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

    /// Writes data to a file (default: no pre-compression filters)
    ///
    /// For custom filter pipeline (Delta / Shuffle / typesize), use [`write_data_with_filters`].
    pub fn write_data(&self, inode_id: u64, file_offset: u64, data: &[u8], compression_mode: CompressionMode) -> Result<(), DiskManagerError> {
        self.write_data_with_filters(inode_id, file_offset, data, compression_mode, crate::filters::FilterConfig::none())
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
    /// # Data Pipeline (Write)
    /// ```text
    /// Raw Data → [Delta Encode] → [Byte Shuffle] → [Zstd Compress] → [Encrypt] → Disk
    /// ```
    ///
    /// # Compression Strategy
    /// - `Always`: Always compress regardless of size
    /// - `Never`: Never compress
    /// - `Auto`: Compress files >= 8KB if beneficial
    ///
    /// # Limitations
    /// - Maximum file size: 48KB (12 blocks × 4KB)
    /// - Cannot append to already-compressed files (offset > 0)
    /// - Exceeding 48KB returns `FileTooLarge` error
    pub fn write_data_with_filters(&self, inode_id: u64, file_offset: u64, data: &[u8], compression_mode: CompressionMode, filter_config: crate::filters::FilterConfig) -> Result<(), DiskManagerError> {
        let mut guard = self.inner.lock().unwrap();
        let mut inode = Self::read_inode_internal(&guard, inode_id)?;
        
        // === PRE-COMPRESSION FILTER STEP ===
        // Apply filters (Delta -> Shuffle) before compression for better entropy reduction
        let filtered_data = crate::filters::apply_filters_cow(data, &filter_config);
        let working_data: &[u8] = &filtered_data;

        let final_data: std::borrow::Cow<[u8]>;
        let mut is_compressed = false;

        // Determine if we should compress based on mode
        let should_compress = match compression_mode {
            CompressionMode::Always => true,
            CompressionMode::Never => false,
            CompressionMode::Auto => working_data.len() >= 8192,
        };

        // Attempt compression for files written from start
        if inode.mode == crate::inode::FileType::File && file_offset == 0 && should_compress {
            let compressed = zstd::stream::encode_all(std::io::Cursor::new(working_data), 0)
                .map_err(DiskManagerError::Io)?;
            
            // Decision logic based on compression mode
            match compression_mode {
                CompressionMode::Always => {
                    // Always use compression, even if it increases size
                    // (User may want this for privacy - to prevent hexdump visibility)
                    final_data = std::borrow::Cow::Owned(compressed);
                    is_compressed = true;
                }
                CompressionMode::Auto => {
                    // Only use compression if it reduces size
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
            // Handle non-compressed writes (small files, directories, appends)
            if inode.mode == crate::inode::FileType::File && inode.compressed_size > 0 {
                // Prevent appending to already-compressed files
                // (would require decompress-modify-recompress sequence)
                return Err(DiskManagerError::Io(std::io::Error::other("Cannot append to compressed file")));
            }
            final_data = filtered_data;
        }

        // === ENCRYPTION STEP ===
        // Encrypt after compression (if encryption key available)
        let final_encrypted: Vec<u8>;
        let write_buffer: &[u8] = if let Some(encryption_key) = &guard.encryption_key {
            // Generate unique nonce
            let nonce = crate::encryption::generate_nonce();
            
            // Encrypt (possibly compressed) data
            final_encrypted = crate::encryption::encrypt_data(
                final_data.as_ref(),
                encryption_key,
                &nonce
            )?;
            
            // Mark inode as encrypted
            inode.encrypted = true;
            inode.encryption_nonce = nonce;
            
            &final_encrypted
        } else {
            final_data.as_ref()
        };
        let mut written = 0;
        let mut current_offset = file_offset;
        
        while written < write_buffer.len() {
            let blk_idx = (current_offset / BLOCK_SIZE as u64) as usize;
            let blk_id = Self::get_or_alloc_block(&mut guard, &mut inode, blk_idx, true)?;
            
            let in_blk_off = (current_offset % BLOCK_SIZE as u64) as usize;
            let to_write = std::cmp::min(write_buffer.len() - written, BLOCK_SIZE - in_blk_off);
            
            if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, blk_id) {
                slice[in_blk_off..in_blk_off+to_write].copy_from_slice(&write_buffer[written..written+to_write]);
            }
            written += to_write;
            current_offset += to_write as u64;
        }
        
        if is_compressed {
            inode.size = data.len() as u64; // Logical
            inode.compressed_size = write_buffer.len() as u64; // Physical
        } else {
            // If append mode (offset > 0)
            inode.size = std::cmp::max(inode.size, current_offset);
            // inode.compressed_size stays 0 (Raw)
        }

        // Store filter metadata in inode for correct reverse-filtering on read
        inode.filter_typesize = filter_config.typesize;
        inode.filter_delta = filter_config.delta;
        inode.filter_shuffle = filter_config.shuffle;
        inode.filter_bitshuffle = filter_config.bitshuffle;

        inode.modified_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        Self::write_inode_internal(&mut guard, inode_id, &inode)?;
        
        // Explicit sync for metadata update
        guard.mmap.flush()?;
        Ok(())
    }
    
    // Internal Helpers working on guards
    fn read_inode_internal(guard: &DiskManagerInner, inode_id: u64) -> Result<Inode, DiskManagerError> {
        let inode_size = 256u64;
        let offset = guard.superblock.inode_table_block * BLOCK_SIZE as u64 + inode_id * inode_size;
        if offset + inode_size > guard.mmap.len() as u64 { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Bounds"))); }
        let slice = &guard.mmap[offset as usize .. (offset+inode_size) as usize];
        Ok(bincode::deserialize(slice)?)
    }
    
    fn write_inode_internal(guard: &mut DiskManagerInner, inode_id: u64, inode: &Inode) -> Result<(), DiskManagerError> {
        let inode_size = 256usize;
        let offset = guard.superblock.inode_table_block * BLOCK_SIZE as u64 + inode_id * inode_size as u64;
        let slice = &mut guard.mmap[offset as usize .. (offset+inode_size as u64) as usize];
        let bytes = bincode::serialize(inode)?;
        if bytes.len() > inode_size { return Err(DiskManagerError::Serialization(Box::new(bincode::ErrorKind::SizeLimit))); }
        slice[..bytes.len()].copy_from_slice(&bytes);
        Ok(())
    }

    fn allocate_block(guard: &mut DiskManagerInner) -> Result<u64, DiskManagerError> {
        let db_blk = guard.superblock.data_bitmap_block;
        let db_start = guard.superblock.data_block_start;
        let slice = Self::get_block_mut_from_map(&mut guard.mmap, db_blk).unwrap();
        let mut da = SimpleBlockAllocator::new(slice, db_start);
        da.allocate().map_err(DiskManagerError::Allocator)
    }

    fn get_or_alloc_block(
        guard: &mut DiskManagerInner,
        inode: &mut Inode,
        logical_block_idx: usize,
        allocate: bool,
    ) -> Result<u64, DiskManagerError> {
        // Direct block case
        if logical_block_idx < 10 {
            let mut blk_id = inode.blocks[logical_block_idx];
            if blk_id == 0 && allocate {
                blk_id = Self::allocate_block(guard)?;
                inode.blocks[logical_block_idx] = blk_id;
            }
            return Ok(blk_id);
        }

        // Single Indirect block case (indices 10..522)
        if logical_block_idx < 10 + 512 {
            let idx = logical_block_idx - 10;
            let mut sib_id = inode.blocks[10];
            if sib_id == 0 {
                if !allocate { return Ok(0); }
                sib_id = Self::allocate_block(guard)?;
                inode.blocks[10] = sib_id;
                
                // Zero out the newly allocated single indirect block
                if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, sib_id) {
                    slice.fill(0);
                }
            }

            let start = idx * 8;
            let mut blk_id = 0;
            
            // Read existing pointer using immutable borrow
            if let Some(slice) = Self::get_block_from_map(&guard.mmap, sib_id) {
                let mut blk_bytes = [0u8; 8];
                blk_bytes.copy_from_slice(&slice[start..start+8]);
                blk_id = u64::from_le_bytes(blk_bytes);
            }

            // Allocate and write back if needed
            if blk_id == 0 && allocate {
                blk_id = Self::allocate_block(guard)?;
                if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, sib_id) {
                    slice[start..start+8].copy_from_slice(&blk_id.to_le_bytes());
                }
            }
            return Ok(blk_id);
        }

        // Double Indirect block case (indices 522..262666)
        if logical_block_idx < 10 + 512 + 512 * 512 {
            let idx = logical_block_idx - (10 + 512);
            let s_idx = idx / 512;
            let d_idx = idx % 512;

            let mut dib_id = inode.blocks[11];
            if dib_id == 0 {
                if !allocate { return Ok(0); }
                dib_id = Self::allocate_block(guard)?;
                inode.blocks[11] = dib_id;
                
                // Zero out the newly allocated double indirect block
                if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, dib_id) {
                    slice.fill(0);
                }
            }

            // Get or allocate single indirect block within double indirect block
            let mut sib_id = 0;
            let s_start = s_idx * 8;
            
            // Read sib_id immutably
            if let Some(slice) = Self::get_block_from_map(&guard.mmap, dib_id) {
                let mut sib_bytes = [0u8; 8];
                sib_bytes.copy_from_slice(&slice[s_start..s_start+8]);
                sib_id = u64::from_le_bytes(sib_bytes);
            }

            if sib_id == 0 {
                if !allocate { return Ok(0); }
                sib_id = Self::allocate_block(guard)?;
                if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, dib_id) {
                    slice[s_start..s_start+8].copy_from_slice(&sib_id.to_le_bytes());
                }
                
                // Zero out the newly allocated single indirect block
                if let Some(s_slice) = Self::get_block_mut_from_map(&mut guard.mmap, sib_id) {
                    s_slice.fill(0);
                }
            }

            // Get or allocate data block within single indirect block
            let d_start = d_idx * 8;
            let mut blk_id = 0;
            
            // Read blk_id immutably
            if let Some(slice) = Self::get_block_from_map(&guard.mmap, sib_id) {
                let mut blk_bytes = [0u8; 8];
                blk_bytes.copy_from_slice(&slice[d_start..d_start+8]);
                blk_id = u64::from_le_bytes(blk_bytes);
            }

            if blk_id == 0 && allocate {
                blk_id = Self::allocate_block(guard)?;
                if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, sib_id) {
                    slice[d_start..d_start+8].copy_from_slice(&blk_id.to_le_bytes());
                }
            }
            return Ok(blk_id);
        }

        // Limit exceeded
        Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::FileTooLarge, "File too large (max 1GB)")))
    }
    
    fn get_block_from_map(mmap: &MmapMut, block_id: u64) -> Option<&[u8]> {
        let start = block_id as usize * BLOCK_SIZE;
        let end = start + BLOCK_SIZE;
        if end > mmap.len() { None } else { Some(&mmap[start..end]) }
    }
    
    fn resolve_path_internal(guard: &DiskManagerInner, parts: &[&str]) -> Result<u64, DiskManagerError> {
        let mut curr = guard.superblock.root_inode;
        for &part in parts {
            let parent = Self::read_inode_internal(guard, curr)?;
            if parent.mode != crate::inode::FileType::Directory {
                return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, "Not dir")));
            }
            let blk = parent.blocks[0];
            if blk == 0 {
                return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "Not found")));
            }

            if let Some(slice) = Self::get_block_from_map(&guard.mmap, blk) {
                if let Some(inode_id) = crate::directory::find_entry_in_block(slice, part) {
                    curr = inode_id;
                    continue;
                }
            }
            return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "Not found")));
        }
        Ok(curr)
    }

    // Path resolution API (public) - wraps lookup
    pub fn resolve_path(&self, path: &str) -> Result<u64, DiskManagerError> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
        let guard = self.inner.lock().unwrap();
        Self::resolve_path_internal(&guard, &parts)
    }

    pub fn get_block_copy(&self, block_id: u64) -> Option<Vec<u8>> {
        let guard = self.inner.lock().unwrap();
        Self::get_block_from_map(&guard.mmap, block_id).map(|s| s.to_vec())
    }

    /// Lists all entries in a directory
    pub fn list_dir(&self, dir_inode_id: u64) -> Result<Vec<crate::directory::DirectoryEntry>, DiskManagerError> {
        let guard = self.inner.lock().unwrap();
        let inode = Self::read_inode_internal(&guard, dir_inode_id)?;
        if inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not a directory")));
        }
        let block_id = inode.blocks[0];
        if block_id == 0 {
            return Ok(Vec::new());
        }
        Self::read_dir_entries_from_block(&guard.mmap, block_id)
    }

    pub fn resolve_parent(&self, path: &str) -> Result<(u64, String), DiskManagerError> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
        if parts.is_empty() {
            return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Empty")));
        }
        let name = parts.last().unwrap().to_string();
        let parent_parts = &parts[..parts.len() - 1];

        let guard = self.inner.lock().unwrap();
        let parent_id = if parent_parts.is_empty() {
            guard.superblock.root_inode
        } else {
            Self::resolve_path_internal(&guard, parent_parts)?
        };
        Ok((parent_id, name))
    }

    pub fn flush(&self) -> Result<(), DiskManagerError> {
        let guard = self.inner.lock().unwrap();
        guard.mmap.flush().map_err(DiskManagerError::Io)
    }

    pub fn delete_file(&self, parent_inode_id: u64, name: &str) -> Result<(), DiskManagerError> {
        let mut guard = self.inner.lock().unwrap();

        let parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not a directory")));
        }

        let dir_block_id = parent_inode.blocks[0];
        if dir_block_id == 0 {
            return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "File not found")));
        }

        let entries = Self::read_dir_entries_from_block(&guard.mmap, dir_block_id)?;
        let mut remaining_entries = Vec::new();
        let mut target_inode = None;

        for entry in entries {
            if entry.name == name {
                target_inode = Some(entry.inode);
            } else {
                remaining_entries.push(entry);
            }
        }

        let target_inode_id = target_inode
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "File not found")))?;

        // 2. Rewrite Directory Block
        Self::rewrite_dir_entries_in_block(&mut guard.mmap, dir_block_id, &remaining_entries)?;
        // 3. Free Inode & Blocks
        let file_inode = Self::read_inode_internal(&guard, target_inode_id)?;
        let blocks_to_free = Self::collect_inode_blocks(&guard.mmap, &file_inode);

        // Free Data Blocks
        {
            let db_blk = guard.superblock.data_bitmap_block;
            let db_start = guard.superblock.data_block_start;
            let slice = Self::get_block_mut_from_map(&mut guard.mmap, db_blk).unwrap();
            let mut da = SimpleBlockAllocator::new(slice, db_start);

            for blk in blocks_to_free {
                da.free(blk)?;
            }
        }

        // Free Inode
        {
            let ib_blk = guard.superblock.inode_bitmap_block;
            let slice = Self::get_block_mut_from_map(&mut guard.mmap, ib_blk).unwrap();
            let mut ia = SimpleBlockAllocator::new(slice, 0);
            ia.free(target_inode_id)?;
        }

        // Update Parent Mtime
        let mut parent_inode = parent_inode;
        parent_inode.modified_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        Self::write_inode_internal(&mut guard, parent_inode_id, &parent_inode)?;

        // Explicit sync
        guard.mmap.flush()?;
        Ok(())
    }

    /// Analyzes disk fragmentation and returns statistics
    ///
    /// Scans the data block bitmap to calculate fragmentation metrics.
    ///
    /// # Returns
    /// `FragmentationStats` containing detailed fragmentation information
    pub fn analyze_fragmentation(&self) -> Result<FragmentationStats, DiskManagerError> {
        let guard = self.inner.lock().unwrap();
        let sb = &guard.superblock;
        
        // Get data bitmap block
        let bitmap_block = sb.data_bitmap_block;
        let bitmap_slice = Self::get_block_from_map(&guard.mmap, bitmap_block)
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::other("Bitmap not found")))?;
        
        // Create mutable copy for Bitmap analysis (it requires &mut but we only read)
        let mut bitmap_data = bitmap_slice.to_vec();
        let bitmap = crate::bitmap::Bitmap::new(&mut bitmap_data);
        let total_blocks = (sb.block_count - sb.data_block_start) as usize;
        
        let mut used_blocks = 0;
        let mut free_runs = 0;
        let mut current_run_len = 0;
        let mut largest_free_run = 0;
        let mut total_gap_size = 0;
        
        // Scan bitmap to collect statistics
        let mut in_free_run = false;
        for i in 0..total_blocks {
            let is_free = !bitmap.get(i); // get() returns bool, true = used, false = free
            
            if is_free {
                if !in_free_run {
                    // Start of new free run
                    free_runs += 1;
                    in_free_run = true;
                    current_run_len = 1;
                } else {
                    current_run_len += 1;
                }
            } else {
                used_blocks += 1;
                if in_free_run {
                    // End of free run
                    total_gap_size += current_run_len;
                    largest_free_run = largest_free_run.max(current_run_len);
                    in_free_run = false;
                    current_run_len = 0;
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
    pub fn defragment(&self, source_path: &str, mode: DefragMode, output_path: Option<&str>) -> Result<DefragStats, DiskManagerError> {
        match mode {
            DefragMode::Safe => self.defragment_safe(source_path, output_path),
            DefragMode::InPlace => self.defragment_inplace(),
        }
    }

    /// Safe defragmentation: creates new image with defragmented layout
    fn defragment_safe(&self, source_path: &str, output_path: Option<&str>) -> Result<DefragStats, DiskManagerError> {
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
            let guard = temp_dm.inner.lock().unwrap();
            let ib_blk = guard.superblock.inode_bitmap_block;
            if let Some(bitmap_slice) = Self::get_block_from_map(&guard.mmap, ib_blk) {
                let mut bitmap_copy = bitmap_slice.to_vec();
                let bitmap = crate::bitmap::Bitmap::new(&mut bitmap_copy);
                for i in 0..sb.inode_count as usize {
                    if bitmap.get(i) {
                        allocated_inodes.push(i as u64);
                    }
                }
            }
        }

        for inode_id in allocated_inodes {
            if let Ok(inode) = temp_dm.read_inode(inode_id) {
                if inode.mode == crate::inode::FileType::Directory {
                    let guard = temp_dm.inner.lock().unwrap();
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
                    temp_dm.write_inode(inode_id, &cleared_inode)?;
                }
            }
        }

        // Step 3: Clear data bitmap and preserve directory blocks
        {
            let mut guard = temp_dm.inner.lock().unwrap();
            let data_bitmap_block = guard.superblock.data_bitmap_block;
            let data_start = guard.superblock.data_block_start;
            if let Some(bitmap_slice) = Self::get_block_mut_from_map(&mut guard.mmap, data_bitmap_block) {
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
                            blocks_freed: stats_before.free_runs.saturating_sub(stats_after.free_runs),
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
            "In-place defragmentation not yet implemented"
        )))
    }

    /// Structural consistency check (fsck) for OIFS filesystem
    pub fn verify_integrity(&self) -> Result<FsckReport, DiskManagerError> {
        let guard = self.inner.lock().unwrap();
        let sb = guard.superblock;

        // 1. Collect all allocated Inode IDs from Inode Bitmap
        let mut allocated_inodes = std::collections::HashSet::new();
        let ib_blk = sb.inode_bitmap_block;
        if let Some(bitmap_slice) = Self::get_block_from_map(&guard.mmap, ib_blk) {
            let mut bitmap_copy = bitmap_slice.to_vec();
            let bitmap = crate::bitmap::Bitmap::new(&mut bitmap_copy);
            for i in 0..sb.inode_count as usize {
                if bitmap.get(i) {
                    allocated_inodes.insert(i as u64);
                }
            }
        }

        // 2. Collect all allocated Data Blocks from Data Bitmap
        let mut allocated_data_blocks = std::collections::HashSet::new();
        let db_blk = sb.data_bitmap_block;
        let db_start = sb.data_block_start;
        if let Some(bitmap_slice) = Self::get_block_from_map(&guard.mmap, db_blk) {
            let mut bitmap_copy = bitmap_slice.to_vec();
            let bitmap = crate::bitmap::Bitmap::new(&mut bitmap_copy);
            let max_data_blocks = sb.block_count.saturating_sub(db_start);
            for i in 0..max_data_blocks as usize {
                if bitmap.get(i) {
                    allocated_data_blocks.insert(db_start + i as u64);
                }
            }
        }

        // 3. Traversal tracking sets
        let mut referenced_inodes = std::collections::HashSet::new();
        let mut referenced_data_blocks: std::collections::HashMap<u64, Vec<u64>> = std::collections::HashMap::new();
        let mut cross_linked_blocks = std::collections::HashSet::new();

        // Always reference root inode
        referenced_inodes.insert(sb.root_inode);

        // Recursive directory scanner
        let mut queue = vec![sb.root_inode];
        let mut visited = std::collections::HashSet::new();

        while let Some(dir_id) = queue.pop() {
            if !visited.insert(dir_id) { continue; }

            let dir_inode = Self::read_inode_internal(&guard, dir_id)?;
            if dir_inode.mode != crate::inode::FileType::Directory { continue; }

            let block_id = dir_inode.blocks[0];
            if block_id == 0 { continue; }

            if let Some(block_data) = Self::get_block_from_map(&guard.mmap, block_id) {
                for entry in crate::directory::DirectoryIterator::new(block_data).flatten() {
                    referenced_inodes.insert(entry.inode);

                    if let Ok(child_inode) = Self::read_inode_internal(&guard, entry.inode)
                        && child_inode.mode == crate::inode::FileType::Directory {
                            queue.push(entry.inode);
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
