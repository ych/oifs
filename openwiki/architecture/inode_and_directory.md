---
type: architecture
title: Inode Layout and Multi-Block Directories
description: Detailed specification of OIFS's 256-byte inode structure (format v2), directory entry format with filename encryption, block pointer hierarchy, multi-block directory structure with SipHash-2-4 indexing, and directory entry encoding.
tags: [inode, directory, directory-entry, block-pointers, filetype, kani-verified, chunked-compression, encryption]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-24T19:09:23.676Z
sources:
  - id: openwiki-source-577ab4c8720ea065ae15ce27
    resource: repo://src/directory.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-fd65460dcd62d8e13b82908b
    resource: repo://src/disk/caches.rs
  - id: openwiki-source-88657ea41344918d5e874716
    resource: repo://src/encryption.rs
  - id: openwiki-source-062270e1dcd4de6ec5011c98
    resource: repo://src/fuse.rs
  - id: openwiki-source-bc305a37042018e1ebd6d860
    resource: repo://src/inode.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-24T19:09:23.676Z" }
---

## Responsibility and ownership

The inode and directory subsystem defines the structural foundation of the OIFS filesystem across these modules:

<!-- openwiki: broken internal link [../src/inode.rs] file "../src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/inode.rs#L33-L69] file "../src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/inode.rs#L10-L15] file "../src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/inode.rs`](../src/inode.rs): Defines the 256-byte fixed-size [`Inode`](../src/inode.rs#L33-L69) struct, the [`FileType`](../src/inode.rs#L10-L15) discrimination model, block pointer tiers, encryption nonces, filter flags, chunked compression support, and `ChunkEntry` bit layout.
<!-- openwiki: broken internal link [../src/inode_format.rs] file "../src/inode_format.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/inode_format.rs`](../src/inode_format.rs): Implements deterministic on-disk encoding/decoding for the **format v2** 256-byte inode record (`INODE_SLOT_SIZE = 256`), including the frozen field layout `[0..166)`, `record_version` at offset 166, `flags` (u32) at offset 167, and zero-filled padding to byte 256. Legacy v1 (`bincode`) decoding is retained for backward compatibility.
<!-- openwiki: broken internal link [../src/directory.rs] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/directory.rs#L5-L10] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/directory.rs#L84-L93] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/directory.rs#L211-L232] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/directory.rs#L170-L208] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/directory.rs#L234-L250] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/directory.rs`](../src/directory.rs): Defines the on-disk variable-length [`DirectoryEntry`](../src/directory.rs#L5-L10) record format, streaming [`DirectoryIterator`](../src/directory.rs#L84-L93), and zero-allocation lookup algorithms ([`find_entry_in_block`](../src/directory.rs#L211-L232), [`find_entry_in_block_with_hash`](../src/directory.rs#L170-L208), [`find_insert_offset_in_block`](../src/directory.rs#L234-L250)). Also provides SipHash-2-4 filename hashing for fast filtering.
<!-- openwiki: broken internal link [../src/disk/caches.rs] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/disk/caches.rs#L175-L195] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/disk/caches.rs#L202-L254] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/disk/caches.rs`](../src/disk/caches.rs): Defines [`DirIndex`](../src/disk/caches.rs#L175-L195) with `stored` (on-disk ciphertext) and `plain` (plaintext) maps, cold-scan promotion threshold (`DIR_INDEX_BUILD_AFTER_SCANS = 8`), `may_have_holes` and `tail_offset` for O(1) inserts, and the 32-shard [`ShardedDirCache`](../src/disk/caches.rs#L202-L254) using Fibonacci hashing.
<!-- openwiki: broken internal link [../src/encryption.rs] file "../src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/encryption.rs`](../src/encryption.rs): Implements deterministic filename encryption via 192-bit SIV nonce (`derive_filename_siv_nonce`), XChaCha20-Poly1305 AEAD with `parent_inode` bound in AAD (`filename_aad_stack`), and Base64URL `_e_` prefix encoding. Legacy v1 (12-byte nonce) fallback is supported.
<!-- openwiki: broken internal link [../src/fuse.rs] file "../src/fuse.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/fuse.rs`](../src/fuse.rs): Implements UTF-8 NFD→NFC filename normalization for macOS FUSE compatibility, filtering macOS metadata filenames (`._*`, `.DS_Store`, etc.), and providing the FUSE VFS layer backed by `DiskManager`.
<!-- openwiki: broken internal link [../src/superblock.rs] file "../src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/superblock.rs`](../src/superblock.rs): Manages filesystem layout, format versioning (`format_version`), and inode table placement. Controls whether inodes use legacy v1 or fixed v2 format via `inode_record_version_for()`.

## The 256-byte Inode structure (format v2)

<!-- openwiki: broken internal link [../src/inode.rs#L33-L69] file "../src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Every file and directory in OIFS is represented by an [`Inode`](../src/inode.rs#L33-L69) stored within the contiguous Inode Table starting at `superblock.inode_table_block`. The on-disk format is **format v2**: a deterministic 256-byte record (`INODE_SLOT_SIZE = 256`) with frozen field offsets `[0..166)`, a `record_version` byte at offset 166, `flags` (u32) at offset 167, and zero padding to byte 256. The in-memory `#[repr(C)]` struct mirrors this layout.

```rust
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
```

### On-disk format v2 record layout (fixed 256 bytes)

| Offset | Size | Field | Notes |
|--------|------|-------|-------|
| 0      | 1    | `mode` | `FileType` discriminant (0 = File, 1 = Directory) |
| 1      | 8    | `size` | LE u64 |
| 9      | 8    | `compressed_size` | LE u64 |
| 17     | 8    | `created_at` | LE u64 |
| 25     | 8    | `modified_at` | LE u64 |
| 33     | 96   | `blocks` | 12 × LE u64 |
| 129    | 1    | `encrypted` | bool (0/1) |
| 130    | 24   | `encryption_nonce` | 24-byte XChaCha20-Poly1305 nonce |
| 154    | 1    | `filter_typesize` | u8 (0, 1, 2, 4, 8) |
| 155    | 1    | `filter_delta` | bool |
| 156    | 1    | `filter_shuffle` | bool |
| 157    | 1    | `filter_bitshuffle` | bool |
| 158    | 8    | `triple_indirect` | LE u64 |
| 166    | 1    | `record_version` | Fixed to `2` (format v2) |
| 167    | 4    | `flags` | LE u32 (e.g., `INODE_FLAG_SEEKABLE_64K = 0x01`) |
| 171    | 85   | `padding` | Zero-filled to 256 bytes |

The frozen region `[0..166)` is cryptographically bound: any change to those 166 bytes alters the inode's on-disk representation and must be accompanied by a new `record_version` if the layout ever changes. Format v1 (`bincode`-serialized, variable-length) is still decodable via `Inode::decode_v1` but is no longer written.

### FileType discrimination

```rust
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileType {
    File = 0,
    Directory = 1,
}
```

Only `File` and `Directory` exist. There are no symlinks, device nodes, or other special types in OIFS. The `mode` field is a single byte (offset 0) in the on-disk record.

### Block pointer hierarchy

OIFS uses a three-tier block pointer scheme to support files from a few bytes up to 513 GB:

- **Direct (12)**: `blocks[0..12]` → 12 × 4 KiB = 48 KiB
- **Double indirect**: `triple_indirect` points to a block containing 1024 block IDs (each 8 bytes), each of which points to a data block → 1024 × 1024 × 4 KiB = 4 GiB per double-indirect level
- **Triple indirect**: The `triple_indirect` field points to a level-3 index block. A level-3 block points to 1024 level-2 blocks, each pointing to 1024 data blocks → 1024³ × 4 KiB = 4 TiB theoretical, capped by `u64` logical size and practical limits to ~513 GB.

Block ID `0` always means unallocated/empty. All block IDs are LE u64 on disk.

### Inode encoding/decoding (format v2)

<!-- openwiki: broken internal link [../src/inode_format.rs] file "../src/inode_format.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`src/inode_format.rs`](../src/inode_format.rs) provides:

- `encode_v2(inode: &Inode) -> [u8; 256]` — deterministic LE encoding matching the table above.
- `decode_v2(bytes: &[u8; 256]) -> Result<Inode, InodeFormatError>` — strict decoding; validates `record_version == 2`, `mode` is valid, and padding bytes 171–255 are zero.
- `decode_v1(bytes: &[u8]) -> Result<Inode, InodeFormatError>` — legacy `bincode` deserialization for backward compatibility.
- `encode_v1` is not exposed; v1 is read-only.

## Directory entry format and encoding

<!-- openwiki: broken internal link [../src/directory.rs#L5-L10] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
A directory is a sequence of variable-length [`DirectoryEntry`](../src/directory.rs#L5-L10) records packed contiguously within one or more 4 KiB blocks. There is no on-disk B-tree, hash table, or separate index; lookup is linear scan with SipHash-2-4 filtering.

### On-disk record layout

| Offset | Size | Field | Notes |
|--------|------|-------|-------|
| 0      | 8    | `inode` | LE u64, target inode ID |
| 8      | 8    | `hash` | LE u64, SipHash-2-4 of plaintext filename (or 0 if unhashed) |
| 16     | 2    | `name_len` | LE u16, length of UTF-8 name in bytes (1–255) |
| 18     | `name_len` | `name` | Variable-length UTF-8 filename (no NUL terminator) |

Total per entry = 18 + `name_len` bytes. Max filename length is 255 bytes (`DirectoryEntry::MAX_FILENAME_LEN`). A `name_len` of 0 signals end of entries in the block (remaining bytes are zero-padded).

### Filename encryption in directory entries

When the filesystem is encrypted (`DiskManager::encryption_key` is `Some`), directory entry `name` fields store **ciphertext**, not plaintext. The encryption scheme:

<!-- openwiki: broken internal link [../src/encryption.rs#L629-L672] file "../src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
1. **Deterministic SIV nonce derivation** (`derive_filename_siv_nonce` in [`src/encryption.rs`](../src/encryption.rs#L629-L672)):
   - Uses BLAKE2b-512 with domain separator `OIFS_SIV_FILENAME_V2_XCHACHA20`
   - Inputs: domain-separated `filename_siv_key` (256-bit), `parent_inode` (u64 LE), plaintext filename bytes
   - Output: 192-bit (24-byte) synthetic IV (`XNonce`)

<!-- openwiki: broken internal link [../src/encryption.rs#L647-L720] file "../src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
2. **AEAD encryption** (`encrypt_filename` in [`src/encryption.rs`](../src/encryption.rs#L647-L720)):
   - Cipher: XChaCha20-Poly1305 with domain-separated `filename_cipher_key` (256-bit)
   - AAD: `filename_aad_stack(parent_inode)` — 20-byte stack-allocated buffer binding ciphertext to parent directory inode (kind = `AAD_KIND_FILENAME = 0x03`, domain = `OIFS-AAD-v1`, 8-byte parent inode LE)
   - Plaintext: raw UTF-8 filename bytes
   - Output format: `[24-byte XNonce || ciphertext || 16-byte Poly1305 tag]`, then unpadded Base64URL-encoded, prefixed with `_e_`

<!-- openwiki: broken internal link [../src/encryption.rs#L752-L855] file "../src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
3. **Decryption & verification** (`decrypt_filename_authenticated` in [`src/encryption.rs`](../src/encryption.rs#L752-L855)):
   - Strips `_e_` prefix, Base64URL-decodes
   - Splits payload into 24-byte nonce, ciphertext, 16-byte tag
   - Verifies Poly1305 tag with AAD = `filename_aad_stack(parent_inode)`
   - Decrypts plaintext
   - **Constant-time SIV nonce verification**: recomputes expected nonce via `derive_filename_siv_nonce` and compares with `ct_eq_slice` — prevents nonce-reuse/malleability attacks
   - On failure, falls back to **legacy v1 format** (12-byte `ChaCha20Poly1305` nonce, domain `OIFS_SIV_FILENAME_V1`, no AAD) if `encryption_version < ENCRYPTION_SCHEME_CONTEXT_BOUND` (anti-downgrade)

4. **Plaintext passthrough**: Names not starting with `_e_` pass through `decrypt_filename` unchanged (used for unencrypted filesystems and special entries `.`, `..`).

The `_e_` prefix is a constant (`FILENAME_ENC_PREFIX = "_e_"`) and is itself encrypted if a filename literally begins with `_e_` (deterministic encryption means the same plaintext always produces the same ciphertext).

### SipHash-2-4 directory indexing

<!-- openwiki: broken internal link [../src/directory.rs] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`src/directory.rs`](../src/directory.rs) provides `hash_filename` / `hash_filename_bytes` using SipHash-2-4 with fixed keys (`k0 = 0x0706050403020100`, `k1 = 0x0f0e0d0c0b0a0908`). This 64-bit hash is stored in each `DirectoryEntry.hash` field and used for fast filtering during linear scan (`find_entry_in_block_with_hash`): entries with mismatching hash are skipped without string comparison. Hash value `0` is treated as "no hash" and forces string comparison (backward compatibility).

### Zero-allocation directory operations

All directory block operations work directly on `&[u8]` / `&mut [u8]` slices with no heap allocations:

- `find_entry_in_block(slice, name) -> Option<u64>` — linear scan, string compare
- `find_entry_in_block_with_hash(slice, name, hash) -> Option<u64>` — hash filter then string compare
- `find_insert_offset_in_block(slice) -> usize` — finds first zero `name_len` (append position)
- `remove_entry_in_block_bytes(slice, name, hash) -> Option<u64>` — in-place compaction, zeros tail
- `has_valid_entry_in_block(slice) -> bool` — `fsck` helper
- `for_each_valid_inode_in_block(slice, f)` — `fsck` visitor over valid inode IDs
- `DirectoryIterator<'a>` — streaming deserializer over `&[u8]`

<!-- openwiki: broken internal link [../src/disk.rs] file "../src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
These are used by [`DiskManager::build_dir_index`](../src/disk.rs) and the directory cache to avoid per-entry allocations during cold scans.

## Directory cache and lookup flow

<!-- openwiki: broken internal link [../src/disk/caches.rs] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`src/disk/caches.rs`](../src/disk/caches.rs) defines:

- **`DirIndex`**: Two `BTreeMap<String, ...>` maps:
  - `stored`: on-disk ciphertext name (with `_e_` prefix) → `(inode_id, block_id, stored_hash)`
  - `plain`: plaintext name → `(inode_id, plain_hash)` (only populated on encrypted filesystems)
  - `complete`: boolean, true when index represents the full directory (all blocks scanned)
  - `plain_complete`: boolean, true when `plain` map has all entries matching `stored`
  - `cold_scans`: counter of full-directory scans since last promotion
  - `may_have_holes`: true if removals created gaps usable for O(1) insert
  - `tail_offset`: `Option<(block_id, used_bytes)>` for fast appends when no holes

- **`ShardedDirCache`**: 32 shards (`ShardedDirCache::SHARDS = 32`), Fibonacci hash `inode_id.wrapping_mul(0x9e3779b97f4a7c15) >> 27` to select shard. Each shard guarded by `RwLock<BTreeMap<u64, DirIndex>>`.

<!-- openwiki: broken internal link [../src/disk.rs] file "../src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
### Lookup algorithm (`DiskManager::dir_lookup` in [`src/disk.rs`](../src/disk.rs))

1. **Cache hit**: Check shard for `parent_id`. If `encrypted`, probe `plain` map with plaintext name; else probe `stored` map. If `complete` (and `plain_complete` if encrypted), negative result is authoritative → return `None` immediately.
2. **Complete index miss**: If index exists and is `complete` but `plain_complete` is false (encrypted FS, first lookup after index built), scan on-disk blocks once to populate `plain` map.
3. **Cold scan → index build**: On first miss with no index, `build_dir_index` reads all directory blocks, decrypts filenames (if encrypted), builds complete `stored` and `plain` maps, promotes to `complete = true`, caches `tail_offset` for O(1) append.
4. **Targeted scan**: If index exists but not complete, scans only the block where the entry might reside (based on `tail_offset` or hole list).

After a successful lookup or insert, `plain` map is updated and `plain_complete` is set when `plain.len() == stored.len()`.

## Multi-block directory structure

Directories can span multiple 4 KiB blocks linked via the inode's `blocks[0..12]` (direct) and `triple_indirect` pointers. There is no separate directory block bitmap; the inode's block array is the authoritative list. `DiskManager::build_dir_index` iterates all block IDs in the inode (via `resolve_logical_block_id` for indirect blocks), reads each block, and parses entries until `name_len == 0` or block end.

## Key invariants and failure modes

| Invariant | Enforcement |
|-----------|-------------|
| Inode record is exactly 256 bytes, `record_version == 2` | `encode_v2` / `decode_v2`; padding bytes 171–255 must be zero |
| `mode` is only `File` (0) or `Directory` (1) | `decode_v2` validates discriminant |
| `blocks[0] == 0` for empty files/dirs | Caller convention; `allocate_block` never returns 0 |
| `encryption_nonce` is unique per file | `generate_nonce()` uses `OsRng` (24 bytes = 192 bits) |
| Directory entry `name_len ≤ 255` | `validate_filename` rejects longer names; `serialize_into` enforces |
| No path separators (`/`, `\`) or NUL in filenames | `validate_filename` |
| `.` and `..` never stored as directory entries | `validate_filename` rejects them; VFS synthesizes them |
| On encrypted FS, directory entries always store `_e_` ciphertext | `stored_filename` encrypts before write; `decrypt_filename_authenticated` rejects non-prefixed entries during scan |
| SIV nonce verification is constant-time | `ct_eq_slice` in `decrypt_filename_authenticated` |
| Legacy v1 filenames rejected on v2+ images (anti-downgrade) | `decrypt_filename_authenticated` checks `encryption_version >= ENCRYPTION_SCHEME_CONTEXT_BOUND` |
| DirIndex `complete` ⇔ all blocks scanned | `build_dir_index` sets `complete = true` after full scan |
| `plain_complete` ⇔ `plain.len() == stored.len()` | Updated after every lookup/insert on encrypted FS |

### Failure semantics

- `InodeFormatError::InvalidRecordVersion` → on-disk corruption or version mismatch
- `InodeFormatError::InvalidFileType` → `mode` not 0 or 1
- `InodeFormatError::NonZeroPadding` → bytes 171–255 not zero (tampering or v1 residue)
- `DirectoryError::EntryTooLarge` → `name_len > 255` on read or write
- `DirectoryError::InvalidFilename` → empty, `.`, `..`, contains `/`, `\`, or NUL
- `EncryptionError::DecryptionFailed` → Poly1305 tag mismatch, SIV nonce mismatch, Base64 decode error, or UTF-8 decode error
- `DiskManagerError::Io` → underlying block device error

## Extension points and configuration

| Feature | Configuration | Source |
|---------|---------------|--------|
<!-- openwiki: broken internal link [../src/encryption.rs] file "../src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| Argon2id KDF parameters | `ARGON2_M_COST_KIB`, `ARGON2_T_COST`, `ARGON2_P_COST` constants in [`src/encryption.rs`](../src/encryption.rs) | OWASP / RFC 9106 defaults (64 MiB, 3, 4) |
<!-- openwiki: broken internal link [../src/superblock.rs] file "../src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| Encryption scheme version | `superblock.encryption_version` (1 = legacy, 2 = context-bound) | [`src/superblock.rs`](../src/superblock.rs) |
<!-- openwiki: broken internal link [../src/inode.rs] file "../src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| Seekable 64 KiB chunk compression | `INODE_FLAG_SEEKABLE_64K = 0x01` in `Inode.flags` | [`src/inode.rs`](../src/inode.rs) |
<!-- openwiki: broken internal link [../src/disk/caches.rs#L175] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| Directory cache promotion threshold | `DIR_INDEX_BUILD_AFTER_SCANS = 8` | [`src/disk/caches.rs`](../src/disk/caches.rs#L175) |
<!-- openwiki: broken internal link [../src/disk/caches.rs#L202] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| Shard count | `ShardedDirCache::SHARDS = 32` (compile-time) | [`src/disk/caches.rs`](../src/disk/caches.rs#L202) |
<!-- openwiki: broken internal link [../src/directory.rs#L5] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| Max filename length | `DirectoryEntry::MAX_FILENAME_LEN = 255` | [`src/directory.rs`](../src/directory.rs#L5) |

## Focused tests and verification

| Test | Location | Coverage |
|------|----------|----------|
<!-- openwiki: broken internal link [../src/inode_format.rs] file "../src/inode_format.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| Inode v2 roundtrip encode/decode | [`src/inode_format.rs`](../src/inode_format.rs) tests | Frozen layout, version, padding |
<!-- openwiki: broken internal link [../src/inode_format.rs] file "../src/inode_format.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| Inode v1 legacy decode | [`src/inode_format.rs`](../src/inode_format.rs) tests | Backward compatibility |
<!-- openwiki: broken internal link [../src/directory.rs] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| `DirectoryEntry` serialize/deserialize | [`src/directory.rs`](../src/directory.rs) tests | Variable-length encoding, validation |
<!-- openwiki: broken internal link [../src/directory.rs] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| `hash_filename` determinism & SipHash-2-4 reference | [`src/directory.rs`](../src/directory.rs) tests | Hash stability, filtering |
<!-- openwiki: broken internal link [../src/directory.rs] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| `find_entry_in_block*` zero-allocation correctness | [`src/directory.rs`](../src/directory.rs) tests | Lookup, insert offset, removal compaction |
<!-- openwiki: broken internal link [../src/disk/caches.rs] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| `DirIndex` cold-scan promotion | [`src/disk/caches.rs`](../src/disk/caches.rs) tests | Sharding, `complete`, `plain_complete`, `tail_offset` |
<!-- openwiki: broken internal link [../src/encryption.rs#L971-L1012] file "../src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| `encrypt_filename` / `decrypt_filename` roundtrip | [`src/encryption.rs`](../src/encryption.rs#L971-L1012) | Determinism, parent_inode binding, `_e_` prefix handling |
<!-- openwiki: broken internal link [../src/encryption.rs#L1064-L1092] file "../src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| SIV nonce constant-time verification | [`src/encryption.rs`](../src/encryption.rs#L1064-L1092) | Tampering detection on nonce/ciphertext/tag |
<!-- openwiki: broken internal link [../src/encryption.rs#L1092-L1102] file "../src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| Legacy v1 fallback & anti-downgrade | [`src/encryption.rs`](../src/encryption.rs#L1092-L1102) | v1 readable on v1, rejected on v2+ |
<!-- openwiki: broken internal link [../src/encryption.rs#L1102+] file "../src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
| Short-message scalar XChaCha20 path | [`src/encryption.rs`](../src/encryption.rs#L1102+) | Byte-for-byte match with `XChaCha20Poly1305` reference |
<!-- openwiki: broken internal link [../src/kani] file "../src/kani" does not exist. Fix the href or restore the target, then delete this comment. -->
| Kani proof harnesses | [`src/kani`](../src/kani) | Memory safety for directory block ops, inode encoding, encryption primitives |
