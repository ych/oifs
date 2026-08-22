//! Inode module for OIFS file system
//!
//! Inodes store metadata about files and directories, including size,
//! timestamps, and block pointers for data storage.

use serde::{Deserialize, Serialize};

/// Type of file system entry
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
pub enum FileType {
    /// Regular file
    File,
    /// Directory (container for other files/directories)
    Directory,
}

/// Inode structure storing file/directory metadata
///
/// Each inode represents a file or directory in the file system.
/// The inode contains metadata and pointers to data blocks.
///
/// # Storage Layout
/// - Inodes are stored in a contiguous inode table
/// - Each inode occupies 256 bytes on disk
/// - Maximum file size: 48KB (12 direct blocks × 4KB)
///
/// # Compression
/// Files ≥ 8KB may be compressed using zstd:
/// - `size`: Logical (uncompressed) size
/// - `compressed_size`: Physical size on disk (0 if not compressed)
#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
#[repr(C)]
pub struct Inode {
    /// Type of this inode (File or Directory)
    pub mode: FileType,
    /// Logical size in bytes (original/uncompressed size)
    pub size: u64,
    /// Physical size in bytes if compressed, 0 if stored raw
    pub compressed_size: u64,
    /// Creation timestamp (Unix epoch seconds)
    pub created_at: u64,
    /// Last modification timestamp (Unix epoch seconds)
    pub modified_at: u64,
    /// Direct block pointers (12 blocks × 4KB = 48KB max file size)
    /// Block ID 0 indicates unallocated/empty block
    pub blocks: [u64; 12],
    
    // Encryption fields
    /// Is this file encrypted?
    pub encrypted: bool,
    /// Nonce for XChaCha20-Poly1305 (24 bytes, unique per file)
    pub encryption_nonce: [u8; 24],
}

impl Inode {
    /// Creates a new empty inode with the specified type
    ///
    /// # Arguments
    /// * `mode` - Type of inode (File or Directory)
    ///
    /// # Returns
    /// A new inode with:
    /// - Zero size
    /// - No allocated blocks
    /// - Zero timestamps (to be set by DiskManager)
    pub fn new(mode: FileType) -> Self {
        Self {
            mode,
            size: 0,
            compressed_size: 0,
            created_at: 0,
            modified_at: 0,
            blocks: [0; 12],
            // Encryption fields (default: not encrypted)
            encrypted: false,
            encryption_nonce: [0u8; 24],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inode_creation() {
        let file_inode = Inode::new(FileType::File);
        assert_eq!(file_inode.mode, FileType::File);
        assert_eq!(file_inode.size, 0);
        assert_eq!(file_inode.compressed_size, 0);
        assert_eq!(file_inode.blocks, [0; 12]);
        assert!(!file_inode.encrypted);

        let dir_inode = Inode::new(FileType::Directory);
        assert_eq!(dir_inode.mode, FileType::Directory);
    }

    #[test]
    fn test_inode_serialization_size_limit() {
        let mut inode = Inode::new(FileType::File);
        inode.size = 1048576;
        inode.compressed_size = 524288;
        inode.created_at = 1600000000;
        inode.modified_at = 1600000100;
        inode.blocks[0] = 100;
        inode.blocks[10] = 200; // single indirect
        inode.blocks[11] = 300; // double indirect
        inode.encrypted = true;
        inode.encryption_nonce = [9u8; 24];

        let bytes = bincode::serialize(&inode).expect("serialize inode");
        // An inode table entry slot is 256 bytes; serialized inode must fit
        assert!(bytes.len() <= 256);

        let deserialized: Inode = bincode::deserialize(&bytes).expect("deserialize inode");
        assert_eq!(deserialized.mode, FileType::File);
        assert_eq!(deserialized.size, 1048576);
        assert_eq!(deserialized.blocks[10], 200);
        assert_eq!(deserialized.blocks[11], 300);
        assert_eq!(deserialized.encryption_nonce, [9u8; 24]);
    }
}

