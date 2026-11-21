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

/* Opaque handle representing a shared read-lock session (amortizes RwLock & Inode lookups) */
typedef struct OIFSReadSession OIFSReadSession;

/* Opaque handle representing a pinned file within an OIFSReadSession */
typedef struct OIFSFileHandle OIFSFileHandle;

/* Opaque handle representing an in-flight or inline-completed asynchronous operation */
typedef struct OIFSAsyncFuture OIFSAsyncFuture;

/* Callback function type for directory listing: oifs_ls */
typedef void (*ListCallback)(const char *name, uint64_t size, uint64_t mtime, void *user_data);

/* Callback function type for asynchronous read completion: oifs_read_at_async_cb */
typedef void (*OifsAsyncReadCallback)(int64_t status_or_len, const uint8_t *data, size_t data_len, void *user_data);

/* Callback function type for asynchronous write/create completion: oifs_create_file_with_data_async_cb */
typedef void (*OifsAsyncStatusCallback)(int64_t status_or_inode, void *user_data);

/*
 * Storage Mode & Durability Constants
 */
#define OIFS_STORAGE_MODE_MMAP      0  /* Memory-mapped container (default) */
#define OIFS_STORAGE_MODE_FILE_IO   1  /* Pure positional pread/pwrite (zero mmap) */

#define OIFS_DURABILITY_LAZY        0  /* OS page-cache writeback (default) */
#define OIFS_DURABILITY_RANGE_ASYNC 1  /* Batched dirty-range async flush */
#define OIFS_DURABILITY_STRICT      2  /* Synchronous msync / sync_data barriers */

/*
 * Lifecycle Management
 */
OIFSHandle* oifs_open(const char *path, uint64_t size);
OIFSHandle* oifs_open_with_password(const char *path, uint64_t size, const char *password);
OIFSHandle* oifs_open_with_options(
    const char *path,
    uint64_t size,
    const char *password,
    uint8_t storage_mode,
    uint8_t journaled
);

/**
 * Opens an existing OIFS image in read-only mode (shared F_RDLCK lock, zero writes).
 *
 * Works on read-only (0444) images and permits multiple concurrent read-only processes
 * to inspect the same image simultaneously.
 *
 * @param path Path to an existing OIFS image file
 * @param password Password for encrypted images, or NULL/empty for unencrypted images
 * @param storage_mode Storage engine mode (OIFS_STORAGE_MODE_MMAP or OIFS_STORAGE_MODE_FILE_IO)
 * @return Handle on success, or NULL on error
 */
OIFSHandle* oifs_open_read_only(
    const char *path,
    const char *password,
    uint8_t storage_mode
);

/**
 * Queries whether an OIFS handle was opened in read-only mode.
 *
 * @return 1 if read-only, 0 if read-write, -1 if handle is NULL
 */
int32_t oifs_is_read_only(const OIFSHandle *handle);

OIFSHandle* oifs_get_or_open(const char *path, uint64_t size);
OIFSHandle* oifs_get_or_open_with_password(const char *path, uint64_t size, const char *password);
void oifs_close(OIFSHandle *handle);
int32_t oifs_flush(OIFSHandle *handle);
int32_t oifs_set_durability_mode(OIFSHandle *handle, uint8_t mode);
int32_t oifs_set_file_io_cache_bytes(OIFSHandle *handle, size_t bytes);

/*
 * Directory Operations
 */
int32_t oifs_ls(OIFSHandle *handle, ListCallback cb, void *user_data);
int32_t oifs_mkdir(OIFSHandle *handle, const char *path);

/*
 * File Operations
 */
int32_t oifs_create_file(OIFSHandle *handle, const char *path);
int32_t oifs_create_or_open_file(OIFSHandle *handle, const char *path);
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
 * Write Policy Flags
 */
#define OIFS_WRITE_POLICY_DEFAULT       0x00  /* Auto / Full-file stream compression (default) */
#define OIFS_WRITE_POLICY_RAW           0x01  /* Never compress (Raw) */
#define OIFS_WRITE_POLICY_STREAM        0x02  /* Stream compression (with custom zstd_level) */
#define OIFS_WRITE_POLICY_SEEKABLE_64K  0x08  /* Seekable 64KB chunked compression */

/**
 * Extended write interface supporting write policies, offsets, and custom zstd compression level.
 *
 * @param handle OIFS filesystem handle
 * @param filename Target file path
 * @param offset Logical byte offset to write at
 * @param buf Data buffer pointer
 * @param buf_size Number of bytes to write
 * @param policy_flags Policy flags (OIFS_WRITE_POLICY_*)
 * @param zstd_level Zstd compression level (1 ~ 19, or 0 for default)
 * @return 0 on success, -1 on failure (error accessible via oifs_last_error)
 */
int32_t oifs_write_file_with_policy(
    OIFSHandle *handle,
    const char *filename,
    uint64_t offset,
    const uint8_t *buf,
    uint64_t buf_size,
    uint32_t policy_flags,
    int32_t zstd_level
);

/**
 * Truncates or extends a file to the specified size in bytes.
 *
 * If new_size < current size, trailing bytes and unused blocks are discarded and freed.
 * If new_size > current size, the file is extended (sparse for raw files).
 *
 * @param handle OIFS filesystem handle
 * @param filename File path to truncate
 * @param new_size Target file size in bytes
 * @return 0 on success, -1 on failure (error string accessible via oifs_last_error)
 */
int32_t oifs_truncate_file(OIFSHandle *handle, const char *filename, uint64_t new_size);

/*
 * Direct Inode, Single-Transaction Creation & Scatter-Gather Batch I/O
 */
int64_t oifs_resolve_path(OIFSHandle *handle, const char *path);

int64_t oifs_create_file_with_data(
    OIFSHandle *handle,
    const char *path,
    const uint8_t *buf,
    uint64_t buf_size,
    uint32_t policy_flags,
    int32_t zstd_level
);

int64_t oifs_read_at_inode(
    OIFSHandle *handle,
    uint64_t inode_id,
    uint64_t offset,
    uint8_t *buf,
    uint64_t buf_size
);

typedef struct {
    uint64_t inode_id;
    uint64_t offset;
    uint8_t *buf;
    uint64_t buf_size;
    int64_t result;
} OIFSReadBatchItem;

int32_t oifs_read_at_batch(OIFSHandle *handle, OIFSReadBatchItem *items, size_t count);

/*
 * Shared Read Session, Pinned File Handle & True Zero-Copy Mmap View
 */
OIFSReadSession* oifs_begin_read(OIFSHandle *handle);
void oifs_end_read(OIFSReadSession *session);

OIFSFileHandle* oifs_session_open_file(const OIFSReadSession *session, const char *path);
OIFSFileHandle* oifs_session_open_inode(const OIFSReadSession *session, uint64_t inode_id);
void oifs_file_close(OIFSFileHandle *file);

uint64_t oifs_file_size(const OIFSFileHandle *file);
uint64_t oifs_file_inode_id(const OIFSFileHandle *file);
int64_t oifs_file_read_at(
    const OIFSFileHandle *file,
    uint64_t offset,
    uint8_t *buf,
    uint64_t buf_size
);

/**
 * True Zero-Copy view into the memory-mapped image for a pinned OIFSFileHandle.
 *
 * Returns:
 * -  0: Zero-copy succeeded; *out_ptr points directly to the mapped file bytes (valid while
 *       the OIFSFileHandle or its parent OIFSReadSession remains open) and *out_len is the
 *       clamped byte count.
 * -  1: Zero-copy unavailable (e.g. StorageMode::FileIo, compressed/encrypted file, sparse
 *       hole, or non-contiguous block boundary); caller should fall back to oifs_file_read_at.
 * - -1: Error or null pointer argument.
 */
int32_t oifs_file_mmap_slice(
    const OIFSFileHandle *file,
    uint64_t offset,
    uint64_t max_len,
    const uint8_t **out_ptr,
    uint64_t *out_len
);

/*
 * Asynchronous I/O & Group-Commit Write Engine (Future & Callback APIs)
 *
 * - Reads use an Inline Fast-Path: cache/mmap hits complete immediately on the calling thread
 *   (oifs_async_poll == 1, zero thread switch), while cold reads and compressed/encrypted
 *   decodes execute on the persistent background pool.
 * - Writes use Group Commit: concurrent async writes under OIFS_DURABILITY_STRICT share a
 *   single physical sync barrier per batch.
 * - Use-After-Free Safe: oifs_async_free waits for any in-flight write to `buf` to finish
 *   before returning.
 */
OIFSAsyncFuture* oifs_read_at_async(
    OIFSHandle *handle,
    uint64_t inode_id,
    uint64_t offset,
    uint8_t *buf,
    uint64_t buf_size
);

OIFSAsyncFuture* oifs_create_file_with_data_async(
    OIFSHandle *handle,
    const char *path,
    const uint8_t *buf,
    uint64_t buf_size,
    uint32_t policy_flags,
    int32_t zstd_level
);

OIFSAsyncFuture* oifs_write_file_async(
    OIFSHandle *handle,
    const char *path,
    uint64_t offset,
    const uint8_t *buf,
    uint64_t buf_size,
    uint32_t policy_flags,
    int32_t zstd_level
);

OIFSAsyncFuture* oifs_flush_async(OIFSHandle *handle);

int32_t oifs_async_poll(const OIFSAsyncFuture *fut);
int64_t oifs_async_wait(OIFSAsyncFuture *fut);
int32_t oifs_async_last_error(const OIFSAsyncFuture *fut, char *buf, uint32_t buf_size);
void oifs_async_free(OIFSAsyncFuture *fut);

int32_t oifs_read_at_async_cb(
    OIFSHandle *handle,
    uint64_t inode_id,
    uint64_t offset,
    uint64_t len,
    OifsAsyncReadCallback cb,
    void *user_data
);

int32_t oifs_create_file_with_data_async_cb(
    OIFSHandle *handle,
    const char *path,
    const uint8_t *buf,
    uint64_t buf_size,
    uint32_t policy_flags,
    int32_t zstd_level,
    OifsAsyncStatusCallback cb,
    void *user_data
);

/*
 * Diagnostics and Error Handling
 */
int32_t oifs_last_error(OIFSHandle *handle, char *buf, uint32_t buf_size);

/*
 * I/O Engine Backend Configuration
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

/*
 * Dynamic Library Versioning & Compatibility Handshake
 *
 * Policy: "Only block older versions; do NOT block newer versions."
 *
 * Compares the compile-time version (embedded in this header) against the
 * runtime version provided by the loaded liboifs dynamic library.
 */
#define OIFS_VERSION_MAJOR 1
#define OIFS_VERSION_MINOR 2
#define OIFS_VERSION_PATCH 0
#define OIFS_VERSION_STRING "1.2.0"
#define OIFS_VERSION_CODE (((uint64_t)OIFS_VERSION_MAJOR << 32) | ((uint64_t)OIFS_VERSION_MINOR << 16) | (uint64_t)OIFS_VERSION_PATCH)

/*
 * Dynamic Library Compatibility Status Codes
 */
#define OIFS_VERSION_COMPAT_OK       0  /* Exact expected version: fully verified, clean */
#define OIFS_VERSION_COMPAT_WARN     1  /* Newer version loaded: permitted, but emits warning */
#define OIFS_VERSION_COMPAT_ERR     -1  /* Older version loaded: incompatible, error out */

uint32_t oifs_version_major(void);
uint32_t oifs_version_minor(void);
uint32_t oifs_version_patch(void);
uint64_t oifs_version_code(void);
const char* oifs_version_string(void);

/**
 * Checks compatibility between the application binary and the loaded dynamic library.
 *
 * Policy:
 * - Returns  0 (OIFS_VERSION_COMPAT_OK):   Exact expected version match (clean).
 * - Returns  1 (OIFS_VERSION_COMPAT_WARN): Newer version loaded (warning, allowed to proceed).
 * - Returns -1 (OIFS_VERSION_COMPAT_ERR):  Older version loaded (error out, blocked).
 *
 * @param req_major Minimum required major version.
 * @param req_minor Minimum required minor version.
 * @param req_patch Minimum required patch version.
 */
int32_t oifs_check_version(uint32_t req_major, uint32_t req_minor, uint32_t req_patch);

/**
 * Retrieves the absolute filesystem path on disk from which this dynamic library was loaded.
 *
 * @param buf Target buffer to receive null-terminated path.
 * @param buf_size Size of target buffer in bytes.
 * @return 0 on success, or -1 on error/truncation.
 */
int32_t oifs_loaded_path(char *buf, size_t buf_size);

/**
 * Convenience macro: Evaluates the loaded dynamic library against compile-time header version.
 *
 * Usage pattern:
 *   int status = OIFS_CHECK_VERSION();
 *   if (status == OIFS_VERSION_COMPAT_ERR) {
 *       char path[512] = {0};
 *       oifs_loaded_path(path, sizeof(path));
 *       fprintf(stderr, "FATAL [liboifs]: Outdated library '%s' (v%s) loaded! Expected >= v%s.\n",
 *               path, oifs_version_string(), OIFS_VERSION_STRING);
 *       abort();
 *   } else if (status == OIFS_VERSION_COMPAT_WARN) {
 *       fprintf(stderr, "WARN [liboifs]: Newer library (v%s) loaded than compiled with (v%s).\n",
 *               oifs_version_string(), OIFS_VERSION_STRING);
 *   }
 */
#define OIFS_CHECK_VERSION() \
    oifs_check_version(OIFS_VERSION_MAJOR, OIFS_VERSION_MINOR, OIFS_VERSION_PATCH)

#ifdef __cplusplus
}
#endif

#endif /* OIFS_H */
