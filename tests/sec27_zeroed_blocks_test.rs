//! Regression tests for SEC-27: a data block must never expose the content of
//! a deleted or truncated file.

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

const SECRET: &[u8] = b"SECRET-PLAINTEXT-OF-THE-DELETED-FILE-0123456789";

/// Writes a file that fills one block, deletes it and creates a new file in
/// the freed block. The new file must not expose the old content.
fn check_freed_block_is_not_disclosed(dm: &DiskManager, label: &str) {
    let root = dm.superblock().root_inode;

    // A file that fills exactly one block.
    let victim = dm.create_file(root, "victim.bin").expect("create victim");
    let payload: Vec<u8> = SECRET.iter().copied().cycle().take(4096).collect();
    dm.write_data(victim, 0, &payload, CompressionMode::Never)
        .expect("write victim");
    assert_eq!(dm.read_data(victim).unwrap().len(), 4096);
    let victim_block = dm.read_inode(victim).unwrap().blocks[0];

    // Fill the data area so that the data blocks are exhausted: after the
    // victim is deleted, the freed block is the only one left and the next
    // allocation has to reuse it.
    let mut filler = 0u32;
    loop {
        let name = format!("filler{filler}.bin");
        let Ok(id) = dm.create_file(root, &name) else {
            break;
        };
        if dm
            .write_data(id, 0, &[0x33u8; 4096], CompressionMode::Never)
            .is_err()
        {
            // The data area is full.
            break;
        }
        filler += 1;
        assert!(filler < 4096, "the data area must fill up");
    }

    // Delete the victim: its block returns to the free list.
    dm.delete_file(root, "victim.bin").expect("delete victim");

    // The next file must reuse the freed block.
    let newcomer = dm
        .create_file(root, "newcomer.bin")
        .expect("create newcomer in the freed block");
    dm.write_data(newcomer, 0, b"fresh", CompressionMode::Never)
        .expect("write newcomer");

    let newcomer_block = dm.read_inode(newcomer).unwrap().blocks[0];
    assert_eq!(
        newcomer_block, victim_block,
        "the newcomer must reuse the freed block"
    );

    // The raw block must not contain the plaintext of the deleted file.
    let image = fs::read(dm.backing_path()).expect("read image");
    let start = (newcomer_block as usize) * 4096;
    let block = &image[start..start + 4096];
    assert!(
        !block.windows(SECRET.len()).any(|w| w == SECRET),
        "{label}: the reused block still holds the plaintext of the deleted file"
    );

    // Extending the file to the full block must not reveal the old content.
    dm.truncate(newcomer, 4096).expect("truncate newcomer");
    let content = dm.read_data(newcomer).expect("read newcomer");
    assert_eq!(content.len(), 4096);
    assert!(
        !content.windows(SECRET.len()).any(|w| w == SECRET),
        "{label}: the new file exposes the content of the deleted file"
    );
    assert!(
        content[5..].iter().all(|&b| b == 0),
        "{label}: the extended region must be zeroed"
    );
}

#[test]
fn test_freed_block_is_not_disclosed_in_place() {
    let img = TestImage::new("test_sec27_freed");
    let dm = DiskManager::open(&img.path, 400 * 1024).expect("open dm");
    check_freed_block_is_not_disclosed(&dm, "in-place");
}

#[test]
fn test_freed_block_is_not_disclosed_on_a_journaled_image() {
    let img = TestImage::new("test_sec27_freed_journaled");
    let dm = DiskManager::open_journaled(&img.path, 400 * 1024).expect("open journaled dm");
    check_freed_block_is_not_disclosed(&dm, "journaled");
}

#[test]
fn test_truncate_extension_zeroes_the_slack() {
    let img = TestImage::new("test_sec27_slack");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    let id = dm.create_file(root, "grow.bin").expect("create file");
    let payload = vec![0x7Fu8; 4096];
    dm.write_data(id, 0, &payload, CompressionMode::Never)
        .expect("write file");

    // Shrink to 100 bytes, then grow again: the region between must be zero.
    dm.truncate(id, 100).expect("truncate down");
    dm.truncate(id, 4096).expect("truncate up");

    let content = dm.read_data(id).expect("read file");
    assert_eq!(content.len(), 4096);
    assert!(
        content[100..].iter().all(|&b| b == 0),
        "the re-extended region must be zeroed, not the old content"
    );
}
