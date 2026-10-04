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

    dm.write_data(file_id, 0, small_data, CompressionMode::Always)
        .expect("write data");

    let inode = dm.read_inode(file_id).expect("read inode");
    assert_eq!(inode.size, small_data.len() as u64);
    assert!(
        inode.compressed_size > 0,
        "Compressed size should be recorded when Always is used"
    );

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

    dm.write_data(
        file_id,
        0,
        repetitive_large.as_bytes(),
        CompressionMode::Never,
    )
    .expect("write data");

    let inode = dm.read_inode(file_id).expect("read inode");
    assert_eq!(inode.size, 16384);
    assert_eq!(
        inode.compressed_size, 0,
        "Compressed size should be 0 when Never is specified"
    );

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
    dm.write_data(file_c, 0, compressible.as_bytes(), CompressionMode::Auto)
        .expect("write data");

    let inode_c = dm.read_inode(file_c).expect("read inode");
    assert_eq!(inode_c.size, compressible.len() as u64);
    assert!(
        inode_c.compressed_size > 0,
        "Auto should compress repetitive data >= 8KB"
    );
    assert!(
        inode_c.compressed_size < inode_c.size,
        "Compressed size must be smaller than logical size"
    );

    let read_c = dm.read_data(file_c).expect("read data");
    assert_eq!(read_c, compressible.as_bytes());

    // 2. High-entropy random data >= 8KB -> should NOT compress
    let file_u = dm.create_file(root_id, "random.bin").expect("create file");
    let mut state: u64 = 123456789;
    let random_data: Vec<u8> = (0..16384)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            (state >> 33) as u8
        })
        .collect();

    dm.write_data(file_u, 0, &random_data, CompressionMode::Auto)
        .expect("write data");

    let inode_u = dm.read_inode(file_u).expect("read inode");
    assert_eq!(inode_u.size, 16384);
    assert_eq!(
        inode_u.compressed_size, 0,
        "Auto should not compress random data if not smaller"
    );

    let read_u = dm.read_data(file_u).expect("read data");
    assert_eq!(read_u, random_data);
}

#[test]
fn test_append_semantics() {
    let ctx = TestContext::new("test_append_modes");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    // 1. Append to uncompressed file
    let file_uncomp = dm
        .create_file(root_id, "uncompressed_log.txt")
        .expect("create file");
    dm.write_data(file_uncomp, 0, b"Line 1\n", CompressionMode::Never)
        .expect("write line 1");
    dm.write_data(file_uncomp, 7, b"Line 2\n", CompressionMode::Never)
        .expect("write line 2");

    let read_log = dm.read_data(file_uncomp).expect("read log");
    assert_eq!(read_log, b"Line 1\nLine 2\n");

    // 2. Append to compressed file using Zstd multi-frame concatenation
    let file_comp = dm
        .create_file(root_id, "compressed_log.txt")
        .expect("create file");
    dm.write_data(
        file_comp,
        0,
        b"Initial compressed data\n",
        CompressionMode::Always,
    )
    .expect("write initial");

    let inode_initial = dm.read_inode(file_comp).expect("read inode");
    assert!(
        inode_initial.compressed_size > 0,
        "File should be compressed"
    );
    assert_eq!(inode_initial.size, 24);

    // First multi-frame append
    dm.write_data(
        file_comp,
        24,
        b"Second line of log\n",
        CompressionMode::Always,
    )
    .expect("append frame 1");
    let inode_append1 = dm.read_inode(file_comp).expect("read inode");
    assert_eq!(inode_append1.size, 43);
    assert!(inode_append1.compressed_size > inode_initial.compressed_size);

    // Second multi-frame append
    dm.write_data(
        file_comp,
        43,
        b"Third line of log\n",
        CompressionMode::Always,
    )
    .expect("append frame 2");
    let inode_append2 = dm.read_inode(file_comp).expect("read inode");
    assert_eq!(inode_append2.size, 61);
    assert!(inode_append2.compressed_size > inode_append1.compressed_size);

    // Verify all frames decode seamlessly via zstd multi-frame
    let read_comp = dm.read_data(file_comp).expect("read compressed file");
    assert_eq!(
        read_comp,
        b"Initial compressed data\nSecond line of log\nThird line of log\n"
    );
}

#[test]
fn test_compressed_append_across_block_boundaries() {
    let ctx = TestContext::new("test_compressed_boundary");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    let file_id = dm
        .create_file(root_id, "large_stream.bin")
        .expect("create file");

    // Write a large initial chunk (10KB)
    let chunk1: Vec<u8> = (0..10240).map(|i| (i % 256) as u8).collect();
    dm.write_data(file_id, 0, &chunk1, CompressionMode::Always)
        .expect("write chunk 1");

    let inode_1 = dm.read_inode(file_id).expect("read inode");
    assert!(inode_1.compressed_size > 0);

    // Append another large chunk (20KB) to cross block boundaries
    let chunk2: Vec<u8> = (0..20480).map(|i| ((i + 7) % 256) as u8).collect();
    dm.write_data(file_id, 10240, &chunk2, CompressionMode::Always)
        .expect("append chunk 2");

    let read_full = dm.read_data(file_id).expect("read full data");
    assert_eq!(read_full.len(), 30720);
    assert_eq!(&read_full[..10240], &chunk1[..]);
    assert_eq!(&read_full[10240..], &chunk2[..]);
}

#[test]
fn test_compressed_middle_overwrite_fallback() {
    let ctx = TestContext::new("test_compressed_middle");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    let file_id = dm
        .create_file(root_id, "middle_modify.txt")
        .expect("create file");
    dm.write_data(file_id, 0, b"AAAAABBBBBCCCCC", CompressionMode::Always)
        .expect("write initial");

    // Overwrite middle "BBBBB" with "XXXXX"
    dm.write_data(file_id, 5, b"XXXXX", CompressionMode::Always)
        .expect("modify middle");

    let read = dm.read_data(file_id).expect("read modified");
    assert_eq!(read, b"AAAAAXXXXXCCCCC");
}
