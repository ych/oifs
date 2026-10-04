use oifs::disk::DiskManager;
use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn test_flush_persistence() {
    let image_path = "crash_test.img";
    if Path::new(image_path).exists() {
        fs::remove_file(image_path).unwrap();
    }

    // 1. Create Image
    Command::new(env!("CARGO_BIN_EXE_oifs"))
        .args(["--image", image_path, "create", "--size", "10"])
        .status()
        .expect("Cmd failed");

    // 2. Open and Write Data
    {
        let dm = DiskManager::open(image_path, 0).expect("Open failed");
        let root = dm.resolve_path(".").unwrap();
        let file_id = dm
            .create_file(root, "important.txt")
            .expect("Create failed");
        dm.write_data(
            file_id,
            0,
            b"Critical Data",
            oifs::disk::CompressionMode::Auto,
        )
        .expect("Write failed");

        // 3. Explicit Flush
        dm.flush().expect("Flush failed");

        // Drop dm here (should also flush, but we test explicit first)
    }

    // 4. Reopen and Verify
    {
        let dm = DiskManager::open(image_path, 0).expect("Reopen failed");
        let root = dm.resolve_path(".").unwrap();
        let file_id = dm.lookup(root, "important.txt").expect("File lost");
        let data = dm.read_data(file_id).expect("Read failed");
        assert_eq!(data, b"Critical Data", "Data mismatch after flush/reopen");
    }

    fs::remove_file(image_path).unwrap();
}

#[test]
fn test_drop_flush() {
    let image_path = "drop_test.img";
    if Path::new(image_path).exists() {
        fs::remove_file(image_path).unwrap();
    }

    // 1. Create Image
    Command::new(env!("CARGO_BIN_EXE_oifs"))
        .args(["--image", image_path, "create", "--size", "10"])
        .status()
        .expect("Cmd failed");

    // 2. Write Data and Drop WITHOUT explicit flush
    {
        let dm = DiskManager::open(image_path, 0).expect("Open failed");
        let root = dm.resolve_path(".").unwrap();
        let file_id = dm.create_file(root, "drop.txt").expect("Create failed");
        dm.write_data(file_id, 0, b"Drop Data", oifs::disk::CompressionMode::Auto)
            .expect("Write failed");
        // dm is dropped here. Drop impl should call flush.
    }

    // 3. Verify Persistence
    {
        let dm = DiskManager::open(image_path, 0).expect("Reopen failed");
        let root = dm.resolve_path(".").unwrap();
        let file_id = dm.lookup(root, "drop.txt").expect("File lost");
        let data = dm.read_data(file_id).expect("Read failed");
        assert_eq!(data, b"Drop Data", "Data mismatch after drop");
    }

    fs::remove_file(image_path).unwrap();
}

#[test]
fn test_block_overflow_safety_does_not_return_superblock() {
    let image_path = "overflow_test.img";
    if Path::new(image_path).exists() {
        fs::remove_file(image_path).unwrap();
    }

    Command::new(env!("CARGO_BIN_EXE_oifs"))
        .args(["--image", image_path, "create", "--size", "10"])
        .status()
        .expect("Cmd failed");

    let dm = DiskManager::open(image_path, 0).expect("Open failed");

    // Test that an overflowing block_id safely returns None and does not return Block 0
    let huge_id = (usize::MAX as u64 / 4096) + 1;
    assert_eq!(dm.get_block_copy(huge_id), None);
    assert_eq!(dm.get_block_copy(u64::MAX), None);
    assert_eq!(dm.get_block_copy(u64::MAX - 1), None);

    // Verify Block 0 (SuperBlock) magic is still intact
    let sb_block = dm.get_block_copy(0).expect("SuperBlock must exist");
    let magic = u32::from_le_bytes(sb_block[0..4].try_into().unwrap());
    assert_eq!(magic, 0x4F494653, "SuperBlock magic must be intact");

    drop(dm);
    fs::remove_file(image_path).unwrap();
}
