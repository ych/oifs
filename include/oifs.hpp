#ifndef OIFS_HPP
#define OIFS_HPP

#include "oifs.h"

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <string_view>
#include <type_traits>
#include <utility>
#include <vector>

#if __cplusplus >= 202002L
#include <span>
#endif

namespace oifs {

/**
 * Exception thrown by OIFS C++ operations on failure.
 */
class Error : public std::runtime_error {
public:
    explicit Error(const std::string& msg) : std::runtime_error(msg) {}
    explicit Error(const char* msg)
        : std::runtime_error(msg != nullptr ? msg : "Unknown OIFS error") {}
};

/**
 * Storage engine mode for OIFS container images.
 */
enum class StorageMode : uint8_t {
    Mmap = OIFS_STORAGE_MODE_MMAP,
    FileIo = OIFS_STORAGE_MODE_FILE_IO,
};

/**
 * Durability mode controlling physical sync barriers.
 */
enum class DurabilityMode : uint8_t {
    Lazy = OIFS_DURABILITY_LAZY,
    RangeAsync = OIFS_DURABILITY_RANGE_ASYNC,
    Strict = OIFS_DURABILITY_STRICT,
};

/**
 * Write compression policy flags.
 */
enum class WritePolicy : uint32_t {
    Default = OIFS_WRITE_POLICY_DEFAULT,
    Raw = OIFS_WRITE_POLICY_RAW,
    Stream = OIFS_WRITE_POLICY_STREAM,
    Seekable64K = OIFS_WRITE_POLICY_SEEKABLE_64K,
};

/**
 * Options for opening or creating an OIFS filesystem container.
 */
struct OpenOptions {
    uint64_t size = 0;
    std::string password{};
    StorageMode storage_mode = StorageMode::Mmap;
    bool journaled = false;
};

/**
 * Directory entry metadata returned by `FileSystem::ls()`.
 */
struct DirEntry {
    std::string name;
    uint64_t size = 0;
    uint64_t mtime = 0;
};

/**
 * Lightweight typed non-owning contiguous span (C++17 & C++20 compatible).
 * In C++20 mode, implicitly converts to `std::span<T>`.
 */
template <typename T>
class Span {
public:
    using element_type = T;
    using value_type = std::remove_cv_t<T>;
    using size_type = std::size_t;
    using pointer = T*;
    using reference = T&;
    using iterator = T*;

    constexpr Span() noexcept : data_(nullptr), size_(0) {}
    constexpr Span(T* data, std::size_t size) noexcept : data_(data), size_(size) {}

    constexpr T* data() const noexcept { return data_; }
    constexpr std::size_t size() const noexcept { return size_; }
    constexpr std::size_t size_bytes() const noexcept { return size_ * sizeof(T); }
    constexpr bool empty() const noexcept { return size_ == 0; }

    constexpr T* begin() const noexcept { return data_; }
    constexpr T* end() const noexcept { return data_ + size_; }
    constexpr T& operator[](std::size_t idx) const noexcept { return data_[idx]; }

#if __cplusplus >= 202002L && defined(__cpp_lib_span)
    constexpr operator std::span<T>() const noexcept { return std::span<T>(data_, size_); }
#endif

private:
    T* data_;
    std::size_t size_;
};

/**
 * Non-owning read-only view over a contiguous byte buffer.
 */
class ByteView {
public:
    constexpr ByteView() noexcept : data_(nullptr), size_(0) {}
    constexpr ByteView(const uint8_t* data, std::size_t size) noexcept
        : data_(data), size_(size) {}
    ByteView(const char* data, std::size_t size) noexcept
        : data_(reinterpret_cast<const uint8_t*>(data)), size_(size) {}
    ByteView(std::string_view sv) noexcept
        : data_(reinterpret_cast<const uint8_t*>(sv.data())), size_(sv.size()) {}
    ByteView(const std::vector<uint8_t>& vec) noexcept
        : data_(vec.data()), size_(vec.size()) {}

#if __cplusplus >= 202002L && defined(__cpp_lib_span)
    constexpr ByteView(std::span<const uint8_t> sp) noexcept
        : data_(sp.data()), size_(sp.size()) {}
#endif

    constexpr const uint8_t* data() const noexcept { return data_; }
    constexpr std::size_t size() const noexcept { return size_; }
    constexpr bool empty() const noexcept { return size_ == 0; }

    constexpr const uint8_t* begin() const noexcept { return data_; }
    constexpr const uint8_t* end() const noexcept { return data_ + size_; }
    constexpr uint8_t operator[](std::size_t idx) const noexcept { return data_[idx]; }

    std::string_view as_string_view() const noexcept {
        return std::string_view(reinterpret_cast<const char*>(data_), size_);
    }

    std::vector<uint8_t> to_vector() const {
        if (data_ == nullptr || size_ == 0) {
            return {};
        }
        return std::vector<uint8_t>(data_, data_ + size_);
    }

    /**
     * Reinterprets the byte view as a typed read-only span `Span<const T>`.
     * Validates both size multiple and pointer alignment.
     */
    template <typename T>
    Span<const std::remove_cv_t<T>> as_span() const {
        using ValueT = std::remove_cv_t<T>;
        static_assert(std::is_trivially_copyable_v<ValueT>,
                      "ByteView::as_span<T> requires a trivially copyable type");
        if (size_ % sizeof(ValueT) != 0) {
            throw Error("ByteView size is not a multiple of sizeof(T)");
        }
        if (size_ > 0 && (reinterpret_cast<std::uintptr_t>(data_) % alignof(ValueT) != 0)) {
            throw Error("ByteView pointer is not aligned to alignof(T)");
        }
        return Span<const ValueT>(reinterpret_cast<const ValueT*>(data_),
                                  size_ / sizeof(ValueT));
    }

private:
    const uint8_t* data_;
    std::size_t size_;
};

/**
 * Non-owning mutable view over a contiguous byte buffer.
 */
class MutableByteSpan {
public:
    constexpr MutableByteSpan() noexcept : data_(nullptr), size_(0) {}
    constexpr MutableByteSpan(uint8_t* data, std::size_t size) noexcept
        : data_(data), size_(size) {}
    MutableByteSpan(char* data, std::size_t size) noexcept
        : data_(reinterpret_cast<uint8_t*>(data)), size_(size) {}
    MutableByteSpan(std::vector<uint8_t>& vec) noexcept
        : data_(vec.data()), size_(vec.size()) {}

    template <std::size_t N>
    constexpr MutableByteSpan(uint8_t (&arr)[N]) noexcept : data_(arr), size_(N) {}

#if __cplusplus >= 202002L && defined(__cpp_lib_span)
    constexpr MutableByteSpan(std::span<uint8_t> sp) noexcept
        : data_(sp.data()), size_(sp.size()) {}
#endif

    constexpr uint8_t* data() const noexcept { return data_; }
    constexpr std::size_t size() const noexcept { return size_; }
    constexpr bool empty() const noexcept { return size_ == 0; }

    constexpr uint8_t* begin() const noexcept { return data_; }
    constexpr uint8_t* end() const noexcept { return data_ + size_; }
    constexpr uint8_t& operator[](std::size_t idx) const noexcept { return data_[idx]; }

private:
    uint8_t* data_;
    std::size_t size_;
};

class FileReadHandle;

/**
 * RocksDB-style zero-copy / 1-copy fallback read slice.
 *
 * - On `StorageMode::Mmap` raw contiguous extents (`oifs_file_mmap_slice == 0`),
 *   `PinnableSlice` pins the memory-mapped address directly (`is_pinned() == true`,
 *   0 memcpy, 0 heap allocation) while retaining a shared reference to the underlying
 *   `OIFSFileHandle` so the mapping remains valid for the lifetime of the slice.
 * - On `StorageMode::FileIo`, compressed, encrypted, sparse, or non-contiguous ranges
 *   (`oifs_file_mmap_slice == 1`), `PinnableSlice` automatically falls back to reading
 *   into its reusable internal `std::vector<uint8_t>` (`is_pinned() == false`).
 */
class PinnableSlice {
public:
    PinnableSlice() noexcept = default;
    ~PinnableSlice() noexcept = default;

    PinnableSlice(const PinnableSlice&) = delete;
    PinnableSlice& operator=(const PinnableSlice&) = delete;

    PinnableSlice(PinnableSlice&& other) noexcept
        : fallback_buf_(std::move(other.fallback_buf_)),
          pin_holder_(std::move(other.pin_holder_)),
          data_(other.pinned_ ? other.data_
                              : (other.size_ > 0 ? fallback_buf_.data() : nullptr)),
          size_(other.size_),
          pinned_(other.pinned_) {
        other.data_ = nullptr;
        other.size_ = 0;
        other.pinned_ = false;
    }

    PinnableSlice& operator=(PinnableSlice&& other) noexcept {
        if (this != &other) {
            bool was_pinned = other.pinned_;
            const uint8_t* ext_ptr = other.data_;
            std::size_t sz = other.size_;

            fallback_buf_ = std::move(other.fallback_buf_);
            pin_holder_ = std::move(other.pin_holder_);
            data_ = was_pinned ? ext_ptr : (sz > 0 ? fallback_buf_.data() : nullptr);
            size_ = sz;
            pinned_ = was_pinned;

            other.data_ = nullptr;
            other.size_ = 0;
            other.pinned_ = false;
        }
        return *this;
    }

    const uint8_t* data() const noexcept { return data_; }
    std::size_t size() const noexcept { return size_; }
    bool empty() const noexcept { return size_ == 0; }

    /**
     * Returns true when this slice points directly into the memory-mapped image
     * with zero copies, or false when backed by the internal fallback buffer.
     */
    bool is_pinned() const noexcept { return pinned_; }
    bool is_zero_copy() const noexcept { return pinned_; }

    ByteView view() const noexcept { return ByteView(data_, size_); }
    operator ByteView() const noexcept { return view(); }

    std::string_view as_string_view() const noexcept {
        return std::string_view(reinterpret_cast<const char*>(data_), size_);
    }

    template <typename T>
    Span<const std::remove_cv_t<T>> as_span() const {
        return view().template as_span<T>();
    }

    const uint8_t* begin() const noexcept { return data_; }
    const uint8_t* end() const noexcept { return data_ + size_; }
    uint8_t operator[](std::size_t idx) const noexcept { return data_[idx]; }

    /**
     * Releases any pinned handle reference and clears the slice view while retaining
     * internal vector capacity for future fallback reads.
     */
    void reset() noexcept {
        data_ = nullptr;
        size_ = 0;
        pinned_ = false;
        pin_holder_.reset();
    }

    void Reset() noexcept { reset(); }

private:
    friend class FileReadHandle;

    void pin_mmap(const uint8_t* ptr,
                  std::size_t len,
                  const std::shared_ptr<OIFSFileHandle>& holder) noexcept {
        data_ = ptr;
        size_ = len;
        pinned_ = true;
        if (pin_holder_.get() != holder.get()) {
            pin_holder_ = holder;
        }
    }

    uint8_t* prepare_fallback(std::size_t cap) {
        pin_holder_.reset();
        pinned_ = false;
        fallback_buf_.resize(cap);
        data_ = fallback_buf_.data();
        size_ = cap;
        return fallback_buf_.data();
    }

    void commit_fallback(std::size_t actual_len) noexcept {
        fallback_buf_.resize(actual_len);
        data_ = fallback_buf_.data();
        size_ = actual_len;
        pinned_ = false;
    }

    std::vector<uint8_t> fallback_buf_{};
    std::shared_ptr<OIFSFileHandle> pin_holder_{};
    const uint8_t* data_ = nullptr;
    std::size_t size_ = 0;
    bool pinned_ = false;
};

/**
 * Move-only RAII wrapper around `OIFSFileHandle*`.
 *
 * Internally holds a ref-counted `OIFSFileHandle` (which in Rust holds an `Arc` to the
 * shared read session), guaranteeing out-of-order destruction safety even if the parent
 * `ReadSession` is destroyed before `FileReadHandle` or a pinned `PinnableSlice`.
 */
class FileReadHandle {
public:
    FileReadHandle() noexcept = default;

    explicit FileReadHandle(OIFSFileHandle* raw)
        : handle_(raw, [](OIFSFileHandle* p) noexcept {
              if (p != nullptr) {
                  oifs_file_close(p);
              }
          }) {
        if (raw == nullptr) {
            throw Error("Failed to construct FileReadHandle from null OIFSFileHandle");
        }
    }

    ~FileReadHandle() noexcept = default;

    FileReadHandle(const FileReadHandle&) = delete;
    FileReadHandle& operator=(const FileReadHandle&) = delete;

    FileReadHandle(FileReadHandle&&) noexcept = default;
    FileReadHandle& operator=(FileReadHandle&&) noexcept = default;

    explicit operator bool() const noexcept { return handle_ != nullptr; }
    OIFSFileHandle* raw() const noexcept { return handle_.get(); }

    uint64_t size() const noexcept {
        return handle_ ? oifs_file_size(handle_.get()) : 0;
    }

    uint64_t inode_id() const noexcept {
        return handle_ ? oifs_file_inode_id(handle_.get()) : 0;
    }

    /**
     * Positional read into a caller-supplied mutable buffer without lock or inode-lookup overhead.
     */
    std::size_t read_at(uint64_t offset, MutableByteSpan dst) const {
        if (!handle_) {
            throw Error("FileReadHandle::read_at called on closed handle");
        }
        int64_t n = oifs_file_read_at(handle_.get(), offset, dst.data(), dst.size());
        if (n < 0) {
            throw Error("oifs_file_read_at failed");
        }
        return static_cast<std::size_t>(n);
    }

    /**
     * Attempts a true zero-copy memory-mapped slice view over `[offset, offset + max_len)`.
     * Returns `std::nullopt` when zero-copy is unavailable (`FileIo`, compressed, encrypted,
     * sparse hole, or non-contiguous block boundary).
     */
    std::optional<ByteView> try_mmap_view(uint64_t offset, uint64_t max_len) const {
        if (!handle_) {
            throw Error("FileReadHandle::try_mmap_view called on closed handle");
        }
        const uint8_t* ptr = nullptr;
        uint64_t len = 0;
        int32_t rc = oifs_file_mmap_slice(handle_.get(), offset, max_len, &ptr, &len);
        if (rc == 0) {
            return ByteView(ptr, static_cast<std::size_t>(len));
        }
        if (rc == 1) {
            return std::nullopt;
        }
        throw Error("oifs_file_mmap_slice failed");
    }

    /**
     * Populates `out` with either a pinned zero-copy mmap pointer (`out.is_pinned() == true`)
     * or a 1-copy fallback read into `out`'s internal buffer (`out.is_pinned() == false`).
     * Returns the number of bytes available in `out`.
     */
    std::size_t read_pinnable(uint64_t offset, uint64_t max_len, PinnableSlice& out) const {
        if (!handle_) {
            throw Error("FileReadHandle::read_pinnable called on closed handle");
        }
        const uint8_t* ptr = nullptr;
        uint64_t len = 0;
        int32_t rc = oifs_file_mmap_slice(handle_.get(), offset, max_len, &ptr, &len);
        if (rc == 0) {
            out.pin_mmap(ptr, static_cast<std::size_t>(len), handle_);
            return static_cast<std::size_t>(len);
        }
        if (rc < 0) {
            throw Error("oifs_file_mmap_slice failed in read_pinnable");
        }

        uint64_t fsize = size();
        if (offset >= fsize || max_len == 0) {
            out.prepare_fallback(0);
            return 0;
        }
        uint64_t avail = fsize - offset;
        std::size_t to_read = static_cast<std::size_t>(max_len < avail ? max_len : avail);
        uint8_t* buf = out.prepare_fallback(to_read);
        int64_t n = oifs_file_read_at(handle_.get(), offset, buf, to_read);
        if (n < 0) {
            out.reset();
            throw Error("oifs_file_read_at fallback failed in read_pinnable");
        }
        out.commit_fallback(static_cast<std::size_t>(n));
        return static_cast<std::size_t>(n);
    }

    /**
     * Convenience overload returning a newly populated `PinnableSlice`.
     */
    PinnableSlice read_pinnable(uint64_t offset, uint64_t max_len) const {
        PinnableSlice slice;
        read_pinnable(offset, max_len, slice);
        return slice;
    }

    void close() noexcept { handle_.reset(); }

private:
    std::shared_ptr<OIFSFileHandle> handle_{};
};

/**
 * Move-only RAII wrapper around `OIFSReadSession*`.
 * Amortizes outer `RwLock` acquisition across multiple file lookups and reads.
 */
class ReadSession {
public:
    ReadSession() noexcept = default;

    explicit ReadSession(OIFSReadSession* session) : session_(session) {
        if (session_ == nullptr) {
            throw Error("Failed to construct ReadSession from null OIFSReadSession");
        }
    }

    ~ReadSession() noexcept { close(); }

    ReadSession(const ReadSession&) = delete;
    ReadSession& operator=(const ReadSession&) = delete;

    ReadSession(ReadSession&& other) noexcept : session_(other.session_) {
        other.session_ = nullptr;
    }

    ReadSession& operator=(ReadSession&& other) noexcept {
        if (this != &other) {
            close();
            session_ = other.session_;
            other.session_ = nullptr;
        }
        return *this;
    }

    explicit operator bool() const noexcept { return session_ != nullptr; }
    OIFSReadSession* raw() const noexcept { return session_; }

    void close() noexcept {
        if (session_ != nullptr) {
            oifs_end_read(session_);
            session_ = nullptr;
        }
    }

    FileReadHandle open_file(std::string_view path) const {
        if (session_ == nullptr) {
            throw Error("ReadSession::open_file called on closed session");
        }
        std::string c_path(path);
        OIFSFileHandle* fh = oifs_session_open_file(session_, c_path.c_str());
        if (fh == nullptr) {
            throw Error("oifs_session_open_file failed for path: " + c_path);
        }
        return FileReadHandle(fh);
    }

    FileReadHandle open_inode(uint64_t inode_id) const {
        if (session_ == nullptr) {
            throw Error("ReadSession::open_inode called on closed session");
        }
        OIFSFileHandle* fh = oifs_session_open_inode(session_, inode_id);
        if (fh == nullptr) {
            throw Error("oifs_session_open_inode failed for inode " + std::to_string(inode_id));
        }
        return FileReadHandle(fh);
    }

private:
    OIFSReadSession* session_ = nullptr;
};

/**
 * Move-only RAII wrapper around `OIFSAsyncFuture*`.
 *
 * - Supports non-blocking `is_ready()` polling (Inline Fast-Path cache/mmap hits are
 *   already ready before `is_ready()` or `wait()` is called).
 * - Destructor calls `oifs_async_free`, which enforces a drop barrier if a background
 *   read is still writing into a caller-supplied buffer (preventing Use-After-Free).
 */
class AsyncFuture {
public:
    AsyncFuture() noexcept = default;

    explicit AsyncFuture(OIFSAsyncFuture* fut) : fut_(fut) {
        if (fut_ == nullptr) {
            throw Error("Failed to construct AsyncFuture from null OIFSAsyncFuture");
        }
    }

    ~AsyncFuture() noexcept { reset(); }

    AsyncFuture(const AsyncFuture&) = delete;
    AsyncFuture& operator=(const AsyncFuture&) = delete;

    AsyncFuture(AsyncFuture&& other) noexcept : fut_(other.fut_) {
        other.fut_ = nullptr;
    }

    AsyncFuture& operator=(AsyncFuture&& other) noexcept {
        if (this != &other) {
            reset();
            fut_ = other.fut_;
            other.fut_ = nullptr;
        }
        return *this;
    }

    explicit operator bool() const noexcept { return fut_ != nullptr; }
    OIFSAsyncFuture* raw() const noexcept { return fut_; }

    void reset() noexcept {
        if (fut_ != nullptr) {
            oifs_async_free(fut_);
            fut_ = nullptr;
        }
    }

    /**
     * Non-blocking readiness check. Returns true if the operation has completed.
     */
    bool is_ready() const noexcept {
        return fut_ != nullptr && oifs_async_poll(fut_) == 1;
    }

    /**
     * Blocks until the operation completes and returns the result (`bytes_read`,
     * created `inode_id`, or `0` for flush). Throws `oifs::Error` on failure.
     */
    int64_t wait() {
        if (fut_ == nullptr) {
            throw Error("AsyncFuture::wait called on empty future");
        }
        int64_t res = oifs_async_wait(fut_);
        if (res < 0) {
            throw Error(last_error());
        }
        return res;
    }

    /**
     * Retrieves the diagnostic error message from a failed future.
     */
    std::string last_error() const {
        if (fut_ == nullptr) {
            return "Empty AsyncFuture";
        }
        char buf[512] = {0};
        if (oifs_async_last_error(fut_, buf, sizeof(buf)) == 0) {
            return std::string(buf);
        }
        return "Unknown async error";
    }

private:
    OIFSAsyncFuture* fut_ = nullptr;
};

/**
 * Move-only RAII wrapper around `OIFSHandle*`.
 */
class FileSystem {
public:
    FileSystem() noexcept = default;

    explicit FileSystem(OIFSHandle* handle) : handle_(handle) {
        if (handle_ == nullptr) {
            throw Error("Failed to open OIFS FileSystem");
        }
    }

    ~FileSystem() noexcept { close(); }

    FileSystem(const FileSystem&) = delete;
    FileSystem& operator=(const FileSystem&) = delete;

    FileSystem(FileSystem&& other) noexcept : handle_(other.handle_) {
        other.handle_ = nullptr;
    }

    FileSystem& operator=(FileSystem&& other) noexcept {
        if (this != &other) {
            close();
            handle_ = other.handle_;
            other.handle_ = nullptr;
        }
        return *this;
    }

    explicit operator bool() const noexcept { return handle_ != nullptr; }
    OIFSHandle* raw() const noexcept { return handle_; }

    void close() noexcept {
        if (handle_ != nullptr) {
            oifs_close(handle_);
            handle_ = nullptr;
        }
    }

    static FileSystem open(std::string_view path, uint64_t size = 0) {
        std::string c_path(path);
        OIFSHandle* h = oifs_open(c_path.c_str(), size);
        if (h == nullptr) {
            throw Error("oifs_open failed for path: " + c_path);
        }
        return FileSystem(h);
    }

    static FileSystem open_with_password(std::string_view path,
                                         uint64_t size,
                                         std::string_view password) {
        std::string c_path(path);
        std::string c_pwd(password);
        OIFSHandle* h = oifs_open_with_password(
            c_path.c_str(), size, c_pwd.empty() ? nullptr : c_pwd.c_str());
        if (h == nullptr) {
            throw Error("oifs_open_with_password failed for path: " + c_path);
        }
        return FileSystem(h);
    }

    static FileSystem open_with_options(std::string_view path, const OpenOptions& opts) {
        std::string c_path(path);
        const char* pwd_ptr = opts.password.empty() ? nullptr : opts.password.c_str();
        OIFSHandle* h = oifs_open_with_options(c_path.c_str(),
                                               opts.size,
                                               pwd_ptr,
                                               static_cast<uint8_t>(opts.storage_mode),
                                               opts.journaled ? 1 : 0);
        if (h == nullptr) {
            throw Error("oifs_open_with_options failed for path: " + c_path);
        }
        return FileSystem(h);
    }

    static FileSystem open_with_options(std::string_view path,
                                        uint64_t size,
                                        std::string_view password,
                                        StorageMode storage_mode,
                                        bool journaled = false) {
        OpenOptions opts;
        opts.size = size;
        opts.password = std::string(password);
        opts.storage_mode = storage_mode;
        opts.journaled = journaled;
        return open_with_options(path, opts);
    }

    static FileSystem open_with_options(std::string_view path,
                                        uint64_t size,
                                        const char* password,
                                        uint8_t storage_mode,
                                        bool journaled = false) {
        std::string c_path(path);
        OIFSHandle* h = oifs_open_with_options(
            c_path.c_str(), size, password, storage_mode, journaled ? 1 : 0);
        if (h == nullptr) {
            throw Error("oifs_open_with_options failed for path: " + c_path);
        }
        return FileSystem(h);
    }

    /**
     * Opens an existing OIFS image in read-only mode (shared F_RDLCK lock, zero writes).
     * Works on 0444 images and allows concurrent read-only mounts across processes.
     */
    static FileSystem open_read_only(const std::string& path,
                                     const std::string& password = "",
                                     StorageMode mode = StorageMode::Mmap) {
        const char* pwd_ptr = password.empty() ? nullptr : password.c_str();
        OIFSHandle* h = oifs_open_read_only(
            path.c_str(), pwd_ptr, static_cast<uint8_t>(mode));
        if (h == nullptr) {
            throw Error("oifs_open_read_only failed for path: " + path);
        }
        return FileSystem(h);
    }

    /**
     * Returns true if this filesystem handle is mounted in read-only mode.
     */
    bool is_read_only() const noexcept {
        return handle_ != nullptr && oifs_is_read_only(handle_) == 1;
    }

    std::string last_error() const {
        if (handle_ == nullptr) {
            return "Closed FileSystem handle";
        }
        char buf[512] = {0};
        if (oifs_last_error(handle_, buf, sizeof(buf)) == 0) {
            return std::string(buf);
        }
        return "Unknown OIFS error";
    }

    void flush() {
        ensure_open();
        if (oifs_flush(handle_) != 0) {
            throw Error(last_error());
        }
    }

    void set_durability_mode(DurabilityMode mode) {
        set_durability_mode(static_cast<uint8_t>(mode));
    }

    void set_durability_mode(uint8_t mode) {
        ensure_open();
        if (oifs_set_durability_mode(handle_, mode) != 0) {
            throw Error("oifs_set_durability_mode failed");
        }
    }

    void set_file_io_cache_bytes(std::size_t bytes) {
        ensure_open();
        if (oifs_set_file_io_cache_bytes(handle_, bytes) != 0) {
            throw Error("oifs_set_file_io_cache_bytes failed");
        }
    }

    void mkdir(std::string_view path) {
        ensure_open();
        std::string c_path(path);
        if (oifs_mkdir(handle_, c_path.c_str()) != 0) {
            throw Error(last_error());
        }
    }

    void delete_file(std::string_view path) {
        ensure_open();
        std::string c_path(path);
        if (oifs_delete_file(handle_, c_path.c_str()) != 0) {
            throw Error(last_error());
        }
    }

    void truncate_file(std::string_view path, uint64_t new_size) {
        ensure_open();
        std::string c_path(path);
        if (oifs_truncate_file(handle_, c_path.c_str(), new_size) != 0) {
            throw Error(last_error());
        }
    }

    std::vector<DirEntry> ls() const {
        ensure_open();
        std::vector<DirEntry> entries;
        ListCallback cb = [](const char* name, uint64_t size, uint64_t mtime, void* user_data) {
            auto* out = static_cast<std::vector<DirEntry>*>(user_data);
            try {
                out->push_back(
                    DirEntry{name != nullptr ? std::string(name) : std::string{}, size, mtime});
            } catch (...) {
                // Never allow C++ exceptions to unwind across the extern "C" ABI boundary.
            }
        };
        if (oifs_ls(handle_, cb, &entries) != 0) {
            throw Error(last_error());
        }
        return entries;
    }

    uint64_t resolve_path(std::string_view path) const {
        ensure_open();
        std::string c_path(path);
        int64_t id = oifs_resolve_path(handle_, c_path.c_str());
        if (id < 0) {
            throw Error(last_error());
        }
        return static_cast<uint64_t>(id);
    }

    uint64_t create_file_with_data(std::string_view path,
                                   ByteView data,
                                   uint32_t policy_flags = OIFS_WRITE_POLICY_DEFAULT,
                                   int32_t zstd_level = 0) {
        ensure_open();
        std::string c_path(path);
        int64_t id = oifs_create_file_with_data(
            handle_, c_path.c_str(), data.data(), data.size(), policy_flags, zstd_level);
        if (id < 0) {
            throw Error(last_error());
        }
        return static_cast<uint64_t>(id);
    }

    uint64_t create_file_with_data(std::string_view path,
                                   ByteView data,
                                   WritePolicy policy,
                                   int32_t zstd_level = 0) {
        return create_file_with_data(path, data, static_cast<uint32_t>(policy), zstd_level);
    }

    void write_file(std::string_view path, ByteView data) {
        ensure_open();
        std::string c_path(path);
        if (oifs_write_file(handle_, c_path.c_str(), data.data(), data.size()) != 0) {
            throw Error(last_error());
        }
    }

    void write_file(std::string_view path,
                    uint64_t offset,
                    ByteView data,
                    uint32_t policy_flags = OIFS_WRITE_POLICY_DEFAULT,
                    int32_t zstd_level = 0) {
        ensure_open();
        std::string c_path(path);
        if (oifs_write_file_with_policy(handle_,
                                        c_path.c_str(),
                                        offset,
                                        data.data(),
                                        data.size(),
                                        policy_flags,
                                        zstd_level) != 0) {
            throw Error(last_error());
        }
    }

    void write_file(std::string_view path,
                    uint64_t offset,
                    ByteView data,
                    WritePolicy policy,
                    int32_t zstd_level = 0) {
        write_file(path, offset, data, static_cast<uint32_t>(policy), zstd_level);
    }

    std::size_t read_at(std::string_view path, uint64_t offset, MutableByteSpan dst) const {
        ensure_open();
        std::string c_path(path);
        int64_t n = oifs_read_at(handle_, c_path.c_str(), offset, dst.data(), dst.size());
        if (n < 0) {
            throw Error(last_error());
        }
        return static_cast<std::size_t>(n);
    }

    std::size_t read_at_inode(uint64_t inode_id, uint64_t offset, MutableByteSpan dst) const {
        ensure_open();
        int64_t n = oifs_read_at_inode(handle_, inode_id, offset, dst.data(), dst.size());
        if (n < 0) {
            throw Error(last_error());
        }
        return static_cast<std::size_t>(n);
    }

    void read_at_batch(OIFSReadBatchItem* items, std::size_t count) const {
        ensure_open();
        if (count == 0) {
            return;
        }
        if (oifs_read_at_batch(handle_, items, count) != 0) {
            throw Error(last_error());
        }
    }

    void read_at_batch(std::vector<OIFSReadBatchItem>& items) const {
        read_at_batch(items.data(), items.size());
    }

    ReadSession begin_read() const {
        ensure_open();
        OIFSReadSession* sess = oifs_begin_read(handle_);
        if (sess == nullptr) {
            throw Error("oifs_begin_read failed");
        }
        return ReadSession(sess);
    }

    AsyncFuture read_at_async(uint64_t inode_id, uint64_t offset, MutableByteSpan dst) const {
        ensure_open();
        OIFSAsyncFuture* fut =
            oifs_read_at_async(handle_, inode_id, offset, dst.data(), dst.size());
        if (fut == nullptr) {
            throw Error("oifs_read_at_async failed");
        }
        return AsyncFuture(fut);
    }

    AsyncFuture create_file_with_data_async(std::string_view path,
                                            ByteView data,
                                            uint32_t policy_flags = OIFS_WRITE_POLICY_DEFAULT,
                                            int32_t zstd_level = 0) {
        ensure_open();
        std::string c_path(path);
        OIFSAsyncFuture* fut = oifs_create_file_with_data_async(
            handle_, c_path.c_str(), data.data(), data.size(), policy_flags, zstd_level);
        if (fut == nullptr) {
            throw Error("oifs_create_file_with_data_async failed");
        }
        return AsyncFuture(fut);
    }

    AsyncFuture create_file_with_data_async(std::string_view path,
                                            ByteView data,
                                            WritePolicy policy,
                                            int32_t zstd_level = 0) {
        return create_file_with_data_async(
            path, data, static_cast<uint32_t>(policy), zstd_level);
    }

    AsyncFuture write_file_async(std::string_view path,
                                 uint64_t offset,
                                 ByteView data,
                                 uint32_t policy_flags = OIFS_WRITE_POLICY_DEFAULT,
                                 int32_t zstd_level = 0) {
        ensure_open();
        std::string c_path(path);
        OIFSAsyncFuture* fut = oifs_write_file_async(
            handle_, c_path.c_str(), offset, data.data(), data.size(), policy_flags, zstd_level);
        if (fut == nullptr) {
            throw Error("oifs_write_file_async failed");
        }
        return AsyncFuture(fut);
    }

    AsyncFuture write_file_async(std::string_view path,
                                 uint64_t offset,
                                 ByteView data,
                                 WritePolicy policy,
                                 int32_t zstd_level = 0) {
        return write_file_async(path, offset, data, static_cast<uint32_t>(policy), zstd_level);
    }

    AsyncFuture write_file_async(std::string_view path, ByteView data) {
        return write_file_async(path, 0, data, OIFS_WRITE_POLICY_DEFAULT, 0);
    }

    AsyncFuture flush_async() {
        ensure_open();
        OIFSAsyncFuture* fut = oifs_flush_async(handle_);
        if (fut == nullptr) {
            throw Error("oifs_flush_async failed");
        }
        return AsyncFuture(fut);
    }

    /**
     * Asynchronous read with a C++ callable/lambda callback.
     * The callback may accept either `(int64_t status, oifs::ByteView data)` or
     * `(int64_t status, const uint8_t* data, size_t len)`.
     */
    template <typename Fn>
    void read_at_async_cb(uint64_t inode_id, uint64_t offset, uint64_t len, Fn&& cb) const {
        ensure_open();
        using DecayFn = std::decay_t<Fn>;
        auto* heap_cb = new DecayFn(std::forward<Fn>(cb));
        OifsAsyncReadCallback trampoline =
            [](int64_t status, const uint8_t* data, std::size_t data_len, void* user_data) {
                std::unique_ptr<DecayFn> fn(static_cast<DecayFn*>(user_data));
                try {
                    ByteView view(status >= 0 ? data : nullptr, status >= 0 ? data_len : 0);
                    if constexpr (std::is_invocable_v<DecayFn, int64_t, ByteView>) {
                        (*fn)(status, view);
                    } else {
                        (*fn)(status, data, data_len);
                    }
                } catch (...) {
                    // Never allow C++ exceptions to unwind across the extern "C" ABI boundary.
                }
            };
        if (oifs_read_at_async_cb(handle_, inode_id, offset, len, trampoline, heap_cb) != 0) {
            delete heap_cb;
            throw Error("oifs_read_at_async_cb failed");
        }
    }

    /**
     * Asynchronous single-transaction file creation with a C++ callable/lambda callback
     * taking `(int64_t status_or_inode)`.
     */
    template <typename Fn,
              typename = std::enable_if_t<std::is_invocable_v<std::decay_t<Fn>, int64_t>>>
    void create_file_with_data_async_cb(std::string_view path,
                                        ByteView data,
                                        uint32_t policy_flags,
                                        int32_t zstd_level,
                                        Fn&& cb) {
        ensure_open();
        std::string c_path(path);
        using DecayFn = std::decay_t<Fn>;
        auto* heap_cb = new DecayFn(std::forward<Fn>(cb));
        OifsAsyncStatusCallback trampoline = [](int64_t status_or_inode, void* user_data) {
            std::unique_ptr<DecayFn> fn(static_cast<DecayFn*>(user_data));
            try {
                (*fn)(status_or_inode);
            } catch (...) {
                // Never allow C++ exceptions to unwind across the extern "C" ABI boundary.
            }
        };
        if (oifs_create_file_with_data_async_cb(handle_,
                                                c_path.c_str(),
                                                data.data(),
                                                data.size(),
                                                policy_flags,
                                                zstd_level,
                                                trampoline,
                                                heap_cb) != 0) {
            delete heap_cb;
            throw Error("oifs_create_file_with_data_async_cb failed");
        }
    }

    template <typename Fn,
              typename = std::enable_if_t<std::is_invocable_v<std::decay_t<Fn>, int64_t>>>
    void create_file_with_data_async_cb(std::string_view path,
                                        ByteView data,
                                        WritePolicy policy,
                                        int32_t zstd_level,
                                        Fn&& cb) {
        create_file_with_data_async_cb(
            path, data, static_cast<uint32_t>(policy), zstd_level, std::forward<Fn>(cb));
    }

    template <typename Fn,
              typename = std::enable_if_t<std::is_invocable_v<std::decay_t<Fn>, int64_t>>>
    void create_file_with_data_async_cb(std::string_view path, ByteView data, Fn&& cb) {
        create_file_with_data_async_cb(
            path, data, OIFS_WRITE_POLICY_DEFAULT, 0, std::forward<Fn>(cb));
    }

private:
    void ensure_open() const {
        if (handle_ == nullptr) {
            throw Error("Operation on closed oifs::FileSystem");
        }
    }

    OIFSHandle* handle_ = nullptr;
};

}  // namespace oifs

#endif  // OIFS_HPP
