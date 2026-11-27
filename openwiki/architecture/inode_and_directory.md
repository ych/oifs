---
type: architecture
title: Inode Layout and Multi-Block Directories
description: Detailed specification of OIFS's 256-byte inode structure (format v2), directory entry format with filename encryption, block pointer hierarchy (BlockPath), multi-block directory structure with SipHash-2-4 indexing, and directory cache interactions.
tags: [inode, directory, directory-entry, block-pointers, filetype, kani-verified, chunked-compression, encryption]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
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
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

## Responsibility and ownership

The inode and directory subsystem defines the structural foundation of the OIFS filesystem across these modules:

<!-- openwiki: broken internal link [../src/inode.rs] file "../src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/inode.rs#L33-L69] file "../src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/inode.rs#L10-L15] file "../src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/inode.rs`](../src/inode.rs): Defines the 256-byte fixed-size [`Inode`](../src/inode.rs#L33-L69) struct, the [`FileType`](../src/inode.rs#L10-L15) discrimination model, block pointer tiers, encryption nonces, filter flags, chunked compression support, and `ChunkEntry` bit layout.
<!-- openwiki: broken internal link [../src/inode_format.rs] file "../src/inode_format.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/inode_format.rs`](../src/inode_format.rs): Implements deterministic on-disk encoding/decoding for the **format v2** 256-byte inode record (`INODE_SLOT_SIZE = 256`), including the frozen field layout `[0..166)`, `record_version` at offset 166, `flags` (u32) at offset 167, CRC32C at offset 171, and zero-filled padding to byte 256. Legacy v1 (`bincode`) decoding is retained for backward compatibility.
<!-- openwiki: broken internal link [../src/directory.rs] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/directory.rs#L5-L10] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/directory.rs#L84-L93] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/directory.rs#L211-L232] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/directory.rs#L170-L208] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/directory.rs#L234-L250] file "../src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/directory.rs`](../src/directory.rs): Defines the on-disk variable-length [`DirectoryEntry`](../src/directory.rs#L5-L10) record format, streaming [`DirectoryIterator`](../src/directory.rs#L84-L93), and zero-allocation lookup algorithms ([`find_entry_in_block`](../src/directory.rs#L211-L232), [`find_entry_in_block_with_hash`](../src/directory.rs#L170-L208), [`find_insert_offset_in_block`](../src/directory.rs#L234-L250)). Also provides SipHash-2-4 filename hashing for fast filtering.
<!-- openwiki: broken internal link [../src/disk/caches.rs] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/disk/caches.rs#L301-L328] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/disk/caches.rs#L353-L395] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/disk/caches.rs`](../src/disk/caches.rs): Defines [`DirIndex`](../src/disk/caches.rs#L301-L328) with `stored` (on-disk ciphertext) and `plain` (plaintext) maps, cold-scan promotion threshold (`DIR_INDEX_BUILD_AFTER_SCANS = 8`), `may_have_holes` and `tail_offset` for O(1) inserts, and the 32-shard [`ShardedDirCache`](../src/disk/caches.rs#L353-L395) using Fibonacci hashing.
<!-- openwiki: broken internal link [../src/encryption.rs] file "../src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/encryption.rs`](../src/encryption.rs): Implements deterministic filename encryption via 192-bit SIV nonce (`derive_filename_siv_nonce`), XChaCha20-Poly1305 AEAD with `parent_inode` bound in AAD (`filename_aad_stack`), and Base64URL `_e_` prefix encoding. Legacy v1 (12-byte nonce) fallback is supported.
<!-- openwiki: broken internal link [../src/fuse.rs] file "../src/fuse.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/fuse.rs`](../src/fuse.rs): Implements UTF-8 NFD→NFC filename normalization for macOS FUSE compatibility, filtering macOS metadata filenames (`._*`, `.DS_Store`, etc.), and providing the FUSE VFS layer backed by `DiskManager`.
<!-- openwiki: broken internal link [../src/superblock.rs] file "../src/superblock.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/superblock.rs`](../src/superblock.rs): Manages filesystem layout, format versioning (`format_version`), and inode table placement. Controls whether inodes use legacy v1 or fixed v2 format via `inode_record_version_for()`.

---

## The 256-byte Inode structure (format v2)

<!-- openwiki: broken internal link [../src/inode.rs#L33-L69] file "../src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Every file and directory in OIFS is represented by an [`Inode`](../src/inode.rs#L33-L69) stored within the contiguous Inode Table starting at `superblock.inode_table_block`. The on-disk format is **format v2**: a deterministic 256-byte record (`INODE_SLOT_SIZE = 256`) with frozen field offsets `[0..166)`, a `record_version` byte at offset 166, `flags` (u32) at offset 167, CRC32C at offset 171, and zero padding to byte 256. The in-memory `#[repr(C)]` struct mirrors this layout.

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
    /// Direct block pointers (10 blocks) + single indirect (blocks[10]) + double indirect (blocks[11])
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

```text
[0]           u8     mode tag (0 = File, 1 = Directory)
[1..9]        u64    size                  (logical / uncompressed)
[9..17]       u64    compressed_size       (physical, 0 when stored raw)
[17..25]      u64    created_at
[25..33]      u64    modified_at
[33..129]     12×u64 block pointers (direct[0..9], single[10], double[11])
[129]         u8     encrypted flag (0/1)
[130..154]    24B    encryption_nonce (XChaCha20-Poly1305)
[154]         u8     filter_typesize
[155]         u8     filter_delta
[156]         u8     filter_shuffle
[157]         u8     filter_bitshuffle
[158..166]    u64    triple_indirect
[166]         u8     record_version (2 = legacy v2 no CRC, 3 = v3 with CRC32C)
[167..171]    u32    flags (extended inode flags)
[171..175]    u32    crc32c (CRC32C over bytes [0..171) when record_version == 3)
[175..256]    zero padding (reserved for future additive fields)
```

**Field stability guarantee**: Offsets `[0..166)` are frozen for all revisions ≥ 2. New fields may only be appended into the reserved tail `[167..256)`; existing offsets must never move, or every previously written record becomes unreadable.

### Versioning

Two independent version numbers gate decoding:

- **SuperBlock `format_version`** selects the *decoder family*. `1` means the historical `bincode` layout (no self-describing version byte); `≥ 2` means the fixed 256-byte record described here.
- **`record_version`** (inode_format.rs) selects the record revision *within* the fixed family, making future formats additive.

The split matters because a version byte alone cannot bootstrap the v1 → v2 step: a v1 `bincode` record and a v2 record are not reliably distinguishable by inspection, so the superblock has to say which decoder to use. Once inside the fixed family, `record_version` lets a future v3 append fields at offsets ≥ 166 without moving any existing field, so a v3 image can be migrated lazily one record at a time.

### Encoding / Decoding

- `encode_v2()` produces a fully deterministic 256-byte record with zero-filled padding.
- `decode_v2()` validates the file-type tag, verifies CRC32C when `record_version == 3`, and rejects unknown tags rather than guessing.
- `encode_for()` / `decode_for()` dispatch based on superblock `format_version`, preserving v1 compatibility for unmigrated images.

---

## ChunkEntry: 64KB Seekable Compression

When an inode has `INODE_FLAG_SEEKABLE_64K` set (`0x0001`), each 64-bit slot in `blocks[0..12]` (and indirect blocks) represents a **64KB logical chunk** rather than a single 4KB block. This enables random access within compressed files.

```rust
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ChunkEntry(pub u64);
```

### Bit layout (64 bits)

| Bits | Field | Description |
|------|-------|-------------|
| 0..32 | `start_block: u32` | Physical start block on disk |
| 32..40 | `block_count: u8` | Number of contiguous 4KB blocks allocated (1..16) |
| 40..48 | `flags: u8` | Chunk state: `FLAG_EMPTY=0`, `FLAG_COMPRESSED=1`, `FLAG_RAW=2` |
| 48..64 | `compressed_len: u16` | Exact compressed byte length (≤ 65535) |

**State predicates**:
- `is_empty()`: `raw == 0 || flags == FLAG_EMPTY` (sparse hole)
- `is_compressed()`: `flags == FLAG_COMPRESSED` (Zstd compressed)
- `is_raw()`: `flags == FLAG_RAW` (uncompressed fallback when compression inflates)

Each chunk maps up to 16 × 4KB = 64KB physical blocks, matching the logical chunk size. The `compressed_len` allows the reader to know exactly how many bytes to read and decompress.

---

## BlockPath: Hierarchical Block Addressing

<!-- openwiki: broken internal link [../src/inode.rs#L203-L228] file "../src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
OIFS uses a 4-tier block pointer hierarchy (direct, single indirect, double indirect, triple indirect) to address up to ~513GB per file (4KB blocks). The [`BlockPath`](../src/inode.rs#L203-L228) enum decomposes a logical block index into the exact pointer-tree slots.

```rust
pub enum BlockPath {
    Direct(usize),      // inode.blocks[i]              (0 ≤ i < 10)
    Single(usize),      // inode.blocks[10] -> slot i   (0 ≤ i < 512)
    Double(usize, usize), // inode.blocks[11] -> a -> b (0 ≤ a,b < 512)
    Triple(usize, usize, usize), // inode.triple_indirect -> a -> b -> c (0 ≤ a,b,c < 512)
}
```

### Tier boundaries (constants)

```rust
const DIRECT_BLOCKS: usize = 10;         // blocks[0..9]
const PTRS_PER_BLOCK: usize = 512;       // 4096 / 8
const SINGLE_END: usize = DIRECT_BLOCKS + PTRS_PER_BLOCK;                    // 522
const DOUBLE_END: usize = SINGLE_END + PTRS_PER_BLOCK * PTRS_PER_BLOCK;      // 262,666
const MAX_LOGICAL_BLOCKS: usize = DOUBLE_END + PTRS_PER_BLOCK^3;             // ~134M blocks (~513GB)
```

### Decomposition / reconstruction

```rust
BlockPath::from_logical(idx)  // logical block index → BlockPath
BlockPath::to_logical(self)   // BlockPath → logical block index (inverse)
BlockPath::slots_in_bounds(self) // validates all indices < PTRS_PER_BLOCK
```

The decomposition is **provably correct** via Kani symbolic verification: for every possible logical block index, the pointer-tree decomposition never produces an out-of-bounds slot and is exactly invertible (no two logical blocks map to the same physical pointer slot, and none is skipped). The Kani harness exhaustively verifies tier boundaries and round-trip properties.

---

## Directory Entry Format

A directory is stored as a sequence of variable-length `DirectoryEntry` records in one or more 4KB data blocks. The on-disk wire format per entry:

```text
[0..8]     u64  inode_id
[8..16]    u64  hash (SipHash-2-4 of plaintext filename; 0 = legacy/unhashed)
[16..18]   u16  name_len (1..255)
[18..]     bytes  name (UTF-8, no NUL, no '/', no '\', not "." or "..")
```

Entries are packed contiguously; a `name_len == 0` marks the end of valid entries (remaining bytes are zero). The max filename length is 255 bytes (`DirectoryEntry::MAX_FILENAME_LEN`).

### SipHash-2-4 for fast filtering

```rust
pub fn hash_filename(name: &str) -> u64;
pub fn hash_filename_bytes(bytes: &[u8]) -> u64;
```

The hash is stored in each directory entry. During lookup, `find_entry_in_block_with_hash()` first compares the precomputed target hash against the stored entry hash (if both are non-zero) — a single `u64` comparison — before doing the string `memcmp`. This turns most negative lookups into a single 8-byte integer compare per entry, avoiding UTF-8 string comparisons.

**SipHash-2-4 parameters** (fixed, deterministic):
- `k0 = 0x0706050403020100`, `k1 = 0x0f0e0d0c0b0a0908`
- Initialization constants: `0x736f6d6570736575`, `0x646f72616e646f6d`, `0x6c7967656e657261`, `0x7465646279746573`

The hash is **not cryptographic**; it is a fast, deterministic filter. Collisions merely fall back to string comparison.

### Zero-allocation lookup primitives

All directory scans operate on raw `&[u8]` slices with zero heap allocation:

- `find_entry_in_block(slice, name)` — linear scan, string compare only
- `find_entry_in_block_with_hash(slice, name, hash)` — hash filter + string compare
- `find_insert_offset_in_block(slice)` — finds append position (end of last entry)
- `remove_entry_in_block_bytes(slice, name, hash)` — in-place removal, compacts entries forward, zeroes tail
- `has_valid_entry_in_block(slice)` — returns true if ≥ 1 valid entry
- `for_each_valid_inode_in_block(slice, f)` — visitor for `fsck` (yields inode IDs)

---

## Multi-Block Directories and SipHash Indexing

A directory can span up to `MAX_DIR_BLOCKS = 65,536` blocks (256 MB directory table, supporting millions of files). The directory inode's `blocks[]` array and `size` field encode the block list:

```rust
pub fn dir_block_count(size: u64, first_block: u64, block_size: u64) -> usize {
    if block_size == 0 { return 0; }
    if size == 0 {
        usize::from(first_block != 0)  // legacy: 1 block
    } else {
        (size.div_ceil(block_size) as usize).min(MAX_DIR_BLOCKS)
    }
}
```

**Legacy v1 single-block directories** used `blocks[0]` only and stored `size == 0`. The `size == 0 && first_block != 0` pattern implies exactly one block.

### DirIndex: In-memory directory cache

<!-- openwiki: broken internal link [../src/disk/caches.rs#L301-L328] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
To avoid repeated full-directory scans, each directory gets a [`DirIndex`](../src/disk/caches.rs#L301-L328) when promoted:

```rust
pub(crate) struct DirIndex {
    pub(crate) complete: bool,              // true once every on-disk entry is in `plain`
    pub(crate) plain_complete: bool,
    pub(crate) cold_scans: u32,             // counts full scans before promotion
    pub(crate) may_have_holes: bool,        // earlier blocks may have free space from deletions
    pub(crate) tail_offset: Option<(u64, u16)>, // (physical_block_id, next_insert_offset) for O(1) append
    pub(crate) stored: FxHashMap<String, (u64, u64, u64)>, // ciphertext name → (inode, block_id, stored_hash)
    pub(crate) plain: FxHashMap<String, (u64, u64)>,       // plaintext name → (inode, plain_hash)
}
```

**Promotion logic**:
- A directory starts with an empty `DirIndex` (`complete = false`).
- Each cold scan (`lookup` that misses the cache) increments `cold_scans`.
- After `DIR_INDEX_BUILD_AFTER_SCANS = 8` cold scans **or** on a **negative lookup** (name not found after full scan), the directory is fully indexed: `build_dir_index()` reads all blocks, populates `stored` and `plain` maps, sets `complete = true`, and caches `tail_offset`.
- Once `complete`, **both hits and misses are O(1)** via hash map lookup.

On **encrypted filesystems**, `stored` holds the ciphertext names (as stored on disk) while `plain` holds plaintext names → avoids re-encrypting during `list_dir` or re-lookup. On **unencrypted filesystems**, both maps are identical (plaintext = ciphertext).

### ShardedDirCache: 32-way concurrent cache

```rust
const NUM_DIR_CACHE_SHARDS: usize = 32;
type DirCacheShard = CachePadded<RwLock<FxHashMap<u64, DirIndex>>>;
```

<!-- openwiki: broken internal link [../src/disk/caches.rs#L353-L395] file "../src/disk/caches.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The [`ShardedDirCache`](../src/disk/caches.rs#L353-L395) divides `dir_inode_id → DirIndex` across 32 independent `RwLock` shards using **64-bit Fibonacci hashing** (multiplicative bijection with `0x517cc1b727220a95`). This ensures:

- Cold indexing (`build_dir_index`), `list_dir` promotion, or `create_file`/`delete_file` cache maintenance on one directory **never stalls** concurrent `lookup`, `resolve_path`, or `list_dir` operations on other directories.
- Shard index computation: `(dir_inode_id.wrapping_mul(0x517cc1b727220a95) as usize) & 31` — single multiply + bitwise AND, no modulo.

Operations:
- `read_shard(dir_inode_id)` / `write_shard(dir_inode_id)` — acquire shard lock
- `remove(dir_inode_id)` — evicts index on directory deletion/rename
- `clear()` — drops all indices (format migration)
- `for_each_mut(f)` — iterates all indices (used for cache-wide invalidation)

---

## Filename Encryption

On encrypted filesystems, directory entries store **ciphertext filenames** (with `_e_` prefix) instead of plaintext. Encryption is **deterministic** (SIV construction) so identical names in the same parent directory produce identical ciphertext, enabling hash-based lookup on ciphertext.

### Encryption algorithm

```
encrypt_filename(key, parent_inode, name):
    1. Skip if name is "", ".", ".." (pass through plaintext)
    2. nonce = derive_filename_siv_nonce(key, parent_inode, name_bytes)
       // BLAKE2b-512("OIFS_SIV_FILENAME_V2_XCHACHA20" || filename_siv_key || parent_inode_le || name) → first 24 bytes
    3. aad = filename_aad_stack(parent_inode) // binds parent inode to AAD
    4. ciphertext = XChaCha20-Poly1305_encrypt(key.filename_cipher_key, nonce, aad, name)
    5. payload = [24-byte nonce | ciphertext | 16-byte Poly1305 tag]
    6. encoded = "_e_" + Base64URL_unpadded(payload)
    7. return encoded
```

**Key separation**: The master `EncryptionKey` derives two independent subkeys via HKDF:
- `filename_siv_key` → used only for SIV nonce derivation
- `filename_cipher_key` → used only for XChaCha20-Poly1305 encryption

**AAD binding**: `parent_inode` (8-byte LE) is included in Poly1305 AAD, so a ciphertext block cannot be transplanted to a different parent directory without authentication failure.

### Decryption with legacy fallback

```
decrypt_filename_authenticated(key, parent_inode, name, encryption_version):
    1. If not prefixed "_e_" → return plaintext unchanged
    2. Decode Base64URL → payload
    3. If payload_len ≥ 40 (24 nonce + ≥1 ciphertext + 16 tag):
         nonce = payload[0..24]
         ciphertext_with_tag = payload[24..]
         decrypted = XChaCha20-Poly1305_decrypt(key.filename_cipher_key, nonce, aad, ciphertext_with_tag)
         Verify SIV: derive_filename_siv_nonce(key, parent_inode, decrypted) == nonce (constant-time)
         If OK → return decrypted
    4. If encryption_version < ENCRYPTION_SCHEME_CONTEXT_BOUND:
         Try legacy v1 decrypt (12-byte ChaCha20-Poly1305, BLAKE2b-512 "OIFS_SIV_FILENAME_V1")
         Verify SIV similarly
    5. Return DecryptionFailed
```

**Constant-time SIV verification** uses `ct_eq_slice` to avoid timing side-channels on the nonce comparison.

---

## Cache Interactions

### Inode cache: BoundedInodeCache

- 32 shards, Second-Chance (Clock) eviction, capacity 2048 entries
- `get(inode_id)` → marks `referenced` bit (shared load, keeps cache line in Shared MESI state)
- `insert(inode_id, inode)` → evicts LRU unreferenced entry when shard full
- `remove(inode_id)` / `clear()` — invalidation on write/migration

### Payload caches

- **ChunkReadCache** (16 shards): Caches decompressed 64KB chunks for seekable files. Key = `(inode_id, chunk_idx, raw_entry, valid_len)`. Max 2 entries/shard.
- **DecodedBlobCache** (16 shards): Caches full decoded payloads for non-seekable compressed/encrypted files. Key = `(inode_id, size, compressed_size, first_block, nonce)`. Bounded by 32 MB total, 16 MB/entry.

### Directory cache: ShardedDirCache

As described above — 32 shards, `DirIndex` promotion after 8 cold scans or on negative lookup, O(1) lookups once complete, Fibonacci hashing for shard distribution.

---

## Summary of Key Invariants

| Invariant | Enforced by |
|-----------|-------------|
| Inode slot = exactly 256 bytes, zero-padded | `encode_v2()` / `INODE_SLOT_SIZE` |
| Frozen field offsets `[0..166)` never change | `INODE_V2_PAYLOAD_LEN`, Kani proof |
| CRC32C covers `[0..171)` when `record_version == 3` | `encode_v2_inner(stamp_v3_crc=true)` / `decode_v2()` |
| BlockPath decomposition is bijective and bounds-safe | `from_logical()` / `to_logical()` / Kani proofs |
| ChunkEntry bit fields are isolated | `new()` / accessors / Kani field isolation proof |
| DirIndex promotion: `cold_scans ≥ 8` ∨ negative lookup → `complete = true` | `build_dir_index()` logic in `DiskManager` |
| SipHash filter never causes false negatives | `check_hash_filter_soundness` Kani proof |
| Filename encryption binds `parent_inode` in AAD + SIV | `filename_aad_stack`, `derive_filename_siv_nonce` |
| Sharded caches never contend across unrelated inodes/dirs | 32-way Fibonacci sharding (`0x517cc1b727220a95`) |
