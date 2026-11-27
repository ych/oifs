//! Regression tests for Batch 5 performance optimizations and behavioral parity.

use oifs::OifsSession;
use oifs::disk::{CompressionMode, DiskManager};
use oifs::ffi::{oifs_close, oifs_create_file_with_data, oifs_last_error, oifs_open, oifs_read_at};
use oifs::filters::{FilterConfig, recommend_filters};
use oifs::fuse::{FuseFileHandle, FuseHandleTable, FuseMountOptions, OifsFuseState};
use oifs::nfs::{
    NFS_PROGRAM, NFS_VERSION_3, NFS3_OK, NfsMountOptions, OifsNfsFs, RPC_MSG_CALL, RPC_VERSION_2,
    XdrReader, XdrWriter,
};
use std::ffi::{CStr, CString};
use std::sync::{Arc, Barrier};
use tempfile::tempdir;

#[test]
fn test_fuse_readdir_pagination_and_rdonly_flush_skip() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("fuse_page.img");
    let dm = DiskManager::open(&img, 8 * 1024 * 1024).unwrap();
    let root = dm.superblock().root_inode;

    for i in 0..250u32 {
        let name = format!("item_{i:04}.txt");
        dm.create_file(root, &name).unwrap();
    }
    let rd_file = dm.create_file(root, "rd.txt").unwrap();
    dm.write_data(
        rd_file,
        0,
        b"readonly handle payload",
        CompressionMode::Never,
    )
    .unwrap();

    let state = OifsFuseState::new(FuseMountOptions::default());

    // Open O_RDONLY handle and invoke flush -> must succeed immediately without error.
    let open_out = state.open(&dm, 1 + rd_file, libc::O_RDONLY as u32).unwrap();
    state.flush(&dm, 1 + rd_file, open_out.fh).unwrap();
    state.release(1 + rd_file, open_out.fh).unwrap();

    // Paginate with small 512-byte wire pages until empty.
    let opendir_out = state.opendir(&dm, 1, 0).unwrap();
    let mut collected = Vec::new();
    let mut off = 0u64;
    loop {
        let page = state.readdir(&dm, 1, opendir_out.fh, off, 512).unwrap();
        if page.is_empty() {
            break;
        }
        off = page.last().unwrap().off;
        for entry in page {
            collected.push(entry.name);
        }
    }
    state.releasedir(1, opendir_out.fh).unwrap();

    assert_eq!(collected.len(), 253); // "." + ".." + 250 item_XXXX + rd.txt
    assert_eq!(collected[0], ".");
    assert_eq!(collected[1], "..");
    let mut sorted_children = collected[2..].to_vec();
    let orig_children = sorted_children.clone();
    sorted_children.sort();
    assert_eq!(orig_children, sorted_children);
}

#[test]
fn test_rename_preserves_dir_cache_consistency() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("rename_dir_cache.img");
    let dm = DiskManager::open(&img, 8 * 1024 * 1024).unwrap();
    let root = dm.superblock().root_inode;
    let d1 = dm.create_directory(root, "d1").unwrap();
    let d2 = dm.create_directory(root, "d2").unwrap();

    let f_a = dm.create_file(d1, "a.txt").unwrap();
    dm.write_data(f_a, 0, b"alpha", CompressionMode::Never)
        .unwrap();
    let f_b = dm.create_file(d1, "b.txt").unwrap();
    dm.write_data(f_b, 0, b"beta", CompressionMode::Never)
        .unwrap();

    // Warm up dir_cache for root, d1, d2.
    assert_eq!(dm.list_dir(d1).unwrap().len(), 2);
    assert_eq!(dm.list_dir(d2).unwrap().len(), 0);

    // Same-parent rename: b.txt -> c.txt
    dm.rename(d1, "b.txt", d1, "c.txt").unwrap();
    assert!(dm.lookup(d1, "b.txt").is_err());
    assert_eq!(dm.lookup(d1, "c.txt").unwrap(), f_b);

    // Cross-parent overwrite: d1/a.txt -> d2/target.txt (first create target to overwrite)
    let f_old = dm.create_file(d2, "target.txt").unwrap();
    dm.write_data(f_old, 0, b"old", CompressionMode::Never)
        .unwrap();
    dm.rename(d1, "a.txt", d2, "target.txt").unwrap();
    assert!(dm.lookup(d1, "a.txt").is_err());
    assert_eq!(dm.lookup(d2, "target.txt").unwrap(), f_a);
    assert_eq!(dm.read_data(f_a).unwrap(), b"alpha");
    assert_eq!(dm.list_dir(d1).unwrap().len(), 1);
    assert_eq!(dm.list_dir(d2).unwrap().len(), 1);
}

#[test]
fn test_async_reads_cold_miss_and_hot_hit_across_modes() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("async_modes.img");
    let dm = DiskManager::open(&img, 16 * 1024 * 1024).unwrap();
    let root = dm.superblock().root_inode;

    let raw_id = dm.create_file(root, "raw.bin").unwrap();
    let raw_data: Vec<u8> = (0..128 * 1024).map(|i| (i & 0xFF) as u8).collect();
    dm.write_data(raw_id, 0, &raw_data, CompressionMode::Never)
        .unwrap();

    let comp_id = dm.create_file(root, "comp.bin").unwrap();
    let comp_data = vec![0x3Cu8; 96 * 1024];
    dm.write_data(comp_id, 0, &comp_data, CompressionMode::Always)
        .unwrap();

    let seek_id = dm.create_file(root, "seek.bin").unwrap();
    let seek_data: Vec<u8> = (0..130 * 1024).map(|i| ((i * 7) & 0xFF) as u8).collect();
    dm.write_data(
        seek_id,
        0,
        &seek_data,
        CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        },
    )
    .unwrap();

    // First reads (comp/seek cold miss -> background pool; raw -> inline fast-path).
    assert_eq!(dm.read_data_async(raw_id).wait().unwrap(), raw_data);
    assert_eq!(dm.read_data_async(comp_id).wait().unwrap(), comp_data);
    assert_eq!(
        dm.read_at_async(seek_id, 70_000, 4096).wait().unwrap(),
        seek_data[70_000..74_096]
    );

    // Repeat reads (hot DecodedBlobCache & ChunkReadCache inline fast-path hits).
    assert_eq!(
        dm.read_at_async(comp_id, 1024, 4096).wait().unwrap(),
        comp_data[1024..5120]
    );
    assert_eq!(
        dm.read_at_async(seek_id, 70_000, 4096).wait().unwrap(),
        seek_data[70_000..74_096]
    );
}

#[test]
fn test_recommend_filters_preserves_order_and_baseline() {
    let mut sample = Vec::with_capacity(64 * 1024);
    for i in 0..16_384u32 {
        sample.extend_from_slice(&i.to_le_bytes());
    }
    let rec = recommend_filters(&sample);
    assert_eq!(rec.candidates.len(), 14);
    assert_eq!(rec.candidates[0].label, "None (Raw Zstd)");
    assert_eq!(rec.candidates[0].config, FilterConfig::none());
    assert_eq!(
        rec.candidates[0].compressed_size,
        rec.baseline_compressed_size
    );
    assert!(rec.best_report.compressed_size <= rec.baseline_compressed_size);
}

#[test]
fn test_ffi_last_error_is_thread_local_across_threads() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("ffi_tls_err.img");
    let c_img = CString::new(img.to_str().unwrap()).unwrap();
    let handle = oifs_open(c_img.as_ptr(), 4 * 1024 * 1024);
    assert!(!handle.is_null());

    let c_good = CString::new("/good.txt").unwrap();
    let payload = b"tls-ok";
    assert!(
        oifs_create_file_with_data(
            handle,
            c_good.as_ptr(),
            payload.as_ptr(),
            payload.len() as u64,
            0,
            0
        ) >= 0
    );

    let handle_addr = handle as usize;
    let barrier = Arc::new(Barrier::new(2));

    // Thread 1 triggers an error while Thread 2 performs valid operations.
    let b1 = Arc::clone(&barrier);
    let t1 = std::thread::spawn(move || {
        let h = handle_addr as *mut oifs::ffi::OIFSHandle;
        let c_missing = CString::new("/does_not_exist.txt").unwrap();
        let mut out = [0u8; 8];
        let rc = oifs_read_at(h, c_missing.as_ptr(), 0, out.as_mut_ptr(), out.len() as u64);
        assert_eq!(rc, -1);
        b1.wait();
        b1.wait();
        let mut err_buf = [0i8; 256];
        assert_eq!(oifs_last_error(h, err_buf.as_mut_ptr(), 256), 0);
        let msg = unsafe { CStr::from_ptr(err_buf.as_ptr()) }.to_string_lossy();
        assert_ne!(
            msg, "No error",
            "thread-local error should not be clobbered"
        );
    });

    let b2 = Arc::clone(&barrier);
    let t2 = std::thread::spawn(move || {
        let h = handle_addr as *mut oifs::ffi::OIFSHandle;
        b2.wait();
        let c_ok = CString::new("/good.txt").unwrap();
        let mut out = [0u8; 16];
        let n = oifs_read_at(h, c_ok.as_ptr(), 0, out.as_mut_ptr(), out.len() as u64);
        assert_eq!(n, 6);
        let mut err_buf = [0i8; 256];
        assert_eq!(oifs_last_error(h, err_buf.as_mut_ptr(), 256), 0);
        let msg = unsafe { CStr::from_ptr(err_buf.as_ptr()) }.to_string_lossy();
        assert_eq!(msg, "No error");
        b2.wait();
    });

    t1.join().unwrap();
    t2.join().unwrap();
    oifs_close(handle);
}

#[test]
fn test_ipc_server_drop_does_not_stall_1500ms() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("ipc_fast_drop.img");

    let master = OifsSession::open_local(&img, 4 * 1024 * 1024).unwrap();
    let client = OifsSession::open_local(&img, 0).unwrap();
    assert!(!client.is_direct());

    let start = std::time::Instant::now();
    drop(master);
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(800),
        "IpcServer::drop stalled too long: {elapsed:?}"
    );
    drop(client);
}

#[test]
fn test_fuse_handle_table_lock_free_seqlock_concurrency() {
    let table = Arc::new(FuseHandleTable::new());
    let mut initial = Vec::new();
    for i in 0..32u64 {
        let fh = table.insert(FuseFileHandle {
            inode_id: 100 + i,
            flags: libc::O_RDONLY as u32,
            is_dir: i % 2 == 0,
        });
        initial.push((fh, 100 + i));
    }

    let mut handles = Vec::new();
    for t in 0..6usize {
        let tbl = Arc::clone(&table);
        let init = initial.clone();
        handles.push(std::thread::spawn(move || {
            for iter in 0..4000usize {
                let (fh, expected_ino) = init[(iter + t) % init.len()];
                if let Some(h) = tbl.get(fh) {
                    assert_eq!(h.inode_id, expected_ino);
                }
            }
        }));
    }
    for t in 0..2u64 {
        let tbl = Arc::clone(&table);
        handles.push(std::thread::spawn(move || {
            for iter in 0..1000u64 {
                let fh = tbl.insert(FuseFileHandle {
                    inode_id: 10_000 + t * 2000 + iter,
                    flags: libc::O_RDWR as u32,
                    is_dir: false,
                });
                assert_eq!(tbl.get(fh).unwrap().inode_id, 10_000 + t * 2000 + iter);
                tbl.remove(fh);
                assert!(tbl.get(fh).is_none());
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(table.len(), 32);
}

#[test]
fn test_ffi_error_fast_path_recovers_when_error_thread_exits() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("ffi_exit_recover.img");
    let c_img = CString::new(img.to_str().unwrap()).unwrap();
    let handle = oifs_open(c_img.as_ptr(), 4 * 1024 * 1024);
    assert!(!handle.is_null());

    let c_ok = CString::new("/ok.txt").unwrap();
    let payload = b"hello";
    assert!(
        oifs_create_file_with_data(
            handle,
            c_ok.as_ptr(),
            payload.as_ptr(),
            payload.len() as u64,
            0,
            0
        ) >= 0
    );

    let handle_addr = handle as usize;
    // Thread triggers an FFI error and exits WITHOUT reading or clearing it.
    std::thread::spawn(move || {
        let h = handle_addr as *mut oifs::ffi::OIFSHandle;
        let c_missing = CString::new("/missing_on_exit.txt").unwrap();
        let mut out = [0u8; 8];
        assert_eq!(
            oifs_read_at(h, c_missing.as_ptr(), 0, out.as_mut_ptr(), 8),
            -1
        );
    })
    .join()
    .unwrap();

    // Another thread performing normal calls sees "No error".
    let mut out = [0u8; 16];
    assert_eq!(
        oifs_read_at(handle, c_ok.as_ptr(), 0, out.as_mut_ptr(), 16),
        5
    );
    let mut err_buf = [0i8; 256];
    assert_eq!(oifs_last_error(handle, err_buf.as_mut_ptr(), 256), 0);
    let msg = unsafe { CStr::from_ptr(err_buf.as_ptr()) }.to_string_lossy();
    assert_eq!(msg, "No error");
    oifs_close(handle);
}

fn call_nfs_proc(nfs: &OifsNfsFs, xid: u32, proc_num: u32, args: &[u8]) -> Vec<u8> {
    let mut req = XdrWriter::new();
    req.write_u32(xid);
    req.write_u32(RPC_MSG_CALL);
    req.write_u32(RPC_VERSION_2);
    req.write_u32(NFS_PROGRAM);
    req.write_u32(NFS_VERSION_3);
    req.write_u32(proc_num);
    // AUTH_NONE cred & verf
    req.write_u32(0);
    req.write_opaque(&[]);
    req.write_u32(0);
    req.write_opaque(&[]);
    let mut full = req.into_bytes();
    full.extend_from_slice(args);
    nfs.dispatch_rpc(&full)
}

#[test]
fn test_dir_snapshot_cache_invalidates_on_direct_disk_mutations() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("snapshot_inval.img");
    let dm = Arc::new(DiskManager::open(&img, 8 * 1024 * 1024).unwrap());
    let root = dm.superblock().root_inode;
    for i in 0..80u32 {
        dm.create_file(root, &format!("f_{i:03}.txt")).unwrap();
    }

    // 1. FUSE snapshot warm + invalidate after create / rename / delete via DiskManager.
    let fuse = OifsFuseState::new(FuseMountOptions::default());
    let mut first_names = Vec::new();
    let mut off = 0u64;
    loop {
        let p = fuse.readdir(&dm, 1, 0, off, 512).unwrap();
        if p.is_empty() {
            break;
        }
        off = p.last().unwrap().off;
        first_names.extend(p.into_iter().map(|e| e.name));
    }
    assert_eq!(first_names.len(), 82);

    // 2. NFS snapshot warm on READDIR & READDIRPLUS.
    let nfs = OifsNfsFs::from_arc(Arc::clone(&dm), NfsMountOptions::default());
    let root_fh = nfs.root_handle().to_bytes();
    let collect_nfs = |proc_num: u32, is_plus: bool| -> Vec<String> {
        let mut names = Vec::new();
        let mut cookie = 0u64;
        let mut xid = 1u32;
        loop {
            let mut args = XdrWriter::new();
            args.write_opaque(&root_fh);
            args.write_u64(cookie);
            args.write_fixed_opaque(&[0u8; 8]);
            args.write_u32(512);
            if is_plus {
                args.write_u32(1024);
            }
            let reply = call_nfs_proc(&nfs, xid, proc_num, &args.into_bytes());
            xid += 1;
            let mut r = XdrReader::new(&reply);
            let _xid = r.read_u32().unwrap();
            assert_eq!(r.read_u32(), Some(1)); // RPC_REPLY
            assert_eq!(r.read_u32(), Some(0)); // MSG_ACCEPTED
            assert_eq!(r.read_u32(), Some(0)); // AUTH_NONE
            let _ = r.read_opaque(400).unwrap();
            assert_eq!(r.read_u32(), Some(0)); // RPC_ACCEPT_SUCCESS
            assert_eq!(r.read_u32(), Some(NFS3_OK));
            if r.read_bool().unwrap() {
                let _ = r.read_fixed_opaque(21 * 4).unwrap();
            }
            let _verf = r.read_fixed_opaque(8).unwrap();
            while r.read_bool().unwrap() {
                let _fileid = r.read_u64().unwrap();
                let name = r.read_string(256).unwrap().to_string();
                cookie = r.read_u64().unwrap();
                if is_plus {
                    if r.read_bool().unwrap() {
                        let _ = r.read_fixed_opaque(21 * 4).unwrap();
                    }
                    if r.read_bool().unwrap() {
                        let _ = r.read_opaque(64).unwrap();
                    }
                }
                names.push(name);
            }
            if r.read_bool().unwrap() {
                break;
            }
        }
        names
    };
    assert_eq!(collect_nfs(16, false).len(), 82);
    assert_eq!(collect_nfs(17, true).len(), 82);

    // Mutate root directly on DiskManager (bypassing FUSE/NFS).
    dm.create_file(root, "created_new.txt").unwrap();
    dm.rename(root, "f_010.txt", root, "renamed_010.txt")
        .unwrap();
    dm.delete_file(root, "f_020.txt").unwrap();

    // FUSE readdir must reflect all 3 mutations.
    let mut fuse_after = Vec::new();
    let mut off = 0u64;
    loop {
        let p = fuse.readdir(&dm, 1, 0, off, 512).unwrap();
        if p.is_empty() {
            break;
        }
        off = p.last().unwrap().off;
        fuse_after.extend(p.into_iter().map(|e| e.name));
    }
    assert_eq!(fuse_after.len(), 82);
    assert!(fuse_after.contains(&"created_new.txt".to_string()));
    assert!(fuse_after.contains(&"renamed_010.txt".to_string()));
    assert!(!fuse_after.contains(&"f_010.txt".to_string()));
    assert!(!fuse_after.contains(&"f_020.txt".to_string()));

    // NFS readdir & readdirplus must also reflect all 3 mutations.
    let nfs_rd_after = collect_nfs(16, false);
    let nfs_plus_after = collect_nfs(17, true);
    for names in [nfs_rd_after, nfs_plus_after] {
        assert_eq!(names.len(), 82);
        assert!(names.contains(&"created_new.txt".to_string()));
        assert!(names.contains(&"renamed_010.txt".to_string()));
        assert!(!names.contains(&"f_010.txt".to_string()));
        assert!(!names.contains(&"f_020.txt".to_string()));
    }
}

#[test]
#[ignore]
fn bench_opt_wave_1_2_3_4_8() {
    use oifs::filters::{
        delta_decode_inplace, delta_encode_inplace, shuffle_decode, shuffle_encode,
    };
    use oifs::journal::{MetadataOp, coalesce_ops};
    use oifs::{DurabilityMode, StorageMode};
    use std::time::Instant;

    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;

    // 1. Seekable 64K encrypted writes (Mmap & FileIo)
    println!("\n=== [Wave #1/#2] Encrypted Seekable-64K Writes (32 MiB) ===");
    let seek_payload: Vec<u8> = (0..(32 * MB))
        .map(|i| ((i.wrapping_mul(2654435761)) >> 24) as u8)
        .collect();
    let seek_mode = CompressionMode::Seekable {
        chunk_size: 65536,
        level: 1,
    };
    for mode in [StorageMode::Mmap, StorageMode::FileIo] {
        let dir = tempdir().unwrap();
        let img = dir.path().join("seek_enc.img");
        let dm =
            DiskManager::open_with_options(&img, 256 * MB as u64, Some("bench-pw"), false, mode)
                .unwrap();
        let t0 = Instant::now();
        let ino = dm
            .create_file_with_data(0, "bulk.bin", &seek_payload, seek_mode)
            .unwrap();
        let elapsed = t0.elapsed().as_secs_f64();
        println!(
            "  {mode:?} create_file_with_data (32 MiB encrypted Seekable-64K): {:.2} ms ({:.2} GB/s)",
            elapsed * 1e3,
            (seek_payload.len() as f64) / (elapsed * 1e9)
        );

        // 2. FileIo cold 64 KiB coalesced read_at after reopen
        if mode == StorageMode::FileIo {
            drop(dm);
            let dm_ro = DiskManager::open_with_options(
                &img,
                256 * MB as u64,
                Some("bench-pw"),
                false,
                StorageMode::FileIo,
            )
            .unwrap();
            let mut rbuf = vec![0u8; 64 * KB];
            let mut off = 0u64;
            let tr0 = Instant::now();
            while (off as usize) < seek_payload.len() {
                dm_ro.read_at(ino, off, &mut rbuf).unwrap();
                off += (64 * KB) as u64;
            }
            let r_sec = tr0.elapsed().as_secs_f64();
            println!(
                "  FileIo cold 64K sequential read_at (32 MiB): {:.2} ms ({:.2} GB/s)",
                r_sec * 1e3,
                (seek_payload.len() as f64) / (r_sec * 1e9)
            );
        }
    }

    // 3. WAL coalesce_ops (4,000 scattered/adjacent ops) & Strict small-file WAL traffic
    println!("\n=== [Wave #3] WAL coalesce_ops (4,000 ops) & Strict Small-File Commit ===");
    let raw_ops: Vec<MetadataOp> = (0..4000u64)
        .map(|i| MetadataOp::SetDataBitmapRange {
            start_block: (i * 3) % 8192,
            count: ((i % 4) as u32) + 1,
            allocated: i % 5 != 0,
        })
        .collect();
    let tc0 = Instant::now();
    let coalesced = coalesce_ops(raw_ops);
    let tc_us = tc0.elapsed().as_secs_f64() * 1e6;
    println!(
        "  coalesce_ops (4,000 ops -> {} intervals): {:.1} us",
        coalesced.len(),
        tc_us
    );
    {
        let dir = tempdir().unwrap();
        let img = dir.path().join("strict_wal.img");
        let dm =
            DiskManager::open_with_options(&img, 64 * MB as u64, None, true, StorageMode::Mmap)
                .unwrap();
        dm.set_durability_mode(DurabilityMode::Strict);
        let t0 = Instant::now();
        for i in 0..20 {
            let name = format!("f_{i:03}.txt");
            dm.create_file_with_data(0, &name, b"small-payload", CompressionMode::Never)
                .unwrap();
        }
        let dt_ms = t0.elapsed().as_secs_f64() * 1e3;
        println!(
            "  Strict journaled 20x create_file_with_data: {:.2} ms ({:.2} ms/file)",
            dt_ms,
            dt_ms / 20.0
        );
    }

    // 4. WriteBatch (10 ops inside 5,000-file encrypted directory)
    println!("\n=== [Wave #4] WriteBatch (10 ops) in 5,000-File Encrypted Dir ===");
    {
        let dir = tempdir().unwrap();
        let img = dir.path().join("batch_5k.img");
        let dm = DiskManager::open_with_options(
            &img,
            128 * MB as u64,
            Some("batch-pw"),
            true,
            StorageMode::Mmap,
        )
        .unwrap();
        let mut seed_batch = dm.begin_write_batch();
        for i in 0..5000 {
            seed_batch.create_file_with_data(
                0,
                format!("seed_{i:05}.txt"),
                b"v".to_vec(),
                CompressionMode::Never,
            );
        }
        seed_batch.commit().unwrap();

        let mut b10 = dm.begin_write_batch();
        for i in 0..10 {
            b10.create_file_with_data(
                0,
                format!("hot_{i:02}.txt"),
                vec![b'x'; 256],
                CompressionMode::Never,
            );
        }
        let tb0 = Instant::now();
        b10.commit().unwrap();
        let tb_ms = tb0.elapsed().as_secs_f64() * 1e3;
        println!("  10-op encrypted WriteBatch commit (in 5,000-file dir): {tb_ms:.2} ms");
    }

    // 5. SIMD Filters (1 MiB x 100 iters)
    println!("\n=== [Wave #8] SIMD Filters (1 MiB x 100 iterations) ===");
    let src_1m: Vec<u8> = (0..MB).map(|i| (i.wrapping_mul(131)) as u8).collect();
    let iters = 100usize;
    let total_bytes = (MB * iters) as f64;
    for ts in [2usize, 4, 8] {
        let mut enc_last = Vec::new();
        let t0 = Instant::now();
        for _ in 0..iters {
            enc_last = shuffle_encode(&src_1m, ts);
        }
        let sec_enc = t0.elapsed().as_secs_f64();
        let mut dec_last = Vec::new();
        let t1 = Instant::now();
        for _ in 0..iters {
            dec_last = shuffle_decode(&enc_last, ts);
        }
        let sec_dec = t1.elapsed().as_secs_f64();
        assert_eq!(dec_last, src_1m);
        println!(
            "  ByteShuffle ts={ts}: encode {:6.2} GB/s | decode {:6.2} GB/s",
            total_bytes / (sec_enc * 1e9),
            total_bytes / (sec_dec * 1e9)
        );
    }
    for ts in [1usize, 2, 4, 8] {
        let mut buf = src_1m.clone();
        let t0 = Instant::now();
        for _ in 0..iters {
            delta_encode_inplace(&mut buf, ts);
        }
        let sec_enc = t0.elapsed().as_secs_f64();
        let t1 = Instant::now();
        for _ in 0..iters {
            delta_decode_inplace(&mut buf, ts);
        }
        let sec_dec = t1.elapsed().as_secs_f64();
        assert_eq!(buf, src_1m);
        println!(
            "  DeltaInplace ts={ts}: encode {:6.2} GB/s | decode {:6.2} GB/s",
            total_bytes / (sec_enc * 1e9),
            total_bytes / (sec_dec * 1e9)
        );
    }

    // 6. Wave #7: Decode double-copy & unaligned parallel seekable read & multi-frame pooled Zstd
    println!(
        "\n=== [Wave #7] Unaligned Seekable Read, Sub-Chunk Cache & 100-Frame Zstd Stream ==="
    );
    {
        let dir = tempdir().unwrap();
        let img = dir.path().join("wave7_seek.img");
        let dm = DiskManager::open(&img, 128 * MB as u64).unwrap();
        let payload: Vec<u8> = (0..(16 * MB))
            .map(|i| ((i % 251) ^ ((i >> 8) & 0xFF)) as u8)
            .collect();
        let ino = dm
            .create_file_with_data(
                0,
                "seek16m.bin",
                &payload,
                CompressionMode::Seekable {
                    chunk_size: 65536,
                    level: 1,
                },
            )
            .unwrap();

        // Warm once then time unaligned 16 MiB - 4 KiB read_at (triggers parallel path at offset 4096)
        let mut unaligned_buf = vec![0u8; payload.len() - 4096];
        let _ = dm.read_at(ino, 4096, &mut unaligned_buf).unwrap();
        let t0 = Instant::now();
        for _ in 0..5 {
            let n = dm.read_at(ino, 4096, &mut unaligned_buf).unwrap();
            assert_eq!(n, payload.len() - 4096);
        }
        let sec_unaligned = t0.elapsed().as_secs_f64();
        assert_eq!(unaligned_buf, payload[4096..]);
        println!(
            "  Unaligned parallel Seekable-64K read_at(off=4096, 16M x 5): {:.2} ms ({:.2} GB/s)",
            sec_unaligned * 1e3,
            (unaligned_buf.len() * 5) as f64 / (sec_unaligned * 1e9)
        );

        // 4 KiB sequential sub-chunk read_at exercising Arc<Vec<u8>> ChunkReadCache
        let mut sub_buf = vec![0u8; 4096];
        let t1 = Instant::now();
        let mut off = 0u64;
        while (off as usize) < 4 * MB {
            dm.read_at(ino, off, &mut sub_buf).unwrap();
            off += 4096;
        }
        let sec_sub = t1.elapsed().as_secs_f64();
        println!(
            "  4 KiB sequential sub-chunk read_at (4 MiB): {:.2} ms ({:.2} GB/s)",
            sec_sub * 1e3,
            (4 * MB) as f64 / (sec_sub * 1e9)
        );

        // 100-frame concatenated Stream Zstd file read_data via pooled STREAM_DECOMPRESSOR
        let mf_ino = dm.create_file(0, "multiframe_100.zst").unwrap();
        let frame_chunk = vec![b'A'; 4096];
        for i in 0..100u64 {
            dm.write_data(mf_ino, i * 4096, &frame_chunk, CompressionMode::Always)
                .unwrap();
        }
        let _ = dm.read_data(mf_ino).unwrap();
        let t2 = Instant::now();
        for _ in 0..20 {
            let out = dm.read_data(mf_ino).unwrap();
            assert_eq!(out.len(), 100 * 4096);
        }
        let sec_mf = t2.elapsed().as_secs_f64();
        println!(
            "  100-frame Zstd stream read_data x 20 (400 KiB each): {:.2} ms ({:.2} GB/s)",
            sec_mf * 1e3,
            (100 * 4096 * 20) as f64 / (sec_mf * 1e9)
        );
    }
}

#[test]
fn test_unaligned_parallel_seekable_read_and_multiframe_pooled_zstd() {
    use oifs::StorageMode;
    use oifs::zstd_pool::train_dictionary;

    // Build synthetic structured corpus and train a Zstd dictionary
    let samples: Vec<Vec<u8>> = (0..32u32)
        .map(|s| {
            let mut v = Vec::with_capacity(4096);
            for i in 0..256u32 {
                v.extend_from_slice(format!("REC:{s:02}:{i:04}:alpha_beta_gamma;").as_bytes());
            }
            v
        })
        .collect();
    let dict = train_dictionary(&samples, 16 * 1024).unwrap();

    // 1. Unaligned parallel read_at across both Mmap and FileIo with encryption + Zstd dict
    let payload: Vec<u8> = (0usize..(512 * 1024 + 137))
        .map(|i| {
            if i % 64 < 32 {
                b'K'
            } else {
                ((i.wrapping_mul(2654435761)) >> 24) as u8
            }
        })
        .collect();

    for mode in [StorageMode::Mmap, StorageMode::FileIo] {
        let dir = tempdir().unwrap();
        let img = dir.path().join(format!("unaligned_{mode:?}.img"));
        let dm =
            DiskManager::open_with_options(&img, 32 * 1024 * 1024, Some("pw-wave7"), false, mode)
                .unwrap();
        let dict_id = dm.register_and_use_zstd_dict(&dict).unwrap();

        let ino = dm
            .create_file_with_data(
                0,
                "seek_unaligned.bin",
                &payload,
                CompressionMode::Seekable {
                    chunk_size: 65536,
                    level: 1,
                },
            )
            .unwrap();

        for &off in &[1u64, 4099, 65_535, 65_537, 131_071] {
            let len = payload.len() - off as usize;
            let mut got = vec![0u8; len];
            let n = dm.read_at(ino, off, &mut got).unwrap();
            assert_eq!(n, len);
            assert_eq!(
                got,
                payload[off as usize..],
                "unaligned parallel read_at mismatch for mode={mode:?}, off={off}"
            );
        }

        // 2. Multi-frame concatenated stream mixing dict frames & non-dict frames via STREAM_DECOMPRESSOR
        let stream_ino = dm.create_file(0, "mixed_frames.zst").unwrap();
        let mut expected_stream = Vec::new();
        for frame_idx in 0..30usize {
            if frame_idx % 3 == 0 {
                dm.set_default_zstd_dict(None).unwrap();
            } else {
                dm.set_default_zstd_dict(Some(dict_id)).unwrap();
            }
            let part: Vec<u8> = (0..3072)
                .map(|j| ((frame_idx * 31 + j) % 199) as u8)
                .collect();
            let offset = expected_stream.len() as u64;
            dm.write_data(stream_ino, offset, &part, CompressionMode::Always)
                .unwrap();
            expected_stream.extend_from_slice(&part);
        }
        let actual_stream = dm.read_data(stream_ino).unwrap();
        assert_eq!(
            actual_stream, expected_stream,
            "mixed multi-frame pooled Zstd decode mismatch for mode={mode:?}"
        );
    }
}
