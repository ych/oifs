//! End-to-End Modern C++17 / C++20 Native API (`include/oifs.hpp`) Integration & Benchmark Suite
//!
//! Validates:
//! 1. `include/oifs.hpp` compiles cleanly with `-Wall -Wextra -Wpedantic -Werror` under C++17 and C++20.
//! 2. `oifs::PinnableSlice` true 0-copy on `StorageMode::Mmap` (`slice.is_pinned() == true`, pointer
//!    stability across repeated reads, 0 memcpy) and automatic 1-copy fallback (`slice.is_pinned() == false`)
//!    on `StorageMode::FileIo`, compressed (`Stream` / `Seekable64K`), and encrypted files.
//! 3. Out-of-order destruction safety: destroying `oifs::ReadSession` before `oifs::FileReadHandle`
//!    and before a pinned `oifs::PinnableSlice` without Use-After-Free.
//! 4. Scatter-gather `read_at_batch`, `oifs::AsyncFuture` (`read_at_async` inline fast-path & cold path,
//!    `create_file_with_data_async` Group Commit, `write_file_async`, `flush_async`, and early destruction
//!    drop-barrier UAF safety), plus C++ template lambda callback helpers (`read_at_async_cb` and
//!    `create_file_with_data_async_cb`).
//! 5. Micro-benchmark comparing Legacy C path-based `oifs_read_at` vs C++ `FileReadHandle::read_at`
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
    let disk_src = base.join("src/disk.rs");
    let needs_rebuild = match (
        target_dylib.metadata().and_then(|m| m.modified()),
        ffi_src.metadata().and_then(|m| m.modified()),
        disk_src.metadata().and_then(|m| m.modified()),
    ) {
        (Ok(dylib_time), Ok(ffi_time), Ok(disk_time)) => {
            dylib_time < ffi_time || dylib_time < disk_time
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
    let bin_path = tmp_dir.path().join("cpp_api_suite_bin");

    let mmap_img = tmp_dir.path().join("cpp_mmap.img");
    let file_io_img = tmp_dir.path().join("cpp_file_io.img");
    let enc_img = tmp_dir.path().join("cpp_enc.img");
    let async_img = tmp_dir.path().join("cpp_async_strict.img");
    let bench_img = tmp_dir.path().join("cpp_bench.img");

    let cpp_code = format!(
        r#"
#include "oifs.hpp"

#include <atomic>
#include <cassert>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <cstring>
#include <iostream>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

static void test_pinnable_slice_zero_copy_and_fallbacks(
    const std::string& mmap_path,
    const std::string& file_io_path,
    const std::string& enc_path
) {{
    // 1. Mmap StorageMode: raw uncompressed file -> True 0-Copy (is_pinned() == true)
    {{
        oifs::FileSystem fs = oifs::FileSystem::open_with_options(
            mmap_path, 32ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, false);

        // Prepare 64 KiB aligned float array payload
        constexpr size_t kNumFloats = 16384;
        std::vector<float> floats(kNumFloats);
        for (size_t i = 0; i < kNumFloats; ++i) {{
            floats[i] = static_cast<float>(i) * 1.25f;
        }}
        oifs::ByteView raw_view(reinterpret_cast<const uint8_t*>(floats.data()),
                                floats.size() * sizeof(float));
        uint64_t raw_inode = fs.create_file_with_data(
            "weights.raw", raw_view, oifs::WritePolicy::Raw, 0);

        // Also create a Stream-compressed file and a Seekable-64K compressed file on the same Mmap image
        std::string comp_text(32768, 'Z');
        for (size_t i = 0; i < comp_text.size(); i += 64) {{
            comp_text[i] = 'A';
        }}
        uint64_t stream_inode = fs.create_file_with_data(
            "compressed.stream", oifs::ByteView(comp_text), oifs::WritePolicy::Stream, 3);
        uint64_t seek_inode = fs.create_file_with_data(
            "compressed.seek64k", oifs::ByteView(comp_text), oifs::WritePolicy::Seekable64K, 3);

        assert(fs.resolve_path("weights.raw") == raw_inode);
        assert(fs.resolve_path("compressed.stream") == stream_inode);
        assert(fs.resolve_path("compressed.seek64k") == seek_inode);

        oifs::ReadSession session = fs.begin_read();
        oifs::FileReadHandle raw_fh = session.open_file("weights.raw");
        assert(raw_fh.inode_id() == raw_inode);
        assert(raw_fh.size() == raw_view.size());

        // Verify try_mmap_view returns direct zero-copy pointer
        auto opt_view = raw_fh.try_mmap_view(0, raw_view.size());
        assert(opt_view.has_value());
        assert(opt_view->size() == raw_view.size());
        assert(std::memcmp(opt_view->data(), raw_view.data(), raw_view.size()) == 0);

        // Verify ByteView::as_span<const float>() typed zero-copy access
        auto float_span = opt_view->as_span<const float>();
        assert(float_span.size() == kNumFloats);
        assert(float_span[0] == 0.0f);
        assert(float_span[100] == 125.0f);
        assert(float_span[kNumFloats - 1] == static_cast<float>(kNumFloats - 1) * 1.25f);

        // Verify PinnableSlice is_pinned() == true and pointer stability across reads
        oifs::PinnableSlice slice;
        size_t n1 = raw_fh.read_pinnable(0, raw_view.size(), slice);
        assert(n1 == raw_view.size());
        assert(slice.is_pinned());
        assert(slice.size() == raw_view.size());
        const uint8_t* pinned_ptr_1 = slice.data();
        assert(pinned_ptr_1 == opt_view->data());
        assert(std::memcmp(pinned_ptr_1, raw_view.data(), raw_view.size()) == 0);

        // Second read_pinnable at offset 4096 must point directly to pinned_ptr_1 + 4096 (0 memcpy!)
        oifs::PinnableSlice slice_offset;
        size_t n2 = raw_fh.read_pinnable(4096, 8192, slice_offset);
        assert(n2 == 8192);
        assert(slice_offset.is_pinned());
        assert(slice_offset.data() == pinned_ptr_1 + 4096);

        // Compressed files on Mmap image must automatically fall back to 1-Copy buffer
        oifs::FileReadHandle stream_fh = session.open_inode(stream_inode);
        assert(!stream_fh.try_mmap_view(0, comp_text.size()).has_value());
        oifs::PinnableSlice comp_slice;
        size_t cn = stream_fh.read_pinnable(0, comp_text.size(), comp_slice);
        assert(cn == comp_text.size());
        assert(!comp_slice.is_pinned());
        assert(comp_slice.as_string_view() == comp_text);

        oifs::FileReadHandle seek_fh = session.open_inode(seek_inode);
        assert(!seek_fh.try_mmap_view(0, comp_text.size()).has_value());
        size_t sn = seek_fh.read_pinnable(0, comp_text.size(), comp_slice);
        assert(sn == comp_text.size());
        assert(!comp_slice.is_pinned());
        assert(comp_slice.as_string_view() == comp_text);
    }}

    // 2. FileIo StorageMode (no mmap): PinnableSlice automatically falls back to 1-Copy
    {{
        oifs::FileSystem fs = oifs::FileSystem::open_with_options(
            file_io_path, 16ULL * 1024 * 1024, "", oifs::StorageMode::FileIo, true);
        fs.set_file_io_cache_bytes(8ULL * 1024 * 1024);

        std::string payload = "Pure pread/pwrite FileIo storage engine test payload for PinnableSlice!";
        uint64_t ino = fs.create_file_with_data(
            "fileio.bin", oifs::ByteView(payload), oifs::WritePolicy::Raw, 0);

        oifs::ReadSession sess = fs.begin_read();
        oifs::FileReadHandle fh = sess.open_inode(ino);
        assert(!fh.try_mmap_view(0, payload.size()).has_value());

        oifs::PinnableSlice slice = fh.read_pinnable(0, payload.size());
        assert(!slice.is_pinned());
        assert(slice.size() == payload.size());
        assert(slice.as_string_view() == payload);
    }}

    // 3. Encrypted Image: PinnableSlice automatically falls back to 1-Copy decrypted buffer
    {{
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
        assert(!fh.try_mmap_view(0, secret.size()).has_value());

        oifs::PinnableSlice slice = fh.read_pinnable(0, secret.size());
        assert(!slice.is_pinned());
        assert(slice.as_string_view() == secret);
    }}
}}

static void test_out_of_order_destruction_safety(const std::string& mmap_path) {{
    oifs::FileSystem fs = oifs::FileSystem::open(mmap_path, 0);

    std::string msg = "Out-of-order destruction safety verification payload (ReadSession dies first)";
    uint64_t ino = fs.create_file_with_data(
        "out_of_order.bin", oifs::ByteView(msg), oifs::WritePolicy::Raw, 0);

    oifs::FileReadHandle surviving_fh;
    oifs::PinnableSlice surviving_slice;

    {{
        oifs::ReadSession inner_session = fs.begin_read();
        surviving_fh = inner_session.open_inode(ino);
        surviving_fh.read_pinnable(0, msg.size(), surviving_slice);
        assert(surviving_slice.is_pinned());
        assert(surviving_slice.as_string_view() == msg);
        // inner_session is destroyed here while surviving_fh and surviving_slice outlive it!
    }}

    // Both surviving_fh and surviving_slice must remain 100% valid after ReadSession destruction
    assert(surviving_slice.is_pinned());
    assert(surviving_slice.as_string_view() == msg);

    std::vector<uint8_t> buf(msg.size(), 0);
    size_t n = surviving_fh.read_at(0, oifs::MutableByteSpan(buf));
    assert(n == msg.size());
    assert(std::memcmp(buf.data(), msg.data(), msg.size()) == 0);

    // Now destroy surviving_fh while surviving_slice STILL holds the pinned zero-copy slice!
    surviving_fh.close();
    assert(surviving_slice.is_pinned());
    assert(surviving_slice.as_string_view() == msg);
}}

static void test_batch_read_async_futures_and_callbacks(const std::string& async_path) {{
    oifs::FileSystem fs = oifs::FileSystem::open_with_options(
        async_path, 32ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, true);
    fs.set_durability_mode(oifs::DurabilityMode::Strict);

    // 1. Concurrent Group-Commit create_file_with_data_async
    constexpr int kNumFiles = 16;
    std::vector<oifs::AsyncFuture> create_futs;
    create_futs.reserve(kNumFiles);
    std::vector<std::string> payloads(kNumFiles);

    for (int i = 0; i < kNumFiles; ++i) {{
        payloads[i] = "Group-Commit Async Payload #" + std::to_string(i) + " with deterministic bytes";
        std::string fname = "async_file_" + std::to_string(i) + ".dat";
        create_futs.push_back(fs.create_file_with_data_async(
            fname, oifs::ByteView(payloads[i]), OIFS_WRITE_POLICY_RAW, 0));
    }}

    std::vector<uint64_t> inodes(kNumFiles);
    for (int i = 0; i < kNumFiles; ++i) {{
        int64_t id = create_futs[i].wait();
        assert(id > 0);
        inodes[i] = static_cast<uint64_t>(id);
    }}

    // 2. Scatter-gather read_at_batch across all created inodes
    std::vector<std::vector<uint8_t>> batch_bufs(kNumFiles);
    std::vector<OIFSReadBatchItem> batch_items(kNumFiles);
    for (int i = 0; i < kNumFiles; ++i) {{
        batch_bufs[i].resize(payloads[i].size(), 0);
        batch_items[i].inode_id = inodes[i];
        batch_items[i].offset = 0;
        batch_items[i].buf = batch_bufs[i].data();
        batch_items[i].buf_size = batch_bufs[i].size();
        batch_items[i].result = -1;
    }}
    fs.read_at_batch(batch_items);
    for (int i = 0; i < kNumFiles; ++i) {{
        assert(batch_items[i].result == static_cast<int64_t>(payloads[i].size()));
        assert(std::memcmp(batch_bufs[i].data(), payloads[i].data(), payloads[i].size()) == 0);
    }}

    // 3. read_at_async Inline Fast-Path (is_ready() == true immediately on Mmap raw file)
    std::vector<uint8_t> async_read_buf(payloads[0].size(), 0);
    oifs::AsyncFuture r_fut = fs.read_at_async(
        inodes[0], 0, oifs::MutableByteSpan(async_read_buf));
    assert(r_fut.is_ready());
    assert(r_fut.wait() == static_cast<int64_t>(payloads[0].size()));
    assert(std::memcmp(async_read_buf.data(), payloads[0].data(), payloads[0].size()) == 0);

    // 4. Early destruction UAF safety on slow-path compressed read_at_async
    std::string large_comp(128 * 1024, 'X');
    for (size_t i = 0; i < large_comp.size(); i += 17) {{
        large_comp[i] = static_cast<char>('a' + (i % 26));
    }}
    uint64_t comp_ino = fs.create_file_with_data(
        "slow_async_comp.zst", oifs::ByteView(large_comp), oifs::WritePolicy::Stream, 3);
    {{
        std::vector<uint8_t> temp_stack_buf(large_comp.size(), 0);
        // Immediately drop AsyncFuture without calling wait(); destructor must block if in-flight
        oifs::AsyncFuture dropped_fut = fs.read_at_async(
            comp_ino, 0, oifs::MutableByteSpan(temp_stack_buf));
    }}

    // 5. write_file_async & flush_async
    std::string updated_payload = "Updated via write_file_async!";
    oifs::AsyncFuture w_fut = fs.write_file_async("async_file_0.dat", oifs::ByteView(updated_payload));
    assert(w_fut.wait() == static_cast<int64_t>(inodes[0]));

    oifs::AsyncFuture fl_fut = fs.flush_async();
    assert(fl_fut.wait() == 0);

    // Verify sync FileSystem::read_at, read_at_inode, write_file, flush, and oifs::Error handling
    std::vector<uint8_t> sync_buf(updated_payload.size(), 0);
    assert(fs.read_at("async_file_0.dat", 0, oifs::MutableByteSpan(sync_buf)) == updated_payload.size());
    assert(std::memcmp(sync_buf.data(), updated_payload.data(), updated_payload.size()) == 0);

    std::string sync_written = "Written synchronously via FileSystem::write_file";
    fs.write_file("sync_check.txt", oifs::ByteView(sync_written));
    fs.flush();
    uint64_t sync_ino = fs.resolve_path("sync_check.txt");
    std::vector<uint8_t> sync_ino_buf(sync_written.size(), 0);
    assert(fs.read_at_inode(sync_ino, 0, oifs::MutableByteSpan(sync_ino_buf)) == sync_written.size());
    assert(std::memcmp(sync_ino_buf.data(), sync_written.data(), sync_written.size()) == 0);

    bool caught_missing = false;
    try {{
        (void)fs.resolve_path("does_not_exist.bin");
    }} catch (const oifs::Error& e) {{
        caught_missing = std::strlen(e.what()) > 0;
    }}
    assert(caught_missing);

    // Failed async op populates last_error() and throws oifs::Error on wait()
    std::vector<uint8_t> bad_buf(16, 0);
    oifs::AsyncFuture bad_fut = fs.read_at_async(999999, 0, oifs::MutableByteSpan(bad_buf));
    bool caught_async_err = false;
    try {{
        (void)bad_fut.wait();
    }} catch (const oifs::Error& e) {{
        caught_async_err = std::strlen(e.what()) > 0 && !bad_fut.last_error().empty();
    }}
    assert(caught_async_err);

    // 6. C++ Lambda Callback Helpers: create_file_with_data_async_cb & read_at_async_cb
    std::mutex mtx;
    std::condition_variable cv;
    bool create_cb_done = false;
    int64_t created_cb_inode = -1;
    std::string cb_payload = "Lambda callback async creation and read verification!";

    fs.create_file_with_data_async_cb(
        "cb_file.bin",
        oifs::ByteView(cb_payload),
        [&](int64_t status_or_inode) {{
            std::lock_guard<std::mutex> lk(mtx);
            created_cb_inode = status_or_inode;
            create_cb_done = true;
            cv.notify_all();
        }});

    {{
        std::unique_lock<std::mutex> lk(mtx);
        cv.wait(lk, [&] {{ return create_cb_done; }});
    }}
    assert(created_cb_inode > 0);

    bool read_cb_done = false;
    std::string read_cb_result;
    fs.read_at_async_cb(
        static_cast<uint64_t>(created_cb_inode),
        0,
        cb_payload.size(),
        [&](int64_t status, oifs::ByteView view) {{
            std::lock_guard<std::mutex> lk(mtx);
            if (status >= 0) {{
                read_cb_result = std::string(view.as_string_view());
            }}
            read_cb_done = true;
            cv.notify_all();
        }});

    {{
        std::unique_lock<std::mutex> lk(mtx);
        cv.wait(lk, [&] {{ return read_cb_done; }});
    }}
    assert(read_cb_result == cb_payload);

    // Throwing callbacks must be safely caught at the C ABI trampoline boundary
    bool throw_cb_ran = false;
    fs.read_at_async_cb(
        static_cast<uint64_t>(created_cb_inode),
        0,
        cb_payload.size(),
        [&](int64_t, oifs::ByteView) {{
            {{
                std::lock_guard<std::mutex> lk(mtx);
                throw_cb_ran = true;
            }}
            cv.notify_all();
            throw std::runtime_error("intentional exception inside read callback");
        }});
    {{
        std::unique_lock<std::mutex> lk(mtx);
        cv.wait(lk, [&] {{ return throw_cb_ran; }});
    }}
}}

static void run_micro_benchmark(const std::string& bench_path) {{
    oifs::FileSystem fs = oifs::FileSystem::open_with_options(
        bench_path, 32ULL * 1024 * 1024, "", oifs::StorageMode::Mmap, false);

    constexpr size_t kFileSize = 64 * 1024;
    constexpr size_t kReadLen = 4096;
    constexpr size_t kIterations = 20000;

    std::vector<uint8_t> data(kFileSize);
    for (size_t i = 0; i < kFileSize; ++i) {{
        data[i] = static_cast<uint8_t>((i * 31 + 7) & 0xFF);
    }}
    fs.mkdir("bench");
    fs.create_file_with_data(
        "bench/hot_asset.bin", oifs::ByteView(data), oifs::WritePolicy::Raw, 0);

    std::vector<uint8_t> scratch(kReadLen, 0);
    uint64_t checksum = 0;

    // 1. Legacy C path-based oifs_read_at
    auto t0 = std::chrono::steady_clock::now();
    for (size_t i = 0; i < kIterations; ++i) {{
        uint64_t off = (i * 4096) % (kFileSize - kReadLen);
        int64_t n = oifs_read_at(
            fs.raw(), "bench/hot_asset.bin", off, scratch.data(), kReadLen);
        assert(n == static_cast<int64_t>(kReadLen));
        checksum += scratch[0];
    }}
    auto t1 = std::chrono::steady_clock::now();

    // 2. C++ Pinned FileReadHandle::read_at (1-Copy, 0 path resolution, 0 RwLock per read)
    oifs::ReadSession session = fs.begin_read();
    oifs::FileReadHandle fh = session.open_file("bench/hot_asset.bin");

    auto t2 = std::chrono::steady_clock::now();
    for (size_t i = 0; i < kIterations; ++i) {{
        uint64_t off = (i * 4096) % (kFileSize - kReadLen);
        size_t n = fh.read_at(off, oifs::MutableByteSpan(scratch));
        assert(n == kReadLen);
        checksum += scratch[0];
    }}
    auto t3 = std::chrono::steady_clock::now();

    // 3. C++ 0-Copy PinnableSlice (0-Copy, 0 path resolution, 0 RwLock per read)
    oifs::PinnableSlice slice;
    auto t4 = std::chrono::steady_clock::now();
    for (size_t i = 0; i < kIterations; ++i) {{
        uint64_t off = (i * 4096) % (kFileSize - kReadLen);
        size_t n = fh.read_pinnable(off, kReadLen, slice);
        assert(n == kReadLen);
        assert(slice.is_pinned());
        checksum += slice[0];
    }}
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
}}

int main() {{
    assert(OIFS_CHECK_VERSION() == OIFS_VERSION_COMPAT_OK);

    test_pinnable_slice_zero_copy_and_fallbacks(
        "{mmap_img}",
        "{file_io_img}",
        "{enc_img}"
    );
    test_out_of_order_destruction_safety("{mmap_img}");
    test_batch_read_async_futures_and_callbacks("{async_img}");
    run_micro_benchmark("{bench_img}");

    std::cout << "ALL C++ NATIVE API (include/oifs.hpp) TESTS & BENCHMARKS PASSED!\n";
    return 0;
}}
"#,
        mmap_img = mmap_img.to_str().unwrap(),
        file_io_img = file_io_img.to_str().unwrap(),
        enc_img = enc_img.to_str().unwrap(),
        async_img = async_img.to_str().unwrap(),
        bench_img = bench_img.to_str().unwrap(),
    );

    std::fs::write(&cpp_src_path, &cpp_code).expect("write C++ source");

    // 1. Compile and run in C++17 mode with -O2 and strict warnings
    let status_cpp17 = Command::new(compiler)
        .args([
            "-std=c++17",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Wpedantic",
            "-Werror",
            "-I",
            include_dir.to_str().unwrap(),
            cpp_src_path.to_str().unwrap(),
            dylib_path.to_str().unwrap(),
            "-o",
            bin_path.to_str().unwrap(),
        ])
        .status()
        .expect("invoke C++17 compiler");
    assert!(
        status_cpp17.success(),
        "C++17 compilation against include/oifs.hpp failed"
    );

    let dylib_dir = dylib_path.parent().unwrap();
    let mut cmd = Command::new(&bin_path);
    if cfg!(target_os = "macos") {
        cmd.env("DYLD_LIBRARY_PATH", dylib_dir);
    } else {
        cmd.env("LD_LIBRARY_PATH", dylib_dir);
    }

    let output = cmd.output().expect("run C++17 test binary");
    assert!(
        output.status.success(),
        "C++17 test binary failed!\nStdout:\n{}\nStderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));

    // 2. Also verify clean compilation under -std=c++20 if supported by the compiler
    if compiler_supports_flag(compiler, "-std=c++20") {
        let bin_cpp20 = tmp_dir.path().join("cpp_api_suite_cpp20_bin");
        let status_cpp20 = Command::new(compiler)
            .args([
                "-std=c++20",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Wpedantic",
                "-Werror",
                "-I",
                include_dir.to_str().unwrap(),
                cpp_src_path.to_str().unwrap(),
                dylib_path.to_str().unwrap(),
                "-o",
                bin_cpp20.to_str().unwrap(),
            ])
            .status()
            .expect("invoke C++20 compiler");
        assert!(
            status_cpp20.success(),
            "C++20 compilation against include/oifs.hpp failed"
        );
    }
}
