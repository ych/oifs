use oifs::disk::{DiskManager, CompressionMode};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_create_encrypted_filesystem() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");
    
    // Create encrypted filesystem
    let password = "test_password_123";
    let size = 10 * 1024 * 1024; // 10MB
    
    let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
    
    // Verify superblock has encryption enabled
    let sb = dm.superblock();
    assert!(sb.encrypted, "Superblock should have encrypted flag set");
    assert_ne!(sb.encryption_salt, [0u8; 16], "Salt should be non-zero");
    assert_eq!(sb.encryption_version, 1, "Encryption version should be 1");
}

#[test]
fn test_encrypted_write_read_roundtrip() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");
    
    let password = "secure_password";
    let size = 10 * 1024 * 1024;
    
    // Create encrypted filesystem
    let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
    
    // Create a file
    let root_inode = dm.superblock().root_inode;
    let file_inode = dm.create_file(root_inode, "secret.txt").unwrap();
    
    // Write encrypted data
    let plaintext = b"This is secret data that should be encrypted!";
    dm.write_data(file_inode, 0, plaintext, CompressionMode::Never).unwrap();
    
    // Read back and verify
    let decrypted = dm.read_data(file_inode).unwrap();
    assert_eq!(decrypted, plaintext, "Decrypted data should match original");
    
    // Verify the inode is marked as encrypted
    let inode = dm.read_inode(file_inode).unwrap();
    assert!(inode.encrypted, "Inode should be marked as encrypted");
    assert_ne!(inode.encryption_nonce, [0u8; 24], "Nonce should be non-zero");
}

#[test]
fn test_wrong_password_fails() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");
    
    let correct_password = "correct_password";
    let wrong_password = "wrong_password";
    let size = 10 * 1024 * 1024;
    
    // Create encrypted filesystem with correct password
    {
        let dm = DiskManager::create_encrypted(&image_path, size, correct_password).unwrap();
        let root_inode = dm.superblock().root_inode;
        let file_inode = dm.create_file(root_inode, "file.txt").unwrap();
        dm.write_data(file_inode, 0, b"secret", CompressionMode::Never).unwrap();
    }
    
    // Try to open with wrong password - should succeed in opening
    let dm = DiskManager::open_with_password(&image_path, 0, Some(wrong_password)).unwrap();
    
    // But reading should fail with decryption error
    let root_inode = dm.superblock().root_inode;
    let file_inode = dm.lookup(root_inode, "file.txt").unwrap();
    let read_result = dm.read_data(file_inode);
    assert!(read_result.is_err(), "Reading with wrong password should fail");
    
    // Verify it's a decryption error
    match read_result {
        Err(oifs::disk::DiskManagerError::DecryptionFailed) => {
            // Expected error
        }
        _ => panic!("Expected DecryptionFailed error, got: {:?}", read_result),
    }
}

#[test]
fn test_encryption_with_compression() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");
    
    let password = "compress_and_encrypt";
    let size = 10 * 1024 * 1024;
    
    let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
    let root_inode = dm.superblock().root_inode;
    let file_inode = dm.create_file(root_inode, "compressible.txt").unwrap();
    
    // Create highly compressible data (10KB of repeated pattern)
    let plaintext = vec![b'A'; 10 * 1024];
    
    // Write with compression enabled
    dm.write_data(file_inode, 0, &plaintext, CompressionMode::Always).unwrap();
    
    // Verify inode shows both compression and encryption
    let inode = dm.read_inode(file_inode).unwrap();
    assert!(inode.encrypted, "File should be encrypted");
    assert!(inode.compressed_size > 0, "File should be compressed");
    assert_eq!(inode.size, plaintext.len() as u64, "Logical size should match original");
    
    // Read back and verify
    let decrypted = dm.read_data(file_inode).unwrap();
    assert_eq!(decrypted, plaintext, "Decrypted+decompressed data should match original");
}

#[test]
fn test_multiple_files_unique_nonces() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");
    
    let password = "multi_file_test";
    let size = 10 * 1024 * 1024;
    
    let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
    let root_inode = dm.superblock().root_inode;
    
    // Create multiple files
    let mut nonces = Vec::new();
    for i in 0..5 {
        let filename = format!("file{}.txt", i);
        let file_inode = dm.create_file(root_inode, &filename).unwrap();
        dm.write_data(file_inode, 0, b"data", CompressionMode::Never).unwrap();
        
        let inode = dm.read_inode(file_inode).unwrap();
        nonces.push(inode.encryption_nonce);
    }
    
    // Verify all nonces are unique
    for i in 0..nonces.len() {
        for j in (i + 1)..nonces.len() {
            assert_ne!(nonces[i], nonces[j], "Nonces should be unique for different files");
        }
    }
}

#[test]
fn test_cannot_open_encrypted_without_password() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");
    
    let password = "required_password";
    let size = 10 * 1024 * 1024;
    
    // Create encrypted filesystem
    {
        let _dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
    }
    
    // Try to open without password using regular open()
    let result = DiskManager::open(&image_path, 0);
    assert!(result.is_err(), "Opening encrypted filesystem without password should fail");
    
    // Verify it's specifically a PasswordRequired error
    match result {
        Err(oifs::disk::DiskManagerError::PasswordRequired) => {
            // Expected error
        }
        _ => panic!("Expected PasswordRequired error"),
    }
}

#[test]
fn test_data_is_actually_encrypted_on_disk() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");
    
    let password = "encryption_test";
    let size = 10 * 1024 * 1024;
    let plaintext = b"VERY_DISTINCTIVE_SECRET_TEXT_THAT_SHOULD_NOT_APPEAR_IN_RAW_DISK";
    
    // Create encrypted filesystem and write data
    {
        let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
        let root_inode = dm.superblock().root_inode;
        let file_inode = dm.create_file(root_inode, "secret.txt").unwrap();
        dm.write_data(file_inode, 0, plaintext, CompressionMode::Never).unwrap();
    }
    
    // Read raw disk image
    let raw_disk = fs::read(&image_path).unwrap();
    
    // Convert plaintext to string for searching
    let plaintext_str = String::from_utf8_lossy(plaintext);
    let raw_disk_str = String::from_utf8_lossy(&raw_disk);
    
    // Verify plaintext does NOT appear in raw disk
    assert!(
        !raw_disk_str.contains(plaintext_str.as_ref()),
        "Plaintext should not be visible in raw disk image - data is not encrypted!"
    );
    
    // Verify we can still read it correctly with password
    // Need to reopen the file since we closed it above
    let dm = DiskManager::open_with_password(&image_path, size, Some(password)).unwrap();
    let root_inode = dm.superblock().root_inode;
    let file_inode = dm.lookup(root_inode, "secret.txt").unwrap();
    let decrypted = dm.read_data(file_inode).unwrap();
    assert_eq!(decrypted, plaintext, "Should be able to decrypt with correct password");
}

#[test]
fn test_unencrypted_filesystem_still_works() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("unencrypted.img");
    
    let size = 10 * 1024 * 1024;
    
    // Create normal (unencrypted) filesystem
    let dm = DiskManager::open(&image_path, size).unwrap();
    
    // Verify superblock shows not encrypted
    let sb = dm.superblock();
    assert!(!sb.encrypted, "Superblock should not have encrypted flag");
    
    // Write and read data normally
    let root_inode = sb.root_inode;
    let file_inode = dm.create_file(root_inode, "normal.txt").unwrap();
    let data = b"Normal unencrypted data";
    dm.write_data(file_inode, 0, data, CompressionMode::Never).unwrap();
    
    let read_data = dm.read_data(file_inode).unwrap();
    assert_eq!(read_data, data);
    
    // Verify inode is not marked as encrypted
    let inode = dm.read_inode(file_inode).unwrap();
    assert!(!inode.encrypted, "Inode should not be encrypted");
}
