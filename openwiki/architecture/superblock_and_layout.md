---
type: "Reference"
title: "Superblock and layout"
openwiki_generated: true
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-88657ea41344918d5e874716
    resource: repo://src/encryption.rs
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---


## Responsibility and ownership

The superblock subsystem ([`src/superblock.rs`](../../src/superblock.rs)) defines the foundational on-disk geometry and physical layout of an OIFS image.

The [`SuperBlock`](../../src/superblock.rs#L21-L72) resides at **Block 0** (byte offset `0..4096`) of the container image. It acts as the root configuration header, storing filesystem identification, block metrics, physical offsets to metadata regions (bitmaps and inode tables), encryption parameters, and root directory references.

## The SuperBlock structure

The [`SuperBlock`](../../src/superblock.rs#L21-L72) struct is marked `#[repr(C)]` for fixed binary alignment:

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
    pub format_version: u32,
    pub migration_cursor: u64,
}
```

### Magic identification

The filesystem is identified by a 4-byte magic number ([`SuperBlock::MAGIC = 0x4F494653`](../../src/superblock.rs#L77)), which corresponds to ASCII `"OIFS"` (`0x4F` = `'O'`, `0x49` = `'I'`, `0x46` = `'F'`, `0x53` = `'S'`). During `DiskManager::open`, any file lacking this exact magic returns `DiskManagerError::InvalidMagic`, rejecting non-OIFS binary files before attempting to interpret metadata.

### Field descriptions

| Field | Description |
|-------|-------------|
| `magic` | Fixed value `0x4F494653` (`"OIFS"` in ASCII) used to validate the filesystem type. |
| `block_size` | Uniform filesystem block size in bytes (set to [`BLOCK_SIZE = 4096`](../../src/lib.rs#L17)). |
| `block_count` | Total physical block capacity of the container image (`file_size / 4096`). |
| `inode_bitmap_block` | Block ID where the inode bitmap starts (legacy: fixed Block `1`; v3: variable). |
| `data_bitmap_block` | Block ID where the data bitmap starts (legacy: fixed Block `2`; v3: variable). |
| `inode_table_block` | Block ID where the inode table starts (legacy/journaled: fixed; v3: calculated). |
| `inode_count` | Total number of allocated inode slots in the inode table. |
| `data_block_start` | First block number assigned for file and directory data payloads. |
| `root_inode` | Index of the root directory inode (always `0`). |
| `encrypted` | Boolean flag indicating if cryptographic AEAD encryption is active. |
| `encryption_salt` | 16-byte random salt used by Argon2id to derive the master key. |
| `encryption_version` | Cryptographic suite version (`1` = legacy XChaCha20-Poly1305 without AAD, `2` = context-bound with associated data). |
| `format_version` | On-disk format version. Determines the inode encoding layout and bitmap sizing strategy. Must remain the last field to preserve backward compatibility. |
| `migration_cursor` | During format migration, indicates which inode IDs have been converted to the new format. Zero means no migration in progress. |

### Format versions

| Version | Constant | Meaning |
|---------|----------|---------|
| 0 (from zero padding) | — | Pre-versioning image; treated as legacy |
| 1 | `FORMAT_VERSION_LEGACY` | Legacy `bincode` inode encoding, single-block bitmaps |
| 2 | `FORMAT_VERSION_FIXED_INODE` | Fixed 256-byte inode records, single-block bitmaps |
| 3 | `FORMAT_VERSION_V3_LAYOUT` | **v3 layout** — multi-block bitmaps sized to image, 256-byte inode records |

The `format_version` field is **appended last** so that pre-versioning images (which lack this field entirely) decode with `format_version == 0` from zero padding, which correctly classifies them as legacy. Adding the field in the middle would shift every subsequent field and silently corrupt every existing image.

### Inode format dispatch

During a migration the superblock still advertises the legacy version so an interrupted migration stays readable, while slots below `migration_cursor` already hold current-format records:

```rust
pub fn inode_record_version_for(&self, inode_id: u64) -> u32 {
    if !self.is_legacy_inode_format() || inode_id < self.migration_cursor {
        Self::FORMAT_VERSION_FIXED_INODE
    } else {
        Self::FORMAT_VERSION_LEGACY
    }
}
```

## Encryption schemes

Two encryption schemes are supported, identified by `encryption_version`:

| Version | Constant | Description |
|---------|----------|-------------|
| 1 | `ENCRYPTION_SCHEME_LEGACY` | XChaCha20-Poly1305 **without associated data**. Every file shares the same master key; nothing but the nonce distinguishes ciphertexts. An attacker with write access could swap or roll back chunks undetected. |
| 2 | `ENCRYPTION_SCHEME_CONTEXT_BOUND` | XChaCha20-Poly1305 **with associated data (AAD)** binding ciphertext to file/chunk/filename context. AAD includes a domain separator (`"OIFS-AAD-v1"`), a kind byte (`FILE=0x01`, `CHUNK=0x02`, `FILENAME=0x03`), the inode ID, and for chunks the chunk index and flags. A chunk cannot be moved to another file, reordered, or rolled back without authentication failure. |

When `encrypted == false`, `encryption_version` must be `0`. When `encrypted == true`, `encryption_version` must be `1` or `2`. Unknown versions are rejected by validation.

## On-disk layout variants

OIFS supports three layout families distinguished by `format_version` and geometry:

### 1. Legacy layout (`format_version < 3`, non-journaled)

Fixed single-block bitmaps, 128-byte logical inode sizing (historical):

```
┌──────────────┬──────────────┬──────────────┬────────────────────────────┬────────────────────────────┐
│ Block 0      │ Block 1      │ Block 2      │ Blocks 3 .. 1026           │ Blocks 1027 .. N           │
│ SuperBlock   │ Inode Bitmap │ Data Bitmap  │ Inode Table (1024 blocks)  │ Data Blocks                │
│ (4 KB)       │ (4 KB)       │ (4 KB)       │ (32,768 slots @ 128 B)     │ (File & Directory Content) │
└──────────────┴──────────────┴──────────────┴────────────────────────────┴────────────────────────────┘
```

### 2. v3 layout (`format_version == 3`, non-journaled)

Multi-block bitmaps sized to image capacity, 256-byte inode records (16 inodes/block). The inode table keeps the historical 1024-block maximum, yielding 16,384 inodes. Bitmaps grow as needed:

```
┌──────────────┬──────────────┬──────────────┬──────────────┬────────────────────────────┬────────────────────────────┐
│ Block 0      │ Block 1..    │ Block ..     │ Block ..     │ Blocks ..                  │ Blocks ..                  │
│ SuperBlock   │ Inode Bitmap │ Data Bitmap  │ Inode Table  │ Data Blocks                │                            │
│ (4 KB)       │ (variable)   │ (variable)   │ (≤1024 blks) │ (File & Directory Content) │                            │
└──────────────┴──────────────┴──────────────┴──────────────┴────────────────────────────┴────────────────────────────┘
          ↑
     inode_bitmap_block=1
```

The v3 layout computes bitmap spans in a single pass (no iterative fixed-point to keep CBMC tractable):

```rust
let metadata_blocks = 1 + 1 + 1 + journal_blocks; // superblock + inode_bmp + data_bmp + journal
let inode_table_blocks = if total_blocks > metadata_blocks + 1024 {
    1024
} else {
    (total_blocks - metadata_blocks - 1) / 4
};
let inode_count = inode_table_blocks * 16; // 256-byte slots
let inode_bitmap_blocks = inode_count.div_ceil(32768).max(1);
let data_blocks = total_blocks - 1 - inode_bitmap_blocks - 1 - journal_blocks - inode_table_blocks;
let data_bitmap_blocks = data_blocks.div_ceil(32768).max(1);
```

### 3. Journaled layout (`format_version == 3`, journaled)

Identical to v3 but reserves **33 blocks** (1 header + 32 ring) for the metadata WAL between the data bitmap and inode table. The journal header lives at `data_bitmap_block + data_bitmap_blocks`, and the inode table starts at `journal_header_block + 33`. Detected via `inode_table_block >= 36` (i.e., `3 + JOURNAL_RESERVED_BLOCKS`).

```
┌──────────────┬──────────────┬──────────────┬──────────────┬──────────────────────┬──────────────────────┬──────────────────────┐
│ Block 0      │ Block 1..    │ Block ..     │ Block ..     │ Blocks ..            │ Blocks ..            │ Blocks ..            │
│ SuperBlock   │ Inode Bitmap │ Data Bitmap  │ Journal Hdr  │ Journal Ring (32 blks)│ Inode Table         │ Data Blocks          │
│ (4 KB)       │ (variable)   │ (variable)   │ (1 block)    │ (32 blocks = 128 KB) │ (≤1024 blocks)      │ (File & Directory)   │
└──────────────┴──────────────┴──────────────┴──────────────┴──────────────────────┴──────────────────────┴──────────────────────┘
```

The journaled layout is **100% backward compatible**: the `SuperBlock` struct is unchanged; a journaled image is distinguished purely by its geometry (`inode_table_block >= 36`) plus the magic word `0x4A52_4E4C` (`"JRNL"`) inside the journal header block. Non-journaled images behave exactly as before.

### Mermaid block diagram: layout comparison

```mermaid
flowchart TD
    subgraph LEGACY["Legacy / v2 Layout (format_version < 3)"]
        L0["Block 0: SuperBlock"]
        L1["Block 1: Inode Bitmap (1 block)"]
        L2["Block 2: Data Bitmap (1 block)"]
        L3["Blocks 3-1026: Inode Table (1024 blocks, 32,768 inodes @ 128 B)"]
        L4["Blocks 1027+: Data Area"]
        L0 --> L1 --> L2 --> L3 --> L4
    end

    subgraph V3["v3 Layout (format_version = 3, non-journaled)"]
        V0["Block 0: SuperBlock"]
        V1["Block 1+: Inode Bitmap (variable, ≥1 block)"]
        V2["Block ...: Data Bitmap (variable, ≥1 block)"]
        V3["Block ...: Inode Table (≤1024 blocks, 16,384 inodes @ 256 B)"]
        V4["Block ...: Data Area"]
        V0 --> V1 --> V2 --> V3 --> V4
    end

    subgraph JOURNALED["Journaled v3 Layout (format_version = 3, journaled)"]
        J0["Block 0: SuperBlock"]
        J1["Block 1+: Inode Bitmap (variable)"]
        J2["Block ...: Data Bitmap (variable)"]
        J3["Block ...: Journal Header (1 block, magic=JRNL)"]
        J4["Blocks ...: Journal Ring (32 blocks = 128 KB)"]
        J5["Block ...: Inode Table (≤1024 blocks)"]
        J6["Block ...: Data Area"]
        J0 --> J1 --> J2 --> J3 --> J4 --> J5 --> J6
    end

    classDef region fill:#f9f9f9,stroke:#333,stroke-width:1px;
    class LEGACY,V3,JOURNALED region;
```

**Caption:** Physical block layout comparison. Legacy uses fixed 1-block bitmaps and 128-byte logical inodes. v3 introduces variable-size bitmaps and 256-byte fixed inode records. Journaled inserts a 33-block WAL region (1 header + 32 ring) between the data bitmap and inode table.

## Sizing rules and layout calculation

Layout calculation is handled deterministically in [`SuperBlock::new_v3`](../../src/superblock.rs#L152-L204) (v3) and [`SuperBlock::new_with_layout`](../../src/superblock.rs#L206-L250) (legacy entry point used by journaled builder).

### Minimum size constraint

A valid OIFS filesystem requires at least **5 blocks**: Block 0 (superblock), Block 1 (inode bitmap), Block 2 (data bitmap), 1 inode table block, and 1 data block. Journaled images require at least `3 + JOURNAL_RESERVED_BLOCKS + 2 = 37` blocks for the same minimum metadata plus the journal region.

### Inode table capacity

- **Legacy**: 128 bytes/inode → 32 inodes/block → 1024 blocks = 32,768 inodes
- **v3**: 256 bytes/inode → 16 inodes/block → 1024 blocks = 16,384 inodes

The `inode_count` field reports the logical count; `inode_table_capacity()` returns the physical slots that fit between `inode_table_block` and `data_block_start`. `usable_inode_count()` bounds by the smaller of `inode_count`, table capacity, and `MAX_TRACKED_INODES (32,768)`.

### Data area sizing

`data_block_start = inode_table_block + inode_table_blocks`. The data bitmap spans enough blocks to cover `data_blocks = block_count - data_block_start`.

## SuperBlock validation logic

[`SuperBlock::validate`](../../src/superblock.rs#L430-L500) performs exhaustive bounds, geometry, and alignment checks against the mapped image size. It is called during `DiskManager::open` before any metadata is trusted.

### Validation checklist

| Check | Error message |
|-------|---------------|
| Magic number equals `0x4F494653` | "Invalid filesystem magic number" |
| Block size equals 4096 | "Invalid block size (must be 4096)" |
| Total blocks ≥ 5 | "Total block count too small (minimum 5 blocks)" |
| `block_count * 4096` fits in mapped file | "Filesystem block count exceeds mapped file image size" |
| `inode_bitmap_block == 1` | "Invalid inode bitmap block (must be block 1)" |
| `data_bitmap_block == 2` | "Invalid data bitmap block (must be block 2)" |
| `inode_table_block > data_bitmap_block` | "Inode table block overlaps bitmaps or superblock" |
| `inode_table_block ≤ data_block_start` | "Inode table block start exceeds data block start" |
| `data_block_start ≤ block_count` | "Data block start exceeds total block count" |
| `data_block_start < block_count` | "No data blocks available in filesystem" |
| `inode_count > 0` | "Inode count cannot be zero" |
| `inode_count ≤ 32,768` | "Inode count exceeds the capacity of the inode bitmap" |
| Inode table fits before data area (`table_end ≤ data_block_start`) | "Inode table does not fit before the data area" |
| `root_inode < usable_inode_count()` | "Root inode ID exceeds the usable inode count" |
| `format_version ≤ 3` | "Unsupported filesystem format version" |
| `migration_cursor ≤ usable_inode_count()` | "Migration cursor exceeds usable inode count" |
| If encrypted: `encryption_version ∈ {1,2}` | "Unsupported encryption scheme version" |
| If not encrypted: `encryption_version == 0` | "Unencrypted filesystem has non-zero encryption version" |
| **v3-specific**: `data_bitmap_block == inode_bitmap_block + inode_bitmap_blocks()` | "Data bitmap does not follow the inode bitmap" |
| **v3-specific**: `inode_table_block == data_bitmap_block + data_bitmap_blocks() + journal_blocks` | "Inode table does not follow the data bitmap" |

The v3-specific checks derive `inode_bitmap_blocks()` and `data_bitmap_blocks()` from the recorded counts and verify the implicit layout matches the explicit offsets. This guarantees that a corrupted or hand-crafted superblock cannot claim a v3 layout while actually using legacy geometry.

## Journal layout detection

A journaled image is identified by geometry, not by a superblock flag:

```rust
pub fn has_journal_layout(&self) -> bool {
    if self.is_v3_layout() {
        let without_journal = 1 + self.inode_bitmap_blocks() + self.data_bitmap_blocks();
        self.inode_table_block == without_journal + JOURNAL_RESERVED_BLOCKS
    } else {
        crate::journal::is_journaled_layout(self) // legacy: inode_table_block >= 36
    }
}
```

The journal header block is at `data_bitmap_block + data_bitmap_blocks()` for v3, or fixed Block 3 for legacy. The ring comprises 32 blocks (128 KB) following the header.

## Formal verification with Kani

`src/superblock.rs` includes automated mathematical proofs verified with the AWS Kani Rust Verifier ([`kani_proofs` module](../../src/superblock.rs#L600-L680)):

| Proof | Property verified |
|-------|-------------------|
| `proof_superblock_layout_ordering` | Across any filesystem size 5..1,000,000 blocks: magic is correct, regions never overlap, physical ordering preserved, ≥1 data block guaranteed |
| `proof_v3_layout_keeps_the_legacy_geometry` | `SuperBlock::new` and `new_with_layout(..., 3)` produce identical geometry for non-journaled images; `has_journal_layout` is false |
| `proof_journaled_layout_non_overlapping` | Journaled layout: journal header at Block 3, ring after it, inode table ≥ Block 36, bitmaps before journal, all regions within image bounds |
| `proof_journaled_shifts_data_start` | Journaled image of same size has strictly larger `data_block_start` than legacy, same `inode_count` (capacity unchanged, only placement moves) |
| `proof_superblock_validate_soundness` | Validly initialized superblocks (both standard and journaled) always pass `validate()` |

## Backward compatibility and format stability

1. **Fixed structural offsets**: In standard non-journaled images (≥ 1028 blocks ≈ 4.1 MB), `inode_table_block` is always 3 and `data_block_start` is always 1027. Legacy tools and images maintain identical offsets.
2. **Deterministic bincode serialization**: `SuperBlock` uses fixed-width integer fields (`u32`, `u64`, `[u8; 16]`). Serialization length is invariant across runs and compiler releases.
3. **Appended-only fields**: `format_version` and `migration_cursor` are appended at the end. Pre-versioning images decode with zero padding → `format_version == 0` → legacy classification. Adding fields in the middle would shift offsets and corrupt every existing image.
4. **Decoupled indirect indexing**: Large file support up to 513 GB (via single, double, and triple indirect blocks) was introduced without modifying `SuperBlock` or on-disk geometry; it utilized previously reserved block pointer fields in the `Inode` struct.
5. **Journaling is opt-in geometry**: The `SuperBlock` struct is unchanged. A journaled image is distinguished by `inode_table_block ≥ 36` plus the `JRNL` magic in the journal header. Non-journaled images behave exactly as before.
6. **Migration cursor**: During in-place format migration (v1 → v2 inode encoding), `migration_cursor` tracks progress. Slots below the cursor use the new format; slots at or above use the old format. A crash leaves a cleanly readable mixed image. `format_version` remains legacy until migration completes, then flips to current.
