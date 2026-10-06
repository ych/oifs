use oifs::disk::{CompressionMode, DiskManager};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

struct CleanupGuard(&'static str);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}

#[test]
fn test_concurrent_readers_scaling_and_safety() {
    let img_path = "test_rwlock_concurrency.img";
    let _guard = CleanupGuard(img_path);
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let dm = DiskManager::open(img_path, 20 * 1024 * 1024).expect("Failed to create disk");
    let root_id = dm.superblock().root_inode;
    let file_id = dm
        .create_file(root_id, "shared_read.bin")
        .expect("create_file");

    // Write 50KB payload
    let payload: Vec<u8> = (0..50_000).map(|i| (i * 31 % 256) as u8).collect();
    dm.write_data(file_id, 0, &payload, CompressionMode::Never)
        .expect("write_data");

    let num_readers = 16;
    let reads_per_thread = 500;
    let total_bytes_read = Arc::new(AtomicUsize::new(0));

    let start = Instant::now();
    let mut handles = Vec::new();

    for thread_idx in 0..num_readers {
        let dm_clone = dm.clone();
        let payload_ref = payload.clone();
        let total_bytes = total_bytes_read.clone();

        handles.push(thread::spawn(move || {
            let mut buf = vec![0u8; 1024];
            for i in 0..reads_per_thread {
                // Varying offset across the file
                let offset = ((thread_idx * 137 + i * 97) % (payload_ref.len() - 1024)) as u64;
                let n = dm_clone
                    .read_at(file_id, offset, &mut buf)
                    .expect("read_at");
                assert_eq!(n, 1024);
                assert_eq!(
                    &buf[..],
                    &payload_ref[offset as usize..offset as usize + 1024]
                );
                total_bytes.fetch_add(n, Ordering::Relaxed);
            }
        }));
    }

    for h in handles {
        h.join().expect("reader thread panicked");
    }

    let elapsed = start.elapsed();
    let total_read = total_bytes_read.load(Ordering::Relaxed);
    assert_eq!(total_read, num_readers * reads_per_thread * 1024);
    println!(
        "Concurrent readers completed: {} bytes in {:?} ({:.2} MB/s)",
        total_read,
        elapsed,
        (total_read as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64()
    );
}

#[test]
fn test_mixed_readers_and_writers_safety() {
    let img_path = "test_rwlock_mixed.img";
    let _guard = CleanupGuard(img_path);
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let dm = DiskManager::open(img_path, 30 * 1024 * 1024).expect("Failed to create disk");
    let root_id = dm.superblock().root_inode;
    let main_file_id = dm
        .create_file(root_id, "main_file.bin")
        .expect("create_file");

    let payload: Vec<u8> = (0..20_000).map(|i| (i * 17 % 256) as u8).collect();
    dm.write_data(main_file_id, 0, &payload, CompressionMode::Never)
        .expect("write_data");

    let stop_flag = Arc::new(AtomicBool::new(false));
    let mut reader_handles = Vec::new();

    // 8 concurrent reader threads
    for t in 0..8 {
        let dm_clone = dm.clone();
        let payload_ref = payload.clone();
        let stop = stop_flag.clone();

        reader_handles.push(thread::spawn(move || {
            let mut buf = vec![0u8; 512];
            let mut read_count = 0;
            while !stop.load(Ordering::Relaxed) {
                let offset = ((t * 100 + read_count * 50) % (payload_ref.len() - 512)) as u64;
                let n = dm_clone
                    .read_at(main_file_id, offset, &mut buf)
                    .expect("read_at");
                assert_eq!(n, 512);
                assert_eq!(
                    &buf[..],
                    &payload_ref[offset as usize..offset as usize + 512]
                );
                read_count += 1;
            }
            read_count
        }));
    }

    // 2 concurrent writer threads creating separate files
    let mut writer_handles = Vec::new();
    for w in 0..2 {
        let dm_clone = dm.clone();
        writer_handles.push(thread::spawn(move || {
            for i in 0..50 {
                let fname = format!("writer_{}_{}.bin", w, i);
                let fid = dm_clone.create_file(root_id, &fname).expect("create_file");
                let data = vec![(w * 10 + i) as u8; 1000];
                dm_clone
                    .write_data(fid, 0, &data, CompressionMode::Never)
                    .expect("write_data");
                let read_back = dm_clone.read_data(fid).expect("read_data");
                assert_eq!(read_back, data);
            }
        }));
    }

    for wh in writer_handles {
        wh.join().expect("writer panicked");
    }

    // Stop readers after writers are done
    stop_flag.store(true, Ordering::Relaxed);
    for rh in reader_handles {
        let count = rh.join().expect("reader panicked");
        assert!(count > 0);
    }
}

#[test]
fn test_concurrent_flush_and_readers() {
    let img_path = "test_rwlock_flush.img";
    let _guard = CleanupGuard(img_path);
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let dm = DiskManager::open(img_path, 20 * 1024 * 1024).expect("Failed to create disk");
    let root_id = dm.superblock().root_inode;
    let file_id = dm
        .create_file(root_id, "flush_read_test.bin")
        .expect("create_file");

    let payload: Vec<u8> = (0..100_000).map(|i| (i * 23 % 256) as u8).collect();
    dm.write_data(file_id, 0, &payload, CompressionMode::Never)
        .expect("write_data");

    let stop_flag = Arc::new(AtomicBool::new(false));
    let mut reader_handles = Vec::new();

    // 8 concurrent reader threads reading in a tight loop
    for t in 0..8 {
        let dm_clone = dm.clone();
        let payload_ref = payload.clone();
        let stop = stop_flag.clone();

        reader_handles.push(thread::spawn(move || {
            let mut buf = vec![0u8; 1024];
            let mut read_count = 0;
            while !stop.load(Ordering::Relaxed) {
                let offset = ((t * 200 + read_count * 128) % (payload_ref.len() - 1024)) as u64;
                let n = dm_clone
                    .read_at(file_id, offset, &mut buf)
                    .expect("read_at");
                assert_eq!(n, 1024);
                assert_eq!(
                    &buf[..],
                    &payload_ref[offset as usize..offset as usize + 1024]
                );
                read_count += 1;
            }
            read_count
        }));
    }

    // 4 concurrent flush threads calling flush() and flush_async() repeatedly
    let mut flush_handles = Vec::new();
    for f in 0..4 {
        let dm_clone = dm.clone();
        flush_handles.push(thread::spawn(move || {
            for _ in 0..20 {
                if f % 2 == 0 {
                    dm_clone.flush().expect("flush failed");
                } else {
                    dm_clone.flush_async().expect("flush_async failed");
                }
                thread::sleep(std::time::Duration::from_millis(5));
            }
        }));
    }

    // Wait for all flushes to complete
    for fh in flush_handles {
        fh.join().expect("flush thread panicked");
    }

    // Stop reader threads
    stop_flag.store(true, Ordering::Relaxed);
    let mut total_reads = 0;
    for rh in reader_handles {
        let count = rh.join().expect("reader thread panicked");
        total_reads += count;
    }

    assert!(
        total_reads >= 800,
        "Readers should have made significant progress during flush: got {}",
        total_reads
    );
}

#[test]
fn test_p4_1_out_of_lock_compression_and_encryption_concurrency() {
    let img_path = "test_p4_1_concurrency.img";
    let _guard = CleanupGuard(img_path);
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    // Open an encrypted disk image to exercise both Zstd compression and ChaCha20 encryption
    let dm =
        DiskManager::open_with_password(img_path, 40 * 1024 * 1024, Some("P41_Secure_Passwd!"))
            .expect("Failed to open encrypted disk image");
    let root_id = dm.superblock().root_inode;

    // 1. Create a reference file for readers to read concurrently
    let read_target_id = dm
        .create_file(root_id, "reader_target.bin")
        .expect("create reader_target");
    let reader_payload: Vec<u8> = (0..64_000).map(|i| (i * 47 % 256) as u8).collect();
    dm.write_data(read_target_id, 0, &reader_payload, CompressionMode::Never)
        .expect("seed reader target");

    let stop_flag = Arc::new(AtomicBool::new(false));
    let mut reader_handles = Vec::new();

    // Spawn 8 concurrent reader threads querying `read_at` on the reference file
    for t in 0..8 {
        let dm_clone = dm.clone();
        let payload_ref = reader_payload.clone();
        let stop = stop_flag.clone();

        reader_handles.push(thread::spawn(move || {
            let mut buf = vec![0u8; 1024];
            let mut reads_done = 0;
            while !stop.load(Ordering::Relaxed) {
                let offset = ((t * 313 + reads_done * 101) % (payload_ref.len() - 1024)) as u64;
                let n = dm_clone
                    .read_at(read_target_id, offset, &mut buf)
                    .expect("read_at during heavy writer");
                assert_eq!(n, 1024);
                assert_eq!(
                    &buf[..],
                    &payload_ref[offset as usize..offset as usize + 1024]
                );
                reads_done += 1;
            }
            reads_done
        }));
    }

    // Spawn 2 heavy writer threads: each repeatedly writes 256KB files with
    // CompressionMode::Always and FilterConfig::numeric(4), running Delta + Shuffle + Zstd + Encryption!
    let mut writer_handles = Vec::new();
    for w in 0..2 {
        let dm_clone = dm.clone();
        writer_handles.push(thread::spawn(move || {
            let mut written_files = Vec::new();
            for i in 0..8 {
                let fname = format!("heavy_writer_{}_{}.dat", w, i);
                let fid = dm_clone.create_file(root_id, &fname).expect("create_file");
                // Construct structured numeric data that exercises Delta + Shuffle filters
                let size = 256 * 1024;
                let mut data = vec![0u8; size];
                let (chunks, _) = data.as_chunks_mut::<4>();
                for (j, chunk) in chunks.iter_mut().enumerate() {
                    let val = (j as u32).wrapping_mul(13).to_le_bytes();
                    chunk.copy_from_slice(&val);
                }
                dm_clone
                    .write_data_with_filters(
                        fid,
                        0,
                        &data,
                        CompressionMode::Always,
                        oifs::filters::FilterConfig::numeric(4),
                    )
                    .expect("heavy write_data_with_filters");
                written_files.push((fid, data));
            }

            // Verify integrity of all written files
            for (fid, original) in written_files {
                let read_back = dm_clone.read_data(fid).expect("read_data back");
                assert_eq!(read_back, original, "Integrity mismatch after write");
            }
        }));
    }

    // Wait for writers to complete
    for wh in writer_handles {
        wh.join().expect("writer panicked");
    }

    // Signal readers to stop
    stop_flag.store(true, Ordering::Relaxed);
    let mut total_reads = 0;
    for rh in reader_handles {
        let reads = rh.join().expect("reader panicked");
        total_reads += reads;
    }

    println!(
        "P4.1 Concurrency Proof: Reader threads performed {} reads concurrently during heavy compression/encryption writes!",
        total_reads
    );
    assert!(
        total_reads >= 500,
        "Readers should maintain high throughput while out-of-lock writes are processed: got {}",
        total_reads
    );
}

#[test]
fn test_p4_1_reader_latency_under_heavy_writes() {
    let img_path = "test_p4_1_latency.img";
    let _guard = CleanupGuard(img_path);
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let dm =
        DiskManager::open_with_password(img_path, 40 * 1024 * 1024, Some("P41_Latency_Passwd!"))
            .expect("Failed to open encrypted disk image");
    let root_id = dm.superblock().root_inode;

    let target_file = dm
        .create_file(root_id, "target.bin")
        .expect("create target");
    let target_data = vec![42u8; 32 * 1024];
    dm.write_data(target_file, 0, &target_data, CompressionMode::Never)
        .expect("seed target");

    let stop_flag = Arc::new(AtomicBool::new(false));
    let max_reader_latency_micros = Arc::new(AtomicUsize::new(0));

    // Reader thread measuring per-read latency
    let dm_reader = dm.clone();
    let stop_reader = stop_flag.clone();
    let max_lat = max_reader_latency_micros.clone();

    let reader_handle = thread::spawn(move || {
        let mut buf = vec![0u8; 1024];
        let mut total_reads = 0;
        let mut latencies_us = Vec::with_capacity(10000);

        while !stop_reader.load(Ordering::Relaxed) {
            let start = Instant::now();
            let n = dm_reader
                .read_at(target_file, 0, &mut buf)
                .expect("read_at");
            let elapsed_us = start.elapsed().as_micros() as usize;

            assert_eq!(n, 1024);
            latencies_us.push(elapsed_us);
            max_lat.fetch_max(elapsed_us, Ordering::Relaxed);
            total_reads += 1;
        }

        latencies_us.sort_unstable();
        let p50 = latencies_us
            .get(latencies_us.len() / 2)
            .copied()
            .unwrap_or(0);
        let p99 = latencies_us
            .get(latencies_us.len() * 99 / 100)
            .copied()
            .unwrap_or(0);
        (total_reads, p50, p99)
    });

    // Writer thread doing heavy compressed writes
    let dm_writer = dm.clone();
    let writer_handle = thread::spawn(move || {
        let heavy_file = dm_writer
            .create_file(root_id, "heavy.bin")
            .expect("create heavy");
        let heavy_data = vec![0xabu8; 512 * 1024];
        for _ in 0..10 {
            dm_writer
                .write_data_with_filters(
                    heavy_file,
                    0,
                    &heavy_data,
                    CompressionMode::Always,
                    oifs::filters::FilterConfig::numeric(4),
                )
                .expect("write_data_with_filters");
        }
    });

    writer_handle.join().expect("writer finished");
    stop_flag.store(true, Ordering::Relaxed);

    let (total_reads, p50, p99) = reader_handle.join().expect("reader finished");
    let max_observed = max_reader_latency_micros.load(Ordering::Relaxed);

    println!(
        "P4.1 Latency Proof: Total reads: {}, p50: {} µs, p99: {} µs, max: {} µs",
        total_reads, p50, p99, max_observed
    );
    assert!(total_reads > 1000, "Reader should maintain high throughput");
    assert!(
        p99 < 15_000,
        "p99 read latency should stay low, got {} µs",
        p99
    );
}

#[test]
fn test_sharded_inode_cache_sparse_readers_concurrency() {
    let img_path = "test_sharded_cache_sparse.img";
    let _guard = CleanupGuard(img_path);
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let dm = DiskManager::open(img_path, 30 * 1024 * 1024).expect("Failed to open disk");
    let root_id = dm.superblock().root_inode;

    // Create 128 different files to distribute across all 32 cache shards
    let file_count = 128;
    let mut files = Vec::with_capacity(file_count);
    for i in 0..file_count {
        let fname = format!("sparse_file_{:03}.dat", i);
        let fid = dm.create_file(root_id, &fname).expect("create_file");
        let payload = format!("Content for file {:03} with padding bytes...", i).into_bytes();
        dm.write_data(fid, 0, &payload, CompressionMode::Never)
            .expect("write_data");
        files.push((fid, payload));
    }

    let files_arc = Arc::new(files);
    let mut handles = Vec::new();
    let num_threads = 16;
    let ops_per_thread = 1_000;

    let start = Instant::now();
    for t in 0..num_threads {
        let dm_clone = dm.clone();
        let files_ref = files_arc.clone();
        handles.push(thread::spawn(move || {
            let mut buf = vec![0u8; 128];
            for i in 0..ops_per_thread {
                // Read pseudo-random files to test sharded cache access
                let target_idx = (t * 7919 + i * 1013) % files_ref.len();
                let (fid, ref expected) = files_ref[target_idx];
                let n = dm_clone.read_at(fid, 0, &mut buf).expect("read_at");
                assert_eq!(n, expected.len());
                assert_eq!(&buf[..n], &expected[..]);
            }
        }));
    }

    for h in handles {
        h.join().expect("reader thread panicked");
    }

    let elapsed = start.elapsed();
    let total_ops = num_threads * ops_per_thread;
    let ops_per_sec = total_ops as f64 / elapsed.as_secs_f64();
    println!(
        "Sharded Inode Cache: {} sparse read_at operations completed in {:?} ({:.0} ops/sec)",
        total_ops, elapsed, ops_per_sec
    );
    assert!(
        ops_per_sec > 10_000.0,
        "Sharded cache must achieve high throughput across threads, got {:.0} ops/sec",
        ops_per_sec
    );
}
