---
type: integration guide
title: FFI Integration Guide
description: Comprehensive guide for embedding OIFS from C/C++, Python (ctypes/cffi), Node.js (node-ffi), Go (cgo), and Rust via the shared C ABI layer. Covers building the cdylib, opaque handle lifecycle, error handling, async callback patterns, encryption key management, and version negotiation handshake.
tags: [ffi, integration, c, cpp, python, node, go, rust, encryption, versioning, async]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-f9a8a5ec259e5381eba4c51a
    resource: repo://include/oifs.h
  - id: openwiki-source-c9e5b32aad7cafdb095c81a4
    resource: repo://src/ffi.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---

# OIFS FFI Integration Guide

This guide explains how to integrate the OIFS shared library (`liboifs.so` / `liboifs.dylib` / `oifs.dll`) into applications written in C/C++, Python, Node.js, Go, and Rust. The FFI provides a stable C ABI interface for all filesystem operations, encryption, I/O backend selection, and version compatibility checks.

## Library Build

The OIFS shared library is built as a `cdylib` crate using Cargo:

```bash
# Build release version
cargo build --release

# Output locations:
#   target/release/liboifs.so     (Linux)
#   target/release/liboifs.dylib  (macOS)
#   target/release/oifs.dll       (Windows)
```

The public C header is located at `include/oifs.h`. The Rust implementation lives in `src/ffi.rs`.

## Header Inclusion (C/C++)

```c
#include <oifs.h>
```

Add the `include` directory to your compiler's include path:

```bash
# GCC/Clang
-I/path/to/oifs/include

# MSVC
/I"C:\path\to\oifs\include"
```

## Linking (C/C++)

```bash
# Linux
-L/path/to/oifs/target/release -loifs -ldl

# macOS
-L/path/to/oifs/target/release -loifs

# Windows (MSVC)
/LIBPATH:C:\path\to\oifs\target\release oifs.lib
```

**Note**: On Linux, `-ldl` is required for `dladdr` used in `oifs_loaded_path`.

---

## Minimal C Example: Create → Put → Get → Close

```c
#include <oifs.h>
#include <stdio.h>
#include <string.h>

int main(void) {
    // 1. Version handshake (recommended at startup)
    int v_status = OIFS_CHECK_VERSION();
    if (v_status == OIFS_VERSION_COMPAT_ERR) {
        char path[512] = {0};
        oifs_loaded_path(path, sizeof(path));
        fprintf(stderr, "FATAL: Outdated liboifs at '%s' (v%s), need >= v%s\n",
                path, oifs_version_string(), OIFS_VERSION_STRING);
        return 1;
    } else if (v_status == OIFS_VERSION_COMPAT_WARN) {
        fprintf(stderr, "WARN: Newer liboifs (v%s) than headers (v%s)\n",
                oifs_version_string(), OIFS_VERSION_STRING);
    }

    // 2. Open/create a 10 MB filesystem
    OIFSHandle* fs = oifs_open("./mydisk.oifs", 10 * 1024 * 1024);
    if (!fs) {
        fprintf(stderr, "Failed to open filesystem\n");
        return 1;
    }

    // 3. Create directory and file
    if (oifs_mkdir(fs, "docs") != 0) {
        char err[256]; oifs_last_error(fs, err, sizeof(err));
        fprintf(stderr, "mkdir failed: %s\n", err);
        oifs_close(fs); return 1;
    }

    // 4. Put: write data
    const char* msg = "Hello, OIFS FFI!";
    if (oifs_write_file(fs, "docs/hello.txt",
                        (const uint8_t*)msg, strlen(msg)) != 0) {
        char err[256]; oifs_last_error(fs, err, sizeof(err));
        fprintf(stderr, "write failed: %s\n", err);
        oifs_close(fs); return 1;
    }

    // 5. Get: read data
    uint8_t buf[128] = {0};
    int64_t n = oifs_read_file(fs, "docs/hello.txt", buf, sizeof(buf));
    if (n < 0) {
        char err[256]; oifs_last_error(fs, err, sizeof(err));
        fprintf(stderr, "read failed: %s\n", err);
        oifs_close(fs); return 1;
    }
    printf("Read %ld bytes: %.*s\n", n, (int)n, buf);

    // 6. Close
    oifs_close(fs);
    return 0;
}
```

---

## Opaque Handle Lifecycle

All operations require an `OIFSHandle*` obtained from one of four factory functions. The handle is an opaque pointer; its internal structure is private to the library.

### Factory Functions

| Function | Purpose |
|----------|---------|
| `oifs_open(path, size)` | Open existing or create new **unencrypted** image. |
| `oifs_open_with_password(path, size, password)` | Open/create **encrypted** image using Argon2id key derivation. |
| `oifs_get_or_open(path, size)` | Reuse existing session (IPC/local/network) or open new. |
| `oifs_get_or_open_with_password(path, size, password)` | Session reuse with password validation for encrypted images. |

- `size` is ignored when opening an existing image; only used for creation.
- `password` may be `NULL` or empty for unencrypted images.
- All return `NULL` on failure (no handle exists to query for errors).

### Handle Destruction

```c
void oifs_close(OIFSHandle* handle);
```

- Safe to call with `NULL` (no-op).
- Drops the `DiskManager`, flushes mmap, and frees the heap allocation.

---

## Error Handling Pattern

Every operation that takes a handle returns an integer status and stores a detailed error message in the handle's thread-local error slot.

### Return Conventions

| Function Category | Success | Failure |
|-------------------|---------|---------|
| Boolean-style ops (`oifs_write_file`, `oifs_mkdir`, `oifs_ls`, etc.) | `0` | `-1` |
| Byte-count ops (`oifs_read_at`, `oifs_read_file`) | `bytes_read ≥ 0` | `-1` |
| Version/Query ops (`oifs_version_*`, `oifs_get_io_backend`) | meaningful value | `-1` on invalid handle |
| `oifs_last_error` | `0` | `-1` on bad args |

### Retrieving Error Messages

```c
int32_t oifs_last_error(OIFSHandle* handle, char* buf, uint32_t buf_size);
```

- Copies the null-terminated error string into `buf`.
- Guarantees null-termination even if truncated.
- Returns `0` on success, `-1` if `handle==NULL`, `buf==NULL`, or `buf_size==0`.
- If no error is set, returns `"No error"`.

```c
char err[256];
if (oifs_write_file(fs, "x", (uint8_t*)"x", 1) != 0) {
    oifs_last_error(fs, err, sizeof(err));
    fprintf(stderr, "Error: %s\n", err);
}
```

---

## Directory Iteration Callback

`oifs_ls` uses a C callback for directory listing because C lacks standard iterators.

### Callback Signature

```c
typedef void (*ListCallback)(const char* name, uint64_t size, uint64_t mtime, void* user_data);
```

- `name`: Entry name (null-terminated UTF-8).
- `size`: File size in bytes (0 for directories).
- `mtime`: Modification time (Unix epoch milliseconds).
- `user_data`: Caller-provided opaque pointer.

### Thread-Safety Requirements

- The callback is invoked **while holding the `DiskManager` read lock**.
- **Do not** call back into OIFS FFI functions on the same handle from within the callback (will deadlock).
- The callback runs on the caller's thread; it must be reentrant if `oifs_ls` is called concurrently on different handles.
- Copy any data you need; the `name` pointer is valid only for the duration of the call.

### Example (C)

```c
struct ListCtx { int count; uint64_t total; };

void my_cb(const char* name, uint64_t size, uint64_t mtime, void* u) {
    struct ListCtx* ctx = (struct ListCtx*)u;
    ctx->count++;
    ctx->total += size;
    printf("  %s  %lu bytes\n", name, size);
}

struct ListCtx ctx = {0};
oifs_ls(fs, my_cb, &ctx);
```

### Example (C++17 Lambda)

```cpp
std::vector<std::string> names;
auto cb = [](const char* n, uint64_t sz, uint64_t mt, void* ud) {
    auto* vec = static_cast<std::vector<std::string>*>(ud);
    vec->emplace_back(n);
};
oifs_ls(fs.get(), cb, &names);
```

---

## Encryption Key Management Across FFI

OIFS uses **password-based encryption** with Argon2id key derivation. The FFI does **not** expose raw key import/export; all encryption is driven by passphrases.

### How It Works

1. **Creation**: `oifs_open_with_password(path, size, "passphrase")` on a non-existent file creates a new encrypted image.
   - A random 32-byte salt is generated and stored in the superblock.
   - Argon2id derives a 256-bit master key from the passphrase + salt.
   - The master key is split into subkeys (payload AEAD, filename SIV, filename cipher, IPC auth).

2. **Opening**: Same function on an existing encrypted image re-derives the key using the stored salt.
   - Wrong passphrase → different key → decryption failures on data reads (not on open).

3. **Session Reuse**: `oifs_get_or_open_with_password` consults the process-wide session registry.
   - If a compatible session exists, it is reused (avoids re-derivation and mmap).
   - Password is validated against the session's stored key hash.

### Passing Keys from Higher-Level Languages

| Language | Pattern |
|----------|---------|
| **Python** | Pass `str`/`bytes` password to `oifs_open_with_password`; the FFI expects UTF-8 `const char*`. |
| **Node.js** | Use `Buffer.from(password, 'utf8')` or `string` directly via `node-ffi-napi`. |
| **Go** | `C.CString(password)` → `defer C.free(unsafe.Pointer(cstr))`. |
| **Rust** | `CString::new(password).unwrap().as_ptr()` via the same `extern "C"` bindings. |

### Important Invariants

- **No raw key API**: The FFI deliberately does not expose `EncryptionKey` or raw 32-byte keys. This prevents key material from leaking into host language memory where it may be harder to zeroize.
- **Argon2id parameters** are fixed (hardened defaults) and not configurable via FFI.
- **Wrong password behavior**: `oifs_open_with_password` succeeds (returns a handle) even with a wrong password; the first data read (`oifs_read_file`/`oifs_read_at`) will fail with a decryption error. This is by design—key derivation is deterministic and does not require server round-trips.

---

## Version Negotiation Handshake

The C header embeds compile-time version constants; the loaded library exports runtime versions. The compatibility policy: **only block older versions; never block newer versions**.

### Header Constants (`include/oifs.h`)

```c
#define OIFS_VERSION_MAJOR 1
#define OIFS_VERSION_MINOR 2
#define OIFS_VERSION_PATCH 0
#define OIFS_VERSION_STRING "1.2.0"
#define OIFS_VERSION_CODE (((uint64_t)MAJOR << 32) | ((uint64_t)MINOR << 16) | PATCH)

#define OIFS_VERSION_COMPAT_OK       0   // Exact match
#define OIFS_VERSION_COMPAT_WARN     1   // Newer library (allowed)
#define OIFS_VERSION_COMPAT_ERR     -1   // Older library (blocked)
```

### Runtime Query Functions

```c
uint32_t oifs_version_major(void);
uint32_t oifs_version_minor(void);
uint32_t oifs_version_patch(void);
uint64_t oifs_version_code(void);
const char* oifs_version_string(void);  // static null-terminated string
```

### Compatibility Check

```c
int32_t oifs_check_version(uint32_t req_major, uint32_t req_minor, uint32_t req_patch);
```

Returns:
- `0` (`OK`) if loaded version == requested version.
- `1` (`WARN`) if loaded version > requested (newer library, permitted).
- `-1` (`ERR`) if loaded version < requested (older library, blocked).

### Convenience Macro

```c
#define OIFS_CHECK_VERSION() \
    oifs_check_version(OIFS_VERSION_MAJOR, OIFS_VERSION_MINOR, OIFS_VERSION_PATCH)
```

### Recommended Startup Pattern

```c
int status = OIFS_CHECK_VERSION();
if (status == OIFS_VERSION_COMPAT_ERR) {
    char path[512] = {0};
    oifs_loaded_path(path, sizeof(path));
    fprintf(stderr, "FATAL: Outdated liboifs '%s' (v%s) loaded. Expected >= v%s.\n",
            path, oifs_version_string(), OIFS_VERSION_STRING);
    abort();
} else if (status == OIFS_VERSION_COMPAT_WARN) {
    fprintf(stderr, "WARN: Newer liboifs (v%s) than headers (v%s).\n",
            oifs_version_string(), OIFS_VERSION_STRING);
}
```

### Loaded Library Path

```c
int32_t oifs_loaded_path(char* buf, size_t buf_size);
```

Returns the absolute filesystem path of the loaded `liboifs` (via `dladdr` on Unix). Useful for diagnostics in the version mismatch case above.

---

## I/O Backend Configuration

OIFS supports pluggable I/O backends selectable at runtime per handle.

### Backend Constants

```c
#define OIFS_IO_BACKEND_MMAP     0  // Default: memory-mapped I/O
#define OIFS_IO_BACKEND_PREAD    1  // pread/pwrite syscalls
#define OIFS_IO_BACKEND_IO_URING 2  // Linux io_uring (falls back to Pread)
```

### API

```c
int32_t oifs_set_io_backend(OIFSHandle* handle, uint8_t backend);
int32_t oifs_get_io_backend(OIFSHandle* handle);
```

- `oifs_set_io_backend` returns the **effective** backend (after fallback).
- On non-Linux, `IO_URING` falls back to `PREAD` and returns `1`.
- Backend changes take effect immediately for subsequent operations.

---

## Language Binding Examples

### Python (ctypes)

```python
import ctypes
import os

# Load library
lib_path = os.path.join(os.path.dirname(__file__), "target/release/liboifs.so")
lib = ctypes.CDLL(lib_path)

# Define types
OIFSHandle = ctypes.c_void_p
ListCallback = ctypes.CFUNCTYPE(None, ctypes.c_char_p, ctypes.c_uint64, ctypes.c_uint64, ctypes.c_void_p)

# Function signatures
lib.oifs_open.argtypes = [ctypes.c_char_p, ctypes.c_uint64]
lib.oifs_open.restype = OIFSHandle

lib.oifs_close.argtypes = [OIFSHandle]
lib.oifs_close.restype = None

lib.oifs_write_file.argtypes = [OIFSHandle, ctypes.c_char_p, ctypes.POINTER(ctypes.c_ubyte), ctypes.c_uint64]
lib.oifs_write_file.restype = ctypes.c_int32

lib.oifs_read_file.argtypes = [OIFSHandle, ctypes.c_char_p, ctypes.POINTER(ctypes.c_ubyte), ctypes.c_uint64]
lib.oifs_read_file.restype = ctypes.c_int64

lib.oifs_last_error.argtypes = [OIFSHandle, ctypes.c_char_p, ctypes.c_uint32]
lib.oifs_last_error.restype = ctypes.c_int32

lib.oifs_check_version.argtypes = [ctypes.c_uint32, ctypes.c_uint32, ctypes.c_uint32]
lib.oifs_check_version.restype = ctypes.c_int32

lib.oifs_version_string.restype = ctypes.c_char_p

# Version check
OIFS_VERSION_COMPAT_OK = 0
status = lib.oifs_check_version(1, 2, 0)
if status != OIFS_VERSION_COMPAT_OK:
    raise RuntimeError(f"Version mismatch: {status}")

# Open filesystem
fs = lib.oifs_open(b"./pydisk.oifs", 10 * 1024 * 1024)
if not fs:
    raise RuntimeError("Failed to open filesystem")

# Write
msg = b"Hello from Python!"
buf = (ctypes.c_ubyte * len(msg)).from_buffer_copy(msg)
if lib.oifs_write_file(fs, b"hello.txt", buf, len(msg)) != 0:
    err = ctypes.create_string_buffer(256)
    lib.oifs_last_error(fs, err, 256)
    raise RuntimeError(err.value.decode())

# Read
read_buf = (ctypes.c_ubyte * 128)()
n = lib.oifs_read_file(fs, b"hello.txt", read_buf, 128)
if n < 0:
    err = ctypes.create_string_buffer(256)
    lib.oifs_last_error(fs, err, 256)
    raise RuntimeError(err.value.decode())
print(f"Read {n} bytes: {bytes(read_buf[:n]).decode()}")

lib.oifs_close(fs)
```

### Python (cffi)

```python
from cffi import FFI
import os

ffibuilder = FFI()
ffibuilder.cdef("""
    typedef struct OIFSHandle OIFSHandle;
    OIFSHandle* oifs_open(const char* path, uint64_t size);
    void oifs_close(OIFSHandle* handle);
    int32_t oifs_write_file(OIFSHandle* handle, const char* filename, const uint8_t* buf, uint64_t buf_size);
    int64_t oifs_read_file(OIFSHandle* handle, const char* filename, uint8_t* buf, uint64_t buf_size);
    int32_t oifs_last_error(OIFSHandle* handle, char* buf, uint32_t buf_size);
    int32_t oifs_check_version(uint32_t req_major, uint32_t req_minor, uint32_t req_patch);
    const char* oifs_version_string(void);
""")

lib_path = os.path.join(os.path.dirname(__file__), "target/release/liboifs.so")
lib = ffibuilder.dlopen(lib_path)

# Version check
assert lib.oifs_check_version(1, 2, 0) == 0

fs = lib.oifs_open(b"./pydisk.oifs", 10 * 1024 * 1024)
assert fs != ffi.NULL

msg = b"Hello from cffi!"
buf = ffi.new("uint8_t[]", msg)
assert lib.oifs_write_file(fs, b"hello.txt", buf, len(msg)) == 0

read_buf = ffi.new("uint8_t[128]")
n = lib.oifs_read_file(fs, b"hello.txt", read_buf, 128)
assert n > 0
print(f"Read {n}: {ffi.buffer(read_buf, n)[:].decode()}")

lib.oifs_close(fs)
```

### Node.js (node-ffi-napi)

```javascript
const ffi = require('ffi-napi');
const ref = require('ref-napi');
const path = require('path');

const libPath = path.join(__dirname, 'target/release/liboifs.so');
// On macOS: 'target/release/liboifs.dylib'
// On Windows: 'target/release/oifs.dll'

const lib = ffi.Library(libPath, {
  'oifs_open': ['pointer', ['string', 'uint64']],
  'oifs_close': ['void', ['pointer']],
  'oifs_write_file': ['int32', ['pointer', 'string', 'pointer', 'uint64']],
  'oifs_read_file': ['int64', ['pointer', 'string', 'pointer', 'uint64']],
  'oifs_last_error': ['int32', ['pointer', 'pointer', 'uint32']],
  'oifs_check_version': ['int32', ['uint32', 'uint32', 'uint32']],
  'oifs_version_string': ['string', []],
  'oifs_mkdir': ['int32', ['pointer', 'string']],
});

const OIFS_VERSION_COMPAT_OK = 0;
const status = lib.oifs_check_version(1, 2, 0);
if (status !== OIFS_VERSION_COMPAT_OK) {
  throw new Error(`Version mismatch: ${status}`);
}

const fs = lib.oifs_open('./node_disk.oifs', 10 * 1024 * 1024);
if (fs.isNull()) throw new Error('Failed to open filesystem');

lib.oifs_mkdir(fs, 'docs');

const msg = Buffer.from('Hello from Node.js!');
const writeBuf = Buffer.alloc(msg.length);
msg.copy(writeBuf);
if (lib.oifs_write_file(fs, 'docs/hello.txt', writeBuf, msg.length) !== 0) {
  const errBuf = Buffer.alloc(256);
  lib.oifs_last_error(fs, errBuf, 256);
  throw new Error(errBuf.toString().trim());
}

const readBuf = Buffer.alloc(128);
const n = lib.oifs_read_file(fs, 'docs/hello.txt', readBuf, 128);
if (n < 0) {
  const errBuf = Buffer.alloc(256);
  lib.oifs_last_error(fs, errBuf, 256);
  throw new Error(errBuf.toString().trim());
}

console.log(`Read ${n} bytes: ${readBuf.subarray(0, n).toString()}`);

lib.oifs_close(fs);
```

### Go (cgo)

```go
package main

/*
#cgo LDFLAGS: -L${SRCDIR}/target/release -loifs -ldl
#cgo CFLAGS: -I${SRCDIR}/include
#include <oifs.h>
#include <stdlib.h>
*/
import "C"
import (
	"fmt"
	"unsafe"
)

func main() {
	// Version check
	status := C.oifs_check_version(C.OIFS_VERSION_MAJOR, C.OIFS_VERSION_MINOR, C.OIFS_VERSION_PATCH)
	if status == C.OIFS_VERSION_COMPAT_ERR {
		var pathBuf [512]C.char
		C.oifs_loaded_path(&pathBuf[0], C.size_t(len(pathBuf)))
		panic(fmt.Sprintf("Outdated liboifs: %s", C.GoString(&pathBuf[0])))
	}

	// Open
	cPath := C.CString("./godisk.oifs")
	defer C.free(unsafe.Pointer(cPath))
	fs := C.oifs_open(cPath, 10*1024*1024)
	if fs == nil {
		panic("Failed to open filesystem")
	}
	defer C.oifs_close(fs)

	// Mkdir
	cDir := C.CString("docs")
	C.oifs_mkdir(fs, cDir)
	C.free(unsafe.Pointer(cDir))

	// Write
	msg := "Hello from Go!"
	cFile := C.CString("docs/hello.txt")
	cMsg := (*C.uint8_t)(unsafe.Pointer(&[]byte(msg)[0]))
	if C.oifs_write_file(fs, cFile, cMsg, C.uint64_t(len(msg))) != 0 {
		var errBuf [256]C.char
		C.oifs_last_error(fs, &errBuf[0], C.uint32_t(len(errBuf)))
		panic(fmt.Sprintf("Write failed: %s", C.GoString(&errBuf[0])))
	}
	C.free(unsafe.Pointer(cFile))

	// Read
	readBuf := make([]byte, 128)
	cFile = C.CString("docs/hello.txt")
	n := C.oifs_read_file(fs, cFile, (*C.uint8_t)(unsafe.Pointer(&readBuf[0])), C.uint64_t(len(readBuf)))
	C.free(unsafe.Pointer(cFile))
	if n < 0 {
		var errBuf [256]C.char
		C.oifs_last_error(fs, &errBuf[0], C.uint32_t(len(errBuf)))
		panic(fmt.Sprintf("Read failed: %s", C.GoString(&errBuf[0])))
	}
	fmt.Printf("Read %d bytes: %s\n", n, string(readBuf[:n]))
}
```

### Rust (via the same FFI layer)

Rust callers can use the generated bindings from `bindgen` or call the `extern "C"` functions directly from `src/ffi.rs`.

```rust
// In your Cargo.toml, add: oifs = { path = "../oifs" }  (or publish as a crate)
// Then use the FFI functions directly:

use oifs::ffi::*;
use std::ffi::CString;

fn main() {
    // Version check
    let status = oifs_check_version(OIFS_VERSION_MAJOR, OIFS_VERSION_MINOR, OIFS_VERSION_PATCH);
    assert_eq!(status, OIFS_VERSION_COMPAT_OK);

    // Open
    let path = CString::new("./rustdisk.oifs").unwrap();
    let fs = oifs_open(path.as_ptr(), 10 * 1024 * 1024);
    assert!(!fs.is_null());

    // Write
    let file = CString::new("hello.txt").unwrap();
    let msg = b"Hello from Rust FFI!";
    assert_eq!(oifs_write_file(fs, file.as_ptr(), msg.as_ptr(), msg.len() as u64), 0);

    // Read
    let mut buf = vec![0u8; 128];
    let n = oifs_read_file(fs, file.as_ptr(), buf.as_mut_ptr(), buf.len() as u64);
    assert!(n > 0);
    buf.truncate(n as usize);
    println!("Read {} bytes: {}", n, String::from_utf8_lossy(&buf));

    // Close
    oifs_close(fs);
}
```

---

## Thread Safety Summary

| Aspect | Guarantee |
|--------|-----------|
| **Handle independence** | Each `OIFSHandle` has its own `last_error` mutex; no cross-handle error interference. |
| **Shared `DiskManager`** | Handles from `oifs_get_or_open` share the same `DiskManager` (via `Arc`); internal `RwLock` protects all operations. |
| **Panic safety** | Every `extern "C"` function wraps its body in `catch_unwind`; panics are caught, `last_error` set, and error code returned. |
| **Callback reentrancy** | `oifs_ls` callback runs under read lock; **do not** call FFI on the same handle from the callback (deadlock). |
| **Concurrent handles** | Different handles (different filesystem instances) can operate fully concurrently. |
| **Handle sharing** | Sharing a single handle across threads requires external synchronization (mutex). |

---

## File I/O Reference

### Offset Read (Zero-Copy for Uncompressed Files)

```c
int64_t oifs_read_at(OIFSHandle* handle, const char* filename, uint64_t offset,
                     uint8_t* buf, uint64_t buf_size);
```

- Returns bytes read (`≥ 0`) or `-1` on error.
- For uncompressed files, copies directly from mmap blocks (no heap allocation).
- `oifs_read_file(handle, filename, buf, buf_size)` ≡ `oifs_read_at(handle, filename, 0, buf, buf_size)`.

### Sequential Write (Auto-Compression)

```c
int32_t oifs_write_file(OIFSHandle* handle, const char* filename,
                        const uint8_t* buf, uint64_t buf_size);
```

- Creates file if missing, writes at offset 0 with `CompressionMode::Auto`.
- Returns `0` on success, `-1` on error.

### Extended Write with Policy

```c
int32_t oifs_write_file_with_policy(OIFSHandle* handle, const char* filename,
                                    uint64_t offset, const uint8_t* buf, uint64_t buf_size,
                                    uint32_t policy_flags, int32_t zstd_level);
```

Policy flags:
- `OIFS_WRITE_POLICY_DEFAULT` (0x00): Auto / full-file stream compression.
- `OIFS_WRITE_POLICY_RAW` (0x01): Never compress.
- `OIFS_WRITE_POLICY_STREAM` (0x02): Stream compression with custom zstd level.
- `OIFS_WRITE_POLICY_SEEKABLE_64K` (0x08): Seekable 64KB chunked compression.

### Truncate

```c
int32_t oifs_truncate_file(OIFSHandle* handle, const char* filename, uint64_t new_size);
```

- Shrinks (discards blocks) or extends (sparse for raw files).

---

## Related Documentation

- [C FFI Interface Architecture](../architecture/ffi_interface.md) — Low-level specification, handle model, error mapping, thread-safety proofs.
- [FFI Usage Workflow](../workflows/ffi_workflow.md) — Step-by-step development and testing workflow.
- [CLI Reference](../operations/cli_reference.md) — Command-line tool usage for filesystem management.

---

## Test Validation

The FFI is validated through multiple test suites:

- **Native C/C++ E2E** (`tests/ffi_c_cpp_test.rs`): Compiles and runs C/C++ programs against the shared library, exercising version handshake, open/write/read/ls/delete, encryption with good/bad passwords, I/O backend switching, RAII, lambdas, and multi-threaded concurrency.
- **Extended FFI Tests** (`tests/ffi_extended_test.rs`): Version API, encryption workflow, `oifs_loaded_path`, error diagnostics.
- **Robustness Tests** (`tests/ffi_robustness_test.rs`): Null-pointer handling, UTF-8 rejection, buffer boundaries, empty/zero-size I/O, session reuse, cross-block offset reads.
- **Formal Verification** (`src/ffi.rs` Kani harness): Proves `oifs_check_version` policy soundness for all possible inputs.
