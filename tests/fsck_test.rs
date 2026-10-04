use oifs::disk::{CompressionMode, DiskManager};
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

#[test]
fn test_fsck_diagnostic_flow() {
    let path_str = "test_fsck.img";
    let path = Path::new(path_str);
    let total_size = 10 * 1024 * 1024; // 10MB

    if path.exists() {
        fs::remove_file(path).unwrap();
    }

    // 1. Create a healthy filesystem and write some files
    {
        let dm = DiskManager::open(path, total_size).unwrap();
        let root = dm.superblock().root_inode;

        let file_id = dm.create_file(root, "clean.txt").unwrap();
        dm.write_data(
            file_id,
            0,
            b"Healthy data block content",
            CompressionMode::Never,
        )
        .unwrap();

        // Check integrity of clean filesystem
        let report = dm.verify_integrity().unwrap();
        assert!(report.is_clean, "Healthy filesystem should be clean");
        assert!(report.orphan_inodes.is_empty());
        assert!(report.leaked_blocks.is_empty());
        assert!(report.missing_blocks.is_empty());
        assert!(report.cross_linked_blocks.is_empty());
    }

    // 2. Corrupt the data block bitmap
    // The data bitmap block is block 2. Offset = 2 * 4096 = 8192 bytes.
    // The first allocated data block has index 0 in the data bitmap.
    // So the first byte of the data bitmap is 0x01. We overwrite it to 0x00.
    {
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(8192)).unwrap();
        file.write_all(&[0x00]).unwrap(); // Mark block 131 as free
    }

    // 3. Re-open and verify fsck detects the corruption
    {
        let dm = DiskManager::open(path, total_size).unwrap();
        let report = dm.verify_integrity().unwrap();

        println!("FSCK Corrupted Report: {:?}", report);
        assert!(
            !report.is_clean,
            "Corrupted filesystem should NOT report clean"
        );

        // Block 131 should be reported as missing
        assert!(
            !report.missing_blocks.is_empty(),
            "FSCK should detect missing blocks"
        );
        assert_eq!(report.missing_blocks[0], dm.superblock().data_block_start);
    }

    fs::remove_file(path).unwrap();
}
