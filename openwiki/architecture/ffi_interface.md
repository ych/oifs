---
type: "Reference"
title: "C FFI Interface"
openwiki_generated: true
verified:
  - by: openwiki/0.9.0
    at: 2026-11-22T10:29:12.696Z
sources:
  - id: openwiki-source-c9e5b32aad7cafdb095c81a4
    resource: repo://src/ffi.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-22T10:29:12.696Z" }
---


# C FFI Interface

The C Foreign Function Interface ([`src/ffi.rs`](repo://src/ffi.rs), [`include/oifs.h`](repo://include/oifs.h)) exports the OIFS storage engine as a standard C-compatible shared library (`liboifs.so` on Linux, `liboifs.dylib` on macOS, `oifs.dll` on Windows). A C++17/20 RAII wrapper header ([`include/oifs.hpp`](repo://include/oifs.hpp)) adds move-only resource management, span/byte-view helpers, and optional coroutine awaiters.

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
    pub fuse_state: std::sync::OnceLock<crate::fuse::OifsFuseState>,
}
```

In C ([`include/oifs.h#L52`](repo://include/oifs.h#L52)):
```c
typedef struct OIFSHandle OIFSHandle;
```

- **Handle-Isolated Errors**: Rather than using a global `errno` that risks race conditions in multithreaded host programs, each `OIFSHandle` maintains its own `last_error: std::sync::Mutex<Option<String>>`. Successful calls lock the mutex and set the Option to `None`, while errors lock the mutex and set the Option to `Some(String)`. The error string is retrieved via `oifs_last_error`.
- **FUSE State**: The `fuse_state: OnceLock<OifsFuseState>` lazily initializes the FUSE kernel protocol dispatcher (ABI v7.28) on first use, enabling `oifs_fuse_dispatch_frame` without per-call allocation.
- **Underlying Engine**: `OIFSHandle` embeds a `DiskManager` ([`src/disk.rs`](repo://src/disk.rs)). Because `DiskManager` wraps its inner state in `Arc<RwLock<DiskManagerInner>>`, cloning is lightweight and shares the underlying memory maps, cache layers, and locks.

## Handle Lifecycle: Open, Session Reuse, and Close

OIFS provides multiple entry points for instantiating handles, accommodating direct disk access or process-wide session caching:

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
- `oifs_open_with_options` ([`src/ffi.rs#L901-L967`](repo://src/ffi.rs#L901-L967)):
  ```c
  OIFSHandle* oifs_open_with_options(const char* path, uint64_t size, const char* password,
                                     uint8_t storage_mode, uint8_t journaled);
  ```
  Explicitly selects `StorageMode::Mmap` (0) or `StorageMode::FileIo` (1) and enables/disables metadata WAL journaling.
- `oifs_open_read_only` ([`src/ffi.rs#L970-L1010`](repo://src/ffi.rs#L970-L1010)):
  ```c
  OIFSHandle* oifs_open_read_only(const char* path, const char* password, uint8_t storage_mode);
  ```
  Acquires a shared OS read lock (`F_RDLCK`), never opens the backing file for writing, and supports multiple concurrent read-only readers on `0444` images.

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

Because C lacks standard iterators, `oifs_ls` ([`src/ffi.rs#L266-L331`](repo://src/ffi.rs#L266-L331)) and `oifs_ls_dir` ([`src/ffi.rs#L2401-L2476`](repo://src/ffi.rs#L2401-L2476)) provide a C callback mechanism:

```c
typedef void (*ListCallback)(const char* name, uint64_t size, uint64_t mtime, void* user_data);
int32_t oifs_ls(OIFSHandle* handle, ListCallback cb, void* user_data);
int32_t oifs_ls_dir(OIFSHandle* handle, const char* path, ListCallback cb, void* user_data);
```

- Traverses root directory entries or a subdirectory using `DirectoryIterator` ([`src/directory.rs`](repo://src/directory.rs)).
- For each entry, invokes `cb(c_name.as_ptr(), inode.size, inode.modified_at, user_data)`.
- The caller passes arbitrary state via `user_data` without requiring global variables.
- Returns `0` on success or `-1` on error (check `oifs_last_error` for details).

### File and Directory CRUD

- `oifs_create_file` ([`src/ffi.rs#L334-L390`](repo://src/ffi.rs#L334-L390)): Creates a new file at path, returns `0` on success, `-1` on error.
- `oifs_create_or_open_file` ([`src/ffi.rs#L393-L450`](repo://src/ffi.rs#L393-L450)): Creates a file or opens an existing one atomically.
- `oifs_delete_file` ([`src/ffi.rs#L451-L505`](repo://src/ffi.rs#L451-L505)): Deletes a file.
- `oifs_mkdir` ([`src/ffi.rs#L601-L657`](repo://src/ffi.rs#L601-L657)): Creates a directory.
- `oifs_rename` ([`src/ffi.rs#L2101-L2163`](repo://src/ffi.rs#L2101-L2163)): Atomically renames or moves a file or directory within the filesystem.
- `oifs_truncate_file` ([`src/ffi.rs#L938-L980`](repo://src/ffi.rs#L938-L980)): Truncates or extends a file to the specified size in bytes.

### File Metadata
- `oifs_stat` ([`src/ffi.rs#L2479-L2537`](repo://src/ffi.rs#L2479-L2537)): Retrieves inode metadata into `OIFSFileStat` (size, compressed size, timestamps, encryption flag, seekable-64k flag, filter metadata).
- `oifs_exists` ([`src/ffi.rs#L2540-L2572`](repo://src/ffi.rs#L2540-L2572)): Checks whether a path exists (returns `1` yes, `0` no, `-1` error).
- `oifs_resolve_path` ([`src/ffi.rs#L1012-L1053`](repo://src/ffi.rs#L1012-L1053)): Resolves a filesystem path to its `inode_id` (`>= 0` on success).

## High-Performance Direct I/O

### Synchronous Reads

`oifs_read_at` ([`src/ffi.rs#L508-L571`](repo://src/ffi.rs#L508-L571)) performs a positional read starting from an arbitrary byte offset. For uncompressed files in `StorageMode::Mmap`, it directly copies from mmap blocks without heap allocations. Returns bytes read on success, or `-1` on error.

```c
int64_t oifs_read_at(OIFSHandle* handle, const char* filename, uint64_t offset,
                     uint8_t* buf, uint64_t buf_size);
```

- `oifs_read_file` ([`src/ffi.rs#L574-L576`](repo://src/ffi.rs#L574-L576)) delegates to `oifs_read_at` with offset 0.
- `oifs_read_at_inode` ([`src/ffi.rs#L1190-L1239`](repo://src/ffi.rs#L1190-L1239)) reads by `inode_id` without path resolution overhead.

### Scatter-Gather Batch Read

`oifs_read_at_batch` ([`src/ffi.rs#L1242-L1319`](repo://src/ffi.rs#L1242-L1319)) executes a batch of positional reads under a single read lock and single I/O engine submission. Populates `items[i].result` with the number of bytes read (`>= 0`) or `-1` on per-item error. Returns `0` if all items succeeded, or `-1` if any item failed.

```c
typedef struct {
    uint64_t inode_id;
    uint64_t offset;
    uint8_t* buf;
    uint64_t buf_size;
    int64_t result;
} OIFSReadBatchItem;

int32_t oifs_read_at_batch(OIFSHandle* handle, OIFSReadBatchItem* items, size_t count);
```

### Synchronous Writes

- `oifs_write_file` ([`src/ffi.rs#L579-L657`](repo://src/ffi.rs#L579-L657)): Writes data to a file at offset 0 with automatic compression (creates file if it doesn't exist).
- `oifs_write_file_with_policy` ([`src/ffi.rs#L660-L798`](repo://src/ffi.rs#L660-L798)): Extended write interface supporting write policies, offsets, and custom zstd compression level:

```c
#define OIFS_WRITE_POLICY_DEFAULT       0x00  // Auto / Full-file stream compression
#define OIFS_WRITE_POLICY_RAW           0x01  // Never compress
#define OIFS_WRITE_POLICY_STREAM        0x02  // Stream compression (custom zstd_level)
#define OIFS_WRITE_POLICY_SEEKABLE_64K  0x08  // Seekable 64KB chunked compression

int32_t oifs_write_file_with_policy(OIFSHandle* handle, const char* filename,
                                    uint64_t offset, const uint8_t* buf, uint64_t buf_size,
                                    uint32_t policy_flags, int32_t zstd_level);
```

- `oifs_create_file_with_data` ([`src/ffi.rs#L1056-L1187`](repo://src/ffi.rs#L1056-L1187)): Atomically creates a new file and writes its initial payload in a single transaction. Returns the new `inode_id` (`>= 0`) on success, or `-1` on error.

### SIMD Numeric Pre-Compression Filters

Filter pipelines (Delta, ByteShuffle, BitShuffle, TruncPrecision) can be applied before compression ([`src/filters.rs`](repo://src/filters.rs)):

```c
typedef struct {
    uint8_t typesize;   // Element size in bytes (1, 2, 4, 8; 0 = disabled)
    uint8_t delta;      // Non-zero to enable first-order delta encoding
    uint8_t shuffle;    // Non-zero to enable byte shuffle
    uint8_t bitshuffle; // Non-zero to enable bit shuffle
} OIFSFilterConfig;

int64_t oifs_create_file_with_data_filtered(OIFSHandle* handle, const char* path,
                                            const uint8_t* buf, uint64_t buf_size,
                                            uint32_t policy_flags, int32_t zstd_level,
                                            const OIFSFilterConfig* filter_cfg);

int32_t oifs_write_file_with_filters(OIFSHandle* handle, const char* path, uint64_t offset,
                                     const uint8_t* buf, uint64_t buf_size,
                                     uint32_t policy_flags, int32_t zstd_level,
                                     const OIFSFilterConfig* filter_cfg);
```

Pass `NULL` for `filter_cfg` to disable filters.

## I/O Engine Backend Configuration

Runtime pluggable I/O backend selection ([`src/ffi.rs#L758-L798`](repo://src/ffi.rs#L758-L798)):

```c
#define OIFS_IO_BACKEND_MMAP     0
#define OIFS_IO_BACKEND_PREAD    1
#define OIFS_IO_BACKEND_IO_URING 2

int32_t oifs_set_io_backend(OIFSHandle* handle, uint8_t backend);
int32_t oifs_get_io_backend(OIFSHandle* handle);
```

- `Mmap`: Memory-mapped container (default).
- `Pread`: Pure positional `pread`/`pwrite` (zero mmap).
- `IoUring`: Linux `io_uring` (falls back to `Pread` if unsupported).

Durability modes ([`src/ffi.rs#L983-L1010`](repo://src/ffi.rs#L983-L1010)):
```c
#define OIFS_DURABILITY_LAZY        0  // OS page-cache writeback (default)
#define OIFS_DURABILITY_RANGE_ASYNC 1  // Batched dirty-range async flush
#define OIFS_DURABILITY_STRICT      2  // Synchronous msync / sync_data barriers

int32_t oifs_set_durability_mode(OIFSHandle* handle, uint8_t mode);
```

`oifs_flush` and `oifs_flush_async` commit pending data and WAL transactions.

## Shared Read Session & Pinned File Handle (Amortized RwLock)

For read-heavy workloads, OIFS provides a session model that amortizes the outer `RwLock` acquisition and inode lookups across multiple reads:

```c
typedef struct OIFSReadSession OIFSReadSession;
typedef struct OIFSFileHandle OIFSFileHandle;

OIFSReadSession* oifs_begin_read(OIFSHandle* handle);
void oifs_end_read(OIFSReadSession* session);

OIFSFileHandle* oifs_session_open_file(const OIFSReadSession* session, const char* path);
OIFSFileHandle* oifs_session_open_inode(const OIFSReadSession* session, uint64_t inode_id);
void oifs_file_close(OIFSFileHandle* file);

uint64_t oifs_file_size(const OIFSFileHandle* file);
uint64_t oifs_file_inode_id(const OIFSFileHandle* file);

int64_t oifs_file_read_at(const OIFSFileHandle* file, uint64_t offset,
                          uint8_t* buf, uint64_t buf_size);
```

Internally, `OIFSReadSession` holds an `Arc<FfiSharedSession>` containing a `DiskManager::ReadSession<'static>` (the read lock guard) and the owning `DiskManager`. `OIFSFileHandle` holds an `Arc` to the same shared session and a `FileReadHandle` borrowing from it, guaranteeing **zero use-after-free** even if the C caller destroys the `OIFSReadSession` before the `OIFSFileHandle` or a pinned mmap slice.

## True Zero-Copy mmap Slice

`oifs_file_mmap_slice` ([`src/ffi.rs#L1485-L1548`](repo://src/ffi.rs#L1485-L1548)) provides a true zero-copy view into the memory-mapped image for uncompressed contiguous files:

```c
int32_t oifs_file_mmap_slice(const OIFSFileHandle* file, uint64_t offset, uint64_t max_len,
                             const uint8_t** out_ptr, uint64_t* out_len);
```

**Returns:**
- `0`: Zero-copy succeeded; `*out_ptr` points directly to the mapped file bytes (valid while the `OIFSFileHandle` or its parent `OIFSReadSession` remains open) and `*out_len` is the clamped byte count.
- `1`: Zero-copy unavailable (e.g., `StorageMode::FileIo`, compressed/encrypted file, sparse hole, or non-contiguous block boundary); caller should fall back to `oifs_file_read_at`.
- `-1`: Error or null pointer argument.

## Asynchronous I/O & Group-Commit Write Engine

OIFS provides a Future-based async API with an **Inline Fast-Path** and **Group Commit** for writes.

### Future API

```c
typedef struct OIFSAsyncFuture OIFSAsyncFuture;

OIFSAsyncFuture* oifs_read_at_async(OIFSHandle* handle, uint64_t inode_id, uint64_t offset,
                                    uint8_t* buf, uint64_t buf_size);
OIFSAsyncFuture* oifs_create_file_with_data_async(OIFSHandle* handle, const char* path,
                                                  const uint8_t* buf, uint64_t buf_size,
                                                  uint32_t policy_flags, int32_t zstd_level);
OIFSAsyncFuture* oifs_write_file_async(OIFSHandle* handle, const char* path, uint64_t offset,
                                       const uint8_t* buf, uint64_t buf_size,
                                       uint32_t policy_flags, int32_t zstd_level);
OIFSAsyncFuture* oifs_flush_async(OIFSHandle* handle);

int32_t oifs_async_poll(const OIFSAsyncFuture* fut);    // 1 = ready, 0 = pending, -1 = invalid
int64_t oifs_async_wait(OIFSAsyncFuture* fut);          // Blocks, returns result or -1
int32_t oifs_async_last_error(const OIFSAsyncFuture* fut, char* buf, uint32_t buf_size);
void oifs_async_free(OIFSAsyncFuture* fut);             // Drop barrier: waits for in-flight write to buf
```

**Key Properties:**
- **Inline Fast-Path**: Hot cache/mmap reads complete immediately on the calling thread (`oifs_async_poll == 1`) with zero thread context switch.
- **Group Commit**: Concurrent async writes under `OIFS_DURABILITY_STRICT` share a single physical sync barrier per batch.
- **Use-After-Free Safe**: `oifs_async_free` waits for any in-flight background read writing into the caller's buffer to finish before returning.

### Callback API

For environments that prefer callbacks over futures:

```c
typedef void (*OifsAsyncReadCallback)(int64_t status_or_len, const uint8_t* data, size_t data_len, void* user_data);
typedef void (*OifsAsyncStatusCallback)(int64_t status_or_inode, void* user_data);

int32_t oifs_read_at_async_cb(OIFSHandle* handle, uint64_t inode_id, uint64_t offset, uint64_t len,
                              OifsAsyncReadCallback cb, void* user_data);
int32_t oifs_create_file_with_data_async_cb(OIFSHandle* handle, const char* path,
                                            const uint8_t* buf, uint64_t buf_size,
                                            uint32_t policy_flags, int32_t zstd_level,
                                            OifsAsyncStatusCallback cb, void* user_data);
```

The inline fast-path is also probed inside the callback variant.

## FUSE Kernel Protocol (ABI v7.28) Frame Dispatcher

`oifs_fuse_dispatch_frame` ([`src/ffi.rs#L1863-L1938`](repo://src/ffi.rs#L1863-L1938)) implements a pure-Rust FUSE kernel protocol parser and dispatcher with **zero external C dependencies**:

```c
int64_t oifs_fuse_dispatch_frame(OIFSHandle* handle, const uint8_t* req_buf, size_t req_len,
                                 uint8_t* resp_buf, size_t resp_cap);
```

- Parses raw FUSE request frames (`fuse_in_header` + payload) against **ABI v7.28** (major 7, minor 28).
- Supports **dual Linux & macOS ABI**:
  - Linux: 88-byte `FuseAttr` (standard), `FuseEntryOut` 128B, `FuseAttrOut` 104B.
  - macOS/Darwin: 104-byte `FuseAttr` (with `crtime`/`crtimensec`/`flags`), `FuseEntryOut` 144B, `FuseAttrOut` 120B.
- **Single-FD container**: Any number of open FUSE file handles (`fh`) inside `FuseHandleTable` multiplex over the single underlying OIFS image descriptor.
- **Strict read-only enforcement**: When mounted read-only (via `DiskManager::is_read_only()` or `FuseMountOptions::read_only`), every mutating operation returns `-EROFS` with zero disk writes.
- **Panic-free frame dispatch**: Validates every header and payload boundary; never panics on truncated or malformed input.

Open FUSE file/directory handles (`fh`) persist across successive calls on the same `OIFSHandle`.

## Cross-Image Operations

### Whole-Image CoW Clone
```c
#define OIFS_CLONE_METHOD_APFS_CLONEFILE  0
#define OIFS_CLONE_METHOD_LINUX_FICLONE   1
#define OIFS_CLONE_METHOD_STANDARD_COPY   2

int32_t oifs_clone_image(const char* src_path, const char* dst_path, uint8_t* out_method);
```
Clones a whole OIFS `.img` container using the host OS's fastest CoW primitive (`clonefile` on macOS APFS, `ioctl(FICLONE)` on Linux, or standard copy fallback).

### 3-Tier Fast Transfer
```c
#define OIFS_TRANSFER_METHOD_REFLINK           0
#define OIFS_TRANSFER_METHOD_COPY_FILE_RANGE   1
#define OIFS_TRANSFER_METHOD_RAW_BLOCK_COPY    2
#define OIFS_TRANSFER_METHOD_LOGICAL_REENCODE  3

#define OIFS_TRANSFER_FLAG_MOVE       0x01
#define OIFS_TRANSFER_FLAG_OVERWRITE  0x02

typedef struct { ... } OIFSTransferStats;

int32_t oifs_transfer_file_to(OIFSHandle* src_handle, const char* src_path,
                              OIFSHandle* dst_handle, const char* dst_path,
                              uint32_t flags, OIFSTransferStats* out_stats);
```
Transfers a regular file using the 3-tier fast-path pipeline (`Reflink` -> `CopyFileRange` -> `RawBlockCopy`), falling back to `LogicalReencode` across encrypted boundaries.

### Sub-Range Splice
```c
#define OIFS_SPLICE_TRIM_KEEP       0
#define OIFS_SPLICE_TRIM_PUNCH_HOLE 1
#define OIFS_SPLICE_TRIM_COLLAPSE   2

typedef struct { ... } OIFSSpliceStats;

int32_t oifs_splice_range_to(OIFSHandle* src_handle, uint64_t src_inode, uint64_t src_off,
                             OIFSHandle* dst_handle, uint64_t dst_inode, uint64_t dst_off,
                             uint64_t len, uint8_t trim_mode, OIFSSpliceStats* out_stats);
```
Splices a byte sub-range `[src_off, src_off + len)` from `src_inode` into `[dst_off, dst_off + len)` of `dst_inode`. The `trim_mode` controls what happens to the source range after the splice.

## Online Maintenance & Diagnostics

```c
typedef struct { ... } OIFSFsckReport;
typedef struct { ... } OIFSFragmentationStats;
typedef struct { ... } OIFSDefragStats;

int32_t oifs_verify_integrity(OIFSHandle* handle, OIFSFsckReport* out_report);
int32_t oifs_analyze_fragmentation(OIFSHandle* handle, OIFSFragmentationStats* out_stats);
int32_t oifs_defragment(OIFSHandle* handle, OIFSDefragStats* out_stats);
```

- `verify_integrity`: Runs a full structural consistency check (`fsck`). Safe on both read-write and read-only handles.
- `analyze_fragmentation`: Analyzes data-block fragmentation metrics.
- `defragment`: Runs online in-place defragmentation (WAL-atomic COW on journaled images, cycle-aware permutation on non-journaled images). Rejects read-only handles.

## Dynamic Library Versioning & 3-State Compatibility Handshake

OIFS embeds compile-time version constants and provides runtime introspection functions ([`src/ffi.rs#L801-L870`](repo://src/ffi.rs#L801-L870)):

```c
#define OIFS_VERSION_MAJOR 1
#define OIFS_VERSION_MINOR 4
#define OIFS_VERSION_PATCH 0
#define OIFS_VERSION_STRING "1.4.0"
#define OIFS_VERSION_CODE (((uint64_t)OIFS_VERSION_MAJOR << 32) | \
                           ((uint64_t)OIFS_VERSION_MINOR << 16) | (uint64_t)OIFS_VERSION_PATCH)

#define OIFS_VERSION_COMPAT_OK       0  // Exact expected version
#define OIFS_VERSION_COMPAT_WARN     1  // Newer library loaded (permitted)
#define OIFS_VERSION_COMPAT_ERR     -1  // Older library loaded (blocked)

uint32_t oifs_version_major(void);
uint32_t oifs_version_minor(void);
uint32_t oifs_version_patch(void);
uint64_t oifs_version_code(void);
const char* oifs_version_string(void);

/**
 * Checks compatibility between the application binary and the loaded dynamic library.
 * Policy: "Only block older versions; do NOT block newer versions."
 */
int32_t oifs_check_version(uint32_t req_major, uint32_t req_minor, uint32_t req_patch);

/**
 * Retrieves the absolute filesystem path from which this dynamic library was loaded.
 */
int32_t oifs_loaded_path(char* buf, size_t buf_size);
```

**Policy Semantics:**
- **0 (`OIFS_VERSION_COMPAT_OK`)**: Exact version match — fully verified, clean.
- **1 (`OIFS_VERSION_COMPAT_WARN`)**: Newer library version loaded — warning, allowed to proceed.
- **-1 (`OIFS_VERSION_COMPAT_ERR`)**: Older library version loaded — incompatible, blocked.

The Kani-formally-verified `oifs_check_version` function ([`src/ffi.rs#L844-L860`](repo://src/ffi.rs#L844-L860)) uses monotonic version codes and is proven to never overflow or panic for any symbolic input.

Convenience macro for C callers:
```c
#define OIFS_CHECK_VERSION() oifs_check_version(OIFS_VERSION_MAJOR, OIFS_VERSION_MINOR, OIFS_VERSION_PATCH)
```

## C++17/20 RAII Wrappers (`include/oifs.hpp`)

The `oifs.hpp` header provides a comprehensive C++ abstraction layer over the C API:

### Resource Management
- **`FileSystem`**: Move-only RAII wrapper around `OIFSHandle*` with `open`, `open_with_password`, `open_read_only`, `get_or_open` static factories.
- **`ReadSession`**: Move-only wrapper around `OIFSReadSession*`; amortizes outer `RwLock` acquisition.
- **`FileReadHandle`**: Move-only wrapper around `OIFSFileHandle*`; holds `shared_ptr` to parent session for out-of-order destruction safety.
- **`AsyncFuture`**: Move-only wrapper around `OIFSAsyncFuture*` with `is_ready()`, `wait()`, `last_error()`, and destructor calling `oifs_async_free` (drop barrier).

### Modern C++ Types
- **`ByteView`**: Non-owning read-only view over contiguous bytes; `as_span<T>()` with alignment/size validation.
- **`MutableByteSpan`**: Non-owning mutable byte buffer view.
- **`Span<T>`**: Lightweight typed non-owning contiguous span (C++17 & C++20 compatible; implicitly converts to `std::span` in C++20).
- **`PinnableSlice`**: RocksDB-style zero-copy / 1-copy fallback read slice. On `StorageMode::Mmap` raw contiguous extents, pins the mmap address directly (`is_pinned() == true`, 0 memcpy). On `FileIo`, compressed, encrypted, sparse, or non-contiguous ranges, falls back to internal `std::vector<uint8_t>` buffer.
- **`FilterConfig`**: Type-safe filter pipeline builder with `none()`, `numeric()`, `delta_only()`, `shuffle_only()`, `bitshuffle()` presets.

### C++20 Coroutine Support
When compiled with C++20 coroutines (`OIFS_HAS_CPP20_COROUTINE`), `AsyncFuture` provides an `awaiter`:
```cpp
co_await future;  // Inline Fast-Path hits resume immediately; pending futures suspend on background thread
```

### Ergonomic Overloads
Templated `create_file_with_data`, `write_file`, `write_span` overloads accept `ByteView`, `Span<const T>`, `std::vector<T>`, and `std::span` with automatic `as_bytes` conversion and filter pipeline support.

### FUSE Dispatch Helpers
```cpp
std::size_t fuse_dispatch_frame(ByteView req_frame, MutableByteSpan resp_buf);
std::vector<uint8_t> fuse_dispatch_frame(ByteView req_frame);  // Auto-sized response buffer
```

### Error Handling
All throwing operations throw `oifs::Error` (derived from `std::runtime_error`) with the handle's `last_error` message.

## Error Handling

All C API functions return error codes:
- Pointer-returning functions: `NULL` on error.
- Integer-returning functions: `-1` on error (except `oifs_check_version` and `oifs_async_poll`).
- Detailed error message retrieved via `oifs_last_error(OIFSHandle* handle, char* buf, uint32_t buf_size)`.

Async futures retrieve errors via `oifs_async_last_error(OIFSAsyncFuture* fut, char* buf, uint32_t buf_size)`.

All `extern "C"` boundaries are protected by `std::panic::catch_unwind` to prevent panics from unwinding across the FFI boundary.
