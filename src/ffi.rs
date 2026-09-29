#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::disk::DiskManager;
use crate::directory::DirectoryIterator;
use crate::inode::FileType;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::ptr;

// Opaque handle for C
pub struct OIFSHandle {
    pub dm: DiskManager,
    pub last_error: Option<String>,
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_open(path: *const c_char, size: u64) -> *mut OIFSHandle {
    if path.is_null() {
        return ptr::null_mut();
    }
    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    match DiskManager::open(path_str, size) {
        Ok(dm) => {
            let handle = Box::new(OIFSHandle { dm, last_error: None });
            Box::into_raw(handle)
        }
        Err(_) => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_open_with_password(
    path: *const c_char,
    size: u64,
    password: *const c_char,
) -> *mut OIFSHandle {
    if path.is_null() {
        return ptr::null_mut();
    }
    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let pwd_str = if password.is_null() {
        None
    } else {
        match unsafe { CStr::from_ptr(password) }.to_str() {
            Ok(s) => Some(s),
            Err(_) => return ptr::null_mut(),
        }
    };

    match DiskManager::open_with_password(path_str, size, pwd_str) {
        Ok(dm) => {
            let handle = Box::new(OIFSHandle { dm, last_error: None });
            Box::into_raw(handle)
        }
        Err(_) => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_get_or_open(path: *const c_char, size: u64) -> *mut OIFSHandle {
    if path.is_null() {
        return ptr::null_mut();
    }
    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    match crate::session::OifsSession::get_or_open(path_str, size) {
        Ok(crate::session::OifsSession::Direct { dm, .. }) => {
            let handle = Box::new(OIFSHandle {
                dm: (*dm).clone(),
                last_error: None,
            });
            Box::into_raw(handle)
        }
        _ => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_get_or_open_with_password(
    path: *const c_char,
    size: u64,
    password: *const c_char,
) -> *mut OIFSHandle {
    if path.is_null() {
        return ptr::null_mut();
    }
    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let pwd_str = if password.is_null() {
        None
    } else {
        match unsafe { CStr::from_ptr(password) }.to_str() {
            Ok(s) => Some(s),
            Err(_) => return ptr::null_mut(),
        }
    };

    match crate::session::OifsSession::get_or_open_with_password(path_str, size, pwd_str) {
        Ok(crate::session::OifsSession::Direct { dm, .. }) => {
            let handle = Box::new(OIFSHandle {
                dm: (*dm).clone(),
                last_error: None,
            });
            Box::into_raw(handle)
        }
        _ => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_close(handle: *mut OIFSHandle) {
    if !handle.is_null() {
        unsafe {
            let _ = Box::from_raw(handle);
        }
    }
}

// Callback: void (*cb)(const char* name, uint64_t size, uint64_t mtime, void* user_data)
pub type ListCallback = extern "C" fn(*const c_char, u64, u64, *mut c_void);

#[unsafe(no_mangle)]
pub extern "C" fn oifs_ls(handle: *mut OIFSHandle, cb: ListCallback, user_data: *mut c_void) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() {
            return -1;
        }
        &mut (*handle)
    };

    let dm = &mut handle_ref.dm;
    let root_inode_id = dm.superblock().root_inode;

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let root_inode = dm.read_inode(root_inode_id)?;
        if root_inode.mode != FileType::Directory {
             return Ok(());
        }

        let block_id = root_inode.blocks[0];
        if block_id == 0 {
             return Ok(());
        }

        if let Some(block_data) = dm.get_block_copy(block_id) {
            let iter = DirectoryIterator::new(&block_data);
            for entry_res in iter {
                if let Ok(entry) = entry_res
                    && let Ok(inode) = dm.read_inode(entry.inode) {
                        let c_name = CString::new(entry.name).unwrap_or_default();
                        cb(c_name.as_ptr(), inode.size, inode.modified_at, user_data);
                    }
            }
        }
         Ok(())
     })();

    match result {
        Ok(_) => {
            handle_ref.last_error = None;
            0
        }
        Err(e) => {
            handle_ref.last_error = Some(e.to_string());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_create_file(handle: *mut OIFSHandle, path: *const c_char) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() { return -1; }
        &mut (*handle)
    };
    
    let c_str = unsafe { CStr::from_ptr(path) };
    let filename = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle_ref.last_error = Some("Invalid UTF-8 filename".to_string());
            return -1;
        }
    };

    let dm = &handle_ref.dm;
    let root_inode_id = dm.superblock().root_inode;

    match dm.create_file(root_inode_id, filename) {
        Ok(_) => {
            handle_ref.last_error = None;
            0
        }
        Err(e) => {
            handle_ref.last_error = Some(e.to_string());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_delete_file(handle: *mut OIFSHandle, path: *const c_char) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() { return -1; }
        &mut (*handle)
    };

    let c_str = unsafe { CStr::from_ptr(path) };
    let filename = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle_ref.last_error = Some("Invalid UTF-8 filename".to_string());
            return -1;
        }
    };

    let dm = &handle_ref.dm;
    let root_inode_id = dm.superblock().root_inode;

    match dm.delete_file(root_inode_id, filename) {
        Ok(_) => {
            handle_ref.last_error = None;
            0
        }
        Err(e) => {
            handle_ref.last_error = Some(e.to_string());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_read_at(
    handle: *mut OIFSHandle,
    filename: *const c_char,
    offset: u64,
    buf: *mut u8,
    buf_size: u64,
) -> i64 {
    let handle_ref = unsafe {
        if handle.is_null() { return -1; }
        &mut (*handle)
    };

    if filename.is_null() || buf.is_null() {
        handle_ref.last_error = Some("Null argument provided".to_string());
        return -1;
    }

    let c_str = unsafe { CStr::from_ptr(filename) };
    let filename_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle_ref.last_error = Some("Invalid UTF-8 filename".to_string());
            return -1;
        }
    };

    let dm = &handle_ref.dm;
    match (|| -> Result<i64, Box<dyn std::error::Error>> {
        let inode_id = dm.resolve_path(filename_str)?;
        let out_slice = unsafe { std::slice::from_raw_parts_mut(buf, buf_size as usize) };
        let bytes_read = dm.read_at(inode_id, offset, out_slice)?;
        Ok(bytes_read as i64)
    })() {
        Ok(bytes) => {
            handle_ref.last_error = None;
            bytes
        }
        Err(e) => {
            handle_ref.last_error = Some(e.to_string());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_read_file(
    handle: *mut OIFSHandle,
    filename: *const c_char,
    buf: *mut u8,
    buf_size: u64,
) -> i64 {
    oifs_read_at(handle, filename, 0, buf, buf_size)
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_write_file(
    handle: *mut OIFSHandle,
    filename: *const c_char,
    buf: *const u8,
    buf_size: u64,
) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() { return -1; }
        &mut (*handle)
    };

    if filename.is_null() || buf.is_null() {
        handle_ref.last_error = Some("Null argument provided".to_string());
        return -1;
    }

    let c_str = unsafe { CStr::from_ptr(filename) };
    let filename_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle_ref.last_error = Some("Invalid UTF-8 filename".to_string());
            return -1;
        }
    };

    let dm = &handle_ref.dm;
    match (|| -> Result<(), Box<dyn std::error::Error>> {
        let (parent_id, name) = dm.resolve_parent(filename_str)?;
        let inode_id = match dm.lookup(parent_id, &name) {
            Ok(existing_id) => existing_id,
            Err(_) => dm.create_file(parent_id, &name)?,
        };
        let data = unsafe { std::slice::from_raw_parts(buf, buf_size as usize) };
        dm.write_data(inode_id, 0, data, crate::disk::CompressionMode::Auto)?;
        Ok(())
    })() {
        Ok(_) => {
            handle_ref.last_error = None;
            0
        }
        Err(e) => {
            handle_ref.last_error = Some(e.to_string());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_mkdir(
    handle: *mut OIFSHandle,
    path: *const c_char,
) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() { return -1; }
        &mut (*handle)
    };

    if path.is_null() {
        handle_ref.last_error = Some("Null path provided".to_string());
        return -1;
    }

    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle_ref.last_error = Some("Invalid UTF-8 path".to_string());
            return -1;
        }
    };

    let dm = &handle_ref.dm;
    match (|| -> Result<(), Box<dyn std::error::Error>> {
        let (parent_id, name) = dm.resolve_parent(path_str)?;
        dm.create_directory(parent_id, &name)?;
        Ok(())
    })() {
        Ok(_) => {
            handle_ref.last_error = None;
            0
        }
        Err(e) => {
            handle_ref.last_error = Some(e.to_string());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_last_error(
    handle: *mut OIFSHandle,
    buf: *mut c_char,
    buf_size: u32,
) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() { return -1; }
        &mut (*handle)
    };
    
    if buf.is_null() || buf_size == 0 {
        return -1;
    }

    let err_str = match &handle_ref.last_error {
        Some(s) => s.as_str(),
        None => "No error",
    };

    let c_err = match CString::new(err_str) {
        Ok(c) => c,
        Err(_) => return -1,
    };

    let bytes = c_err.as_bytes_with_nul();
    let to_copy = std::cmp::min(bytes.len(), buf_size as usize);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, to_copy);
        if to_copy > 0 {
            std::ptr::write(buf.add(to_copy - 1), 0);
        }
    }
    0
}
