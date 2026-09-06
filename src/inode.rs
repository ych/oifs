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
/// - Maximum file size: ~513GB (10 direct + 512 single + 512^2 double + 512^3 triple indirect blocks)
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

    // Pre-compression filter fields (blosc2-style pipeline)
    /// Element size for shuffle/delta filters (1, 2, 4, or 8 bytes). 0 = no filters.
    pub filter_typesize: u8,
    /// Whether delta encoding was applied before compression
    pub filter_delta: bool,
    /// Whether byte shuffle was applied before compression
    pub filter_shuffle: bool,
    /// Whether bit shuffle was applied before compression
    pub filter_bitshuffle: bool,

    /// Triple indirect block pointer for files > 1GB (supports up to 513GB)
    pub triple_indirect: u64,
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
    /// - No filters applied
    /// - Zero triple indirect block pointer
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
            // Filter fields (default: no filters)
            filter_typesize: 0,
            filter_delta: false,
            filter_shuffle: false,
            filter_bitshuffle: false,
            // Triple indirect block
            triple_indirect: 0,
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Prove that Inode::new produces a zero-initialized inode with correct mode.
    #[kani::proof]
    fn proof_inode_new_file() {
        let inode = Inode::new(FileType::File);
        assert!(matches!(inode.mode, FileType::File));
        assert_eq!(inode.size, 0);
        assert_eq!(inode.compressed_size, 0);
        assert_eq!(inode.blocks, [0u64; 12]);
        assert!(!inode.encrypted);
        assert_eq!(inode.encryption_nonce, [0u8; 24]);
        // Filter fields must default to disabled
        assert_eq!(inode.filter_typesize, 0);
        assert!(!inode.filter_delta);
        assert!(!inode.filter_shuffle);
        assert!(!inode.filter_bitshuffle);
        assert_eq!(inode.triple_indirect, 0);
    }

    /// Prove that Inode::new(Directory) produces a valid directory inode.
    #[kani::proof]
    fn proof_inode_new_directory() {
        let inode = Inode::new(FileType::Directory);
        assert!(matches!(inode.mode, FileType::Directory));
        assert_eq!(inode.size, 0);
        assert_eq!(inode.compressed_size, 0);
        assert_eq!(inode.blocks, [0u64; 12]);
        assert!(!inode.encrypted);
        assert_eq!(inode.triple_indirect, 0);
    }

    /// Prove that no block pointer in a new inode is ever non-zero.
    #[kani::proof]
    fn proof_inode_no_dangling_blocks() {
        let mode_flag: bool = kani::any();
        let mode = if mode_flag { FileType::File } else { FileType::Directory };
        let inode = Inode::new(mode);

        for i in 0..12 {
            assert_eq!(inode.blocks[i], 0, "All block pointers must be zero in a new inode");
        }
        assert_eq!(inode.triple_indirect, 0, "Triple indirect block pointer must be zero in a new inode");
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
        assert_eq!(file_inode.triple_indirect, 0);
        assert!(!file_inode.encrypted);

        let dir_inode = Inode::new(FileType::Directory);
        assert_eq!(dir_inode.mode, FileType::Directory);
        assert_eq!(dir_inode.triple_indirect, 0);
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
        inode.triple_indirect = 400; // triple indirect
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
        assert_eq!(deserialized.triple_indirect, 400);
        assert_eq!(deserialized.encryption_nonce, [9u8; 24]);
    }
}

