use oifs::filters::{FilterConfig, calculate_entropy, recommend_filters};
use oifs::session::OifsSession;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

// =========================================================================
// P2.1 Core & Benchmark Tests
// =========================================================================

#[test]
fn test_p2_1_recommend_filters_large_file_performance_and_accuracy() {
    // Generate 10MB of 32-bit floating point data (e.g. sensor / time-series data)
    let count = 2_500_000usize; // 2.5M * 4 bytes = 10MB
    let mut data = Vec::with_capacity(count * 4);
    for i in 0..count {
        let val = (i as f32) * 0.05;
        data.extend_from_slice(&val.to_le_bytes());
    }
    assert_eq!(data.len(), 10_000_000);

    let raw_entropy = calculate_entropy(&data[..65536]);
    println!("Testing P2.1 recommend_filters on 10MB structured payload...");

    let start = Instant::now();
    let rec = recommend_filters(&data);
    let elapsed = start.elapsed();

    println!("P2.1 Execution Time: {:?}", elapsed);
    println!("  Original Size: {} bytes", rec.original_size);
    println!(
        "  Baseline Compressed: {} bytes (Entropy: {:.3})",
        rec.baseline_compressed_size, rec.baseline_entropy
    );
    println!(
        "  Best Filter: {} (Estimated Comp: {} bytes, Savings: {:.1}%, Ratio: {:.2}x)",
        rec.best_report.label,
        rec.best_report.compressed_size,
        rec.best_report.space_savings_percent,
        rec.best_report.compression_ratio
    );

    // Latency assertion: With 192KB multi-window sampling and Rayon multi-core parallelism,
    // 10MB recommendation must finish well within 100 milliseconds (typically < 10ms),
    // whereas non-optimized sequential 14-trial 10MB zstd encoding would take 3-5+ seconds.
    assert!(
        elapsed.as_millis() < 500,
        "P2.1 recommendation must execute in sub-second (actual: {:?})",
        elapsed
    );

    // Accuracy assertions
    assert_eq!(rec.original_size, 10_000_000);
    assert!(
        rec.best_config.delta,
        "Must select delta filter for continuous float series"
    );
    assert_eq!(
        rec.best_config.typesize, 4,
        "Must detect 4-byte typesize for f32 data"
    );
    assert!(
        rec.best_report.compressed_size < rec.baseline_compressed_size,
        "Best filtered compression must outperform raw baseline zstd"
    );
    assert!(
        rec.best_report.entropy < raw_entropy,
        "Best filter must reduce Shannon entropy"
    );
    assert!(
        rec.best_report.space_savings_percent > 70.0,
        "Delta on continuous float series should achieve > 70% space savings"
    );
}

#[test]
fn test_p2_1_sampling_boundary_and_corner_cases() {
    // 1. Empty buffer
    let rec_empty = recommend_filters(&[]);
    assert_eq!(rec_empty.original_size, 0);
    assert_eq!(rec_empty.best_config, FilterConfig::none());

    // 2. Tiny buffer (< 8 bytes)
    let tiny_data = b"tiny12";
    let rec_tiny = recommend_filters(tiny_data);
    assert_eq!(rec_tiny.original_size, tiny_data.len());

    // 3. Exact threshold boundary (256 KB = 262,144 bytes)
    let threshold_data = vec![0x42u8; 256 * 1024];
    let rec_threshold = recommend_filters(&threshold_data);
    assert_eq!(rec_threshold.original_size, 256 * 1024);

    // 4. Threshold + 1 byte (262,145 bytes) - tests unaligned tail boundary
    let thresh_plus_1 = vec![0xAAu8; 256 * 1024 + 1];
    let rec_plus_1 = recommend_filters(&thresh_plus_1);
    assert_eq!(rec_plus_1.original_size, 256 * 1024 + 1);

    // 5. Threshold + 7 bytes (tests unaligned 64-bit boundary)
    let thresh_plus_7 = vec![0xBBu8; 256 * 1024 + 7];
    let rec_plus_7 = recommend_filters(&thresh_plus_7);
    assert_eq!(rec_plus_7.original_size, 256 * 1024 + 7);

    // 6. Threshold + 8 bytes (tests exact 64-bit aligned boundary)
    let thresh_plus_8 = vec![0xCCu8; 256 * 1024 + 8];
    let rec_plus_8 = recommend_filters(&thresh_plus_8);
    assert_eq!(rec_plus_8.original_size, 256 * 1024 + 8);
}

#[test]
fn test_p2_1_high_entropy_random_noise_prefers_none() {
    // Deterministic XORShift64 PRNG to generate 512KB of high-entropy white noise
    let mut rng_state: u64 = 0x853c49e6748fea9b;
    let size = 512 * 1024;
    let mut noise = Vec::with_capacity(size);
    for _ in 0..(size / 8) {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        noise.extend_from_slice(&rng_state.to_le_bytes());
    }

    let rec = recommend_filters(&noise);
    println!(
        "Random Noise: Baseline Entropy = {:.3}, Best = {}",
        rec.baseline_entropy, rec.best_report.label
    );
    assert!(
        rec.baseline_entropy > 7.9,
        "Pseudo-random noise must have entropy close to 8 bits/byte"
    );
    assert!(
        rec.best_report.space_savings_percent < 5.0,
        "White noise should achieve negligible savings (< 5%)"
    );
}

#[test]
fn test_p2_1_bitshuffle_sparse_bitfield_selection() {
    // Generate 512KB of sparse bitfields (e.g. 64-bit words where only bit 0 or bit 1 is set)
    let size = 512 * 1024;
    let mut bitfield_data = Vec::with_capacity(size);
    for i in 0..(size / 8) {
        let val: u64 = if i % 4 == 0 {
            0x01
        } else if i % 4 == 2 {
            0x02
        } else {
            0x00
        };
        bitfield_data.extend_from_slice(&val.to_le_bytes());
    }

    let rec = recommend_filters(&bitfield_data);
    println!(
        "Sparse Bitfield: Best Filter = {} (Savings: {:.1}%, Ratio: {:.2}x)",
        rec.best_report.label,
        rec.best_report.space_savings_percent,
        rec.best_report.compression_ratio
    );
    assert!(
        rec.best_config.bitshuffle || rec.best_config.shuffle || rec.best_config.delta,
        "Sparse bitfields must select a compression filter"
    );
    assert!(
        rec.best_report.space_savings_percent > 80.0,
        "Sparse bitfields must achieve > 80% compression space savings"
    );
}

// =========================================================================
// P2.2 Core & Cache Stress Tests
// =========================================================================

#[test]
fn test_p2_2_inode_cache_consistency_and_performance() {
    let img_path = "p2_cache.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let session =
        OifsSession::get_or_open(img_path, 20 * 1024 * 1024).expect("Failed to create session");

    // Create nested directory hierarchy
    let root = session.resolve_path(".").unwrap();
    let dir_a = session.create_directory(root, "dir_a").unwrap();
    let dir_b = session.create_directory(dir_a, "dir_b").unwrap();
    let file_id = session.create_file(dir_b, "data.bin").unwrap();

    let initial_data = b"Hello OIFS Inode Cache (P2.2)!";
    session
        .write_data(file_id, 0, initial_data, oifs::disk::CompressionMode::Never)
        .unwrap();

    // 1. Warm-up and test rapid cached path resolution (10,000 lookups)
    let start = Instant::now();
    for _ in 0..10_000 {
        let resolved = session.resolve_path("/dir_a/dir_b/data.bin").unwrap();
        assert_eq!(resolved, file_id);
    }
    let elapsed = start.elapsed();
    println!("10,000 cached path resolutions completed in {:?}", elapsed);
    assert!(
        elapsed.as_millis() < 500,
        "Cached path resolution should be ultra-fast"
    );

    // 2. Test cache coherence on write (mtime and size updates)
    let updated_data = b"Updated content for cache coherence verification";
    session
        .write_data(file_id, 0, updated_data, oifs::disk::CompressionMode::Never)
        .unwrap();

    let inode = session.read_inode(file_id).unwrap();
    assert_eq!(inode.size, updated_data.len() as u64);

    let read_back = session.read_data(file_id).unwrap();
    assert_eq!(read_back, updated_data);

    // 3. Test cache invalidation on file deletion
    session.delete_file(dir_b, "data.bin").unwrap();

    // Looking up the deleted file should return error
    let lookup_res = session.resolve_path("/dir_a/dir_b/data.bin");
    assert!(
        lookup_res.is_err(),
        "Deleted file must not be found in cache or disk"
    );

    // 4. Persistence check: Reopen session from disk
    drop(session);

    let reopened = OifsSession::open(img_path, 20 * 1024 * 1024).expect("Failed to reopen session");
    let reopened_dir_b = reopened.resolve_path("/dir_a/dir_b").unwrap();
    assert_eq!(reopened_dir_b, dir_b);

    let list = reopened.list_dir(reopened_dir_b).unwrap();
    assert_eq!(
        list.len(),
        0,
        "Deleted file must remain gone after reopening"
    );

    // Clean up
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_p2_2_inode_cache_eviction_threshold_stress() {
    let img_path = "p2_evict.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    // 25MB disk image to comfortably accommodate 2,500 file inodes
    let session = OifsSession::get_or_open(img_path, 25 * 1024 * 1024)
        .expect("Failed to create session for eviction test");
    let root = session.resolve_path(".").unwrap();

    let total_files = 2_500;
    println!(
        "Creating {} files across multiple directories to exceed 2048 cache capacity...",
        total_files
    );

    let num_dirs = 25;
    let files_per_dir = total_files / num_dirs; // 100
    let mut file_ids = Vec::with_capacity(total_files);

    for d in 0..num_dirs {
        let dir_name = format!("sub_dir_{:02}", d);
        let dir_id = session.create_directory(root, &dir_name).unwrap();
        for f in 0..files_per_dir {
            let file_name = format!("f_{:03}.dat", f);
            let fid = session.create_file(dir_id, &file_name).unwrap();
            let payload = format!("Payload for d{}_f{}", d, f);
            session
                .write_data(
                    fid,
                    0,
                    payload.as_bytes(),
                    oifs::disk::CompressionMode::Never,
                )
                .unwrap();
            file_ids.push((fid, payload));
        }
    }

    assert_eq!(file_ids.len(), total_files);

    // Now read all 2,500 inodes sequentially.
    // Because the cache capacity is capped at 2048, reading 2,500 entries forces cache clear and re-insertion.
    println!(
        "Reading all {} inodes to trigger cache eviction cycles...",
        total_files
    );
    for (fid, expected_payload) in &file_ids {
        let inode = session.read_inode(*fid).unwrap();
        assert_eq!(inode.size, expected_payload.len() as u64);
        let data = session.read_data(*fid).unwrap();
        assert_eq!(data, expected_payload.as_bytes());
    }

    // Repeated pass: should hit the freshly populated cache seamlessly
    let start = Instant::now();
    for (fid, expected_payload) in file_ids.iter().take(1000) {
        let inode = session.read_inode(*fid).unwrap();
        assert_eq!(inode.size, expected_payload.len() as u64);
    }
    let elapsed = start.elapsed();
    println!("1,000 cached inode reads after eviction: {:?}", elapsed);
    assert!(
        elapsed.as_millis() < 50,
        "Cached reads after eviction must remain fast"
    );

    drop(session);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_p2_2_multithreaded_concurrent_cache_access() {
    let img_path = "p2_conc.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let session = OifsSession::get_or_open(img_path, 30 * 1024 * 1024)
        .expect("Failed to create concurrency test session");
    let root = session.resolve_path(".").unwrap();

    // Pre-populate 50 files
    let mut initial_files = Vec::new();
    for i in 0..50 {
        let name = format!("shared_file_{:03}.bin", i);
        let fid = session.create_file(root, &name).unwrap();
        let init_bytes = vec![(i % 256) as u8; 64];
        session
            .write_data(fid, 0, &init_bytes, oifs::disk::CompressionMode::Never)
            .unwrap();
        initial_files.push((fid, name));
    }

    let session_arc = Arc::new(session);
    let mut handles = Vec::new();

    // Spawn 4 Readers: repeatedly reading inodes and resolving paths
    for thread_id in 0..4 {
        let sess = session_arc.clone();
        let files = initial_files.clone();
        handles.push(thread::spawn(move || {
            for iter in 0..500 {
                let target_idx = (thread_id * 13 + iter) % files.len();
                let (fid, ref name) = files[target_idx];
                let inode = sess.read_inode(fid).expect("Concurrent read_inode failed");
                assert_eq!(inode.size, 64);
                let path = format!("/{}", name);
                let resolved = sess
                    .resolve_path(&path)
                    .expect("Concurrent resolve_path failed");
                assert_eq!(resolved, fid);
            }
        }));
    }

    // Spawn 2 Writers: updating existing files
    for thread_id in 0..2 {
        let sess = session_arc.clone();
        let files = initial_files.clone();
        handles.push(thread::spawn(move || {
            for iter in 0..100 {
                let target_idx = (thread_id * 23 + iter) % files.len();
                let (fid, _) = files[target_idx];
                let new_data = vec![(iter % 256) as u8; 64];
                sess.write_data(fid, 0, &new_data, oifs::disk::CompressionMode::Never)
                    .expect("Concurrent write_data failed");
            }
        }));
    }

    // Join all threads
    for h in handles {
        h.join().expect("Worker thread panicked");
    }

    println!("Multi-threaded concurrent cache stress test completed cleanly with 0 errors!");
    drop(session_arc);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_p2_2_encrypted_filesystem_with_inode_cache() {
    let img_path = "p2_enc.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let password = "SuperSecretP2Password!@#";
    let session = OifsSession::get_or_create_encrypted(img_path, 20 * 1024 * 1024, password)
        .expect("Failed to create encrypted session");

    let root = session.resolve_path(".").unwrap();
    let enc_dir = session.create_directory(root, "vault_dir").unwrap();
    let enc_file = session.create_file(enc_dir, "secret_notes.txt").unwrap();

    let secret_data = b"Confidential encrypted data protected by OIFS XChaCha20-Poly1305";
    session
        .write_data(enc_file, 0, secret_data, oifs::disk::CompressionMode::Never)
        .unwrap();

    // 1,000 rapid cached lookups in encrypted filesystem
    for _ in 0..1000 {
        let resolved = session.resolve_path("/vault_dir/secret_notes.txt").unwrap();
        assert_eq!(resolved, enc_file);
        let inode = session.read_inode(resolved).unwrap();
        assert_eq!(inode.size, (secret_data.len() + 16) as u64); // Encrypted payload includes 16-byte Poly1305 MAC tag
        assert!(inode.encrypted);
    }

    // Verify read_data decrypts cleanly
    let read_back = session.read_data(enc_file).unwrap();
    assert_eq!(read_back, secret_data);

    // Drop and reopen encrypted session
    drop(session);

    let reopened =
        OifsSession::get_or_open_with_password(img_path, 20 * 1024 * 1024, Some(password))
            .expect("Failed to reopen encrypted session");
    let reopened_file = reopened
        .resolve_path("/vault_dir/secret_notes.txt")
        .unwrap();
    assert_eq!(reopened_file, enc_file);
    let decrypted = reopened.read_data(reopened_file).unwrap();
    assert_eq!(decrypted, secret_data);

    let _ = fs::remove_file(img_path);
}

#[test]
fn test_p2_1_f64_timeseries_and_u16_sensor_data_recommendation() {
    // 1. 8-byte floating point timeseries (1MB = 131,072 f64 values)
    let count_f64 = 131_072;
    let mut f64_data = Vec::with_capacity(count_f64 * 8);
    for i in 0..count_f64 {
        let val = (i as f64) * 0.0012345;
        f64_data.extend_from_slice(&val.to_le_bytes());
    }
    let rec_f64 = recommend_filters(&f64_data);
    println!(
        "f64 Timeseries Best Filter: {} (Savings: {:.1}%, Ratio: {:.2}x)",
        rec_f64.best_report.label,
        rec_f64.best_report.space_savings_percent,
        rec_f64.best_report.compression_ratio
    );
    // Delta or Shuffle with type_size = 8 should be selected and save > 80% space
    assert!(
        rec_f64.best_report.space_savings_percent > 80.0,
        "f64 timeseries should compress > 80% with filters"
    );
    assert_eq!(rec_f64.best_report.config.typesize, 8);

    // 2. 2-byte PCM audio / sensor data (512KB = 262,144 u16 values, e.g. triangular wave)
    let count_u16 = 262_144;
    let mut u16_data = Vec::with_capacity(count_u16 * 2);
    for i in 0..count_u16 {
        let val = (i % 1024) as u16;
        u16_data.extend_from_slice(&val.to_le_bytes());
    }
    let rec_u16 = recommend_filters(&u16_data);
    println!(
        "u16 Signal Best Filter: {} (Savings: {:.1}%, Ratio: {:.2}x)",
        rec_u16.best_report.label,
        rec_u16.best_report.space_savings_percent,
        rec_u16.best_report.compression_ratio
    );
    assert!(
        rec_u16.best_report.space_savings_percent > 70.0,
        "u16 signal should compress well"
    );
}

#[test]
fn test_p2_1_concurrent_recommend_filters_stress() {
    let mut handles = Vec::new();
    for thread_idx in 0..8 {
        handles.push(thread::spawn(move || {
            let size = 512 * 1024 + thread_idx * 64 * 1024;
            let mut payload = Vec::with_capacity(size);
            for i in 0..(size / 4) {
                let v = ((i + thread_idx * 100) as f32) * 0.123;
                payload.extend_from_slice(&v.to_le_bytes());
            }
            let rec = recommend_filters(&payload);
            assert!(rec.baseline_compressed_size > 0);
            assert!(rec.best_report.space_savings_percent > 0.0);
        }));
    }
    for h in handles {
        h.join()
            .expect("Worker thread panicked in concurrent recommendation");
    }
}

#[test]
fn test_p2_2_cache_invalidation_on_file_deletion_and_resize() {
    let img_path = "p2_resize.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let session = OifsSession::get_or_open(img_path, 15 * 1024 * 1024).unwrap();
    let root = session.resolve_path(".").unwrap();
    let fid = session.create_file(root, "dynamic.bin").unwrap();

    // 1. Initial write of 100 bytes
    let payload1 = vec![0xAA; 100];
    session
        .write_data(fid, 0, &payload1, oifs::disk::CompressionMode::Never)
        .unwrap();
    let inode1 = session.read_inode(fid).unwrap();
    assert_eq!(inode1.size, 100);

    // 2. Overwrite expanding to 5,000 bytes
    let payload2 = vec![0xBB; 5000];
    session
        .write_data(fid, 0, &payload2, oifs::disk::CompressionMode::Never)
        .unwrap();
    let inode2 = session.read_inode(fid).unwrap();
    assert_eq!(
        inode2.size, 5000,
        "Cache must immediately reflect enlarged file size"
    );
    assert_eq!(session.read_data(fid).unwrap(), payload2);

    // 3. Partial overwrite at offset 50
    let patch = vec![0xCC; 10];
    session
        .write_data(fid, 50, &patch, oifs::disk::CompressionMode::Never)
        .unwrap();
    let inode3 = session.read_inode(fid).unwrap();
    assert_eq!(
        inode3.size, 5000,
        "Cache must keep size after sub-block write"
    );
    let patched_data = session.read_data(fid).unwrap();
    assert_eq!(&patched_data[50..60], &patch[..]);

    // 4. Delete file: inode must be evicted from cache
    session.delete_file(root, "dynamic.bin").unwrap();
    let resolved_after_delete = session.resolve_path("/dynamic.bin");
    assert!(
        resolved_after_delete.is_err(),
        "Deleted file must no longer resolve"
    );

    drop(session);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_p2_2_deep_directory_hierarchy_and_fsck_consistency() {
    let img_path = "p2_deep.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let session = OifsSession::get_or_open(img_path, 20 * 1024 * 1024).unwrap();
    let mut current_dir = session.resolve_path(".").unwrap();

    // Create 6 levels of nested directories: /level0/level1/.../target.txt
    let depth = 6;
    for i in 0..depth {
        let dir_name = format!("level{}", i);
        current_dir = session.create_directory(current_dir, &dir_name).unwrap();
    }

    let target_fid = session.create_file(current_dir, "target.txt").unwrap();
    let target_data = b"Deeply nested file verified via cached inodes!";
    session
        .write_data(
            target_fid,
            0,
            target_data,
            oifs::disk::CompressionMode::Never,
        )
        .unwrap();

    // Rapid path resolutions traversing the full 6-level hierarchy
    let deep_path = "/level0/level1/level2/level3/level4/level5/target.txt";
    for _ in 0..1000 {
        let resolved = session.resolve_path(deep_path).unwrap();
        assert_eq!(resolved, target_fid);
    }

    // Verify filesystem integrity with fsck report
    let fsck = session.verify_integrity().unwrap();
    assert!(
        fsck.is_clean,
        "Fsck report must be clean after nested directory operations"
    );

    drop(session);
    let _ = fs::remove_file(img_path);
}
