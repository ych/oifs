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
