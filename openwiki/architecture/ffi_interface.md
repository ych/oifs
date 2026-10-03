---
type: architecture
title: C FFI Interface
description: How OIFS exposes its storage engine as a standard C shared library (liboifs) through an opaque handle model, callback-driven directory iteration, zero-copy offset reads, pluggable I/O backend configuration, and thread-safe error reporting.
tags: [ffi, c-api, shared-library, handles, callbacks, abi, io_engine, no-mangle]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-03T11:29:24.571Z
sources:
  - id: openwiki-source-f9a8a5ec259e5381eba4c51a
    resource: repo://include/oifs.h
  - id: openwiki-source-c9e5b32aad7cafdb095c81a4
    resource: repo://src/ffi.rs
generated: { by: "antigravity", at: "2026-10-03T11:29:24.571Z" }
---

# C FFI Interface

<!-- openwiki: broken internal link [src/ffi.rs] file "src/ffi.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [include/oifs.h] file "include/oifs.h" does not exist. Fix the href or restore the target, then delete this comment. -->
The C Foreign Function Interface ([`src/ffi.rs`](src/ffi.rs), [`include/oifs.h`](include/oifs.h)) exports the OIFS storage engine as a standard C-compatible shared library (`liboifs.so` on Linux, `liboifs.dylib` on macOS).

It bridges the idiomatic Rust engine (`DiskManager`, `OifsSession`, `RwLock`, `IoEngine`) to C/C++, Python, Go, and other host runtimes:
- Manages raw memory boundaries using opaque boxed pointers.
- Enforces null-pointer and UTF-8 safety checks on all foreign inputs.
- Preserves detailed error messages per handle via `oifs_last_error`.
- Exports unmangled C symbols via `#[unsafe(no_mangle)] pub extern "C" fn`.
- Exposes runtime I/O engine configuration (`oifs_set_io_backend`, `oifs_get_io_backend`) for P3.2.

## The Opaque Handle Model

All operations require an opaque pointer to `OIFSHandle` (`src/ffi.rs#L11-L14`):

```rust
pub struct OIFSHandle {
    pub dm: DiskManager,
    pub last_error: Option<String>,
}
```

In C (`include/oifs.h#L52`):
```c
typedef struct OIFSHandle OIFSHandle;
```

- **Thread-Local / Handle-Isolated Errors**: Rather than using a global `errno` that risks race conditions in multithreaded host programs, each `OIFSHandle` maintains its own `last_error: Option<String>`. Successful calls clear `last_error` to `None`, while errors store human-readable descriptions.
- **Underlying Engine**: `OIFSHandle` embeds a cloned `DiskManager` (`src/disk.rs`). Because `DiskManager` wraps its inner state in `Arc<RwLock<DiskManagerInner>>`, cloning is lightweight and shares the underlying memory maps, cache layers, and locks.

## Handle Lifecycle: Open, Session Reuse, and Close

OIFS provides four entry points for instantiating handles, accommodating direct disk access or process-wide session caching:

### 1. Direct Disk Access
- `oifs_open` (`src/ffi.rs#L17-L37`):
  ```c
  OIFSHandle* oifs_open(const char* path, uint64_t size);
  ```
  Opens an existing unencrypted image or creates a new image of `size` bytes. Returns a raw pointer `*mut OIFSHandle` or `NULL` on error.
- `oifs_open_with_password` (`src/ffi.rs#L40-L73`):
  ```c
  OIFSHandle* oifs_open_with_password(const char* path, uint64_t size, const char* password);
  ```
  Opens or creates an encrypted OIFS image. The passphrase is used with Argon2id and the superblock salt to derive the 256-bit AEAD key.

### 2. Session Registry Reuse
- `oifs_get_or_open` (`src/ffi.rs#L76-L96`):
  ```c
  OIFSHandle* oifs_get_or_open(const char* path, uint64_t size);
  ```
  Consults the process-wide session registry (`OifsSession::get_or_open`). If the session resolves to a direct master instance, it returns an `OIFSHandle` sharing the underlying `DiskManager`.
- `oifs_get_or_open_with_password` (`src/ffi.rs#L99-L132`):
  ```c
  OIFSHandle* oifs_get_or_open_with_password(const char* path, uint64_t size, const char* password);
  ```
  Performs session registry lookup with password validation.

### 3. Handle Destruction
- `oifs_close` (`src/ffi.rs#L135-L141`):
  ```c
  void oifs_close(OIFSHandle* handle);
  ```
  Reclaims the heap allocation via `Box::from_raw(handle)` and drops the handle. Dropping the embedded `DiskManager` decrements reference counts and triggers `DiskManagerInner::drop()`, ensuring `mmap.flush()` commits all changes to disk. Passing `NULL` is a safe no-op.

## Filesystem Operations

### Directory Iteration via C Callback

Because C lacks standard iterators, `oifs_ls` (`src/ffi.rs#L147-L197`) provides a C callback mechanism:

```c
typedef void (*ListCallback)(const char* name, uint64_t size, uint64_t mtime, void* user_data);
int32_t oifs_ls(OIFSHandle* handle, ListCallback cb, void* user_data);
```

- Traverses root directory entries using `DirectoryIterator` (`src/directory.rs`).
- For each entry, invokes `cb(c_name.as_ptr(), inode.size, inode.modified_at, user_data)`.
- The caller passes arbitrary state via `user_data` without requiring global variables.
- Returns `0` on success or `-1` on error.

### Path Resolution, Creation, and Deletion

- `oifs_create_file` (`src/ffi.rs#L200-L230`):
  ```c
  int32_t oifs_create_file(OIFSHandle* handle, const char* path);
  ```
  Allocates an inode and links a new file entry under the root directory. Returns `0` on success, `-1` on error.
- `oifs_delete_file` (`src/ffi.rs#L233-L263`):
  ```c
  int32_t oifs_delete_file(OIFSHandle* handle, const char* path);
  ```
  Removes a file from the root directory and deallocates its blocks and inode. Returns `0` on success, `-1` on error.
- `oifs_mkdir` (`src/ffi.rs#L373-L411`):
  ```c
  int32_t oifs_mkdir(OIFSHandle* handle, const char* path);
  ```
  Resolves the parent path and creates a directory inode with an initialized directory block. Returns `0` on success, `-1` on error.

### File Reading and Writing

- `oifs_read_at` (`src/ffi.rs#L266-L310`):
  ```c
  int64_t oifs_read_at(OIFSHandle* handle, const char* filename, uint64_t offset, uint8_t* buf, uint64_t buf_size);
  ```
  Reads up to `buf_size` bytes starting at `offset` into caller-provided buffer `buf`. For uncompressed files under `Mmap`, executes zero-copy memory copies directly from mmap blocks. Under `Pread` and `IoUring`, uses coalesced extent reads. Returns bytes read ($\ge 0$), or `-1` on failure.
- `oifs_read_file` (`src/ffi.rs#L313-L320`):
  ```c
  int64_t oifs_read_file(OIFSHandle* handle, const char* filename, uint8_t* buf, uint64_t buf_size);
  ```
  Convenience alias delegating to `oifs_read_at(handle, filename, 0, buf, buf_size)`.
- `oifs_write_file` (`src/ffi.rs#L323-L370`):
  ```c
  int32_t oifs_write_file(OIFSHandle* handle, const char* filename, const uint8_t* buf, uint64_t buf_size);
  ```
  Resolves parent path, creates the file if not present, and writes data from offset 0 with `CompressionMode::Auto`. Returns `0` on success, `-1` on error.

## I/O Engine Backend Configuration (P3.2)

In P3.2, OIFS exposed runtime configuration of the payload-block read engine to foreign callers (`include/oifs.h#L106-L119`, `src/ffi.rs#L446-L468`):

```c
#define OIFS_IO_BACKEND_MMAP     0
#define OIFS_IO_BACKEND_PREAD    1
#define OIFS_IO_BACKEND_IO_URING 2

int32_t oifs_set_io_backend(OIFSHandle *handle, uint8_t backend);
int32_t oifs_get_io_backend(OIFSHandle *handle);
```

- `oifs_set_io_backend`: Configures the active I/O backend on the underlying `DiskManager`. If `OIFS_IO_BACKEND_IO_URING` is requested on an unsupported OS or kernel (< Linux 5.15), it automatically falls back to `Pread` and returns `1`. Returns `-1` if `handle` is null.
- `oifs_get_io_backend`: Queries the currently active effective backend (`0` for Mmap, `1` for Pread, `2` for IoUring).

## Error Handling Conventions

1. **Integer Status Codes**: Mutating functions return `int32_t` (`0` = success, `-1` = failure). Reading functions return `int64_t` (bytes read = success, `-1` = failure). Constructors return non-null pointer on success, `NULL` on failure.
2. **Defensive Pointer and String Validation**: Every function verifies `if handle.is_null()`. String parameters are converted via `CStr::from_ptr`. Non-UTF-8 strings set `last_error = Some("Invalid UTF-8 ...")` and return `-1`.
3. **Retrieving Last Error Message**:
   `oifs_last_error` (`src/ffi.rs#L413-L444`):
   ```c
   int32_t oifs_last_error(OIFSHandle* handle, char* buf, uint32_t buf_size);
   ```
   Copies the stored error description into `buf` up to `buf_size` bytes with guaranteed null termination via `std::ptr::write(buf.add(to_copy - 1), 0)`.
