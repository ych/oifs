---
type: "Reference"
title: "Testing and Formal Verification"
description: "Documents OIFS's multi-tiered verification strategy including unit/integration tests (40+ test files), Shuttle DPOR concurrency testing, Bolero property-based fuzzing (16 harnesses), Kani formal verification (92 proofs across 14 modules), TLA+ protocol model checking, crash safety validation, and benchmarks."
tags: [testing, verification, kani, shuttle, bolero, tla+, crash-safety, benchmarks]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-23T20:55:55.458Z
sources:
  - id: openwiki-source-69dc9c7ca45b38a30db2f06f
    resource: repo://src/bitmap.rs
  - id: openwiki-source-388414179b3b4a07da1a42a4
    resource: repo://src/filters.rs
  - id: openwiki-source-ea9e30b0c99ad48bf309d4ab
    resource: repo://src/io_engine.rs
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
  - id: openwiki-source-4dd40261c9252cfa88de26ff
    resource: repo://tests/bolero_fuzz_test.rs
  - id: openwiki-source-ac22c6c75748af0e3789862c
    resource: repo://tests/ffi_test.rs
  - id: openwiki-source-8ab3a63a56b627706c5b2737
    resource: repo://tests/integration_test.rs
  - id: openwiki-source-5b30823597738f668b07d33c
    resource: repo://tests/shuttle_concurrency_test.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-23T20:55:55.458Z" }
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
│ 40+ test files   │       │ deadlocks        │       │ 92 Kani proofs   │
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
                        │ 16 harnesses             │
                        └──────────────────────────┘
```

1. **Empirical Test Suites (`tests/`)**: 40+ test files validate end-to-end integration, CLI operations, FFI memory safety, pluggable I/O engines, cross-platform endianness, encryption, defragmentation, fsck, MCP server integration, crash safety, network mode, FUSE, and format migration.
2. **Controlled Concurrency Exploration (Shuttle)**: Uses DPOR (Dynamic Partial Order Reduction) to systematically explore thread interleavings, exposing heisenbugs, race conditions, and deadlocks.
3. **Property-Based Fuzzing (Bolero)**: Generates arbitrary inputs to verify roundtrip invariants, panic-freedom, and semantic equivalence across parsers, codecs, and state machines.
4. **Formal Verification (AWS Kani Rust Verifier & TLA+)**: Utilizes SAT/SMT solvers (CBMC/CaDiCaL) and formal specification to mathematically prove safety invariants across all possible symbolic inputs and concurrent protocol transitions.
5. **Benchmarks**: Ad-hoc performance tests for directory scaling, journal overhead, flush latency, and implementation comparisons.

---

## Test Suite Structure

The `tests/` directory organizes ~40 test cases across specialized functional domains:

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
| `tests/fsck_extended_test.rs` | Extended FSCK | Orphan, leaked, missing, cross-linked block detection |
| `tests/defrag_test.rs` | Online Defragmentation | 3-step atomic rename, metadata preservation, rollback safety |
| `tests/defrag_seekable_chunk_test.rs` | Seekable Defrag | COW extent swap crash safety |
| `tests/cli_test.rs` | Command-Line Interface | Argument parsing, password prompts, `--json` format |
| `tests/cli_all_commands_test.rs` | Full CLI Coverage | All subcommands, edge cases |
| `tests/cli_concurrent_process_test.rs` | Multi-Process CLI | IPC mode, master election, concurrent puts/gets |
| `tests/cli_extended_test.rs` | Extended CLI | Additional flags, error paths |
| `tests/cli_mkdir_test.rs` | mkdir CLI | Directory creation via CLI |
| `tests/cli_recursive_test.rs` | Recursive Operations | Recursive delete, copy |
| `tests/cli_rm_test.rs` | rm CLI | File/directory removal |
| `tests/ffi_test.rs` | C Shared Library ABI | Handle lifecycle, callback directory streaming, backend switching |
| `tests/ffi_c_cpp_test.rs` | C/C++ Compiler Integration | Strict `-Wall -Wextra -Wpedantic -Werror`, C11 & C++17 |
| `tests/ffi_delete_test.rs` | FFI Delete | Delete file via FFI |
| `tests/ffi_extended_test.rs` | Extended FFI | Additional FFI APIs |
| `tests/ffi_robustness_test.rs` | FFI Robustness | Error handling, invalid handles |
| `tests/ffi_version_test.rs` | FFI Version | Version handshake |
| `tests/mcp_server_test.rs` | MCP Tool Integration | JSON-RPC tool schemas, stdio interaction, agent sandboxing |
| `tests/shuttle_concurrency_test.rs` | Randomized Threading (DPOR) | 8 controlled interleaving tests exploring data-race permutations |
| `tests/bolero_fuzz_test.rs` | Property / Fuzz Harnesses | 16 harnesses: roundtrip invariants, panic-freedom, model equivalence |
| `tests/dir_bench.rs` | Directory Scaling Benchmarks | Create, warm/cold lookup, negative lookup, cache speedup |
| `tests/journal_bench.rs` | Journal Overhead Benchmarks | Journaled vs legacy, durability mode cost, checkpoint cost |
| `tests/flush_perf_test.rs` | Flush Latency Benchmark | Exclusive vs shared lock read stall measurement |
| `tests/perf_comparison.rs` | Implementation Comparisons | Old vs new bitmap lookup, directory scan, filter encoding |
| `tests/crash_safety_test.rs` | Power-Loss Simulation | Explicit/implicit flush, drop semantics, block overflow |
| `tests/durability_test.rs` | Durability Modes | Lazy/RangeAsync/Strict mode persistence |
| `tests/journal_test.rs` | WAL & Recovery | Journal recovery, defrag crash, truncate WAL-first |
| `tests/concurrency_test.rs` | Basic Concurrency | Reader/writer patterns |
| `tests/concurrency_stress_test.rs` | High-Contention Stress | Multi-thread read/write, live fsck, indirect block expansion |
| `tests/session_and_cache_test.rs` | Session/Cache | Session registry, sharded caches |
| `tests/session_edge_cases_test.rs` | Session Edge Cases | Reopen, fallback, timeout |
| `tests/session_ipc_test.rs` | IPC Session | Multi-process, network, failover, encryption |
| `tests/ipc_frame_test.rs` | IPC Framing | Frame encode/decode, length validation |
| `tests/ipc_security_test.rs` | IPC Security | Authentication, challenge-response |
| `tests/signal_storm_test.rs` | Signal Handling | Signal delivery under load |
| `tests/transfer_test.rs` | Data Transfer | Network sync, single-file |
| `tests/network_single_file_sync_test.rs` | Network Mode | TCP multi-node, master election, ping/pong |
| `tests/backward_compatibility_test.rs` | Format v1 Compatibility | V1 image open, read, write |
| `tests/format_migration_test.rs` | v1→v2 Migration | In-place downgrade, detection, fsck |
| `tests/large_image_bitmap_test.rs` | Multi-Block Bitmaps | >128 MiB images, v3 layout |
| `tests/large_file_limit_test.rs` | Large File Limits | Boundary conditions |
| `tests/zeroed_block_reuse_test.rs` | Zeroed Block Reuse | Allocator returns zeroed blocks |
| `tests/zstd_context_pool_test.rs` | Zstd Context Pool | Thread-local context reuse |
| `tests/compression_modes_test.rs` | Compression Modes | LZ4, Zstd, Never modes |
| `tests/encrypted_context_binding_test.rs` | Encryption Context | AAD domain separation |
| `tests/encrypted_partial_write_test.rs` | Partial Encrypted Writes | Sub-block encryption |
| `tests/filter_recommendation_and_inode_cache_test.rs` | Filter Recommendation | Auto filter selection, inode cache |
| `tests/cache_coherence_and_simd_filter_test.rs` | Cache Coherence | Sharded cache, SIMD filters |
| `tests/batch_read_sharded_dir_and_durability_test.rs` | Batch Reads | Sharded dir, durability |
| `tests/block_pointer_bounds_test.rs` | Block Pointer Bounds | OOB detection |
| `tests/contiguous_and_truncate_test.rs` | Contiguous Alloc + Truncate | Contiguous extent, truncate |
| `tests/get_or_open_test.rs` | Get or Open | Session deduplication |
| `tests/indirect_blocks_test.rs` | Indirect Blocks | Single/double/triple indirect |
| `tests/inode_table_bounds_test.rs` | Inode Table Bounds | Inode table limits |
| `tests/multi_block_directory_test.rs` | Multi-Block Directories | Directory spanning blocks |
| `tests/nfs_test.rs` / `tests/nfs_live_mount_test.rs` | NFS Integration | NFS mount, operations |
| `tests/fuse_test.rs` / `tests/fuse_live_mount_test.rs` | FUSE Integration | FUSE mount, operations |
| `tests/read_only_mode_test.rs` | Read-Only Mode | Read-only mount enforcement |
| `tests/refactoring_and_perf_test.rs` | Refactoring Validation | Performance regressions |
| `tests/rename_test.rs` | Rename | Atomic rename, cross-dir |
| `tests/async_and_ffi_test.rs` | Async + FFI | Async operations via FFI |
| `tests/cpp_api_test.rs` | C++ API | Extended C++ bindings |

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

### Harness Inventory (16 Harnesses)

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

OIFS incorporates **92 formal proofs** using the **AWS Kani Rust Verifier (CBMC/CaDiCaL SMT solver)**. These proofs are embedded as `#[kani::proof]` functions in `#[cfg(kani)]` modules alongside the production code across **14 modules**. They mathematically verify properties across the **entire symbolic input space** — not just concrete test cases.

### Proof Distribution by Module

| Module | Proof Count | Key Properties Verified |
| :--- | :--- | :--- |
| `src/filters.rs` | 16 | Delta bijectivity (128-bit), Shuffle/BitShuffle inverses, composite pipeline invariance, transpose involutions, all typesizes |
| `src/inode.rs` | 9 | `BlockPath` roundtrip all 262,144 indices, tier boundaries, inode zero-initialization, no dangling pointers, `ChunkEntry` roundtrip all $2^{64}$ states, state predicates, field isolation |
| `src/superblock.rs` | 8 | Layout ordering non-overlap, journaled non-overlap, inode capacity, root inode fixed, minimum data blocks, validation acceptance |
| `src/journal.rs` | 21 | Ring cursor invariants, checkpoint empties ring, CRC32C detects single-bit flips, frame length overflow rejection, `WriteBlockSlice` bounds, ring `used()` invariants, append bounds, `checkpoint_to` monotonicity, all `MetadataOp` idempotence, coalescing equivalence, state encode/decode bijection, `MetadataOp` encode/decode roundtrip, `push_data_bitmap_op` coalescing, `push_block_slice_op` coalescing, ring append state transitions |
| `src/bitmap.rs` | 6 | Set-clear idempotency, bit isolation, `find_first_free` correctness, OOB returns false, contiguous free soundness, wrap-around allocator scan soundness |
| `src/directory.rs` | 9 | Directory entry find/insert/remove invariants, hash consistency, serialization roundtrips, bounds, hash filter, SipHash panic-free |
| `src/disk.rs` | 11 | Durability mode conversion, checked arithmetic prevents wrap to block 0, inode byte range bounds, write-from-start size invariant, slice clamping soundness, **64KB chunked compression** slicing conservation, sharded dir cache Fibonacci hashing, indirect hole skip conservation, mmap extent coalescing, incremental compressed size accounting |
| `src/allocator.rs` | 5 | Allocation bounds, allocate-free roundtrip, double allocation uniqueness, exhaustion, OOB free safety |
| `src/encryption.rs` | 1 | AAD domain separation, filename AAD injectivity |
| `src/ffi.rs` | 1 | FFI version check policy soundness |
| `src/auto_defrag.rs` | 1 | Config sanitization (no NaN, minimum interval) |
| `src/io_engine.rs` | 2 | Extent push preserves coverage, `IoBackend::from_u8` totality |
| `src/ipc.rs` | 2 | Frame length validation soundness, zero-length frame liveness |
| `src/storage.rs` | 1 | Storage abstraction invariants |

### Notable Proof Categories

#### 1. Filter Bijectivity and Involution Proofs (`src/filters.rs`)
- **Delta Filter Bijectivity** (`proof_delta_roundtrip_u32`): Proves `delta_decode(delta_encode(x, 4), 4) = x` for all $2^{128}$ possible 16-byte arrays.
- **Shuffle & BitShuffle Inverses** (`proof_shuffle_roundtrip`, `proof_bitshuffle_roundtrip`): Matrix and byte transposition reconstruct exact original bit layouts.
- **Composite Pipeline Invariance** (`proof_full_pipeline_roundtrip`): Chained multistage filters (`Delta → Shuffle → BitShuffle`) unapply cleanly in reverse.
- **Transpose Involutions** (`proof_transpose_8x8_u64_involution`): 8×8 matrix transpose is an exact involution.

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
- `proof_journaled_layout_non_overlapping`: Journal region never overlaps metadata or data areas.
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

## Crash Safety Testing

OIFS validates crash safety through power-loss simulation, journal recovery verification, and fsck validation — documented in detail at [Crash Safety Testing](../testing/crash_safety_testing.md).

### Key Test Areas

| Test File | Crash Safety Focus |
| :--- | :--- |
| `tests/crash_safety_test.rs` | Explicit flush, implicit drop flush, block overflow safety |
| `tests/durability_test.rs` | Lazy/RangeAsync/Strict mode persistence guarantees |
| `tests/journal_test.rs` | WAL recovery, torn transaction discard, defrag crash midway (pre-commit/post-commit), truncate WAL-first ordering |
| `tests/fsck_test.rs` / `tests/fsck_extended_test.rs` | Orphan inode, leaked block, missing block, cross-linked block detection |
| `tests/defrag_test.rs` / `tests/defrag_seekable_chunk_test.rs` | Defrag crash simulation, fsck after crash |

The tests simulate crashes by **directly manipulating on-disk image bytes** between well-defined phases (pre-commit, post-commit, post-apply) and then mounting the resulting image — precisely controlling the crash point at syscall/phase boundaries.

### Running Crash Safety Tests

```bash
# Core crash safety tests
cargo test --test crash_safety_test

# Journal recovery and defrag crash tests
cargo test --test journal_test

# FSCK validation tests
cargo test --test fsck_test --test fsck_extended_test

# Truncate and defragmentation crash tests
cargo test --test contiguous_and_truncate_test --test defrag_test --test defrag_seekable_chunk_test
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
| **Unit / Integration** | `cargo test` | 40+ test files: end-to-end I/O, FFI, CLI, MCP, encryption, defrag, fsck, io_uring, endianness, filters, concurrency, crash safety, network, FUSE, format migration |
| **Shuttle (DPOR Concurrency)** | `cargo test --test shuttle_concurrency_test` | 8 randomized interleaving tests: file creation races, reader-writer isolation, create-delete races, overlapping writes, compression races, nested dir resolution, sharded cache + read_at |
| **Kani (SMT Formal Proofs)** | `cargo kani` | 92 proofs across 14 modules: bitmap ops, allocator, filters (bijectivity), io_engine (extent coalescing), inode (BlockPath, ChunkEntry), superblock, journal (WAL ring, CRC, idempotence, coalescing), directory, encryption, ipc, auto_defrag, disk, storage, ffi |
| **Bolero (Property/Fuzz)** | `cargo test --test bolero_fuzz_test` | 16 harnesses: BlockPath/ChunkEntry/Superblock/Inode roundtrips, directory parsers, bitmap model equivalence, filter pipeline, IPC frames, CRC32C, journal decoders/recovery/coalescing, sparse read_at |
| **Bolero Continuous Fuzzing** | `cargo bolero test --test bolero_fuzz_test <harness>` | LibFuzzer/AFL/honggfuzz/Kani backend for unbounded input generation |
| **TLA+ (Protocol Model Checking)** | `java -cp tla2tools.jar tlc2.TLC OifsCowProtection.tla` | COW extent swap: NoTornRead, InodePointsToValidData, NoDanglingBlockPointer |
| **Benchmarks** | `cargo test --release --test <bench> -- --ignored --nocapture` | Directory scaling, journal overhead, flush latency, implementation comparisons |

---

## Notes on ThreadSanitizer

The repository does not define a `tsan` Cargo feature. Data race detection can be performed using the standard Rust ThreadSanitizer support:

```bash
# Requires nightly toolchain and clang with TSAN support
RUSTFLAGS="-Z sanitizer=thread" cargo +nightly test --target x86_64-unknown-linux-gnu -Z build-std --features io_uring
```

Key areas for TSAN analysis:
- `src/ipc.rs` — Connection pools, failover synchronization, frame handling
- `src/disk/caches.rs` — 32-shard `DirCache` and `BoundedInodeCache` with Fibonacci hashing
- `src/session.rs` — Session registry, transparent failover, `dm_fallback` mutex
- `src/auto_defrag.rs` — Background worker vs writer races, config mutex

The existing Shuttle and Kani concurrency proofs provide stronger guarantees than dynamic TSAN analysis for the verified code paths. See [TSAN Testing](../testing/tsan_testing.md) for detailed TSAN usage and known suppression patterns.
