use oifs::bitmap::Bitmap;
use oifs::disk::{CompressionMode, DiskManager};
use oifs::inode::{FileType, Inode};
use std::fs::{self, OpenOptions};
use std::path::Path;

struct TestContext {
    image_path: String,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let image_path = format!("{}.img", name);
        if Path::new(&image_path).exists() {
            let _ = fs::remove_file(&image_path);
        }
        Self { image_path }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        if Path::new(&self.image_path).exists() {
            let _ = fs::remove_file(&self.image_path);
        }
    }
}

#[test]
fn test_fsck_clean_filesystem() {
    let ctx = TestContext::new("test_fsck_clean");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    let id1 = dm.create_file(root_id, "file1.txt").expect("create file 1");
    dm.write_data(id1, 0, b"Hello file 1", CompressionMode::Never).expect("write file 1");

    let dir1 = dm.create_directory(root_id, "subdir").expect("create subdir");
    let id2 = dm.create_file(dir1, "file2.txt").expect("create file 2");
    dm.write_data(id2, 0, b"Hello file 2 inside subdir", CompressionMode::Never).expect("write file 2");

    let report = dm.verify_integrity().expect("fsck");
    assert!(report.is_clean);
    assert!(report.orphan_inodes.is_empty());
    assert!(report.leaked_blocks.is_empty());
    assert!(report.missing_blocks.is_empty());
    assert!(report.cross_linked_blocks.is_empty());
}

#[test]
fn test_fsck_detect_orphan_inode() {
    let ctx = TestContext::new("test_fsck_orphan");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let sb = dm.superblock();
    drop(dm);

    // Modify image directly: set bit 5 in inode bitmap (inode 5)
    let file = OpenOptions::new().read(true).write(true).open(&ctx.image_path).expect("open file");
    let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file).expect("mmap") };
    let ib_start = (sb.inode_bitmap_block * 4096) as usize;
    let mut bitmap = Bitmap::new(&mut mmap[ib_start..ib_start + 4096]);
    bitmap.set(5); // Inode 5 marked as allocated, but not in any directory
    mmap.flush().expect("flush");
    drop(mmap);
    drop(file);

    let dm = DiskManager::open(&ctx.image_path, 0).expect("open dm");
    let report = dm.verify_integrity().expect("fsck");
    assert!(!report.is_clean);
    assert_eq!(report.orphan_inodes, vec![5]);
}

#[test]
fn test_fsck_detect_leaked_data_block() {
    let ctx = TestContext::new("test_fsck_leaked");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let sb = dm.superblock();
    drop(dm);

    // Modify image directly: set bit 20 in data bitmap (block: data_block_start + 20)
    let file = OpenOptions::new().read(true).write(true).open(&ctx.image_path).expect("open file");
    let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file).expect("mmap") };
    let db_start = (sb.data_bitmap_block * 4096) as usize;
    let mut bitmap = Bitmap::new(&mut mmap[db_start..db_start + 4096]);
    bitmap.set(20);
    mmap.flush().expect("flush");
    drop(mmap);
    drop(file);

    let dm = DiskManager::open(&ctx.image_path, 0).expect("open dm");
    let report = dm.verify_integrity().expect("fsck");
    assert!(!report.is_clean);
    assert_eq!(report.leaked_blocks, vec![sb.data_block_start + 20]);
}

#[test]
fn test_fsck_detect_missing_data_block() {
    let ctx = TestContext::new("test_fsck_missing");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;
    let file_id = dm.create_file(root_id, "missing_test.txt").expect("create file");
    dm.write_data(file_id, 0, b"Important data", CompressionMode::Never).expect("write data");

    let inode = dm.read_inode(file_id).expect("read inode");
    let allocated_block = inode.blocks[0];
    let sb = dm.superblock();
    drop(dm);

    // Modify image directly: clear the bit in data bitmap for allocated_block
    let file = OpenOptions::new().read(true).write(true).open(&ctx.image_path).expect("open file");
    let mut mmap = unsafe { memmap2::MmapMut::map_mut(&file).expect("mmap") };
    let db_start = (sb.data_bitmap_block * 4096) as usize;
    let mut bitmap = Bitmap::new(&mut mmap[db_start..db_start + 4096]);
    let bit_idx = (allocated_block - sb.data_block_start) as usize;
    bitmap.clear(bit_idx);
    mmap.flush().expect("flush");
    drop(mmap);
    drop(file);

    let dm = DiskManager::open(&ctx.image_path, 0).expect("open dm");
    let report = dm.verify_integrity().expect("fsck");
    assert!(!report.is_clean);
    assert_eq!(report.missing_blocks, vec![allocated_block]);
}

#[test]
fn test_fsck_detect_cross_linked_blocks() {
    let ctx = TestContext::new("test_fsck_cross_linked");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    let f1 = dm.create_file(root_id, "file_a.txt").expect("create f1");
    dm.write_data(f1, 0, b"Data in block A", CompressionMode::Never).expect("write f1");

    let f2 = dm.create_file(root_id, "file_b.txt").expect("create f2");
    dm.write_data(f2, 0, b"Data in block B", CompressionMode::Never).expect("write f2");

    let inode1 = dm.read_inode(f1).expect("read inode 1");
    let target_block = inode1.blocks[0];

    // Corrupt inode 2 to also point to inode 1's block
    let mut inode2 = Inode::new(FileType::File);
    inode2.size = 15;
    inode2.blocks[0] = target_block;
    dm.write_inode(f2, &inode2).expect("write corrupted inode 2");

    let report = dm.verify_integrity().expect("fsck");
    assert!(!report.is_clean);
    assert_eq!(report.cross_linked_blocks, vec![target_block]);
}
