---
type: operations
title: Testing and Formal Verification
description: Detailed overview of the OIFS multi-tier quality assurance strategy, spanning unit and integration test suites, Shuttle randomized concurrency exploration, and mathematical formal verification using the AWS Kani Rust Verifier.
tags: [testing, verification, kani, shuttle, concurrency, bijectivity, formal-proofs, model-checking]
sources:
  - id: openwiki-source-69dc9c7ca45b38a30db2f06f
    resource: repo://src/bitmap.rs
  - id: openwiki-source-388414179b3b4a07da1a42a4
    resource: repo://src/filters.rs
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
  - id: openwiki-source-ac22c6c75748af0e3789862c
    resource: repo://tests/ffi_test.rs
  - id: openwiki-source-8ab3a63a56b627706c5b2737
    resource: repo://tests/integration_test.rs
  - id: openwiki-source-5b30823597738f668b07d33c
    resource: repo://tests/shuttle_concurrency_test.rs
generated: { by: "pi", at: "2026-09-29T16:14:34.721Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-03T08:18:49.684Z
---

## Overview

OIFS employs a multi-tiered verification strategy designed to ensure absolute data durability, thread safety, and algorithmic correctness:

```
┌────────────────────────────────────────────────────────────────────────┐
│                        Quality Assurance Strategy                      │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │
         ┌──────────────────────────┼──────────────────────────┐
         ▼                          ▼                          ▼
┌──────────────────┐       ┌──────────────────┐       ┌──────────────────┐
│  Empirical Tests │       │  Shuttle Testing │       │   Kani Proofs    │
│  (Unit / Integ)  │       │ (Randomized Pre) │       │(Formal SMT/CBMC) │
├──────────────────┤       ├──────────────────┤       ├──────────────────┤
│ Concrete values  │       │ Permutes thread  │       │ Mathematical     │
│ End-to-end I/O   │       │ interleavings    │       │ proofs for ALL   │
│ FFI, CLI, MCP    │       │ Catches races &  │       │ symbolic inputs  │
│ 80+ test cases   │       │ deadlocks        │       │ 25 formal proofs │
└──────────────────┘       └──────────────────┘       └──────────────────┘
```

1. **Empirical Test Suites (`tests/`)**: Validate end-to-end integration, CLI operations, FFI memory safety, and cross-platform endianness.
2. **Controlled Concurrency Exploration (Shuttle)**: Randomizes thread scheduling to expose elusive heisenbugs, race conditions, and deadlocks.
3. **Formal Verification (Kani Rust Verifier)**: Utilizes SAT/SMT solvers to mathematically prove safety invariants (bijectivity, arithmetic overflow freedom, pointer validity).

## Test suite structure

The `tests/` directory organizes test cases across specialized functional domains:

| Test File | Verification Scope | Key Invariants Checked |
| :--- | :--- | :--- |
| `tests/integration_test.rs` | Core File & Dir Operations | Inode creation, direct/indirect writes, truncation, reads |
| `tests/read_at_test.rs` | Zero-Copy Random Slices | Direct mmap slice copies, EOF bounds, encrypted/compressed reads |
| `tests/rwlock_concurrency_test.rs`| Concurrent Read Scaling | 16 parallel reader threads without lock contention |
| `tests/endianness_and_portability_test.rs`| Endian Invariance | Involution $T(T(x)) == x$, Big/Little-Endian integer parity |
| `tests/filter_test.rs` | Pre-Compression Filters | Roundtrip fidelity for Delta, ByteShuffle, BitShuffle |
| `tests/encryption_test.rs` | Cryptography Subsystem | XChaCha20-Poly1305 AEAD integrity, SIV filename encryption |
| `tests/fsck_test.rs` | Consistency Scanner | Detection of orphan inodes, leaked blocks, and cross-links |
| `tests/defrag_test.rs` | Online Defragmentation | 3-step atomic rename, metadata preservation, rollback safety |
| `tests/cli_test.rs` | Command-Line Interface | Argument parsing, password prompts, `--json` format |
| `tests/ffi_test.rs` | C Shared Library ABI | Handle lifecycle, callback directory streaming, null checks |
| `tests/mcp_server_test.rs` | MCP Tool Integration | JSON-RPC tool schemas, stdio interaction, agent sandboxing |
| `tests/shuttle_concurrency_test.rs`| Randomized Threading | Controlled interleavings exploring data-race permutations |

Running the full suite:
```bash
cargo test
```

## Controlled concurrency testing with Shuttle

Standard multi-threaded tests rely on the host OS scheduler, which exhibits nondeterministic timing and rarely triggers low-probability race conditions. OIFS integrates **Shuttle** ([`tests/shuttle_concurrency_test.rs`](tests/shuttle_concurrency_test.rs)), a tool for randomized concurrency testing.

Shuttle intercepts thread creation, synchronization primitives (`Mutex`, `RwLock`), and context switches, systematically injecting random scheduling points (`shuttle::check_random(..., iterations)`):

### 1. Concurrent file creation (`test_shuttle_concurrent_file_creation`)
- Multiple concurrent worker threads simultaneously allocate inodes, format filenames, and write small payloads into the same parent directory (`tests/shuttle_concurrency_test.rs#L15-L53`).
- Verifies that directory block appends (`DirectoryEntry`) and block allocation hints do not race or drop entries under hostile thread interleavings.

### 2. Concurrent reader-writer isolation (`test_shuttle_concurrent_reader_writer`)
- Explores interleaved execution between a writer mutating a file across two consecutive write calls (`0xAA` followed by `0xBB`) and an active reader thread (`tests/shuttle_concurrency_test.rs#L58-L106`).
- Proves that the reader **never observes torn writes or corrupted state**: reads strictly see either 0 bytes, 512 bytes (`0xAA`), or the final 1024 bytes (`0xAA` + `0xBB`).

### 3. Concurrent create and delete race (`test_shuttle_concurrent_create_delete`)
- One thread continuously creates files while another concurrently unlinks them (`tests/shuttle_concurrency_test.rs#L111-L160`).
- Validates that simultaneous block frees and inode reallocations maintain mutual exclusion without deadlock.

## Formal verification with AWS Kani

While empirical testing verifies specific concrete inputs, formal verification mathematically proves properties across the **entire input space**. OIFS incorporates 25 formal proofs using the **AWS Kani Rust Verifier (CBMC/CaDiCaL)**:

### 1. Filter bijectivity and involution proofs (`src/filters.rs`)

Pre-compression filters must be strictly bijective: any payload transformed by an encoding pipeline must be bit-for-bit restored by its inverse decoder without loss:

- **Delta Filter Bijectivity** ([`proof_delta_roundtrip_u32`](src/filters.rs#L844-L855)):
  ```rust
  #[kani::proof]
  fn proof_delta_roundtrip_u32() {
      let mut data = [0u8; 16];
      for i in 0..16 { data[i] = kani::any(); }
      let encoded = delta_encode(&data, 4);
      let decoded = delta_decode(&encoded, 4);
      assert_eq!(data, decoded.as_slice());
  }
  ```
  Proves that for *all* $2^{128}$ possible 16-byte arrays, `delta_decode(delta_encode(x, 4), 4) == x`.
- **Shuffle & BitShuffle Inverses** ([`proof_shuffle_roundtrip`](src/filters.rs#L857-L868), [`proof_bitshuffle_roundtrip`](src/filters.rs#L954-L964)):
  Proves that matrix and byte transposition algorithms reconstruct exact original bit layouts across arbitrary symbolic arrays.
- **Composite Pipeline Invariance** ([`proof_full_pipeline_roundtrip`](src/filters.rs#L883-L895)):
  Proves that chained multistage filters (`Delta -> Shuffle -> BitShuffle`) unapply cleanly in reverse.

### 2. Arithmetic overflow safety (`src/filters.rs`)

Delta filters compute differences using modular subtraction:
- [`proof_delta_wrapping_extremes`](src/filters.rs#L896-L916):
  Symbolically verifies that boundary extremes (e.g. `0.wrapping_sub(255) = 1` and `255.wrapping_add(1) = 0`) never trigger integer overflow panics in release or debug modes, guaranteeing deterministic two's-complement wrapping.

### 3. Allocation correctness and bit isolation (`src/bitmap.rs`)

- [`proof_set_clear_roundtrip`](src/bitmap.rs#L266-L281):
  Proves that `bitmap.set(i)` followed by `bitmap.clear(i)` strictly restores the original bit state.
- [`proof_set_isolation`](src/bitmap.rs#L284-L304):
  Proves that setting bit $i$ modifies *only* bit $i$, leaving all other bits in the word strictly unchanged.
- [`proof_find_first_free_correctness`](src/bitmap.rs#L307-L326):
  Proves that `find_first_free` is guaranteed to return an index whose bit is physically unset (`0`).
- [`proof_get_oob_returns_false`](src/bitmap.rs#L330-L335):
  Proves that accessing an out-of-bounds bit index safely returns `false` without panicking or triggering undefined memory reads.

### 4. Superblock layout ordering (`src/superblock.rs`)

- [`proof_superblock_layout_ordering`](src/superblock.rs#L122-L149):
  Symbolically assumes an arbitrary total block count between 5 and 1,000,000 blocks and proves:
  1. No metadata regions overlap:
     $$\text{superblock}(0) < \text{inode\_bitmap}(1) < \text{data\_bitmap}(2) < \text{inode\_table}(3) \le \text{data\_block\_start} \le \text{block\_count}$$
  2. Guarantees that at least one data block is always available ($\text{block\_count} - \text{data\_block\_start} \ge 1$).

### 5. Inode memory initialization (`src/inode.rs`)

- [`proof_inode_no_dangling_blocks`](src/inode.rs#L138-L149):
  Proves that initializing an inode with `Inode::new` guarantees all direct, single indirect, double indirect, and triple indirect pointers evaluate strictly to `0`, ensuring newly allocated files never inherit dangling physical blocks.

## Running verification tools

### Running empirical and Shuttle tests

```bash
# Standard test suite
cargo test

# Run Shuttle concurrency tests with multiple iterations
cargo test --test shuttle_concurrency_test -- --nocapture
```

### Running Kani verification proofs

To execute the mathematical model checker across all formal proofs:

```bash
cargo kani
```
