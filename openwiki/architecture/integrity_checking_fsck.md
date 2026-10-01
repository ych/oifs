---
type: architecture
title: Filesystem Integrity Checking (fsck)
description: How the OIFS fsck structural consistency scanner performs set-theoretic verification between bitmap allocation states and directory reachability to detect orphan inodes, leaked blocks, missing blocks, and cross-linked blocks.
tags: [fsck, integrity, consistency, bitmaps, verification, orphan-inodes, leaked-blocks, cross-linked-blocks]
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
generated: { by: "pi", at: "2026-09-29T16:14:34.721Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-09-30T20:17:56.754Z
---

## Responsibility and ownership

<!-- openwiki: broken internal link [src/disk.rs#L1762-L1877] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Filesystem structural consistency verification is owned by [`DiskManager::verify_integrity`](src/disk.rs#L1762-L1877).

It serves as the OIFS equivalents of `fsck` (filesystem consistency check). Unlike continuous runtime operations that trust filesystem invariants, `verify_integrity` treats the raw on-disk state as untrusted, scanning block bitmaps, the inode table, and directory pointer trees to diagnose storage corruption, crash remnants, or software bugs.

<!-- openwiki: broken internal link [src/disk.rs#L115-L126] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/bin/oifs.rs#L619-L650] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Results are encapsulated within [`FsckReport`](src/disk.rs#L115-L126), which is exposed directly to the CLI command `oifs fsck` ([`src/bin/oifs.rs`](src/bin/oifs.rs#L619-L650)) and over the IPC layer via `IpcRequest::Fsck`.

## The consistency model: allocation vs reachability

Filesystem consistency in OIFS requires that physical allocation states strictly match logical directory reachability:

$$\text{Allocated in Bitmaps} \iff \text{Reachable from Root Directory}$$

Corruption occurs whenever there is a divergence between what the bitmaps declare and what the inode and directory structures actually reference.

```
       [On-Disk Bitmaps]                       [Directory & Inode Tree]
(inode_bitmap / data_bitmap)                 (Root Inode 0 -> Dirs -> Files)
             │                                              │
             ▼                                              ▼
   Allocated Bit Sets                             Referenced Target Sets
             │                                              │
             └─────────────── Set Differences ──────────────┘
                                     │
         ┌───────────────────────────┼───────────────────────────┐
         ▼                           ▼                           ▼
   Orphan Inodes               Leaked Blocks               Missing Blocks
 (Allocated Inode,           (Allocated Block,           (Referenced Block,
  No Directory Ref)           No Inode Pointer)           Free in Bitmap)
                                     │
                                     ▼
                            Cross-Linked Blocks
                        (Block mapped by > 1 Inode)
```

## The 4 structural corruption categories

The integrity scanner partitions discrepancies into four formal failure modes:

| Defect Category | Field in `FsckReport` | Mathematical Definition | Severity / Impact |
| :--- | :--- | :--- | :--- |
| **Orphan Inodes** | `orphan_inodes` | $\text{AllocatedInodes} \setminus \text{ReferencedInodes}$ | Medium: Inodes occupy space in the inode table but cannot be accessed or unlinked by users. |
| **Leaked Blocks** | `leaked_blocks` | $\text{AllocatedBlocks} \setminus \text{ReferencedBlocks}$ | Low: Physical storage capacity is wasted because free blocks are marked in-use. |
| **Missing Blocks** | `missing_blocks` | $\text{ReferencedBlocks} \setminus \text{AllocatedBlocks}$ | **Critical**: Active files reference blocks that the block allocator considers free; future writes will overwrite and corrupt active file data. |
| **Cross-Linked Blocks**| `cross_linked_blocks` | $\{b \mid \text{Count}(\text{References}(b)) > 1\}$ | **Critical**: Multiple distinct inodes point to the exact same physical block; modifying one file silently corrupts another. |

## Verification algorithm

<!-- openwiki: broken internal link [src/disk.rs#L1762-L1877] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`DiskManager::verify_integrity`](src/disk.rs#L1762-L1877) operates under a shared read lock (`self.inner.read().unwrap()`), allowing concurrent inspections without halting read workloads.

### Step 1: Bitmap ground-truth harvesting

<!-- openwiki: broken internal link [src/bitmap.rs#L94-L108] file "src/bitmap.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Using 64-bit word scanning via [`BitmapRef::for_each_set_bit`](src/bitmap.rs#L94-L108), the scanner collects all physically marked bits:
- `allocated_inodes` (`src/disk.rs#L1767-L1774`): Scans `superblock.inode_bitmap_block` across `superblock.inode_count`.
- `allocated_data_blocks` (`src/disk.rs#L1777-L1786`): Scans `superblock.data_bitmap_block` starting at `superblock.data_block_start`.

Because `for_each_set_bit` advances using CPU bitwise intrinsics (`word &= word - 1`), scanning hundreds of thousands of blocks completes in sub-millisecond time.

### Step 2: Queue-based directory tree traversal

Starting from `superblock.root_inode` (inode 0), the scanner performs a breadth-first search (BFS) across the directory hierarchy (`src/disk.rs#L1796-L1820`):

```rust
let mut queue = vec![sb.root_inode];
let mut visited = std::collections::HashSet::new();

while let Some(dir_id) = queue.pop() {
    if !visited.insert(dir_id) { continue; }
    let dir_inode = Self::read_inode_internal(&guard, dir_id)?;
    if dir_inode.mode != crate::inode::FileType::Directory { continue; }
    let block_id = dir_inode.blocks[0];
    if block_id == 0 { continue; }

    if let Some(block_data) = Self::get_block_from_map(&guard.mmap, block_id) {
        for entry in crate::directory::DirectoryIterator::new(block_data).flatten() {
            referenced_inodes.insert(entry.inode);
            if let Ok(child_inode) = Self::read_inode_internal(&guard, entry.inode)
                && child_inode.mode == crate::inode::FileType::Directory {
                    queue.push(entry.inode);
            }
        }
    }
}
```

- Tracks visited directories in `visited` to prevent infinite loops caused by circular directory links.
<!-- openwiki: broken internal link [src/directory.rs] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- Uses streaming [`DirectoryIterator`](src/directory.rs) to collect all reachable child inodes.

### Step 3: Block reference resolution and cross-link detection

<!-- openwiki: broken internal link [src/disk.rs#L1822-L1833] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
For every reachable inode in `referenced_inodes`, [`DiskManager::collect_inode_blocks`](src/disk.rs#L1822-L1833) extracts all physical block pointers:
- Resolves all 10 direct blocks, 512 single indirect blocks, $512^2$ double indirect blocks, and $512^3$ triple indirect blocks.
- Appends each block reference to `referenced_data_blocks: HashMap<u64, Vec<u64>>`.
- If a physical block is encountered more than once across any files (`entries.len() > 1`), it is immediately recorded in `cross_linked_blocks`.

### Step 4: Set-theoretic difference analysis

The scanner executes difference queries (`src/disk.rs#L1835-L1859`):
1. Any inode in `allocated_inodes` missing from `referenced_inodes` is appended to `orphan_inodes`.
2. Any block in `allocated_data_blocks` missing from `referenced_data_blocks` is appended to `leaked_blocks`.
3. Any block in `referenced_data_blocks` missing from `allocated_data_blocks` is appended to `missing_blocks`.

Lists are deterministically sorted, and `is_clean` is evaluated:

```rust
let is_clean = orphan_inodes.is_empty()
    && leaked_blocks.is_empty()
    && missing_blocks.is_empty()
    && cross_linked_blocks.is_empty();
```

## The FsckReport structure

<!-- openwiki: broken internal link [src/disk.rs#L115-L126] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The resulting report is defined in [`src/disk.rs#L115-L126`](src/disk.rs#L115-L126):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsckReport {
    pub is_clean: bool,
    pub orphan_inodes: Vec<u64>,
    pub leaked_blocks: Vec<u64>,
    pub missing_blocks: Vec<u64>,
    pub cross_linked_blocks: Vec<u64>,
}
```

## CLI integration and automation

<!-- openwiki: broken internal link [src/bin/oifs.rs#L619-L650] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The `oifs fsck` command ([`src/bin/oifs.rs#L619-L650`](src/bin/oifs.rs#L619-L650)) provides human and machine interfaces:

### Human-readable terminal output

```bash
$ oifs fsck test.img

=== Filesystem Consistency Check (fsck) ===
Status:              ✅ CLEAN
Orphan Inodes:       0
Leaked Blocks:       0
Missing Blocks:      0
Cross-linked Blocks: 0
```

When corruption is detected, `Status` displays `❌ CORRUPTED` along with lists of affected inode and block IDs.

### Machine-readable JSON output (`--json`)

Adding `--json` outputs structured JSON suitable for monitoring agents and CI/CD pipelines:

```json
{
  "is_clean": true,
  "orphan_inodes": [],
  "leaked_blocks": [],
  "missing_blocks": [],
  "cross_linked_blocks": []
}
```
