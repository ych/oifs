use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use oifs::ffi::{oifs_open, oifs_close, oifs_ls, oifs_create_file};

// Mock callback
extern "C" fn test_cb(name: *const c_char, size: u64, mtime: u64, user_data: *mut c_void) {
    let name_str = unsafe { std::ffi::CStr::from_ptr(name) }.to_str().unwrap();
    println!("Found file: {}, Size: {}, Mtime: {}", name_str, size, mtime);
    
    // Verify user data
    let count = unsafe { &mut *(user_data as *mut i32) };
    *count += 1;
}

#[test]
fn test_ffi_create_and_list() {
    let path = CString::new("test_fs_ffi.img").unwrap();
    
    // Clean up
    if std::path::Path::new("test_fs_ffi.img").exists() {
        std::fs::remove_file("test_fs_ffi.img").unwrap();
    }

    // Open/Create
    let handle = oifs_open(path.as_ptr(), 10 * 1024 * 1024);
    assert!(!handle.is_null());

    // Create file
    let filename = CString::new("test_ffi.txt").unwrap();
    let res = oifs_create_file(handle, filename.as_ptr());
    assert_eq!(res, 0);

    // List files
    let mut count = 0;
    let res_ls = oifs_ls(handle, test_cb, &mut count as *mut _ as *mut c_void);
    assert_eq!(res_ls, 0);
    assert_eq!(count, 1);

    oifs_close(handle);

    // Cleanup
    std::fs::remove_file("test_fs_ffi.img").unwrap();
}

#[test]
fn test_ffi_get_or_open() {
    use oifs::ffi::oifs_get_or_open;
    let path = CString::new("test_ffi_shared.img").unwrap();

    if std::path::Path::new("test_ffi_shared.img").exists() {
        let _ = std::fs::remove_file("test_ffi_shared.img");
    }
    oifs::OifsSession::unregister_from_registry("test_ffi_shared.img");

    // Open first handle
    let handle1 = oifs_get_or_open(path.as_ptr(), 10 * 1024 * 1024);
    assert!(!handle1.is_null());

    // Create file through handle 1
    let filename = CString::new("shared_file.txt").unwrap();
    let res = oifs_create_file(handle1, filename.as_ptr());
    assert_eq!(res, 0);

    // Open second handle concurrently from another thread or place
    let handle2 = oifs_get_or_open(path.as_ptr(), 10 * 1024 * 1024);
    assert!(!handle2.is_null());

    // Verify handle 2 can see the created file
    let mut count = 0;
    let res_ls = oifs_ls(handle2, test_cb, &mut count as *mut _ as *mut c_void);
    assert_eq!(res_ls, 0);
    assert_eq!(count, 1);

    oifs_close(handle1);
    oifs_close(handle2);

    oifs::OifsSession::unregister_from_registry("test_ffi_shared.img");
    let _ = std::fs::remove_file("test_ffi_shared.img");
}
