//! Regression test for SEC-26: the defragmentation must handle seekable
//! 64K chunked files, whose pointer slots hold packed `ChunkEntry` values
//! instead of raw block ids.

use oifs::disk::{CompressionMode, DefragMode, DiskManager};
use std::fs;

struct TestContext {
    image_path: String,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let image_path = format!("target/{}_{}.img", name, std::process::id());
        for suffix in ["", ".old", ".defrag.tmp"] {
            let _ = fs::remove_file(format!("{image_path}{suffix}"));
        }
        Self { image_path }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        for suffix in ["", ".old", ".defrag.tmp"] {
            let _ = fs::remove_file(format!("{}{suffix}", self.image_path));
        }
    }
}

/// Writes a file whose payload is large enough to need several 64K chunks.
fn write_seekable_file(dm: &DiskManager, root: u64, name: &str, seed: u8) -> (u64, Vec<u8>) {
    let id = dm.create_file(root, name).expect("create file");
    let payload: Vec<u8> = (0..200_000u32)
        .map(|i| ((i as u8).wrapping_mul(seed).wrapping_add(17)) ^ (i as u8))
        .collect();
    dm.write_data_with_filters(
        id,
        0,
        &payload,
        CompressionMode::Seekable {
            chunk_size: 64 * 1024,
            level: 1,
        },
        oifs::filters::FilterConfig::none(),
    )
    .expect("write seekable data");
    (id, payload)
}

#[test]
fn test_in_place_defrag_preserves_seekable_chunked_files() {
    let ctx = TestContext::new("test_sec26_inplace");
    let dm = DiskManager::open(&ctx.image_path, 20 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    let (chunked_id, chunked_payload) = write_seekable_file(&dm, root, "chunked.bin", 3);
    let (plain_id, plain_payload) = {
        let id = dm.create_file(root, "plain.bin").expect("create plain");
        let payload = vec![0x42u8; 50_000];
        dm.write_data(id, 0, &payload, CompressionMode::Never)
            .expect("write plain");
        (id, payload)
    };

    // The seekable file must really use the chunked layout.
    let inode = dm.read_inode(chunked_id).expect("read inode");
    assert_ne!(
        inode.flags & oifs::inode::INODE_FLAG_SEEKABLE_64K,
        0,
        "the file must be flagged as seekable"
    );

    // Fragment the image, then defragment it in place.
    let stats = dm
        .defragment(&ctx.image_path, DefragMode::InPlace, None)
        .expect("in-place defrag");
    assert!(stats.frag_after <= stats.frag_before);

    // The content of both files must survive the relocation.
    assert_eq!(
        dm.read_data(chunked_id).unwrap(),
        chunked_payload,
        "the seekable file must keep its content"
    );
    assert_eq!(
        dm.read_data(plain_id).unwrap(),
        plain_payload,
        "the plain file must keep its content"
    );

    let fsck = dm.verify_integrity().expect("fsck");
    assert!(
        fsck.is_clean,
        "the filesystem must stay consistent: {fsck:?}"
    );
}

#[test]
fn test_safe_defrag_preserves_seekable_chunked_files() {
    let ctx = TestContext::new("test_sec26_safe");
    let dm = DiskManager::open(&ctx.image_path, 20 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    let (chunked_id, chunked_payload) = write_seekable_file(&dm, root, "chunked.bin", 5);
    let (plain_id, plain_payload) = {
        let id = dm.create_file(root, "plain.bin").expect("create plain");
        let payload = vec![0x11u8; 30_000];
        dm.write_data(id, 0, &payload, CompressionMode::Never)
            .expect("write plain");
        (id, payload)
    };

    dm.defragment(&ctx.image_path, DefragMode::Safe, None)
        .expect("safe defrag");
    drop(dm);

    // Reopen the defragmented image and check the content.
    let dm = DiskManager::open(&ctx.image_path, 0).expect("reopen dm");
    assert_eq!(
        dm.read_data(chunked_id).unwrap(),
        chunked_payload,
        "the seekable file must keep its content across a safe defrag"
    );
    assert_eq!(
        dm.read_data(plain_id).unwrap(),
        plain_payload,
        "the plain file must keep its content across a safe defrag"
    );

    let fsck = dm.verify_integrity().expect("fsck");
    assert!(
        fsck.is_clean,
        "the filesystem must stay consistent: {fsck:?}"
    );
}
