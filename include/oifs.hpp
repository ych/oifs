#ifndef OIFS_HPP
#define OIFS_HPP

#include "oifs.h"

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <mutex>
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

#if __cplusplus >= 202002L && defined(__cpp_impl_coroutine)
#if __has_include(<coroutine>)
#define OIFS_HAS_CPP20_COROUTINE 1
#include <coroutine>
#include <thread>
#endif
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

namespace detail {

inline std::string to_c_string(std::string_view sv) {
    if (sv.find('\0') != std::string_view::npos) {
        throw Error("String contains embedded NUL byte");
    }
    return std::string(sv);
}

}  // namespace detail

/**
 * Checks dynamic library compatibility against the header (or requested) version.
 * Returns 0 (exact match), 1 (newer library loaded), or -1 (older incompatible library).
 */
inline int32_t check_version(uint32_t major = OIFS_VERSION_MAJOR,
                             uint32_t minor = OIFS_VERSION_MINOR,
                             uint32_t patch = OIFS_VERSION_PATCH) noexcept {
    return oifs_check_version(major, minor, patch);
}

/**
 * Returns the runtime version string of the loaded liboifs dynamic library.
 */
inline std::string_view version_string() noexcept {
    const char* s = oifs_version_string();
    return s != nullptr ? std::string_view(s) : std::string_view{};
}

/**
 * Returns the absolute filesystem path from which liboifs was loaded.
 */
inline std::string loaded_path() {
    char buf[4096] = {0};
    if (oifs_loaded_path(buf, sizeof(buf)) != 0) {
        throw Error("oifs_loaded_path failed");
    }
    return std::string(buf);
}

/**
 * Storage engine mode for OIFS container images.
 */
enum class StorageMode : uint8_t {
    Mmap = OIFS_STORAGE_MODE_MMAP,
    FileIo = OIFS_STORAGE_MODE_FILE_IO,
};

/**
 * Pluggable read/write I/O engine backend.
 */
enum class IoBackend : uint8_t {
    Mmap = OIFS_IO_BACKEND_MMAP,
    Pread = OIFS_IO_BACKEND_PREAD,
    IoUring = OIFS_IO_BACKEND_IO_URING,
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
 * Whole-image CoW clone method selected by the host OS.
 */
enum class CloneMethod : uint8_t {
    ApfsClonefile = OIFS_CLONE_METHOD_APFS_CLONEFILE,
    LinuxFiclone = OIFS_CLONE_METHOD_LINUX_FICLONE,
    StandardCopy = OIFS_CLONE_METHOD_STANDARD_COPY,
};

/**
 * Tier selected by cross-image file transfer or sub-range splice.
 */
enum class TransferMethod : uint8_t {
    Reflink = OIFS_TRANSFER_METHOD_REFLINK,
    CopyFileRange = OIFS_TRANSFER_METHOD_COPY_FILE_RANGE,
    RawBlockCopy = OIFS_TRANSFER_METHOD_RAW_BLOCK_COPY,
    LogicalReencode = OIFS_TRANSFER_METHOD_LOGICAL_REENCODE,
};

/**
 * Options for cross-image file transfer (`FileSystem::transfer_file_to`).
 */
struct TransferOptions {
    bool move_file = false;
    bool overwrite = false;
};

/**
 * Statistics returned by `FileSystem::transfer_file_to`.
 */
struct TransferStats {
    uint64_t src_inode = 0;
    uint64_t dst_inode = 0;
    uint64_t logical_bytes = 0;
    uint64_t physical_bytes = 0;
    uint64_t extents_transferred = 0;
    TransferMethod method = TransferMethod::LogicalReencode;
};

/**
 * Source range trim behavior after `FileSystem::splice_range_to`.
 */
enum class SpliceTrimMode : uint8_t {
    Keep = OIFS_SPLICE_TRIM_KEEP,
    PunchHole = OIFS_SPLICE_TRIM_PUNCH_HOLE,
    CollapseRange = OIFS_SPLICE_TRIM_COLLAPSE,
};

/**
 * Options for cross-image sub-range splice (`FileSystem::splice_range_to`).
 */
struct SpliceOptions {
    SpliceTrimMode trim_mode = SpliceTrimMode::Keep;
};

/**
 * Statistics returned by `FileSystem::splice_range_to`.
 */
struct SpliceStats {
    uint64_t src_inode = 0;
    uint64_t dst_inode = 0;
    uint64_t bytes_spliced = 0;
    uint64_t logical_rmw_bytes = 0;
    uint64_t body_fast_path_bytes = 0;
    uint64_t physical_bytes = 0;
    uint64_t extents_transferred = 0;
    uint64_t src_blocks_freed = 0;
    TransferMethod method = TransferMethod::LogicalReencode;
};

/**
 * SIMD pre-compression numeric filter pipeline configuration.
 */
struct FilterConfig {
    uint8_t typesize = 0;
    bool enable_delta = false;
    bool enable_shuffle = false;
    bool enable_bitshuffle = false;

    static constexpr FilterConfig none() noexcept {
        return FilterConfig{0, false, false, false};
    }

    static constexpr FilterConfig numeric(uint8_t elem_typesize) noexcept {
        return FilterConfig{elem_typesize, true, true, false};
    }

    static constexpr FilterConfig delta_only(uint8_t elem_typesize) noexcept {
        return FilterConfig{elem_typesize, true, false, false};
    }

    static constexpr FilterConfig shuffle_only(uint8_t elem_typesize) noexcept {
        return FilterConfig{elem_typesize, false, true, false};
    }

    static constexpr FilterConfig bitshuffle(uint8_t elem_typesize) noexcept {
        return FilterConfig{elem_typesize, false, false, true};
    }

    constexpr OIFSFilterConfig to_c() const noexcept {
        return OIFSFilterConfig{
            typesize,
            static_cast<uint8_t>(enable_delta ? 1 : 0),
            static_cast<uint8_t>(enable_shuffle ? 1 : 0),
            static_cast<uint8_t>(enable_bitshuffle ? 1 : 0),
        };
    }
};

/**
 * Inode metadata returned by `FileSystem::stat()`.
 */
struct FileStat {
    uint64_t inode_id = 0;
    uint64_t size = 0;
    uint64_t compressed_size = 0;
    uint64_t created_at = 0;
    uint64_t modified_at = 0;
    bool is_dir = false;
    bool encrypted = false;
    bool seekable_64k = false;
    uint8_t filter_typesize = 0;
    bool filter_delta = false;
    bool filter_shuffle = false;
    bool filter_bitshuffle = false;
};

/**
 * Structural integrity check report returned by `FileSystem::verify_integrity()`.
 */
struct FsckReport {
    bool is_clean = false;
    uint64_t orphan_inodes = 0;
    uint64_t leaked_blocks = 0;
    uint64_t missing_blocks = 0;
    uint64_t cross_linked_blocks = 0;
};

/**
 * Data-block fragmentation report returned by `FileSystem::analyze_fragmentation()`.
 */
struct FragmentationStats {
    uint64_t total_blocks = 0;
    uint64_t used_blocks = 0;
    uint64_t free_blocks = 0;
    uint64_t free_runs = 0;
    uint64_t largest_free_run = 0;
    double avg_gap_size = 0.0;
    double fragmentation_ratio = 0.0;
};

/**
 * Defragmentation summary returned by `FileSystem::defragment()`.
 */
struct DefragStats {
    uint64_t files_processed = 0;
    uint64_t bytes_moved = 0;
    uint64_t blocks_freed = 0;
    double frag_before = 0.0;
    double frag_after = 0.0;
};

/**
 * Strategy used by a resize or vacuum operation.
 */
enum class ResizeMethod : uint8_t {
    NoOp = OIFS_RESIZE_METHOD_NO_OP,
    InPlace = OIFS_RESIZE_METHOD_IN_PLACE,
    AtomicRelayout = OIFS_RESIZE_METHOD_ATOMIC_RELAYOUT,
};

/**
 * Policy controlling automatic container growth on block or inode exhaustion.
 */
struct AutoGrowConfig {
    bool enabled = false;
    double growth_factor = 1.5;
    uint64_t min_increment_bytes = 4ULL * 1024 * 1024;
    uint64_t max_size_bytes = 0;
    bool allow_relayout = true;

    static constexpr AutoGrowConfig disabled() noexcept {
        return AutoGrowConfig{false, 1.5, 4ULL * 1024 * 1024, 0, true};
    }

    static constexpr AutoGrowConfig create_enabled() noexcept {
        return AutoGrowConfig{true, 1.5, 4ULL * 1024 * 1024, 0, true};
    }

    constexpr AutoGrowConfig& with_growth_factor(double factor) noexcept {
        growth_factor = factor;
        return *this;
    }

    constexpr AutoGrowConfig& with_min_increment_bytes(uint64_t bytes) noexcept {
        min_increment_bytes = bytes;
        return *this;
    }

    constexpr AutoGrowConfig& with_max_size_bytes(uint64_t max_bytes) noexcept {
        max_size_bytes = max_bytes;
        return *this;
    }

    constexpr AutoGrowConfig& with_allow_relayout(bool allow) noexcept {
        allow_relayout = allow;
        return *this;
    }

    constexpr OIFSAutoGrowConfig to_c() const noexcept {
        return OIFSAutoGrowConfig{
            static_cast<uint8_t>(enabled ? 1 : 0),
            static_cast<uint8_t>(allow_relayout ? 1 : 0),
            {0, 0, 0, 0, 0, 0},
            growth_factor,
            min_increment_bytes,
            max_size_bytes,
        };
    }

    static constexpr AutoGrowConfig from_c(const OIFSAutoGrowConfig& raw) noexcept {
        return AutoGrowConfig{
            raw.enabled != 0,
            raw.growth_factor,
            raw.min_increment_bytes,
            raw.max_size_bytes,
            raw.allow_relayout != 0,
        };
    }
};

/**
 * Summary returned by `FileSystem::grow()` and `FileSystem::resize()`.
 */
struct ResizeStats {
    uint64_t bytes_before = 0;
    uint64_t bytes_after = 0;
    uint64_t blocks_before = 0;
    uint64_t blocks_after = 0;
    uint64_t inodes_before = 0;
    uint64_t inodes_after = 0;
    ResizeMethod method = ResizeMethod::NoOp;
};

/**
 * Options controlling `FileSystem::vacuum()`.
 */
struct VacuumOptions {
    uint64_t min_headroom_bytes = 1024ULL * 1024;
    uint8_t headroom_percent = 15;
    uint64_t min_image_bytes = 0;
    bool allow_relayout = true;

    constexpr VacuumOptions& with_min_headroom_bytes(uint64_t bytes) noexcept {
        min_headroom_bytes = bytes;
        return *this;
    }

    constexpr VacuumOptions& with_headroom_percent(uint8_t pct) noexcept {
        headroom_percent = pct > 100 ? static_cast<uint8_t>(100) : pct;
        return *this;
    }

    constexpr VacuumOptions& with_min_image_bytes(uint64_t bytes) noexcept {
        min_image_bytes = bytes;
        return *this;
    }

    constexpr VacuumOptions& with_allow_relayout(bool allow) noexcept {
        allow_relayout = allow;
        return *this;
    }

    constexpr OIFSVacuumOptions to_c() const noexcept {
        return OIFSVacuumOptions{
            min_headroom_bytes,
            min_image_bytes,
            headroom_percent,
            static_cast<uint8_t>(allow_relayout ? 1 : 0),
            {0, 0, 0, 0, 0, 0},
        };
    }
};

/**
 * Summary returned by `FileSystem::vacuum()`.
 */
struct VacuumStats {
    uint64_t bytes_before = 0;
    uint64_t bytes_after = 0;
    uint64_t bytes_reclaimed = 0;
    uint64_t blocks_before = 0;
    uint64_t blocks_after = 0;
    uint64_t blocks_relocated = 0;
    ResizeMethod method = ResizeMethod::NoOp;
};

/**
 * Summary returned by `WriteBatch::commit()`.
 */
struct BatchCommitResult {
    std::vector<uint64_t> created_inodes{};
    uint64_t ops_executed = 0;
    uint64_t wal_transactions = 0;
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

    template <typename U,
              typename = std::enable_if_t<std::is_const_v<T> &&
                                          std::is_same_v<std::remove_const_t<T>, U> &&
                                          !std::is_const_v<U>>>
    constexpr Span(const Span<U>& other) noexcept
        : data_(other.data()), size_(other.size()) {}

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
 * Converts a typed contiguous span or vector of trivially copyable elements into a `ByteView`.
 */
template <typename T>
inline ByteView as_bytes(Span<const T> data) noexcept {
    using ValueT = std::remove_cv_t<T>;
    static_assert(std::is_trivially_copyable_v<ValueT>,
                  "oifs::as_bytes requires a trivially copyable element type");
    return ByteView(reinterpret_cast<const uint8_t*>(data.data()), data.size_bytes());
}

template <typename T, typename = std::enable_if_t<!std::is_const_v<T>>>
inline ByteView as_bytes(Span<T> data) noexcept {
    return oifs::as_bytes(Span<const T>(data.data(), data.size()));
}

template <typename T, typename Alloc>
inline ByteView as_bytes(const std::vector<T, Alloc>& vec) noexcept {
    using ValueT = std::remove_cv_t<T>;
    static_assert(std::is_trivially_copyable_v<ValueT>,
                  "oifs::as_bytes requires a trivially copyable element type");
    return ByteView(reinterpret_cast<const uint8_t*>(vec.data()), vec.size() * sizeof(ValueT));
}

#if __cplusplus >= 202002L && defined(__cpp_lib_span)
template <typename T, std::size_t Extent>
inline ByteView as_bytes(std::span<const T, Extent> data) noexcept {
    using ValueT = std::remove_cv_t<T>;
    static_assert(std::is_trivially_copyable_v<ValueT>,
                  "oifs::as_bytes requires a trivially copyable element type");
    return ByteView(reinterpret_cast<const uint8_t*>(data.data()), data.size_bytes());
}

template <typename T, std::size_t Extent, typename = std::enable_if_t<!std::is_const_v<T>>>
inline ByteView as_bytes(std::span<T, Extent> data) noexcept {
    return oifs::as_bytes(std::span<const T, Extent>(data.data(), data.size()));
}
#endif

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
        data_ = nullptr;
        size_ = 0;
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
        std::string c_path = detail::to_c_string(path);
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

    explicit AsyncFuture(OIFSAsyncFuture* fut)
        : fut_(fut, [](OIFSAsyncFuture* p) noexcept {
              if (p != nullptr) {
                  oifs_async_free(p);
              }
          }) {
        if (fut_ == nullptr) {
            throw Error("Failed to construct AsyncFuture from null OIFSAsyncFuture");
        }
    }

    ~AsyncFuture() noexcept { reset(); }

    AsyncFuture(const AsyncFuture&) = delete;
    AsyncFuture& operator=(const AsyncFuture&) = delete;

    AsyncFuture(AsyncFuture&&) noexcept = default;
    AsyncFuture& operator=(AsyncFuture&&) noexcept = default;

    explicit operator bool() const noexcept { return fut_ != nullptr; }
    OIFSAsyncFuture* raw() const noexcept { return fut_.get(); }

    void reset() noexcept { fut_.reset(); }

    /**
     * Non-blocking readiness check. Returns true if the operation has completed.
     */
    bool is_ready() const noexcept {
        return fut_ != nullptr && oifs_async_poll(fut_.get()) == 1;
    }

    /**
     * Blocks until the operation completes and returns the result (`bytes_read`,
     * created `inode_id`, or `0` for flush). Throws `oifs::Error` on failure.
     */
    int64_t wait() {
        if (fut_ == nullptr) {
            throw Error("AsyncFuture::wait called on empty future");
        }
        int64_t res = oifs_async_wait(fut_.get());
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
        if (oifs_async_last_error(fut_.get(), buf, sizeof(buf)) == 0) {
            return std::string(buf);
        }
        return "Unknown async error";
    }

#if defined(OIFS_HAS_CPP20_COROUTINE)
    /**
     * C++20 coroutine awaiter for `AsyncFuture`.
     *
     * - Inline Fast-Path hits (`await_ready() == true`) resume immediately with zero
     *   suspension or thread hop.
     * - Pending futures suspend and await `oifs_async_wait` on an owned waiter `std::thread`.
     *   If the coroutine is cancelled/destroyed early from outside the waiter thread, `~Awaiter()`
     *   marks `cancelled = true` and synchronously joins `worker`, holding the drop barrier
     *   until the underlying Rust task stops touching any caller buffer (preventing UAF).
     */
    class Awaiter {
    public:
        explicit Awaiter(AsyncFuture& fut)
            : fut_(fut), state_(std::make_shared<SharedState>()) {
            state_->fut = fut.fut_;
        }

        ~Awaiter() {
            if (!state_ || !suspended_) {
                return;
            }
            std::thread to_join;
            {
                std::lock_guard<std::mutex> lock(state_->mtx);
                if (!state_->worker.joinable()) {
                    state_->cancelled = true;
                    return;
                }
                // If `~Awaiter()` is running from inside `h.resume()` on the waiter thread,
                // self-joining would deadlock; detach the finishing thread instead.
                if (state_->resuming && state_->worker_tid == std::this_thread::get_id()) {
                    state_->worker.detach();
                    return;
                }
                state_->cancelled = true;
                to_join = std::move(state_->worker);
            }
            if (to_join.joinable()) {
                to_join.join();
            }
        }

        Awaiter(const Awaiter&) = delete;
        Awaiter& operator=(const Awaiter&) = delete;

        bool await_ready() const noexcept { return fut_.is_ready(); }

        void await_suspend(std::coroutine_handle<> h) {
            suspended_ = true;
            auto st = state_;
            std::lock_guard<std::mutex> lock(st->mtx);
            st->worker = std::thread([st, h]() mutable {
                int64_t res = (st->fut != nullptr) ? oifs_async_wait(st->fut.get()) : -1;
                bool should_resume = false;
                {
                    std::lock_guard<std::mutex> lk(st->mtx);
                    st->fut.reset();
                    if (!st->cancelled) {
                        st->result = res;
                        st->resuming = true;
                        st->worker_tid = std::this_thread::get_id();
                        should_resume = true;
                    }
                }
                if (should_resume) {
                    h.resume();
                }
            });
        }

        int64_t await_resume() {
            if (!suspended_) {
                return fut_.wait();
            }
            int64_t res = 0;
            {
                std::lock_guard<std::mutex> lock(state_->mtx);
                res = state_->result;
            }
            if (res < 0) {
                throw Error(fut_.last_error());
            }
            return res;
        }

    private:
        struct SharedState {
            std::mutex mtx{};
            bool cancelled = false;
            bool resuming = false;
            int64_t result = 0;
            std::thread::id worker_tid{};
            std::thread worker{};
            std::shared_ptr<OIFSAsyncFuture> fut{};
        };

        AsyncFuture& fut_;
        std::shared_ptr<SharedState> state_;
        bool suspended_ = false;
    };

    Awaiter operator co_await() noexcept { return Awaiter{*this}; }
#endif

private:
    std::shared_ptr<OIFSAsyncFuture> fut_{};
};

/**
 * Move-only RAII builder for explicit multi-operation atomic write batches.
 */
class WriteBatch {
public:
    WriteBatch() noexcept = default;

    explicit WriteBatch(OIFSWriteBatch* batch, std::weak_ptr<OIFSHandle*> owner_handle = {})
        : batch_(batch), owner_handle_(std::move(owner_handle)) {
        if (batch_ == nullptr) {
            throw Error("Failed to construct WriteBatch from null OIFSWriteBatch");
        }
    }

    ~WriteBatch() noexcept { reset(); }

    WriteBatch(const WriteBatch&) = delete;
    WriteBatch& operator=(const WriteBatch&) = delete;

    WriteBatch(WriteBatch&& other) noexcept
        : batch_(other.batch_),
          owner_handle_(std::move(other.owner_handle_)),
          expected_created_count_(other.expected_created_count_) {
        other.batch_ = nullptr;
        other.expected_created_count_ = 0;
    }

    WriteBatch& operator=(WriteBatch&& other) noexcept {
        if (this != &other) {
            reset();
            batch_ = other.batch_;
            owner_handle_ = std::move(other.owner_handle_);
            expected_created_count_ = other.expected_created_count_;
            other.batch_ = nullptr;
            other.expected_created_count_ = 0;
        }
        return *this;
    }

    explicit operator bool() const noexcept { return batch_ != nullptr; }
    OIFSWriteBatch* raw() const noexcept { return batch_; }

    void reset() noexcept {
        if (batch_ != nullptr) {
            oifs_batch_free(batch_);
            batch_ = nullptr;
        }
        expected_created_count_ = 0;
    }

    std::size_t size() const noexcept {
        return batch_ != nullptr ? oifs_batch_len(batch_) : 0;
    }

    bool empty() const noexcept { return size() == 0; }

    WriteBatch& create_file_with_data(std::string_view path,
                                      ByteView data,
                                      uint32_t policy_flags = OIFS_WRITE_POLICY_DEFAULT,
                                      int32_t zstd_level = 0,
                                      FilterConfig filter = FilterConfig::none()) {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
        OIFSFilterConfig raw_filter = filter.to_c();
        if (oifs_batch_create_file_with_data(batch_,
                                             c_path.c_str(),
                                             data.data(),
                                             data.size(),
                                             policy_flags,
                                             zstd_level,
                                             &raw_filter) != 0) {
            throw Error(fetch_error("oifs_batch_create_file_with_data failed"));
        }
        ++expected_created_count_;
        return *this;
    }

    WriteBatch& create_file_with_data(std::string_view path,
                                      ByteView data,
                                      WritePolicy policy,
                                      int32_t zstd_level = 0,
                                      FilterConfig filter = FilterConfig::none()) {
        return create_file_with_data(
            path, data, static_cast<uint32_t>(policy), zstd_level, filter);
    }

    WriteBatch& mkdir(std::string_view path) {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
        if (oifs_batch_mkdir(batch_, c_path.c_str()) != 0) {
            throw Error(fetch_error("oifs_batch_mkdir failed"));
        }
        ++expected_created_count_;
        return *this;
    }

    WriteBatch& delete_file(std::string_view path) {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
        if (oifs_batch_delete(batch_, c_path.c_str()) != 0) {
            throw Error(fetch_error("oifs_batch_delete failed"));
        }
        return *this;
    }

    WriteBatch& rename(std::string_view old_path, std::string_view new_path) {
        ensure_open();
        std::string c_old = detail::to_c_string(old_path);
        std::string c_new = detail::to_c_string(new_path);
        if (oifs_batch_rename(batch_, c_old.c_str(), c_new.c_str()) != 0) {
            throw Error(fetch_error("oifs_batch_rename failed"));
        }
        return *this;
    }

    BatchCommitResult commit() {
        ensure_open();
        std::vector<uint64_t> created(expected_created_count_);
        OIFSBatchCommitResult raw{};
        if (oifs_batch_commit(batch_,
                              &raw,
                              created.empty() ? nullptr : created.data(),
                              created.size()) != 0) {
            throw Error(fetch_error("oifs_batch_commit failed"));
        }
        created.resize(static_cast<std::size_t>(raw.created_count));
        BatchCommitResult out;
        out.created_inodes = std::move(created);
        out.ops_executed = raw.ops_executed;
        out.wal_transactions = raw.wal_transactions;
        return out;
    }

private:
    void ensure_open() const {
        if (batch_ == nullptr) {
            throw Error("Operation on closed oifs::WriteBatch");
        }
    }

    std::string fetch_error(const char* fallback) const {
        auto sp = owner_handle_.lock();
        if (sp && *sp != nullptr) {
            char buf[512] = {0};
            if (oifs_last_error(*sp, buf, sizeof(buf)) == 0 && buf[0] != '\0') {
                return std::string(buf);
            }
        }
        return std::string(fallback);
    }

    OIFSWriteBatch* batch_ = nullptr;
    std::weak_ptr<OIFSHandle*> owner_handle_{};
    std::size_t expected_created_count_ = 0;
};

/**
 * Move-only RAII wrapper around `OIFSHandle*`.
 */
class FileSystem {
public:
    FileSystem() noexcept = default;

    explicit FileSystem(OIFSHandle* handle)
        : handle_(handle), handle_token_(std::make_shared<OIFSHandle*>(handle)) {
        if (handle_ == nullptr) {
            throw Error("Failed to open OIFS FileSystem");
        }
    }

    ~FileSystem() noexcept { close(); }

    FileSystem(const FileSystem&) = delete;
    FileSystem& operator=(const FileSystem&) = delete;

    FileSystem(FileSystem&& other) noexcept
        : handle_(other.handle_), handle_token_(std::move(other.handle_token_)) {
        other.handle_ = nullptr;
    }

    FileSystem& operator=(FileSystem&& other) noexcept {
        if (this != &other) {
            close();
            handle_ = other.handle_;
            handle_token_ = std::move(other.handle_token_);
            other.handle_ = nullptr;
        }
        return *this;
    }

    explicit operator bool() const noexcept { return handle_ != nullptr; }
    OIFSHandle* raw() const noexcept { return handle_; }

    void close() noexcept {
        if (handle_token_) {
            *handle_token_ = nullptr;
            handle_token_.reset();
        }
        if (handle_ != nullptr) {
            oifs_close(handle_);
            handle_ = nullptr;
        }
    }

    static FileSystem open(std::string_view path, uint64_t size = 0) {
        std::string c_path = detail::to_c_string(path);
        OIFSHandle* h = oifs_open(c_path.c_str(), size);
        if (h == nullptr) {
            throw Error("oifs_open failed for path: " + c_path);
        }
        return FileSystem(h);
    }

    static FileSystem open_with_password(std::string_view path,
                                         uint64_t size,
                                         std::string_view password) {
        std::string c_path = detail::to_c_string(path);
        std::string c_pwd = detail::to_c_string(password);
        OIFSHandle* h = oifs_open_with_password(
            c_path.c_str(), size, c_pwd.empty() ? nullptr : c_pwd.c_str());
        if (h == nullptr) {
            throw Error("oifs_open_with_password failed for path: " + c_path);
        }
        return FileSystem(h);
    }

    static FileSystem open_with_options(std::string_view path, const OpenOptions& opts) {
        std::string c_path = detail::to_c_string(path);
        std::string c_pwd = detail::to_c_string(opts.password);
        const char* pwd_ptr = c_pwd.empty() ? nullptr : c_pwd.c_str();
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
        std::string c_path = detail::to_c_string(path);
        OIFSHandle* h = oifs_open_with_options(
            c_path.c_str(), size, password, storage_mode, journaled ? 1 : 0);
        if (h == nullptr) {
            throw Error("oifs_open_with_options failed for path: " + c_path);
        }
        return FileSystem(h);
    }

    static FileSystem get_or_open(std::string_view path, uint64_t size = 0) {
        std::string c_path = detail::to_c_string(path);
        OIFSHandle* h = oifs_get_or_open(c_path.c_str(), size);
        if (h == nullptr) {
            throw Error("oifs_get_or_open failed for path: " + c_path);
        }
        return FileSystem(h);
    }

    static FileSystem get_or_open_with_password(std::string_view path,
                                                uint64_t size,
                                                std::string_view password) {
        std::string c_path = detail::to_c_string(path);
        std::string c_pwd = detail::to_c_string(password);
        OIFSHandle* h = oifs_get_or_open_with_password(
            c_path.c_str(), size, c_pwd.empty() ? nullptr : c_pwd.c_str());
        if (h == nullptr) {
            throw Error("oifs_get_or_open_with_password failed for path: " + c_path);
        }
        return FileSystem(h);
    }

    /**
     * Opens an existing OIFS image in read-only mode (shared F_RDLCK lock, zero writes).
     * Works on 0444 images and allows concurrent read-only mounts across processes.
     */
    static FileSystem open_read_only(std::string_view path,
                                     std::string_view password = {},
                                     StorageMode mode = StorageMode::Mmap) {
        std::string c_path = detail::to_c_string(path);
        std::string c_pwd = detail::to_c_string(password);
        const char* pwd_ptr = c_pwd.empty() ? nullptr : c_pwd.c_str();
        OIFSHandle* h = oifs_open_read_only(
            c_path.c_str(), pwd_ptr, static_cast<uint8_t>(mode));
        if (h == nullptr) {
            throw Error("oifs_open_read_only failed for path: " + c_path);
        }
        return FileSystem(h);
    }

    /**
     * Clones a whole OIFS container image using the fastest host OS CoW primitive.
     */
    static CloneMethod clone_image(std::string_view src, std::string_view dst) {
        std::string c_src = detail::to_c_string(src);
        std::string c_dst = detail::to_c_string(dst);
        uint8_t method = OIFS_CLONE_METHOD_STANDARD_COPY;
        if (oifs_clone_image(c_src.c_str(), c_dst.c_str(), &method) != 0) {
            throw Error("oifs_clone_image failed from '" + c_src + "' to '" + c_dst + "'");
        }
        return static_cast<CloneMethod>(method);
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

    IoBackend set_io_backend(IoBackend backend) {
        ensure_open();
        int32_t eff = oifs_set_io_backend(handle_, static_cast<uint8_t>(backend));
        if (eff < 0) {
            throw Error(last_error());
        }
        return static_cast<IoBackend>(eff);
    }

    IoBackend get_io_backend() const {
        ensure_open();
        int32_t b = oifs_get_io_backend(handle_);
        if (b < 0) {
            throw Error(last_error());
        }
        return static_cast<IoBackend>(b);
    }

    void mkdir(std::string_view path) {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
        if (oifs_mkdir(handle_, c_path.c_str()) != 0) {
            throw Error(last_error());
        }
    }

    void delete_file(std::string_view path) {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
        if (oifs_delete_file(handle_, c_path.c_str()) != 0) {
            throw Error(last_error());
        }
    }

    void rename(std::string_view old_path, std::string_view new_path) {
        ensure_open();
        std::string c_old = detail::to_c_string(old_path);
        std::string c_new = detail::to_c_string(new_path);
        if (oifs_rename(handle_, c_old.c_str(), c_new.c_str()) != 0) {
            throw Error(last_error());
        }
    }

    void truncate_file(std::string_view path, uint64_t new_size) {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
        if (oifs_truncate_file(handle_, c_path.c_str(), new_size) != 0) {
            throw Error(last_error());
        }
    }

    std::vector<DirEntry> ls() const {
        ensure_open();
        std::vector<DirEntry> entries;
        if (oifs_ls(handle_, &FileSystem::ls_callback, &entries) != 0) {
            throw Error(last_error());
        }
        return entries;
    }

    std::vector<DirEntry> ls(std::string_view path) const {
        ensure_open();
        std::vector<DirEntry> entries;
        std::string c_path = detail::to_c_string(path);
        if (oifs_ls_dir(handle_, c_path.c_str(), &FileSystem::ls_callback, &entries) != 0) {
            throw Error(last_error());
        }
        return entries;
    }

    FileStat stat(std::string_view path) const {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
        OIFSFileStat raw{};
        if (oifs_stat(handle_, c_path.c_str(), &raw) != 0) {
            throw Error(last_error());
        }
        FileStat out;
        out.inode_id = raw.inode_id;
        out.size = raw.size;
        out.compressed_size = raw.compressed_size;
        out.created_at = raw.created_at;
        out.modified_at = raw.modified_at;
        out.is_dir = (raw.is_dir != 0);
        out.encrypted = (raw.encrypted != 0);
        out.seekable_64k = (raw.seekable_64k != 0);
        out.filter_typesize = raw.filter_typesize;
        out.filter_delta = (raw.filter_delta != 0);
        out.filter_shuffle = (raw.filter_shuffle != 0);
        out.filter_bitshuffle = (raw.filter_bitshuffle != 0);
        return out;
    }

    bool exists(std::string_view path) const {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
        int32_t rc = oifs_exists(handle_, c_path.c_str());
        if (rc < 0) {
            throw Error(last_error());
        }
        return rc != 0;
    }

    uint64_t resolve_path(std::string_view path) const {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
        int64_t id = oifs_resolve_path(handle_, c_path.c_str());
        if (id < 0) {
            throw Error(last_error());
        }
        return static_cast<uint64_t>(id);
    }

    static constexpr std::size_t MAX_READ_FILE_BYTES =
        (sizeof(std::size_t) >= 8) ? static_cast<std::size_t>(2ULL * 1024 * 1024 * 1024) : SIZE_MAX;

    std::vector<uint8_t> read_file(std::string_view path,
                                   std::size_t max_bytes = MAX_READ_FILE_BYTES) const {
        ensure_open();
        FileStat st = stat(path);
        if (st.is_dir) {
            throw Error("Cannot read_file on a directory: " + std::string(path));
        }
        if (st.size == 0) {
            return {};
        }
        if (st.size > static_cast<uint64_t>(max_bytes) ||
            st.size > static_cast<uint64_t>(SIZE_MAX)) {
            throw Error("File size exceeds read_file limit");
        }
        std::vector<uint8_t> buf(static_cast<std::size_t>(st.size));
        std::size_t got = read_at(path, 0, MutableByteSpan(buf));
        buf.resize(got);
        return buf;
    }

    uint64_t create_file_with_data(std::string_view path,
                                   ByteView data,
                                   uint32_t policy_flags = OIFS_WRITE_POLICY_DEFAULT,
                                   int32_t zstd_level = 0) {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
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

    uint64_t create_file_with_data(std::string_view path,
                                   ByteView data,
                                   uint32_t policy_flags,
                                   int32_t zstd_level,
                                   FilterConfig filter) {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
        OIFSFilterConfig raw_filter = filter.to_c();
        int64_t id = oifs_create_file_with_data_filtered(handle_,
                                                         c_path.c_str(),
                                                         data.data(),
                                                         data.size(),
                                                         policy_flags,
                                                         zstd_level,
                                                         &raw_filter);
        if (id < 0) {
            throw Error(last_error());
        }
        return static_cast<uint64_t>(id);
    }

    uint64_t create_file_with_data(std::string_view path,
                                   ByteView data,
                                   WritePolicy policy,
                                   int32_t zstd_level,
                                   FilterConfig filter) {
        return create_file_with_data(
            path, data, static_cast<uint32_t>(policy), zstd_level, filter);
    }

    template <typename T>
    uint64_t create_file_with_span(std::string_view path,
                                   Span<const T> data,
                                   WritePolicy policy = WritePolicy::Default,
                                   int32_t zstd_level = 0,
                                   FilterConfig filter = FilterConfig::none()) {
        return create_file_with_data(path, oifs::as_bytes(data), policy, zstd_level, filter);
    }

    template <typename T, typename = std::enable_if_t<!std::is_const_v<T>>>
    uint64_t create_file_with_span(std::string_view path,
                                   Span<T> data,
                                   WritePolicy policy = WritePolicy::Default,
                                   int32_t zstd_level = 0,
                                   FilterConfig filter = FilterConfig::none()) {
        return create_file_with_data(path, oifs::as_bytes(data), policy, zstd_level, filter);
    }

    template <typename T, typename Alloc>
    uint64_t create_file_with_span(std::string_view path,
                                   const std::vector<T, Alloc>& data,
                                   WritePolicy policy = WritePolicy::Default,
                                   int32_t zstd_level = 0,
                                   FilterConfig filter = FilterConfig::none()) {
        return create_file_with_data(path, oifs::as_bytes(data), policy, zstd_level, filter);
    }

#if __cplusplus >= 202002L && defined(__cpp_lib_span)
    template <typename T, std::size_t Extent>
    uint64_t create_file_with_span(std::string_view path,
                                   std::span<const T, Extent> data,
                                   WritePolicy policy = WritePolicy::Default,
                                   int32_t zstd_level = 0,
                                   FilterConfig filter = FilterConfig::none()) {
        return create_file_with_data(path, oifs::as_bytes(data), policy, zstd_level, filter);
    }

    template <typename T, std::size_t Extent, typename = std::enable_if_t<!std::is_const_v<T>>>
    uint64_t create_file_with_span(std::string_view path,
                                   std::span<T, Extent> data,
                                   WritePolicy policy = WritePolicy::Default,
                                   int32_t zstd_level = 0,
                                   FilterConfig filter = FilterConfig::none()) {
        return create_file_with_data(path, oifs::as_bytes(data), policy, zstd_level, filter);
    }
#endif

    void write_file(std::string_view path, ByteView data) {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
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
        std::string c_path = detail::to_c_string(path);
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

    void write_file(std::string_view path,
                    uint64_t offset,
                    ByteView data,
                    uint32_t policy_flags,
                    int32_t zstd_level,
                    FilterConfig filter) {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
        OIFSFilterConfig raw_filter = filter.to_c();
        if (oifs_write_file_with_filters(handle_,
                                         c_path.c_str(),
                                         offset,
                                         data.data(),
                                         data.size(),
                                         policy_flags,
                                         zstd_level,
                                         &raw_filter) != 0) {
            throw Error(last_error());
        }
    }

    void write_file(std::string_view path,
                    uint64_t offset,
                    ByteView data,
                    WritePolicy policy,
                    int32_t zstd_level,
                    FilterConfig filter) {
        write_file(path, offset, data, static_cast<uint32_t>(policy), zstd_level, filter);
    }

    template <typename T>
    void write_span(std::string_view path,
                    uint64_t offset,
                    Span<const T> data,
                    WritePolicy policy = WritePolicy::Default,
                    int32_t zstd_level = 0,
                    FilterConfig filter = FilterConfig::none()) {
        write_file(path, offset, oifs::as_bytes(data), policy, zstd_level, filter);
    }

    template <typename T, typename = std::enable_if_t<!std::is_const_v<T>>>
    void write_span(std::string_view path,
                    uint64_t offset,
                    Span<T> data,
                    WritePolicy policy = WritePolicy::Default,
                    int32_t zstd_level = 0,
                    FilterConfig filter = FilterConfig::none()) {
        write_file(path, offset, oifs::as_bytes(data), policy, zstd_level, filter);
    }

    template <typename T, typename Alloc>
    void write_span(std::string_view path,
                    uint64_t offset,
                    const std::vector<T, Alloc>& data,
                    WritePolicy policy = WritePolicy::Default,
                    int32_t zstd_level = 0,
                    FilterConfig filter = FilterConfig::none()) {
        write_file(path, offset, oifs::as_bytes(data), policy, zstd_level, filter);
    }

#if __cplusplus >= 202002L && defined(__cpp_lib_span)
    template <typename T, std::size_t Extent>
    void write_span(std::string_view path,
                    uint64_t offset,
                    std::span<const T, Extent> data,
                    WritePolicy policy = WritePolicy::Default,
                    int32_t zstd_level = 0,
                    FilterConfig filter = FilterConfig::none()) {
        write_file(path, offset, oifs::as_bytes(data), policy, zstd_level, filter);
    }

    template <typename T, std::size_t Extent, typename = std::enable_if_t<!std::is_const_v<T>>>
    void write_span(std::string_view path,
                    uint64_t offset,
                    std::span<T, Extent> data,
                    WritePolicy policy = WritePolicy::Default,
                    int32_t zstd_level = 0,
                    FilterConfig filter = FilterConfig::none()) {
        write_file(path, offset, oifs::as_bytes(data), policy, zstd_level, filter);
    }
#endif

    TransferStats transfer_file_to(std::string_view src_path,
                                   FileSystem& dst,
                                   std::string_view dst_path,
                                   TransferOptions opts = {}) {
        ensure_open();
        dst.ensure_open();
        std::string c_src = detail::to_c_string(src_path);
        std::string c_dst = detail::to_c_string(dst_path);
        uint32_t flags = 0;
        if (opts.move_file) {
            flags |= OIFS_TRANSFER_FLAG_MOVE;
        }
        if (opts.overwrite) {
            flags |= OIFS_TRANSFER_FLAG_OVERWRITE;
        }
        OIFSTransferStats raw{};
        if (oifs_transfer_file_to(
                handle_, c_src.c_str(), dst.handle_, c_dst.c_str(), flags, &raw) != 0) {
            throw Error(last_error());
        }
        TransferStats out;
        out.src_inode = raw.src_inode;
        out.dst_inode = raw.dst_inode;
        out.logical_bytes = raw.logical_bytes;
        out.physical_bytes = raw.physical_bytes;
        out.extents_transferred = raw.extents_transferred;
        out.method = static_cast<TransferMethod>(raw.method);
        return out;
    }

    SpliceStats splice_range_to(uint64_t src_inode,
                                uint64_t src_off,
                                FileSystem& dst,
                                uint64_t dst_inode,
                                uint64_t dst_off,
                                uint64_t len,
                                SpliceOptions opts = {}) {
        ensure_open();
        dst.ensure_open();
        OIFSSpliceStats raw{};
        if (oifs_splice_range_to(handle_,
                                 src_inode,
                                 src_off,
                                 dst.handle_,
                                 dst_inode,
                                 dst_off,
                                 len,
                                 static_cast<uint8_t>(opts.trim_mode),
                                 &raw) != 0) {
            throw Error(last_error());
        }
        SpliceStats out;
        out.src_inode = raw.src_inode;
        out.dst_inode = raw.dst_inode;
        out.bytes_spliced = raw.bytes_spliced;
        out.logical_rmw_bytes = raw.logical_rmw_bytes;
        out.body_fast_path_bytes = raw.body_fast_path_bytes;
        out.physical_bytes = raw.physical_bytes;
        out.extents_transferred = raw.extents_transferred;
        out.src_blocks_freed = raw.src_blocks_freed;
        out.method = static_cast<TransferMethod>(raw.method);
        return out;
    }

    FsckReport verify_integrity() const {
        ensure_open();
        OIFSFsckReport raw{};
        if (oifs_verify_integrity(handle_, &raw) != 0) {
            throw Error(last_error());
        }
        FsckReport out;
        out.is_clean = (raw.is_clean != 0);
        out.orphan_inodes = raw.orphan_inodes;
        out.leaked_blocks = raw.leaked_blocks;
        out.missing_blocks = raw.missing_blocks;
        out.cross_linked_blocks = raw.cross_linked_blocks;
        return out;
    }

    FragmentationStats analyze_fragmentation() const {
        ensure_open();
        OIFSFragmentationStats raw{};
        if (oifs_analyze_fragmentation(handle_, &raw) != 0) {
            throw Error(last_error());
        }
        FragmentationStats out;
        out.total_blocks = raw.total_blocks;
        out.used_blocks = raw.used_blocks;
        out.free_blocks = raw.free_blocks;
        out.free_runs = raw.free_runs;
        out.largest_free_run = raw.largest_free_run;
        out.avg_gap_size = raw.avg_gap_size;
        out.fragmentation_ratio = raw.fragmentation_ratio;
        return out;
    }

    DefragStats defragment() {
        ensure_open();
        OIFSDefragStats raw{};
        if (oifs_defragment(handle_, &raw) != 0) {
            throw Error(last_error());
        }
        DefragStats out;
        out.files_processed = raw.files_processed;
        out.bytes_moved = raw.bytes_moved;
        out.blocks_freed = raw.blocks_freed;
        out.frag_before = raw.frag_before;
        out.frag_after = raw.frag_after;
        return out;
    }

    uint64_t image_size_bytes() const {
        ensure_open();
        int64_t sz = oifs_image_size_bytes(handle_);
        if (sz < 0) {
            throw Error(last_error());
        }
        return static_cast<uint64_t>(sz);
    }

    void set_auto_grow(const AutoGrowConfig& config) {
        ensure_open();
        OIFSAutoGrowConfig raw = config.to_c();
        if (oifs_set_auto_grow(handle_, &raw) != 0) {
            throw Error(last_error());
        }
    }

    AutoGrowConfig auto_grow_config() const {
        ensure_open();
        OIFSAutoGrowConfig raw{};
        if (oifs_get_auto_grow(handle_, &raw) != 0) {
            throw Error(last_error());
        }
        return AutoGrowConfig::from_c(raw);
    }

    ResizeStats grow(uint64_t new_size_bytes) {
        ensure_open();
        OIFSResizeStats raw{};
        if (oifs_grow(handle_, new_size_bytes, &raw) != 0) {
            throw Error(last_error());
        }
        return to_resize_stats(raw);
    }

    ResizeStats resize(uint64_t new_size_bytes) {
        ensure_open();
        OIFSResizeStats raw{};
        if (oifs_resize(handle_, new_size_bytes, &raw) != 0) {
            throw Error(last_error());
        }
        return to_resize_stats(raw);
    }

    VacuumStats vacuum(const VacuumOptions& options = {}) {
        ensure_open();
        OIFSVacuumOptions raw_opts = options.to_c();
        OIFSVacuumStats raw{};
        if (oifs_vacuum(handle_, &raw_opts, &raw) != 0) {
            throw Error(last_error());
        }
        VacuumStats out;
        out.bytes_before = raw.bytes_before;
        out.bytes_after = raw.bytes_after;
        out.bytes_reclaimed = raw.bytes_reclaimed;
        out.blocks_before = raw.blocks_before;
        out.blocks_after = raw.blocks_after;
        out.blocks_relocated = raw.blocks_relocated;
        out.method = static_cast<ResizeMethod>(raw.method);
        return out;
    }

    WriteBatch begin_write_batch() {
        ensure_open();
        OIFSWriteBatch* wb = oifs_batch_begin(handle_);
        if (wb == nullptr) {
            throw Error(last_error());
        }
        return WriteBatch(wb, handle_token_);
    }

    uint32_t register_zstd_dict(ByteView dict_bytes) {
        ensure_open();
        int64_t id =
            oifs_register_zstd_dict(handle_, dict_bytes.data(), dict_bytes.size(), 0);
        if (id < 0) {
            throw Error(last_error());
        }
        return static_cast<uint32_t>(id);
    }

    uint32_t register_and_use_zstd_dict(ByteView dict_bytes) {
        ensure_open();
        int64_t id =
            oifs_register_zstd_dict(handle_, dict_bytes.data(), dict_bytes.size(), 1);
        if (id < 0) {
            throw Error(last_error());
        }
        return static_cast<uint32_t>(id);
    }

    void set_default_zstd_dict(std::optional<uint32_t> dict_id) {
        ensure_open();
        if (dict_id.has_value() && *dict_id == 0) {
            throw Error("Zstd dict_id must be non-zero");
        }
        uint32_t raw_id = dict_id.value_or(0);
        if (oifs_set_default_zstd_dict(handle_, raw_id) != 0) {
            throw Error(last_error());
        }
    }

    std::optional<uint32_t> default_zstd_dict_id() const {
        ensure_open();
        int64_t id = oifs_default_zstd_dict_id(handle_);
        if (id < 0) {
            throw Error(last_error());
        }
        if (id == 0) {
            return std::nullopt;
        }
        return static_cast<uint32_t>(id);
    }

    bool unregister_zstd_dict(uint32_t dict_id) {
        ensure_open();
        int32_t rc = oifs_unregister_zstd_dict(handle_, dict_id);
        if (rc < 0) {
            throw Error(last_error());
        }
        return rc != 0;
    }

    std::vector<uint32_t> registered_zstd_dict_ids() const {
        ensure_open();
        int64_t count = oifs_registered_zstd_dict_ids(handle_, nullptr, 0);
        if (count < 0) {
            throw Error(last_error());
        }
        if (count == 0) {
            return {};
        }
        std::vector<uint32_t> ids(static_cast<std::size_t>(count));
        for (;;) {
            int64_t total = oifs_registered_zstd_dict_ids(handle_, ids.data(), ids.size());
            if (total < 0) {
                throw Error(last_error());
            }
            if (total <= static_cast<int64_t>(ids.size())) {
                ids.resize(static_cast<std::size_t>(total));
                return ids;
            }
            ids.resize(static_cast<std::size_t>(total));
        }
    }

    void clear_zstd_dicts() {
        ensure_open();
        if (oifs_clear_zstd_dicts(handle_) != 0) {
            throw Error(last_error());
        }
    }

    static std::vector<uint8_t> train_zstd_dict(const std::vector<ByteView>& samples,
                                                std::size_t max_dict_bytes) {
        if (samples.empty() || max_dict_bytes == 0) {
            throw Error("train_zstd_dict requires non-empty samples and max_dict_bytes");
        }
        std::vector<OIFSByteSlice> raw_samples;
        raw_samples.reserve(samples.size());
        for (const auto& s : samples) {
            raw_samples.push_back(OIFSByteSlice{s.data(), s.size()});
        }
        std::vector<uint8_t> out(max_dict_bytes);
        int64_t written = oifs_train_zstd_dict(
            raw_samples.data(), raw_samples.size(), max_dict_bytes, out.data(), out.size());
        if (written < 0) {
            throw Error("oifs_train_zstd_dict failed");
        }
        out.resize(static_cast<std::size_t>(written));
        return out;
    }

    static std::vector<uint8_t> train_zstd_dict(const std::vector<std::vector<uint8_t>>& samples,
                                                std::size_t max_dict_bytes) {
        std::vector<ByteView> views;
        views.reserve(samples.size());
        for (const auto& s : samples) {
            views.emplace_back(s);
        }
        return train_zstd_dict(views, max_dict_bytes);
    }

    std::vector<uint8_t> train_zstd_dict_from_dir(std::string_view dir_path,
                                                std::size_t max_dict_bytes,
                                                std::size_t max_samples = 1024) const {
        ensure_open();
        if (max_dict_bytes == 0) {
            throw Error("train_zstd_dict_from_dir requires non-zero max_dict_bytes");
        }
        std::string c_dir = detail::to_c_string(dir_path);
        std::vector<uint8_t> out(max_dict_bytes);
        int64_t written = oifs_train_zstd_dict_from_dir(
            handle_, c_dir.c_str(), max_dict_bytes, max_samples, out.data(), out.size());
        if (written < 0) {
            throw Error(last_error());
        }
        out.resize(static_cast<std::size_t>(written));
        return out;
    }

    std::size_t read_at(std::string_view path, uint64_t offset, MutableByteSpan dst) const {
        ensure_open();
        std::string c_path = detail::to_c_string(path);
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
        std::string c_path = detail::to_c_string(path);
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
        std::string c_path = detail::to_c_string(path);
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
        std::string c_path = detail::to_c_string(path);
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

    /**
     * Dispatches a raw FUSE Kernel ABI v7.28 request frame against this filesystem and
     * writes the serialized response frame into `resp_buf`.
     *
     * Returns the response byte length (`>= 16` for request/reply opcodes, `0` for one-way
     * notifications like `FUSE_FORGET`). Throws `oifs::Error` on invalid arguments or
     * insufficient `resp_buf` capacity.
     */
    std::size_t fuse_dispatch_frame(ByteView req_frame, MutableByteSpan resp_buf) const {
        ensure_open();
        int64_t n = oifs_fuse_dispatch_frame(
            handle_, req_frame.data(), req_frame.size(), resp_buf.data(), resp_buf.size());
        if (n < 0) {
            throw Error(last_error());
        }
        return static_cast<std::size_t>(n);
    }

    /**
     * Dispatches a raw FUSE Kernel ABI v7.28 request frame against this filesystem and
     * returns the serialized response frame (`fuse_out_header` + body).
     */
    std::vector<uint8_t> fuse_dispatch_frame(ByteView req_frame) const {
        ensure_open();
        std::size_t cap = 16 + 128 * 1024;
        if (req_frame.size() >= 60) {
            uint32_t opcode = 0;
            uint32_t req_size = 0;
            std::memcpy(&opcode, req_frame.data() + 4, sizeof(opcode));
            std::memcpy(&req_size, req_frame.data() + 56, sizeof(req_size));
            opcode = OIFS_FROM_LE32(opcode);
            req_size = OIFS_FROM_LE32(req_size);
            if (opcode == 15 || opcode == 28) {
                constexpr std::size_t kMaxReply = 16ULL * 1024 * 1024 + 16;
                std::size_t needed = static_cast<std::size_t>(req_size) + 16;
                if (needed > cap) {
                    cap = needed < kMaxReply ? needed : kMaxReply;
                }
            }
        }
        std::vector<uint8_t> out(cap);
        std::size_t n = fuse_dispatch_frame(req_frame, MutableByteSpan(out));
        out.resize(n);
        return out;
    }

private:
    static void ls_callback(const char* name, uint64_t size, uint64_t mtime, void* user_data) {
        auto* out = static_cast<std::vector<DirEntry>*>(user_data);
        try {
            out->push_back(
                DirEntry{name != nullptr ? std::string(name) : std::string{}, size, mtime});
        } catch (...) {
            // Never allow C++ exceptions to unwind across the extern "C" ABI boundary.
        }
    }

    static ResizeStats to_resize_stats(const OIFSResizeStats& raw) noexcept {
        ResizeStats out;
        out.bytes_before = raw.bytes_before;
        out.bytes_after = raw.bytes_after;
        out.blocks_before = raw.blocks_before;
        out.blocks_after = raw.blocks_after;
        out.inodes_before = raw.inodes_before;
        out.inodes_after = raw.inodes_after;
        out.method = static_cast<ResizeMethod>(raw.method);
        return out;
    }

    void ensure_open() const {
        if (handle_ == nullptr) {
            throw Error("Operation on closed oifs::FileSystem");
        }
    }

    OIFSHandle* handle_ = nullptr;
    std::shared_ptr<OIFSHandle*> handle_token_{};
};

}  // namespace oifs

#endif  // OIFS_HPP
