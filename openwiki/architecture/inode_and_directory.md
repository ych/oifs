---
type: architecture
title: Inode and Directory Entries
description: How OIFS defines the 256-byte Inode metadata layout, the dual FileType model, variable-length directory records, streaming directory iteration, and zero-allocation block lookup.
tags: [inode, directory, directory-entry, zero-allocation, block-pointers, filetype, kani-verified]
verified:
  - by: openwiki/0.6.1
    at: 2026-09-29T16:14:34.721Z
sources:
  - id: openwiki-source-577ab4c8720ea065ae15ce27
    resource: repo://src/directory.rs
  - id: openwiki-source-bc305a37042018e1ebd6d860
    resource: repo://src/inode.rs
generated: { by: "pi", at: "2026-09-29T16:14:34.721Z" }
---

## Responsibility and ownership

The inode and directory subsystem defines the structural foundation of the OIFS filesystem across two modules:
<!-- openwiki: broken internal link [src/inode.rs] file "src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/inode.rs#L31-L66] file "src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/inode.rs#L9-L15] file "src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/inode.rs`](src/inode.rs): Defines the 256-byte fixed-size [`Inode`](src/inode.rs#L31-L66) struct, the [`FileType`](src/inode.rs#L9-L15) discrimination model, block pointer tiers, encryption nonces, and filter flags.
<!-- openwiki: broken internal link [src/directory.rs] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/directory.rs#L5-L10] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/directory.rs#L81-L108] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/directory.rs#L113-L133] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/directory.rs#L137-L151] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`src/directory.rs`](src/directory.rs): Defines the on-disk variable-length [`DirectoryEntry`](src/directory.rs#L5-L10) record format, streaming [`DirectoryIterator`](src/directory.rs#L81-L108), and zero-allocation lookup algorithms ([`find_entry_in_block`](src/directory.rs#L113-L133) and [`find_insert_offset_in_block`](src/directory.rs#L137-L151)).

## The 256-byte Inode structure

<!-- openwiki: broken internal link [src/inode.rs#L31-L66] file "src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Every file and directory in OIFS is represented by an [`Inode`](src/inode.rs#L31-L66) stored within the contiguous Inode Table starting at `superblock.inode_table_block`.

To ensure consistent on-disk packing and cache alignment, the struct is marked with `#[repr(C)]` and serialized into a 256-byte block slot:

```rust
#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
#[repr(C)]
pub struct Inode {
    pub mode: FileType,
    pub size: u64,
    pub compressed_size: u64,
    pub created_at: u64,
    pub modified_at: u64,
    pub blocks: [u64; 12],
    pub encrypted: bool,
    pub encryption_nonce: [u8; 24],
    pub filter_typesize: u8,
    pub filter_delta: bool,
    pub filter_shuffle: bool,
    pub filter_bitshuffle: bool,
    pub triple_indirect: u64,
}
```

### Metadata and sizing semantics

- `mode`: Indicates whether the inode is a regular file or directory container.
- `size`: Logical, uncompressed payload size in bytes. This represents the true file size exposed to users and applications.
- `compressed_size`: Physical disk space occupied by the compressed payload. A value of `0` denotes that the file is stored uncompressed (raw).
- `created_at` and `modified_at`: 64-bit Unix epoch timestamps in seconds.

### Multi-tier block pointer scheme (~513 GB capacity)

The 4KB block pointer architecture supports files ranging from small configurations to massive 513 GB scientific datasets:

| Pointer Field | Slot Index | Block Capacity | Max Cumulative Address Space |
| :--- | :--- | :--- | :--- |
| Direct Blocks | `blocks[0..10]` | 10 blocks | 40 KB |
| Single Indirect | `blocks[10]` | 512 blocks | $\approx 2.04$ MB |
| Double Indirect | `blocks[11]` | $512^2 = 262,144$ blocks | $\approx 1.002$ GB |
| Triple Indirect | `triple_indirect` | $512^3 = 134,217,728$ blocks | $\approx 513.002$ GB |

*Note*: Block ID `0` indicates an unallocated sparse hole or unassigned pointer.

### Cryptographic and filter flags

- `encrypted`: Boolean flag signaling that the payload is encrypted with XChaCha20-Poly1305.
- `encryption_nonce`: 192-bit (24-byte) unique cryptographic nonce generated per-file via `OsRng`.
- `filter_typesize`: Pre-compression data element width ($1, 2, 4, 8$ bytes; $0 = \text{disabled}$).
- `filter_delta`, `filter_shuffle`, `filter_bitshuffle`: Blosc2-style pre-compression filter pipeline indicators required to reverse transformations on read.

### Formal verification with Kani

<!-- openwiki: broken internal link [src/inode.rs#L103-L149] file "src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
`src/inode.rs` includes automated formal proofs ([`kani_proofs`](src/inode.rs#L103-L149)):
- `proof_inode_new_file` and `proof_inode_new_directory`: Formally verifies that `Inode::new` creates zero-initialized sizes, disabled filters, unencrypted states, and valid modes across all execution branches.
- `proof_inode_no_dangling_blocks`: Proves that all 12 direct/indirect block pointers and the triple-indirect pointer are strictly `0` upon initialization, guaranteeing that newly created inodes never reference uninitialized or dangling physical storage blocks.

## FileType discrimination

<!-- openwiki: broken internal link [src/inode.rs#L9-L15] file "src/inode.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`FileType`](src/inode.rs#L9-L15) classifies storage entities:

```rust
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq)]
pub enum FileType {
    File,
    Directory,
}
```

<!-- openwiki: broken internal link [src/disk.rs] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
In [`DiskManager`](src/disk.rs), `inode.mode` gates operations:
- Attempting to write payload data via `write_data_with_filters` to an inode with `mode == FileType::Directory` returns `DiskManagerError::Io("Cannot write data to non-file inode")` (`src/disk.rs#L1059`).
- Attempting directory operations (`lookup`, `list_dir`, `delete_file`) on an inode with `mode == FileType::File` returns `DiskManagerError::Io("Not a directory")` (`src/disk.rs#L569, L1353, L1397`).

## Directory entry format and wire layout

<!-- openwiki: broken internal link [src/directory.rs#L5-L10] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Directories in OIFS are stored within ordinary 4KB data blocks pointed to by `inode.blocks[0]`. A directory block contains packed, variable-length [`DirectoryEntry`](src/directory.rs#L5-L10) records:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub inode: u64,
    pub hash: u64,
    pub name: String,
}
```

### On-disk wire layout

<!-- openwiki: broken internal link [src/directory.rs#L25-L40] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Each entry is serialized sequentially ([`serialize_into`](src/directory.rs#L25-L40)):

```
┌──────────────────┬──────────────────┬─────────────────┬────────────────────────────┐
│  inode (8B LE)   │   hash (8B LE)   │ name_len (2B LE)│    name (name_len bytes)   │
└──────────────────┴──────────────────┴─────────────────┴────────────────────────────┘
0                  8                  16                18               18 + name_len
```

1. **Fixed Header (18 bytes)**:
   - `inode` (8 bytes, little-endian): Target inode number.
   - `hash` (8 bytes, little-endian): Optional fast-lookup hash.
   - `name_len` (2 bytes, little-endian): Length of the UTF-8 filename in bytes.
2. **Variable Filename (`name`)**:
   - Up to 255 bytes (`MAX_FILENAME_LEN = 255`). Filenames exceeding 255 bytes return `DirectoryError::EntryTooLarge`.
   - Filenames containing path separator `/` are rejected with `DirectoryError::Io("Filename cannot contain '/'")`.
3. **End-of-Block Sentinel**:
   - When scanning or deserializing, `name_len == 0` serves as a sentinel indicating the termination of active entries in the block (`src/directory.rs#L57-L60`).

## Zero-allocation lookup and fast insertion

Traditional directory lookups allocate intermediate strings and vectors for every scanned record. OIFS provides zero-allocation primitives for maximum throughput:

### Zero-allocation lookup (`find_entry_in_block`)

<!-- openwiki: broken internal link [src/directory.rs#L110-L133] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`find_entry_in_block`](src/directory.rs#L110-L133) searches a raw directory memory slice without a single heap allocation:

```rust
#[inline]
pub fn find_entry_in_block(slice: &[u8], target_name: &str) -> Option<u64> {
    let target_bytes = target_name.as_bytes();
    let mut offset = 0;
    while offset + 18 <= slice.len() {
        let len = u16::from_le_bytes([slice[offset + 16], slice[offset + 17]]) as usize;
        if len == 0 { break; }
        let name_start = offset + 18;
        let name_end = name_start + len;
        if name_end > slice.len() { break; }
        if len == target_bytes.len() && &slice[name_start..name_end] == target_bytes {
            let inode = u64::from_le_bytes(slice[offset..offset + 8].try_into().unwrap());
            return Some(inode);
        }
        offset = name_end;
    }
    None
}
```

- Bypasses UTF-8 validation and `String` allocation by directly comparing byte slices: `&slice[name_start..name_end] == target_bytes`.
- Decodes integers in place with `u16::from_le_bytes` and `u64::from_le_bytes`.
- Operates at L1 CPU cache bandwidth, enabling tens of millions of lookups per second.

### Fast append offset (`find_insert_offset_in_block`)

<!-- openwiki: broken internal link [src/directory.rs#L135-L151] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
When linking a new file into a directory, [`find_insert_offset_in_block`](src/directory.rs#L135-L151) steps through existing entries using the 18-byte header and length field until encountering `len == 0`. It returns the exact byte offset where the new record should be written, eliminating full block rewrites.

## Streaming directory iteration

<!-- openwiki: broken internal link [src/directory.rs#L81-L108] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
To enumerate directory contents for tools like `ls`, [`DirectoryIterator`](src/directory.rs#L81-L108) implements the standard Rust `Iterator` trait over a borrowed slice `&'a [u8]`:

- Wraps `std::io::Cursor<&'a [u8]>`.
<!-- openwiki: broken internal link [src/directory.rs#L42-L78] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- Successively calls [`DirectoryEntry::deserialize_from`](src/directory.rs#L42-L78).
- Automatically halts when reaching the end of the memory buffer or encountering a zero-length name sentinel.
- Propagates I/O and UTF-8 errors via `Result<DirectoryEntry, DirectoryError>`.
