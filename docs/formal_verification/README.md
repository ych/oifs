# OIFS Formal Verification Suite

This directory contains formal specifications and mathematical verification artifacts for the OIFS file system.

## 1. Scope of Formal Verification in OIFS

OIFS employs a multi-tiered formal verification and model checking methodology:

| Tier | Methodology | Verification Target | Status |
| :--- | :--- | :--- | :--- |
| **Language** | **RustBelt Type Semantics (Coq)** | Data-Race Freedom under Safe Rust and `RwLock` | Proven by Rust language type system |
| **Concurrency** | **Shuttle Bounded Model Checking (DPOR)** | Randomized thread interleavings, No Torn Reads, `OpenMode` race mutual exclusion | Active (`tests/shuttle_concurrency_test.rs`) |
| **Symbolic SMT** | **AWS Kani Rust Verifier (CBMC/CaDiCaL)** | Exhaustive symbolic verification of extent allocation, bit isolation, slice clamping, and 64KB chunk slicing | Active (`src/disk.rs`, `src/inode.rs`, `src/bitmap.rs`) |
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

To execute Kani symbolic execution proofs:

```bash
cargo kani --harness proof_chunked_64k_offset_and_slicing_soundness
cargo kani --harness proof_chunk_entry_roundtrip_all
cargo kani --harness proof_clamp_slice_range_soundness
```
