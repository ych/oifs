use oifs::disk::{CompressionMode, DiskManager, DurabilityMode};
use oifs::session::OifsSession;
use std::sync::Arc;
use std::time::Instant;
use tempfile::tempdir;

const MB: u64 = 1024 * 1024;

#[test]
#[ignore]
fn bench_p4_wave2() {
    let dir = tempdir().unwrap();

    // 1. P4.33: RangeAsync small-file create + write (500 files x 512B) and medium writes (100 x 64KB)
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
            "[P4.33 RangeAsync] 500x512B (+flush): {:?} ({:.0} files/s) | 100x64KB (+flush): {:?} ({:.1} MB/s)",
            dt_small,
            500.0 / dt_small.as_secs_f64(),
            dt_med,
            (100.0 * 64.0 / 1024.0) / dt_med.as_secs_f64()
        );
    }

    // 2. P4.18: Journaled in-place 4KB overwrites on a 256MB v3 image + 16MB journaled delete
    {
        let p_j = dir.path().join("p4_18.img");
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
            "[P4.18 Journal] 5000x4KB in-place overwrite (256MB img): {:?} ({:.0} ops/s) | 8x16MB delete: {:?}",
            dt_ow,
            5000.0 / dt_ow.as_secs_f64(),
            dt_del
        );
    }

    // 3. P4.28: Multi-threaded OifsSession Remote Proxy (8 threads x 300 ReadAt RPCs of 32KB)
    {
        let p_ipc = dir.path().join("p4_28_ipc.img");
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
            "[P4.28 IPC Proxy] 8 threads x 300 x 32KB read_at (2400 RPCs): {:?} ({:.0} RPC/s, {:.1} MB/s)",
            dt_ipc,
            2400.0 / dt_ipc.as_secs_f64(),
            (2400.0 * 32.0 / 1024.0) / dt_ipc.as_secs_f64()
        );
    }
}

#[test]
fn test_p4_33_range_async_batching_mode_transition_and_recovery() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("p4_33_regression.img");

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
fn test_p4_18_journaled_coalesced_large_write_truncate_and_delete() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("p4_18_regression.img");

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
        "fsck must be clean after P4.18 ops: {report:?}"
    );
    drop(dm);

    // Reopen to verify WAL recovery / checkpoint invariants
    let dm2 = DiskManager::open(&img_path, 0).unwrap();
    assert!(dm2.verify_integrity().unwrap().is_clean);
}

#[test]
fn test_p4_28_ipc_pooled_concurrent_uds_and_tcp_encrypted() {
    let dir = tempdir().unwrap();

    // 1. Local UDS IPC with >64KB frames (`read_framed` direct streaming) + 8 concurrent proxy threads
    {
        let uds_img = dir.path().join("p4_28_uds.img");
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
        let tcp_img = dir.path().join("p4_28_tcp_enc.img");
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
fn test_p4_22_hot_directory_survives_cold_inode_scan_flood() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("p4_22_clock.img");
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
