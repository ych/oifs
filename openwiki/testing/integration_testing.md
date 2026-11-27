---
type: "Reference"
title: "Integration Testing"
description: "Documents end-to-end integration tests for OIFS: FUSE live mount, NFS live mount, FFI C/C++ interop, multi-process IPC security, async/FFI, and cross-image operations."
tags: ["integration-testing", "fuse", "nfs", "ffi", "ipc", "async", "testing"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
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
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

# Integration Testing

This guide describes the end-to-end integration tests that validate the OIFS file system's behavior across process boundaries, network transports, language bindings, and on-disk format transitions. These tests exercise the system as a whole, ensuring components work together correctly under realistic multi-process, multi-language, and multi-version conditions.

## Test Suite Overview

| Test File | Category | Focus Area |
|-----------|----------|------------|
| `tests/ffi_c_cpp_test.rs` | FFI/C++ Interop | Native C (C11) and C++ (C++17/20) compiler integration against `liboifs` |
| `tests/fuse_live_mount_test.rs` | FUSE Live Mount | Real kernel FUSE mount (macFUSE/Linux) with full POSIX lifecycle, read-only immutability, encryption, benchmarks |
| `tests/nfs_live_mount_test.rs` | NFS Live Mount | OS `mount_nfs` against pure-Rust NFSv3 server (zero kext/third-party drivers) |
| `tests/ipc_security_test.rs` | IPC Security | Challenge-response authentication, socket/secret permissions, bounds on unauthenticated peers |
| `tests/async_and_ffi_test.rs` | Async & FFI | Zero-copy mmap slices, async read fast/slow paths, group commit, C FFI session lifecycle, UAF safety |
| `tests/cli_concurrent_process_test.rs` | Multi-Process CLI | Concurrent `oifs` CLI processes using Local IPC (Unix Domain Sockets) |
| `tests/nfs_test.rs` | NFS Protocol | ONC RPC v2, XDR, MOUNT v3, all 18 NFSv3 procedures, pagination, fuzzing |
| `tests/session_edge_cases_test.rs` | Session Edge Cases | IPC boundary conditions, corrupted rendezvous self-healing, high concurrency, remote fsck/defrag, mode interleaving |
| `tests/mcp_server_test.rs` | MCP Server | JSON-RPC over stdio, tool schemas, initialize/tools_list, hostile name escaping |
| `tests/network_single_file_sync_test.rs` | Network Mode | Multi-node concurrent slice writes to single file via `OifsSession::open_network` |
| `tests/backward_compatibility_test.rs` | Backward Compat | Opening pre-format-bump v1 images (bincode inodes, no format_version) |
| `tests/format_migration_test.rs` | Format Migration | Synthesized v1 → v2 inode format migration, detection, in-place upgrade, fsck |
| `tests/large_image_bitmap_test.rs` | Large Images | Multi-block data bitmaps beyond 128 MiB, allocator/fsck/defrag consistency |

---

## FFI C/C++ Interop

**File**: `tests/ffi_c_cpp_test.rs`

**Purpose**: Validates that `include/oifs.h` compiles cleanly under strict warnings (`-Wall -Wextra -Wpedantic -Werror`) in both C11 and C++17, and that the full lifecycle works end-to-end through the dynamic library. Also tests C++20 modern features.

### Compilation & Test Harness

The test dynamically locates `liboifs.{so,dylib,dll}` in `target/debug` or `target/release`, builds it with `cargo build --lib` if missing, and selects `clang`/`gcc` or `clang++`/`g++` from PATH. Compiler detection falls back gracefully (test is skipped if no compiler found).

Platform-specific library load paths are set at execution time:
- macOS: `DYLD_LIBRARY_PATH`
- Linux/others: `LD_LIBRARY_PATH`
- Windows: `PATH`

### Test: `test_native_c_e2e` (C11)

**C Workflow Verified**:

1. **Version handshake**: `OIFS_CHECK_VERSION()` macro and `oifs_version_*` functions (major=1, minor=6, patch=1, string="1.6.1").
2. **Loaded library path**: `oifs_loaded_path()` returns non-empty absolute path to the dynamic library.
3. **I/O backend API**: `oifs_get_io_backend`, `oifs_set_io_backend` roundtrip (Mmap ↔ Pread).
4. **Directory & file CRUD**: `oifs_mkdir`, `oifs_create_file`, `oifs_write_file`, `oifs_read_file`.
5. **Offset reads**: `oifs_read_at` for chunked access (e.g., offset 5, read 4 bytes).
6. **Directory listing callback**: `oifs_ls` with user context struct (`struct ListContext { int file_count; uint64_t total_bytes; }`).
7. **File deletion & error diagnostics**: `oifs_delete_file`, `oifs_last_error`.
8. **Encrypted image workflow**: `oifs_open_with_password`, read with good/bad passwords (wrong password causes read to return -1).
9. **Session handle reuse**: `oifs_get_or_open` shares underlying `DiskManager`.

### Test: `test_native_cpp_e2e` (C++17)

**Modern C++ Features Verified**:

1. **RAII smart pointer**: `std::unique_ptr<OIFSHandle, OIFSDeleter>` with custom deleter calling `oifs_close`.
2. **`std::vector<uint8_t>` I/O buffer integration**: Write and read-back into vectors.
3. **Lambda callback capture**: `oifs_ls` with C++ lambda capturing `std::vector<DirEntry>` by reference.
4. **Multi-threaded concurrency**: 8 `std::thread` workers, each performing 10 write+read cycles (80 files total), all sharing the same `OIFSHandle*`. Verifies final `oifs_ls` returns ≥81 entries.

### Test: `test_native_cpp20_e2e` (C++20)

**Additional C++20 Features Verified** (compiled with `-std=c++20` when supported):

1. **`std::span<const uint8_t>` / `std::span<uint8_t>`** for zero-copy buffer views; compile-time `ContiguousByteRange` concept validates compatibility.
2. **Designated initializers**: `Stats stats{.count = 0, .total_bytes = 0}` in callback context.
3. **`std::jthread`** (when `__cpp_lib_jthread` available): Automatic RAII join on scope exit for 6 worker threads.
4. Same concurrency model as C++17 test.

### Version Compatibility Policy

The `OIFS_CHECK_VERSION()` macro and `oifs_check_version()` implement a **"block older, allow newer"** policy:

| Return Code | Constant | Meaning |
|-------------|----------|---------|
| 0 | `OIFS_VERSION_COMPAT_OK` | Exact version match (clean) |
| 1 | `OIFS_VERSION_COMPAT_WARN` | Newer runtime version loaded (warning, proceed) |
| -1 | `OIFS_VERSION_COMPAT_ERR` | Older runtime version loaded (error, abort) |

This allows applications compiled against an older header to run safely on a newer `liboifs`, while blocking known-incompatible older libraries.

---

## FUSE Live Mount Tests

**File**: `tests/fuse_live_mount_test.rs`

**Purpose**: Mounts real OIFS `.img` files onto temporary mountpoints through the host OS kernel FUSE stack (`macFUSE` on macOS, `fusermount3`/FUSE on Linux) and exercises genuine POSIX syscalls via `std::fs`.

Tests are marked `#[ignore]` so headless CI environments without FUSE installed remain green. Run explicitly on a FUSE-capable host:

```bash
cargo test --release --test fuse_live_mount_test -- --ignored --nocapture --test-threads=1
```

### Test Infrastructure: `LiveMountGuard`

RAII guard that:
- Mounts `OifsFuseFs` on a background thread
- Polls `st_dev` (device ID) until it differs from parent directory (confirms kernel mount active)
- Guarantees unmount + thread join on `Drop` even if test panics
- Provides `force_unmount()` fallback using `diskutil unmount force` (macOS) or `fusermount3 -uz` (Linux)

### Test: `test_live_fuse_mount_rw_lifecycle_large_file_dirs_and_macos_unicode`

**Full POSIX Lifecycle**:

1. **Create, read, overwrite, append, truncate, extend**: `std::fs::write`, `OpenOptions::append`, `set_len`.
2. **Large file (1.5 MiB)**: Exercises multi-packet `FUSE_WRITE` / `FUSE_READ`; pattern-verified readback.
3. **Nested directories + 210 files**: Exercises kernel `READDIR` pagination; verifies all entries present.
4. **Atomic POSIX rename**: Same-dir, cross-dir, and overwrite rename scenarios via live kernel FUSE.
5. **UTF-8 Unicode filenames**: NFC creation (`café_測試.txt`) + NFD lookup on macOS (NFD normalization by macFUSE).
6. **macOS Finder noise rejection**: `.DS_Store` lookup returns `ENOENT`; never pollutes `.img`.

**Persistence Verification**:
- Clean unmount via `guard.unmount_now()`
- Reopen image directly via `DiskManager::open`
- Verify all file contents byte-for-byte identical
- Run `verify_integrity()` (fsck) and assert `report.is_clean`

### Test: `test_live_fuse_mount_read_only_immutability`

**Read-Only Mount Guarantees**:

- Seeds image, snapshots exact bytes before mount
- Opens via `DiskManager::open_read_only` + `FuseMountOptions { read_only: true }`
- **Reads succeed**: `fs::read`, directory traversal
- **All mutations fail**: `write`, `open O_WRONLY`, `create_dir`, `remove_file` → all return errors
- Post-unmount byte-for-byte comparison: `bytes_before == bytes_after` (100% identical)

### Test: `test_live_fuse_mount_encrypted_file_io`

**Encrypted Filesystem via FUSE**:

- Creates encrypted image with `DiskManager::open_with_options(..., Some(password), true, StorageMode::FileIo)`
- Mounts via FUSE, writes/reads secret data
- Verifies ciphertext never exposed: reopens encrypted image directly, confirms plaintext via `read_data`
- Runs `verify_integrity()` on encrypted image post-mount

### Benchmark: `bench_live_fuse_mount_performance`

**Comparative Performance** (run with `--nocapture --test-threads=1`):

| Configuration | Storage Mode | `max_write` | Notes |
|---------------|--------------|-------------|-------|
| APFS (native) | — | — | Host filesystem baseline (3 rounds) |
| Mmap+WAL | `StorageMode::Mmap` | 128 KiB / 1 MiB | FUSE `max_write` tuning |
| FileIo+WAL | `StorageMode::FileIo` | 128 KiB / 1 MiB | Pure pread/pwrite path |

**Workload**:
- 500 × 4 KiB small file write/read/delete (ops/sec)
- 16 MiB sequential write + read (MB/s)
- 2,000 random 4 KiB reads (ops/sec)

Outputs formatted table with checksum verification (`[chk=sum]`) to ensure data integrity.

---

## NFS Live Mount Tests

**File**: `tests/nfs_live_mount_test.rs`

**Purpose**: OS `mount_nfs` against OIFS's pure-Rust NFSv3 server (`OifsNfsServer`). Zero third-party drivers, zero kext, zero reboot required on macOS.

Tests are marked `#[ignore]`; run directly via:
```bash
cargo test --test nfs_live_mount_test -- --ignored --nocapture
```

### Test Infrastructure: `LiveNfsMountGuard`

RAII guard that:
- Binds `OifsNfsServer` on ephemeral port via `OifsNfsServer::bind`
- Calls `mount_os_nfs(server.port(), mountpoint, read_only)` (platform `mount_nfs` wrapper)
- Verifies `is_mounted(mountpoint)` via `mount`/`findmnt` output parsing
- On `Drop`: unmounts + stops server

### Test: `test_live_nfs_mount_full_posix_lifecycle_and_fsck`

**Full POSIX Lifecycle via NFSv3**:

1. **Create, write, read, append, seek-write, truncate**: Standard `std::fs` operations.
2. **Large 2 MiB file**: Pattern-verified write + random 4 KiB read at offset 1 MiB.
3. **Nested directories**: `alpha/beta`, `gamma`; Unicode NFC/NFD filename (`café.txt`) with cross-directory atomic rename.
4. **`readdir`**: Verifies directory entries including unicode names.
5. **Delete + rmdir**: File and empty directory removal.

**Post-Unmount Verification**:
- `dm.flush()` + `dm.verify_integrity()` → `report.is_clean`

### Test: `test_live_nfs_read_only_mount_immutability`

**Read-Only NFS Mount Guarantees**:

- Seeds image, snapshots bytes before mount
- Opens via `DiskManager::open_read_only`, mounts with `read_only: true`
- Reads succeed; `fs::write` fails
- Post-unmount byte-for-byte comparison: `before == after`

### NFS Server Architecture Notes

`OifsNfsServer` implements:
- **ONC RPC v2** + **XDR** encoding/decoding
- **MOUNT v3** (MOUNTPROC3_MNT, MNT3ERR_* errors)
- **All 18 NFSv3 procedures**: NULL, GETATTR, SETATTR, LOOKUP, ACCESS, READLINK, READ, WRITE, CREATE, MKDIR, SYMLINK, MKNOD, REMOVE, RMDIR, RENAME, LINK, READDIR, READDIRPLUS, FSSTAT, FSINFO, PATHCONF, COMMIT
- **Attribute caching**: `OIFS_ATTR_TIMEOUT` (default 1s) controls kernel attribute cache TTL; `setattr` invalidates cache
- **Pure Rust**: No external NFS libraries; runs in-process as TCP server on loopback

---

## IPC Security & Multi-Process Session Tests

**File**: `tests/ipc_security_test.rs`

**Purpose**: Validates the IPC transport hardening: challenge-response authentication, socket/secret permissions, and bounds on unauthenticated peers.

### Test: `test_socket_and_token_are_owner_only`

- Opens IPC master session (`OifsSession::open_ipc`)
- Verifies UDS socket file permissions: `0o600` (owner read/write only)
- Verifies session token file permissions: `0o600`
- Verifies socket parent directory permissions: `0o700` (private, not world-writable `/tmp`)

### Test: `test_rendezvous_file_is_owner_only`

- Opens network master session (`OifsSession::open_network`)
- Verifies rendezvous file (`.oifs_master_<hash>.json`) permissions: `0o600`
- Verifies content is `MasterInfo` JSON with 64-char `auth_token` (not bare address)

### Test: `test_client_rejects_a_master_that_cannot_prove_its_key`

- Spawns fake master on UDS that answers challenge with wrong MAC
- Client attempts authentication → fails with "failed to prove" error

### Test: `test_unauthenticated_peer_cannot_read_data`

- Master creates file with secret data
- Rogue peer connects directly to UDS, sends `ReadData` without handshake
- Server responds with `Error("Password required")`; no data leaked

### Test: `test_client_authenticates_with_the_published_secret`

- Second session opens as client (`OifsSession::open_ipc` with size 0)
- Client successfully authenticates using token file secret
- Client can resolve root inode (0)

### Test: `test_master_and_client_agree_on_the_session_secret`

- Client reads token file directly (32 bytes)
- Connects via `bind_or_connect` as `MasterOrClient::Client`
- Authenticates with `AuthCredential::Secret(secret)` → succeeds

### Test: `test_non_loopback_bind_requires_an_explicit_opt_in`

- `is_loopback_bind()` validates: `127.0.0.1`, `[::1]` → true; `0.0.0.0`, `192.168.x.x`, `:` → false
- CLI `--network --bind 0.0.0.0:19050` refused without `--allow-remote-ipc`
- Error message mentions `--allow-remote-ipc` opt-in flag

### Test: `test_session_event_channel_is_bounded`

- Master + client; client generates `MAX_PENDING_SESSION_EVENTS + 200` read operations without draining
- Master's event receiver buffers at most `MAX_PENDING_SESSION_EVENTS` events (bounded channel)
- Excess events dropped, not unbounded memory growth

---

## Async & FFI Deep Integration Tests

**File**: `tests/async_and_ffi_test.rs`

**Purpose**: Validates zero-copy mmap slices, async read fast/slow paths, group commit, C FFI session lifecycle, and UAF (use-after-free) safety.

### Test: `test_file_read_handle_mmap_slice_zero_copy_and_fallbacks`

**Zero-Copy `mmap_slice` Conditions** (returns `Some(&[u8])`):
- `StorageMode::Mmap`
- Uncompressed raw file (contiguous blocks)
- Not encrypted

**Fallback to `None` (caller must use `read_at`)**:
- `StorageMode::FileIo` (even for raw files)
- Stream-compressed files
- Seekable 64K chunked compression
- Sparse files with unallocated holes
- Encrypted files (never exposes ciphertext)

**Verified Scenarios**:
- Single-block file (3 KiB)
- Multi-block 1 MiB file (direct + single-indirect blocks)
- Sub-slice clamping past EOF → empty slice
- Encrypted filesystem on Mmap → `None`, fallback `read_all` works

### Test: `test_async_read_inline_fast_path_and_background_slow_path`

**Inline Fast-Path** (`is_ready() == true` immediately):
- `StorageMode::Mmap` raw file: `read_at_async`, `read_data_async`
- Warm `ChunkReadCache` (seekable 64K): second read in same chunk
- Warm `DecodedBlobCache` (stream-compressed): subsequent reads
- Warm `DataPageCache` (FileIo): after cold read populates cache

**Background Slow-Path** (dispatched to persistent pool):
- Cold seekable 64K read (first chunk)
- Cold stream-compressed read (first decode)
- Cold FileIo read (first page load)

**Cache Warming Verification**: Each storage mode demonstrates cold → warm transition with `is_ready()` flipping from false to true.

### Test: `test_group_commit_strict_durability_mmap_and_file_io_wal`

**Group Commit Under `DurabilityMode::Strict`**:

- 40 concurrent `create_file_with_data_async` tasks synchronized via `Barrier`
- All writes share **single physical sync barrier** per batch (group commit)
- Runs for both `StorageMode::Mmap` and `StorageMode::FileIo` (WAL)
- Post-commit: `verify_integrity()` → clean
- Reopen image: all 40 files present with exact payloads

### Test: `test_c_ffi_session_zero_copy_out_of_order_drop_and_async_uaf_safety`

**C FFI Full Lifecycle**:

1. **Open with options**: `oifs_open_with_options` (Mmap + WAL), `oifs_set_durability_mode`, `oifs_set_file_io_cache_bytes`
2. **Direct inode I/O**: `oifs_create_file_with_data`, `oifs_resolve_path`, `oifs_read_at_inode`
3. **Batch read**: `oifs_read_at_batch` across two files (raw + compressed)
4. **Shared read session + zero-copy**:
   - `oifs_begin_read` → `OIFSReadSession*`
   - `oifs_session_open_file` / `oifs_session_open_inode` → `OIFSFileHandle*`
   - **Out-of-order drop**: `oifs_end_read(session)` **before** `oifs_file_close(fh1/fh2)` — must not UAF (file handles hold `Arc<FfiSharedSession>`)
   - `oifs_file_mmap_slice` on raw file → returns 0, valid pointer+len, zero-copy verified
   - `oifs_file_mmap_slice` on compressed file → returns 1 (fallback), `oifs_file_read_at` works
5. **Async FFI UAF Safety**:
   - Hot mmap read: `oifs_read_at_async` → `oifs_async_poll` = 1 (inline) → `oifs_async_wait` → `oifs_async_free`
   - **Cold compressed read + immediate drop**: 16 rounds of 128 KiB stream-compressed files; `oifs_read_at_async` dispatches to background; **immediately** `oifs_async_free(fut)` **without** `oifs_async_wait`; buffer poisoned (`fill(0xAA)`) and dropped right after → `oifs_async_free` waits for background write to complete before returning (wait-on-drop barrier)
6. **Async create/write/flush**: `oifs_create_file_with_data_async`, `oifs_write_file_async`, `oifs_flush_async`
7. **Callback APIs**: `oifs_create_file_with_data_async_cb` + `oifs_read_at_async_cb` with C-compatible callbacks (`extern "C" fn`), synchronized via `Mutex<CbState>` + `Condvar`

---

## Cross-Platform Test Considerations

The test suite handles platform differences explicitly:

| Aspect | macOS | Linux | Windows |
|--------|-------|-------|---------|
| Dynamic library name | `liboifs.dylib` | `liboifs.so` | `oifs.dll` |
| Library load path env | `DYLD_LIBRARY_PATH` | `LD_LIBRARY_PATH` | `PATH` |
| Compiler detection | `clang`/`gcc` | `clang`/`gcc` | `clang`/`gcc` (MSVC not tested) |
| Temp directories | `tempfile` crate | `tempfile` crate | `tempfile` crate |
| Path encoding | UTF-8 assumption | UTF-8 assumption | UTF-8 assumption |
| TCP bind | Loopback only | Loopback only | Loopback only |
| Unix Domain Sockets | Yes (0600/0700 perms) | Yes (0600/0700 perms) | No (skipped) |
| On-disk endianness | Little-endian | Little-endian | Little-endian |
| FUSE helper | macFUSE / osxfuse | `/dev/fuse` + `fusermount3` | N/A |
| NFS mount | `mount_nfs` (built-in) | `mount.nfs` (nfs-common) | N/A |

**Key Invariants**:
- Little-endian on-disk format enforced via `OIFS_LITTLE_ENDIAN` macros in `include/oifs.h`
- IPC socket + token directory permissions: `0700` dir, `0600` files (owner-only)
- Network mode binds loopback only unless `--allow-remote-ipc` explicitly passed
- All C/C++ compilation uses `-std=c11` / `-std=c++17` / `-std=c++20` with `-Wall -Wextra -Wpedantic -Werror`

---

## Running Integration Tests

### Prerequisites

| Test Category | Requirements |
|---------------|--------------|
| FFI C/C++ | `clang`/`gcc` and `clang++`/`g++` in PATH |
| FUSE Live Mount | macFUSE (macOS) or `/dev/fuse` + `fusermount3` (Linux) |
| NFS Live Mount | `mount_nfs` (macOS) or `mount.nfs` from `nfs-common` (Linux); root/sudo for privileged ports if needed |
| All Tests | `cargo build --release --lib` to produce `liboifs` |

### Commands

```bash
# FFI C/C++ (runs by default in CI if compilers present)
cargo test --test ffi_c_cpp_test --release

# FUSE Live Mount (explicit opt-in)
cargo test --test fuse_live_mount_test --release -- --ignored --nocapture --test-threads=1

# NFS Live Mount (explicit opt-in)
cargo test --test nfs_live_mount_test --release -- --ignored --nocapture

# IPC Security
cargo test --test ipc_security_test --release

# Async & FFI
cargo test --test async_and_ffi_test --release

# Full suite (excluding ignored)
cargo test --release
```

---

## Related Pages

- [FUSE Engine](../architecture/fuse_engine.md) — FUSE kernel protocol implementation
- [NFS Server](../architecture/nfs_server.md) — Pure-Rust NFSv3 server architecture
- [Testing & Verification](../operations/testing_and_verification.md) — Test strategy, CI integration, property testing
- [Crash Safety Testing](./crash_safety_testing.md) — Power-loss simulation, journal replay, fsck verification
