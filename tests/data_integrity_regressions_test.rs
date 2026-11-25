//! End-to-end regression tests for core data integrity, cache coherency,
//! WAL pointer pruning, filtered reads, and bounded allocator wraparound.

use oifs::disk::{CompressionMode, DiskManager, DurabilityMode, SpliceOptions, SpliceTrimMode};
use oifs::filters::FilterConfig;
use oifs::storage::StorageMode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use tempfile::tempdir;

fn pseudo_random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push((state >> 24) as u8);
    }
    out
}

#[test]
fn test_truncate_expand_block_aligned_preserves_last_block() {
    let dir = tempdir().unwrap();

    for (journaled, mode) in [
        (false, StorageMode::Mmap),
        (true, StorageMode::Mmap),
        (false, StorageMode::FileIo),
        (true, StorageMode::FileIo),
    ] {
        let img = dir
            .path()
            .join(format!("trunc_aligned_{}_{:?}.img", journaled, mode));
        let dm =
            DiskManager::open_with_options(&img, 8 * 1024 * 1024, None, journaled, mode).unwrap();
        dm.set_durability_mode(DurabilityMode::Strict);
        let root = dm.superblock().root_inode;

        // 1 block exact (4096 bytes)
        let f1 = dm.create_file(root, "one_block.bin").unwrap();
        let data1: Vec<u8> = (0..4096).map(|i| ((i * 17 + 5) & 0xFF) as u8).collect();
        dm.write_data(f1, 0, &data1, CompressionMode::Never)
            .unwrap();
        dm.truncate(f1, 8192).unwrap();

        let got1 = dm.read_data(f1).unwrap();
        assert_eq!(got1.len(), 8192);
        assert_eq!(
            &got1[..4096],
            &data1[..],
            "4096 -> 8192 expand wiped last block (journaled={journaled}, mode={mode:?})"
        );
        assert!(got1[4096..].iter().all(|&b| b == 0));

        // 2 blocks exact (8192 bytes)
        let f2 = dm.create_file(root, "two_blocks.bin").unwrap();
        let data2: Vec<u8> = (0..8192).map(|i| ((i * 29 + 11) & 0xFF) as u8).collect();
        dm.write_data(f2, 0, &data2, CompressionMode::Never)
            .unwrap();
        dm.truncate(f2, 12288).unwrap();

        let got2 = dm.read_data(f2).unwrap();
        assert_eq!(got2.len(), 12288);
        assert_eq!(
            &got2[..8192],
            &data2[..],
            "8192 -> 12288 expand wiped last block (journaled={journaled}, mode={mode:?})"
        );
        assert!(got2[8192..].iter().all(|&b| b == 0));
    }
}

#[test]
fn test_journaled_double_indirect_shrink_preserves_data_and_fsck() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("journaled_double_shrink.img");
    let dm = DiskManager::open_journaled(&img, 16 * 1024 * 1024).unwrap();
    let root = dm.superblock().root_inode;
    let fid = dm.create_file(root, "big.bin").unwrap();

    // Incompressible > 522 blocks written with CompressionMode::Always enters Double Indirect
    // (530 blocks = 2,170,880 bytes) and triggers FullOverwrite shrinking on subsequent overwrites.
    let big = pseudo_random_bytes(530 * 4096, 0x1010);
    dm.write_data(fid, 0, &big, CompressionMode::Always)
        .unwrap();
    assert_eq!(dm.read_data(fid).unwrap(), big);
    assert!(dm.verify_integrity().unwrap().is_clean);

    // Shrink slightly within double-indirect (525 blocks -> boundary SIB inside DIB[0])
    let mid = pseudo_random_bytes(525 * 4096, 0x2020);
    dm.write_data(fid, 0, &mid, CompressionMode::Always)
        .unwrap();
    assert_eq!(dm.read_data(fid).unwrap(), mid);
    assert!(dm.verify_integrity().unwrap().is_clean);

    // Shrink across DOUBLE_SPAN -> SINGLE_SPAN boundary (100 blocks)
    let small_sib = pseudo_random_bytes(100 * 4096, 0x3030);
    dm.write_data(fid, 0, &small_sib, CompressionMode::Always)
        .unwrap();
    assert_eq!(dm.read_data(fid).unwrap(), small_sib);
    assert!(
        dm.verify_integrity().unwrap().is_clean,
        "journaled double->single shrink must leave DIB pruned cleanly"
    );

    // Shrink down to direct blocks (5 blocks)
    let tiny = vec![0xABu8; 5 * 4096];
    dm.write_data(fid, 0, &tiny, CompressionMode::Always)
        .unwrap();
    assert_eq!(dm.read_data(fid).unwrap(), tiny);
    assert!(dm.verify_integrity().unwrap().is_clean);
}

#[test]
fn test_uncompressed_filtered_read_at_and_splice() {
    let dir = tempdir().unwrap();
    let src_img = dir.path().join("src_filtered.img");
    let dst_img = dir.path().join("dst_unfiltered.img");
    let src_dm = DiskManager::open(&src_img, 8 * 1024 * 1024).unwrap();
    let dst_dm = DiskManager::open(&dst_img, 8 * 1024 * 1024).unwrap();
    let src_root = src_dm.superblock().root_inode;
    let dst_root = dst_dm.superblock().root_inode;

    let payload: Vec<u8> = (0..4096)
        .flat_map(|i| (i as u32 * 7 + 1000).to_le_bytes())
        .collect();
    let configs = [
        ("delta.bin", FilterConfig::delta_only(4)),
        ("shuffle.bin", FilterConfig::shuffle_only(4)),
        ("bitshuffle.bin", FilterConfig::bitshuffle_only(4)),
    ];

    for (name, cfg) in configs {
        let sid = src_dm.create_file(src_root, name).unwrap();
        src_dm
            .write_data_with_filters(sid, 0, &payload, CompressionMode::Never, cfg)
            .unwrap();
        let sino = src_dm.read_inode(sid).unwrap();
        assert_eq!(sino.compressed_size, 0);

        // read_data, read_at, FileReadHandle::read_at, and read_at_async must all unapply filters
        assert_eq!(src_dm.read_data(sid).unwrap(), payload);

        let mut slice_buf = vec![0u8; 256];
        let n = src_dm.read_at(sid, 1024, &mut slice_buf).unwrap();
        assert_eq!(n, 256);
        assert_eq!(&slice_buf[..], &payload[1024..1280]);

        {
            let sess = src_dm.begin_read();
            let fh = sess.open_file(sid).unwrap();
            let mut fh_buf = vec![0u8; 256];
            let m = fh.read_at(2048, &mut fh_buf).unwrap();
            assert_eq!(m, 256);
            assert_eq!(&fh_buf[..], &payload[2048..2304]);
        }

        let async_got = src_dm.read_at_async(sid, 4096, 512).wait().unwrap();
        assert_eq!(async_got, payload[4096..4608]);

        // splice_range_to into an unfiltered destination must use LogicalReencode (not raw block copy)
        let did = dst_dm.create_file(dst_root, name).unwrap();
        src_dm
            .splice_range_to(
                sid,
                4096,
                &dst_dm,
                did,
                0,
                8192,
                SpliceOptions {
                    trim_mode: SpliceTrimMode::Keep,
                },
            )
            .unwrap();
        assert_eq!(dst_dm.read_data(did).unwrap(), payload[4096..12288]);
    }
}

#[test]
fn test_non_journaled_seekable_overwrite_invalidates_chunk_read_cache() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("seekable_cache_inv.img");
    let dm = DiskManager::open(&img, 16 * 1024 * 1024).unwrap();
    let root = dm.superblock().root_inode;
    let fid = dm.create_file(root, "seek.bin").unwrap();

    // Incompressible 128 KiB -> stored as FLAG_RAW chunks so same-length overwrite keeps ChunkEntry identical!
    let mut initial = pseudo_random_bytes(128 * 1024, 0xCAFE_BABE);
    dm.write_data(
        fid,
        0,
        &initial,
        CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        },
    )
    .unwrap();

    // Warm ChunkReadCache with sub-chunk read_at
    let mut sub = vec![0u8; 4096];
    assert_eq!(dm.read_at(fid, 1024, &mut sub).unwrap(), 4096);
    assert_eq!(&sub[..], &initial[1024..5120]);

    // Overwrite chunk 0 with another incompressible chunk of the exact same length (reuses in-place)
    let replacement = pseudo_random_bytes(65536, 0xDEAD_BEEF);
    dm.write_data(
        fid,
        0,
        &replacement,
        CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        },
    )
    .unwrap();
    initial[..65536].copy_from_slice(&replacement);

    let mut sub_after = vec![0u8; 4096];
    assert_eq!(dm.read_at(fid, 1024, &mut sub_after).unwrap(), 4096);
    assert_eq!(
        &sub_after[..],
        &replacement[1024..5120],
        "ChunkReadCache returned stale bytes after non-journaled in-place overwrite"
    );

    // Shrinking truncate must also invalidate ChunkReadCache
    dm.truncate(fid, 5000).unwrap();
    let mut tail = vec![0u8; 4096];
    let n = dm.read_at(fid, 1024, &mut tail).unwrap();
    assert_eq!(n, 3976);
    assert_eq!(&tail[..n], &replacement[1024..5000]);
}

#[test]
fn test_encrypted_seekable_sparse_expansion_and_gap_write() {
    let dir = tempdir().unwrap();

    for journaled in [false, true] {
        let img = dir
            .path()
            .join(format!("enc_seekable_sparse_{journaled}.img"));
        let dm = if journaled {
            DiskManager::create_encrypted_journaled(&img, 16 * 1024 * 1024, "secret_pw").unwrap()
        } else {
            DiskManager::create_encrypted(&img, 16 * 1024 * 1024, "secret_pw").unwrap()
        };
        let root = dm.superblock().root_inode;
        let seek_mode = CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        };

        // Case A: normal partial encrypted FLAG_RAW tail (1,000 B -> compressed_len = 1040 < 65535)
        let f_small = dm.create_file(root, "small_tail.bin").unwrap();
        let small_data = pseudo_random_bytes(1000, 0x1111_2222);
        dm.write_data(f_small, 0, &small_data, seek_mode).unwrap();
        dm.truncate(f_small, 140_000).unwrap();

        let got_small = dm.read_data(f_small).unwrap();
        assert_eq!(got_small.len(), 140_000);
        assert_eq!(&got_small[..1000], &small_data[..]);
        assert!(got_small[1000..].iter().all(|&b| b == 0));

        // Gap-skipping write at offset 128 KiB on another 1000B encrypted seekable file
        let f_gap_small = dm.create_file(root, "gap_small.bin").unwrap();
        dm.write_data(f_gap_small, 0, &small_data, seek_mode)
            .unwrap();
        let gap_patch = pseudo_random_bytes(4096, 0x3333_4444);
        dm.write_data(f_gap_small, 128 * 1024, &gap_patch, seek_mode)
            .unwrap();
        let got_gap_small = dm.read_data(f_gap_small).unwrap();
        assert_eq!(got_gap_small.len(), 128 * 1024 + 4096);
        assert_eq!(&got_gap_small[..1000], &small_data[..]);
        assert!(got_gap_small[1000..128 * 1024].iter().all(|&b| b == 0));
        assert_eq!(&got_gap_small[128 * 1024..], &gap_patch[..]);

        // Case B: saturated boundary (65,500 B -> 65500 + 40 = 65540, clamped to 65535 in ChunkEntry)
        let f_sat = dm.create_file(root, "sat_tail.bin").unwrap();
        let sat_data = pseudo_random_bytes(65_500, 0x5555_6666);
        dm.write_data(f_sat, 0, &sat_data, seek_mode).unwrap();
        dm.truncate(f_sat, 150_000).unwrap();

        let got_sat = dm.read_data(f_sat).unwrap();
        assert_eq!(got_sat.len(), 150_000);
        assert_eq!(&got_sat[..65_500], &sat_data[..]);
        assert!(got_sat[65_500..].iter().all(|&b| b == 0));

        // Gap-skipping write on saturated boundary chunk
        let f_gap_sat = dm.create_file(root, "gap_sat.bin").unwrap();
        dm.write_data(f_gap_sat, 0, &sat_data, seek_mode).unwrap();
        dm.write_data(f_gap_sat, 128 * 1024, &gap_patch, seek_mode)
            .unwrap();
        let got_gap_sat = dm.read_data(f_gap_sat).unwrap();
        assert_eq!(got_gap_sat.len(), 128 * 1024 + 4096);
        assert_eq!(&got_gap_sat[..65_500], &sat_data[..]);
        assert!(got_gap_sat[65_500..128 * 1024].iter().all(|&b| b == 0));
        assert_eq!(&got_gap_sat[128 * 1024..], &gap_patch[..]);

        let mut pbuf = vec![0u8; 4096];
        assert_eq!(dm.read_at(f_gap_sat, 64 * 1024, &mut pbuf).unwrap(), 4096);
        assert_eq!(&pbuf[..1024], &vec![0u8; 1024][..]);
    }
}

#[test]
fn test_data_page_cache_concurrent_resize_clear_coherency() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("page_cache_race.img");
    let dm = Arc::new(DiskManager::open_file_io(&img, 16 * 1024 * 1024).unwrap());
    let root = dm.superblock().root_inode;
    let fid = dm.create_file(root, "concurrent_file.bin").unwrap();

    // Write 16 blocks (64 KiB) of tag = 1
    let mut v1 = vec![1u8; 16 * 4096];
    dm.write_data(fid, 0, &v1, CompressionMode::Never).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..4 {
        let dm_c = Arc::clone(&dm);
        let stop_c = Arc::clone(&stop);
        readers.push(thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            let mut iter = 0u64;
            while !stop_c.load(Ordering::Relaxed) {
                let off = (iter % 16) * 4096;
                let n = dm_c.read_at(fid, off, &mut buf).unwrap();
                assert_eq!(n, 4096);
                let first = buf[0];
                assert!(buf.iter().all(|&b| b == first), "torn block read observed");
                iter += 1;
            }
        }));
    }

    let dm_resizer = Arc::clone(&dm);
    let stop_resizer = Arc::clone(&stop);
    let resizer = thread::spawn(move || {
        for i in 0..40 {
            if i % 2 == 0 {
                dm_resizer.set_file_io_cache_bytes(0);
            } else {
                dm_resizer.set_file_io_cache_bytes(1024 * 1024);
            }
            thread::yield_now();
        }
        stop_resizer.store(true, Ordering::Relaxed);
    });

    for round in 2..=12u8 {
        v1.fill(round);
        dm.write_data(fid, 0, &v1, CompressionMode::Never).unwrap();
    }

    resizer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }

    // Disabling cache must drain used_bytes cleanly and subsequent overwrites must remain coherent
    dm.set_file_io_cache_bytes(1024 * 1024);
    let mut probe = vec![0u8; 4096];
    assert_eq!(dm.read_at(fid, 0, &mut probe).unwrap(), 4096);
    dm.set_file_io_cache_bytes(0);
    assert_eq!(dm.file_io_cache_used_bytes(), 0);

    dm.set_file_io_cache_bytes(1024 * 1024);
    let _ = dm.read_at(fid, 0, &mut probe).unwrap();
    v1.fill(0xAA);
    dm.write_data(fid, 0, &v1, CompressionMode::Never).unwrap();
    assert_eq!(dm.read_at(fid, 0, &mut probe).unwrap(), 4096);
    assert!(probe.iter().all(|&b| b == 0xAA));
}

#[test]
fn test_allocator_hint_wraparound_no_false_nospace() {
    let dir = tempdir().unwrap();

    for journaled in [false, true] {
        let img = dir.path().join(format!("wrap_alloc_{journaled}.img"));
        let total_size = if journaled {
            // 36 metadata + inode table + ~40 data blocks -> 1 MiB (256 blocks)
            1024 * 1024
        } else {
            // Small ~256 KiB (64 blocks)
            256 * 1024
        };
        let dm = if journaled {
            DiskManager::open_journaled(&img, total_size).unwrap()
        } else {
            DiskManager::open(&img, total_size).unwrap()
        };
        let root = dm.superblock().root_inode;

        // Allocate single-block files until NoSpace pushes free_block_hint to usable block_count edge.
        let mut names = Vec::new();
        let one_block = vec![0x42u8; 4096];
        for idx in 0..256 {
            let name = format!("f_{idx}.bin");
            match dm.create_file_with_data(root, &name, &one_block, CompressionMode::Never) {
                Ok(_) => names.push(name),
                Err(_) => break,
            }
        }
        assert!(
            names.len() >= 10,
            "expected at least 10 files, got {}",
            names.len()
        );

        // Delete several early files (including contiguous pair) so holes exist strictly BEFORE hint.
        dm.delete_file(root, &names[0]).unwrap();
        dm.delete_file(root, &names[1]).unwrap();
        dm.delete_file(root, &names[2]).unwrap();
        dm.delete_file(root, &names[4]).unwrap();

        // Allocating both a single block file AND a 2-block contiguous seekable chunk must wrap around and succeed!
        let f_single = dm
            .create_file_with_data(
                root,
                "wrapped_single.bin",
                &one_block,
                CompressionMode::Never,
            )
            .expect("single block allocation must wrap around instead of returning NoSpace");
        assert_eq!(dm.read_data(f_single).unwrap(), one_block);

        let f_contig = dm.create_file(root, "wrapped_contig.bin").unwrap();
        let two_blk_incomp = pseudo_random_bytes(8192, 0x9988_7766);
        dm.write_data(
            f_contig,
            0,
            &two_blk_incomp,
            CompressionMode::Seekable {
                chunk_size: 65536,
                level: 1,
            },
        )
        .expect("contiguous block allocation must wrap around instead of returning NoSpace");
        assert_eq!(dm.read_data(f_contig).unwrap(), two_blk_incomp);
    }
}
