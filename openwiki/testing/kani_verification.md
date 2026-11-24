---
type: concept
title: Kani Formal Verification
description: AWS Kani Rust Verifier is used to mathematically prove safety and correctness properties across the entire input space for core OIFS components, with 90+ formal proofs of bijectivity, arithmetic safety, layout validity, and filter roundtrip fidelity across 17 modules.
tags: [formal-verification, kani, safety-properties, bijectivity, smt-solving, model-checking]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-23T20:55:55.458Z
sources:
  - id: openwiki-source-388414179b3b4a07da1a42a4
    resource: repo://src/filters.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-23T20:55:55.458Z" }
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
- DurabilityMode from_u8 soundness and range-based classification
- Checked arithmetic prevents integer wrap-around (non-zero block_id never resolves to SuperBlock)
- Inode byte range checked bounds (never wraps to low memory)
- Write-from-start size invariant (full overwrite truncates, partial write preserves)
- Read-at slice clamping soundness (no overflow, no inverted ranges)

### Cross-Image Transfer (`src/disk/transfer.rs` — 4 proofs)
- Fast-path gate: raw transfer allowed iff both source and destination unencrypted
- Transfer tier combination: commutative, associative, idempotent, monotone
- Chunk rebase preserves all fields except start_block; errno classification accuracy
- Extent coalescing conserves exact count and ordered bijection of non-zero blocks

### Encryption (`src/encryption.rs` — 1 proof)
- AAD domain separation: file/chunk/filename AAD tags strictly separated by kind byte (0x01/0x02/0x03)
- Filename AAD injective in parent_inode (bijective encoding)

### FFI/C API (`src/ffi.rs` — 2 proofs)
- `oifs_check_version` policy soundness for all symbolic version tuples
- `policy_to_compression_mode` total, panic-free, correct precedence (Seekable64K > Raw > Stream > Auto)

### FUSE ABI (`src/fuse.rs` — 3 proofs)
- `inode_to_nodeid` / `nodeid_to_inode` strict bijection; root (0) ↔ FUSE_ROOT_ID (1); invalid nodeid 0 rejected
- 8-byte dirent alignment rounding never overflows, always multiple of 8, within [unaligned, unaligned+8)
- Frame header parse: no panic on arbitrary bytes; rejects <40 bytes; rejects length mismatch

### I/O Engine (`src/io_engine.rs` — 2 proofs)
- Extent coalescing preserves coverage; merges exactly when both disk and buffer ranges continue
- `IoBackend::from_u8` total and inverts repr(u8) discriminant

### IPC Protocol (`src/ipc.rs` — 2 proofs)
- Frame length validation: accepted iff payload ≤ MAX_FRAME_BYTES
- Zero-length frame always accepted (protocol liveness)

### NFSv3 Server (`src/nfs.rs` — 3 proofs)
- XDR padding aligns to 4 bytes without overflow; XdrReader bounds checks never panic
- ONC RPC TCP record-marking header parsing and RpcCallHeader::parse panic-free
- NfsFileHandle serialization (fs_generation, inode_id) ↔ [u8;16] lossless bijection; rejects non-16-byte slices

### Storage Backends (`src/storage.rs` — 3 proofs)
- BitmapSliceMut dirty range coverage and bounds (covers every bit in range, never exceeds slice)
- Cache shard index bounds (Metadata <16, Data <64) and Fibonacci mixing non-collision in top bits
- DataPageCache blocks_per_shard clamping against MAX_FILE_IO_CACHE_BYTES, no overflow
- Read-only access policy: read ops permitted, write/bitmap/journal/flush ops rejected when read_only=true

### Auto Defragmentation (`src/auto_defrag.rs` — 1 proof)
- Sanitized config never carries NaN threshold or zero-length check interval

### Zstd Worker Pool (`src/zstd_pool.rs` — 1 proof)
- Exact wakeup liveness: `workers_to_wake` never exceeds workers or total-1; sufficient reachability guarantees no deadlock with `notify_one()`

## Proof Maintenance Patterns

### Stubbing & Assumptions
- Use `kani::any()` for symbolic inputs
- Constrain with `kani::assume()` to restrict to valid ranges (e.g., `total_blocks >= 5 && total_blocks <= 1_000_000`)
- Use `kani::cover!()` to confirm specific branches/edge cases are exercised

### Loop Unwinding
- Add `#[kani::unwind(N)]` where CBMC needs explicit bounds
- Typical values: 4-9 for small loops, up to 70 for bitmap scans
- Example: `#[kani::unwind(9)] fn proof_delta_arbitrary_typesize_panic_free()`

### Harness Design
- Each proof is a `#[kani::proof]` function in a `mod kani_proofs { ... }` or `mod verification { ... }` block
- Proofs share helper functions with unit tests via `#[cfg(any(test, kani))]`
- Prefer small, focused proofs over monolithic ones

## Key Properties Verified

| Property | Modules | Technique |
| :--- | :--- | :--- |
| **Bijectivity** | filters, inode, superblock, journal, encryption, nfs, fuse | Roundtrip assertions (`assert_eq!(decode(encode(x)), x)`) |
| **Arithmetic Safety** | disk, allocator, superblock, journal, io_engine, storage | `checked_add`, `checked_mul`, `saturating_add` verified against symbolic inputs |
| **Bounds Safety** | directory, bitmap, disk, storage, io_engine | Symbolic indices with `assume` + `assert` on results |
| **Invariant Preservation** | journal, superblock, allocator, disk/transfer | State machine transitions verified with `cover!` for all branches |
| **Panic Freedom** | filters, directory, encryption, nfs, fuse, io_engine | Arbitrary byte buffers; prove no `panic!` or OOB access |

## Running Verification

### Prerequisites
```bash
rustup component add kani
cargo kani --setup  # Downloads CBMC/CaDiCaL
```

### Run All Proofs
```bash
cargo kani
```

### Run Specific Harness
```bash
cargo kani --harness proof_block_path_roundtrip_all_indices
cargo kani --harness proof_crc32c_detects_single_bit_flip
cargo kani --harness proof_fuse_nodeid_inode_bijection_and_root_mapping
```

### Run Proofs in a Module
```bash
cargo kani --package oifs --lib filters
```

### Verbose Output
```bash
cargo kani --harness proof_superblock_layout_ordering -v
```

### Increase Unwind Limit Globally
```bash
cargo kani --default-unwind 100
```

## Kani Integration in CI

Kani proofs run in CI via `.github/workflows/kani.yml` on every PR and push to main. The workflow:
1. Installs Kani toolchain
2. Runs `cargo kani` with a timeout
3. Reports pass/fail per harness
4. Fails the build on any verification failure

## Related Documentation

- [Block Allocation Architecture](../architecture/block_allocation.md) — allocator and bitmap design
- [Inode & Directory Architecture](../architecture/inode_and_directory.md) — BlockPath, ChunkEntry, directory encoding
- [Testing & Verification Overview](../operations/testing_and_verification.md) — full test pyramid including Shuttle and TLA+
