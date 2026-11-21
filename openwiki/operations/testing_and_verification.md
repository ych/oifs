---
type: "Reference"
title: "Testing and Formal Verification"
openwiki_generated: true
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
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
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---


# Testing and Formal Verification

OIFS employs a multi-tiered verification strategy designed to ensure absolute data durability, thread safety, and algorithmic correctness across the entire input space:

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        Quality Assurance Strategy                        │
└────────────────────────────────────┬────────────────────────────────────┘
                                     │
        ┌────────────────────────────┼────────────────────────────┐
        ▼                            ▼                            ▼
┌──────────────────┐       ┌──────────────────┐       ┌──────────────────┐
│  Empirical Tests │       │  Shuttle Testing │       │   Formal Proofs  │
│  (Unit / Integ)  │       │ (Randomized DPOR)│       │   (Kani / TLA+)  │
├──────────────────┤       ├──────────────────┤       ├──────────────────┤
│ Concrete values  │       │ Permutes thread  │       │ Mathematical     │
│ End-to-end I/O   │       │ interleavings    │       │ proofs for ALL   │
│ FFI, CLI, MCP    │       │ Catches races &  │       │ symbolic inputs  │
│ 15+ test files   │       │ deadlocks        │       │ ~80 Kani proofs  │
└──────────────────┘       └──────────────────┘       └──────────────────┘
        ▲                            ▲                            ▲
        │                            │                            │
        └────────────────────────────┼────────────────────────────┘
                                     ▼
                        ┌──────────────────────────┐
                        │  Property / Fuzz Tests   │
                        │        (Bolero)          │
                        ├──────────────────────────┤
                        │ Arbitrary input gen      │
                        │ Roundtrip invariants     │
                        │ 12+ harnesses            │
                        └──────────────────────────┘
```

1. **Empirical Test Suites (`tests/`)**: Validate end-to-end integration, CLI operations, FFI memory safety, pluggable I/O engines, cross-platform endianness, encryption, defragmentation, fsck, and MCP server integration.
2. **Controlled Concurrency Exploration (Shuttle)**: Uses DPOR (Dynamic Partial Order Reduction) to systematically explore thread interleavings, exposing heisenbugs, race conditions, and deadlocks.
3. **Property-Based Fuzzing (Bolero)**: Generates arbitrary inputs to verify roundtrip invariants, panic-freedom, and semantic equivalence across parsers, codecs, and state machines.
4. **Formal Verification (AWS Kani Rust Verifier & TLA+)**: Utilizes SAT/SMT solvers (CBMC/CaDiCaL) and formal specification to mathematically prove safety invariants across all possible symbolic inputs and concurrent protocol transitions.
5. **Benchmarks**: Ad-hoc performance tests for directory scaling, journal overhead, flush latency, and implementation comparisons.

---

## Test Suite Structure

The `tests/` directory organizes test cases across specialized functional domains:

| Test File | Verification Scope | Key Invariants Checked |
| :--- | :--- | :--- |
| `tests/integration_test.rs` | Core File & Dir Operations | Inode creation, direct/indirect writes, truncation, reads |
| `tests/read_at_test.rs` | Zero-Copy Random Slices | Direct mmap slice copies, EOF bounds, encrypted/compressed reads |
| `tests/seekable_chunked_concurrency_test.rs` | Seekable 64K Multithreading | 16-thread sliced reads, parallel partitioned chunk writes, COW extent swaps |
| `tests/concurrent_same_file_test.rs` | POSIX OpenMode Concurrency | 16-thread simultaneous `create_file` (exactly 1 wins, 15 get `EEXIST`) |
| `tests/io_engine_test.rs` | Pluggable I/O Engine (P3.2) | Backend selection (Mmap, Pread, io_uring), batched reads, extent coalescing |
| `tests/rwlock_concurrency_test.rs` | Concurrent Read Scaling | 16 parallel reader threads without lock contention |
| `tests/endianness_and_portability_test.rs` | Endian Invariance | Involution $T(T(x)) = x$, Big/Little-Endian integer parity |
| `tests/filter_test.rs` | Pre-Compression Filters | Roundtrip fidelity for Delta, ByteShuffle, BitShuffle |
| `tests/encryption_test.rs` | Cryptography Subsystem | XChaCha20-Poly1305 AEAD integrity, SIV filename encryption |
| `tests/fsck_test.rs` | Consistency Scanner | Detection of orphan inodes, leaked blocks, and cross-links |
| `tests/defrag_test.rs` | Online Defragmentation | 3-step atomic rename, metadata preservation, rollback safety |
| `tests/cli_test.rs` | Command-Line Interface | Argument parsing, password prompts, `--json` format |
| `tests/ffi_test.rs` | C Shared Library ABI | Handle lifecycle, callback directory streaming, backend switching |
| `tests/mcp_server_test.rs` | MCP Tool Integration | JSON-RPC tool schemas, stdio interaction, agent sandboxing |
| `tests/shuttle_concurrency_test.rs` | Randomized Threading (DPOR) | Controlled interleavings exploring data-race permutations |
| `tests/bolero_fuzz_test.rs` | Property / Fuzz Harnesses | Arbitrary input generation, roundtrip invariants, panic-freedom |
| `tests/dir_bench.rs` | Directory Scaling Benchmarks | Create, warm/cold lookup, negative lookup, cache speedup |
| `tests/journal_bench.rs` | Journal Overhead Benchmarks | Journaled vs legacy, durability mode cost, checkpoint cost |
| `tests/flush_perf_test.rs` | Flush Latency Benchmark | Exclusive vs shared lock read stall measurement |
| `tests/perf_comparison.rs` | Implementation Comparisons | Old vs new bitmap lookup, directory scan, filter encoding |

Running the full empirical suite:
```bash
cargo test
```

---

## Controlled Concurrency Testing with Shuttle

Standard multi-threaded tests rely on the host OS scheduler, which exhibits nondeterministic timing and rarely triggers low-probability race conditions. OIFS integrates **Shuttle** (`tests/shuttle_concurrency_test.rs`), systematically injecting random scheduling points via `shuttle::check_random(..., iterations)` using **DPOR (Dynamic Partial Order Reduction)** to explore distinct thread interleavings:

### Test Coverage Matrix

| Test | Threads | Scenario | Invariant Verified |
| :--- | :--- | :--- | :--- |
| `test_shuttle_concurrent_file_creation` | 3 | Concurrent inode allocation + dir entry append | No dropped entries, all files readable |
| `test_shuttle_concurrent_reader_writer` | 2 | Writer: 2× write; Reader: concurrent `read_data` | **No torn reads** — reader sees 0, 512 (0xAA), or 1024 (0xAA+0xBB) bytes |
| `test_shuttle_concurrent_create_delete` | 2 | Create vs unlink same filename | Mutual exclusion, no deadlock, `fsck` clean |
| `test_shuttle_concurrent_same_filename_creation` | 3 | Triple create identical filename | Exactly 1 succeeds, others get `EEXIST`, `fsck` clean |
| `test_shuttle_overlapping_offset_writes` | 3 | Overlapping regions [0..1024], [512..1536], [1024..2048] | Exclusive regions pure; overlaps consistent; no uninitialized bytes |
| `test_shuttle_concurrent_compression_and_decompression_race` | 2 | Compressed write vs decompress read | Decompression sees 0 or full valid payload |
| `test_shuttle_nested_directory_and_path_resolution_race` | 3 | Nested dir creation + path resolution | No panic/corruption; final resolution correct |
| `test_shuttle_sharded_dir_cache_multi_dir_and_read_at` | 3 | Sharded cache promotion + indirect `read_at` | No deadlock, no stale data, `fsck` clean |

### Running Shuttle Tests

```bash
# Run all Shuttle tests with default iterations (40-50)
cargo test --test shuttle_concurrency_test -- --nocapture

# Run a specific test with more iterations for deeper exploration
cargo test --test shuttle_concurrency_test test_shuttle_concurrent_file_creation -- --nocapture
```

**How it works**: Shuttle replaces `std::thread::spawn` with a deterministic scheduler that explores all feasible thread interleavings up to a bounded context-switch budget. Each `check_random(..., N)` invocation runs N randomized schedules, maximizing coverage of the state space.

---

## Property-Based Fuzzing with Bolero

Bolero (`tests/bolero_fuzz_test.rs`) generates **arbitrary structured inputs** using type-driven generators (`bolero::TypeGenerator`) and checks properties via `bolero::check!()`. Under plain `cargo test`, each harness runs a bounded number of generated cases; for continuous fuzzing, it integrates with libFuzzer, AFL, honggfuzz, or Kani.

### Harness Inventory

| Harness | Generator Input | Property Verified |
| :--- | :--- | :--- |
| `blockpath_roundtrip` | `usize` | `BlockPath::from_logical` ⟷ `to_logical` bijective for all indices |
| `chunk_entry_roundtrip` | `(u32, u8, u8, u16)` | `ChunkEntry` bitfield serialization roundtrips all $2^{64}$ states |
| `superblock_validate_never_panics` | `(Vec<u8>, u32)` | Deserialization + `validate()` never panics; validated superblock has coherent geometry |
| `superblock_new_layout_is_consistent` | `(u16, bool)` | `SuperBlock::new_v3` produces valid layout for all block counts |
| `inode_format_decode_never_panics_and_roundtrips` | `Vec<u8>` | `decode_v2` ⟷ `encode_v2` roundtrip; no panics on arbitrary bytes |
| `directory_block_parsers_never_panic` | `Vec<u8>` | All directory parsers (`has_valid_entry`, `DirectoryIterator`, `find_entry`, `remove_entry`) panic-free |
| `directory_names_serialization_roundtrip` | `(String, u64)` | `DirectoryEntry` serialize ⟷ deserialize roundtrip; invalid names rejected |
| `bitmap_matches_naive_model` | `(Vec<u8>, u16, u16, u16)` | `Bitmap` operations match naive bit-vector model for set/clear/find |
| `filter_pipeline_roundtrip` | `(Vec<u8>, u8, bool, bool, bool)` | Arbitrary `FilterConfig` pipeline encodes ⟷ decodes to identical bytes |
| `ipc_read_framed_never_panics` | `Vec<u8>` | IPC frame decoders (`IpcRequest`, `IpcResponse`) never panic on arbitrary bytes |
| `ipc_frame_roundtrip` | `Vec<u8>` | `write_framed` ⟷ `read_framed` roundtrip for valid frames |
| `crc32c_matches_bytewise_reference` | `Vec<u8>` | `crc32c` matches bitwise reference implementation |
| `journal_decoders_never_panic` | `Vec<u8>` | `JournalState::decode`, `decode_frame`, `MetadataOp::decode_all` panic-free |
| `journal_recover_terminates_on_arbitrary_ring` | `(Vec<u8>, u64, u64)` | Journal recovery terminates on arbitrary ring noise |
| `journal_coalesce_and_frame_preserve_semantics` | `Vec<RawOp>` | `coalesce_ops` preserves `apply_op_in_place` semantics; frame roundtrip identical |
| `read_at_batch_indirect_and_sparse_matches_reference` | `(u32, u16, bool)` | `read_at` with Mmap/Pread matches reference for sparse multi-tier files |

### Running Bolero Tests

```bash
# Run all Bolero harnesses (bounded iterations under cargo test)
cargo test --test bolero_fuzz_test -- --nocapture

# Continuous fuzzing with libFuzzer (requires nightly + bolero CLI)
cargo bolero test --test bolero_fuzz_test blockpath_roundtrip

# Continuous fuzzing with AFL
cargo bolero test --test bolero_fuzz_test --engine afl filter_pipeline_roundtrip

# Symbolic fuzzing via Kani backend
cargo bolero test --test bolero_fuzz_test --engine kani superblock_validate_never_panics
```

---

## Formal Verification with AWS Kani

OIFS incorporates **~80 formal proofs** using the **AWS Kani Rust Verifier (CBMC/CaDiCaL SMT solver)**. These proofs are embedded as `#[kani::proof]` functions in `#[cfg(kani)]` modules alongside the production code. They mathematically verify properties across the **entire symbolic input space** — not just concrete test cases.

### Proof Distribution by Module

| Module | Proof Count | Key Properties Verified |
| :--- | :--- | :--- |
| `src/bitmap.rs` | 6 | Set-clear idempotency, bit isolation, `find_first_free` correctness, OOB returns false, contiguous free soundness, wrap-around allocator scan soundness |
| `src/allocator.rs` | 5 | Allocation bounds, allocate-free roundtrip, double allocation uniqueness, exhaustion, OOB free safety |
| `src/filters.rs` | 18 | Delta bijectivity (128-bit), Shuffle/BitShuffle inverses, composite pipeline invariance, all typesizes |
| `src/io_engine.rs` | 2 | Extent coalescing coverage preservation, `IoBackend::from_u8` totality |
| `src/inode.rs` | 9 | `BlockPath` roundtrip all 262,144 indices, tier boundaries, inode zero-initialization, no dangling pointers, `ChunkEntry` roundtrip all $2^{64}$ states, state predicates, field isolation |
| `src/superblock.rs` | 8 | Layout ordering non-overlap, durability mode decoding, slice clamping soundness |
| `src/journal.rs` | 24 | Ring cursor invariants, checkpoint empties ring, CRC32C detects single-bit flips, frame length overflow rejection, `WriteBlockSlice` bounds, ring `used()` invariants, append bounds, `checkpoint_to` monotonicity, all `MetadataOp` idempotence, coalescing equivalence, state encode/decode bijection, `MetadataOp` encode/decode roundtrip, `push_data_bitmap_op` coalescing, `push_block_slice_op` coalescing, ring append state transitions |
| `src/directory.rs` | 9 | Directory entry find/insert/remove invariants, hash consistency, serialization roundtrips |
| `src/encryption.rs` | 1 | Key derivation / AEAD properties |
| `src/ffi.rs` | 1 | FFI boundary safety |
| `src/ipc.rs` | 2 | IPC frame encode/decode roundtrip |
| `src/auto_defrag.rs` | 1 | Defragmentation safety |
| `src/disk.rs` | 12 | Chunked 64K offset/slicing soundness, extent management |

### Notable Proof Categories

#### 1. Filter Bijectivity and Involution Proofs (`src/filters.rs`)
- **Delta Filter Bijectivity** (`proof_delta_roundtrip_u32`): Proves `delta_decode(delta_encode(x, 4), 4) = x` for all $2^{128}$ possible 16-byte arrays.
- **Shuffle & BitShuffle Inverses** (`proof_shuffle_roundtrip`, `proof_bitshuffle_roundtrip`): Matrix and byte transposition reconstruct exact original bit layouts.
- **Composite Pipeline Invariance** (`proof_full_pipeline_roundtrip`): Chained multistage filters (`Delta → Shuffle → BitShuffle`) unapply cleanly in reverse.

#### 2. I/O Engine and Extent Coalescing Verification (`src/io_engine.rs`)
- **Extent Coalescing Coverage Invariance** (`proof_extent_push_preserves_coverage`): `ExtentList::push` strictly preserves total byte coverage across arbitrary symbolic offsets and lengths; merges iff both disk offset and buffer offset are contiguous.
- **Backend Decoding Totality** (`proof_io_backend_from_u8_soundness`): `IoBackend::from_u8` safely handles all 256 possible `u8` values.

#### 3. Allocation Correctness and Bit Isolation (`src/bitmap.rs`)
- `proof_set_clear_roundtrip`: `bitmap.set(i)` followed by `bitmap.clear(i)` restores original bit state.
- `proof_set_isolation`: Setting bit $i$ modifies only bit $i$, leaving all other bits unchanged.
- `proof_find_first_free_correctness`: `find_first_free` returns a genuinely free bit index; all earlier bits are set.
- `proof_get_oob_returns_false`: Out-of-bounds bit indices return `false` without panicking.
- `proof_find_contiguous_free_correctness`: Returned range contains only free bits.
- `proof_find_next_free_wrapped_soundness`: Wrap-around allocator scan never skips a free bit; returns `None` only when bitmap is full.

#### 4. Superblock Layout and Durability Proofs (`src/superblock.rs`, `src/disk.rs`)
- `proof_superblock_layout_ordering`: Metadata regions never overlap for any total block count.
- `proof_durability_mode_from_u8_soundness`: Total enum decoding and consistency of range-based flushing.
- `proof_clamp_slice_range_soundness`: Arbitrary symbolic file offsets and slice lengths never cause buffer overflow or range inversion.

#### 5. Inode Memory Initialization and Pointer Validations (`src/inode.rs`)
- `proof_inode_no_dangling_blocks`: Initializing an inode zeroes all 12 direct/indirect and triple-indirect block pointers.
- `proof_block_path_roundtrip_all_indices`: Bijectivity across all 262,144 logical block pointer paths.
- `proof_chunk_entry_roundtrip_all`: 64-bit `ChunkEntry` bitfield serialization roundtrips all combinations of `start_block`, `block_count`, `flags`, `compressed_len` across $2^{64}$ states.

#### 6. Journal WAL Ring and Metadata Operation Proofs (`src/journal.rs`)
- `proof_ring_cursor_advance_stays_in_ring`: Frame placement `(cursor + len) % ring` always yields valid in-ring offset.
- `proof_checkpoint_empties_ring`: `checkpoint` sets `tail := head`; `used()` reports zero pending bytes.
- `proof_crc32c_detects_single_bit_flip`: Any single-bit flip changes CRC32C; torn frames rejected.
- `proof_frame_length_overflow_rejected`: `decode_frame` rejects frames whose declared length exceeds available bytes.
- `proof_block_slice_offset_checked`: `WriteBlockSlice` bounds enforced before any write.
- `proof_ring_used_invariants`: `used()` calculation invariants (no overflow/underflow, `used < ring`, empty iff `head==tail`).
- `proof_ring_append_bounds_guarantee`: After wrap logic, `head + len ≤ ring_bytes`; slice indexing safe.
- `proof_ring_checkpoint_to_monotonicity`: `checkpoint_to` moves tail toward head without passing it.
- **Idempotence proofs** for all `MetadataOp` variants: `SetInodeBitmap`, `SetDataBitmap`, `WriteBlockSlice`, `WriteInode`, `SetDataBitmapRange` — re-application produces identical state.
- `proof_push_data_bitmap_op_coalesces_adjacent_same_polarity`: Adjacent same-polarity spans coalesce forward and backward.
- `proof_push_block_slice_op_coalesces_contiguous_and_contained`: Contiguous extension and contained overwrite coalesce; disjoint slices stay separate.
- `proof_ring_append_state_transition_invariants`: After `append`, `head ≠ tail`; checkpointed ring yields `new_tail = write_start`, `new_used = len` even when wrapping to 0.

### Running Kani Verification

```bash
# Run all Kani proofs (requires kani toolchain: cargo install --locked kani-verifier)
cargo kani

# Run a specific proof harness
cargo kani --harness proof_chunk_entry_roundtrip_all
cargo kani --harness proof_extent_push_preserves_coverage
cargo kani --harness proof_ring_append_state_transition_invariants

# Run with increased unwind bound for complex proofs
cargo kani --harness proof_find_next_free_wrapped_soundness --unwind 70
```

---

## Protocol Verification with TLA+

OIFS specifies the atomic **Copy-on-Write (COW) extent replacement protocol** in TLA+ (`docs/formal_verification/OifsCowProtection.tla`). The specification models concurrent execution between arbitrary reader threads and a writer updating an extent at a file offset.

Model checking via the **TLC Model Checker** mathematically proves that:

1. **`NoTornReadInvariant`**: Readers strictly observe either `OLD_DATA` or `NEW_DATA`, never intermediate unallocated or partially written garbage.
2. **`InodePointsToValidData`**: At all times, the published Inode pointer references only allocated data blocks.
3. **`NoDanglingBlockPointer`**: Blocks marked free in the allocation bitmap are never referenced by an active Inode.

### Running the TLC Model Checker

```bash
# Using tlapm / tla2tools
java -cp tla2tools.jar tlc2.TLC OifsCowProtection.tla \
  -config OifsCowProtection.cfg \
  -deadlock \
  -workers 4
```

---

## Benchmarks

OIFS includes ad-hoc benchmark tests (marked `#[ignore]`) to measure performance characteristics. Run with `--ignored` and `--nocapture` to see output.

| Benchmark | File | What It Measures |
| :--- | :--- | :--- |
| **Directory Scaling** | `tests/dir_bench.rs` | Create 10k files, warm/cold lookup, negative lookup, cache speedup (cold vs warm `list_dir`), `resolve_path`/`resolve_parent` throughput |
| **Journal Overhead** | `tests/journal_bench.rs` | Journaled vs legacy metadata throughput (create/write/delete), durability mode cost (Lazy/RangeAsync/Strict), `flush()` checkpoint cost |
| **Flush Latency** | `tests/flush_perf_test.rs` | 8 concurrent readers + 4 flushers: exclusive write lock vs shared read lock — read throughput, stalls >500µs, max latency |
| **Implementation Comparison** | `tests/perf_comparison.rs` | Old vs new: bitmap `find_first_free` (byte scan vs u64 chunks), directory lookup (Cursor deserialize vs optimized scan), insert offset scan, filter encoding (naive vs optimized) |

### Running Benchmarks

```bash
# Directory scaling benchmark
cargo test --release --test dir_bench -- --ignored --nocapture

# Journal overhead benchmark
cargo test --release --test journal_bench -- --ignored --nocapture

# Flush latency benchmark (exclusive vs shared lock)
cargo test --release --test flush_perf_test -- --ignored --nocapture

# Implementation comparison benchmark
cargo test --release --test perf_comparison -- --ignored --nocapture
```

---

## Test Type → Command → Coverage Summary

| Test Type | Command | What It Verifies |
| :--- | :--- | :--- |
| **Unit / Integration** | `cargo test` | 15+ test files: end-to-end I/O, FFI, CLI, MCP, encryption, defrag, fsck, io_uring, endianness, filters, concurrency |
| **Shuttle (DPOR Concurrency)** | `cargo test --test shuttle_concurrency_test` | 8 randomized interleaving tests: file creation races, reader-writer isolation, create-delete races, overlapping writes, compression races, nested dir resolution, sharded cache + read_at |
| **Kani (SMT Formal Proofs)** | `cargo kani` | ~80 proofs: bitmap ops, allocator, filters (bijectivity), io_engine (extent coalescing), inode (BlockPath, ChunkEntry), superblock, journal (WAL ring, CRC, idempotence, coalescing), directory, encryption, ipc, auto_defrag, disk |
| **Bolero (Property/Fuzz)** | `cargo test --test bolero_fuzz_test` | 16 harnesses: BlockPath/ChunkEntry/Superblock/Inode roundtrips, directory parsers, bitmap model equivalence, filter pipeline, IPC frames, CRC32C, journal decoders/recovery/coalescing, sparse read_at |
| **Bolero Continuous Fuzzing** | `cargo bolero test --test bolero_fuzz_test <harness>` | LibFuzzer/AFL/honggfuzz/Kani backend for unbounded input generation |
| **TLA+ (Protocol Model Checking)** | `java -cp tla2tools.jar tlc2.TLC OifsCowProtection.tla` | COW extent swap: NoTornRead, InodePointsToValidData, NoDanglingBlockPointer |
| **Benchmarks** | `cargo test --release --test <bench> -- --ignored --nocapture` | Directory scaling, journal overhead, flush latency, implementation comparisons |

---

## Notes on ThreadSanitizer

The repository does not currently define a `tsan` Cargo feature. Data race detection can be performed using the standard Rust ThreadSanitizer support:

```bash
# Requires nightly toolchain and clang with TSAN support
RUSTFLAGS="-Z sanitizer=thread" cargo test --target x86_64-unknown-linux-gnu -Z build-std --features io_uring
```

The existing Shuttle and Kani concurrency proofs provide stronger guarantees than dynamic TSAN analysis for the verified code paths.
