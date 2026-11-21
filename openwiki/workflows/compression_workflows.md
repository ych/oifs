---
type: "Reference"
title: "Analyze a host file (no image required)"
openwiki_generated: true
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-95790d7cf7011b1d80e862fd
    resource: repo://tests/compression_modes_test.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---


## Overview

OIFS integrates **zstd** compression with **Blosc2-style pre-compression filters** to achieve optimal space savings for diverse data patterns. The system supports **three compression modes**, a **flexible filter pipeline**, a **persistent scoped worker pool** for parallel chunk (de)compression, and two **read-acceleration caches** (`DecodedBlobCache` for stream files, `ChunkReadCache` for seekable chunked files).

This guide covers:
- Compression mode selection and CLI usage
- Filter pipeline design and the `filter-analyze` recommendation tool
- Read-path cache interaction (`read_at` fast paths)
- Decision flowchart: file size + access pattern → compression mode + filters

---

## Compression Modes

Compression mode determines **when** and **how** zstd compression is applied during write operations. The mode is stored in the inode metadata and automatically honored on read.

| Mode | CLI Flag | Behavior | Typical Use Case |
|------|----------|----------|------------------|
| **Auto** (default) | *(none)* | Compress only when filtered data ≥ 8 KiB **and** compressed result is strictly smaller. | General-purpose cold/warm storage |
| **Always** | `--compress` | Force zstd compression regardless of size or ratio. | Archival / cold storage where CPU is available |
| **Never** | `--no-compress` | Store raw bytes; filters still run but output is kept uncompressed. | High-entropy data (already compressed, encrypted) or latency-critical writes |
| **Seekable 64K** | `--chunked` / `--seekable` / `--chunked-64k` | Fixed 64 KiB logical chunks, each independently compressed/encrypted with COW extent swap. Random-access large files (databases, VM images, scientific arrays) |

> **Note**: The `--compress` flag in the CLI maps to `CompressionMode::Always` (stream compression with default level 0). There is no per-file CLI option to set an explicit zstd level for stream mode; the level is determined by the session context (default 0).

### Auto Mode Threshold

The 8 KiB threshold is evaluated **after** filters run. If the filtered payload is < 8 KiB, or if zstd does not shrink it, the file is stored as `FLAG_RAW` (uncompressed). This avoids negative compression overhead on small or incompressible data.

---

## Pre-compression Filters

Before zstd compression, data can be processed through a **filter pipeline** to increase compressibility. Filters are **lossless** (except `TruncPrecision`) and are recorded in the inode so the read path can reverse them automatically.

### Filter Types

| Filter | `FilterType` Variant | Best For | Notes |
|--------|----------------------|----------|-------|
| **Delta Encoding** | `Delta` | Sequential data, timestamps, sensor readings, monotonic sequences | Computes first-order differences per typed element (`typesize` ∈ {1,2,4,8}) |
| **Byte Shuffle** | `ByteShuffle` | Structured records / Array-of-Structures (AoS), integer/float data with limited range | Transposes bytes across adjacent elements (MSBs together, then next bytes, …) |
| **Bit Shuffle** | `BitShuffle` | Sparse bitmasks, boolean arrays, bitfields, binary data | 8×8 bit-matrix transposition per 64-bit word (symmetric Delta-Swap) |
| **Truncation Precision** | `TruncPrecision { prec_bits }` | Floating-point sensor data where controlled precision loss is acceptable | **Lossy**: zeros least-significant mantissa bits of f32/f64; inverse is identity |

### Filter Configuration

`FilterConfig` (stored compactly on the inode) specifies:
- `typesize`: element size in bytes (1, 2, 4, or 8) — required for shuffle/delta/bitshuffle
- Boolean flags: `delta`, `shuffle`, `bitshuffle`
- `TruncPrecision` is expressed via `FilterPipeline::then(FilterType::TruncPrecision { prec_bits })` directly

**Preset helpers**:
```rust
FilterConfig::none()                    // No filters
FilterConfig::numeric(typesize)         // Delta + ByteShuffle (standard for numeric data)
FilterConfig::delta_only(typesize)
FilterConfig::shuffle_only(typesize)
FilterConfig::bitshuffle_only(typesize)
```

### Filter Pipeline

Multiple filters chain into a `FilterPipeline`:
- **Forward** (compression): applies filters in declared order
- **Reverse** (decompression): applies inverses in reverse order
- `TruncPrecision` is **lossy** — only runs forward; reverse is a no-op

```rust
let pipeline = FilterPipeline::new(4)   // 4-byte typesize (u32/f32)
    .then(FilterType::Delta)
    .then(FilterType::ByteShuffle);
```

---

## Filter Recommendation: `oifs filter-analyze`

The `filter-analyze` subcommand (and `--filter auto` in `put`) **analyzes Shannon entropy** and evaluates **14 candidate pipelines** in parallel to recommend the optimal configuration.

### CLI Usage

```bash
# Analyze a host file (no image required)
oifs filter-analyze ./data.bin

# JSON output for automation
oifs filter-analyze ./data.bin --json

# Import with auto-selected filter
oifs -i disk.img put ./sensors.csv --filter auto
```

### What It Evaluates

| Candidate Pipeline | Description |
|--------------------|-------------|
| `none` | Raw zstd (baseline) |
| `delta` (typesize 1, 2, 4, 8) | First-order difference |
| `shuffle` (typesize 2, 4, 8) | Byte shuffle only |
| `bitshuffle` (typesize 2, 4, 8) | Bit shuffle only |
| `delta+shuffle` / `numeric` (typesize 2, 4, 8) | Combined numeric pipeline |

For files > 256 KiB, it samples **three 64 KiB windows** (head, middle, tail, each 8-byte aligned) and scales results. Evaluation runs in parallel across CPU cores via `std::thread::scope`.

### Example Output (Human-Readable)

```
=== OIFS Filter Recommendation Report for "data.bin" ===
Original Size:        1048576 bytes
Baseline Zstd Size:   128450 bytes (Entropy: 0.981 bits/byte)

Filter Pipeline              Entropy   Zstd Size    Ratio   Savings
--------------------------------------------------------------------------------
none                         0.981     128450      8.16x   87.8%
delta (ts=4)                 0.112      12450     84.22x   98.8% [*RECOMMENDED*]
shuffle (ts=4)               0.892      98200     10.68x   90.6%
...

Recommendation Rationale: Data displays strong linear/temporal correlation; first-order delta collapses dynamic range, shrinking entropy.
Recommended Blosc2 Filter(s): ["blosc2::Filter::Delta"]
Command to import with recommended filter:
  oifs -i <image.img> put "data.bin" --filter delta --typesize 4
```

### JSON Output Schema

```json
{
  "original_size": 1048576,
  "baseline_entropy": 0.981,
  "baseline_compressed_size": 128450,
  "best_config": { "delta": true, "shuffle": false, "bitshuffle": false, "typesize": 4 },
  "best_report": { ... },
  "candidates": [ ... ]
}
```

### Blosc2 Filter Equivalents

The recommendation report maps OIFS filter combos to native C-Blosc2 filters:

| OIFS Config | Blosc2 Filters |
|-------------|----------------|
| `delta` | `blosc2::Filter::Delta` |
| `shuffle` | `blosc2::Filter::ByteShuffle` |
| `bitshuffle` | `blosc2::Filter::BitShuffle` |
| `delta + shuffle` (`numeric`) | `[Delta, ByteShuffle]` |
| `none` | `blosc2::Filter::None` |

---

## Anti-Inflation Fallback (`FLAG_RAW`)

Both stream and seekable chunked compression enforce a **strict size check**: if the compressed payload is **not strictly smaller** than the input (after filters), the chunk/file is stored **uncompressed** with the `FLAG_RAW` bit set in its `ChunkEntry` (seekable) or as a raw stream frame.

This eliminates **negative compression overhead** (CPU waste + size increase) on:
- Already-compressed data (zip, gz, video, images)
- Encrypted blobs
- High-entropy / random data
- Small payloads where zstd framing overhead exceeds savings

---

## Read-Path Caches and `read_at` Interaction

OIFS maintains two distinct bounded caches to accelerate **positional reads** (`read_at` / `read_data_at`) on compressed/encrypted files.

### DecodedBlobCache — Stream Files (Non-Seekable)

| Property | Value |
|----------|-------|
| Purpose | Whole-file decoded payload cache for stream-compressed or whole-file encrypted files |
| Capacity | 4 entries (`MAX_ENTRIES`) |
| Per-entry limit | 16 MiB (`MAX_ENTRY_BYTES`) |
| Total cap | 32 MiB (`MAX_TOTAL_BYTES`) |
| Key | `(inode_id, size, compressed_size, first_block, nonce)` — any metadata change invalidates |
| Lookup | Shared read lock in `read_at_with_inode`; slices directly from `Arc<Vec<u8>>` |
| Insert | On cache miss for files ≤ 16 MiB: full decode → insert → slice |
| Eviction | LRU by insertion order; evicts when count or byte limit exceeded |

**Behavior**: Eliminates the **O(calls × file_size)** full-file decode cliff when callers issue sequential or random sub-file reads on non-seekable blobs.

### ChunkReadCache — Seekable 64K Chunked Files

| Property | Value |
|----------|-------|
| Purpose | Per-chunk decompression cache for 64 KiB seekable chunks |
| Capacity | 8 entries (`CAPACITY`) |
| Key | `(inode_id, chunk_idx, raw_entry, valid_len)` — chunk rewrite (new `raw_entry`) invalidates |
| Lookup | `copy_slice_if_hit` copies sub-range directly from cached `Arc<[u8]>` under shared read lock (zero `Arc` clone/drop on hit) |
| Prefetch | Sequential access detection triggers parallel prefetch of up to **4 subsequent chunks** via `par_for_index`, inserting all under a single write lock |

**Sequential Read-Ahead Logic** (in `read_at_chunked_64k`):
1. Current chunk **miss** in cache
2. Previous chunk (`chunk_idx - 1`) **hit** in cache
3. → Prefetch `min(4, remaining_chunks)` chunks in parallel
4. Insert prefetched chunks into cache

This gives **near-stream throughput** for sequential scans over chunked files while preserving O(1) random access.

---

## Persistent Scoped Worker Pool

Both chunked (de)compression and cache prefetch use a **persistent scoped worker pool** (`zstd_pool::par_for_index`) to avoid per-call thread creation and Zstd context allocation overhead.

### Architecture

- **Thread-local Zstd pools**: Each thread keeps one compressor per level + one decompressor in `thread_local!` storage. On drop, contexts return to a global `Mutex` pool (`MAX_GLOBAL_COMPRESSORS_PER_LEVEL = 32`, `MAX_GLOBAL_DECOMPRESSORS = 32`).
- **Worker threads**: `num_cpus - 1` threads (clamped to 1..15), named `oifs-chunk-w{N}`, spawned at first `par_for_index` call.
- **Dispatch**: Generation counter + `Condvar`; calling thread participates via dynamic work-stealing (`next.fetch_add`).
- **Fallback**: Sequential execution when `total <= 1` or another dispatch holds the pool lock (avoids oversubscription / re-entrant deadlocks).

### Used By

| Operation | Parallelism Scope |
|-----------|-------------------|
| `read_at_chunked_64k` (aligned, ≥ 192 KiB) | Full-file parallel decode across chunks |
| Chunked write pipeline (`write_chunked_64k_pipeline`) | Parallel chunk (re)compression |
| `ChunkReadCache` sequential prefetch | Up to 4 chunks in parallel |
| `filter-analyze` candidate evaluation | 14 pipelines in parallel (via `std::thread::scope`) |

**Key benefit**: Worker threads keep their **thread-local Zstd contexts warm** across calls — zero `pthread_create` / `ZSTD_createDCtx` churn on steady-state workloads.

---

## Decision Flowchart: File Size + Access Pattern → Compression Mode + Filters

<!-- openwiki: mermaid parse failed and this diagram was converted to a text fence so it does not break rendering. Fix the diagram source and restore the mermaid fence. Parser error: Parse error on line 7: ...M_Rand[Seekable 64K (-chunked)<br/>→ fi Expecting 'SQE', 'DOUBLECIRCLEEND', 'PE', '-)', 'STADIUMEND', 'SUBROUTINEEND', 'PIPE', 'CYLINDEREND', 'DIAMOND_STOP', 'TAGEND', 'TRAPEND', 'INVTRAPEND', 'UNICODE_TEXT', 'TEXT', 'TAGSTART', got '(-' -->
```text
flowchart TD
    Start([New file / workload]) --> Size{File size?}
    
    Size -->|< 8 KiB| Small[Auto mode<br/>→ likely FLAG_RAW<br/>No filter needed]
    Size -->|8 KiB – 64 MiB| Medium{Access pattern?}
    Size -->|> 64 MiB| Large{Access pattern?}
    
    Medium -->|Sequential / append-only| M_Seq[Auto mode + filter-analyze<br/>→ delta/shuffle for numeric<br/>→ Stream zstd frames]
    Medium -->|Random read/write| M_Rand[Seekable 64K (--chunked)<br/>→ filter-analyze per workload<br/>→ COW chunk updates]
    
    Large -->|Sequential scan / cold storage| L_Seq[--compress (Always) + filter-analyze<br/>→ Maximum ratio, CPU available<br/>→ Stream multi-frame append]
    Large -->|Random access / DB / VM image| L_Rand[Seekable 64K (--chunked) + filter-analyze<br/>→ O(1) random read/write<br/>→ ChunkReadCache read-ahead]
    Large -->|Mixed: mostly sequential,<br/>occasional random| L_Mix[Seekable 64K (--chunked)<br/>→ Best of both worlds<br/>→ Sequential read-ahead + random O(1)]
    
    %% Filter selection sub-flow
    FilterChoice{Data type?} -->|Time-series / monotonic| F_Delta[delta + numeric(typesize)]
    FilterChoice -->|Structured records (AoS)| F_Shuffle[shuffle / numeric(typesize)]
    FilterChoice -->|Bitmasks / boolean / sparse| F_Bit[bitshuffle(typesize)]
    FilterChoice -->|FP sensor data (lossy OK)| F_Trunc[TruncPrecision + bitshuffle]
    FilterChoice -->|Unknown / mixed| F_Auto[--filter auto (filter-analyze)]
    
    M_Seq -.-> FilterChoice
    M_Rand -.-> FilterChoice
    L_Seq -.-> FilterChoice
    L_Rand -.-> FilterChoice
    L_Mix -.-> FilterChoice
    
    Small -.-> Done[Write with recommended config]
    M_Seq -.-> Done
    M_Rand -.-> Done
    L_Seq -.-> Done
    L_Rand -.-> Done
    L_Mix -.-> Done
```

### Quick Reference Table

| Scenario | Recommended Mode | Filter Strategy | Cache Benefit |
|----------|------------------|-----------------|---------------|
| **Small config / log files** (< 8 KiB) | Auto (default) | `none` (filters add overhead) | N/A (stored raw) |
| **Append-only time-series** (8 KiB – 64 MiB) | Auto | `delta` (+ `shuffle` for numeric) | `DecodedBlobCache` on re-read |
| **Large sequential archives** (> 64 MiB, cold) | `--compress` (Always) | `filter-analyze` → `numeric`/`delta` | `DecodedBlobCache` (≤ 16 MiB files) |
| **Database / VM image / random access** | `--chunked` (Seekable 64K) | `filter-analyze` per column/table | `ChunkReadCache` + **sequential read-ahead** |
| **Scientific arrays (FP32/64)** | `--chunked` | `bitshuffle` (+ `TruncPrecision` if lossy OK) | `ChunkReadCache` read-ahead |
| **Bitmaps / boolean masks** | `--chunked` | `bitshuffle(typesize=1)` | `ChunkReadCache` |
| **Mixed sequential + random** | `--chunked` | `filter-analyze` | Both caches (mode-dependent) |

---

## Recommended Workflows by Data Pattern

### 1. Numeric Sequential Data (Timestamps, Sensor Readings, IDs)

```bash
# Auto-detect (recommended)
oifs -i disk.img put ./sensor.csv --filter auto

# Explicit: delta + byte shuffle for 32-bit integers
oifs -i disk.img put ./sensor.csv --filter both --typesize 4
```

**Read path**: Stream mode → `DecodedBlobCache` serves subsequent `read_at` calls from cached whole-file decode.

### 2. Floating-Point Sensor Data (Lossy Compression Acceptable)

```bash
# Bit shuffle + truncate to 16 mantissa bits (half precision)
# Requires programmatic FilterPipeline; no direct CLI flag for TruncPrecision yet
```

```rust
let pipeline = FilterPipeline::new(4)  // f32
    .then(FilterType::TruncPrecision { prec_bits: 16 })
    .then(FilterType::BitShuffle);
session.write_data_with_filters(file_id, 0, &data, CompressionMode::Auto, pipeline.to_config())?;
```

**Read path**: Bit shuffle reversed automatically; truncation is **not** reversed (lossy).

### 3. Binary / Bitfield Data (Flags, Masks, Bloom Filters)

```bash
# Bit shuffle for 1-byte elements (bit arrays)
oifs -i disk.img put ./bitmap.bin --filter bitshuffle --typesize 1
```

### 4. Structured Records / Array-of-Structures (Mixed Fields)

```bash
# Byte shuffle groups same-significance bytes (e.g., all MSBs together)
oifs -i disk.img put ./records.bin --filter shuffle --typesize 8
```

### 5. Maximum Compression Ratio (Archival, CPU Available)

```bash
# Force compression + auto filter selection
oifs -i disk.img put ./archive.dat --compress --filter auto
```

### 6. Random-Access Large Files (Databases, VM Images, > 64 MiB)

```bash
# Seekable 64K chunked mode
oifs -i disk.img put ./database.db --chunked --filter auto
```

**Read path**: `read_at_chunked_64k` decodes **only the touched 64 KiB chunk** directly into caller's buffer. Sequential scans trigger **prefetch of up to 4 chunks** into `ChunkReadCache`.

---

## Cache Invalidation Summary

Both caches are **automatically invalidated** on any operation that mutates the logical payload or its metadata:

| Operation | Invalidation |
|-----------|--------------|
| **Write** (any `write_data_with_filters`, including chunked) | `invalidate_payload_caches(inode_id)` |
| **Truncate** (`truncate`, `truncate_chunked_64k`) | `invalidate_payload_caches(inode_id)` |
| **Delete** (journal replay `SetInodeBitmap { allocated: false }`) | `invalidate_payload_caches(inode_id)` |
| **Migrate** (v1→v2 inode rewrite) | `guard.clear_payload_caches()` (both caches, all inodes) |
| **Defragment** (safe mode live-reload, in-place) | `guard.clear_payload_caches()` |
| **Reload** (after safe defrag atomic swap) | `guard.clear_payload_caches()` |

The cache key includes `compressed_size`, `first_block`, and `nonce`, so **any metadata change** (even without data modification) naturally misses and re-populates.

---

## Implementation Notes

- **Filter config persistence**: Compact bit-packed fields in inode (`filter_typesize`, `filter_delta`, `filter_shuffle`, `filter_bitshuffle`).
- **Zero-copy filter staging**: `apply_filters_cow` returns `Cow::Borrowed` when no filter runs, eliminating intermediate copies on the common unfiltered path.
- **Compression ordering**: filter → compress → encrypt. This preserves compressibility while keeping ciphertext entropy flat.
- **Kani proofs**: All filter round-trips (including composite pipelines and unaligned tails) are formally verified via `#[kani::proof]` in `src/filters.rs`.
- **Native C-Blosc2**: Available via `blosc2_compress` / `blosc2_decompress` in `filters.rs` when the `blosc2` feature is enabled.

---

## Related Pages

- [CLI Reference](../operations/cli_reference.md#2-put) — `put`, `filter-analyze`, `get`, `append` command details
- [Compression and Filters Architecture](../architecture/compression_and_filters.md) — Deep dive on pipeline, caches, worker pool, COW chunked layout
- [Async I/O and Engines](../architecture/async_io_and_engines.md) — Session modes, Master-Proxy concurrency
- [Encrypted Workflows](./encrypted_workflows.md) — Encryption interaction with compression modes
