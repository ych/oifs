#ifndef OIFS_H
#define OIFS_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Endianness Detection & Portability Macros
 *
 * OIFS on-disk format, indirect block pointers, and IPC message framing
 * are standardized to Little-Endian across all architectures.
 */
#if defined(__BYTE_ORDER__) && defined(__ORDER_LITTLE_ENDIAN__) && (__BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__)
    #define OIFS_LITTLE_ENDIAN 1
    #define OIFS_BIG_ENDIAN    0
    #define OIFS_TO_LE16(x)    ((uint16_t)(x))
    #define OIFS_TO_LE32(x)    ((uint32_t)(x))
    #define OIFS_TO_LE64(x)    ((uint64_t)(x))
    #define OIFS_FROM_LE16(x)  ((uint16_t)(x))
    #define OIFS_FROM_LE32(x)  ((uint32_t)(x))
    #define OIFS_FROM_LE64(x)  ((uint64_t)(x))
#elif defined(__BYTE_ORDER__) && defined(__ORDER_BIG_ENDIAN__) && (__BYTE_ORDER__ == __ORDER_BIG_ENDIAN__)
    #define OIFS_LITTLE_ENDIAN 0
    #define OIFS_BIG_ENDIAN    1
    #define OIFS_TO_LE16(x)    __builtin_bswap16((uint16_t)(x))
    #define OIFS_TO_LE32(x)    __builtin_bswap32((uint32_t)(x))
    #define OIFS_TO_LE64(x)    __builtin_bswap64((uint64_t)(x))
    #define OIFS_FROM_LE16(x)  __builtin_bswap16((uint16_t)(x))
    #define OIFS_FROM_LE32(x)  __builtin_bswap32((uint32_t)(x))
    #define OIFS_FROM_LE64(x)  __builtin_bswap64((uint64_t)(x))
#else
    /* Fallback if compiler byte order macros are not present */
    #if defined(_WIN32) || defined(__x86_64__) || defined(__i386__) || defined(__aarch64__) || defined(__arm64__)
        #define OIFS_LITTLE_ENDIAN 1
        #define OIFS_BIG_ENDIAN    0
        #define OIFS_TO_LE16(x)    ((uint16_t)(x))
        #define OIFS_TO_LE32(x)    ((uint32_t)(x))
        #define OIFS_TO_LE64(x)    ((uint64_t)(x))
        #define OIFS_FROM_LE16(x)  ((uint16_t)(x))
        #define OIFS_FROM_LE32(x)  ((uint32_t)(x))
        #define OIFS_FROM_LE64(x)  ((uint64_t)(x))
    #else
        #error "Unable to determine target architecture endianness. Define __BYTE_ORDER__."
    #endif
#endif

/* Opaque handle representing an open OIFS filesystem instance */
typedef struct OIFSHandle OIFSHandle;

/* Callback function type for directory listing: oifs_ls */
typedef void (*ListCallback)(const char *name, uint64_t size, uint64_t mtime, void *user_data);

/*
 * Lifecycle Management
 */
OIFSHandle* oifs_open(const char *path, uint64_t size);
OIFSHandle* oifs_open_with_password(const char *path, uint64_t size, const char *password);
OIFSHandle* oifs_get_or_open(const char *path, uint64_t size);
OIFSHandle* oifs_get_or_open_with_password(const char *path, uint64_t size, const char *password);
void oifs_close(OIFSHandle *handle);

/*
 * Directory Operations
 */
int32_t oifs_ls(OIFSHandle *handle, ListCallback cb, void *user_data);
int32_t oifs_mkdir(OIFSHandle *handle, const char *path);

/*
 * File Operations
 */
int32_t oifs_create_file(OIFSHandle *handle, const char *path);
int32_t oifs_delete_file(OIFSHandle *handle, const char *path);

/*
 * High-Performance Direct I/O
 *
 * oifs_read_at performs chunked/direct read starting from arbitrary byte offset.
 * For uncompressed files, it directly copies from mmap blocks without heap allocations.
 * Returns bytes read on success, or -1 on error.
 */
int64_t oifs_read_at(
    OIFSHandle *handle,
    const char *filename,
    uint64_t offset,
    uint8_t *buf,
    uint64_t buf_size
);

/*
 * Sequential File Read / Write
 *
 * oifs_read_file delegates to oifs_read_at with offset 0.
 */
int64_t oifs_read_file(OIFSHandle *handle, const char *filename, uint8_t *buf, uint64_t buf_size);
int32_t oifs_write_file(OIFSHandle *handle, const char *filename, const uint8_t *buf, uint64_t buf_size);

/*
 * Diagnostics and Error Handling
 */
int32_t oifs_last_error(OIFSHandle *handle, char *buf, uint32_t buf_size);

/*
 * I/O Engine Backend Configuration (P3.2)
 *
 * Backend constants:
 * 0 = Mmap (default)
 * 1 = Pread
 * 2 = IoUring (Linux only, falls back to Pread if unsupported)
 */
#define OIFS_IO_BACKEND_MMAP     0
#define OIFS_IO_BACKEND_PREAD    1
#define OIFS_IO_BACKEND_IO_URING 2

int32_t oifs_set_io_backend(OIFSHandle *handle, uint8_t backend);
int32_t oifs_get_io_backend(OIFSHandle *handle);

#ifdef __cplusplus
}
#endif

#endif /* OIFS_H */
