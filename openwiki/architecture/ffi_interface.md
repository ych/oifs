---
type: architecture
title: C FFI Interface
description: How OIFS exposes its storage engine as a standard C shared library (liboifs) through an opaque handle model, callback-driven directory iteration, zero-copy offset reads, pluggable I/O backend configuration, version handshake, and thread-safe error reporting.
tags: [ffi, c-api, shared-library, handles, callbacks, abi, io-engine, no-mangle, versioning, encryption]
sources:
  - id: openwiki-source-c9e5b32aad7cafdb095c81a4
    resource: repo://src/ffi.rs
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---

# C FFI Interface

The C Foreign Function Interface ([`src/ffi.rs`](repo://src/ffi.rs), [`include/oifs.h`](repo://include/oifs.h)) exports the OIFS storage engine as a standard C-compatible shared library (`liboifs.so` on Linux, `liboifs.dylib` on macOS, `oifs.dll` on Windows).

It bridges the idiomatic Rust engine (`DiskManager`, `OifsSession`, `RwLock`, `IoEngine`) to C/C++, Python, Go, and other host runtimes:
- Manages raw memory boundaries using opaque boxed pointers.
- Enforces null-pointer and UTF-8 safety checks on all foreign inputs.
- Preserves detailed error messages per handle via `oifs_last_error`.
- Exports unmangled C symbols via `#[unsafe(no_mangle)] pub extern "C" fn`.
- Exposes runtime I/O engine configuration (`oifs_set_io_backend`, `oifs_get_io_backend`).
- Provides encryption support via `oifs_open_with_password` and `oifs_get_or_open_with_password`.
- Supports session modes (standalone, local IPC, network) through session registry functions.

## The Opaque Handle Model

All operations require an opaque pointer to `OIFSHandle` ([`src/ffi.rs#L11-L25`](repo://src/ffi.rs#L11-L25)):

```rust
pub struct OIFSHandle {
    pub dm: DiskManager,
    pub last_error: std::sync::Mutex<Option<String>>,
}
```

In C ([`include/oifs.h#L52`](repo://include/oifs.h#L52)):
```c
typedef struct OIFSHandle OIFSHandle;
```

- **Handle-Isolated Errors**: Rather than using a global `errno` that risks race conditions in multithreaded host programs, each `OIFSHandle` maintains its own `last_error: std::sync::Mutex<Option<String>>`. Successful calls lock the mutex and set the Option to `None`, while errors lock the mutex and set the Option to `Some(String)`. The error string is retrieved via `oifs_last_error`.
- **Underlying Engine**: `OIFSHandle` embeds a `DiskManager` ([`src/disk.rs`](repo://src/disk.rs)). Because `DiskManager` wraps its inner state in `Arc<RwLock<DiskManagerInner>>`, cloning is lightweight and shares the underlying memory maps, cache layers, and locks.

## Handle Lifecycle: Open, Session Reuse, and Close

OIFS provides four entry points for instantiating handles, accommodating direct disk access or process-wide session caching:

### 1. Direct Disk Access (Standalone Mode)
- `oifs_open` ([`src/ffi.rs#L75-L106`](repo://src/ffi.rs#L75-L106)):
  ```c
  OIFSHandle* oifs_open(const char* path, uint64_t size);
  ```
  Opens an existing unencrypted image or creates a new image of `size` bytes. Returns a raw pointer `*mut OIFSHandle` or `NULL` on error.
- `oifs_open_with_password` ([`src/ffi.rs#L109-L165`](repo://src/ffi.rs#L109-L165)):
  ```c
  OIFSHandle* oifs_open_with_password(const char* path, uint64_t size, const char* password);
  ```
  Opens or creates an encrypted OIFS image. The passphrase is used with Argon2id and the superblock salt to derive the 256-bit AEAD key ([`src/encryption.rs#L124`](repo://src/encryption.rs#L124)). If the file doesn't exist and a non-empty password is provided, it creates a new encrypted image; otherwise it opens an existing image (encrypted or unencrypted) with optional password verification.

### 2. Session Registry Reuse (IPC/Local/Network Modes)
- `oifs_get_or_open` ([`src/ffi.rs#L168-L195`](repo://src/ffi.rs#L168-L195)):
  ```c
  OIFSHandle* oifs_get_or_open(const char* path, uint64_t size);
  ```
  Consults the process-wide session registry (`OifsSession::get_or_open`). If the session resolves to a direct master instance, it returns an `OIFSHandle` sharing the underlying `DiskManager`. This enables multi-process access via the IPC layer.
- `oifs_get_or_open_with_password` ([`src/ffi.rs#L198-L252`](repo://src/ffi.rs#L198-L252)):
  ```c
  OIFSHandle* oifs_get_or_open_with_password(const char* path, uint64_t size, const char* password);
  ```
  Performs session registry lookup with password validation for encrypted images. Supports both local and network session modes.

### 3. Handle Destruction
- `oifs_close` ([`src/ffi.rs#L255-L263`](repo://src/ffi.rs#L255-L263)):
  ```c
  void oifs_close(OIFSHandle* handle);
  ```
  Reclaims the heap allocation via `Box::from_raw(handle)` and drops the handle. Dropping the embedded `DiskManager` decrements reference counts and triggers `DiskManagerInner::drop()`, ensuring `mmap.flush()` commits all changes to disk. Passing `NULL` is a safe no-op.

## Filesystem Operations

### Directory Iteration via C Callback

Because C lacks standard iterators, `oifs_ls` ([`src/ffi.rs#L266-L331`](repo://src/ffi.rs#L266-L331)) provides a C callback mechanism:

```c
typedef void (*ListCallback)(const char* name, uint64_t size, uint64_t mtime, void* user_data);
int32_t oifs_ls(OIFSHandle* handle, ListCallback cb, void* user_data);
```

- Traverses root directory entries using `DirectoryIterator` ([`src/directory.rs`](repo://src/directory.rs)).
- For each entry, invokes `cb(c_name.as_ptr(), inode.size, inode.modified_at, user_data)`.
- The caller passes arbitrary state via `user_data` without requiring global variables.
- Returns `0` on success or `-1` on error (check `oifs_last_error` for details).

### File and Directory CRUD

- `oifs_create_file` ([`src/ffi.rs#L334-L390`](repo://src/ffi.rs#L334-L390)): Creates a new file at path, returns `0` on success, `-1` on error.
- `oifs_create_or_open_file` ([`src/ffi.rs#L393-L448`](repo://src/ffi.rs#L393-L448)): Creates file if missing, or opens existing file.
- `oifs_delete_file` ([`src/ffi.rs#L451-L505`](repo://src/ffi.rs#L451-L505)): Deletes a file at path.
- `oifs_mkdir` ([`src/ffi.rs#L611-L658`](repo://src/ffi.rs#L611-L658)): Creates a directory at path.

All path arguments are validated for null pointers and UTF-8 correctness. On UTF-8 conversion failure, the function returns `-1` and sets `last_error` to "Invalid UTF-8 filename".

## File I/O

### Offset Read (Zero-Copy)

`oifs_read_at` ([`src/ffi.rs#L508-L571`](repo://src/ffi.rs#L508-L571)) performs chunked/direct read starting from an arbitrary byte offset:

```c
int64_t oifs_read_at(
    OIFSHandle* handle,
    const char* filename,
    uint64_t offset,
    uint8_t* buf,
    uint64_t buf_size
);
```

- For uncompressed files, it directly copies from `mmap` blocks without heap allocations (zero-copy).
- Constructs a mutable slice via `std::slice::from_raw_parts_mut(buf, buf_size as usize)` and passes it to `dm.read_at()`.
- Returns bytes read on success (as `int64_t`), or `-1` on error.
- `oifs_read_file` ([`src/ffi.rs#L574-L576`](repo://src/ffi.rs#L574-L576)) delegates to `oifs_read_at` with offset `0`.

### Sequential Write (Auto-Compression)

`oifs_write_file` ([`src/ffi.rs#L579-L657`](repo://src/ffi.rs#L579-L657)) writes data to a file at offset 0 with automatic compression:

```c
int32_t oifs_write_file(
    OIFSHandle* handle,
    const char* filename,
    const uint8_t* buf,
    uint64_t buf_size
);
```

- Resolves or creates the file, then calls `dm.write_data(inode_id, 0, data, CompressionMode::Auto)`.
- Returns `0` on success, `-1` on error.
- A null buffer with non-zero size is treated as a caller error.

### Extended Write with Policy Control

`oifs_write_file_with_policy` ([`src/ffi.rs#L661-L742`](repo://src/ffi.rs#L661-L742)) supports write policies, offsets, and custom zstd compression level:

```c
int32_t oifs_write_file_with_policy(
    OIFSHandle* handle,
    const char* filename,
    uint64_t offset,
    const uint8_t* buf,
    uint64_t buf_size,
    uint32_t policy_flags,
    int32_t zstd_level
);
```

Policy flags (from [`include/oifs.h#L64-L71`](repo://include/oifs.h#L64-L71)):
- `OIFS_WRITE_POLICY_DEFAULT` (0x00): Auto / Full-file stream compression (default)
- `OIFS_WRITE_POLICY_RAW` (0x01): Never compress (Raw)
- `OIFS_WRITE_POLICY_STREAM` (0x02): Stream compression (with custom zstd_level)
- `OIFS_WRITE_POLICY_SEEKABLE_64K` (0x08): Seekable 64KB chunked compression

### Truncate File

`oifs_truncate_file` ([`src/ffi.rs#L904-L953`](repo://src/ffi.rs#L904-L953)) truncates or extends a file:

```c
int32_t oifs_truncate_file(OIFSHandle* handle, const char* filename, uint64_t new_size);
```

- If `new_size < current size`, trailing bytes and unused blocks are discarded and freed.
- If `new_size > current size`, the file is extended (sparse for raw files).

## Error Handling

### Per-Handle Error Strings

`oifs_last_error` ([`src/ffi.rs#L745-L791`](repo://src/ffi.rs#L745-L791)) retrieves the last error message for a handle:

```c
int32_t oifs_last_error(OIFSHandle* handle, char* buf, uint32_t buf_size);
```

- Copies the null-terminated error string into the provided buffer.
- Guarantees null-termination even if the buffer is too small (truncates and writes final `\0`).
- Returns `0` on success, `-1` on invalid arguments (null handle, null buffer, zero buffer size).
- Sanitizes internal strings by replacing embedded nulls with spaces to prevent premature truncation.
- If no error is set, returns "No error".

### Null Pointer and UTF-8 Safety

All FFI functions validate against null pointers:
- Constructor functions (`oifs_open`, `oifs_open_with_password`, `oifs_get_or_open`, `oifs_get_or_open_with_password`) return `NULL` without setting `last_error` (no handle exists yet).
- Other functions set `last_error` and return `-1` on null handle or null path arguments.
- Non-UTF-8 path strings cause the function to return `-1` with `last_error` set to "Invalid UTF-8 filename".

## I/O Engine Backend Configuration

Runtime I/O engine selection is exposed to foreign callers ([`src/ffi.rs#L794-L825`](repo://src/ffi.rs#L794-L825)):

```c
#define OIFS_IO_BACKEND_MMAP     0
#define OIFS_IO_BACKEND_PREAD    1
#define OIFS_IO_BACKEND_IO_URING 2

int32_t oifs_set_io_backend(OIFSHandle* handle, uint8_t backend);
int32_t oifs_get_io_backend(OIFSHandle* handle);
```

- `oifs_set_io_backend` converts the `uint8_t` discriminant to `IoBackend`, applies it via `dm.set_io_backend()`, and returns the effective backend (after automatic fallback, e.g., `IoUring` falls back to `Pread` on non-Linux).
- `oifs_get_io_backend` returns the currently effective backend.

## Dynamic Library Versioning & Compatibility Handshake

The C FFI exposes version handshake functions ([`src/ffi.rs#L828-L901`](repo://src/ffi.rs#L828-L901)) to allow host programs to verify compatibility with the loaded dynamic library:

```c
#define OIFS_VERSION_MAJOR 1
#define OIFS_VERSION_MINOR 2
#define OIFS_VERSION_PATCH 0
#define OIFS_VERSION_STRING "1.2.0"
#define OIFS_VERSION_CODE (((uint64_t)OIFS_VERSION_MAJOR << 32) | ((uint64_t)OIFS_VERSION_MINOR << 16) | (uint64_t)OIFS_VERSION_PATCH)

#define OIFS_VERSION_COMPAT_OK       0  // Exact expected version: fully verified, clean
#define OIFS_VERSION_COMPAT_WARN     1  // Newer version loaded: permitted, but emits warning
#define OIFS_VERSION_COMPAT_ERR     -1  // Older version loaded: incompatible, error out

uint32_t oifs_version_major(void);
uint32_t oifs_version_minor(void);
uint32_t oifs_version_patch(void);
uint64_t oifs_version_code(void);
const char* oifs_version_string(void);
int32_t oifs_check_version(uint32_t req_major, uint32_t req_minor, uint32_t req_patch);
int32_t oifs_loaded_path(char* buf, size_t buf_size);
```

- `oifs_check_version` implements the policy: "Only block older versions; do NOT block newer versions."
  - Returns `OIFS_VERSION_COMPAT_OK` (0): exact version match.
  - Returns `OIFS_VERSION_COMPAT_WARN` (1): newer library loaded (warning, allowed).
  - Returns `OIFS_VERSION_COMPAT_ERR` (-1): older library loaded (blocked).
- `oifs_loaded_path` retrieves the absolute filesystem path from which the dynamic library was loaded (using `dladdr` on Unix).

### Convenience Macro

```c
#define OIFS_CHECK_VERSION() \
    oifs_check_version(OIFS_VERSION_MAJOR, OIFS_VERSION_MINOR, OIFS_VERSION_PATCH)
```

Usage pattern in host C/C++ code:
```c
int status = OIFS_CHECK_VERSION();
if (status == OIFS_VERSION_COMPAT_ERR) {
    char path[512] = {0};
    oifs_loaded_path(path, sizeof(path));
    fprintf(stderr, "FATAL [liboifs]: Outdated library '%s' (v%s) loaded! Expected >= v%s.\n",
            path, oifs_version_string(), OIFS_VERSION_STRING);
    abort();
} else if (status == OIFS_VERSION_COMPAT_WARN) {
    fprintf(stderr, "WARN [liboifs]: Newer library (v%s) loaded than compiled with (v%s).\n",
            oifs_version_string(), OIFS_VERSION_STRING);
}
```

## Encryption Helpers for Language Bindings

The FFI exposes encryption through password-based open functions:
- `oifs_open_with_password`: Creates or opens an encrypted image using Argon2id key derivation from the provided password and superblock salt.
- `oifs_get_or_open_with_password`: Session-registry variant that reuses existing sessions with password validation.

Internally, `DiskManager::derive_key` ([`src/encryption.rs#L124`](repo://src/encryption.rs#L124)) performs Argon2id key derivation, and `EncryptionKey` ([`src/encryption.rs#L65`](repo://src/encryption.rs#L65)) splits the 256-bit master key into subkeys for payload encryption, filename SIV, filename cipher, and IPC authentication.

## Thread-Safety Guarantees

- **Handle Independence**: Each `OIFSHandle` contains its own `last_error` mutex, eliminating cross-handle error interference.
- **Shared DiskManager**: Multiple handles created via `oifs_get_or_open` share the same `DiskManager` (via `Arc`), and all operations are protected by the internal `RwLock`.
- **Panic Safety**: Every `extern "C"` function wraps its body in `std::panic::catch_unwind` to prevent panics from unwinding across the FFI boundary (which would be UB). Panics are caught, `last_error` is set to "Panic occurred during ...", and the function returns an error code.
- **Reentrancy**: The C callback in `oifs_ls` is invoked while holding the `DiskManager` read lock; the callback must not call back into the FFI on the same handle (would deadlock).

## Callback-Based Async Read/Write API

> **Note**: The current FFI exposes *synchronous* read/write operations (`oifs_read_at`, `oifs_read_file`, `oifs_write_file`, `oifs_write_file_with_policy`). The underlying `DiskManager` supports asynchronous `msync` via `flush_async` and `flush_async_range` ([`src/disk.rs#L5467-L5487`](repo://src/disk.rs#L5467-L5487)), but these are not yet exposed through the C FFI. Language bindings requiring async I/O should use thread pools or the engine's `RangeAsync` durability mode with explicit `oifs_flush` calls.

## FfiError Mapping

The Rust side uses `DiskManagerError` ([`src/disk.rs#L287`](repo://src/disk.rs#L287)) which is converted to boxed `dyn std::error::Error` and stored as string messages in `last_error`. The FFI layer does not expose numeric error codes beyond `0` (success) and `-1` (failure); detailed diagnostics require calling `oifs_last_error`.

## Representative Tests

The FFI is validated through:
- **Native C tests**: [`tests/ffi_c_cpp_test.rs`](repo://tests/ffi_c_cpp_test.rs) compiles and runs a C program against the shared library, exercising version handshake, open/write/read/ls/delete, encryption with good/bad passwords, and I/O backend switching.
- **C++ tests**: Same file includes C++ RAII (`std::unique_ptr<OIFSHandle>`), lambda callbacks, `std::vector<uint8_t>` buffers, and multi-threaded concurrent access.
- **Robustness tests**: [`tests/ffi_robustness_test.rs`](repo://tests/ffi_robustness_test.rs) validates null-pointer handling, UTF-8 rejection, error buffer boundary checks, empty/zero-size I/O, multi-threaded session reuse, and cross-block offset reads.
- **Extended tests**: [`tests/ffi_extended_test.rs`](repo://tests/ffi_extended_test.rs) validates version API, encryption workflow, and `oifs_loaded_path`.
- **Rust FFI tests**: [`tests/ffi_test.rs`](repo://tests/ffi_test.rs) exercises the FFI functions from Rust via the generated bindings.

The Kani formal verification proof ([`src/ffi.rs#L935-L953`](repo://src/ffi.rs#L935-L953)) proves `oifs_check_version` policy soundness for all possible symbolic inputs.
