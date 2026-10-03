use oifs::disk::{CompressionMode, DiskManager, DurabilityMode};
use std::fs;
use std::path::Path;

struct TestDisk {
    path: String,
}

impl TestDisk {
    fn new(name: &str) -> Self {
        let path = format!("target/{}_{}.img", name, std::process::id());
        if Path::new(&path).exists() {
            let _ = fs::remove_file(&path);
        }
        Self { path }
    }
}

impl Drop for TestDisk {
    fn drop(&mut self) {
        if Path::new(&self.path).exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[test]
fn test_durability_mode_transitions() {
    let td = TestDisk::new("test_durability_mode_transitions");
    let dm = DiskManager::open(&td.path, 10 * 1024 * 1024).unwrap();

    // Default mode must be Lazy (ProcessSafe)
    assert_eq!(dm.durability_mode(), DurabilityMode::Lazy);
    assert!(!dm.durability_mode().is_range_based());

    // Switch to RangeAsync
    dm.set_durability_mode(DurabilityMode::RangeAsync);
    assert_eq!(dm.durability_mode(), DurabilityMode::RangeAsync);
    assert!(dm.durability_mode().is_range_based());

    // Switch to Strict
    dm.set_durability_mode(DurabilityMode::Strict);
    assert_eq!(dm.durability_mode(), DurabilityMode::Strict);
    assert!(dm.durability_mode().is_range_based());

    // Switch to LegacyWholeMmapAsync
    dm.set_durability_mode(DurabilityMode::LegacyWholeMmapAsync);
    assert_eq!(dm.durability_mode(), DurabilityMode::LegacyWholeMmapAsync);
    assert!(!dm.durability_mode().is_range_based());

    // Switch back to Lazy
    dm.set_durability_mode(DurabilityMode::Lazy);
    assert_eq!(dm.durability_mode(), DurabilityMode::Lazy);
}

#[test]
fn test_durability_mode_builder() {
    let td = TestDisk::new("test_durability_mode_builder");
    let dm = DiskManager::open(&td.path, 10 * 1024 * 1024)
        .unwrap()
        .with_durability_mode(DurabilityMode::Strict);

    assert_eq!(dm.durability_mode(), DurabilityMode::Strict);
}

#[test]
fn test_durability_mode_lazy_lifecycle() {
    let td = TestDisk::new("test_durability_mode_lazy_lifecycle");
    let root_inode;
    {
        let dm = DiskManager::open(&td.path, 10 * 1024 * 1024)
            .unwrap()
            .with_durability_mode(DurabilityMode::Lazy);

        root_inode = dm.superblock().root_inode;
        let sub_dir = dm.create_directory(root_inode, "sub").unwrap();
        let f1 = dm.create_file(sub_dir, "file1.txt").unwrap();
        dm.write_data(f1, 0, b"Hello from Lazy mode!", CompressionMode::Never)
            .unwrap();

        let f2 = dm.create_file(sub_dir, "file2.bin").unwrap();
        let payload = vec![0xAB; 16384]; // 4 blocks
        dm.write_data(f2, 0, &payload, CompressionMode::Auto)
            .unwrap();

        // Delete f1 to verify deletion under Lazy mode
        dm.delete_file(sub_dir, "file1.txt").unwrap();

        // Explicit flush
        dm.flush().unwrap();
    }

    // Reopen and verify persistence
    {
        let dm = DiskManager::open(&td.path, 0).unwrap();
        let sub_dir = dm.lookup(root_inode, "sub").unwrap();
        assert!(dm.lookup(sub_dir, "file1.txt").is_err());

        let f2 = dm.lookup(sub_dir, "file2.bin").unwrap();
        let read_data = dm.read_data(f2).unwrap();
        assert_eq!(read_data.len(), 16384);
        assert!(read_data.iter().all(|&b| b == 0xAB));
    }
}

#[test]
fn test_durability_mode_range_async_lifecycle() {
    let td = TestDisk::new("test_durability_mode_range_async_lifecycle");
    let root_inode;
    {
        let dm = DiskManager::open(&td.path, 10 * 1024 * 1024)
            .unwrap()
            .with_durability_mode(DurabilityMode::RangeAsync);

        root_inode = dm.superblock().root_inode;
        let d = dm.create_directory(root_inode, "range_dir").unwrap();
        let f = dm.create_file(d, "multi_write.txt").unwrap();

        dm.write_data(f, 0, b"First block", CompressionMode::Never)
            .unwrap();
        dm.write_data(f, 4096, b"Second block", CompressionMode::Never)
            .unwrap();
    }

    // Reopen and verify
    {
        let dm = DiskManager::open(&td.path, 0).unwrap();
        let d = dm.lookup(root_inode, "range_dir").unwrap();
        let f = dm.lookup(d, "multi_write.txt").unwrap();
        let data = dm.read_data(f).unwrap();
        assert!(data.starts_with(b"First block"));
        assert_eq!(&data[4096..4096 + 12], b"Second block");
    }
}

#[test]
fn test_durability_mode_strict_lifecycle() {
    let td = TestDisk::new("test_durability_mode_strict_lifecycle");
    let root_inode;
    {
        let dm = DiskManager::open(&td.path, 10 * 1024 * 1024)
            .unwrap()
            .with_durability_mode(DurabilityMode::Strict);

        root_inode = dm.superblock().root_inode;
        let d = dm.create_directory(root_inode, "strict_dir").unwrap();
        let f = dm.create_file(d, "strict.bin").unwrap();

        let big_data = vec![0x42u8; 32768];
        dm.write_data(f, 0, &big_data, CompressionMode::Auto)
            .unwrap();
    }

    // Reopen and verify
    {
        let dm = DiskManager::open(&td.path, 0).unwrap();
        let d = dm.lookup(root_inode, "strict_dir").unwrap();
        let f = dm.lookup(d, "strict.bin").unwrap();
        let data = dm.read_data(f).unwrap();
        assert_eq!(data.len(), 32768);
        assert!(data.iter().all(|&b| b == 0x42));
    }
}

#[test]
fn test_durability_mode_legacy_mmap_lifecycle() {
    let td = TestDisk::new("test_durability_mode_legacy_mmap_lifecycle");
    let root_inode;
    {
        let dm = DiskManager::open(&td.path, 10 * 1024 * 1024)
            .unwrap()
            .with_durability_mode(DurabilityMode::LegacyWholeMmapAsync);

        root_inode = dm.superblock().root_inode;
        let d = dm.create_directory(root_inode, "legacy_dir").unwrap();
        let f = dm.create_file(d, "legacy.txt").unwrap();
        dm.write_data(f, 0, b"Legacy whole mmap sync", CompressionMode::Never)
            .unwrap();
    }

    // Reopen and verify
    {
        let dm = DiskManager::open(&td.path, 0).unwrap();
        let d = dm.lookup(root_inode, "legacy_dir").unwrap();
        let f = dm.lookup(d, "legacy.txt").unwrap();
        let data = dm.read_data(f).unwrap();
        assert_eq!(data, b"Legacy whole mmap sync");
    }
}
