use oifs::disk::{CompressionMode, DiskManager};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
struct BenchResult {
    total_reads: usize,
    stalls_over_500us: usize,
    max_read_latency_us: u64,
    flush_count: usize,
    throughput_mb_s: f64,
}

fn run_flush_read_benchmark(use_legacy_exclusive_lock: bool, duration_ms: u64) -> BenchResult {
    let img_path = format!(
        "test_bench_flush_{}.img",
        if use_legacy_exclusive_lock {
            "exclusive"
        } else {
            "shared"
        }
    );
    if std::path::Path::new(&img_path).exists() {
        let _ = fs::remove_file(&img_path);
    }

    let dm = DiskManager::open(&img_path, 50 * 1024 * 1024).expect("Failed to create disk");
    let root_id = dm.superblock().root_inode;
    let file_id = dm.create_file(root_id, "data.bin").expect("create_file");

    // Write 5MB payload
    let payload_size = 5 * 1024 * 1024;
    let payload: Vec<u8> = (0..payload_size).map(|i| (i * 37 % 256) as u8).collect();
    dm.write_data(file_id, 0, &payload, CompressionMode::Never)
        .expect("write_data");

    let stop = Arc::new(AtomicBool::new(false));
    let total_reads = Arc::new(AtomicUsize::new(0));
    let total_bytes = Arc::new(AtomicUsize::new(0));
    let stalls_count = Arc::new(AtomicUsize::new(0));
    let max_latency_us = Arc::new(AtomicU64::new(0));
    let flush_count = Arc::new(AtomicUsize::new(0));

    let num_readers = 8;
    let mut reader_handles = Vec::new();

    for t in 0..num_readers {
        let dm_clone = dm.clone();
        let stop_clone = stop.clone();
        let total_reads_clone = total_reads.clone();
        let total_bytes_clone = total_bytes.clone();
        let max_lat_clone = max_latency_us.clone();
        let stalls_clone = stalls_count.clone();

        reader_handles.push(thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            let mut reads = 0;
            let mut bytes = 0;
            let mut stalls = 0;
            let mut local_max_lat = 0u64;

            while !stop_clone.load(Ordering::Relaxed) {
                let offset = ((t * 8192 + reads * 4096) % (payload_size - 4096)) as u64;
                let t0 = Instant::now();
                let n = dm_clone
                    .read_at(file_id, offset, &mut buf)
                    .expect("read_at");
                let elapsed_us = t0.elapsed().as_micros() as u64;

                if elapsed_us > 500 {
                    stalls += 1;
                }
                if elapsed_us > local_max_lat {
                    local_max_lat = elapsed_us;
                }
                reads += 1;
                bytes += n;
            }

            total_reads_clone.fetch_add(reads, Ordering::Relaxed);
            total_bytes_clone.fetch_add(bytes, Ordering::Relaxed);
            stalls_clone.fetch_add(stalls, Ordering::Relaxed);
            max_lat_clone.fetch_max(local_max_lat, Ordering::Relaxed);
        }));
    }

    // 4 concurrent flusher threads
    let mut flusher_handles = Vec::new();
    for _ in 0..4 {
        let dm_flusher = dm.clone();
        let stop_flusher = stop.clone();
        let flush_count_clone = flush_count.clone();

        let dirty_chunk = vec![0xABu8; 65536];
        flusher_handles.push(thread::spawn(move || {
            let mut count = 0;
            while !stop_flusher.load(Ordering::Relaxed) {
                let write_off = (count as u64 * 65536) % (payload_size as u64 - 65536);
                let _ =
                    dm_flusher.write_data(file_id, write_off, &dirty_chunk, CompressionMode::Never);
                if use_legacy_exclusive_lock {
                    dm_flusher.flush_exclusive_legacy().expect("flush failed");
                } else {
                    dm_flusher.flush().expect("flush failed");
                }
                count += 1;
                thread::sleep(Duration::from_millis(10));
            }
            flush_count_clone.fetch_add(count, Ordering::Relaxed);
        }));
    }

    // Run for requested duration
    thread::sleep(Duration::from_millis(duration_ms));
    stop.store(true, Ordering::Relaxed);

    for h in reader_handles {
        h.join().unwrap();
    }
    for h in flusher_handles {
        h.join().unwrap();
    }

    let _ = fs::remove_file(&img_path);

    let reads = total_reads.load(Ordering::Relaxed);
    let bytes = total_bytes.load(Ordering::Relaxed);
    let stalls = stalls_count.load(Ordering::Relaxed);
    let max_lat = max_latency_us.load(Ordering::Relaxed);
    let flushes = flush_count.load(Ordering::Relaxed);
    let throughput = (bytes as f64 / (1024.0 * 1024.0)) / (duration_ms as f64 / 1000.0);

    BenchResult {
        total_reads: reads,
        stalls_over_500us: stalls,
        max_read_latency_us: max_lat,
        flush_count: flushes,
        throughput_mb_s: throughput,
    }
}

#[test]
fn bench_flush_exclusive_vs_shared() {
    println!("\n================================================================================");
    println!("        BENCHMARK: Exclusive Write Lock vs Shared Read Lock for flush()         ");
    println!("================================================================================");
    println!("Configuration: 8 Concurrent Readers + 4 Concurrent Flushers (high sync frequency)");

    let duration_ms = 1500;

    // Run Exclusive (Legacy)
    let legacy = run_flush_read_benchmark(true, duration_ms);

    // Warmup / pause
    thread::sleep(Duration::from_millis(200));

    // Run Shared (New P4.5)
    let new_impl = run_flush_read_benchmark(false, duration_ms);

    println!("\n[Legacy: Exclusive Write Lock (inner.write())]");
    println!("  - Total flushes executed:   {}", legacy.flush_count);
    println!("  - Total reads completed:    {}", legacy.total_reads);
    println!(
        "  - Read throughput:          {:.2} MB/s",
        legacy.throughput_mb_s
    );
    println!(
        "  - Read stalls (>500µs):     {} occurrences",
        legacy.stalls_over_500us
    );
    println!(
        "  - Max read latency:         {:.2} ms ({} µs)",
        legacy.max_read_latency_us as f64 / 1000.0,
        legacy.max_read_latency_us
    );

    println!("\n[New P4.5: Sync Mutex + Shared Read Lock (inner.read())]");
    println!("  - Total flushes executed:   {}", new_impl.flush_count);
    println!("  - Total reads completed:    {}", new_impl.total_reads);
    println!(
        "  - Read throughput:          {:.2} MB/s",
        new_impl.throughput_mb_s
    );
    println!(
        "  - Read stalls (>500µs):     {} occurrences",
        new_impl.stalls_over_500us
    );
    println!(
        "  - Max read latency:         {:.2} ms ({} µs)",
        new_impl.max_read_latency_us as f64 / 1000.0,
        new_impl.max_read_latency_us
    );

    let speedup = (new_impl.throughput_mb_s / legacy.throughput_mb_s - 1.0) * 100.0;
    let stalls_reduction = if legacy.stalls_over_500us > 0 {
        (1.0 - (new_impl.stalls_over_500us as f64 / legacy.stalls_over_500us as f64)) * 100.0
    } else {
        0.0
    };

    println!("\n--------------------------------------------------------------------------------");
    println!(
        "  >>> Read Throughput Change:  {:+.1}% ({:.2} MB/s vs {:.2} MB/s)",
        speedup, legacy.throughput_mb_s, new_impl.throughput_mb_s
    );
    println!(
        "  >>> Read Stalls (>500µs):    {:.1}% reduction ({} stalls -> {} stalls)",
        stalls_reduction, legacy.stalls_over_500us, new_impl.stalls_over_500us
    );
    println!("================================================================================\n");
}
