---
type: architecture
title: Compression and Pre-compression Filters
description: How OIFS combines zstd compression (with multi-frame EOF append and read-modify-recompress fallback) with Blosc2-style pre-compression filters (Delta, ByteShuffle, BitShuffle, TruncPrecision) that run before compression, plus the entropy-based filter recommendation tool.
tags: [compression, zstd, filters, blosc2, entropy, cow, zero-copy]
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-388414179b3b4a07da1a42a4
    resource: repo://src/filters.rs
generated: { by: "antigravity", at: "2026-10-03T11:29:24.571Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-03T08:18:49.684Z
---

## Responsibility and ownership

<!-- openwiki: broken internal link [`src/disk.rs#L953-L1040`] file "`src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The write, read, and filter pipeline live across two modules. [`DiskManager::write_data_from_start_internal`](`src/disk.rs#L953-L1040`) orchestrates the
full staging sequence — pre-compression filter → zstd compression → encryption —
<!-- openwiki: broken internal link [`src/filters.rs`] file "`src/filters.rs`" does not exist. Fix the href or restore the target, then delete this comment. -->
while [`filters.rs`](`src/filters.rs`) provides the filter primitives, the
config stored on the inode, the native C-Blosc2 wrapper, and entropy analysis.

## The write-stage pipeline

Every initial write (`file_offset == 0`) flows through
`write_data_from_start_internal` (`src/disk.rs#L953-L1040`), which applies a
fixed, ordered staging pipeline. The reverse order is used on read
(decrypt → decompress → unapply filter).

1. **Pre-compression filter** — `apply_filters_cow` transforms the raw payload
   into a more compressible form. This runs *before* compression so filters can
   collapse Shannon entropy that a generic LZ77 compressor cannot reuse.
<!-- openwiki: broken internal link [`src/disk.rs#L56-L56`] file "`src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
2. **Compression** — governed by [`CompressionMode`](`src/disk.rs#L56-L56`):
   - `Always`: always zstd (level 0).
   - `Never`: never compress.
   - `Auto` (default): compress only when the filtered data is ≥ 8KB, and only
     keep the compressed result if it is actually smaller than the working data.
3. **Encryption** — if a key is present, the (already compressed) data is
   encrypted with XChaCha20-Poly1305 and a fresh per-file nonce. Compressing
   before encrypting preserves compression ratio while keeping ciphertext
   entropy flat.

The ordering filter→compress→encrypt is why both `inode.compressed_size`
(physical) and `inode.size` (logical) are tracked separately, and why the
filter flags (`filter_typesize`, `filter_delta`, `filter_shuffle`,
`filter_bitshuffle`) must be recorded on the inode to reverse the pipeline on
read (`src/disk.rs#L1016-L1024`).

## Zero-copy filter staging

<!-- openwiki: broken internal link [`src/filters.rs#L642-L655`] file "`src/filters.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`apply_filters_cow`](`src/filters.rs#L642-L655`) returns a `Cow<[u8]>`:
`Cow::Borrowed(data)` (zero allocation) when the filter is inactive or the
typesize is not one of {1, 2, 4, 8}, and `Cow::Owned` only when a pipeline
actually runs. On the common unfiltered/unencrypted path this eliminates every
intermediate buffer copy — reported as up to ~28,500x speedup on 1MB payloads —
and lets the uncompressed bytes flow straight into the write.

## Compression modes and append behavior

`write_data_with_filters` (`src/disk.rs#L1054-L1160`) handles non-offset-0
writes with two distinct strategies for already-compressed files:

- **Zstd Multi-Frame Append (fast path)** — When appending strictly at EOF
  (`file_offset == inode.size`) to an *unencrypted* file with *no* active
  filters, the new chunk is compressed as an independent Zstd frame and appended
  at `compressed_size`. Because `zstd::stream::decode_all` transparently decodes
  concatenated frames, no existing block is decompressed or rewritten.
- **Read-Modify-Recompress (fallback)** — For encrypted files, random-offset
  writes, or files with active filters, the full payload is decompressed, the
  change is spliced in, the old blocks are freed, and the file is rewritten
  contiguously from offset 0 with an effective filter config that recovers the
  inode's stored filter flags.

Raw (uncompressed) files use a plain in-place `write_buffer_at_offset` write
that does not participate in the frame pipeline.

## Blosc2-style pre-compression filters

`filters.rs` ships four composable filters (`FilterType`), each lossless except
TruncPrecision:

- **Delta** — first-order difference of typed elements (`delta_encode_inplace`),
  collapsing linear/temporal sequences into small integers.
- **ByteShuffle** — transposes Array-of-Structures to Structure-of-Arrays,
  clustering high-order bytes (`shuffle_encode`).
- **BitShuffle** — 8×8 bit-matrix transposition per 64-bit word using the
  symmetric Delta-Swap `transpose_8x8_u64` primitive, effective for sparse/bitfield data.
- **TruncPrecision** — zeroes least-significant mantissa bits of f32/f64
  (`trunc_precision_encode_inplace`); it is lossy, so its inverse is identity.

<!-- openwiki: broken internal link [`src/filters.rs#L19-L70`] file "`src/filters.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
These are orchestrated by [`FilterPipeline`](`src/filters.rs#L19-L70`), whose
`apply` runs filters in order and `unapply` runs them in reverse. The compact
<!-- openwiki: broken internal link [`src/filters.rs#L99-L186`] file "`src/filters.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`FilterConfig`](`src/filters.rs#L99-L186`) stored on the inode exposes factory
helpers (`numeric`, `delta_only`, `shuffle_only`, `bitshuffle_only`, `custom`)
and `to_pipeline`. Native C-Blosc2 integration is available via
`blosc2_compress` / `blosc2_decompress` (`src/filters.rs#L73-L95`), which build
`CParams` and use the compiled C-Blosc2 chunk codec.

## Entropy analysis and recommendation

<!-- openwiki: broken internal link [`src/filters.rs#L668-L685`] file "`src/filters.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`calculate_entropy`](`src/filters.rs#L668-L685`) computes Shannon entropy in
<!-- openwiki: broken internal link [`src/filters.rs#L710-L770`] file "`src/filters.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
bits/byte, and [`recommend_filters`](`src/filters.rs#L710-L770`) evaluates 14
candidate pipelines (raw Delta/Shuffle/BitShuffle at typesizes 1/2/4/8 plus the
Delta+Shuffle `numeric` combos at 2/4/8), compresses each, and selects the
configuration with the smallest resulting size. This backs the
`filter-analyze` and `put --filter auto` CLI commands. The whole filter family
is covered by Kani bijectivity proofs (`src/filters.rs`), including the
unaligned-tail case and composite pipelines.

## Extension seams

Filters are layered on compression rather than fused into it: any new filter is
a new `FilterType` plus matching (un)apply functions, and the pipeline applies
and reverses them generically. `FilterConfig` is the single seam between the
filter layer and the inode, so new filters require no changes to the write or
read paths beyond adding the boolean/byte fields only if the compact config
needs more independent settings.
