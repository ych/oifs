---
type: concept
title: Kani Formal Verification
description: AWS Kani Rust Verifier is used to mathematically prove safety and correctness properties across the entire input space for core OIFS components, covering 49 formal proofs of bijectivity, arithmetic safety, layout validity, and filter roundtrip fidelity.
tags: [formal-verification, kani, safety-properties, bijectivity, smt-solving, model-checking]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-04T10:12:53.730Z
sources:
  - id: openwiki-source-388414179b3b4a07da1a42a4
    resource: repo://src/filters.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-04T10:12:53.730Z" }
---

# Kani Formal Verification

OIFS integrates the **AWS Kani Rust Verifier** to provide machine-checked mathematical proofs of critical safety and correctness properties. Unlike empirical testing that checks specific concrete inputs, Kani uses symbolic execution and SAT/SMT solving to verify properties hold for **all possible inputs** within defined bounds.

Kani proofs in the filters.rs module verify the roundtrip correctness of compression filters, ensuring that encoding followed by decoding returns the original input for all possible byte sequences.

## Verification Scope

Kani proofs are configured via `#[cfg(kani)]` and reside alongside implementation code in dedicated `kani_proofs` modules. The verification strategy focuses on:

1. **Bijectivity and Roundtrip Properties**: Ensuring encoding/decoding, serialization/deserialization, and mapping functions are perfect inverses
2. **Arithmetic Safety**: Proving absence of overflow, underflow, and division by zero
3. **Memory Safety**: Verifying bounds checks and absence of panics
4. **Layout Correctness**: Confirming on-disk structures maintain required ordering and validity constraints
5. **State Machine Invariants**: Validating transitions and consistency conditions

## Proof Categories

### Filter Pipeline Correctness (`filters.rs`)
The filter subsystem includes proofs for:
- Delta encoding/decoding roundtrip for u16, u32, u64 elements
- Byte shuffle and bit shuffle involution properties (T(T(x)) = x)
- Full pipeline roundtrip fidelity (Delta → Shuffle → unshuffle → undelta)
- Handling of edge cases: wrapping arithmetic, zero typesize, unaligned tails
- Transpose operations as strict mathematical involutions

### Inode Pointer Validity (`inode.rs`)
Proofs cover the logical-to-physical block mapping:
- Complete coverage of all 262,144 logical block indices
- Exact invertibility: no two logical blocks map to same physical slot
- Tier boundary validation (direct/single/double/triple indirect transitions)
- Zero-initialization of new inodes and absence of dangling pointers

### Superblock Layout (`superblock.rs`)
Layout ordering proofs ensure:
- Correct block allocation sequencing: superblock < inode bitmap < data bitmap < inode table < data area
- Root inode fixed at block 0
- Minimum data block availability (>=1 data block)
- Proper inode count scaling for small vs large filesystems

### Concurrency Safety (`io_engine.rs`)
I/O operation proofs validate:
- Extent coalescing correctness under arbitrary lengths and positions
- Read/write operation commutativity and independence
- Bounds checking for all scatter/gather operations
- Panic-free execution across symbolic input ranges

### Low-Level Primitives
Additional proofs cover:
- Bitmap allocation/deallocation invariants (`bitmap.rs`)
- Directory hash table soundness and collision handling (`directory.rs`)
- Disk memory mapping safety and offset calculations (`disk.rs`)
- FFI version checking and ABI compatibility (`ffi.rs`)
- Allocator bump pointer safety and reset properties (`allocator.rs`)

### Seekable 64K Chunked Compression (`disk.rs`, `inode.rs`)
Proofs validate:
- Invertibility and lossless roundtrip of 64-bit `ChunkEntry` bitfield serialization (`proof_chunk_entry_roundtrip_all`)
- Slicing conservation and boundary invariants across arbitrary symbolic write offsets and lengths (`proof_chunked_64k_offset_and_slicing_soundness`)

## Running Kani Proofs

To execute the formal verification suite:

```bash
cargo kani
```

Kani harnesses the CBMC model checker and CaDiCaL SAT solver to discharge proof obligations. The `#[kani::unwind]` annotations specify loop bounds for symbolic execution, while `kani::assume()` constrains input ranges to maintain tractability without sacrificing generality.

## Proof Coverage

As documented in the testing strategy, OIFS maintains **50 formal proofs** spanning core data structures and algorithms. These proofs provide mathematical guarantees that complement empirical testing by verifying properties hold across the entire input space, not just exercised test cases.

The verification focuses on safety-critical components where bugs would compromise data durability or correctness, particularly:
- Data transformation pipelines (filters)
- Metadata layout and allocation (superblock, inode, bitmap)
- Low-level I/O and memory operations
- Concurrency control primitives
- Chunked compression slicing bounds and layout invariants

Together with Shuttle concurrency testing and traditional unit/integration tests, Kani proofs form a layered verification strategy aimed at eliminating entire classes of bugs before they can manifest in practice.
