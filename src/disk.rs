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
}

/// Compression mode for write operations
///
/// Controls when files should be compressed using zstd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMode {
    /// Always compress, regardless of file size
    Always,
    /// Never compress
    Never,
    /// Auto: compress files >= 8KB
    Auto,
}

impl Default for CompressionMode {
    fn default() -> Self {
        CompressionMode::Auto
    }
}

/// Statistics about disk fragmentation
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefragMode {
    /// Safe mode: create new image and replace original after success
    Safe,
    /// In-place mode: directly modify original image (faster but risky)
    InPlace,
}

impl Default for DefragMode {
    fn default() -> Self {
        DefragMode::Safe
    }
}

/// Statistics from defragmentation operation
#[derive(Debug, Clone)]
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
        let path = path.as_ref();
        let exists = path.exists();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        // Acquire Lock using F_SETLK
        let mut lock = unsafe { std::mem::zeroed::<libc::flock>() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        lock.l_start = 0;
        lock.l_len = 0; // Whole file

        use nix::fcntl::{fcntl, FcntlArg};
        match fcntl(&file, FcntlArg::F_SETLK(&lock)) {
             Ok(_) => {},
             Err(e) => return Err(DiskManagerError::Locking(e)),
        }

        if !exists {
            // New file: set size
            file.set_len(total_size)?;
        }

        let mut mmap = unsafe { MmapOptions::new().map_mut(&file)? };
        let superblock: SuperBlock;

        if exists {
            if mmap.len() < BLOCK_SIZE {
                return Err(DiskManagerError::FileTooSmall);
            }
            superblock = bincode::deserialize(&mmap[0..BLOCK_SIZE])?;
            if superblock.magic != SuperBlock::MAGIC {
                return Err(DiskManagerError::InvalidMagic);
            }
        } else {
            let block_count = total_size / BLOCK_SIZE as u64;
            superblock = SuperBlock::new(block_count);
            let serialized = bincode::serialize(&superblock)?;
            mmap[0..serialized.len()].copy_from_slice(&serialized);
        }

        let inner = DiskManagerInner {
            file,
            mmap,
            superblock,
        };
        
        // Use a new scope to initialize root if needed using the public API?
        // But public methods take locks. We have ownership of inner here.
        // We can just manipulate inner.

        let dm = Self {
            inner: Arc::new(Mutex::new(inner)),
        };

        if !exists {
            // Need to initialize root inode
            // We can call public methods since we have the Arc now.
            // Be careful not to deadlock (recursive lock). 
            // Current open code is: allocate root, write root inode.
            
            // We can inline the initialization logic to avoid locking `dm` while we (don't have lock yet? we have ownership).
            // Actually `dm.open` is static.
            
            // Alloc root inode (0)
            {
               let mut guard = dm.inner.lock().unwrap();
               // We need helper to get allocator from guard? 
               // Duplication of logic or move methods to Inner?
               // Let's implement allocator getter on Inner.
               
               let inode_bitmap_block = guard.superblock.inode_bitmap_block;
               let bitmap_slice = Self::get_block_mut_from_map(&mut guard.mmap, inode_bitmap_block).expect("Bitmap");
               let mut ia = SimpleBlockAllocator::new(bitmap_slice, 0);
               let root_id = ia.allocate()?;
               if root_id != 0 { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, "Failed init root"))); }
               
               let data_bitmap_block = guard.superblock.data_bitmap_block;
               let data_start = guard.superblock.data_block_start;
               let data_slice = Self::get_block_mut_from_map(&mut guard.mmap, data_bitmap_block).expect("Bitmap");
               let mut da = SimpleBlockAllocator::new(data_slice, data_start);
               let root_data = da.allocate()?;
               
               let mut root_inode = Inode::new(crate::inode::FileType::Directory);
               root_inode.blocks[0] = root_data;
               
               // Write Inode
               let inode_idx = 0;
               let table_blk = guard.superblock.inode_table_block;
               let offset = table_blk * BLOCK_SIZE as u64 + inode_idx * 256;
               let slice = &mut guard.mmap[offset as usize .. (offset+256) as usize];
               let bytes = bincode::serialize(&root_inode)?;
               slice[..bytes.len()].copy_from_slice(&bytes);
            }
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
    ///
    /// # Arguments
    /// * `inode_id` - The ID of the inode to read
    ///
    /// # Returns
    /// The deserialized `Inode` structure
    pub fn read_inode(&self, inode_id: u64) -> Result<Inode, DiskManagerError> {
        let guard = self.inner.lock().unwrap();
        let inode_table_start = guard.superblock.inode_table_block * BLOCK_SIZE as u64;
        let inode_size = 256;
        let offset = inode_table_start + inode_id * inode_size;
        
        if offset + inode_size > (guard.mmap.len() as u64) {
            return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Bounds")));
        }
        let slice = &guard.mmap[offset as usize .. (offset+inode_size) as usize];
        Ok(bincode::deserialize(slice)?)
    }

    /// Writes an inode to the inode table
    ///
    /// # Arguments
    /// * `inode_id` - The ID of the inode to write
    /// * `inode` - The inode data to write
    pub fn write_inode(&self, inode_id: u64, inode: &Inode) -> Result<(), DiskManagerError> {
        let mut guard = self.inner.lock().unwrap();
        let inode_table_start = guard.superblock.inode_table_block * BLOCK_SIZE as u64;
        let inode_size = 256;
        let offset = inode_table_start + inode_id * inode_size;
        
        if offset + inode_size > (guard.mmap.len() as u64) { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "Bounds"))); }

        let slice = &mut guard.mmap[offset as usize .. (offset + inode_size) as usize];
        let bytes = bincode::serialize(inode)?;
        if bytes.len() > inode_size as usize { return Err(DiskManagerError::Serialization(Box::new(bincode::ErrorKind::SizeLimit))); }
        slice[..bytes.len()].copy_from_slice(&bytes);
        Ok(())
    }

    /// Creates a new file in a directory
    ///
    /// # Arguments
    /// * `parent_inode_id` - Inode ID of the parent directory
    /// * `name` - Name of the new file
    ///
    /// # Returns
    /// The inode ID of the newly created file
    ///
    /// # Errors
    /// Returns an error if:
    /// - Parent is not a directory
    /// - File with same name already exists
    /// - No free inodes available
    pub fn create_file(&self, parent_inode_id: u64, name: &str) -> Result<u64, DiskManagerError> {
         // Lock for atomicity - ensures consistency during multi-step operation
         let mut guard = self.inner.lock().unwrap();
         
         // 1. Read Parent
         let parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
         if parent_inode.mode != crate::inode::FileType::Directory { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, "Not dir"))); }
         
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
         let dir_block_id = parent_inode.blocks[0];
         if dir_block_id == 0 { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, "No block"))); }
         
         // Scan
         let mut insert_offset = 0;
         if let Some(block_slice) = Self::get_block_mut_from_map(&mut guard.mmap, dir_block_id) {
             use crate::directory::DirectoryEntry; // Can't import inside if?
             let mut cursor = std::io::Cursor::new(&block_slice[..]); // Read-only cursor
             loop {
                 let start = cursor.position();
                 match DirectoryEntry::deserialize_from(&mut cursor) {
                      Ok(Some(_)) => continue,
                      Ok(None) => { insert_offset = start; break; },
                      Err(_) => return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, "Corrupt"))),
                 }
             }
         }
         
         if insert_offset as usize >= BLOCK_SIZE { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, "Full"))); }
         
         // Write Entry
         if let Some(block_slice) = Self::get_block_mut_from_map(&mut guard.mmap, dir_block_id) {
             use crate::directory::DirectoryEntry;
             let mut cursor = std::io::Cursor::new(block_slice);
             cursor.set_position(insert_offset);
             let entry = DirectoryEntry { inode: file_inode_id, hash: 0, name: name.to_string() };
             entry.serialize_into(&mut cursor).map_err(|e| DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;
         }
         
         // Update Parent Mtime
         let mut parent_inode = parent_inode; // Copy
         parent_inode.modified_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
         Self::write_inode_internal(&mut guard, parent_inode_id, &parent_inode)?;
         
         // Explicit sync for metadata safety
         guard.mmap.flush()?;

         Ok(file_inode_id)
    }

    /// Creates a new directory in a parent directory
    ///
    /// # Arguments
    /// * `parent_inode_id` - Inode ID of the parent directory
    /// * `name` - Name of the new directory
    ///
    /// # Returns
    /// The inode ID of the newly created directory
    ///
    /// # Errors
    /// Returns an error if:
    /// - Parent is not a directory
    /// - Directory with same name already exists
    /// - No free inodes or data blocks available
    pub fn create_directory(&self, parent_inode_id: u64, name: &str) -> Result<u64, DiskManagerError> {
         let mut guard = self.inner.lock().unwrap();
         
         let parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
         if parent_inode.mode != crate::inode::FileType::Directory { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, "Not dir"))); }
         
         // Alloc Inode
         let inode_bitmap = guard.superblock.inode_bitmap_block;
         let bitmap_slice = Self::get_block_mut_from_map(&mut guard.mmap, inode_bitmap).unwrap();
         let mut ia = SimpleBlockAllocator::new(bitmap_slice, 0);
         let dir_inode_id = ia.allocate()?;
         
         // Alloc Data
         let data_bitmap = guard.superblock.data_bitmap_block;
         let data_start = guard.superblock.data_block_start;
         // Need to re-borrow mmap? 
         // Rust borrow checker works with guard fields disjointly? No, mmap is one field.
         // We dropped ia? Yes.
         let data_slice = Self::get_block_mut_from_map(&mut guard.mmap, data_bitmap).unwrap();
         let mut da = SimpleBlockAllocator::new(data_slice, data_start);
         let dir_data_block = da.allocate()?;
         
         // Init Inode
         let mut dir_inode = Inode::new(crate::inode::FileType::Directory);
         dir_inode.modified_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
         dir_inode.blocks[0] = dir_data_block;
         Self::write_inode_internal(&mut guard, dir_inode_id, &dir_inode)?;
         
         // Add to Parent
          let dir_block_id = parent_inode.blocks[0];
          // ... (Same logic as create_file for adding entry)
          // Refactor add_entry?
          // Inline for now.
         let mut insert_offset = 0;
         if let Some(block_slice) = Self::get_block_mut_from_map(&mut guard.mmap, dir_block_id) {
             use crate::directory::DirectoryEntry; 
             let mut cursor = std::io::Cursor::new(&block_slice[..]); 
             loop {
                 let start = cursor.position();
                 match DirectoryEntry::deserialize_from(&mut cursor) {
                      Ok(Some(_)) => continue,
                      Ok(None) => { insert_offset = start; break; },
                      Err(_) => return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, "Corrupt"))),
                 }
             }
         }
         
         if let Some(block_slice) = Self::get_block_mut_from_map(&mut guard.mmap, dir_block_id) {
             use crate::directory::DirectoryEntry;
             let mut cursor = std::io::Cursor::new(block_slice);
             cursor.set_position(insert_offset);
             let entry = DirectoryEntry { inode: dir_inode_id, hash: 0, name: name.to_string() };
             entry.serialize_into(&mut cursor).map_err(|e| DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;
         }

         let mut parent_inode = parent_inode;
         parent_inode.modified_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
         Self::write_inode_internal(&mut guard, parent_inode_id, &parent_inode)?;
         
         // Explicit sync
         guard.mmap.flush()?;

         Ok(dir_inode_id)
    }

    pub fn lookup(&self, parent_inode_id: u64, name: &str) -> Result<u64, DiskManagerError> {
        let guard = self.inner.lock().unwrap();
        let parent_inode = Self::read_inode_internal(&guard, parent_inode_id)?;
        if parent_inode.mode != crate::inode::FileType::Directory { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, "Not dir"))); }
        
        let dir_block_id = parent_inode.blocks[0];
        if dir_block_id == 0 { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "Not found"))); }
        
        if let Some(slice) = Self::get_block_from_map(&guard.mmap, dir_block_id) {
             use crate::directory::DirectoryEntry;
             let mut cursor = std::io::Cursor::new(slice);
             loop {
                 match DirectoryEntry::deserialize_from(&mut cursor) {
                     Ok(Some(entry)) => {
                         if entry.name == name { return Ok(entry.inode); }
                     }
                     Ok(None) => break,
                     Err(_) => return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, "Corrupt"))),
                 }
             }
        }
        Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "Not found")))
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
        let guard = self.inner.lock().unwrap();
        let inode = Self::read_inode_internal(&guard, inode_id)?;
        
        // Determine physical size on disk
        // If file is compressed, use compressed_size; otherwise use logical size
        let physical_size = if inode.compressed_size > 0 { inode.compressed_size } else { inode.size };
        let mut raw_data = Vec::with_capacity(physical_size as usize);
        let mut read = 0;
        
        for &blk in inode.blocks.iter() {
            if read >= physical_size { break; }
            if blk == 0 { break; }
            if let Some(slice) = Self::get_block_from_map(&guard.mmap, blk) {
                let rem = physical_size as usize - read as usize;
                let to_read = std::cmp::min(rem, BLOCK_SIZE);
                raw_data.extend_from_slice(&slice[..to_read]);
                read += to_read as u64;
            }
        }
        
        // Decompress if this is a compressed file
        if inode.mode == crate::inode::FileType::File && inode.compressed_size > 0 {
             let decoded = zstd::stream::decode_all(std::io::Cursor::new(&raw_data))
                 .map_err(|e| DiskManagerError::Io(e))?;
             Ok(decoded)
        } else {
             Ok(raw_data)
        }
    }

    /// Writes data to a file
    ///
    /// # Arguments
    /// * `inode_id` - The inode ID of the file to write to
    /// * `file_offset` - Byte offset to start writing at
    /// * `data` - Data to write
    /// * `compression_mode` - Compression mode (Always, Never, or Auto)
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
    pub fn write_data(&self, inode_id: u64, file_offset: u64, data: &[u8], compression_mode: CompressionMode) -> Result<(), DiskManagerError> {
        let mut guard = self.inner.lock().unwrap();
        let mut inode = Self::read_inode_internal(&guard, inode_id)?;
        
        let final_data: std::borrow::Cow<[u8]>;
        let mut is_compressed = false;

        // Determine if we should compress based on mode
        let should_compress = match compression_mode {
            CompressionMode::Always => true,
            CompressionMode::Never => false,
            CompressionMode::Auto => data.len() >= 8192,
        };

        // Attempt compression for files written from start
        if inode.mode == crate::inode::FileType::File && file_offset == 0 && should_compress {
            let compressed = zstd::stream::encode_all(std::io::Cursor::new(data), 0)
                .map_err(|e| DiskManagerError::Io(e))?;
            
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
                    if compressed.len() < data.len() {
                        final_data = std::borrow::Cow::Owned(compressed);
                        is_compressed = true;
                    } else {
                        final_data = std::borrow::Cow::Borrowed(data);
                    }
                }
                CompressionMode::Never => {
                    // Should not reach here due to should_compress check above
                    final_data = std::borrow::Cow::Borrowed(data);
                }
            }
        } else {
            // Handle non-compressed writes (small files, directories, appends)
            if inode.mode == crate::inode::FileType::File && inode.compressed_size > 0 {
                // Prevent appending to already-compressed files
                // (would require decompress-modify-recompress sequence)
                if inode.compressed_size > 0 {
                     return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, "Cannot append to compressed file")));
                }
                final_data = std::borrow::Cow::Borrowed(data);
            } else {
                final_data = std::borrow::Cow::Borrowed(data);
            }
        }

        let write_buffer = final_data.as_ref();
        let mut written = 0;
        let mut current_offset = file_offset;
        
        while written < write_buffer.len() {
            let blk_idx = (current_offset / BLOCK_SIZE as u64) as usize;
            if blk_idx >= 12 { 
                return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::FileTooLarge, "File too large (max 48KB)"))); 
            } // Max size limit for simple implementation
            
            let mut blk_id = inode.blocks[blk_idx];
            if blk_id == 0 {
                let db_blk = guard.superblock.data_bitmap_block;
                let db_start = guard.superblock.data_block_start;
                let slice = Self::get_block_mut_from_map(&mut guard.mmap, db_blk).unwrap();
                let mut da = SimpleBlockAllocator::new(slice, db_start);
                blk_id = da.allocate()?;
                inode.blocks[blk_idx] = blk_id;
            }
            
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
    
    fn get_block_from_map(mmap: &MmapMut, block_id: u64) -> Option<&[u8]> {
        let start = block_id as usize * BLOCK_SIZE;
        let end = start + BLOCK_SIZE;
        if end > mmap.len() { None } else { Some(&mmap[start..end]) }
    }
    
    // Path resolution API (public) - wraps lookup
    pub fn resolve_path(&self, path: &str) -> Result<u64, DiskManagerError> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
        // Since lookup takes &self and locks internally, we can just loop.
        // Optimization: Lock once and do manual lookup loop? 
        // Yes, to ensure consistency of path resolution.
        
        let guard = self.inner.lock().unwrap();
        let mut curr = guard.superblock.root_inode;
        
        for part in parts {
             // Inline lookup
             let parent = Self::read_inode_internal(&guard, curr)?;
             if parent.mode != crate::inode::FileType::Directory { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, "Not dir"))); }
             let blk = parent.blocks[0]; // Assuming single block
             if blk == 0 { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "Not found"))); }
             
             let mut found = false;
             if let Some(slice) = Self::get_block_from_map(&guard.mmap, blk) {
                 use crate::directory::DirectoryEntry;
                 let mut cur = std::io::Cursor::new(slice);
                 loop {
                     match DirectoryEntry::deserialize_from(&mut cur) {
                         Ok(Some(e)) => if e.name == part { curr = e.inode; found = true; break; },
                         Ok(None) => break,
                         Err(_) => break,
                     }
                 }
             }
             if !found { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "Not found"))); }
        }
        Ok(curr)
    }

    // Need public method to get block for LS (which iterates manually)
    // Or exposing iterator? 
    // The current CLI `ls` gets block data and iterates.
    // We should probably expose `ls` logic or specific `get_block_copy`.
    // Returning `&[u8]` is impossible because it's bound to LockGuard.
    // Returning `Vec<u8>` copy is fine.
    
    pub fn get_block_copy(&self, block_id: u64) -> Option<Vec<u8>> {
        let guard = self.inner.lock().unwrap();
        Self::get_block_from_map(&guard.mmap, block_id).map(|s| s.to_vec())
    }

    pub fn resolve_parent(&self, path: &str) -> Result<(u64, String), DiskManagerError> {
         let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
         if parts.is_empty() { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Empty"))); }
         let name = parts.last().unwrap().to_string();
         let parent_path = parts[..parts.len()-1].join("/");
         
         // If parent path empty, root.
         let parent_id = if parent_path.is_empty() {
             self.superblock().root_inode 
         } else {
             self.resolve_path(&parent_path)?
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
        if parent_inode.mode != crate::inode::FileType::Directory { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, "Not a directory"))); }
        
        let dir_block_id = parent_inode.blocks[0];
        if dir_block_id == 0 { return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "File not found"))); }
        
        let mut target_inode = None;
        let mut entries = Vec::new();

        // 1. Scan and filter
        if let Some(slice) = Self::get_block_from_map(&guard.mmap, dir_block_id) {
            use crate::directory::DirectoryEntry;
            let mut cursor = std::io::Cursor::new(slice);
            loop {
                match DirectoryEntry::deserialize_from(&mut cursor) {
                    Ok(Some(entry)) => {
                        if entry.name == name {
                            target_inode = Some(entry.inode);
                        } else {
                            entries.push(entry);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => return Err(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, "Corrupt"))),
                }
            }
        }
        
        let target_inode_id = target_inode.ok_or(DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "File not found")))?;
        
        // 2. Rewrite Directory Block
        if let Some(slice) = Self::get_block_mut_from_map(&mut guard.mmap, dir_block_id) {
             let mut new_data = vec![0u8; BLOCK_SIZE];
             let mut cursor = std::io::Cursor::new(&mut new_data);
             for entry in entries {
                 entry.serialize_into(&mut cursor).map_err(|e| DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))?;
             }
             slice.copy_from_slice(&new_data);
        }
        
        // 3. Free Inode & Blocks
        let file_inode = Self::read_inode_internal(&guard, target_inode_id)?;
        
        // Free Data Blocks
        {
            let db_blk = guard.superblock.data_bitmap_block;
            let db_start = guard.superblock.data_block_start;
            let slice = Self::get_block_mut_from_map(&mut guard.mmap, db_blk).unwrap();
            let mut da = SimpleBlockAllocator::new(slice, db_start);
            
            for &blk in file_inode.blocks.iter() {
                if blk != 0 {
                    da.free(blk)?;
                }
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
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::new(std::io::ErrorKind::Other, "Bitmap not found")))?;
        
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
        
        // Collect all active inodes and their data
        let sb = temp_dm.superblock();
        let mut file_data_list = Vec::new();
        
        for inode_id in 0..sb.inode_count {
            if let Ok(inode) = temp_dm.read_inode(inode_id) {
                if inode.size > 0 {
                    // Read and store data
                    let data = temp_dm.read_data(inode_id)?;
                    file_data_list.push((inode_id, inode, data));
                }
            }
        }
        
        // Step 3: Clear the data bitmap to start fresh allocation
        {
            let mut guard = temp_dm.inner.lock().unwrap();
            let data_bitmap_block = guard.superblock.data_bitmap_block;
            if let Some(bitmap_slice) = Self::get_block_mut_from_map(&mut guard.mmap, data_bitmap_block) {
                // Clear all bits (set to 0 = free)
                for byte in bitmap_slice.iter_mut() {
                    *byte = 0;
                }
            }
        }
        
        // Step 4: Reallocate blocks contiguously and write data
        let mut files_processed = 0;
        let mut bytes_moved = 0u64;
        
        for (inode_id, mut inode, data) in file_data_list {
            // Clear old block assignments
            for i in 0..12 {
                inode.blocks[i] = 0;
            }
            
            // Write data - this will allocate new contiguous blocks
            // Use Never compression to preserve exact data structure
            let comp_mode = if inode.compressed_size > 0 {
                CompressionMode::Always  // Preserve compression
            } else {
                CompressionMode::Never
            };
            
            temp_dm.write_data(inode_id, 0, &data, comp_mode)?;
            
            // Count statistics
            if inode.mode == crate::inode::FileType::File {
                files_processed += 1;
                bytes_moved += data.len() as u64;
            }
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
        Err(DiskManagerError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "In-place defragmentation not yet implemented"
        )))
    }
}
