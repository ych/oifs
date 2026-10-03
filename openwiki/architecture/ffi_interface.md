---
type: architecture
title: C FFI Interface
description: How OIFS exposes its storage engine as a standard C shared library (liboifs.so) through an opaque handle model, callback-driven directory iteration, zero-copy offset reads, and thread-safe error reporting.
tags: [ffi, c-api, shared-library, handles, callbacks, abi, no-mangle]
sources:
  - id: openwiki-source-c9e5b32aad7cafdb095c81a4
    resource: repo://src/ffi.rs
generated: { by: "pi", at: "2026-09-29T16:14:34.721Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-03T08:18:49.684Z
---

## Responsibility and ownership

The C Foreign Function Interface ([`src/ffi.rs`](src/ffi.rs)) exports the OIFS storage engine as a standard C-compatible shared library (`liboifs.so` on Linux, `liboifs.dylib` on macOS).

It bridges the idiomatic Rust engine (`DiskManager`, `OifsSession`, `RwLock`) to C/C++, Python, Go, and other host runtimes:
- Manages raw memory boundaries using opaque boxed pointers.
- Enforces null-pointer and UTF-8 safety checks on all foreign inputs.
- Preserves detailed error messages per handle via `oifs_last_error`.
- Exports unmangled C symbols via `#[unsafe(no_mangle)] pub extern "C" fn`.

## The opaque handle model

All operations require an opaque pointer to [`OIFSHandle`](src/ffi.rs#L10-L14):

```rust
pub struct OIFSHandle {
    pub dm: DiskManager,
    pub last_error: Option<String>,
}
```

- **Thread-Local / Handle-Isolated Errors**: Rather than using a global `errno` that risks race conditions in multithreaded host programs, each `OIFSHandle` maintains its own `last_error: Option<String>`. Successful calls clear `last_error` to `None`, while errors store human-readable descriptions.
- **Underlying Engine**: `OIFSHandle` embeds a cloned [`DiskManager`](src/disk.rs). Because `DiskManager` wraps its state in `Arc<RwLock<DiskManagerInner>>`, cloning is cheap and shares underlying memory maps and locks.

## Handle lifecycle: open, session reuse, and close

OIFS provides four entry points for instantiating handles, accommodating direct disk access or process-wide session caching:

### 1. Direct disk access
- [`oifs_open`](src/ffi.rs#L16-L34):
  ```c
  OIFSHandle* oifs_open(const char* path, uint64_t size);
  ```
  Opens an existing unencrypted image or creates a new image of `size` bytes. Returns a raw pointer `*mut OIFSHandle` or `NULL` on error.
- [`oifs_open_with_password`](src/ffi.rs#L36-L67):
  ```c
  OIFSHandle* oifs_open_with_password(const char* path, uint64_t size, const char* password);
  ```
  Opens or creates an encrypted OIFS image. The passphrase is used with Argon2id and the superblock salt to derive the 256-bit AEAD key.

### 2. Session registry and reference counting reuse
- [`oifs_get_or_open`](src/ffi.rs#L69-L90):
  ```c
  OIFSHandle* oifs_get_or_open(const char* path, uint64_t size);
  ```
  Consults the process-wide [`SESSION_REGISTRY`](src/session.rs#L77). If the image is already open, increments its reference count and shares the existing `DiskManager` instance. This prevents file locking contention (`F_SETLK`) within a single process.
- [`oifs_get_or_open_with_password`](src/ffi.rs#L92-L126):
  ```c
  OIFSHandle* oifs_get_or_open_with_password(const char* path, uint64_t size, const char* password);
  ```
  Performs session registry lookup with password validation.

### 3. Handle destruction
- [`oifs_close`](src/ffi.rs#L128-L135):
  ```c
  void oifs_close(OIFSHandle* handle);
  ```
  Reclaims the heap allocation via `Box::from_raw(handle)` and drops the handle. Dropping the embedded `DiskManager` decrements reference counts and triggers `DiskManagerInner::drop()`, ensuring `mmap.flush()` commits all changes to disk. It is safe to pass `NULL` (no-op).

## Filesystem operations

### Directory iteration via C callback

Because C lacks standard iterators and closures, [`oifs_ls`](src/ffi.rs#L140-L186) provides a C-style callback mechanism:

```c
typedef void (*ListCallback)(const char* name, uint64_t size, uint64_t mtime, void* user_data);
int oifs_ls(OIFSHandle* handle, ListCallback cb, void* user_data);
```

- Traverses root directory entries using [`DirectoryIterator`](src/directory.rs).
- For each entry, invokes `cb(c_name.as_ptr(), inode.size, inode.modified_at, user_data)`.
- The caller passes arbitrary state via `user_data` without requiring global variables.
- Returns `0` on success or `-1` on error.

### Path resolution, creation, and deletion

- [`oifs_create_file`](src/ffi.rs#L188-L217):
  ```c
  int oifs_create_file(OIFSHandle* handle, const char* path);
  ```
  Allocates an inode and links a new file entry under the root directory. Returns `0` on success, `-1` on error.
- [`oifs_delete_file`](src/ffi.rs#L219-L249):
  ```c
  int oifs_delete_file(OIFSHandle* handle, const char* path);
  ```
  Removes a file from the root directory and deallocates its blocks and inode. Returns `0` on success, `-1` on error.
- [`oifs_mkdir`](src/ffi.rs#L353-L393):
  ```c
  int oifs_mkdir(OIFSHandle* handle, const char* path);
  ```
  Resolves parent path using [`DiskManager::resolve_parent`](src/disk.rs#L1370) and creates a directory inode with initialized directory data block. Returns `0` on success, `-1` on error.

### File reading and writing

- [`oifs_read_at`](src/ffi.rs#L251-L293):
  ```c
  int64_t oifs_read_at(OIFSHandle* handle, const char* filename, uint64_t offset, uint8_t* buf, uint64_t buf_size);
  ```
  Reads up to `buf_size` bytes starting at `offset` into caller-provided buffer `buf`. For uncompressed files, executes zero-copy memory copies directly from mmap. Returns the number of bytes read ($\ge 0$), or `-1` on failure.
- [`oifs_read_file`](src/ffi.rs#L295-L303):
  ```c
  int64_t oifs_read_file(OIFSHandle* handle, const char* filename, uint8_t* buf, uint64_t buf_size);
  ```
  Convenience alias delegating to `oifs_read_at(handle, filename, 0, buf, buf_size)`.
- [`oifs_write_file`](src/ffi.rs#L305-L351):
  ```c
  int oifs_write_file(OIFSHandle* handle, const char* filename, const uint8_t* buf, uint64_t buf_size);
  ```
  Resolves parent path, creates the file if not present, and writes data from offset 0 with `CompressionMode::Auto`. Returns `0` on success, `-1` on error.

## Error handling conventions

The C FFI adheres to strict error reporting conventions:

1. **Integer Status Codes**:
   - Mutating functions (`oifs_create_file`, `oifs_delete_file`, `oifs_write_file`, `oifs_mkdir`, `oifs_ls`) return `int` (`0` = success, `-1` = failure).
   - Reading functions (`oifs_read_at`, `oifs_read_file`) return `int64_t` / `ssize_t` (positive/zero byte count = success, `-1` = failure).
   - Constructor functions (`oifs_open`, `oifs_get_or_open`) return non-null pointer on success, `NULL` on failure.
2. **Defensive Pointer and String Validation**:
   - Every function checks `if handle.is_null()`, returning immediately with `-1` or `NULL` without dereferencing (`src/ffi.rs#L18, L143, L191, L259`).
   - String parameters are converted via `CStr::from_ptr`. If the string is invalid UTF-8, the call sets `last_error = Some("Invalid UTF-8 filename".to_string())` and returns `-1` (`src/ffi.rs#L198-L201`).
3. **Retrieving Last Error Message**:
   - [`oifs_last_error`](src/ffi.rs#L394-L428):
     ```c
     int oifs_last_error(OIFSHandle* handle, char* buf, uint32_t buf_size);
     ```
     Copies the stored error description into `buf` up to `buf_size` bytes. It explicitly guarantees null-termination via `std::ptr::write(buf.add(to_copy - 1), 0)`.
