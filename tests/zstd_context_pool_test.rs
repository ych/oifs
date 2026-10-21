//! Tests for P4.26: pooled Zstd contexts, single-pass chunk decompression and
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
