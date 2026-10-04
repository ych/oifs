---
type: integration guide
title: FFI Integration Guide
description: Detailed instructions for integrating OIFS shared library with C/C++ projects, including build linking and error handling.
tags: [ffi, integration, c, cpp, build]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-04T10:12:53.730Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-f9a8a5ec259e5381eba4c51a
    resource: repo://include/oifs.h
  - id: openwiki-source-c9e5b32aad7cafdb095c81a4
    resource: repo://src/ffi.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-04T10:12:53.730Z" }
---

# OIFS FFI Integration Guide

This guide explains how to integrate the OIFS shared library (`liboifs.so` or `liboifs.dylib`) into C/C++ projects. The OIFS FFI provides a stable C ABI interface for filesystem operations.

## Library Build

The OIFS shared library is built as part of the Rust project using Cargo:

```bash
# Build release version
cargo build --release

# The shared library will be placed in:
#   target/release/liboifs.so (Linux)
#   target/release/liboifs.dylib (macOS)
#   target/release/oifs.dll (Windows)
```

## Header Inclusion

Include the public FFI header in your C/C++ source:

```c
#include <oifs.h>
```

Ensure the compiler can locate the header by adding the `include` directory to your include path:

```bash
# Example with gcc/clang
-I/path/to/oifs/include
```

## Linking

Link against the shared library and any required system dependencies:

```bash
# Linux example
-L/path/to/oifs/target/release -loifs -ldl

# macOS example
-L/path/to/oifs/target/release -loifs

# Windows example (MSVC)
/LIBPATH:/path/to/oifs/target/release oifs.lib
```

Note: On Linux, `-ldl` is required for the `dladdr` function used in `oifs_loaded_path`.

## Basic Usage

### Opening a Filesystem

```c
#include <oifs.h>
#include <stdio.h>

int main() {
    // Open or create a 1GB filesystem at "./mydisk.oifs"
    OIFSHandle* fs = oifs_open("./mydisk.oifs", 1ULL * 1024 * 1024 * 1024);
    if (!fs) {
        char err[256];
        oifs_last_error(fs, err, sizeof(err));
        fprintf(stderr, "Failed to open filesystem: %s\n", err);
        return 1;
    }

    // Use the filesystem...
    
    // Clean up
    oifs_close(fs);
    return 0;
}
```

### Directory Listing

```c
// Callback function for directory listing
void list_callback(const char* name, uint64_t size, uint64_t mtime, void* user_data) {
    printf("%s  %lu bytes  modified: %lu\n", name, size, mtime);
}

// Usage
oifs_ls(fs, list_callback, NULL);
```

### File Operations

```c
// Create a file
if (oifs_create_file(fs, "hello.txt") != 0) {
    char err[256];
    oifs_last_error(fs, err, sizeof(err));
    fprintf(stderr, "Create failed: %s\n", err);
}

// Write data
const char* msg = "Hello, OIFS!";
if (oifs_write_file(fs, "hello.txt", (const uint8_t*)msg, strlen(msg)) != 0) {
    char err[256];
    oifs_last_error(fs, err, sizeof(err));
    fprintf(stderr, "Write failed: %s\n", err);
}

// Read data
uint8_t buffer[128];
int64_t bytes = oifs_read_file(fs, "hello.txt", buffer, sizeof(buffer));
if (bytes > 0) {
    printf("Read %ld bytes: %.*s\n", bytes, (int)bytes, buffer);
} else {
    char err[256];
    oifs_last_error(fs, err, sizeof(err));
    fprintf(stderr, "Read failed: %s\n", err);
}
```

## Error Handling

Most OIFS functions return `0` on success and a negative value on error. Retrieve error messages using `oifs_last_error`:

```c
char error_buffer[256];
if (oifs_some_operation(fs, ...) != 0) {
    oifs_last_error(fs, error_buffer, sizeof(error_buffer));
    fprintf(stderr, "Operation failed: %s\n", error_buffer);
}
```

Note: The error buffer must be null-terminated by the caller if space permits. The function returns the number of characters written (excluding null terminator) or `-1` on buffer error.

## Version Compatibility

OIFS uses semantic versioning with a strict compatibility policy: only older versions are blocked. Use the version check functions to ensure compatibility:

```c
// Check exact version match (recommended)
if (oifs_check_version(0, 1, 0) != OIFS_VERSION_COMPAT_OK) {
    char path[512];
    if (oifs_loaded_path(path, sizeof(path)) == 0) {
        fprintf(stderr, 
            "Incompatible OIFS library loaded from '%s' (expected v0.1.0)\n",
            path);
    }
    exit(1);
}

// Convenience macro (does exact version check)
#define OIFS_CHECK_VERSION() \
    oifs_check_version(OIFS_VERSION_MAJOR, OIFS_VERSION_MINOR, OIFS_VERSION_PATCH)

// Usage
if (OIFS_CHECK_VERSION() != OIFS_VERSION_COMPAT_OK) {
    // Handle version mismatch
}
```

Version return values:
- `OIFS_VERSION_COMPAT_OK (0)`: Exact version match
- `OIFS_VERSION_COMPAT_WARN (1)`: Newer version (compatible, but may warn)
- `OIFS_VERSION_COMPAT_ERR (-1)`: Older version (incompatible)

## I/O Backend Configuration

Configure the I/O backend for performance tuning:

```c
// Available backends (defined in oifs.h)
// OIFS_IO_BACKEND_MMAP     0 (default)
// OIFS_IO_BACKEND_PREAD    1
// OIFS_IO_BACKEND_IO_URING 2 (Linux only)

// Set backend
if (oifs_set_io_backend(fs, OIFS_IO_BACKEND_IO_URING) != 0) {
    char err[256];
    oifs_last_error(fs, err, sizeof(err));
    fprintf(stderr, "Failed to set I/O backend: %s\n", err);
}

// Get current backend
int backend = oifs_get_io_backend(fs);
```

## Password-Protected Filesystems

OIFS supports optional AES-256 encryption for filesystems:

```c
// Open with password
OIFSHandle* fs = oifs_open_with_password(
    "./secure.oifs", 
    1ULL * 1024 * 1024 * 1024, 
    "my-secret-password"
);
if (!fs) {
    // Handle error as above
}
```

Note: Password handling follows the same UTF-8 validation and error reporting as other string parameters.

## Thread Safety

The OIFS handle is **not** thread-safe. Each thread requiring filesystem access should:
1. Open its own handle using `oifs_open`/`oifs_get_or_open`, or
2. Implement external synchronization when sharing a handle between threads.

Filesystem operations on different handles (referring to different filesystem instances) can proceed concurrently without synchronization.

## Memory Management

- All memory allocated by the library is managed internally.
- Callers must not attempt to free pointers returned by OIFS functions.
- The `oifs_close` function releases all resources associated with a handle.
- Handles obtained via `oifs_get_or_open` or `oifs_get_or_open_with_password` must also be released with `oifs_close`.

## Building Against Specific Versions

To ensure compatibility with a specific OIFS version, check the version at runtime:

```c
// In your build system (e.g., CMake), you might define:
// #define OIFS_REQUIRED_MAJOR 0
// #define OIFS_REQUIRED_MINOR 1
// #define OIFS_REQUIRED_PATCH 0

// Then in code:
if (oifs_check_version(
        OIFS_REQUIRED_MAJOR,
        OIFS_REQUIRED_MINOR,
        OIFS_REQUIRED_PATCH) != OIFS_VERSION_COMPAT_OK) {
    // Handle incompatible version
}
```

## Related Documentation

- [FFI Interface Details](../architecture/ffi_interface.md) - Low-level FFI specification
- [CLI Reference](../operations/cli_reference.md) - Command-line tool usage
- [FFI Workflow](../workflows/ffi_workflow.md) - Development and testing workflows
