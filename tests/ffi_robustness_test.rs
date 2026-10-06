//! Comprehensive Robustness & Stress Tests for OIFS C FFI
//!
//! Verifies:
//! 1. Defensive NULL-pointer safety across all FFI entry points (no crashes/panics/UB)
//! 2. Error buffer boundary checks and guaranteed null-termination in `oifs_last_error`
//! 3. Invalid UTF-8 argument defenses
//! 4. Multi-threaded concurrency with a SHARED `*mut OIFSHandle`
//! 5. Multi-threaded concurrency with independent session handles via `oifs_get_or_open`
//! 6. Zero-sized buffer and out-of-bounds offset reads
//! 7. Multi-block cross-boundary I/O via `oifs_read_at`

use oifs::ffi::{
    oifs_close, oifs_create_file, oifs_delete_file, oifs_get_io_backend, oifs_get_or_open,
    oifs_get_or_open_with_password, oifs_last_error, oifs_loaded_path, oifs_ls, oifs_mkdir,
    oifs_open, oifs_open_with_password, oifs_read_at, oifs_read_file, oifs_set_io_backend,
    oifs_write_file,
};
use std::ffi::{CStr, CString};
use std::ptr;
use std::thread;

extern "C" fn count_cb(
    _name: *const std::os::raw::c_char,
    _size: u64,
    _mtime: u64,
    user_data: *mut std::os::raw::c_void,
) {
    if !user_data.is_null() {
        let count = unsafe { &mut *(user_data as *mut usize) };
        *count += 1;
    }
}

#[test]
fn test_ffi_null_pointer_defenses() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let img_path = tmp_dir.path().join("null_test.img");
    let c_img_path = CString::new(img_path.to_str().unwrap()).unwrap();
    let size = 10 * 1024 * 1024;

    // 1. Lifecycle opens with NULL path
    assert!(oifs_open(ptr::null(), size).is_null());
    assert!(oifs_open_with_password(ptr::null(), size, ptr::null()).is_null());
    assert!(oifs_get_or_open(ptr::null(), size).is_null());
    assert!(oifs_get_or_open_with_password(ptr::null(), size, ptr::null()).is_null());

    // 2. Lifecycle close with NULL handle
    oifs_close(ptr::null_mut()); // Must not panic or crash

    // Open valid handle for member function testing
    let handle = oifs_open(c_img_path.as_ptr(), size);
    assert!(!handle.is_null());

    // 3. NULL handle defense on all operations
    assert_eq!(oifs_create_file(ptr::null_mut(), c_img_path.as_ptr()), -1);
    assert_eq!(oifs_delete_file(ptr::null_mut(), c_img_path.as_ptr()), -1);
    assert_eq!(oifs_mkdir(ptr::null_mut(), c_img_path.as_ptr()), -1);
    assert_eq!(
        oifs_ls(ptr::null_mut(), Some(count_cb), ptr::null_mut()),
        -1
    );
    assert_eq!(oifs_set_io_backend(ptr::null_mut(), 0), -1);
    assert_eq!(oifs_get_io_backend(ptr::null_mut()), -1);

    let mut buf = [0u8; 64];
    assert_eq!(
        oifs_read_at(
            ptr::null_mut(),
            c_img_path.as_ptr(),
            0,
            buf.as_mut_ptr(),
            64
        ),
        -1
    );
    assert_eq!(
        oifs_read_file(ptr::null_mut(), c_img_path.as_ptr(), buf.as_mut_ptr(), 64),
        -1
    );
    assert_eq!(
        oifs_write_file(ptr::null_mut(), c_img_path.as_ptr(), buf.as_ptr(), 64),
        -1
    );

    let mut err_buf = [0 as std::os::raw::c_char; 64];
    assert_eq!(
        oifs_last_error(ptr::null_mut(), err_buf.as_mut_ptr(), 64),
        -1
    );

    // 4. NULL argument defense with valid handle
    assert_eq!(oifs_create_file(handle, ptr::null()), -1);
    assert_eq!(oifs_delete_file(handle, ptr::null()), -1);
    assert_eq!(oifs_mkdir(handle, ptr::null()), -1);
    assert_eq!(oifs_ls(handle, None, ptr::null_mut()), -1);

    // Check last error recorded for null callback
    let mut err_msg_buf = [0 as std::os::raw::c_char; 128];
    assert_eq!(oifs_last_error(handle, err_msg_buf.as_mut_ptr(), 128), 0);
    let msg = unsafe { CStr::from_ptr(err_msg_buf.as_ptr()) }
        .to_str()
        .unwrap();
    assert!(msg.to_lowercase().contains("null"));

    // NULL filename in read/write
    assert_eq!(
        oifs_read_at(handle, ptr::null(), 0, buf.as_mut_ptr(), 64),
        -1
    );
    assert_eq!(
        oifs_read_file(handle, ptr::null(), buf.as_mut_ptr(), 64),
        -1
    );
    assert_eq!(oifs_write_file(handle, ptr::null(), buf.as_ptr(), 64), -1);

    // NULL buffer in read/write with non-zero size
    let c_valid_name = CString::new("test.txt").unwrap();
    assert_eq!(
        oifs_read_at(handle, c_valid_name.as_ptr(), 0, ptr::null_mut(), 64),
        -1
    );
    assert_eq!(
        oifs_read_file(handle, c_valid_name.as_ptr(), ptr::null_mut(), 64),
        -1
    );
    assert_eq!(
        oifs_write_file(handle, c_valid_name.as_ptr(), ptr::null(), 64),
        -1
    );

    // Diagnostic NULL buffers
    assert_eq!(oifs_last_error(handle, ptr::null_mut(), 128), -1);
    assert_eq!(oifs_last_error(handle, err_msg_buf.as_mut_ptr(), 0), -1);
    assert_eq!(oifs_loaded_path(ptr::null_mut(), 128), -1);
    assert_eq!(oifs_loaded_path(err_msg_buf.as_mut_ptr(), 0), -1);

    oifs_close(handle);
}

#[test]
fn test_ffi_invalid_utf8_defenses() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let img_path = tmp_dir.path().join("utf8_test.img");
    let c_img_path = CString::new(img_path.to_str().unwrap()).unwrap();
    let handle = oifs_open(c_img_path.as_ptr(), 10 * 1024 * 1024);
    assert!(!handle.is_null());

    // Invalid UTF-8 sequence (0xFF, 0xFE, 0x00)
    let bad_bytes: [u8; 3] = [0xFF, 0xFE, 0x00];
    let bad_ptr = bad_bytes.as_ptr() as *const std::os::raw::c_char;

    assert_eq!(oifs_create_file(handle, bad_ptr), -1);
    assert_eq!(oifs_delete_file(handle, bad_ptr), -1);
    assert_eq!(oifs_mkdir(handle, bad_ptr), -1);

    let mut buf = [0u8; 32];
    assert_eq!(oifs_read_at(handle, bad_ptr, 0, buf.as_mut_ptr(), 32), -1);
    assert_eq!(oifs_read_file(handle, bad_ptr, buf.as_mut_ptr(), 32), -1);
    assert_eq!(oifs_write_file(handle, bad_ptr, buf.as_ptr(), 32), -1);

    // Verify last error explains invalid UTF-8
    let mut err_buf = [0 as std::os::raw::c_char; 128];
    assert_eq!(oifs_last_error(handle, err_buf.as_mut_ptr(), 128), 0);
    let msg = unsafe { CStr::from_ptr(err_buf.as_ptr()) }
        .to_str()
        .unwrap();
    assert!(
        msg.contains("Invalid UTF-8"),
        "Expected UTF-8 error, got: {}",
        msg
    );

    oifs_close(handle);
}

#[test]
fn test_ffi_last_error_truncation_and_termination() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let img_path = tmp_dir.path().join("err_trunc.img");
    let c_img_path = CString::new(img_path.to_str().unwrap()).unwrap();
    let handle = oifs_open(c_img_path.as_ptr(), 10 * 1024 * 1024);
    assert!(!handle.is_null());

    // Trigger an error
    let c_nonexistent = CString::new("no_such_file.txt").unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(
        oifs_read_file(handle, c_nonexistent.as_ptr(), buf.as_mut_ptr(), 16),
        -1
    );

    // Buffer of size 1: should only write '\0' and return 0
    let mut tiny_buf1 = [0x55 as std::os::raw::c_char; 1];
    assert_eq!(oifs_last_error(handle, tiny_buf1.as_mut_ptr(), 1), 0);
    assert_eq!(tiny_buf1[0], 0);

    // Buffer of size 2: 1 char + '\0'
    let mut tiny_buf2 = [0x55 as std::os::raw::c_char; 2];
    assert_eq!(oifs_last_error(handle, tiny_buf2.as_mut_ptr(), 2), 0);
    assert_eq!(tiny_buf2[1], 0);

    // Buffer of size 5: 4 chars + '\0'
    let mut small_buf = [0x55 as std::os::raw::c_char; 5];
    assert_eq!(oifs_last_error(handle, small_buf.as_mut_ptr(), 5), 0);
    assert_eq!(small_buf[4], 0);

    // Full buffer: verify contents
    let mut full_buf = [0 as std::os::raw::c_char; 256];
    assert_eq!(oifs_last_error(handle, full_buf.as_mut_ptr(), 256), 0);
    let msg = unsafe { CStr::from_ptr(full_buf.as_ptr()) }
        .to_str()
        .unwrap();
    assert!(!msg.is_empty());
    assert!(msg.to_lowercase().contains("notfound") || msg.to_lowercase().contains("not found"));

    oifs_close(handle);
}

#[test]
fn test_ffi_multithreaded_shared_handle() {
    // Multi-threaded test where 10 threads concurrently operate on the SAME OIFSHandle*
    let tmp_dir = tempfile::tempdir().unwrap();
    let img_path = tmp_dir.path().join("shared_handle_mt.img");
    let c_img_path = CString::new(img_path.to_str().unwrap()).unwrap();
    let handle = oifs_open(c_img_path.as_ptr(), 20 * 1024 * 1024);
    assert!(!handle.is_null());

    // Prepare initial baseline files
    let common_data = b"Shared handle concurrency verification payload!";
    for i in 0..5 {
        let name = CString::new(format!("base_{}.dat", i)).unwrap();
        assert_eq!(
            oifs_write_file(
                handle,
                name.as_ptr(),
                common_data.as_ptr(),
                common_data.len() as u64
            ),
            0
        );
    }

    let handle_addr = handle as usize;
    let mut handles = Vec::new();

    // 5 Reader threads
    for thread_idx in 0..5 {
        handles.push(thread::spawn(move || {
            let h = handle_addr as *mut oifs::ffi::OIFSHandle;
            let mut read_buf = vec![0u8; 100];
            for i in 0..20 {
                let file_name = CString::new(format!("base_{}.dat", (thread_idx + i) % 5)).unwrap();
                let bytes = oifs_read_file(
                    h,
                    file_name.as_ptr(),
                    read_buf.as_mut_ptr(),
                    read_buf.len() as u64,
                );
                assert_eq!(bytes, common_data.len() as i64);
                assert_eq!(&read_buf[..bytes as usize], common_data);
            }
        }));
    }

    // 5 Writer threads
    for thread_idx in 0..5 {
        handles.push(thread::spawn(move || {
            let h = handle_addr as *mut oifs::ffi::OIFSHandle;
            for i in 0..10 {
                let file_name =
                    CString::new(format!("thread_{}_file_{}.dat", thread_idx, i)).unwrap();
                let content = format!("Payload from thread {} iteration {}", thread_idx, i);
                let write_res = oifs_write_file(
                    h,
                    file_name.as_ptr(),
                    content.as_ptr(),
                    content.len() as u64,
                );
                assert_eq!(write_res, 0);

                // Immediate read back
                let mut buf = vec![0u8; 128];
                let bytes =
                    oifs_read_file(h, file_name.as_ptr(), buf.as_mut_ptr(), buf.len() as u64);
                assert_eq!(bytes, content.len() as i64);
                assert_eq!(&buf[..bytes as usize], content.as_bytes());
            }
        }));
    }

    for h in handles {
        h.join().expect("thread failed");
    }

    // Verify all files present in directory listing
    let mut total_files: usize = 0;
    assert_eq!(
        oifs_ls(
            handle,
            Some(count_cb),
            &mut total_files as *mut _ as *mut std::os::raw::c_void
        ),
        0
    );
    // 5 base files + 5 threads * 10 files = 55 files
    assert_eq!(total_files, 55);

    oifs_close(handle);
}

#[test]
fn test_ffi_multithreaded_session_handles() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let img_path = tmp_dir.path().join("session_handles_mt.img");
    let path_str = img_path.to_str().unwrap().to_string();
    let c_path_str = path_str.clone();

    // Initialize session image
    {
        let c_path = CString::new(c_path_str.as_str()).unwrap();
        let h = oifs_get_or_open(c_path.as_ptr(), 20 * 1024 * 1024);
        assert!(!h.is_null());
        oifs_close(h);
    }

    let num_threads = 8;
    let mut handles = Vec::new();

    for t in 0..num_threads {
        let p = path_str.clone();
        handles.push(thread::spawn(move || {
            let c_path = CString::new(p).unwrap();
            let h = oifs_get_or_open(c_path.as_ptr(), 20 * 1024 * 1024);
            assert!(!h.is_null());

            for i in 0..10 {
                let filename = CString::new(format!("session_t{}_f{}.txt", t, i)).unwrap();
                let data = format!("Session data from thread {} file {}", t, i);
                let write_res =
                    oifs_write_file(h, filename.as_ptr(), data.as_ptr(), data.len() as u64);
                assert_eq!(write_res, 0);

                let mut buf = vec![0u8; 128];
                let read_res =
                    oifs_read_file(h, filename.as_ptr(), buf.as_mut_ptr(), buf.len() as u64);
                assert_eq!(read_res, data.len() as i64);
                assert_eq!(&buf[..read_res as usize], data.as_bytes());
            }

            oifs_close(h);
        }));
    }

    for h in handles {
        h.join().expect("session thread panicked");
    }

    oifs::OifsSession::unregister_from_registry(&path_str);
}

#[test]
fn test_ffi_zero_sized_and_cross_block_io() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let img_path = tmp_dir.path().join("cross_block.img");
    let c_img_path = CString::new(img_path.to_str().unwrap()).unwrap();
    let handle = oifs_open(c_img_path.as_ptr(), 20 * 1024 * 1024);
    assert!(!handle.is_null());

    // 1. Zero-sized file
    let c_empty = CString::new("empty.dat").unwrap();
    assert_eq!(oifs_write_file(handle, c_empty.as_ptr(), ptr::null(), 0), 0);

    let mut buf0 = [0u8; 10];
    let r0 = oifs_read_file(handle, c_empty.as_ptr(), buf0.as_mut_ptr(), 10);
    assert_eq!(r0, 0);

    // Reading with 0 buffer size on non-empty file
    let c_payload = CString::new("payload.dat").unwrap();
    let large_data: Vec<u8> = (0..16384).map(|i| (i % 251) as u8).collect(); // 16KB (4 blocks)
    assert_eq!(
        oifs_write_file(
            handle,
            c_payload.as_ptr(),
            large_data.as_ptr(),
            large_data.len() as u64
        ),
        0
    );

    let r_zero_buf = oifs_read_file(handle, c_payload.as_ptr(), ptr::null_mut(), 0);
    assert_eq!(r_zero_buf, 0);

    // 2. Cross-block offset reads via oifs_read_at
    // Block size is 4096. Read 100 bytes starting at offset 4050 (spans block 0 and block 1)
    let mut cross_buf = vec![0u8; 100];
    let cross_read = oifs_read_at(
        handle,
        c_payload.as_ptr(),
        4050,
        cross_buf.as_mut_ptr(),
        cross_buf.len() as u64,
    );
    assert_eq!(cross_read, 100);
    assert_eq!(&cross_buf[..], &large_data[4050..4150]);

    // Read starting at offset 8190 for 20 bytes (spans block 1 and block 2)
    let mut cross_buf2 = vec![0u8; 20];
    let cross_read2 = oifs_read_at(
        handle,
        c_payload.as_ptr(),
        8190,
        cross_buf2.as_mut_ptr(),
        cross_buf2.len() as u64,
    );
    assert_eq!(cross_read2, 20);
    assert_eq!(&cross_buf2[..], &large_data[8190..8210]);

    // Read near EOF with buffer larger than remaining bytes
    let mut eof_buf = vec![0u8; 50];
    let eof_read = oifs_read_at(
        handle,
        c_payload.as_ptr(),
        16370,
        eof_buf.as_mut_ptr(),
        eof_buf.len() as u64,
    );
    assert_eq!(eof_read, 14); // 16384 - 16370 = 14 bytes remaining
    assert_eq!(&eof_buf[..14], &large_data[16370..]);

    // Read completely past EOF
    let mut past_eof_buf = vec![0u8; 32];
    let past_eof_read = oifs_read_at(
        handle,
        c_payload.as_ptr(),
        20000,
        past_eof_buf.as_mut_ptr(),
        past_eof_buf.len() as u64,
    );
    assert_eq!(past_eof_read, 0);

    oifs_close(handle);
}
