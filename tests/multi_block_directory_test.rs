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
        let _ = fs::remove_file(format!("{}.old", image_path_str(&self.image_path)));
        let _ = fs::remove_file(format!("{}.defrag.tmp", image_path_str(&self.image_path)));
    }
}

fn image_path_str(path: &str) -> &str {
    path
}

#[test]
fn test_multi_block_directory_expansion_and_lookup() {
    let ctx = TestContext::new("test_multi_block_expand");
    // 20MB disk image
    let dm = DiskManager::open(&ctx.image_path, 20 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;

    let sub_dir_id = dm
        .create_directory(root_id, "big_dir")
        .expect("create big_dir");

    // Insert 1,200 files. At ~32 bytes per entry, 1,200 entries = ~38.4 KB (requires >= 10 blocks)
    let file_count = 1200;
    for i in 0..file_count {
        let filename = format!("entry_{:04}.dat", i);
        let file_id = dm.create_file(sub_dir_id, &filename).expect("create file");
        let payload = format!("payload_content_{}", i);
        dm.write_data(file_id, 0, payload.as_bytes(), CompressionMode::Never)
            .expect("write payload");
    }

    // Verify directory blocks
    let dir_inode = dm.read_inode(sub_dir_id).expect("read dir inode");
    let num_blocks = DiskManager::dir_num_blocks(&dir_inode);
    assert!(
        num_blocks >= 10,
        "Directory with 1200 files must span multiple blocks, got {}",
        num_blocks
    );

    // Verify all 1,200 files can be found via lookup and verified
    for i in 0..file_count {
        let filename = format!("entry_{:04}.dat", i);
        let file_id = dm.lookup(sub_dir_id, &filename).expect("lookup file");
        let data = dm.read_data(file_id).expect("read file");
        let expected = format!("payload_content_{}", i);
        assert_eq!(data, expected.as_bytes());
    }

    // Verify list_dir returns all 1,200 entries
    let entries = dm.list_dir(sub_dir_id).expect("list_dir");
    assert_eq!(entries.len(), file_count);

    // Verify FSCK integrity check passes cleanly
    let report = dm.verify_integrity().expect("verify_integrity");
    assert!(
        report.is_clean,
        "FSCK must report clean multi-block directory: {:?}",
        report
    );
}

#[test]
fn test_multi_block_directory_deletion_and_compaction() {
    let ctx = TestContext::new("test_multi_block_delete");
    let dm = DiskManager::open(&ctx.image_path, 20 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;
    let sub_dir_id = dm.create_directory(root_id, "del_dir").expect("create dir");

    let total = 800;
    for i in 0..total {
        let name = format!("item_{:04}.bin", i);
        let id = dm.create_file(sub_dir_id, &name).expect("create file");
        dm.write_data(id, 0, b"data", CompressionMode::Never)
            .expect("write data");
    }

    // Delete all even-indexed files (0, 2, 4, ... -> 400 files)
    for i in (0..total).step_by(2) {
        let name = format!("item_{:04}.bin", i);
        dm.delete_file(sub_dir_id, &name).expect("delete file");
    }

    // Verify even files return NotFound
    for i in (0..total).step_by(2) {
        let name = format!("item_{:04}.bin", i);
        assert!(dm.lookup(sub_dir_id, &name).is_err());
    }

    // Verify odd files still exist and are readable
    for i in (1..total).step_by(2) {
        let name = format!("item_{:04}.bin", i);
        let id = dm.lookup(sub_dir_id, &name).expect("lookup surviving file");
        let data = dm.read_data(id).expect("read surviving file");
        assert_eq!(data, b"data");
    }

    let entries = dm.list_dir(sub_dir_id).expect("list_dir after delete");
    assert_eq!(entries.len(), total / 2);

    let report = dm.verify_integrity().expect("verify_integrity");
    assert!(
        report.is_clean,
        "FSCK must be clean after deletions: {:?}",
        report
    );
}

#[test]
fn test_multi_block_directory_defrag_safety() {
    let ctx = TestContext::new("test_multi_block_defrag");
    let dm = DiskManager::open(&ctx.image_path, 20 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;
    let dir_id = dm
        .create_directory(root_id, "defrag_dir")
        .expect("create dir");

    let total = 600;
    for i in 0..total {
        let name = format!("frag_{:04}.txt", i);
        let file_id = dm.create_file(dir_id, &name).expect("create file");
        let content = format!("File {} contents", i);
        dm.write_data(file_id, 0, content.as_bytes(), CompressionMode::Never)
            .expect("write");
    }

    // Introduce fragmentation by deleting alternating files
    for i in (0..total).step_by(2) {
        let name = format!("frag_{:04}.txt", i);
        dm.delete_file(dir_id, &name).expect("delete file");
    }

    // Run safe defragmentation
    let stats = dm
        .defragment(&ctx.image_path, DefragMode::Safe, None)
        .expect("defrag");
    assert_eq!(stats.files_processed, total / 2);

    // Reopen filesystem
    drop(dm);
    let dm_reopened = DiskManager::open(&ctx.image_path, 0).expect("reopen dm");
    let reopened_dir_id = dm_reopened
        .lookup(root_id, "defrag_dir")
        .expect("lookup dir");

    // Verify surviving files
    for i in (1..total).step_by(2) {
        let name = format!("frag_{:04}.txt", i);
        let id = dm_reopened
            .lookup(reopened_dir_id, &name)
            .expect("lookup surviving file");
        let data = dm_reopened.read_data(id).expect("read");
        let expected = format!("File {} contents", i);
        assert_eq!(data, expected.as_bytes());
    }

    let report = dm_reopened.verify_integrity().expect("fsck");
    assert!(
        report.is_clean,
        "FSCK must be clean after defragmentation: {:?}",
        report
    );
}

#[test]
fn test_multi_block_directory_encrypted() {
    let ctx = TestContext::new("test_multi_block_enc");
    let password = "SuperSecretPassword123!";
    let dm = DiskManager::create_encrypted(&ctx.image_path, 20 * 1024 * 1024, password)
        .expect("create encrypted dm");
    let root_id = dm.superblock().root_inode;
    let dir_id = dm
        .create_directory(root_id, "secure_folder")
        .expect("create encrypted dir");

    let count = 500;
    for i in 0..count {
        let name = format!("secret_record_{:04}.json", i);
        let id = dm
            .create_file(dir_id, &name)
            .expect("create encrypted file");
        let content = format!("{{\"record\": {}}}", i);
        dm.write_data(id, 0, content.as_bytes(), CompressionMode::Never)
            .expect("write encrypted data");
    }

    // Verify lookup & decrypt
    for i in 0..count {
        let name = format!("secret_record_{:04}.json", i);
        let id = dm.lookup(dir_id, &name).expect("lookup encrypted file");
        let data = dm.read_data(id).expect("read encrypted data");
        let expected = format!("{{\"record\": {}}}", i);
        assert_eq!(data, expected.as_bytes());
    }

    // Verify list_dir returns decrypted names
    let entries = dm.list_dir(dir_id).expect("list_dir encrypted");
    assert_eq!(entries.len(), count);
    for entry in &entries {
        assert!(entry.name.starts_with("secret_record_"));
    }

    let report = dm.verify_integrity().expect("fsck encrypted");
    assert!(
        report.is_clean,
        "FSCK must pass on encrypted multi-block directory: {:?}",
        report
    );
}

#[test]
fn test_large_directory_10k_files_lookup_performance() {
    let ctx = TestContext::new("test_large_dir_10k");
    // 50MB disk image
    let dm = DiskManager::open(&ctx.image_path, 50 * 1024 * 1024).expect("open dm");
    let root_id = dm.superblock().root_inode;
    let big_dir_id = dm
        .create_directory(root_id, "mega_dir")
        .expect("create mega_dir");

    let count = 10_000;
    println!("Creating {} files in multi-block mega_dir...", count);
    let start_create = std::time::Instant::now();
    for i in 0..count {
        let name = format!("data_node_{:05}.bin", i);
        let id = dm.create_file(big_dir_id, &name).expect("create file");
        // write a small 16-byte payload
        let payload = (i as u64).to_le_bytes();
        dm.write_data(id, 0, &payload, CompressionMode::Never)
            .expect("write");
    }
    let create_dur = start_create.elapsed();
    println!("Created {} files in {:?}", count, create_dur);

    let dir_inode = dm.read_inode(big_dir_id).expect("read dir inode");
    let num_blocks = DiskManager::dir_num_blocks(&dir_inode);
    println!(
        "mega_dir spans {} blocks ({:.1} KB)",
        num_blocks,
        (num_blocks * 4096) as f64 / 1024.0
    );
    assert!(
        num_blocks >= 70,
        "10,000 files should occupy >= 70 blocks, got {}",
        num_blocks
    );

    // Measure lookup performance across 10,000 files
    let start_lookup = std::time::Instant::now();
    for i in 0..count {
        let name = format!("data_node_{:05}.bin", i);
        let id = dm.lookup(big_dir_id, &name).expect("lookup file");
        let data = dm.read_data(id).expect("read file");
        assert_eq!(data, (i as u64).to_le_bytes());
    }
    let lookup_dur = start_lookup.elapsed();
    let per_lookup_micros = lookup_dur.as_micros() as f64 / count as f64;
    println!(
        "Lookup {} files completed in {:?} ({:.2} microseconds / lookup)",
        count, lookup_dur, per_lookup_micros
    );

    // Must be ultra-fast (< 5.0 microseconds per lookup)
    assert!(
        per_lookup_micros < 10.0,
        "Lookup must be < 10us on average, got {:.2}us",
        per_lookup_micros
    );

    // Verify fsck
    let fsck = dm.verify_integrity().expect("fsck");
    assert!(
        fsck.is_clean,
        "FSCK must be clean for 10k files directory: {:?}",
        fsck
    );
}

/// Regression: a deleted directory's cached names must not resolve under a new directory that
/// reuses its inode id.
#[test]
fn test_dir_cache_not_stale_after_inode_reuse() {
    let ctx = TestContext::new("test_dir_cache_inode_reuse");
    let dm = DiskManager::open(&ctx.image_path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    let old_dir = dm.create_directory(root, "old").expect("mkdir old");
    let child = dm.create_file(old_dir, "x").expect("create x");
    assert_eq!(dm.lookup(old_dir, "x").expect("lookup x"), child); // warms old_dir's index

    // Remove the directory itself while "x" is still cached under it.
    dm.delete_file(root, "old").expect("rmdir old");

    let new_dir = dm.create_directory(root, "new").expect("mkdir new");
    assert_eq!(new_dir, old_dir, "test precondition: inode id is reused");
    assert!(
        dm.lookup(new_dir, "x").is_err(),
        "stale name resolved in reused directory"
    );
    assert!(dm.list_dir(new_dir).expect("ls").is_empty());
}

/// The complete in-memory index must stay consistent with disk across create/delete/re-create,
/// and a fresh DiskManager (cold cache) must agree with it.
#[test]
fn test_dir_index_consistency_across_mutations_and_reopen() {
    let ctx = TestContext::new("test_dir_index_consistency");
    let root;
    {
        let dm = DiskManager::open(&ctx.image_path, 20 * 1024 * 1024).expect("open dm");
        root = dm.superblock().root_inode;
        let d = dm.create_directory(root, "d").expect("mkdir");
        for i in 0..600 {
            dm.create_file(d, &format!("f{i}")).expect("create");
        }
        // Negative lookup promotes the directory to a complete index.
        assert!(dm.lookup(d, "missing").is_err());
        for i in (0..600).step_by(3) {
            dm.delete_file(d, &format!("f{i}")).expect("delete");
        }
        for i in 0..600 {
            assert_eq!(dm.lookup(d, &format!("f{i}")).is_ok(), i % 3 != 0, "f{i}");
        }
        // Deleted names can be re-created, existing ones are rejected.
        dm.create_file(d, "f0").expect("re-create f0");
        assert!(dm.create_file(d, "f1").is_err());
    }
    let dm = DiskManager::open(&ctx.image_path, 0).expect("reopen");
    let d = dm.lookup(root, "d").expect("lookup d");
    for i in 0..600 {
        let expect = i == 0 || i % 3 != 0;
        assert_eq!(dm.lookup(d, &format!("f{i}")).is_ok(), expect, "cold f{i}");
    }
    assert!(dm.verify_integrity().expect("fsck").is_clean);
}

#[test]
fn test_list_dir_cache_acceleration_p4_6() {
    let ctx = TestContext::new("test_list_dir_cache_p4_6");
    let dm = DiskManager::open(&ctx.image_path, 20 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;
    let d = dm.create_directory(root, "test_dir").expect("mkdir");

    for i in 0..50 {
        dm.create_file(d, &format!("item_{:02}.txt", i))
            .expect("create file");
    }

    // First list_dir: cold pass, reads disk blocks and promotes to complete in dir_cache
    let entries1 = dm.list_dir(d).expect("list_dir 1");
    assert_eq!(entries1.len(), 50);

    // Second list_dir: warm pass, hits complete dir_cache directly
    let entries2 = dm.list_dir(d).expect("list_dir 2");
    assert_eq!(entries2.len(), 50);

    let mut names1: Vec<_> = entries1.iter().map(|e| &e.name).collect();
    let mut names2: Vec<_> = entries2.iter().map(|e| &e.name).collect();
    names1.sort();
    names2.sort();
    assert_eq!(names1, names2);

    // Mutate: add an item, list_dir should reflect the update via cache
    dm.create_file(d, "new_item.txt").expect("create new_item");
    let entries3 = dm.list_dir(d).expect("list_dir 3");
    assert_eq!(entries3.len(), 51);
    assert!(entries3.iter().any(|e| e.name == "new_item.txt"));

    // Mutate: delete an item, list_dir should reflect the removal
    dm.delete_file(d, "item_00.txt").expect("delete item_00");
    let entries4 = dm.list_dir(d).expect("list_dir 4");
    assert_eq!(entries4.len(), 50);
    assert!(!entries4.iter().any(|e| e.name == "item_00.txt"));

    // Also test encrypted directory cache listing
    let enc_path = format!("{}.enc.img", ctx.image_path);
    let dm_enc = DiskManager::create_encrypted(&enc_path, 20 * 1024 * 1024, "P4_6_SecretPass!")
        .expect("create enc dm");
    let enc_root = dm_enc.superblock().root_inode;
    let enc_dir = dm_enc.create_directory(enc_root, "enc_dir").expect("mkdir");
    for i in 0..30 {
        dm_enc
            .create_file(enc_dir, &format!("secret_{:02}.dat", i))
            .expect("create enc file");
    }
    // Cold list_dir decrypts names and populates plain cache
    let enc_entries1 = dm_enc.list_dir(enc_dir).expect("list_dir enc 1");
    assert_eq!(enc_entries1.len(), 30);
    // Warm list_dir hits plain cache directly without repeating decryption
    let enc_entries2 = dm_enc.list_dir(enc_dir).expect("list_dir enc 2");
    assert_eq!(enc_entries2.len(), 30);
    let mut enc_names1: Vec<_> = enc_entries1.iter().map(|e| &e.name).collect();
    let mut enc_names2: Vec<_> = enc_entries2.iter().map(|e| &e.name).collect();
    enc_names1.sort();
    enc_names2.sort();
    assert_eq!(enc_names1, enc_names2);
    let _ = std::fs::remove_file(enc_path);
}

#[test]
fn test_zero_allocation_path_resolution_p4_6() {
    let ctx = TestContext::new("test_zero_alloc_paths_p4_6");
    let dm = DiskManager::open(&ctx.image_path, 20 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    // Create nested hierarchy: a/b/c/file.txt
    let a = dm.create_directory(root, "a").expect("mkdir a");
    let b = dm.create_directory(a, "b").expect("mkdir b");
    let c = dm.create_directory(b, "c").expect("mkdir c");
    let f = dm.create_file(c, "file.txt").expect("create file");

    // 1. Test resolve_path across various formats
    assert_eq!(dm.resolve_path(".").unwrap(), root);
    assert_eq!(dm.resolve_path("/.").unwrap(), root);
    assert_eq!(dm.resolve_path("a").unwrap(), a);
    assert_eq!(dm.resolve_path("/a").unwrap(), a);
    assert_eq!(dm.resolve_path("a/b").unwrap(), b);
    assert_eq!(dm.resolve_path("/a/b").unwrap(), b);
    assert_eq!(dm.resolve_path("a/b/c/file.txt").unwrap(), f);
    assert_eq!(dm.resolve_path("/a/b/c/file.txt").unwrap(), f);
    assert_eq!(dm.resolve_path("///a///b///c///file.txt///").unwrap(), f);
    assert_eq!(dm.resolve_path("a/./b/./c/file.txt").unwrap(), f);

    // 2. Test resolve_parent across various formats
    let (p1, name1) = dm.resolve_parent("simple.txt").unwrap();
    assert_eq!(p1, root);
    assert_eq!(name1, "simple.txt");

    let (p2, name2) = dm.resolve_parent("/simple.txt").unwrap();
    assert_eq!(p2, root);
    assert_eq!(name2, "simple.txt");

    let (p3, name3) = dm.resolve_parent("a/b/c/file.txt").unwrap();
    assert_eq!(p3, c);
    assert_eq!(name3, "file.txt");

    let (p4, name4) = dm.resolve_parent("/a/b/c/file.txt").unwrap();
    assert_eq!(p4, c);
    assert_eq!(name4, "file.txt");

    let (p5, name5) = dm.resolve_parent("a/b/c/file.txt///").unwrap();
    assert_eq!(p5, c);
    assert_eq!(name5, "file.txt");

    // Trailing dot edge case
    let (p6, name6) = dm.resolve_parent("a/b/c/file.txt/.").unwrap();
    assert_eq!(p6, c);
    assert_eq!(name6, "file.txt");

    // Invalid / empty inputs
    assert!(dm.resolve_parent("").is_err());
    assert!(dm.resolve_parent("/").is_err());
    assert!(dm.resolve_parent("///").is_err());
    assert!(dm.resolve_parent(".").is_err());
    assert!(dm.resolve_parent("./.").is_err());
}
