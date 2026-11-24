---
type: "Reference"
title: "OIFS FFI Integration Guide"
description: "Embedding OIFS via C/C++ FFI: build linkage, initialization, error handling, and async patterns."
tags: ["ffi", "c", "cpp", "integration", "embedding"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-24T19:09:23.676Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-f9a8a5ec259e5381eba4c51a
    resource: repo://include/oifs.h
  - id: openwiki-source-b74bd574146fa2bcf5a3aea3
    resource: repo://include/oifs.hpp
  - id: openwiki-source-c9e5b32aad7cafdb095c81a4
    resource: repo://src/ffi.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-24T19:09:23.676Z" }
---

# OIFS FFI Integration Guide

This guide explains how to embed the OIFS shared library (`liboifs.so` / `liboifs.dylib` / `oifs.dll`) into applications via the stable C ABI ([`include/oifs.h`](repo://include/oifs.h), [`src/ffi.rs`](repo://src/ffi.rs)) and the header-only C++17/20 RAII wrapper ([`include/oifs.hpp`](repo://include/oifs.hpp)). It covers building the cdylib, opaque handle lifecycle, multi-process IPC (Master-Proxy), synchronous and asynchronous I/O, zero-copy `PinnableSlice` reads, encryption, and common pitfalls.

## 1. Building and Linking

### Build the Shared Library

OIFS is a `cdylib` crate built with Cargo:

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
# Linux (requires -ldl for oifs_loaded_path)
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
    fprintf(stderr, "FATAL: Outdated liboifs at '%s' (v%s), need >= v%s\n",
            path, oifs_version_string(), OIFS_VERSION_STRING);
    abort();
} else if (v_status == OIFS_VERSION_COMPAT_WARN) {  // 1: newer lib → WARN, continue
    fprintf(stderr, "WARN: Newer liboifs (v%s) loaded than compiled with (v%s)\n",
            oifs_version_string(), OIFS_VERSION_STRING);
}
```

The policy: **only block older versions; do NOT block newer versions**.

C++ wrapper equivalent:
```cpp
int status = oifs::check_version();
if (status == OIFS_VERSION_COMPAT_ERR) {
    throw std::runtime_error("Outdated liboifs");
}
```

## 4. Filesystem Handle Lifecycle

### Core Types (Opaque)

| C Type | Description |
|--------|-------------|
| `OIFSHandle*` | Open filesystem instance (read-write or read-only) |
| `OIFSReadSession*` | Shared read lock session (amortizes `RwLock` + inode lookups) |
| `OIFSFileHandle*` | Pinned file within a read session (zero-copy mmap capable) |
| `OIFSAsyncFuture*` | In-flight or inline-completed async operation |

### Opening / Creating a Filesystem

| Function | Use Case |
|----------|----------|
| `oifs_open(path, size)` | Open existing or create new unencrypted image |
| `oifs_open_with_password(path, size, password)` | Open/create **encrypted** image (AES-256-GCM) |
| `oifs_open_with_options(path, size, password, storage_mode, journaled)` | Full control: `StorageMode::Mmap` / `FileIo`, optional WAL |
| `oifs_open_read_only(path, password, storage_mode)` | Read-only, shared `F_RDLCK`, multiple processes allowed |
| `oifs_get_or_open(path, size)` | Multi-process "master-proxy" reuse (opens existing or creates) |
| `oifs_get_or_open_with_password(path, size, password)` | Encrypted master-proxy reuse |

All open functions return `NULL` on failure. Retrieve diagnostics via `oifs_last_error`.

### Closing and Flushing

```c
void oifs_close(OIFSHandle *handle);           // Destroys handle; NO implicit flush
int32_t oifs_flush(OIFSHandle *handle);        // Explicit fsync / WAL checkpoint
```

**Important**: `oifs_close` does **not** auto-flush. Call `oifs_flush` before closing if durability matters.

### Durability Modes

```c
oifs_set_durability_mode(handle, OIFS_DURABILITY_LAZY);        // OS page-cache (default)
oifs_set_durability_mode(handle, OIFS_DURABILITY_RANGE_ASYNC); // Batched dirty-range async
oifs_set_durability_mode(handle, OIFS_DURABILITY_STRICT);      // Synchronous msync/barriers
```

### Storage Modes

```c
oifs_open_with_options(..., OIFS_STORAGE_MODE_MMAP, ...);      // Memory-mapped (default)
oifs_open_with_options(..., OIFS_STORAGE_MODE_FILE_IO, ...);   // pread/pwrite, zero mmap
```

### I/O Engine Backend

```c
oifs_set_io_backend(handle, OIFS_IO_BACKEND_MMAP);    // Mmap (default)
oifs_set_io_backend(handle, OIFS_IO_BACKEND_PREAD);   // Positional read
oifs_set_io_backend(handle, OIFS_IO_BACKEND_IO_URING); // io_uring (Linux, falls back to pread)
```

## 5. Directory Operations

```c
// Root directory listing
int32_t oifs_ls(OIFSHandle *handle, ListCallback cb, void *user_data);

// Subdirectory listing
int32_t oifs_ls_dir(OIFSHandle *handle, const char *path, ListCallback cb, void *user_data);

// Create directory
int32_t oifs_mkdir(OIFSHandle *handle, const char *path);
```

Callback signature:
```c
typedef void (*ListCallback)(const char *name, uint64_t size, uint64_t mtime, void *user_data);
```

Returns `0` on success, `-1` on error.

## 6. File Operations & Metadata

### Creation, Deletion, Rename

```c
int32_t oifs_create_file(OIFSHandle *handle, const char *path);
int32_t oifs_create_or_open_file(OIFSHandle *handle, const char *path);
int32_t oifs_delete_file(OIFSHandle *handle, const char *path);
int32_t oifs_rename(OIFSHandle *handle, const char *old_path, const char *new_path);
```

### Metadata

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

int32_t oifs_stat(OIFSHandle *handle, const char *path, OIFSFileStat *out_stat);
int32_t oifs_exists(OIFSHandle *handle, const char *path);  // 1=exists, 0=not, -1=error
int64_t oifs_resolve_path(OIFSHandle *handle, const char *path);  // Returns inode_id or -1
```

### Truncation

```c
int32_t oifs_truncate_file(OIFSHandle *handle, const char *filename, uint64_t new_size);
```

## 7. Synchronous I/O

### Direct Positional Read

```c
int64_t oifs_read_at(OIFSHandle *handle, const char *filename, uint64_t offset,
                     uint8_t *buf, uint64_t buf_size);
// Returns bytes read (>=0), or -1 on error
```

### Sequential Read / Write

```c
int64_t oifs_read_file(OIFSHandle *handle, const char *filename, uint8_t *buf, uint64_t buf_size);
// Equivalent to oifs_read_at(handle, filename, 0, buf, buf_size)

int32_t oifs_write_file(OIFSHandle *handle, const char *filename,
                        const uint8_t *buf, uint64_t buf_size);
// Stream compression (Auto mode) at offset 0; creates file if missing
```

### Extended Write with Policy

```c
#define OIFS_WRITE_POLICY_DEFAULT       0x00  // Auto / Full-file stream compression
#define OIFS_WRITE_POLICY_RAW           0x01  // Never compress
#define OIFS_WRITE_POLICY_STREAM        0x02  // Stream compression (custom zstd_level)
#define OIFS_WRITE_POLICY_SEEKABLE_64K  0x08  // Seekable 64KB chunked compression

int32_t oifs_write_file_with_policy(OIFSHandle *handle, const char *filename,
                                    uint64_t offset, const uint8_t *buf, uint64_t buf_size,
                                    uint32_t policy_flags, int32_t zstd_level);
```

### Single-Transaction Create + Write (Atomic)

```c
int64_t oifs_create_file_with_data(OIFSHandle *handle, const char *path,
                                   const uint8_t *buf, uint64_t buf_size,
                                   uint32_t policy_flags, int32_t zstd_level);
// Returns new inode_id (>=0) or -1
```

### Inode-Level Direct Read (No Path Resolution)

```c
int64_t oifs_read_at_inode(OIFSHandle *handle, uint64_t inode_id,
                           uint64_t offset, uint8_t *buf, uint64_t buf_size);
```

### Scatter-Gather Batch Read

```c
typedef struct {
    uint64_t inode_id;
    uint64_t offset;
    uint8_t *buf;
    uint64_t buf_size;
    int64_t result;  // Output: bytes read or -1
} OIFSReadBatchItem;

int32_t oifs_read_at_batch(OIFSHandle *handle, OIFSReadBatchItem *items, size_t count);
// Executes all reads under single read lock + single I/O submission
// Returns 0 if all succeeded, -1 if any failed
```

## 8. Shared Read Session & Zero-Copy Mmap

**Purpose**: Amortize `RwLock` acquisition and inode lookup across many reads.

```c
// 1. Begin session (acquires shared read lock)
OIFSReadSession *session = oifs_begin_read(handle);

// 2. Open file(s) by path or inode within the session
OIFSFileHandle *file = oifs_session_open_file(session, "data/large.bin");
OIFSFileHandle *file2 = oifs_session_open_inode(session, inode_id);

// 3. Positional read (zero RwLock, zero inode lookup)
int64_t n = oifs_file_read_at(file, offset, buf, buf_size);

// 4. TRUE ZERO-COPY mmap slice
const uint8_t *ptr = NULL;
uint64_t len = 0;
int32_t rc = oifs_file_mmap_slice(file, offset, max_len, &ptr, &len);
// rc == 0: zero-copy succeeded, ptr/len valid while file/session open
// rc == 1: zero-copy unavailable (FileIo, compressed, encrypted, sparse, non-contiguous)
// rc == -1: error

// 5. Cleanup
oifs_file_close(file);
oifs_end_read(session);
```

**Lifetime Safety**: `OIFSFileHandle` internally holds an `Arc` to the session. Destroying the `OIFSReadSession` before `OIFSFileHandle` is safe—mapping stays valid.

## 9. PinnableSlice (RocksDB-Style Zero-Copy / 1-Copy Fallback)

**C++ Only** (`oifs::PinnableSlice` in `include/oifs.hpp`):

```cpp
oifs::ReadSession session = fs.begin_read();
oifs::FileReadHandle file = session.open_file("data.bin");

// Try zero-copy first
auto opt = file.try_mmap_view(0, 1_MiB);
if (opt) {
    // opt->data() points directly into mmap, zero copies
    process(opt->data(), opt->size());
} else {
    // Automatic 1-copy fallback into internal buffer
    oifs::PinnableSlice slice;
    file.read_pinnable(0, 1_MiB, slice);  // slice.is_pinned() == false
    process(slice.data(), slice.size());
}
```

## 10. Asynchronous I/O

### Future / Poll API

```c
// Submit async read
OIFSAsyncFuture *fut = oifs_read_at_async(handle, inode_id, offset, buf, buf_size);

// Submit async single-transaction create+write
OIFSAsyncFuture *fut = oifs_create_file_with_data_async(handle, path, buf, buf_size,
                                                        policy_flags, zstd_level);

// Submit async write at offset
OIFSAsyncFuture *fut = oifs_write_file_async(handle, path, offset, buf, buf_size,
                                             policy_flags, zstd_level);

// Submit async flush
OIFSAsyncFuture *fut = oifs_flush_async(handle);

// Poll (non-blocking): 1=ready, 0=pending, -1=invalid
int32_t ready = oifs_async_poll(fut);

// Block until completion, return result (bytes_read / inode_id / 0) or -1
int64_t result = oifs_async_wait(fut);

// Get error string on failure
char err[256]; oifs_async_last_error(fut, err, sizeof(err));

// Release future (drop barrier: waits for background write into buf to finish)
oifs_async_free(fut);
```

**Key Properties**:

| Property | Description |
|----------|-------------|
| **Inline Fast-Path** | Hot cache/mmap reads complete immediately on caller thread (`poll==1`) |
| **Group Commit** | Concurrent async writes under `DURABILITY_STRICT` share a single sync barrier |
| **Use-After-Free Safe** | `oifs_async_free` blocks until any in-flight write to caller's `buf` completes |

### Callback API

```c
// Async read with callback
typedef void (*OifsAsyncReadCallback)(int64_t status_or_len,
                                      const uint8_t *data, size_t data_len,
                                      void *user_data);

int32_t oifs_read_at_async_cb(OIFSHandle *handle, uint64_t inode_id,
                              uint64_t offset, uint64_t len,
                              OifsAsyncReadCallback cb, void *user_data);

// Async create+write with callback
typedef void (*OifsAsyncStatusCallback)(int64_t status_or_inode, void *user_data);

int32_t oifs_create_file_with_data_async_cb(OIFSHandle *handle, const char *path,
                                            const uint8_t *buf, uint64_t buf_size,
                                            uint32_t policy_flags, int32_t zstd_level,
                                            OifsAsyncStatusCallback cb, void *user_data);
```

**Callback Rules**: The `data` pointer in read callback is valid only for the callback duration. Never throw C++ exceptions across the extern "C" boundary.

## 11. Cross-Image Operations

### Whole-Image CoW Clone

```c
#define OIFS_CLONE_METHOD_APFS_CLONEFILE    0  // macOS APFS
#define OIFS_CLONE_METHOD_LINUX_FICLONE     1  // Linux FICLONE ioctl
#define OIFS_CLONE_METHOD_STANDARD_COPY     2  // Fallback

int32_t oifs_clone_image(const char *src_path, const char *dst_path, uint8_t *out_method);
// Creates dst_path using fastest OS CoW primitive
```

### 3-Tier File Transfer (Reflink → copy_file_range → Raw Block → Logical Re-encode)

```c
#define OIFS_TRANSFER_FLAG_MOVE      0x01
#define OIFS_TRANSFER_FLAG_OVERWRITE 0x02

typedef struct { ... } OIFSTransferStats;

int32_t oifs_transfer_file_to(OIFSHandle *src_handle, const char *src_path,
                              OIFSHandle *dst_handle, const char *dst_path,
                              uint32_t flags, OIFSTransferStats *out_stats);
```

### Sub-Range Splice

```c
#define OIFS_SPLICE_TRIM_KEEP          0
#define OIFS_SPLICE_TRIM_PUNCH_HOLE    1
#define OIFS_SPLICE_TRIM_COLLAPSE      2

typedef struct { ... } OIFSSpliceStats;

int32_t oifs_splice_range_to(OIFSHandle *src_handle, uint64_t src_inode, uint64_t src_off,
                             OIFSHandle *dst_handle, uint64_t dst_inode, uint64_t dst_off,
                             uint64_t len, uint8_t trim_mode, OIFSSpliceStats *out_stats);
```

## 12. Online Maintenance & Diagnostics

```c
typedef struct { ... } OIFSFsckReport;
typedef struct { ... } OIFSFragmentationStats;
typedef struct { ... } OIFSDefragStats;

int32_t oifs_verify_integrity(OIFSHandle *handle, OIFSFsckReport *out_report);     // fsck
int32_t oifs_analyze_fragmentation(OIFSHandle *handle, OIFSFragmentationStats *out_stats);
int32_t oifs_defragment(OIFSHandle *handle, OIFSDefragStats *out_stats);            // In-place defrag
```

## 13. FUSE Kernel Protocol Dispatch

```c
int64_t oifs_fuse_dispatch_frame(OIFSHandle *handle,
                                 const uint8_t *req_buf, size_t req_len,
                                 uint8_t *resp_buf, size_t resp_cap);
// Dispatches raw FUSE Kernel ABI v7.28 request frame
// File handles (fh) persist across calls on the same OIFSHandle
// Read-only handles reject mutating opcodes with -EROFS
```

## 14. SIMD Numeric Pre-Compression Filters

```c
typedef struct {
    uint8_t typesize;   // 1,2,4,8 (0 = disabled)
    uint8_t delta;      // 1 = enable delta encoding
    uint8_t shuffle;    // 1 = enable byte shuffle
    uint8_t bitshuffle; // 1 = enable bit shuffle
} OIFSFilterConfig;

int64_t oifs_create_file_with_data_filtered(OIFSHandle *handle, const char *path,
                                            const uint8_t *buf, uint64_t buf_size,
                                            uint32_t policy_flags, int32_t zstd_level,
                                            const OIFSFilterConfig *filter_cfg);

int32_t oifs_write_file_with_filters(OIFSHandle *handle, const char *path, uint64_t offset,
                                     const uint8_t *buf, uint64_t buf_size,
                                     uint32_t policy_flags, int32_t zstd_level,
                                     const OIFSFilterConfig *filter_cfg);
```

Pass `NULL` for `filter_cfg` to disable all filters.

## 15. Error Handling

Every FFI function that can fail provides diagnostics via:

```c
int32_t oifs_last_error(OIFSHandle *handle, char *buf, uint32_t buf_size);
// Returns 0 on success, -1 on error. Writes NUL-terminated message to buf.

int32_t oifs_async_last_error(OIFSAsyncFuture *fut, char *buf, uint32_t buf_size);
// For async futures
```

**Conventions**:
- Return codes: `0` = success, `-1` = error (or positive byte counts for reads)
- `NULL` handle returns `-1` immediately (no error message)
- All FFI entry points catch Rust panics and convert to error returns
- C++ wrapper (`oifs.hpp`) throws `oifs::Error` (derived from `std::runtime_error`)

## 16. Thread Safety

**`OIFSHandle` is NOT thread-safe**. Each thread requiring filesystem access should either:
1. Open its own handle (`oifs_open` / `oifs_open_read_only`), or
2. Implement external synchronization (mutex) around a shared handle.

`OIFSReadSession` and `OIFSFileHandle` are similarly not thread-safe. The async API is safe for concurrent submission from multiple threads because each call creates its own `OIFSAsyncFuture`.

## 17. Minimal Embedding Examples

### C (Complete Lifecycle)

```c
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include "oifs.h"

int main() {
    // 1. Version handshake
    if (OIFS_CHECK_VERSION() == OIFS_VERSION_COMPAT_ERR) return 1;

    // 2. Open filesystem
    OIFSHandle *fs = oifs_open("test.img", 16 * 1024 * 1024);
    if (!fs) { fprintf(stderr, "open failed\n"); return 1; }

    // 3. Create directory and file
    oifs_mkdir(fs, "docs");
    oifs_create_file(fs, "docs/note.txt");

    // 4. Write data
    const char *msg = "Hello OIFS FFI";
    oifs_write_file(fs, "docs/note.txt", (const uint8_t*)msg, strlen(msg));

    // 5. Read back (chunked)
    uint8_t buf[128] = {0};
    int64_t n = oifs_read_at(fs, "docs/note.txt", 0, buf, sizeof(buf));
    printf("Read %lld bytes: %.*s\n", n, (int)n, buf);

    // 6. List directory
    struct { int count; } ctx = {0};
    auto cb = [](const char *name, uint64_t sz, uint64_t mt, void *ud) {
        ((int*)ud)[0]++; (void)name; (void)sz; (void)mt;
    };
    oifs_ls(fs, (ListCallback)cb, &ctx);

    // 7. Cleanup
    oifs_flush(fs);
    oifs_close(fs);
    return 0;
}
```

Compile:
```bash
clang -std=c11 -Wall -Wextra -Wpedantic -Werror \
      -I/path/to/oifs/include \
      main.c /path/to/oifs/target/release/liboifs.so -o main \
      -ldl  # Linux only
LD_LIBRARY_PATH=/path/to/oifs/target/release ./main
```

### C++17 (RAII Wrapper)

```cpp
#include <iostream>
#include <vector>
#include <string>
#include <memory>
#include <thread>
#include "oifs.hpp"

int main() {
    // Version handshake
    if (oifs::check_version() == OIFS_VERSION_COMPAT_ERR) return 1;

    // RAII FileSystem handle
    oifs::FileSystem fs = oifs::FileSystem::open("test.img", 20 * 1024 * 1024);

    // Write using std::vector<uint8_t>
    std::string text = "C++17 RAII Test";
    std::vector<uint8_t> data(text.begin(), text.end());
    fs.write_file("cpp_test.txt", data);

    // Read back
    std::vector<uint8_t> read_buf = fs.read_file("cpp_test.txt");
    assert(read_buf == data);

    // Lambda callback for directory listing
    std::vector<oifs::FileSystem::DirEntry> entries;
    auto cb = [](const char *name, uint64_t sz, uint64_t mt, void *ud) {
        auto *vec = static_cast<decltype(entries)*>(ud);
        vec->push_back({name ? name : "", sz, mt});
    };
    fs.ls(cb, &entries);

    // Multi-threaded concurrent access (each thread opens own handle)
    std::vector<std::thread> workers;
    for (int t = 0; t < 4; ++t) {
        workers.emplace_back([t] {
            oifs::FileSystem local_fs = oifs::FileSystem::open("test.img", 0);
            std::string fname = "worker_" + std::to_string(t) + ".bin";
            std::vector<uint8_t> payload = {uint8_t(t), 0xAA, 0x55};
            local_fs.write_file(fname, payload);
        });
    }
    for (auto &th : workers) th.join();

    std::cout << "C++ test passed\n";
    return 0;
}
```

Compile:
```bash
clang++ -std=c++17 -Wall -Wextra -Wpedantic -Werror \
        -I/path/to/oifs/include \
        main.cpp /path/to/oifs/target/release/liboifs.so -o main
```

### C++20 (Coroutines + std::span)

```cpp
#include <span>
#include <coroutine>
#include "oifs.hpp"

oifs::AsyncFuture read_async(oifs::FileSystem &fs, uint64_t inode, uint64_t off, size_t sz) {
    std::vector<uint8_t> buf(sz);
    co_return co_await fs.read_at_async(inode, off, oifs::MutableByteSpan(buf));
}

int main() {
    oifs::FileSystem fs = oifs::FileSystem::open("test.img", 16 * 1024 * 1024);
    // Hot path (mmap cache hit): co_await resumes immediately, zero thread hop
    auto fut = read_async(fs, some_inode, 0, 4096);
    std::size_t n = fut.wait();  // or co_await fut in another coroutine
}
```

Requires `-std=c++20` and compiler support for `<coroutine>`.

## 18. Common Pitfalls

| Pitfall | Solution |
|---------|----------|
| Forgetting `oifs_flush` before `oifs_close` | Always call `oifs_flush` explicitly when durability matters |
| Using `OIFSHandle` across threads | Open per-thread handles or guard with mutex |
| Passing non-NULL buffer with zero size to write | Validated as error; pass `NULL` for empty writes |
| Expecting async callback data to persist | Data valid only within callback; copy if needed |
| Not checking `OIFS_CHECK_VERSION` at startup | Required—blocks silently on older liboifs |
| Assuming `oifs_file_mmap_slice` always succeeds | Check return code: `1` = fallback to `oifs_file_read_at` |
| Destroying `OIFSReadSession` before `OIFSFileHandle` | Safe—Arc keeps mmap alive; but prefer ordered cleanup |

## 19. Formal Verification

The FFI layer includes [Kani](https://github.com/model-checking/kani) proofs in [`src/ffi.rs`](repo://src/ffi.rs#L3180-L3230):
- `oifs_check_version` policy soundness (never blocks newer, always blocks older)
- `policy_to_compression_mode` totality and precedence for all symbolic inputs

Run with:
```bash
cargo kani --harness proof_oifs_check_version_policy_soundness
cargo kani --harness proof_policy_to_compression_mode_total_and_panic_free
```

---

*For the C++ RAII wrapper API reference, see `include/oifs.hpp`. For the stable C ABI, see `include/oifs.h`.*
