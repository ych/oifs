//! Tests for SEC-20: block pointers and WAL replay must never reach outside
//! the data area.

use oifs::disk::{CompressionMode, DiskManager};
use oifs::journal::MetadataOp;
use oifs::superblock::SuperBlock;
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
fn test_reading_a_file_never_exposes_metadata_blocks() {
    let img = TestImage::new("test_sec20_read");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "evil.bin").expect("create file");
    dm.write_data(id, 0, b"legitimate payload", CompressionMode::Never)
        .expect("write data");

    // Point the file at the superblock (block 0) and at the inode bitmap
    // (block 1): a corrupted or hostile image must not turn those blocks
    // into file content.
    let mut inode = dm.read_inode(id).expect("read inode");
    inode.blocks[0] = 0; // the superblock
    dm.write_inode(id, &inode).expect("write inode");
    drop(dm);

    let dm = DiskManager::open(&img.path, 0).expect("reopen dm");
    let data = dm.read_data(id).expect("read data");
    assert!(
        !data.windows(4).any(|w| w == b"OIFS"),
        "the superblock must not be readable as file content"
    );

    let mut inode = dm.read_inode(id).expect("read inode");
    inode.blocks[0] = 1; // the inode bitmap
    dm.write_inode(id, &inode).expect("write inode");
    drop(dm);

    let dm = DiskManager::open(&img.path, 0).expect("reopen dm");
    let data = dm.read_data(id).expect("read data");
    assert!(
        data.len() <= 4096,
        "a pointer outside the data area must not expose other blocks"
    );
}

#[test]
fn test_writing_through_a_forged_pointer_cannot_corrupt_the_filesystem() {
    let img = TestImage::new("test_sec20_write");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "evil.bin").expect("create file");
    dm.write_data(id, 0, b"legitimate payload", CompressionMode::Never)
        .expect("write data");

    // Point the file at the inode bitmap (block 1): a partial write that
    // lands in the first block would otherwise overwrite the bitmap.
    let mut inode = dm.read_inode(id).expect("read inode");
    inode.blocks[0] = 1;
    dm.write_inode(id, &inode).expect("write inode");
    drop(dm);

    let dm = DiskManager::open(&img.path, 0).expect("reopen dm");
    let result = dm.write_data(id, 100, b"OVERWRITE", CompressionMode::Never);
    assert!(result.is_err(), "a forged pointer must not be writable");
    drop(dm);

    // The image must still be mountable and usable: the superblock and the
    // inode bitmap were not touched by the rejected write.
    let dm = DiskManager::open(&img.path, 0).expect("reopen after the rejected write");
    let sb = dm.superblock();
    assert_eq!(sb.magic, SuperBlock::MAGIC, "the superblock must be intact");
    let root = sb.root_inode;
    let new_id = dm
        .create_file(root, "after.bin")
        .expect("the filesystem must still be usable");
    dm.write_data(new_id, 0, b"still working", CompressionMode::Never)
        .expect("the filesystem must still be writable");
    assert_eq!(dm.read_data(new_id).unwrap(), b"still working");
}

#[test]
fn test_wal_replay_refuses_blocks_outside_the_data_area() {
    let img = TestImage::new("test_sec20_wal");
    let dm = DiskManager::open_journaled(&img.path, 10 * 1024 * 1024).expect("open journaled dm");
    let sb = dm.superblock();
    drop(dm);

    // A crafted WAL transaction that tries to overwrite the superblock, the
    // inode bitmap and an out-of-range inode.
    let mut image = fs::read(&img.path).expect("read image");

    let err = oifs::journal::apply_op_in_place(
        &mut image,
        &sb,
        &MetadataOp::WriteBlockSlice {
            block_id: 0,
            offset: 0,
            data: vec![0xFF; 16],
        },
    );
    assert!(
        err.is_err(),
        "the superblock must not be writable from the WAL"
    );

    let err = oifs::journal::apply_op_in_place(
        &mut image,
        &sb,
        &MetadataOp::WriteBlockSlice {
            block_id: sb.inode_bitmap_block,
            offset: 0,
            data: vec![0xFF; 16],
        },
    );
    assert!(
        err.is_err(),
        "the inode bitmap must not be writable from the WAL"
    );

    let err = oifs::journal::apply_op_in_place(
        &mut image,
        &sb,
        &MetadataOp::WriteInode {
            inode_id: sb.inode_count + 1000,
            inode_bytes: Box::new([0u8; 256]),
        },
    );
    assert!(err.is_err(), "an out-of-range inode must not be writable");

    let err = oifs::journal::apply_op_in_place(
        &mut image,
        &sb,
        &MetadataOp::SetDataBitmap {
            block_id: 1,
            allocated: true,
        },
    );
    assert!(
        err.is_err(),
        "the data bitmap must only describe the data area"
    );

    // A legitimate data-area write still works.
    let ok = oifs::journal::apply_op_in_place(
        &mut image,
        &sb,
        &MetadataOp::WriteBlockSlice {
            block_id: sb.data_block_start + 1,
            offset: 0,
            data: vec![0x42; 16],
        },
    );
    assert!(ok.is_ok(), "a data-area write must still be accepted");
}
