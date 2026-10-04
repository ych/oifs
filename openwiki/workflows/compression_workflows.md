---
type: workflow
title: Compression and Filter Workflows
description: Guide to using Blosc2 pre-compression filters, compression modes, and filter pipelines for optimal space savings.
tags: [compression, filters, workflow, blosc2]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-04T10:12:53.730Z
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-95790d7cf7011b1d80e862fd
    resource: repo://tests/compression_modes_test.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-04T10:12:53.730Z" }
---

## Overview

OIFS integrates Blosc2 compression with configurable pre-compression filters to achieve optimal space savings. The system supports three compression modes and a flexible filter pipeline system that can be tuned for specific data patterns.

## Compression Modes

Compression mode determines when Blosc2 compression is applied during write operations:

- **Always**: Compress all data regardless of size
- **Never**: Store data uncompressed (compressed_size = 0)
- **Auto** (default): Compress only when data size >= 8KB

The compression mode is specified per write operation and stored in the inode metadata. During read operations, the system automatically decompresses data if it was stored compressed.

<!-- openwiki: broken internal link [../operations/testing_and_verification.md#compression-mode-enum] heading anchor "compression-mode-enum" does not exist in "../operations/testing_and_verification.md". Fix the href or restore the target, then delete this comment. -->
See [CompressionMode enum](../operations/testing_and_verification.md#compression-mode-enum) for implementation details.

## Pre-compression Filters

Before Blosc2 compression, data can be processed through pre-compression filters to increase compressibility:

### Filter Types

1. **Delta Encoding** (`FilterType::Delta`)
   - Computes differences between consecutive values
   - Most effective for sequential data, time series, or slowly changing values
   - Operates on integer types (typesize 1, 2, 4, or 8 bytes)

2. **Byte Shuffle** (`FilterType::ByteShuffle`)
   - Transposes bytes across adjacent elements
   - Groups similar bit patterns together (e.g., all MSBs, then all next bits)
   - Effective for integer and floating-point data with limited value ranges

3. **Bit Shuffle** (`FilterType::BitShuffle`)
   - Transposes bits across adjacent elements
   - Provides even finer-grained pattern grouping than byte shuffle
   - Particularly effective for binary/boolean data and bitfields

4. **Truncation Precision** (`FilterType::TruncPrecision`)
   - Zeros least-significant mantissa bits of floating-point values
   - Controlled by `prec_bits` parameter (number of bits to preserve)
   - Lossy filter: trades precision for compression

### Filter Configuration

Filters are configured via `FilterConfig` which specifies:
- `typesize`: Element size in bytes (1, 2, 4, or 8) for shuffle/delta operations
- Boolean flags for each filter type (`delta`, `shuffle`, `bitshuffle`)
- For truncation precision: `prec_bits` field in `FilterType` enum

Common preset configurations:
- `FilterConfig::none()`: No filters
- `FilterConfig::numeric(typesize)`: Delta + Byte Shuffle (standard for numeric data)
- `FilterConfig::delta_only(typesize)`: Delta encoding only
- `FilterConfig::shuffle_only(typesize)`: Byte shuffle only
- `FilterConfig::bitshuffle_only(typesize)`: Bit shuffle only

## Filter Pipelines

Multiple filters can be chained into a `FilterPipeline` that applies them in sequence:
- Forward pipeline applies filters in specified order during compression
- Reverse pipeline applies inverse filters in reverse order during decompression
- Truncation precision is lossy and only applies in the forward direction

Example pipeline creation:
```rust
let pipeline = FilterPipeline::new(4)  // 4-byte typesize
    .then(FilterType::Delta)
    .then(FilterType::ByteShuffle);
```

## Recommended Workflows

### 1. Numeric Sequential Data
For sequential numbers, timestamps, or sensor readings:
```rust
// Write with delta + byte shuffle pipeline
session.write_data_with_filters(
    file_id,
    0,
    &numeric_data,
    CompressionMode::Auto,
    FilterConfig::numeric(4),  // For 32-bit integers
)?;
// Read automatically reverses the pipeline
let data = session.read_data(file_id)?;
```

### 2. Floating-Point Sensor Data
For floating-point measurements where some precision loss is acceptable:
```rust
// Write with precision truncation followed by bit shuffle
session.write_data_with_filters(
    file_id,
    0,
    &sensor_data,
    CompressionMode::Auto,
    FilterConfig {
        typesize: 4,  // 32-bit floats
        delta: false,
        shuffle: false,
        bitshuffle: true,
        // Note: trunc_precision requires direct FilterType usage
        // See FilterPipeline::then(FilterType::TruncPrecision { prec_bits })
    },
)?;
// Decompression handles bit shuffle inverse; truncation is not reversed
```

### 3. Binary/Bitfield Data
For flags, masks, or binary arrays:
```rust
// Write with bit shuffle only
session.write_data_with_filters(
    file_id,
    0,
    &bitfield_data,
    CompressionMode::Always,
    FilterConfig::bitshuffle_only(1),  // 1-byte typesize for bit arrays
)?;
// Read applies bit shuffle inverse
```

### 4. Maximum Compression Ratio
When space is critical and CPU time is available:
```rust
// Combine multiple filters with highest compression level
session.write_data_with_filters(
    file_id,
    0,
    &data,
    CompressionMode::Always,
    FilterConfig::numeric(8),  // Delta + byte shuffle for 64-bit data
)?;
// System uses Blosc2's default codec (zstd) with clevel from context
```

## Filter Recommendation System

The system includes a filter recommendation tool that analyzes data entropy to suggest optimal filter configurations:
- `calculate_entropy()`: Measures Shannon entropy of raw data
- `recommend_filters()`: Tests common filter configurations and returns the one achieving smallest compressed size
- Used in `test_filter_recommendation_tool` to automatically select delta for sequential data

## Integration with Compression Modes

Filters and compression modes work orthogonally:
- Filters are applied before Blosc2 compression regardless of mode
- In `Never` mode, filters still process data but the result is stored uncompressed
- Filter configuration is stored in inode metadata and transparently applied during read

## Testing and Verification

See the following test files for practical examples:
<!-- openwiki: broken internal link [../tests/filter_test.rs] file "../tests/filter_test.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Filter pipelines test](../tests/filter_test.rs): Demonstrates roundtrip correctness for various filter combinations
<!-- openwiki: broken internal link [../tests/compression_modes_test.rs] file "../tests/compression_modes_test.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Compression modes test](../tests/compression_modes_test.rs): Validates Always/Never/Auto behavior
<!-- openwiki: broken internal link [../tests/filter_test.rs] file "../tests/filter_test.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Filter recommendation test](../tests/filter_test.rs): Shows automated filter selection based on entropy analysis

## Implementation Notes

- Filter configuration is compactly stored in inode metadata for persistence
- All filter operations are zero-copy where possible, using in-place transformations
- The system handles endianness and alignment transparently for portable data
- Lossy filters (truncation precision) are clearly marked and only apply in compression direction
