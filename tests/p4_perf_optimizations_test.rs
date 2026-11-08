//! Regression and benchmark suite for P4.17–P4.32 performance optimizations.
//!
//! Run regression tests:
//!   cargo test --release --test p4_perf_optimizations_test
//!
//! Run before/after performance benchmark comparison:
//!   cargo test --release --test p4_perf_optimizations_test bench_p4_wave_after -- --ignored --nocapture

use oifs::DiskManager;
use oifs::disk::{CompressionMode, DefragMode};
use oifs::filters::{
    FilterPipeline, FilterType, bitshuffle_decode, bitshuffle_encode, delta_decode_inplace,
    delta_encode_inplace, shuffle_decode, shuffle_encode,
};
use oifs::inode::INODE_FLAG_SEEKABLE_64K;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const MB: u64 = 1024 * 1024;
static NEXT_IMG_ID: AtomicU64 = AtomicU64::new(1);

struct TempImg(PathBuf);

impl TempImg {
    fn new(tag: &str) -> Self {
        let seq = NEXT_IMG_ID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let p = std::env::temp_dir().join(format!("oifs_p4_opt_{pid}_{seq}_{tag}.img"));
        let _ = std::fs::remove_file(&p);
        Self(p)
    }
}

impl Drop for TempImg {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Deterministic moderately-compressible buffer (identical to baseline probe).
fn bench_data(n: usize) -> Vec<u8> {
    let mut x: u64 = 0x9E3779B97F4A7C15;
    (0..n)
        .map(|i| {
            if i % 4 == 0 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
            }
            ((x >> ((i % 4) * 8)) as u8) & 0x3F
        })
        .collect()
}

fn patterned_bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| {
            ((i / 17) as u8)
                .wrapping_mul(31)
                .wrapping_add((i % 13) as u8)
                ^ seed
        })
        .collect()
}

#[test]
fn test_p424_decoded_blob_cache_coherence_on_same_second_overwrite_and_truncate() {
    for encrypted in [false, true] {
        let img = TempImg::new(if encrypted { "p424_enc" } else { "p424_plain" });
        let dm = if encrypted {
            DiskManager::create_encrypted_journaled(&img.0, 64 * MB, "pw-p424").unwrap()
        } else {
            DiskManager::open_journaled(&img.0, 64 * MB).unwrap()
        };

        let id = dm.create_file(0, "stream_blob.bin").unwrap();
        let v1 = patterned_bytes(256 * 1024, 0x11);
        dm.write_data(id, 0, &v1, CompressionMode::Always).unwrap();

        // Populate and hit DecodedBlobCache with sequential 4 KB read_at calls.
        let mut buf = [0u8; 4096];
        for blk in 0..64 {
            let off = blk * 4096;
            let n = dm.read_at(id, off as u64, &mut buf).unwrap();
            assert_eq!(n, 4096);
            assert_eq!(&buf[..], &v1[off..off + 4096]);
        }

        // Immediate same-second full overwrite with identical length (modified_at & size match!).
        let v2 = patterned_bytes(256 * 1024, 0x7A);
        dm.write_data(id, 0, &v2, CompressionMode::Always).unwrap();
        for blk in [0usize, 1, 31, 63] {
            let off = blk * 4096;
            let n = dm.read_at(id, off as u64, &mut buf).unwrap();
            assert_eq!(n, 4096);
            assert_eq!(
                &buf[..],
                &v2[off..off + 4096],
                "DecodedBlobCache returned stale bytes after same-second overwrite (encrypted={encrypted})"
            );
        }

        // Immediate same-second truncate shrink + extend must also invalidate DecodedBlobCache.
        dm.truncate(id, 10_000).unwrap();
        let n = dm.read_at(id, 8192, &mut buf).unwrap();
        assert_eq!(n, 10_000 - 8192);
        assert_eq!(&buf[..n], &v2[8192..10_000]);

        dm.truncate(id, 16_384).unwrap();
        let n = dm.read_at(id, 10_000, &mut buf).unwrap();
        assert_eq!(n, 4096);
        assert!(buf[..n].iter().all(|&b| b == 0));

        assert!(dm.verify_integrity().unwrap().is_clean);
    }
}

#[test]
fn test_p417_seekable_parallel_read_and_subchunk_cache_coherence() {
    for encrypted in [false, true] {
        let img = TempImg::new(if encrypted { "p417_enc" } else { "p417_plain" });
        let dm = if encrypted {
            DiskManager::create_encrypted_journaled(&img.0, 64 * MB, "pw-p417").unwrap()
        } else {
            DiskManager::open_journaled(&img.0, 64 * MB).unwrap()
        };

        let mode = CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        };
        // 8 full chunks + 15,200-byte tail chunk (> 3 chunks exercises Rayon parallel decode).
        let total_len = 8 * 65536 + 15_200;
        let mut expected = patterned_bytes(total_len, 0x3C);
        let id = dm.create_file(0, "seekable.bin").unwrap();
        dm.write_data(id, 0, &expected, mode).unwrap();

        // 1. Parallel full-file read_data.
        let full = dm.read_data(id).unwrap();
        assert_eq!(full, expected);

        // 2. Parallel chunk-aligned multi-chunk read_at (4 chunks = 256 KB from chunk 2).
        let mut big = vec![0u8; 4 * 65536];
        let n = dm.read_at(id, 2 * 65536, &mut big).unwrap();
        assert_eq!(n, big.len());
        assert_eq!(&big[..], &expected[2 * 65536..6 * 65536]);

        // 3. Unaligned cross-chunk read_at.
        let mut cross = vec![0u8; 70_000];
        let n = dm.read_at(id, 60_000, &mut cross).unwrap();
        assert_eq!(n, 70_000);
        assert_eq!(&cross[..], &expected[60_000..130_000]);

        // 4. Sequential 4 KB sub-chunk read_at (populates and hits ChunkReadCache).
        let mut sub = [0u8; 4096];
        let mut off = 0usize;
        while off < total_len {
            let n = dm.read_at(id, off as u64, &mut sub).unwrap();
            let want = (total_len - off).min(4096);
            assert_eq!(n, want);
            assert_eq!(&sub[..n], &expected[off..off + n]);
            off += 4096;
        }

        // 5. Same-second partial chunk overwrite inside chunk 1 must invalidate ChunkReadCache.
        let patch = patterned_bytes(5000, 0xE5);
        let patch_off = 65536 + 1234;
        dm.write_data(id, patch_off as u64, &patch, mode).unwrap();
        expected[patch_off..patch_off + patch.len()].copy_from_slice(&patch);

        let n = dm.read_at(id, 65536, &mut sub).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(&sub[..], &expected[65536..65536 + 4096]);
        assert_eq!(dm.read_data(id).unwrap(), expected);

        assert!(dm.verify_integrity().unwrap().is_clean);
    }
}

#[test]
fn test_p423_encrypted_nonzero_offset_auto_upgrades_to_seekable_64k() {
    for journaled in [false, true] {
        let img = TempImg::new(if journaled { "p423_wal" } else { "p423_leg" });
        let dm = if journaled {
            DiskManager::create_encrypted_journaled(&img.0, 64 * MB, "pw-p423").unwrap()
        } else {
            DiskManager::create_encrypted(&img.0, 64 * MB, "pw-p423").unwrap()
        };

        // Case A: Sequential 64 KB appends under both Never and Auto modes.
        let chunk = patterned_bytes(65536, 0x42);
        for (name, mode) in [
            ("append_never.bin", CompressionMode::Never),
            ("append_auto.bin", CompressionMode::Auto),
        ] {
            let id = dm.create_file(0, name).unwrap();
            for i in 0..16u64 {
                dm.write_data(id, i * 65536, &chunk, mode).unwrap();
            }
            let ino = dm.read_inode(id).unwrap();
            assert_ne!(
                ino.flags & INODE_FLAG_SEEKABLE_64K,
                0,
                "expected auto-upgrade to seekable 64K for {name} (journaled={journaled})"
            );
            assert!(ino.encrypted);
            assert_eq!(ino.size, 16 * 65536);

            let got = dm.read_data(id).unwrap();
            assert_eq!(got.len(), 16 * 65536);
            for i in 0..16 {
                assert_eq!(&got[i * 65536..(i + 1) * 65536], &chunk[..]);
            }
        }

        // Case B: Non-zero-offset partial splice on an existing multi-block whole-file encrypted blob.
        let id_splice = dm.create_file(0, "whole_then_splice.bin").unwrap();
        let mut expected = patterned_bytes(150_000, 0x91);
        dm.write_data(id_splice, 0, &expected, CompressionMode::Auto)
            .unwrap();
        assert_eq!(
            dm.read_inode(id_splice).unwrap().flags & INODE_FLAG_SEEKABLE_64K,
            0,
            "single offset-0 write should remain whole-file initially"
        );

        let patch = patterned_bytes(30_000, 0x2D);
        dm.write_data(id_splice, 50_000, &patch, CompressionMode::Auto)
            .unwrap();
        expected[50_000..80_000].copy_from_slice(&patch);

        let ino_after = dm.read_inode(id_splice).unwrap();
        assert_ne!(ino_after.flags & INODE_FLAG_SEEKABLE_64K, 0);
        assert!(
            ino_after.compressed_size > 0 && ino_after.compressed_size < ino_after.size,
            "seekable compressed_size should track total chunk bytes, got {}",
            ino_after.compressed_size
        );
        assert_eq!(dm.read_data(id_splice).unwrap(), expected);

        let report = dm.verify_integrity().unwrap();
        assert!(
            report.is_clean,
            "fsck report not clean (journaled={journaled}): {report:?}"
        );

        // Drop and reopen to verify WAL recovery / on-disk persistence.
        drop(dm);
        let dm_reopen = DiskManager::open_with_password(&img.0, 64 * MB, Some("pw-p423")).unwrap();
        assert_eq!(dm_reopen.read_data(id_splice).unwrap(), expected);
        assert!(dm_reopen.verify_integrity().unwrap().is_clean);
    }
}

#[test]
fn test_p420_inplace_defrag_short_circuit_and_front_hole_compaction() {
    let img = TempImg::new("p420_defrag");
    let dm = DiskManager::open(&img.0, 64 * MB).unwrap();
    let src_path = img.0.to_str().unwrap().to_string();

    let payload = patterned_bytes(4096, 0x55);
    let mut ids = Vec::with_capacity(200);
    for i in 0..200 {
        let id = dm.create_file(0, &format!("item_{i:04}.bin")).unwrap();
        dm.write_data(id, 0, &payload, CompressionMode::Never)
            .unwrap();
        ids.push(id);
    }

    // 1. Clean image ([1..1][0..0]): O(1) short-circuit must move 0 bytes!
    let stats_noop = dm.defragment(&src_path, DefragMode::InPlace, None).unwrap();
    assert_eq!(stats_noop.bytes_moved, 0);
    assert_eq!(stats_noop.frag_before, 0.0);
    assert_eq!(stats_noop.frag_after, 0.0);

    // 2. Delete the first 20 files to create a front hole ([0..0][1..1][0..0]).
    for i in 0..20 {
        dm.delete_file(0, &format!("item_{i:04}.bin")).unwrap();
    }
    let stats_compact = dm.defragment(&src_path, DefragMode::InPlace, None).unwrap();
    assert!(stats_compact.bytes_moved > 0);
    assert_eq!(stats_compact.frag_after, 0.0);

    // 3. Verify preserved dir_cache (with remapped phys_blk) supports immediate lookup, delete, and create.
    for (idx, &expected_id) in ids.iter().enumerate().skip(20) {
        let looked_up = dm.lookup(0, &format!("item_{idx:04}.bin")).unwrap();
        assert_eq!(looked_up, expected_id);
        assert_eq!(dm.read_data(expected_id).unwrap(), payload);
    }
    for i in 20..50 {
        dm.delete_file(0, &format!("item_{i:04}.bin")).unwrap();
    }
    for i in 200..230 {
        let id = dm.create_file(0, &format!("item_{i:04}.bin")).unwrap();
        dm.write_data(id, 0, &payload, CompressionMode::Never)
            .unwrap();
    }

    assert!(dm.verify_integrity().unwrap().is_clean);
}

#[test]
fn test_p430_p419_p427_directory_bulk_delete_encrypted_neg_lookup_and_bitset_fsck() {
    for encrypted in [false, true] {
        let img = TempImg::new(if encrypted { "p430_enc" } else { "p430_plain" });
        let dm = if encrypted {
            DiskManager::create_encrypted_journaled(&img.0, 64 * MB, "pw-p430").unwrap()
        } else {
            DiskManager::open_journaled(&img.0, 64 * MB).unwrap()
        };

        let dir_id = dm.create_directory(0, "work").unwrap();
        let count = 500usize;
        for i in 0..count {
            let fid = dm
                .create_file(dir_id, &format!("entry_{i:05}.txt"))
                .unwrap();
            if i % 25 == 0 {
                dm.write_data(fid, 0, b"hello-oifs", CompressionMode::Never)
                    .unwrap();
            }
        }

        // Cold + warm negative lookups exercise plain_complete without cipher or write-lock churn.
        for i in 0..200 {
            assert!(dm.lookup(dir_id, &format!("absent_{i}")).is_err());
        }
        assert_eq!(dm.list_dir(dir_id).unwrap().len(), count);

        // SEC-29: non-empty directory deletion must fail cleanly.
        assert!(dm.delete_file(0, "work").is_err());

        // P4.27 bitset fsck must report clean image.
        let fsck_mid = dm.verify_integrity().unwrap();
        assert!(fsck_mid.is_clean, "fsck_mid failed: {fsck_mid:?}");

        // P4.30 O(1) phys_blk + zero-alloc raw-byte entry compaction on half delete.
        for i in (0..count).step_by(2) {
            dm.delete_file(dir_id, &format!("entry_{i:05}.txt"))
                .unwrap();
        }
        assert_eq!(dm.list_dir(dir_id).unwrap().len(), count / 2);

        // Re-insert into compacted holes, then delete everything + remove directory.
        for i in 0..50 {
            dm.create_file(dir_id, &format!("reused_{i:03}.txt"))
                .unwrap();
        }
        for i in (1..count).step_by(2) {
            dm.delete_file(dir_id, &format!("entry_{i:05}.txt"))
                .unwrap();
        }
        for i in 0..50 {
            dm.delete_file(dir_id, &format!("reused_{i:03}.txt"))
                .unwrap();
        }
        assert!(dm.list_dir(dir_id).unwrap().is_empty());
        dm.delete_file(0, "work").unwrap();

        let fsck_end = dm.verify_integrity().unwrap();
        assert!(fsck_end.is_clean, "fsck_end failed: {fsck_end:?}");

        drop(dm);
        let dm2 = if encrypted {
            DiskManager::open_with_password(&img.0, 64 * MB, Some("pw-p430")).unwrap()
        } else {
            DiskManager::open_journaled(&img.0, 64 * MB).unwrap()
        };
        assert!(dm2.verify_integrity().unwrap().is_clean);
    }
}

#[test]
fn test_p432_simd_delta_decode_and_inplace_bitshuffle_roundtrip() {
    let lengths = [
        0usize, 1, 2, 3, 4, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 511, 4099,
        65_539,
    ];
    for typesize in [1usize, 2, 4, 8] {
        for &len in &lengths {
            let orig = patterned_bytes(len, (typesize as u8).wrapping_mul(19));

            // 1. Delta encode + SIMD prefix-sum decode roundtrip.
            let mut buf = orig.clone();
            delta_encode_inplace(&mut buf, typesize);
            delta_decode_inplace(&mut buf, typesize);
            assert_eq!(
                buf, orig,
                "delta roundtrip mismatch for typesize={typesize}, len={len}"
            );

            // 2. Single-allocation in-place bitshuffle encode/decode roundtrip.
            let bsh = bitshuffle_encode(&orig, typesize);
            let unbsh = bitshuffle_decode(&bsh, typesize);
            assert_eq!(
                unbsh, orig,
                "bitshuffle roundtrip mismatch for typesize={typesize}, len={len}"
            );

            // 3. Combined FilterPipeline (Delta + BitShuffle) single-allocation apply/unapply.
            let pipeline = FilterPipeline::new(typesize as u8)
                .then(FilterType::Delta)
                .then(FilterType::BitShuffle);
            let encoded = pipeline.apply(&orig);
            let decoded = pipeline.unapply(&encoded);
            assert_eq!(
                decoded, orig,
                "FilterPipeline roundtrip mismatch for typesize={typesize}, len={len}"
            );
        }
    }
}

fn elapsed_ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

#[test]
#[ignore]
fn bench_p4_wave_after() {
    println!("\n=== [1] read_at paths (sequential 4 KiB reads over the file) ===");
    for (label, mode, size) in [
        ("raw 32MB", CompressionMode::Never, 32 * MB as usize),
        ("stream 4MB", CompressionMode::Always, 4 * MB as usize),
        (
            "seekable 16MB",
            CompressionMode::Seekable {
                chunk_size: 65536,
                level: 1,
            },
            16 * MB as usize,
        ),
    ] {
        let img = TempImg::new(&label.replace(' ', "_"));
        let dm = DiskManager::open(&img.0, 256 * MB).unwrap();
        let id = dm.create_file(0, "f").unwrap();
        let payload = bench_data(size);
        let t = Instant::now();
        dm.write_data(id, 0, &payload, mode).unwrap();
        let w = elapsed_ms(t);
        let t = Instant::now();
        let full = dm.read_data(id).unwrap();
        let r = elapsed_ms(t);
        assert_eq!(full.len(), size);
        let mut buf = vec![0u8; 4096];
        let t = Instant::now();
        let mut off = 0u64;
        while (off as usize) < size {
            dm.read_at(id, off, &mut buf).unwrap();
            off += 4096;
        }
        let seq = elapsed_ms(t);
        let mut big = vec![0u8; 1 << 20];
        let t = Instant::now();
        let mut off = 0u64;
        while (off as usize) < size {
            dm.read_at(id, off, &mut big).unwrap();
            off += 1 << 20;
        }
        let seq1m = elapsed_ms(t);
        println!(
            "{label:>14} | write {w:8.2} ms | read_data {r:8.2} ms | seq read_at 4K {seq:9.2} ms ({:8.1} MB/s) | 1M {seq1m:7.2} ms",
            size as f64 / MB as f64 / (seq / 1e3)
        );
    }

    println!("\n=== [2] seekable writes ===");
    {
        let img = TempImg::new("seek_w");
        let dm = DiskManager::open(&img.0, 512 * MB).unwrap();
        let id = dm.create_file(0, "f").unwrap();
        let mode = CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        };
        let payload = bench_data(64 * MB as usize);
        let t = Instant::now();
        dm.write_data(id, 0, &payload, mode).unwrap();
        println!("  64MB initial write: {:.2} ms", elapsed_ms(t));
        let t = Instant::now();
        for i in 0..200u64 {
            dm.write_data(id, (i * 7919 * 4096) % (60 * MB), &payload[..4096], mode)
                .unwrap();
        }
        let rw = elapsed_ms(t);
        println!(
            "  200 x 4K random overwrite: {:.2} ms ({:.1} µs/op)",
            rw,
            rw * 5.0
        );
        let id2 = dm.create_file(0, "g").unwrap();
        let t = Instant::now();
        for i in 0..256u64 {
            dm.write_data(id2, i * 65536, &payload[..65536], mode)
                .unwrap();
        }
        println!("  256 x 64K append (16MB): {:.2} ms", elapsed_ms(t));
    }

    println!("\n=== [3] appends (128 x 64K = 8 MB) ===");
    for enc in [false, true] {
        let img = TempImg::new(if enc { "app_enc" } else { "app_plain" });
        let dm = if enc {
            DiskManager::create_encrypted(&img.0, 256 * MB, "pw").unwrap()
        } else {
            DiskManager::open(&img.0, 256 * MB).unwrap()
        };
        let payload = bench_data(65536);
        for (label, mode) in [
            ("never", CompressionMode::Never),
            ("auto", CompressionMode::Auto),
        ] {
            let id = dm.create_file(0, label).unwrap();
            let t = Instant::now();
            for i in 0..128u64 {
                dm.write_data(id, i * 65536, &payload, mode).unwrap();
            }
            let e = elapsed_ms(t);
            println!(
                "  enc={enc:<5} mode={label:<6} 128 x 64K append (8MB): {:8.2} ms ({:7.1} MB/s)",
                e,
                8.0 / (e / 1e3)
            );
        }
    }

    println!("\n=== [4] directory ops (10k files in one dir) ===");
    for enc in [false, true] {
        let img = TempImg::new(if enc { "dir_enc" } else { "dir_plain" });
        let dm = if enc {
            DiskManager::create_encrypted(&img.0, 256 * MB, "pw").unwrap()
        } else {
            DiskManager::open(&img.0, 256 * MB).unwrap()
        };
        let d = dm.create_directory(0, "d").unwrap();
        let n = 10_000;
        let t = Instant::now();
        for i in 0..n {
            dm.create_file(d, &format!("file_{i:06}.dat")).unwrap();
        }
        let c = elapsed_ms(t);
        let t = Instant::now();
        for i in 0..n {
            dm.lookup(d, &format!("file_{i:06}.dat")).unwrap();
        }
        let l = elapsed_ms(t);
        let t = Instant::now();
        for i in 0..n {
            assert!(dm.lookup(d, &format!("missing_{i}")).is_err());
        }
        let nl = elapsed_ms(t);
        let t = Instant::now();
        for _ in 0..20 {
            dm.list_dir(d).unwrap();
        }
        let ld = elapsed_ms(t) / 20.0;
        let t = Instant::now();
        for i in 0..n {
            dm.resolve_path(&format!("/d/file_{i:06}.dat")).unwrap();
        }
        let rp = elapsed_ms(t);
        let t = Instant::now();
        let rep = dm.verify_integrity().unwrap();
        let fsck = elapsed_ms(t);
        assert!(rep.is_clean);
        let t = Instant::now();
        for i in 0..n {
            dm.delete_file(d, &format!("file_{i:06}.dat")).unwrap();
        }
        let del = elapsed_ms(t);
        println!(
            "  enc={enc:<5} create {c:8.2} ms | lookup {l:7.2} | neg-lookup {nl:7.2} | list_dir {ld:6.2} | resolve_path {rp:7.2} | fsck {fsck:7.2} | delete {del:8.2} ms"
        );
    }

    println!("\n=== [5] filters (16 MB, typesize=4) & stream read_data & clean defrag ===");
    let raw = bench_data(16 * MB as usize);
    let mut d = raw.clone();
    let t = Instant::now();
    delta_encode_inplace(&mut d, 4);
    let de = elapsed_ms(t);
    let t = Instant::now();
    delta_decode_inplace(&mut d, 4);
    let dd = elapsed_ms(t);
    assert_eq!(d, raw);

    let t = Instant::now();
    let sh = shuffle_encode(&raw, 4);
    let se = elapsed_ms(t);
    let t = Instant::now();
    let unsh = shuffle_decode(&sh, 4);
    let sd = elapsed_ms(t);
    assert_eq!(unsh, raw);

    let t = Instant::now();
    let bsh = bitshuffle_encode(&raw, 4);
    let bse = elapsed_ms(t);
    let t = Instant::now();
    let unbsh = bitshuffle_decode(&bsh, 4);
    let bsd = elapsed_ms(t);
    assert_eq!(unbsh, raw);

    println!(
        "  delta enc {de:6.2} ms ({:7.1} MB/s) | dec {dd:6.2} ms ({:7.1} MB/s)",
        16.0 / (de / 1e3),
        16.0 / (dd / 1e3)
    );
    println!(
        "  shuffle enc {se:6.2} ms ({:7.1} MB/s) | dec {sd:6.2} ms ({:7.1} MB/s)",
        16.0 / (se / 1e3),
        16.0 / (sd / 1e3)
    );
    println!(
        "  bitshuffle enc {bse:6.2} ms ({:7.1} MB/s) | dec {bsd:6.2} ms ({:7.1} MB/s)",
        16.0 / (bse / 1e3),
        16.0 / (bsd / 1e3)
    );

    let img_s = TempImg::new("stream_small");
    let dm_s = DiskManager::open(&img_s.0, 128 * MB).unwrap();
    let id_s = dm_s.create_file(0, "s").unwrap();
    dm_s.write_data(id_s, 0, &raw[..16384], CompressionMode::Always)
        .unwrap();
    let t = Instant::now();
    for _ in 0..1000 {
        let out = dm_s.read_data(id_s).unwrap();
        assert_eq!(out.len(), 16384);
    }
    let st_ms = elapsed_ms(t);
    println!(
        "  1000 x 16KB stream read_data: {:.2} ms ({:.2} µs/op, {:.1} MB/s)",
        st_ms,
        st_ms,
        (1000.0 * 16384.0 / MB as f64) / (st_ms / 1e3)
    );

    let img_d = TempImg::new("defrag_noop");
    let dm_d = DiskManager::open(&img_d.0, 256 * MB).unwrap();
    for i in 0..5000 {
        let fid = dm_d.create_file(0, &format!("f{i}")).unwrap();
        dm_d.write_data(fid, 0, &raw[..512], CompressionMode::Never)
            .unwrap();
    }
    let src_str = img_d.0.to_str().unwrap();
    let t = Instant::now();
    let st = dm_d.defragment(src_str, DefragMode::InPlace, None).unwrap();
    let df_ms = elapsed_ms(t);
    println!(
        "  defragment(InPlace, moved={} bytes): {:.3} ms",
        st.bytes_moved, df_ms
    );
}
