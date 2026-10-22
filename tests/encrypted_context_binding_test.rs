//! Regression test for SEC-19: the AEAD must bind the ciphertext to its
//! file, so a chunk (or a whole file) cannot be moved to another file
//! without being detected.

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
fn test_swapping_two_encrypted_files_is_detected() {
    let img = TestImage::new("test_sec19_swap");
    let dm = DiskManager::create_encrypted(&img.path, 10 * 1024 * 1024, "pw123456")
        .expect("create encrypted image");
    let root = dm.superblock().root_inode;

    let a = dm.create_file(root, "a.txt").expect("create a");
    let b = dm.create_file(root, "b.txt").expect("create b");
    let secret_a = b"CONTENT-OF-FILE-A-0123456789-ABCDEFGHIJKLMNOP";
    let secret_b = b"CONTENT-OF-FILE-B-0123456789-ABCDEFGHIJKLMNOP";
    dm.write_data(a, 0, secret_a, CompressionMode::Never)
        .expect("write a");
    dm.write_data(b, 0, secret_b, CompressionMode::Never)
        .expect("write b");

    // The blocks of both files.
    let block_a = dm.read_inode(a).expect("read a").blocks[0];
    let block_b = dm.read_inode(b).expect("read b").blocks[0];
    assert_ne!(block_a, block_b);
    drop(dm);

    // Swap the two ciphertexts on disk.
    let mut image = fs::read(&img.path).expect("read image");
    let off_a = (block_a as usize) * 4096;
    let off_b = (block_b as usize) * 4096;
    let mut tmp = [0u8; 4096];
    tmp.copy_from_slice(&image[off_a..off_a + 4096]);
    let (head, tail) = image.split_at_mut(off_b);
    head[off_a..off_a + 4096].copy_from_slice(&tail[..4096]);
    tail[..4096].copy_from_slice(&tmp);
    fs::write(&img.path, &image).expect("write image");

    // The swap must be detected: the associated data binds the ciphertext to
    // its inode, so the authentication fails.
    let dm = DiskManager::open_with_password(&img.path, 0, Some("pw123456"))
        .expect("reopen encrypted image");
    let read_a = dm.read_data(a);
    let read_b = dm.read_data(b);
    assert!(
        read_a.is_err() || read_b.is_err(),
        "the swapped ciphertexts must not authenticate (a={:?}, b={:?})",
        read_a.as_ref().map(|d| d.len()),
        read_b.as_ref().map(|d| d.len())
    );
}

#[test]
fn test_encrypted_content_is_not_plaintext_on_disk() {
    let img = TestImage::new("test_sec19_plaintext");
    let dm = DiskManager::create_encrypted(&img.path, 10 * 1024 * 1024, "pw123456")
        .expect("create encrypted image");
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "secret.txt").expect("create file");
    let secret = b"THIS-MUST-NEVER-APPEAR-IN-THE-IMAGE-FILE-0123456789";
    dm.write_data(id, 0, secret, CompressionMode::Never)
        .expect("write data");
    drop(dm);

    let image = fs::read(&img.path).expect("read image");
    assert!(
        !image.windows(secret.len()).any(|w| w == secret.as_slice()),
        "the plaintext must not be visible in the image"
    );
}
