//! Shuttle-based Controlled Concurrency Testing for OIFS
//!
//! Uses Shuttle's randomized scheduler to explore thread interleavings,
//! detecting race conditions, data corruption, and deadlocks in OIFS DiskManager.

use oifs::disk::{CompressionMode, DiskManager};
use shuttle::thread;
use std::sync::Arc;
use tempfile::tempdir;

/// Test 1: Multiple threads concurrently creating files in the same directory.
/// Verifies directory block allocation, inode allocation, and lookup consistency
/// under randomized thread interleavings.
#[test]
fn test_shuttle_concurrent_file_creation() {
    shuttle::check_random(|| {
        let dir = tempdir().unwrap();
        let img_path = dir.path().join("shuttle_create.img");

        let dm = DiskManager::open(&img_path, 2 * 1024 * 1024).expect("Failed to create image");
        let dm = Arc::new(dm);

        let mut handles = Vec::new();
        for i in 0..3 {
            let dm_clone = dm.clone();
            let h = thread::spawn(move || {
                let root = dm_clone.resolve_path(".").unwrap();
                let name = format!("file_{}.txt", i);
                let inode_id = dm_clone.create_file(root, &name).expect("Create file failed");
                
                // Write small content
                let data = format!("content-{}", i).into_bytes();
                dm_clone.write_data(inode_id, 0, &data, CompressionMode::Never).expect("Write failed");
            });
            handles.push(h);
        }

        for h in handles {
            h.join().unwrap();
        }

        // Verify all 3 files exist and have exact content
        let root = dm.resolve_path(".").unwrap();
        for i in 0..3 {
            let name = format!("file_{}.txt", i);
            let inode_id = dm.lookup(root, &name).expect("Lookup failed");
            let data = dm.read_data(inode_id).expect("Read failed");
            assert_eq!(data, format!("content-{}", i).into_bytes());
        }

        dm.flush().unwrap();
    }, 50);
}

/// Test 2: Concurrent writer and reader on the same file.
/// Verifies that read_data never observes torn writes or corrupted state.
#[test]
fn test_shuttle_concurrent_reader_writer() {
    shuttle::check_random(|| {
        let dir = tempdir().unwrap();
        let img_path = dir.path().join("shuttle_rw.img");

        let dm = DiskManager::open(&img_path, 2 * 1024 * 1024).expect("Failed to create image");
        let root = dm.resolve_path(".").unwrap();
        let inode_id = dm.create_file(root, "shared.txt").expect("Create shared file failed");
        let dm = Arc::new(dm);

        let dm_writer = dm.clone();
        let writer_h = thread::spawn(move || {
            let payload1 = vec![0xAA; 512];
            dm_writer.write_data(inode_id, 0, &payload1, CompressionMode::Never).expect("Write 1 failed");

            let payload2 = vec![0xBB; 512];
            dm_writer.write_data(inode_id, 512, &payload2, CompressionMode::Never).expect("Write 2 failed");
        });

        let dm_reader = dm.clone();
        let reader_h = thread::spawn(move || {
            let read_data = dm_reader.read_data(inode_id).expect("Read failed");
            // The reader must see either:
            // - 0 bytes (not yet written)
            // - 512 bytes of 0xAA (after write 1)
            // - 1024 bytes of 0xAA..0xAA + 0xBB..0xBB (after write 2)
            match read_data.len() {
                0 => {}
                512 => {
                    assert!(read_data.iter().all(|&b| b == 0xAA));
                }
                1024 => {
                    assert!(read_data[..512].iter().all(|&b| b == 0xAA));
                    assert!(read_data[512..].iter().all(|&b| b == 0xBB));
                }
                other => panic!("Unexpected intermediate file length: {}", other),
            }
        });

        writer_h.join().unwrap();
        reader_h.join().unwrap();

        // Final check
        let final_data = dm.read_data(inode_id).expect("Final read failed");
        assert_eq!(final_data.len(), 1024);
        assert!(final_data[..512].iter().all(|&b| b == 0xAA));
        assert!(final_data[512..].iter().all(|&b| b == 0xBB));
    }, 50);
}

/// Test 3: Concurrent create and delete race.
/// Ensures that concurrent create/delete operations maintain inode and directory table integrity without deadlocks.
#[test]
fn test_shuttle_concurrent_create_delete() {
    shuttle::check_random(|| {
        let dir = tempdir().unwrap();
        let img_path = dir.path().join("shuttle_cd.img");

        let dm = DiskManager::open(&img_path, 2 * 1024 * 1024).expect("Failed to create image");
        let dm = Arc::new(dm);

        let dm1 = dm.clone();
        let h1 = thread::spawn(move || {
            let root = dm1.resolve_path(".").unwrap();
            let _ = dm1.create_file(root, "race_target.txt");
        });

        let dm2 = dm.clone();
        let h2 = thread::spawn(move || {
            let root = dm2.resolve_path(".").unwrap();
            // Either the file exists and delete succeeds, or it doesn't and returns NotFound
            let _ = dm2.delete_file(root, "race_target.txt");
        });

        h1.join().unwrap();
        h2.join().unwrap();

        // System must remain healthy and able to create another file
        let root = dm.resolve_path(".").unwrap();
        let sentinel_id = dm.create_file(root, "sentinel.txt").expect("Post-race create failed");
        assert!(dm.lookup(root, "sentinel.txt").is_ok());
        let _ = sentinel_id;

        let report = dm.verify_integrity().expect("Integrity check failed");
        assert!(report.is_clean);
    }, 50);
}

/// Edge Case 4: Overlapping Offset Write Race on the Same File.
/// 3 threads concurrently write overlapping regions:
/// - T1: [0..1024] = 0x11
/// - T2: [512..1536] = 0x22
/// - T3: [1024..2048] = 0x33
/// Checks that non-overlapping portions are pure, overlapping portions are either value,
/// and no byte is uninitialized or torn.
#[test]
fn test_shuttle_overlapping_offset_writes() {
    shuttle::check_random(|| {
        let dir = tempdir().unwrap();
        let img_path = dir.path().join("shuttle_overlap.img");

        let dm = DiskManager::open(&img_path, 2 * 1024 * 1024).expect("Failed to create image");
        let root = dm.resolve_path(".").unwrap();
        let file_id = dm.create_file(root, "overlap.dat").expect("Create failed");
        let dm = Arc::new(dm);

        let dm1 = dm.clone();
        let h1 = thread::spawn(move || {
            let buf = vec![0x11u8; 1024];
            dm1.write_data(file_id, 0, &buf, CompressionMode::Never).expect("Write 1 failed");
        });

        let dm2 = dm.clone();
        let h2 = thread::spawn(move || {
            let buf = vec![0x22u8; 1024];
            dm2.write_data(file_id, 512, &buf, CompressionMode::Never).expect("Write 2 failed");
        });

        let dm3 = dm.clone();
        let h3 = thread::spawn(move || {
            let buf = vec![0x33u8; 1024];
            dm3.write_data(file_id, 1024, &buf, CompressionMode::Never).expect("Write 3 failed");
        });

        h1.join().unwrap();
        h2.join().unwrap();
        h3.join().unwrap();

        let data = dm.read_data(file_id).expect("Read failed");
        assert_eq!(data.len(), 2048, "File size must be 2048");

        // Region 1: 0..512 is exclusively written by T1
        assert!(data[..512].iter().all(|&b| b == 0x11), "Exclusive T1 range corrupted");

        // Region 2: 512..1024 is overlap of T1 and T2
        let overlap1_val = data[512];
        assert!(overlap1_val == 0x11 || overlap1_val == 0x22, "Invalid overlap 1 byte");
        assert!(data[512..1024].iter().all(|&b| b == overlap1_val), "Torn write in overlap 1");

        // Region 3: 1024..1536 is overlap of T2 and T3
        let overlap2_val = data[1024];
        assert!(overlap2_val == 0x22 || overlap2_val == 0x33, "Invalid overlap 2 byte");
        assert!(data[1024..1536].iter().all(|&b| b == overlap2_val), "Torn write in overlap 2");

        // Region 4: 1536..2048 is exclusively written by T3
        assert!(data[1536..2048].iter().all(|&b| b == 0x33), "Exclusive T3 range corrupted");

        let report = dm.verify_integrity().expect("Integrity check failed");
        assert!(report.is_clean);
    }, 40);
}

/// Edge Case 5: Concurrent Compression & Decompression Race.
/// Thread 1 writes a 16KB compressible payload using CompressionMode::Always.
/// Thread 2 concurrently attempts to read and decompress the file.
/// Verifies that decompression either sees 0 bytes (not yet written) or full decompressed payload,
/// never corrupted zstd frames or partial reads.
#[test]
fn test_shuttle_concurrent_compression_and_decompression_race() {
    shuttle::check_random(|| {
        let dir = tempdir().unwrap();
        let img_path = dir.path().join("shuttle_compress.img");

        let dm = DiskManager::open(&img_path, 2 * 1024 * 1024).expect("Failed to create image");
        let dm = Arc::new(dm);
        let root = dm.resolve_path(".").unwrap();
        let file_id = dm.create_file(root, "compressed.dat").expect("Create failed");

        let dm_writer = dm.clone();
        let writer_h = thread::spawn(move || {
            // Highly compressible payload: repeated pattern of 16KB
            let mut payload = Vec::with_capacity(16384);
            for i in 0..16384 {
                payload.push((i % 16) as u8);
            }
            dm_writer
                .write_data(file_id, 0, &payload, CompressionMode::Always)
                .expect("Compressed write failed");
        });

        let dm_reader = dm.clone();
        let reader_h = thread::spawn(move || {
            let data = dm_reader.read_data(file_id).expect("Read failed");
            if !data.is_empty() {
                assert_eq!(data.len(), 16384, "Decompressed length mismatch");
                for i in 0..16384 {
                    assert_eq!(data[i], (i % 16) as u8);
                }
            }
        });

        writer_h.join().unwrap();
        reader_h.join().unwrap();

        // Final verification
        let final_data = dm.read_data(file_id).expect("Final read failed");
        assert_eq!(final_data.len(), 16384);

        let report = dm.verify_integrity().expect("Integrity check failed");
        assert!(report.is_clean);
    }, 40);
}

/// Edge Case 6: Nested Directory Creation and Traversal Race.
/// Concurrent threads racing to create nested directories and subfiles:
/// - T1 creates `dir_a` and `dir_a/file_a.txt`
/// - T2 creates `dir_b` and `dir_b/file_b.txt`
/// - T3 queries `resolve_path` for `dir_a/file_a.txt` and `dir_b/file_b.txt`
#[test]
fn test_shuttle_nested_directory_and_path_resolution_race() {
    shuttle::check_random(|| {
        let dir = tempdir().unwrap();
        let img_path = dir.path().join("shuttle_nested.img");

        let dm = DiskManager::open(&img_path, 2 * 1024 * 1024).expect("Failed to create image");
        let dm = Arc::new(dm);
        let root = dm.resolve_path(".").unwrap();

        let dm1 = dm.clone();
        let h1 = thread::spawn(move || {
            let dir_a = dm1.create_directory(root, "dir_a").expect("create dir_a failed");
            let file_a = dm1.create_file(dir_a, "file_a.txt").expect("create file_a failed");
            dm1.write_data(file_a, 0, b"Hello A", CompressionMode::Never).expect("write file_a failed");
        });

        let dm2 = dm.clone();
        let h2 = thread::spawn(move || {
            let dir_b = dm2.create_directory(root, "dir_b").expect("create dir_b failed");
            let file_b = dm2.create_file(dir_b, "file_b.txt").expect("create file_b failed");
            dm2.write_data(file_b, 0, b"Hello B", CompressionMode::Never).expect("write file_b failed");
        });

        let dm3 = dm.clone();
        let h3 = thread::spawn(move || {
            // Concurrently query resolution - might fail if not created yet, but should never panic or corrupt
            let _ = dm3.resolve_path("dir_a/file_a.txt");
            let _ = dm3.resolve_path("dir_b/file_b.txt");
        });

        h1.join().unwrap();
        h2.join().unwrap();
        h3.join().unwrap();

        // Final verification: both files MUST resolve and read correctly
        let inode_a = dm.resolve_path("dir_a/file_a.txt").expect("Failed to resolve dir_a/file_a.txt");
        let inode_b = dm.resolve_path("dir_b/file_b.txt").expect("Failed to resolve dir_b/file_b.txt");

        assert_eq!(dm.read_data(inode_a).unwrap(), b"Hello A");
        assert_eq!(dm.read_data(inode_b).unwrap(), b"Hello B");

        let report = dm.verify_integrity().expect("Integrity check failed");
        assert!(report.is_clean);
    }, 40);
}
