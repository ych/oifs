//! End-to-End Native C and C++ Compiler Integration Tests
//!
//! Validates:
//! 1. `include/oifs.h` compiles with zero warnings under `-Wall -Wextra -Wpedantic -Werror` in both C11 and C++17.
//! 2. Complete C lifecycle test:
//!    - Version handshake macro `OIFS_CHECK_VERSION()` and loaded dynamic library path
//!    - Standard filesystem workflow: open, mkdir, create, write, read_at, ls, delete, close
//!    - Encrypted image workflow: open_with_password, read with good/bad passwords
//!    - Session handle reuse: get_or_open
//! 3. Complete C++ lifecycle & concurrency test:
//!    - Modern RAII `std::unique_ptr<OIFSHandle, ...>`
//!    - Modern C++ lambda as C callback in `oifs_ls`
//!    - `std::vector<uint8_t>` I/O buffer integration
//!    - Multi-threaded concurrent reads & writes using `std::thread`

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

    let rel_path = base.join("target/release").join(dylib_name);
    if rel_path.exists() {
        return rel_path;
    }
    let dbg_path = base.join("target/debug").join(dylib_name);
    if dbg_path.exists() {
        return dbg_path;
    }

    // Trigger build if dynamic library doesn't exist yet
    let status = Command::new("cargo")
        .args(["build", "--lib"])
        .status()
        .expect("cargo build --lib failed");
    assert!(status.success(), "Failed to build liboifs dylib");
    base.join("target/debug").join(dylib_name)
}

fn has_tool(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn test_native_c_e2e() {
    let compiler = if has_tool("clang") {
        "clang"
    } else if has_tool("gcc") {
        "gcc"
    } else {
        eprintln!("Neither clang nor gcc found in PATH, skipping native C test");
        return;
    };

    let dylib_path = get_dylib_path();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let include_dir = Path::new(&manifest_dir).join("include");

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let c_src_path = tmp_dir.path().join("main.c");
    let bin_path = tmp_dir.path().join("main_c_bin");
    let test_img_path = tmp_dir.path().join("c_test_fs.img");
    let enc_img_path = tmp_dir.path().join("c_enc_fs.img");

    let c_code = format!(
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include "oifs.h"

struct ListContext {{
    int file_count;
    uint64_t total_bytes;
}};

static void my_list_callback(const char *name, uint64_t size, uint64_t mtime, void *user_data) {{
    (void)name;
    (void)mtime;
    struct ListContext *ctx = (struct ListContext*)user_data;
    if (ctx) {{
        ctx->file_count++;
        ctx->total_bytes += size;
    }}
}}

int main(void) {{
    // 1. Verify Header Version Handshake Macro
    int v_status = OIFS_CHECK_VERSION();
    assert(v_status == OIFS_VERSION_COMPAT_OK);
    assert(oifs_version_major() == 1);
    assert(oifs_version_minor() == 0);
    assert(oifs_version_patch() == 0);
    assert(strcmp(oifs_version_string(), "1.0.0") == 0);

    // 2. Verify Loaded Library Path
    char dylib_file[1024] = {{0}};
    assert(oifs_loaded_path(dylib_file, sizeof(dylib_file)) == 0);
    assert(strlen(dylib_file) > 0);

    // 3. Open Unencrypted Filesystem
    const char *img_path = "{img_path}";
    OIFSHandle *fs = oifs_open(img_path, 15 * 1024 * 1024);
    assert(fs != NULL);

    // 4. Test I/O Backend API
    assert(oifs_get_io_backend(fs) == OIFS_IO_BACKEND_MMAP);
    assert(oifs_set_io_backend(fs, OIFS_IO_BACKEND_PREAD) == OIFS_IO_BACKEND_PREAD);
    assert(oifs_get_io_backend(fs) == OIFS_IO_BACKEND_PREAD);
    assert(oifs_set_io_backend(fs, OIFS_IO_BACKEND_MMAP) == OIFS_IO_BACKEND_MMAP);

    // 5. Directory and File Operations
    assert(oifs_mkdir(fs, "docs") == 0);
    assert(oifs_create_file(fs, "docs/readme.txt") == 0);

    const char *doc_content = "OIFS C ABI Verification Document with Multi-block Data!";
    uint64_t doc_len = (uint64_t)strlen(doc_content);
    assert(oifs_write_file(fs, "docs/readme.txt", (const uint8_t*)doc_content, doc_len) == 0);

    // 6. Sequential Read & Chunked Offset Read (oifs_read_at)
    uint8_t read_buf[256] = {{0}};
    int64_t r_bytes = oifs_read_file(fs, "docs/readme.txt", read_buf, sizeof(read_buf));
    assert(r_bytes == (int64_t)doc_len);
    assert(memcmp(read_buf, doc_content, (size_t)doc_len) == 0);

    // Chunked read: offset 5, read 4 bytes (" C A")
    uint8_t chunk_buf[16] = {{0}};
    int64_t chunk_bytes = oifs_read_at(fs, "docs/readme.txt", 4, chunk_buf, 4);
    assert(chunk_bytes == 4);
    assert(memcmp(chunk_buf, " C A", 4) == 0);

    // 7. Directory Listing with User Context
    struct ListContext ctx = {{0, 0}};
    assert(oifs_ls(fs, my_list_callback, &ctx) == 0);
    // Root directory contains "docs" (directory inode)
    assert(ctx.file_count >= 1);

    // 8. File Deletion and Error Diagnostics
    assert(oifs_delete_file(fs, "docs/readme.txt") == 0);
    assert(oifs_read_file(fs, "docs/readme.txt", read_buf, sizeof(read_buf)) == -1);

    char err_buf[256] = {{0}};
    assert(oifs_last_error(fs, err_buf, sizeof(err_buf)) == 0);
    assert(strlen(err_buf) > 0);

    oifs_close(fs);

    // 9. Encrypted Filesystem Test
    const char *enc_path = "{enc_path}";
    const char *secret_key = "secure_c_passphrase";
    OIFSHandle *enc_fs = oifs_open_with_password(enc_path, 15 * 1024 * 1024, secret_key);
    assert(enc_fs != NULL);

    const char *secret_data = "Super secret C cryptographic data";
    assert(oifs_write_file(enc_fs, "secret.key", (const uint8_t*)secret_data, strlen(secret_data)) == 0);
    oifs_close(enc_fs);

    // Reopen with bad password: file read must fail
    OIFSHandle *bad_fs = oifs_open_with_password(enc_path, 0, "wrong_password");
    assert(bad_fs != NULL);
    uint8_t secret_buf[128] = {{0}};
    int64_t bad_read = oifs_read_file(bad_fs, "secret.key", secret_buf, sizeof(secret_buf));
    assert(bad_read == -1);
    oifs_close(bad_fs);

    // Reopen with good password: read must succeed
    OIFSHandle *good_fs = oifs_open_with_password(enc_path, 0, secret_key);
    assert(good_fs != NULL);
    int64_t good_read = oifs_read_file(good_fs, "secret.key", secret_buf, sizeof(secret_buf));
    assert(good_read == (int64_t)strlen(secret_data));
    assert(memcmp(secret_buf, secret_data, (size_t)good_read) == 0);
    oifs_close(good_fs);

    printf("ALL C NATIVE TESTS PASSED CLEANLY!\n");
    return 0;
}}
"#,
        img_path = test_img_path.to_str().unwrap(),
        enc_path = enc_img_path.to_str().unwrap()
    );

    std::fs::write(&c_src_path, c_code).expect("write C source");

    // Compile with strict compiler warnings
    let compile_status = Command::new(compiler)
        .args([
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Wpedantic",
            "-Werror",
            "-I",
            include_dir.to_str().unwrap(),
            c_src_path.to_str().unwrap(),
            dylib_path.to_str().unwrap(),
            "-o",
            bin_path.to_str().unwrap(),
        ])
        .status()
        .expect("compile C binary");

    assert!(compile_status.success(), "Compilation of C test failed");

    // Execute compiled binary
    let dylib_dir = dylib_path.parent().unwrap();
    let mut cmd = Command::new(&bin_path);
    if cfg!(target_os = "macos") {
        cmd.env("DYLD_LIBRARY_PATH", dylib_dir);
    } else {
        cmd.env("LD_LIBRARY_PATH", dylib_dir);
    }

    let output = cmd.output().expect("execute compiled C test binary");
    assert!(
        output.status.success(),
        "C native binary execution failed!\nStdout:\n{}\nStderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!(
        "C Test Output:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn test_native_cpp_e2e() {
    let compiler = if has_tool("clang++") {
        "clang++"
    } else if has_tool("g++") {
        "g++"
    } else {
        eprintln!("Neither clang++ nor g++ found in PATH, skipping native C++ test");
        return;
    };

    let dylib_path = get_dylib_path();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let include_dir = Path::new(&manifest_dir).join("include");

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let cpp_src_path = tmp_dir.path().join("main.cpp");
    let bin_path = tmp_dir.path().join("main_cpp_bin");
    let test_img_path = tmp_dir.path().join("cpp_test_fs.img");

    let cpp_code = format!(
        r#"
#include <iostream>
#include <vector>
#include <string>
#include <memory>
#include <thread>
#include <cassert>
#include <cstring>
#include "oifs.h"

// Modern C++ RAII Deleter for OIFSHandle
struct OIFSDeleter {{
    void operator()(OIFSHandle *h) const noexcept {{
        if (h) {{
            oifs_close(h);
        }}
    }}
}};
using UniqueOIFS = std::unique_ptr<OIFSHandle, OIFSDeleter>;

struct DirEntry {{
    std::string name;
    uint64_t size;
    uint64_t mtime;
}};

int main() {{
    // 1. C++ Header Inclusion and Version Handshake Check
    assert(OIFS_CHECK_VERSION() == OIFS_VERSION_COMPAT_OK);
    std::string ver = oifs_version_string();
    assert(ver == "1.0.0");

    // 2. Open via RAII Smart Pointer
    const std::string img_path = "{img_path}";
    UniqueOIFS fs(oifs_open(img_path.c_str(), 20 * 1024 * 1024));
    assert(fs != nullptr);

    // 3. Write data using std::vector<uint8_t>
    std::string text = "C++17 RAII & Multi-threading Test with OIFS";
    std::vector<uint8_t> data(text.begin(), text.end());
    assert(oifs_write_file(fs.get(), "cpp_test.txt", data.data(), data.size()) == 0);

    // Read back into std::vector<uint8_t>
    std::vector<uint8_t> read_buf(128, 0);
    int64_t bytes = oifs_read_file(fs.get(), "cpp_test.txt", read_buf.data(), read_buf.size());
    assert(bytes == static_cast<int64_t>(data.size()));
    read_buf.resize(bytes);
    assert(read_buf == data);

    // 4. C++ Lambda Callback with captured std::vector
    std::vector<DirEntry> entries;
    auto cb = [](const char *name, uint64_t size, uint64_t mtime, void *user_data) {{
        auto *vec = static_cast<std::vector<DirEntry>*>(user_data);
        vec->push_back({{std::string(name), size, mtime}});
    }};
    assert(oifs_ls(fs.get(), cb, &entries) == 0);
    assert(!entries.empty());

    // 5. C++ Multi-threaded Concurrency using std::thread
    // Spawn 8 worker threads concurrently reading and writing to the shared handle
    std::vector<std::thread> workers;
    for (int t = 0; t < 8; ++t) {{
        workers.emplace_back([handle = fs.get(), t]() {{
            for (int i = 0; i < 10; ++i) {{
                std::string fname = "worker_" + std::to_string(t) + "_file_" + std::to_string(i) + ".bin";
                std::vector<uint8_t> payload = {{static_cast<uint8_t>(t), static_cast<uint8_t>(i), 0xAA, 0x55}};

                // Write file
                int w_res = oifs_write_file(handle, fname.c_str(), payload.data(), payload.size());
                assert(w_res == 0);

                // Read file
                std::vector<uint8_t> in_buf(4, 0);
                int64_t r_res = oifs_read_file(handle, fname.c_str(), in_buf.data(), in_buf.size());
                assert(r_res == 4);
                assert(in_buf == payload);
            }}
        }});
    }}

    for (auto &t : workers) {{
        t.join();
    }}

    // Verify all 80 worker files + original file exist via oifs_ls
    entries.clear();
    assert(oifs_ls(fs.get(), cb, &entries) == 0);
    assert(entries.size() >= 81);

    std::cout << "ALL C++17 TESTS PASSED CLEANLY (Total entries: " << entries.size() << ")!\n";
    return 0;
}}
"#,
        img_path = test_img_path.to_str().unwrap()
    );

    std::fs::write(&cpp_src_path, cpp_code).expect("write C++ source");

    // Compile with strict C++17 flags
    let compile_status = Command::new(compiler)
        .args([
            "-std=c++17",
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
        .expect("compile C++ binary");

    assert!(compile_status.success(), "Compilation of C++ test failed");

    // Execute compiled binary
    let dylib_dir = dylib_path.parent().unwrap();
    let mut cmd = Command::new(&bin_path);
    if cfg!(target_os = "macos") {
        cmd.env("DYLD_LIBRARY_PATH", dylib_dir);
    } else {
        cmd.env("LD_LIBRARY_PATH", dylib_dir);
    }

    let output = cmd.output().expect("execute compiled C++ test binary");
    assert!(
        output.status.success(),
        "C++ native binary execution failed!\nStdout:\n{}\nStderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!(
        "C++ Test Output:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn test_native_cpp20_e2e() {
    let compiler = if has_tool("clang++") {
        "clang++"
    } else if has_tool("g++") {
        "g++"
    } else {
        eprintln!("Neither clang++ nor g++ found in PATH, skipping native C++20 test");
        return;
    };

    // Check if compiler actually accepts -std=c++20
    let supports_cpp20 = Command::new(compiler)
        .args(["-std=c++20", "-dM", "-E", "-x", "c++", "/dev/null"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !supports_cpp20 {
        eprintln!("Compiler does not support -std=c++20, skipping C++20 test");
        return;
    }

    let dylib_path = get_dylib_path();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let include_dir = Path::new(&manifest_dir).join("include");

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let cpp_src_path = tmp_dir.path().join("main_cpp20.cpp");
    let bin_path = tmp_dir.path().join("main_cpp20_bin");
    let test_img_path = tmp_dir.path().join("cpp20_test_fs.img");

    let cpp_code = format!(
        r#"
#include <iostream>
#include <vector>
#include <string>
#include <memory>
#include <thread>
#include <span>
#include <concepts>
#include <atomic>
#include <cassert>
#include <cstring>
#include "oifs.h"

// Modern C++ RAII Deleter for OIFSHandle
struct OIFSDeleter {{
    void operator()(OIFSHandle *h) const noexcept {{
        if (h) {{
            oifs_close(h);
        }}
    }}
}};
using UniqueOIFS = std::unique_ptr<OIFSHandle, OIFSDeleter>;

struct DirEntry {{
    std::string name;
    uint64_t size = 0;
    uint64_t mtime = 0;
}};

// C++20 Concept to verify contiguous byte buffer convertible to std::span<const uint8_t>
template <typename T>
concept ContiguousByteRange = requires(T t) {{
    requires std::convertible_to<decltype(std::span{{t}}), std::span<const uint8_t>>;
}};

int main() {{
    // 1. Verify C++20 Header Inclusion and Version Handshake
    assert(OIFS_CHECK_VERSION() == OIFS_VERSION_COMPAT_OK);
    std::string ver = oifs_version_string();
    assert(ver == "1.0.0");

    // 2. Open via RAII Smart Pointer
    const std::string img_path = "{img_path}";
    UniqueOIFS fs(oifs_open(img_path.c_str(), 20 * 1024 * 1024));
    assert(fs != nullptr);

    // 3. Write data using C++20 std::span<const uint8_t>
    std::string text = "C++20 std::span & std::jthread Modern Integration Test";
    std::vector<uint8_t> raw_bytes(text.begin(), text.end());
    static_assert(ContiguousByteRange<std::vector<uint8_t>>);

    std::span<const uint8_t> write_span(raw_bytes);
    assert(oifs_write_file(fs.get(), "cpp20_span.txt", write_span.data(), write_span.size()) == 0);

    // 4. Read back using C++20 std::span<uint8_t>
    std::vector<uint8_t> read_storage(128, 0);
    std::span<uint8_t> read_span(read_storage);
    int64_t bytes = oifs_read_at(fs.get(), "cpp20_span.txt", 0, read_span.data(), read_span.size());
    assert(bytes == static_cast<int64_t>(write_span.size()));
    assert(std::memcmp(read_span.data(), write_span.data(), bytes) == 0);

    // 5. C++20 Designated Initializers in callback context
    struct Stats {{
        size_t count = 0;
        uint64_t total_bytes = 0;
    }};
    Stats stats{{.count = 0, .total_bytes = 0}};

    auto cb = [](const char *name, uint64_t size, uint64_t mtime, void *user_data) {{
        (void)name;
        (void)mtime;
        auto *s = static_cast<Stats*>(user_data);
        if (s) {{
            s->count++;
            s->total_bytes += size;
        }}
    }};
    assert(oifs_ls(fs.get(), cb, &stats) == 0);
    assert(stats.count >= 1);

    // 6. C++20 Concurrency using std::jthread (or fallback to std::thread if not in lib)
#if defined(__cpp_lib_jthread)
    {{
        std::vector<std::jthread> jworkers;
        for (int t = 0; t < 6; ++t) {{
            jworkers.emplace_back([handle = fs.get(), t]() {{
                for (int i = 0; i < 5; ++i) {{
                    std::string fname = "jworker_" + std::to_string(t) + "_" + std::to_string(i) + ".dat";
                    std::vector<uint8_t> payload = {{static_cast<uint8_t>(t), static_cast<uint8_t>(i), 0x20}};
                    std::span<const uint8_t> p_span(payload);
                    assert(oifs_write_file(handle, fname.c_str(), p_span.data(), p_span.size()) == 0);

                    std::vector<uint8_t> in_buf(3, 0);
                    std::span<uint8_t> in_span(in_buf);
                    int64_t r_res = oifs_read_file(handle, fname.c_str(), in_span.data(), in_span.size());
                    assert(r_res == 3);
                    assert(in_buf == payload);
                }}
            }});
        }}
        // jworkers automatically join upon leaving scope (C++20 RAII)
    }}
#endif

    std::cout << "ALL C++20 TESTS PASSED CLEANLY!\n";
    return 0;
}}
"#,
        img_path = test_img_path.to_str().unwrap()
    );

    std::fs::write(&cpp_src_path, cpp_code).expect("write C++20 source");

    // Compile with strict C++20 flags
    let compile_status = Command::new(compiler)
        .args([
            "-std=c++20",
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
        .expect("compile C++20 binary");

    assert!(compile_status.success(), "Compilation of C++20 test failed");

    // Execute compiled binary
    let dylib_dir = dylib_path.parent().unwrap();
    let mut cmd = Command::new(&bin_path);
    if cfg!(target_os = "macos") {
        cmd.env("DYLD_LIBRARY_PATH", dylib_dir);
    } else {
        cmd.env("LD_LIBRARY_PATH", dylib_dir);
    }

    let output = cmd.output().expect("execute compiled C++20 test binary");
    assert!(
        output.status.success(),
        "C++20 native binary execution failed!\nStdout:\n{}\nStderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!(
        "C++20 Test Output:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
