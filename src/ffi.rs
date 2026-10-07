#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::directory::DirectoryIterator;
use crate::disk::DiskManager;
use crate::inode::FileType;
use crate::io_engine::IoBackend;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::ptr;

// Opaque handle for C
pub struct OIFSHandle {
    pub dm: DiskManager,
    pub last_error: std::sync::Mutex<Option<String>>,
}

impl OIFSHandle {
    pub fn new(dm: DiskManager) -> Self {
        Self {
            dm,
            last_error: std::sync::Mutex::new(None),
        }
    }

    pub fn set_last_error(&self, err: Option<String>) {
        if let Ok(mut lock) = self.last_error.lock() {
            *lock = err;
        }
    }

    pub fn get_last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|guard| guard.clone())
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_open(path: *const c_char, size: u64) -> *mut OIFSHandle {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if path.is_null() {
            return ptr::null_mut();
        }
        let c_str = unsafe { CStr::from_ptr(path) };
        let path_str = match c_str.to_str() {
            Ok(s) => s,
            Err(_) => return ptr::null_mut(),
        };

        match DiskManager::open(path_str, size) {
            Ok(dm) => Box::into_raw(Box::new(OIFSHandle::new(dm))),
            Err(_) => ptr::null_mut(),
        }
    }));
    res.unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_open_with_password(
    path: *const c_char,
    size: u64,
    password: *const c_char,
) -> *mut OIFSHandle {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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

        let dm_res = if !std::path::Path::new(path_str).exists()
            && let Some(pwd) = pwd_str
            && !pwd.is_empty()
        {
            DiskManager::create_encrypted(path_str, size, pwd)
        } else {
            DiskManager::open_with_password(path_str, size, pwd_str)
        };

        match dm_res {
            Ok(dm) => Box::into_raw(Box::new(OIFSHandle::new(dm))),
            Err(_) => ptr::null_mut(),
        }
    }));
    res.unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_get_or_open(path: *const c_char, size: u64) -> *mut OIFSHandle {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
                Box::into_raw(Box::new(OIFSHandle::new((*dm).clone())))
            }
            _ => ptr::null_mut(),
        }
    }));
    res.unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_get_or_open_with_password(
    path: *const c_char,
    size: u64,
    password: *const c_char,
) -> *mut OIFSHandle {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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

        let session_res = if !std::path::Path::new(path_str).exists()
            && let Some(pwd) = pwd_str
            && !pwd.is_empty()
        {
            crate::session::OifsSession::get_or_create_encrypted(path_str, size, pwd)
        } else {
            crate::session::OifsSession::get_or_open_with_password(path_str, size, pwd_str)
        };

        match session_res {
            Ok(crate::session::OifsSession::Direct { dm, .. }) => {
                Box::into_raw(Box::new(OIFSHandle::new((*dm).clone())))
            }
            _ => ptr::null_mut(),
        }
    }));
    res.unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_close(handle: *mut OIFSHandle) {
    if !handle.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            let _ = Box::from_raw(handle);
        }));
    }
}

// Callback: void (*cb)(const char* name, uint64_t size, uint64_t mtime, void* user_data)
pub type ListCallback = extern "C" fn(*const c_char, u64, u64, *mut c_void);

#[unsafe(no_mangle)]
pub extern "C" fn oifs_ls(
    handle: *mut OIFSHandle,
    cb: Option<ListCallback>,
    user_data: *mut c_void,
) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() {
            return -1;
        }
        &(*handle)
    };

    let cb_fn = match cb {
        Some(f) => f,
        None => {
            handle_ref.set_last_error(Some("Null callback provided".to_string()));
            return -1;
        }
    };

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dm = &handle_ref.dm;
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
                        && let Ok(inode) = dm.read_inode(entry.inode)
                    {
                        let c_name = CString::new(entry.name).unwrap_or_default();
                        cb_fn(c_name.as_ptr(), inode.size, inode.modified_at, user_data);
                    }
                }
            }
            Ok(())
        })();

        match result {
            Ok(_) => {
                handle_ref.set_last_error(None);
                0
            }
            Err(e) => {
                handle_ref.set_last_error(Some(e.to_string()));
                -1
            }
        }
    }));

    match res {
        Ok(code) => code,
        Err(_) => {
            handle_ref.set_last_error(Some("Panic occurred during directory listing".to_string()));
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_create_file(handle: *mut OIFSHandle, path: *const c_char) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() {
            return -1;
        }
        &(*handle)
    };

    if path.is_null() {
        handle_ref.set_last_error(Some("Null path provided".to_string()));
        return -1;
    }

    let c_str = unsafe { CStr::from_ptr(path) };
    let filename = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle_ref.set_last_error(Some("Invalid UTF-8 filename".to_string()));
            return -1;
        }
    };

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dm = &handle_ref.dm;
        match (|| -> Result<(), Box<dyn std::error::Error>> {
            let (parent_id, name) = dm.resolve_parent(filename)?;
            dm.create_file(parent_id, &name)?;
            Ok(())
        })() {
            Ok(_) => {
                handle_ref.set_last_error(None);
                0
            }
            Err(e) => {
                handle_ref.set_last_error(Some(e.to_string()));
                -1
            }
        }
    }));

    match res {
        Ok(code) => code,
        Err(_) => {
            handle_ref.set_last_error(Some("Panic occurred during file creation".to_string()));
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_delete_file(handle: *mut OIFSHandle, path: *const c_char) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() {
            return -1;
        }
        &(*handle)
    };

    if path.is_null() {
        handle_ref.set_last_error(Some("Null path provided".to_string()));
        return -1;
    }

    let c_str = unsafe { CStr::from_ptr(path) };
    let filename = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle_ref.set_last_error(Some("Invalid UTF-8 filename".to_string()));
            return -1;
        }
    };

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dm = &handle_ref.dm;
        match (|| -> Result<(), Box<dyn std::error::Error>> {
            let (parent_id, name) = dm.resolve_parent(filename)?;
            dm.delete_file(parent_id, &name)?;
            Ok(())
        })() {
            Ok(_) => {
                handle_ref.set_last_error(None);
                0
            }
            Err(e) => {
                handle_ref.set_last_error(Some(e.to_string()));
                -1
            }
        }
    }));

    match res {
        Ok(code) => code,
        Err(_) => {
            handle_ref.set_last_error(Some("Panic occurred during file deletion".to_string()));
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
        if handle.is_null() {
            return -1;
        }
        &(*handle)
    };

    if filename.is_null() {
        handle_ref.set_last_error(Some("Null argument provided".to_string()));
        return -1;
    }

    if buf.is_null() {
        if buf_size == 0 {
            return 0;
        }
        handle_ref.set_last_error(Some("Null argument provided".to_string()));
        return -1;
    }

    let c_str = unsafe { CStr::from_ptr(filename) };
    let filename_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle_ref.set_last_error(Some("Invalid UTF-8 filename".to_string()));
            return -1;
        }
    };

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dm = &handle_ref.dm;
        match (|| -> Result<i64, Box<dyn std::error::Error>> {
            let inode_id = dm.resolve_path(filename_str)?;
            let out_slice = unsafe { std::slice::from_raw_parts_mut(buf, buf_size as usize) };
            let bytes_read = dm.read_at(inode_id, offset, out_slice)?;
            Ok(bytes_read as i64)
        })() {
            Ok(bytes) => {
                handle_ref.set_last_error(None);
                bytes
            }
            Err(e) => {
                handle_ref.set_last_error(Some(e.to_string()));
                -1
            }
        }
    }));

    match res {
        Ok(val) => val,
        Err(_) => {
            handle_ref.set_last_error(Some("Panic occurred during read_at".to_string()));
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
        if handle.is_null() {
            return -1;
        }
        &(*handle)
    };

    if filename.is_null() {
        handle_ref.set_last_error(Some("Null argument provided".to_string()));
        return -1;
    }

    if buf.is_null() && buf_size > 0 {
        handle_ref.set_last_error(Some("Null argument provided".to_string()));
        return -1;
    }

    let c_str = unsafe { CStr::from_ptr(filename) };
    let filename_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle_ref.set_last_error(Some("Invalid UTF-8 filename".to_string()));
            return -1;
        }
    };

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dm = &handle_ref.dm;
        match (|| -> Result<(), Box<dyn std::error::Error>> {
            let (parent_id, name) = dm.resolve_parent(filename_str)?;
            let inode_id = match dm.lookup(parent_id, &name) {
                Ok(existing_id) => existing_id,
                Err(_) => dm.create_file(parent_id, &name)?,
            };
            let data = if buf.is_null() {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(buf, buf_size as usize) }
            };
            dm.write_data(inode_id, 0, data, crate::disk::CompressionMode::Auto)?;
            Ok(())
        })() {
            Ok(_) => {
                handle_ref.set_last_error(None);
                0
            }
            Err(e) => {
                handle_ref.set_last_error(Some(e.to_string()));
                -1
            }
        }
    }));

    match res {
        Ok(val) => val,
        Err(_) => {
            handle_ref.set_last_error(Some("Panic occurred during write_file".to_string()));
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_mkdir(handle: *mut OIFSHandle, path: *const c_char) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() {
            return -1;
        }
        &(*handle)
    };

    if path.is_null() {
        handle_ref.set_last_error(Some("Null path provided".to_string()));
        return -1;
    }

    let c_str = unsafe { CStr::from_ptr(path) };
    let path_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle_ref.set_last_error(Some("Invalid UTF-8 path".to_string()));
            return -1;
        }
    };

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dm = &handle_ref.dm;
        match (|| -> Result<(), Box<dyn std::error::Error>> {
            let (parent_id, name) = dm.resolve_parent(path_str)?;
            dm.create_directory(parent_id, &name)?;
            Ok(())
        })() {
            Ok(_) => {
                handle_ref.set_last_error(None);
                0
            }
            Err(e) => {
                handle_ref.set_last_error(Some(e.to_string()));
                -1
            }
        }
    }));

    match res {
        Ok(val) => val,
        Err(_) => {
            handle_ref.set_last_error(Some("Panic occurred during mkdir".to_string()));
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_last_error(handle: *mut OIFSHandle, buf: *mut c_char, buf_size: u32) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() {
            return -1;
        }
        &(*handle)
    };

    if buf.is_null() || buf_size == 0 {
        return -1;
    }

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let err_opt = handle_ref.get_last_error();
        let err_str = match &err_opt {
            Some(s) => s.as_str(),
            None => "No error",
        };

        let sanitized = err_str.replace('\0', " ");
        let c_err = match CString::new(sanitized) {
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
    }));

    res.unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_set_io_backend(handle: *mut OIFSHandle, backend: u8) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() {
            return -1;
        }
        &(*handle)
    };
    let b = IoBackend::from_u8(backend);
    let effective = handle_ref.dm.set_io_backend(b);
    effective as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_get_io_backend(handle: *mut OIFSHandle) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() {
            return -1;
        }
        &(*handle)
    };
    handle_ref.dm.io_backend() as i32
}

const fn parse_u32(s: &str) -> u32 {
    let bytes = s.as_bytes();
    let mut val = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        val = val * 10 + (bytes[i] - b'0') as u32;
        i += 1;
    }
    val
}

pub const OIFS_VERSION_MAJOR: u32 = parse_u32(env!("CARGO_PKG_VERSION_MAJOR"));
pub const OIFS_VERSION_MINOR: u32 = parse_u32(env!("CARGO_PKG_VERSION_MINOR"));
pub const OIFS_VERSION_PATCH: u32 = parse_u32(env!("CARGO_PKG_VERSION_PATCH"));
pub const OIFS_VERSION_STRING: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

#[inline]
pub const fn make_version_code(major: u32, minor: u32, patch: u32) -> u64 {
    ((major as u64) << 32) | ((minor as u64) << 16) | (patch as u64)
}

pub const OIFS_VERSION_CODE: u64 =
    make_version_code(OIFS_VERSION_MAJOR, OIFS_VERSION_MINOR, OIFS_VERSION_PATCH);

/// Returns the major version number of the loaded OIFS dynamic library.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_version_major() -> u32 {
    OIFS_VERSION_MAJOR
}

/// Returns the minor version number of the loaded OIFS dynamic library.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_version_minor() -> u32 {
    OIFS_VERSION_MINOR
}

/// Returns the patch version number of the loaded OIFS dynamic library.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_version_patch() -> u32 {
    OIFS_VERSION_PATCH
}

/// Returns the monotonic 64-bit encoded version code: (major << 32) | (minor << 16) | patch.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_version_code() -> u64 {
    OIFS_VERSION_CODE
}

/// Returns a null-terminated static C string of the library version (e.g., "0.1.0").
#[unsafe(no_mangle)]
pub extern "C" fn oifs_version_string() -> *const c_char {
    OIFS_VERSION_STRING.as_ptr() as *const c_char
}

pub const OIFS_VERSION_COMPAT_OK: i32 = 0;
pub const OIFS_VERSION_COMPAT_WARN: i32 = 1;
pub const OIFS_VERSION_COMPAT_ERR: i32 = -1;

/// Checks whether the loaded dynamic library is compatible with the requested version.
///
/// Policy:
/// - Returns 0 (`OIFS_VERSION_COMPAT_OK`): Expected version match (exact match, all good).
/// - Returns 1 (`OIFS_VERSION_COMPAT_WARN`): Newer library version (warning, allowed to proceed).
/// - Returns -1 (`OIFS_VERSION_COMPAT_ERR`): Older library version (error out, blocked).
#[unsafe(no_mangle)]
pub extern "C" fn oifs_check_version(req_major: u32, req_minor: u32, req_patch: u32) -> i32 {
    let req_code = make_version_code(req_major, req_minor, req_patch);
    if OIFS_VERSION_CODE == req_code {
        OIFS_VERSION_COMPAT_OK
    } else if OIFS_VERSION_CODE > req_code {
        OIFS_VERSION_COMPAT_WARN
    } else {
        OIFS_VERSION_COMPAT_ERR
    }
}

/// Retrieves the absolute filesystem path from which this dynamic library was loaded.
/// Writes up to `buf_size` bytes into `buf` (including null terminator).
/// Returns 0 on success, or -1 on failure/truncation.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_loaded_path(buf: *mut c_char, buf_size: usize) -> i32 {
    if buf.is_null() || buf_size == 0 {
        return -1;
    }
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        let ret = libc::dladdr(oifs_version_code as *const c_void, &mut info);
        if ret == 0 || info.dli_fname.is_null() {
            return -1;
        }
        let c_fname = CStr::from_ptr(info.dli_fname);
        let bytes = c_fname.to_bytes_with_nul();
        if bytes.len() > buf_size {
            return -1;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, buf, bytes.len());
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_truncate_file(
    handle: *mut OIFSHandle,
    filename: *const c_char,
    new_size: u64,
) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() {
            return -1;
        }
        &(*handle)
    };

    if filename.is_null() {
        handle_ref.set_last_error(Some("Null filename provided".to_string()));
        return -1;
    }

    let c_str = unsafe { CStr::from_ptr(filename) };
    let filename_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle_ref.set_last_error(Some("Invalid UTF-8 filename".to_string()));
            return -1;
        }
    };

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dm = &handle_ref.dm;
        match (|| -> Result<(), Box<dyn std::error::Error>> {
            let (parent_id, name) = dm.resolve_parent(filename_str)?;
            let inode_id = dm.lookup(parent_id, &name)?;
            dm.truncate(inode_id, new_size)?;
            Ok(())
        })() {
            Ok(_) => {
                handle_ref.set_last_error(None);
                0
            }
            Err(e) => {
                handle_ref.set_last_error(Some(e.to_string()));
                -1
            }
        }
    }));

    match res {
        Ok(val) => val,
        Err(_) => {
            handle_ref.set_last_error(Some("Panic occurred during truncate_file".to_string()));
            -1
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// Formally prove that oifs_check_version adheres strictly to the version policy:
    /// - Returns OIFS_VERSION_COMPAT_OK (0) iff req_code == OIFS_VERSION_CODE
    /// - Returns OIFS_VERSION_COMPAT_WARN (1) iff req_code < OIFS_VERSION_CODE (newer library)
    /// - Returns OIFS_VERSION_COMPAT_ERR (-1) iff req_code > OIFS_VERSION_CODE (outdated library)
    /// - Never overflows or panics for ANY symbolic (req_major, req_minor, req_patch).
    #[kani::proof]
    fn proof_oifs_check_version_policy_soundness() {
        let req_major: u32 = kani::any();
        let req_minor: u32 = kani::any();
        let req_patch: u32 = kani::any();

        let res = oifs_check_version(req_major, req_minor, req_patch);
        let req_code = make_version_code(req_major, req_minor, req_patch);

        if req_code == OIFS_VERSION_CODE {
            assert_eq!(res, OIFS_VERSION_COMPAT_OK);
        } else if req_code < OIFS_VERSION_CODE {
            assert_eq!(res, OIFS_VERSION_COMPAT_WARN);
        } else {
            assert_eq!(res, OIFS_VERSION_COMPAT_ERR);
        }
    }
}
