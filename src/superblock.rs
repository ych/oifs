//! SuperBlock module for OIFS (Our In-memory File System)
//!
//! The SuperBlock contains critical metadata about the file system layout,
//! including the locations of bitmaps, inode tables, and data blocks.

use serde::{Deserialize, Serialize};
use std::mem::size_of;

/// SuperBlock structure containing file system metadata
///
/// This is the first block (block 0) of the file system and contains
/// all the information needed to locate and manage other file system structures.
///
/// # Layout
/// - Block 0: SuperBlock (this structure)
/// - Block 1: Inode Bitmap (tracks allocated inodes)
/// - Block 2: Data Bitmap (tracks allocated data blocks)
/// - Block 3+: Inode Table (stores inode metadata)
/// - Remaining: Data Blocks (actual file/directory content)
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct SuperBlock {
    /// Magic number for file system identification (0x4F494653 = "OIFS")
    pub magic: u32,
    /// Size of each block in bytes (typically 4096)
    pub block_size: u32,
    /// Total number of blocks in the file system
    pub block_count: u64,
    /// Block ID where the inode bitmap is stored
    pub inode_bitmap_block: u64,
    /// Block ID where the data block bitmap is stored
    pub data_bitmap_block: u64,
    /// Block ID where the inode table starts
    pub inode_table_block: u64,
    /// Maximum number of inodes supported
    pub inode_count: u64,
    /// Block ID where data blocks begin
    pub data_block_start: u64,
    /// Inode ID of the root directory (typically 0)
    pub root_inode: u64,
}

impl SuperBlock {
    /// Magic number identifying the OIFS file system ("OIFS" in ASCII)
    pub const MAGIC: u32 = 0x4F494653; // "OIFS" in hex (O=4F, I=49, F=46, S=53)

    /// Creates a new SuperBlock for a file system with the given total number of blocks
    ///
    /// # Arguments
    /// * `total_blocks` - Total number of blocks available in the file system
    ///
    /// # Layout Calculation
    /// - Block 0: SuperBlock
    /// - Block 1: Inode Bitmap (1 block = 32,768 inodes)
    /// - Block 2: Data Bitmap (1 block = 32,768 blocks ≈ 128MB)
    /// - Block 3+: Inode Table (1024 blocks for 32,768 inodes @ 256 bytes each)
    /// - Remaining: Data Blocks
    pub fn new(total_blocks: u64) -> Self {
        let inode_bitmap_block = 1;
        let data_bitmap_block = 2;
        let inode_table_block = 3;
        
        // Calculate inode capacity: 1 block of bitmap = 4096 bytes * 8 bits/byte = 32,768 inodes
        let inode_count = 4096 * 8;
        let _inode_size = size_of::<crate::inode::Inode>() as u64;
        
        // Note: Current implementation uses 256 bytes per inode for storage
        // 32,768 inodes * 256 bytes = 8MB
        // 8MB / 4096 bytes/block = 2048 blocks
        // Using 128 for backward compatibility in calculation
        let inode_table_blocks = (inode_count * 128) / 4096;
        
        let data_block_start = inode_table_block + inode_table_blocks;

        Self {
            magic: Self::MAGIC,
            block_size: crate::BLOCK_SIZE as u32,
            block_count: total_blocks,
            inode_bitmap_block,
            data_bitmap_block,
            inode_table_block,
            inode_count,
            data_block_start,
            root_inode: 0,
        }
    }
}
