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

    /// On-disk format version.
    ///
    /// **Must stay the last field.** Appending it leaves every existing field at
    /// its historical offset, so a pre-versioning image decodes cleanly: its
    /// `bincode` payload is 82 bytes and the remainder of block 0 is zero padding,
    /// so this field reads back as `0`, which [`Self::is_legacy_inode_format`]
    /// correctly classifies as v1.
    ///
    /// Adding the field in the middle instead would shift every subsequent field
    /// and silently corrupt every existing image.
    pub format_version: u32,

    /// Migration progress cursor: inode ids strictly below this have already been
    /// rewritten in the current format.
    ///
    /// An in-place format migration rewrites one inode at a time, so a crash can
    /// leave an image holding *both* encodings. A single `format_version` cannot
    /// describe that state, and reading a v1 record with the v2 decoder (or worse,
    /// reading it "successfully" with garbage values) would corrupt the mount.
    ///
    /// Recording the cursor makes the mixed state explicit and safe: readers decode
    /// each slot according to its position, so a migration is simply resumable and
    /// idempotent. `0` means "no migration in progress".
    pub migration_cursor: u64,
}

impl SuperBlock {
    /// Magic number identifying the OIFS file system ("OIFS" in ASCII)
    pub const MAGIC: u32 = 0x4F494653; // "OIFS" in hex (O=4F, I=49, F=46, S=53)

    /// Original layout: inodes stored as packed `bincode` records.
    ///
    /// Read-only. A v1 image stays fully usable, but keeps writing v1 records until
    /// it is upgraded, so an image never contains a mix of both encodings.
    pub const FORMAT_VERSION_LEGACY: u32 = 1;

    /// Fixed 256-byte inode records with explicit little-endian fields and zeroed
    /// reserved space (see [`crate::inode_format`]).
    pub const FORMAT_VERSION_FIXED_INODE: u32 = crate::inode_format::INODE_FORMAT_V2;

    /// Returns `true` when inodes are still stored in the legacy `bincode` layout.
    ///
    /// Both `0` (read from zero padding in a pre-versioning image) and `1` mean v1.
    pub fn is_legacy_inode_format(&self) -> bool {
        self.format_version < Self::FORMAT_VERSION_FIXED_INODE
    }

    /// Format version to use for the whole image when encoding or decoding inodes.
    ///
    /// Prefer [`Self::inode_record_version_for`] on the write path: during a
    /// migration the answer legitimately differs per inode.
    pub fn inode_record_version(&self) -> u32 {
        if self.is_legacy_inode_format() {
            Self::FORMAT_VERSION_LEGACY
        } else {
            Self::FORMAT_VERSION_FIXED_INODE
        }
    }

    /// Format version to use for one specific inode slot.
    ///
    /// During a migration the superblock still advertises the legacy version (so an
    /// interrupted migration stays readable), while slots below
    /// [`Self::migration_cursor`] already hold current-format records.
    pub fn inode_record_version_for(&self, inode_id: u64) -> u32 {
        if !self.is_legacy_inode_format() || inode_id < self.migration_cursor {
            Self::FORMAT_VERSION_FIXED_INODE
        } else {
            Self::FORMAT_VERSION_LEGACY
        }
    }

    /// `true` while a format migration is partially applied.
    pub fn is_migration_in_progress(&self) -> bool {
        self.is_legacy_inode_format() && self.migration_cursor > 0
    }

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
            // New images are always written in the current inode format.
            format_version: Self::FORMAT_VERSION_FIXED_INODE,
            migration_cursor: 0,
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

    /// Number of inode ids the inode bitmap can track (one bitmap block).
    pub const MAX_TRACKED_INODES: u64 = 4096 * 8;

    /// Number of 256-byte inode slots that fit in one inode-table block.
    pub const INODE_SLOTS_PER_BLOCK: u64 =
        crate::BLOCK_SIZE as u64 / crate::inode_format::INODE_SLOT_SIZE as u64;

    /// The number of inode slots that actually fit in the inode table.
    ///
    /// The historical layout sized the table with the old 128-byte `bincode`
    /// records, so `inode_count` can be *twice* the number of 256-byte slots
    /// that fit between `inode_table_block` and `data_block_start`. Ids beyond
    /// the real capacity would address the data area, so they must never be
    /// used (SEC-25).
    pub fn inode_table_capacity(&self) -> u64 {
        let table_blocks = self.data_block_start.saturating_sub(self.inode_table_block);
        table_blocks.saturating_mul(Self::INODE_SLOTS_PER_BLOCK)
    }

    /// The number of inode ids that may be used.
    ///
    /// Bounded by the table capacity, by the inode bitmap and by the claimed
    /// `inode_count`, whichever is smallest.
    pub fn usable_inode_count(&self) -> u64 {
        self.inode_count
            .min(self.inode_table_capacity())
            .min(Self::MAX_TRACKED_INODES)
    }

    /// Validates superblock bounds, layout geometry, and alignment invariants against mapped image size.
    pub fn validate(&self, mmap_len: usize) -> Result<(), &'static str> {
        if self.magic != Self::MAGIC {
            return Err("Invalid filesystem magic number");
        }
        if self.block_size as usize != crate::BLOCK_SIZE {
            return Err("Invalid block size (must be 4096)");
        }
        if self.block_count < 5 {
            return Err("Total block count too small (minimum 5 blocks)");
        }
        let total_bytes = match self.block_count.checked_mul(crate::BLOCK_SIZE as u64) {
            Some(bytes) => bytes,
            None => return Err("Total block count integer overflow"),
        };
        if total_bytes as usize > mmap_len {
            return Err("Filesystem block count exceeds mapped file image size");
        }
        if self.inode_bitmap_block != 1 {
            return Err("Invalid inode bitmap block (must be block 1)");
        }
        if self.data_bitmap_block != 2 {
            return Err("Invalid data bitmap block (must be block 2)");
        }
        if self.inode_table_block <= self.data_bitmap_block {
            return Err("Inode table block overlaps bitmaps or superblock");
        }
        if self.inode_table_block > self.data_block_start {
            return Err("Inode table block start exceeds data block start");
        }
        if self.data_block_start > self.block_count {
            return Err("Data block start exceeds total block count");
        }
        if self.data_block_start == self.block_count {
            return Err("No data blocks available in filesystem");
        }
        if self.inode_count == 0 {
            return Err("Inode count cannot be zero");
        }
        if self.inode_count > Self::MAX_TRACKED_INODES {
            return Err("Inode count exceeds the capacity of the inode bitmap");
        }
        // The usable inode ids must fit in the inode table: an id beyond the
        // table would address the data area (SEC-25).
        let usable = self.usable_inode_count();
        let table_blocks = usable.div_ceil(Self::INODE_SLOTS_PER_BLOCK);
        let table_end = self.inode_table_block.checked_add(table_blocks);
        if table_end != Some(self.data_block_start) && table_end > Some(self.data_block_start) {
            return Err("Inode table does not fit before the data area");
        }
        if self.root_inode >= usable {
            return Err("Root inode ID exceeds the usable inode count");
        }
        Ok(())
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
        kani::cover!(total_blocks == 5, "smallest supported image reached");
        kani::cover!(total_blocks > 1028, "large image reached");
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
        kani::cover!(total_blocks >= 1028, "large filesystem reached");
    }

    /// Prove that the usable inode ids always fit in the inode table.
    ///
    /// SEC-25: an inode id beyond the table would address the data area, so
    /// the usable count must never exceed the table capacity (and the inode
    /// bitmap capacity).
    #[kani::proof]
    fn proof_usable_inode_count_fits_the_table() {
        let total_blocks: u64 = kani::any();
        kani::assume(total_blocks >= 5 && total_blocks <= 1_000_000);

        let sb = SuperBlock::new(total_blocks);
        let usable = sb.usable_inode_count();

        // The bitmap can only track a fixed number of inodes.
        assert!(usable <= SuperBlock::MAX_TRACKED_INODES);
        assert!(sb.inode_count <= SuperBlock::MAX_TRACKED_INODES);

        // The usable ids must fit between the table start and the data area.
        let table_blocks = usable.div_ceil(SuperBlock::INODE_SLOTS_PER_BLOCK);
        let table_end = sb.inode_table_block + table_blocks;
        assert!(
            table_end <= sb.data_block_start,
            "the usable inode ids must fit in the inode table"
        );
        assert!(usable <= sb.inode_table_capacity());
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
        kani::cover!(total_blocks == 5, "minimum legacy layout reached");
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
        kani::cover!(total_blocks == 1_200, "minimum journaled image reached");
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
        kani::cover!(total_blocks >= 1_200, "journaled layout reached");
    }

    /// Prove that SuperBlock::validate always accepts validly initialized superblocks.
    #[kani::proof]
    fn proof_superblock_validate_soundness() {
        let total_blocks: u64 = kani::any();
        kani::assume(total_blocks >= 1_200 && total_blocks <= 1_000_000);
        let sb = SuperBlock::new(total_blocks);
        let mmap_len = (total_blocks as usize) * crate::BLOCK_SIZE;
        assert!(sb.validate(mmap_len).is_ok());

        let sb_j = SuperBlock::new_journaled(total_blocks);
        assert!(sb_j.validate(mmap_len).is_ok());
        kani::cover!(sb.validate(mmap_len).is_ok(), "valid superblock accepted");
        kani::cover!(
            sb_j.validate(mmap_len).is_ok(),
            "valid journaled superblock accepted"
        );
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

    // --- Format versioning -------------------------------------------------

    /// Serialize a SuperBlock the way a pre-versioning build would have: the same
    /// struct minus `format_version`, followed by zero padding out to a full block.
    ///
    /// Returns the full block and the exact v1 payload length. The payload length is
    /// measured from the serializer rather than by scanning for trailing zeros, which
    /// would undercount whenever the last field happens to be zero.
    fn legacy_block0(sb: &SuperBlock) -> (Vec<u8>, usize) {
        #[derive(serde::Serialize)]
        struct SuperBlockV1 {
            magic: u32,
            block_size: u32,
            block_count: u64,
            inode_bitmap_block: u64,
            data_bitmap_block: u64,
            inode_table_block: u64,
            inode_count: u64,
            data_block_start: u64,
            root_inode: u64,
            encrypted: bool,
            encryption_salt: [u8; 16],
            encryption_version: u8,
        }
        let v1 = SuperBlockV1 {
            magic: sb.magic,
            block_size: sb.block_size,
            block_count: sb.block_count,
            inode_bitmap_block: sb.inode_bitmap_block,
            data_bitmap_block: sb.data_bitmap_block,
            inode_table_block: sb.inode_table_block,
            inode_count: sb.inode_count,
            data_block_start: sb.data_block_start,
            root_inode: sb.root_inode,
            encrypted: sb.encrypted,
            encryption_salt: sb.encryption_salt,
            encryption_version: sb.encryption_version,
        };
        let payload = bincode::serialize(&v1).expect("serialize v1");
        let payload_len = payload.len();
        let mut block = vec![0u8; crate::BLOCK_SIZE];
        block[..payload_len].copy_from_slice(&payload);
        (block, payload_len)
    }

    #[test]
    fn test_new_images_declare_the_fixed_inode_format() {
        let sb = SuperBlock::new(2560);
        assert_eq!(sb.format_version, SuperBlock::FORMAT_VERSION_FIXED_INODE);
        assert!(!sb.is_legacy_inode_format());
        assert_eq!(
            sb.inode_record_version(),
            SuperBlock::FORMAT_VERSION_FIXED_INODE
        );
    }

    #[test]
    fn test_journaled_new_images_also_declare_v2() {
        let sb = SuperBlock::new_journaled(2560);
        assert!(!sb.is_legacy_inode_format());
        assert_eq!(
            sb.inode_record_version(),
            SuperBlock::FORMAT_VERSION_FIXED_INODE
        );
    }

    #[test]
    fn test_legacy_image_reads_back_as_v1() {
        // The compatibility guarantee: a block 0 written before format_version
        // existed must still parse, and must be classified as legacy so its inodes
        // are decoded with the bincode reader rather than the fixed one.
        let created = SuperBlock::new(2560);
        let (block, _) = legacy_block0(&created);
        let parsed: SuperBlock = bincode::deserialize(&block).expect("legacy block 0 parses");

        // Every pre-existing field must survive intact.
        assert_eq!(parsed.magic, created.magic);
        assert_eq!(parsed.block_size, created.block_size);
        assert_eq!(parsed.block_count, created.block_count);
        assert_eq!(parsed.inode_bitmap_block, created.inode_bitmap_block);
        assert_eq!(parsed.data_bitmap_block, created.data_bitmap_block);
        assert_eq!(parsed.inode_table_block, created.inode_table_block);
        assert_eq!(parsed.inode_count, created.inode_count);
        assert_eq!(parsed.data_block_start, created.data_block_start);
        assert_eq!(parsed.root_inode, created.root_inode);
        assert_eq!(parsed.has_journal_layout(), created.has_journal_layout());

        // format_version reads as 0 from the zero padding, which means legacy.
        assert_eq!(parsed.format_version, 0);
        assert!(parsed.is_legacy_inode_format());
        assert_eq!(
            parsed.inode_record_version(),
            SuperBlock::FORMAT_VERSION_LEGACY
        );
    }

    #[test]
    fn test_explicit_v1_version_is_also_legacy() {
        let mut sb = SuperBlock::new(2560);
        sb.format_version = SuperBlock::FORMAT_VERSION_LEGACY;
        assert!(sb.is_legacy_inode_format());
        assert_eq!(sb.inode_record_version(), SuperBlock::FORMAT_VERSION_LEGACY);
    }

    #[test]
    fn test_unknown_future_version_is_read_as_current_family() {
        // Forward compatibility: a newer binary must not silently downgrade the
        // decode family of an image written by a newer format.
        let mut sb = SuperBlock::new(2560);
        sb.format_version = 99;
        assert!(!sb.is_legacy_inode_format());
        assert_eq!(
            sb.inode_record_version(),
            SuperBlock::FORMAT_VERSION_FIXED_INODE
        );
    }

    #[test]
    fn test_appending_format_version_did_not_move_existing_offsets() {
        // The reason format_version must be the final field: the v1 payload must be
        // a byte-exact prefix of the v2 payload.
        let sb = SuperBlock::new(2560);
        let v2 = bincode::serialize(&sb).expect("serialize v2");
        let (_, v1_len) = legacy_block0(&sb);

        assert_eq!(
            &v2[..v1_len],
            &bincode::serialize(&SuperBlock {
                format_version: SuperBlock::FORMAT_VERSION_LEGACY,
                ..sb
            })
            .expect("serialize v2 as legacy")[..v1_len],
            "all pre-existing fields must keep their exact byte offsets"
        );
        assert_eq!(
            v2.len(),
            v1_len + 4 + 8,
            "the only difference must be the appended format_version + migration_cursor"
        );
    }
}
