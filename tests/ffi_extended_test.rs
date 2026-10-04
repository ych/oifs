use oifs::ffi::{
    oifs_close, oifs_last_error, oifs_mkdir, oifs_open_with_password, oifs_read_file,
    oifs_write_file,
};
use std::ffi::{CStr, CString};
use std::fs;
use std::path::Path;

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

    // 6. Open via C FFI using a wrong password
    // Argon2 KDF allows opening (it derives *some* key), but file decryptions will fail.
    let c_wrong_pass = CString::new("wrong_password").unwrap();
    let handle_wrong = oifs_open_with_password(c_path.as_ptr(), 0, c_wrong_pass.as_ptr());
    assert!(
        !handle_wrong.is_null(),
        "FFI open with wrong password should return a handle"
    );

    // 7. Try to read with the wrong handle (must fail with DecryptionFailed)
    let mut read_buf_wrong = vec![0u8; 100];
    let bytes_read_wrong = oifs_read_file(
        handle_wrong,
        c_file.as_ptr(),
        read_buf_wrong.as_mut_ptr(),
        read_buf_wrong.len() as u64,
    );
    assert_eq!(
        bytes_read_wrong, -1,
        "Reading file with wrong password must fail"
    );

    // 8. Verify error diagnostics on wrong handle
    let mut err_msg_buf_wrong = vec![0 as std::os::raw::c_char; 200];
    let err_res_wrong = oifs_last_error(
        handle_wrong,
        err_msg_buf_wrong.as_mut_ptr(),
        err_msg_buf_wrong.len() as u32,
    );
    assert_eq!(err_res_wrong, 0);
    let err_msg_wrong = unsafe { CStr::from_ptr(err_msg_buf_wrong.as_ptr()) }
        .to_str()
        .unwrap();
    println!("Caught expected wrong-password error: {}", err_msg_wrong);
    assert!(
        err_msg_wrong.to_lowercase().contains("decryption")
            || err_msg_wrong.to_lowercase().contains("not found"),
        "Error message should describe failure with wrong password (either filename lookup or data decryption failure)"
    );

    // 9. Test oifs_last_error on a fake file read on the good handle
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

    // 10. Close handles and clean up
    oifs_close(handle);
    oifs_close(handle_wrong);
    fs::remove_file(path).unwrap();
}
