//! Tests for pooled Zstd contexts, single-pass chunk decompression and
//! the compaction of a stream of small appended frames.

use oifs::disk::{CompressionMode, DiskManager};
use std::fs;
use std::path::Path;

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

#[test]
fn test_pooled_compression_round_trips() {
    let img = TestImage::new("test_p426_pool");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    // The same payload is written many times: every write goes through the
    // pooled compressor.
    let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    for i in 0..20 {
        let id = dm
            .create_file(root, &format!("f{i}.bin"))
            .expect("create file");
        dm.write_data(id, 0, &payload, CompressionMode::Always)
            .expect("write data");
        assert_eq!(dm.read_data(id).unwrap(), payload, "round trip must hold");
    }
}

#[test]
fn test_many_small_appends_are_compacted() {
    let img = TestImage::new("test_p426_compact");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "appends.bin").expect("create file");

    // Incompressible payloads: every appended frame adds its own header to
    // the physical size, so the overhead grows with the number of appends.
    let mut seed = 0x12345678u32;
    let mut payload = vec![0u8; 32];
    let mut max_overhead = 0u64;

    for i in 0..1000u64 {
        for byte in payload.iter_mut() {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            *byte = (seed >> 16) as u8;
        }
        dm.write_data(id, i * 32, &payload, CompressionMode::Always)
            .expect("append chunk");

        let inode = dm.read_inode(id).expect("read inode");
        let overhead = inode.compressed_size.saturating_sub(inode.size);
        max_overhead = max_overhead.max(overhead);
    }

    // Without the compaction the overhead would grow with every append (one
    // frame header each); with it the overhead stays bounded.
    let inode = dm.read_inode(id).expect("read inode");
    assert!(
        max_overhead <= inode.size / 4 + 8192,
        "the frame overhead must stay bounded: {max_overhead} bytes for {} logical bytes",
        inode.size
    );

    // The content must be intact.
    let data = dm.read_data(id).expect("read data");
    assert_eq!(data.len() as u64, inode.size);
}

#[test]
fn test_seekable_chunks_survive_the_pooled_decompressor() {
    let img = TestImage::new("test_p426_chunks");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "chunks.bin").expect("create file");

    // Three chunks: the last one is only partially filled.
    let payload: Vec<u8> = (0..160_000u32).map(|i| (i % 97) as u8).collect();
    dm.write_data_with_filters(
        id,
        0,
        &payload,
        CompressionMode::Seekable {
            chunk_size: 64 * 1024,
            level: 3,
        },
        oifs::filters::FilterConfig::none(),
    )
    .expect("write seekable data");

    let data = dm.read_data(id).expect("read data");
    assert_eq!(data, payload, "the chunked content must round trip");
}

#[test]
fn test_par_for_index_small_and_large_chunk_stress() {
    let img = TestImage::new("test_zstd_pool_stress");
    let dm = DiskManager::open(&img.path, 32 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    // Exercise 1, 2, 3, 4, 8, 16, and 24 chunks back-to-back to stress every
    // exact-worker wakeup boundary (`active_workers = (total - 1).min(num_workers)`).
    let mode = CompressionMode::Seekable {
        chunk_size: 64 * 1024,
        level: 1,
    };
    for &chunks in &[1usize, 2, 3, 4, 5, 8, 16, 24] {
        let len = chunks * 64 * 1024;
        let payload: Vec<u8> = (0..len).map(|i| ((i * 17 + chunks) % 251) as u8).collect();
        let id = dm
            .create_file(root, &format!("c_{chunks}.bin"))
            .expect("create");
        for round in 0..5 {
            dm.write_data(id, 0, &payload, mode).expect("write");
            let mut out = vec![0u8; len];
            // Read sequentially in 32 KiB steps to trigger 4-chunk read-ahead
            let mut off = 0usize;
            while off < len {
                let n = dm
                    .read_at(id, off as u64, &mut out[off..off + 32 * 1024])
                    .expect("read_at");
                assert_eq!(n, 32 * 1024);
                off += n;
            }
            assert_eq!(out, payload, "mismatch for chunks={chunks} round={round}");
        }
    }
}

#[test]
#[ignore = "Manual performance benchmark (run with --release -- --ignored --nocapture)"]
fn bench_small_chunk_worker_wakeup() {
    use std::time::Instant;

    let img = TestImage::new("bench_zstd_pool_wakeup");
    let dm = DiskManager::open(&img.path, 64 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;
    let mode = CompressionMode::Seekable {
        chunk_size: 64 * 1024,
        level: 1,
    };

    for &(chunks, iters) in &[(2usize, 400usize), (3, 300), (4, 300), (8, 150), (16, 100)] {
        let len = chunks * 64 * 1024;
        let payload: Vec<u8> = (0..len)
            .map(|i| ((i / 64).wrapping_mul(13) % 251) as u8)
            .collect();
        let id = dm
            .create_file(root, &format!("bench_{chunks}.bin"))
            .expect("create");

        // Warm up pool + Zstd contexts
        for _ in 0..10 {
            dm.write_data(id, 0, &payload, mode).unwrap();
        }

        let t0 = Instant::now();
        for _ in 0..iters {
            dm.write_data(id, 0, &payload, mode).unwrap();
        }
        let write_us = t0.elapsed().as_secs_f64() * 1e6 / iters as f64;

        let t1 = Instant::now();
        for _ in 0..iters {
            let got = dm.read_data(id).unwrap();
            std::hint::black_box(got);
        }
        let read_us = t1.elapsed().as_secs_f64() * 1e6 / iters as f64;

        println!(
            "chunks={chunks:2} ({:4} KiB) | write_avg={write_us:7.1} us | read_avg={read_us:7.1} us",
            len / 1024
        );
    }
}
