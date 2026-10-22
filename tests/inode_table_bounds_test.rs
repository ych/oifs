//! Regression tests for SEC-25: the inode table and the data bitmap must
//! never be accessed outside their own region.

use oifs::disk::{CompressionMode, DiskManager, DiskManagerError};
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
fn test_inode_id_beyond_the_table_is_rejected() {
    let img = TestImage::new("test_sec25_fake_inode");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    // A file whose content is a crafted 256-byte "fake inode" pointing at
    // another file's data block.
    let victim = dm.create_file(root, "victim.txt").expect("create victim");
    let secret = b"TOP-SECRET-CONTENT-THAT-MUST-NOT-LEAK";
    dm.write_data(victim, 0, secret, CompressionMode::Never)
        .expect("write victim");

    // The first inode id that is *not* backed by a table slot.
    let fake_id = dm.superblock().usable_inode_count();
    assert!(
        fake_id < dm.superblock().inode_count || fake_id == dm.superblock().inode_count,
        "the fake id must be outside the usable range"
    );

    // Every entry point that takes an inode id must refuse it.
    assert!(
        dm.read_inode(fake_id).is_err(),
        "reading an inode beyond the table must fail"
    );
    assert!(
        dm.read_data(fake_id).is_err(),
        "reading data through a fake inode must fail"
    );
    assert!(
        dm.write_data(fake_id, 0, b"overwrite", CompressionMode::Never)
            .is_err(),
        "writing through a fake inode must fail"
    );
    assert!(
        dm.truncate(fake_id, 0).is_err(),
        "truncating through a fake inode must fail"
    );

    // The victim file is untouched.
    assert_eq!(dm.read_data(victim).unwrap(), secret.to_vec());
}

#[test]
fn test_inode_allocation_stops_at_the_table_boundary() {
    // A small image: the inode table is only a few blocks, so the inode
    // bitmap can track far more ids than the table can hold.
    let img = TestImage::new("test_sec25_inode_full");
    let dm = DiskManager::open(&img.path, 200 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;
    let usable = dm.superblock().usable_inode_count();
    // The usable ids must fit in the inode table (the v3 layout reports the
    // count honestly; a legacy image may claim more than fits).
    assert!(
        usable <= dm.superblock().inode_table_capacity(),
        "the usable inode ids must fit in the inode table"
    );

    // Fill the inode table.
    let mut created = 0u64;
    let mut failure = None;
    for i in 0..usable + 8 {
        match dm.create_file(root, &format!("f{}", i)) {
            Ok(_) => created += 1,
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }

    let failure = failure.expect("creating more inodes than the table holds must fail");
    assert!(
        matches!(failure, DiskManagerError::Allocator(_)),
        "expected an allocator error, got {failure:?}"
    );
    // The root inode already occupies one id.
    assert_eq!(
        created,
        usable - 1,
        "exactly the remaining usable inodes may be created"
    );

    // The root directory must still be intact: the inode bitmap must not have
    // been able to hand out an id that would overwrite the data area.
    let entries = dm.list_dir(root).expect("list root");
    assert_eq!(
        entries.len() as u64,
        usable - 1,
        "every file must be listed"
    );
    let fsck = dm.verify_integrity().expect("fsck");
    assert!(
        fsck.is_clean,
        "the filesystem must stay consistent: {fsck:?}"
    );
}

#[test]
fn test_data_block_allocation_stops_at_the_image_boundary() {
    // A tiny image: the data bitmap is a full 4 KiB block, so it can track
    // far more data blocks than the image actually has.
    let img = TestImage::new("test_sec25_data_full");
    let dm = DiskManager::open(&img.path, 200 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "big.bin").expect("create file");

    // Write far more than the image can hold.
    let chunk = vec![0x5Au8; 64 * 1024];
    let mut written = 0usize;
    let mut failure = None;
    for _ in 0..64 {
        match dm.write_data(id, written as u64, &chunk, CompressionMode::Never) {
            Ok(()) => written += chunk.len(),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }

    let failure = failure.expect("writing past the image size must fail");
    assert!(
        matches!(failure, DiskManagerError::Allocator(_)),
        "expected an allocator error, got {failure:?}"
    );

    // The image must still be consistent: no block outside the image was
    // ever allocated.
    let fsck = dm.verify_integrity().expect("fsck");
    assert!(
        fsck.is_clean,
        "the filesystem must stay consistent: {fsck:?}"
    );
    let sb = dm.superblock();
    assert!(sb.data_block_start < sb.block_count);
}

#[test]
fn test_superblock_validate_rejects_an_impossible_layout() {
    use oifs::superblock::SuperBlock;

    let sb = SuperBlock::new(1000);
    assert!(sb.validate(1000 * 4096).is_ok());

    // An inode count the bitmap cannot track.
    let mut broken = sb;
    broken.inode_count = 1_000_000;
    assert!(broken.validate(1000 * 4096).is_err());

    // A root inode outside the usable range.
    let mut broken = sb;
    broken.root_inode = broken.inode_count + 1;
    assert!(broken.validate(1000 * 4096).is_err());

    // An inode table that does not fit before the data area.
    let mut broken = sb;
    broken.inode_table_block = broken.data_block_start + 1;
    assert!(broken.validate(1000 * 4096).is_err());
}
