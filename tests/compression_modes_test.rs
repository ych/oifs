use oifs::disk::{CompressionMode, DiskManager};
use std::fs;
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
fn test_compression_mode_always() {
    let ctx = TestContext::new("test_comp_always");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    let file_id = dm.create_file(root_id, "always.txt").expect("create file");
    let small_data = b"small repeatable data small repeatable data";

    dm.write_data(file_id, 0, small_data, CompressionMode::Always).expect("write data");

    let inode = dm.read_inode(file_id).expect("read inode");
    assert_eq!(inode.size, small_data.len() as u64);
    assert!(inode.compressed_size > 0, "Compressed size should be recorded when Always is used");

    let read_back = dm.read_data(file_id).expect("read data");
    assert_eq!(read_back, small_data);
}

#[test]
fn test_compression_mode_never() {
    let ctx = TestContext::new("test_comp_never");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    let file_id = dm.create_file(root_id, "never.txt").expect("create file");
    let repetitive_large = "A".repeat(16384);

    dm.write_data(file_id, 0, repetitive_large.as_bytes(), CompressionMode::Never).expect("write data");

    let inode = dm.read_inode(file_id).expect("read inode");
    assert_eq!(inode.size, 16384);
    assert_eq!(inode.compressed_size, 0, "Compressed size should be 0 when Never is specified");

    let read_back = dm.read_data(file_id).expect("read data");
    assert_eq!(read_back, repetitive_large.as_bytes());
}

#[test]
fn test_compression_mode_auto_decision() {
    let ctx = TestContext::new("test_comp_auto");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    // 1. Highly compressible data >= 8KB -> should compress
    let file_c = dm.create_file(root_id, "comp.txt").expect("create file");
    let compressible = "PATTERN_DATA_1234567890_".repeat(500); // 12KB
    dm.write_data(file_c, 0, compressible.as_bytes(), CompressionMode::Auto).expect("write data");

    let inode_c = dm.read_inode(file_c).expect("read inode");
    assert_eq!(inode_c.size, compressible.len() as u64);
    assert!(inode_c.compressed_size > 0, "Auto should compress repetitive data >= 8KB");
    assert!(inode_c.compressed_size < inode_c.size, "Compressed size must be smaller than logical size");

    let read_c = dm.read_data(file_c).expect("read data");
    assert_eq!(read_c, compressible.as_bytes());

    // 2. High-entropy random data >= 8KB -> should NOT compress
    let file_u = dm.create_file(root_id, "random.bin").expect("create file");
    let mut state: u64 = 123456789;
    let random_data: Vec<u8> = (0..16384).map(|_| {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 33) as u8
    }).collect();

    dm.write_data(file_u, 0, &random_data, CompressionMode::Auto).expect("write data");

    let inode_u = dm.read_inode(file_u).expect("read inode");
    assert_eq!(inode_u.size, 16384);
    assert_eq!(inode_u.compressed_size, 0, "Auto should not compress random data if not smaller");

    let read_u = dm.read_data(file_u).expect("read data");
    assert_eq!(read_u, random_data);
}

#[test]
fn test_append_semantics() {
    let ctx = TestContext::new("test_append_modes");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    // 1. Append to uncompressed file
    let file_uncomp = dm.create_file(root_id, "uncompressed_log.txt").expect("create file");
    dm.write_data(file_uncomp, 0, b"Line 1\n", CompressionMode::Never).expect("write line 1");
    dm.write_data(file_uncomp, 7, b"Line 2\n", CompressionMode::Never).expect("write line 2");

    let read_log = dm.read_data(file_uncomp).expect("read log");
    assert_eq!(read_log, b"Line 1\nLine 2\n");

    // 2. Attempt append to compressed file -> should return error
    let file_comp = dm.create_file(root_id, "compressed_log.txt").expect("create file");
    dm.write_data(file_comp, 0, b"Initial compressed data", CompressionMode::Always).expect("write initial");

    let append_err = dm.write_data(file_comp, 23, b"Appended data", CompressionMode::Never);
    assert!(append_err.is_err(), "Appending to an already-compressed file must return an error");
}
