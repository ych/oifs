---
type: "Reference"
title: "Integration Testing"
openwiki_generated: true
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
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
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---


# Integration Testing

This guide describes the end-to-end integration tests that validate the OIFS file system's behavior across process boundaries, network transports, language bindings, and on-disk format transitions. These tests exercise the system as a whole, ensuring components work together correctly under realistic multi-process, multi-language, and multi-version conditions.

## Test Categories

The integration tests fall into seven main categories:

1. **Multi-Process CLI Concurrency** (`tests/cli_concurrent_process_test.rs`) — Concurrent `oifs` CLI processes using IPC mode.
2. **FFI C/C++ Interop** (`tests/ffi_c_cpp_test.rs`) — Native C and C++ compiler integration against `liboifs`.
3. **MCP Server Protocol Compliance** (`tests/mcp_server_test.rs`) — JSON-RPC over stdio, tool schemas, and hostile name escaping.
4. **Network Mode Over TCP** (`tests/network_single_file_sync_test.rs`) — Multi-node concurrent access via `OifsSession::open_network`.
5. **Backward Compatibility** (`tests/backward_compatibility_test.rs`) — Opening pre-format-bump v1 images.
6. **Format Migration (v1 → v2 Inode Format)** (`tests/format_migration_test.rs`) — Synthesized v1 images, detection, migration, and fsck.
7. **Large Image & Bitmap Tests** (`tests/large_image_bitmap_test.rs`) — Multi-block data bitmaps beyond 128 MiB.

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

**Purpose**: Validates that `include/oifs.h` compiles cleanly under strict warnings (`-Wall -Wextra -Wpedantic -Werror`) in both C11 and C++17, and that the full lifecycle works end-to-end through the dynamic library.

### Test: `test_native_c_e2e`

**Compilation**: Dynamically locates `liboifs.{so,dylib,dll}` (debug or release), compiles a C program with `clang` or `gcc`.

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

### Test: `test_native_cpp_e2e`

**Compilation**: Uses `clang++` or `g++` with `-std=c++17` and same strict flags.

**C++ Workflow Verified**:
1. **Version handshake** via C++ strings.
2. **RAII handle**: `std::unique_ptr<OIFSHandle, OIFSDeleter>` auto-closes on scope exit.
3. **`std::vector<uint8_t>` I/O buffers** for write and read.
4. **C++ lambda callback** capturing `std::vector<DirEntry>` for `oifs_ls`.
5. **Multi-threaded concurrency**: 8 `std::thread` workers concurrently writing and reading distinct files through the **same shared handle** (which wraps an `Arc<RwLock<DiskManagerInner>>`).

**Key Invariants**:
- Zero compiler warnings in C11 and C++17.
- Opaque handle model with per-handle error isolation works across language boundary.
- Callback ABI compatibility (C function pointer invoked from Rust, capturing C++ state).
- Thread-safe shared handle: 80 concurrent files created and verified.
- Encrypted image roundtrip with password validation.

---

## MCP Server Protocol Compliance

**File**: `tests/mcp_server_test.rs`

**Purpose**: Validates the `oifs_mcp` binary implements the Model Context Protocol (MCP) over stdio correctly, including JSON-RPC framing, tool schema advertisement, and robust JSON escaping for hostile file names.

### Test: `test_mcp_cli_help_and_arguments`

- Verifies `--help` output contains usage and `--size` flag.
- Invalid `--size` value returns non-zero exit code.

### Test: `test_mcp_stdio_initialize_and_tools_list`

- Spawns `oifs_mcp --size 10 <image>` as child process with piped stdio.
- Sends MCP `initialize` request (protocol version `2024-11-05`).
- Sends `notifications/initialized`.
- Sends `tools/list` request.
- Asserts response contains valid JSON-RPC, server instructions ("OIFS Memory Sandbox"), and `tools` capability.

### Test: `test_mcp_tool_response_escapes_hostile_names`

- Sends `tools/call` for `write_file` with a path containing quotes, backslashes, and control characters: `inject\"),\"evil\":1,\"x.json`.
- Sends `tools/call` for `list_dir` to embed the hostile name in response.
- **Critical invariant**: Every non-empty stdout line from the server must parse as valid JSON. The tool payload `text` field must also parse as valid JSON. This guarantees `serde_json` serialization (never hand-rolled `format!`) protects against injection.

**Key Invariants**:
- JSON-RPC 2.0 compliance over stdio transport.
- Tool schemas advertised during initialize.
- All tool responses are valid JSON even with adversarial input.
- Single-image sandbox model: all paths resolved relative to image root inode.

---

## Network Mode Over TCP

**File**: `tests/network_single_file_sync_test.rs`

**Purpose**: Validates `OifsSession::open_network` enables multi-node concurrent access to a single image over TCP, with master election via rendezvous file and clock-free active Ping/Pong liveness probe.

### Test: `test_network_multi_node_concurrent_slice_writes_to_single_file`

**Setup**:
- Node 0 (Master) calls `OifsSession::open_network(&path, 20 MB, Some("127.0.0.1:0"))` — binds TCP on loopback ephemeral port, writes `.master` rendezvous file with `addr`, `pid`, `auth_token`.
- Creates `shared_matrix.dat` in root directory.
- Spawns 8 client threads (Node 1..8), each calling `open_network(&path, 0, None)` — reads `.master`, connects to published `addr`, authenticates with `auth_token`.

**Concurrency**:
- Each node writes a unique 4 KiB byte pattern to a **disjoint slice** of the same file (offset = `node_idx * 4096`).
- All writes use `CompressionMode::Never`.

**Verification**:
- New verification session reads full file.
- Asserts total length = 8 × 4096.
- Each 4096-byte slice matches the expected node's byte pattern.

### Test: `test_network_multi_node_concurrent_partitioned_records_in_single_file`

- 6 nodes, each writing 30 fixed 32-byte log records into dedicated slots (partitioned by `node_idx * 30 + seq`).
- Master reads full log, splits lines, asserts every slot matches expected format `NODE:{idx:02}:SEQ:{seq:04}:DATA:OK!======`.

### Test: `test_network_multi_node_concurrent_readers_and_writers_on_single_file`

- 4 writer threads, 4 reader threads, 20 iterations each.
- Writers update distinct 64-byte regions; readers read full 256-byte file.
- Asserts at least one concurrent read succeeds (no deadlock, no torn reads causing panic).

**Key Invariants**:
- Master election via atomic `create_new` on `.master` rendezvous file.
- Clock-free active Ping/Pong: client probes `addr` with `IpcRequest::Ping`, expects `Pong` within 500 ms before connecting.
- TCP listener binds loopback only by default (`127.0.0.1:0`); non-loopback requires explicit opt-in.
- Session secret (`auth_token`) exchanged via rendezvous file (mode 0600).
- Concurrent disjoint writes to same file are serialized by Master's `DiskManager` (single writer at a time via `sync_mutex`).
- Readers can run concurrently with writers (RwLock read access).

---

## Backward Compatibility

**File**: `tests/backward_compatibility_test.rs`

**Purpose**: Validates that a current build can open, read, and write a **v1 on-disk image** (pre-format-bump, no `format_version` on superblock, inodes stored as packed `bincode` records).

### Test: `test_v1_image_compatibility`

**Synthesis**:
- Creates a 10 MB zero-filled image manually.
- Writes a **V1 SuperBlock** (struct without `format_version`) to block 0 via `bincode::serialize`.
- Marks inode 0 and data block 1027 in bitmaps.
- Writes a **V1 Inode** (packed `bincode` with `V1FileType`, 12 direct blocks) at inode table block 3.

**Verification**:
- Opens with current `DiskManager::open`.
- Asserts superblock magic, block count match.
- Asserts `dm.superblock().is_legacy_inode_format()` returns `true`.
- Asserts `dm.superblock().inode_record_version() == SuperBlock::FORMAT_VERSION_LEGACY`.
- Lookup `hello.txt` (created in v1 image) and read content — **v1 inode decoding works**.
- Create new file `test_v1_append.txt`, write, read back — **new writes on v1 image stay in v1 format** (no mixed encodings).

**Key Invariants**:
- `decode_v1` path in `inode_format.rs` handles packed bincode + stale tail bytes.
- Superblock `format_version` field reads as 0 (zero-padded block 0) → detected as legacy.
- New inodes allocated on a v1 image are encoded as v1 (format version persists per-image).
- Encryption flags default to `false` for v1 images.

---

## Format Migration (v1 → v2 Inode Format)

**File**: `tests/format_migration_test.rs`

**Purpose**: Validates the **in-place migration** from v1 (bincode-packed inodes, no `format_version`) to v2 (fixed-size slots, `format_version = 2` on superblock). The test synthesizes genuine v1 images by rewriting a fresh v2 image, then exercises detection, mutation, reopen, and fsck.

### Helper: `downgrade_to_v1(path)`

1. Reads current v2 superblock, serializes a **V1 SuperBlock** (omits `format_version`), zero-pads rest of block 0.
2. For every allocated inode (per inode bitmap):
   - Decodes v2 record (`decode_v2`).
   - Serializes to v1 bincode.
   - Writes v1 payload into inode slot, **fills tail with `0x5A`** (emulates stale bytes left by v1 writers).
3. Writes full image back.

### Test: `test_v1_image_is_detected_and_stays_usable`

- Creates v2 image, adds `hello.txt`.
- Calls `downgrade_to_v1`.
- Opens: asserts `is_legacy_inode_format()`, `inode_record_version() == FORMAT_VERSION_LEGACY`.
- Reads `hello.txt` — content correct.
- Creates `new.txt`, writes — **image remains legacy** (no auto-migration on write).
- Asserts `is_legacy_inode_format()` still true after mutation.

### Test: `test_v1_image_survives_reopen`

- Creates v2 image with files `a`, `b`.
- Downgrades to v1.
- Opens, deletes `a`, creates `c` with data.
- Reopens: asserts `a` gone, `b` persists, `c` persists with correct data.
- **All mutations persisted in v1 layout** (no format upgrade on reopen).

### Test: `test_v1_image_passes_fsck`

- Creates v2 image with directory and 10 files.
- Downgrades to v1.
- Opens, runs `dm.verify_integrity()`.
- Asserts `report.is_clean` — **fsck understands v1 inode encoding**.

**Key Invariants**:
- Migration is **explicit, not automatic**: v1 images stay v1 unless user runs a migration tool (not yet implemented).
- `decode_v1` tolerates stale tail bytes (`0x5A` pattern).
- `encode_v2` used only for new images (format_version = 2).
- `SuperBlock::FORMAT_VERSION_LEGACY` constant distinguishes v1.
- Fsck validates both v1 and v2 inode layouts.

---

## Large Image & Bitmap Tests

**File**: `tests/large_image_bitmap_test.rs`

**Purpose**: Validates the **v3 layout** (multi-block data bitmaps) for images exceeding 128 MiB (32,768 blocks × 4 KiB = 128 MiB per bitmap block).

### Test: `test_large_image_gets_a_multi_block_data_bitmap`

- Creates 200 MiB image via `DiskManager::open`.
- Asserts `sb.is_v3_layout() == true`.
- Asserts `sb.data_bitmap_blocks() > 1` (200 MiB needs multiple bitmap blocks).
- Validates region layout:
  - `inode_bitmap_block == 1`
  - `data_bitmap_block == inode_bitmap_block + inode_bitmap_blocks()`
  - `inode_table_block == data_bitmap_block + data_bitmap_blocks()`
  - `data_block_start == inode_table_block + 1024` (inode table fixed at 1024 blocks)
- Runs `sb.validate(image.len())` — superblock self-validation passes.

### Test: `test_small_image_keeps_a_single_bitmap_block`

- Creates 10 MiB image.
- Asserts `data_bitmap_blocks() == 1`, `inode_bitmap_blocks() == 1`.
- Classic layout: block 1 = inode bitmap, 2 = data bitmap, 3 = inode table, 1027 = data start.

### Test: `test_multi_block_image_stays_consistent`

- Creates 200 MiB image.
- Creates 8 files, each 32 KiB, writes distinct byte patterns, reads back.
- Runs `dm.verify_integrity()` — asserts `fsck.is_clean`.
- Validates allocator, fsck, and defragmentation all handle multi-block bitmaps correctly.

**Key Invariants**:
- Layout version bumped to v3 (`format_version` concept extended) when data bitmap spans multiple blocks.
- Region offsets computed dynamically from bitmap block counts.
- Allocation, fsck, and defrag operate on multi-block bitmaps without special-casing.
- Sparse image creation is cheap (only metadata blocks allocated).

---

## Cross-Platform Test Considerations

| Aspect | Handling |
|--------|----------|
| **Dynamic library naming** | `ffi_c_cpp_test.rs` resolves `liboifs.so` / `liboifs.dylib` / `oifs.dll` via `cfg!(target_os = "...")`. |
| **Library load path** | `DYLD_LIBRARY_PATH` (macOS) / `LD_LIBRARY_PATH` (Linux) / `PATH` (Windows) set when executing compiled binaries. |
| **Compiler detection** | Tries `clang`/`clang++` first, falls back to `gcc`/`g++`; skips test if neither found. |
| **Temp directories** | `tempfile::tempdir()` / `std::env::temp_dir()` — portable across Unix and Windows. |
| **Path handling** | `PathBuf` and `to_str().unwrap()` — tests assume UTF-8 paths (reasonable for CI). |
| **TCP bind address** | Network tests use `127.0.0.1:0` (loopback ephemeral) — works everywhere; non-loopback requires explicit opt-in. |
| **UDS socket paths** | Local mode uses `$XDG_RUNTIME_DIR/oifs` or `/tmp/oifs-<uid>/` — Unix-only; Windows uses named pipes (not tested in these suites). |
| **File permissions** | Socket/token/rendezvous files created mode 0600/0700 — Unix semantics; Windows ignored. |
| **Endianness** | On-disk format is little-endian; `endianness_and_portability_test.rs` covers cross-endian validation separately. |

---

## Running the Integration Tests

```bash
# All integration tests
cargo test --test cli_concurrent_process_test
cargo test --test ffi_c_cpp_test
cargo test --test mcp_server_test
cargo test --test network_single_file_sync_test
cargo test --test backward_compatibility_test
cargo test --test format_migration_test
cargo test --test large_image_bitmap_test
cargo test --test integration_test

# Or all at once
cargo test
```

**Note**: FFI tests require a C/C++ compiler (`clang`/`gcc` or `clang++`/`g++`) and the `liboifs` dynamic library (built via `cargo build --lib`). Network tests bind to localhost TCP ports. MCP tests spawn subprocesses with piped stdio.

---

## Related Architecture Pages

- [Concurrency & Session Model](../architecture/concurrency_and_session.md) — Session modes, master election, IPC framing.
- [C FFI Interface](../architecture/ffi_interface.md) — Opaque handles, callbacks, version handshake, encryption.
- [MCP Server Integration](../architecture/mcp_server.md) — `oifs_mcp` architecture, tool router, sandbox model.
- [Testing and Verification](../operations/testing_and_verification.md) — Full test strategy including Shuttle, Bolero, Kani.
