---
type: workflow
title: FFI Usage Workflow
description: Step-by-step guide to using the C FFI interface for integrating OIFS with C/C++ applications, covering lifecycle management, file operations, error handling, and version compatibility.
tags: [ffi, c, integration, workflow]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-04T10:12:53.730Z
sources:
  - id: openwiki-source-c9e5b32aad7cafdb095c81a4
    resource: repo://src/ffi.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-04T10:12:53.730Z" }
---

# FFI Usage Workflow

<!-- openwiki: broken internal link [../include/oifs.h] file "../include/oifs.h" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [../src/ffi.rs] file "../src/ffi.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
This document outlines the typical workflow for using the OIFS C FFI interface from a C or C++ application. The FFI bindings are defined in [`oifs.h`](../include/oifs.h) and implemented in [`src/ffi.rs`](../src/ffi.rs).

## 1. Include the Header and Link the Library

Include the OIFS header in your C/C++ source:

```c
#include "oifs.h"
```

Link against the compiled dynamic library (`liboifs.so` on Linux, `liboifs.dylib` on macOS, `oifs.dll` on Windows) at build time.

## 2. Lifecycle Management

### Opening a Filesystem Image

Use `oifs_open` to open or create a new filesystem image:

```c
OIFSHandle* handle = oifs_open("/path/to/image.img", 10 * 1024 * 1024); // 10 MB
if (handle == NULL) {
    // Handle error (see error handling below)
}
```

For password-protected images, use `oifs_open_with_password`:

```c
OIFSHandle* handle = oifs_open_with_password("/path/to/encrypted.img", 0, "my_password");
```

The `size` parameter is ignored when opening an existing image; it is only used when creating a new image.

### Getting or Opening an Existing Handle

To obtain a handle to an already-open image (avoiding multiple opens), use `oifs_get_or_open` or `oifs_get_or_open_with_password`:

```c
OIFSHandle* handle = oifs_get_or_open("/path/to/image.img", 0);
```

### Closing the Handle

When finished, release the handle with `oifs_close`:

```c
oifs_close(handle);
```

## 3. Directory Operations

### Listing Directory Contents

Use `oifs_ls` with a callback function to list entries in the root directory:

```c
void my_list_callback(const char* name, uint64_t size, uint64_t mtime, void* user_data) {
    printf("Found: %s (size: %lu, mtime: %lu)\\n", name, size, mtime);
    // Optionally update user_data
}

int file_count = 0;
int result = oifs_ls(handle, my_list_callback, &file_count);
if (result != 0) {
    // Handle error
}
```

### Creating a Directory

Create a directory with `oifs_mkdir`:

```c
int result = oifs_mkdir(handle, "mydir");
if (result != 0) {
    // Handle error
}
```

## 4. File Operations

### Creating a File

Create an empty file with `oifs_create_file`:

```c
int result = oifs_create_file(handle, "myfile.txt");
if (result != 0) {
    // Handle error
}
```

### Deleting a File

Delete a file with `oifs_delete_file`:

```c
int result = oifs_delete_file(handle, "myfile.txt");
if (result != 0) {
    // Handle error
}
```

### Writing to a File

Write data to a file with `oifs_write_file`. If the file does not exist, it will be created:

```c
const char* data = "Hello, OIFS!";
int result = oifs_write_file(handle, "myfile.txt", (const uint8_t*)data, strlen(data));
if (result != 0) {
    // Handle error
}
```

### Reading from a File

Read data from a file using `oifs_read_file` (which reads from offset 0) or `oifs_read_at` for arbitrary offsets:

```c
uint8_t buffer[256];
int64_t bytes_read = oifs_read_file(handle, "myfile.txt", buffer, sizeof(buffer));
if (bytes_read < 0) {
    // Handle error
} else {
    // Process the data read (bytes_read bytes)
}
```

For reading at a specific offset:

```c
uint8_t buffer[256];
int64_t bytes_read = oifs_read_at(handle, "myfile.txt", 100, buffer, sizeof(buffer)); // Read from offset 100
```

## 5. I/O Engine Backend Configuration

Configure the I/O backend used for disk access:

```c
// Get current backend (defaults to 0: Mmap)
int backend = oifs_get_io_backend(handle);

// Set backend to 1: Pread
oifs_set_io_backend(handle, 1);

// Set backend to 2: IoUring (Linux only; falls back to Pread if unsupported)
oifs_set_io_backend(handle, 2);
```

Backend constants:
- `OIFS_IO_BACKEND_MMAP` (0)
- `OIFS_IO_BACKEND_PREAD` (1)
- `OIFS_IO_BACKEND_IO_URING` (2)

## 6. Error Handling

Most FFI functions return `0` on success and `-1` on error. To retrieve a human-readable error message, use `oifs_last_error`:

```c
char error_buffer[256];
int result = oifs_last_error(handle, error_buffer, sizeof(error_buffer));
if (result == 0) {
    fprintf(stderr, "OIFS error: %s\\n", error_buffer);
}
```

## 7. Version Compatibility

Check compatibility between the application and the loaded dynamic library:

```c
int status = oifs_check_version(0, 1, 0); // Request version 0.1.0
if (status == OIFS_VERSION_COMPAT_ERR) {
    // Older library loaded; handle incompatibility
    char path_buf[512];
    oifs_loaded_path(path_buf, sizeof(path_buf));
    fprintf(stderr, "FATAL: Outdated library loaded from %s\\n", path_buf);
    exit(1);
} else if (status == OIFS_VERSION_COMPAT_WARN) {
    // Newer library loaded; proceed with warning
    fprintf(stderr, "WARNING: Newer OIFS library loaded than expected\\n");
}
```

Convenience macro `OIFS_CHECK_VERSION()` performs the check using compile-time version constants.

You can also query individual version components:

```c
printf("OIFS version: %lu.%lu.%lu\\n",
       (unsigned long)oifs_version_major(),
       (unsigned long)oifs_version_minor(),
       (unsigned long)oifs_version_patch());
```

## 8. Retrieving Library Load Path

To determine the absolute path from which the dynamic library was loaded:

```c
char path_buf[1024];
if (oifs_loaded_path(path_buf, sizeof(path_buf)) == 0) {
    printf("Loaded library from: %s\\n", path_buf);
}
```

## Complete Example

See the test suite for a complete workflow example:
<!-- openwiki: broken internal link [../tests/ffi_test.rs] file "../tests/ffi_test.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`tests/ffi_test.rs`](../tests/ffi_test.rs) demonstrates basic create/list operations.
<!-- openwiki: broken internal link [../tests/ffi_extended_test.rs] file "../tests/ffi_extended_test.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- [`tests/ffi_extended_test.rs`](../tests/ffi_extended_test.rs) demonstrates encrypted flows, error handling, and version checks.

## Summary of Key Functions

| Function | Purpose |
|----------|---------|
| `oifs_open` / `oifs_open_with_password` | Open/create filesystem image |
| `oifs_get_or_open` / `oifs_get_or_open_with_password` | Get existing handle |
| `oifs_close` | Release handle |
| `oifs_ls` | List directory contents |
| `oifs_mkdir` | Create directory |
| `oifs_create_file` | Create file |
| `oifs_delete_file` | Delete file |
| `oifs_write_file` | Write file |
| `oifs_read_file` / `oifs_read_at` | Read file |
| `oifs_set_io_backend` / `oifs_get_io_backend` | Configure I/O backend |
| `oifs_last_error` | Retrieve last error message |
| `oifs_check_version` | Check version compatibility |
| `oifs_version_*` | Query version components |
| `oifs_loaded_path` | Get library load path |

Follow this workflow to integrate OIFS storage capabilities into your C/C++ applications safely and efficiently.
