use oifs::disk::{CompressionMode, DiskManager};
use oifs::ffi::{oifs_close, oifs_open, oifs_read_at, oifs_read_file, oifs_write_file};
use std::ffi::CString;
use std::fs;
use std::path::Path;

struct CleanupGuard(&'static str);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}

#[test]
fn test_read_at_basic_and_boundaries() {
    let img_path = "test_read_at_basic.img";
    let _guard = CleanupGuard(img_path);
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let dm = DiskManager::open(img_path, 20 * 1024 * 1024).expect("Failed to create disk");
    let root_id = dm.superblock().root_inode;
    let file_id = dm.create_file(root_id, "test_file.bin").expect("create_file");

    // 10,000 bytes pattern
    let payload: Vec<u8> = (0..10_000).map(|i| (i % 251) as u8).collect();
    dm.write_data(file_id, 0, &payload, CompressionMode::Never).expect("write_data");

    // 1. Read at offset 0
    let mut buf = vec![0u8; 100];
    let n = dm.read_at(file_id, 0, &mut buf).expect("read_at 0");
    assert_eq!(n, 100);
    assert_eq!(&buf[..], &payload[0..100]);

    // 2. Read at middle offset within block 0
    let mut buf = vec![0u8; 50];
    let n = dm.read_at(file_id, 250, &mut buf).expect("read_at 250");
    assert_eq!(n, 50);
    assert_eq!(&buf[..], &payload[250..300]);

    // 3. Read spanning block boundary (block 0 ends at 4096, block 1 starts at 4096)
    let mut buf = vec![0u8; 256];
    let n = dm.read_at(file_id, 4000, &mut buf).expect("read_at across boundary");
    assert_eq!(n, 256);
    assert_eq!(&buf[..], &payload[4000..4256]);

    // 4. Read near EOF where buffer is larger than remaining bytes
    let mut buf = vec![0u8; 100];
    let n = dm.read_at(file_id, 9950, &mut buf).expect("read_at near EOF");
    assert_eq!(n, 50);
    assert_eq!(&buf[..50], &payload[9950..10000]);

    // 5. Read at exact EOF
    let mut buf = vec![0u8; 100];
    let n = dm.read_at(file_id, 10000, &mut buf).expect("read_at exact EOF");
    assert_eq!(n, 0);

    // 6. Read past EOF
    let mut buf = vec![0u8; 100];
    let n = dm.read_at(file_id, 15000, &mut buf).expect("read_at past EOF");
    assert_eq!(n, 0);

    // 7. Read with zero-sized buffer
    let mut buf = [];
    let n = dm.read_at(file_id, 50, &mut buf).expect("read_at empty buffer");
    assert_eq!(n, 0);
}

#[test]
fn test_read_at_large_indirect_blocks() {
    let img_path = "test_read_at_large.img";
    let _guard = CleanupGuard(img_path);
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    // 30MB image to hold > 3MB file
    let dm = DiskManager::open(img_path, 30 * 1024 * 1024).expect("Failed to create disk");
    let root_id = dm.superblock().root_inode;
    let file_id = dm.create_file(root_id, "large_file.dat").expect("create_file");

    // File size: 2.5 MB (crosses direct [48KB], single indirect [48KB..2096KB], and double indirect [>2096KB])
    let size = 2_500_000usize;
    let payload: Vec<u8> = (0..size).map(|i| (i * 7 % 256) as u8).collect();
    dm.write_data(file_id, 0, &payload, CompressionMode::Never).expect("write_data");

    // Test direct block read
    let mut buf = vec![0u8; 1024];
    let n = dm.read_at(file_id, 1024, &mut buf).expect("read direct");
    assert_eq!(n, 1024);
    assert_eq!(&buf[..], &payload[1024..2048]);

    // Test single indirect block read (e.g., at 100 KB)
    let offset = 100 * 1024;
    let mut buf = vec![0u8; 8192];
    let n = dm.read_at(file_id, offset as u64, &mut buf).expect("read single indirect");
    assert_eq!(n, 8192);
    assert_eq!(&buf[..], &payload[offset..offset + 8192]);

    // Test double indirect block read (e.g., at 2.2 MB)
    let offset = 2_200_000usize;
    let mut buf = vec![0u8; 16384];
    let n = dm.read_at(file_id, offset as u64, &mut buf).expect("read double indirect");
    assert_eq!(n, 16384);
    assert_eq!(&buf[..], &payload[offset..offset + 16384]);

    // Test unaligned chunk spanning multiple blocks across double-indirect range
    let offset = 2_150_123usize;
    let mut buf = vec![0u8; 12_345];
    let n = dm.read_at(file_id, offset as u64, &mut buf).expect("read unaligned double indirect");
    assert_eq!(n, 12_345);
    assert_eq!(&buf[..], &payload[offset..offset + 12_345]);
}

#[test]
fn test_read_at_compressed_file() {
    let img_path = "test_read_at_compressed.img";
    let _guard = CleanupGuard(img_path);
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let dm = DiskManager::open(img_path, 10 * 1024 * 1024).expect("Failed to create disk");
    let root_id = dm.superblock().root_inode;
    let file_id = dm.create_file(root_id, "compressed.txt").expect("create_file");

    // Highly compressible payload: 64KB
    let payload = b"The quick brown fox jumps over the lazy dog. Repetition for compression! "
        .repeat(1000);
    dm.write_data(file_id, 0, &payload, CompressionMode::Always).expect("write_data");

    let inode = dm.read_inode(file_id).expect("read_inode");
    assert!(inode.compressed_size > 0, "File should be compressed");

    // Read middle slice
    let offset = 12345usize;
    let mut buf = vec![0u8; 500];
    let n = dm.read_at(file_id, offset as u64, &mut buf).expect("read_at on compressed");
    assert_eq!(n, 500);
    assert_eq!(&buf[..], &payload[offset..offset + 500]);
}

#[test]
fn test_read_at_encrypted_file() {
    let img_path = "test_read_at_enc.img";
    let _guard = CleanupGuard(img_path);
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let dm = DiskManager::open_with_password(img_path, 10 * 1024 * 1024, Some("safe_password"))
        .expect("Failed to create encrypted disk");
    let root_id = dm.superblock().root_inode;
    let file_id = dm.create_file(root_id, "secret.dat").expect("create_file");

    let payload: Vec<u8> = (0..20_000).map(|i| (i * 13 % 256) as u8).collect();
    dm.write_data(file_id, 0, &payload, CompressionMode::Never).expect("write_data");

    let offset = 4090usize;
    let mut buf = vec![0u8; 100];
    let n = dm.read_at(file_id, offset as u64, &mut buf).expect("read_at encrypted");
    assert_eq!(n, 100);
    assert_eq!(&buf[..], &payload[offset..offset + 100]);
}

#[test]
fn test_ffi_read_at_and_read_file() {
    let img_path = "test_ffi_read_at.img";
    let _guard = CleanupGuard(img_path);
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let c_path = CString::new(img_path).unwrap();
    let handle = oifs_open(c_path.as_ptr(), 10 * 1024 * 1024);
    assert!(!handle.is_null());

    let filename = CString::new("data.bin").unwrap();
    let payload = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

    let write_res = oifs_write_file(
        handle,
        filename.as_ptr(),
        payload.as_ptr(),
        payload.len() as u64,
    );
    assert_eq!(write_res, 0);

    // 1. Test oifs_read_at with offset
    let mut buf = [0u8; 10];
    let bytes_read = oifs_read_at(
        handle,
        filename.as_ptr(),
        10,
        buf.as_mut_ptr(),
        buf.len() as u64,
    );
    assert_eq!(bytes_read, 10);
    assert_eq!(&buf, &payload[10..20]);

    // 2. Test oifs_read_file (which delegates to oifs_read_at with offset 0)
    let mut full_buf = vec![0u8; payload.len()];
    let full_read = oifs_read_file(
        handle,
        filename.as_ptr(),
        full_buf.as_mut_ptr(),
        full_buf.len() as u64,
    );
    assert_eq!(full_read, payload.len() as i64);
    assert_eq!(&full_buf[..], &payload[..]);

    // 3. Error case: nonexistent file
    let bad_filename = CString::new("nonexistent.txt").unwrap();
    let err_read = oifs_read_at(
        handle,
        bad_filename.as_ptr(),
        0,
        buf.as_mut_ptr(),
        buf.len() as u64,
    );
    assert_eq!(err_read, -1);

    oifs_close(handle);
}
