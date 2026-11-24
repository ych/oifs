//! Regression tests for Batch 5 performance optimizations and behavioral parity.

use oifs::OifsSession;
use oifs::disk::{CompressionMode, DiskManager};
use oifs::ffi::{
    oifs_close, oifs_create_file_with_data, oifs_last_error, oifs_open, oifs_read_at,
};
use oifs::filters::{FilterConfig, recommend_filters};
use oifs::fuse::{FuseMountOptions, OifsFuseState};
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
    dm.write_data(rd_file, 0, b"readonly handle payload", CompressionMode::Never)
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
    assert_eq!(rec.candidates[0].compressed_size, rec.baseline_compressed_size);
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
        assert_ne!(msg, "No error", "thread-local error should not be clobbered");
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
