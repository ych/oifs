# OIFS Formal Verification Suite

This directory contains formal specifications and mathematical verification artifacts for the OIFS file system.

## 1. Scope of Formal Verification in OIFS

OIFS employs a multi-tiered formal verification and model checking methodology:

| Tier | Methodology | Verification Target | Status |
| :--- | :--- | :--- | :--- |
| **Language** | **RustBelt Type Semantics (Coq)** | Data-Race Freedom under Safe Rust and `RwLock` | Proven by Rust language type system |
| **Concurrency** | **Shuttle Bounded Model Checking (DPOR)** | Randomized thread interleavings, No Torn Reads, `OpenMode` race mutual exclusion | Active (`tests/shuttle_concurrency_test.rs`) |
| **Symbolic SMT** | **AWS Kani Rust Verifier (CBMC/CaDiCaL)** | Exhaustive symbolic verification (**135 proofs**) across extent allocation, bit isolation, inode v3 CRC32C, Auto-Grow/Vacuum resize, WriteBatch chunking, range coalescing, slice clamping, transfer/splice gates, NFSv3/FUSE protocols, and 128B cache padding | Active (`cargo kani --lib -j 8 --output-format=terse`) |
| **Protocol** | **TLA+ Specification & TLC Model Checker** | Linearizability and atomic commit of Copy-on-Write (COW) extent pointer swapping | Active (`OifsCowProtection.tla`) |

---

## 2. TLA+ Specification: `OifsCowProtection.tla`

The TLA+ model checks that under arbitrary preemptive thread interleavings between multiple Reader threads and a concurrent Writer thread updating an extent:

1. **`NoTornReadInvariant`**: Readers strictly observe either `OLD_DATA` or `NEW_DATA`, never unwritten garbage or torn blocks.
2. **`InodePointsToValidData`**: At any instant, the published Inode pointer points exclusively to valid data.
3. **`NoDanglingBlockPointer`**: Free blocks in the bitmap are never referenced by an active Inode pointer.

### Running the TLC Model Checker

To model check `OifsCowProtection.tla` using the TLC model checker:

```bash
# Using tlapm / tla2tools
java -cp tla2tools.jar tlc2.TLC OifsCowProtection.tla \
  -config OifsCowProtection.cfg \
  -deadlock \
  -workers 4
```

---

## 3. Kani SMT Proofs in Rust

OIFS maintains **135 AWS Kani harnesses** across its core engine (`superblock`, `inode`, `inode_format`, `bitmap`, `allocator`, `directory`, `disk`, `disk::resize`, `disk::batch`, `disk::caches`, `disk::transfer`, `journal`, `storage`, `encryption`, `filters`, `zstd_pool`, `io_engine`, `auto_defrag`, `ipc`, `ffi`, `fuse`, and `nfs`).

To execute the Kani symbolic execution proofs:

```bash
# Run all 131 proofs in parallel
cargo kani --lib -j 8 --output-format=terse

# Run individual harnesses
cargo kani --lib --harness proof_inode_v3_crc32c_detects_any_single_bit_flip
cargo kani --lib --harness proof_in_place_resize_bounds_sound
cargo kani --lib --harness proof_chunked_64k_offset_and_slicing_soundness
cargo kani --lib --harness proof_chunk_entry_roundtrip_all
```
