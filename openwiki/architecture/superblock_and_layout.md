---
type: "Reference"
title: "Superblock & On-Disk Layout"
description: "Comprehensive documentation of the OIFS superblock structure, format versioning (v1/v2/v3), block layout geometry, journal region placement, encryption fields, and migration cursor mechanics for in-place format upgrades."
tags: ["superblock", "on-disk-format", "layout", "journal", "encryption", "migration", "fsck"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-115024023ade9f243c6b4ec3
    resource: repo://src/disk/maintenance.rs
  - id: openwiki-source-88657ea41344918d5e874716
    resource: repo://src/encryption.rs
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

## Responsibility and ownership

The superblock subsystem ([`src/superblock.rs`](../../src/superblock.rs)) defines the foundational on-disk geometry and physical layout of an OIFS image.

The [`SuperBlock`](../../src/superblock.rs#L21-L72) resides at **Block 0** (byte offset `0..4096`) of the container image. It acts as the root configuration header, storing filesystem identification, block metrics, physical offsets to metadata regions (bitmaps and inode tables), encryption parameters, root directory references, and format versioning state.

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
| `format_version` | On-disk format version. Determines the inode encoding layout and bitmap sizing strategy. Must remain the last field before `migration_cursor` to preserve backward compatibility. |
| `migration_cursor` | During format migration, indicates which inode IDs have been converted to the new format. Zero means no migration in progress. |

### Format versions

| Version | Constant | Meaning |
|---------|----------|---------|
| 0 (from zero padding) | — | Pre-versioning image; treated as legacy |
| 1 | `FORMAT_VERSION_LEGACY` | Legacy `bincode` inode encoding, single-block bitmaps |
| 2 | `FORMAT_VERSION_FIXED_INODE` | Fixed 256-byte inode records, single-block bitmaps |
| 3 | `FORMAT_VERSION_V3_LAYOUT` | **v3 layout** — multi-block bitmaps sized to image, 256-byte inode records, optional journal region |

The `format_version` field is **appended before `migration_cursor`** so that pre-versioning images (which lack this field entirely) decode with `format_version == 0` from zero padding, which correctly classifies them as legacy. Adding the field in the middle would shift every subsequent field and silently corrupt every existing image.

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

This per-inode dispatch means a mixed-format image is **safe and resumable**, not corrupt.

---

## v3 Layout (format_version = 3)

The v3 layout introduces **multi-block bitmaps** sized to image capacity, removing the 128 MiB / 32K inode caps that constrained v1/v2. Bitmap spans are **implicit** in the recorded block offsets and validated by [`SuperBlock::validate()`](../../src/superblock.rs#L508-L588).

### Layout regions

The v3 metadata regions are laid out sequentially:

```
[superblock][inode bitmap: N][data bitmap: M][journal: J][inode table: T][data area]
```

Where:
- **Superblock**: Block 0 (always)
- **Inode bitmap**: Starts at Block 1, spans `N = ceil(inode_count / BITMAP_BLOCK_BITS)` blocks
- **Data bitmap**: Starts immediately after inode bitmap, spans `M = ceil(data_blocks / BITMAP_BLOCK_BITS)` blocks
- **Journal (optional)**: `J = JOURNAL_RESERVED_BLOCKS = 33` blocks (1 header + 32 ring) when `journaled=true`
- **Inode table**: Starts after data bitmap (and journal if present), spans `T` blocks (capped at 1024 blocks = 16,384 inodes at 256 bytes/record)
- **Data area**: Everything after the inode table

### Layout computation in `SuperBlock::new_v3`

The layout is computed in a **single pass** (no iterative fixed-point) to keep CBMC proofs tractable:

1. **Metadata overhead**: `1 (superblock) + inode_bitmap_blocks + 1 (data bitmap placeholder) + journal_blocks`
2. **Inode table**: Historical cap of 1024 blocks (16,384 inodes at 256 bytes/record). If space is tight, sized to ~25% of available space.
3. **Inode count**: `inode_table_blocks * INODE_SLOTS_PER_BLOCK` (where `INODE_SLOTS_PER_BLOCK = 4096 / 256 = 16`)
4. **Inode bitmap blocks**: `ceil(inode_count / BITMAP_BLOCK_BITS)`, minimum 1
5. **Data blocks**: Remaining blocks after all metadata
6. **Data bitmap blocks**: `ceil(data_blocks / BITMAP_BLOCK_BITS)`, minimum 1
6. **Block offsets**: Computed sequentially from the spans above

Minimum size: **5 blocks** for non-journaled; **37+ blocks** for journaled (requires 33-block WAL region).

### Key constants

| Constant | Value | Meaning |
|----------|-------|---------|
| `BITMAP_BLOCK_BITS` | 32,768 | Bits per bitmap block (4096 × 8) |
| `MAX_TRACKED_INODES` | 32,768 | Maximum inodes the inode bitmap can track |
| `INODE_SLOTS_PER_BLOCK` | 16 | 256-byte inode records per 4096-byte block |
| `JOURNAL_RESERVED_BLOCKS` | 33 | 1 header + 32 ring blocks |
| `FORMAT_VERSION_V3_LAYOUT` | 3 | Format version marker |

---

## Journal Region Detection

Journal presence is detected **purely by geometry** — the `SuperBlock` struct is **identical** between journaled and non-journaled images. No extra flag is needed.

### Legacy/journaled (v1/v2 layout)

Fixed layout: inode table starts at block 36 (`JOURNAL_INODE_TABLE_BLOCK = 3 + 33`). Detected by:

```rust
pub fn is_journaled_layout(sb: &SuperBlock) -> bool {
    sb.inode_table_block >= JOURNAL_INODE_TABLE_BLOCK  // 36
}
```

### v3 layout (format_version ≥ 3)

Variable layout: journal region sits between data bitmap and inode table. Detected by:

```rust
pub fn has_journal_layout(&self) -> bool {
    if self.is_v3_layout() {
        let without_journal = 1 + self.inode_bitmap_blocks() + self.data_bitmap_blocks();
        self.inode_table_block == without_journal + crate::journal::JOURNAL_RESERVED_BLOCKS
    } else {
        crate::journal::is_journaled_layout(self)
    }
}
```

The journal header block is at `data_bitmap_block + data_bitmap_blocks()` and carries magic `JRNL` (`0x4A52_4E4C`).

---

## Superblock Validation

[`SuperBlock::validate(mmap_len)`](../../src/superblock.rs#L508-L588) enforces:

1. **Magic**: Must equal `0x4F494653`
2. **Block size**: Must equal 4096
3. **Minimum blocks**: ≥ 5 (≥ 37 for journaled via geometry)
4. **Bounds**: `block_count × 4096 ≤ mmap_len`
5. **Fixed offsets**: `inode_bitmap_block == 1`, `data_bitmap_block` after inode bitmap
6. **Ordering**: `inode_table_block > data_bitmap_block`, `inode_table_block ≤ data_block_start ≤ block_count`
7. **Inode count**: > 0, ≤ `MAX_TRACKED_INODES` (32,768)
8. **Table fit**: Usable inodes must fit between `inode_table_block` and `data_block_start`
9. **Root inode**: Must be < usable inode count
10. **Format version**: ≤ 3
11. **Migration cursor**: ≤ usable inode count
12. **Encryption consistency**: Version must match `encrypted` flag
13. **v3 geometry**: Bitmap spans must exactly match recorded offsets (implicit span validation)

---

## Format Migration (v1 → v2 → v3)

### Migration cursor mechanism

The [`migration_cursor`](../../src/superblock.rs#L72) enables **crash-safe, resumable in-place migration**:

- **Before migration**: `format_version = 1`, `migration_cursor = 0`
- **During migration**: `format_version = 1` (legacy), `migration_cursor` advances as inodes are rewritten
- **After migration**: `format_version = 3` (v3 layout), `migration_cursor = 0`

### Migration algorithm in `DiskManager::migrate`

1. **Checkpoint journal** first (prevents pending `WriteInode` ops in old format from corrupting rewritten slots)
2. **Snapshot inode bitmap** to know which slots are allocated
3. **For each inode from `cursor` to `inode_count`**:
   - If allocated: decode with v1 format, encode as v2 (256-byte fixed), write slot, flush
   - Advance `migration_cursor`, persist superblock, flush block 0
4. **On completion**: Set `format_version = 3`, clear `migration_cursor`, flush

### Per-inode version dispatch

Readers use [`inode_record_version_for(inode_id)`](../../src/superblock.rs#L110-L118) to decode the correct format per slot, making the intermediate mixed state **safe and readable**.

---

## In-Place Resize

The method [`in_place_block_count_range()`](../../src/superblock.rs#L354-L382) returns the `(min_blocks, max_blocks)` range where `block_count` can change **without relocating any metadata or data blocks**. The bitmap spans (`N`, `M`) and journal presence (`J`) remain invariant within this range.

For v3:
- `base = 1 + N + 1 + J + T`
- `max_blocks = base + M × BITMAP_BLOCK_BITS`
- `min_blocks` ensures the data bitmap doesn't shrink below its recorded span

---

## Encryption Fields

- `encrypted: bool` — master toggle
- `encryption_salt: [u8; 16]` — Argon2id salt for key derivation
- `encryption_version: u8` — 1 = legacy XChaCha20-Poly1305 (no AAD), 2 = context-bound (with AAD)

Validation rejects unknown versions or mismatched `encrypted`/`encryption_version` combinations.

---

## Related Pages

- [Block Allocation](block_allocation.md) — bitmap structure and allocation algorithms
- [Inode & Directory](inode_and_directory.md) — inode record formats (v1 bincode vs v2 fixed 256-byte)
- [Journal](journal.md) — WAL ring buffer, recovery, and journal region layout
- [Overview](overview.md) — high-level architecture
