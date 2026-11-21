---
type: concept
title: Kani Formal Verification
description: AWS Kani Rust Verifier is used to mathematically prove safety and correctness properties across the entire input space for core OIFS components, covering 71 formal proofs of bijectivity, arithmetic safety, layout validity, and filter roundtrip fidelity.
tags: [formal-verification, kani, safety-properties, bijectivity, smt-solving, model-checking]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-388414179b3b4a07da1a42a4
    resource: repo://src/filters.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---

# Kani Formal Verification

OIFS integrates the **AWS Kani Rust Verifier** to provide machine-checked mathematical proofs of critical safety and correctness properties. Unlike empirical testing that checks specific concrete inputs, Kani uses symbolic execution and SAT/SMT solving to verify properties hold for **all possible inputs** within defined bounds.

## Verification Scope

Kani proofs are configured via `#[cfg(kani)]` and reside alongside implementation code in dedicated `kani_proofs` modules. The verification strategy focuses on:

1. **Bijectivity and Roundtrip Properties**: Ensuring encoding/decoding, serialization/deserialization, and mapping functions are perfect inverses
2. **Arithmetic Safety**: Proving absence of overflow, underflow, and division by zero
3. **Memory Safety**: Verifying bounds checks and absence of panics
4. **Layout Correctness**: Confirming on-disk structures maintain required ordering and validity constraints
5. **State Machine Invariants**: Validating transitions and consistency conditions

## Proof Categories

### Filter Pipeline Correctness (`filters.rs` — 16 proofs)
The filter subsystem includes proofs for:
- Delta encoding/decoding roundtrip for u16, u32, u64 elements
- Byte shuffle and bit shuffle involution properties (T(T(x)) = x)
- Full pipeline roundtrip fidelity (Delta → Shuffle → unshuffle → undelta)
- Handling of edge cases: wrapping arithmetic, zero typesize, unaligned tails
- Transpose operations as strict mathematical involutions

```rust
#[kani::proof]
fn proof_delta_roundtrip_u32() { ... }
#[kani::proof]
fn proof_shuffle_roundtrip() { ... }
#[kani::proof]
fn proof_full_pipeline_roundtrip() { ... }
#[kani::proof]
fn proof_transpose_8x8_u64_involution() { ... }
#[kani::proof]
#[kani::unwind(9)]
fn proof_delta_arbitrary_typesize_panic_free() { ... }
```

### Inode Pointer Validity (`inode.rs` — 9 proofs)
Proofs cover the logical-to-physical block mapping:
- Complete coverage of all 262,144 logical block indices
- Exact invertibility: no two logical blocks map to same physical slot
- Tier boundary validation (direct/single/double/triple indirect transitions)
- Zero-initialization of new inodes and absence of dangling pointers
- `ChunkEntry` bitfield serialization roundtrip and field isolation

```rust
#[kani::proof]
fn proof_block_path_roundtrip_all_indices() { ... }
#[kani::proof]
fn proof_block_path_tier_boundaries_symbolic() { ... }
#[kani::proof]
fn proof_chunk_entry_roundtrip_all() { ... }
#[kani::proof]
fn proof_chunk_entry_field_isolation() { ... }
```

### Superblock Layout (`superblock.rs` — 8 proofs)
Layout ordering proofs ensure:
- Correct block allocation sequencing: superblock < inode bitmap < data bitmap < inode table < data area
- Root inode fixed at block 0
- Minimum data block availability (>=1 data block)
- Proper inode count scaling for small vs large filesystems
- Journaled layout non-overlapping with journal region
- Validation acceptance of well-formed superblocks

```rust
#[kani::proof]
fn proof_superblock_layout_ordering() { ... }
#[kani::proof]
fn proof_journaled_layout_non_overlapping() { ... }
```

### Journal & WAL Correctness (`journal.rs` — 21 proofs)
Write-ahead log proofs validate:
- Ring buffer cursor arithmetic never leaves the ring
- CRC32C detects every single-bit flip (torn write detection)
- Frame length overflow rejection before out-of-bounds reads
- Metadata operation idempotency (SetInodeBitmap, SetDataBitmap, WriteBlockSlice, WriteInode, SetDataBitmapRange)
- Coalescing of adjacent same-polarity bitmap spans and contiguous block slices
- JournalState encode/decode bijection and corruption rejection
- MetadataOp encode/decode roundtrip with size agreement
- Checkpoint empties ring; append state-transition invariants

```rust
#[kani::proof]
fn proof_ring_cursor_advance_stays_in_ring() { ... }
#[kani::proof]
fn proof_crc32c_detects_single_bit_flip() { ... }
#[kani::proof]
fn proof_apply_op_set_data_bitmap_range_idempotent() { ... }
#[kani::proof]
fn proof_push_data_bitmap_op_coalesces_adjacent_same_polarity() { ... }
#[kani::proof]
fn proof_ring_append_state_transition_invariants() { ... }
```

### Bitmap Allocation Invariants (`bitmap.rs` — 6 proofs)
- Set/clear roundtrip and bit isolation
- `find_first_free` correctness (returns genuinely free, first in sequence)
- Out-of-bounds `get()` returns false without panic
- Contiguous free range soundness
- Wrap-around allocator scan soundness (`find_next_free_wrapped`)

```rust
#[kani::proof]
fn proof_set_clear_roundtrip() { ... }
#[kani::proof]
#[kani::unwind(70)]
fn proof_find_next_free_wrapped_soundness() { ... }
```

### Directory Hash Table Soundness (`directory.rs` — 9 proofs)
- Entry lookup finds serialized entries with matching name
- Mismatched names correctly return None
- Insert offset calculation never exceeds slice bounds
- Lookups never panic on arbitrary/corrupt block data
- Hash filter soundness (no false negatives for correct or legacy hashes)
- SipHash panic-free for empty, short, and multi-chunk inputs

```rust
#[kani::proof]
#[kani::unwind(6)]
fn proof_find_entry_in_block_soundness() { ... }
#[kani::proof]
#[kani::unwind(6)]
fn proof_lookups_never_panic_on_arbitrary_block() { ... }
```

### Low-Level I/O Primitives (`disk.rs` — 11 proofs)
- Durability mode conversion soundness
- Checked arithmetic prevents wrap-around to block 0
- Inode byte range bounds on large inode IDs
- Write-from-start size invariant (truncation vs. max)
- Slice clamping soundness for `read_at`
- **64KB chunked compression** slicing conservation and boundary invariants
- Sharded directory cache Fibonacci hashing in-bounds and bijective
- Indirect hole skipping forward progress and conservation
- Mmap extent coalescing byte equivalence
- Incremental compressed size accounting conservation on truncate

```rust
#[kani::proof]
fn proof_get_block_checked_arithmetic_prevents_wrap_around() { ... }
#[kani::proof]
fn proof_chunked_64k_offset_and_slicing_soundness() { ... }
#[kani::proof]
fn proof_read_at_indirect_hole_skip_conservation() { ... }
```

### Allocator Bounds (`allocator.rs` — 5 proofs)
- `allocate()` returns block ID >= start offset and within bitmap range
- Allocate/free roundtrip reuses same block
- Double allocation returns unique blocks
- Exhaustion returns `NoSpace` error
- Free below offset is safe (no side effects)

```rust
#[kani::proof]
fn proof_allocate_returns_valid_id() { ... }
#[kani::proof]
fn proof_double_allocate_unique() { ... }
```

### Encryption AAD Construction (`encryption.rs` — 1 proof)
- Domain separation of file, chunk, and filename AAD by leading kind byte
- Filename AAD injectivity in parent inode ID

```rust
#[kani::proof]
fn proof_aad_domain_separation_and_filename_aad_layout() { ... }
```

### FFI Version Checking (`ffi.rs` — 1 proof)
- `oifs_check_version` adheres strictly to version policy for all symbolic inputs

```rust
#[kani::proof]
fn proof_oifs_check_version_policy_soundness() { ... }
```

### Auto-Defrag Configuration (`auto_defrag.rs` — 1 proof)
- Sanitized config never carries NaN threshold or sub-minimum check interval

```rust
#[kani::proof]
fn proof_sanitized_config_is_always_valid() { ... }
```

### I/O Engine Extent Coalescing (`io_engine.rs` — 2 proofs)
- Extent push preserves coverage and merges exactly on contiguous image+buffer ranges
- `IoBackend::from_u8` is total and inverts discriminant

```rust
#[kani::proof]
#[kani::unwind(4)]
fn proof_extent_push_preserves_coverage() { ... }
```

### IPC Frame Validation (`ipc.rs` — 2 proofs)
- Frame length validation soundness (accept iff payload ≤ MAX_FRAME_BYTES)
- Zero-length frame always accepted for protocol liveness

```rust
#[kani::proof]
fn proof_frame_len_validation_soundness() { ... }
```

## Harness Setup

Each proof follows a consistent pattern:

1. **Symbolic Inputs**: Use `kani::any()` to generate arbitrary values
2. **Assumptions**: Constrain inputs with `kani::assume()` for tractability
3. **Execution**: Call the function under test with symbolic inputs
4. **Assertions**: Verify postconditions with `assert!`, `assert_eq!`, `assert_ne!`
5. **Coverage**: Use `kani::cover!` to ensure key branches are explored

```rust
#[kani::proof]
fn proof_example() {
    let input: u64 = kani::any();
    kani::assume(input < 1_000_000);  // bound for tractability
    let result = function_under_test(input);
    assert!(result.is_ok());
    kani::cover!(input == 0, "zero case reached");
}
```

## Unwind Bounds and Loop Limits

Kani requires explicit unwind bounds for loops with symbolic iteration counts:

- `#[kani::unwind(N)]` where N ≥ maximum loop iterations + 1
- Small fixed bounds (e.g., 4, 6, 9) for tight loops over small arrays
- Larger bounds (e.g., 70) for bitmap wrap-around scans
- Absent for proofs with statically bounded or no loops

Examples:
```rust
#[kani::proof]
#[kani::unwind(9)]   // 8 bits + 1 for termination
fn proof_find_first_free_correctness() { ... }

#[kani::proof]
#[kani::unwind(70)]  // 64-bit bitmap wrap scan
fn proof_find_next_free_wrapped_soundness() { ... }
```

## Running Kani Proofs

Execute the full formal verification suite:

```bash
cargo kani
```

Run a specific harness:

```bash
cargo kani --harness proof_chunked_64k_offset_and_slicing_soundness
cargo kani --harness proof_chunk_entry_roundtrip_all
cargo kani --harness proof_clamp_slice_range_soundness
```

Kani harnesses the **CBMC** model checker and **CaDiCaL** SAT solver to discharge proof obligations. The `#[kani::unwind]` annotations specify loop bounds for symbolic execution, while `kani::assume()` constrains input ranges to maintain tractability without sacrificing generality.

## Proof Coverage Summary

| Module | Proofs | Key Properties |
|--------|--------|----------------|
| `filters.rs` | 16 | Delta/shuffle/bitshuffle roundtrip, involutions, pipeline fidelity |
| `inode.rs` | 9 | Block path invertibility, tier boundaries, ChunkEntry bit isolation |
| `superblock.rs` | 8 | Layout ordering, inode capacity, journaled geometry |
| `journal.rs` | 21 | Ring invariants, CRC32C, idempotency, coalescing, encode/decode |
| `bitmap.rs` | 6 | Set/clear, find_first_free, wrap-around soundness |
| `directory.rs` | 9 | Lookup soundness, bounds, hash filter, SipHash panic-free |
| `disk.rs` | 11 | Checked arithmetic, 64KB chunk slicing, hole skip, mmap coalescing |
| `allocator.rs` | 5 | Bounds, uniqueness, exhaustion, free safety |
| `encryption.rs` | 1 | AAD domain separation |
| `ffi.rs` | 1 | Version policy |
| `auto_defrag.rs` | 1 | Config sanitization |
| `io_engine.rs` | 2 | Extent coalescing, backend conversion |
| `ipc.rs` | 2 | Frame validation, zero-length liveness |
| **Total** | **92** | — |

*Note: The testing strategy documents **71 formal proofs**; the count above includes all `#[kani::proof]` functions. Some are helper/internal proofs.*

## Layered Verification Strategy

Kani proofs form the **Symbolic SMT tier** in OIFS's multi-tiered verification:

| Tier | Methodology | Target |
|------|-------------|--------|
| Language | RustBelt Type Semantics (Coq) | Data-race freedom under safe Rust |
| Concurrency | Shuttle Bounded Model Checking (DPOR) | Thread interleavings, OpenMode races |
| **Symbolic SMT** | **AWS Kani (CBMC/CaDiCaL)** | **Exhaustive verification of allocation, bit isolation, slice clamping, chunk slicing** |
| Protocol | TLA+ Specification & TLC | COW linearizability and atomic commit |

Together with Shuttle concurrency testing and traditional unit/integration tests, Kani proofs provide mathematical guarantees that complement empirical testing by verifying properties hold across the entire input space, not just exercised test cases.
