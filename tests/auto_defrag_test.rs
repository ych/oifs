//! Tests for the background automatic defragmentation coordinator
//! (Blueprint item 7).

use oifs::auto_defrag::AutoDefragConfig;
use oifs::disk::{CompressionMode, DiskManager};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

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

/// Creates 10 files of 8 KiB and deletes every other one, which leaves a
/// fragmented data area with several free runs.
fn build_fragmented_image(dm: &DiskManager) -> Vec<(String, String)> {
    let root_id = dm.superblock().root_inode;
    let mut files = Vec::new();
    for i in 0..10 {
        let name = format!("file_{}.txt", i);
        let id = dm.create_file(root_id, &name).expect("create file");
        let content = format!("payload of file {}: {}", i, "X".repeat(8192));
        dm.write_data(id, 0, content.as_bytes(), CompressionMode::Never)
            .expect("write data");
        files.push((name, content));
    }
    for i in (1..10).step_by(2) {
        let name = format!("file_{}.txt", i);
        dm.delete_file(root_id, &name).expect("delete file");
    }
    files
}

/// Waits until `predicate` holds or the timeout expires.
fn wait_until<F: Fn() -> bool>(timeout: Duration, predicate: F) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    predicate()
}

#[test]
fn test_auto_defrag_config_defaults() {
    let config = AutoDefragConfig::default();
    assert!(!config.enabled, "auto-defrag must be opt-in");
    assert_eq!(config.fragmentation_threshold, 0.25);
    assert_eq!(config.check_interval, Duration::from_secs(300));
    assert!(!config.allow_journaled);

    let enabled = AutoDefragConfig::new(0.5);
    assert!(enabled.enabled);
    assert_eq!(enabled.fragmentation_threshold, 0.5);

    // The threshold is clamped into the valid range.
    assert_eq!(AutoDefragConfig::new(5.0).fragmentation_threshold, 1.0);
    assert_eq!(AutoDefragConfig::new(-1.0).fragmentation_threshold, 0.0);
}

#[test]
fn test_auto_defrag_disabled_by_default_runs_no_pass() {
    let img = TestImage::new("test_auto_defrag_disabled");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    build_fragmented_image(&dm);

    assert!(!dm.auto_defrag_config().enabled);
    std::thread::sleep(Duration::from_millis(200));
    let stats = dm.auto_defrag_stats();
    assert_eq!(stats.passes, 0, "no pass may run while disabled");
    assert_eq!(stats.checks, 0);
}

#[test]
fn test_auto_defrag_runs_pass_and_keeps_data_intact() {
    let img = TestImage::new("test_auto_defrag_pass");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let files = build_fragmented_image(&dm);

    let before = dm.analyze_fragmentation().expect("frag before");
    assert!(before.free_runs > 1, "image should be fragmented");

    // Threshold 0.0 makes every measurement trigger a pass.
    dm.set_auto_defrag(AutoDefragConfig::new(0.0).with_check_interval(Duration::from_millis(50)));
    assert!(dm.auto_defrag_config().enabled);

    let ran = wait_until(Duration::from_secs(10), || {
        dm.auto_defrag_stats().passes >= 1
    });
    let stats = dm.auto_defrag_stats();
    assert!(
        ran,
        "the background coordinator should have run a pass: {stats:?}"
    );
    assert!(stats.checks >= 1);
    assert_eq!(stats.errors, 0, "the pass must not fail: {stats:?}");

    // The filesystem must remain consistent and readable.
    let fsck = dm.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "fsck must stay clean: {fsck:?}");

    let root_id = dm.superblock().root_inode;
    for (name, content) in &files {
        if name.contains("_1.")
            || name.contains("_3.")
            || name.contains("_5.")
            || name.contains("_7.")
            || name.contains("_9.")
        {
            continue; // deleted files
        }
        let id = dm.lookup(root_id, name).expect("lookup");
        let data = dm.read_data(id).expect("read data");
        assert_eq!(&data[..], content.as_bytes(), "content of {name} changed");
    }

    // A second measurement must show a non-fragmented layout.
    let after = dm.analyze_fragmentation().expect("frag after");
    assert!(
        after.free_runs <= before.free_runs,
        "free runs should not increase: {before:?} -> {after:?}"
    );

    dm.stop_auto_defrag();
}

#[test]
fn test_auto_defrag_stops_on_disable() {
    let img = TestImage::new("test_auto_defrag_stop");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    build_fragmented_image(&dm);

    dm.set_auto_defrag(AutoDefragConfig::new(0.0).with_check_interval(Duration::from_millis(30)));
    assert!(wait_until(Duration::from_secs(5), || dm
        .auto_defrag_stats()
        .passes
        >= 1));

    dm.set_auto_defrag(AutoDefragConfig::default());
    assert!(!dm.auto_defrag_config().enabled);
    let passes_at_stop = dm.auto_defrag_stats().passes;

    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        dm.auto_defrag_stats().passes,
        passes_at_stop,
        "no further pass may run after the coordinator was disabled"
    );
}

#[test]
fn test_auto_defrag_skips_journaled_image_by_default() {
    let img = TestImage::new("test_auto_defrag_journaled");
    let dm = DiskManager::open_journaled(&img.path, 10 * 1024 * 1024).expect("open journaled dm");
    build_fragmented_image(&dm);

    dm.set_auto_defrag(AutoDefragConfig::new(0.0).with_check_interval(Duration::from_millis(30)));

    assert!(wait_until(Duration::from_secs(5), || dm
        .auto_defrag_stats()
        .checks
        >= 1));
    let stats = dm.auto_defrag_stats();
    assert_eq!(
        stats.passes, 0,
        "in-place defrag must not run on a WAL-enabled image: {stats:?}"
    );
    assert!(stats.skipped >= 1);
    assert!(
        stats
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("WAL-enabled"),
        "the skip reason should be recorded: {stats:?}"
    );

    dm.stop_auto_defrag();
}

#[test]
fn test_auto_defrag_respects_min_free_blocks() {
    let img = TestImage::new("test_auto_defrag_min_free");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    build_fragmented_image(&dm);

    // Require more free blocks than the image has: the pass must be skipped.
    dm.set_auto_defrag(
        AutoDefragConfig::new(0.0)
            .with_check_interval(Duration::from_millis(30))
            .with_min_free_blocks(u64::MAX / 2),
    );

    assert!(wait_until(Duration::from_secs(5), || dm
        .auto_defrag_stats()
        .checks
        >= 1));
    let stats = dm.auto_defrag_stats();
    assert_eq!(stats.passes, 0, "the pass must be skipped: {stats:?}");
    assert!(stats.skipped >= 1);

    dm.stop_auto_defrag();
}
