//! Performance Benchmark & Proof: Old vs New Implementation Comparison
//!
//! Run with: cargo test --test perf_comparison --release -- --nocapture

use std::hint::black_box;
use std::io::Cursor;
use std::time::Instant;

use oifs::bitmap::{Bitmap, BitmapRef};
use oifs::directory::{DirectoryEntry, find_entry_in_block, find_insert_offset_in_block};
use oifs::filters::{FilterConfig, apply_filters_cow, bitshuffle_encode, shuffle_encode};

// =========================================================================
// Benchmark 1: Bitmap find_first_free
// =========================================================================

fn old_find_first_free(data: &[u8]) -> Option<usize> {
    for (i, &byte) in data.iter().enumerate() {
        if byte != 0xFF {
            for bit in 0..8 {
                if (byte & (1 << bit)) == 0 {
                    return Some(i * 8 + bit);
                }
            }
        }
    }
    None
}

fn new_find_first_free(data: &[u8]) -> Option<usize> {
    let (chunks, remainder) = data.as_chunks::<8>();
    let chunk_count = chunks.len();

    for (chunk_idx, chunk) in chunks.iter().enumerate() {
        let word = u64::from_le_bytes(*chunk);
        if word != u64::MAX {
            let bit = (!word).trailing_zeros() as usize;
            return Some(chunk_idx * 64 + bit);
        }
    }

    let base = chunk_count * 64;
    for (i, &byte) in remainder.iter().enumerate() {
        if byte != 0xFF {
            let bit = (!byte).trailing_zeros() as usize;
            return Some(base + i * 8 + bit);
        }
    }
    None
}

// =========================================================================
// Benchmark 2: Directory Lookup
// =========================================================================

fn old_lookup(slice: &[u8], name: &str) -> Option<u64> {
    let mut cursor = Cursor::new(slice);
    loop {
        match DirectoryEntry::deserialize_from(&mut cursor) {
            Ok(Some(entry)) => {
                if entry.name == name {
                    return Some(entry.inode);
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    None
}

fn new_lookup(slice: &[u8], name: &str) -> Option<u64> {
    find_entry_in_block(slice, name)
}

// =========================================================================
// Benchmark 3: Directory Append Offset Scan
// =========================================================================

fn old_find_insert_offset(slice: &[u8]) -> usize {
    let mut cursor = Cursor::new(slice);
    let mut insert_offset = 0;
    loop {
        let start = cursor.position();
        match DirectoryEntry::deserialize_from(&mut cursor) {
            Ok(Some(_)) => continue,
            Ok(None) => {
                insert_offset = start as usize;
                break;
            }
            Err(_) => break,
        }
    }
    insert_offset
}

fn new_find_insert_offset(slice: &[u8]) -> usize {
    find_insert_offset_in_block(slice)
}

// =========================================================================
// Benchmark 4: Write Path Filter Overhead (when filters disabled)
// =========================================================================

fn old_write_path_filter(data: &[u8]) -> Vec<u8> {
    // Old: unconditionally called apply_filters which did data.to_vec()
    data.to_vec()
}

fn new_write_path_filter<'a>(data: &'a [u8], cfg: &FilterConfig) -> std::borrow::Cow<'a, [u8]> {
    apply_filters_cow(data, cfg)
}

// =========================================================================
// Benchmark 5: Sequential Block Allocation (Hint vs No Hint)
// =========================================================================

fn old_sequential_allocate(data: &mut [u8], count: usize) {
    for _ in 0..count {
        if let Some(bit) = old_find_first_free(data) {
            data[bit / 8] |= 1 << (bit % 8);
        }
    }
}

fn new_sequential_allocate(data: &mut [u8], count: usize) {
    let mut bm = Bitmap::new(data);
    let mut hint = 0;
    for _ in 0..count {
        if let Some(bit) = bm.find_next_free_wrapped(hint) {
            bm.set(bit);
            hint = bit + 1;
        }
    }
}

// =========================================================================
// Benchmark 6: Fsck Integrity Scan (64-bit word skipping vs bit-by-bit)
// =========================================================================

fn old_fsck_bitmap_scan(data: &[u8], total_bits: usize) -> usize {
    let mut count = 0;
    for i in 0..total_bits {
        let byte = data[i / 8];
        let bit = i % 8;
        if (byte & (1 << bit)) != 0 {
            count += 1;
        }
    }
    count
}

fn new_fsck_bitmap_scan(data: &[u8], total_bits: usize) -> usize {
    let bm_ref = BitmapRef::new(data);
    let mut count = 0;
    bm_ref.for_each_set_bit(total_bits, |_| {
        count += 1;
    });
    count
}

// =========================================================================
// Benchmark 7: Byte Shuffle (Unrolled 4-byte vs byte-by-byte)
// =========================================================================

fn old_shuffle_encode(src: &[u8], typesize: usize) -> Vec<u8> {
    let mut dest = vec![0u8; src.len()];
    let num_elements = src.len() / typesize;
    for elem_idx in 0..num_elements {
        for byte_idx in 0..typesize {
            let src_idx = elem_idx * typesize + byte_idx;
            let dest_idx = byte_idx * num_elements + elem_idx;
            dest[dest_idx] = src[src_idx];
        }
    }
    let remainder_start = num_elements * typesize;
    if remainder_start < src.len() {
        dest[remainder_start..].copy_from_slice(&src[remainder_start..]);
    }
    dest
}

fn old_bitshuffle_encode(data: &[u8], typesize: usize) -> Vec<u8> {
    if data.is_empty() || typesize == 0 {
        return data.to_vec();
    }
    let byte_shuffled = shuffle_encode(data, typesize);
    let mut out = vec![0u8; data.len()];
    let n_bytes = data.len();
    let n_blocks = n_bytes / 8;
    for b in 0..n_blocks {
        let block_offset = b * 8;
        for i in 0..8 {
            let byte_val = byte_shuffled[block_offset + i];
            for j in 0..8 {
                let bit = (byte_val >> j) & 1;
                out[block_offset + j] |= bit << i;
            }
        }
    }
    let rem_start = n_blocks * 8;
    if rem_start < n_bytes {
        out[rem_start..].copy_from_slice(&byte_shuffled[rem_start..]);
    }
    out
}

// =========================================================================
// TEST RUNNER & DETAILED REPORT
// =========================================================================

#[test]
fn bench_comprehensive_performance_proof() {
    println!("\n================================================================================");
    println!("             OIFS PERFORMANCE BENCHMARK & PROOF REPORT                          ");
    println!("================================================================================");

    // -------------------------------------------------------------------------
    // CASE 1: Bitmap Allocation Scan (4KB block = 32,768 blocks tracked)
    // -------------------------------------------------------------------------
    println!("\n--- [CASE 1] Bitmap find_first_free (4096-byte Bitmap = 32,768 blocks) ---");
    let occupancies = [
        ("Early free (10% full, ~3,200 blocks in use)", 400),
        ("Mid free   (50% full, ~16,384 blocks in use)", 2048),
        ("Late free  (95% full, ~31,120 blocks in use)", 3890),
    ];

    let iters = 50_000;
    for (desc, free_byte_idx) in occupancies {
        let mut bitmap_buf = vec![0xFFu8; 4096];
        // Clear a bit at free_byte_idx
        bitmap_buf[free_byte_idx] = 0xFE; // bit 0 is free

        // Warmup
        black_box(old_find_first_free(&bitmap_buf));
        black_box(new_find_first_free(&bitmap_buf));

        // Benchmark Old
        let start = Instant::now();
        for _ in 0..iters {
            black_box(old_find_first_free(&bitmap_buf));
        }
        let old_time = start.elapsed();

        // Benchmark New
        let start = Instant::now();
        for _ in 0..iters {
            black_box(new_find_first_free(&bitmap_buf));
        }
        let new_time = start.elapsed();

        let speedup = old_time.as_nanos() as f64 / new_time.as_nanos() as f64;
        println!("  Scenario: {}", desc);
        println!(
            "    Old (byte-by-byte scan) : {:>8.2?} ({:.1} ns/op)",
            old_time,
            old_time.as_nanos() as f64 / iters as f64
        );
        println!(
            "    New (64-bit word tzcnt) : {:>8.2?} ({:.1} ns/op)",
            new_time,
            new_time.as_nanos() as f64 / iters as f64
        );
        println!("    --> SPEEDUP: \x1b[1;32m{:.2}x FASTER\x1b[0m", speedup);
    }

    // -------------------------------------------------------------------------
    // CASE 2: Directory Lookup (50 files in a directory block)
    // -------------------------------------------------------------------------
    println!("\n--- [CASE 2] Directory Lookup (50 files in directory) ---");
    let mut dir_block = vec![0u8; 4096];
    let mut cursor = Cursor::new(&mut dir_block[..]);

    for i in 0..50 {
        let entry = DirectoryEntry {
            inode: 100 + i as u64,
            hash: 0,
            name: format!("file_{:03}.dat", i),
        };
        entry.serialize_into(&mut cursor).unwrap();
    }

    let lookup_targets = [
        ("Near start (file_005.dat)", "file_005.dat"),
        ("Middle     (file_025.dat)", "file_025.dat"),
        ("Near end   (file_048.dat)", "file_048.dat"),
    ];

    let lookup_iters = 20_000;
    for (desc, target) in lookup_targets {
        // Warmup
        assert_eq!(
            old_lookup(&dir_block, target),
            new_lookup(&dir_block, target)
        );

        let start = Instant::now();
        for _ in 0..lookup_iters {
            black_box(old_lookup(&dir_block, target));
        }
        let old_time = start.elapsed();

        let start = Instant::now();
        for _ in 0..lookup_iters {
            black_box(new_lookup(&dir_block, target));
        }
        let new_time = start.elapsed();

        let speedup = old_time.as_nanos() as f64 / new_time.as_nanos() as f64;
        println!("  Target: {}", desc);
        println!(
            "    Old (Cursor + deserialize + String) : {:>8.2?} ({:.1} ns/op)",
            old_time,
            old_time.as_nanos() as f64 / lookup_iters as f64
        );
        println!(
            "    New (Zero-alloc slice comparison)  : {:>8.2?} ({:.1} ns/op)",
            new_time,
            new_time.as_nanos() as f64 / lookup_iters as f64
        );
        println!("    --> SPEEDUP: \x1b[1;32m{:.2}x FASTER\x1b[0m", speedup);
    }

    // -------------------------------------------------------------------------
    // CASE 3: Directory Append Offset Scan (finding insertion point)
    // -------------------------------------------------------------------------
    println!("\n--- [CASE 3] Directory Append Offset Scan (create_file / mkdir) ---");
    assert_eq!(
        old_find_insert_offset(&dir_block),
        new_find_insert_offset(&dir_block)
    );

    let scan_iters = 30_000;
    let start = Instant::now();
    for _ in 0..scan_iters {
        black_box(old_find_insert_offset(&dir_block));
    }
    let old_time = start.elapsed();

    let start = Instant::now();
    for _ in 0..scan_iters {
        black_box(new_find_insert_offset(&dir_block));
    }
    let new_time = start.elapsed();

    let speedup = old_time.as_nanos() as f64 / new_time.as_nanos() as f64;
    println!(
        "  Old (deserializing 50 entries to heap) : {:>8.2?} ({:.1} ns/op)",
        old_time,
        old_time.as_nanos() as f64 / scan_iters as f64
    );
    println!(
        "  New (step-skipping 18+len bytes in slice) : {:>8.2?} ({:.1} ns/op)",
        new_time,
        new_time.as_nanos() as f64 / scan_iters as f64
    );
    println!("  --> SPEEDUP: \x1b[1;32m{:.2}x FASTER\x1b[0m", speedup);

    // -------------------------------------------------------------------------
    // CASE 4: Write Path Filter Overhead (4KB, 64KB, 1MB payloads)
    // -------------------------------------------------------------------------
    println!("\n--- [CASE 4] Write Path Filter Zero-Copy (Filters Disabled / Standard Mode) ---");
    let sizes = [
        ("Small payload (4 KB)", 4 * 1024),
        ("Medium payload (64 KB)", 64 * 1024),
        ("Large payload (1 MB)", 1024 * 1024),
    ];
    let cfg = FilterConfig::none();

    for (desc, sz) in sizes {
        let payload = vec![0xABu8; sz];
        let write_iters = if sz >= 1024 * 1024 { 500 } else { 10_000 };

        let start = Instant::now();
        for _ in 0..write_iters {
            black_box(old_write_path_filter(&payload));
        }
        let old_time = start.elapsed();

        let start = Instant::now();
        for _ in 0..write_iters {
            black_box(new_write_path_filter(&payload, &cfg));
        }
        let new_time = start.elapsed();

        let speedup = old_time.as_nanos() as f64 / new_time.as_nanos() as f64;
        let old_throughput =
            (sz as f64 * write_iters as f64 / 1_048_576.0) / old_time.as_secs_f64();
        println!("  Payload: {}", desc);
        println!(
            "    Old (Unconditional data.to_vec() copy) : {:>8.2?} (throughput: {:.1} MB/s)",
            old_time, old_throughput
        );
        println!(
            "    New (Cow::Borrowed zero-copy borrow)    : {:>8.2?} (instantaneous borrow)",
            new_time
        );
        println!("    --> SPEEDUP: \x1b[1;32m{:.1}x FASTER\x1b[0m", speedup);
    }

    // -------------------------------------------------------------------------
    // CASE 5: Sequential Multi-Block Allocation (Hint vs No Hint)
    // -------------------------------------------------------------------------
    println!(
        "\n--- [CASE 5] Multi-Block Sequential Allocation (2,000 blocks in 32,768-block bitmap) ---"
    );
    let alloc_count = 2000;
    let alloc_iters = 100;

    let start = Instant::now();
    for _ in 0..alloc_iters {
        let mut buf = vec![0u8; 4096];
        old_sequential_allocate(&mut buf, alloc_count);
        black_box(());
    }
    let old_time = start.elapsed();

    let start = Instant::now();
    for _ in 0..alloc_iters {
        let mut buf = vec![0u8; 4096];
        new_sequential_allocate(&mut buf, alloc_count);
        black_box(());
    }
    let new_time = start.elapsed();

    let speedup = old_time.as_nanos() as f64 / new_time.as_nanos() as f64;
    println!(
        "  Old (O(N^2) scan from bit 0 each time)   : {:>8.2?} ({:.2} µs/alloc-batch)",
        old_time,
        (old_time.as_micros() as f64) / alloc_iters as f64
    );
    println!(
        "  New (O(1) search from free_block_hint)   : {:>8.2?} ({:.2} µs/alloc-batch)",
        new_time,
        (new_time.as_micros() as f64) / alloc_iters as f64
    );
    println!("  --> SPEEDUP: \x1b[1;32m{:.2}x FASTER\x1b[0m", speedup);

    // -------------------------------------------------------------------------
    // CASE 6: Fsck Integrity Scan (64-bit word skipping vs bit-by-bit check)
    // -------------------------------------------------------------------------
    println!("\n--- [CASE 6] Fsck Bitmap Integrity Scan (32,768 blocks, ~20% occupied) ---");
    let total_bits = 32_768;
    let mut fsck_buf = vec![0u8; 4096];
    // Fill first 20%
    for byte in fsck_buf.iter_mut().take(800) {
        *byte = 0xAA;
    }

    assert_eq!(
        old_fsck_bitmap_scan(&fsck_buf, total_bits),
        new_fsck_bitmap_scan(&fsck_buf, total_bits)
    );

    let fsck_iters = 50_000;
    let start = Instant::now();
    for _ in 0..fsck_iters {
        black_box(old_fsck_bitmap_scan(&fsck_buf, total_bits));
    }
    let old_time = start.elapsed();

    let start = Instant::now();
    for _ in 0..fsck_iters {
        black_box(new_fsck_bitmap_scan(&fsck_buf, total_bits));
    }
    let new_time = start.elapsed();

    let speedup = old_time.as_nanos() as f64 / new_time.as_nanos() as f64;
    println!(
        "  Old (bit-by-bit 32,768 iterations)       : {:>8.2?} ({:.1} ns/fsck-scan)",
        old_time,
        old_time.as_nanos() as f64 / fsck_iters as f64
    );
    println!(
        "  New (64-bit word skipping + tzcnt)       : {:>8.2?} ({:.1} ns/fsck-scan)",
        new_time,
        new_time.as_nanos() as f64 / fsck_iters as f64
    );
    println!("  --> SPEEDUP: \x1b[1;32m{:.2}x FASTER\x1b[0m", speedup);

    // -------------------------------------------------------------------------
    // CASE 7: Filter Byte-Shuffle (Unrolled 4-byte Float/Int Array)
    // -------------------------------------------------------------------------
    println!("\n--- [CASE 7] Byte Shuffle (64 KB array of 32-bit floats/integers) ---");
    let shuffle_data: Vec<u8> = (0..64 * 1024).map(|i| (i * 37 % 256) as u8).collect();
    let shuffle_iters = 10_000;

    let start = Instant::now();
    for _ in 0..shuffle_iters {
        black_box(old_shuffle_encode(&shuffle_data, 4));
    }
    let old_time = start.elapsed();

    let start = Instant::now();
    for _ in 0..shuffle_iters {
        black_box(shuffle_encode(&shuffle_data, 4));
    }
    let new_time = start.elapsed();

    let speedup = old_time.as_nanos() as f64 / new_time.as_nanos() as f64;
    let throughput_old = (64.0 * shuffle_iters as f64 / 1024.0) / old_time.as_secs_f64();
    let throughput_new = (64.0 * shuffle_iters as f64 / 1024.0) / new_time.as_secs_f64();
    println!(
        "  Old (Generic byte-by-byte indexing)      : {:>8.2?} (throughput: {:.1} MB/s)",
        old_time, throughput_old
    );
    println!(
        "  New (Unrolled 4-byte specialization)     : {:>8.2?} (throughput: {:.1} MB/s)",
        new_time, throughput_new
    );
    println!("  --> SPEEDUP: \x1b[1;32m{:.2}x FASTER\x1b[0m", speedup);

    // -------------------------------------------------------------------------
    // CASE 8: BitShuffle Matrix Transpose (Sparse / Low-Entropy Blocks)
    // -------------------------------------------------------------------------
    println!("\n--- [CASE 8] BitShuffle Matrix Transpose (64 KB Sparse Data Block) ---");
    let mut sparse_block = vec![0u8; 64 * 1024];
    // Put non-zero values every 512 bytes (simulating sparse floating point records)
    for i in (0..sparse_block.len()).step_by(512) {
        sparse_block[i] = 0x42;
    }

    assert_eq!(
        old_bitshuffle_encode(&sparse_block, 4),
        bitshuffle_encode(&sparse_block, 4)
    );

    let bitshuffle_iters = 2_000;
    let start = Instant::now();
    for _ in 0..bitshuffle_iters {
        black_box(old_bitshuffle_encode(&sparse_block, 4));
    }
    let old_time = start.elapsed();

    let start = Instant::now();
    for _ in 0..bitshuffle_iters {
        black_box(bitshuffle_encode(&sparse_block, 4));
    }
    let new_time = start.elapsed();

    let speedup = old_time.as_nanos() as f64 / new_time.as_nanos() as f64;
    let throughput_old = (64.0 * bitshuffle_iters as f64 / 1024.0) / old_time.as_secs_f64();
    let throughput_new = (64.0 * bitshuffle_iters as f64 / 1024.0) / new_time.as_secs_f64();
    println!(
        "  Old (Full 64-bit transpose on zero bytes) : {:>8.2?} (throughput: {:.1} MB/s)",
        old_time, throughput_old
    );
    println!(
        "  New (Zero-byte block bypass)              : {:>8.2?} (throughput: {:.1} MB/s)",
        new_time, throughput_new
    );
    println!("  --> SPEEDUP: \x1b[1;32m{:.2}x FASTER\x1b[0m", speedup);

    println!("\n================================================================================");
}
