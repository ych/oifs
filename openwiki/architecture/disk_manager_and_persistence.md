---
type: "Reference"
title: "DiskManager & Persistence"
description: "Central coordinator for OIFS storage: image open/create, superblock management, inode CRUD, data read/write with compression/encryption, durability modes, flush/msync, read-only mounts, and atomic rename."
tags: ["disk-manager", "persistence", "storage", "durability", "encryption", "compression", "superblock", "inode"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

# DiskManager & Persistence

[`DiskManager`](../../src/disk.rs#L324-L327) is the central coordinator of the OIFS storage engine. It provides the single authoritative interface between raw on-disk bytes and high-level filesystem operations (file and directory creation, reads, writes, deletion, path resolution, defragmentation, and integrity checks).

## DiskManagerInner State

Internally, state is maintained by `DiskManagerInner` ([`src/disk.rs#L191-L213`](../../src/disk.rs#L191-L213)), wrapped in an `Arc<RwLock<DiskManagerInner>>` ([`src/disk.rs#L324-L327`](../../src/disk.rs#L324-L327)):

- **`file`**: The underlying host image file handle with an exclusive POSIX advisory lock (`F_SETLK` for read-write, `F_RDLCK` for read-only).
- **`backing`**: The active storage engine (`ImageBacking`) supporting `Mmap` (shared `PROT_READ|PROT_WRITE`), `MmapReadOnly` (shared `PROT_READ`), `Pread`, and `IoUring` backends.
- **`superblock`**: In-memory cached copy of the deserialized `SuperBlock` ([`src/superblock.rs`](../../src/superblock.rs)).
- **`encryption_key`**: Derived cryptographic key (`Option<EncryptionKey>`) used for authenticated AEAD encryption.
- **`password_verifier`**: Keyed BLAKE2b verifier of the mount password for session re-authentication.
- **`free_block_hint`** and **`free_inode_hint`**: Allocation watermarks that convert sequential bitmap scans into amortized $O(1)$ operations.
- **`inode_cache`**: In-memory zero-copy cache (`RwLock<BoundedInodeCache>`) avoiding redundant bincode deserialization.
- **`dir_cache`**: Sharded per-directory name index (`ShardedDirCache`) providing $O(1)$ path queries.
- **`chunk_read_cache`**: `RwLock<ChunkReadCache>` — bounded sub-chunk decompression cache for seekable 64K `read_at`.
- **`decoded_blob_cache`**: `Mutex<DecodedBlobCache>` — bounded decoded-payload cache for non-seekable `read_at` (compressed/encrypted files ≤ 16 MiB).
- **`durability_mode`**: `AtomicU8` durability policy governing `msync` flush behavior on writes.
- **`pending_async_ranges`**: `Mutex<PendingAsyncRanges>` — batched dirty-range accumulator for `DurabilityMode::RangeAsync`.
- **`async_drain_ready`**: `Arc<AtomicBool>` — fast atomic flag set when `pending_async_ranges` reaches its drain threshold.
- **`io_engine`**: Pluggable data-block read engine supporting `Mmap`, `Pread`, and Linux `io_uring` backends.
- **`inode_write_gen`**: Per-inode write generation counters (indexed by `inode_id & 4095`) for lock-free Stage-1 → Stage-3 staleness detection.
- **`read_only`**: `bool` — strictly read-only mount (`F_RDLCK`, zero disk writes).
- **`zstd_dicts`**: `Arc<ZstdDictRegistry>` — custom Zstd dictionaries registered on this mount.
- **`auto_grow`**: `AutoGrowConfig` — policy controlling automatic image growth on block/inode exhaustion.

## Open / Create / Mount Flows

`DiskManager` exposes a family of factory methods that cover all common provisioning scenarios:

| Method | Use Case | Journal | Encryption | StorageMode |
|--------|----------|---------|------------|-------------|
| `open(path, size)` | Open existing or create new | No | No | Default (Mmap) |
| `open_with_options(path, size, pwd, journaled, mode)` | Full control | Opt-in | Opt-in | Explicit |
| `open_journaled(path, size)` | New or existing with WAL | Yes | No | Default |
| `create_encrypted(path, size, pwd)` | New encrypted FS | No | Yes | Default |
| `create_encrypted_journaled(path, size, pwd)` | New encrypted + WAL | Yes | Yes | Default |
| `open_read_only(path)` | **Read-only mount** | N/A | Optional | Default |
| `open_read_only_with_password(path, pwd)` | Encrypted read-only | N/A | Required | Default |
| `open_read_only_with_storage_mode(path, pwd, mode)` | Encrypted read-only + explicit I/O | N/A | Optional | Explicit |

### Mount Sequence

1. **File Lock Acquisition**  
   - Read-write: `F_SETLK` with `F_WRLCK` (exclusive) — fails if another process holds any lock.  
   - Read-only: `F_SETLK` with `F_RDLCK` (shared) — allows multiple concurrent readers; blocks writers.

2. **Superblock Read & Validation**  
   Reads block 0, deserializes `SuperBlock`, checks magic (`0x4F494653`), and validates geometry against image size.

3. **Encryption Key Derivation**  
   If `superblock.encrypted`:
   - Derives `EncryptionKey` from password + `encryption_salt` (Argon2id).
   - Computes `password_verifier` (keyed BLAKE2b) for session reuse.

4. **Storage Engine Initialization**  
   - `StorageMode::Mmap`: `mmap` with `PROT_READ|PROT_WRITE` (or `PROT_READ` for read-only).  
   - `StorageMode::FileIo`: `Pread`/`IoUring` with optional user-space page cache.  
   - Loads in-memory bitmap mirrors and journal region for `FileIo`.

5. **Journal Recovery (journaled images, non-read-only)**  
   - Snapshots the WAL ring, decodes & validates frames (CRC32C).  
   - Redo-replays every `MetadataOp` in-place against the image.  
   - Flushes replayed metadata, then resets ring header to `cleanly_unmounted = true`.

6. **Clean-shutdown Marker (non-journaled or read-only)**  
   Writes `cleanly_unmounted = true` into the journal header (journaled images) so next mount skips recovery scan.

7. **Root Inode Initialization (new images)**  
   Allocates inode 0 + one data block, writes root directory inode, flushes.

The resulting `DiskManager` is `Clone` + `Send + Sync`; all public methods acquire the inner `RwLock` as needed.

## Read-Only Mount Enforcement

`open_read_only*` variants enforce a strictly read-only mount at every layer:

1. **OS File Descriptor**: Opened `O_RDONLY` — succeeds on `0444` image files.
2. **Advisory Lock**: `fcntl(F_SETLK, F_RDLCK)` — multiple readers share the lock; any writer requesting `F_WRLCK` blocks.
3. **Memory Protection**: Under `StorageMode::Mmap`, maps `PROT_READ` (`MmapReadOnly` backing). No `PROT_WRITE` pages exist.
4. **Write Path Guards**: Every mutating public method begins with `if guard.read_only { return Err(ReadOnly); }`.
5. **Journal State Check**: On journaled images, verifies `0` unrecovered transactions remain (`is_journal_clean_read_only`). A dirty journal forces the caller to open read-write first.
6. **Drop Behavior**: `DiskManagerInner::drop` performs **zero** flushes when `read_only == true` (no `msync`, no bitmap writes).

```rust
// Enforced at the API boundary
pub fn write_inode(&self, inode_id: u64, inode: &Inode) -> Result<(), DiskManagerError> {
    let mut guard = self.inner.write().unwrap();
    if guard.read_only { return Err(DiskManagerError::ReadOnly); }
    // ...
}
```

## Durability Modes

[`DurabilityMode`](../../src/disk.rs#L145-L182) controls when and how modified pages reach stable storage. Shared `MAP_SHARED` mappings already survive process crashes (SIGKILL); these modes balance power-loss resilience against throughput.

| Mode | Behavior | Sync Barrier | Throughput (bulk writes) |
|------|----------|--------------|--------------------------|
| **Lazy** (default) | No per-mutation `msync`; relies on OS writeback, explicit `flush()`, or `Drop` | None per-op | **Baseline ×45** |
| **RangeAsync** | Accumulates mutated byte ranges; coalesces & issues `msync(MS_ASYNC)` in batches (64 ops / 4 MiB) outside write lock | Async, batched | ~Lazy |
| **Strict** | `msync(MS_SYNC)` on **every** mutation over coalesced ranges (256 KiB gap bridge) | Sync per-op | Baseline |
| **LegacyWholeMmapAsync** | `msync(MS_ASYNC)` on entire mapping after every mutation | Async, whole map | Slowest |

### Implementation Details

- **Range Coalescing**: `coalesce_ranges_with_gap(ranges, mmap_len, max_gap)` expands to page/block boundaries, sorts, merges, and bridges gaps ≤ `max_gap` (1 MiB for RangeAsync, 256 KiB for Strict).
- **Journal-Before-Metadata Ordering**: `sync_mutation_ranges` always flushes the WAL region **before** metadata ranges so a power loss never leaves metadata without its WAL record.
- **PendingAsyncRanges Drain**: Under `RangeAsync`, mutations push ranges into a `Mutex<PendingAsyncRanges>`; when `pending_ops ≥ 64` or `approx_bytes ≥ 4 MiB`, an atomic flag (`async_drain_ready`) triggers an async drain (under `sync_mutex`, no write lock held).

```rust
pub fn set_durability_mode(&self, mode: DurabilityMode) {
    let prev = { let g = self.inner.read().unwrap(); let p = g.durability_mode(); g.set_durability_mode(mode); p };
    if prev == DurabilityMode::RangeAsync && mode != DurabilityMode::RangeAsync {
        self.drain_pending_async_ranges(); // flush residual batch
    }
}
```

## Write Path: Three-Stage Pipeline

`write_data_with_filters` (the core write entrypoint) uses a lock-segmented pipeline that minimizes exclusive-lock hold time:

<!-- openwiki: mermaid parse failed and this diagram was converted to a text fence so it does not break rendering. Fix the diagram source and restore the mermaid fence. Parser error: Parse error on line 21: ...mit via journal<br/>(single WAL tx) or<b Expecting 'SQE', 'DOUBLECIRCLEEND', 'PE', '-)', 'STADIUMEND', 'SUBROUTINEEND', 'PIPE', 'CYLINDEREND', 'DIAMOND_STOP', 'TAGEND', 'TRAPEND', 'INVTRAPEND', 'UNICODE_TEXT', 'TEXT', 'TAGSTART', got 'PS' -->
```text
flowchart TB
    subgraph S1["Stage 1: Shared Read Lock (inner.read())"]
        A1[Read inode & detect<br/>Seekable 64K fast path] --> B1{Transformed payload?}
        B1 -->|No| C1[Capture initial_gen,<br/>size, flags, blocks]
        B1 -->|Yes| D1[Read & decode full<br/>file payload]
        C1 --> E1[Compute AAD for inode_id]
        D1 --> E1
        E1 --> F1[Release read lock]
    end

    subgraph S2["Stage 2: Zero Locks (CPU-bound)"]
        F1 --> G2[plan_write_prepared:<br/>filter → compress → encrypt]
        G2 --> H2[Produce WritePlan:<br/>FullOverwrite /<br/>CompressedAppend /<br/>Recompress / Raw]
    end

    subgraph S3["Stage 3: Exclusive Write Lock (inner.write())"]
        H2 --> I3[Re-read inode & verify<br/>inode_gen == initial_gen]
        I3 --> J3{Stale?}
        J3 -->|Yes| K3[Re-plan under lock]
        J3 -->|No| L3[Use prepared plan]
        K3 --> M3[Commit via journal<br/>(single WAL tx) or<br/>legacy in-place]
        L3 --> M3
        M3 --> N3[sync_mutation_ranges<br/>per durability mode]
        N3 --> O3[Release write lock]
    end
```

### Stage 1 — Inspection & Snapshot (Shared Lock)

- Acquires `inner.read()`.
- Reads inode; detects **Seekable 64K** (`INODE_FLAG_SEEKABLE_64K`) or **encrypted partial upgrade** (random-offset write on encrypted v2+ file).
- For transformed payloads (compressed / encrypted / filtered): reads & decodes the **entire** logical file (`read_data_internal`) to enable splicing.
- Captures `initial_gen = inode_write_gen[inode_id & 4095]`, size, compressed_size, flags, block pointers, encryption nonce, filter metadata.
- Computes stack-staged AAD (`file_aad_for_scheme_stack`) binding ciphertext to `(inode_id, encryption_version)`.
- Releases read lock immediately.

### Stage 2 — CPU-Bound Transformation (No Filesystem Locks)

`plan_write_prepared` executes the full filter → compress → encrypt pipeline **outside** any lock:

| WriteCase | Trigger | Action |
|-----------|---------|--------|
| **FullOverwrite** | `file_offset == 0` ∧ (`size == 0` ∨ `data.len() ≥ old_size` ∨ transformed) | Filter → compress (optional) → encrypt (optional) entire new payload |
| **CompressedAppend** | Appending at EOF to unencrypted, uncompressed, unfiltered stream | Compress new data as independent Zstd frame; append to physical end |
| **Recompress** | Transformed payload + non-full-overwrite (random offset, encryption, filters) | Decode existing → splice → filter → compress → encrypt from offset 0 |
| **Raw** | Uncompressed, unencrypted, unfiltered | Borrow caller slice directly; no transformation |

Returns a `WritePlan` containing `case`, `phys_off`, `buffer` (`Cow<[u8]>`), `new_size`, `new_compressed_size`, `encryption_nonce`.

### Stage 3 — Commit (Exclusive Lock)

- Acquires `inner.write()`.
- Re-reads inode; verifies `inode_gen == initial_gen` **and** all captured fields match. Mismatch → re-plan under lock.
- **Journaled images**: Stage payload + metadata ops into `AllocSim`, `commit_journal_tx` (WAL frame + `msync` per durability), `apply_journal_ops`, checkpoint if ring ≥ 50% full (`Strict` only).
- **Legacy images**: In-place bitmap updates, block writes, inode write, `sync_mutation_ranges`.
- Updates `inode_write_gen`, invalidates payload caches, bumps `free_block_hint`/`free_inode_hint`.

## Atomic Rename Implementation

[`DiskManager::rename`](../../src/disk.rs#L7948-L7963) provides POSIX `renameat2` semantics with crash safety via the metadata WAL.

### Semantics

- **Metadata-only**: Never touches file payload blocks. On encrypted images, only recomputes the directory entry's SIV-encrypted filename (payload AEAD is bound to `inode_id`).
- **POSIX Overwrite**:
  - File → File: atomic replace, old inode+blocks freed.
  - Dir → Dir: allowed only if target directory is empty.
  - File ↔ Dir mismatch: `ENOTDIR` / `EISDIR`.
  - Move into own subtree: `EINVAL`.
- **No-op**: `rename(x, y)` where `x == y` succeeds silently.

### Two-Phase Commit via Journal

1. **Stage Mutations In-Memory** (under `inner.write()`):
   - Loads source & destination directory blocks into a `BTreeMap<u64, Vec<u8>>` (composes multiple edits to the same block).
   - Removes old entry from source block.
   - Removes existing target entry (if overwriting).
   - Inserts new entry (with encrypted filename for `new_parent`) into destination block; grows directory if needed.
   - Prepares parent inode post-images (updated `size`, `modified_at`).
   - If overwriting: collects target inode's blocks for bitmap free ops.

2. **Single WAL Transaction** (journaled images):
   - `SetInodeBitmap(target_inode, false)` + `SetDataBitmap(block, false)` for all freed blocks.
   - `WriteBlockSlice` for every modified directory block.
   - `WriteInode` for source parent, dest parent (and new inode if created).
   - `commit_journal_tx` → `apply_journal_ops` → checkpoint check.

3. **Cache Invalidation & Sync**:
   - Removes overwritten inode from caches, bumps `inode_write_gen`.
   - Updates `dir_cache` for both parents (plain + stored name indices).
   - `sync_mutation_ranges` over inode bitmap, data bitmap, inode slots, modified directory blocks.

```rust
// Journaled path: single atomic transaction
if guard.superblock.has_journal_layout() {
    // Stage all ops...
    Self::commit_journal_tx(guard, &ops)?;
    Self::apply_journal_ops(guard, &ops)?;
    // Sync per durability mode
    guard.sync_mutation_ranges(&ranges)?;
    Self::maybe_checkpoint(guard)?;
}
```

## Read Path: `read_data_internal`

[`read_data_internal`](../../src/disk.rs#L4839-L5079) materializes a file's logical content from on-disk payload:

1. **Seekable 64K Fast Path**: Delegates to `read_chunked_64k_internal` (parallel chunk reads, per-chunk AEAD).
2. **Physical Size Resolution**: `physical_size = compressed_size > 0 ? compressed_size : size`.
3. **Block Walk**: `walk_payload_blocks` traverses the inode's block pointer hierarchy in logical order:
   - **Direct** (0..9): `inode.blocks[i]`
   - **Single Indirect** (10..521): Reads `inode.blocks[10]` pointer block; batch-resolves 512 `u64` entries.
   - **Double Indirect** (522..262665): Reads `inode.blocks[11]` DIB → per-SIB batch resolution.
   - **Triple Indirect** (262666..134480393): Reads `inode.triple_indirect` TIB → DIB → SIB → leaf.
   - Skips zero pointers and pointers outside `[data_start, block_count)` (sparse holes + safety).
4. **Zero-Copy Read**:
   - `Mmap` + `IoBackend::Mmap`: Coalesces physically contiguous runs, `memcpy` from mapping.
   - `Pread`/`IoUring`: Builds `ExtentList`, submits to `IoEngine::read_backing`.
5. **Reverse Pipeline** (in exact order):
   - **Decrypt**: XChaCha20-Poly1305 (if `inode.encrypted`) with stack-staged AAD.
   - **Decompress**: Zstd streaming (concatenated frames, dictionary support) with 256 MiB hard cap.
   - **Unapply Filters**: Delta / Shuffle / Bitshuffle / Typesize (if `filter_typesize > 0` etc.).

## Persistence Guarantees Summary

| Operation | Journaled (`Strict`) | Journaled (`RangeAsync`/`Lazy`) | Legacy |
|-----------|----------------------|----------------------------------|--------|
| `create_file` / `mkdir` | 1 sync (WAL + metadata) | Async batch | In-place + `msync` per mode |
| `write_data` (full overwrite) | 1 sync (WAL + payload + metadata) | Async batch | In-place + `msync` per mode |
| `rename` | 1 sync (single WAL tx) | Async batch | In-place + `msync` per mode |
| `truncate` (compressed/encrypted) | 1 sync | Async batch | In-place + `msync` per mode |
| `flush()` | Full `msync` (WAL → metadata → data) | Drains `RangeAsync` batch | Full `msync` |
| Clean `Drop` | WAL header + full backing `flush()` | Same | Full backing `flush()` |
| Crash mid-transaction | Mount-time Redo replay (idempotent) | Mount-time Redo replay | Potential corruption (no WAL) |

The **write-ahead rule** is invariant: the WAL frame describing a mutation reaches stable storage **before** the in-place metadata it describes. This holds across all durability modes because `sync_mutation_ranges` always flushes the journal region first.
