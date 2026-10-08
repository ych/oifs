---
type: operations
title: Testing and Formal Verification
description: Detailed overview of the OIFS multi-tier quality assurance strategy, spanning unit and integration test suites, Shuttle randomized concurrency exploration, mathematical formal verification using the AWS Kani Rust Verifier, and TLA+ protocol specifications.
tags: [testing, verification, kani, shuttle, concurrency, bijectivity, formal-proofs, model-checking, io_engine, tla]
sources:
  - id: openwiki-source-69dc9c7ca45b38a30db2f06f
    resource: repo://src/bitmap.rs
  - id: openwiki-source-388414179b3b4a07da1a42a4
    resource: repo://src/filters.rs
  - id: openwiki-source-ea9e30b0c99ad48bf309d4ab
    resource: repo://src/io_engine.rs
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-bc305a37042018e1ebd6d860
    resource: repo://src/inode.rs
  - id: openwiki-source-5b30823597738f668b07d33c
    resource: repo://tests/shuttle_concurrency_test.rs
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
│  Empirical Tests │       │  Shuttle Testing │       │   Formal Proofs  │
│  (Unit / Integ)  │       │ (Randomized Pre) │       │   (Kani / TLA+)  │
├──────────────────┤       ├──────────────────┤       ├──────────────────┤
│ Concrete values  │       │ Permutes thread  │       │ Mathematical     │
│ End-to-end I/O   │       │ interleavings    │       │ proofs for ALL   │
│ FFI, CLI, MCP    │       │ Catches races &  │       │ symbolic inputs  │
│ 90+ test cases   │       │ deadlocks        │       │ 50 Kani proofs   │
└──────────────────┘       └──────────────────┘       └──────────────────┘
```

1. **Empirical Test Suites (`tests/`)**: Validate end-to-end integration, CLI operations, FFI memory safety, pluggable I/O engines, and cross-platform endianness.
2. **Controlled Concurrency Exploration (Shuttle)**: Randomizes thread scheduling to expose elusive heisenbugs, race conditions, and deadlocks.
3. **Formal Verification (Kani Rust Verifier & TLA+)**: Utilizes SAT/SMT solvers and formal specification to mathematically prove safety invariants across all possible symbolic inputs and concurrent protocol transitions.

## Test Suite Structure

The `tests/` directory organizes test cases across specialized functional domains:

| Test File | Verification Scope | Key Invariants Checked |
| :--- | :--- | :--- |
| `tests/integration_test.rs` | Core File & Dir Operations | Inode creation, direct/indirect writes, truncation, reads |
| `tests/read_at_test.rs` | Zero-Copy Random Slices | Direct mmap slice copies, EOF bounds, encrypted/compressed reads |
| `tests/seekable_chunked_concurrency_test.rs` | Seekable 64K Multithreading | 16-thread sliced reads, parallel partitioned chunk writes, COW extent swaps |
| `tests/concurrent_same_file_test.rs` | POSIX OpenMode Concurrency | 16-thread simultaneous `create_file` (exactly 1 wins, 15 get `EEXIST`) |
| `tests/io_engine_test.rs` | Pluggable I/O Engine (P3.2) | Backend selection (Mmap, Pread, io_uring), batched reads, extent coalescing |
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

Standard multi-threaded tests rely on the host OS scheduler, which exhibits nondeterministic timing and rarely triggers low-probability race conditions. OIFS integrates **Shuttle** (`tests/shuttle_concurrency_test.rs`), systematically injecting random scheduling points (`shuttle::check_random(..., iterations)`):

### 1. Concurrent File Creation (`test_shuttle_concurrent_file_creation`)
- Multiple concurrent worker threads simultaneously allocate inodes, format filenames, and write small payloads into the same parent directory.
- Verifies that directory block appends (`DirectoryEntry`) and block allocation hints do not race or drop entries under hostile thread interleavings.

### 2. Concurrent Reader-Writer Isolation (`test_shuttle_concurrent_reader_writer`)
- Explores interleaved execution between a writer mutating a file across two consecutive write calls (`0xAA` followed by `0xBB`) and an active reader thread.
- Proves that the reader **never observes torn writes or corrupted state**: reads strictly see either 0 bytes, 512 bytes (`0xAA`), or the final 1024 bytes (`0xAA` + `0xBB`).

### 3. Concurrent Create and Delete Race (`test_shuttle_concurrent_create_delete`)
- One thread continuously creates files while another concurrently unlinks them.
- Validates that simultaneous block frees and inode reallocations maintain mutual exclusion without deadlock.

### 4. Concurrent Same-Filename Creation Race (`test_shuttle_concurrent_same_filename_creation`)
- Multiple threads attempt to create the identical filename concurrently under randomized scheduling.
- Validates that directory double-checked locking guarantees mutual exclusion: exactly one thread succeeds, while other threads cleanly receive `ErrorKind::AlreadyExists` (`EEXIST`), leaving `fsck` 100% clean.

## Formal Verification with AWS Kani

While empirical testing verifies specific concrete inputs, formal verification mathematically proves properties across the **entire input space**. OIFS incorporates **50 formal proofs** using the **AWS Kani Rust Verifier (CBMC/CaDiCaL)**:

### 1. Filter Bijectivity and Involution Proofs (`src/filters.rs`)
- **Delta Filter Bijectivity** (`proof_delta_roundtrip_u32`): Proves that for *all* $2^{128}$ possible 16-byte arrays, `delta_decode(delta_encode(x, 4), 4) == x`.
- **Shuffle & BitShuffle Inverses** (`proof_shuffle_roundtrip`, `proof_bitshuffle_roundtrip`): Proves that matrix and byte transposition algorithms reconstruct exact original bit layouts across arbitrary symbolic arrays.
- **Composite Pipeline Invariance** (`proof_full_pipeline_roundtrip`): Proves that chained multistage filters (`Delta -> Shuffle -> BitShuffle`) unapply cleanly in reverse.

### 2. I/O Engine and Extent Coalescing Verification (`src/io_engine.rs`)
- **Extent Coalescing Coverage Invariance** (`proof_extent_push_preserves_coverage`): Proves that `ExtentList::push` strictly preserves total byte coverage across arbitrary symbolic offsets and lengths.
- **Backend Decoding Totality** (`proof_io_backend_from_u8_soundness`): Symbolically verifies that `IoBackend::from_u8` safely handles all 256 possible `u8` values.

### 3. Allocation Correctness and Bit Isolation (`src/bitmap.rs`)
- `proof_set_clear_roundtrip`: Proves that `bitmap.set(i)` followed by `bitmap.clear(i)` strictly restores the original bit state.
- `proof_set_isolation`: Proves that setting bit $i$ modifies *only* bit $i$, leaving all other bits in the word strictly unchanged.
- `proof_find_first_free_correctness`: Proves that `find_first_free` is guaranteed to return an unset bit index.
- `proof_get_oob_returns_false`: Proves that accessing out-of-bounds bit indices returns `false` without panicking.

### 4. Superblock Layout and Durability Proofs (`src/superblock.rs`, `src/disk.rs`)
- `proof_superblock_layout_ordering`: Proves metadata regions never overlap for any total block count.
- `proof_durability_mode_from_u8_soundness`: Proves total enum decoding and consistency of range-based flushing.
- `proof_clamp_slice_range_soundness`: Proves that arbitrary symbolic file offsets and slice lengths never cause buffer overflow or range inversion.

### 5. Inode Memory Initialization and Pointer Validations (`src/inode.rs`)
- `proof_inode_no_dangling_blocks`: Proves that initializing an inode zeroes all 12 direct/indirect and triple-indirect block pointers.
- `proof_block_path_roundtrip_all_indices`: Proves bijectivity across all 262,144 logical block pointer paths.

### 6. Seekable 64K Chunked Compression Invariants (`src/disk.rs`, `src/inode.rs`)
- `proof_chunked_64k_offset_and_slicing_soundness` (`src/disk.rs`): Proves that for arbitrary symbolic offsets and lengths, chunk index decomposition `first_chunk <= last_chunk` is strictly ordered, and spliced source slice lengths strictly equal target destination slice lengths with zero out-of-bounds memory indexing.
- `proof_chunk_entry_roundtrip_all` (`src/inode.rs`): Proves that 64-bit `ChunkEntry` bitfield serialization roundtrips all combinations of `start_block`, `block_count`, `flags`, and `compressed_len` across $2^{64}$ states without distortion.

## Protocol Verification with TLA+

OIFS specifies the atomic Copy-on-Write (COW) extent replacement protocol in TLA+ (`docs/formal_verification/OifsCowProtection.tla`). The specification models concurrent execution between arbitrary reader threads and a writer updating an extent at a file offset.

Model checking via the **TLC Model Checker** mathematically proves that:
1. **`NoTornReadInvariant`**: Readers strictly observe either `OLD_DATA` or `NEW_DATA`, never intermediate unallocated or partially written garbage.
2. **`InodePointsToValidData`**: At all times, the published Inode pointer references only allocated data blocks.
3. **`NoDanglingBlockPointer`**: Blocks marked free in the allocation bitmap are never referenced by an active Inode.

## Running Verification Tools

### Running Empirical and Shuttle Tests
```bash
# Standard test suite
cargo test

# Run Shuttle concurrency tests with multiple iterations
cargo test --test shuttle_concurrency_test -- --nocapture
```

### Running Kani Verification Proofs
```bash
cargo kani
```
