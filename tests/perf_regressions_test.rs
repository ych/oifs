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
