//! Tests for P4.25: the data bitmap may span several blocks, so the data
//! area is no longer capped at the 32,768 blocks a single bitmap block can
//! track (128 MiB).

use oifs::disk::{CompressionMode, DiskManager};
use std::fs;
use std::path::Path;

struct TestImage {
    path: String,
}

impl TestImage {
    fn new(name: &str) -> Self {
        let path = format!("target/{}_{}.img", name, std::process::id());
        if Path::new(&path).exists() {
            let _ = fs::remove_file(&path);
        }
        Self { path }
    }
}

impl Drop for TestImage {
    fn drop(&mut self) {
        if Path::new(&self.path).exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[test]
fn test_large_image_gets_a_multi_block_data_bitmap() {
    // 200 MiB: the data area is far beyond the 32,768 blocks a single bitmap
    // block can track.
    let img = TestImage::new("test_p425_layout");
    let dm = DiskManager::open(&img.path, 200 * 1024 * 1024).expect("open dm");
    let sb = dm.superblock();

    assert!(sb.is_v3_layout(), "a new image must use the v3 layout");
    assert!(
        sb.data_bitmap_blocks() > 1,
        "a 200 MiB image needs more than one data bitmap block, got {}",
        sb.data_bitmap_blocks()
    );

    // The regions must follow each other exactly.
    assert_eq!(sb.inode_bitmap_block, 1);
    assert_eq!(
        sb.data_bitmap_block,
        sb.inode_bitmap_block + sb.inode_bitmap_blocks()
    );
    assert_eq!(
        sb.inode_table_block,
        sb.data_bitmap_block + sb.data_bitmap_blocks()
    );
    assert_eq!(sb.data_block_start, sb.inode_table_block + 1024);

    // The superblock must validate.
    let image = fs::read(&img.path).expect("read image");
    assert!(sb.validate(image.len()).is_ok());
}

#[test]
fn test_small_image_keeps_a_single_bitmap_block() {
    let img = TestImage::new("test_p425_small");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let sb = dm.superblock();
    assert_eq!(sb.data_bitmap_blocks(), 1);
    assert_eq!(sb.inode_bitmap_blocks(), 1);
    assert_eq!(sb.inode_bitmap_block, 1);
    assert_eq!(sb.data_bitmap_block, 2);
    assert_eq!(sb.inode_table_block, 3);
    assert_eq!(sb.data_block_start, 3 + 1024);
}

#[test]
fn test_multi_block_image_stays_consistent() {
    // The image is sparse, so creating it is cheap; the functional part is
    // that the multi-block bitmap is read consistently by the allocator, the
    // fsck and the defragmentation.
    let img = TestImage::new("test_p425_consistency");
    let dm = DiskManager::open(&img.path, 200 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    for i in 0..8 {
        let id = dm
            .create_file(root, &format!("f{i}.bin"))
            .expect("create file");
        let payload = vec![i as u8; 32 * 1024];
        dm.write_data(id, 0, &payload, CompressionMode::Never)
            .expect("write data");
        assert_eq!(dm.read_data(id).expect("read data"), payload);
    }

    let fsck = dm.verify_integrity().expect("fsck");
    assert!(
        fsck.is_clean,
        "the filesystem must stay consistent: {fsck:?}"
    );

    // The defragmentation must survive the multi-block bitmap as well.
    let stats = dm
        .defragment(&img.path, oifs::disk::DefragMode::InPlace, None)
        .expect("in-place defrag");
    assert!(stats.frag_after <= stats.frag_before);
    let fsck = dm.verify_integrity().expect("fsck after defrag");
    assert!(
        fsck.is_clean,
        "the filesystem must stay consistent: {fsck:?}"
    );
}
