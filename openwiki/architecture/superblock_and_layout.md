---
type: "Reference"
title: "Superblock and On-Disk Layout"
description: "Comprehensive documentation of the OIFS superblock structure, format versioning (v1/v2/v3), block layout geometry, journal region placement, encryption fields, and migration cursor mechanics for in-place format upgrades."
tags: ["superblock", "on-disk-format", "layout", "journal", "encryption", "migration", "fsck"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-24T19:09:23.676Z
sources:
  - id: openwiki-source-115024023ade9f243c6b4ec3
    resource: repo://src/disk/maintenance.rs
  - id: openwiki-source-88657ea41344918d5e874716
    resource: repo://src/encryption.rs
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-24T19:09:23.676Z" }
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

## On-disk layout geometry

The physical layout varies by format version and journaling mode.

### Legacy layout (v1/v2, non-journaled)

```
[Block 0: SuperBlock]
[Block 1: Inode Bitmap (1 block = 32,768 bits)]
[Block 2: Data Bitmap (1 block = 32,768 bits)]
[Block 3..3+T-1: Inode Table (T blocks, max 1024 = 32,768 inodes @ 128 bytes)]
[Block 3+T..N-1: Data Blocks]
```

- Fixed single-block bitmaps cap the data area at **32,768 blocks (128 MiB)** and inodes at **32,768**.
- Inode table maximum is 1024 blocks for backward compatibility.
- Inode records use `bincode` encoding (~169 bytes, packed) in v1, or 256-byte fixed records in v2.

### Journaled layout (v1/v2 with journal)

```
[Block 0: SuperBlock]
[Block 1: Inode Bitmap]
[Block 2: Data Bitmap]
[Block 3: Journal Header (magic "JRNL")]
[Block 4..35: Journal Ring (32 blocks = 128 KiB)]
[Block 36..36+T-1: Inode Table (T blocks)]
[Block 36+T..N-1: Data Blocks]
```

- Journal region reserves **33 blocks** (1 header + 32 ring) = **132 KiB**.
- Detected by [`is_journaled_layout`](../../src/journal.rs#L1619): `inode_table_block >= 36` (`JOURNAL_INODE_TABLE_BLOCK`).

### v3 layout (format_version = 3)

The v3 layout introduces **multi-block bitmaps** sized to the image capacity, removing the 128 MiB / 32K inode caps.

```
[Block 0: SuperBlock]
[Block 1..1+I-1: Inode Bitmap (I blocks = ceil(inode_count / 32768))]
[Block 1+I..1+I+D-1: Data Bitmap (D blocks = ceil(data_blocks / 32768))]
[Block 1+I+D..1+I+D+J-1: Journal Region (J = 33 if journaled, else 0)]
[Block 1+I+D+J..1+I+D+J+T-1: Inode Table (T blocks, max 1024 = 16,384 inodes @ 256 bytes)]
[Block 1+I+D+J+T..N-1: Data Blocks]
```

Key v3 constants ([`src/superblock.rs`](../../src/superblock.rs#L352-L358)):

```rust
pub const FORMAT_VERSION_V3_LAYOUT: u32 = 3;
pub const BITMAP_BLOCK_BITS: u64 = 4096 * 8;  // 32,768 bits per bitmap block
pub const INODE_SLOTS_PER_BLOCK: u64 = 16;    // 4096 / 256
```

- **Inode bitmap blocks** = `ceil(inode_count / 32768)`, minimum 1
- **Data bitmap blocks** = `ceil(data_blocks / 32768)`, minimum 1
- Inode table: 1024 blocks max → 16,384 inodes (256-byte records, 16 per block)
- The historical table size is preserved; only the *reported* inode count changes to match actual capacity
- Bitmap spans are **implicit** in the recorded block offsets; `validate()` verifies they fit exactly

##### Single-pass layout calculation (CBMC-tractable)

`SuperBlock::new_v3` computes bitmap spans in a single pass (no iterative fixed-point) to keep CBMC tractable:
- Inode bitmap sized to `inode_count`
- Data bitmap sized to remaining data blocks
- Inode table capped at 1024 blocks (16,384 inodes at 256 bytes/record)
- Minimum 5 blocks for non-journaled; journaled requires 37+ blocks for the 33-block WAL region

This single-pass design avoids CBMC explosion from exploring every branch of every iteration.

#### Journal detection in v3

The journal header block moves with the data bitmap span:

```rust
pub fn journal_header_block(&self) -> u64 {
    if self.is_v3_layout() {
        self.data_bitmap_block + self.data_bitmap_blocks()
    } else {
        crate::journal::JOURNAL_HEADER_BLOCK  // fixed 3
    }
}
```

Both layouts are distinguished purely by geometry — the `SuperBlock` struct itself is unchanged. A v3 image with `data_bitmap_blocks() == 1` and no journal has identical offsets to a legacy image.

The `has_journal_layout()` method checks both geometries:

```rust
pub fn has_journal_layout(&self) -> bool {
    if self.is_v3_layout() {
        // The v3 layout places the journal region between the data bitmap
        // and the inode table.
        let without_journal = 1 + self.inode_bitmap_blocks() + self.data_bitmap_blocks();
        self.inode_table_block == without_journal + crate::journal::JOURNAL_RESERVED_BLOCKS
    } else {
        crate::journal::is_journaled_layout(self)
    }
}
```

### On-disk layout diagram

<!-- openwiki: mermaid parse failed and this diagram was converted to a text fence so it does not break rendering. Fix the diagram source and restore the mermaid fence. Parser error: Parse error on line 4: ...ock 1: Inode Bitmap (32,768 bits)] Expecting 'SQE', 'DOUBLECIRCLEEND', 'PE', '-)', 'STADIUMEND', 'SUBROUTINEEND', 'PIPE', 'CYLINDEREND', 'DIAMOND_STOP', 'TAGEND', 'TRAPEND', 'INVTRAPEND', 'UNICODE_TEXT', 'TEXT', 'TAGSTART', got 'PS' -->
```text
flowchart TD
    subgraph LegacyLayout [Legacy v1/v2 Layout non-journaled]
        L0[Block 0: SuperBlock]
        L1[Block 1: Inode Bitmap (32,768 bits)]
        L2[Block 2: Data Bitmap (32,768 bits)]
        L3[Blocks 3..3+T-1: Inode Table (T <= 1024 blocks)]
        L4[Blocks 3+T..N-1: Data Blocks]
    end

    subgraph JournaledLayout [Journaled v1/v2 Layout]
        J0[Block 0: SuperBlock]
        J1[Block 1: Inode Bitmap]
        J2[Block 2: Data Bitmap]
        J3[Block 3: Journal Header (JRNL)]
        J4[Blocks 4..35: Journal Ring (32 blocks)]
        J5[Blocks 36..36+T-1: Inode Table]
        J6[Blocks 36+T..N-1: Data Blocks]
    end

    subgraph V3Layout [v3 Layout format_version = 3]
        V0[Block 0: SuperBlock]
        V1[Blocks 1..1+I-1: Inode Bitmap (I = ceil(inode_count/32768))]
        V2[Blocks 1+I..1+I+D-1: Data Bitmap (D = ceil(data_blocks/32768))]
        V3[Blocks 1+I+D..1+I+D+J-1: Journal Region (J=33 or 0)]
        V4[Blocks 1+I+D+J..1+I+D+J+T-1: Inode Table (T <= 1024, 16,384 inodes @ 256B)]
        V5[Blocks 1+I+D+J+T..N-1: Data Blocks]
    end

    style LegacyLayout fill:#f9f,stroke:#333
    style JournaledLayout fill:#bbf,stroke:#333
    style V3Layout fill:#bfb,stroke:#333
```

---

## Migration cursor mechanics

The `migration_cursor` field ([`src/superblock.rs`](../../src/superblock.rs#L72)) enables **crash-safe in-place format upgrades** from legacy (v1) to fixed inode format (v2/v3).

### Why a cursor is needed

A `bincode` v1 record and a fixed 256-byte v2 record are **not reliably distinguishable by inspection**. The superblock's single `format_version` cannot describe a mixed state. Without the cursor, reading a v1 record with the v2 decoder (or "successfully" with garbage values) would corrupt the mount.

### How it works

- `migration_cursor == 0` → no migration in progress
- During migration: `format_version` still reads `1` (legacy), but inode IDs `< migration_cursor` hold v2 records
- Readers use [`inode_record_version_for(inode_id)`](../../src/superblock.rs#L114) to select the correct decoder per slot
- Cursor is persisted **after each rewritten inode** (see [`DiskManager::migrate`](../../src/disk/maintenance.rs#L72-L98)): slot is flushed, then `migration_cursor` advanced, superblock persisted, and block 0 flushed
- On completion: `format_version` updated to `2` (`FORMAT_VERSION_FIXED_INODE`), `migration_cursor` cleared to `0`

### Migration algorithm

1. **Checkpoint journal first** — a pending `WriteInode` op in old format would corrupt rewritten slots on replay
2. Snapshot inode bitmap to know which slots are allocated
3. For each allocated inode `cursor..inode_count`:
   - Decode with outgoing (v1) format
   - Encode with current (v2) format
   - Write back to same slot
   - Flush the rewritten slot, then persist `migration_cursor` + superblock + flush (after **each** inode)
4. On finish: update `format_version` to `FORMAT_VERSION_FIXED_INODE`, clear cursor to `0`, flush, clear caches

The process is **idempotent** — safe to call on already-current images and safe to resume after interruption.

### CLI command

```bash
oifs -i <image> migrate
```

This upgrades a legacy (v1) image to the current inode format (v2) in place. If interrupted, re-running resumes from the persisted cursor.

---

## Journal region

The metadata WAL (Write-Ahead Log) uses a fixed 33-block region (1 header + 32 ring blocks = 132 KiB).

### Journal header (Block 3 in legacy, variable in v3)

40-byte persistent state ([`JournalState`](../../src/journal.rs#L220-L247)):
- `head` / `tail`: ring cursor offsets
- `tx_seq`: monotonically increasing transaction sequence
- `cleanly_unmounted`: clean shutdown flag
- Magic `"JRNL"` (`0x4A524E4C`), version `1`, ring blocks `32`

### Ring buffer

- Circular buffer of checksummed frames
- Each frame: magic `"WALT"`, CRC32C, tx_seq, payload_len, payload, commit marker `0xDEADBEEF`
- CRC32C (Castagnoli) detects any torn write
- Operations are **absolute post-image assignments** (idempotent redo)

### Journal layout constants

```rust
JOURNAL_HEADER_BLOCK: u64 = 3;
JOURNAL_RING_BLOCKS: u64 = 32;
JOURNAL_RESERVED_BLOCKS: u64 = 33;
JOURNAL_INODE_TABLE_BLOCK: u64 = 36;
```

### Journal detection

```rust
pub fn is_journaled_layout(sb: &SuperBlock) -> bool {
    sb.inode_table_block >= JOURNAL_INODE_TABLE_BLOCK
}
```

In v3, the journal region sits **between the data bitmap and the inode table**, so the inode table block is `data_bitmap_block + data_bitmap_blocks() + JOURNAL_RESERVED_BLOCKS`. The superblock's `has_journal_layout()` checks both legacy and v3 geometries.

---

## Encryption schemes

Two encryption schemes are supported, identified by `encryption_version` ([`src/encryption.rs`](../../src/encryption.rs#L321-L328)):

| Value | Constant | Scheme |
|-------|----------|--------|
| 0 | — | Unencrypted (default) |
| 1 | `ENCRYPTION_SCHEME_LEGACY` | XChaCha20-Poly1305 **without** associated data |
| 2 | `ENCRYPTION_SCHEME_CONTEXT_BOUND` | XChaCha20-Poly1305 **with** associated data (AAD) binding ciphertext to inode ID, chunk index, and flags |

When `encrypted == true`, `encryption_version` must be either 1 or 2. When `encrypted == false`, `encryption_version` must be 0. The 16-byte `encryption_salt` is used by Argon2id to derive the master key from the user password.

**Context-bound encryption (v2)** prevents whole-file or chunk ciphertexts from being swapped, reordered, or rolled back without the AEAD authentication failing. The associated data (AAD) includes a domain separator (`"OIFS-AAD-v1"`), a kind byte (`0x01` file / `0x02` chunk / `0x03` filename), the target inode ID, and for chunks the chunk index and flags.

---

## On-disk format validation

`SuperBlock::validate(mmap_len)` ([`src/superblock.rs`](../../src/superblock.rs#L430-L500)) enforces:

- Magic number matches
- Block size = 4096
- Minimum 5 blocks total
- Block count fits in mapped file
- Bitmap blocks at fixed positions (1 and 2 in legacy; sequential in v3)
- Inode table after bitmaps, before data area
- Data area non-empty
- Inode count within bitmap capacity (32,768 legacy; `bitmap_blocks * 32768` v3)
- Usable inodes fit in table (256-byte slots)
- Root inode within usable range
- Format version ≤ 3
- Migration cursor within usable range
- Encryption version consistent with `encrypted` flag
- v3 layout: bitmap spans and journal placement match recorded offsets exactly

---

## Summary of key invariants

| Invariant | Enforced by |
|-----------|-------------|
| Superblock at block 0 | `DiskManager::open` |
| Magic = `0x4F494653` | `validate()` |
| Block size = 4096 | `validate()` / `BLOCK_SIZE` const |
| `inode_bitmap_block = 1` (legacy) | `validate()` |
| `data_bitmap_block = 2` (legacy) | `validate()` |
| Inode table after bitmaps | `validate()` |
| Data area non-empty | `validate()` |
| Inode count ≤ bitmap capacity | `validate()` / `usable_inode_count()` |
| Usable inodes fit in table | `validate()` / `inode_table_capacity()` |
| Format version monotonic (appended last) | Source code structure |
| Migration cursor ≤ usable inodes | `validate()` |
| Journal region non-overlapping | `is_journaled_layout()` / `validate()` |
| v3 bitmap spans match offsets | `validate()` |
