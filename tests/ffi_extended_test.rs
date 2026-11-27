use oifs::ffi::{
    OIFS_CIPHER_AES256_GCM, OIFS_CIPHER_DEFAULT, OIFS_CIPHER_XCHACHA20_POLY1305,
    OIFS_RESIZE_METHOD_IN_PLACE, OIFS_WRITE_POLICY_DEFAULT, OIFS_WRITE_POLICY_RAW,
    OIFS_WRITE_POLICY_SEEKABLE_64K, OIFSAutoGrowConfig, OIFSBatchCommitResult, OIFSByteSlice,
    OIFSFilterConfig, OIFSResizeStats, OIFSVacuumOptions, OIFSVacuumStats, oifs_batch_begin,
    oifs_batch_commit, oifs_batch_create_file_with_data, oifs_batch_delete, oifs_batch_free,
    oifs_batch_len, oifs_batch_mkdir, oifs_batch_rename, oifs_check_version, oifs_clear_zstd_dicts,
    oifs_close, oifs_default_zstd_dict_id, oifs_encryption_cipher, oifs_flush, oifs_get_auto_grow,
    oifs_grow, oifs_image_size_bytes, oifs_last_error, oifs_loaded_path, oifs_mkdir, oifs_open,
    oifs_open_read_only, oifs_open_with_cipher, oifs_open_with_password, oifs_read_file,
    oifs_register_zstd_dict, oifs_registered_zstd_dict_ids, oifs_resize, oifs_set_auto_grow,
    oifs_set_default_zstd_dict, oifs_train_zstd_dict, oifs_train_zstd_dict_from_dir,
    oifs_unregister_zstd_dict, oifs_vacuum, oifs_version_code, oifs_version_major,
    oifs_version_minor, oifs_version_patch, oifs_version_string, oifs_write_file,
};
use std::ffi::{CStr, CString};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[test]
fn test_ffi_extended_flow() {
    let path_str = "test_ffi_extended.img";
    let path = Path::new(path_str);
    let total_size = 10 * 1024 * 1024; // 10MB
    let password = "ffi_secure_pass";

    if path.exists() {
        fs::remove_file(path).unwrap();
    }

    // 1. Setup an encrypted image using the Rust API first
    {
        let _dm =
            oifs::disk::DiskManager::create_encrypted(path_str, total_size, password).unwrap();
    }

    // 2. Open via C FFI using the correct password (should succeed)
    let c_path = CString::new(path_str).unwrap();
    let c_correct_pass = CString::new(password).unwrap();
    let handle = oifs_open_with_password(c_path.as_ptr(), 0, c_correct_pass.as_ptr());
    assert!(
        !handle.is_null(),
        "FFI open with correct password should succeed"
    );

    // 3. Test oifs_mkdir via FFI
    let c_dir = CString::new("docs").unwrap();
    let mkdir_res = oifs_mkdir(handle, c_dir.as_ptr());
    assert_eq!(mkdir_res, 0, "oifs_mkdir should succeed");

    // 4. Test oifs_write_file via FFI
    let c_file = CString::new("docs/memo.txt").unwrap();
    let content = "Hello FFI Encrypted World!";
    let write_res = oifs_write_file(
        handle,
        c_file.as_ptr(),
        content.as_ptr(),
        content.len() as u64,
    );
    assert_eq!(write_res, 0, "oifs_write_file should succeed");

    // 5. Test oifs_read_file via FFI (should succeed)
    let mut read_buf = vec![0u8; 100];
    let bytes_read = oifs_read_file(
        handle,
        c_file.as_ptr(),
        read_buf.as_mut_ptr(),
        read_buf.len() as u64,
    );
    assert!(bytes_read > 0, "oifs_read_file should read > 0 bytes");
    let read_content = String::from_utf8(read_buf[..bytes_read as usize].to_vec()).unwrap();
    assert_eq!(
        read_content, content,
        "Read content should match written content exactly"
    );

    // 6. Open via C FFI using a wrong password (must reject at mount via header_mac)
    let c_wrong_pass = CString::new("wrong_password").unwrap();
    let handle_wrong = oifs_open_with_password(c_path.as_ptr(), 0, c_wrong_pass.as_ptr());
    assert!(
        handle_wrong.is_null(),
        "FFI open with wrong password must fail at mount"
    );

    // 7. Test oifs_last_error on a fake file read on the good handle
    let c_fake_file = CString::new("fake.txt").unwrap();
    let mut err_buf = vec![0u8; 100];
    let read_fake_res = oifs_read_file(
        handle,
        c_fake_file.as_ptr(),
        err_buf.as_mut_ptr(),
        err_buf.len() as u64,
    );
    assert_eq!(
        read_fake_res, -1,
        "Reading non-existent file should return -1"
    );

    let mut err_msg_buf = vec![0 as std::os::raw::c_char; 200];
    let err_res = oifs_last_error(handle, err_msg_buf.as_mut_ptr(), err_msg_buf.len() as u32);
    assert_eq!(err_res, 0);

    let err_msg = unsafe { CStr::from_ptr(err_msg_buf.as_ptr()) }
        .to_str()
        .unwrap();
    println!("Caught last FFI error: {}", err_msg);
    assert!(
        err_msg.contains("NotFound")
            || err_msg.contains("not found")
            || err_msg.contains("IO error"),
        "Error message should describe failure"
    );

    // 8. Close handles and clean up
    oifs_close(handle);
    fs::remove_file(path).unwrap();
}

#[test]
fn test_ffi_version_handshake_compatibility() {
    let major = oifs_version_major();
    let minor = oifs_version_minor();
    let patch = oifs_version_patch();
    let code = oifs_version_code();
    let version_str = unsafe { CStr::from_ptr(oifs_version_string()) }
        .to_str()
        .unwrap();

    assert_eq!(major, 1);
    assert_eq!(minor, 7);
    assert_eq!(patch, 0);
    assert_eq!(version_str, "1.7.0");
    assert!(code > 0);

    // Rule: "舊版本 error out (-1)，新版本 warning (1)，預期版本就沒事 (0)"

    // 1. Exact expected version match -> 0 (OK)
    assert_eq!(
        oifs_check_version(major, minor, patch),
        oifs::ffi::OIFS_VERSION_COMPAT_OK,
        "Exact version must return OK (0)"
    );

    // 2. Newer library loaded than requested -> 1 (WARN)
    // Binary requests 0.0.9, loaded library is 0.1.0 (newer)
    assert_eq!(
        oifs_check_version(0, 0, 9),
        oifs::ffi::OIFS_VERSION_COMPAT_WARN,
        "Newer library must return WARN (1)"
    );
    assert_eq!(
        oifs_check_version(0, 0, 1),
        oifs::ffi::OIFS_VERSION_COMPAT_WARN,
        "Newer library must return WARN (1)"
    );

    // 3. Older library loaded than requested -> -1 (ERR)
    // Binary requests 0.1.1, loaded library is 0.1.0 (older / outdated)
    assert_eq!(
        oifs_check_version(major, minor, patch + 1),
        oifs::ffi::OIFS_VERSION_COMPAT_ERR,
        "Outdated library must return ERR (-1)"
    );
    assert_eq!(
        oifs_check_version(major, minor + 1, 0),
        oifs::ffi::OIFS_VERSION_COMPAT_ERR,
        "Outdated library must return ERR (-1)"
    );
    assert_eq!(
        oifs_check_version(major + 1, 0, 0),
        oifs::ffi::OIFS_VERSION_COMPAT_ERR,
        "Outdated library must return ERR (-1)"
    );

    // 4. Test oifs_loaded_path returns non-empty path
    let mut path_buf = vec![0 as std::os::raw::c_char; 1024];
    let ret = oifs_loaded_path(path_buf.as_mut_ptr(), path_buf.len());
    assert_eq!(ret, 0, "oifs_loaded_path should succeed");
    let loaded_path = unsafe { CStr::from_ptr(path_buf.as_ptr()) }
        .to_str()
        .unwrap();
    println!("oifs_loaded_path reported: {}", loaded_path);
    assert!(!loaded_path.is_empty(), "Loaded path should not be empty");
}

#[test]
fn test_ffi_autogrow_resize_vacuum_batch_and_zstd_dict() {
    let dir = tempfile::tempdir().unwrap();
    let img_path = dir.path().join("ffi_v161_features.img");
    let c_path = CString::new(img_path.to_str().unwrap()).unwrap();

    let handle = oifs_open(c_path.as_ptr(), 4 * 1024 * 1024);
    assert!(!handle.is_null());

    // 1. Auto-Grow & Resize & Vacuum
    assert_eq!(oifs_image_size_bytes(handle), 4 * 1024 * 1024);
    let ag = OIFSAutoGrowConfig {
        enabled: 1,
        allow_relayout: 1,
        _pad: [0; 6],
        growth_factor: 1.5,
        min_increment_bytes: 2 * 1024 * 1024,
        max_size_bytes: 16 * 1024 * 1024,
    };
    assert_eq!(oifs_set_auto_grow(handle, &ag), 0);

    let mut ag_out = OIFSAutoGrowConfig::default();
    assert_eq!(oifs_get_auto_grow(handle, &mut ag_out), 0);
    assert_eq!(ag_out.enabled, 1);
    assert_eq!(ag_out.max_size_bytes, 16 * 1024 * 1024);

    let mut resize_stats = OIFSResizeStats::default();
    assert_eq!(oifs_grow(handle, 8 * 1024 * 1024, &mut resize_stats), 0);
    assert_eq!(resize_stats.bytes_after, 8 * 1024 * 1024);
    assert_eq!(resize_stats.method, OIFS_RESIZE_METHOD_IN_PLACE);
    assert_eq!(oifs_image_size_bytes(handle), 8 * 1024 * 1024);

    // 2. WriteBatch (mixed ops + created inode capture + double-commit & null safety)
    let c_dir = CString::new("logs").unwrap();
    assert_eq!(oifs_mkdir(handle, c_dir.as_ptr()), 0);

    let batch = oifs_batch_begin(handle);
    assert!(!batch.is_null());
    assert_eq!(oifs_batch_len(batch), 0);

    let c_sub = CString::new("logs/sub").unwrap();
    let c_f1 = CString::new("sample1.json").unwrap();
    let c_f2 = CString::new("sample2.json").unwrap();
    let c_f2_renamed = CString::new("logs/sample2.json").unwrap();
    let c_f3 = CString::new("drop_me.txt").unwrap();

    let s1 = br#"{"service":"oifs","level":"info","msg":"batch-one-payload-0001"}"#;
    let s2 = br#"{"service":"oifs","level":"warn","msg":"batch-two-payload-0002"}"#;
    let filter_none = OIFSFilterConfig::default();

    assert_eq!(oifs_batch_mkdir(batch, c_sub.as_ptr()), 0);
    assert_eq!(
        oifs_batch_create_file_with_data(
            batch,
            c_f1.as_ptr(),
            s1.as_ptr(),
            s1.len() as u64,
            OIFS_WRITE_POLICY_RAW,
            0,
            std::ptr::null(),
        ),
        0
    );
    assert_eq!(
        oifs_batch_create_file_with_data(
            batch,
            c_f2.as_ptr(),
            s2.as_ptr(),
            s2.len() as u64,
            OIFS_WRITE_POLICY_SEEKABLE_64K,
            1,
            &filter_none,
        ),
        0
    );
    assert_eq!(
        oifs_batch_create_file_with_data(
            batch,
            c_f3.as_ptr(),
            b"bye".as_ptr(),
            3,
            OIFS_WRITE_POLICY_DEFAULT,
            0,
            std::ptr::null(),
        ),
        0
    );
    assert_eq!(
        oifs_batch_rename(batch, c_f2.as_ptr(), c_f2_renamed.as_ptr()),
        0
    );
    assert_eq!(oifs_batch_delete(batch, c_f3.as_ptr()), 0);
    assert_eq!(oifs_batch_len(batch), 6);

    let mut commit_res = OIFSBatchCommitResult::default();
    let mut created_inodes = [0u64; 8];
    assert_eq!(
        oifs_batch_commit(
            batch,
            &mut commit_res,
            created_inodes.as_mut_ptr(),
            created_inodes.len(),
        ),
        0
    );
    assert_eq!(commit_res.ops_executed, 6);
    assert_eq!(commit_res.created_count, 4);
    assert!(created_inodes[..4].iter().all(|&id| id > 0));

    // Re-committing an already committed batch must fail cleanly
    assert_eq!(
        oifs_batch_commit(batch, std::ptr::null_mut(), std::ptr::null_mut(), 0),
        -1
    );
    oifs_batch_free(batch);

    // Atomic rollback check: one invalid delete prevents all other ops in batch
    let bad_batch = oifs_batch_begin(handle);
    let c_should_not_exist = CString::new("should_not_exist.txt").unwrap();
    let c_missing = CString::new("does_not_exist_anywhere.txt").unwrap();
    assert_eq!(
        oifs_batch_create_file_with_data(
            bad_batch,
            c_should_not_exist.as_ptr(),
            b"abc".as_ptr(),
            3,
            OIFS_WRITE_POLICY_DEFAULT,
            0,
            std::ptr::null(),
        ),
        0
    );
    assert_eq!(oifs_batch_delete(bad_batch, c_missing.as_ptr()), 0);
    assert_eq!(
        oifs_batch_commit(bad_batch, std::ptr::null_mut(), std::ptr::null_mut(), 0),
        -1
    );
    oifs_batch_free(bad_batch);
    let mut tmp_read = [0u8; 8];
    assert_eq!(
        oifs_read_file(
            handle,
            c_should_not_exist.as_ptr(),
            tmp_read.as_mut_ptr(),
            tmp_read.len() as u64,
        ),
        -1
    );

    // 3. Zstd dictionary train (from samples + from dir), register, default, ids, unregister, clear
    let mut raw_samples = Vec::new();
    for i in 0..16u32 {
        raw_samples.push(
            format!(
                "{{\"event\":\"heartbeat\",\"worker\":{i},\"region\":\"ap-east-1\",\"ok\":true}}"
            )
            .into_bytes(),
        );
    }
    let slice_vec: Vec<OIFSByteSlice> = raw_samples
        .iter()
        .map(|v| OIFSByteSlice {
            data: v.as_ptr(),
            len: v.len(),
        })
        .collect();
    let mut dict_buf = vec![0u8; 4096];
    let trained_len = oifs_train_zstd_dict(
        slice_vec.as_ptr(),
        slice_vec.len(),
        4096,
        dict_buf.as_mut_ptr(),
        dict_buf.len(),
    );
    assert!(trained_len > 0);
    let dict_bytes = &dict_buf[..trained_len as usize];

    let dict_id = oifs_register_zstd_dict(handle, dict_bytes.as_ptr(), dict_bytes.len(), 1);
    assert!(dict_id > 0);
    assert_eq!(oifs_default_zstd_dict_id(handle), dict_id);

    let mut ids = [0u32; 8];
    let n_ids = oifs_registered_zstd_dict_ids(handle, ids.as_mut_ptr(), ids.len());
    assert_eq!(n_ids, 1);
    assert_eq!(ids[0], dict_id as u32);

    // Write small dict-compressed file and verify roundtrip
    let c_dict_file = CString::new("dict_small.json").unwrap();
    let payload = &raw_samples[5];
    assert_eq!(
        oifs_write_file(
            handle,
            c_dict_file.as_ptr(),
            payload.as_ptr(),
            payload.len() as u64
        ),
        0
    );
    let mut back = vec![0u8; payload.len()];
    let got = oifs_read_file(
        handle,
        c_dict_file.as_ptr(),
        back.as_mut_ptr(),
        back.len() as u64,
    );
    assert_eq!(got, payload.len() as i64);
    assert_eq!(&back, payload);

    // Train another dictionary directly from the root directory in the image
    let c_dir_samples = CString::new("corpus").unwrap();
    assert_eq!(oifs_mkdir(handle, c_dir_samples.as_ptr()), 0);
    let dir_batch = oifs_batch_begin(handle);
    for i in 0..10u32 {
        let cname = CString::new(format!("corpus/r_{i}.json")).unwrap();
        let data = format!(
            "{{\"kind\":\"telemetry\",\"node\":\"host-{i:02}\",\"status\":\"healthy\",\"zone\":\"tw\"}}"
        );
        assert_eq!(
            oifs_batch_create_file_with_data(
                dir_batch,
                cname.as_ptr(),
                data.as_ptr(),
                data.len() as u64,
                OIFS_WRITE_POLICY_RAW,
                0,
                std::ptr::null(),
            ),
            0
        );
    }
    assert_eq!(
        oifs_batch_commit(dir_batch, std::ptr::null_mut(), std::ptr::null_mut(), 0),
        0
    );
    oifs_batch_free(dir_batch);

    let mut dir_dict_buf = vec![0u8; 4096];
    let dir_dict_len = oifs_train_zstd_dict_from_dir(
        handle,
        c_dir_samples.as_ptr(),
        4096,
        32,
        dir_dict_buf.as_mut_ptr(),
        dir_dict_buf.len(),
    );
    assert!(dir_dict_len > 0);

    // Clear default, unregister, clear all
    assert_eq!(oifs_set_default_zstd_dict(handle, 0), 0);
    assert_eq!(oifs_default_zstd_dict_id(handle), 0);
    assert_eq!(oifs_unregister_zstd_dict(handle, dict_id as u32), 1);
    assert_eq!(oifs_unregister_zstd_dict(handle, dict_id as u32), 0);
    assert_eq!(oifs_clear_zstd_dicts(handle), 0);
    assert_eq!(
        oifs_registered_zstd_dict_ids(handle, std::ptr::null_mut(), 0),
        0
    );

    // Vacuum + resize down
    let vac_opts = OIFSVacuumOptions {
        min_headroom_bytes: 512 * 1024,
        min_image_bytes: 2 * 1024 * 1024,
        headroom_percent: 10,
        allow_relayout: 1,
        _pad: [0; 6],
    };
    let mut vac_stats = OIFSVacuumStats::default();
    assert_eq!(oifs_vacuum(handle, &vac_opts, &mut vac_stats), 0);
    assert!(vac_stats.bytes_after <= 8 * 1024 * 1024);
    let mut shrink_check = OIFSResizeStats::default();
    assert_eq!(oifs_resize(handle, 5 * 1024 * 1024, &mut shrink_check), 0);
    assert_eq!(oifs_image_size_bytes(handle), 5 * 1024 * 1024);

    // Re-register dict on read-only mount to verify read-only gate & dictionary decoding
    oifs_close(handle);
    let ro = oifs_open_read_only(c_path.as_ptr(), std::ptr::null(), 0);
    assert!(!ro.is_null());
    assert_eq!(oifs_set_auto_grow(ro, &ag), -1);
    assert!(oifs_batch_begin(ro).is_null());
    assert_eq!(oifs_grow(ro, 10 * 1024 * 1024, std::ptr::null_mut()), -1);
    assert_eq!(oifs_vacuum(ro, std::ptr::null(), std::ptr::null_mut()), -1);
    assert!(oifs_register_zstd_dict(ro, dict_bytes.as_ptr(), dict_bytes.len(), 0) == dict_id);
    let mut ro_back = vec![0u8; payload.len()];
    assert_eq!(
        oifs_read_file(
            ro,
            c_dict_file.as_ptr(),
            ro_back.as_mut_ptr(),
            ro_back.len() as u64
        ),
        payload.len() as i64
    );
    assert_eq!(&ro_back, payload);
    oifs_close(ro);
}

#[test]
fn test_ffi_selectable_cipher_suites() {
    let dir = tempdir().unwrap();
    let pw = CString::new("FfiCipher#2026").unwrap();

    for (fname, cipher_flag, expected_ver) in [
        (
            "c_default.img",
            OIFS_CIPHER_DEFAULT,
            OIFS_CIPHER_AES256_GCM as i32,
        ),
        (
            "c_v3.img",
            OIFS_CIPHER_AES256_GCM,
            OIFS_CIPHER_AES256_GCM as i32,
        ),
        (
            "c_v2.img",
            OIFS_CIPHER_XCHACHA20_POLY1305,
            OIFS_CIPHER_XCHACHA20_POLY1305 as i32,
        ),
    ] {
        let p = dir.path().join(fname);
        let c_p = CString::new(p.to_str().unwrap()).unwrap();
        let h = oifs_open_with_cipher(
            c_p.as_ptr(),
            4 * 1024 * 1024,
            pw.as_ptr(),
            0,
            1,
            cipher_flag,
        );
        assert!(!h.is_null());
        assert_eq!(oifs_encryption_cipher(h), expected_ver);

        let cname = CString::new("secret.txt").unwrap();
        let payload = b"ffi selectable cipher payload";
        assert_eq!(
            oifs_write_file(h, cname.as_ptr(), payload.as_ptr(), payload.len() as u64),
            0
        );
        assert_eq!(oifs_flush(h), 0);
        oifs_close(h);

        // Reopen with generic options auto-detects cipher
        let h2 = oifs_open_read_only(c_p.as_ptr(), pw.as_ptr(), 0);
        assert!(!h2.is_null());
        assert_eq!(oifs_encryption_cipher(h2), expected_ver);
        let mut back = vec![0u8; payload.len()];
        assert_eq!(
            oifs_read_file(h2, cname.as_ptr(), back.as_mut_ptr(), back.len() as u64),
            payload.len() as i64
        );
        assert_eq!(&back, payload);
        oifs_close(h2);
    }

    // Invalid cipher ID must fail cleanly
    let bad_p = CString::new(dir.path().join("bad_cipher.img").to_str().unwrap()).unwrap();
    assert!(
        oifs_open_with_cipher(bad_p.as_ptr(), 4 * 1024 * 1024, pw.as_ptr(), 0, 0, 99).is_null()
    );
    assert_eq!(oifs_encryption_cipher(std::ptr::null()), -1);
}
