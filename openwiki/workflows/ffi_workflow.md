---
type: workflow
title: FFI Usage Workflow
description: Step-by-step guide to using the C FFI interface for integrating OIFS with C/C++ applications, covering version handshake, lifecycle management, file/directory operations, error handling, encryption, I/O backend configuration, session reuse, async I/O, zero-copy reads, cross-image transfers, SIMD filters, diagnostics, and FUSE frame dispatch.
tags: [ffi, c, integration, workflow, versioning, encryption, io-backend, async, zero-copy, fuse, diagnostics]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-24T19:09:23.676Z
sources:
  - id: openwiki-source-c9e5b32aad7cafdb095c81a4
    resource: repo://src/ffi.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-24T19:09:23.676Z" }
---

# FFI Usage Workflow

This document outlines the complete workflow for using the OIFS C FFI interface from a C or C++ application. The FFI bindings are defined in [include/oifs.h](repo://include/oifs.h) and implemented in [src/ffi.rs](repo://src/ffi.rs). The interface provides a stable C ABI for filesystem operations, encryption, I/O backend selection, version compatibility checks, asynchronous I/O, zero-copy reads, cross-image copy-on-write transfers, SIMD pre-compression filters, online diagnostics, and FUSE kernel protocol dispatch.

## 1. Include the Header and Link the Library

Include the OIFS header in your C/C++ source:

```c
#include "oifs.h"
```

Link against the compiled dynamic library at build time:
- Linux: `liboifs.so` (requires `-ldl` for `oifs_loaded_path`)
- macOS: `liboifs.dylib`
- Windows: `oifs.dll`

```bash
# GCC/Clang example
gcc -std=c11 -Wall -Wextra -I/path/to/oifs/include main.c -L/path/to/oifs/target/release -loifs -ldl -o main
```

## 2. Version Handshake at Startup

**Always perform a version check immediately after loading the library** to detect ABI mismatches. The header embeds compile-time version constants (`OIFS_VERSION_MAJOR`, `OIFS_VERSION_MINOR`, `OIFS_VERSION_PATCH`, `OIFS_VERSION_STRING`, `OIFS_VERSION_CODE`). The runtime library exposes matching getters.

```c
int main(void) {
    // 1. Version handshake using convenience macro
    int v_status = OIFS_CHECK_VERSION();
    
    if (v_status == OIFS_VERSION_COMPAT_ERR) {
        // Loaded library is OLDER than headers: FATAL, abort
        char lib_path[512] = {0};
        oifs_loaded_path(lib_path, sizeof(lib_path));
        fprintf(stderr, "FATAL: Outdated liboifs at '%s' (v%s), need >= v%s\n",
                lib_path, oifs_version_string(), OIFS_VERSION_STRING);
        return 1;
    } else if (v_status == OIFS_VERSION_COMPAT_WARN) {
        // Loaded library is NEWER: allowed, but warn
        fprintf(stderr, "WARN: Newer liboifs (v%s) than headers (v%s)\n",
                oifs_version_string(), OIFS_VERSION_STRING);
    }
    // v_status == OIFS_VERSION_COMPAT_OK: exact match, clean
    
    // Optional: Query individual version components
    printf("Running against liboifs v%u.%u.%u\n",
           oifs_version_major(), oifs_version_minor(), oifs_version_patch());
           
    // ... continue with filesystem operations
}
```

**Version Policy**: "Only block older versions; do NOT block newer versions."
- `OIFS_VERSION_COMPAT_OK` (0): Exact version match
- `OIFS_VERSION_COMPAT_WARN` (1): Newer library loaded (permitted)
- `OIFS_VERSION_COMPAT_ERR` (-1): Older library loaded (blocked)

You can also retrieve the absolute filesystem path from which the dynamic library was loaded via `oifs_loaded_path(buf, buf_size)`.

## 3. Lifecycle Management

### Opening a Filesystem Image

Use `oifs_open` to open an existing unencrypted image or create a new one:

```c
// Create new 10 MB image or open existing
OIFSHandle* handle = oifs_open("/path/to/image.img", 10 * 1024 * 1024);
if (handle == NULL) {
    // Handle error via oifs_last_error (see §15)
}
```

The `size` parameter (bytes) is **only used when creating a new image**; it is ignored when opening an existing image.

### Opening an Encrypted Image

Use `oifs_open_with_password` for encrypted filesystems. The passphrase is processed with Argon2id using the superblock salt to derive a 256-bit AEAD key.

```c
// Create new encrypted image or open existing encrypted image
OIFSHandle* handle = oifs_open_with_password("/path/to/encrypted.img", 
                                              10 * 1024 * 1024, 
                                              "my_secure_password");
if (handle == NULL) {
    // Handle error
}
```

### Advanced Open Options

Use `oifs_open_with_options` for explicit storage mode (`Mmap` or `FileIo`) and optional metadata WAL journaling:

```c
// Open with FileIo backend and journaling enabled
OIFSHandle* handle = oifs_open_with_options("/path/to/image.img",
                                             10 * 1024 * 1024,
                                             "password",     // or NULL for unencrypted
                                             OIFS_STORAGE_MODE_FILE_IO,
                                             1);             // journaled != 0
```

### Read-Only Open

Use `oifs_open_read_only` to open an existing image with a shared `F_RDLCK` lock. This never opens the backing file for writing and supports multiple concurrent readers on `0444` images:

```c
OIFSHandle* handle = oifs_open_read_only("/path/to/image.img",
                                          "password",        // or NULL
                                          OIFS_STORAGE_MODE_MMAP);
if (handle == NULL) { /* error */ }

// Check read-only status
int ro = oifs_is_read_only(handle);  // 1 = read-only, 0 = read-write, -1 = error
```

### Session Reuse (Process-Level Deduplication)

`oifs_get_or_open` and `oifs_get_or_open_with_password` reuse an existing `DiskManager` for the same image path within the process, avoiding duplicate mmap/file handles:

```c
// Returns existing handle if image already opened in this process
OIFSHandle* handle = oifs_get_or_open("/path/to/image.img", 0);
```

### Closing a Handle

```c
oifs_close(handle);
```

### Flushing and Durability

```c
// Flush all pending writes and WAL transactions
oifs_flush(handle);

// Set durability mode: LAZY (0), RANGE_ASYNC (1), STRICT (2)
oifs_set_durability_mode(handle, OIFS_DURABILITY_STRICT);

// Tune FileIo read cache size
oifs_set_file_io_cache_bytes(handle, 64 * 1024 * 1024);
```

## 4. Directory Operations

### List Root Directory

```c
void list_cb(const char* name, uint64_t size, uint64_t mtime, void* user_data) {
    printf("Entry: %s, size=%llu, mtime=%llu\n", name, size, mtime);
}

int rc = oifs_ls(handle, list_cb, NULL);  // 0 on success, -1 on error
```

### List Arbitrary Subdirectory

```c
int rc = oifs_ls_dir(handle, "/subdir/path", list_cb, NULL);
```

### Create Directory

```c
int rc = oifs_mkdir(handle, "/new/subdir");
```

## 5. File Operations

### Create Empty File

```c
int rc = oifs_create_file(handle, "/path/to/file");
```

### Create or Open Existing File

```c
int rc = oifs_create_or_open_file(handle, "/path/to/file");
```

### Delete File

```c
int rc = oifs_delete_file(handle, "/path/to/file");
```

### Rename / Move

```c
int rc = oifs_rename(handle, "/old/path", "/new/path");
```

### Sequential Read (entire file from offset 0)

```c
uint8_t buf[4096];
int64_t bytes = oifs_read_file(handle, "/path/to/file", buf, sizeof(buf));
if (bytes < 0) { /* error */ }
```

### Positional Read (arbitrary offset)

```c
uint8_t buf[4096];
int64_t bytes = oifs_read_at(handle, "/path/to/file", 1024, buf, sizeof(buf));
```

### Write Full File (create or overwrite from offset 0)

```c
uint8_t data[] = "Hello, OIFS!";
int rc = oifs_write_file(handle, "/path/to/file", data, sizeof(data));
```

### Positional Write with Policy

```c
// Write at offset with explicit compression policy
int rc = oifs_write_file_with_policy(handle, "/path/to/file", 1024,
                                      data, sizeof(data),
                                      OIFS_WRITE_POLICY_STREAM,  // stream compression
                                      3);                         // zstd level 3
```

**Write Policy Flags:**
- `OIFS_WRITE_POLICY_DEFAULT` (0x00): Auto / full-file stream compression
- `OIFS_WRITE_POLICY_RAW` (0x01): Never compress
- `OIFS_WRITE_POLICY_STREAM` (0x02): Stream compression with custom zstd level
- `OIFS_WRITE_POLICY_SEEKABLE_64K` (0x08): Seekable 64KB chunked compression

### Truncate / Extend File

```c
// Truncate to 100 bytes or extend sparsely
int rc = oifs_truncate_file(handle, "/path/to/file", 100);
```

## 6. Atomic Single-Transaction File Creation

Create a file and write its initial payload in a single transaction (no intermediate visible state):

```c
int64_t inode_id = oifs_create_file_with_data(handle, "/path/to/file",
                                               data, sizeof(data),
                                               OIFS_WRITE_POLICY_DEFAULT, 0);
if (inode_id < 0) { /* error */ }
```

## 7. Direct Inode Operations (Path Resolution Bypass)

### Resolve Path to Inode ID

```c
int64_t inode_id = oifs_resolve_path(handle, "/path/to/file");
```

### Read by Inode ID (no path lookup)

```c
int64_t bytes = oifs_read_at_inode(handle, inode_id, offset, buf, buf_size);
```

### Scatter-Gather Batch Read

```c
OIFSReadBatchItem items[3] = {
    { .inode_id = id1, .offset = 0,   .buf = buf1, .buf_size = 4096 },
    { .inode_id = id2, .offset = 100, .buf = buf2, .buf_size = 2048 },
    { .inode_id = id3, .offset = 0,   .buf = buf3, .buf_size = 8192 },
};
int rc = oifs_read_at_batch(handle, items, 3);
// Each items[i].result contains bytes read (>=0) or -1 on per-item error
```

## 8. Shared Read Session & Zero-Copy Mmap Access

For high-throughput read workloads, use a shared read session to amortize `RwLock` acquisition and inode lookups. File handles pinned from a session support true zero-copy mmap slices.

### Begin / End Read Session

```c
OIFSReadSession* session = oifs_begin_read(handle);
if (!session) { /* error */ }

// ... use session ...

oifs_end_read(session);
```

### Open File Handle from Session (by path or inode)

```c
OIFSFileHandle* file = oifs_session_open_file(session, "/path/to/file");
// or
OIFSFileHandle* file = oifs_session_open_inode(session, inode_id);
```

### Read from Pinned File Handle

```c
// Positional read (zero RwLock, zero inode lookup)
int64_t bytes = oifs_file_read_at(file, offset, buf, buf_size);

// Query file metadata
uint64_t size = oifs_file_size(file);
uint64_t inode_id = oifs_file_inode_id(file);

oifs_file_close(file);
```

### True Zero-Copy Mmap Slice

```c
const uint8_t* ptr = NULL;
uint64_t len = 0;

// Returns 0 = zero-copy OK (ptr/len valid while file/session open)
//          1 = zero-copy unavailable (fall back to oifs_file_read_at)
//         -1 = error
int rc = oifs_file_mmap_slice(file, offset, max_len, &ptr, &len);
if (rc == 0) {
    // Direct access to memory-mapped bytes: process ptr[0..len-1]
} else if (rc == 1) {
    // Fallback: use oifs_file_read_at
}
```

Zero-copy succeeds only for uncompressed, unencrypted files on `Mmap` storage mode at contiguous block boundaries. On `FileIo`, compressed/encrypted, sparse holes, or non-contiguous ranges it returns `1`.

## 9. Asynchronous I/O

The async API uses an **Inline Fast-Path**: cache/mmap hits complete immediately on the calling thread (`oifs_async_poll` returns `1`), while cold reads and compressed/encrypted decodes execute on a persistent background pool. Write operations use **Group Commit**: concurrent async writes under `OIFS_DURABILITY_STRICT` share a single physical sync barrier per batch.

### Future-Based API

```c
// Async read
OIFSAsyncFuture* fut = oifs_read_at_async(handle, inode_id, offset, buf, buf_size);
if (!fut) { /* error */ }

// Poll without blocking: 1 = ready, 0 = pending, -1 = invalid
while (oifs_async_poll(fut) == 0) { /* do other work */ }

// Wait for completion and get result (bytes read or -1 on error)
int64_t result = oifs_async_wait(fut);

// Retrieve error message on failure
if (result < 0) {
    char errbuf[256];
    oifs_async_last_error(fut, errbuf, sizeof(errbuf));
    fprintf(stderr, "Async read failed: %s\n", errbuf);
}

// Release future (waits for in-flight write to caller's buffer if needed)
oifs_async_free(fut);
```

### Async Write / Create

```c
// Async create file with data (Group Commit)
OIFSAsyncFuture* fut = oifs_create_file_with_data_async(handle, "/path",
                                                         data, len,
                                                         OIFS_WRITE_POLICY_STREAM, 3);

// Async write at offset
OIFSAsyncFuture* fut = oifs_write_file_async(handle, "/path", offset,
                                              data, len,
                                              OIFS_WRITE_POLICY_RAW, 0);

// Async flush
OIFSAsyncFuture* fut = oifs_flush_async(handle);
```

### Callback-Based API

```c
void read_cb(int64_t status_or_len, const uint8_t* data, size_t data_len, void* user_data) {
    if (status_or_len >= 0) {
        // data[0..data_len-1] valid for callback duration
    } else {
        // Error
    }
}

int rc = oifs_read_at_async_cb(handle, inode_id, offset, len,
                                read_cb, user_data);

void create_cb(int64_t status_or_inode, void* user_data) {
    if (status_or_inode >= 0) {
        // File created with inode_id = status_or_inode
    }
}

int rc = oifs_create_file_with_data_async_cb(handle, "/path", data, len,
                                              policy, zstd_level,
                                              create_cb, user_data);
```

**Use-After-Free Safety**: `oifs_async_free` blocks until any background task writing into the caller's buffer completes, guaranteeing the buffer is never accessed after `oifs_async_free` returns.

## 10. I/O Backend Configuration

Switch the I/O engine backend at runtime:

```c
// Backends: 0 = Mmap, 1 = Pread, 2 = IoUring (Linux, falls back to Pread)
oifs_set_io_backend(handle, OIFS_IO_BACKEND_IO_URING);

// Query current backend
int backend = oifs_get_io_backend(handle);
```

## 11. Cross-Image CoW Clone, Transfer & Splice

### Clone Entire Image (CoW)

```c
uint8_t method = 0;
int rc = oifs_clone_image("/src.img", "/dst.img", &method);
// method: 0=APFS clonefile, 1=Linux FICLONE, 2=Standard copy
```

### Transfer File Between Images (3-Tier Fast Path)

```c
OIFSTransferStats stats = {0};
int rc = oifs_transfer_file_to(src_handle, "/src/file",
                                dst_handle, "/dst/file",
                                OIFS_TRANSFER_FLAG_OVERWRITE,
                                &stats);
// Stats: logical_bytes, physical_bytes, extents_transferred, method
```

**Transfer Methods:** Reflink → CopyFileRange → RawBlockCopy → LogicalReencode (across encryption boundaries)

### Splice Sub-Range Between Files

```c
OIFSSpliceStats stats = {0};
int rc = oifs_splice_range_to(src_handle, src_inode, src_off,
                               dst_handle, dst_inode, dst_off,
                               len,
                               OIFS_SPLICE_TRIM_PUNCH_HOLE,  // trim mode
                               &stats);
```

**Trim Modes:** `KEEP` (0), `PUNCH_HOLE` (1), `COLLAPSE` (2)

## 12. SIMD Numeric Pre-Compression Filters

Apply fast SIMD filters (delta, shuffle, bitshuffle) before compression for numerical data:

```c
OIFSFilterConfig cfg = {
    .typesize = 4,      // element size: 1, 2, 4, or 8 (0 = disabled)
    .delta = 1,         // first-order delta encoding
    .shuffle = 1,       // byte shuffle
    .bitshuffle = 0     // bit shuffle
};

// Atomic create with filters
int64_t inode_id = oifs_create_file_with_data_filtered(handle, "/path",
                                                        data, len,
                                                        OIFS_WRITE_POLICY_STREAM, 3,
                                                        &cfg);

// Write with filters at offset
int rc = oifs_write_file_with_filters(handle, "/path", offset,
                                       data, len,
                                       OIFS_WRITE_POLICY_STREAM, 3,
                                       &cfg);
```

Pass `NULL` for `filter_cfg` to disable all filters.

## 13. File Metadata & Existence

### Stat (Inode Metadata)

```c
OIFSFileStat stat = {0};
int rc = oifs_stat(handle, "/path/to/file", &stat);
// stat: inode_id, size, compressed_size, created_at, modified_at,
//       is_dir, encrypted, seekable_64k, filter_typesize, filter_delta,
//       filter_shuffle, filter_bitshuffle
```

### Exists Check

```c
int exists = oifs_exists(handle, "/path/to/file");  // 1 = yes, 0 = no, -1 = error
```

## 14. FUSE Kernel Protocol Frame Dispatch

Dispatch raw FUSE ABI v7.28 request frames for FUSE filesystem implementations:

```c
uint8_t req_buf[4096];
uint8_t resp_buf[4096];
// ... fill req_buf with fuse_in_header + payload ...

int64_t resp_len = oifs_fuse_dispatch_frame(handle, req_buf, req_len,
                                             resp_buf, sizeof(resp_buf));
if (resp_len < 0) { /* error */ }
// resp_buf contains fuse_out_header + response body
```

- File handles (`fh`) opened via `FUSE_OPEN`/`FUSE_CREATE`/`FUSE_OPENDIR` persist across calls on the same `OIFSHandle`.
- Read-only handles automatically reject mutating opcodes with `-EROFS`.

## 15. Error Handling

All FFI functions set a thread-local error message on failure. Retrieve it via:

```c
char errbuf[512];
int rc = oifs_last_error(handle, errbuf, sizeof(errbuf));
if (rc == 0) {
    fprintf(stderr, "Error: %s\n", errbuf);
}
```

The error buffer is overwritten on each call; call `oifs_last_error` immediately after a failing function.

## 16. Online Maintenance & Diagnostics

### Integrity Verification (fsck)

```c
OIFSFsckReport report = {0};
int rc = oifs_verify_integrity(handle, &report);
// report: is_clean, orphan_inodes, leaked_blocks, missing_blocks, cross_linked_blocks
```

### Fragmentation Analysis

```c
OIFSFragmentationStats stats = {0};
int rc = oifs_analyze_fragmentation(handle, &stats);
// stats: total_blocks, used_blocks, free_blocks, free_runs,
//        largest_free_run, avg_gap_size, fragmentation_ratio
```

### Online Defragmentation

```c
OIFSDefragStats stats = {0};
int rc = oifs_defragment(handle, &stats);
// stats: files_processed, bytes_moved, blocks_freed, frag_before, frag_after
```

- Requires read-write handle.
- On journaled images: WAL-atomic COW.
- On non-journaled: cycle-aware permutation.

## 17. Complete Workflow Example

```c
#include "oifs.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

void list_cb(const char* name, uint64_t size, uint64_t mtime, void* ud) {
    (void)ud;
    printf("  %s (%llu bytes)\n", name, size);
}

int main(void) {
    // 1. Version handshake
    int v = OIFS_CHECK_VERSION();
    if (v == OIFS_VERSION_COMPAT_ERR) {
        char path[512] = {0};
        oifs_loaded_path(path, sizeof(path));
        fprintf(stderr, "FATAL: Outdated liboifs at '%s'\n", path);
        return 1;
    }

    // 2. Open encrypted image with journaling
    OIFSHandle* h = oifs_open_with_options("data.img", 50*1024*1024,
                                            "secret123",
                                            OIFS_STORAGE_MODE_MMAP,
                                            1);
    if (!h) {
        char err[256]; oifs_last_error(h, err, sizeof(err));
        fprintf(stderr, "Open failed: %s\n", err);
        return 1;
    }

    // 3. Create directory and file
    oifs_mkdir(h, "/logs");
    const char* msg = "Hello, OIFS FFI!";
    oifs_write_file_with_policy(h, "/logs/hello.txt", 0,
                                 (uint8_t*)msg, strlen(msg),
                                 OIFS_WRITE_POLICY_STREAM, 3);

    // 4. Read back
    char buf[256];
    int64_t n = oifs_read_file(h, "/logs/hello.txt", (uint8_t*)buf, sizeof(buf)-1);
    if (n > 0) { buf[n] = '\0'; printf("Read: %s\n", buf); }

    // 5. List directory
    printf("Directory /logs:\n");
    oifs_ls_dir(h, "/logs", list_cb, NULL);

    // 6. Cleanup
    oifs_close(h);
    return 0;
}
```

---

This workflow covers the complete surface area of the OIFS C FFI as of the current implementation. For the canonical C declarations, refer to [include/oifs.h](repo://include/oifs.h); for implementation details, see [src/ffi.rs](repo://src/ffi.rs).
