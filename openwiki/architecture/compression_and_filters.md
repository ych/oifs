---
type: architecture
title: Compression and Pre-compression Filters
description: How OIFS combines zstd compression (with multi-frame EOF append, seekable 64K chunked compression with COW extent swap, and read-modify-recompress fallback) with Blosc2-style pre-compression filters (Delta, ByteShuffle, BitShuffle, TruncPrecision).
tags: [compression, zstd, filters, blosc2, entropy, cow, zero-copy, seekable-64k]
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-388414179b3b4a07da1a42a4
    resource: repo://src/filters.rs
  - id: openwiki-source-bc305a37042018e1ebd6d860
    resource: repo://src/inode.rs
---

## Responsibility and ownership

The write, read, and filter pipeline live across two modules. `write_data_with_filters` (`src/disk.rs#L3308-L3450`) orchestrates the full staging sequence — pre-compression filter → zstd compression → encryption — while `filters.rs` (`src/filters.rs`) provides the filter primitives, the config stored on the inode, the native C-Blosc2 wrapper, and entropy analysis.

## The write-stage pipeline

Every initial write (`file_offset == 0`) flows through `plan_write_prepared` (`src/disk.rs#L3208-L3306`), which applies a fixed, ordered staging pipeline. The reverse order is used on read (decrypt → decompress → unapply filter).

1. **Pre-compression filter** — `apply_filters_cow` transforms the raw payload into a more compressible form. This runs *before* compression so filters can collapse Shannon entropy that a generic LZ77 compressor cannot reuse.
2. **Compression** — governed by `CompressionMode` (`src/disk.rs#L66-L78`):
   - `Always`: always zstd (level 0).
   - `Never`: never compress.
   - `Auto` (default): compress only when the filtered data is ≥ 8KB, and only keep the compressed result if it is actually smaller than the working data.
   - `Seekable { level }`: fixed 64KB chunked compression with O(1) random access.
3. **Encryption** — if a key is present, the (already compressed) data is encrypted with XChaCha20-Poly1305 and a fresh per-file nonce. Compressing before encrypting preserves compression ratio while keeping ciphertext entropy flat.

The ordering filter→compress→encrypt is why both `inode.compressed_size` (physical) and `inode.size` (logical) are tracked separately, and why the filter flags (`filter_typesize`, `filter_delta`, `filter_shuffle`, `filter_bitshuffle`) must be recorded on the inode to reverse the pipeline on read.

## Zero-copy filter staging

`apply_filters_cow` (`src/filters.rs#L642-L655`) returns a `Cow<[u8]>`: `Cow::Borrowed(data)` (zero allocation) when the filter is inactive or the typesize is not one of {1, 2, 4, 8}, and `Cow::Owned` only when a pipeline actually runs. On the common unfiltered/unencrypted path this eliminates every intermediate buffer copy — reported as up to ~28,500x speedup on 1MB payloads — and lets the uncompressed bytes flow straight into the write.

## Compression modes and append behavior

`write_data_with_filters` (`src/disk.rs#L3308-L3450`) handles non-offset-0 writes with three distinct strategies for already-compressed files:

- **Seekable 64K Chunked Compression (P4.2 fast path)** — When `INODE_FLAG_SEEKABLE_64K` is active or `CompressionMode::Seekable` is selected, writes enter `write_chunked_64k_internal` (`src/disk.rs#L3826-L3980`). Only affected 64KB chunks are re-encoded and swapped via COW extents, eliminating full-file rewrites.
- **Zstd Multi-Frame Append (stream fast path)** — When appending strictly at EOF (`file_offset == inode.size`) to an *unencrypted* stream-compressed file with *no* active filters, the new chunk is compressed as an independent Zstd frame and appended at `compressed_size`. Because `zstd::stream::decode_all` transparently decodes concatenated frames, no existing block is decompressed or rewritten.
- **Read-Modify-Recompress (stream fallback)** — For encrypted stream files, random-offset stream writes, or files with active filters, the full payload is decompressed, the change is spliced in, old blocks are freed, and the file is rewritten contiguously from offset 0 with an effective filter config that recovers the inode's stored filter flags.

Raw (uncompressed) files use a plain in-place `write_buffer_at_offset` write that does not participate in the frame pipeline.

## Seekable 64KB Chunked Compression (P4.2)

To avoid $O(N)$ full-payload decompression on random reads and partial writes, OIFS implements **Seekable 64KB Chunked Compression**:

- **Fixed 64KB Logical Chunks**: Files are partitioned into uniform 64KB chunks (`CHUNK_SIZE_64K = 65536`).
- **64-bit ChunkEntry**: Stored directly in the inode block table, each 8-byte entry packs `start_block: u32`, `block_count: u8`, `flags: u8` (`FLAG_RAW` or `FLAG_COMPRESSED`), and `compressed_len: u16`.
- **Copy-on-Write (COW) Extent Swap**: Updating data at any offset (`write_chunked_64k_internal`, `src/disk.rs#L3826-L3980`) allocates new physical contiguous blocks, writes compressed or raw bytes, atomically updates the chunk pointer (`set_logical_chunk_entry`), and then frees the old blocks.
- **Anti-Inflation Fallback**: If a compressed chunk does not strictly save space (or exceeds 64KB), it is stored as uncompressed `FLAG_RAW` blocks, eliminating negative compression overhead.
- **$O(1)$ Random Reads**: `read_at_chunked_64k` (`src/disk.rs#L3753-L3824`) decodes only the single targeted 64KB chunk directly into the caller's buffer without decompressing the entire file, completely bypassing stateful file offset tracking.

## Blosc2-style pre-compression filters

`filters.rs` ships four composable filters (`FilterType`), each lossless except TruncPrecision:

- **Delta** — first-order difference of typed elements (`delta_encode_inplace`), collapsing linear/temporal sequences into small integers.
- **ByteShuffle** — transposes Array-of-Structures to Structure-of-Arrays, clustering high-order bytes (`shuffle_encode`).
- **BitShuffle** — 8×8 bit-matrix transposition per 64-bit word using the symmetric Delta-Swap `transpose_8x8_u64` primitive, effective for sparse/bitfield data.
- **TruncPrecision** — zeroes least-significant mantissa bits of f32/f64 (`trunc_precision_encode_inplace`); it is lossy, so its inverse is identity.

These are orchestrated by `FilterPipeline` (`src/filters.rs#L19-L70`), whose `apply` runs filters in order and `unapply` runs them in reverse. The compact `FilterConfig` (`src/filters.rs#L99-L186`) stored on the inode exposes factory helpers (`numeric`, `delta_only`, `shuffle_only`, `bitshuffle_only`, `custom`) and `to_pipeline`. Native C-Blosc2 integration is available via `blosc2_compress` / `blosc2_decompress` (`src/filters.rs#L73-L95`), which build `CParams` and use the compiled C-Blosc2 chunk codec.

## Entropy analysis and recommendation

`calculate_entropy` (`src/filters.rs#L668-L685`) computes Shannon entropy in bits/byte, and `recommend_filters` (`src/filters.rs#L710-L770`) evaluates 14 candidate pipelines (raw Delta/Shuffle/BitShuffle at typesizes 1/2/4/8 plus the Delta+Shuffle `numeric` combos at 2/4/8), compresses each, and selects the configuration with the smallest resulting size. This backs the `filter-analyze` and `put --filter auto` CLI commands. The whole filter family is covered by Kani bijectivity proofs (`src/filters.rs`), including the unaligned-tail case and composite pipelines.

## Extension seams

Filters are layered on compression rather than fused into it: any new filter is a new `FilterType` plus matching (un)apply functions, and the pipeline applies and reverses them generically. `FilterConfig` is the single seam between the filter layer and the inode, so new filters require no changes to the write or read paths beyond adding the boolean/byte fields only if the compact config needs more independent settings.
