use std::path::Path;
use std::fs;
use std::process::Command;
use std::sync::Arc;
use oifs::disk::DiskManager;

#[test]
fn test_flush_persistence() {
    let image_path = "crash_test.img";
    if Path::new(image_path).exists() { fs::remove_file(image_path).unwrap(); }

    // 1. Create Image
    Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "create", "--size", "10"])
        .status().expect("Cmd failed");

    // 2. Open and Write Data
    {
        let dm = DiskManager::open(image_path, 0).expect("Open failed");
        let root = dm.resolve_path(".").unwrap();
        let file_id = dm.create_file(root, "important.txt").expect("Create failed");
        dm.write_data(file_id, 0, b"Critical Data", oifs::disk::CompressionMode::Auto).expect("Write failed");
        
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
    if Path::new(image_path).exists() { fs::remove_file(image_path).unwrap(); }

    // 1. Create Image
    Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "create", "--size", "10"])
        .status().expect("Cmd failed");

    // 2. Write Data and Drop WITHOUT explicit flush
    {
        let dm = DiskManager::open(image_path, 0).expect("Open failed");
        let root = dm.resolve_path(".").unwrap();
        let file_id = dm.create_file(root, "drop.txt").expect("Create failed");
        dm.write_data(file_id, 0, b"Drop Data", oifs::disk::CompressionMode::Auto).expect("Write failed");
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
