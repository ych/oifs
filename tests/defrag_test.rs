use oifs::disk::{CompressionMode, DefragMode, DiskManager};
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
        let _ = fs::remove_file(format!("{}.old", image_path));
        let _ = fs::remove_file(format!("{}.defrag.tmp", image_path));
        Self { image_path }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        if Path::new(&self.image_path).exists() {
            let _ = fs::remove_file(&self.image_path);
        }
        let _ = fs::remove_file(format!("{}.old", self.image_path));
        let _ = fs::remove_file(format!("{}.defrag.tmp", self.image_path));
    }
}

#[test]
fn test_fragmentation_analysis_and_safe_defragmentation() {
    let ctx = TestContext::new("test_defrag_flow");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");

    let root_id = dm.superblock().root_inode;

    // Create 10 files of 8KB (2 blocks each)
    let mut files_data = Vec::new();
    for i in 0..10 {
        let name = format!("file_{}.txt", i);
        let id = dm.create_file(root_id, &name).expect("create file");
        let content = format!("Content of file {} repeated data: {}", i, "X".repeat(8192));
        dm.write_data(id, 0, content.as_bytes(), CompressionMode::Never)
            .expect("write data");
        files_data.push((name, content));
    }

    // Now delete alternating files (1, 3, 5, 7, 9) to introduce fragmentation gaps
    for i in (1..10).step_by(2) {
        let name = format!("file_{}.txt", i);
        dm.delete_file(root_id, &name).expect("delete file");
    }

    // Analyze fragmentation before defrag
    let stats_before = dm.analyze_fragmentation().expect("analyze frag before");
    assert!(
        stats_before.free_runs > 1,
        "Should have multiple free runs after deleting alternating files"
    );
    assert!(
        stats_before.fragmentation_ratio > 0.0,
        "Fragmentation ratio should be > 0.0"
    );

    // Perform safe defragmentation
    let defrag_stats = dm
        .defragment(&ctx.image_path, DefragMode::Safe, None)
        .expect("defrag safe");
    assert_eq!(defrag_stats.files_processed, 5);
    assert!(defrag_stats.frag_after <= defrag_stats.frag_before);

    // Reopen filesystem to verify defragmented disk state
    drop(dm);
    let dm_reopened = DiskManager::open(&ctx.image_path, 0).expect("reopen dm");

    // Verify surviving files (0, 2, 4, 6, 8) have 100% data integrity
    for i in (0..10).step_by(2) {
        let (ref name, ref expected_content) = files_data[i];
        let file_id = dm_reopened
            .lookup(root_id, name)
            .expect("lookup existing file");
        let data = dm_reopened.read_data(file_id).expect("read data");
        assert_eq!(
            data,
            expected_content.as_bytes(),
            "Data for {} must match perfectly",
            name
        );
    }

    // Verify deleted files (1, 3, 5, 7, 9) are truly gone
    for i in (1..10).step_by(2) {
        let (ref name, _) = files_data[i];
        assert!(
            dm_reopened.lookup(root_id, name).is_err(),
            "Deleted file {} must not exist",
            name
        );
    }

    // Verify filesystem integrity
    let fsck = dm_reopened.verify_integrity().expect("verify integrity");
    assert!(
        fsck.is_clean,
        "Defragmented filesystem must pass fsck cleanly: {:?}",
        fsck
    );
}

#[test]
fn test_inplace_defragmentation() {
    let ctx = TestContext::new("test_defrag_inplace");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    // Create 12 files
    let mut files_data = Vec::new();
    for i in 0..12 {
        let name = format!("file_{}.txt", i);
        let id = dm.create_file(root_id, &name).expect("create file");
        let content = format!(
            "InPlace File {} Payload: {}",
            i,
            "Z".repeat(4096 * (1 + (i % 3)))
        );
        dm.write_data(id, 0, content.as_bytes(), CompressionMode::Never)
            .expect("write data");
        files_data.push((name, content));
    }

    // Delete alternating files (1, 3, 5, 7, 9, 11) to create multiple fragmentation gaps
    for i in (1..12).step_by(2) {
        let name = format!("file_{}.txt", i);
        dm.delete_file(root_id, &name).expect("delete file");
    }

    let stats_before = dm.analyze_fragmentation().expect("analyze frag before");
    assert!(stats_before.free_runs > 1, "Should have multiple free runs");
    assert!(stats_before.fragmentation_ratio > 0.0);

    // Run IN-PLACE defragmentation
    let defrag_stats = dm
        .defragment(&ctx.image_path, DefragMode::InPlace, None)
        .expect("defrag inplace");

    assert_eq!(defrag_stats.files_processed, 6);
    assert_eq!(
        defrag_stats.frag_after, 0.0,
        "In-place defrag must eliminate all gaps"
    );
    assert!(defrag_stats.bytes_moved > 0);

    // Verify surviving files directly on the live dm handle
    for i in (0..12).step_by(2) {
        let (ref name, ref expected_content) = files_data[i];
        let file_id = dm.lookup(root_id, name).expect("lookup existing file");
        let data = dm.read_data(file_id).expect("read data");
        assert_eq!(
            data,
            expected_content.as_bytes(),
            "Data for {} must match",
            name
        );
    }

    // Verify live handle integrity
    let fsck = dm.verify_integrity().expect("verify integrity");
    assert!(
        fsck.is_clean,
        "In-place defrag must leave filesystem clean: {:?}",
        fsck
    );

    // Reopen and verify disk persistence
    drop(dm);
    let dm_reopened = DiskManager::open(&ctx.image_path, 0).expect("reopen dm");
    for i in (0..12).step_by(2) {
        let (ref name, ref expected_content) = files_data[i];
        let file_id = dm_reopened
            .lookup(root_id, name)
            .expect("lookup existing file");
        let data = dm_reopened.read_data(file_id).expect("read data");
        assert_eq!(data, expected_content.as_bytes());
    }
    let fsck_reopened = dm_reopened
        .verify_integrity()
        .expect("verify integrity reopened");
    assert!(fsck_reopened.is_clean);
}

#[test]
fn test_safe_defragmentation_live_reload() {
    let ctx = TestContext::new("test_defrag_live_reload");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    for i in 0..8 {
        let name = format!("live_{}.bin", i);
        let id = dm.create_file(root_id, &name).expect("create file");
        let payload = vec![i as u8 * 11; 8192];
        dm.write_data(id, 0, &payload, CompressionMode::Never)
            .expect("write data");
    }

    for i in (1..8).step_by(2) {
        dm.delete_file(root_id, &format!("live_{}.bin", i))
            .expect("delete");
    }

    // Run safe defragmentation
    let stats = dm
        .defragment(&ctx.image_path, DefragMode::Safe, None)
        .expect("defrag safe");
    assert_eq!(stats.files_processed, 4);

    // Crucial check: Without dropping `dm`, can we read and write new files?
    let new_id = dm
        .create_file(root_id, "after_defrag.txt")
        .expect("create new file on live handle");
    dm.write_data(
        new_id,
        0,
        b"Fresh content after live reload",
        CompressionMode::Never,
    )
    .expect("write on live handle");

    let read_back = dm.read_data(new_id).expect("read on live handle");
    assert_eq!(read_back, b"Fresh content after live reload");

    let fsck = dm.verify_integrity().expect("verify integrity live handle");
    assert!(fsck.is_clean, "Live handle fsck must be clean: {:?}", fsck);
}

#[test]
fn test_encrypted_defragmentation_both_modes() {
    let ctx = TestContext::new("test_defrag_encrypted");
    let password = "UltraSecurePassword2026!";
    let dm = DiskManager::create_encrypted(&ctx.image_path, 10 * 1024 * 1024, password)
        .expect("create encrypted");
    let root_id = dm.superblock().root_inode;

    let mut expected_data = Vec::new();
    for i in 0..6 {
        let name = format!("enc_file_{}.dat", i);
        let id = dm.create_file(root_id, &name).expect("create enc file");
        let content = format!("Encrypted confidential payload #{} {}", i, "K".repeat(6000));
        dm.write_data(id, 0, content.as_bytes(), CompressionMode::Auto)
            .expect("write enc");
        expected_data.push((name, content));
    }

    // Delete alternating files (1, 3, 5)
    for i in (1..6).step_by(2) {
        let name = format!("enc_file_{}.dat", i);
        dm.delete_file(root_id, &name).expect("delete enc");
    }

    // 1. In-place defrag on encrypted filesystem
    let stats_inplace = dm
        .defragment(&ctx.image_path, DefragMode::InPlace, None)
        .expect("inplace defrag encrypted");
    assert_eq!(stats_inplace.files_processed, 3);
    assert_eq!(stats_inplace.frag_after, 0.0);

    for i in (0..6).step_by(2) {
        let (ref name, ref expected) = expected_data[i];
        let id = dm.lookup(root_id, name).expect("lookup enc");
        let data = dm.read_data(id).expect("read enc");
        assert_eq!(data, expected.as_bytes());
    }

    // 2. Safe defrag on encrypted filesystem
    let stats_safe = dm
        .defragment(&ctx.image_path, DefragMode::Safe, None)
        .expect("safe defrag encrypted");
    assert_eq!(stats_safe.files_processed, 3);

    for i in (0..6).step_by(2) {
        let (ref name, ref expected) = expected_data[i];
        let id = dm.lookup(root_id, name).expect("lookup enc");
        let data = dm.read_data(id).expect("read enc");
        assert_eq!(data, expected.as_bytes());
    }

    let fsck = dm.verify_integrity().expect("verify encrypted fsck");
    assert!(fsck.is_clean);
}

#[test]
fn test_large_indirect_file_defragmentation() {
    let ctx = TestContext::new("test_defrag_indirect");
    let dm = DiskManager::open(&ctx.image_path, 15 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    // Create a large file using single indirect blocks (64KB = 16 blocks > 10 direct)
    let large_id = dm.create_file(root_id, "large.bin").expect("create large");
    let large_data = vec![0xAB; 64 * 1024];
    dm.write_data(large_id, 0, &large_data, CompressionMode::Never)
        .expect("write large");

    // Create intermediate files and delete to create fragmentation
    for i in 0..5 {
        let id = dm
            .create_file(root_id, &format!("temp_{}.bin", i))
            .expect("create temp");
        let chunk = vec![i as u8; 4096];
        dm.write_data(id, 0, &chunk, CompressionMode::Never)
            .expect("write temp");
    }
    for i in 0..5 {
        dm.delete_file(root_id, &format!("temp_{}.bin", i))
            .expect("delete temp");
    }

    // In-place defrag
    let stats = dm
        .defragment(&ctx.image_path, DefragMode::InPlace, None)
        .expect("defrag");
    assert_eq!(stats.frag_after, 0.0);

    // Verify large file read
    let read_large = dm.read_data(large_id).expect("read large");
    assert_eq!(read_large, large_data);

    let fsck = dm.verify_integrity().expect("verify integrity");
    assert!(fsck.is_clean);
}
