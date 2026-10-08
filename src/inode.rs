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
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
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

    /// Extended inode flags (e.g. INODE_FLAG_SEEKABLE_64K)
    pub flags: u32,
}

/// Standard chunk size for seekable compressed files (64KB).
pub const CHUNK_SIZE_64K: usize = 64 * 1024;

/// Inode flags bitmask: File uses seekable 64KB chunked compression.
pub const INODE_FLAG_SEEKABLE_64K: u32 = 0x0001;

/// Chunk entry representation inside an inode's block pointer tree.
///
/// When an inode has `INODE_FLAG_SEEKABLE_64K` set, each 64-bit slot in `blocks[0..12]`
/// (and indirect blocks) represents a 64KB logical chunk rather than a single 4KB block.
///
/// Layout (64 bits):
/// - `bits [0..32]`:  `start_block: u32` (physical start block on disk)
/// - `bits [32..40]`: `block_count: u8` (number of contiguous 4KB blocks allocated, 1..16)
/// - `bits [40..48]`: `flags: u8` (chunk state flags: Empty, Compressed, Raw)
/// - `bits [48..64]`: `compressed_len: u16` (exact compressed byte length <= 65535)
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChunkEntry(pub u64);

impl ChunkEntry {
    /// Chunk is empty / unallocated (sparse hole).
    pub const FLAG_EMPTY: u8 = 0;
    /// Chunk is compressed with Zstd.
    pub const FLAG_COMPRESSED: u8 = 1;
    /// Chunk is stored uncompressed (anti-inflation fallback).
    pub const FLAG_RAW: u8 = 2;

    /// Empty chunk entry.
    pub const EMPTY: Self = Self(0);

    /// Creates a new `ChunkEntry` encoding the provided fields.
    #[inline]
    pub fn new(start_block: u32, block_count: u8, flags: u8, compressed_len: u16) -> Self {
        let val = (start_block as u64)
            | ((block_count as u64) << 32)
            | ((flags as u64) << 40)
            | ((compressed_len as u64) << 48);
        Self(val)
    }

    /// Physical start block on disk.
    #[inline]
    pub fn start_block(&self) -> u32 {
        self.0 as u32
    }

    /// Number of contiguous 4KB blocks allocated on disk (1..16).
    #[inline]
    pub fn block_count(&self) -> u8 {
        ((self.0 >> 32) & 0xFF) as u8
    }

    /// Chunk state flags (`FLAG_EMPTY`, `FLAG_COMPRESSED`, `FLAG_RAW`).
    #[inline]
    pub fn flags(&self) -> u8 {
        ((self.0 >> 40) & 0xFF) as u8
    }

    /// Exact compressed byte length stored on disk (<= 65535).
    #[inline]
    pub fn compressed_len(&self) -> u16 {
        ((self.0 >> 48) & 0xFFFF) as u16
    }

    /// Returns true if this chunk is unallocated or empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0 == 0 || self.flags() == Self::FLAG_EMPTY
    }

    /// Returns true if this chunk is compressed with Zstd.
    #[inline]
    pub fn is_compressed(&self) -> bool {
        self.flags() == Self::FLAG_COMPRESSED
    }

    /// Returns true if this chunk is stored uncompressed as raw blocks.
    #[inline]
    pub fn is_raw(&self) -> bool {
        self.flags() == Self::FLAG_RAW
    }
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
    /// - Zero flags
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
            // Extended flags
            flags: 0,
        }
    }
}
/// Number of direct block pointers in an inode (`blocks[0..10]`).
pub const DIRECT_BLOCKS: usize = 10;
/// Number of 8-byte block pointers that fit in one 4KB indirect block.
pub const PTRS_PER_BLOCK: usize = 512;
const SINGLE_END: usize = DIRECT_BLOCKS + PTRS_PER_BLOCK;
const DOUBLE_END: usize = SINGLE_END + PTRS_PER_BLOCK * PTRS_PER_BLOCK;
/// Total number of logical blocks addressable by one inode (~513GB at 4KB blocks).
pub const MAX_LOGICAL_BLOCKS: usize = DOUBLE_END + PTRS_PER_BLOCK * PTRS_PER_BLOCK * PTRS_PER_BLOCK;

/// Location of a logical file block inside the inode's pointer tree.
///
/// Every index stored in a variant is the slot index *within* the block at that level,
/// so it is always `< PTRS_PER_BLOCK` (or `< DIRECT_BLOCKS` for `Direct`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockPath {
    /// `inode.blocks[i]`
    Direct(usize),
    /// `inode.blocks[10]` -> slot `i`
    Single(usize),
    /// `inode.blocks[11]` -> slot `a` -> slot `b`
    Double(usize, usize),
    /// `inode.triple_indirect` -> slot `a` -> slot `b` -> slot `c`
    Triple(usize, usize, usize),
}

impl BlockPath {
    /// Decomposes a logical block index. Returns `None` past the addressable limit.
    #[inline]
    pub fn from_logical(idx: usize) -> Option<Self> {
        if idx < DIRECT_BLOCKS {
            Some(BlockPath::Direct(idx))
        } else if idx < SINGLE_END {
            Some(BlockPath::Single(idx - DIRECT_BLOCKS))
        } else if idx < DOUBLE_END {
            let rel = idx - SINGLE_END;
            Some(BlockPath::Double(
                rel / PTRS_PER_BLOCK,
                rel % PTRS_PER_BLOCK,
            ))
        } else if idx < MAX_LOGICAL_BLOCKS {
            let rel = idx - DOUBLE_END;
            let rem = rel % (PTRS_PER_BLOCK * PTRS_PER_BLOCK);
            Some(BlockPath::Triple(
                rel / (PTRS_PER_BLOCK * PTRS_PER_BLOCK),
                rem / PTRS_PER_BLOCK,
                rem % PTRS_PER_BLOCK,
            ))
        } else {
            None
        }
    }

    /// Inverse of [`BlockPath::from_logical`].
    #[inline]
    pub fn to_logical(self) -> usize {
        match self {
            BlockPath::Direct(i) => i,
            BlockPath::Single(i) => DIRECT_BLOCKS + i,
            BlockPath::Double(a, b) => SINGLE_END + a * PTRS_PER_BLOCK + b,
            BlockPath::Triple(a, b, c) => {
                DOUBLE_END + a * PTRS_PER_BLOCK * PTRS_PER_BLOCK + b * PTRS_PER_BLOCK + c
            }
        }
    }

    /// True if every slot index is within the bounds of the block it indexes.
    #[inline]
    pub fn slots_in_bounds(self) -> bool {
        match self {
            BlockPath::Direct(i) => i < DIRECT_BLOCKS,
            BlockPath::Single(i) => i < PTRS_PER_BLOCK,
            BlockPath::Double(a, b) => a < PTRS_PER_BLOCK && b < PTRS_PER_BLOCK,
            BlockPath::Triple(a, b, c) => {
                a < PTRS_PER_BLOCK && b < PTRS_PER_BLOCK && c < PTRS_PER_BLOCK
            }
        }
    }
}

/// Shared property checked by both the Kani harness (symbolic `idx`) and unit tests.
#[cfg(any(test, kani))]
fn check_block_path_roundtrip(idx: usize) {
    match BlockPath::from_logical(idx) {
        Some(path) => {
            assert!(idx < MAX_LOGICAL_BLOCKS);
            assert!(path.slots_in_bounds(), "slot index out of bounds");
            assert_eq!(path.to_logical(), idx, "decomposition must be invertible");
        }
        None => {
            assert!(idx >= MAX_LOGICAL_BLOCKS);
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::*;

    /// Prove that, for EVERY possible logical block index, the pointer-tree decomposition
    /// never produces an out-of-bounds slot and is exactly invertible (no two logical
    /// blocks map to the same physical pointer slot, and none is skipped).
    #[kani::proof]
    fn proof_block_path_roundtrip_all_indices() {
        let idx: usize = kani::any();
        check_block_path_roundtrip(idx);
    }

    /// Prove that tier boundaries are exactly where the on-disk format expects them.
    #[kani::proof]
    fn proof_block_path_tier_boundaries() {
        assert_eq!(BlockPath::from_logical(9), Some(BlockPath::Direct(9)));
        assert_eq!(BlockPath::from_logical(10), Some(BlockPath::Single(0)));
        assert_eq!(BlockPath::from_logical(521), Some(BlockPath::Single(511)));
        assert_eq!(BlockPath::from_logical(522), Some(BlockPath::Double(0, 0)));
        assert_eq!(
            BlockPath::from_logical(262_665),
            Some(BlockPath::Double(511, 511))
        );
        assert_eq!(
            BlockPath::from_logical(262_666),
            Some(BlockPath::Triple(0, 0, 0))
        );
        assert_eq!(
            BlockPath::from_logical(MAX_LOGICAL_BLOCKS - 1),
            Some(BlockPath::Triple(511, 511, 511))
        );
        assert_eq!(BlockPath::from_logical(MAX_LOGICAL_BLOCKS), None);
    }

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
        assert_eq!(inode.flags, 0);
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
        assert_eq!(inode.flags, 0);
    }

    /// Prove that no block pointer in a new inode is ever non-zero.
    #[kani::proof]
    fn proof_inode_no_dangling_blocks() {
        let mode_flag: bool = kani::any();
        let mode = if mode_flag {
            FileType::File
        } else {
            FileType::Directory
        };
        let inode = Inode::new(mode);

        for i in 0..12 {
            assert_eq!(
                inode.blocks[i], 0,
                "All block pointers must be zero in a new inode"
            );
        }
        assert_eq!(
            inode.triple_indirect, 0,
            "Triple indirect block pointer must be zero in a new inode"
        );
        assert_eq!(inode.flags, 0);
    }

    /// Prove that ChunkEntry correctly roundtrips any arbitrary combination of
    /// start_block (u32), block_count (u8), flags (u8), and compressed_len (u16).
    #[kani::proof]
    fn proof_chunk_entry_roundtrip_all() {
        let start_block: u32 = kani::any();
        let block_count: u8 = kani::any();
        let flags: u8 = kani::any();
        let compressed_len: u16 = kani::any();

        let entry = ChunkEntry::new(start_block, block_count, flags, compressed_len);
        assert_eq!(entry.start_block(), start_block);
        assert_eq!(entry.block_count(), block_count);
        assert_eq!(entry.flags(), flags);
        assert_eq!(entry.compressed_len(), compressed_len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_entry_roundtrip() {
        let entry = ChunkEntry::new(1048575, 16, ChunkEntry::FLAG_COMPRESSED, 45056);
        assert_eq!(entry.start_block(), 1048575);
        assert_eq!(entry.block_count(), 16);
        assert_eq!(entry.flags(), ChunkEntry::FLAG_COMPRESSED);
        assert_eq!(entry.compressed_len(), 45056);
        assert!(!entry.is_empty());
        assert!(entry.is_compressed());
        assert!(!entry.is_raw());

        let raw_entry = ChunkEntry::new(200, 8, ChunkEntry::FLAG_RAW, 32768);
        assert_eq!(raw_entry.start_block(), 200);
        assert_eq!(raw_entry.block_count(), 8);
        assert!(raw_entry.is_raw());
        assert!(!raw_entry.is_compressed());

        let empty = ChunkEntry::EMPTY;
        assert!(empty.is_empty());
        assert_eq!(empty.start_block(), 0);
        assert_eq!(empty.block_count(), 0);
        assert_eq!(empty.compressed_len(), 0);
    }

    #[test]
    fn test_inode_creation() {
        let file_inode = Inode::new(FileType::File);
        assert_eq!(file_inode.mode, FileType::File);
        assert_eq!(file_inode.size, 0);
        assert_eq!(file_inode.compressed_size, 0);
        assert_eq!(file_inode.blocks, [0; 12]);
        assert_eq!(file_inode.triple_indirect, 0);
        assert!(!file_inode.encrypted);
        assert_eq!(file_inode.flags, 0);

        let dir_inode = Inode::new(FileType::Directory);
        assert_eq!(dir_inode.mode, FileType::Directory);
        assert_eq!(dir_inode.triple_indirect, 0);
        assert_eq!(dir_inode.flags, 0);
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
        inode.flags = INODE_FLAG_SEEKABLE_64K;

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
        assert_eq!(deserialized.flags, INODE_FLAG_SEEKABLE_64K);
    }

    #[test]
    fn test_block_path_roundtrip_boundaries_and_sweep() {
        let boundaries = [0, DIRECT_BLOCKS, SINGLE_END, DOUBLE_END, MAX_LOGICAL_BLOCKS];
        for &b in &boundaries {
            for idx in b.saturating_sub(2)..=b + 2 {
                check_block_path_roundtrip(idx);
            }
        }
        // Dense sweep through direct/single/double tiers and the start of triple.
        for idx in 0..DOUBLE_END + 3 * PTRS_PER_BLOCK * PTRS_PER_BLOCK {
            check_block_path_roundtrip(idx);
        }
        check_block_path_roundtrip(usize::MAX);
    }
}
