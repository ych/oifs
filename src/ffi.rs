use crate::disk::DiskManager;
use crate::directory::DirectoryIterator;
use crate::inode::FileType;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::ptr;

// Opaque handle for C
pub struct OIFSHandle {
    dm: DiskManager,
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
            let handle = Box::new(OIFSHandle { dm });
            Box::into_raw(handle)
        }
        Err(_) => ptr::null_mut(),
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

    // Use a catch logic to convert internal errors to -1
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let root_inode = dm.read_inode(root_inode_id)?;
        // println!("DEBUG: Root Inode Mode: {:?} Block[0]: {}", root_inode.mode, root_inode.blocks[0]);
        if root_inode.mode != FileType::Directory {
             // println!("DEBUG: Not directory");
             return Ok(());
        }

        let block_id = root_inode.blocks[0];
        if block_id == 0 {
             // println!("DEBUG: Block 0");
             return Ok(());
        }

        if let Some(block_data) = dm.get_block_copy(block_id) {
             let iter = DirectoryIterator::new(&block_data);
             for entry_res in iter {
                 match entry_res {
                     Ok(entry) => {
                         // println!("DEBUG: Found entry: {}", entry.name);
                         if let Ok(inode) = dm.read_inode(entry.inode) {
                             let c_name = CString::new(entry.name).unwrap_or_default();
                             cb(c_name.as_ptr(), inode.size, inode.modified_at, user_data);
                         } else {
                             // println!("DEBUG: Failed to read inode {}", entry.inode);
                         }
                     }
                     Err(_e) => {
                         // println!("DEBUG: Entry error: {}", e);
                     }
                 }
             }
        }
        Ok(())
    })();

    match result {
        Ok(_) => 0,
        Err(_) => -1,
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
        Err(_) => return -1,
    };

    let dm = &handle_ref.dm;
    let root_inode_id = dm.superblock().root_inode;

    match dm.create_file(root_inode_id, filename) {
        Ok(_) => 0,
        Err(_) => -1,
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
        Err(_) => return -1,
    };

    let dm = &handle_ref.dm;
    let root_inode_id = dm.superblock().root_inode;

    match dm.delete_file(root_inode_id, filename) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}
