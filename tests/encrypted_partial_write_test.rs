//! Reproduction tests for SEC-16: encryption bypass / AEAD corruption on
//! non-zero-offset writes to encrypted images.

use oifs::disk::{CompressionMode, DiskManager};
use std::fs;
use std::path::Path;

struct EncImage {
    path: String,
}

impl EncImage {
    fn new(name: &str) -> Self {
        let path = format!("target/{}_{}.img", name, std::process::id());
        if Path::new(&path).exists() {
            let _ = fs::remove_file(&path);
        }
        Self { path }
    }
}

impl Drop for EncImage {
    fn drop(&mut self) {
        if Path::new(&self.path).exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Scenario A: a *new* file written at a non-zero offset on an encrypted image
/// must not leave plaintext on the disk.
#[test]
fn repro_scenario_a_new_file_at_nonzero_offset() {
    let img = EncImage::new("test_sec16_a");
    let dm = DiskManager::create_encrypted(&img.path, 10 * 1024 * 1024, "pw123456").unwrap();
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "a.txt").unwrap();

    let secret = b"TOP_SECRET_PLAINTEXT_MARKER_0123456789";
    dm.write_data(id, 100, secret, CompressionMode::Never)
        .unwrap();

    let inode = dm.read_inode(id).unwrap();
    println!(
        "inode: size={} compressed={} encrypted={}",
        inode.size, inode.compressed_size, inode.encrypted
    );

    let raw = fs::read(&img.path).unwrap();
    let leaked = raw.windows(secret.len()).any(|w| w == secret.as_slice());
    println!("plaintext on disk: {leaked}");
    assert!(!leaked, "SEC-16 A: plaintext leaked to an encrypted image");
    assert!(inode.encrypted, "SEC-16 A: inode must be marked encrypted");
}

/// Scenario B: appending / partially overwriting an already encrypted
/// uncompressed file at a non-zero offset must not corrupt the ciphertext.
#[test]
fn repro_scenario_b_partial_write_into_encrypted_file() {
    let img = EncImage::new("test_sec16_b");
    let dm = DiskManager::create_encrypted(&img.path, 10 * 1024 * 1024, "pw123456").unwrap();
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "b.txt").unwrap();

    let first = b"AAAAAAAABBBBBBBBCCCCCCCCDDDDDDDD";
    dm.write_data(id, 0, first, CompressionMode::Never).unwrap();
    assert_eq!(dm.read_data(id).unwrap(), first.to_vec());

    // Now write into the middle of the encrypted file.
    dm.write_data(id, 8, b"ZZZZ", CompressionMode::Never)
        .unwrap();

    let read_back = dm.read_data(id);
    println!("read back: {read_back:?}");
    let mut expected = first.to_vec();
    expected[8..12].copy_from_slice(b"ZZZZ");
    assert_eq!(
        read_back.unwrap(),
        expected,
        "SEC-16 B: partial write corrupted the encrypted file"
    );
}

/// Scenario C: shrinking overwrite of an encrypted uncompressed file at
/// offset 0 must keep the file readable and correctly sized.
#[test]
fn repro_scenario_c_shrinking_overwrite() {
    let img = EncImage::new("test_sec16_c");
    let dm = DiskManager::create_encrypted(&img.path, 10 * 1024 * 1024, "pw123456").unwrap();
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "c.txt").unwrap();

    let first = vec![0x41u8; 200];
    dm.write_data(id, 0, &first, CompressionMode::Never)
        .unwrap();
    assert_eq!(dm.read_data(id).unwrap(), first);

    let second = vec![0x42u8; 20];
    dm.write_data(id, 0, &second, CompressionMode::Never)
        .unwrap();

    let read_back = dm.read_data(id);
    println!("read back len: {:?}", read_back.as_ref().map(|d| d.len()));
    assert_eq!(
        read_back.unwrap(),
        second,
        "SEC-16 C: shrinking overwrite corrupted the encrypted file"
    );
}
