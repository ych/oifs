use oifs::disk::{CompressionMode, DiskManager};
use oifs::ffi::*;
use std::ffi::CString;
use tempfile::tempdir;

#[test]
fn test_contiguous_block_allocation() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("contiguous.img");
    let path = img_path.to_str().unwrap();

    // 10 MB image = ~2560 blocks
    let dm = DiskManager::open(path, 10 * 1024 * 1024).unwrap();

    // Allocate 16 contiguous blocks
    let blk1 = dm.allocate_contiguous_blocks(16).unwrap();
    assert!(blk1 >= 3); // Must be in data block area

    // Allocate another 32 contiguous blocks
    let blk2 = dm.allocate_contiguous_blocks(32).unwrap();
    assert_eq!(blk2, blk1 + 16, "Sequential allocation should be adjacent");

    // Free the first 16 blocks
    dm.free_contiguous_blocks(blk1, 16).unwrap();

    // Allocate 8 contiguous blocks: should reuse the hole at blk1
    let blk3 = dm.allocate_contiguous_blocks(8).unwrap();
    assert_eq!(blk3, blk1, "Should reuse freed contiguous space");
}

#[test]
fn test_truncate_uncompressed_raw_file_shrink_and_expand() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("raw.img");
    let path = img_path.to_str().unwrap();

    let dm = DiskManager::open(path, 10 * 1024 * 1024).unwrap();
    let root = dm.superblock().root_inode;

    // 1. Create a 20 KB uncompressed raw file (5 blocks: 4096 * 5 = 20480 bytes)
    let file_id = dm.create_file(root, "raw_data.bin").unwrap();
    let mut initial_data = Vec::with_capacity(20480);
    for i in 0..20480 {
        initial_data.push((i % 251) as u8);
    }
    dm.write_data(file_id, 0, &initial_data, CompressionMode::Never)
        .unwrap();

    let inode_before = dm.read_inode(file_id).unwrap();
    assert_eq!(inode_before.size, 20480);
    assert_eq!(inode_before.compressed_size, 0);
    assert_ne!(inode_before.blocks[0], 0);
    assert_ne!(inode_before.blocks[4], 0);

    // 2. Truncate down to 6000 bytes (fits in 2 blocks: 4096 + 1904)
    dm.truncate(file_id, 6000).unwrap();

    let inode_after = dm.read_inode(file_id).unwrap();
    assert_eq!(inode_after.size, 6000);
    assert_eq!(inode_after.compressed_size, 0);
    assert_ne!(inode_after.blocks[0], 0);
    assert_ne!(inode_after.blocks[1], 0);
    // Blocks 2, 3, 4 should now be cleared!
    assert_eq!(inode_after.blocks[2], 0);
    assert_eq!(inode_after.blocks[3], 0);
    assert_eq!(inode_after.blocks[4], 0);

    // Verify content matches the first 6000 bytes
    let read_data = dm.read_data(file_id).unwrap();
    assert_eq!(read_data.len(), 6000);
    assert_eq!(read_data, &initial_data[..6000]);

    // Verify slack bytes in block 1 (from 1904 to 4096) are zeroed out
    let blk1 = inode_after.blocks[1];
    let blk1_data = dm.get_block_copy(blk1).unwrap();
    for byte in &blk1_data[1904..4096] {
        assert_eq!(*byte, 0, "Slack bytes in truncated block must be zeroed");
    }

    // 3. Expand to 10000 bytes (sparse extension)
    dm.truncate(file_id, 10000).unwrap();
    let inode_expanded = dm.read_inode(file_id).unwrap();
    assert_eq!(inode_expanded.size, 10000);

    let read_expanded = dm.read_data(file_id).unwrap();
    assert_eq!(read_expanded.len(), 10000);
    assert_eq!(&read_expanded[..6000], &initial_data[..6000]);
    // The newly extended bytes (6000..10000) must read as 0
    for byte in &read_expanded[6000..10000] {
        assert_eq!(*byte, 0, "Sparse extended bytes must read as 0");
    }

    // 4. Truncate to 0 bytes
    dm.truncate(file_id, 0).unwrap();
    let inode_zero = dm.read_inode(file_id).unwrap();
    assert_eq!(inode_zero.size, 0);
    assert_eq!(inode_zero.blocks, [0; 12]);
    let read_zero = dm.read_data(file_id).unwrap();
    assert!(read_zero.is_empty());
}

#[test]
fn test_write_data_truncated_fixes_corr01() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("corr01.img");
    let path = img_path.to_str().unwrap();

    let dm = DiskManager::open(path, 10 * 1024 * 1024).unwrap();
    let root = dm.superblock().root_inode;

    let file_id = dm.create_file(root, "document.txt").unwrap();
    let big_data = b"Hello World, this is a very long string that spans many bytes in the file!";
    dm.write_data(file_id, 0, big_data, CompressionMode::Never)
        .unwrap();
    assert_eq!(dm.read_inode(file_id).unwrap().size, big_data.len() as u64);

    // Overwriting with shorter content using write_data_truncated
    let small_data = b"Short text";
    dm.write_data_truncated(file_id, small_data, CompressionMode::Never)
        .unwrap();

    // File MUST be truncated to exactly small_data.len() (CORR-01 fixed!)
    let inode = dm.read_inode(file_id).unwrap();
    assert_eq!(
        inode.size,
        small_data.len() as u64,
        "write_data_truncated must shrink file size"
    );

    let read_back = dm.read_data(file_id).unwrap();
    assert_eq!(read_back, small_data);
}

#[test]
fn test_truncate_compressed_file() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("comp.img");
    let path = img_path.to_str().unwrap();

    let dm = DiskManager::open(path, 10 * 1024 * 1024).unwrap();
    let root = dm.superblock().root_inode;
    let file_id = dm.create_file(root, "compressed.bin").unwrap();

    // 50 KB repetitive data (highly compressible)
    let data = vec![b'A'; 50 * 1024];
    dm.write_data(file_id, 0, &data, CompressionMode::Always)
        .unwrap();

    let inode = dm.read_inode(file_id).unwrap();
    assert_eq!(inode.size, 50 * 1024);
    assert!(inode.compressed_size > 0);
    assert!(inode.compressed_size < inode.size);

    // Truncate to 10 KB
    dm.truncate(file_id, 10 * 1024).unwrap();

    let inode_after = dm.read_inode(file_id).unwrap();
    assert_eq!(inode_after.size, 10 * 1024);
    assert!(inode_after.compressed_size > 0);

    let read_data = dm.read_data(file_id).unwrap();
    assert_eq!(read_data.len(), 10 * 1024);
    assert_eq!(read_data, vec![b'A'; 10 * 1024]);
}

#[test]
fn test_truncate_journaled_filesystem() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("journaled.img");
    let path = img_path.to_str().unwrap();

    // Open in journaled mode
    let dm = DiskManager::open_journaled(path, 10 * 1024 * 1024).unwrap();
    assert!(dm.superblock().has_journal_layout());

    let root = dm.superblock().root_inode;
    let file_id = dm.create_file(root, "journaled.dat").unwrap();
    let data = vec![0x42u8; 16384]; // 4 blocks
    dm.write_data(file_id, 0, &data, CompressionMode::Never)
        .unwrap();

    // Truncate down to 4096 bytes (1 block)
    dm.truncate(file_id, 4096).unwrap();

    // Close and reopen to verify journal replay & consistency
    drop(dm);

    let dm_reopened = DiskManager::open_journaled(path, 10 * 1024 * 1024).unwrap();
    let inode = dm_reopened.read_inode(file_id).unwrap();
    assert_eq!(inode.size, 4096);
    assert_ne!(inode.blocks[0], 0);
    assert_eq!(inode.blocks[1], 0);
    assert_eq!(inode.blocks[2], 0);
    assert_eq!(inode.blocks[3], 0);

    let content = dm_reopened.read_data(file_id).unwrap();
    assert_eq!(content, vec![0x42u8; 4096]);

    // Check integrity via fsck
    let report = dm_reopened.verify_integrity().unwrap();
    assert!(
        report.is_clean,
        "Journaled filesystem must be 100% clean after truncate"
    );
}

#[test]
fn test_ffi_truncate_file() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("ffi_truncate.img");
    let path = img_path.to_str().unwrap();

    let c_path = CString::new(path).unwrap();
    let handle = oifs_open(c_path.as_ptr(), 10 * 1024 * 1024);
    assert!(!handle.is_null());

    let filename = CString::new("test_ffi.bin").unwrap();
    let data = b"0123456789abcdef";
    assert_eq!(
        oifs_write_file(handle, filename.as_ptr(), data.as_ptr(), data.len() as u64),
        0
    );

    // Truncate to 10 bytes
    assert_eq!(oifs_truncate_file(handle, filename.as_ptr(), 10), 0);

    let mut buf = vec![0u8; 32];
    let n = oifs_read_file(
        handle,
        filename.as_ptr(),
        buf.as_mut_ptr(),
        buf.len() as u64,
    );
    assert_eq!(n, 10);
    assert_eq!(&buf[..10], b"0123456789");

    oifs_close(handle);
}

#[test]
fn test_cli_truncate_command() {
    use std::process::Command;

    let dir = tempdir().unwrap();
    let img_path = dir.path().join("cli_trunc.img");
    let img_str = img_path.to_str().unwrap();

    // 1. Create image with CLI
    let status = Command::new(env!("CARGO_BIN_EXE_oifs"))
        .args(["-i", img_str, "create", "--size", "10"])
        .status()
        .unwrap();
    assert!(status.success());

    // 2. Put / write file with CLI
    let temp_src = dir.path().join("source.txt");
    std::fs::write(&temp_src, "0123456789abcdefghijklmnopqrstuvwxyz").unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_oifs"))
        .args([
            "-i",
            img_str,
            "put",
            temp_src.to_str().unwrap(),
            "--no-compress",
        ])
        .status()
        .unwrap();
    assert!(status.success());

    // 3. Truncate with CLI
    let status = Command::new(env!("CARGO_BIN_EXE_oifs"))
        .args(["-i", img_str, "truncate", "source.txt", "--size", "5"])
        .status()
        .unwrap();
    assert!(status.success());

    // 4. Verify size and content
    let dm = DiskManager::open(img_str, 0).unwrap();
    let id = dm.resolve_path("source.txt").unwrap();
    let data = dm.read_data(id).unwrap();
    assert_eq!(data, b"01234");
}
