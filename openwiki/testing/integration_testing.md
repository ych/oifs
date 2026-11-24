---
type: "Reference"
title: "Integration Testing"
description: "Documents end-to-end integration tests for OIFS: FUSE mount/unmount, NFS server, CLI workflows, FFI/C++ API, multi-process IPC, async/FFI, and cross-image operations."
tags: ["integration-testing", "fuse", "nfs", "ffi", "ipc", "async", "testing"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-23T20:55:55.458Z
sources:
  - id: openwiki-source-e0c73e47d42a2fa215e6ce2a
    resource: repo://tests/backward_compatibility_test.rs
  - id: openwiki-source-c7b570b88a7253380c45b32b
    resource: repo://tests/cli_concurrent_process_test.rs
  - id: openwiki-source-bbb9785c826d2923a01e6e09
    resource: repo://tests/ffi_c_cpp_test.rs
  - id: openwiki-source-ca0472ea7f5780d5442c7c15
    resource: repo://tests/format_migration_test.rs
  - id: openwiki-source-0b8862607b7dc9346dfa4a03
    resource: repo://tests/large_image_bitmap_test.rs
  - id: openwiki-source-b0f9117e3c34930435de9b17
    resource: repo://tests/mcp_server_test.rs
  - id: openwiki-source-cb9a877a2a968da6a562a2e9
    resource: repo://tests/network_single_file_sync_test.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-23T20:55:55.458Z" }
---

# Integration Testing

This guide describes the end-to-end integration tests that validate the OIFS file system's behavior across process boundaries, network transports, language bindings, and on-disk format transitions. These tests exercise the system as a whole, ensuring components work together correctly under realistic multi-process, multi-language, and multi-version conditions.

## Test Suite Overview

| Test File | Category | Focus Area |
|-----------|----------|------------|
| `tests/cli_concurrent_process_test.rs` | Multi-Process CLI | Concurrent `oifs` CLI processes using Local IPC (Unix Domain Sockets) |
| `tests/ffi_c_cpp_test.rs` | FFI/C++ Interop | Native C (C11) and C++ (C++17/20) compiler integration against `liboifs` |
| `tests/async_and_ffi_test.rs` | Async & FFI | Zero-copy mmap slices, async read fast/slow paths, group commit, C FFI session lifecycle, UAF safety |
| `tests/fuse_live_mount_test.rs` | FUSE Live Mount | Real kernel FUSE mount (macFUSE/Linux) with full POSIX lifecycle, read-only immutability, encryption, benchmarks |
| `tests/nfs_live_mount_test.rs` | NFS Live Mount | OS `mount_nfs` against pure-Rust NFSv3 server (zero kext/third-party drivers) |
| `tests/nfs_test.rs` | NFS Protocol | ONC RPC v2, XDR, MOUNT v3, all 18 NFSv3 procedures, pagination, fuzzing |
| `tests/session_edge_cases_test.rs` | Session Edge Cases | IPC boundary conditions, corrupted rendezvous self-healing, high concurrency, remote fsck/defrag, mode interleaving |
| `tests/mcp_server_test.rs` | MCP Server | JSON-RPC over stdio, tool schemas, initialize/tools_list, hostile name escaping |
| `tests/network_single_file_sync_test.rs` | Network Mode | Multi-node concurrent slice writes to single file via `OifsSession::open_network` |
| `tests/backward_compatibility_test.rs` | Backward Compat | Opening pre-format-bump v1 images (bincode inodes, no format_version) |
| `tests/format_migration_test.rs` | Format Migration | Synthesized v1 → v2 inode format migration, detection, in-place upgrade, fsck |
| `tests/large_image_bitmap_test.rs` | Large Images | Multi-block data bitmaps beyond 128 MiB, allocator/fsck/defrag consistency |

---

## Multi-Process CLI Concurrency

**File**: `tests/cli_concurrent_process_test.rs`

**Purpose**: Validates that multiple independent `oifs` CLI processes can concurrently write to and read from the same image using Local IPC mode (Unix Domain Sockets).

### Test: `test_cli_concurrent_processes`

**Setup**:
- Creates a temporary 10 MB image via `oifs create --size 10`.
- Prepares 4 payload files with distinct content.

**Concurrency**:
- Spawns 4 threads, each executing `oifs --ipc --image <path> put <payload> <remote_name>`.
- All 4 processes target the **same image** concurrently.
- The `--ipc` flag forces Local mode; master election uses a deterministic UDS socket path derived from the canonical image path and a companion `.tok` file for authentication.

**Verification**:
- After all puts complete, runs `oifs --ipc --image <path> ls` and asserts all 4 remote files appear.
- Spawns 4 threads to concurrently `get` each file back.
- Verifies downloaded content matches original payloads.

**Key Invariants**:
- Master election is deterministic and single-winner.
- Session registry deduplicates by canonical path (symlinks resolved).
- IPC framing (length-prefixed Bincode) + challenge-response authentication works under concurrent load.
- File metadata and data survive concurrent mutations.

---

## FFI C/C++ Interop

**File**: `tests/ffi_c_cpp_test.rs`

**Purpose**: Validates that `include/oifs.h` compiles cleanly under strict warnings (`-Wall -Wextra -Wpedantic -Werror`) in both C11 and C++17, and that the full lifecycle works end-to-end through the dynamic library. Also tests C++20 modern features.

### Test: `test_native_c_e2e` (C11)

**Compilation**: Dynamically locates `liboifs.{so,dylib,dll}` (debug or release), compiles with `clang` or `gcc`.

**C Workflow Verified**:
1. **Version handshake**: `OIFS_CHECK_VERSION()` macro and `oifs_version_*` functions.
2. **Loaded library path**: `oifs_loaded_path()` returns non-empty path.
3. **I/O backend API**: `oifs_get_io_backend`, `oifs_set_io_backend` roundtrip (Mmap ↔ Pread).
4. **Directory & file CRUD**: `oifs_mkdir`, `oifs_create_file`, `oifs_write_file`, `oifs_read_file`.
5. **Offset reads**: `oifs_read_at` for chunked access.
6. **Directory listing callback**: `oifs_ls` with user context struct.
7. **File deletion & error diagnostics**: `oifs_delete_file`, `oifs_last_error`.
8. **Encrypted image workflow**: `oifs_open_with_password`, read with good/bad passwords.
9. **Session handle reuse**: `oifs_get_or_open` shares underlying `DiskManager`.

### Test: `test_native_cpp_e2e` (C++17)

**Compilation**: Uses `clang++` or `g++` with `-std=c++17` and same strict flags.

**C++ Workflow Verified**:
1. **Version handshake**: `OIFS_CHECK_VERSION()` and `oifs_version_string()`.
2. **Modern RAII**: `std::unique_ptr<OIFSHandle, OIFSDeleter>` for automatic `oifs_close`.
3. **Directory & file CRUD**: `oifs_create_file`, `oifs_write_file`, `oifs_read_file` with `std::vector<uint8_t>`.
4. **Offset reads**: `oifs_read_at` into `std::vector<uint8_t>`.
5. **C++ Lambda Callback**: Captured `std::vector<DirEntry>` in `oifs_ls` callback.
6. **Multi-threaded Concurrency**: 8 worker threads (`std::thread`) concurrently reading/writing through a **shared handle** (10 files each = 80 files total).
7. **Final verification**: `oifs_ls` confirms all 80 worker files + original exist.

### Test: `test_native_cpp20_e2e` (C++20)

**Compilation**: Uses `-std=c++20` when compiler supports it.

**C++20 Features Exercised**:
1. **Concepts**: `ContiguousByteRange` concept verifying `std::span<const uint8_t>` convertibility.
2. **Designated Initializers**: `Stats{.count = 0, .total_bytes = 0}` in callback context.
3. **std::span**: Write/read via `std::span<const uint8_t>` and `std::span<uint8_t>`.
4. **std::jthread**: RAII threads with automatic join (when `__cpp_lib_jthread` available).
5. **Version handshake**: Same as C++17 plus modern API usage.

---

## Async & FFI Deep Integration

**File**: `tests/async_and_ffi_test.rs`

**Purpose**: Exercises the async read path (inline fast-path vs. background pool), zero-copy mmap slices across storage modes, group commit durability, and the complete C FFI session lifecycle including out-of-order drops and UAF safety barriers.

### Test: `test_file_read_handle_mmap_slice_zero_copy_and_fallbacks`

Validates `mmap_slice` behavior across storage modes and file types:

| Storage Mode | File Type | Expected Result |
|--------------|-----------|-----------------|
| `Mmap` | Single-block raw contiguous | `Some(&[u8])` zero-copy |
| `Mmap` | Multi-block (1 MiB) raw contiguous | `Some(&[u8])` zero-copy |
| `Mmap` | Stream-compressed | `None` (fallback to read) |
| `Mmap` | Seekable 64K chunked | `None` |
| `Mmap` | Sparse with holes | `None` |
| `FileIo` | Raw contiguous | `None` (uses `DataPageCache`) |
| `Mmap` + Encrypted | Raw | `None` (never exposes ciphertext) |

**Key Assertions**:
- Sub-slice and clamping past EOF work correctly on raw Mmap files.
- Fallback reads (`read_at`) return correct data when `mmap_slice` returns `None`.

### Test: `test_async_read_inline_fast_path_and_background_slow_path`

Verifies the async read state machine transitions:

1. **Mmap Raw File**: `read_at_async`/`read_data_async` → `is_ready() == true` immediately (Inline Fast-Path).
2. **Seekable 64K**: Cold read via background pool → warms `ChunkReadCache` → subsequent sub-chunk read is inline-ready.
3. **Stream-Compressed**: Cold read via background pool → warms `DecodedBlobCache` → subsequent reads inline-ready.
4. **FileIo**: Cold read (background) → warms `DataPageCache` → warm reads inline-ready.

**Mechanism**: `OifsAsyncTask<T>::is_ready()` exposes whether completion happened synchronously on the fast path.

### Test: `test_group_commit_strict_durability_mmap_and_file_io_wal`

Stress-tests group commit under `DurabilityMode::Strict`:

- 40 concurrent threads each call `create_file_with_data_async` (2 KiB payload, `CompressionMode::Never`).
- Synchronized start via `Barrier` to maximize contention.
- All 40 tasks awaited via `block_on`.
- **Verification**: `verify_integrity()` passes, reopen confirms all 40 files with exact payloads.
- Runs for both `StorageMode::Mmap` and `StorageMode::FileIo` (WAL enabled).

### Test: `test_c_ffi_session_zero_copy_out_of_order_drop_and_async_uaf_safety`

Comprehensive C FFI API test covering:

1. **Open with options**: `oifs_open_with_options` (Mmap + WAL), durability config, cache sizing.
2. **File CRUD**: `oifs_create_file_with_data`, `oifs_resolve_path`, `oifs_read_at_inode`.
3. **Batch Read**: `oifs_read_at_batch` across raw + stream-compressed files in single call.
4. **Session API**: `oifs_begin_read`, `oifs_session_open_file`, `oifs_session_open_inode`.
5. **Zero-Copy mmap_slice**: `oifs_file_mmap_slice` on raw file returns direct pointer; on compressed returns fallback code (1).
6. **Out-of-Order Drop Safety**: `oifs_end_read` called **before** `oifs_file_close` — handles remain valid because `OIFSFileHandle` holds its own `Arc<FfiSharedSession>`.
7. **Async FFI Fast-Path**: `oifs_read_at_async` on Mmap raw → `oifs_async_poll` returns 1 (ready) → `oifs_async_wait` returns bytes → `oifs_async_free`.
8. **UAF Drop Barrier**: 16× large (128 KiB) stream-compressed files; `oifs_read_at_async` dispatches to background pool → **immediately** `oifs_async_free` without `oifs_async_wait` → buffer mutated/dropped. `wait_on_drop` blocks background thread until write completes.
9. **Async Write Path**: `oifs_create_file_with_data_async`, `oifs_write_file_async`, `oifs_flush_async`.
10. **Callback APIs**: `oifs_create_file_with_data_async_cb` and `oifs_read_at_async_cb` with `Mutex`+`Condvar` synchronization.

---

## FUSE Live Mount Tests

**File**: `tests/fuse_live_mount_test.rs`

**Purpose**: Mounts real OIFS `.img` files onto temporary mountpoints through the host OS kernel FUSE stack (`macFUSE` on macOS, `fusermount3`/FUSE on Linux) and exercises genuine POSIX syscalls via `std::fs`.

**Execution**: Marked `#[ignore]`; run explicitly on FUSE-capable host:
```bash
cargo test --release --test fuse_live_mount_test -- --ignored --nocapture --test-threads=1
```

### Test: `test_live_fuse_mount_full_posix_lifecycle`

**Mount Configuration**: `FuseMountOptions` with configurable `max_write` (128 KiB / 1 MiB), `read_only`, `volname`.

**Operations Exercised**:
1. **File lifecycle**: Create, write, read, append (`O_APPEND` + `fsync`), truncate shorter, extend with zero-fill.
2. **Large file**: 1.5 MiB write + readback (multi-packet FUSE_WRITE/FUSE_READ).
3. **Directory pagination**: Nested directories with **210 files** (exceeds single READDIR reply, exercises kernel pagination).
4. **Atomic POSIX rename**: Same-dir, cross-dir, and overwrite semantics via live kernel FUSE.
5. **UTF-8 filenames**: NFC creation + NFD lookup on macOS (NFC/NFD normalization).
6. **macOS metadata rejection**: `.DS_Store` lookups return `ENOENT`, never pollute `.img`.

**Persistence Verification**: After unmount, reopen image via `DiskManager` and verify all content + `fsck` clean.

### Test: `test_live_fuse_mount_read_only_immutability`

- Opens image `read_only` via `DiskManager::open_read_only`.
- Mounts with `FuseMountOptions { read_only: true, ... }`.
- All reads succeed; **every mutating operation fails** (create, write, mkdir, unlink).
- **Byte-for-byte identity**: Snapshots image before/after mount; asserts exact match.

### Test: `test_live_fuse_mount_encrypted_file_io`

- Creates encrypted image with `DiskManager::open_with_options` (password, `StorageMode::FileIo`).
- Mounts via FUSE; writes/reads secret payload.
- Unmount, reopen with password, verify content + `fsck` clean.

### Test: `bench_live_fuse_mount_performance`

**Benchmarks** (run with `--nocapture`):
- **Baseline**: Native host filesystem (APFS on macOS) — 3 rounds.
- **OIFS FUSE A/B**: `Mmap+WAL` vs `FileIo+WAL` × `max_write=128K` vs `1M`.
- **Workloads**: 500×4 KiB write/read/delete, 16 MiB sequential write/read, 2,000 random 4 KiB reads.
- Outputs: ops/sec and MB/sec for each configuration.

---

## NFS Live Mount Tests

**File**: `tests/nfs_live_mount_test.rs`

**Purpose**: End-to-end OS `mount_nfs` against the pure-Rust NFSv3 server (`OifsNfsServer`). Zero third-party drivers, zero kext, zero reboot required on macOS.

**Execution**: Marked `#[ignore]`; requires mount privileges:
```bash
cargo test --test nfs_live_mount_test -- --ignored --nocapture
```

### Test: `test_live_nfs_mount_full_posix_lifecycle_and_fsck`

**Server Setup**: `OifsNfsServer::bind` with `NfsMountOptions`, `mount_os_nfs` helper.

**Operations**:
1. **File lifecycle**: Create, write, read, append, seek-write, truncate.
2. **Large file**: 2 MiB write + random read at 1 MiB offset.
3. **Nested directories**: Multi-level creation, atomic cross-directory rename, Unicode NFC/NFD.
4. **Readdir**: Lists root entries including created files/dirs.
5. **Cleanup**: Delete files, remove empty directories.
6. **Post-unmount fsck**: `verify_integrity()` must be clean.

### Test: `test_live_nfs_read_only_mount_immutability`

- Seeds image, snapshots bytes.
- Opens `read_only`, mounts with `read_only=true`.
- Reads succeed; writes fail.
- Post-unmount byte-for-byte identity asserted.

---

## NFS Protocol Tests

**File**: `tests/nfs_test.rs`

**Purpose**: Unit-level protocol integration for the pure-Rust NFSv3 server (`src/nfs.rs`). Exercises ONC RPC v2, XDR, MOUNT v3, all 18 NFSv3 procedures directly over TCP without kernel mount.

### Coverage

| Area | Details |
|------|---------|
| **ONC RPC v2** | TCP record marking (4-byte length prefix), XDR encoding/decoding |
| **MOUNT v3** | `MNTPROC3_MNT`, `MNTPROC3_UMNT`, `MNTPROC3_UMNTALL`, `MNTPROC3_EXPORT`, `MNTPROC3_DUMP`, `MNTPROC3_NULL` |
| **NFSv3 Procedures** (18) | `NULL`, `GETATTR`, `SETATTR`, `LOOKUP`, `ACCESS`, `READLINK`, `READ`, `WRITE`, `CREATE`, `MKDIR`, `SYMLINK`, `MKNOD`, `REMOVE`, `RMDIR`, `RENAME`, `LINK`, `READDIR`, `READDIRPLUS`, `FSSTAT`, `FSINFO`, `PATHCONF`, `COMMIT` |
| **Pagination** | Multi-page `READDIR`/`READDIRPLUS` with >200 entries (cookie-based) |
| **Read-Only Mode** | `NFS3ERR_ROFS` on mutating ops; byte-for-byte image immutability |
| **Storage Parity** | `Mmap`/`FileIo`/Journaled/Encrypted all produce identical NFS behavior |
| **Fuzzing** | Malformed RPC frames, truncated records, invalid XDR, oversized frames |

### Test Infrastructure

- `build_rpc_call` / `rpc_roundtrip`: Constructs and parses ONC RPC calls over `TcpStream`.
- `XdrWriter` / `XdrReader`: XDR (de)serialization helpers.
- `NfsFileHandle`: Opaque 64-byte handle encoding inode + generation.

---

## Session Edge Cases & IPC Stress

**File**: `tests/session_edge_cases_test.rs`

**Purpose**: Validates `OifsSession` behavior across IPC boundaries (Local and Network modes) under error conditions, concurrency, and lifecycle transitions.

### Test Matrix

| Test | Scenario | Key Assertion |
|------|----------|---------------|
| `test_empty_zero_byte_files_across_ipc` | 0-byte create/read/write/list | Empty `Vec` returned, metadata consistent |
| `test_deep_nested_directory_tree_and_path_resolution_across_ipc` | 5-level nested dirs + file | `resolve_parent`/`resolve_path` work across IPC |
| `test_invalid_lookups_and_nonexistent_operations_across_ipc` | Missing files, bad inodes, bad parents, bad paths | All return `Error` cleanly; subsequent valid ops succeed |
| `test_file_deletion_lifecycle_and_reuse_across_ipc` | Create → delete → recreate same name | New inode, new content; `fsck` clean |
| `test_large_multi_block_boundary_payloads_across_ipc` | 40 KiB (10 blocks) with Auto/Always/Never | All compression modes work; master verifies |
| `test_corrupted_rendezvous_file_self_healing` | Corrupt `.image.master` JSON | Client detects, removes, becomes Master; healthy ops |
| `test_high_concurrency_burst_operations_across_ipc` | 6 threads × 15 ops (dir+file+write+read+lookup) | All 90 files present; `fsck` clean |
| `test_remote_defragmentation_and_fsck_across_ipc` | Client triggers `analyze_fragmentation`, `defragment`, `verify_integrity` | Remote ops succeed; mismatched path defrag fails |
| `test_interleaved_local_and_network_modes_on_different_images` | Local session on image A + Network session on image B | Independent operation; no cross-contamination |
| `test_standalone_session_mode_default` | `OifsSession::open` (no mode flag) | `SessionMode::Standalone`, `is_direct()`, `peer_count=0` |
| `test_remote_create_or_open_file_existing` | Two `create_or_open_file` calls same name | Returns same inode |
| `test_master_drop_with_idle_remote_client_fails_over_cleanly` | Drop master while client idle | Client RPC fails over seamlessly; read succeeds |

---

## MCP Server Protocol Compliance

**File**: `tests/mcp_server_test.rs`

**Purpose**: Validates the `oifs_mcp` server implements MCP (Model Context Protocol) over stdio correctly.

### Tests

| Test | Coverage |
|------|----------|
| `test_mcp_cli_help_and_arguments` | `--help` output, invalid `--size` rejection |
| `test_mcp_stdio_initialize_and_tools_list` | `initialize` request/response, `notifications/initialized`, `tools/list` response with tool schemas |
| `test_mcp_tools_call_hostile_names` | Tool calls with Unicode, path traversal attempts, null bytes — all safely escaped/rejected |

---

## Network Mode Multi-Node Sync

**File**: `tests/network_single_file_sync_test.rs`

**Purpose**: Validates concurrent multi-node access to a single file via `OifsSession::open_network` (TCP).

### Test: `test_network_multi_node_concurrent_slice_writes_to_single_file`

- **Master (Node 0)**: Creates image, creates `shared_matrix.dat`.
- **8 Client Nodes**: Each opens via `open_network`, writes unique 4 KiB slice at designated offset (`node_idx * 4096`).
- **Verification**: New client reads full 32 KiB file; each 4 KiB region matches expected node pattern.

---

## Backward Compatibility & Format Migration

### Backward Compatibility
**File**: `tests/backward_compatibility_test.rs`

**Test: `test_v1_image_compatibility`**
- Synthesizes a v1 image: superblock without `format_version`, inodes as packed `bincode` records.
- Opens with current `DiskManager::open` — must succeed.
- Verifies superblock fields, root inode, basic read/write.

### Format Migration
**File**: `tests/format_migration_test.rs`

**Test: `test_v1_to_v2_migration_and_fsck`**
1. Creates fresh v2 image, writes data.
2. `downgrade_to_v1()` rewrites superblock (drops `format_version`) and converts all allocated inode slots from v2 fixed layout → packed `bincode` + stale tail.
3. Opens downgraded image — `DiskManager` detects v1, migrates in-place to v2.
4. All data accessible; `verify_integrity()` clean.

**Test: `test_v1_encrypted_migration`**
- Same flow with encrypted image (password-protected).
- Migration preserves encryption; reopen with password works.

---

## Large Image & Multi-Block Bitmap Tests

**File**: `tests/large_image_bitmap_test.rs`

**Purpose**: Validates the v3 layout where data bitmap spans multiple blocks, enabling images beyond 128 MiB (32,768 blocks × 4 KiB).

### Tests

| Test | Assertion |
|------|-----------|
| `test_large_image_gets_a_multi_block_data_bitmap` | 200 MiB image → `data_bitmap_blocks() > 1`; region layout contiguous (inode_bitmap → data_bitmap → inode_table → data_blocks) |
| `test_small_image_keeps_a_single_bitmap_block` | 10 MiB image → single bitmap blocks; classic layout |
| `test_multi_block_image_stays_consistent` | 200 MiB sparse image: create 8×32 KiB files, write/read, `fsck` clean |

---

## Cross-Platform Test Considerations

The test suite handles platform differences explicitly:

| Concern | Handling |
|---------|----------|
| **Dynamic library naming** | `liboifs.so` (Linux), `liboifs.dylib` (macOS), `oifs.dll` (Windows) |
| **Library load path** | `LD_LIBRARY_PATH` (Linux), `DYLD_LIBRARY_PATH` (macOS), `PATH` (Windows) |
| **Compiler detection** | `clang`/`clang++` preferred; falls back to `gcc`/`g++` |
| **Temp directories** | `tempfile::tempdir()` (OS-appropriate) |
| **Path encoding** | UTF-8 assumed; NFC/NFD normalization tested on macOS |
| **Network binding** | Loopback-only (`127.0.0.1:0`) for TCP tests |
| **Unix Domain Sockets** | Local IPC only; `0600` socket, `0700` directory permissions |
| **On-disk format** | Little-endian fixed layout; no platform-dependent padding |

---

## Related Pages

- [Testing and Verification](../operations/testing_and_verification.md) — Overall testing strategy, CI gates, property-based testing
- [Crash Safety Testing](./crash_safety_testing.md) — Power-loss simulation, journal replay, fsck validation
- [TSan Testing](./tsan_testing.md) — ThreadSanitizer integration, data race detection in CI
