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

#[test]
fn test_p425_allocate_and_free_beyond_first_bitmap_block() {
    let img = TestImage::new("test_p425_beyond_32k");
    let dm = DiskManager::open(&img.path, 200 * 1024 * 1024).expect("open 200 MiB dm");
    let sb = dm.superblock();
    assert!(sb.data_bitmap_blocks() >= 2);
    let root = sb.root_inode;

    // Root directory uses 1 data block (bit 0). Reserve the remaining 32,767
    // blocks of the first 4 KiB data bitmap block so the next allocation must
    // cross into the second bitmap block (bit >= 32,768).
    let filler_count = 32_767;
    let filler_start = dm
        .allocate_contiguous_blocks(filler_count)
        .expect("reserve rest of first bitmap block");
    assert_eq!(filler_start, sb.data_block_start + 1);

    let id = dm
        .create_file(root, "beyond_32k.bin")
        .expect("create file beyond 32k");
    let payload = vec![0x5Au8; 12 * 1024]; // 3 blocks in the 2nd bitmap block
    dm.write_data(id, 0, &payload, CompressionMode::Never)
        .expect("write beyond 32k");

    let inode = dm.read_inode(id).expect("read inode");
    assert!(
        inode.blocks[0] >= sb.data_block_start + 32_768,
        "block {} must lie in the second bitmap block (>= {})",
        inode.blocks[0],
        sb.data_block_start + 32_768
    );
    assert_eq!(dm.read_data(id).expect("read beyond 32k"), payload);

    // Shrink and delete the file so both truncate and delete_file free bits in
    // the second data bitmap block.
    dm.truncate(id, 4096).expect("truncate beyond 32k");
    dm.delete_file(root, "beyond_32k.bin")
        .expect("delete beyond 32k");

    // Release the reserved blocks and verify full filesystem integrity.
    dm.free_contiguous_blocks(filler_start, filler_count)
        .expect("free filler blocks");
    let fsck = dm.verify_integrity().expect("fsck");
    assert!(
        fsck.is_clean,
        "fsck must be clean after freeing blocks in the second bitmap block: {fsck:?}"
    );
}

/// The v3 layout places the journal right after the data bitmap. The journal
/// I/O used to address the fixed legacy block 3, which on a journaled image
/// with a multi-block data bitmap *is* the second data bitmap block: the
/// allocator then overwrote the journal header (`Journal(BadMagic)`) and every
/// journal frame overwrote allocation bits.
#[test]
fn test_journal_does_not_overlap_a_multi_block_data_bitmap() {
    let img = TestImage::new("test_p425_journal_overlap");
    let size = 512 * 1024 * 1024;
    let payload: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    {
        let dm = DiskManager::open_journaled(&img.path, size).expect("create journaled");
        let sb = dm.superblock();
        assert!(dm.has_journal());
        assert!(
            sb.data_bitmap_blocks() > 1,
            "the test needs a multi-block data bitmap"
        );
        assert_eq!(
            sb.journal_header_block(),
            sb.data_bitmap_block + sb.data_bitmap_blocks(),
            "the journal must start after the data bitmap"
        );

        // 50 x 4 MiB = 51,200 data blocks: well beyond the first bitmap block.
        for i in 0..50 {
            let id = dm.create_file(0, &format!("f{i}")).expect("create");
            dm.write_data(id, 0, &payload, CompressionMode::Never)
                .expect("journaled write beyond the first bitmap block");
        }
        let report = dm.verify_integrity().expect("fsck");
        assert!(report.is_clean, "fsck before reopen: {report:?}");
    }

    let dm = DiskManager::open_journaled(&img.path, size).expect("reopen journaled");
    let report = dm.verify_integrity().expect("fsck");
    assert!(report.is_clean, "fsck after reopen: {report:?}");
    for i in [0, 25, 49] {
        let id = dm.lookup(0, &format!("f{i}")).expect("lookup");
        assert_eq!(dm.read_data(id).expect("read"), payload, "file f{i}");
    }
}
