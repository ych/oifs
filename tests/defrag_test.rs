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
        dm.write_data(id, 0, content.as_bytes(), CompressionMode::Never).expect("write data");
        files_data.push((name, content));
    }

    // Now delete alternating files (1, 3, 5, 7, 9) to introduce fragmentation gaps
    for i in (1..10).step_by(2) {
        let name = format!("file_{}.txt", i);
        dm.delete_file(root_id, &name).expect("delete file");
    }

    // Analyze fragmentation before defrag
    let stats_before = dm.analyze_fragmentation().expect("analyze frag before");
    assert!(stats_before.free_runs > 1, "Should have multiple free runs after deleting alternating files");
    assert!(stats_before.fragmentation_ratio > 0.0, "Fragmentation ratio should be > 0.0");

    // Perform safe defragmentation
    let defrag_stats = dm.defragment(&ctx.image_path, DefragMode::Safe, None).expect("defrag safe");
    assert_eq!(defrag_stats.files_processed, 5);
    assert!(defrag_stats.frag_after <= defrag_stats.frag_before);

    // Reopen filesystem to verify defragmented disk state
    drop(dm);
    let dm_reopened = DiskManager::open(&ctx.image_path, 0).expect("reopen dm");

    // Verify surviving files (0, 2, 4, 6, 8) have 100% data integrity
    for i in (0..10).step_by(2) {
        let (ref name, ref expected_content) = files_data[i];
        let file_id = dm_reopened.lookup(root_id, name).expect("lookup existing file");
        let data = dm_reopened.read_data(file_id).expect("read data");
        assert_eq!(data, expected_content.as_bytes(), "Data for {} must match perfectly", name);
    }

    // Verify deleted files (1, 3, 5, 7, 9) are truly gone
    for i in (1..10).step_by(2) {
        let (ref name, _) = files_data[i];
        assert!(dm_reopened.lookup(root_id, name).is_err(), "Deleted file {} must not exist", name);
    }

    // Verify filesystem integrity
    let fsck = dm_reopened.verify_integrity().expect("verify integrity");
    assert!(fsck.is_clean, "Defragmented filesystem must pass fsck cleanly: {:?}", fsck);
}
