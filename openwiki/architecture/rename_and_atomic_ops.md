---
type: architecture
title: Rename & Atomic Operations
description: Documents the atomic POSIX rename implementation with WAL transactions, SIV re-encryption for cross-directory moves on encrypted filesystems, FUSE_RENAME/RENAME2 opcode handling, and the MetadataOp journal framing for create/delete/mkdir/rename.
tags: [rename, wal, encryption, fuse, journaling, atomic-operations]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-88657ea41344918d5e874716
    resource: repo://src/encryption.rs
  - id: openwiki-source-062270e1dcd4de6ec5011c98
    resource: repo://src/fuse.rs
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

# Rename & Atomic Operations

This page documents the atomic POSIX rename implementation in OIFS, covering the FUSE opcode handling, the WAL transaction structure using `MetadataOp` variants, SIV filename re-encryption when moving across directories on encrypted filesystems, and the single-transaction atomicity guarantees for rename, create, delete, and mkdir operations.

## FUSE Opcode Handling

### FUSE_RENAME (Legacy)

The legacy `FUSE_RENAME` opcode (value `23` on Linux) carries only `newdir: u64` followed by two NUL-terminated names. The macOS FUSE implementation may extend this to 16 bytes with an optional flags field.

```rust
// src/fuse.rs:3734-3762
FuseOpcode::Rename => {
    let Some(newdir) = read_u64_le(payload, 0) else {
        return FuseOutHeader::error_frame(unique, -libc::EINVAL);
    };
    // On Linux, `fuse_rename_in` is 8 bytes (`newdir: u64`).
    // On macOS (macFUSE 4.x/5.x), `fuse_rename_in` may be 16 bytes
    // (`newdir: u64, flags: u32, padding: u32`) when `FUSE_RENAME` carries flags.
    let (raw_flags, names_slice) = if platform == FuseAbiPlatform::MacOs
        && payload.len() > 16
        && payload[12..16] == [0, 0, 0, 0]
        && (payload[8] == 0 || payload[9..12] == [0, 0, 0])
    {
        (read_u32_le(payload, 8).unwrap_or(0), &payload[16..])
    } else {
        (0, &payload[8..])
    };
    let flags = match map_rename_flags(platform, raw_flags) {
        Ok(f) => f,
        Err(e) => return FuseOutHeader::error_frame(unique, e),
    };
    let (old_name, new_name) = match parse_fuse_two_names(names_slice) {
        Ok(pair) => pair,
        Err(e) => return FuseOutHeader::error_frame(unique, e),
    };
    match self.rename(dm, hdr.nodeid, old_name, newdir, new_name, flags) {
        Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
        Err(e) => FuseOutHeader::error_frame(unique, e),
    }
}
```

The macOS-specific path parses a 16-byte header (`newdir: u64`, `flags: u32`, `padding: u32`) and extracts flags from bytes 8–11 when the padding bytes 12–15 are zero and the upper flag bytes 9–11 are zero (allowing only the low byte to carry flags).

### FUSE_RENAME2 (Modern, with Flags)

The modern `FUSE_RENAME2` opcode (value `28` on Linux) uses a fixed 16-byte header (`newdir: u64, flags: u32, padding: u32`) followed by the two names. The only flag currently supported is `FUSE_RENAME_NOREPLACE` (bit 0).

```rust
// src/fuse.rs:3763-3790
FuseOpcode::Rename2 => {
    let (Some(newdir), Some(raw_flags)) =
        (read_u64_le(payload, 0), read_u32_le(payload, 8))
    else {
        return FuseOutHeader::error_frame(unique, -libc::EINVAL);
    };
    if payload.len() < 16 {
        return FuseOutHeader::error_frame(unique, -libc::EINVAL);
    }
    let flags = match map_rename_flags(platform, raw_flags) {
        Ok(f) => f,
        Err(e) => return FuseOutHeader::error_frame(unique, e),
    };
    let (old_name, new_name) = match parse_fuse_two_names(&payload[16..]) {
        Ok(pair) => pair,
        Err(e) => return FuseOutHeader::error_frame(unique, e),
    };
    match self.rename(dm, hdr.nodeid, old_name, newdir, new_name, flags) {
        Ok(()) => FuseOutHeader::ok_frame(unique, &[]),
        Err(e) => FuseOutHeader::error_frame(unique, e),
    }
}
```

Flag validation is centralized in `map_rename_flags`:

```rust
// src/fuse.rs:2674-2690
fn map_rename_flags(platform: FuseAbiPlatform, raw_flags: u32) -> Result<u32, i32> {
    if raw_flags == 0 {
        return Ok(0);
    }
    match platform {
        FuseAbiPlatform::Linux => {
            if raw_flags == FUSE_RENAME_NOREPLACE {
                Ok(FUSE_RENAME_NOREPLACE)
            } else {
                Err(-libc::EINVAL)
            }
        }
        FuseAbiPlatform::MacOs => {
            if raw_flags == MACOS_RENAME_EXCL {
                Ok(FUSE_RENAME_NOREPLACE)
            } else {
                Err(-libc::EINVAL)
            }
        }
    }
}
```

**Supported renameat2 flags:**
- **RENAME_NOREPLACE** (bit 0, value 1): Fail with `EEXIST` if the destination path already exists. This is exposed via `FUSE_RENAME2` on Linux and mapped from `MACOS_RENAME_EXCL` on macOS.
- **RENAME_EXCHANGE**: Not currently supported — the kernel FUSE protocol does not expose exchange semantics, and the codebase rejects any non-zero flags other than `RENAME_NOREPLACE`.

### FUSE rename handler

The FUSE layer validates flags before invoking `DiskManager::rename`:

```rust
// src/fuse.rs:3081-3110
pub fn rename(
    &self,
    dm: &DiskManager,
    parent: u64,
    name: &str,
    newparent: u64,
    newname: &str,
    flags: u32,
) -> Result<(), i32> {
    if self.is_read_only(dm) {
        return Err(-libc::EROFS);
    }
    if flags != 0 && flags != FUSE_RENAME_NOREPLACE {
        return Err(-libc::EINVAL);
    }
    // ...
    if flags == FUSE_RENAME_NOREPLACE && dm.lookup(new_parent_inode, newname.as_ref()).is_ok() {
        return Err(-libc::EEXIST);
    }
    // ...
}
```

## DiskManager::rename Implementation

### Overview

`DiskManager::rename` is the core entry point. It validates inputs, ensures capacity, and retries once on `NoSpace` with auto-grow.

```rust
// src/disk.rs:7750-7800
pub fn rename(
    &self,
    old_parent_inode_id: u64,
    old_name: &str,
    new_parent_inode_id: u64,
    new_name: &str,
) -> Result<(), DiskManagerError> {
    if self.is_read_only() {
        return Err(DiskManagerError::ReadOnly);
    }
    for name in [old_name, new_name] {
        crate::directory::validate_filename(name).map_err(|e| {
            DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
    }

    self.ensure_capacity_for(2, 0)?;

    for attempt in 0..2 {
        let mut guard = self.inner.write().unwrap();
        if guard.read_only {
            return Err(DiskManagerError::ReadOnly);
        }
        let res = Self::rename_internal(
            &mut guard,
            old_parent_inode_id,
            old_name,
            new_parent_inode_id,
            new_name,
        );
        drop(guard);
        match res {
            Ok(()) => {
                self.maybe_drain_async_ranges();
                return Ok(());
            }
            Err(DiskManagerError::Allocator(AllocatorError::NoSpace))
                if attempt == 0 && self.try_auto_grow_on_nospace(2, 0)? =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(DiskManagerError::Allocator(AllocatorError::NoSpace))
}
```

### rename_internal — The Atomic Core

`rename_internal` holds the inner write lock for the entire operation and builds a single WAL transaction on journaled images.

#### Stage 1: Validation & Resolution (read-only)

```rust
// src/disk.rs:8012-8075
fn rename_internal(
    guard: &mut DiskManagerInner,
    old_parent_inode_id: u64,
    old_name: &str,
    new_parent_inode_id: u64,
    new_name: &str,
) -> Result<(), DiskManagerError> {
    let mut old_parent = Self::read_inode_internal(guard, old_parent_inode_id)?;
    if old_parent.mode != crate::inode::FileType::Directory {
        return Err(DiskManagerError::Io(std::io::Error::other("Not a directory")));
    }

    let same_parent = old_parent_inode_id == new_parent_inode_id;
    let mut new_parent_opt = if same_parent {
        None
    } else {
        let np = Self::read_inode_internal(guard, new_parent_inode_id)?;
        if np.mode != crate::inode::FileType::Directory {
            return Err(DiskManagerError::Io(std::io::Error::other("Not a directory")));
        }
        Some(np)
    };

    // Encrypted FS: compute SIV-encrypted stored names
    let old_stored = Self::stored_filename(guard, old_parent_inode_id, old_name)?.into_owned();
    let new_stored = Self::stored_filename(guard, new_parent_inode_id, new_name)?.into_owned();
    let old_hash = crate::directory::hash_filename(&old_stored);
    let new_hash = crate::directory::hash_filename(&new_stored);

    // Locate source entry
    let (src_inode_id, src_phys_blk) =
        Self::locate_entry_in_dir(&guard.backing, &old_parent, &old_stored, old_hash)
            .ok_or_else(|| DiskManagerError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound, "File not found")))?;

    // POSIX no-op: renaming an entry onto itself succeeds without mutating anything
    if same_parent && old_name == new_name {
        return Ok(());
    }

    let src_inode = Self::read_inode_internal(guard, src_inode_id)?;

    // Prevent moving a directory into itself or any of its descendants
    if src_inode.mode == crate::inode::FileType::Directory
        && (src_inode_id == new_parent_inode_id
            || (!same_parent
                && Self::is_ancestor_dir(guard, src_inode_id, new_parent_inode_id)?))
    {
        return Err(DiskManagerError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Cannot move a directory into itself or its subdirectory",
        )));
    }
```

#### Stage 2: Destination Handling & Overwrite Semantics

```rust
// src/disk.rs:8077-8115
// Check whether destination already exists.
let dst_loc = {
    let np_ref = new_parent_opt.as_ref().unwrap_or(&old_parent);
    Self::locate_entry_in_dir(&guard.backing, np_ref, &new_stored, new_hash)
};
let dst_info = if let Some((dst_inode_id, dst_phys_blk)) = dst_loc {
    if dst_inode_id == src_inode_id {
        return Ok(());
    }
    let dst_inode = Self::read_inode_internal(guard, dst_inode_id)?;
    match (src_inode.mode, dst_inode.mode) {
        (crate::inode::FileType::Directory, m)
            if m != crate::inode::FileType::Directory =>
        {
            return Err(DiskManagerError::Io(std::io::Error::other("Not a directory")));
        }
        (m, crate::inode::FileType::Directory)
            if m != crate::inode::FileType::Directory =>
        {
            return Err(DiskManagerError::Io(std::io::Error::other("Is a directory")));
        }
        (crate::inode::FileType::Directory, crate::inode::FileType::Directory)
            if !Self::is_dir_empty(guard, &dst_inode)? =>
        {
            return Err(DiskManagerError::Io(std::io::Error::other("Directory is not empty")));
        }
        _ => {}
    }
    let dst_blocks = Self::collect_inode_blocks(&guard.backing, &dst_inode);
    Some((dst_inode_id, dst_phys_blk, dst_blocks))
} else {
    None
};
```

**Overwrite rules:**
- File → File: Atomic replace; victim inode and its blocks freed.
- Directory → Empty Directory: Atomic replace; victim inode and its (empty) blocks freed.
- Directory → Non-empty Directory: Rejected with "Directory is not empty".
- Type mismatch (File → Directory or Directory → File): Rejected with "Is a directory" / "Not a directory".

#### Stage 3: Build Post-Images In-Memory

All directory block mutations are staged in a `BTreeMap<u64, Vec<u8>>` so that multiple edits to the same block compose cleanly.

```rust
// src/disk.rs:8117-8185
// ---- Stage directory block post-images in memory ----
let mut modified_blocks: std::collections::BTreeMap<u64, Vec<u8>> =
    std::collections::BTreeMap::new();
fn load_block_mut<'a>(
    backing: &ImageBacking,
    map: &'a mut std::collections::BTreeMap<u64, Vec<u8>>,
    blk: u64,
) -> Result<&'a mut Vec<u8>, DiskManagerError> {
    match map.entry(blk) {
        std::collections::btree_map::Entry::Occupied(e) => Ok(e.into_mut()),
        std::collections::btree_map::Entry::Vacant(e) => {
            let bytes = backing.with_block(blk, |s| s.to_vec()).map_err(|_| {
                DiskManagerError::Io(std::io::Error::other("Directory block not found"))
            })?;
            Ok(e.insert(bytes))
        }
    }
}

// 1. Remove old directory entry from src_phys_blk.
{
    let blk_buf = load_block_mut(&guard.backing, &mut modified_blocks, src_phys_blk)?;
    crate::directory::remove_entry_in_block_bytes(blk_buf, &old_stored, old_hash);
}

// 2. If overwriting an existing destination, remove its entry from dst_phys_blk.
if let Some((_, dst_phys_blk, _)) = &dst_info {
    let blk_buf = load_block_mut(&guard.backing, &mut modified_blocks, *dst_phys_blk)?;
    crate::directory::remove_entry_in_block_bytes(blk_buf, &new_stored, new_hash);
}

// 3. Serialize the new directory entry pointing to src_inode_id onto the stack.
let new_stored_for_cache = new_stored.clone();
let new_entry = crate::directory::DirectoryEntry {
    inode: src_inode_id,
    hash: new_hash,
    name: new_stored,
};
let mut entry_buf = [0u8; 20 + crate::directory::DirectoryEntry::MAX_FILENAME_LEN];
let entry_len = {
    let mut cursor = std::io::Cursor::new(&mut entry_buf[..]);
    new_entry.serialize_into(&mut cursor)
        .map_err(|e| DiskManagerError::Io(std::io::Error::other(e.to_string())))?;
    cursor.position() as usize
};
let entry_bytes = &entry_buf[..entry_len];
let needed_space = 20 + new_entry.name.len();
```

#### Stage 4: Find or Allocate Destination Slot

```rust
// src/disk.rs:8187-8225
// 4. Find a free slot in new_parent's existing blocks (accounting for staged removals).
let journaled = guard.superblock.has_journal_layout();
let mut journal_ops: Vec<crate::journal::MetadataOp> = Vec::new();
let mut sim_opt: Option<crate::journal::AllocSim> = None;

let target_slot = {
    let np_ref = new_parent_opt.as_ref().unwrap_or(&old_parent);
    let num_blocks = Self::dir_num_blocks(np_ref);
    let mut found: Option<(u64, usize)> = None;
    for blk_idx in 0..num_blocks {
        let phys_blk = Self::resolve_logical_block_id(&guard.backing, np_ref, blk_idx);
        if !guard.is_data_block(phys_blk) {
            continue;
        }
        let insert_offset = if let Some(staged) = modified_blocks.get(&phys_blk) {
            crate::directory::find_insert_offset_in_block(staged)
        } else {
            guard.backing.with_block(phys_blk, crate::directory::find_insert_offset_in_block)
                .unwrap_or(BLOCK_SIZE)
        };
        if insert_offset + needed_space <= BLOCK_SIZE {
            found = Some((phys_blk, insert_offset));
            break;
        }
    }
    found
};

let (inserted_phys_blk, allocated_new_block) = if let Some((phys_blk, insert_offset)) = target_slot {
    let blk_buf = load_block_mut(&guard.backing, &mut modified_blocks, phys_blk)?;
    blk_buf[insert_offset..insert_offset + entry_bytes.len()].copy_from_slice(entry_bytes);
    // Zero-terminate trailing header bytes if room remains so scans stop cleanly.
    let end = insert_offset + entry_bytes.len();
    if end + 18 <= BLOCK_SIZE {
        blk_buf[end..end + 18].fill(0);
    }
    (phys_blk, false)
} else {
    // Grow new_parent by one directory block.
    let np_mut = new_parent_opt.as_mut().unwrap_or(&mut old_parent);
    let new_blk_idx = Self::dir_num_blocks(np_mut);
    let new_phys_blk = if journaled {
        let sim = sim_opt.insert(crate::journal::AllocSim::new_data_only(
            guard,
            guard.free_inode_hint,
            guard.free_block_hint,
        )?);
        crate::journal::sim_get_or_alloc_block(
            &guard.backing,
            np_mut,
            new_blk_idx,
            sim,
            &mut journal_ops,
        )?
    } else {
        Self::get_or_alloc_block(guard, np_mut, new_blk_idx, true)?
    };
    let mut block_image = vec![0u8; BLOCK_SIZE];
    block_image[..entry_bytes.len()].copy_from_slice(entry_bytes);
    modified_blocks.insert(new_phys_blk, block_image);
    np_mut.size = (new_blk_idx + 1) as u64 * BLOCK_SIZE as u64;
    (new_phys_blk, true)
};
```

#### Stage 5: Update Parent Inode Timestamps

```rust
// src/disk.rs:8227-8230
let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_secs();
old_parent.modified_at = now;
if let Some(np) = new_parent_opt.as_mut() {
    np.modified_at = now;
}
```

#### Stage 6: Build WAL Transaction (Journaled Images)

On journaled images, the entire rename commits in a **single atomic WAL transaction** containing:

```rust
// src/disk.rs:8232-8280
if journaled {
    if let Some((dst_inode_id, _, ref dst_blocks)) = dst_info {
        journal_ops.push(crate::journal::MetadataOp::SetInodeBitmap {
            inode_id: dst_inode_id,
            allocated: false,
        });
        for &blk in dst_blocks {
            crate::journal::push_data_bitmap_op(&mut journal_ops, blk, 1, false);
        }
    }
    for (&blk_id, blk_data) in &modified_blocks {
        journal_ops.push(crate::journal::MetadataOp::WriteBlockSlice {
            block_id: blk_id,
            offset: 0,
            data: blk_data.clone(),
        });
    }
    let old_parent_img =
        Self::build_inode_post_image(guard, old_parent_inode_id, &old_parent)?;
    journal_ops.push(crate::journal::MetadataOp::WriteInode {
        inode_id: old_parent_inode_id,
        inode_bytes: Box::new(old_parent_img),
    });
    if let Some(ref np) = new_parent_opt {
        let new_parent_img = Self::build_inode_post_image(guard, new_parent_inode_id, np)?;
        journal_ops.push(crate::journal::MetadataOp::WriteInode {
            inode_id: new_parent_inode_id,
            inode_bytes: Box::new(new_parent_img),
        });
    }
    let ops = if crate::journal::is_already_coalesced(&journal_ops) {
        journal_ops
    } else {
        crate::journal::coalesce_ops(journal_ops)
    };

    Self::commit_journal_tx(guard, &ops)?;
    Self::apply_journal_ops(guard, &ops)?;
    // ...
}
```

**Transaction composition (canonical phase order):**
1. `SetInodeBitmap` — free victim inode (if overwriting)
2. `SetDataBitmapRange` — free victim data blocks (coalesced runs)
3. `WriteBlockSlice` — all modified directory blocks (source removal, dest removal, new entry insertion)
4. `WriteInode` — updated parent inode(s) with new `modified_at` and size

The `coalesce_ops` pass merges adjacent `SetDataBitmap` into `SetDataBitmapRange` and ensures `WriteBlockSlice` ordering.

#### Stage 7: Apply & Cache Refresh

```rust
// src/disk.rs:8282-8360
if let Some(sim) = sim_opt {
    guard.free_block_hint = sim.free_block_hint;
}
if let Some((dst_inode_id, _, ref dst_blocks)) = dst_info {
    if let Some(min_freed) = dst_blocks.iter().copied().min()
        && min_freed < guard.free_block_hint
    {
        guard.free_block_hint = min_freed;
    }
    if dst_inode_id < guard.free_inode_hint {
        guard.free_inode_hint = dst_inode_id;
    }
}
// Cache updates for parents and overwritten destination
let encrypted = guard.superblock.encrypted;
let dst_phys_blk_opt = dst_info.as_ref().map(|&(_, b, _)| b);
guard.cache_inode(old_parent_inode_id, old_parent);
if same_parent {
    let mut cache = guard.dir_cache.write_shard(old_parent_inode_id);
    if let Some(ix) = cache.get_mut(&old_parent_inode_id) {
        if encrypted {
            ix.plain.remove(old_name);
            ix.plain.insert(
                new_name.to_string(),
                (src_inode_id, crate::directory::hash_filename(new_name)),
            );
        }
        ix.stored.remove(&old_stored);
        ix.stored.insert(
            new_stored_for_cache,
            (src_inode_id, inserted_phys_blk, new_hash),
        );
        ix.may_have_holes = true;
        if allocated_new_block
            || ix.tail_offset.is_some_and(|(tb, _)| {
                tb == src_phys_blk
                    || tb == inserted_phys_blk
                    || dst_phys_blk_opt == Some(tb)
            })
        {
            ix.tail_offset = None;
        }
    }
} else {
    // old parent cache updates
    // new parent cache updates
}
// Invalidate overwritten destination
if let Some((dst_inode_id, _, _)) = dst_info {
    guard.inode_cache.remove(dst_inode_id);
    guard.dir_cache.remove(dst_inode_id);
    guard.invalidate_payload_caches(dst_inode_id);
    guard.bump_inode_gen(dst_inode_id);
}
```

#### Stage 8: Durability & Checkpoint

```rust
// src/disk.rs:8362-8380
if guard.durability_mode().is_range_based() {
    let mut ranges = vec![
        guard.inode_bitmap_byte_range(),
        guard.data_bitmap_byte_range(),
        guard.inode_byte_range(old_parent_inode_id),
    ];
    if !same_parent {
        ranges.push(guard.inode_byte_range(new_parent_inode_id));
    }
    for &blk_id in modified_blocks.keys() {
        ranges.push(guard.block_byte_range(blk_id));
    }
    guard.sync_mutation_ranges(&ranges)?;
} else if guard.durability_mode() == DurabilityMode::LegacyWholeMmapAsync {
    guard.sync_mutation_ranges(&[])?;
}
if journaled {
    Self::maybe_checkpoint(guard)?;
}
```

## Encrypted Filesystem Semantics

On encrypted images, **only the directory entry's SIV-encrypted filename is recomputed** for the new `(parent_inode_id, new_name)` tuple. Payload blocks are never re-encrypted because AEAD is bound to the stable `inode_id`.

```rust
// src/disk.rs:8040-8041
let old_stored = Self::stored_filename(guard, old_parent_inode_id, old_name)?.into_owned();
let new_stored = Self::stored_filename(guard, new_parent_inode_id, new_name)?.into_owned();
```

The `stored_filename` function derives the SIV ciphertext using the parent inode ID as additional authenticated data (AAD):

```rust
// src/disk.rs:2478-2495
fn stored_filename<'a>(
    guard: &DiskManagerInner,
    parent_inode_id: u64,
    name: &'a str,
) -> Result<std::borrow::Cow<'a, str>, DiskManagerError> {
    if let Some(key) = &guard.encryption_key {
        let enc = crate::encryption::encrypt_filename(key, parent_inode_id, name)?;
        Ok(std::borrow::Cow::Owned(enc))
    } else {
        Ok(std::borrow::Cow::Borrowed(name))
    }
}
```

The encryption implementation (`src/encryption.rs:665-710`) uses XChaCha20-Poly1305 with a SIV-derived nonce:
- AAD = `parent_inode_id` (8 bytes LE)
- Nonce = HMAC-SHA256(filename_key, parent_inode_id || name) truncated to 24 bytes
- Ciphertext = `_e_` + Base64URL(nonce || ciphertext || tag)

Cross-directory rename therefore produces a **different on-disk directory entry name** while the payload blocks remain byte-for-byte identical.

## MetadataOp Journal Framing

All mutating metadata operations (`create_file`, `create_directory`, `delete_file`, `rename`, `mkdir`, `truncate`) use the same `MetadataOp` WAL framing. The enum variants are absolute post-image assignments, making redo replay idempotent and repeatable.

```rust
// src/journal.rs:308-360
pub enum MetadataOp {
    SetInodeBitmap { inode_id: u64, allocated: bool },
    SetDataBitmap { block_id: u64, allocated: bool },
    SetDataBitmapRange { start_block: u64, count: u32, allocated: bool },
    WriteInode { inode_id: u64, inode_bytes: Box<[u8; INODE_BYTES]> },
    WriteBlockSlice { block_id: u64, offset: u32, data: Vec<u8> },
}
```

**Key properties:**
- Every variant writes a **complete post-image** (no deltas).
- `SetDataBitmapRange` coalesces adjacent bitmap bit updates.
- `WriteBlockSlice` supports partial block updates for directory entry mutations.
- Transaction commit order: bitmap frees → block writes → inode writes (phase order enforced by `coalesce_ops`).

## Crash Recovery & Atomicity Guarantees

1. **WAL-first ordering**: `commit_journal_tx` flushes the journal region (including header cursor update) **before** returning. In-place metadata is applied by `apply_journal_ops` only after the WAL frame is durable.
2. **Idempotent redo**: Recovery scans the journal ring, decodes frames, and re-applies all `MetadataOp` variants. Because each op is an absolute post-image, replay is safe regardless of how many times it runs.
3. **Single-transaction rename**: The entire rename (source unlink, optional target overwrite + block/inode release, destination link, both parent inode updates) commits in **one CRC32C-protected WAL frame**. No partial rename state can survive a crash.
4. **Checkpointing**: `maybe_checkpoint` advances the checkpoint pointer after groups of transactions, bounding replay work.

## Related Pages

- [Disk Manager & Persistence](disk_manager_and_persistence.md) — WAL, journal layout, durability modes
- [Inode & Directory](inode_and_directory.md) — directory entry format, inode structure, block allocation
- [Journal](journal.md) — `MetadataOp` encoding, ring buffer, recovery, coalescing

## Tests

Key integration tests in `tests/rename_test.rs`:
- `test_rename_matrix_plain_journaled_and_file_io` — same-dir, cross-dir, overwrite, type-mismatch, cycle prevention, fsck integrity across all storage/journal combinations
- `test_rename_encrypted_cross_directory_recomputes_siv_without_touching_payload` — verifies SIV filename recomputation with untouched payload blocks on encrypted images
- `test_rename_read_only_and_fuse_wire_protocol` — FUSE_RENAME/RENAME2 wire format, NOREPLACE flag, read-only rejection
