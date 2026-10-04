---
type: operations
title: Testing and Formal Verification
description: Detailed overview of the OIFS multi-tier quality assurance strategy, spanning unit and integration test suites, Shuttle randomized concurrency exploration, and mathematical formal verification using the AWS Kani Rust Verifier.
tags: [testing, verification, kani, shuttle, concurrency, bijectivity, formal-proofs, model-checking, io_engine]
sources:
  - id: openwiki-source-69dc9c7ca45b38a30db2f06f
    resource: repo://src/bitmap.rs
  - id: openwiki-source-388414179b3b4a07da1a42a4
    resource: repo://src/filters.rs
  - id: openwiki-source-ea9e30b0c99ad48bf309d4ab
    resource: repo://src/io_engine.rs
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
  - id: openwiki-source-ac22c6c75748af0e3789862c
    resource: repo://tests/ffi_test.rs
  - id: openwiki-source-8ab3a63a56b627706c5b2737
    resource: repo://tests/integration_test.rs
  - id: openwiki-source-5b30823597738f668b07d33c
    resource: repo://tests/shuttle_concurrency_test.rs
generated: { by: "antigravity", at: "2026-10-04T06:56:29.774Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-04T06:56:29.774Z
---

# Testing and Formal Verification

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
│ 85+ test cases   │       │ deadlocks        │       │ 44 formal proofs │
└──────────────────┘       └──────────────────┘       └──────────────────┘
```

1. **Empirical Test Suites (`tests/`)**: Validate end-to-end integration, CLI operations, FFI memory safety, pluggable I/O engines, and cross-platform endianness.
2. **Controlled Concurrency Exploration (Shuttle)**: Randomizes thread scheduling to expose elusive heisenbugs, race conditions, and deadlocks.
3. **Formal Verification (Kani Rust Verifier)**: Utilizes SAT/SMT solvers to mathematically prove safety invariants (bijectivity, arithmetic overflow freedom, extent coalescing, and pointer validity).

## Test Suite Structure

The `tests/` directory organizes test cases across specialized functional domains:

| Test File | Verification Scope | Key Invariants Checked |
| :--- | :--- | :--- |
| `tests/integration_test.rs` | Core File & Dir Operations | Inode creation, direct/indirect writes, truncation, reads |
| `tests/read_at_test.rs` | Zero-Copy Random Slices | Direct mmap slice copies, EOF bounds, encrypted/compressed reads |
| `tests/io_engine_test.rs` | Pluggable I/O Engine (P3.2) | Backend selection (Mmap, Pread, io_uring), batched reads, extent coalescing, fallback |
| `tests/rwlock_concurrency_test.rs`| Concurrent Read Scaling | 16 parallel reader threads without lock contention |
| `tests/endianness_and_portability_test.rs`| Endian Invariance | Involution $T(T(x)) == x$, Big/Little-Endian integer parity |
| `tests/filter_test.rs` | Pre-Compression Filters | Roundtrip fidelity for Delta, ByteShuffle, BitShuffle |
| `tests/encryption_test.rs` | Cryptography Subsystem | XChaCha20-Poly1305 AEAD integrity, SIV filename encryption |
| `tests/fsck_test.rs` | Consistency Scanner | Detection of orphan inodes, leaked blocks, and cross-links |
| `tests/defrag_test.rs` | Online Defragmentation | 3-step atomic rename, metadata preservation, rollback safety |
| `tests/cli_test.rs` | Command-Line Interface | Argument parsing, password prompts, `--json` format |
| `tests/ffi_test.rs` | C Shared Library ABI | Handle lifecycle, callback directory streaming, backend switching |
| `tests/mcp_server_test.rs` | MCP Tool Integration | JSON-RPC tool schemas, stdio interaction, agent sandboxing |
| `tests/shuttle_concurrency_test.rs`| Randomized Threading | Controlled interleavings exploring data-race permutations |

Running the full suite:
```bash
cargo test
```

## Controlled Concurrency Testing with Shuttle

<!-- openwiki: broken internal link [tests/shuttle_concurrency_test.rs] file "tests/shuttle_concurrency_test.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Standard multi-threaded tests rely on the host OS scheduler, which exhibits nondeterministic timing and rarely triggers low-probability race conditions. OIFS integrates **Shuttle** ([`tests/shuttle_concurrency_test.rs`](tests/shuttle_concurrency_test.rs)), a tool for randomized concurrency testing.

Shuttle intercepts thread creation, synchronization primitives (`Mutex`, `RwLock`), and context switches, systematically injecting random scheduling points (`shuttle::check_random(..., iterations)`):

### 1. Concurrent File Creation (`test_shuttle_concurrent_file_creation`)
- Multiple concurrent worker threads simultaneously allocate inodes, format filenames, and write small payloads into the same parent directory.
- Verifies that directory block appends (`DirectoryEntry`) and block allocation hints do not race or drop entries under hostile thread interleavings.

### 2. Concurrent Reader-Writer Isolation (`test_shuttle_concurrent_reader_writer`)
- Explores interleaved execution between a writer mutating a file across two consecutive write calls (`0xAA` followed by `0xBB`) and an active reader thread.
- Proves that the reader **never observes torn writes or corrupted state**: reads strictly see either 0 bytes, 512 bytes (`0xAA`), or the final 1024 bytes (`0xAA` + `0xBB`).

### 3. Concurrent Create and Delete Race (`test_shuttle_concurrent_create_delete`)
- One thread continuously creates files while another concurrently unlinks them.
- Validates that simultaneous block frees and inode reallocations maintain mutual exclusion without deadlock.

## Formal Verification with AWS Kani

While empirical testing verifies specific concrete inputs, formal verification mathematically proves properties across the **entire input space**. OIFS incorporates 49 formal proofs using the **AWS Kani Rust Verifier (CBMC/CaDiCaL)**:

### 1. Filter Bijectivity and Involution Proofs (`src/filters.rs`)

Pre-compression filters must be strictly bijective: any payload transformed by an encoding pipeline must be bit-for-bit restored by its inverse decoder without loss:

- **Delta Filter Bijectivity** (`proof_delta_roundtrip_u32`):
  Proves that for *all* $2^{128}$ possible 16-byte arrays, `delta_decode(delta_encode(x, 4), 4) == x`.
- **Shuffle & BitShuffle Inverses** (`proof_shuffle_roundtrip`, `proof_bitshuffle_roundtrip`):
  Proves that matrix and byte transposition algorithms reconstruct exact original bit layouts across arbitrary symbolic arrays.
- **Composite Pipeline Invariance** (`proof_full_pipeline_roundtrip`):
  Proves that chained multistage filters (`Delta -> Shuffle -> BitShuffle`) unapply cleanly in reverse.

### 2. I/O Engine and Extent Coalescing Verification (`src/io_engine.rs`)

- **Extent Coalescing Coverage Invariance** (`proof_extent_push_preserves_coverage`, `src/io_engine.rs#L832-L856`):
  Proves that `ExtentList::push` strictly preserves total byte coverage across arbitrary symbolic offsets and lengths, merging two extents if and only if both the physical disk offset and destination buffer offset are contiguous.
- **Backend Decoding Totality** (`proof_io_backend_from_u8_soundness`, `src/io_engine.rs#L858-L867`):
  Symbolically verifies that `IoBackend::from_u8` safely handles all 256 possible `u8` values, correctly inverting valid discriminants and safely falling back to `IoBackend::Mmap` for unknown discriminants.

### 3. Allocation Correctness and Bit Isolation (`src/bitmap.rs`)

- `proof_set_clear_roundtrip`: Proves that `bitmap.set(i)` followed by `bitmap.clear(i)` strictly restores the original bit state.
- `proof_set_isolation`: Proves that setting bit $i$ modifies *only* bit $i$, leaving all other bits in the word strictly unchanged.
- `proof_find_first_free_correctness`: Proves that `find_first_free` is guaranteed to return an index whose bit is physically unset (`0`).
- `proof_get_oob_returns_false`: Proves that accessing an out-of-bounds bit index safely returns `false` without panicking.

### 4. Superblock Layout and Durability Proofs (`src/superblock.rs`, `src/disk.rs`)

- `proof_superblock_layout_ordering` (`src/superblock.rs#L122-L149`):
  Assumes an arbitrary total block count between 5 and 1,000,000 blocks and proves metadata regions never overlap.
- `proof_durability_mode_from_u8_soundness` and `proof_durability_mode_is_range_based_consistency` (`src/disk.rs#L2645-L2667`):
  Proves total enum decoding and verifies that `is_range_based()` evaluates to true strictly for `RangeAsync` and `Strict` modes.

### 5. Inode Memory Initialization (`src/inode.rs`)

- `proof_inode_no_dangling_blocks`: Proves that initializing an inode with `Inode::new` zeroes all direct, single indirect, double indirect, and triple indirect pointers, ensuring newly allocated files never inherit dangling physical blocks.

## Running Verification Tools

### Running Empirical and Shuttle Tests

```bash
# Standard test suite
cargo test

# Run Shuttle concurrency tests with multiple iterations
cargo test --test shuttle_concurrency_test -- --nocapture
```

### Running Kani Verification Proofs

To execute the mathematical model checker across all formal proofs:

```bash
cargo kani
```
