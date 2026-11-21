---
type: workflow
title: FFI Usage Workflow
description: Step-by-step guide to using the C FFI interface for integrating OIFS with C/C++ applications, covering version handshake, lifecycle management, file/directory operations, error handling, encryption, I/O backend configuration, and session reuse.
tags: [ffi, c, integration, workflow, versioning, encryption, io-backend]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-c9e5b32aad7cafdb095c81a4
    resource: repo://src/ffi.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---

# FFI Usage Workflow

This document outlines the complete workflow for using the OIFS C FFI interface from a C or C++ application. The FFI bindings are defined in [include/oifs.h](repo://include/oifs.h) and implemented in [src/ffi.rs](repo://src/ffi.rs). The interface provides a stable C ABI for filesystem operations, encryption, I/O backend selection, and version compatibility checks.

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

## 3. Lifecycle Management

### Opening a Filesystem Image

Use `oifs_open` to open an existing unencrypted image or create a new one:

```c
// Create new 10 MB image or open existing
OIFSHandle* handle = oifs_open("/path/to/image.img", 10 * 1024 * 1024);
if (handle == NULL) {
    // Handle error via oifs_last_error (see §6)
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

Behavior:
- If the file **does not exist** and a **non-empty password** is provided → creates a new encrypted image
- If the file **exists** → opens it with password verification (empty password for unencrypted, correct password for encrypted)
- Returns `NULL` on wrong password or corruption

### Session Handle Reuse (Multi-Process / IPC)

To obtain a handle to an already-open image (avoiding multiple opens and enabling IPC sharing), use the session registry functions:

```c
// Returns existing handle if image is already open in this process, otherwise opens/creates
OIFSHandle* handle = oifs_get_or_open("/path/to/image.img", 10 * 1024 * 1024);

// With password for encrypted images
OIFSHandle* handle = oifs_get_or_open_with_password("/path/to/encrypted.img",
                                                     10 * 1024 * 1024,
                                                     "my_password");
```

These consult the process-wide session registry (`OifsSession::get_or_open`). If a master session exists (local IPC or network mode), the returned handle shares the underlying `DiskManager`.

### Closing the Handle

Release the handle when finished. This decrements reference counts and flushes all pending changes to disk via `mmap.flush()`.

```c
oifs_close(handle);
```

Passing `NULL` is a safe no-op. After `oifs_close`, the handle pointer is invalid and must not be used.

## 4. Directory Operations

### Listing Directory Contents

Use `oifs_ls` with a C callback function to iterate entries in the root directory:

```c
// Callback signature (defined in oifs.h):
// typedef void (*ListCallback)(const char* name, uint64_t size, uint64_t mtime, void* user_data);

struct ListContext {
    int file_count;
    uint64_t total_bytes;
};

void my_list_callback(const char* name, uint64_t size, uint64_t mtime, void* user_data) {
    struct ListContext* ctx = (struct ListContext*)user_data;
    if (ctx) {
        ctx->file_count++;
        ctx->total_bytes += size;
    }
    printf("Entry: %s  size=%lu  mtime=%lu\n", name, size, mtime);
}

int main(void) {
    // ... version check and open ...
    
    struct ListContext ctx = {0, 0};
    int result = oifs_ls(handle, my_list_callback, &ctx);
    if (result != 0) {
        char err[256];
        oifs_last_error(handle, err, sizeof(err));
        fprintf(stderr, "oifs_ls failed: %s\n", err);
    } else {
        printf("Found %d entries, total %lu bytes\n", ctx.file_count, ctx.total_bytes);
    }
}
```

**Key points about the callback:**
- The callback runs synchronously during `oifs_ls` execution (not async)
- `user_data` is an opaque `void*` passed through to each invocation—use it for caller-defined context
- The `name` pointer is valid only during the callback invocation; copy it if needed
- Returns `0` on success, `-1` on error (check `oifs_last_error`)

### Creating a Directory

```c
int result = oifs_mkdir(handle, "mydir");
if (result != 0) {
    // Handle error
}
```

## 5. File Operations

### Creating a File

```c
int result = oifs_create_file(handle, "myfile.txt");
if (result != 0) {
    // Handle error
}
```

### Creating or Opening a File (Idempotent)

```c
int result = oifs_create_or_open_file(handle, "myfile.txt");
// Returns 0 whether file was created or already existed
```

### Writing Data (Simple Interface)

```c
const char* message = "Hello, OIFS!";
int result = oifs_write_file(handle, "myfile.txt", 
                             (const uint8_t*)message, strlen(message));
if (result != 0) {
    // Handle error
}
```

### Writing Data with Policy Control (Advanced)

Use `oifs_write_file_with_policy` for explicit compression control, custom Zstd level, and offset writes:

```c
#define OIFS_WRITE_POLICY_DEFAULT       0x00  // Auto / Full-file stream compression (default)
#define OIFS_WRITE_POLICY_RAW           0x01  // Never compress (Raw)
#define OIFS_WRITE_POLICY_STREAM        0x02  // Stream compression (custom zstd_level)
#define OIFS_WRITE_POLICY_SEEKABLE_64K  0x08  // Seekable 64KB chunked compression

// Write at offset with seekable compression, Zstd level 3
int result = oifs_write_file_with_policy(handle, "data.bin",
                                          0,                          // offset
                                          data, data_len,             // buffer
                                          OIFS_WRITE_POLICY_SEEKABLE_64K,
                                          3);                         // zstd_level
```

### Reading Data (Sequential)

```c
uint8_t buffer[256];
int64_t bytes_read = oifs_read_file(handle, "myfile.txt", buffer, sizeof(buffer));
if (bytes_read < 0) {
    // Handle error
} else {
    printf("Read %ld bytes: %.*s\n", bytes_read, (int)bytes_read, buffer);
}
```

### Reading Data at Offset (Random Access)

Use `oifs_read_at` for chunked/direct reads from arbitrary byte offsets. For uncompressed files, it copies directly from mmap blocks without heap allocations.

```c
uint8_t chunk[64];
// Read 64 bytes starting at offset 1024
int64_t bytes = oifs_read_at(handle, "largefile.dat", 1024, chunk, sizeof(chunk));
if (bytes < 0) {
    // Handle error
}
```

`oifs_read_file` delegates to `oifs_read_at` with offset 0.

### Deleting a File

```c
int result = oifs_delete_file(handle, "myfile.txt");
if (result != 0) {
    // Handle error
}
```

### Truncating a File

```c
// Truncate to 100 bytes (or extend sparsely if larger)
int result = oifs_truncate_file(handle, "myfile.txt", 100);
```

## 6. Error Handling

Each `OIFSHandle` maintains its own thread-local error string (not a global `errno`). Successful calls clear the error; failures set a descriptive message.

### Retrieving the Last Error

```c
int32_t oifs_last_error(OIFSHandle* handle, char* buf, uint32_t buf_size);
```

```c
char err_buf[256];
if (oifs_last_error(handle, err_buf, sizeof(err_buf)) == 0) {
    if (strlen(err_buf) > 0 && strcmp(err_buf, "No error") != 0) {
        fprintf(stderr, "OIFS Error: %s\n", err_buf);
    }
}
```

- Returns `0` on success (copies error string into `buf`, null-terminated)
- Returns `-1` if `handle` is `NULL`, `buf` is `NULL`, or `buf_size` is 0
- The error string is sanitized (embedded nulls replaced with spaces)
- Always call after a function returns an error code (`-1` or `NULL`)

## 7. I/O Backend Configuration

OIFS supports three I/O backends selectable at runtime per handle:

```c
#define OIFS_IO_BACKEND_MMAP     0  // Default: memory-mapped I/O
#define OIFS_IO_BACKEND_PREAD    1  // posix pread/pwrite
#define OIFS_IO_BACKEND_IO_URING 2  // Linux io_uring (falls back to Pread if unsupported)
```

```c
// Query current backend
int current = oifs_get_io_backend(handle);
assert(current == OIFS_IO_BACKEND_MMAP);

// Switch to pread
int effective = oifs_set_io_backend(handle, OIFS_IO_BACKEND_PREAD);
assert(effective == OIFS_IO_BACKEND_PREAD);

// Switch to io_uring (Linux only)
effective = oifs_set_io_backend(handle, OIFS_IO_BACKEND_IO_URING);
// On non-Linux, falls back to Pread and returns OIFS_IO_BACKEND_PREAD
```

## 8. Complete C Program Example

Here is a complete, compilable C program demonstrating the full workflow:

```c
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include "oifs.h"

struct ListContext {
    int file_count;
    uint64_t total_bytes;
};

static void list_callback(const char* name, uint64_t size, uint64_t mtime, void* user_data) {
    (void)mtime;
    struct ListContext* ctx = (struct ListContext*)user_data;
    if (ctx) {
        ctx->file_count++;
        ctx->total_bytes += size;
    }
    printf("  - %s (%lu bytes)\n", name, size);
}

int main(void) {
    // 1. VERSION HANDSHAKE
    int v_status = OIFS_CHECK_VERSION();
    if (v_status == OIFS_VERSION_COMPAT_ERR) {
        char path[512] = {0};
        oifs_loaded_path(path, sizeof(path));
        fprintf(stderr, "FATAL: Outdated liboifs at '%s' (v%s), expected >= v%s\n",
                path, oifs_version_string(), OIFS_VERSION_STRING);
        return 1;
    } else if (v_status == OIFS_VERSION_COMPAT_WARN) {
        fprintf(stderr, "WARN: Newer liboifs (v%s) than headers (v%s)\n",
                oifs_version_string(), OIFS_VERSION_STRING);
    }
    printf("Version check passed: liboifs v%s\n", oifs_version_string());

    // 2. OPEN/CREATE FILESYSTEM (10 MB)
    const char* img_path = "example_fs.img";
    OIFSHandle* fs = oifs_open(img_path, 10 * 1024 * 1024);
    if (!fs) {
        fprintf(stderr, "Failed to open/create filesystem\n");
        return 1;
    }
    printf("Opened filesystem: %s\n", img_path);

    // 3. CREATE DIRECTORY
    if (oifs_mkdir(fs, "docs") != 0) {
        char err[256]; oifs_last_error(fs, err, sizeof(err));
        fprintf(stderr, "mkdir failed: %s\n", err);
        oifs_close(fs);
        return 1;
    }
    printf("Created directory: docs\n");

    // 4. CREATE FILE
    if (oifs_create_file(fs, "docs/readme.txt") != 0) {
        char err[256]; oifs_last_error(fs, err, sizeof(err));
        fprintf(stderr, "create_file failed: %s\n", err);
        oifs_close(fs);
        return 1;
    }

    // 5. WRITE DATA
    const char* content = "OIFS FFI Example: Multi-block data with compression!";
    uint64_t content_len = (uint64_t)strlen(content);
    if (oifs_write_file(fs, "docs/readme.txt", (const uint8_t*)content, content_len) != 0) {
        char err[256]; oifs_last_error(fs, err, sizeof(err));
        fprintf(stderr, "write_file failed: %s\n", err);
        oifs_close(fs);
        return 1;
    }
    printf("Wrote %lu bytes to docs/readme.txt\n", content_len);

    // 6. SEQUENTIAL READ
    uint8_t read_buf[256] = {0};
    int64_t r_bytes = oifs_read_file(fs, "docs/readme.txt", read_buf, sizeof(read_buf));
    if (r_bytes < 0) {
        char err[256]; oifs_last_error(fs, err, sizeof(err));
        fprintf(stderr, "read_file failed: %s\n", err);
        oifs_close(fs);
        return 1;
    }
    assert(r_bytes == (int64_t)content_len);
    assert(memcmp(read_buf, content, content_len) == 0);
    printf("Sequential read verified (%ld bytes)\n", r_bytes);

    // 7. CHUNKED OFFSET READ (oifs_read_at)
    uint8_t chunk_buf[16] = {0};
    int64_t chunk_bytes = oifs_read_at(fs, "docs/readme.txt", 4, chunk_buf, 4);
    assert(chunk_bytes == 4);
    assert(memcmp(chunk_buf, "FS ", 4) == 0);
    printf("Offset read verified: '%.4s'\n", chunk_buf);

    // 8. DIRECTORY LISTING WITH USER CONTEXT
    struct ListContext ctx = {0, 0};
    if (oifs_ls(fs, list_callback, &ctx) != 0) {
        char err[256]; oifs_last_error(fs, err, sizeof(err));
        fprintf(stderr, "oifs_ls failed: %s\n", err);
    } else {
        printf("Directory listing: %d entries, %lu total bytes\n", 
               ctx.file_count, ctx.total_bytes);
    }

    // 9. FILE DELETION AND ERROR DIAGNOSTICS
    assert(oifs_delete_file(fs, "docs/readme.txt") == 0);
    int64_t fail_read = oifs_read_file(fs, "docs/readme.txt", read_buf, sizeof(read_buf));
    assert(fail_read == -1);
    
    char err_buf[256] = {0};
    oifs_last_error(fs, err_buf, sizeof(err_buf));
    printf("Expected error after delete: %s\n", err_buf);

    // 10. ENCRYPTED FILESYSTEM DEMO
    const char* enc_path = "encrypted_fs.img";
    const char* password = "secure_passphrase";
    
    OIFSHandle* enc_fs = oifs_open_with_password(enc_path, 10 * 1024 * 1024, password);
    if (!enc_fs) {
        fprintf(stderr, "Failed to create encrypted filesystem\n");
        oifs_close(fs);
        return 1;
    }
    printf("\nCreated encrypted filesystem: %s\n", enc_path);

    const char* secret = "Top secret data";
    assert(oifs_write_file(enc_fs, "secret.txt", (const uint8_t*)secret, strlen(secret)) == 0);
    oifs_close(enc_fs);
    printf("Wrote secret data, closed encrypted FS\n");

    // Reopen with WRONG password - read must fail
    OIFSHandle* bad_fs = oifs_open_with_password(enc_path, 0, "wrong_password");
    assert(bad_fs != NULL);  // Handle opens but decryption fails on read
    uint8_t secret_buf[128] = {0};
    int64_t bad_read = oifs_read_file(bad_fs, "secret.txt", secret_buf, sizeof(secret_buf));
    assert(bad_read == -1);
    oifs_last_error(bad_fs, err_buf, sizeof(err_buf));
    printf("Wrong password read failed as expected: %s\n", err_buf);
    oifs_close(bad_fs);

    // Reopen with CORRECT password - read must succeed
    OIFSHandle* good_fs = oifs_open_with_password(enc_path, 0, password);
    assert(good_fs != NULL);
    int64_t good_read = oifs_read_file(good_fs, "secret.txt", secret_buf, sizeof(secret_buf));
    assert(good_read == (int64_t)strlen(secret));
    assert(memcmp(secret_buf, secret, good_read) == 0);
    printf("Correct password read succeeded: %.16s\n", secret_buf);
    oifs_close(good_fs);

    // 11. CLEANUP
    oifs_close(fs);
    printf("\nAll operations completed successfully!\n");
    return 0;
}
```

**Compile and run:**
```bash
gcc -std=c11 -Wall -Wextra -Wpedantic -Werror \
    -I/path/to/oifs/include \
    example.c -L/path/to/oifs/target/release -loifs -ldl -o example

# Run (set library path)
export LD_LIBRARY_PATH=/path/to/oifs/target/release:$LD_LIBRARY_PATH
./example
```

## 9. C++ RAII Integration (Modern)

For C++17+, wrap the handle in a `std::unique_ptr` with a custom deleter:

```cpp
#include <memory>
#include <vector>
#include <string>
#include <cassert>
#include "oifs.h"

struct OIFSDeleter {
    void operator()(OIFSHandle* h) const noexcept {
        if (h) oifs_close(h);
    }
};
using UniqueOIFS = std::unique_ptr<OIFSHandle, OIFSDeleter>;

int main() {
    assert(OIFS_CHECK_VERSION() == OIFS_VERSION_COMPAT_OK);
    
    UniqueOIFS fs(oifs_open("cpp_fs.img", 20 * 1024 * 1024));
    assert(fs != nullptr);

    // Write using std::vector
    std::string text = "C++17 RAII Test";
    std::vector<uint8_t> data(text.begin(), text.end());
    assert(oifs_write_file(fs.get(), "test.txt", data.data(), data.size()) == 0);

    // Read into std::vector
    std::vector<uint8_t> read_buf(128);
    int64_t bytes = oifs_read_file(fs.get(), "test.txt", read_buf.data(), read_buf.size());
    assert(bytes == (int64_t)data.size());
    read_buf.resize(bytes);
    assert(read_buf == data);

    // Lambda callback with captured vector
    struct DirEntry { std::string name; uint64_t size; uint64_t mtime; };
    std::vector<DirEntry> entries;
    auto cb = [](const char* name, uint64_t size, uint64_t mtime, void* user_data) {
        auto* vec = static_cast<std::vector<DirEntry>*>(user_data);
        vec->push_back({std::string(name), size, mtime});
    };
    assert(oifs_ls(fs.get(), cb, &entries) == 0);
    assert(!entries.empty());

    // Automatic cleanup via RAII when fs goes out of scope
    return 0;
}
```

## 10. Key Function Reference

| Function | Purpose |
|----------|---------|
| `OIFS_CHECK_VERSION()` | Version handshake macro (use at startup) |
| `oifs_version_major/minor/patch/string/code()` | Runtime version queries |
| `oifs_loaded_path()` | Get absolute path of loaded liboifs |
| `oifs_open()` | Open/create unencrypted filesystem |
| `oifs_open_with_password()` | Open/create encrypted filesystem |
| `oifs_get_or_open()` | Session registry reuse (unencrypted) |
| `oifs_get_or_open_with_password()` | Session registry reuse (encrypted) |
| `oifs_close()` | Release handle and flush |
| `oifs_mkdir()` | Create directory |
| `oifs_ls()` | List root directory via callback |
| `oifs_create_file()` | Create new file |
| `oifs_create_or_open_file()` | Create or open existing file |
| `oifs_write_file()` | Write full file (auto compression) |
| `oifs_write_file_with_policy()` | Write with policy/offset/Zstd level |
| `oifs_read_file()` | Sequential read from offset 0 |
| `oifs_read_at()` | Random-access read at offset |
| `oifs_delete_file()` | Delete file |
| `oifs_truncate_file()` | Truncate/extend file |
| `oifs_last_error()` | Get handle-local error string |
| `oifs_set_io_backend()` | Set I/O backend (Mmap/Pread/IoUring) |
| `oifs_get_io_backend()` | Get current I/O backend |

## 11. Testing and Validation

The repository includes end-to-end native compiler tests in `tests/ffi_c_cpp_test.rs` that validate:
- C11 compilation with `-Wall -Wextra -Wpedantic -Werror`
- C++17 compilation with same strict flags
- Complete lifecycle: version handshake → open → mkdir → create → write → read_at → ls → delete → close
- Encrypted image workflow: open_with_password, read with good/bad passwords
- Session handle reuse: get_or_open
- I/O backend switching
- Multi-threaded concurrent reads/writes (C++ test)

Run tests with:
```bash
cargo test ffi_c_cpp_test -- --nocapture
```

This workflow covers all essential FFI operations for integrating OIFS into C/C++ applications with proper error handling, version safety, and encryption support.
