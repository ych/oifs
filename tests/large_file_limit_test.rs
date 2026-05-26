use oifs::disk::{DiskManager, CompressionMode};
use std::fs;
use std::path::Path;
use rand::RngCore;

#[test]
fn test_large_file_success_and_boundary() {
    let path = Path::new("test_large_file_success.img");
    let total_size = 15 * 1024 * 1024; // 15MB filesystem image

    if path.exists() {
        fs::remove_file(path).unwrap();
    }

    // 1. Create a 15MB filesystem
    let dm = DiskManager::open(path, total_size).expect("Failed to create DiskManager");
    let root = dm.superblock().root_inode;

    // 2. Create a file inode
    let file_inode = dm.create_file(root, "large_file.bin").expect("Failed to create file");

    // 3. Generate 1MB of uncompressible (random) data
    let mut plaintext = vec![0u8; 1024 * 1024]; // 1MB
    rand::thread_rng().fill_bytes(&mut plaintext);

    // 4. Write the 1MB uncompressible data (requires single indirect block)
    // This MUST succeed now that we have indirect blocks!
    let write_res = dm.write_data(file_inode, 0, &plaintext, CompressionMode::Never);
    assert!(write_res.is_ok(), "Writing 1MB of data should succeed: {:?}", write_res);

    // 5. Read back and verify integrity
    let read_res = dm.read_data(file_inode);
    assert!(read_res.is_ok(), "Reading 1MB of data should succeed: {:?}", read_res);
    let decrypted = read_res.unwrap();
    assert_eq!(decrypted, plaintext, "Read data matches written data exactly");

    // 6. Test FileTooLarge boundary by trying to write at > 1GB offset
    // Max blocks: 10 direct + 512 single indirect + 262144 double indirect = 262666 blocks
    // 262666 * 4096 = 1,075,879,936 bytes
    let too_large_offset = 262666 * 4096;
    let limit_res = dm.write_data(file_inode, too_large_offset, &[0u8], CompressionMode::Never);
    assert!(limit_res.is_err(), "Writing past 1GB boundary should fail");
    
    let err = limit_res.unwrap_err();
    println!("Caught expected limit error: {:?}", err);
    
    match err {
        oifs::disk::DiskManagerError::Io(io_err) => {
            assert_eq!(io_err.kind(), std::io::ErrorKind::FileTooLarge);
            assert!(io_err.to_string().contains("File too large"));
        }
        other => panic!("Unexpected error type: {:?}", other),
    }

    // Clean up
    fs::remove_file(path).unwrap();
}
