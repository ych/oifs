//! SuperBlock module for OIFS (Our In-memory File System)
//!
//! The SuperBlock contains critical metadata about the file system layout,
//! including the locations of bitmaps, inode tables, and data blocks.

use serde::{Deserialize, Serialize};

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

    // Encryption fields
    /// Is this filesystem encrypted?
    pub encrypted: bool,
    /// Salt for Argon2 key derivation (16 bytes)
    pub encryption_salt: [u8; 16],
    /// Encryption version/algorithm identifier
    pub encryption_version: u8,
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
    /// - Block 1: Inode Bitmap (1 block = up to 32,768 inodes)
    /// - Block 2: Data Bitmap (1 block = up to 32,768 data blocks)
    /// - Block 3+: Inode Table (dynamically sized based on available space)
    /// - Remaining: Data Blocks
    ///
    /// # Panics
    /// Panics if `total_blocks < 5` (minimum: superblock + 2 bitmaps + 1 inode table + 1 data)
    pub fn new(total_blocks: u64) -> Self {
        Self::new_with_layout(total_blocks, 3)
    }

    /// Creates a new SuperBlock, placing the inode table at `inode_table_block`.
    ///
    /// Journaled images reuse this with a table start past the reserved journal
    /// region, so both layouts share one sizing formula and stay provably
    /// non-overlapping.
    ///
    /// # Panics
    /// Panics if `total_blocks` cannot hold the metadata regions plus one data block.
    pub fn new_with_layout(total_blocks: u64, inode_table_block: u64) -> Self {
        let reserved = inode_table_block;
        assert!(
            total_blocks >= reserved + 2,
            "File system too small for the requested metadata layout"
        );

        let inode_bitmap_block = 1;
        let data_bitmap_block = 2;

        // Blocks available after fixed metadata (superblock + 2 bitmaps + any reserved journal)
        let available = total_blocks - reserved;

        // Maximum inodes the bitmap can track (1 block = 4096 * 8 = 32,768 bits)
        let bitmap_max_inodes: u64 = 4096 * 8;

        // Standard inode capacity sizing (historical 128 bytes ratio yields standard 1024 blocks = 32,768 inodes)
        let inodes_per_block: u64 = 4096 / 128;

        // Maximum inode table blocks to fill the bitmap
        let inode_table_blocks_cap = bitmap_max_inodes / inodes_per_block; // = 1024

        // If the filesystem has room for the standard 1024 inode table blocks (plus at least 1 data block),
        // allocate the standard 1024 blocks (32,768 inodes) for full capacity and backward compatibility.
        // For smaller filesystems, dynamically size the table up to 25% of available space.
        let inode_table_blocks = if total_blocks > inode_table_block + inode_table_blocks_cap {
            inode_table_blocks_cap // = 1024 blocks = 32,768 inodes
        } else {
            (available.saturating_sub(1) / 4).min(inode_table_blocks_cap)
        };

        let inode_count = inode_table_blocks * inodes_per_block;
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
            // Encryption fields (default: not encrypted)
            encrypted: false,
            encryption_salt: [0u8; 16],
            encryption_version: 0,
        }
    }

    /// Creates a SuperBlock for a journaled image, reserving space for the
    /// metadata WAL ring between the data bitmap and the inode table.
    ///
    /// The returned layout is `inode_table_block == 3 + JOURNAL_RESERVED_BLOCKS`,
    /// which is what [`crate::journal::is_journaled_layout`] detects on mount.
    ///
    /// # Panics
    /// Panics if `total_blocks` is too small to hold the journal plus one data block.
    pub fn new_journaled(total_blocks: u64) -> Self {
        Self::new_with_layout(total_blocks, crate::journal::JOURNAL_INODE_TABLE_BLOCK)
    }

    /// Returns `true` when this image reserves a metadata WAL region.
    pub fn has_journal_layout(&self) -> bool {
        crate::journal::is_journaled_layout(self)
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Prove that SuperBlock::new always produces valid layout ordering.
    #[kani::proof]
    fn proof_superblock_layout_ordering() {
        let total_blocks: u64 = kani::any();
        // Minimum 5 blocks (enforced by assert in new()), cap for CBMC tractability
        kani::assume(total_blocks >= 5 && total_blocks <= 1_000_000);

        let sb = SuperBlock::new(total_blocks);

        // Magic must always be set correctly
        assert_eq!(sb.magic, SuperBlock::MAGIC);

        // Block size must match
        assert_eq!(sb.block_size, crate::BLOCK_SIZE as u32);

        // Layout ordering: superblock(0) < inode_bitmap < data_bitmap <= inode_table <= data_start
        assert!(sb.inode_bitmap_block < sb.data_bitmap_block);
        assert!(sb.data_bitmap_block < sb.inode_table_block);
        assert!(sb.inode_table_block <= sb.data_block_start);

        // Data must start within the total block range (the original bug)
        assert!(sb.data_block_start <= sb.block_count);

        // Must have at least 1 data block
        assert!(sb.block_count - sb.data_block_start >= 1);

        // Root inode is always 0
        assert_eq!(sb.root_inode, 0);
    }

    /// Prove that large file systems (>= 1028 blocks) get the full 32,768 inodes.
    #[kani::proof]
    fn proof_superblock_large_fs_full_inodes() {
        let total_blocks: u64 = kani::any();
        // At total_blocks >= 1028: full capacity
        kani::assume(total_blocks >= 1028 && total_blocks <= 1_000_000);

        let sb = SuperBlock::new(total_blocks);
        assert_eq!(
            sb.inode_count,
            4096 * 8,
            "Large FS must have full 32,768 inodes"
        );
    }

    /// Prove that new SuperBlock defaults to unencrypted.
    #[kani::proof]
    fn proof_superblock_default_unencrypted() {
        let sb = SuperBlock::new(1000);
        assert!(!sb.encrypted);
        assert_eq!(sb.encryption_salt, [0u8; 16]);
        assert_eq!(sb.encryption_version, 0);
    }

    /// Prove that `SuperBlock::new` and `SuperBlock::new_with_layout` agree for the
    /// legacy table start, so journaling cannot silently change legacy geometry.
    #[kani::proof]
    fn proof_legacy_layout_unchanged() {
        let total_blocks: u64 = kani::any();
        kani::assume(total_blocks >= 5 && total_blocks <= 1_000_000);

        let legacy = SuperBlock::new(total_blocks);
        let explicit = SuperBlock::new_with_layout(total_blocks, 3);
        assert_eq!(legacy, explicit);
        assert_eq!(legacy.inode_table_block, 3);
        assert!(!crate::journal::is_journaled_layout(&legacy));
    }

    /// Prove that a journaled layout never overlaps the journal region and that
    /// the inode table starts strictly after the reserved journal blocks.
    #[kani::proof]
    fn proof_journaled_layout_non_overlapping() {
        use crate::journal::{
            JOURNAL_HEADER_BLOCK, JOURNAL_INODE_TABLE_BLOCK, JOURNAL_RESERVED_BLOCKS,
        };

        let total_blocks: u64 = kani::any();
        kani::assume(total_blocks >= 1_200 && total_blocks <= 1_000_000);

        let sb = SuperBlock::new_journaled(total_blocks);

        // The journal header occupies block 3; the record ring follows it.
        assert_eq!(JOURNAL_HEADER_BLOCK, 3);
        assert_eq!(JOURNAL_INODE_TABLE_BLOCK, 3 + JOURNAL_RESERVED_BLOCKS);
        assert!(sb.inode_table_block >= JOURNAL_INODE_TABLE_BLOCK);

        // Bitmaps sit before the journal, the inode table after it.
        assert!(sb.inode_bitmap_block < sb.data_bitmap_block);
        assert!(sb.data_bitmap_block <= JOURNAL_HEADER_BLOCK);
        assert!(JOURNAL_HEADER_BLOCK < sb.inode_table_block);

        // No region runs past the end of the image.
        assert!(sb.inode_table_block <= sb.data_block_start);
        assert!(sb.data_block_start <= sb.block_count);
        assert!(sb.block_count - sb.data_block_start >= 1);

        assert!(crate::journal::is_journaled_layout(&sb));
    }

    /// Prove that a journaled image is strictly larger in metadata than the legacy
    /// image of the same size, so `has_journal_layout` never false-positives.
    #[kani::proof]
    fn proof_journaled_shifts_data_start() {
        let total_blocks: u64 = kani::any();
        kani::assume(total_blocks >= 1_200 && total_blocks <= 1_000_000);

        let legacy = SuperBlock::new(total_blocks);
        let journaled = SuperBlock::new_journaled(total_blocks);

        assert!(journaled.data_block_start > legacy.data_block_start);
        // Inode capacity is unchanged; only the placement moves.
        assert_eq!(journaled.inode_count, legacy.inode_count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_superblock_new_layout() {
        let total_blocks = 2560; // 10MB / 4KB
        let sb = SuperBlock::new(total_blocks);

        assert_eq!(sb.magic, SuperBlock::MAGIC);
        assert_eq!(sb.block_size, 4096);
        assert_eq!(sb.block_count, 2560);
        assert_eq!(sb.inode_bitmap_block, 1);
        assert_eq!(sb.data_bitmap_block, 2);
        assert_eq!(sb.inode_table_block, 3);
        assert_eq!(sb.inode_count, 32768);
        assert_eq!(sb.data_block_start, 3 + 1024); // 1027
        assert_eq!(sb.root_inode, 0);
        assert!(!sb.encrypted);
    }

    #[test]
    fn test_superblock_serialization_roundtrip() {
        let mut sb = SuperBlock::new(5000);
        sb.encrypted = true;
        sb.encryption_salt = [7u8; 16];
        sb.encryption_version = 1;

        let serialized = bincode::serialize(&sb).expect("serialize sb");
        assert!(serialized.len() <= 4096);

        let deserialized: SuperBlock = bincode::deserialize(&serialized).expect("deserialize sb");
        assert_eq!(deserialized, sb);
    }
}
