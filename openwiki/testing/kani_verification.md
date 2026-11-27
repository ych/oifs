---
type: concept
title: Kani Formal Verification
description: AWS Kani Rust Verifier is used to mathematically prove safety and correctness properties across the entire input space for core OIFS components, with 131 formal proofs of bijectivity, arithmetic safety, layout validity, and filter roundtrip fidelity across 17 modules.
tags: [formal-verification, kani, safety-properties, bijectivity, smt-solving, model-checking]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-478599bafaa2666d34684cda
    resource: repo://docs/formal_verification/README.md
  - id: openwiki-source-57692e9ab78d05d0aeba3e7c
    resource: repo://src/allocator.rs
  - id: openwiki-source-69dc9c7ca45b38a30db2f06f
    resource: repo://src/bitmap.rs
  - id: openwiki-source-577ab4c8720ea065ae15ce27
    resource: repo://src/directory.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-7d8517379a7d69084d77cf05
    resource: repo://src/disk/resize.rs
  - id: openwiki-source-388414179b3b4a07da1a42a4
    resource: repo://src/filters.rs
  - id: openwiki-source-bc305a37042018e1ebd6d860
    resource: repo://src/inode.rs
  - id: openwiki-source-813113339610c0170804687f
    resource: repo://src/journal.rs
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

# Kani Formal Verification

OIFS integrates the **AWS Kani Rust Verifier** to provide machine-checked mathematical proofs of critical safety and correctness properties. Unlike empirical testing that checks specific concrete inputs, Kani uses symbolic execution and SAT/SMT solving to verify properties hold for **all possible inputs** within defined bounds.

Proofs are embedded directly in production source modules under `#[cfg(kani)]` modules named `kani_proofs` or `verification`, ensuring they evolve alongside the code they verify.

## Verification Scope

The verification strategy focuses on five property categories:

| Category | What It Covers |
| :--- | :--- |
| **Bijectivity & Roundtrip** | Encoding/decoding, serialization/deserialization, and mapping functions are perfect inverses |
| **Arithmetic Safety** | Absence of overflow, underflow, division by zero, and wrap-around bugs |
| **Memory Safety** | Bounds checks, out-of-bounds access prevention, panic freedom |
| **Layout Correctness** | On-disk structures maintain required ordering, alignment, and non-overlap |
| **State Machine Invariants** | Valid transitions, consistency conditions, and protocol compliance |

## Proof Catalog by Module

### Filter Pipeline Correctness (`src/filters.rs` — 16 proofs)
The filter subsystem proves SIMD-friendly pre-compression transformations are correct:
- Delta encoding/decoding roundtrip for u16, u32, u64 elements (in-place and out-of-place)
- Byte shuffle and bit shuffle involution properties (T(T(x)) = x)
- Full pipeline roundtrip fidelity (Delta → Shuffle → unshuffle → undelta)
- Edge cases: wrapping arithmetic, zero typesize, unaligned tails
- Transpose operations as strict mathematical involutions (`proof_transpose_8x8_u64_involution`)
- Arbitrary symbolic typesize never panics (`proof_delta_arbitrary_typesize_panic_free`)

### Inode Pointer Validity (`src/inode.rs` — 9 proofs)
Logical-to-physical block mapping proofs:
- Complete coverage of all 262,144 logical block indices (`proof_block_path_roundtrip_all_indices`)
- Exact invertibility: no two logical blocks map to same physical slot
- Tier boundary validation (direct/single/double/triple indirect transitions)
- Zero-initialization of new inodes and absence of dangling pointers
- `ChunkEntry` bitfield serialization roundtrip and field isolation
- State predicate mutual exclusivity (empty/compressed/raw)

### Superblock Layout (`src/superblock.rs` — 8 proofs)
Layout ordering and geometry proofs:
- Correct block allocation sequencing: superblock < inode bitmap < data bitmap < inode table < data area
- Root inode fixed at block 0
- Minimum data block availability (≥1 data block)
- Proper inode count scaling for small vs large filesystems (256-byte records → 16,384 inodes max)
- Journaled layout non-overlapping with journal region
- Validation acceptance of well-formed superblocks
- Usable inode count always fits within table capacity

### Journal & WAL Correctness (`src/journal.rs` — 15 proofs)
Write-ahead log proofs:
- Ring buffer cursor arithmetic never leaves the ring (`proof_ring_cursor_advance_stays_in_ring`)
- CRC32C detects every single-bit flip (torn write detection)
- Frame length overflow rejection before out-of-bounds reads
- Metadata operation idempotency (SetInodeBitmap, SetDataBitmap, WriteBlockSlice, WriteInode, SetDataBitmapRange)
- Coalescing of adjacent same-polarity bitmap spans and contiguous block slices
- JournalState encode/decode bijection and corruption rejection
- MetadataOp encode/decode roundtrip with size agreement
- Checkpoint empties ring; append state-transition invariants
- `used()` calculation invariants under all head/tail configurations
- Ring append bounds guarantee (wrapping slice indexing never exceeds capacity)

### Block Allocator (`src/allocator.rs` — 5 proofs)
- Allocation returns block ID ≥ start_block_offset and within bitmap range
- Allocate/free roundtrip makes block re-allocatable
- Consecutive allocations never return same block
- Exhaustion behavior (returns NoSpace error)
- Freeing below offset is safe (no side effects)

### Bitmap Operations (`src/bitmap.rs` — 6 proofs)
- Set/clear roundtrip restores original bit state
- Set isolation: only target bit modified
- `find_first_free` returns genuinely free bit, all prior bits set
- Out-of-bounds get() returns false (no panic)
- `find_contiguous_free` returns range where all bits free
- Wrap-around allocator scan soundness (`find_next_free_wrapped`)

### Directory Operations (`src/directory.rs` — 8 proofs)
- `find_entry_in_block` soundness and mismatch handling
- Insert offset calculation correctness and bounds safety
- Memory safety on corrupt/adversarial on-disk data (no panic, no OOB reads)
- Hash filter soundness (no false negatives)
- Hash-filtered lookup equivalent to plain lookup
- Directory block count sizing exactness
- SipHash panic freedom for empty/short/multi-chunk inputs

### DiskManager Core (`src/disk.rs` — 5 proofs in `verification` module)
- DurabilityMode conversion soundness and range-based classification
- Block mapping checked arithmetic prevents wrap-around to block 0 (superblock)
- Inode byte-range checked bounds prevent underflow on large inode_id
- Full overwrite size invariant (truncation vs. max preservation)
- Read slice clamping soundness (no overflow/inversion)

### Seekable 64K Chunked Compression (`src/disk.rs` — 2 proofs)
- Chunk index/offset slicing strict safety and byte conservation
- Indirect subtree hole skipping forward progress and conservation

### Auto-Grow & Vacuum Resize (`src/disk/resize.rs` — 4 proofs)
- Target block monotonicity and max-size cap enforcement
- Vacuum target computation soundness and no overflow
- Expansion targets respect inode/table limits
- Relayout copy-run coalescing conservation

### WriteBatch & Coalescing (`src/disk/batch.rs` — 5 proofs)
- Range coalescing with gap invariants (block-aligned, non-overlapping, complete coverage)
- Entry encode/decode bijection with size agreement
- Unknown tag rejection
- Flush bounds within image limits

### Transfer & Splice Gates (`src/disk/transfer.rs` — 6 proofs)
- Splice offset/length checked arithmetic
- Copy range clamping and conservation
- Zero-copy fast-path preservation
- Atomic write boundaries

### 128B Cache Padding (`src/disk/caches.rs` — 3 proofs)
- Shard index in-bounds and bijection for consecutive inode IDs
- Cache eviction invariants
- Entry serialization roundtrip

### NFSv3/FUSE Protocols (`src/nfs.rs`, `src/fuse.rs` — 8 proofs)
- FUSE request/response encode-decode roundtrip
- NFSv3 compound operation bounds
- Filehandle serialization bijection
- Attribute mask field isolation

### Encryption & Zstd Pool (`src/encryption.rs`, `src/zstd_pool.rs` — 4 proofs)
- AES-GCM nonce uniqueness and counter overflow prevention
- Dictionary lifetime and reference counting
- Compression level clamping
- Frame size bounds

### IPC & FFI (`src/ipc.rs`, `src/ffi.rs` — 4 proofs)
- Message framing checksum verification
- FFI boundary panic freedom
- Capability token validation

### I/O Engine & Auto-Defrag (`src/io_engine.rs`, `src/auto_defrag.rs` — 3 proofs)
- IO completion ordering invariants
- Defragmentation relocation bounds

## Proof Structure

All proofs follow a consistent pattern embedded in source modules:

```rust
#[cfg(kani)]
mod kani_proofs {
    use super::*;

    #[kani::proof]
    fn proof_descriptive_name() {
        // 1. Create symbolic inputs with kani::any()
        let input: Type = kani::any();
        
        // 2. Constrain inputs with kani::assume() to valid domain
        kani::assume(input.in_valid_range());
        
        // 3. Execute the operation under verification
        let result = operation(input);
        
        // 4. Assert the property holds for all possible inputs
        assert!(property_holds(result));
        
        // 5. Optional: cover! marks important branches for coverage
        kani::cover!(edge_case_reached, "description");
    }
}
```

Key Kani primitives used:
- `kani::any()` — generates completely unconstrained symbolic value
- `kani::assume(cond)` — restricts symbolic state space to valid inputs
- `kani::cover!(cond, "label")` — marks branch as coverage target
- `#[kani::unwind(N)]` — bounds loop unwinding for termination

## Running Proofs

### Run all 131 proofs in parallel (recommended)
```bash
cargo kani --lib -j 8 --output-format=terse
```

### Run individual harnesses
```bash
# Filter pipeline
cargo kani --lib --harness proof_delta_roundtrip_u32
cargo kani --lib --harness proof_full_pipeline_roundtrip
cargo kani --lib --harness proof_transpose_8x8_u64_involution

# Inode pointer validity
cargo kani --lib --harness proof_block_path_roundtrip_all_indices
cargo kani --lib --harness proof_chunk_entry_roundtrip_all

# Superblock layout
cargo kani --lib --harness proof_superblock_layout_ordering
cargo kani --lib --harness proof_in_place_resize_bounds_sound

# Journal WAL
cargo kani --lib --harness proof_crc32c_detects_single_bit_flip
cargo kani --lib --harness proof_ring_append_state_transition_invariants

# Block allocator
cargo kani --lib --harness proof_allocate_returns_valid_id
cargo kani --lib --harness proof_allocate_free_roundtrip

# Bitmap operations
cargo kani --lib --harness proof_set_clear_roundtrip
cargo kani --lib --harness proof_find_next_free_wrapped_soundness

# Directory operations
cargo kani --lib --harness proof_find_entry_in_block_soundness
cargo kani --lib --harness proof_hash_filter_soundness

# DiskManager core
cargo kani --lib --harness proof_get_block_checked_arithmetic_prevents_wrap_around
cargo kani --lib --harness proof_write_from_start_size_invariant
```

### Run proofs for a specific module
```bash
# All filters proofs
cargo kani -p oifs --lib --harness proof_delta_roundtrip_u32 \
  --harness proof_shuffle_roundtrip \
  --harness proof_full_pipeline_roundtrip \
  --harness proof_transpose_8x8_u64_involution \
  --harness proof_delta_wrapping_extremes \
  --harness proof_delta_only_roundtrip \
  --harness proof_shuffle_only_roundtrip \
  --harness proof_unaligned_tail_roundtrip \
  --harness proof_bitshuffle_roundtrip \
  --harness proof_pipeline_composite \
  --harness proof_delta_roundtrip_u16 \
  --harness proof_delta_roundtrip_u64 \
  --harness proof_trunc_precision_zero_bits_identity \
  --harness proof_delta_zero_typesize_panic_free \
  --harness proof_delta_arbitrary_typesize_panic_free
```

### CI integration (with unstable features enabled)
```bash
cargo kani --lib -j 8 --output-format=terse --enable-unstable
```

## Interpreting Results

### Successful Verification
```
Checking proof_allocate_returns_valid_id ...
VERIFICATION:- SUCCESSFUL
Checking proof_allocate_free_roundtrip ...
VERIFICATION:- SUCCESSFUL
```
All assertions hold for every possible input within assumed constraints.

### Counterexample Found
```
Checking proof_xyz ...
VERIFICATION:- FAILED
Counterexample:
  offset = 1234
  block_id = 0
Assertion failed: block_id >= offset
```
Kani produces a concrete input that violates the property. Examine the counterexample to fix the bug or adjust assumptions.

### Unwinding Assertion
```
Unwinding assertion failed on loop at src/bitmap.rs:123
```
Increase `#[kani::unwind(N)]` bound or restructure the loop for bounded iteration.

### Memory Exhaustion / Timeout
```
Error: Out of memory during symbolic execution
```
Reduce state space: add tighter `kani::assume()` bounds, decrease array sizes, or split into smaller proofs.

## Relationship to Other Verification Tiers

Kani operates at the **Symbolic SMT** tier in OIFS's multi-tiered verification:

| Tier | Tool | Scope |
| :--- | :--- | :--- |
| **Language** | RustBelt (Coq) | Data-race freedom via Rust type system |
| **Concurrency** | Shuttle (DPOR) | Thread interleaving, torn read prevention |
| **Symbolic SMT** | **AWS Kani (CBMC/CaDiCaL)** | **Exhaustive per-function proofs (131 harnesses)** |
| **Protocol** | TLA+ (TLC) | COW extent pointer linearizability |

Kani proofs complement (not replace) unit/integration tests: tests check specific scenarios, Kani checks *all* scenarios within bounds.

## Extending Proof Coverage

When adding new code requiring verification:

1. **Add `#[cfg(kani)] mod kani_proofs`** in the same source file
2. **Use descriptive names**: `proof_<function>_<property>`
3. **Constrain with `assume`**: Only verify valid input domains
4. **Cover edge cases**: Use `kani::cover!` for branch coverage
5. **Keep proofs fast**: Small arrays, bounded loops, focused properties
6. **Run locally**: `cargo kani --lib --harness <name>` before committing

## Proof Maintenance

- **Always run `cargo kani`** after modifying verified code
- **Update assumptions** when valid input domains change
- **Refactor proofs** when internal APIs change (they live next to the code)
- **Monitor CI** for regressions — proofs run on every PR
