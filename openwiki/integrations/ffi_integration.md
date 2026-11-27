---
type: "Reference"
title: "OIFS FFI Integration Guide"
description: "Embedding OIFS via C/C++ FFI: build linkage, initialization, error handling, and async patterns."
tags: ["ffi", "c", "cpp", "integration", "embedding"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-f9a8a5ec259e5381eba4c51a
    resource: repo://include/oifs.h
  - id: openwiki-source-b74bd574146fa2bcf5a3aea3
    resource: repo://include/oifs.hpp
  - id: openwiki-source-c9e5b32aad7cafdb095c81a4
    resource: repo://src/ffi.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

# OIFS FFI Integration Guide

This guide explains how to embed the OIFS shared library (`liboifs.so` / `liboifs.dylib` / `oifs.dll`) into applications via the stable C ABI ([`include/oifs.h`](repo://include/oifs.h), [`src/ffi.rs`](repo://src/ffi.rs)) and the header-only C++17/20 RAII wrapper ([`include/oifs.hpp`](repo://include/oifs.hpp)). It covers building the cdylib/rlib, opaque handle lifecycle, multi-process IPC (Master-Proxy), synchronous and asynchronous I/O, zero-copy `PinnableSlice` reads, encryption, and common pitfalls.

## 1. Building and Linking

### Build the Shared Library

OIFS is a `cdylib` crate built with Cargo. The `[lib]` section declares both `cdylib` (dynamic library for C/C++ FFI) and `rlib` (Rust static library for Rust consumers):

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib", "rlib"]
```

```bash
# Release build (optimized, no debug symbols)
cargo build --release

# Output:
#   target/release/liboifs.so     (Linux)
#   target/release/liboifs.dylib  (macOS)
#   target/release/oifs.dll       (Windows)
```

### C/C++ Include Path

```bash
# GCC / Clang
-I/path/to/oifs/include

# MSVC
/I"C:\path\to\oifs\include"
```

### Linking

```bash
# Linux (requires -ldl for oifs_loaded_path symbol resolution)
-L/path/to/oifs/target/release -loifs -ldl

# macOS
-L/path/to/oifs/target/release -loifs

# Windows (MSVC)
/LIBPATH:C:\path\to\oifs\target\release oifs.lib
```

### Runtime Library Path

Set the runtime library search path so the dynamic loader finds `liboifs`:

```bash
# Linux
export LD_LIBRARY_PATH=/path/to/oifs/target/release:$LD_LIBRARY_PATH

# macOS
export DYLD_LIBRARY_PATH=/path/to/oifs/target/release:$DYLD_LIBRARY_PATH

# Windows: place oifs.dll next to the executable or in PATH
```

## 2. Header Inclusion

### C

```c
#include <oifs.h>
```

### C++ (Recommended: RAII Wrapper)

```cpp
#include <oifs.hpp>
// namespace oifs provides FileSystem, ReadSession, FileReadHandle, PinnableSlice, AsyncFuture
```

## 3. Version Handshake (Required at Startup)

**Always check version compatibility immediately after loading the library**. The header embeds compile-time constants (`OIFS_VERSION_MAJOR`, `OIFS_VERSION_MINOR`, `OIFS_VERSION_PATCH`, `OIFS_VERSION_STRING`). The runtime library exposes matching getters.

```c
int v_status = OIFS_CHECK_VERSION();  // macro expands to oifs_check_version(...)
if (v_status == OIFS_VERSION_COMPAT_ERR) {  // -1: loaded lib is OLDER → BLOCK
    char path[512] = {0};
    oifs_loaded_path(path, sizeof(path));
    fprintf(stderr, "FATAL [liboifs]: Outdated library '%s' (v%s) loaded! Expected >= v%s.\n",
            path, oifs_version_string(), OIFS_VERSION_STRING);
    abort();
} else if (v_status == OIFS_VERSION_COMPAT_WARN) {  // 1: loaded lib is NEWER → WARN
    fprintf(stderr, "WARN [liboifs]: Newer library (v%s) loaded than compiled with (v%s).\n",
            oifs_version_string(), OIFS_VERSION_STRING);
}
```

### Compatibility Policy

| Return Code | Constant | Meaning |
|-------------|----------|---------|
| 0 | `OIFS_VERSION_COMPAT_OK` | Exact version match — fully verified, clean |
| 1 | `OIFS_VERSION_COMPAT_WARN` | Newer library loaded — permitted, but emits warning |
| -1 | `OIFS_VERSION_COMPAT_ERR` | Older library loaded — incompatible, error out |

The policy is **"only block older versions; do NOT block newer versions."** Newer minor/patch releases are allowed to proceed with a warning, enabling forward-compatible deployment.

### C++ Version Helpers

```cpp
#include <oifs.hpp>

int status = oifs::check_version();  // uses header compile-time version by default
if (status == oifs::OIFS_VERSION_COMPAT_ERR) { /* block */ }
else if (status == oifs::OIFS_VERSION_COMPAT_WARN) { /* log warning */ }

std::string_view runtime_ver = oifs::version_string();
std::string lib_path = oifs::loaded_path();  // absolute path to loaded liboifs
```

## 4. C API — Core Lifecycle & Error Handling

### Opaque Handles

All C API objects are opaque pointers with strict ownership:

| Handle | Description |
|--------|-------------|
| `OIFSHandle*` | Open filesystem instance (read-write or read-only) |
| `OIFSReadSession*` | Shared read-lock session; amortizes `RwLock` acquisition across lookups/reads |
| `OIFSFileHandle*` | Pinned file within a read session; holds `Arc` to session for out-of-order destruction safety |
| `OIFSAsyncFuture*` | In-flight or inline-completed async operation; drop barrier prevents use-after-free |

### Opening Filesystems

```c
// Create new or open existing (read-write, default Mmap storage)
OIFSHandle* h = oifs_open("/path/to/data.img", 1024*1024*1024);  // 1 GiB

// Open with password (XChaCha20-Poly1305 encryption, Argon2id KDF)
OIFSHandle* h = oifs_open_with_password("/path/to/secure.img", 1024*1024*1024, "passphrase");

// Full control: storage mode, WAL journaling, encryption
OIFSHandle* h = oifs_open_with_options("/path/to/data.img", 1024*1024*1024,
                                       "passphrase",
                                       OIFS_STORAGE_MODE_MMAP,  // or OIFS_STORAGE_MODE_FILE_IO
                                       1);  // journaled = true

// Read-only mount (shared F_RDLCK, multiple processes allowed on 0444 images)
OIFSHandle* h = oifs_open_read_only("/path/to/data.img", NULL, OIFS_STORAGE_MODE_MMAP);

// Get-or-create (multi-process singleton via lock file)
OIFSHandle* h = oifs_get_or_open("/path/to/data.img", 1024*1024*1024);
```

### Closing & Flushing

```c
int32_t oifs_flush(OIFSHandle* handle);                    // sync data + metadata
int32_t oifs_set_durability_mode(OIFSHandle* h, uint8_t);  // 0=Lazy, 1=RangeAsync, 2=Strict
void oifs_close(OIFSHandle* handle);                       // drops handle, flushes if RW
```

### Error Handling

Every C function that can fail returns a status code (`0` = success, `-1` = error) and stores a UTF-8 error message in thread-local storage. Retrieve it via:

```c
int32_t oifs_last_error(OIFSHandle* handle, char* buf, uint32_t buf_size);
```

```c
char errbuf[256];
oifs_last_error(handle, errbuf, sizeof(errbuf));
fprintf(stderr, "OIFS error: %s\n", errbuf);
```

**Thread-safety:** Error slots are thread-local. A panic in one thread does not pollute another thread's error state.

### Durability Modes

| Constant | Value | Behavior |
|----------|-------|----------|
| `OIFS_DURABILITY_LAZY` | 0 | OS page-cache writeback (default, fastest) |
| `OIFS_DURABILITY_RANGE_ASYNC` | 1 | Batched dirty-range async flush |
| `OIFS_DURABILITY_STRICT` | 2 | Synchronous `msync` / `sync_data` barriers |

```c
oifs_set_durability_mode(handle, OIFS_DURABILITY_STRICT);  // for crash-consistent workloads
```

## 5. C API — Synchronous I/O

### Directory Operations

```c
typedef void (*ListCallback)(const char* name, uint64_t size, uint64_t mtime, void* user_data);

int32_t oifs_ls(OIFSHandle* handle, ListCallback cb, void* user_data);
int32_t oifs_ls_dir(OIFSHandle* handle, const char* path, ListCallback cb, void* user_data);
int32_t oifs_mkdir(OIFSHandle* handle, const char* path);
```

### File Metadata

```c
typedef struct {
    uint64_t inode_id;
    uint64_t size;
    uint64_t compressed_size;
    uint64_t created_at;
    uint64_t modified_at;
    uint8_t is_dir;
    uint8_t encrypted;
    uint8_t seekable_64k;
    uint8_t filter_typesize;
    uint8_t filter_delta;
    uint8_t filter_shuffle;
    uint8_t filter_bitshuffle;
    uint8_t _pad;
} OIFSFileStat;

int32_t oifs_stat(OIFSHandle* handle, const char* path, OIFSFileStat* out_stat);
int32_t oifs_exists(OIFSHandle* handle, const char* path);  // 1=exists, 0=not found, -1=error
```

### Basic Read / Write

```c
// Positional read at arbitrary offset (handles compressed/encrypted transparently)
int64_t oifs_read_at(OIFSHandle* handle, const char* filename, uint64_t offset,
                     uint8_t* buf, uint64_t buf_size);

// Sequential read from start (delegates to oifs_read_at with offset 0)
int64_t oifs_read_file(OIFSHandle* handle, const char* filename, uint8_t* buf, uint64_t buf_size);

// Write (creates file if missing; auto compression by default)
int32_t oifs_write_file(OIFSHandle* handle, const char* filename,
                        const uint8_t* buf, uint64_t buf_size);
```

### Extended Write Policies

```c
#define OIFS_WRITE_POLICY_DEFAULT       0x00
#define OIFS_WRITE_POLICY_RAW           0x01
#define OIFS_WRITE_POLICY_STREAM        0x02
#define OIFS_WRITE_POLICY_SEEKABLE_64K  0x08

int32_t oifs_write_file_with_policy(OIFSHandle* handle, const char* filename,
                                    uint64_t offset, const uint8_t* buf, uint64_t buf_size,
                                    uint32_t policy_flags, int32_t zstd_level);
```

**Policy selection:**

| Policy | Use Case |
|--------|----------|
| `DEFAULT` (0) | Auto: full-file stream compression for large files, raw for small |
| `RAW` | Never compress — incompressible data, maximum throughput |
| `STREAM` | Streaming compression with custom zstd level (1-19) |
| `SEEKABLE_64K` | Seekable 64 KiB chunked compression for random access |

```c
// Example: Write 1 MiB with zstd level 3, seekable 64K chunks
oifs_write_file_with_policy(handle, "data.bin", 0, data, 1024*1024,
                            OIFS_WRITE_POLICY_SEEKABLE_64K, 3);
```

### Single-Transaction Create + Write

```c
// Atomically creates file and writes initial payload — single transaction, no partial state
int64_t oifs_create_file_with_data(OIFSHandle* handle, const char* path,
                                   const uint8_t* buf, uint64_t buf_size,
                                   uint32_t policy_flags, int32_t zstd_level);
// Returns new inode_id (>= 0) on success, -1 on error
```

### Direct Inode I/O (Bypass Path Resolution)

```c
int64_t oifs_resolve_path(OIFSHandle* handle, const char* path);  // path -> inode_id
int64_t oifs_read_at_inode(OIFSHandle* handle, uint64_t inode_id, uint64_t offset,
                           uint8_t* buf, uint64_t buf_size);
```

### Scatter-Gather Batch Read

```c
typedef struct {
    uint64_t inode_id;
    uint64_t offset;
    uint8_t* buf;
    uint64_t buf_size;
    int64_t result;  // filled on return
} OIFSReadBatchItem;

int32_t oifs_read_at_batch(OIFSHandle* handle, OIFSReadBatchItem* items, size_t count);
```

Executes multiple reads under a single read lock and I/O submission. Populates `items[i].result` with bytes read or `-1` per item.

### SIMD Pre-Compression Filters

```c
typedef struct {
    uint8_t typesize;   // 1, 2, 4, 8 (0 = disabled)
    uint8_t delta;      // first-order delta encoding
    uint8_t shuffle;    // byte shuffle
    uint8_t bitshuffle; // bit shuffle
} OIFSFilterConfig;

int64_t oifs_create_file_with_data_filtered(OIFSHandle* handle, const char* path,
                                            const uint8_t* buf, uint64_t buf_size,
                                            uint32_t policy_flags, int32_t zstd_level,
                                            const OIFSFilterConfig* filter_cfg);

int32_t oifs_write_file_with_filters(OIFSHandle* handle, const char* path,
                                     uint64_t offset, const uint8_t* buf, uint64_t buf_size,
                                     uint32_t policy_flags, int32_t zstd_level,
                                     const OIFSFilterConfig* filter_cfg);
```

Optimized for numeric arrays (time-series, scientific data). `typesize` must match element width.

## 6. C API — Shared Read Session & Zero-Copy

### Session Lifecycle

```c
OIFSReadSession* oifs_begin_read(OIFSHandle* handle);  // acquires shared read lock
void oifs_end_read(OIFSReadSession* session);          // releases lock
```

A session holds a single `RwLock` read guard for its lifetime, amortizing lock contention across many file opens and reads.

### Pinned File Handles

```c
OIFSFileHandle* oifs_session_open_file(const OIFSReadSession* session, const char* path);
OIFSFileHandle* oifs_session_open_inode(const OIFSReadSession* session, uint64_t inode_id);
void oifs_file_close(OIFSFileHandle* file);

uint64_t oifs_file_size(const OIFSFileHandle* file);
uint64_t oifs_file_inode_id(const OIFSFileHandle* file);
int64_t oifs_file_read_at(const OIFSFileHandle* file, uint64_t offset,
                          uint8_t* buf, uint64_t buf_size);
```

`OIFSFileHandle` internally holds an `Arc` to the session, so the handle remains valid even if the parent `OIFSReadSession` is destroyed first (no use-after-free).

### True Zero-Copy `mmap` Slice

```c
// Returns:
//   0 = zero-copy succeeded; *out_ptr points into mmap, *out_len is byte count
//   1 = zero-copy unavailable (FileIo, compressed, encrypted, sparse hole, non-contiguous)
//  -1 = error
int32_t oifs_file_mmap_slice(const OIFSFileHandle* file, uint64_t offset,
                             uint64_t max_len, const uint8_t** out_ptr, uint64_t* out_len);
```

**Usage pattern:**

```c
const uint8_t* ptr = NULL;
uint64_t len = 0;
int32_t rc = oifs_file_mmap_slice(file, offset, max_len, &ptr, &len);
if (rc == 0) {
    // Zero-copy: ptr is valid while file/session is open
    process(ptr, len);
} else if (rc == 1) {
    // Fallback: allocate buffer and use oifs_file_read_at
    uint8_t* buf = malloc(len);
    oifs_file_read_at(file, offset, buf, len);
    process(buf, len);
    free(buf);
}
```

The returned pointer is **directly into the memory-mapped image**. It remains valid as long as the `OIFSFileHandle` or its parent `OIFSReadSession` is alive.

## 7. C API — Asynchronous I/O

OIFS async operations use `OIFSAsyncFuture*` with two completion models:

- **Inline Fast-Path:** Cache/mmap hits complete immediately on the calling thread (`oifs_async_poll == 1`, zero thread switch).
- **Background Pool:** Cold reads or compressed/encrypted decodes execute on a persistent thread pool.
- **Drop Barrier:** `oifs_async_free` waits for any in-flight write to caller buffers before returning (prevents use-after-free).
- **Group Commit:** Concurrent async writes under `OIFS_DURABILITY_STRICT` share a single physical sync barrier per batch.

### Future & Poll/Wait API

```c
// Async read by inode_id (no path resolution)
OIFSAsyncFuture* oifs_read_at_async(OIFSHandle* handle, uint64_t inode_id,
                                    uint64_t offset, uint8_t* buf, uint64_t buf_size);

// Async single-transaction create + write
OIFSAsyncFuture* oifs_create_file_with_data_async(OIFSHandle* handle, const char* path,
                                                  const uint8_t* buf, uint64_t buf_size,
                                                  uint32_t policy_flags, int32_t zstd_level);

// Async write at offset
OIFSAsyncFuture* oifs_write_file_async(OIFSHandle* handle, const char* path,
                                       uint64_t offset, const uint8_t* buf, uint64_t buf_size,
                                       uint32_t policy_flags, int32_t zstd_level);

// Async flush
OIFSAsyncFuture* oifs_flush_async(OIFSHandle* handle);

// Poll without blocking: 1=ready, 0=pending, -1=invalid
int32_t oifs_async_poll(const OIFSAsyncFuture* fut);

// Block until completion; returns result or -1 (call oifs_async_last_error on failure)
int64_t oifs_async_wait(OIFSAsyncFuture* fut);

// Retrieve error message from failed future
int32_t oifs_async_last_error(const OIFSAsyncFuture* fut, char* buf, uint32_t buf_size);

// Release future; if background task still writing to caller buffer, blocks until done
void oifs_async_free(OIFSAsyncFuture* fut);
```

**Typical polling loop:**

```c
OIFSAsyncFuture* fut = oifs_read_at_async(handle, inode_id, offset, buf, size);
while (!oifs_async_poll(fut)) {
    // do other work, or sleep briefly
}
int64_t bytes = oifs_async_wait(fut);
if (bytes < 0) {
    char err[256];
    oifs_async_last_error(fut, err, sizeof(err));
    // handle error
}
oifs_async_free(fut);
```

### Callback API (C-style)

```c
typedef void (*OifsAsyncReadCallback)(int64_t status_or_len, const uint8_t* data,
                                      size_t data_len, void* user_data);
typedef void (*OifsAsyncStatusCallback)(int64_t status_or_inode, void* user_data);

int32_t oifs_read_at_async_cb(OIFSHandle* handle, uint64_t inode_id, uint64_t offset,
                              uint64_t len, OifsAsyncReadCallback cb, void* user_data);

int32_t oifs_create_file_with_data_async_cb(OIFSHandle* handle, const char* path,
                                            const uint8_t* buf, uint64_t buf_size,
                                            uint32_t policy_flags, int32_t zstd_level,
                                            OifsAsyncStatusCallback cb, void* user_data);
```

Callbacks are invoked on the background thread (or inline for fast-path). **Never block in callbacks.**

## 8. C++ RAII Wrapper (`oifs.hpp`)

The header-only wrapper (`include/oifs.hpp`) provides move-only, exception-safe types that map 1:1 to C opaque handles.

### Type Mapping

| C Type | C++ Wrapper | Notes |
|--------|-------------|-------|
| `OIFSHandle*` | `oifs::FileSystem` | Move-only, owns handle |
| `OIFSReadSession*` | `oifs::ReadSession` | Move-only, amortizes RwLock |
| `OIFSFileHandle*` | `oifs::FileReadHandle` | Move-only, `Arc`-protected |
| `OIFSAsyncFuture*` | `oifs::AsyncFuture` | Move-only, drop barrier |
| — | `oifs::PinnableSlice` | RocksDB-style zero-copy/1-copy fallback |
| — | `oifs::ByteView` / `oifs::MutableByteSpan` | Non-owning buffer views |
| — | `oifs::Span<T>` | Typed contiguous span (C++17 compatible) |

### Core Types

#### `oifs::FileSystem`

```cpp
// Open
oifs::FileSystem fs = oifs::FileSystem::open("/path/to/data.img", 1024*1024*1024);

// Open with password
oifs::FileSystem fs = oifs::FileSystem::open_with_password("/path/to/secure.img",
                                                           1024*1024*1024, "passphrase");

// Full options
oifs::OpenOptions opts;
opts.size = 1024*1024*1024;
opts.password = "passphrase";
opts.storage_mode = oifs::StorageMode::Mmap;
opts.journaled = true;
oifs::FileSystem fs = oifs::FileSystem::open_with_options("/path/to/data.img", opts);

// Read-only (shared lock, concurrent readers allowed)
oifs::FileSystem fs = oifs::FileSystem::open_read_only("/path/to/data.img", {},
                                                       oifs::StorageMode::Mmap);

// Get-or-create (multi-process singleton)
oifs::FileSystem fs = oifs::FileSystem::get_or_open("/path/to/data.img", 1024*1024*1024);

// Clone whole image (CoW: APFS clonefile, Linux FICLONE, or copy)
oifs::CloneMethod method = oifs::FileSystem::clone_image("/src.img", "/dst.img");

// RAII close
fs.close();  // or let destructor run
```

**Filesystem operations:**

```cpp
// Directory
fs.mkdir("dir");
auto entries = fs.ls();                    // root
auto entries = fs.ls("subdir");            // subdirectory
fs.delete_file("file");
fs.rename("old", "new");
fs.truncate_file("file", 4096);

// Metadata
oifs::FileStat st = fs.stat("file");
bool exists = fs.exists("path");
uint64_t inode = fs.resolve_path("path");

// Durability & I/O backend
fs.set_durability_mode(oifs::DurabilityMode::Strict);
fs.set_file_io_cache_bytes(64 * 1024 * 1024);
oifs::IoBackend eff = fs.set_io_backend(oifs::IoBackend::IoUring);  // Linux only
oifs::IoBackend cur = fs.get_io_backend();

// Diagnostics
oifs::FsckReport report = fs.verify_integrity();
oifs::FragmentationStats frag = fs.analyze_fragmentation();
oifs::DefragStats defrag = fs.defragment();
```

#### `oifs::ReadSession` + `oifs::FileReadHandle`

```cpp
oifs::ReadSession session = fs.begin_read();

// Open by path or inode
oifs::FileReadHandle fh = session.open_file("data.bin");
oifs::FileReadHandle fh = session.open_inode(42);

// Positional read
std::vector<uint8_t> buf(4096);
std::size_t n = fh.read_at(0, oifs::MutableByteSpan(buf));

// Zero-copy mmap view (optional<ByteView> = nullopt if unavailable)
if (auto view = fh.try_mmap_view(0, 4096)) {
    // view->data() points directly into mmap, zero copies
    process(view->data(), view->size());
}

// PinnableSlice: automatic zero-copy or 1-copy fallback
oifs::PinnableSlice slice;
std::size_t n = fh.read_pinnable(0, 4096, slice);
if (slice.is_pinned()) {
    // Zero-copy: slice.data() points into mmap
    process(slice.data(), slice.size());
} else {
    // 1-copy: slice backed by internal vector
    process(slice.data(), slice.size());
}

// Convenience overload
oifs::PinnableSlice slice = fh.read_pinnable(0, 4096);
```

**`PinnableSlice` semantics:**

- On `StorageMode::Mmap` with raw contiguous extents: pins the mmap address directly (`is_pinned() == true`, zero memcpy, zero heap allocation).
- On `StorageMode::FileIo`, compressed, encrypted, sparse, or non-contiguous ranges: falls back to reading into internal `std::vector<uint8_t>` (`is_pinned() == false`).
- Holds a `shared_ptr` to the underlying `OIFSFileHandle`, so the mapping stays valid for the slice's lifetime.

#### `oifs::AsyncFuture` — Poll / Wait / Coroutines

```cpp
// Start async read
oifs::AsyncFuture fut = fs.read_at_async(inode_id, offset, oifs::MutableByteSpan(buf));

// Non-blocking poll (inline fast-path hits return true immediately)
if (fut.is_ready()) { /* already done */ }

// Block until completion
int64_t bytes = fut.wait();  // throws oifs::Error on failure

// Error from failed future
std::string err = fut.last_error();
```

**C++20 Coroutine Support (when `__cpp_impl_coroutine` and `<coroutine>` available):**

```cpp
// co_await an AsyncFuture
int64_t bytes = co_await fs.read_at_async(inode_id, offset, buf_span);
// - Inline fast-path hits: await_ready() == true, resumes immediately (zero suspension)
// - Pending: suspends, background thread runs oifs_async_wait, resumes on completion
// - Destructor of awaiter joins worker thread if coroutine cancelled early (drop barrier)
```

**Coroutine awaiter behavior:**

| State | Behavior |
|-------|----------|
| `await_ready() == true` | Inline fast-path hit; resumes synchronously, no thread hop |
| `await_ready() == false` | Spawns `std::thread` calling `oifs_async_wait`; suspends coroutine |
| Early cancellation | Awaiter dtor marks `cancelled=true`, joins worker thread (holds drop barrier until Rust task stops touching buffers) |

#### Buffer Integration (`ByteView`, `MutableByteSpan`, `Span<T>`)

```cpp
// From std::vector, string_view, C arrays, std::span
std::vector<uint8_t> vec = {1,2,3,4};
oifs::ByteView view(vec);                    // read-only
oifs::MutableByteSpan mut(vec);              // mutable
oifs::Span<const uint8_t> span(vec);         // typed

// C++20: implicit conversion to std::span
#if __cplusplus >= 202002L
std::span<const uint8_t> std_span = view;    // implicit
std::span<uint8_t> mut_span = mut;           // implicit
#endif

// Typed reinterpretation (validates size multiple + alignment)
auto int_span = view.as_span<int32_t>();     // Span<const int32_t>

// Write helpers accept any buffer view
fs.write_file("file", oifs::ByteView(data_ptr, data_len));
fs.write_file("file", offset, oifs::ByteView(data_ptr, data_len),
              oifs::WritePolicy::Seekable64K, 3);

// Span-based helpers (templated on trivially copyable T)
std::vector<float> samples = generate();
fs.create_file_with_span("samples.bin", oifs::Span<const float>(samples),
                         oifs::WritePolicy::Default, 0,
                         oifs::FilterConfig::numeric(sizeof(float)));  // delta+shuffle

// Read into MutableByteSpan
std::vector<uint8_t> buf(file_size);
std::size_t n = fs.read_at("file", 0, oifs::MutableByteSpan(buf));
```

#### Async Callbacks (Lambda-Friendly)

```cpp
// Async read with lambda: receives (int64_t status, oifs::ByteView data)
fs.read_at_async_cb(inode_id, offset, len,
    [](int64_t status, oifs::ByteView data) {
        if (status >= 0) {
            process(data.data(), data.size());
        } else {
            handle_error();
        }
    });

// Async create+write with lambda: receives (int64_t status_or_inode)
fs.create_file_with_data_async_cb("new_file", oifs::ByteView(payload),
    [](int64_t status_or_inode) {
        if (status_or_inode >= 0) {
            uint64_t new_inode = status_or_inode;
        } else {
            handle_error();
        }
    });
```

Trampolines heap-allocate the lambda, invoke it from the background thread, and clean up. **Exceptions never unwind across the extern "C" boundary.**

## 9. Cross-Image Operations

### CoW Clone (Whole Image)

```cpp
// C
uint8_t method = 0;
oifs_clone_image("/src.img", "/dst.img", &method);

// C++
oifs::CloneMethod method = oifs::FileSystem::clone_image("/src.img", "/dst.img");
// method = ApfsClonefile | LinuxFiclone | StandardCopy
```

Uses host OS CoW primitive: `clonefile()` on macOS APFS, `ioctl(FICLONE)` on Linux, or standard copy fallback.

### 3-Tier Fast File Transfer

```cpp
// C
OIFSTransferStats stats;
oifs_transfer_file_to(src_h, "src/file", dst_h, "dst/file",
                      OIFS_TRANSFER_FLAG_OVERWRITE, &stats);

// C++
oifs::TransferOptions opts;
opts.move_file = false;
opts.overwrite = true;
oifs::TransferStats stats = fs.transfer_file_to("src/file", dst_fs, "dst/file", opts);
// stats.method = Reflink | CopyFileRange | RawBlockCopy | LogicalReencode
```

Pipeline attempts in order:

1. **Reflink** — `ioctl(FICLONE)` / `FICLONERANGE` (Linux) or `clonefile` (macOS)
2. **CopyFileRange** — `copy_file_range()` syscall (Linux)
3. **RawBlockCopy** — Physical block copy (same image, same encryption)
4. **LogicalReencode** — Decrypt → re-encrypt (cross-encryption boundaries)

### Sub-Range Splice

```cpp
// C
OIFSSpliceStats stats;
oifs_splice_range_to(src_h, src_inode, src_off,
                     dst_h, dst_inode, dst_off,
                     len, OIFS_SPLICE_TRIM_KEEP, &stats);

// C++
oifs::SpliceOptions opts;
opts.trim_mode = oifs::SpliceTrimMode::PunchHole;  // or Keep, CollapseRange
oifs::SpliceStats stats = fs.splice_range_to(src_inode, src_off,
                                             dst_fs, dst_inode, dst_off,
                                             len, opts);
```

Efficiently moves a byte range between files/images using the same tier pipeline as whole-file transfer.

## 10. Encryption

OIFS uses **XChaCha20-Poly1305** AEAD with **Argon2id** key derivation.

```c
// C: pass password to open/create
OIFSHandle* h = oifs_open_with_password("/secure.img", size, "passphrase");
OIFSHandle* h = oifs_open_with_options("/secure.img", size, "passphrase",
                                       OIFS_STORAGE_MODE_MMAP, 1);

// C++
oifs::FileSystem fs = oifs::FileSystem::open_with_password("/secure.img", size, "passphrase");
oifs::OpenOptions opts;
opts.password = "passphrase";
oifs::FileSystem fs = oifs::FileSystem::open_with_options("/secure.img", opts);
```

- Password is never stored; only the derived key encrypts the superblock and data blocks.
- Read-only mounts also require the password to decrypt metadata.
- Changing password requires full re-encryption (not supported in-place).

## 11. Multi-Process IPC (Master-Proxy)

OIFS supports concurrent access via a **Master-Proxy** architecture:

- **Master process** holds the `OIFSHandle` (read-write), serves FUSE or direct API.
- **Proxy processes** open read-only handles via `oifs_open_read_only` or `oifs_get_or_open` with shared `F_RDLCK`.
- **Lock protocol:** Master acquires exclusive `F_WRLCK` on write; proxies acquire shared `F_RDLCK` on read.
- **`get_or_open` family:** Uses a lock file (`.lock`) beside the image for cross-process singleton.

```c
// Master (read-write)
OIFSHandle* master = oifs_open("/data.img", size);
// ... write operations ...

// Proxy (read-only, multiple processes)
OIFSHandle* proxy = oifs_open_read_only("/data.img", NULL, OIFS_STORAGE_MODE_MMAP);
// ... read operations ...

// Or get-or-open (auto-creates if missing, coordinates via lock file)
OIFSHandle* h = oifs_get_or_open("/data.img", size);
```

## 12. FUSE Kernel Protocol Integration

OIFS can serve as a FUSE filesystem by dispatching raw Kernel ABI v7.28 frames:

```c
// C: dispatch raw FUSE request frame, fill response buffer
int64_t oifs_fuse_dispatch_frame(OIFSHandle* handle,
                                 const uint8_t* req_buf, size_t req_len,
                                 uint8_t* resp_buf, size_t resp_cap);

// C++
std::vector<uint8_t> resp = fs.fuse_dispatch_frame(req_frame);  // auto-sizes
// or
std::size_t n = fs.fuse_dispatch_frame(req_frame, resp_span);   // caller buffer
```

- File handles (`fh`) opened via `FUSE_OPEN`/`FUSE_CREATE`/`FUSE_OPENDIR` persist across calls on the same `OIFSHandle`.
- Read-only handles automatically reject mutating opcodes with `-EROFS`.
- One-way notifications (e.g., `FUSE_FORGET`) return response length `0`.

## 13. I/O Engine Backends

```c
// C
oifs_set_io_backend(handle, OIFS_IO_BACKEND_MMAP);     // 0: default mmap
oifs_set_io_backend(handle, OIFS_IO_BACKEND_PREAD);    // 1: positional pread/pwrite
oifs_set_io_backend(handle, OIFS_IO_BACKEND_IO_URING); // 2: io_uring (Linux only)

// C++
fs.set_io_backend(oifs::IoBackend::IoUring);
oifs::IoBackend cur = fs.get_io_backend();
```

- **Mmap** (default): Memory-mapped, zero-copy reads, OS page cache.
- **Pread**: Pure positional I/O, no mmap; useful for network filesystems or when mmap is undesirable.
- **IoUring**: Linux-only async submission; falls back to `Pread` if unsupported.

The `io_uring` feature is opt-in via `cfg(target_os = "linux")` in Cargo.toml.

## 14. Storage Modes

| Mode | Constant | Description |
|------|----------|-------------|
| Mmap | `OIFS_STORAGE_MODE_MMAP` (0) | Memory-mapped container (default). Enables zero-copy reads. |
| FileIo | `OIFS_STORAGE_MODE_FILE_IO` (1) | Pure `pread`/`pwrite`, zero mmap. Required for some network FS. |

```c
// C
OIFSHandle* h = oifs_open_with_options(path, size, pwd, OIFS_STORAGE_MODE_FILE_IO, 1);

// C++
oifs::OpenOptions opts;
opts.storage_mode = oifs::StorageMode::FileIo;
oifs::FileSystem fs = oifs::FileSystem::open_with_options(path, opts);
```

## 15. Common Pitfalls & Best Practices

| Pitfall | Solution |
|---------|----------|
| **Skipping version handshake** | Always call `OIFS_CHECK_VERSION()` at startup. |
| **Use-after-free on async buffers** | Never free `buf` passed to `oifs_read_at_async` until `oifs_async_free` returns. |
| **Blocking in async callbacks** | Callbacks run on background thread; offload work to a queue. |
| **Mixing storage modes** | Don't open the same image with different `storage_mode` concurrently. |
| **Forgetting `oifs_flush` on crash-critical data** | Use `OIFS_DURABILITY_STRICT` or explicit `oifs_flush`/`oifs_flush_async`. |
| **Cross-process read-write conflicts** | Use read-only proxies (`oifs_open_read_only`) for readers; single writer. |
| **NUL bytes in C strings** | C++ wrapper validates and throws `oifs::Error` on embedded NUL. |
| **Buffer overlap in `oifs_read_at_batch`** | API rejects overlapping output buffers; ensure disjoint destinations. |

## 16. Minimal Working Examples

### C: Synchronous Read/Write

```c
#include <oifs.h>
#include <stdio.h>
#include <stdlib.h>

int main() {
    int status = OIFS_CHECK_VERSION();
    if (status == OIFS_VERSION_COMPAT_ERR) return 1;

    OIFSHandle* h = oifs_open("test.img", 1024*1024*10);  // 10 MiB
    if (!h) return 1;

    // Write
    const char* msg = "Hello, OIFS!";
    oifs_write_file(h, "greeting.txt", (uint8_t*)msg, strlen(msg));
    oifs_flush(h);

    // Read
    uint8_t buf[256];
    int64_t n = oifs_read_file(h, "greeting.txt", buf, sizeof(buf));
    if (n > 0) {
        fwrite(buf, 1, n, stdout);
        putchar('\n');
    }

    oifs_close(h);
    return 0;
}
```

### C++: RAII + Zero-Copy + Coroutines

```cpp
#include <oifs.hpp>
#include <iostream>
#include <vector>

int main() {
    // Version check
    if (oifs::check_version() == oifs::OIFS_VERSION_COMPAT_ERR) return 1;

    // Open
    oifs::FileSystem fs = oifs::FileSystem::open("test.img", 1024*1024*10);

    // Write with seekable compression
    std::vector<float> data(100000);
    std::iota(data.begin(), data.end(), 0.0f);
    fs.create_file_with_span("data.f32", oifs::Span<const float>(data),
                             oifs::WritePolicy::Seekable64K, 3,
                             oifs::FilterConfig::numeric(sizeof(float)));

    // Read session + zero-copy
    oifs::ReadSession session = fs.begin_read();
    oifs::FileReadHandle fh = session.open_file("data.f32");

    oifs::PinnableSlice slice = fh.read_pinnable(0, fh.size());
    if (slice.is_pinned()) {
        std::cout << "Zero-copy: " << slice.size() << " bytes\n";
    } else {
        std::cout << "1-copy fallback: " << slice.size() << " bytes\n";
    }
    auto floats = slice.as_span<float>();
    std::cout << "First 5: ";
    for (size_t i = 0; i < 5 && i < floats.size(); ++i) std::cout << floats[i] << ' ';
    std::cout << '\n';

    return 0;
}
```

### C++: Async Read with `co_await` (C++20)

```cpp
#include <oifs.hpp>
#include <coroutine>
#include <iostream>

struct Task {
    struct promise_type {
        Task get_return_object() { return {}; }
        std::suspend_never initial_suspend() { return {}; }
        std::suspend_never final_suspend() noexcept { return {}; }
        void return_void() {}
        void unhandled_exception() {}
    };
};

Task read_async(oifs::FileSystem& fs, uint64_t inode_id) {
    std::vector<uint8_t> buf(4096);
    int64_t bytes = co_await fs.read_at_async(inode_id, 0,
                                              oifs::MutableByteSpan(buf));
    if (bytes > 0) {
        std::cout << "Read " << bytes << " bytes\n";
    }
}

int main() {
    oifs::FileSystem fs = oifs::FileSystem::open("test.img", 1024*1024);
    // ... create file, get inode_id ...
    read_async(fs, inode_id);
    return 0;
}
```

## 17. Build Integration (CMake Example)

```cmake
cmake_minimum_required(VERSION 3.16)
project(myapp)

find_library(OIFS_LIB oifs PATHS /path/to/oifs/target/release)
if (NOT OIFS_LIB)
    message(FATAL_ERROR "liboifs not found")
endif()

include_directories(/path/to/oifs/include)

add_executable(myapp main.cpp)
target_link_libraries(myapp PRIVATE ${OIFS_LIB})
if (UNIX AND NOT APPLE)
    target_link_libraries(myapp PRIVATE dl)
endif()

# Runtime path (Linux/macOS)
if (CMAKE_SYSTEM_NAME STREQUAL "Linux")
    set_target_properties(myapp PROPERTIES
        INSTALL_RPATH "\$ORIGIN/../lib"
        BUILD_RPATH "/path/to/oifs/target/release"
    )
elseif (CMAKE_SYSTEM_NAME STREQUAL "Darwin")
    set_target_properties(myapp PROPERTIES
        INSTALL_RPATH "@loader_path/../lib"
        BUILD_RPATH "/path/to/oifs/target/release"
    )
endif()
```

## 18. Summary of Key Entry Points

| Category | C Function | C++ Method |
|----------|------------|------------|
| **Lifecycle** | `oifs_open`, `oifs_close`, `oifs_flush` | `FileSystem::open`, `close()`, `flush()` |
| **Version** | `OIFS_CHECK_VERSION`, `oifs_version_string` | `oifs::check_version()`, `oifs::version_string()` |
| **Directory** | `oifs_ls`, `oifs_mkdir`, `oifs_delete_file` | `fs.ls()`, `fs.mkdir()`, `fs.delete_file()` |
| **Metadata** | `oifs_stat`, `oifs_exists`, `oifs_resolve_path` | `fs.stat()`, `fs.exists()`, `fs.resolve_path()` |
| **Read** | `oifs_read_at`, `oifs_read_file` | `fs.read_at()`, `fs.read_file()` |
| **Write** | `oifs_write_file`, `oifs_write_file_with_policy` | `fs.write_file()`, `fs.create_file_with_data()` |
| **Zero-Copy** | `oifs_begin_read`, `oifs_file_mmap_slice` | `fs.begin_read()`, `fh.try_mmap_view()`, `fh.read_pinnable()` |
| **Async** | `oifs_read_at_async`, `oifs_async_poll`, `oifs_async_wait` | `fs.read_at_async()`, `fut.is_ready()`, `fut.wait()`, `co_await fut` |
| **Callbacks** | `oifs_read_at_async_cb` | `fs.read_at_async_cb(lambda)` |
| **Cross-Image** | `oifs_clone_image`, `oifs_transfer_file_to`, `oifs_splice_range_to` | `FileSystem::clone_image()`, `fs.transfer_file_to()`, `fs.splice_range_to()` |
| **Diagnostics** | `oifs_verify_integrity`, `oifs_analyze_fragmentation`, `oifs_defragment` | `fs.verify_integrity()`, `fs.analyze_fragmentation()`, `fs.defragment()` |

---

*This guide reflects OIFS v1.6.1 API. For the C header, see [`include/oifs.h`](repo://include/oifs.h). For the C++ wrapper, see [`include/oifs.hpp`](repo://include/oifs.hpp). For the Rust FFI implementation, see [`src/ffi.rs`](repo://src/ffi.rs).*
