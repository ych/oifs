#![allow(clippy::not_unsafe_ptr_arg_deref)]

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

/// Converts a C string pointer into a `&str`.
///
/// The dereference happens at the call site, which is always inside a
/// `catch_unwind` closure, so a fault while converting the string cannot
/// unwind across the `extern "C"` boundary.
unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> Result<&'a str, &'static str> {
    if ptr.is_null() {
        return Err("Null argument provided");
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| "Invalid UTF-8 filename")
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

            let entries = dm.list_dir(root_inode_id)?;
            for entry in entries {
                if let Ok(inode) = dm.read_inode(entry.inode) {
                    let c_name = CString::new(entry.name).unwrap_or_default();
                    cb_fn(c_name.as_ptr(), inode.size, inode.modified_at, user_data);
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

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let filename = match unsafe { cstr_to_str(path) } {
            Ok(s) => s,
            Err(msg) => {
                handle_ref.set_last_error(Some(msg.to_string()));
                return -1;
            }
        };

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
pub extern "C" fn oifs_create_or_open_file(handle: *mut OIFSHandle, path: *const c_char) -> i32 {
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

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let filename = match unsafe { cstr_to_str(path) } {
            Ok(s) => s,
            Err(msg) => {
                handle_ref.set_last_error(Some(msg.to_string()));
                return -1;
            }
        };

        let dm = &handle_ref.dm;
        match (|| -> Result<(), Box<dyn std::error::Error>> {
            let (parent_id, name) = dm.resolve_parent(filename)?;
            dm.create_or_open_file(parent_id, &name)?;
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
            handle_ref.set_last_error(Some(
                "Panic occurred during file create_or_open".to_string(),
            ));
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

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let filename = match unsafe { cstr_to_str(path) } {
            Ok(s) => s,
            Err(msg) => {
                handle_ref.set_last_error(Some(msg.to_string()));
                return -1;
            }
        };

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

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let filename_str = match unsafe { cstr_to_str(filename) } {
            Ok(s) => s,
            Err(msg) => {
                handle_ref.set_last_error(Some(msg.to_string()));
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

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let filename_str = match unsafe { cstr_to_str(filename) } {
            Ok(s) => s,
            Err(msg) => {
                handle_ref.set_last_error(Some(msg.to_string()));
                return -1;
            }
        };

        let dm = &handle_ref.dm;
        match (|| -> Result<(), Box<dyn std::error::Error>> {
            let (parent_id, name) = dm.resolve_parent(filename_str)?;
            // A null buffer with a non-zero size is a caller error and
            // must not be silently treated as an empty write.
            let data = if buf.is_null() {
                if buf_size > 0 {
                    return Err(
                        std::io::Error::other("Null buffer provided with a non-zero size").into(),
                    );
                }
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(buf, buf_size as usize) }
            };
            match dm.lookup(parent_id, &name) {
                Ok(existing_id) => {
                    dm.write_data(existing_id, 0, data, crate::disk::CompressionMode::Auto)?;
                }
                Err(_) => {
                    dm.create_file_with_data(
                        parent_id,
                        &name,
                        data,
                        crate::disk::CompressionMode::Auto,
                    )?;
                }
            }
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

pub const OIFS_WRITE_POLICY_DEFAULT: u32 = 0x00;
pub const OIFS_WRITE_POLICY_RAW: u32 = 0x01;
pub const OIFS_WRITE_POLICY_STREAM: u32 = 0x02;
pub const OIFS_WRITE_POLICY_SEEKABLE_64K: u32 = 0x08;

#[unsafe(no_mangle)]
pub extern "C" fn oifs_write_file_with_policy(
    handle: *mut OIFSHandle,
    filename: *const c_char,
    offset: u64,
    buf: *const u8,
    buf_size: u64,
    policy_flags: u32,
    zstd_level: i32,
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

    let compression_mode = if policy_flags & OIFS_WRITE_POLICY_SEEKABLE_64K != 0 {
        crate::disk::CompressionMode::Seekable {
            chunk_size: crate::inode::CHUNK_SIZE_64K as u32,
            level: zstd_level,
        }
    } else if policy_flags & OIFS_WRITE_POLICY_RAW != 0 {
        crate::disk::CompressionMode::Never
    } else if policy_flags & OIFS_WRITE_POLICY_STREAM != 0 {
        crate::disk::CompressionMode::Stream { level: zstd_level }
    } else {
        crate::disk::CompressionMode::Auto
    };

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let filename_str = match unsafe { cstr_to_str(filename) } {
            Ok(s) => s,
            Err(msg) => {
                handle_ref.set_last_error(Some(msg.to_string()));
                return -1;
            }
        };

        let dm = &handle_ref.dm;
        match (|| -> Result<(), Box<dyn std::error::Error>> {
            let (parent_id, name) = dm.resolve_parent(filename_str)?;
            let data = if buf.is_null() {
                if buf_size > 0 {
                    return Err(
                        std::io::Error::other("Null buffer provided with a non-zero size").into(),
                    );
                }
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(buf, buf_size as usize) }
            };
            match dm.lookup(parent_id, &name) {
                Ok(existing_id) => {
                    dm.write_data(existing_id, offset, data, compression_mode)?;
                }
                Err(_) if offset == 0 => {
                    dm.create_file_with_data(parent_id, &name, data, compression_mode)?;
                }
                Err(_) => {
                    let inode_id = dm.create_file(parent_id, &name)?;
                    dm.write_data(inode_id, offset, data, compression_mode)?;
                }
            }
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
            handle_ref.set_last_error(Some(
                "Panic occurred during write_file_with_policy".to_string(),
            ));
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

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The C string is only dereferenced inside the panic-protected
        // closure.
        let path_str = match unsafe { cstr_to_str(path) } {
            Ok(s) => s,
            Err(msg) => {
                handle_ref.set_last_error(Some(msg.to_string()));
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
    // Never let a panic unwind across the `extern "C"` boundary.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let effective = handle_ref.dm.set_io_backend(b);
        effective as i32
    }));
    match res {
        Ok(val) => val,
        Err(_) => {
            handle_ref.set_last_error(Some("Panic occurred during set_io_backend".to_string()));
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_get_io_backend(handle: *mut OIFSHandle) -> i32 {
    let handle_ref = unsafe {
        if handle.is_null() {
            return -1;
        }
        &(*handle)
    };
    // Never let a panic unwind across the `extern "C"` boundary.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_ref.dm.io_backend() as i32
    }));
    match res {
        Ok(val) => val,
        Err(_) => {
            handle_ref.set_last_error(Some("Panic occurred during get_io_backend".to_string()));
            -1
        }
    }
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
    // Panic-free by construction (pure integer arithmetic), so no `catch_unwind`
    // is required here; this keeps the Kani proof harness below modelable.
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
    // Never let a panic unwind across the `extern "C"` boundary.
    let res = std::panic::catch_unwind(|| unsafe {
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
    });
    res.unwrap_or(-1)
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

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let filename_str = match unsafe { cstr_to_str(filename) } {
            Ok(s) => s,
            Err(msg) => {
                handle_ref.set_last_error(Some(msg.to_string()));
                return -1;
            }
        };

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

#[inline]
fn policy_to_compression_mode(policy_flags: u32, zstd_level: i32) -> crate::disk::CompressionMode {
    if policy_flags & OIFS_WRITE_POLICY_SEEKABLE_64K != 0 {
        crate::disk::CompressionMode::Seekable {
            chunk_size: crate::inode::CHUNK_SIZE_64K as u32,
            level: zstd_level,
        }
    } else if policy_flags & OIFS_WRITE_POLICY_RAW != 0 {
        crate::disk::CompressionMode::Never
    } else if policy_flags & OIFS_WRITE_POLICY_STREAM != 0 {
        crate::disk::CompressionMode::Stream { level: zstd_level }
    } else {
        crate::disk::CompressionMode::Auto
    }
}

pub const OIFS_STORAGE_MODE_MMAP: u8 = 0;
pub const OIFS_STORAGE_MODE_FILE_IO: u8 = 1;

/// Opens or creates an OIFS image with explicit storage mode (`0 = Mmap`, `1 = FileIo`)
/// and optional metadata WAL journaling (`journaled != 0`).
#[unsafe(no_mangle)]
pub extern "C" fn oifs_open_with_options(
    path: *const c_char,
    size: u64,
    password: *const c_char,
    storage_mode: u8,
    journaled: u8,
) -> *mut OIFSHandle {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if path.is_null() {
            return ptr::null_mut();
        }
        let Ok(path_str) = (unsafe { CStr::from_ptr(path) }).to_str() else {
            return ptr::null_mut();
        };
        let pwd_str = if password.is_null() {
            None
        } else {
            match unsafe { CStr::from_ptr(password) }.to_str() {
                Ok(s) if !s.is_empty() => Some(s),
                Ok(_) => None,
                Err(_) => return ptr::null_mut(),
            }
        };
        let mode = if storage_mode == OIFS_STORAGE_MODE_FILE_IO {
            crate::storage::StorageMode::FileIo
        } else {
            crate::storage::StorageMode::Mmap
        };
        let use_journal = journaled != 0;

        match DiskManager::open_with_options(path_str, size, pwd_str, use_journal, mode) {
            Ok(dm) => Box::into_raw(Box::new(OIFSHandle::new(dm))),
            Err(_) => ptr::null_mut(),
        }
    }));
    res.unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_set_file_io_cache_bytes(handle: *mut OIFSHandle, bytes: usize) -> i32 {
    if handle.is_null() {
        return -1;
    }
    let handle_ref = unsafe { &(*handle) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_ref.dm.set_file_io_cache_bytes(bytes);
        0
    }));
    res.unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_flush(handle: *mut OIFSHandle) -> i32 {
    if handle.is_null() {
        return -1;
    }
    let handle_ref = unsafe { &(*handle) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match handle_ref.dm.flush() {
            Ok(()) => {
                handle_ref.set_last_error(None);
                0
            }
            Err(e) => {
                handle_ref.set_last_error(Some(e.to_string()));
                -1
            }
        }
    }));
    res.unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_set_durability_mode(handle: *mut OIFSHandle, mode: u8) -> i32 {
    if handle.is_null() {
        return -1;
    }
    let handle_ref = unsafe { &(*handle) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let d = crate::disk::DurabilityMode::from_u8(mode);
        handle_ref.dm.set_durability_mode(d);
        0
    }));
    res.unwrap_or(-1)
}

/// Resolves a filesystem path to its `inode_id` (`>= 0` on success, `-1` on error).
#[unsafe(no_mangle)]
pub extern "C" fn oifs_resolve_path(handle: *mut OIFSHandle, path: *const c_char) -> i64 {
    if handle.is_null() {
        return -1;
    }
    let handle_ref = unsafe { &(*handle) };
    if path.is_null() {
        handle_ref.set_last_error(Some("Null path provided".to_string()));
        return -1;
    }
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path_str = match unsafe { cstr_to_str(path) } {
            Ok(s) => s,
            Err(msg) => {
                handle_ref.set_last_error(Some(msg.to_string()));
                return -1;
            }
        };
        match handle_ref.dm.resolve_path(path_str) {
            Ok(id) => {
                handle_ref.set_last_error(None);
                id as i64
            }
            Err(e) => {
                handle_ref.set_last_error(Some(e.to_string()));
                -1
            }
        }
    }));
    res.unwrap_or(-1)
}

/// Atomically creates a new file and writes its initial payload in a single transaction.
/// Returns the new `inode_id` (`>= 0`) on success, or `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_create_file_with_data(
    handle: *mut OIFSHandle,
    path: *const c_char,
    buf: *const u8,
    buf_size: u64,
    policy_flags: u32,
    zstd_level: i32,
) -> i64 {
    if handle.is_null() {
        return -1;
    }
    let handle_ref = unsafe { &(*handle) };
    if path.is_null() || (buf.is_null() && buf_size > 0) {
        handle_ref.set_last_error(Some("Null argument provided".to_string()));
        return -1;
    }
    let Ok(len) = usize::try_from(buf_size) else {
        handle_ref.set_last_error(Some("Buffer size overflows usize".to_string()));
        return -1;
    };
    let compression_mode = policy_to_compression_mode(policy_flags, zstd_level);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let path_str = match unsafe { cstr_to_str(path) } {
            Ok(s) => s,
            Err(msg) => {
                handle_ref.set_last_error(Some(msg.to_string()));
                return -1;
            }
        };
        let data = if len == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(buf, len) }
        };
        let dm = &handle_ref.dm;
        match (|| -> Result<u64, Box<dyn std::error::Error>> {
            let (parent_id, name) = dm.resolve_parent(path_str)?;
            let id = dm.create_file_with_data(parent_id, &name, data, compression_mode)?;
            Ok(id)
        })() {
            Ok(id) => {
                handle_ref.set_last_error(None);
                id as i64
            }
            Err(e) => {
                handle_ref.set_last_error(Some(e.to_string()));
                -1
            }
        }
    }));
    res.unwrap_or(-1)
}

/// Performs a positional read by `inode_id` without path resolution overhead.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_read_at_inode(
    handle: *mut OIFSHandle,
    inode_id: u64,
    offset: u64,
    buf: *mut u8,
    buf_size: u64,
) -> i64 {
    if handle.is_null() {
        return -1;
    }
    let handle_ref = unsafe { &(*handle) };
    if buf.is_null() {
        if buf_size == 0 {
            return 0;
        }
        handle_ref.set_last_error(Some("Null buffer provided".to_string()));
        return -1;
    }
    let Ok(len) = usize::try_from(buf_size) else {
        handle_ref.set_last_error(Some("Buffer size overflows usize".to_string()));
        return -1;
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let out_slice = unsafe { std::slice::from_raw_parts_mut(buf, len) };
        match handle_ref.dm.read_at(inode_id, offset, out_slice) {
            Ok(n) => {
                handle_ref.set_last_error(None);
                n as i64
            }
            Err(e) => {
                handle_ref.set_last_error(Some(e.to_string()));
                -1
            }
        }
    }));
    res.unwrap_or(-1)
}

/// Scatter-gather batch read item for [`oifs_read_at_batch`].
#[repr(C)]
pub struct OIFSReadBatchItem {
    pub inode_id: u64,
    pub offset: u64,
    pub buf: *mut u8,
    pub buf_size: u64,
    pub result: i64,
}

/// Executes a batch of positional reads under a single read lock and single I/O engine submission.
/// Populates `items[i].result` with the number of bytes read (`>= 0`) or `-1` on per-item error.
/// Returns `0` if all items succeeded, or `-1` if any item failed.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_read_at_batch(
    handle: *mut OIFSHandle,
    items: *mut OIFSReadBatchItem,
    count: usize,
) -> i32 {
    if handle.is_null() {
        return -1;
    }
    let handle_ref = unsafe { &(*handle) };
    if count == 0 {
        return 0;
    }
    if items.is_null() {
        handle_ref.set_last_error(Some("Null batch items pointer".to_string()));
        return -1;
    }
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let slice = unsafe { std::slice::from_raw_parts_mut(items, count) };
        let mut reqs = Vec::with_capacity(count);
        for item in slice.iter_mut() {
            if item.buf.is_null() && item.buf_size > 0 {
                item.result = -1;
                handle_ref.set_last_error(Some("Null buffer in batch item".to_string()));
                return -1;
            }
            let Ok(len) = usize::try_from(item.buf_size) else {
                item.result = -1;
                return -1;
            };
            let buf_slice = if len == 0 {
                &mut [][..]
            } else {
                unsafe { std::slice::from_raw_parts_mut(item.buf, len) }
            };
            reqs.push(crate::disk::ReadRequest {
                inode_id: item.inode_id,
                offset: item.offset,
                buf: buf_slice,
            });
        }
        let results = handle_ref.dm.read_at_batch(&mut reqs);
        let mut status = 0;
        for (item, r) in slice.iter_mut().zip(results) {
            match r {
                Ok(n) => item.result = n as i64,
                Err(e) => {
                    item.result = -1;
                    status = -1;
                    handle_ref.set_last_error(Some(e.to_string()));
                }
            }
        }
        status
    }));
    res.unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// Shared ReadSession & Pinned FileHandle with Arc-protected FFI lifetime
// ---------------------------------------------------------------------------

struct FfiSharedSession {
    // Field drop order is top-to-bottom: `session` (holding RwLockReadGuard) is dropped
    // BEFORE `_dm` (owning the Arc<RwLock<DiskManagerInner>>).
    session: crate::disk::ReadSession<'static>,
    _dm: Box<DiskManager>,
}

impl FfiSharedSession {
    #[allow(clippy::arc_with_non_send_sync)]
    fn new(dm: DiskManager) -> std::sync::Arc<Self> {
        let boxed_dm = Box::new(dm);
        let session_ref: crate::disk::ReadSession<'_> = boxed_dm.begin_read();
        // SAFETY: `session_ref` borrows from `*boxed_dm`, which lives at a stable heap
        // address inside `FfiSharedSession` and is dropped strictly AFTER `session`.
        let session_static: crate::disk::ReadSession<'static> =
            unsafe { std::mem::transmute(session_ref) };
        std::sync::Arc::new(Self {
            session: session_static,
            _dm: boxed_dm,
        })
    }
}

pub struct OIFSReadSession {
    shared: std::sync::Arc<FfiSharedSession>,
}

pub struct OIFSFileHandle {
    // Field drop order is top-to-bottom: `file` (borrowing from ` _shared.session`) is
    // dropped BEFORE `_shared`. Holds its own `Arc<FfiSharedSession>` so even if a C caller
    // destroys `OIFSReadSession` before `OIFSFileHandle`, the underlying read lock and `mmap`
    // stay valid (zero UAF).
    file: crate::disk::FileReadHandle<'static, 'static>,
    _shared: std::sync::Arc<FfiSharedSession>,
}

impl OIFSFileHandle {
    fn from_session(
        shared: std::sync::Arc<FfiSharedSession>,
        inode_id: u64,
    ) -> Result<Self, crate::disk::DiskManagerError> {
        let fh: crate::disk::FileReadHandle<'_, 'static> = shared.session.open_file(inode_id)?;
        // SAFETY: `fh` borrows from `shared.session`, which is owned by `_shared: Arc<FfiSharedSession>`
        // stored alongside `file` and dropped strictly AFTER `file`.
        let file_static: crate::disk::FileReadHandle<'static, 'static> =
            unsafe { std::mem::transmute(fh) };
        Ok(Self {
            file: file_static,
            _shared: shared,
        })
    }
}

/// Acquires a shared read session over `handle`. Must be released with [`oifs_end_read`].
#[unsafe(no_mangle)]
pub extern "C" fn oifs_begin_read(handle: *mut OIFSHandle) -> *mut OIFSReadSession {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle_ref = unsafe { &(*handle) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let shared = FfiSharedSession::new(handle_ref.dm.clone());
        Box::into_raw(Box::new(OIFSReadSession { shared }))
    }));
    res.unwrap_or(ptr::null_mut())
}

/// Releases an [`OIFSReadSession`].
#[unsafe(no_mangle)]
pub extern "C" fn oifs_end_read(session: *mut OIFSReadSession) {
    if !session.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            let _ = Box::from_raw(session);
        }));
    }
}

/// Opens a pinned [`OIFSFileHandle`] by path within an [`OIFSReadSession`].
#[unsafe(no_mangle)]
pub extern "C" fn oifs_session_open_file(
    session: *const OIFSReadSession,
    path: *const c_char,
) -> *mut OIFSFileHandle {
    if session.is_null() || path.is_null() {
        return ptr::null_mut();
    }
    let sess_ref = unsafe { &(*session) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(path_str) = (unsafe { cstr_to_str(path) }) else {
            return ptr::null_mut();
        };
        let trimmed = path_str.trim_matches('/');
        let root = sess_ref.shared.session.root_inode();
        let inode_id = if trimmed.is_empty() {
            root
        } else {
            let mut cur = root;
            for part in trimmed.split('/') {
                if part.is_empty() || part == "." {
                    continue;
                }
                match sess_ref.shared.session.lookup(cur, part) {
                    Ok(next) => cur = next,
                    Err(_) => return ptr::null_mut(),
                }
            }
            cur
        };
        match OIFSFileHandle::from_session(std::sync::Arc::clone(&sess_ref.shared), inode_id) {
            Ok(fh) => Box::into_raw(Box::new(fh)),
            Err(_) => ptr::null_mut(),
        }
    }));
    res.unwrap_or(ptr::null_mut())
}

/// Opens a pinned [`OIFSFileHandle`] by `inode_id` within an [`OIFSReadSession`].
#[unsafe(no_mangle)]
pub extern "C" fn oifs_session_open_inode(
    session: *const OIFSReadSession,
    inode_id: u64,
) -> *mut OIFSFileHandle {
    if session.is_null() {
        return ptr::null_mut();
    }
    let sess_ref = unsafe { &(*session) };
    let res =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || match OIFSFileHandle::from_session(std::sync::Arc::clone(&sess_ref.shared), inode_id)
            {
                Ok(fh) => Box::into_raw(Box::new(fh)),
                Err(_) => ptr::null_mut(),
            },
        ));
    res.unwrap_or(ptr::null_mut())
}

/// Closes a pinned [`OIFSFileHandle`].
#[unsafe(no_mangle)]
pub extern "C" fn oifs_file_close(file: *mut OIFSFileHandle) {
    if !file.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            let _ = Box::from_raw(file);
        }));
    }
}

/// Returns the logical size in bytes of a pinned [`OIFSFileHandle`].
#[unsafe(no_mangle)]
pub extern "C" fn oifs_file_size(file: *const OIFSFileHandle) -> u64 {
    if file.is_null() {
        return 0;
    }
    let file_ref = unsafe { &(*file) };
    file_ref.file.size()
}

/// Returns the `inode_id` of a pinned [`OIFSFileHandle`].
#[unsafe(no_mangle)]
pub extern "C" fn oifs_file_inode_id(file: *const OIFSFileHandle) -> u64 {
    if file.is_null() {
        return 0;
    }
    let file_ref = unsafe { &(*file) };
    file_ref.file.inode_id()
}

/// Positional read from a pinned [`OIFSFileHandle`] (zero `RwLock` acquisition, zero inode lookup).
#[unsafe(no_mangle)]
pub extern "C" fn oifs_file_read_at(
    file: *const OIFSFileHandle,
    offset: u64,
    buf: *mut u8,
    buf_size: u64,
) -> i64 {
    if file.is_null() {
        return -1;
    }
    if buf.is_null() {
        return if buf_size == 0 { 0 } else { -1 };
    }
    let Ok(len) = usize::try_from(buf_size) else {
        return -1;
    };
    let file_ref = unsafe { &(*file) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let out_slice = unsafe { std::slice::from_raw_parts_mut(buf, len) };
        match file_ref.file.read_at(offset, out_slice) {
            Ok(n) => n as i64,
            Err(_) => -1,
        }
    }));
    res.unwrap_or(-1)
}

/// True Zero-Copy view into the memory-mapped image for a pinned [`OIFSFileHandle`].
///
/// - Returns `0` on zero-copy success, setting `*out_ptr` and `*out_len` to a direct slice
///   into the mapping valid as long as `file` (or its parent `OIFSReadSession`) remains open.
/// - Returns `1` when zero-copy is unavailable for this range (e.g. `StorageMode::FileIo`,
///   compressed/encrypted file, sparse hole, or non-contiguous block boundary), signaling the
///   caller to fall back to [`oifs_file_read_at`].
/// - Returns `-1` on invalid arguments or error.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_file_mmap_slice(
    file: *const OIFSFileHandle,
    offset: u64,
    max_len: u64,
    out_ptr: *mut *const u8,
    out_len: *mut u64,
) -> i32 {
    if file.is_null() || out_ptr.is_null() || out_len.is_null() {
        return -1;
    }
    let file_ref = unsafe { &(*file) };
    let clamp_len = usize::try_from(max_len).unwrap_or(usize::MAX);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match file_ref.file.mmap_slice(offset, clamp_len) {
            Ok(Some(slice)) => unsafe {
                *out_ptr = slice.as_ptr();
                *out_len = slice.len() as u64;
                0
            },
            Ok(None) => unsafe {
                *out_ptr = ptr::null();
                *out_len = 0;
                1
            },
            Err(_) => -1,
        }
    }));
    res.unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// Async FFI: OIFSAsyncFuture (Inline Fast-Path + Drop Barrier + Group Commit)
// ---------------------------------------------------------------------------

pub struct OIFSAsyncFuture {
    task: Option<crate::disk::OifsAsyncTask<Result<i64, String>>>,
    last_error: Option<String>,
}

/// Asynchronously reads up to `buf_size` bytes from `inode_id` at `offset` into `buf`.
///
/// - Hot cache-hit and `Mmap` reads complete immediately on the calling thread (`oifs_async_poll == 1`)
///   with zero thread context switch.
/// - Cold reads or compressed/encrypted decodes execute on the persistent background pool.
/// - Safe against Use-After-Free: if `oifs_async_free` is called while a background read is still
///   writing into `buf`, `oifs_async_free` waits for completion before returning.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_read_at_async(
    handle: *mut OIFSHandle,
    inode_id: u64,
    offset: u64,
    buf: *mut u8,
    buf_size: u64,
) -> *mut OIFSAsyncFuture {
    if handle.is_null() || (buf.is_null() && buf_size > 0) {
        return ptr::null_mut();
    }
    let Ok(len) = usize::try_from(buf_size) else {
        return ptr::null_mut();
    };
    let handle_ref = unsafe { &(*handle) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if len == 0 {
            return Box::into_raw(Box::new(OIFSAsyncFuture {
                task: Some(crate::disk::OifsAsyncTask::ready(Ok(0))),
                last_error: None,
            }));
        }
        let out_slice = unsafe { std::slice::from_raw_parts_mut(buf, len) };
        if let Some(inline_res) = handle_ref
            .dm
            .try_read_at_inline_into(inode_id, offset, out_slice)
        {
            let mapped = inline_res.map(|n| n as i64).map_err(|e| e.to_string());
            return Box::into_raw(Box::new(OIFSAsyncFuture {
                task: Some(crate::disk::OifsAsyncTask::ready(mapped)),
                last_error: None,
            }));
        }

        let dm = handle_ref.dm.clone();
        let raw_addr = buf as usize;
        let task = crate::disk::OifsAsyncTask::spawn_with_drop_policy(
            move || {
                // SAFETY: `wait_on_drop = true` guarantees `OIFSAsyncFuture` will not be destroyed
                // before this closure returns.
                let dst = unsafe { std::slice::from_raw_parts_mut(raw_addr as *mut u8, len) };
                dm.read_at(inode_id, offset, dst)
                    .map(|n| n as i64)
                    .map_err(|e| e.to_string())
            },
            true,
        );
        Box::into_raw(Box::new(OIFSAsyncFuture {
            task: Some(task),
            last_error: None,
        }))
    }));
    res.unwrap_or(ptr::null_mut())
}

/// Asynchronously creates a file and populates it with `buf[..buf_size]` via the Group-Commit
/// write pipeline. The caller's `path` and `buf` are copied before returning, so they may be
/// freed or reused immediately.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_create_file_with_data_async(
    handle: *mut OIFSHandle,
    path: *const c_char,
    buf: *const u8,
    buf_size: u64,
    policy_flags: u32,
    zstd_level: i32,
) -> *mut OIFSAsyncFuture {
    if handle.is_null() || path.is_null() || (buf.is_null() && buf_size > 0) {
        return ptr::null_mut();
    }
    let Ok(len) = usize::try_from(buf_size) else {
        return ptr::null_mut();
    };
    let handle_ref = unsafe { &(*handle) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(path_str) = (unsafe { cstr_to_str(path) }).map(str::to_owned) else {
            return ptr::null_mut();
        };
        let payload = if len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(buf, len) }.to_vec()
        };
        let mode = policy_to_compression_mode(policy_flags, zstd_level);
        let state = crate::disk::AsyncTaskState::new_pending();
        let state_done = std::sync::Arc::clone(&state);
        crate::disk::submit_group_write_op(
            handle_ref.dm.clone(),
            move |dm| {
                let (parent_id, name) = dm.resolve_parent(&path_str)?;
                dm.create_file_with_data(parent_id, &name, &payload, mode)
            },
            move |res| {
                state_done.complete(res.map(|id| id as i64).map_err(|e| e.to_string()));
            },
        );
        Box::into_raw(Box::new(OIFSAsyncFuture {
            task: Some(crate::disk::OifsAsyncTask::from_state(state, false)),
            last_error: None,
        }))
    }));
    res.unwrap_or(ptr::null_mut())
}

/// Asynchronously writes `buf[..buf_size]` to `path` at `offset` via the Group-Commit write pipeline.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_write_file_async(
    handle: *mut OIFSHandle,
    path: *const c_char,
    offset: u64,
    buf: *const u8,
    buf_size: u64,
    policy_flags: u32,
    zstd_level: i32,
) -> *mut OIFSAsyncFuture {
    if handle.is_null() || path.is_null() || (buf.is_null() && buf_size > 0) {
        return ptr::null_mut();
    }
    let Ok(len) = usize::try_from(buf_size) else {
        return ptr::null_mut();
    };
    let handle_ref = unsafe { &(*handle) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(path_str) = (unsafe { cstr_to_str(path) }).map(str::to_owned) else {
            return ptr::null_mut();
        };
        let payload = if len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(buf, len) }.to_vec()
        };
        let mode = policy_to_compression_mode(policy_flags, zstd_level);
        let state = crate::disk::AsyncTaskState::new_pending();
        let state_done = std::sync::Arc::clone(&state);
        crate::disk::submit_group_write_op(
            handle_ref.dm.clone(),
            move |dm| {
                let (parent_id, name) = dm.resolve_parent(&path_str)?;
                match dm.lookup(parent_id, &name) {
                    Ok(existing_id) => {
                        dm.write_data(existing_id, offset, &payload, mode)?;
                        Ok(existing_id)
                    }
                    Err(_) if offset == 0 => {
                        dm.create_file_with_data(parent_id, &name, &payload, mode)
                    }
                    Err(_) => {
                        let id = dm.create_file(parent_id, &name)?;
                        dm.write_data(id, offset, &payload, mode)?;
                        Ok(id)
                    }
                }
            },
            move |res| {
                state_done.complete(res.map(|id| id as i64).map_err(|e| e.to_string()));
            },
        );
        Box::into_raw(Box::new(OIFSAsyncFuture {
            task: Some(crate::disk::OifsAsyncTask::from_state(state, false)),
            last_error: None,
        }))
    }));
    res.unwrap_or(ptr::null_mut())
}

/// Asynchronously flushes all pending data and WAL transactions on the background pool.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_flush_async(handle: *mut OIFSHandle) -> *mut OIFSAsyncFuture {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let handle_ref = unsafe { &(*handle) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let dm = handle_ref.dm.clone();
        let task = crate::disk::OifsAsyncTask::spawn(move || {
            dm.flush().map(|()| 0i64).map_err(|e| e.to_string())
        });
        Box::into_raw(Box::new(OIFSAsyncFuture {
            task: Some(task),
            last_error: None,
        }))
    }));
    res.unwrap_or(ptr::null_mut())
}

/// Polls an [`OIFSAsyncFuture`] without blocking (`1` = ready, `0` = pending, `-1` = invalid/null).
#[unsafe(no_mangle)]
pub extern "C" fn oifs_async_poll(fut: *const OIFSAsyncFuture) -> i32 {
    if fut.is_null() {
        return -1;
    }
    let fut_ref = unsafe { &(*fut) };
    match &fut_ref.task {
        Some(t) => i32::from(t.is_ready()),
        None => 1,
    }
}

/// Blocks until `fut` completes and returns its result (`bytes_read` / `inode_id` / `0`, or `-1`
/// on error). Does not free `fut` so the caller may inspect [`oifs_async_last_error`] on failure;
/// always release `fut` with [`oifs_async_free`].
#[unsafe(no_mangle)]
pub extern "C" fn oifs_async_wait(fut: *mut OIFSAsyncFuture) -> i64 {
    if fut.is_null() {
        return -1;
    }
    let fut_ref = unsafe { &mut (*fut) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(task) = fut_ref.task.take() else {
            return -1;
        };
        match task.wait() {
            Ok(val) => {
                fut_ref.last_error = None;
                val
            }
            Err(err) => {
                fut_ref.last_error = Some(err);
                -1
            }
        }
    }));
    res.unwrap_or(-1)
}

/// Retrieves the error message from a failed [`OIFSAsyncFuture`].
#[unsafe(no_mangle)]
pub extern "C" fn oifs_async_last_error(
    fut: *const OIFSAsyncFuture,
    buf: *mut c_char,
    buf_size: u32,
) -> i32 {
    if fut.is_null() || buf.is_null() || buf_size == 0 {
        return -1;
    }
    let fut_ref = unsafe { &(*fut) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let err_str = fut_ref.last_error.as_deref().unwrap_or("No error");
        let sanitized = err_str.replace('\0', " ");
        let Ok(c_err) = CString::new(sanitized) else {
            return -1;
        };
        let bytes = c_err.as_bytes_with_nul();
        let to_copy = bytes.len().min(buf_size as usize);
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

/// Releases an [`OIFSAsyncFuture`]. If the task is still writing to a caller buffer, blocks
/// until the write finishes so the caller's buffer is never accessed after `oifs_async_free` returns.
#[unsafe(no_mangle)]
pub extern "C" fn oifs_async_free(fut: *mut OIFSAsyncFuture) {
    if !fut.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            let _ = Box::from_raw(fut);
        }));
    }
}

/// Callback invoked when [`oifs_read_at_async_cb`] completes (`status >= 0` is bytes read and
/// `data[..data_len]` is valid for the duration of the callback; `status < 0` indicates error).
pub type OifsAsyncReadCallback = extern "C" fn(i64, *const u8, usize, *mut c_void);

/// Callback invoked when [`oifs_create_file_with_data_async_cb`] completes (`status >= 0` is
/// the created `inode_id`; `status < 0` indicates error).
pub type OifsAsyncStatusCallback = extern "C" fn(i64, *mut c_void);

#[unsafe(no_mangle)]
pub extern "C" fn oifs_read_at_async_cb(
    handle: *mut OIFSHandle,
    inode_id: u64,
    offset: u64,
    len: u64,
    cb: Option<OifsAsyncReadCallback>,
    user_data: *mut c_void,
) -> i32 {
    if handle.is_null() {
        return -1;
    }
    let Some(cb_fn) = cb else {
        return -1;
    };
    let Ok(max_len) = usize::try_from(len) else {
        return -1;
    };
    let handle_ref = unsafe { &(*handle) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Inline Fast-Path probe
        if max_len <= crate::storage::MAX_CACHED_EXTENT
            && let Ok(mut buf) = DiskManager::try_alloc_zeroed_vec(max_len)
            && let Some(inline_res) = handle_ref
                .dm
                .try_read_at_inline_into(inode_id, offset, &mut buf)
        {
            match inline_res {
                Ok(n) => cb_fn(n as i64, buf.as_ptr(), n, user_data),
                Err(_) => cb_fn(-1, ptr::null(), 0, user_data),
            }
            return 0;
        }
        let dm = handle_ref.dm.clone();
        let ud_addr = user_data as usize;
        crate::disk::submit_async_job(Box::new(move || {
            let ud = ud_addr as *mut c_void;
            match dm.read_at_vec(inode_id, offset, max_len) {
                Ok(v) => cb_fn(v.len() as i64, v.as_ptr(), v.len(), ud),
                Err(_) => cb_fn(-1, ptr::null(), 0, ud),
            }
        }));
        0
    }));
    res.unwrap_or(-1)
}

#[unsafe(no_mangle)]
pub extern "C" fn oifs_create_file_with_data_async_cb(
    handle: *mut OIFSHandle,
    path: *const c_char,
    buf: *const u8,
    buf_size: u64,
    policy_flags: u32,
    zstd_level: i32,
    cb: Option<OifsAsyncStatusCallback>,
    user_data: *mut c_void,
) -> i32 {
    if handle.is_null() || path.is_null() || (buf.is_null() && buf_size > 0) {
        return -1;
    }
    let Some(cb_fn) = cb else {
        return -1;
    };
    let Ok(len) = usize::try_from(buf_size) else {
        return -1;
    };
    let handle_ref = unsafe { &(*handle) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(path_str) = (unsafe { cstr_to_str(path) }).map(str::to_owned) else {
            return -1;
        };
        let payload = if len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(buf, len) }.to_vec()
        };
        let mode = policy_to_compression_mode(policy_flags, zstd_level);
        let ud_addr = user_data as usize;
        crate::disk::submit_group_write_op(
            handle_ref.dm.clone(),
            move |dm| {
                let (parent_id, name) = dm.resolve_parent(&path_str)?;
                dm.create_file_with_data(parent_id, &name, &payload, mode)
            },
            move |res| {
                let ud = ud_addr as *mut c_void;
                match res {
                    Ok(id) => cb_fn(id as i64, ud),
                    Err(_) => cb_fn(-1, ud),
                }
            },
        );
        0
    }));
    res.unwrap_or(-1)
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

    /// Formally prove that `policy_to_compression_mode` is total and panic-free for
    /// all symbolic `(policy_flags, zstd_level)` inputs and adheres to the precedence order
    /// (`SEEKABLE_64K > RAW > STREAM > DEFAULT`).
    #[kani::proof]
    fn proof_policy_to_compression_mode_total_and_panic_free() {
        let policy_flags: u32 = kani::any();
        let zstd_level: i32 = kani::any();

        let mode = policy_to_compression_mode(policy_flags, zstd_level);
        if policy_flags & OIFS_WRITE_POLICY_SEEKABLE_64K != 0 {
            assert_eq!(
                mode,
                crate::disk::CompressionMode::Seekable {
                    chunk_size: crate::inode::CHUNK_SIZE_64K as u32,
                    level: zstd_level,
                }
            );
        } else if policy_flags & OIFS_WRITE_POLICY_RAW != 0 {
            assert_eq!(mode, crate::disk::CompressionMode::Never);
        } else if policy_flags & OIFS_WRITE_POLICY_STREAM != 0 {
            assert_eq!(
                mode,
                crate::disk::CompressionMode::Stream { level: zstd_level }
            );
        } else {
            assert_eq!(mode, crate::disk::CompressionMode::Auto);
        }
    }
}
