use oifs::disk::{CompressionMode, DiskManager, DurabilityMode};
use oifs::session::OifsSession;
use std::sync::Arc;
use std::time::Instant;
use tempfile::tempdir;

const MB: u64 = 1024 * 1024;

#[test]
#[ignore]
fn bench_range_async_journal_coalesce_and_ipc_pool() {
    let dir = tempdir().unwrap();

    // 1. RangeAsync small-file create + write (500 files x 512B) and medium writes (100 x 64KB)
    {
        let p_small = dir.path().join("rasync_small.img");
        let dm = DiskManager::open_journaled(&p_small, 128 * MB)
            .unwrap()
            .with_durability_mode(DurabilityMode::RangeAsync);
        let root = dm.superblock().root_inode;
        let payload = vec![0x5Au8; 512];
        let t0 = Instant::now();
        for i in 0..500 {
            let f = dm.create_file(root, &format!("f_{i}")).unwrap();
            dm.write_data(f, 0, &payload, CompressionMode::Never)
                .unwrap();
        }
        dm.flush().unwrap();
        let dt_small = t0.elapsed();

        let p_med = dir.path().join("rasync_med.img");
        let dm_med = DiskManager::open_journaled(&p_med, 128 * MB)
            .unwrap()
            .with_durability_mode(DurabilityMode::RangeAsync);
        let root_med = dm_med.superblock().root_inode;
        let med_payload = vec![0xA5u8; 64 * 1024];
        let t1 = Instant::now();
        for i in 0..100 {
            let f = dm_med.create_file(root_med, &format!("m_{i}")).unwrap();
            dm_med
                .write_data(f, 0, &med_payload, CompressionMode::Never)
                .unwrap();
        }
        dm_med.flush().unwrap();
        let dt_med = t1.elapsed();

        println!(
            "[RangeAsync] 500x512B (+flush): {:?} ({:.0} files/s) | 100x64KB (+flush): {:?} ({:.1} MB/s)",
            dt_small,
            500.0 / dt_small.as_secs_f64(),
            dt_med,
            (100.0 * 64.0 / 1024.0) / dt_med.as_secs_f64()
        );
    }

    // 2. Journaled in-place 4KB overwrites on a 256MB v3 image + 16MB journaled delete
    {
        let p_j = dir.path().join("journal_coalesce.img");
        let dm = DiskManager::open_journaled(&p_j, 256 * MB).unwrap();
        let root = dm.superblock().root_inode;
        let f = dm.create_file(root, "hot.bin").unwrap();
        let init = vec![0x11u8; 64 * 1024];
        dm.write_data(f, 0, &init, CompressionMode::Never).unwrap();

        let page = vec![0x22u8; 4096];
        let t0 = Instant::now();
        for i in 0..5000usize {
            let off = ((i % 16) * 4096) as u64;
            dm.write_data(f, off, &page, CompressionMode::Never)
                .unwrap();
        }
        let dt_ow = t0.elapsed();

        let big_payload = vec![0x33u8; 16 * 1024 * 1024];
        let mut big_names = Vec::new();
        for i in 0..8 {
            let name = format!("big_{i}.bin");
            let bf = dm.create_file(root, &name).unwrap();
            dm.write_data(bf, 0, &big_payload, CompressionMode::Never)
                .unwrap();
            big_names.push(name);
        }
        let t1 = Instant::now();
        for name in &big_names {
            dm.delete_file(root, name).unwrap();
        }
        let dt_del = t1.elapsed();

        println!(
            "[Journal Coalesce] 5000x4KB in-place overwrite (256MB img): {:?} ({:.0} ops/s) | 8x16MB delete: {:?}",
            dt_ow,
            5000.0 / dt_ow.as_secs_f64(),
            dt_del
        );
    }

    // 3. Multi-threaded OifsSession Remote Proxy (8 threads x 300 ReadAt RPCs of 32KB)
    {
        let p_ipc = dir.path().join("ipc_pool_bench.img");
        let master = OifsSession::open_ipc(&p_ipc, 64 * MB).unwrap();
        let root = master.resolve_path(".").unwrap();
        let fid = master.create_file(root, "shared.bin").unwrap();
        let data = vec![0x77u8; 256 * 1024];
        master
            .write_data(fid, 0, &data, CompressionMode::Never)
            .unwrap();

        let proxy = Arc::new(OifsSession::open_ipc(&p_ipc, 0).unwrap());
        assert!(!proxy.is_direct());

        // Warmup
        {
            let mut wbuf = vec![0u8; 32 * 1024];
            proxy.read_at(fid, 0, &mut wbuf).unwrap();
        }

        let t0 = Instant::now();
        let mut handles = Vec::new();
        for t in 0..8usize {
            let prx = proxy.clone();
            handles.push(std::thread::spawn(move || {
                let mut buf = vec![0u8; 32 * 1024];
                for i in 0..300usize {
                    let off = (((t + i) % 8) * 32 * 1024) as u64;
                    let n = prx.read_at(fid, off, &mut buf).unwrap();
                    assert_eq!(n, 32 * 1024);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let dt_ipc = t0.elapsed();
        println!(
            "[IPC Proxy Pool] 8 threads x 300 x 32KB read_at (2400 RPCs): {:?} ({:.0} RPC/s, {:.1} MB/s)",
            dt_ipc,
            2400.0 / dt_ipc.as_secs_f64(),
            (2400.0 * 32.0 / 1024.0) / dt_ipc.as_secs_f64()
        );
    }
}

#[test]
fn test_range_async_batching_mode_transition_and_recovery() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("range_async_batch_regression.img");

    let dm = DiskManager::open_journaled(&img_path, 64 * MB)
        .unwrap()
        .with_durability_mode(DurabilityMode::RangeAsync);
    let root = dm.superblock().root_inode;

    // Exceed RANGE_ASYNC_BATCH_OPS (64) and RANGE_ASYNC_BATCH_BYTES (1 MiB) across
    // mixed create, write, overwrite, truncate, and delete operations.
    let mut ids = Vec::new();
    for i in 0..96usize {
        let id = dm.create_file(root, &format!("file_{i}.bin")).unwrap();
        let data = vec![(i & 0xFF) as u8; 16 * 1024];
        dm.write_data(id, 0, &data, CompressionMode::Never).unwrap();
        ids.push(id);
    }

    // Overwrite and truncate subset while still in RangeAsync
    for &id in &ids[..16] {
        dm.write_data(id, 4096, &[0xEE; 4096], CompressionMode::Never)
            .unwrap();
        dm.truncate(id, 8192).unwrap();
    }
    for i in 80..96usize {
        dm.delete_file(root, &format!("file_{i}.bin")).unwrap();
    }

    // Transition RangeAsync -> Lazy (must drain any remaining buffered ranges cleanly)
    dm.set_durability_mode(DurabilityMode::Lazy);
    assert_eq!(dm.durability_mode(), DurabilityMode::Lazy);

    // Switch back to RangeAsync, write more, then explicit flush() + reopen + fsck
    dm.set_durability_mode(DurabilityMode::RangeAsync);
    let tail_id = dm.create_file(root, "tail.bin").unwrap();
    dm.write_data(tail_id, 0, &[0xAB; 65536], CompressionMode::Auto)
        .unwrap();
    dm.flush().unwrap();
    drop(dm);

    let dm_reopen = DiskManager::open(&img_path, 0).unwrap();
    let report = dm_reopen.verify_integrity().unwrap();
    assert!(
        report.is_clean,
        "fsck must be clean after RangeAsync reopen: {report:?}"
    );
    assert_eq!(dm_reopen.read_data(ids[0]).unwrap().len(), 8192);
    assert_eq!(
        &dm_reopen.read_data(ids[0]).unwrap()[4096..8192],
        &[0xEE; 4096]
    );
    assert_eq!(dm_reopen.read_data(tail_id).unwrap(), vec![0xAB; 65536]);
}

#[test]
fn test_journaled_coalesced_large_write_truncate_and_delete() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("journal_coalesce_regression.img");

    let dm = DiskManager::open_journaled(&img_path, 128 * MB).unwrap();
    let root = dm.superblock().root_inode;

    // Allocate multi-MB raw file spanning direct + single-indirect + double-indirect tiers (~4 MB = 1024 blocks)
    let fid = dm.create_file(root, "multi_tier.bin").unwrap();
    let initial: Vec<u8> = (0..(4 * 1024 * 1024))
        .map(|i| ((i * 7 + 3) & 0xFF) as u8)
        .collect();
    dm.write_data(fid, 0, &initial, CompressionMode::Never)
        .unwrap();

    // In-place sub-range overwrites across direct and indirect blocks
    for blk in [0usize, 9, 10, 250, 521, 522, 900, 1023] {
        let off = (blk * 4096) as u64;
        dm.write_data(fid, off, &[0x99u8; 4096], CompressionMode::Never)
            .unwrap();
    }

    // Truncate across double -> single -> direct tiers
    dm.truncate(fid, 600 * 4096).unwrap();
    dm.truncate(fid, 20 * 4096).unwrap();
    dm.truncate(fid, 4 * 4096).unwrap();

    let read_back = dm.read_data(fid).unwrap();
    assert_eq!(read_back.len(), 4 * 4096);
    assert_eq!(&read_back[..4096], &[0x99u8; 4096]);
    assert_eq!(&read_back[4096..8192], &initial[4096..8192]);

    // Also exercise seekable 64K journaled write + shrink + delete
    let sfid = dm.create_file(root, "seekable.bin").unwrap();
    dm.write_data(
        sfid,
        0,
        &initial[..512 * 1024],
        CompressionMode::Seekable {
            level: 1,
            chunk_size: 65536,
        },
    )
    .unwrap();
    dm.truncate(sfid, 100 * 1024).unwrap();
    assert_eq!(dm.read_data(sfid).unwrap(), &initial[..100 * 1024]);

    dm.delete_file(root, "multi_tier.bin").unwrap();
    dm.delete_file(root, "seekable.bin").unwrap();

    let report = dm.verify_integrity().unwrap();
    assert!(
        report.is_clean,
        "fsck must be clean after coalesced journal ops: {report:?}"
    );
    drop(dm);

    // Reopen to verify WAL recovery / checkpoint invariants
    let dm2 = DiskManager::open(&img_path, 0).unwrap();
    assert!(dm2.verify_integrity().unwrap().is_clean);
}

#[test]
fn test_ipc_pooled_concurrent_uds_and_tcp_encrypted() {
    let dir = tempdir().unwrap();

    // 1. Local UDS IPC with >64KB frames (`read_framed` direct streaming) + 8 concurrent proxy threads
    {
        let uds_img = dir.path().join("ipc_pool_uds.img");
        let master = OifsSession::open_ipc(&uds_img, 64 * MB).unwrap();
        let root = master.resolve_path(".").unwrap();
        let fid = master.create_file(root, "large_frame.bin").unwrap();
        let payload: Vec<u8> = (0..(300 * 1024)).map(|i| (i % 251) as u8).collect();
        master
            .write_data(fid, 0, &payload, CompressionMode::Never)
            .unwrap();

        let proxy = Arc::new(OifsSession::open_ipc(&uds_img, 0).unwrap());
        assert!(!proxy.is_direct());

        let mut handles = Vec::new();
        for t in 0..8usize {
            let prx = proxy.clone();
            let expected = payload.clone();
            handles.push(std::thread::spawn(move || {
                // Multi-chunk (>64 KiB) read_at RPCs exercising direct FRAME_READ_BUF streaming
                let mut buf = vec![0u8; 160 * 1024];
                for iter in 0..16usize {
                    let off = ((t * 4096 + iter * 8192) % (120 * 1024)) as u64;
                    let n = prx.read_at(fid, off, &mut buf).unwrap();
                    assert_eq!(n, buf.len());
                    assert_eq!(&buf[..], &expected[off as usize..off as usize + n]);
                }
                // Application-level error (NotFound) must keep pooled stream synchronized & reusable!
                assert!(prx.lookup(root, "non_existent_entry.xyz").is_err());
                assert_eq!(prx.lookup(root, "large_frame.bin").unwrap(), fid);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    // 2. Encrypted Network TCP IPC with concurrent pooled secondary authentication (cached KDF key)
    {
        let tcp_img = dir.path().join("ipc_pool_tcp_enc.img");
        let pw = "Wave2PoolSecret#2026";
        let master = OifsSession::create_encrypted_network(&tcp_img, 64 * MB, None, pw).unwrap();
        let root = master.resolve_path(".").unwrap();
        let dir_id = master.create_directory(root, "encrypted_dir").unwrap();
        for i in 0..12usize {
            let f = master
                .create_file(dir_id, &format!("item_{i}.dat"))
                .unwrap();
            master
                .write_data(f, 0, &[i as u8; 24 * 1024], CompressionMode::Auto)
                .unwrap();
        }

        let proxy =
            Arc::new(OifsSession::open_network_with_password(&tcp_img, 0, None, Some(pw)).unwrap());
        assert!(!proxy.is_direct());

        let mut handles = Vec::new();
        for t in 0..8usize {
            let prx = proxy.clone();
            handles.push(std::thread::spawn(move || {
                for step in 0..12usize {
                    let idx = (t + step) % 12;
                    let fid = prx.lookup(dir_id, &format!("item_{idx}.dat")).unwrap();
                    let mut sub = vec![0u8; 8192];
                    let n = prx.read_at(fid, 4096, &mut sub).unwrap();
                    assert_eq!(n, 8192);
                    assert_eq!(sub, vec![idx as u8; 8192]);
                }
                let entries = prx.list_dir(dir_id).unwrap();
                assert_eq!(entries.len(), 12);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }
}

#[test]
fn test_hot_directory_survives_cold_inode_scan_flood() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("inode_clock_eviction.img");
    let dm = DiskManager::open(&img_path, 64 * MB).unwrap();
    let root = dm.superblock().root_inode;

    let hot_dir = dm.create_directory(root, "hot").unwrap();
    let hot_file = dm.create_file(hot_dir, "pinned.txt").unwrap();
    dm.write_data(hot_file, 0, b"keep-me-warm", CompressionMode::Never)
        .unwrap();

    // Create > INODE_CACHE_CAPACITY (2048) cold files while periodically touching `hot_dir` & `hot_file`
    let cold_dir = dm.create_directory(root, "cold").unwrap();
    for i in 0..2300usize {
        if i % 64 == 0 {
            assert_eq!(dm.lookup(hot_dir, "pinned.txt").unwrap(), hot_file);
            assert_eq!(dm.read_data(hot_file).unwrap(), b"keep-me-warm");
        }
        let cid = dm.create_file(cold_dir, &format!("c_{i}")).unwrap();
        let _ = dm.read_inode(cid).unwrap();
    }

    assert_eq!(dm.lookup(hot_dir, "pinned.txt").unwrap(), hot_file);
    assert_eq!(dm.read_data(hot_file).unwrap(), b"keep-me-warm");
}

#[test]
fn test_read_at_batch_indirect_and_sparse_parity() {
    use oifs::IoBackend;
    use oifs::ReadRequest;

    let dir = tempdir().unwrap();
    let img_path = dir.path().join("batch_indirect_read_at.img");
    let dm = DiskManager::open(&img_path, 32 * MB).unwrap();
    let root = dm.superblock().root_inode;

    let fid = dm.create_file(root, "sparse_multi_tier.bin").unwrap();
    // Write regions across:
    // - Direct blocks (0..10): offset 100..25_000
    // - Single indirect boundary (blocks 9..15): offset 38_000..60_000
    // - End of single indirect into double indirect (blocks 520..530): offset 520*4096 + 512 .. 530*4096
    // - Second single-indirect table inside double indirect (leaving a full 512-block SIB hole at s_idx=1,
    //   then writing at s_idx=2, i.e., block 10 + 512 + 1024 = 1546): offset 1546*4096 + 123 .. 1560*4096
    let writes: &[(u64, usize, u8)] = &[
        (100, 24_900, 0xA1),
        (38_000, 22_000, 0xB2),
        (520 * 4096 + 512, 10 * 4096, 0xC3),
        (1546 * 4096 + 123, 14 * 4096, 0xD4),
    ];
    for &(off, len, byte) in writes {
        let data: Vec<u8> = (0..len)
            .map(|i| byte.wrapping_add((i % 251) as u8))
            .collect();
        dm.write_data(fid, off, &data, CompressionMode::Never)
            .unwrap();
    }

    let expected_full = dm.read_data(fid).unwrap();
    let file_size = expected_full.len();
    assert!(file_size > 1559 * 4096);

    // Test various unaligned and multi-tier spans under both Mmap and Pread backends
    let test_ranges: &[(usize, usize)] = &[
        (0, 64 * 1024),                  // Direct + single indirect + leading sparse hole
        (50, 50_000),                    // Unaligned across direct -> hole -> single indirect
        (518 * 4096 + 777, 32 * 4096),   // Single indirect -> Double indirect + into SIB hole
        (522 * 4096, 1030 * 4096),       // Entire missing SIB subtree (s_idx=1) inside DIB
        (1545 * 4096 + 2048, 20 * 4096), // Hole -> allocated s_idx=2 -> EOF clamping
        (0, file_size),                  // Whole file via read_at
    ];

    for backend in [IoBackend::Mmap, IoBackend::Pread] {
        dm.set_io_backend(backend);
        for &(off, len) in test_ranges {
            let mut buf = vec![0xFFu8; len];
            let n = dm.read_at(fid, off as u64, &mut buf).unwrap();
            let exp_len = len.min(file_size.saturating_sub(off));
            assert_eq!(n, exp_len, "backend={backend:?} off={off} len={len}");
            assert_eq!(
                &buf[..n],
                &expected_full[off..off + n],
                "mismatch for backend={backend:?} off={off} len={len}"
            );
        }

        // Also verify read_at_batch parity
        let mut b0 = vec![0u8; 16384];
        let mut b1 = vec![0u8; 32768];
        let mut b2 = vec![0u8; 65536];
        let mut reqs = [
            ReadRequest {
                inode_id: fid,
                offset: 100,
                buf: &mut b0,
            },
            ReadRequest {
                inode_id: fid,
                offset: (520 * 4096) as u64,
                buf: &mut b1,
            },
            ReadRequest {
                inode_id: fid,
                offset: (1544 * 4096) as u64,
                buf: &mut b2,
            },
        ];
        let res = dm.read_at_batch(&mut reqs);
        assert_eq!(res[0].as_ref().unwrap(), &16384);
        assert_eq!(res[1].as_ref().unwrap(), &32768);
        assert_eq!(res[2].as_ref().unwrap(), &65536);
        assert_eq!(&b0[..], &expected_full[100..100 + 16384]);
        assert_eq!(&b1[..], &expected_full[520 * 4096..520 * 4096 + 32768]);
        assert_eq!(&b2[..], &expected_full[1544 * 4096..1544 * 4096 + 65536]);
    }
}

#[test]
fn test_sharded_dir_cache_concurrent_multi_directory_ops() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("sharded_dir_cache.img");

    let mut dir_ids = Vec::new();
    {
        let dm = DiskManager::open_journaled(&img_path, 64 * MB).unwrap();
        let root = dm.superblock().root_inode;
        for d in 0..16usize {
            let did = dm.create_directory(root, &format!("dir_{d}")).unwrap();
            for f in 0..80usize {
                dm.create_file(did, &format!("init_{f}.txt")).unwrap();
            }
            dir_ids.push(did);
        }
        dm.flush().unwrap();
    }

    // Reopen so all 32 shards of `ShardedDirCache` start completely cold
    let dm = Arc::new(DiskManager::open_journaled(&img_path, 0).unwrap());
    let mut handles = Vec::new();
    for (t, &did) in dir_ids.iter().enumerate() {
        let dm_c = dm.clone();
        handles.push(std::thread::spawn(move || {
            // 1. Cold negative lookup promotes this directory's shard entry without blocking other shards
            assert!(dm_c.lookup(did, "does_not_exist.txt").is_err());

            // 2. Warm positive lookups + path resolution
            for f in 0..80usize {
                let id = dm_c.lookup(did, &format!("init_{f}.txt")).unwrap();
                assert!(id > 0);
            }
            let resolved = dm_c.resolve_path(&format!("/dir_{t}/init_42.txt")).unwrap();
            assert_eq!(resolved, dm_c.lookup(did, "init_42.txt").unwrap());

            // 3. Create 20 new files and delete 10 old files concurrently across directories
            for f in 0..20usize {
                dm_c.create_file(did, &format!("new_{f}.txt")).unwrap();
            }
            for f in 0..10usize {
                dm_c.delete_file(did, &format!("init_{f}.txt")).unwrap();
            }

            // 4. Verify list_dir hits complete shard cache and returns 80 + 20 - 10 = 90 entries
            let entries = dm_c.list_dir(did).unwrap();
            assert_eq!(entries.len(), 90);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let report = dm.verify_integrity().unwrap();
    assert!(
        report.is_clean,
        "fsck must be clean after concurrent ShardedDirCache ops: {report:?}"
    );
}

#[test]
fn test_triple_indirect_fragmented_and_forged_metadata_ptr_rejection() {
    use oifs::IoBackend;

    let dir = tempdir().unwrap();
    let img_path = dir.path().join("triple_indirect_bounds.img");
    let dm = DiskManager::open(&img_path, 32 * MB).unwrap();
    let root = dm.superblock().root_inode;

    // 1. Sparse write spanning the Double-Indirect -> Triple-Indirect boundary:
    //    blk_idx 262_665 is the last Double-Indirect block; blk_idx 262_666 is the first Triple-Indirect block;
    //    blk_idx 263_200 is inside the second SIB of Triple-Indirect (skipping an entire unallocated SIB hole!).
    let tfid = dm.create_file(root, "triple_sparse.bin").unwrap();
    let b_last_dib = 262_665u64 * 4096;
    let b_first_tib = 262_666u64 * 4096;
    let b_second_sib_tib = 263_200u64 * 4096;

    dm.write_data(
        tfid,
        b_last_dib + 100,
        &[0xE1u8; 4000],
        CompressionMode::Never,
    )
    .unwrap();
    dm.write_data(tfid, b_first_tib, &[0xE2u8; 8192], CompressionMode::Never)
        .unwrap();
    dm.write_data(
        tfid,
        b_second_sib_tib + 512,
        &[0xE3u8; 6000],
        CompressionMode::Never,
    )
    .unwrap();

    for backend in [IoBackend::Mmap, IoBackend::Pread] {
        dm.set_io_backend(backend);

        // Read across the Double -> Triple boundary (b_last_dib .. b_first_tib + 8192)
        let mut cross_buf = vec![0xFFu8; 3 * 4096];
        let n = dm.read_at(tfid, b_last_dib, &mut cross_buf).unwrap();
        assert_eq!(n, 3 * 4096);
        assert_eq!(&cross_buf[..100], &[0u8; 100]);
        assert_eq!(&cross_buf[100..4096], &[0xE1u8; 3996]);
        assert_eq!(&cross_buf[4096..3 * 4096], &[0xE2u8; 8192]);

        // Read across the missing SIB hole in Triple Indirect into b_second_sib_tib
        let span_start = (262_668u64) * 4096;
        let span_len = ((263_202 - 262_668) * 4096) as usize;
        let mut hole_and_tail = vec![0xFFu8; span_len];
        let n2 = dm.read_at(tfid, span_start, &mut hole_and_tail).unwrap();
        assert_eq!(n2, (b_second_sib_tib + 512 + 6000 - span_start) as usize);
        let tail_rel = (b_second_sib_tib - span_start) as usize;
        assert!(hole_and_tail[..tail_rel + 512].iter().all(|&b| b == 0));
        assert_eq!(
            &hole_and_tail[tail_rel + 512..tail_rel + 512 + 6000],
            &[0xE3u8; 6000]
        );
    }

    // 2. Non-contiguous (interleaved/fragmented) block order + forged metadata pointer rejection
    let f1 = dm.create_file(root, "frag_a.bin").unwrap();
    let f2 = dm.create_file(root, "frag_b.bin").unwrap();
    for blk in 0..16u64 {
        dm.write_data(
            f1,
            blk * 4096,
            &[0x41 + blk as u8; 4096],
            CompressionMode::Never,
        )
        .unwrap();
        dm.write_data(
            f2,
            blk * 4096,
            &[0x61 + blk as u8; 4096],
            CompressionMode::Never,
        )
        .unwrap();
    }

    // Inject a forged metadata block pointer (block 0 = SuperBlock) into f1.blocks[2]
    let mut ino1 = dm.read_inode(f1).unwrap();
    let saved_blk2 = ino1.blocks[2];
    ino1.blocks[2] = 0; // first verify hole in middle of fragmented blocks
    dm.write_inode(f1, &ino1).unwrap();

    // Now forge block_id = 1 (inode bitmap metadata block < data_block_start) into blocks[2]
    ino1.blocks[2] = 1;
    dm.write_inode(f1, &ino1).unwrap();

    for backend in [IoBackend::Mmap, IoBackend::Pread] {
        dm.set_io_backend(backend);
        let mut buf = vec![0xFFu8; 16 * 4096];
        let n = dm.read_at(f1, 0, &mut buf).unwrap();
        assert_eq!(n, 16 * 4096);
        for blk in 0..16usize {
            let slice = &buf[blk * 4096..(blk + 1) * 4096];
            if blk == 2 {
                // Forged metadata block pointer MUST be zero-filled, never leaking bitmap bytes!
                assert_eq!(slice, &[0u8; 4096]);
            } else {
                assert_eq!(slice, &[0x41 + blk as u8; 4096]);
            }
        }
    }

    // Restore valid block pointer so image remains clean
    ino1.blocks[2] = saved_blk2;
    dm.write_inode(f1, &ino1).unwrap();
}

#[test]
fn test_sharded_dir_cache_encrypted_cold_scan_promotion_and_defrag() {
    use oifs::disk::DefragMode;

    let dir = tempdir().unwrap();
    let img_path = dir.path().join("sharded_dir_enc_defrag.img");
    let pw = "ShardedDirSecret!2026";

    let mut subdirs = Vec::new();
    {
        let dm = DiskManager::create_encrypted_journaled(&img_path, 64 * MB, pw).unwrap();
        let root = dm.superblock().root_inode;
        for d in 0..8usize {
            let did = dm.create_directory(root, &format!("enc_dir_{d}")).unwrap();
            for f in 0..24usize {
                let fid = dm.create_file(did, &format!("file_{f}.bin")).unwrap();
                dm.write_data(
                    fid,
                    0,
                    &[((d * 31 + f) & 0xFF) as u8; 4096],
                    CompressionMode::Never,
                )
                .unwrap();
            }
            subdirs.push(did);
        }
        dm.flush().unwrap();
    }

    // Reopen encrypted journaled image: `ShardedDirCache` is empty.
    // Creating one new file in each directory after a partial touch exercises `dir_lookup`
    // and subsequent in-place defragmentation remapping across all 32 shards.
    let dm = DiskManager::open_with_password(&img_path, 0, Some(pw)).unwrap();
    for (d, &did) in subdirs.iter().enumerate() {
        // Delete every other file to create fragmentation in data blocks and holes in directories
        for f in (0..24usize).step_by(2) {
            dm.delete_file(did, &format!("file_{f}.bin")).unwrap();
        }
        // Trigger >= 8 lookups on remaining files & negative lookups
        for f in (1..24usize).step_by(2) {
            let fid = dm.lookup(did, &format!("file_{f}.bin")).unwrap();
            assert_eq!(
                dm.read_data(fid).unwrap(),
                vec![((d * 31 + f) & 0xFF) as u8; 4096]
            );
        }
        assert!(dm.lookup(did, "missing_entry.bin").is_err());
    }

    // Run WAL-atomic in-place defragmentation while all 8 encrypted directories are cached in `ShardedDirCache`
    let stats = dm
        .defragment(img_path.to_str().unwrap(), DefragMode::InPlace, None)
        .unwrap();
    assert!(stats.files_processed > 0);

    // Verify cached directory entries (including remapped physical block IDs and tail_offset)
    // still allow immediate lookup, deletion, and new file creation without corruption
    for (d, &did) in subdirs.iter().enumerate() {
        let new_id = dm.create_file(did, "post_defrag.bin").unwrap();
        dm.write_data(new_id, 0, b"post-defrag-ok", CompressionMode::Never)
            .unwrap();
        dm.delete_file(did, "file_1.bin").unwrap();

        let entries = dm.list_dir(did).unwrap();
        // 24 initial - 12 deleted + 1 post_defrag - 1 deleted ("file_1.bin") = 12 entries
        assert_eq!(entries.len(), 12, "dir {d} entry count mismatch");
        assert_eq!(
            dm.read_data(dm.lookup(did, "post_defrag.bin").unwrap())
                .unwrap(),
            b"post-defrag-ok"
        );
    }

    let report = dm.verify_integrity().unwrap();
    assert!(
        report.is_clean,
        "fsck must be clean after encrypted ShardedDirCache + defrag: {report:?}"
    );
}

#[test]
#[ignore]
fn bench_batch_indirect_read_at_and_sharded_dir_cache() {
    let dir = tempdir().unwrap();

    // 1. Double-indirect 16 MiB file positional `read_at` (1000 x 1 MiB + 10000 x 64 KiB)
    {
        let p_read = dir.path().join("batch_read_at_bench.img");
        let dm = DiskManager::open(&p_read, 64 * MB).unwrap();
        let root = dm.superblock().root_inode;
        let fid = dm.create_file(root, "large_16mb.bin").unwrap();
        let data = vec![0x5Au8; 16 * 1024 * 1024];
        dm.write_data(fid, 0, &data, CompressionMode::Never)
            .unwrap();

        // Simulate unbatched per-4KB block resolution & 4KB copy using `get_block_copy` / 4KB `read_at` steps
        let mut buf_1mb = vec![0u8; 1024 * 1024];
        let t_leg_1mb = Instant::now();
        for i in 0..1000usize {
            let base_off = ((2 + (i % 14)) * 1024 * 1024) as u64;
            for b in 0..256usize {
                let off = base_off + (b * 4096) as u64;
                dm.read_at(fid, off, &mut buf_1mb[b * 4096..(b + 1) * 4096])
                    .unwrap();
            }
            std::hint::black_box(&buf_1mb);
        }
        let dt_leg_1mb = t_leg_1mb.elapsed();

        let t0 = Instant::now();
        for i in 0..1000usize {
            let off = ((2 + (i % 14)) * 1024 * 1024) as u64; // inside double-indirect tier
            let n = dm.read_at(fid, off, &mut buf_1mb).unwrap();
            assert_eq!(n, 1024 * 1024);
            std::hint::black_box(&buf_1mb);
        }
        let dt_1mb = t0.elapsed();

        let mut buf_64k = vec![0u8; 64 * 1024];
        let t1 = Instant::now();
        for i in 0..10_000usize {
            let off = ((32 + (i % 200)) * 64 * 1024) as u64;
            let n = dm.read_at(fid, off, &mut buf_64k).unwrap();
            assert_eq!(n, 64 * 1024);
            std::hint::black_box(&buf_64k);
        }
        let dt_64k = t1.elapsed();

        // Sparse double-indirect hole read: 500 x 4 MiB across unallocated SIB subtrees
        let sfid = dm.create_file(root, "sparse_64mb.bin").unwrap();
        dm.write_data(
            sfid,
            64 * 1024 * 1024,
            &[0xAAu8; 4096],
            CompressionMode::Never,
        )
        .unwrap();
        let mut buf_4mb = vec![0u8; 4 * 1024 * 1024];
        let t_leg_sparse = Instant::now();
        for i in 0..200usize {
            let base_off = ((4 + (i % 12) * 4) * 1024 * 1024) as u64;
            for b in 0..1024usize {
                let off = base_off + (b * 4096) as u64;
                dm.read_at(sfid, off, &mut buf_4mb[b * 4096..(b + 1) * 4096])
                    .unwrap();
            }
            std::hint::black_box(&buf_4mb);
        }
        let dt_leg_sparse = t_leg_sparse.elapsed();

        let t_opt_sparse = Instant::now();
        for i in 0..200usize {
            let base_off = ((4 + (i % 12) * 4) * 1024 * 1024) as u64;
            let n = dm.read_at(sfid, base_off, &mut buf_4mb).unwrap();
            assert_eq!(n, 4 * 1024 * 1024);
            std::hint::black_box(&buf_4mb);
        }
        let dt_opt_sparse = t_opt_sparse.elapsed();

        println!(
            "[read_at] 1000x1MB (dense double-indirect): 4KB-step {:?} ({:.2} GB/s) -> batch+extent {:?} ({:.2} GB/s, {:.2}x) | 10000x64KB: {:?} ({:.0} ops/s, {:.2} GB/s)",
            dt_leg_1mb,
            (1000.0 / 1024.0) / dt_leg_1mb.as_secs_f64(),
            dt_1mb,
            (1000.0 / 1024.0) / dt_1mb.as_secs_f64(),
            dt_leg_1mb.as_secs_f64() / dt_1mb.as_secs_f64(),
            dt_64k,
            10_000.0 / dt_64k.as_secs_f64(),
            (10_000.0 * 64.0 / (1024.0 * 1024.0)) / dt_64k.as_secs_f64()
        );
        println!(
            "[sparse hole] 200x4MB (unallocated SIB subtrees): 4KB-step {:?} ({:.2} GB/s) -> subtree-skip {:?} ({:.2} GB/s, {:.2}x)",
            dt_leg_sparse,
            (200.0 * 4.0 / 1024.0) / dt_leg_sparse.as_secs_f64(),
            dt_opt_sparse,
            (200.0 * 4.0 / 1024.0) / dt_opt_sparse.as_secs_f64(),
            dt_leg_sparse.as_secs_f64() / dt_opt_sparse.as_secs_f64()
        );
    }

    // 2. 16 threads x 4000 concurrent directory lookups + cold promotion across 32 directories
    {
        let p_dir = dir.path().join("sharded_dir_bench.img");
        let mut dirs = Vec::new();
        {
            let dm = DiskManager::open(&p_dir, 64 * MB).unwrap();
            let root = dm.superblock().root_inode;
            for d in 0..32usize {
                let did = dm.create_directory(root, &format!("d_{d}")).unwrap();
                for f in 0..128usize {
                    dm.create_file(did, &format!("entry_{f}.dat")).unwrap();
                }
                dirs.push(did);
            }
            dm.flush().unwrap();
        }
        let dirs = Arc::new(dirs);

        // 2a. Cold reopen + 16 threads triggering out-of-lock index builds & 64,000 lookups
        let dm = Arc::new(DiskManager::open(&p_dir, 0).unwrap());
        let t0 = Instant::now();
        let mut handles = Vec::new();
        for t in 0..16usize {
            let dm_c = dm.clone();
            let dirs_c = dirs.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..4000usize {
                    let did = dirs_c[(t * 2 + i) & 31];
                    let fid = dm_c.lookup(did, &format!("entry_{}.dat", i & 127)).unwrap();
                    std::hint::black_box(fid);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let dt_dir = t0.elapsed();
        let total_ops = 16.0 * 4000.0;
        println!(
            "[ShardedDirCache] 16 threads x 4000 lookups (64k ops across 32 dirs, cold start): {:?} ({:.2} M ops/s)",
            dt_dir,
            (total_ops / 1_000_000.0) / dt_dir.as_secs_f64()
        );

        // 2b. Direct lock-architecture microbenchmark (16 threads x 10,000 ops, 5% cold index promotion / mutation + 95% lookup):
        //     Single RwLock (global lock held across index build/mutation) vs 32-shard ( Fibonacci RwLock + out-of-lock build)
        use std::collections::HashMap;
        use std::sync::RwLock;
        type SimDirMap = HashMap<u64, HashMap<u64, u64>>;

        let single_cache: Arc<RwLock<SimDirMap>> = Arc::new(RwLock::new(HashMap::new()));
        for &did in dirs.iter() {
            let mut m = HashMap::with_capacity(128);
            for f in 0..128u64 {
                m.insert(f, did * 1000 + f);
            }
            single_cache.write().unwrap().insert(did, m);
        }

        let t_single = Instant::now();
        let mut h_single = Vec::new();
        for t in 0..16usize {
            let sc = single_cache.clone();
            let dirs_c = dirs.clone();
            h_single.push(std::thread::spawn(move || {
                for i in 0..10_000usize {
                    let did = dirs_c[(t * 2 + i) & 31];
                    if (i & 31) == 0 {
                        // Global lock baseline: global write lock held while rebuilding / mutating one directory's index
                        let mut g = sc.write().unwrap();
                        let mut rebuilt = HashMap::with_capacity(128);
                        for f in 0..128u64 {
                            rebuilt.insert(f, did * 1000 + f);
                        }
                        g.insert(did, rebuilt);
                    } else {
                        let g = sc.read().unwrap();
                        let v = g
                            .get(&did)
                            .and_then(|m| m.get(&((i & 127) as u64)))
                            .copied();
                        std::hint::black_box(v);
                    }
                }
            }));
        }
        for h in h_single {
            h.join().unwrap();
        }
        let dt_single = t_single.elapsed();

        let sharded_cache: Arc<[RwLock<SimDirMap>; 32]> =
            Arc::new(std::array::from_fn(|_| RwLock::new(HashMap::new())));
        for &did in dirs.iter() {
            let shard_idx = (did.wrapping_mul(0x517cc1b727220a95) as usize) & 31;
            let mut m = HashMap::with_capacity(128);
            for f in 0..128u64 {
                m.insert(f, did * 1000 + f);
            }
            sharded_cache[shard_idx].write().unwrap().insert(did, m);
        }

        let t_sharded = Instant::now();
        let mut h_sharded = Vec::new();
        for t in 0..16usize {
            let sc = sharded_cache.clone();
            let dirs_c = dirs.clone();
            h_sharded.push(std::thread::spawn(move || {
                for i in 0..10_000usize {
                    let did = dirs_c[(t * 2 + i) & 31];
                    let shard_idx = (did.wrapping_mul(0x517cc1b727220a95) as usize) & 31;
                    if (i & 31) == 0 {
                        // Build new DirIndex OUTSIDE shard write lock, then swap into target shard only!
                        let mut rebuilt = HashMap::with_capacity(128);
                        for f in 0..128u64 {
                            rebuilt.insert(f, did * 1000 + f);
                        }
                        sc[shard_idx].write().unwrap().insert(did, rebuilt);
                    } else {
                        let g = sc[shard_idx].read().unwrap();
                        let v = g
                            .get(&did)
                            .and_then(|m| m.get(&((i & 127) as u64)))
                            .copied();
                        std::hint::black_box(v);
                    }
                }
            }));
        }
        for h in h_sharded {
            h.join().unwrap();
        }
        let dt_sharded = t_sharded.elapsed();

        println!(
            "[DirCache Lock Contention A/B] 16 threads x 10k ops (97% lookup + 3% index build): Single RwLock {:?} -> 32-Shard + Out-of-Lock Build {:?} ({:.2}x faster)",
            dt_single,
            dt_sharded,
            dt_single.as_secs_f64() / dt_sharded.as_secs_f64()
        );
    }
}
