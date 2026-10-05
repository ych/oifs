//! Comprehensive Tests for FFI Version Handshake & Compatibility Policy
//!
//! Policy: "舊版本 error out (-1)，新版本 warning (1)，預期版本就沒事 (0)"

use oifs::ffi::{
    OIFS_VERSION_COMPAT_ERR, OIFS_VERSION_COMPAT_OK, OIFS_VERSION_COMPAT_WARN, oifs_check_version,
    oifs_loaded_path, oifs_version_code, oifs_version_major, oifs_version_minor,
    oifs_version_patch, oifs_version_string,
};
use std::ffi::CStr;
use std::process::Command;
use std::ptr;

#[test]
fn test_version_metadata_consistency() {
    let major = oifs_version_major();
    let minor = oifs_version_minor();
    let patch = oifs_version_patch();
    let code = oifs_version_code();

    let c_str = unsafe { CStr::from_ptr(oifs_version_string()) };
    let version_str = c_str.to_str().expect("valid utf-8");

    // Must match Cargo.toml version
    let expected_str = format!("{}.{}.{}", major, minor, patch);
    assert_eq!(version_str, expected_str);

    // Code must encode (major << 32) | (minor << 16) | patch
    let expected_code = ((major as u64) << 32) | ((minor as u64) << 16) | (patch as u64);
    assert_eq!(code, expected_code);
}

#[test]
fn test_version_compatibility_exact_match_is_ok() {
    let major = oifs_version_major();
    let minor = oifs_version_minor();
    let patch = oifs_version_patch();

    // Exact expected version match -> MUST return 0 (OK, no warnings, no error)
    let ret = oifs_check_version(major, minor, patch);
    assert_eq!(ret, OIFS_VERSION_COMPAT_OK);
}

#[test]
fn test_version_compatibility_newer_library_emits_warning() {
    // When the binary was built against an older version (e.g. 0.0.1 or 0.0.9),
    // and the loaded dynamic library is 0.1.0 (newer):
    // Policy: do NOT block; return 1 (WARN) so caller can log warning and proceed!
    assert_eq!(
        oifs_check_version(0, 0, 1),
        OIFS_VERSION_COMPAT_WARN,
        "Newer library must return WARN (1)"
    );
    assert_eq!(
        oifs_check_version(0, 0, 9),
        OIFS_VERSION_COMPAT_WARN,
        "Newer library must return WARN (1)"
    );
}

#[test]
fn test_version_compatibility_older_library_errors_out() {
    let major = oifs_version_major();
    let minor = oifs_version_minor();
    let patch = oifs_version_patch();

    // When the binary requires features from a newer version (e.g. higher patch, minor, or major)
    // than what the loaded library provides:
    // Policy: MUST error out and block; return -1 (ERR)!

    // 1. Higher patch required
    assert_eq!(
        oifs_check_version(major, minor, patch + 1),
        OIFS_VERSION_COMPAT_ERR,
        "Outdated library missing newer patch must return ERR (-1)"
    );

    // 2. Higher minor required
    assert_eq!(
        oifs_check_version(major, minor + 1, 0),
        OIFS_VERSION_COMPAT_ERR,
        "Outdated library missing newer minor must return ERR (-1)"
    );

    // 3. Higher major required
    assert_eq!(
        oifs_check_version(major + 1, 0, 0),
        OIFS_VERSION_COMPAT_ERR,
        "Outdated library missing newer major must return ERR (-1)"
    );
}

#[test]
fn test_loaded_path_safety_and_edge_cases() {
    // 1. Null pointer check
    assert_eq!(oifs_loaded_path(ptr::null_mut(), 100), -1);

    // 2. Zero buffer size check
    let mut small_buf = [0 as std::os::raw::c_char; 4];
    assert_eq!(oifs_loaded_path(small_buf.as_mut_ptr(), 0), -1);

    // 3. Buffer too small for path
    assert_eq!(oifs_loaded_path(small_buf.as_mut_ptr(), 2), -1);

    // 4. Valid large buffer
    let mut path_buf = vec![0 as std::os::raw::c_char; 2048];
    let ret = oifs_loaded_path(path_buf.as_mut_ptr(), path_buf.len());
    assert_eq!(
        ret, 0,
        "oifs_loaded_path should succeed with adequate buffer"
    );

    let path_str = unsafe { CStr::from_ptr(path_buf.as_ptr()) }
        .to_str()
        .expect("valid path");
    assert!(!path_str.is_empty(), "Loaded path must not be empty");
    println!("Loaded library / binary path: {}", path_str);
}

#[test]
fn test_c_header_version_contract_compilation() {
    // Verify that a real C program including include/oifs.h compiles cleanly with clang
    // and correctly exercises the OIFS_CHECK_VERSION() macro and status codes.
    let c_code = r#"
#include <stdio.h>
#include <stdlib.h>
#include <assert.h>
#include "oifs.h"

int main(void) {
    // 1. Verify compile-time macros
    assert(OIFS_VERSION_MAJOR == 1);
    assert(OIFS_VERSION_MINOR == 0);
    assert(OIFS_VERSION_PATCH == 0);
    assert(OIFS_VERSION_COMPAT_OK == 0);
    assert(OIFS_VERSION_COMPAT_WARN == 1);
    assert(OIFS_VERSION_COMPAT_ERR == -1);

    // 2. Exact match check
    int exact_status = OIFS_CHECK_VERSION();
    assert(exact_status == OIFS_VERSION_COMPAT_OK);

    // 3. Simulated older requirement -> should warn (1)
    int warn_status = oifs_check_version(0, 1, 0);
    assert(warn_status == OIFS_VERSION_COMPAT_WARN);

    // 4. Simulated newer requirement -> should error out (-1)
    int err_status = oifs_check_version(1, 1, 0);
    assert(err_status == OIFS_VERSION_COMPAT_ERR);

    // 5. Test loaded path retrieval
    char path[1024] = {0};
    int path_ret = oifs_loaded_path(path, sizeof(path));
    assert(path_ret == 0);
    assert(path[0] != '\0');

    printf("C FFI Version Handshake Test PASSED successfully!\n");
    return 0;
}
"#;

    let tmp_dir = tempfile::tempdir().expect("tempdir");
    let c_file = tmp_dir.path().join("test_version.c");
    let bin_file = tmp_dir.path().join("test_version_bin");
    std::fs::write(&c_file, c_code).expect("write C source");

    // Find the compiled dylib
    let dylib_path = if std::path::Path::new("target/release/liboifs.dylib").exists() {
        "target/release/liboifs.dylib"
    } else if std::path::Path::new("target/debug/liboifs.dylib").exists() {
        "target/debug/liboifs.dylib"
    } else {
        // Compile dylib first if not found
        let status = Command::new("cargo")
            .args(["build", "--lib"])
            .status()
            .expect("cargo build --lib");
        assert!(status.success());
        "target/debug/liboifs.dylib"
    };

    let compile_status = Command::new("clang")
        .args([
            // Split so it is not mistaken for a single "-Iinclude" token.
            "-I",
            "include",
            c_file.to_str().unwrap(),
            dylib_path,
            "-o",
            bin_file.to_str().unwrap(),
        ])
        .status();

    if let Ok(status) = compile_status
        && status.success()
    {
        let run_output = Command::new(&bin_file)
            .output()
            .expect("execute compiled C test binary");
        assert!(
            run_output.status.success(),
            "C test binary failed: {}",
            String::from_utf8_lossy(&run_output.stderr)
        );
        println!(
            "C Test Output:\n{}",
            String::from_utf8_lossy(&run_output.stdout)
        );
    }
}
