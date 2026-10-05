//! High-Stress Concurrency Test for OIFS under ThreadSanitizer (TSan)
//!
//! Spawns multiple threads simultaneously performing reads, writes, flushes,
//! and FSCK integrity checks on an OIFS image to detect data races or memory corruption.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use oifs::disk::{CompressionMode, DiskManager};

#[test]
fn test_stress_multithread_read_write_integrity() {
    let img_path = "stress_rw_tsan.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    // 10MB image
    let dm = DiskManager::open(img_path, 10 * 1024 * 1024).expect("Open failed");
    let dm = Arc::new(dm);
    let root = dm.resolve_path(".").unwrap();

    let num_threads = 6;
    let iterations_per_thread = 20;
    let chunk_size = 2048; // 2KB

    let mut handles = Vec::new();

    // Spawn writer threads
    for t_id in 0..num_threads {
        let dm_clone = dm.clone();
        let handle = thread::spawn(move || {
            let filename = format!("worker_file_{}.dat", t_id);
            let inode_id = dm_clone
                .create_file(root, &filename)
                .expect("Create failed");

            let mut expected_bytes = Vec::with_capacity(iterations_per_thread * chunk_size);

            for iter in 0..iterations_per_thread {
                let pattern_byte = ((t_id * 31 + iter) % 251 + 1) as u8;
                let chunk = vec![pattern_byte; chunk_size];
                let offset = (iter * chunk_size) as u64;

                dm_clone
                    .write_data(inode_id, offset, &chunk, CompressionMode::Auto)
                    .expect("Write failed");

                expected_bytes.extend_from_slice(&chunk);

                // Periodic flush
                if iter % 5 == 0 {
                    let _ = dm_clone.flush();
                }

                // Verify self-read consistency
                let current_content = dm_clone.read_data(inode_id).expect("Read failed");
                assert!(
                    current_content.len() >= expected_bytes.len(),
                    "T{} iter {}: length mismatch: actual {} vs expected {}",
                    t_id,
                    iter,
                    current_content.len(),
                    expected_bytes.len()
                );
                assert_eq!(
                    &current_content[..expected_bytes.len()],
                    &expected_bytes[..],
                    "T{} iter {}: data content mismatch",
                    t_id,
                    iter
                );
            }

            (filename, expected_bytes)
        });
        handles.push(handle);
    }

    let mut results = Vec::new();
    for h in handles {
        results.push(h.join().unwrap());
    }

    // Final verification across all files
    for (filename, expected) in results {
        let inode_id = dm.lookup(root, &filename).expect("File missing after test");
        let actual = dm.read_data(inode_id).expect("Final read failed");
        assert_eq!(actual.len(), expected.len());
        assert_eq!(
            actual, expected,
            "Mismatch on final integrity of {}",
            filename
        );
    }

    // Run FSCK to ensure structural integrity
    let report = dm.verify_integrity().expect("FSCK failed");
    assert!(
        report.is_clean,
        "FSCK reported errors after concurrent test: {:?}",
        report
    );

    drop(dm);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_stress_concurrent_mutations_with_live_fsck() {
    let img_path = "stress_fsck_tsan.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let dm = DiskManager::open(img_path, 8 * 1024 * 1024).expect("Open failed");
    let dm = Arc::new(dm);
    let root = dm.resolve_path(".").unwrap();

    let stop_flag = Arc::new(AtomicBool::new(false));

    // Background thread: continuously runs fsck while mutations happen
    let dm_fsck = dm.clone();
    let stop_fsck = stop_flag.clone();
    let fsck_handle = thread::spawn(move || {
        let mut checks = 0;
        while !stop_fsck.load(Ordering::Relaxed) {
            let report = dm_fsck.verify_integrity().expect("Concurrent fsck failed");
            assert!(
                report.is_clean,
                "FSCK detected filesystem corruption during active mutations!"
            );
            checks += 1;
            thread::sleep(Duration::from_millis(5));
        }
        checks
    });

    // Worker threads creating and deleting files concurrently
    let mut workers = Vec::new();
    for w_id in 0..4 {
        let dm_worker = dm.clone();
        let h = thread::spawn(move || {
            for i in 0..15 {
                let fname = format!("temp_{}_{}.tmp", w_id, i);
                let inode = dm_worker
                    .create_file(root, &fname)
                    .expect("Create tmp failed");
                let data = vec![(w_id + i) as u8; 1024];
                dm_worker
                    .write_data(inode, 0, &data, CompressionMode::Never)
                    .expect("Write tmp failed");

                // Read back
                let read = dm_worker.read_data(inode).expect("Read tmp failed");
                assert_eq!(read, data);

                // Delete half of them
                if i % 2 == 0 {
                    dm_worker
                        .delete_file(root, &fname)
                        .expect("Delete tmp failed");
                }
            }
        });
        workers.push(h);
    }

    for w in workers {
        w.join().unwrap();
    }

    stop_flag.store(true, Ordering::Relaxed);
    let total_fsck_runs = fsck_handle.join().unwrap();

    // Final clean check
    let final_report = dm.verify_integrity().expect("Final fsck failed");
    assert!(final_report.is_clean);
    assert!(total_fsck_runs > 0);

    drop(dm);
    let _ = fs::remove_file(img_path);
}

/// Edge Case 3: Large File Single-Indirect Block Expansion under High Concurrency.
/// Direct blocks only cover the first 10 blocks (40KB). Writing beyond 40KB triggers
/// single indirect block allocation (block 10).
/// 4 threads concurrently expand files across the direct/indirect boundary to 80KB (20 blocks),
/// testing indirect table allocation safety, pointer zeroing, and block indexing under TSan.
#[test]
fn test_stress_large_file_indirect_block_expansion() {
    let img_path = "stress_indirect_tsan.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    // 16MB image
    let dm = DiskManager::open(img_path, 16 * 1024 * 1024).expect("Open failed");
    let dm = Arc::new(dm);
    let root = dm.resolve_path(".").unwrap();

    let num_workers = 4;
    let target_blocks = 20; // 20 * 4KB = 80KB (crosses the 10-block 40KB boundary)
    let block_size = 4096;

    let mut workers = Vec::new();
    for w in 0..num_workers {
        let dm_worker = dm.clone();
        let h = thread::spawn(move || {
            let fname = format!("large_indirect_{}.bin", w);
            let inode = dm_worker.create_file(root, &fname).expect("Create failed");

            let mut written_bytes = Vec::with_capacity(target_blocks * block_size);

            for b in 0..target_blocks {
                let pattern = ((w * 17 + b) % 255) as u8;
                let chunk = vec![pattern; block_size];
                let offset = (b * block_size) as u64;

                dm_worker
                    .write_data(inode, offset, &chunk, CompressionMode::Never)
                    .expect("Write block failed");

                written_bytes.extend_from_slice(&chunk);
            }

            // Verify integrity of read-back
            let read_back = dm_worker.read_data(inode).expect("Read failed");
            assert_eq!(read_back.len(), written_bytes.len());
            assert_eq!(read_back, written_bytes);

            (fname, written_bytes)
        });
        workers.push(h);
    }

    let mut results = Vec::new();
    for w in workers {
        results.push(w.join().unwrap());
    }

    // Cross-verify all files
    for (fname, expected) in results {
        let inode = dm.lookup(root, &fname).expect("Lookup failed");
        let data = dm.read_data(inode).expect("Read failed");
        assert_eq!(data, expected, "Mismatch in {}", fname);
    }

    let report = dm.verify_integrity().expect("Integrity check failed");
    assert!(
        report.is_clean,
        "FSCK dirty after indirect block test: {:?}",
        report
    );

    drop(dm);
    let _ = fs::remove_file(img_path);
}

/// Edge Case 4: Concurrent Disjoint Slices Write to the Same File.
/// 4 threads simultaneously write to non-overlapping 16KB sections of a shared 64KB file:
/// - T0: [0..16KB]
/// - T1: [16KB..32KB]
/// - T2: [32KB..48KB]
/// - T3: [48KB..64KB]
///
/// Monitors under TSan that disjoint block assignments to the same inode do not cause data races.
#[test]
fn test_stress_concurrent_disjoint_slices_same_file() {
    let img_path = "stress_disjoint_tsan.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let dm = DiskManager::open(img_path, 8 * 1024 * 1024).expect("Open failed");
    let dm = Arc::new(dm);
    let root = dm.resolve_path(".").unwrap();
    let shared_inode = dm
        .create_file(root, "disjoint_shared.dat")
        .expect("Create shared file failed");

    let slice_size = 16 * 1024; // 16KB (4 blocks)
    let num_threads = 4;

    let mut handles = Vec::new();
    for t in 0..num_threads {
        let dm_t = dm.clone();
        let h = thread::spawn(move || {
            let offset = (t * slice_size) as u64;
            let pattern = (0x10 + t as u8) * 3;
            let chunk = vec![pattern; slice_size];

            dm_t.write_data(shared_inode, offset, &chunk, CompressionMode::Never)
                .expect("Disjoint write failed");
        });
        handles.push(h);
    }

    for h in handles {
        h.join().unwrap();
    }

    // Verify entire 64KB content
    let full_data = dm
        .read_data(shared_inode)
        .expect("Read full shared data failed");
    assert_eq!(full_data.len(), num_threads * slice_size);

    for t in 0..num_threads {
        let start = t * slice_size;
        let end = start + slice_size;
        let expected_pattern = (0x10 + t as u8) * 3;
        assert!(
            full_data[start..end].iter().all(|&b| b == expected_pattern),
            "Slice {} corrupted: expected 0x{:02X}",
            t,
            expected_pattern
        );
    }

    let report = dm.verify_integrity().expect("Integrity check failed");
    assert!(report.is_clean);

    drop(dm);
    let _ = fs::remove_file(img_path);
}
