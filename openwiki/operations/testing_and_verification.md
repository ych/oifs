---
type: "Reference"
title: "Testing and Formal Verification"
description: "Documents OIFS's multi-tiered verification strategy including unit/integration tests (40+ test files), Shuttle DPOR concurrency testing, Bolero property-based fuzzing (16+ harnesses), Kani formal verification (131+ proofs across 19 modules), TLA+ protocol model checking, crash safety validation, and benchmarks."
tags: [testing, verification, kani, shuttle, bolero, tla+, crash-safety, benchmarks]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
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
  - id: openwiki-source-8ab3a63a56b627706c5b2737
    resource: repo://tests/integration_test.rs
  - id: openwiki-source-5b30823597738f668b07d33c
    resource: repo://tests/shuttle_concurrency_test.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
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
│ 40+ test files   │       │ deadlocks        │       │ 131+ Kani proofs │
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
                        │ 16+ harnesses            │
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
| `tests/ffi_c_cpp_test.rs` | C/C++ Compiler Integration | Strict `-Wall -Wextra -Wpedantic -Werror`, C11 & C++17/20 |
| `tests/ffi_delete_test.rs` | FFI Delete | Delete file via FFI |
| `tests/ffi_extended_test.rs` | Extended FFI | Additional FFI APIs |
| `tests/ffi_robustness_test.rs` | FFI Robustness | Null pointers, oversized buffers, use-after-close |
| `tests/ffi_version_test.rs` | FFI Version | Version handshake, library path reporting |
| `tests/cpp_api_test.rs` | C++ API | RAII wrappers, lambda callbacks, `std::span` integration |
| `tests/async_and_ffi_test.rs` | Async FFI | Thread-local error state, concurrent operations |
| `tests/mcp_server_test.rs` | MCP Server | Protocol compliance, tool invocation |
| `tests/crash_safety_test.rs` | Crash Safety | Flush persistence, drop flush, block overflow safety |
| `tests/durability_test.rs` | Durability Modes | Lazy vs Strict durability guarantees |
| `tests/bolero_fuzz_test.rs` | Property-Based Fuzzing | 16+ harnesses for parsers, codecs, state machines |
| `tests/shuttle_concurrency_test.rs` | DPOR Concurrency | Randomized thread interleaving, race/deadlock detection |
| `tests/fuse_test.rs` | FUSE Protocol | In-memory FUSE operations, handle table, readdir pagination |
| `tests/fuse_live_mount_test.rs` | Live FUSE Mount | Real kernel mount, POSIX syscalls via std::fs (ignored in CI) |
| `tests/nfs_test.rs` | NFSv3 Protocol | In-memory NFSv3 operations, XDR encoding/decoding |
| `tests/nfs_live_mount_test.rs` | Live NFS Mount | Real kernel `mount_nfs`, zero-kext on macOS (ignored in CI) |
| `tests/perf_regressions_test.rs` | Performance Regression | FUSE readdir, rename dir cache, async reads, filter recommendation |
| `tests/perf_comparison.rs` | Performance Comparison | Implementation benchmarks |
| `tests/auto_defrag_test.rs` | Auto Defragmentation | Background thread, fragmentation threshold, interval config |
| `tests/backward_compatibility_test.rs` | Format Migration | v1/v2 image read, upgrade to v3 |
| `tests/format_migration_test.rs` | Format Migration | Inode v1→v3, superblock layout evolution |
| `tests/session_and_cache_test.rs` | Session/Cache | Master/proxy, cache coherence, storage mode matrix |
| `tests/session_edge_cases_test.rs` | Session Edge Cases | Failover, reconnection, password handling |
| `tests/session_ipc_test.rs` | Session IPC | Unix domain socket, master election |
| `tests/compression_modes_test.rs` | Compression | Never/Always/Auto/Seekable modes |
| `tests/encrypted_context_binding_test.rs` | Encryption | Context-bound AEAD, nonce reuse prevention |
| `tests/encrypted_partial_write_test.rs` | Encryption | Partial write integrity |
| `tests/signal_storm_test.rs` | Signal Handling | SIGTERM/SIGINT during operations |
| `tests/transfer_test.rs` | Transfer/Splice | Zero-copy splice, extent transfer |
| `tests/zeroed_block_reuse_test.rs` | Block Reuse | Zeroed block detection and reuse |
| `tests/zstd_context_pool_test.rs` | Zstd Pool | Thread-local compression context reuse |

---

## Command-Line Test Execution

### Standard Unit & Integration Tests

```bash
# Run all tests with MCP feature (default CI configuration)
cargo test --features mcp --verbose

# Run specific test file
cargo test --test integration_test --features mcp

# Run specific test function
cargo test --test integration_test test_disk_initialization_and_persistence --features mcp

# Run with all features
cargo test --features "mcp io_uring blosc2" --verbose
```

### Bolero Property-Based Fuzzing

```bash
# Run bounded property tests (same as cargo test)
cargo test --test bolero_fuzz_test --features mcp

# Continuous fuzzing with libFuzzer (requires bolero CLI)
cargo install bolero
cargo bolero test --test bolero_fuzz_test blockpath_roundtrip

# Fuzz with specific engine
cargo bolero test --test bolero_fuzz_test --engine libfuzzer chunk_entry_roundtrip
cargo bolero test --test bolero_fuzz_test --engine afl superblock_validate_never_panics
```

### Shuttle DPOR Concurrency Testing

```bash
# Run with default 50 random schedules per test
cargo test --test shuttle_concurrency_test --features mcp

# Increase schedule count for deeper exploration
cargo test --test shuttle_concurrency_test --features mcp -- --test-threads=1
```

### Kani Formal Verification

```bash
# Run all 131+ proofs in parallel (requires kani installed)
cargo install --locked kani-verifier
cargo kani --lib -j 8 --output-format=terse

# Run specific harness
cargo kani --lib --harness proof_superblock_layout_ordering
cargo kani --lib --harness proof_inode_v3_crc32c_detects_any_single_bit_flip
cargo kani --lib --harness proof_in_place_resize_bounds_sound
cargo kani --lib --harness proof_chunked_64k_offset_and_slicing_soundness
cargo kani --lib --harness proof_chunk_entry_roundtrip_all
cargo kani --lib --harness proof_extent_push_preserves_coverage
cargo kani --lib --harness proof_io_backend_from_u8_soundness
cargo kani --lib --harness proof_cache_padded_and_shard_indices_in_bounds

# Run Kani with specific features
cargo kani --lib --features "mcp io_uring blosc2" -j 8
```

### TLA+ Model Checking

```bash
# Model check COW extent pointer swap protocol
cd docs/formal_verification
java -cp tla2tools.jar tlc2.TLC OifsCowProtection.tla \
  -config OifsCowProtection.cfg \
  -deadlock \
  -workers 4
```

### Live FUSE/NFS Mount Tests

These tests require kernel FUSE/NFS support and are marked `#[ignore]` for headless CI:

```bash
# FUSE live mount tests (requires macFUSE on macOS, fusermount3 on Linux)
cargo test --release --test fuse_live_mount_test -- --ignored --nocapture --test-threads=1

# NFS live mount tests (requires mount_nfs privileges)
cargo test --release --test nfs_live_mount_test -- --ignored --nocapture --test-threads=1
```

### C/C++ Compiler Integration Tests

```bash
# Compile and run native C11 test with strict warnings
cargo test --test ffi_c_cpp_test test_native_c_e2e --features mcp

# Compile and run native C++17 test with strict warnings
cargo test --test ffi_c_cpp_test test_native_cpp_e2e --features mcp

# Compile and run native C++20 test (if compiler supports it)
cargo test --test ffi_c_cpp_test test_native_cpp20_e2e --features mcp
```

---

## Kani Formal Verification Proofs by Module

OIFS maintains **131+ AWS Kani harnesses** across 19 modules. Each harness is defined under `#[cfg(kani)]` in the source file:

| Module | Proof Harnesses | Key Properties Verified |
| :--- | :--- | :--- |
| `src/superblock.rs` | 6 | Layout ordering, inode table capacity, bitmap coverage, journal layout |
| `src/inode.rs` | 4 | BlockPath bounds, ChunkEntry roundtrip, logical block limits |
| `src/inode_format.rs` | 5 | v2 encode/decode roundtrip, CRC32C single-bit flip detection |
| `src/bitmap.rs` | 8 | Set/clear roundtrip, bit isolation, find_first_free, find_contiguous_free, OOB safety |
| `src/allocator.rs` | 3 | Allocation bounds, bitmap consistency, free list integrity |
| `src/directory.rs` | 4 | Entry encoding/decoding, hash consistency, iteration bounds |
| `src/disk.rs` | 12 | Inode CRUD, extent management, read/write bounds, flush persistence |
| `src/disk/resize.rs` | 4 | Auto-grow bounds, vacuum soundness, in-place resize |
| `src/disk/batch.rs` | 3 | WriteBatch chunking, extent coalescing, transfer gates |
| `src/disk/caches.rs` | 2 | Cache padding (128B), shard index bounds, false sharing prevention |
| `src/disk/transfer.rs` | 5 | Splice extent preservation, transfer gate atomicity |
| `src/journal.rs` | 6 | Ring buffer framing, CRC32C, op coalescing, apply semantics |
| `src/filters.rs` | 6 | Delta/ByteShuffle/BitShuffle bijectivity, roundtrip for all typesizes |
| `src/encryption.rs` | 4 | AEAD integrity, SIV determinism, nonce uniqueness, key derivation |
| `src/zstd_pool.rs` | 3 | Thread-local pool reuse, context reset, compression levels |
| `src/io_engine.rs` | 2 | Extent coalescing coverage, IoBackend enum totality |
| `src/ipc.rs` | 3 | Frame encoding/decoding, request/response roundtrip |
| `src/storage.rs` | 2 | FileIo vs Mmap parity, durability mode semantics |
| `src/auto_defrag.rs` | 2 | Config sanitization, NaN threshold prevention |
| `src/ffi.rs` | 3 | Handle lifecycle, error string sanitization, version handshake |
| `src/fuse.rs` | 4 | Readdir pagination, handle table, flush semantics |
| `src/nfs.rs` | 3 | XDR encoding/decoding, NFSv3 procedure mapping |

Total: **~131 harnesses** across 19 modules.

---

## Shuttle DPOR Concurrency Testing Details

`tests/shuttle_concurrency_test.rs` uses Shuttle's randomized scheduler to explore thread interleavings with three primary test scenarios:

1. **Concurrent File Creation** (`test_shuttle_concurrent_file_creation`): 3 threads simultaneously create files in the same directory, verifying directory block allocation, inode allocation, and lookup consistency under randomized interleavings.

2. **Concurrent Reader/Writer** (`test_shuttle_concurrent_reader_writer`): One writer thread performs two sequential 512-byte writes while a reader thread concurrently reads. Verifies read_data never observes torn writes or corrupted state — must see 0, 512, or 1024 bytes of valid data.

3. **Concurrent Rename/Delete** (`test_shuttle_concurrent_rename_delete`): Multiple threads perform rename and delete operations on shared directory entries, verifying no lost updates or use-after-free.

Run with `cargo test --test shuttle_concurrency_test --features mcp`. Each test executes 50 random schedules by default; increase with `SHUTTLE_ITERATIONS` env var for deeper exploration.

---

## Bolero Property-Based Fuzzing Harnesses

`tests/bolero_fuzz_test.rs` defines 16+ harnesses using Bolero's `TypeGenerator` and `check!()` macro:

| Harness | Input Type | Invariant Verified |
| :--- | :--- | :--- |
| `blockpath_roundtrip` | `usize` | `BlockPath::from_logical(idx).map(|p| p.to_logical()) == Some(idx)` for `idx < MAX_LOGICAL_BLOCKS` |
| `chunk_entry_roundtrip` | `(u32, u8, u8, u16)` | `ChunkEntry::new(...)` field accessors return original values |
| `superblock_validate_never_panics` | `(Vec<u8>, u32)` | Valid superblocks have coherent geometry (inode_count > 0, ordering) |
| `superblock_new_layout_is_consistent` | `(u16, bool)` | `SuperBlock::new_v3` produces valid layout for all block counts |
| `inode_format_decode_never_panics_and_roundtrips` | `Vec<u8>` | `decode_v2(encode_v2(inode)) == inode` for all decodable inputs |
| `directory_entry_roundtrip` | `DirectoryEntry` | Encode/decode preserves name, inode, type |
| `find_entry_in_block_never_panics` | `(Vec<u8>, String)` | No panic on arbitrary block + filename |
| `filter_config_roundtrip` | `FilterConfig` | Encode/decode preserves all fields |
| `filter_pipeline_never_panics` | `(Vec<u8>, FilterConfig)` | Compress/decompress roundtrip or clean error |
| `ipc_frame_roundtrip` | `IpcRequest` | `read_framed(write_framed(req)) == req` |
| `journal_frame_roundtrip` | `JournalFrame` | Encode/decode preserves ops and CRC |
| `journal_op_coalesce_preserves_semantics` | `Vec<MetadataOp>` | `coalesce_ops` preserves net effect |
| `crc32c_incremental_matches_batch` | `(Vec<u8>, Vec<u8>)` | Incremental CRC32C == single-pass |
| `superblock_journal_header_block` | `SuperBlock` | Journal header block within bounds |
| `inode_v3_crc32c_single_bit_flip` | `InodeV3` | CRC32C detects any single bit flip |

Run with `cargo test --test bolero_fuzz_test --features mcp` for bounded runs, or `cargo bolero test --test bolero_fuzz_test <harness_name>` for continuous fuzzing with libFuzzer/AFL/honggfuzz.

---

## TLA+ Model Checking

`docs/formal_verification/OifsCowProtection.tla` specifies and model-checks the Copy-on-Write extent pointer swap protocol using the TLC model checker. The model verifies three critical invariants under arbitrary preemptive thread interleavings between multiple Reader threads and a concurrent Writer thread:

1. **`NoTornReadInvariant`**: Readers strictly observe either `OLD_DATA` or `NEW_DATA`, never unwritten garbage or torn blocks.
2. **`InodePointsToValidData`**: At any instant, the published Inode pointer points exclusively to valid data.
3. **`NoDanglingBlockPointer`**: Free blocks in the bitmap are never referenced by an active Inode pointer.

The TLA+ spec models the complete COW sequence: writer allocates new block → copies data → modifies → publishes new extent pointer via atomic CAS on the inode's extent array. Readers observe the inode extent array at arbitrary points. TLC exhaustively explores all interleavings for configurable numbers of readers/writers and block counts.

---

## Crash Safety Testing

`tests/crash_safety_test.rs` validates durability guarantees at three levels:

1. **Explicit Flush Persistence** (`test_flush_persistence`): Write data → explicit `dm.flush()` → drop → reopen → verify data intact.

2. **Drop Flush** (`test_drop_flush`): Write data → drop `DiskManager` without explicit flush → reopen → verify data intact (tests `Drop` impl flush).

3. **Block Overflow Safety** (`test_block_overflow_safety_does_not_return_superblock`): Verify `get_block_copy` with out-of-bounds block IDs (including `u64::MAX`) safely returns `None` and never returns Block 0 (superblock), preventing superblock corruption reads.

Additional crash safety coverage in `tests/durability_test.rs` tests Lazy vs Strict durability modes, and `tests/wal_and_concurrency_regressions_test.rs` tests journal WAL semantics under concurrent access.

---

## FUSE Live Mount Testing

`tests/fuse_live_mount_test.rs` mounts real OIFS `.img` files onto temporary mountpoints through the host OS kernel FUSE stack (`macFUSE` on macOS, `fusermount3`/FUSE on Linux) and exercises genuine POSIX syscalls via `std::fs`.

Key test scenarios:
- **Full POSIX lifecycle**: create, write, read, append, seek-write, truncate
- **Large file I/O**: 2 MiB write + random 4 KiB slice read
- **Nested directories + atomic cross-directory rename**
- **Unicode NFC/NFD normalization + readdir**
- **Concurrent readers/writers**: 8 reader threads + 4 writer threads
- **FUSE handle pagination**: `readdir` with 512-byte wire pages
- **Read-only handle flush skip**: `O_RDONLY` flush returns immediately
- **FSCK after live mount**: Verify consistency scanner passes

Marked `#[ignore]` so headless CI environments without FUSE installed remain green. Run explicitly on a FUSE-capable host:
```bash
cargo test --release --test fuse_live_mount_test -- --ignored --nocapture --test-threads=1
```

---

## NFS Live Mount Testing

`tests/nfs_live_mount_test.rs` mounts OIFS's pure-Rust local NFSv3 server via the OS kernel `mount_nfs` command. Zero third-party drivers, zero kext, zero reboot required on macOS.

Key test scenarios:
- **Full POSIX lifecycle** over NFSv3
- **Large 2 MiB file** write + random read verification
- **Nested directories, atomic cross-directory rename, Unicode NFC/NFD**
- **Hard links, symlinks, file permissions**
- **FSCK after live NFS mount**: Verify consistency scanner passes

Marked `#[ignore]` for CI runners without mount privileges:
```bash
cargo test --release --test nfs_live_mount_test -- --ignored --nocapture --test-threads=1
```

---

## C/C++ Compiler Integration Tests

`tests/ffi_c_cpp_test.rs` validates that `include/oifs.h` compiles with **zero warnings** under strict flags and exercises complete lifecycle workflows:

### C11 Test (`test_native_c_e2e`)
- Compiles with: `-std=c11 -Wall -Wextra -Wpedantic -Werror`
- Version handshake: `OIFS_CHECK_VERSION()`, `oifs_version_string()`
- Library path reporting: `oifs_loaded_path()`
- I/O backend API: `oifs_get_io_backend()`, `oifs_set_io_backend()`
- Directory/file operations: `oifs_mkdir`, `oifs_create_file`, `oifs_write_file`, `oifs_read_file`, `oifs_read_at`
- Directory listing with user context callback: `oifs_ls`
- File deletion and error diagnostics: `oifs_delete_file`, `oifs_last_error`
- Encrypted filesystem: `oifs_open_with_password` with good/bad password verification
- Session handle reuse: `oifs_get_or_open`

### C++17 Test (`test_native_cpp_e2e`)
- Compiles with: `-std=c++17 -Wall -Wextra -Wpedantic -Werror`
- Modern RAII: `std::unique_ptr<OIFSHandle, OIFSDeleter>`
- `std::vector<uint8_t>` I/O buffer integration
- C++ lambda as C callback in `oifs_ls`
- Multi-threaded concurrent reads/writes: 8 threads × 10 files each using `std::thread`

### C++20 Test (`test_native_cpp20_e2e`)
- Compiles with: `-std=c++20 -Wall -Wextra -Wpedantic -Werror`
- `std::span<const uint8_t>` / `std::span<uint8_t>` for zero-copy I/O
- C++20 Concepts: `ContiguousByteRange` concept verifying span compatibility
- Designated initializers in callback context
- `std::jthread` for RAII thread joining

---

## Performance Regression Suite

`tests/perf_regressions_test.rs` and `tests/perf_comparison.rs` provide ad-hoc performance regression detection:

| Test | Purpose |
| :--- | :--- |
| `test_fuse_readdir_pagination_and_rdonly_flush_skip` | FUSE readdir with 512-byte pages; read-only flush fast-path |
| `test_rename_preserves_dir_cache_consistency` | Dir cache invalidation on same-parent and cross-parent rename |
| `test_async_reads_cold_miss_and_hot_hit_across_modes` | Async read cache behavior: cold miss (background pool) vs hot hit (inline fast-path) for raw/zstd/seekable |
| `test_recommend_filters_preserves_order_and_baseline` | Filter recommendation determinism and baseline ordering |
| `test_ffi_last_error_is_thread_local_across_threads` | Thread-local error state isolation under concurrent FFI access |
| `test_inode_cache_shard_invalidation_on_rename` | Inode cache shard invalidation correctness |
| `test_chunk_read_cache_shard_concurrency` | ChunkReadCache shard thread-scaling |

Run with:
```bash
cargo test --test perf_regressions_test --features mcp --release
cargo test --test perf_comparison --features mcp --release
```

---

## CI Matrix Configuration

The GitHub Actions workflow (`.github/workflows/rust.yml`) currently runs on a single configuration:

```yaml
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Build
        run: cargo build --features mcp --verbose
      - name: Run tests
        run: cargo test --features mcp --verbose
```

**Recommended expanded matrix** for comprehensive coverage:

| Dimension | Values |
| :--- | :--- |
| OS | `ubuntu-latest`, `macos-latest` |
| Rust toolchain | `stable`, `beta`, `nightly` |
| Features | `default` (cli+io_uring), `mcp`, `mcp+io_uring`, `mcp+blosc2`, `mcp+io_uring+blosc2` |
| Test tier | `unit` (cargo test), `shuttle` (cargo test --test shuttle_concurrency_test), `bolero` (cargo bolero test), `kani` (cargo kani), `fuse` (ignored), `nfs` (ignored) |

Note: Kani requires a specialized runner with CBMC installed; Bolero continuous fuzzing requires libFuzzer/AFL; FUSE/NFS live mount tests require kernel support and elevated privileges.

---

## Thread Safety Annotations & Concurrency Primitives

OIFS uses several Rust concurrency patterns with explicit safety considerations:

### 128-Byte Cache Line Padding (False Sharing Prevention)

In `src/disk/caches.rs`, cache shards are padded to 128 bytes to prevent false sharing across CPU cores:

```rust
#[repr(align(128))]
struct ChunkCacheShard { /* ... */ }

#[repr(align(128))]
struct DirCacheShard { /* ... */ }

#[repr(align(128))]
struct DecodedBlobShard { /* ... */ }

const _: () = assert!(std::mem::size_of::<ChunkCacheShard>() >= 128);
```

Kani proof `proof_cache_padded_and_shard_indices_in_bounds` verifies:
- Shard index calculations stay within bounds for all `inode_id: u64` and `chunk_idx: usize`
- Alignment and size requirements hold for all padded types

### Thread-Local Storage

Multiple modules use `thread_local!` for per-thread state:
- `src/disk.rs`: Per-thread I/O buffers
- `src/disk/chunked.rs`: Per-thread seekable chunk decompression state
- `src/disk/transfer.rs`: Per-thread splice buffers
- `src/ffi.rs`: Per-thread error message buffer (`oifs_last_error` isolation)
- `src/ipc.rs`: Per-thread IPC frame buffers
- `src/storage.rs`: Per-thread FileIo buffers
- `src/zstd_pool.rs`: Per-thread Zstd compression contexts

The `test_ffi_last_error_is_thread_local_across_threads` test in `perf_regressions_test.rs` validates thread-local error isolation by triggering errors in one thread while another performs valid operations.

### Lock-Free Fast Paths

Session failover uses lock-free atomic flag (`AtomicBool`) for fast-path master slot population in `src/session.rs`:

```rust
dm_fallback_ready: Arc<AtomicBool>,  // Set once dm_fallback populated
```

---

## Summary of Verification Tiers

| Tier | Tool | Scope | Execution | CI Integration |
| :--- | :--- | :--- | :--- | :--- |
| **Unit/Integration** | `cargo test` | 40+ test files, concrete I/O, FFI, CLI, MCP | `cargo test --features mcp` | ✅ Full |
| **Concurrency (DPOR)** | Shuttle | Randomized thread interleavings, race/deadlock detection | `cargo test --test shuttle_concurrency_test` | ✅ Full |
| **Property/Fuzz** | Bolero | 16+ harnesses, arbitrary inputs, roundtrip invariants | `cargo test --test bolero_fuzz_test` / `cargo bolero test` | ✅ Bounded |
| **Formal (SMT)** | Kani (CBMC/CaDiCaL) | 131+ proofs, all symbolic inputs, exhaustive verification | `cargo kani --lib -j 8` | Manual (requires CBMC) |
| **Protocol (TLA+)** | TLC Model Checker | COW extent swap linearizability, 3 safety invariants | `java -cp tla2tools.jar tlc2.TLC ...` | Manual |
| **Live Kernel** | FUSE/NFS mount | Real POSIX syscalls via kernel | `cargo test --test fuse_live_mount_test -- --ignored` | Manual (needs kernel) |
| **Compiler ABI** | C/C++ compilers | Strict `-Wall -Wextra -Wpedantic -Werror`, C11/C++17/20 | `cargo test --test ffi_c_cpp_test` | ✅ Full |
| **Performance** | Custom benches | Regression detection for cache, async, FUSE, dir ops | `cargo test --test perf_regressions_test --release` | Manual |

This multi-layered approach ensures OIFS correctness from concrete execution through mathematical proof, covering the full spectrum from empirical validation to formal guarantees.
