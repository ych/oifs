---
type: architecture
title: SuperBlock and On-Disk Layout
description: How OIFS defines its fixed on-disk physical geometry, SuperBlock magic validation, inode table sizing rules, backward compatibility guarantees, and Kani formal layout verification.
tags: [superblock, layout, on-disk, geometry, magic, backward-compatibility, kani-verified]
sources:
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
generated: { by: "pi", at: "2026-09-29T16:14:34.721Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-09-29T18:26:58.313Z
---

## Responsibility and ownership

<!-- openwiki: broken internal link [src/superblock.rs] file "src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The superblock subsystem ([`src/superblock.rs`](src/superblock.rs)) defines the foundational on-disk geometry and physical layout of an OIFS image.

<!-- openwiki: broken internal link [src/superblock.rs#L19-L48] file "src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The [`SuperBlock`](src/superblock.rs#L19-L48) resides at **Block 0** (byte offset `0..4096`) of the container image. It acts as the root configuration header, storing filesystem identification, block metrics, physical offsets to metadata regions (bitmaps and inode tables), encryption parameters, and root directory references.

## The SuperBlock structure

<!-- openwiki: broken internal link [src/superblock.rs#L19-L48] file "src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The [`SuperBlock`](src/superblock.rs#L19-L48) struct is marked `#[repr(C)]` for fixed binary alignment:

```rust
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct SuperBlock {
    pub magic: u32,
    pub block_size: u32,
    pub block_count: u64,
    pub inode_bitmap_block: u64,
    pub data_bitmap_block: u64,
    pub inode_table_block: u64,
    pub inode_count: u64,
    pub data_block_start: u64,
    pub root_inode: u64,
    pub encrypted: bool,
    pub encryption_salt: [u8; 16],
    pub encryption_version: u8,
}
```

### Magic identification

<!-- openwiki: broken internal link [src/superblock.rs#L52] file "src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The filesystem is identified by a 4-byte magic number ([`SuperBlock::MAGIC = 0x4F494653`](src/superblock.rs#L52)), which corresponds to ASCII `"OIFS"` (`0x4F` = `'O'`, `0x49` = `'I'`, `0x46` = `'F'`, `0x53` = `'S'`). During `DiskManager::open`, any file lacking this exact magic returns `DiskManagerError::InvalidMagic`, rejecting non-OIFS binary files before attempting to interpret metadata.

### Field descriptions

<!-- openwiki: broken internal link [src/lib.rs#L17] file "src/lib.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- `block_size`: Uniform filesystem block size in bytes (set to [`BLOCK_SIZE = 4096`](src/lib.rs#L17)).
- `block_count`: Total physical block capacity of the container image ($(\text{file\_size}) / 4096$).
- `inode_bitmap_block`: Fixed to Block `1`.
- `data_bitmap_block`: Fixed to Block `2`.
- `inode_table_block`: Fixed to Block `3`.
- `inode_count`: Total number of allocated inode slots in the inode table.
- `data_block_start`: First block number assigned for file and directory data payloads.
- `root_inode`: Index of the root directory inode (always `0`).
- `encrypted`: Boolean flag indicating if cryptographic AEAD encryption is active.
- `encryption_salt`: 16-byte random salt used by Argon2id to derive the master key.
- `encryption_version`: Cryptographic suite version (`1` = XChaCha20-Poly1305 + Argon2id).

## Fixed on-disk physical layout

OIFS partitions the container image into five contiguous regions:

```
┌──────────────┬──────────────┬──────────────┬────────────────────────────┬────────────────────────────┐
│ Block 0      │ Block 1      │ Block 2      │ Blocks 3 .. 1026           │ Blocks 1027 .. N           │
│ SuperBlock   │ Inode Bitmap │ Data Bitmap  │ Inode Table                │ Data Blocks                │
│ (4 KB)       │ (4 KB)       │ (4 KB)       │ (1024 blocks = 32,768 slots│ (File & Directory Content) │
└──────────────┴──────────────┴──────────────┴────────────────────────────┴────────────────────────────┘
```

1. **Block 0 (SuperBlock)**: Contains metadata, encryption salts, and block pointers to the rest of the image.
2. **Block 1 (Inode Bitmap)**: A single 4KB block containing $4096 \times 8 = 32,768$ bits. Each bit represents the allocation state (`0` = free, `1` = allocated) of an inode slot in the Inode Table.
3. **Block 2 (Data Bitmap)**: A single 4KB block containing 32,768 bits tracking data blocks starting at `data_block_start`.
4. **Blocks $3 \dots K$ (Inode Table)**: Contiguous sequence of blocks containing packed Inode entries.
5. **Blocks $K+1 \dots N$ (Data Blocks)**: Free/allocated physical 4KB blocks used to store regular file payloads, indirect pointer blocks, and directory entry lists.

## Sizing rules and layout calculation

<!-- openwiki: broken internal link [src/superblock.rs#L68-L114] file "src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Layout calculation is handled deterministically in [`SuperBlock::new`](src/superblock.rs#L68-L114):

- **Minimum Size Constraint**: A valid OIFS filesystem requires at least 5 blocks (`assert!(total_blocks >= 5)`): Block 0 (superblock), Block 1 (inode bitmap), Block 2 (data bitmap), Block 3 (at least 1 inode table block), and Block 4 (at least 1 data block).
- **Bitmap Inode Capacity**:
  $$\text{bitmap\_max\_inodes} = 4096 \times 8 = 32,768 \text{ inodes}$$
  At historical sizing ratio ($\approx 128$ bytes per slot), 1024 blocks hold the maximum 32,768 inodes:
  $$\text{inode\_table\_blocks\_cap} = 1024 \text{ blocks}$$
- **Standard Layout ($\ge 1028$ blocks, $\approx 4.1$ MB+)**:
  For standard images with sufficient capacity ($\text{total\_blocks} \ge 3 + 1024 + 1 = 1028$), OIFS assigns the full 1024 blocks to the Inode Table:
  - `inode_table_blocks = 1024`
  - `inode_count = 32,768`
  - `data_block_start = 3 + 1024 = 1027`
- **Small Image Scaling ($< 1028$ blocks)**:
  For embedded or micro-sized images below 1028 blocks, dedicating 1024 blocks would consume all space and leave zero data blocks. The builder dynamically scales down the Inode Table to consume up to 25% of available space:
  ```rust
  let inode_table_blocks = if total_blocks >= inode_table_block + inode_table_blocks_cap + 1 {
      inode_table_blocks_cap // 1024 blocks
  } else {
      (available.saturating_sub(1) / 4).min(inode_table_blocks_cap)
  };
  ```

## Backward compatibility and format stability

1. **Fixed Structural Offsets**: In standard images ($\ge 4.1$ MB), `inode_table_block` is always 3 and `data_block_start` is always 1027. Legacy tools and images created with older OIFS releases maintain identical offsets.
2. **Deterministic Bincode Serialization**: `SuperBlock` uses fixed-width integer fields (`u32`, `u64`, `[u8; 16]`). Serialization length is invariant across runs and compiler releases.
3. **Decoupled Indirect Indexing**: Large file support up to 513 GB (via single, double, and triple indirect blocks) was introduced without modifying `SuperBlock` or on-disk geometry; it utilized previously reserved block pointer fields in the `Inode` struct, ensuring 100% backward read/write compatibility with legacy images.

## Formal verification with Kani

<!-- openwiki: broken internal link [src/superblock.rs#L117-L170] file "src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
`src/superblock.rs` includes automated mathematical proofs verified with the AWS Kani Rust Verifier ([`kani_proofs`](src/superblock.rs#L117-L170)):

- **`proof_superblock_layout_ordering`**:
  Proves that across any arbitrary filesystem size between 5 and 1,000,000 blocks:
  1. `magic` is always strictly `SuperBlock::MAGIC`.
  2. Block regions never overlap and strictly preserve physical ordering:
     $$\text{superblock}(0) < \text{inode\_bitmap}(1) < \text{data\_bitmap}(2) < \text{inode\_table}(3) \le \text{data\_block\_start} \le \text{block\_count}$$
  3. Guarantees that $\text{block\_count} - \text{data\_block\_start} \ge 1$ (every valid filesystem is mathematically guaranteed to contain at least one usable data block).
- **`proof_superblock_large_fs_full_inodes`**:
  Proves that all filesystems with $\text{total\_blocks} \ge 1028$ are guaranteed to instantiate the full 32,768 inode capacity.
- **`proof_superblock_default_unencrypted`**:
  Proves that fresh superblocks default to unencrypted mode (`encrypted == false`) with zeroed salts and version tags.
