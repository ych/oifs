use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use oifs::ffi::{oifs_open, oifs_close, oifs_ls, oifs_create_file, oifs_delete_file, OIFSHandle};

extern "C" fn test_cb(_name: *const c_char, _size: u64, _mtime: u64, user_data: *mut c_void) {
    let count = unsafe { &mut *(user_data as *mut i32) };
    *count += 1;
}

#[test]
fn test_ffi_create_delete_list() {
    let path = CString::new("test_fs_ffi_del.img").unwrap();
    if std::path::Path::new("test_fs_ffi_del.img").exists() {
        std::fs::remove_file("test_fs_ffi_del.img").unwrap();
    }

    let handle = oifs_open(path.as_ptr(), 10 * 1024 * 1024);
    assert!(!handle.is_null());

    let filename = CString::new("todelete.txt").unwrap();
    assert_eq!(oifs_create_file(handle, filename.as_ptr()), 0);

    let mut count = 0;
    oifs_ls(handle, test_cb, &mut count as *mut _ as *mut c_void);
    assert_eq!(count, 1);

    // Delete
    assert_eq!(oifs_delete_file(handle, filename.as_ptr()), 0);

    // Check list empty
    let mut count_after = 0;
    oifs_ls(handle, test_cb, &mut count_after as *mut _ as *mut c_void);
    assert_eq!(count_after, 0);

    // Check double delete fails
    assert_eq!(oifs_delete_file(handle, filename.as_ptr()), -1);

    oifs_close(handle);
    std::fs::remove_file("test_fs_ffi_del.img").unwrap();
}
