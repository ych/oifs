//! End-to-End Modern C++17 / C++20 Native API (`include/oifs.hpp`) Integration & Benchmark Suite
//!
//! Validates:
//! 1. `include/oifs.hpp` compiles cleanly with `-Wall -Wextra -Wpedantic -Werror -pthread` and
//!    runs all test suites under both `-std=c++17` and `-std=c++20`.
//! 2. `oifs::PinnableSlice` true 0-copy on `StorageMode::Mmap` (`slice.is_pinned() == true`, pointer
//!    stability across repeated reads, 0 memcpy) and automatic 1-copy fallback (`slice.is_pinned() == false`)
//!    on `StorageMode::FileIo`, compressed (`Stream` / `Seekable64K`), and encrypted files.
//! 3. Out-of-order destruction safety: destroying `oifs::ReadSession` before `oifs::FileReadHandle`
//!    and before a pinned `oifs::PinnableSlice` without Use-After-Free.
//! 4. Scatter-gather `read_at_batch`, `oifs::AsyncFuture` (`read_at_async` inline fast-path & cold path,
//!    `create_file_with_data_async` Group Commit, `write_file_async`, `flush_async`, and early destruction
//!    drop-barrier UAF safety), plus C++ template lambda callback helpers (`read_at_async_cb` and
//!    `create_file_with_data_async_cb`).
//! 5. Read-Only Mode (`F_RDLCK`, `PROT_READ`, 0444 images, encrypted read-only, exhaustive mutation gate).
//! 6. RAII move-construction / move-assignment, `PinnableSlice` reuse across pinned and fallback reads,
//!    and boundary / error cases (0-byte, exact-EOF, past-EOF, misaligned `as_span`, wrong password,
//!    `truncate_file` / `mkdir` / `delete_file` / `ls`).
//! 7. Multi-Process x Multi-Thread C++ stress testing:
//!    - 4 independent OS child processes x 4 `std::thread`s (16 threads total) concurrently holding
//!      `open_read_only` (`F_RDLCK`) across `Mmap` and `FileIo` on plain + encrypted journaled images,
//!      verifying RW `F_WRLCK` contention fails with `oifs::Error` while `F_RDLCK` is held.
//!    - 4 C++ reader threads + 4 C++ writer threads (sync + Async Group Commit under `DurabilityMode::Strict`
//!      + callback creates) concurrently on `Mmap+WAL` and `FileIo+WAL`, verified after read-only reopen.
//! 8. Micro-benchmark comparing Legacy C path-based `oifs_read_at` vs C++ `FileReadHandle::read_at`
//!    vs C++ 0-Copy `PinnableSlice`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn get_dylib_path() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let base = Path::new(&manifest_dir);
    let dylib_name = if cfg!(target_os = "macos") {
        "liboifs.dylib"
    } else if cfg!(target_os = "windows") {
        "oifs.dll"
    } else {
        "liboifs.so"
    };

    let is_release = !cfg!(debug_assertions);
    let profile_dir = if is_release { "release" } else { "debug" };
    let target_dylib = base.join("target").join(profile_dir).join(dylib_name);

    let ffi_src = base.join("src/ffi.rs");
    let fuse_src = base.join("src/fuse.rs");
    let disk_src = base.join("src/disk.rs");
    let storage_src = base.join("src/storage.rs");
    let needs_rebuild = match (
        target_dylib.metadata().and_then(|m| m.modified()),
        ffi_src.metadata().and_then(|m| m.modified()),
        fuse_src.metadata().and_then(|m| m.modified()),
        disk_src.metadata().and_then(|m| m.modified()),
        storage_src.metadata().and_then(|m| m.modified()),
    ) {
        (Ok(dylib_time), Ok(ffi_time), Ok(fuse_time), Ok(disk_time), Ok(storage_time)) => {
            dylib_time < ffi_time
                || dylib_time < fuse_time
                || dylib_time < disk_time
                || dylib_time < storage_time
        }
        _ => true,
    };

    if needs_rebuild {
        let mut cmd = Command::new("cargo");
        cmd.arg("build").arg("--lib");
        if is_release {
            cmd.arg("--release");
        }
        let status = cmd.status().expect("cargo build --lib failed");
        assert!(status.success(), "Failed to build liboifs cdylib");
    }

    assert!(
        target_dylib.exists(),
        "Expected cdylib at {}",
        target_dylib.display()
    );
    target_dylib
}

fn find_cpp_compiler() -> Option<&'static str> {
    ["clang++", "c++", "g++"].into_iter().find(|&candidate| {
        Command::new(candidate)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

fn compiler_supports_flag(compiler: &str, flag: &str) -> bool {
    Command::new(compiler)
        .args([flag, "-dM", "-E", "-x", "c++", "/dev/null"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn compile_and_run_cpp_suite(
    compiler: &str,
    std_flag: &str,
    include_dir: &Path,
    cpp_src_path: &Path,
    dylib_path: &Path,
    bin_path: &Path,
    data_dir: &Path,
) {
    std::fs::create_dir_all(data_dir).expect("create data dir");
    let dylib_dir = dylib_path.parent().unwrap();
    let rpath_arg = format!("-Wl,-rpath,{}", dylib_dir.to_str().unwrap());

    let mut compile_args = vec![
        std_flag,
        "-O2",
        "-Wall",
        "-Wextra",
        "-Wpedantic",
        "-Werror",
        "-pthread",
        "-I",
        include_dir.to_str().unwrap(),
        cpp_src_path.to_str().unwrap(),
        dylib_path.to_str().unwrap(),
    ];
    if !cfg!(target_os = "windows") {
        compile_args.push(&rpath_arg);
    }
    compile_args.extend(["-o", bin_path.to_str().unwrap()]);

    let status = Command::new(compiler)
        .args(&compile_args)
        .status()
        .expect("invoke C++ compiler");
    assert!(
        status.success(),
        "{std_flag} compilation against include/oifs.hpp failed"
    );

    let mut cmd = Command::new(bin_path);
    cmd.args(["--run-all", data_dir.to_str().unwrap()]);
    if cfg!(target_os = "macos") {
        cmd.env("DYLD_LIBRARY_PATH", dylib_dir);
    } else if !cfg!(target_os = "windows") {
        cmd.env("LD_LIBRARY_PATH", dylib_dir);
    }

    let output = cmd.output().expect("run C++ test binary");
    assert!(
        output.status.success(),
        "{std_flag} test binary failed!\nStdout:\n{}\nStderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!(
        "=== [{std_flag}] ===\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn test_cpp_native_zero_copy_async_and_benchmark() {
    let Some(compiler) = find_cpp_compiler() else {
        eprintln!("No C++ compiler found in PATH, skipping C++ API test");
        return;
    };

    let dylib_path = get_dylib_path();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let include_dir = Path::new(&manifest_dir).join("include");

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let cpp_src_path = tmp_dir.path().join("cpp_api_suite.cpp");

    let cpp_code = r##"
#include "oifs.hpp"

#include <algorithm>
#include <atomic>
#include <cassert>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <mutex>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#if defined(OIFS_HAS_CPP20_COROUTINE)
#include <exception>
#endif

#ifndef _WIN32
#include <cerrno>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#endif

#define OIFS_CHECK(cond)                                                                           \
    do {                                                                                           \
        if (!(cond)) {                                                                             \
            std::fprintf(stderr, "OIFS_CHECK FAILED at %s:%d: %s\n", __FILE__, __LINE__, #cond);   \
            std::fflush(stderr);                                                                   \
            std::abort();                                                                          \
        }                                                                                          \
    } while (0)

static std::string join_path(const std::string& dir, const std::string& name) {
    return dir + "/" + name;
}

static std::vector<uint64_t> make_u64_pattern(size_t count, uint64_t seed) {
    std::vector<uint64_t> vals(count);
    for (size_t i = 0; i < count; ++i) {
        vals[i] = (static_cast<uint64_t>(i) * 0x9E3779B97F4A7C15ULL) ^ seed;
    }
    return vals;
}

static uint64_t sum_u64_span(oifs::Span<const uint64_t> sp) {
    uint64_t acc = 0;
    for (size_t i = 0; i < sp.size(); ++i) {
        acc += sp[i];
    }
    return acc;
}

static std::vector<uint8_t> make_compressible_pattern(size_t len, uint8_t tag) {
    std::vector<uint8_t> buf(len);
    for (size_t i = 0; i < len; ++i) {
        if ((i % 64) == 0) {
            buf[i] = static_cast<uint8_t>((i / 64 + tag) & 0xFF);
        } else {
            buf[i] = static_cast<uint8_t>('A' + ((tag + (i % 7)) % 26));
        }
    }
    return buf;
}

static void test_pinnable_slice_zero_copy_and_fallbacks(
    const std::string& mmap_path,
    const std::string& file_io_path,
    const std::string& enc_path
) {
    // 1. Mmap StorageMode: raw uncompressed file -> True 0-Copy (is_pinned() == true)
    {
        oifs::FileSystem fs = oifs::FileSystem::open_with_options(
            mmap_path, 32ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, false);

        // Prepare 64 KiB aligned float array payload
        constexpr size_t kNumFloats = 16384;
        std::vector<float> floats(kNumFloats);
        for (size_t i = 0; i < kNumFloats; ++i) {
            floats[i] = static_cast<float>(i) * 1.25f;
        }
        oifs::ByteView raw_view(reinterpret_cast<const uint8_t*>(floats.data()),
                                floats.size() * sizeof(float));
        uint64_t raw_inode = fs.create_file_with_data(
            "weights.raw", raw_view, oifs::WritePolicy::Raw, 0);

        // Also create a Stream-compressed file and a Seekable-64K compressed file on the same Mmap image
        std::string comp_text(32768, 'Z');
        for (size_t i = 0; i < comp_text.size(); i += 64) {
            comp_text[i] = 'A';
        }
        uint64_t stream_inode = fs.create_file_with_data(
            "compressed.stream", oifs::ByteView(comp_text), oifs::WritePolicy::Stream, 3);
        uint64_t seek_inode = fs.create_file_with_data(
            "compressed.seek64k", oifs::ByteView(comp_text), oifs::WritePolicy::Seekable64K, 3);

        OIFS_CHECK(fs.resolve_path("weights.raw") == raw_inode);
        OIFS_CHECK(fs.resolve_path("compressed.stream") == stream_inode);
        OIFS_CHECK(fs.resolve_path("compressed.seek64k") == seek_inode);

        oifs::ReadSession session = fs.begin_read();
        oifs::FileReadHandle raw_fh = session.open_file("weights.raw");
        OIFS_CHECK(raw_fh.inode_id() == raw_inode);
        OIFS_CHECK(raw_fh.size() == raw_view.size());

        // Verify try_mmap_view returns direct zero-copy pointer
        auto opt_view = raw_fh.try_mmap_view(0, raw_view.size());
        OIFS_CHECK(opt_view.has_value());
        OIFS_CHECK(opt_view->size() == raw_view.size());
        OIFS_CHECK(std::memcmp(opt_view->data(), raw_view.data(), raw_view.size()) == 0);

        // Verify ByteView::as_span<const float>() typed zero-copy access
        auto float_span = opt_view->as_span<const float>();
        OIFS_CHECK(float_span.size() == kNumFloats);
        OIFS_CHECK(float_span[0] == 0.0f);
        OIFS_CHECK(float_span[100] == 125.0f);
        OIFS_CHECK(float_span[kNumFloats - 1] == static_cast<float>(kNumFloats - 1) * 1.25f);

        // Verify PinnableSlice is_pinned() == true and pointer stability across reads
        oifs::PinnableSlice slice;
        size_t n1 = raw_fh.read_pinnable(0, raw_view.size(), slice);
        OIFS_CHECK(n1 == raw_view.size());
        OIFS_CHECK(slice.is_pinned());
        OIFS_CHECK(slice.size() == raw_view.size());
        const uint8_t* pinned_ptr_1 = slice.data();
        OIFS_CHECK(pinned_ptr_1 == opt_view->data());
        OIFS_CHECK(std::memcmp(pinned_ptr_1, raw_view.data(), raw_view.size()) == 0);

        // Second read_pinnable at offset 4096 must point directly to pinned_ptr_1 + 4096 (0 memcpy!)
        oifs::PinnableSlice slice_offset;
        size_t n2 = raw_fh.read_pinnable(4096, 8192, slice_offset);
        OIFS_CHECK(n2 == 8192);
        OIFS_CHECK(slice_offset.is_pinned());
        OIFS_CHECK(slice_offset.data() == pinned_ptr_1 + 4096);

        // Compressed files on Mmap image must automatically fall back to 1-Copy buffer
        oifs::FileReadHandle stream_fh = session.open_inode(stream_inode);
        OIFS_CHECK(!stream_fh.try_mmap_view(0, comp_text.size()).has_value());
        oifs::PinnableSlice comp_slice;
        size_t cn = stream_fh.read_pinnable(0, comp_text.size(), comp_slice);
        OIFS_CHECK(cn == comp_text.size());
        OIFS_CHECK(!comp_slice.is_pinned());
        OIFS_CHECK(comp_slice.as_string_view() == comp_text);

        oifs::FileReadHandle seek_fh = session.open_inode(seek_inode);
        OIFS_CHECK(!seek_fh.try_mmap_view(0, comp_text.size()).has_value());
        size_t sn = seek_fh.read_pinnable(0, comp_text.size(), comp_slice);
        OIFS_CHECK(sn == comp_text.size());
        OIFS_CHECK(!comp_slice.is_pinned());
        OIFS_CHECK(comp_slice.as_string_view() == comp_text);
    }

    // 2. FileIo StorageMode (no mmap): PinnableSlice automatically falls back to 1-Copy
    {
        oifs::FileSystem fs = oifs::FileSystem::open_with_options(
            file_io_path, 16ULL * 1024 * 1024, "", oifs::StorageMode::FileIo, true);
        fs.set_file_io_cache_bytes(8ULL * 1024 * 1024);

        std::string payload = "Pure pread/pwrite FileIo storage engine test payload for PinnableSlice!";
        uint64_t ino = fs.create_file_with_data(
            "fileio.bin", oifs::ByteView(payload), oifs::WritePolicy::Raw, 0);

        oifs::ReadSession sess = fs.begin_read();
        oifs::FileReadHandle fh = sess.open_inode(ino);
        OIFS_CHECK(!fh.try_mmap_view(0, payload.size()).has_value());

        oifs::PinnableSlice slice = fh.read_pinnable(0, payload.size());
        OIFS_CHECK(!slice.is_pinned());
        OIFS_CHECK(slice.size() == payload.size());
        OIFS_CHECK(slice.as_string_view() == payload);
    }

    // 3. Encrypted Image: PinnableSlice automatically falls back to 1-Copy decrypted buffer
    {
        oifs::OpenOptions enc_opts;
        enc_opts.size = 16ULL * 1024 * 1024;
        enc_opts.password = "cpp_api_master_key_2026";
        enc_opts.storage_mode = oifs::StorageMode::Mmap;
        enc_opts.journaled = true;

        oifs::FileSystem enc_fs = oifs::FileSystem::open_with_options(enc_path, enc_opts);
        std::string secret = "Authenticated XChaCha20-Poly1305 encrypted payload via C++ PinnableSlice";
        uint64_t enc_ino = enc_fs.create_file_with_data(
            "secret.bin", oifs::ByteView(secret), oifs::WritePolicy::Raw, 0);

        oifs::ReadSession sess = enc_fs.begin_read();
        oifs::FileReadHandle fh = sess.open_inode(enc_ino);
        OIFS_CHECK(!fh.try_mmap_view(0, secret.size()).has_value());

        oifs::PinnableSlice slice = fh.read_pinnable(0, secret.size());
        OIFS_CHECK(!slice.is_pinned());
        OIFS_CHECK(slice.as_string_view() == secret);
    }
}

static void test_out_of_order_destruction_safety(const std::string& mmap_path) {
    oifs::FileSystem fs = oifs::FileSystem::open_with_options(
        mmap_path, 0, "", oifs::StorageMode::Mmap, false);

    std::string msg = "Out-of-order destruction safety verification payload (ReadSession dies first)";
    uint64_t ino = fs.create_file_with_data(
        "out_of_order.bin", oifs::ByteView(msg), oifs::WritePolicy::Raw, 0);

    oifs::FileReadHandle surviving_fh;
    oifs::PinnableSlice surviving_slice;

    {
        oifs::ReadSession inner_session = fs.begin_read();
        surviving_fh = inner_session.open_inode(ino);
        surviving_fh.read_pinnable(0, msg.size(), surviving_slice);
        OIFS_CHECK(surviving_slice.is_pinned());
        OIFS_CHECK(surviving_slice.as_string_view() == msg);
        // inner_session is destroyed here while surviving_fh and surviving_slice outlive it!
    }

    // Both surviving_fh and surviving_slice must remain 100% valid after ReadSession destruction
    OIFS_CHECK(surviving_slice.is_pinned());
    OIFS_CHECK(surviving_slice.as_string_view() == msg);

    std::vector<uint8_t> buf(msg.size(), 0);
    size_t n = surviving_fh.read_at(0, oifs::MutableByteSpan(buf));
    OIFS_CHECK(n == msg.size());
    OIFS_CHECK(std::memcmp(buf.data(), msg.data(), msg.size()) == 0);

    // Now destroy surviving_fh while surviving_slice STILL holds the pinned zero-copy slice!
    surviving_fh.close();
    OIFS_CHECK(surviving_slice.is_pinned());
    OIFS_CHECK(surviving_slice.as_string_view() == msg);
}

static void test_batch_read_async_futures_and_callbacks(const std::string& async_path) {
    oifs::FileSystem fs = oifs::FileSystem::open_with_options(
        async_path, 32ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, true);
    fs.set_durability_mode(oifs::DurabilityMode::Strict);

    // 1. Concurrent Group-Commit create_file_with_data_async
    constexpr int kNumFiles = 16;
    std::vector<oifs::AsyncFuture> create_futs;
    create_futs.reserve(kNumFiles);
    std::vector<std::string> payloads(kNumFiles);

    for (int i = 0; i < kNumFiles; ++i) {
        payloads[i] = "Group-Commit Async Payload #" + std::to_string(i) + " with deterministic bytes";
        std::string fname = "async_file_" + std::to_string(i) + ".dat";
        create_futs.push_back(fs.create_file_with_data_async(
            fname, oifs::ByteView(payloads[i]), oifs::WritePolicy::Raw, 0));
    }

    std::vector<uint64_t> inodes(kNumFiles);
    for (int i = 0; i < kNumFiles; ++i) {
        int64_t id = create_futs[i].wait();
        OIFS_CHECK(id > 0);
        inodes[i] = static_cast<uint64_t>(id);
    }

    // 2. Scatter-gather read_at_batch across all created inodes
    std::vector<std::vector<uint8_t>> batch_bufs(kNumFiles);
    std::vector<OIFSReadBatchItem> batch_items(kNumFiles);
    for (int i = 0; i < kNumFiles; ++i) {
        batch_bufs[i].resize(payloads[i].size(), 0);
        batch_items[i].inode_id = inodes[i];
        batch_items[i].offset = 0;
        batch_items[i].buf = batch_bufs[i].data();
        batch_items[i].buf_size = batch_bufs[i].size();
        batch_items[i].result = -1;
    }
    fs.read_at_batch(batch_items);
    for (int i = 0; i < kNumFiles; ++i) {
        OIFS_CHECK(batch_items[i].result == static_cast<int64_t>(payloads[i].size()));
        OIFS_CHECK(std::memcmp(batch_bufs[i].data(), payloads[i].data(), payloads[i].size()) == 0);
    }

    // 3. read_at_async Inline Fast-Path (is_ready() == true immediately on Mmap raw file)
    std::vector<uint8_t> async_read_buf(payloads[0].size(), 0);
    oifs::AsyncFuture r_fut = fs.read_at_async(
        inodes[0], 0, oifs::MutableByteSpan(async_read_buf));
    OIFS_CHECK(r_fut.is_ready());
    OIFS_CHECK(r_fut.wait() == static_cast<int64_t>(payloads[0].size()));
    OIFS_CHECK(std::memcmp(async_read_buf.data(), payloads[0].data(), payloads[0].size()) == 0);

    // 4. Early destruction UAF safety on slow-path compressed read_at_async
    std::string large_comp(128 * 1024, 'X');
    for (size_t i = 0; i < large_comp.size(); i += 17) {
        large_comp[i] = static_cast<char>('a' + (i % 26));
    }
    uint64_t comp_ino = fs.create_file_with_data(
        "slow_async_comp.zst", oifs::ByteView(large_comp), oifs::WritePolicy::Stream, 3);
    {
        std::vector<uint8_t> temp_stack_buf(large_comp.size(), 0);
        // Immediately drop AsyncFuture without calling wait(); destructor must block if in-flight
        oifs::AsyncFuture dropped_fut = fs.read_at_async(
            comp_ino, 0, oifs::MutableByteSpan(temp_stack_buf));
    }

    // 5. write_file_async & flush_async
    std::string updated_payload = "Updated via write_file_async!";
    oifs::AsyncFuture w_fut = fs.write_file_async("async_file_0.dat", oifs::ByteView(updated_payload));
    OIFS_CHECK(w_fut.wait() == static_cast<int64_t>(inodes[0]));

    oifs::AsyncFuture fl_fut = fs.flush_async();
    OIFS_CHECK(fl_fut.wait() == 0);

    // Verify sync FileSystem::read_at, read_at_inode, write_file, flush, and oifs::Error handling
    std::vector<uint8_t> sync_buf(updated_payload.size(), 0);
    OIFS_CHECK(fs.read_at("async_file_0.dat", 0, oifs::MutableByteSpan(sync_buf)) == updated_payload.size());
    OIFS_CHECK(std::memcmp(sync_buf.data(), updated_payload.data(), updated_payload.size()) == 0);

    std::string sync_written = "Written synchronously via FileSystem::write_file";
    fs.write_file("sync_check.txt", oifs::ByteView(sync_written));
    fs.flush();
    uint64_t sync_ino = fs.resolve_path("sync_check.txt");
    std::vector<uint8_t> sync_ino_buf(sync_written.size(), 0);
    OIFS_CHECK(fs.read_at_inode(sync_ino, 0, oifs::MutableByteSpan(sync_ino_buf)) == sync_written.size());
    OIFS_CHECK(std::memcmp(sync_ino_buf.data(), sync_written.data(), sync_written.size()) == 0);

    bool caught_missing = false;
    try {
        (void)fs.resolve_path("does_not_exist.bin");
    } catch (const oifs::Error& e) {
        caught_missing = std::strlen(e.what()) > 0;
    }
    OIFS_CHECK(caught_missing);

    // Failed async op populates last_error() and throws oifs::Error on wait()
    std::vector<uint8_t> bad_buf(16, 0);
    oifs::AsyncFuture bad_fut = fs.read_at_async(999999, 0, oifs::MutableByteSpan(bad_buf));
    bool caught_async_err = false;
    try {
        (void)bad_fut.wait();
    } catch (const oifs::Error& e) {
        caught_async_err = std::strlen(e.what()) > 0 && !bad_fut.last_error().empty();
    }
    OIFS_CHECK(caught_async_err);

    // 6. C++ Lambda Callback Helpers: create_file_with_data_async_cb & read_at_async_cb
    std::mutex mtx;
    std::condition_variable cv;
    bool create_cb_done = false;
    int64_t created_cb_inode = -1;
    std::string cb_payload = "Lambda callback async creation and read verification!";

    fs.create_file_with_data_async_cb(
        "cb_file.bin",
        oifs::ByteView(cb_payload),
        [&](int64_t status_or_inode) {
            std::lock_guard<std::mutex> lk(mtx);
            created_cb_inode = status_or_inode;
            create_cb_done = true;
            cv.notify_all();
        });

    {
        std::unique_lock<std::mutex> lk(mtx);
        cv.wait(lk, [&] { return create_cb_done; });
    }
    OIFS_CHECK(created_cb_inode > 0);

    bool read_cb_done = false;
    std::string read_cb_result;
    fs.read_at_async_cb(
        static_cast<uint64_t>(created_cb_inode),
        0,
        cb_payload.size(),
        [&](int64_t status, oifs::ByteView view) {
            std::lock_guard<std::mutex> lk(mtx);
            if (status >= 0) {
                read_cb_result = std::string(view.as_string_view());
            }
            read_cb_done = true;
            cv.notify_all();
        });

    {
        std::unique_lock<std::mutex> lk(mtx);
        cv.wait(lk, [&] { return read_cb_done; });
    }
    OIFS_CHECK(read_cb_result == cb_payload);

    // Throwing callbacks must be safely caught at the C ABI trampoline boundary
    bool throw_cb_ran = false;
    fs.read_at_async_cb(
        static_cast<uint64_t>(created_cb_inode),
        0,
        cb_payload.size(),
        [&](int64_t, oifs::ByteView) {
            {
                std::lock_guard<std::mutex> lk(mtx);
                throw_cb_ran = true;
            }
            cv.notify_all();
            throw std::runtime_error("intentional exception inside read callback");
        });
    {
        std::unique_lock<std::mutex> lk(mtx);
        cv.wait(lk, [&] { return throw_cb_ran; });
    }
}

static void test_read_only_mode(const std::string& ro_path, const std::string& ro_enc_path) {
    // Null handle query must return -1
    OIFS_CHECK(oifs_is_read_only(nullptr) == -1);

    // 1. Prepare a plain journaled image with a contiguous raw file (32 KiB = 8 direct blocks)
    //    and a compressed file, flush/checkpoint cleanly, then close it
    constexpr size_t kRawLen = 32 * 1024;
    std::vector<uint8_t> raw_payload(kRawLen);
    for (size_t i = 0; i < kRawLen; ++i) {
        raw_payload[i] = static_cast<uint8_t>((i * 13 + 5) & 0xFF);
    }
    std::string comp_payload(32768, 'R');
    for (size_t i = 0; i < comp_payload.size(); i += 32) {
        comp_payload[i] = 'O';
    }
    uint64_t raw_inode = 0;
    uint64_t comp_inode = 0;
    {
        oifs::FileSystem rw = oifs::FileSystem::open_with_options(
            ro_path, 16ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, true);
        OIFS_CHECK(!rw.is_read_only());
        OIFS_CHECK(oifs_is_read_only(rw.raw()) == 0);
        raw_inode = rw.create_file_with_data(
            "weights_ro.raw", oifs::ByteView(raw_payload), oifs::WritePolicy::Raw, 0);
        comp_inode = rw.create_file_with_data(
            "comp_ro.zst", oifs::ByteView(comp_payload), oifs::WritePolicy::Stream, 3);
        rw.flush();
    }

    // Prepare an encrypted image as well
    std::string enc_secret = "Encrypted 0444 read-only image payload verified via C++ API!";
    {
        oifs::FileSystem rw_enc = oifs::FileSystem::open_with_options(
            ro_enc_path, 16ULL * 1024 * 1024, "ro_secret_pwd", oifs::StorageMode::Mmap, true);
        OIFS_CHECK(!rw_enc.is_read_only());
        (void)rw_enc.create_file_with_data(
            "secret.txt", oifs::ByteView(enc_secret), oifs::WritePolicy::Raw, 0);
        rw_enc.flush();
    }

#ifndef _WIN32
    OIFS_CHECK(::chmod(ro_path.c_str(), 0444) == 0);
    OIFS_CHECK(::chmod(ro_enc_path.c_str(), 0444) == 0);
#endif

    // 2. Open TWO simultaneous C++ read-only instances (Mmap) plus a C FFI FileIo instance
    {
        oifs::FileSystem ro1 = oifs::FileSystem::open_read_only(
            ro_path, "", oifs::StorageMode::Mmap);
        oifs::FileSystem ro2 = oifs::FileSystem::open_read_only(
            ro_path, "", oifs::StorageMode::Mmap);
        OIFSHandle* ro_c_fileio = oifs_open_read_only(
            ro_path.c_str(), nullptr, OIFS_STORAGE_MODE_FILE_IO);
        OIFS_CHECK(ro_c_fileio != nullptr);

        OIFS_CHECK(ro1.is_read_only());
        OIFS_CHECK(ro2.is_read_only());
        OIFS_CHECK(oifs_is_read_only(ro1.raw()) == 1);
        OIFS_CHECK(oifs_is_read_only(ro2.raw()) == 1);
        OIFS_CHECK(oifs_is_read_only(ro_c_fileio) == 1);

        // Verify 0-copy PinnableSlice on both simultaneous read-only Mmap instances
        oifs::ReadSession s1 = ro1.begin_read();
        oifs::ReadSession s2 = ro2.begin_read();
        oifs::FileReadHandle fh1 = s1.open_file("weights_ro.raw");
        oifs::FileReadHandle fh2 = s2.open_inode(raw_inode);
        OIFS_CHECK(fh1.size() == kRawLen);
        OIFS_CHECK(fh2.size() == kRawLen);

        oifs::PinnableSlice slice1 = fh1.read_pinnable(0, kRawLen);
        oifs::PinnableSlice slice2 = fh2.read_pinnable(4096, 8192);
        OIFS_CHECK(slice1.is_pinned());
        OIFS_CHECK(slice2.is_pinned());
        OIFS_CHECK(slice1.size() == kRawLen);
        OIFS_CHECK(std::memcmp(slice1.data(), raw_payload.data(), kRawLen) == 0);
        OIFS_CHECK(slice2.size() == 8192);
        OIFS_CHECK(std::memcmp(slice2.data(), raw_payload.data() + 4096, 8192) == 0);

        // Compressed file on read-only Mmap falls back cleanly to 1-copy
        oifs::FileReadHandle comp_fh = s1.open_inode(comp_inode);
        oifs::PinnableSlice comp_slice = comp_fh.read_pinnable(0, comp_payload.size());
        OIFS_CHECK(!comp_slice.is_pinned());
        OIFS_CHECK(comp_slice.as_string_view() == comp_payload);

        // C FFI FileIo read-only handle also reads exact bytes
        std::vector<uint8_t> fio_buf(kRawLen, 0);
        int64_t fio_n = oifs_read_at(
            ro_c_fileio, "weights_ro.raw", 0, fio_buf.data(), fio_buf.size());
        OIFS_CHECK(fio_n == static_cast<int64_t>(kRawLen));
        OIFS_CHECK(std::memcmp(fio_buf.data(), raw_payload.data(), kRawLen) == 0);

        // Verify read_at_async works on read-only Mmap handle (Inline Fast-Path)
        std::vector<uint8_t> async_buf(4096, 0);
        oifs::AsyncFuture r_fut = ro1.read_at_async(
            raw_inode, 8192, oifs::MutableByteSpan(async_buf));
        OIFS_CHECK(r_fut.is_ready());
        OIFS_CHECK(r_fut.wait() == 4096);
        OIFS_CHECK(std::memcmp(async_buf.data(), raw_payload.data() + 8192, 4096) == 0);

        // Release read sessions before testing mutating calls
        fh1.close();
        fh2.close();
        comp_fh.close();
        slice1.reset();
        slice2.reset();
        comp_slice.reset();
        s1.close();
        s2.close();

        // 3. Exhaustively verify C++ and C FFI mutating calls fail cleanly with ReadOnly error
        auto expect_ro_error = [](const std::string& msg) {
            OIFS_CHECK(msg.find("read-only") != std::string::npos);
        };

        std::string dummy = "must not be written";

        bool caught_create = false;
        try {
            (void)ro1.create_file_with_data("blocked.txt", oifs::ByteView(dummy));
        } catch (const oifs::Error& e) {
            expect_ro_error(e.what());
            caught_create = true;
        }
        OIFS_CHECK(caught_create);

        bool caught_write = false;
        try {
            ro1.write_file("weights_ro.raw", oifs::ByteView(dummy));
        } catch (const oifs::Error& e) {
            expect_ro_error(e.what());
            caught_write = true;
        }
        OIFS_CHECK(caught_write);

        bool caught_async_create = false;
        try {
            oifs::AsyncFuture bad_w = ro1.create_file_with_data_async(
                "blocked_async.txt", oifs::ByteView(dummy));
            (void)bad_w.wait();
        } catch (const oifs::Error& e) {
            expect_ro_error(e.what());
            caught_async_create = true;
        }
        OIFS_CHECK(caught_async_create);

        bool caught_truncate = false;
        try {
            ro1.truncate_file("weights_ro.raw", 0);
        } catch (const oifs::Error& e) {
            expect_ro_error(e.what());
            caught_truncate = true;
        }
        OIFS_CHECK(caught_truncate);

        char err_buf[256] = {0};
        OIFS_CHECK(oifs_mkdir(ro1.raw(), "ro_dir") == -1);
        OIFS_CHECK(oifs_last_error(ro1.raw(), err_buf, sizeof(err_buf)) == 0);
        expect_ro_error(err_buf);

        std::memset(err_buf, 0, sizeof(err_buf));
        OIFS_CHECK(oifs_delete_file(ro1.raw(), "weights_ro.raw") == -1);
        OIFS_CHECK(oifs_last_error(ro1.raw(), err_buf, sizeof(err_buf)) == 0);
        expect_ro_error(err_buf);

        std::memset(err_buf, 0, sizeof(err_buf));
        OIFS_CHECK(oifs_truncate_file(ro1.raw(), "weights_ro.raw", 0) == -1);
        OIFS_CHECK(oifs_last_error(ro1.raw(), err_buf, sizeof(err_buf)) == 0);
        expect_ro_error(err_buf);

        oifs_close(ro_c_fileio);
    }

    // 4. Encrypted 0444 read-only image: missing/wrong password fails, right password succeeds
    {
        bool caught_no_pwd = false;
        try {
            (void)oifs::FileSystem::open_read_only(ro_enc_path, "", oifs::StorageMode::Mmap);
        } catch (const oifs::Error&) {
            caught_no_pwd = true;
        }
        OIFS_CHECK(caught_no_pwd);

        oifs::FileSystem enc_ro = oifs::FileSystem::open_read_only(
            ro_enc_path, "ro_secret_pwd", oifs::StorageMode::Mmap);
        oifs::FileSystem enc_ro_fio = oifs::FileSystem::open_read_only(
            ro_enc_path, "ro_secret_pwd", oifs::StorageMode::FileIo);
        OIFS_CHECK(enc_ro.is_read_only());
        OIFS_CHECK(enc_ro_fio.is_read_only());

        std::vector<uint8_t> dec1(enc_secret.size(), 0);
        std::vector<uint8_t> dec2(enc_secret.size(), 0);
        OIFS_CHECK(enc_ro.read_at("secret.txt", 0, oifs::MutableByteSpan(dec1)) == enc_secret.size());
        OIFS_CHECK(enc_ro_fio.read_at("secret.txt", 0, oifs::MutableByteSpan(dec2)) == enc_secret.size());
        OIFS_CHECK(std::memcmp(dec1.data(), enc_secret.data(), enc_secret.size()) == 0);
        OIFS_CHECK(std::memcmp(dec2.data(), enc_secret.data(), enc_secret.size()) == 0);
    }

#ifndef _WIN32
    OIFS_CHECK(::chmod(ro_path.c_str(), 0644) == 0);
    OIFS_CHECK(::chmod(ro_enc_path.c_str(), 0644) == 0);
#endif
}

static void test_raii_moves_and_edge_cases(
    const std::string& edge_mmap_path,
    const std::string& edge_fio_path,
    const std::string& edge_enc_path
) {
    // 1. Setup Mmap image + FileIo image for RAII move semantics & PinnableSlice reuse
    std::vector<uint64_t> raw_words = make_u64_pattern(2048, 0xCAFEBABE12345678ULL); // 16 KiB
    oifs::ByteView raw_view(reinterpret_cast<const uint8_t*>(raw_words.data()),
                            raw_words.size() * sizeof(uint64_t));
    std::vector<uint8_t> stream_bytes = make_compressible_pattern(24576, 3);
    std::vector<uint8_t> seek_bytes = make_compressible_pattern(131072, 9);
    std::string fio_payload = "FileIo fallback payload for PinnableSlice alternating reuse test!";

    // Move-construction & move-assignment of FileSystem
    oifs::FileSystem fs_default;
    OIFS_CHECK(!static_cast<bool>(fs_default));
    OIFS_CHECK(!fs_default.is_read_only());

    oifs::FileSystem fs_tmp = oifs::FileSystem::open_with_options(
        edge_mmap_path, 32ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, true);
    OIFS_CHECK(static_cast<bool>(fs_tmp));

    oifs::FileSystem fs_moved_ctor(std::move(fs_tmp));
    OIFS_CHECK(!static_cast<bool>(fs_tmp));
    OIFS_CHECK(static_cast<bool>(fs_moved_ctor));

    oifs::FileSystem fs_other = oifs::FileSystem::open_with_options(
        edge_fio_path, 16ULL * 1024 * 1024, "", oifs::StorageMode::FileIo, true);
    uint64_t fio_ino = fs_other.create_file_with_data(
        "fio_reuse.bin", oifs::ByteView(fio_payload), oifs::WritePolicy::Raw, 0);
    fs_other.flush();

    // Move-assign fs_moved_ctor into fs_default
    fs_default = std::move(fs_moved_ctor);
    OIFS_CHECK(!static_cast<bool>(fs_moved_ctor));
    OIFS_CHECK(static_cast<bool>(fs_default));

    // Also test move-assign onto a non-empty FileSystem (closes prior handle cleanly)
    {
        oifs::FileSystem extra = oifs::FileSystem::open_with_options(
            edge_mmap_path + ".extra", 4ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, false);
        OIFS_CHECK(static_cast<bool>(extra));
        extra = std::move(fs_default);
        OIFS_CHECK(!static_cast<bool>(fs_default));
        OIFS_CHECK(static_cast<bool>(extra));
        fs_default = std::move(extra);
        OIFS_CHECK(!static_cast<bool>(extra));
        OIFS_CHECK(static_cast<bool>(fs_default));
    }

    uint64_t raw_ino = fs_default.create_file_with_data(
        "edge_raw.bin", raw_view, oifs::WritePolicy::Raw, 0);
    uint64_t stream_ino = fs_default.create_file_with_data(
        "edge_stream.zst", oifs::ByteView(stream_bytes), oifs::WritePolicy::Stream, 3);
    uint64_t seek_ino = fs_default.create_file_with_data(
        "edge_seek.zst", oifs::ByteView(seek_bytes), oifs::WritePolicy::Seekable64K, 3);
    uint64_t empty_ino = fs_default.create_file_with_data(
        "empty.bin", oifs::ByteView(), oifs::WritePolicy::Raw, 0);

    // 2. Move-construction & move-assignment of ReadSession, FileReadHandle, PinnableSlice
    {
        oifs::ReadSession s_empty;
        OIFS_CHECK(!static_cast<bool>(s_empty));

        oifs::ReadSession s1 = fs_default.begin_read();
        OIFS_CHECK(static_cast<bool>(s1));
        oifs::ReadSession s2(std::move(s1));
        OIFS_CHECK(!static_cast<bool>(s1));
        OIFS_CHECK(static_cast<bool>(s2));

        // Using moved-from ReadSession must throw oifs::Error
        bool caught_moved_sess = false;
        try {
            (void)s1.open_inode(raw_ino);
        } catch (const oifs::Error&) {
            caught_moved_sess = true;
        }
        OIFS_CHECK(caught_moved_sess);

        oifs::ReadSession s3 = fs_default.begin_read();
        s3 = std::move(s2);
        OIFS_CHECK(!static_cast<bool>(s2));
        OIFS_CHECK(static_cast<bool>(s3));

        // FileReadHandle move-construction & move-assignment
        oifs::FileReadHandle fh_empty;
        OIFS_CHECK(!static_cast<bool>(fh_empty));
        OIFS_CHECK(fh_empty.size() == 0);
        OIFS_CHECK(fh_empty.inode_id() == 0);

        oifs::FileReadHandle fh1 = s3.open_inode(raw_ino);
        oifs::FileReadHandle fh2(std::move(fh1));
        OIFS_CHECK(!static_cast<bool>(fh1));
        OIFS_CHECK(static_cast<bool>(fh2));
        OIFS_CHECK(fh2.inode_id() == raw_ino);
        OIFS_CHECK(fh2.size() == raw_view.size());

        bool caught_moved_fh = false;
        try {
            std::vector<uint8_t> tmp(8, 0);
            (void)fh1.read_at(0, oifs::MutableByteSpan(tmp));
        } catch (const oifs::Error&) {
            caught_moved_fh = true;
        }
        OIFS_CHECK(caught_moved_fh);

        oifs::FileReadHandle fh3 = s3.open_inode(stream_ino);
        fh3 = std::move(fh2);
        OIFS_CHECK(!static_cast<bool>(fh2));
        OIFS_CHECK(fh3.inode_id() == raw_ino);

        // PinnableSlice move-construction & move-assignment: pinned (0-copy) case
        oifs::PinnableSlice pin_a = fh3.read_pinnable(0, raw_view.size());
        OIFS_CHECK(pin_a.is_pinned());
        const uint8_t* orig_ptr = pin_a.data();

        oifs::PinnableSlice pin_b(std::move(pin_a));
        OIFS_CHECK(!pin_a.is_pinned());
        OIFS_CHECK(pin_a.empty());
        OIFS_CHECK(pin_a.data() == nullptr);
        OIFS_CHECK(pin_b.is_pinned());
        OIFS_CHECK(pin_b.data() == orig_ptr);
        OIFS_CHECK(pin_b.size() == raw_view.size());
        OIFS_CHECK(sum_u64_span(pin_b.as_span<const uint64_t>()) ==
                   sum_u64_span(raw_view.as_span<const uint64_t>()));

        oifs::PinnableSlice pin_c;
        pin_c = std::move(pin_b);
        OIFS_CHECK(pin_b.empty());
        OIFS_CHECK(pin_c.is_pinned());
        OIFS_CHECK(pin_c.data() == orig_ptr);

        // PinnableSlice move-construction & move-assignment: unpinned 1-copy fallback case
        oifs::FileReadHandle stream_fh = s3.open_inode(stream_ino);
        oifs::PinnableSlice fb_a = stream_fh.read_pinnable(0, stream_bytes.size());
        OIFS_CHECK(!fb_a.is_pinned());
        OIFS_CHECK(fb_a.size() == stream_bytes.size());

        oifs::PinnableSlice fb_b(std::move(fb_a));
        OIFS_CHECK(fb_a.empty());
        OIFS_CHECK(!fb_b.is_pinned());
        OIFS_CHECK(fb_b.size() == stream_bytes.size());
        OIFS_CHECK(std::memcmp(fb_b.data(), stream_bytes.data(), stream_bytes.size()) == 0);

        // Move-assign fallback slice over previously pinned slice
        pin_c = std::move(fb_b);
        OIFS_CHECK(fb_b.empty());
        OIFS_CHECK(!pin_c.is_pinned());
        OIFS_CHECK(pin_c.size() == stream_bytes.size());
        OIFS_CHECK(std::memcmp(pin_c.data(), stream_bytes.data(), stream_bytes.size()) == 0);

        // Move empty slice
        oifs::PinnableSlice empty_src;
        oifs::PinnableSlice empty_dst(std::move(empty_src));
        OIFS_CHECK(empty_dst.empty());
        OIFS_CHECK(!empty_dst.is_pinned());
        empty_src = std::move(empty_dst);
        OIFS_CHECK(empty_src.empty());

        // 3. Reusing the SAME PinnableSlice across alternating 0-copy Mmap and 1-copy compressed/FileIo reads
        oifs::FileReadHandle seek_fh = s3.open_inode(seek_ino);
        oifs::ReadSession fio_sess = fs_other.begin_read();
        oifs::FileReadHandle fio_fh = fio_sess.open_inode(fio_ino);

        oifs::PinnableSlice reused;
        for (int round = 0; round < 3; ++round) {
            // 0-copy Mmap raw full
            OIFS_CHECK(fh3.read_pinnable(0, raw_view.size(), reused) == raw_view.size());
            OIFS_CHECK(reused.is_pinned());
            OIFS_CHECK(reused.data() == orig_ptr);
            OIFS_CHECK(std::memcmp(reused.data(), raw_view.data(), raw_view.size()) == 0);

            // 1-copy Mmap Stream compressed
            OIFS_CHECK(stream_fh.read_pinnable(0, stream_bytes.size(), reused) == stream_bytes.size());
            OIFS_CHECK(!reused.is_pinned());
            OIFS_CHECK(std::memcmp(reused.data(), stream_bytes.data(), stream_bytes.size()) == 0);

            // 0-copy Mmap raw non-zero offset
            OIFS_CHECK(fh3.read_pinnable(4096, 4096, reused) == 4096);
            OIFS_CHECK(reused.is_pinned());
            OIFS_CHECK(reused.data() == orig_ptr + 4096);

            // 1-copy Mmap Seekable64K cross-chunk slice
            OIFS_CHECK(seek_fh.read_pinnable(60000, 12000, reused) == 12000);
            OIFS_CHECK(!reused.is_pinned());
            OIFS_CHECK(std::memcmp(reused.data(), seek_bytes.data() + 60000, 12000) == 0);

            // 1-copy FileIo raw
            OIFS_CHECK(fio_fh.read_pinnable(0, fio_payload.size(), reused) == fio_payload.size());
            OIFS_CHECK(!reused.is_pinned());
            OIFS_CHECK(reused.as_string_view() == fio_payload);

            // Reset retains vector capacity and clears pin
            reused.reset();
            OIFS_CHECK(reused.empty());
            OIFS_CHECK(!reused.is_pinned());
        }

        // 4. Boundary reads: 0-byte, empty file, exact-EOF, past-EOF, partial tail
        std::vector<uint8_t> small_buf(64, 0xAA);
        // 0-byte read on non-empty file
        OIFS_CHECK(fh3.read_at(0, oifs::MutableByteSpan(small_buf.data(), 0)) == 0);
        OIFS_CHECK(fs_default.read_at("edge_raw.bin", 0, oifs::MutableByteSpan(small_buf.data(), 0)) == 0);
        OIFS_CHECK(fh3.read_pinnable(0, 0, reused) == 0);
        OIFS_CHECK(reused.empty());

        // Reading 0-byte empty file
        oifs::FileReadHandle empty_fh = s3.open_inode(empty_ino);
        OIFS_CHECK(empty_fh.size() == 0);
        OIFS_CHECK(empty_fh.read_at(0, oifs::MutableByteSpan(small_buf)) == 0);
        OIFS_CHECK(empty_fh.read_pinnable(0, 64, reused) == 0);
        OIFS_CHECK(reused.empty());

        // Exact-EOF and past-EOF reads
        const uint64_t rsz = raw_view.size();
        OIFS_CHECK(fh3.read_at(rsz, oifs::MutableByteSpan(small_buf)) == 0);
        OIFS_CHECK(fh3.read_at(rsz + 4096, oifs::MutableByteSpan(small_buf)) == 0);
        OIFS_CHECK(fs_default.read_at("edge_raw.bin", rsz, oifs::MutableByteSpan(small_buf)) == 0);
        OIFS_CHECK(fs_default.read_at("edge_raw.bin", rsz + 9999, oifs::MutableByteSpan(small_buf)) == 0);
        OIFS_CHECK(fh3.read_pinnable(rsz, 128, reused) == 0);
        OIFS_CHECK(reused.empty());
        OIFS_CHECK(fh3.read_pinnable(rsz + 1024, 128, reused) == 0);
        OIFS_CHECK(reused.empty());

        // Partial tail read (last 16 bytes when requesting 64)
        OIFS_CHECK(fh3.read_at(rsz - 16, oifs::MutableByteSpan(small_buf)) == 16);
        OIFS_CHECK(std::memcmp(small_buf.data(), raw_view.data() + (rsz - 16), 16) == 0);
        OIFS_CHECK(fh3.read_pinnable(rsz - 16, 64, reused) == 16);
        OIFS_CHECK(reused.is_pinned());
        OIFS_CHECK(std::memcmp(reused.data(), raw_view.data() + (rsz - 16), 16) == 0);

        // Stream compressed exact-EOF and past-EOF
        OIFS_CHECK(stream_fh.read_at(stream_bytes.size(), oifs::MutableByteSpan(small_buf)) == 0);
        OIFS_CHECK(stream_fh.read_at(stream_bytes.size() + 512, oifs::MutableByteSpan(small_buf)) == 0);
        OIFS_CHECK(stream_fh.read_pinnable(stream_bytes.size(), 64, reused) == 0);
        OIFS_CHECK(reused.empty());
    }

    // 5. AsyncFuture move-construction, move-assignment, 0-byte & past-EOF async reads
    {
        oifs::AsyncFuture f_empty;
        OIFS_CHECK(!static_cast<bool>(f_empty));
        OIFS_CHECK(!f_empty.is_ready());
        bool caught_empty_wait = false;
        try {
            (void)f_empty.wait();
        } catch (const oifs::Error&) {
            caught_empty_wait = true;
        }
        OIFS_CHECK(caught_empty_wait);

        std::vector<uint8_t> abuf(256, 0);
        oifs::AsyncFuture f1 = fs_default.read_at_async(raw_ino, 0, oifs::MutableByteSpan(abuf));
        oifs::AsyncFuture f2(std::move(f1));
        OIFS_CHECK(!static_cast<bool>(f1));
        OIFS_CHECK(static_cast<bool>(f2));

        oifs::AsyncFuture f3 = fs_default.read_at_async(raw_ino, 0, oifs::MutableByteSpan(abuf.data(), 0));
        OIFS_CHECK(f3.is_ready());
        f3 = std::move(f2);
        OIFS_CHECK(!static_cast<bool>(f2));
        OIFS_CHECK(f3.wait() == 256);
        OIFS_CHECK(std::memcmp(abuf.data(), raw_view.data(), 256) == 0);

        // 0-byte async read and past-EOF async read
        oifs::AsyncFuture f_zero = fs_default.read_at_async(
            raw_ino, 0, oifs::MutableByteSpan(abuf.data(), 0));
        OIFS_CHECK(f_zero.is_ready());
        OIFS_CHECK(f_zero.wait() == 0);

        oifs::AsyncFuture f_eof = fs_default.read_at_async(
            raw_ino, raw_view.size() + 128, oifs::MutableByteSpan(abuf));
        OIFS_CHECK(f_eof.wait() == 0);

        // Move a pending async Group-Commit write future
        std::string moved_async_payload = "Created through a moved AsyncFuture!";
        oifs::AsyncFuture fw1 = fs_default.create_file_with_data_async(
            "moved_async.bin", oifs::ByteView(moved_async_payload), oifs::WritePolicy::Raw, 0);
        oifs::AsyncFuture fw2(std::move(fw1));
        OIFS_CHECK(!static_cast<bool>(fw1));
        OIFS_CHECK(fw2.wait() > 0);
    }

    // 6. Misaligned and non-multiple ByteView::as_span<const uint64_t>() must throw oifs::Error
    {
        alignas(8) uint8_t aligned_buf[32] = {0};
        for (size_t i = 0; i < sizeof(aligned_buf); ++i) {
            aligned_buf[i] = static_cast<uint8_t>(i);
        }

        // Valid aligned multiple-of-8 view succeeds
        oifs::ByteView good_bv(aligned_buf, 16);
        auto good_sp = good_bv.as_span<const uint64_t>();
        OIFS_CHECK(good_sp.size() == 2);
        OIFS_CHECK(good_sp.size_bytes() == 16);

#if __cplusplus >= 202002L && defined(__cpp_lib_span)
        // Verify C++20 std::span interoperability
        std::span<const uint64_t> std_sp = good_sp;
        OIFS_CHECK(std_sp.size() == 2);
        OIFS_CHECK(std_sp.data() == good_sp.data());

        std::span<const uint8_t> ro_bytes(aligned_buf, 16);
        oifs::ByteView from_span(ro_bytes);
        OIFS_CHECK(from_span.size() == 16);

        std::span<uint8_t> rw_bytes(aligned_buf, 16);
        oifs::MutableByteSpan mut_from_span(rw_bytes);
        OIFS_CHECK(mut_from_span.size() == 16);
#endif

        // Misaligned pointer (offset +1, size 16 is a multiple of 8) -> MUST throw oifs::Error
        oifs::ByteView misaligned_bv(aligned_buf + 1, 16);
        bool caught_misaligned = false;
        try {
            (void)misaligned_bv.as_span<const uint64_t>();
        } catch (const oifs::Error& e) {
            caught_misaligned = std::string(e.what()).find("aligned") != std::string::npos;
        }
        OIFS_CHECK(caught_misaligned);

        // Non-multiple size (size 12) -> MUST throw oifs::Error
        oifs::ByteView bad_size_bv(aligned_buf, 12);
        bool caught_bad_size = false;
        try {
            (void)bad_size_bv.as_span<const uint64_t>();
        } catch (const oifs::Error& e) {
            caught_bad_size = std::string(e.what()).find("multiple") != std::string::npos;
        }
        OIFS_CHECK(caught_bad_size);
    }

    // 7. truncate_file / mkdir / delete_file / ls from C++
    {
        fs_default.mkdir("edge_dir");
        std::string nested_data = "Nested directory file content";
        (void)fs_default.create_file_with_data(
            "edge_dir/nested.bin", oifs::ByteView(nested_data), oifs::WritePolicy::Raw, 0);

        std::vector<uint8_t> trunc_data(8192);
        for (size_t i = 0; i < trunc_data.size(); ++i) {
            trunc_data[i] = static_cast<uint8_t>((i * 7 + 3) & 0xFF);
        }
        uint64_t trunc_ino = fs_default.create_file_with_data(
            "trunc_target.bin", oifs::ByteView(trunc_data), oifs::WritePolicy::Raw, 0);

        // Verify ls() lists root entries including edge_dir and trunc_target.bin
        std::vector<oifs::DirEntry> entries = fs_default.ls();
        auto has_entry = [&](const std::string& name) {
            return std::any_of(entries.begin(), entries.end(), [&](const oifs::DirEntry& e) {
                return e.name == name;
            });
        };
        OIFS_CHECK(has_entry("edge_raw.bin"));
        OIFS_CHECK(has_entry("edge_dir"));
        OIFS_CHECK(has_entry("trunc_target.bin"));

        // Shrink trunc_target.bin from 8192 -> 100 bytes
        fs_default.truncate_file("trunc_target.bin", 100);
        {
            oifs::ReadSession s = fs_default.begin_read();
            oifs::FileReadHandle fh = s.open_inode(trunc_ino);
            OIFS_CHECK(fh.size() == 100);
            std::vector<uint8_t> check(200, 0xFF);
            OIFS_CHECK(fh.read_at(0, oifs::MutableByteSpan(check)) == 100);
            OIFS_CHECK(std::memcmp(check.data(), trunc_data.data(), 100) == 0);
        }

        // Extend trunc_target.bin from 100 -> 10000 bytes (extended bytes must read as zero)
        fs_default.truncate_file("trunc_target.bin", 10000);
        {
            oifs::ReadSession s = fs_default.begin_read();
            oifs::FileReadHandle fh = s.open_inode(trunc_ino);
            OIFS_CHECK(fh.size() == 10000);
            std::vector<uint8_t> check(10000, 0xEE);
            OIFS_CHECK(fh.read_at(0, oifs::MutableByteSpan(check)) == 10000);
            OIFS_CHECK(std::memcmp(check.data(), trunc_data.data(), 100) == 0);
            for (size_t i = 100; i < 10000; ++i) {
                OIFS_CHECK(check[i] == 0);
            }
        }

        // Delete files and verify removal via resolve_path and ls()
        fs_default.delete_file("edge_dir/nested.bin");
        fs_default.delete_file("trunc_target.bin");

        bool caught_deleted = false;
        try {
            (void)fs_default.resolve_path("trunc_target.bin");
        } catch (const oifs::Error&) {
            caught_deleted = true;
        }
        OIFS_CHECK(caught_deleted);

        entries = fs_default.ls();
        OIFS_CHECK(!has_entry("trunc_target.bin"));
    }

    fs_other.close();

    // 8. Encrypted image authentication: missing password fails on open; wrong password
    //    rejects path lookup, inode decryption, and directory listing with oifs::Error;
    //    supplying a password for an unencrypted image fails on open (anti-downgrade).
    {
        uint64_t vault_ino = 0;
        {
            oifs::FileSystem enc_writer = oifs::FileSystem::open_with_options(
                edge_enc_path, 8ULL * 1024 * 1024, "correct_horse_pwd", oifs::StorageMode::Mmap, true);
            std::string sec = "Top-secret bytes";
            vault_ino = enc_writer.create_file_with_data(
                "vault.bin", oifs::ByteView(sec), oifs::WritePolicy::Raw, 0);
            enc_writer.flush();
        }

        // Missing password on encrypted image fails on open
        bool caught_no_pwd = false;
        try {
            (void)oifs::FileSystem::open(edge_enc_path, 0);
        } catch (const oifs::Error&) {
            caught_no_pwd = true;
        }
        OIFS_CHECK(caught_no_pwd);

        // Password supplied for an unencrypted image fails on open (anti-downgrade)
        bool caught_not_enc = false;
        try {
            (void)oifs::FileSystem::open_read_only(
                edge_fio_path, "unexpected_password", oifs::StorageMode::FileIo);
        } catch (const oifs::Error&) {
            caught_not_enc = true;
        }
        OIFS_CHECK(caught_not_enc);

        auto verify_wrong_pwd_rejected = [&](const oifs::FileSystem& wrong_fs) {
            bool caught_lookup = false;
            try {
                (void)wrong_fs.resolve_path("vault.bin");
            } catch (const oifs::Error&) {
                caught_lookup = true;
            }
            OIFS_CHECK(caught_lookup);

            bool caught_read_path = false;
            try {
                std::vector<uint8_t> dec(32, 0);
                (void)wrong_fs.read_at("vault.bin", 0, oifs::MutableByteSpan(dec));
            } catch (const oifs::Error&) {
                caught_read_path = true;
            }
            OIFS_CHECK(caught_read_path);

            bool caught_read_inode = false;
            try {
                std::vector<uint8_t> dec(32, 0);
                (void)wrong_fs.read_at_inode(vault_ino, 0, oifs::MutableByteSpan(dec));
            } catch (const oifs::Error&) {
                caught_read_inode = true;
            }
            OIFS_CHECK(caught_read_inode);

            // Directory listing with wrong key must never expose plaintext filename
            std::vector<oifs::DirEntry> wrong_entries = wrong_fs.ls();
            for (const auto& e : wrong_entries) {
                OIFS_CHECK(e.name != "vault.bin");
            }
        };

        {
            oifs::FileSystem wrong_rw =
                oifs::FileSystem::open_with_password(edge_enc_path, 0, "wrong_pwd_1");
            verify_wrong_pwd_rejected(wrong_rw);
        }
        {
            oifs::FileSystem wrong_fio = oifs::FileSystem::open_with_options(
                edge_enc_path, 0, "wrong_pwd_2", oifs::StorageMode::FileIo, true);
            verify_wrong_pwd_rejected(wrong_fio);
        }
        {
            oifs::FileSystem wrong_ro = oifs::FileSystem::open_read_only(
                edge_enc_path, "wrong_pwd_3", oifs::StorageMode::Mmap);
            verify_wrong_pwd_rejected(wrong_ro);
        }
    }
}

#ifndef _WIN32
static void write_all_fd(int fd, const void* buf, size_t len) {
    const uint8_t* p = static_cast<const uint8_t*>(buf);
    size_t written = 0;
    while (written < len) {
        ssize_t n = ::write(fd, p + written, len - written);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        OIFS_CHECK(n > 0);
        written += static_cast<size_t>(n);
    }
}

static void read_all_fd(int fd, void* buf, size_t len) {
    uint8_t* p = static_cast<uint8_t*>(buf);
    size_t got = 0;
    while (got < len) {
        ssize_t n = ::read(fd, p + got, len - got);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        OIFS_CHECK(n > 0);
        got += static_cast<size_t>(n);
    }
}

struct MpDatasetExpectations {
    static constexpr size_t kRawACount = 4096;  // 32 KiB (8 contiguous direct blocks)
    static constexpr size_t kRawBCount = 8192;  // 64 KiB (16 blocks)
    static constexpr size_t kStreamLen = 49152; // 48 KiB
    static constexpr size_t kSeekLen = 196608;  // 192 KiB (3 x 64 KiB chunks)

    std::vector<uint64_t> raw_a;
    std::vector<uint64_t> raw_b;
    uint64_t raw_a_sum = 0;
    uint64_t raw_b_sum = 0;
    std::vector<uint8_t> stream_data;
    std::vector<uint8_t> seek_data;

    explicit MpDatasetExpectations(uint64_t seed_offset) {
        raw_a = make_u64_pattern(kRawACount, 0x1111222233334444ULL + seed_offset);
        raw_b = make_u64_pattern(kRawBCount, 0xAAAABBBBCCCCDDDDULL + seed_offset);
        raw_a_sum = sum_u64_span(oifs::Span<const uint64_t>(raw_a.data(), raw_a.size()));
        raw_b_sum = sum_u64_span(oifs::Span<const uint64_t>(raw_b.data(), raw_b.size()));
        stream_data = make_compressible_pattern(kStreamLen, static_cast<uint8_t>(11 + seed_offset));
        seek_data = make_compressible_pattern(kSeekLen, static_cast<uint8_t>(23 + seed_offset));
    }
};

static void populate_mp_image(
    const std::string& path,
    const std::string& password,
    const MpDatasetExpectations& exp
) {
    oifs::OpenOptions opts;
    opts.size = 32ULL * 1024 * 1024;
    opts.password = password;
    opts.storage_mode = oifs::StorageMode::Mmap;
    opts.journaled = true;

    oifs::FileSystem fs = oifs::FileSystem::open_with_options(path, opts);
    (void)fs.create_file_with_data(
        "raw_a.bin",
        oifs::ByteView(reinterpret_cast<const uint8_t*>(exp.raw_a.data()),
                       exp.raw_a.size() * sizeof(uint64_t)),
        oifs::WritePolicy::Raw,
        0);
    (void)fs.create_file_with_data(
        "raw_b.bin",
        oifs::ByteView(reinterpret_cast<const uint8_t*>(exp.raw_b.data()),
                       exp.raw_b.size() * sizeof(uint64_t)),
        oifs::WritePolicy::Raw,
        0);
    (void)fs.create_file_with_data(
        "stream.zst",
        oifs::ByteView(exp.stream_data),
        oifs::WritePolicy::Stream,
        3);
    (void)fs.create_file_with_data(
        "seekable.zst",
        oifs::ByteView(exp.seek_data),
        oifs::WritePolicy::Seekable64K,
        3);
    fs.flush();
}

static void run_ro_worker_ops_on_fs(
    const oifs::FileSystem& fs,
    bool expect_pinned_raw,
    const MpDatasetExpectations& exp,
    int proc_id,
    int thread_id,
    int iterations
) {
    const uint64_t ino_raw_a = fs.resolve_path("raw_a.bin");
    const uint64_t ino_raw_b = fs.resolve_path("raw_b.bin");
    const uint64_t ino_stream = fs.resolve_path("stream.zst");
    const uint64_t ino_seek = fs.resolve_path("seekable.zst");

    oifs::PinnableSlice slice;
    std::vector<uint8_t> scratch_stream(exp.stream_data.size(), 0);
    std::vector<uint8_t> scratch_seek(16384, 0);
    std::vector<uint8_t> async_buf(4096, 0);

    std::vector<uint8_t> b_raw(4096, 0);
    std::vector<uint8_t> b_stream(4096, 0);
    std::vector<uint8_t> b_seek(8192, 0);

    for (int it = 0; it < iterations; ++it) {
        const int op = (it + proc_id + thread_id) % 5;

        if (op == 0) {
            // ReadSession + FileReadHandle + PinnableSlice on raw contiguous multi-block files
            oifs::ReadSession sess = fs.begin_read();
            oifs::FileReadHandle fh_a = sess.open_inode(ino_raw_a);
            OIFS_CHECK(fh_a.read_pinnable(0, exp.raw_a.size() * sizeof(uint64_t), slice) ==
                       exp.raw_a.size() * sizeof(uint64_t));
            OIFS_CHECK(slice.is_pinned() == expect_pinned_raw);
            auto sp_a = slice.as_span<const uint64_t>();
            OIFS_CHECK(sp_a.size() == exp.raw_a.size());
            OIFS_CHECK(sum_u64_span(sp_a) == exp.raw_a_sum);

            oifs::FileReadHandle fh_b = sess.open_file("raw_b.bin");
            OIFS_CHECK(fh_b.inode_id() == ino_raw_b);
            // Direct-block contiguous range [0, 40 KiB) must be 0-copy pinned on plain Mmap
            constexpr size_t kDirectBytes = 10 * 4096;
            constexpr size_t kIndirectBytes =
                MpDatasetExpectations::kRawBCount * sizeof(uint64_t) - kDirectBytes;
            OIFS_CHECK(fh_b.read_pinnable(0, kDirectBytes, slice) == kDirectBytes);
            OIFS_CHECK(slice.is_pinned() == expect_pinned_raw);
            uint64_t sum_direct = sum_u64_span(slice.as_span<const uint64_t>());

            // Single-indirect contiguous range [40 KiB, 64 KiB) must also be 0-copy pinned on plain Mmap
            OIFS_CHECK(fh_b.read_pinnable(kDirectBytes, kIndirectBytes, slice) == kIndirectBytes);
            OIFS_CHECK(slice.is_pinned() == expect_pinned_raw);
            uint64_t sum_indirect = sum_u64_span(slice.as_span<const uint64_t>());
            OIFS_CHECK(sum_direct + sum_indirect == exp.raw_b_sum);

            // Full 64 KiB read works seamlessly (pinned or automatic 1-copy across indirect boundary)
            OIFS_CHECK(fh_b.read_pinnable(0, exp.raw_b.size() * sizeof(uint64_t), slice) ==
                       exp.raw_b.size() * sizeof(uint64_t));
            auto sp_b = slice.as_span<const uint64_t>();
            OIFS_CHECK(sp_b.size() == exp.raw_b.size());
            OIFS_CHECK(sum_u64_span(sp_b) == exp.raw_b_sum);
        } else if (op == 1) {
            // read_at on Zstd stream-compressed & Seekable 64K files (including cross-chunk boundary)
            oifs::ReadSession sess = fs.begin_read();
            oifs::FileReadHandle fh_stream = sess.open_inode(ino_stream);
            OIFS_CHECK(fh_stream.read_at(0, oifs::MutableByteSpan(scratch_stream)) ==
                       exp.stream_data.size());
            OIFS_CHECK(std::memcmp(scratch_stream.data(), exp.stream_data.data(),
                                   exp.stream_data.size()) == 0);

            const uint64_t seek_off = (it % 2 == 0) ? 60000ULL : 126000ULL;
            oifs::FileReadHandle fh_seek = sess.open_inode(ino_seek);
            OIFS_CHECK(fh_seek.read_at(seek_off, oifs::MutableByteSpan(scratch_seek)) ==
                       scratch_seek.size());
            OIFS_CHECK(std::memcmp(scratch_seek.data(), exp.seek_data.data() + seek_off,
                                   scratch_seek.size()) == 0);
        } else if (op == 2) {
            // Scatter-gather read_at_batch across raw + stream + seekable inodes
            std::vector<OIFSReadBatchItem> items(3);
            items[0] = {ino_raw_a, 4096ULL, b_raw.data(), b_raw.size(), -1};
            items[1] = {ino_stream, 8192ULL, b_stream.data(), b_stream.size(), -1};
            items[2] = {ino_seek, 63000ULL, b_seek.data(), b_seek.size(), -1};
            fs.read_at_batch(items);
            OIFS_CHECK(items[0].result == static_cast<int64_t>(b_raw.size()));
            OIFS_CHECK(items[1].result == static_cast<int64_t>(b_stream.size()));
            OIFS_CHECK(items[2].result == static_cast<int64_t>(b_seek.size()));
            OIFS_CHECK(std::memcmp(b_raw.data(),
                                   reinterpret_cast<const uint8_t*>(exp.raw_a.data()) + 4096,
                                   b_raw.size()) == 0);
            OIFS_CHECK(std::memcmp(b_stream.data(), exp.stream_data.data() + 8192,
                                   b_stream.size()) == 0);
            OIFS_CHECK(std::memcmp(b_seek.data(), exp.seek_data.data() + 63000,
                                   b_seek.size()) == 0);
        } else if (op == 3) {
            // AsyncFuture read_at_async on raw and Seekable 64K
            const uint64_t raw_off = static_cast<uint64_t>((it % 7) * 4096);
            oifs::AsyncFuture f_raw = fs.read_at_async(
                ino_raw_a, raw_off, oifs::MutableByteSpan(async_buf));
            OIFS_CHECK(f_raw.wait() == static_cast<int64_t>(async_buf.size()));
            OIFS_CHECK(std::memcmp(async_buf.data(),
                                   reinterpret_cast<const uint8_t*>(exp.raw_a.data()) + raw_off,
                                   async_buf.size()) == 0);

            const uint64_t seek_off = 65536ULL + static_cast<uint64_t>((it % 8) * 1024);
            oifs::AsyncFuture f_seek = fs.read_at_async(
                ino_seek, seek_off, oifs::MutableByteSpan(async_buf));
            OIFS_CHECK(f_seek.wait() == static_cast<int64_t>(async_buf.size()));
            OIFS_CHECK(std::memcmp(async_buf.data(), exp.seek_data.data() + seek_off,
                                   async_buf.size()) == 0);
        } else {
            // Lambda callback read_at_async_cb
            std::mutex cb_mtx;
            std::condition_variable cb_cv;
            bool done = false;
            int64_t got_status = -1;
            std::vector<uint8_t> got_bytes;

            const uint64_t off = (it % 2 == 0) ? 12288ULL : 70000ULL;
            const uint64_t target_ino = (it % 2 == 0) ? ino_stream : ino_seek;
            const auto& expected_vec = (it % 2 == 0) ? exp.stream_data : exp.seek_data;

            fs.read_at_async_cb(
                target_ino,
                off,
                4096,
                [&](int64_t status, oifs::ByteView view) {
                    std::lock_guard<std::mutex> lk(cb_mtx);
                    got_status = status;
                    if (status > 0) {
                        got_bytes = view.to_vector();
                    }
                    done = true;
                    cb_cv.notify_one();
                });

            std::unique_lock<std::mutex> lk(cb_mtx);
            cb_cv.wait(lk, [&] { return done; });
            OIFS_CHECK(got_status == 4096);
            OIFS_CHECK(got_bytes.size() == 4096);
            OIFS_CHECK(std::memcmp(got_bytes.data(), expected_vec.data() + off, 4096) == 0);
        }
    }
}

static int run_ro_worker_child(
    int proc_id,
    const std::string& plain_img,
    const std::string& enc_img,
    const std::string& enc_pwd,
    int ready_fd,
    int go_fd
) {
    // Mix StorageMode::Mmap and StorageMode::FileIo across processes AND within every process
    const oifs::StorageMode plain_mode =
        (proc_id % 2 == 0) ? oifs::StorageMode::Mmap : oifs::StorageMode::FileIo;
    const oifs::StorageMode enc_mode =
        (proc_id % 2 == 0) ? oifs::StorageMode::FileIo : oifs::StorageMode::Mmap;

    oifs::FileSystem plain_ro = oifs::FileSystem::open_read_only(plain_img, "", plain_mode);
    oifs::FileSystem enc_ro = oifs::FileSystem::open_read_only(enc_img, enc_pwd, enc_mode);
    OIFS_CHECK(plain_ro.is_read_only());
    OIFS_CHECK(enc_ro.is_read_only());

    // Barrier 1: Signal parent that this OS process holds F_RDLCK on both images
    char sig_ready = 'R';
    write_all_fd(ready_fd, &sig_ready, 1);
    char sig_go = 0;
    read_all_fd(go_fd, &sig_go, 1);
    OIFS_CHECK(sig_go == 'G');

    MpDatasetExpectations plain_exp(0);
    MpDatasetExpectations enc_exp(100);

    // Spawn 4 std::threads per process (16 threads total across 4 OS processes),
    // each performing 220 concurrent operations (110 on plain + 110 on encrypted = 220 ops/thread).
    constexpr int kThreadsPerProc = 4;
    constexpr int kOpsPerFsPerThread = 110;
    std::vector<std::thread> workers;
    workers.reserve(kThreadsPerProc);

    for (int t = 0; t < kThreadsPerProc; ++t) {
        workers.emplace_back([&, t]() {
            run_ro_worker_ops_on_fs(
                plain_ro,
                plain_mode == oifs::StorageMode::Mmap,
                plain_exp,
                proc_id,
                t,
                kOpsPerFsPerThread);
            run_ro_worker_ops_on_fs(
                enc_ro,
                false,
                enc_exp,
                proc_id,
                t,
                kOpsPerFsPerThread);
        });
    }

    for (auto& th : workers) {
        th.join();
    }

    // Barrier 2: Keep F_RDLCK held until all 4 child processes finish their 4-thread loops
    char sig_done = 'D';
    write_all_fd(ready_fd, &sig_done, 1);
    char sig_exit = 0;
    read_all_fd(go_fd, &sig_exit, 1);
    OIFS_CHECK(sig_exit == 'X');

    return 0;
}

static int run_rw_probe_child(
    const std::string& plain_img,
    const std::string& enc_img,
    const std::string& enc_pwd
) {
    bool plain_blocked = false;
    try {
        (void)oifs::FileSystem::open(plain_img, 0);
    } catch (const oifs::Error&) {
        plain_blocked = true;
    }
    OIFS_CHECK(plain_blocked);

    bool enc_blocked = false;
    try {
        (void)oifs::FileSystem::open_with_options(
            enc_img, 0, enc_pwd, oifs::StorageMode::Mmap, true);
    } catch (const oifs::Error&) {
        enc_blocked = true;
    }
    OIFS_CHECK(enc_blocked);
    return 0;
}

static void clear_cloexec(int fd) {
    int flags = ::fcntl(fd, F_GETFD);
    OIFS_CHECK(flags >= 0);
    OIFS_CHECK(::fcntl(fd, F_SETFD, flags & ~FD_CLOEXEC) == 0);
}
#endif

static void run_scenario_b_readers_and_group_commit_writers(
    const std::string& img_path,
    oifs::StorageMode mode
) {
    struct CreatedRecord {
        std::string path;
        uint64_t inode_id = 0;
        std::vector<uint8_t> payload;
    };

    std::vector<uint8_t> seed_raw(16384);
    for (size_t i = 0; i < seed_raw.size(); ++i) {
        seed_raw[i] = static_cast<uint8_t>((i * 29 + 11) & 0xFF);
    }
    std::vector<uint8_t> seed_stream = make_compressible_pattern(16384, 5);
    std::vector<uint8_t> seed_seek = make_compressible_pattern(131072, 13);

    std::mutex registry_mtx;
    std::vector<CreatedRecord> created_files;

    {
        oifs::FileSystem fs = oifs::FileSystem::open_with_options(
            img_path, 64ULL * 1024 * 1024, "", mode, true);
        fs.set_durability_mode(oifs::DurabilityMode::Strict);
        if (mode == oifs::StorageMode::FileIo) {
            fs.set_file_io_cache_bytes(16ULL * 1024 * 1024);
        }

        uint64_t seed_raw_ino = fs.create_file_with_data(
            "seed_raw.bin", oifs::ByteView(seed_raw), oifs::WritePolicy::Raw, 0);
        uint64_t seed_stream_ino = fs.create_file_with_data(
            "seed_stream.zst", oifs::ByteView(seed_stream), oifs::WritePolicy::Stream, 3);
        uint64_t seed_seek_ino = fs.create_file_with_data(
            "seed_seek.zst", oifs::ByteView(seed_seek), oifs::WritePolicy::Seekable64K, 3);

        created_files.push_back({"seed_raw.bin", seed_raw_ino, seed_raw});
        created_files.push_back({"seed_stream.zst", seed_stream_ino, seed_stream});
        created_files.push_back({"seed_seek.zst", seed_seek_ino, seed_seek});

        constexpr int kReaders = 4;
        constexpr int kWriters = 4;
        std::atomic<int> writers_remaining{kWriters};

        auto make_writer_payload = [](int writer_id, int seq) {
            const size_t len = 1024 + static_cast<size_t>((writer_id * 7 + seq * 13) % 4096);
            std::vector<uint8_t> data(len);
            for (size_t i = 0; i < len; ++i) {
                data[i] = static_cast<uint8_t>((writer_id * 53 + seq * 17 + i * 7) & 0xFF);
            }
            return data;
        };

        std::vector<std::thread> writer_threads;
        writer_threads.reserve(kWriters);
        for (int w = 0; w < kWriters; ++w) {
            writer_threads.emplace_back([&, w]() {
                // 1. 4 synchronous create_file_with_data calls (Raw, Stream, Seekable64K, Raw)
                for (int i = 0; i < 4; ++i) {
                    std::string fname = "w" + std::to_string(w) + "_sync_" + std::to_string(i) + ".dat";
                    std::vector<uint8_t> data = make_writer_payload(w, i);
                    oifs::WritePolicy pol = (i == 1) ? oifs::WritePolicy::Stream
                                          : (i == 2) ? oifs::WritePolicy::Seekable64K
                                                     : oifs::WritePolicy::Raw;
                    uint64_t ino = fs.create_file_with_data(fname, oifs::ByteView(data), pol, 3);
                    OIFS_CHECK(ino > 0);
                    std::lock_guard<std::mutex> lk(registry_mtx);
                    created_files.push_back({std::move(fname), ino, std::move(data)});
                }

                // 2. 4 concurrent Async Group-Commit create_file_with_data_async calls under Strict durability
                std::vector<std::string> async_names;
                std::vector<std::vector<uint8_t>> async_payloads;
                std::vector<oifs::AsyncFuture> futs;
                async_names.reserve(4);
                async_payloads.reserve(4);
                futs.reserve(4);
                for (int i = 0; i < 4; ++i) {
                    async_names.push_back(
                        "w" + std::to_string(w) + "_async_" + std::to_string(i) + ".dat");
                    async_payloads.push_back(make_writer_payload(w, 10 + i));
                    futs.push_back(fs.create_file_with_data_async(
                        async_names.back(),
                        oifs::ByteView(async_payloads.back()),
                        (i % 2 == 0) ? oifs::WritePolicy::Raw : oifs::WritePolicy::Stream,
                        3));
                }
                for (int i = 0; i < 4; ++i) {
                    int64_t id = futs[i].wait();
                    OIFS_CHECK(id > 0);
                    std::lock_guard<std::mutex> lk(registry_mtx);
                    created_files.push_back(
                        {std::move(async_names[i]), static_cast<uint64_t>(id),
                         std::move(async_payloads[i])});
                }

                // 3. 4 lambda-callback create_file_with_data_async_cb calls
                std::mutex cb_mtx;
                std::condition_variable cb_cv;
                int cb_completed = 0;
                std::vector<int64_t> cb_inodes(4, -1);
                std::vector<std::string> cb_names(4);
                std::vector<std::vector<uint8_t>> cb_payloads(4);

                for (int i = 0; i < 4; ++i) {
                    cb_names[i] = "w" + std::to_string(w) + "_cb_" + std::to_string(i) + ".dat";
                    cb_payloads[i] = make_writer_payload(w, 20 + i);
                    fs.create_file_with_data_async_cb(
                        cb_names[i],
                        oifs::ByteView(cb_payloads[i]),
                        oifs::WritePolicy::Raw,
                        0,
                        [&, i](int64_t status_or_inode) {
                            std::lock_guard<std::mutex> lk(cb_mtx);
                            cb_inodes[i] = status_or_inode;
                            ++cb_completed;
                            cb_cv.notify_one();
                        });
                }
                {
                    std::unique_lock<std::mutex> lk(cb_mtx);
                    cb_cv.wait(lk, [&] { return cb_completed == 4; });
                }
                for (int i = 0; i < 4; ++i) {
                    OIFS_CHECK(cb_inodes[i] > 0);
                    std::lock_guard<std::mutex> lk(registry_mtx);
                    created_files.push_back(
                        {std::move(cb_names[i]), static_cast<uint64_t>(cb_inodes[i]),
                         std::move(cb_payloads[i])});
                }

                writers_remaining.fetch_sub(1, std::memory_order_release);
            });
        }

        std::vector<std::thread> reader_threads;
        reader_threads.reserve(kReaders);
        for (int r = 0; r < kReaders; ++r) {
            reader_threads.emplace_back([&, r]() {
                oifs::PinnableSlice slice;
                int iter = 0;
                while (writers_remaining.load(std::memory_order_acquire) > 0 || iter < 60) {
                    CreatedRecord target;
                    {
                        std::lock_guard<std::mutex> lk(registry_mtx);
                        OIFS_CHECK(!created_files.empty());
                        const size_t idx =
                            static_cast<size_t>(iter * 7 + r * 3) % created_files.size();
                        target = created_files[idx];
                    }

                    const int op = (iter + r) % 4;
                    if (op == 0) {
                        // ReadSession + FileReadHandle + PinnableSlice
                        oifs::ReadSession sess = fs.begin_read();
                        oifs::FileReadHandle fh = sess.open_inode(target.inode_id);
                        OIFS_CHECK(fh.size() == target.payload.size());
                        OIFS_CHECK(fh.read_pinnable(0, target.payload.size(), slice) ==
                                   target.payload.size());
                        OIFS_CHECK(std::memcmp(slice.data(), target.payload.data(),
                                               target.payload.size()) == 0);
                        slice.reset();
                    } else if (op == 1) {
                        // read_at_batch across seed_raw, seed_stream, and target
                        std::vector<uint8_t> b0(4096, 0);
                        std::vector<uint8_t> b1(4096, 0);
                        std::vector<uint8_t> b2(target.payload.size(), 0);
                        std::vector<OIFSReadBatchItem> items = {
                            {seed_raw_ino, 0, b0.data(), b0.size(), -1},
                            {seed_stream_ino, 4096, b1.data(), b1.size(), -1},
                            {target.inode_id, 0, b2.data(), b2.size(), -1},
                        };
                        fs.read_at_batch(items);
                        OIFS_CHECK(items[0].result == static_cast<int64_t>(b0.size()));
                        OIFS_CHECK(items[1].result == static_cast<int64_t>(b1.size()));
                        OIFS_CHECK(items[2].result == static_cast<int64_t>(b2.size()));
                        OIFS_CHECK(std::memcmp(b0.data(), seed_raw.data(), b0.size()) == 0);
                        OIFS_CHECK(std::memcmp(b1.data(), seed_stream.data() + 4096, b1.size()) == 0);
                        OIFS_CHECK(std::memcmp(b2.data(), target.payload.data(), b2.size()) == 0);
                    } else if (op == 2) {
                        // read_at_async
                        std::vector<uint8_t> abuf(target.payload.size(), 0);
                        oifs::AsyncFuture fut = fs.read_at_async(
                            target.inode_id, 0, oifs::MutableByteSpan(abuf));
                        OIFS_CHECK(fut.wait() == static_cast<int64_t>(target.payload.size()));
                        OIFS_CHECK(std::memcmp(abuf.data(), target.payload.data(),
                                               target.payload.size()) == 0);
                    } else {
                        // read_at_async_cb
                        std::mutex cb_mtx;
                        std::condition_variable cb_cv;
                        bool done = false;
                        int64_t st = -1;
                        std::vector<uint8_t> got;
                        fs.read_at_async_cb(
                            target.inode_id,
                            0,
                            target.payload.size(),
                            [&](int64_t status, oifs::ByteView view) {
                                std::lock_guard<std::mutex> lk(cb_mtx);
                                st = status;
                                if (status >= 0) {
                                    got = view.to_vector();
                                }
                                done = true;
                                cb_cv.notify_one();
                            });
                        std::unique_lock<std::mutex> lk(cb_mtx);
                        cb_cv.wait(lk, [&] { return done; });
                        OIFS_CHECK(st == static_cast<int64_t>(target.payload.size()));
                        OIFS_CHECK(got == target.payload);
                    }
                    ++iter;
                }
            });
        }

        for (auto& t : writer_threads) {
            t.join();
        }
        for (auto& t : reader_threads) {
            t.join();
        }
        fs.flush();
    }

    // Reopen read-only and verify every single created file's contents byte-for-byte
    OIFS_CHECK(created_files.size() == 3 + 4 * 12);
    {
        oifs::FileSystem ro = oifs::FileSystem::open_read_only(img_path, "", mode);
        OIFS_CHECK(ro.is_read_only());
        oifs::ReadSession sess = ro.begin_read();
        oifs::PinnableSlice slice;
        for (const auto& rec : created_files) {
            OIFS_CHECK(ro.resolve_path(rec.path) == rec.inode_id);
            oifs::FileReadHandle fh = sess.open_file(rec.path);
            OIFS_CHECK(fh.inode_id() == rec.inode_id);
            OIFS_CHECK(fh.size() == rec.payload.size());
            OIFS_CHECK(fh.read_pinnable(0, rec.payload.size(), slice) == rec.payload.size());
            OIFS_CHECK(std::memcmp(slice.data(), rec.payload.data(), rec.payload.size()) == 0);
        }
    }
}

static void test_multi_process_and_multi_thread(
    const char* self_exe,
    const std::string& mp_plain_img,
    const std::string& mp_enc_img,
    const std::string& mt_mmap_img,
    const std::string& mt_fio_img
) {
#ifndef _WIN32
    // --- Scenario A: 4 OS child processes x 4 std::threads holding open_read_only (F_RDLCK) ---
    const std::string enc_pwd = "mp_mt_enc_pwd_2026";
    {
        MpDatasetExpectations plain_exp(0);
        MpDatasetExpectations enc_exp(100);
        populate_mp_image(mp_plain_img, "", plain_exp);
        populate_mp_image(mp_enc_img, enc_pwd, enc_exp);
    }

    constexpr int kNumProcs = 4;
    int ready_pipes[kNumProcs][2];
    int go_pipes[kNumProcs][2];
    pid_t child_pids[kNumProcs];

    for (int p = 0; p < kNumProcs; ++p) {
        OIFS_CHECK(::pipe(ready_pipes[p]) == 0);
        OIFS_CHECK(::pipe(go_pipes[p]) == 0);
        clear_cloexec(ready_pipes[p][1]);
        clear_cloexec(go_pipes[p][0]);
    }

    for (int p = 0; p < kNumProcs; ++p) {
        pid_t pid = ::fork();
        OIFS_CHECK(pid >= 0);
        if (pid == 0) {
            // Child closes all pipe ends except its own ready write-end and go read-end
            for (int j = 0; j < kNumProcs; ++j) {
                ::close(ready_pipes[j][0]);
                ::close(go_pipes[j][1]);
                if (j != p) {
                    ::close(ready_pipes[j][1]);
                    ::close(go_pipes[j][0]);
                }
            }
            std::string arg_mode = "--ro-worker";
            std::string arg_id = std::to_string(p);
            std::string arg_plain = mp_plain_img;
            std::string arg_enc = mp_enc_img;
            std::string arg_pwd = enc_pwd;
            std::string arg_ready = std::to_string(ready_pipes[p][1]);
            std::string arg_go = std::to_string(go_pipes[p][0]);
            char* argv_exec[] = {
                const_cast<char*>(self_exe),
                const_cast<char*>(arg_mode.c_str()),
                const_cast<char*>(arg_id.c_str()),
                const_cast<char*>(arg_plain.c_str()),
                const_cast<char*>(arg_enc.c_str()),
                const_cast<char*>(arg_pwd.c_str()),
                const_cast<char*>(arg_ready.c_str()),
                const_cast<char*>(arg_go.c_str()),
                nullptr
            };
            ::execv(self_exe, argv_exec);
            ::_exit(127);
        }
        child_pids[p] = pid;
        ::close(ready_pipes[p][1]);
        ::close(go_pipes[p][0]);
    }

    // Wait for Barrier 1: all 4 child processes hold F_RDLCK on both plain and encrypted images
    for (int p = 0; p < kNumProcs; ++p) {
        char ch = 0;
        read_all_fd(ready_pipes[p][0], &ch, 1);
        OIFS_CHECK(ch == 'R');
    }

    // While all 4 child processes hold F_RDLCK, spawn a separate OS probe process attempting RW F_WRLCK
    {
        pid_t probe_pid = ::fork();
        OIFS_CHECK(probe_pid >= 0);
        if (probe_pid == 0) {
            for (int j = 0; j < kNumProcs; ++j) {
                ::close(ready_pipes[j][0]);
                ::close(go_pipes[j][1]);
            }
            std::string arg_mode = "--rw-probe";
            std::string arg_plain = mp_plain_img;
            std::string arg_enc = mp_enc_img;
            std::string arg_pwd = enc_pwd;
            char* argv_exec[] = {
                const_cast<char*>(self_exe),
                const_cast<char*>(arg_mode.c_str()),
                const_cast<char*>(arg_plain.c_str()),
                const_cast<char*>(arg_enc.c_str()),
                const_cast<char*>(arg_pwd.c_str()),
                nullptr
            };
            ::execv(self_exe, argv_exec);
            ::_exit(127);
        }
        int probe_status = 0;
        OIFS_CHECK(::waitpid(probe_pid, &probe_status, 0) == probe_pid);
        OIFS_CHECK(WIFEXITED(probe_status) && WEXITSTATUS(probe_status) == 0);
    }

    // Release Barrier 1 so all 16 threads across 4 processes execute concurrently
    for (int p = 0; p < kNumProcs; ++p) {
        char go = 'G';
        write_all_fd(go_pipes[p][1], &go, 1);
    }

    // Wait for Barrier 2: all 4 processes finished their 4-thread loops while still holding F_RDLCK
    for (int p = 0; p < kNumProcs; ++p) {
        char done = 0;
        read_all_fd(ready_pipes[p][0], &done, 1);
        OIFS_CHECK(done == 'D');
    }

    // Release Barrier 2 and reap all 4 child processes
    for (int p = 0; p < kNumProcs; ++p) {
        char ex = 'X';
        write_all_fd(go_pipes[p][1], &ex, 1);
        ::close(ready_pipes[p][0]);
        ::close(go_pipes[p][1]);
    }
    for (int p = 0; p < kNumProcs; ++p) {
        int status = 0;
        OIFS_CHECK(::waitpid(child_pids[p], &status, 0) == child_pids[p]);
        OIFS_CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    }

    // After all read-only children exit, RW open (F_WRLCK) must immediately succeed again
    {
        oifs::FileSystem rw_plain = oifs::FileSystem::open(mp_plain_img, 0);
        OIFS_CHECK(!rw_plain.is_read_only());
        oifs::FileSystem rw_enc = oifs::FileSystem::open_with_options(
            mp_enc_img, 0, enc_pwd, oifs::StorageMode::Mmap, true);
        OIFS_CHECK(!rw_enc.is_read_only());
    }
#else
    (void)self_exe;
    (void)mp_plain_img;
    (void)mp_enc_img;
#endif

    // --- Scenario B: 4 C++ Reader Threads + 4 C++ Async Group-Commit Writer Threads ---
    run_scenario_b_readers_and_group_commit_writers(mt_mmap_img, oifs::StorageMode::Mmap);
    run_scenario_b_readers_and_group_commit_writers(mt_fio_img, oifs::StorageMode::FileIo);
}

static void run_micro_benchmark(const std::string& bench_path) {
    oifs::FileSystem fs = oifs::FileSystem::open_with_options(
        bench_path, 32ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, false);

    constexpr size_t kFileSize = 64 * 1024;
    constexpr size_t kReadLen = 4096;
    constexpr size_t kIterations = 20000;

    std::vector<uint8_t> data(kFileSize);
    for (size_t i = 0; i < kFileSize; ++i) {
        data[i] = static_cast<uint8_t>((i * 31 + 7) & 0xFF);
    }
    fs.mkdir("bench");
    fs.create_file_with_data(
        "bench/hot_asset.bin", oifs::ByteView(data), oifs::WritePolicy::Raw, 0);

    std::vector<uint8_t> scratch(kReadLen, 0);
    uint64_t checksum = 0;

    // 1. Legacy C path-based oifs_read_at
    auto t0 = std::chrono::steady_clock::now();
    for (size_t i = 0; i < kIterations; ++i) {
        uint64_t off = (i * 4096) % (kFileSize - kReadLen);
        int64_t n = oifs_read_at(
            fs.raw(), "bench/hot_asset.bin", off, scratch.data(), kReadLen);
        OIFS_CHECK(n == static_cast<int64_t>(kReadLen));
        checksum += scratch[0];
    }
    auto t1 = std::chrono::steady_clock::now();

    // 2. C++ Pinned FileReadHandle::read_at (1-Copy, 0 path resolution, 0 RwLock per read)
    oifs::ReadSession session = fs.begin_read();
    oifs::FileReadHandle fh = session.open_file("bench/hot_asset.bin");

    auto t2 = std::chrono::steady_clock::now();
    for (size_t i = 0; i < kIterations; ++i) {
        uint64_t off = (i * 4096) % (kFileSize - kReadLen);
        size_t n = fh.read_at(off, oifs::MutableByteSpan(scratch));
        OIFS_CHECK(n == kReadLen);
        checksum += scratch[0];
    }
    auto t3 = std::chrono::steady_clock::now();

    // 3. C++ 0-Copy PinnableSlice (0-Copy, 0 path resolution, 0 RwLock per read)
    oifs::PinnableSlice slice;
    auto t4 = std::chrono::steady_clock::now();
    for (size_t i = 0; i < kIterations; ++i) {
        uint64_t off = (i * 4096) % (kFileSize - kReadLen);
        size_t n = fh.read_pinnable(off, kReadLen, slice);
        OIFS_CHECK(n == kReadLen);
        OIFS_CHECK(slice.is_pinned());
        checksum += slice[0];
    }
    auto t5 = std::chrono::steady_clock::now();

    double legacy_ns = std::chrono::duration<double, std::nano>(t1 - t0).count() / kIterations;
    double handle_ns = std::chrono::duration<double, std::nano>(t3 - t2).count() / kIterations;
    double zerocopy_ns = std::chrono::duration<double, std::nano>(t5 - t4).count() / kIterations;

    double legacy_gbps = (static_cast<double>(kReadLen) / legacy_ns);
    double handle_gbps = (static_cast<double>(kReadLen) / handle_ns);
    double zerocopy_gbps = (static_cast<double>(kReadLen) / zerocopy_ns);

    std::printf(
        "\n=== OIFS C++ Native 4 KiB Read Micro-Benchmark (%zu iterations, checksum=%llu) ===\n"
        "  1. Legacy C oifs_read_at (path + RwLock + memcpy): %8.1f ns/op  (%6.2f GB/s)\n"
        "  2. C++ FileReadHandle::read_at (pinned + memcpy) : %8.1f ns/op  (%6.2f GB/s, %5.2fx speedup)\n"
        "  3. C++ 0-Copy PinnableSlice (pinned mmap view)   : %8.1f ns/op  (%6.2f GB/s, %5.2fx speedup)\n",
        kIterations,
        static_cast<unsigned long long>(checksum),
        legacy_ns, legacy_gbps,
        handle_ns, handle_gbps, legacy_ns / handle_ns,
        zerocopy_ns, zerocopy_gbps, legacy_ns / zerocopy_ns
    );
}

static void append_u32_le(std::vector<uint8_t>& buf, uint32_t v) {
    uint32_t le = OIFS_TO_LE32(v);
    const uint8_t* p = reinterpret_cast<const uint8_t*>(&le);
    buf.insert(buf.end(), p, p + 4);
}

static void append_u64_le(std::vector<uint8_t>& buf, uint64_t v) {
    uint64_t le = OIFS_TO_LE64(v);
    const uint8_t* p = reinterpret_cast<const uint8_t*>(&le);
    buf.insert(buf.end(), p, p + 8);
}

static uint32_t read_u32_le_at(const std::vector<uint8_t>& buf, size_t off) {
    uint32_t v = 0;
    std::memcpy(&v, buf.data() + off, sizeof(v));
    return OIFS_FROM_LE32(v);
}

static int32_t read_i32_le_at(const std::vector<uint8_t>& buf, size_t off) {
    uint32_t u = read_u32_le_at(buf, off);
    int32_t v = 0;
    std::memcpy(&v, &u, sizeof(v));
    return v;
}

static uint64_t read_u64_le_at(const std::vector<uint8_t>& buf, size_t off) {
    uint64_t v = 0;
    std::memcpy(&v, buf.data() + off, sizeof(v));
    return OIFS_FROM_LE64(v);
}

static std::vector<uint8_t> build_fuse_frame(
    uint32_t opcode,
    uint64_t unique,
    uint64_t nodeid,
    const std::vector<uint8_t>& payload
) {
    std::vector<uint8_t> frame;
    frame.reserve(40 + payload.size());
    append_u32_le(frame, static_cast<uint32_t>(40 + payload.size()));
    append_u32_le(frame, opcode);
    append_u64_le(frame, unique);
    append_u64_le(frame, nodeid);
    append_u32_le(frame, 0);  // uid
    append_u32_le(frame, 0);  // gid
    append_u32_le(frame, 0);  // pid
    append_u32_le(frame, 0);  // padding
    frame.insert(frame.end(), payload.begin(), payload.end());
    return frame;
}

static void test_fuse_kernel_frame_dispatch(const std::string& fuse_img_path) {
    constexpr uint32_t kFuseLookup = 1;
    constexpr uint32_t kFuseGetattr = 3;
    constexpr uint32_t kFuseUnlink = 10;
    constexpr uint32_t kFuseOpen = 14;
    constexpr uint32_t kFuseRead = 15;
    constexpr uint32_t kFuseWrite = 16;
    constexpr uint32_t kFuseRelease = 18;
    constexpr uint32_t kFuseInit = 26;
    constexpr uint32_t kFuseCreate = 35;
    constexpr uint64_t kFuseRootId = 1;

    uint64_t unique = 1;
    const std::string payload = "Hello FUSE Kernel ABI v7.28 from C++ Native API!";
    const std::string ro_seed_payload = "Persistent read-only FUSE payload!";

    // 1. Read-write FUSE lifecycle through oifs::FileSystem::fuse_dispatch_frame
    {
        oifs::FileSystem fs = oifs::FileSystem::open_with_options(
            fuse_img_path, 16ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, true);

        // Seed a file that will survive for the read-only FUSE test phase
        (void)fs.create_file_with_data(
            "ro_fuse.txt", oifs::ByteView(ro_seed_payload), oifs::WritePolicy::Raw, 0);

        // Invalid C FFI arguments return -1
        uint8_t tiny_resp[8] = {0};
        OIFS_CHECK(oifs_fuse_dispatch_frame(nullptr, tiny_resp, sizeof(tiny_resp), tiny_resp, sizeof(tiny_resp)) == -1);
        OIFS_CHECK(oifs_fuse_dispatch_frame(fs.raw(), nullptr, 40, tiny_resp, sizeof(tiny_resp)) == -1);
        OIFS_CHECK(oifs_fuse_dispatch_frame(fs.raw(), tiny_resp, 0, tiny_resp, sizeof(tiny_resp)) == -1);

        // FUSE_INIT (major=7, minor=28, max_readahead=131072, flags=0)
        std::vector<uint8_t> init_payload;
        append_u32_le(init_payload, 7);
        append_u32_le(init_payload, 28);
        append_u32_le(init_payload, 128 * 1024);
        append_u32_le(init_payload, 0);
        auto init_req = build_fuse_frame(kFuseInit, unique++, 0, init_payload);

        // Insufficient resp_cap must fail cleanly with -1 / oifs::Error
        OIFS_CHECK(oifs_fuse_dispatch_frame(
            fs.raw(), init_req.data(), init_req.size(), tiny_resp, sizeof(tiny_resp)) == -1);
        bool caught_small_buf = false;
        try {
            (void)fs.fuse_dispatch_frame(
                oifs::ByteView(init_req), oifs::MutableByteSpan(tiny_resp, sizeof(tiny_resp)));
        } catch (const oifs::Error&) {
            caught_small_buf = true;
        }
        OIFS_CHECK(caught_small_buf);

        auto init_resp = fs.fuse_dispatch_frame(oifs::ByteView(init_req));
        OIFS_CHECK(init_resp.size() == 16 + 64);
        OIFS_CHECK(read_u32_le_at(init_resp, 0) == static_cast<uint32_t>(init_resp.size()));
        OIFS_CHECK(read_i32_le_at(init_resp, 4) == 0);
        OIFS_CHECK(read_u32_le_at(init_resp, 16) == 7);   // major
        OIFS_CHECK(read_u32_le_at(init_resp, 20) == 28);  // minor
        OIFS_CHECK(read_u32_le_at(init_resp, 36) > 0);    // max_write

        // FUSE_CREATE ("hello_fuse.txt")
        std::vector<uint8_t> create_payload;
#ifndef _WIN32
        uint32_t create_flags = static_cast<uint32_t>(O_RDWR | O_CREAT);
#else
        uint32_t create_flags = 2;
#endif
        append_u32_le(create_payload, create_flags);
        append_u32_le(create_payload, 0100644);  // mode
        append_u32_le(create_payload, 0);        // umask
        append_u32_le(create_payload, 0);        // padding
        const std::string fname = "hello_fuse.txt";
        create_payload.insert(create_payload.end(), fname.begin(), fname.end());
        create_payload.push_back(0);

        auto create_req = build_fuse_frame(kFuseCreate, unique++, kFuseRootId, create_payload);
        auto create_resp = fs.fuse_dispatch_frame(oifs::ByteView(create_req));
        // 16 (FuseOutHeader) + 128 (FuseEntryOut) + 16 (FuseOpenOut)
        OIFS_CHECK(create_resp.size() == 16 + 128 + 16);
        OIFS_CHECK(read_i32_le_at(create_resp, 4) == 0);
        uint64_t created_nodeid = read_u64_le_at(create_resp, 16);
        uint64_t created_fh = read_u64_le_at(create_resp, 16 + 128);
        OIFS_CHECK(created_nodeid >= 2);
        OIFS_CHECK(created_fh != 0);

        // FUSE_WRITE using the persisted fh from FUSE_CREATE
        std::vector<uint8_t> write_payload;
        append_u64_le(write_payload, created_fh);
        append_u64_le(write_payload, 0);  // offset
        append_u32_le(write_payload, static_cast<uint32_t>(payload.size()));
        append_u32_le(write_payload, 0);  // write_flags
        append_u64_le(write_payload, 0);  // lock_owner
        append_u32_le(write_payload, 0);  // flags
        append_u32_le(write_payload, 0);  // padding
        write_payload.insert(write_payload.end(), payload.begin(), payload.end());

        auto write_req = build_fuse_frame(kFuseWrite, unique++, created_nodeid, write_payload);
        auto write_resp = fs.fuse_dispatch_frame(oifs::ByteView(write_req));
        OIFS_CHECK(write_resp.size() == 16 + 8);
        OIFS_CHECK(read_i32_le_at(write_resp, 4) == 0);
        OIFS_CHECK(read_u32_le_at(write_resp, 16) == static_cast<uint32_t>(payload.size()));

        // FUSE_READ using the persisted fh via MutableByteSpan overload
        std::vector<uint8_t> read_payload;
        append_u64_le(read_payload, created_fh);
        append_u64_le(read_payload, 0);     // offset
        append_u32_le(read_payload, 4096);  // size
        append_u32_le(read_payload, 0);     // read_flags
        append_u64_le(read_payload, 0);     // lock_owner
        append_u32_le(read_payload, 0);     // flags
        append_u32_le(read_payload, 0);     // padding

        auto read_req = build_fuse_frame(kFuseRead, unique++, created_nodeid, read_payload);
        std::vector<uint8_t> read_span_buf(4096, 0);
        size_t read_resp_len = fs.fuse_dispatch_frame(
            oifs::ByteView(read_req), oifs::MutableByteSpan(read_span_buf));
        OIFS_CHECK(read_resp_len == 16 + payload.size());
        read_span_buf.resize(read_resp_len);
        OIFS_CHECK(read_i32_le_at(read_span_buf, 4) == 0);
        std::string read_back(
            reinterpret_cast<const char*>(read_span_buf.data() + 16), payload.size());
        OIFS_CHECK(read_back == payload);

        // FUSE_GETATTR on created_nodeid
        auto getattr_req = build_fuse_frame(kFuseGetattr, unique++, created_nodeid, {});
        auto getattr_resp = fs.fuse_dispatch_frame(oifs::ByteView(getattr_req));
        // 16 (FuseOutHeader) + 104 (FuseAttrOut: 16 header + 88 FuseAttr)
        OIFS_CHECK(getattr_resp.size() == 16 + 104);
        OIFS_CHECK(read_i32_le_at(getattr_resp, 4) == 0);
        uint64_t attr_ino = read_u64_le_at(getattr_resp, 16 + 16);
        uint64_t attr_size = read_u64_le_at(getattr_resp, 16 + 16 + 8);
        OIFS_CHECK(attr_ino == created_nodeid);
        OIFS_CHECK(attr_size == payload.size());

        // FUSE_RELEASE
        std::vector<uint8_t> rel_payload;
        append_u64_le(rel_payload, created_fh);
        append_u32_le(rel_payload, 0);
        append_u32_le(rel_payload, 0);
        append_u64_le(rel_payload, 0);
        auto rel_req = build_fuse_frame(kFuseRelease, unique++, created_nodeid, rel_payload);
        auto rel_resp = fs.fuse_dispatch_frame(oifs::ByteView(rel_req));
        OIFS_CHECK(rel_resp.size() == 16);
        OIFS_CHECK(read_i32_le_at(rel_resp, 4) == 0);

        // FUSE_UNLINK ("hello_fuse.txt")
        std::vector<uint8_t> unlink_payload(fname.begin(), fname.end());
        unlink_payload.push_back(0);
        auto unlink_req = build_fuse_frame(kFuseUnlink, unique++, kFuseRootId, unlink_payload);
        auto unlink_resp = fs.fuse_dispatch_frame(oifs::ByteView(unlink_req));
        OIFS_CHECK(unlink_resp.size() == 16);
        OIFS_CHECK(read_i32_le_at(unlink_resp, 4) == 0);

        // Subsequent FUSE_LOOKUP must return -ENOENT
        auto lookup_req = build_fuse_frame(kFuseLookup, unique++, kFuseRootId, unlink_payload);
        auto lookup_resp = fs.fuse_dispatch_frame(oifs::ByteView(lookup_req));
        OIFS_CHECK(lookup_resp.size() == 16);
#ifndef _WIN32
        OIFS_CHECK(read_i32_le_at(lookup_resp, 4) == -ENOENT);
#else
        OIFS_CHECK(read_i32_le_at(lookup_resp, 4) < 0);
#endif
        fs.flush();
    }

    // 2. Read-only oifs::FileSystem: reads succeed, all mutating FUSE frames return -EROFS
    {
        oifs::FileSystem ro = oifs::FileSystem::open_read_only(
            fuse_img_path, "", oifs::StorageMode::Mmap);
        OIFS_CHECK(ro.is_read_only());

        // FUSE_INIT
        std::vector<uint8_t> init_payload;
        append_u32_le(init_payload, 7);
        append_u32_le(init_payload, 28);
        append_u32_le(init_payload, 64 * 1024);
        append_u32_le(init_payload, 0);
        auto init_resp = ro.fuse_dispatch_frame(
            oifs::ByteView(build_fuse_frame(kFuseInit, unique++, 0, init_payload)));
        OIFS_CHECK(read_i32_le_at(init_resp, 4) == 0);

        // FUSE_LOOKUP("ro_fuse.txt") + FUSE_OPEN(O_RDONLY) + FUSE_READ succeed
        const std::string ro_name = "ro_fuse.txt";
        std::vector<uint8_t> ro_name_payload(ro_name.begin(), ro_name.end());
        ro_name_payload.push_back(0);
        auto lookup_resp = ro.fuse_dispatch_frame(
            oifs::ByteView(build_fuse_frame(kFuseLookup, unique++, kFuseRootId, ro_name_payload)));
        OIFS_CHECK(read_i32_le_at(lookup_resp, 4) == 0);
        uint64_t ro_nodeid = read_u64_le_at(lookup_resp, 16);
        OIFS_CHECK(ro_nodeid >= 2);

        std::vector<uint8_t> open_ro_payload;
#ifndef _WIN32
        append_u32_le(open_ro_payload, static_cast<uint32_t>(O_RDONLY));
#else
        append_u32_le(open_ro_payload, 0);
#endif
        append_u32_le(open_ro_payload, 0);
        auto open_resp = ro.fuse_dispatch_frame(
            oifs::ByteView(build_fuse_frame(kFuseOpen, unique++, ro_nodeid, open_ro_payload)));
        OIFS_CHECK(read_i32_le_at(open_resp, 4) == 0);
        uint64_t ro_fh = read_u64_le_at(open_resp, 16);
        OIFS_CHECK(ro_fh != 0);

        std::vector<uint8_t> read_ro_payload;
        append_u64_le(read_ro_payload, ro_fh);
        append_u64_le(read_ro_payload, 0);
        append_u32_le(read_ro_payload, 4096);
        append_u32_le(read_ro_payload, 0);
        append_u64_le(read_ro_payload, 0);
        append_u32_le(read_ro_payload, 0);
        append_u32_le(read_ro_payload, 0);
        auto read_ro_resp = ro.fuse_dispatch_frame(
            oifs::ByteView(build_fuse_frame(kFuseRead, unique++, ro_nodeid, read_ro_payload)));
        OIFS_CHECK(read_i32_le_at(read_ro_resp, 4) == 0);
        OIFS_CHECK(read_ro_resp.size() == 16 + ro_seed_payload.size());
        std::string ro_text(
            reinterpret_cast<const char*>(read_ro_resp.data() + 16), ro_seed_payload.size());
        OIFS_CHECK(ro_text == ro_seed_payload);

        // Mutating FUSE_CREATE, FUSE_WRITE, and FUSE_UNLINK must all return -EROFS
        std::vector<uint8_t> ro_create_payload;
        append_u32_le(ro_create_payload, 2);
        append_u32_le(ro_create_payload, 0100644);
        append_u32_le(ro_create_payload, 0);
        append_u32_le(ro_create_payload, 0);
        const std::string blocked_name = "blocked_ro.txt";
        ro_create_payload.insert(ro_create_payload.end(), blocked_name.begin(), blocked_name.end());
        ro_create_payload.push_back(0);
        auto ro_create_resp = ro.fuse_dispatch_frame(
            oifs::ByteView(build_fuse_frame(kFuseCreate, unique++, kFuseRootId, ro_create_payload)));

        std::vector<uint8_t> ro_write_payload;
        append_u64_le(ro_write_payload, ro_fh);
        append_u64_le(ro_write_payload, 0);
        append_u32_le(ro_write_payload, 4);
        append_u32_le(ro_write_payload, 0);
        append_u64_le(ro_write_payload, 0);
        append_u32_le(ro_write_payload, 0);
        append_u32_le(ro_write_payload, 0);
        ro_write_payload.insert(ro_write_payload.end(), {'n', 'o', 'p', 'e'});
        auto ro_write_resp = ro.fuse_dispatch_frame(
            oifs::ByteView(build_fuse_frame(kFuseWrite, unique++, ro_nodeid, ro_write_payload)));

        auto ro_unlink_resp = ro.fuse_dispatch_frame(
            oifs::ByteView(build_fuse_frame(kFuseUnlink, unique++, kFuseRootId, ro_name_payload)));

#ifndef _WIN32
        OIFS_CHECK(read_i32_le_at(ro_create_resp, 4) == -EROFS);
        OIFS_CHECK(read_i32_le_at(ro_write_resp, 4) == -EROFS);
        OIFS_CHECK(read_i32_le_at(ro_unlink_resp, 4) == -EROFS);
#else
        OIFS_CHECK(read_i32_le_at(ro_create_resp, 4) < 0);
        OIFS_CHECK(read_i32_le_at(ro_write_resp, 4) < 0);
        OIFS_CHECK(read_i32_le_at(ro_unlink_resp, 4) < 0);
#endif
    }
}

static void test_rename_stat_exists_subdir_and_filters(const std::string& img_path) {
    // 5. Version & Loaded-Path Free Functions
    OIFS_CHECK(oifs::check_version() == OIFS_VERSION_COMPAT_OK);
    OIFS_CHECK(oifs::check_version(1, 0, 0) == OIFS_VERSION_COMPAT_WARN);
    OIFS_CHECK(oifs::check_version(99, 0, 0) == OIFS_VERSION_COMPAT_ERR);
    OIFS_CHECK(oifs::version_string() == OIFS_VERSION_STRING);
    std::string dylib_loc = oifs::loaded_path();
    OIFS_CHECK(!dylib_loc.empty());

    {
        oifs::FileSystem fs = oifs::FileSystem::open_with_options(
            img_path, 32ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, true);

        // 5b. IoBackend getter & setter
        OIFS_CHECK(fs.get_io_backend() == oifs::IoBackend::Mmap);
        oifs::IoBackend eff_pread = fs.set_io_backend(oifs::IoBackend::Pread);
        OIFS_CHECK(eff_pread == oifs::IoBackend::Pread);
        OIFS_CHECK(fs.get_io_backend() == oifs::IoBackend::Pread);
        // IoUring either succeeds (Linux with feature) or falls back to Pread cleanly
        oifs::IoBackend eff_uring = fs.set_io_backend(oifs::IoBackend::IoUring);
        OIFS_CHECK(eff_uring == oifs::IoBackend::IoUring || eff_uring == oifs::IoBackend::Pread);
        OIFS_CHECK(fs.set_io_backend(oifs::IoBackend::Mmap) == oifs::IoBackend::Mmap);

        // 3. Subdirectory listing, stat, exists
        OIFS_CHECK(!fs.exists("alpha"));
        fs.mkdir("alpha");
        fs.mkdir("alpha/sub");
        OIFS_CHECK(fs.exists("alpha"));
        OIFS_CHECK(fs.exists("alpha/sub"));

        oifs::FileStat dir_st = fs.stat("alpha/sub");
        OIFS_CHECK(dir_st.is_dir);
        OIFS_CHECK(!dir_st.encrypted);
        OIFS_CHECK(dir_st.inode_id > 0);

        std::string hello = "Hello subdirectory and rename!";
        uint64_t hello_ino = fs.create_file_with_data(
            "alpha/sub/hello.txt", oifs::ByteView(hello), oifs::WritePolicy::Raw, 0);
        OIFS_CHECK(fs.exists("alpha/sub/hello.txt"));

        auto sub_entries = fs.ls("alpha/sub");
        OIFS_CHECK(sub_entries.size() == 1);
        OIFS_CHECK(sub_entries[0].name == "hello.txt");
        OIFS_CHECK(sub_entries[0].size == hello.size());

        // ls("/") and ls("") and ls() all list root
        auto root_slash = fs.ls("/");
        auto root_empty = fs.ls("");
        auto root_default = fs.ls();
        OIFS_CHECK(root_slash.size() == root_default.size());
        OIFS_CHECK(root_empty.size() == root_default.size());

        // ls on a regular file or missing path throws oifs::Error
        bool caught_ls_file = false;
        try {
            (void)fs.ls("alpha/sub/hello.txt");
        } catch (const oifs::Error&) {
            caught_ls_file = true;
        }
        OIFS_CHECK(caught_ls_file);

        // 1. Atomic Rename (cross-directory + within-directory)
        fs.mkdir("beta");
        fs.rename("alpha/sub/hello.txt", "beta/moved.txt");
        OIFS_CHECK(!fs.exists("alpha/sub/hello.txt"));
        OIFS_CHECK(fs.exists("beta/moved.txt"));
        OIFS_CHECK(fs.resolve_path("beta/moved.txt") == hello_ino);
        std::vector<uint8_t> read_moved = fs.read_file("beta/moved.txt");
        OIFS_CHECK(std::string(read_moved.begin(), read_moved.end()) == hello);

        // Rename missing path must fail
        bool caught_rename_missing = false;
        try {
            fs.rename("no_such_src.txt", "no_such_dst.txt");
        } catch (const oifs::Error&) {
            caught_rename_missing = true;
        }
        OIFS_CHECK(caught_rename_missing);

        // 4 & 7. SIMD Numeric Filters + Ergonomic Typed Read/Write Helpers
        constexpr size_t kNumI32 = 4096;
        std::vector<int32_t> ints(kNumI32);
        for (size_t i = 0; i < kNumI32; ++i) {
            ints[i] = static_cast<int32_t>(100000 + i * 3);
        }
        oifs::Span<const int32_t> const_i32_span(ints.data(), ints.size());
        oifs::Span<int32_t> mut_i32_span(ints.data(), ints.size());

        uint64_t num_ino = fs.create_file_with_span(
            "beta/numeric.bin",
            const_i32_span,
            oifs::WritePolicy::Stream,
            3,
            oifs::FilterConfig::numeric(4));
        OIFS_CHECK(num_ino > 0);

        oifs::FileStat num_st = fs.stat("beta/numeric.bin");
        OIFS_CHECK(!num_st.is_dir);
        OIFS_CHECK(num_st.size == ints.size() * sizeof(int32_t));
        OIFS_CHECK(num_st.filter_typesize == 4);
        OIFS_CHECK(num_st.filter_delta);
        OIFS_CHECK(num_st.filter_shuffle);
        OIFS_CHECK(!num_st.filter_bitshuffle);

        std::vector<uint8_t> raw_num = fs.read_file("beta/numeric.bin");
        OIFS_CHECK(raw_num.size() == ints.size() * sizeof(int32_t));
        auto roundtrip_i32 = oifs::ByteView(raw_num).as_span<const int32_t>();
        OIFS_CHECK(roundtrip_i32.size() == kNumI32);
        for (size_t i = 0; i < kNumI32; ++i) {
            OIFS_CHECK(roundtrip_i32[i] == ints[i]);
        }

        // BitShuffle + Stream via vector overload & create_file_with_data filtered overload
        std::vector<uint64_t> u64_vals = make_u64_pattern(12000, 0x123456789ABCDEF0ULL); // 96 KiB
        uint64_t bs_ino = fs.create_file_with_span(
            "beta/bitshuffle_stream.bin",
            u64_vals,
            oifs::WritePolicy::Stream,
            3,
            oifs::FilterConfig::bitshuffle(8));
        OIFS_CHECK(bs_ino > 0);

        oifs::FileStat bs_st = fs.stat("beta/bitshuffle_stream.bin");
        OIFS_CHECK(!bs_st.seekable_64k);
        OIFS_CHECK(bs_st.filter_typesize == 8);
        OIFS_CHECK(!bs_st.filter_delta);
        OIFS_CHECK(!bs_st.filter_shuffle);
        OIFS_CHECK(bs_st.filter_bitshuffle);

        std::vector<uint8_t> raw_bs = fs.read_file("beta/bitshuffle_stream.bin");
        OIFS_CHECK(raw_bs.size() == u64_vals.size() * sizeof(uint64_t));
        OIFS_CHECK(std::memcmp(raw_bs.data(), u64_vals.data(), raw_bs.size()) == 0);

        // Seekable64K stat check
        (void)fs.create_file_with_span(
            "beta/seek64k.bin",
            u64_vals,
            oifs::WritePolicy::Seekable64K,
            3);
        oifs::FileStat seek_st = fs.stat("beta/seek64k.bin");
        OIFS_CHECK(seek_st.seekable_64k);
        OIFS_CHECK(seek_st.size == u64_vals.size() * sizeof(uint64_t));
        OIFS_CHECK(fs.read_file("beta/seek64k.bin").size() == seek_st.size);

        // write_span (non-const Span<T> and offset write with filters)
        std::vector<int32_t> patch_ints = {42, 43, 44, 45};
        fs.write_span(
            "beta/patched.bin",
            0,
            mut_i32_span,
            oifs::WritePolicy::Stream,
            1,
            oifs::FilterConfig::numeric(4));
        fs.write_span(
            "beta/patched.bin",
            16,
            patch_ints,
            oifs::WritePolicy::Stream,
            1,
            oifs::FilterConfig::numeric(4));
        std::vector<uint8_t> patched_bytes = fs.read_file("beta/patched.bin");
        auto patched_sp = oifs::ByteView(patched_bytes).as_span<const int32_t>();
        OIFS_CHECK(patched_sp[0] == ints[0]);
        OIFS_CHECK(patched_sp[4] == 42);
        OIFS_CHECK(patched_sp[5] == 43);
        OIFS_CHECK(patched_sp[6] == 44);
        OIFS_CHECK(patched_sp[7] == 45);
        OIFS_CHECK(patched_sp[8] == ints[8]);

#if __cplusplus >= 202002L && defined(__cpp_lib_span)
        // C++20 std::span<const T> and std::span<T> overloads
        std::span<const int32_t> std_const_sp(ints);
        (void)fs.create_file_with_span(
            "beta/cpp20_span.bin",
            std_const_sp,
            oifs::WritePolicy::Stream,
            1,
            oifs::FilterConfig::numeric(4));
        std::span<int32_t> std_mut_sp(patch_ints);
        fs.write_span(
            "beta/cpp20_span.bin",
            8,
            std_mut_sp,
            oifs::WritePolicy::Stream,
            1,
            oifs::FilterConfig::numeric(4));
        std::vector<uint8_t> c20_read = fs.read_file("beta/cpp20_span.bin");
        auto c20_sp = oifs::ByteView(c20_read).as_span<const int32_t>();
        OIFS_CHECK(c20_sp[2] == 42);
        OIFS_CHECK(c20_sp[3] == 43);
#endif

        // Empty file read_file & directory read_file error check
        (void)fs.create_file_with_data("beta/empty.bin", oifs::ByteView(), oifs::WritePolicy::Raw, 0);
        OIFS_CHECK(fs.read_file("beta/empty.bin").empty());
        bool caught_read_dir = false;
        try {
            (void)fs.read_file("beta");
        } catch (const oifs::Error&) {
            caught_read_dir = true;
        }
        OIFS_CHECK(caught_read_dir);

        // 6. Online Maintenance & Diagnostics
        oifs::FsckReport fsck = fs.verify_integrity();
        OIFS_CHECK(fsck.is_clean);
        OIFS_CHECK(fsck.orphan_inodes == 0);
        OIFS_CHECK(fsck.leaked_blocks == 0);
        OIFS_CHECK(fsck.missing_blocks == 0);
        OIFS_CHECK(fsck.cross_linked_blocks == 0);

        oifs::FragmentationStats frag = fs.analyze_fragmentation();
        OIFS_CHECK(frag.total_blocks > 0);
        OIFS_CHECK(frag.used_blocks + frag.free_blocks == frag.total_blocks);

        // Create fragmented layout and defragment
        for (int i = 0; i < 6; ++i) {
            std::vector<uint8_t> blk(8192, static_cast<uint8_t>('A' + i));
            (void)fs.create_file_with_data(
                "frag_" + std::to_string(i) + ".bin", oifs::ByteView(blk), oifs::WritePolicy::Raw, 0);
        }
        fs.delete_file("frag_1.bin");
        fs.delete_file("frag_3.bin");
        oifs::DefragStats dstats = fs.defragment();
        (void)dstats;
        OIFS_CHECK(fs.verify_integrity().is_clean);
        fs.flush();
    }

    // Unified open_read_only(path) (1-arg, 2-arg, 3-arg) & read-only checks on new methods
    {
        oifs::FileSystem ro = oifs::FileSystem::open_read_only(img_path);
        OIFS_CHECK(ro.is_read_only());
        OIFS_CHECK(ro.exists("beta/moved.txt"));
        OIFS_CHECK(!ro.exists("beta/missing.txt"));
        OIFS_CHECK(ro.stat("beta/numeric.bin").filter_delta);
        OIFS_CHECK(!ro.ls("beta").empty());
        OIFS_CHECK(ro.verify_integrity().is_clean);
        OIFS_CHECK(ro.analyze_fragmentation().total_blocks > 0);

        bool caught_ro_rename = false;
        try {
            ro.rename("beta/moved.txt", "beta/blocked.txt");
        } catch (const oifs::Error&) {
            caught_ro_rename = true;
        }
        OIFS_CHECK(caught_ro_rename);

        bool caught_ro_defrag = false;
        try {
            (void)ro.defragment();
        } catch (const oifs::Error&) {
            caught_ro_defrag = true;
        }
        OIFS_CHECK(caught_ro_defrag);
    }
}

static void test_clone_transfer_splice_and_get_or_open(const std::string& data_dir) {
    const std::string src_img = join_path(data_dir, "cpp_xfer_src.img");
    const std::string dst_img = join_path(data_dir, "cpp_xfer_dst.img");
    const std::string enc_img = join_path(data_dir, "cpp_xfer_enc.img");
    const std::string clone_img = join_path(data_dir, "cpp_xfer_cloned.img");

    // 5c. get_or_open & get_or_open_with_password
    {
        oifs::FileSystem g1 = oifs::FileSystem::get_or_open(src_img, 16ULL * 1024 * 1024);
        oifs::FileSystem g2 = oifs::FileSystem::get_or_open(src_img, 0);
        std::string via_session = "Created via get_or_open session!";
        (void)g1.create_file_with_data(
            "session_file.txt", oifs::ByteView(via_session), oifs::WritePolicy::Raw, 0);
        g1.flush();
        std::vector<uint8_t> got = g2.read_file("session_file.txt");
        OIFS_CHECK(std::string(got.begin(), got.end()) == via_session);
    }
    {
        oifs::FileSystem ge1 = oifs::FileSystem::get_or_open_with_password(
            enc_img, 16ULL * 1024 * 1024, "session_enc_pwd");
        std::string sec_msg = "Encrypted file created via get_or_open_with_password";
        (void)ge1.create_file_with_data(
            "enc_payload.bin", oifs::ByteView(sec_msg), oifs::WritePolicy::Raw, 0);
        ge1.flush();

        oifs::FileSystem ge2 = oifs::FileSystem::get_or_open_with_password(
            enc_img, 0, "session_enc_pwd");
        std::vector<uint8_t> dec = ge2.read_file("enc_payload.bin");
        OIFS_CHECK(std::string(dec.begin(), dec.end()) == sec_msg);
    }

    // 2a. Whole-image CoW clone
    {
        oifs::CloneMethod cm = oifs::FileSystem::clone_image(src_img, clone_img);
        OIFS_CHECK(cm == oifs::CloneMethod::ApfsClonefile ||
                   cm == oifs::CloneMethod::LinuxFiclone ||
                   cm == oifs::CloneMethod::StandardCopy);

        oifs::FileSystem cloned_ro = oifs::FileSystem::open_read_only(clone_img);
        OIFS_CHECK(cloned_ro.exists("session_file.txt"));
        std::vector<uint8_t> got = cloned_ro.read_file("session_file.txt");
        OIFS_CHECK(std::string(got.begin(), got.end()) == "Created via get_or_open session!");

        // Cloning onto an existing path must fail
        bool caught_dup_clone = false;
        try {
            (void)oifs::FileSystem::clone_image(src_img, clone_img);
        } catch (const oifs::Error&) {
            caught_dup_clone = true;
        }
        OIFS_CHECK(caught_dup_clone);
    }

    // 2b & 2c. Cross-image transfer_file_to & splice_range_to
    {
        oifs::FileSystem src = oifs::FileSystem::open_with_options(
            src_img, 0, "", oifs::StorageMode::Mmap, false);
        oifs::FileSystem dst = oifs::FileSystem::open_with_options(
            dst_img, 16ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, false);
        oifs::FileSystem enc = oifs::FileSystem::open_with_options(
            enc_img, 0, "session_enc_pwd", oifs::StorageMode::Mmap, false);

        // Populate 64 KiB raw file in src
        std::vector<uint8_t> big_raw(64 * 1024);
        for (size_t i = 0; i < big_raw.size(); ++i) {
            big_raw[i] = static_cast<uint8_t>((i * 19 + 3) & 0xFF);
        }
        uint64_t src_ino = src.create_file_with_data(
            "big_raw.bin", oifs::ByteView(big_raw), oifs::WritePolicy::Raw, 0);

        // Copy transfer src -> dst
        oifs::TransferStats xfer = src.transfer_file_to("big_raw.bin", dst, "copied_raw.bin");
        OIFS_CHECK(xfer.src_inode == src_ino);
        OIFS_CHECK(xfer.dst_inode > 0);
        OIFS_CHECK(xfer.logical_bytes == big_raw.size());
        OIFS_CHECK(dst.read_file("copied_raw.bin") == big_raw);
        OIFS_CHECK(src.exists("big_raw.bin"));

        // Overwrite + move transfer
        std::string small_src = "Move overwrite payload!";
        (void)src.create_file_with_data(
            "to_move.txt", oifs::ByteView(small_src), oifs::WritePolicy::Raw, 0);
        oifs::TransferOptions move_opts;
        move_opts.move_file = true;
        move_opts.overwrite = true;
        oifs::TransferStats mv_xfer = src.transfer_file_to(
            "to_move.txt", dst, "copied_raw.bin", move_opts);
        OIFS_CHECK(mv_xfer.logical_bytes == small_src.size());
        OIFS_CHECK(!src.exists("to_move.txt"));
        std::vector<uint8_t> moved_read = dst.read_file("copied_raw.bin");
        OIFS_CHECK(std::string(moved_read.begin(), moved_read.end()) == small_src);

        // Cross encrypted boundary must use LogicalReencode
        oifs::TransferStats enc_xfer = src.transfer_file_to(
            "big_raw.bin", enc, "reencrypted.bin");
        OIFS_CHECK(enc_xfer.method == oifs::TransferMethod::LogicalReencode);
        OIFS_CHECK(enc.read_file("reencrypted.bin") == big_raw);

        // Sub-range splice: Keep, PunchHole, CollapseRange
        std::vector<uint8_t> dst_base(64 * 1024, 0x11);
        uint64_t dst_splice_ino = dst.create_file_with_data(
            "splice_dst.bin", oifs::ByteView(dst_base), oifs::WritePolicy::Raw, 0);

        oifs::SpliceStats sp_keep = src.splice_range_to(
            src_ino, 4096, dst, dst_splice_ino, 8192, 16384);
        OIFS_CHECK(sp_keep.src_inode == src_ino);
        OIFS_CHECK(sp_keep.dst_inode == dst_splice_ino);
        OIFS_CHECK(sp_keep.bytes_spliced == 16384);
        std::vector<uint8_t> dst_after_keep = dst.read_file("splice_dst.bin");
        OIFS_CHECK(std::memcmp(dst_after_keep.data() + 8192, big_raw.data() + 4096, 16384) == 0);
        OIFS_CHECK(dst_after_keep[0] == 0x11);

        // PunchHole on [16384, 32768) in src_ino
        oifs::SpliceOptions punch_opts;
        punch_opts.trim_mode = oifs::SpliceTrimMode::PunchHole;
        oifs::SpliceStats sp_punch = src.splice_range_to(
            src_ino, 16384, dst, dst_splice_ino, 0, 16384, punch_opts);
        OIFS_CHECK(sp_punch.bytes_spliced == 16384);
        std::vector<uint8_t> src_after_punch = src.read_file("big_raw.bin");
        OIFS_CHECK(src_after_punch.size() == big_raw.size());
        for (size_t i = 16384; i < 32768; ++i) {
            OIFS_CHECK(src_after_punch[i] == 0);
        }

        // CollapseRange on [0, 8192) in src_ino
        oifs::SpliceOptions collapse_opts;
        collapse_opts.trim_mode = oifs::SpliceTrimMode::CollapseRange;
        oifs::SpliceStats sp_col = src.splice_range_to(
            src_ino, 0, dst, dst_splice_ino, 32768, 8192, collapse_opts);
        OIFS_CHECK(sp_col.bytes_spliced == 8192);
        std::vector<uint8_t> src_after_col = src.read_file("big_raw.bin");
        OIFS_CHECK(src_after_col.size() == big_raw.size() - 8192);
        OIFS_CHECK(std::memcmp(src_after_col.data(), big_raw.data() + 8192, 4096) == 0);
    }

    // Read-only enforcement on transfer_file_to and splice_range_to
    {
        oifs::FileSystem ro_src = oifs::FileSystem::open_read_only(src_img);
        oifs::FileSystem ro_dst = oifs::FileSystem::open_read_only(dst_img);
        oifs::FileSystem rw_dst = oifs::FileSystem::open_with_options(
            join_path(data_dir, "cpp_xfer_rw_dst.img"),
            8ULL * 1024 * 1024,
            "",
            oifs::StorageMode::Mmap,
            false);

        // Copy OUT of read-only source into RW destination is allowed
        oifs::TransferStats ro_copy = ro_src.transfer_file_to(
            "session_file.txt", rw_dst, "from_ro.txt");
        OIFS_CHECK(ro_copy.logical_bytes > 0);
        OIFS_CHECK(rw_dst.exists("from_ro.txt"));

        // Move OUT of read-only source must throw
        oifs::TransferOptions mv;
        mv.move_file = true;
        bool caught_ro_move = false;
        try {
            (void)ro_src.transfer_file_to("session_file.txt", rw_dst, "blocked_mv.txt", mv);
        } catch (const oifs::Error&) {
            caught_ro_move = true;
        }
        OIFS_CHECK(caught_ro_move);

        // Transfer INTO read-only destination must throw
        bool caught_ro_dst = false;
        try {
            (void)rw_dst.transfer_file_to("from_ro.txt", ro_dst, "blocked_into_ro.txt");
        } catch (const oifs::Error&) {
            caught_ro_dst = true;
        }
        OIFS_CHECK(caught_ro_dst);

        // Splice PunchHole on read-only source must throw
        uint64_t ro_src_ino = ro_src.resolve_path("session_file.txt");
        uint64_t rw_dst_ino = rw_dst.resolve_path("from_ro.txt");
        oifs::SpliceOptions punch;
        punch.trim_mode = oifs::SpliceTrimMode::PunchHole;
        bool caught_ro_splice = false;
        try {
            (void)ro_src.splice_range_to(ro_src_ino, 0, rw_dst, rw_dst_ino, 0, 8, punch);
        } catch (const oifs::Error&) {
            caught_ro_splice = true;
        }
        OIFS_CHECK(caught_ro_splice);
    }
}

#if defined(OIFS_HAS_CPP20_COROUTINE)
struct SyncCoroTask {
    struct promise_type {
        std::mutex mtx;
        std::condition_variable cv;
        bool finished = false;
        std::exception_ptr exc;

        SyncCoroTask get_return_object() noexcept {
            return SyncCoroTask{std::coroutine_handle<promise_type>::from_promise(*this)};
        }
        std::suspend_never initial_suspend() noexcept { return {}; }
        struct FinalAwaiter {
            bool await_ready() noexcept { return false; }
            void await_suspend(std::coroutine_handle<promise_type> h) noexcept {
                auto& p = h.promise();
                {
                    std::lock_guard<std::mutex> lk(p.mtx);
                    p.finished = true;
                }
                p.cv.notify_all();
            }
            void await_resume() noexcept {}
        };
        FinalAwaiter final_suspend() noexcept { return {}; }
        void return_void() noexcept {}
        void unhandled_exception() noexcept { exc = std::current_exception(); }
    };

    std::coroutine_handle<promise_type> handle;
    bool waited = false;

    explicit SyncCoroTask(std::coroutine_handle<promise_type> h) noexcept : handle(h) {}
    SyncCoroTask(const SyncCoroTask&) = delete;
    SyncCoroTask& operator=(const SyncCoroTask&) = delete;
    ~SyncCoroTask() {
        if (handle) {
            if (!waited) {
                auto& p = handle.promise();
                std::unique_lock<std::mutex> lk(p.mtx);
                p.cv.wait(lk, [&] { return p.finished; });
            }
            handle.destroy();
        }
    }

    void wait() {
        auto& p = handle.promise();
        {
            std::unique_lock<std::mutex> lk(p.mtx);
            p.cv.wait(lk, [&] { return p.finished; });
        }
        waited = true;
        if (p.exc) {
            std::rethrow_exception(p.exc);
        }
    }
};

static SyncCoroTask run_cpp20_coro_workflow(
    oifs::FileSystem& fs,
    uint64_t raw_ino,
    const std::vector<uint8_t>& raw_expected,
    uint64_t comp_ino,
    const std::vector<uint8_t>& comp_expected
) {
    // 1. Inline Fast-Path hit: await_ready() == true (zero suspend!)
    std::vector<uint8_t> raw_out(raw_expected.size(), 0);
    oifs::AsyncFuture raw_fut = fs.read_at_async(raw_ino, 0, oifs::MutableByteSpan(raw_out));
    OIFS_CHECK(raw_fut.is_ready());
    int64_t n_raw = co_await raw_fut;
    OIFS_CHECK(n_raw == static_cast<int64_t>(raw_expected.size()));
    OIFS_CHECK(raw_out == raw_expected);

    // 2. Background pool slow-path: compressed file suspends & resumes on worker completion
    std::vector<uint8_t> comp_out(comp_expected.size(), 0);
    int64_t n_comp = co_await fs.read_at_async(comp_ino, 0, oifs::MutableByteSpan(comp_out));
    OIFS_CHECK(n_comp == static_cast<int64_t>(comp_expected.size()));
    OIFS_CHECK(comp_out == comp_expected);

    // 3. Group-Commit async create + write + flush via co_await
    std::string coro_text = "Created and flushed via C++20 co_await coroutine!";
    int64_t created_id = co_await fs.create_file_with_data_async(
        "coro_created.txt", oifs::ByteView(coro_text), oifs::WritePolicy::Raw, 0);
    OIFS_CHECK(created_id > 0);

    std::string patched = "CORO";
    int64_t wrote_id = co_await fs.write_file_async("coro_created.txt", oifs::ByteView(patched));
    OIFS_CHECK(wrote_id == created_id);

    int64_t flush_rc = co_await fs.flush_async();
    OIFS_CHECK(flush_rc == 0);

    std::vector<uint8_t> check_buf(coro_text.size(), 0);
    int64_t n_chk = co_await fs.read_at_async(
        static_cast<uint64_t>(created_id), 0, oifs::MutableByteSpan(check_buf));
    OIFS_CHECK(n_chk == static_cast<int64_t>(coro_text.size()));
    OIFS_CHECK(std::memcmp(check_buf.data(), "COROted", 7) == 0);

    // 4. Failing async future throws oifs::Error from co_await
    std::vector<uint8_t> err_buf(16, 0);
    bool caught_coro_err = false;
    try {
        (void)co_await fs.read_at_async(9999999ULL, 0, oifs::MutableByteSpan(err_buf));
    } catch (const oifs::Error& e) {
        caught_coro_err = std::strlen(e.what()) > 0;
    }
    OIFS_CHECK(caught_coro_err);
}

struct CancelableCoro {
    struct promise_type {
        std::atomic<bool> resumed_after_cancel{false};
        CancelableCoro get_return_object() noexcept {
            return CancelableCoro{std::coroutine_handle<promise_type>::from_promise(*this)};
        }
        std::suspend_never initial_suspend() noexcept { return {}; }
        std::suspend_always final_suspend() noexcept { return {}; }
        void return_void() noexcept {}
        void unhandled_exception() noexcept {}
    };

    std::coroutine_handle<promise_type> handle{};

    explicit CancelableCoro(std::coroutine_handle<promise_type> h) noexcept : handle(h) {}
    CancelableCoro(const CancelableCoro&) = delete;
    CancelableCoro& operator=(const CancelableCoro&) = delete;
    ~CancelableCoro() {
        if (handle) {
            handle.destroy();
        }
    }
    void destroy_early() {
        if (handle) {
            handle.destroy();
            handle = nullptr;
        }
    }
};

static CancelableCoro read_in_coro_for_cancel(
    oifs::FileSystem& fs,
    uint64_t ino,
    uint8_t* dst,
    size_t len,
    std::atomic<bool>& completed_flag
) {
    oifs::MutableByteSpan span(dst, len);
    (void)co_await fs.read_at_async(ino, 0, span);
    completed_flag.store(true, std::memory_order_release);
}

static void test_cpp20_coro_early_cancel_no_buffer_uaf(oifs::FileSystem& fs) {
    // 5 MiB compressible file compressed at Zstd Stream level 9 so cold decode runs
    // asynchronously on the background pool (never triggers DecodedBlobCache 4 MiB cap
    // or inline fast-path).
    const size_t kFileSize = 5ULL * 1024 * 1024;
    std::vector<uint8_t> payload(kFileSize);
    for (size_t i = 0; i < payload.size(); ++i) {
        payload[i] = static_cast<uint8_t>((i * 131 + (i >> 8)) & 0xFF);
    }

    for (int iter = 0; iter < 5; ++iter) {
        std::string name = "coro_cancel_" + std::to_string(iter) + ".zst";
        uint64_t ino = fs.create_file_with_data(
            name, oifs::ByteView(payload), oifs::WritePolicy::Stream, 9);

        std::vector<uint8_t> arena(kFileSize, 0xCC);
        std::atomic<bool> completed_after_cancel{false};
        CancelableCoro coro = read_in_coro_for_cancel(
            fs, ino, arena.data(), arena.size(), completed_after_cancel);
        coro.destroy_early();

        // Poison caller buffer immediately after coroutine destruction.
        // If background read wasn't quiesced by ~Awaiter(), the worker will overwrite
        // `arena` with decompressed bytes (UAF).
        std::fill(arena.begin(), arena.end(), 0xDD);
        (void)fs.flush_async().wait();
        std::this_thread::sleep_for(std::chrono::milliseconds(40));

        OIFS_CHECK(!completed_after_cancel.load(std::memory_order_acquire));
        for (size_t i = 0; i < arena.size(); ++i) {
            if (arena[i] != 0xDD) {
                std::fprintf(
                    stderr,
                    "repro_bug3 UAF: cancelled co_await mutated caller buffer at byte %zu (got 0x%02x) on iter %d!\n",
                    i,
                    arena[i],
                    iter);
                std::abort();
            }
        }
    }
}

static void test_cpp20_coroutine_co_await(const std::string& coro_img) {
    oifs::FileSystem fs = oifs::FileSystem::open_with_options(
        coro_img, 16ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, true);
    fs.set_durability_mode(oifs::DurabilityMode::Strict);

    std::vector<uint8_t> raw_data(16384);
    for (size_t i = 0; i < raw_data.size(); ++i) {
        raw_data[i] = static_cast<uint8_t>((i * 11 + 7) & 0xFF);
    }
    std::vector<uint8_t> comp_data = make_compressible_pattern(196608, 42);

    uint64_t raw_ino = fs.create_file_with_data(
        "coro_raw.bin", oifs::ByteView(raw_data), oifs::WritePolicy::Raw, 0);
    uint64_t comp_ino = fs.create_file_with_data(
        "coro_comp.zst", oifs::ByteView(comp_data), oifs::WritePolicy::Seekable64K, 3);

    SyncCoroTask task = run_cpp20_coro_workflow(fs, raw_ino, raw_data, comp_ino, comp_data);
    task.wait();
    test_cpp20_coro_early_cancel_no_buffer_uaf(fs);
}
#else
#if __cplusplus >= 202002L && defined(__cpp_impl_coroutine) && __has_include(<coroutine>)
#error "OIFS_HAS_CPP20_COROUTINE must be defined in C++20 coroutine builds"
#endif
#endif

int main(int argc, char** argv) {
    OIFS_CHECK(OIFS_CHECK_VERSION() == OIFS_VERSION_COMPAT_OK);

#ifndef _WIN32
    if (argc == 8 && std::string(argv[1]) == "--ro-worker") {
        return run_ro_worker_child(
            std::atoi(argv[2]),
            argv[3],
            argv[4],
            argv[5],
            std::atoi(argv[6]),
            std::atoi(argv[7]));
    }
    if (argc == 5 && std::string(argv[1]) == "--rw-probe") {
        return run_rw_probe_child(argv[2], argv[3], argv[4]);
    }
#endif

    OIFS_CHECK(argc == 3 && std::string(argv[1]) == "--run-all");
    const std::string data_dir = argv[2];

    const std::string mmap_img = join_path(data_dir, "cpp_mmap.img");
    const std::string file_io_img = join_path(data_dir, "cpp_file_io.img");
    const std::string enc_img = join_path(data_dir, "cpp_enc.img");
    const std::string async_img = join_path(data_dir, "cpp_async_strict.img");
    const std::string ro_img = join_path(data_dir, "cpp_readonly.img");
    const std::string ro_enc_img = join_path(data_dir, "cpp_readonly_enc.img");
    const std::string edge_mmap_img = join_path(data_dir, "cpp_edge_mmap.img");
    const std::string edge_fio_img = join_path(data_dir, "cpp_edge_fio.img");
    const std::string edge_enc_img = join_path(data_dir, "cpp_edge_enc.img");
    const std::string mp_plain_img = join_path(data_dir, "cpp_mp_ro_plain.img");
    const std::string mp_enc_img = join_path(data_dir, "cpp_mp_ro_enc.img");
    const std::string mt_mmap_img = join_path(data_dir, "cpp_mt_rw_mmap.img");
    const std::string mt_fio_img = join_path(data_dir, "cpp_mt_rw_fio.img");
    const std::string bench_img = join_path(data_dir, "cpp_bench.img");
    const std::string fuse_img = join_path(data_dir, "cpp_fuse.img");
    const std::string features_img = join_path(data_dir, "cpp_features.img");
    const std::string coro_img = join_path(data_dir, "cpp_coro.img");

    test_pinnable_slice_zero_copy_and_fallbacks(mmap_img, file_io_img, enc_img);
    test_out_of_order_destruction_safety(mmap_img);
    test_batch_read_async_futures_and_callbacks(async_img);
    test_read_only_mode(ro_img, ro_enc_img);
    test_raii_moves_and_edge_cases(edge_mmap_img, edge_fio_img, edge_enc_img);
    test_multi_process_and_multi_thread(
        argv[0], mp_plain_img, mp_enc_img, mt_mmap_img, mt_fio_img);
    test_fuse_kernel_frame_dispatch(fuse_img);
    test_rename_stat_exists_subdir_and_filters(features_img);
    test_clone_transfer_splice_and_get_or_open(data_dir);
#if defined(OIFS_HAS_CPP20_COROUTINE)
    test_cpp20_coroutine_co_await(coro_img);
#else
    (void)coro_img;
#endif
    run_micro_benchmark(bench_img);

    std::cout << "ALL C++ NATIVE API (include/oifs.hpp) TESTS & BENCHMARKS PASSED!\n";
    return 0;
}
"##;

    std::fs::write(&cpp_src_path, cpp_code).expect("write C++ source");

    // 1. Compile and run in C++17 mode with -O2, strict warnings, and -pthread
    let bin_cpp17 = tmp_dir.path().join("cpp_api_suite_cpp17_bin");
    let data_cpp17 = tmp_dir.path().join("cpp17_data");
    compile_and_run_cpp_suite(
        compiler,
        "-std=c++17",
        &include_dir,
        &cpp_src_path,
        &dylib_path,
        &bin_cpp17,
        &data_cpp17,
    );

    // 2. Compile and run in C++20 mode with -O2, strict warnings, and -pthread
    assert!(
        compiler_supports_flag(compiler, "-std=c++20"),
        "C++ compiler '{compiler}' must support -std=c++20"
    );
    let bin_cpp20 = tmp_dir.path().join("cpp_api_suite_cpp20_bin");
    let data_cpp20 = tmp_dir.path().join("cpp20_data");
    compile_and_run_cpp_suite(
        compiler,
        "-std=c++20",
        &include_dir,
        &cpp_src_path,
        &dylib_path,
        &bin_cpp20,
        &data_cpp20,
    );
}

#[test]
fn test_cli_read_only_mode() {
    let bin_path = env!("CARGO_BIN_EXE_oifs");
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let img_path = tmp_dir.path().join("cli_ro.img");
    let img_str = img_path.to_str().unwrap();

    // 1. Create a journaled image and populate it with a directory and files
    let out = Command::new(bin_path)
        .args(["-i", img_str, "create", "--size", "4", "--journal"])
        .output()
        .expect("cli create");
    assert!(
        out.status.success(),
        "cli create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(bin_path)
        .args(["-i", img_str, "mkdir", "docs"])
        .output()
        .expect("cli mkdir");
    assert!(out.status.success());

    let host_plain = tmp_dir.path().join("plain.txt");
    let plain_bytes = b"Hello from a 0444 read-only OIFS image!\n";
    std::fs::write(&host_plain, plain_bytes).expect("write plain.txt");

    let out = Command::new(bin_path)
        .args([
            "-i",
            img_str,
            "put",
            "--no-compress",
            host_plain.to_str().unwrap(),
            "docs/plain.txt",
        ])
        .output()
        .expect("cli put");
    assert!(
        out.status.success(),
        "cli put failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 2. Mark image 0444 (read-only) and snapshot exact bytes
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&img_path, std::fs::Permissions::from_mode(0o444))
            .expect("chmod 0444");
    }
    let snapshot_before = std::fs::read(&img_path).expect("snapshot image before");

    // 3. Hold an active read-only DiskManager (F_RDLCK) in this process while running
    //    CLI --read-only commands in child processes (verifies cross-process shared locks).
    let _held_ro =
        oifs::disk::DiskManager::open_read_only(&img_path).expect("open_read_only in parent");

    for extra_args in [&[][..], &["--no-mmap"][..]] {
        let mut base = vec!["--read-only"];
        base.extend_from_slice(extra_args);
        base.extend_from_slice(&["-i", img_str]);

        // ls -r
        let out = Command::new(bin_path)
            .args(&base)
            .args(["ls", "-r"])
            .output()
            .expect("cli ls");
        assert!(
            out.status.success(),
            "cli ls failed ({:?}): {}",
            extra_args,
            String::from_utf8_lossy(&out.stderr)
        );
        let ls_stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            ls_stdout.contains("docs/plain.txt"),
            "ls output: {ls_stdout}"
        );

        // get
        let exported = tmp_dir.path().join(format!(
            "exported_{}.txt",
            if extra_args.is_empty() {
                "mmap"
            } else {
                "fileio"
            }
        ));
        let out = Command::new(bin_path)
            .args(&base)
            .args(["get", "docs/plain.txt", exported.to_str().unwrap()])
            .output()
            .expect("cli get");
        assert!(
            out.status.success(),
            "cli get failed ({:?}): {}",
            extra_args,
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read(&exported).expect("read exported"),
            plain_bytes
        );

        // cat
        let out = Command::new(bin_path)
            .args(&base)
            .args(["cat", "docs/plain.txt"])
            .output()
            .expect("cli cat");
        assert!(
            out.status.success(),
            "cli cat failed ({:?}): {}",
            extra_args,
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, plain_bytes);

        // info (text + --json)
        let out = Command::new(bin_path)
            .args(&base)
            .args(["info"])
            .output()
            .expect("cli info");
        assert!(
            out.status.success(),
            "cli info failed ({:?}): {}",
            extra_args,
            String::from_utf8_lossy(&out.stderr)
        );
        let info_stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            info_stdout.contains("Read-Only:      true"),
            "info output: {info_stdout}"
        );

        let out = Command::new(bin_path)
            .args(&base)
            .args(["--json", "info"])
            .output()
            .expect("cli info json");
        assert!(out.status.success());
        let info_json: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("parse info json");
        assert_eq!(info_json["ok"], true);
        assert_eq!(info_json["read_only"], true);
        assert_eq!(info_json["journaled"], true);

        // stat (text + --json)
        let out = Command::new(bin_path)
            .args(&base)
            .args(["--json", "stat", "docs/plain.txt"])
            .output()
            .expect("cli stat json");
        assert!(
            out.status.success(),
            "cli stat failed ({:?}): {}",
            extra_args,
            String::from_utf8_lossy(&out.stderr)
        );
        let stat_json: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("parse stat json");
        assert_eq!(stat_json["ok"], true);
        assert_eq!(stat_json["kind"], "file");
        assert_eq!(stat_json["size"], plain_bytes.len() as u64);

        // fsck
        let out = Command::new(bin_path)
            .args(&base)
            .args(["fsck"])
            .output()
            .expect("cli fsck");
        assert!(
            out.status.success(),
            "cli fsck failed ({:?}): {}",
            extra_args,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("CLEAN"));
    }

    // 4. Verify mutating subcommands fail cleanly under --read-only
    let mutating_cmds: &[&[&str]] = &[
        &["put", host_plain.to_str().unwrap(), "blocked.txt"],
        &["mkdir", "blocked_dir"],
        &["rm", "docs/plain.txt"],
        &["truncate", "docs/plain.txt", "--size", "0"],
        &["append", "docs/plain.txt", "extra"],
    ];
    for args in mutating_cmds {
        let out = Command::new(bin_path)
            .args(["--read-only", "-i", img_str])
            .args(*args)
            .output()
            .expect("cli mutating cmd");
        assert!(
            !out.status.success(),
            "expected {:?} to fail in --read-only mode",
            args
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.to_lowercase().contains("read-only"),
            "expected read-only error for {:?}, got stderr: {stderr}",
            args
        );
    }

    drop(_held_ro);

    // 5. Assert 100% byte-for-byte immutability of the 0444 image
    let snapshot_after = std::fs::read(&img_path).expect("snapshot image after");
    assert_eq!(
        snapshot_before, snapshot_after,
        "0444 image was modified during --read-only CLI operations"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&img_path, std::fs::Permissions::from_mode(0o644));
    }
}

#[test]
fn test_cli_mount_subcommand_args_and_validation() {
    let bin_path = env!("CARGO_BIN_EXE_oifs");

    // 1. Verify `oifs mount --help` exposes mountpoint, --foreground, and --allow-other
    let help = Command::new(bin_path)
        .args(["mount", "--help"])
        .output()
        .expect("cli mount --help");
    assert!(
        help.status.success(),
        "oifs mount --help failed: {}",
        String::from_utf8_lossy(&help.stderr)
    );
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(help_text.contains("--foreground"), "help: {help_text}");
    assert!(help_text.contains("--allow-other"), "help: {help_text}");

    // 2. Verify `oifs mount` rejects a nonexistent mountpoint cleanly (honoring --read-only & --no-mmap)
    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let img_path = tmp_dir.path().join("cli_mount.img");
    let img_str = img_path.to_str().unwrap();

    let create_out = Command::new(bin_path)
        .args(["-i", img_str, "create", "--size", "4"])
        .output()
        .expect("cli create");
    assert!(create_out.status.success());

    let missing_mp = tmp_dir.path().join("does_not_exist_mp");
    let mount_out = Command::new(bin_path)
        .args([
            "--read-only",
            "--no-mmap",
            "-i",
            img_str,
            "mount",
            "--foreground",
            missing_mp.to_str().unwrap(),
        ])
        .output()
        .expect("cli mount missing mountpoint");
    assert!(
        !mount_out.status.success(),
        "expected mount on nonexistent mountpoint to fail"
    );
    let stderr = String::from_utf8_lossy(&mount_out.stderr);
    assert!(
        stderr.contains("does not exist"),
        "unexpected mount stderr: {stderr}"
    );
}
