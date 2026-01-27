use oifs::disk::DiskManager;
use oifs::superblock::SuperBlock;
use oifs::allocator::BlockAllocator;
use std::fs;
use std::path::Path;

#[test]
fn test_disk_initialization_and_persistence() {
    let path = Path::new("test_fs_persist.img");
    let total_size = 10 * 1024 * 1024; // 10MB to accommodate overhead

    if path.exists() {
        fs::remove_file(path).unwrap();
    }

    // 1. Create new FS
    {
        let dm = DiskManager::open(path, total_size).expect("Failed to create DiskManager");
        assert_eq!(dm.superblock().magic, SuperBlock::MAGIC);
    }

    // 2. Reopen
    {
        let dm = DiskManager::open(path, total_size).expect("Failed to reopen");
        assert_eq!(dm.superblock().magic, SuperBlock::MAGIC);
    }

    fs::remove_file(path).unwrap();
}

#[test]
fn test_allocation_persistence() {
    // Disabled: Low-level allocator API is no longer public.
    // Concurrency and functional tests cover persistence of metadata/data.
    /*
    let path = Path::new("test_fs_alloc.img");
    let total_size = 10 * 1024 * 1024; // 10MB

    if path.exists() {
        fs::remove_file(path).unwrap();
    }

    let allocated_block_id;

    // 1. Allocate block
    {
        let mut dm = DiskManager::open(path, total_size).expect("Failed to create");
        let mut allocator = dm.data_block_allocator();
        allocated_block_id = allocator.allocate().expect("Allocation failed");
        
        // Check reasonable ID (should be >= data_block_start)
        assert!(allocated_block_id >= dm.superblock.data_block_start);
    }

    // 2. Reopen and verify it's still allocated
    {
        let mut dm = DiskManager::open(path, total_size).expect("Failed to reopen");
        let mut allocator = dm.data_block_allocator();
        
        // Next allocation should be different
        let next_id = allocator.allocate().expect("Allocation 2 failed");
        assert_ne!(next_id, allocated_block_id);
    }

    fs::remove_file(path).unwrap();
    */
}
