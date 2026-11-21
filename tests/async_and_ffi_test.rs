use oifs::disk::{CompressionMode, DiskManager, DurabilityMode};
use oifs::ffi::*;
use oifs::storage::StorageMode;
use std::ffi::CString;
use std::os::raw::c_void;
use std::ptr;
use std::sync::{Arc, Condvar, Mutex};
use tempfile::tempdir;

/// Minimal zero-dependency `Future` executor to test `std::future::Future` polling and
/// `Waker` notification on `OifsAsyncTask<T>` via `futures::executor::block_on(task)`.
mod futures {
    pub mod executor {
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        use std::thread::{self, Thread};

        struct ThreadWaker(Thread);

        impl Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }

        pub fn block_on<F: Future>(mut fut: F) -> F::Output {
            let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
            let mut cx = Context::from_waker(&waker);
            // SAFETY: `fut` stays pinned on this stack frame until `poll` returns `Poll::Ready`.
            let mut pinned = unsafe { Pin::new_unchecked(&mut fut) };
            loop {
                match pinned.as_mut().poll(&mut cx) {
                    Poll::Ready(val) => return val,
                    Poll::Pending => thread::park(),
                }
            }
        }
    }
}

#[test]
fn test_file_read_handle_mmap_slice_zero_copy_and_fallbacks() {
    let dir = tempdir().unwrap();

    // 1. StorageMode::Mmap: single-block and 1 MiB multi-block contiguous raw files.
    let mmap_path = dir.path().join("mmap_zero_copy.img");
    let dm = DiskManager::open_with_storage_mode(&mmap_path, 16 * 1024 * 1024, StorageMode::Mmap)
        .unwrap();
    let root = dm.superblock().root_inode;

    let single_payload: Vec<u8> = (0..3000).map(|i| (i % 251) as u8).collect();
    let single_id = dm
        .create_file_with_data(root, "single.bin", &single_payload, CompressionMode::Never)
        .unwrap();

    let multi_len = 1024 * 1024; // 1 MiB (256 blocks across direct + single-indirect)
    let multi_payload: Vec<u8> = (0..multi_len).map(|i| ((i * 7 + 13) % 253) as u8).collect();
    let multi_id = dm
        .create_file_with_data(
            root,
            "multi_1mb.bin",
            &multi_payload,
            CompressionMode::Never,
        )
        .unwrap();

    // Stream-compressed file on Mmap -> mmap_slice must return None.
    let comp_payload = vec![b'Z'; 32 * 1024];
    let comp_id = dm
        .create_file_with_data(
            root,
            "compressed.bin",
            &comp_payload,
            CompressionMode::Always,
        )
        .unwrap();

    // Seekable 64K compressed file on Mmap -> mmap_slice must return None.
    let seek_id = dm
        .create_file_with_data(
            root,
            "seekable.bin",
            &comp_payload,
            CompressionMode::Seekable {
                chunk_size: 65536,
                level: 1,
            },
        )
        .unwrap();

    // Sparse file with unallocated hole blocks on Mmap -> mmap_slice must return None.
    let sparse_id = dm.create_file(root, "sparse.bin").unwrap();
    dm.truncate(sparse_id, 32 * 1024).unwrap();
    dm.write_data(sparse_id, 16 * 1024, &[0xEE; 4096], CompressionMode::Never)
        .unwrap();

    {
        let session = dm.begin_read();

        let fh_single = session.open_file(single_id).unwrap();
        let slice_single = fh_single
            .mmap_slice(0, single_payload.len())
            .unwrap()
            .expect("single-block contiguous raw file on Mmap must return Some(&[u8])");
        assert_eq!(slice_single, &single_payload[..]);

        // Sub-slice and clamping past EOF.
        let sub_single = fh_single.mmap_slice(500, 10_000).unwrap().unwrap();
        assert_eq!(sub_single, &single_payload[500..]);
        let eof_slice = fh_single
            .mmap_slice(single_payload.len() as u64, 128)
            .unwrap()
            .unwrap();
        assert!(eof_slice.is_empty());

        let fh_multi = session.open_file(multi_id).unwrap();
        let slice_multi = fh_multi
            .mmap_slice(0, multi_len)
            .unwrap()
            .expect("1 MiB contiguous raw file on Mmap must return Some(&[u8])");
        assert_eq!(slice_multi.len(), multi_len);
        assert_eq!(slice_multi, &multi_payload[..]);

        let mid_multi = fh_multi
            .mmap_slice(12_345, 500_000)
            .unwrap()
            .expect("mid-file multi-block slice must return Some(&[u8])");
        assert_eq!(mid_multi, &multi_payload[12_345..512_345]);

        let fh_comp = session.open_file(comp_id).unwrap();
        assert!(
            fh_comp.mmap_slice(0, 4096).unwrap().is_none(),
            "stream-compressed file must return None from mmap_slice"
        );

        let fh_seek = session.open_file(seek_id).unwrap();
        assert!(
            fh_seek.mmap_slice(0, 4096).unwrap().is_none(),
            "seekable 64K file must return None from mmap_slice"
        );

        let fh_sparse = session.open_file(sparse_id).unwrap();
        assert!(
            fh_sparse.mmap_slice(0, 4096).unwrap().is_none(),
            "sparse hole must return None from mmap_slice"
        );
        assert!(
            fh_sparse.mmap_slice(0, 20 * 1024).unwrap().is_none(),
            "range spanning a sparse hole must return None from mmap_slice"
        );
    }

    // 2. StorageMode::FileIo: mmap_slice must return None even for raw contiguous files.
    let fio_path = dir.path().join("file_io_no_mmap.img");
    let dm_fio =
        DiskManager::open_with_storage_mode(&fio_path, 8 * 1024 * 1024, StorageMode::FileIo)
            .unwrap();
    let fio_root = dm_fio.superblock().root_inode;
    let fio_id = dm_fio
        .create_file_with_data(fio_root, "raw.bin", &single_payload, CompressionMode::Never)
        .unwrap();
    {
        let session = dm_fio.begin_read();
        let fh_fio = session.open_file(fio_id).unwrap();
        assert!(
            fh_fio
                .mmap_slice(0, single_payload.len())
                .unwrap()
                .is_none(),
            "StorageMode::FileIo must return None from mmap_slice"
        );
        let mut fallback_buf = vec![0u8; single_payload.len()];
        let n = fh_fio.read_at(0, &mut fallback_buf).unwrap();
        assert_eq!(n, single_payload.len());
        assert_eq!(fallback_buf, single_payload);
    }

    // 3. Encrypted filesystem on StorageMode::Mmap: mmap_slice must return None.
    let enc_path = dir.path().join("encrypted_mmap.img");
    let dm_enc =
        DiskManager::create_encrypted(&enc_path, 8 * 1024 * 1024, "correct-horse-battery-staple")
            .unwrap();
    let enc_root = dm_enc.superblock().root_inode;
    let enc_id = dm_enc
        .create_file_with_data(
            enc_root,
            "secret.bin",
            &single_payload,
            CompressionMode::Never,
        )
        .unwrap();
    {
        let session = dm_enc.begin_read();
        let fh_enc = session.open_file(enc_id).unwrap();
        assert!(
            fh_enc
                .mmap_slice(0, single_payload.len())
                .unwrap()
                .is_none(),
            "encrypted file must never expose raw ciphertext via mmap_slice"
        );
        assert_eq!(fh_enc.read_all().unwrap(), single_payload);
    }
}

#[test]
fn test_async_read_inline_fast_path_and_background_slow_path() {
    let dir = tempdir().unwrap();

    // 1. StorageMode::Mmap raw file -> Inline Fast-Path must return is_ready() == true immediately.
    let mmap_path = dir.path().join("async_mmap.img");
    let dm_mmap =
        DiskManager::open_with_storage_mode(&mmap_path, 16 * 1024 * 1024, StorageMode::Mmap)
            .unwrap();
    let root = dm_mmap.superblock().root_inode;

    let raw_data: Vec<u8> = (0..16_384).map(|i| (i % 199) as u8).collect();
    let raw_id = dm_mmap
        .create_file_with_data(root, "raw.bin", &raw_data, CompressionMode::Never)
        .unwrap();

    let task_at = dm_mmap.read_at_async(raw_id, 1024, 4096);
    assert!(
        task_at.is_ready(),
        "Mmap raw read_at_async must complete on the Inline Fast-Path before polling"
    );
    assert_eq!(task_at.wait().unwrap(), &raw_data[1024..5120]);

    let task_data = dm_mmap.read_data_async(raw_id);
    assert!(
        task_data.is_ready(),
        "Mmap raw read_data_async must complete on the Inline Fast-Path before polling"
    );
    assert_eq!(futures::executor::block_on(task_data).unwrap(), raw_data);

    // 2. Seekable 64K file: cold read -> warms ChunkReadCache -> subsequent sub-chunk read_at_async is inline-ready.
    let seek_data: Vec<u8> = (0..131_072).map(|i| ((i / 17) % 251) as u8).collect();
    let seek_id = dm_mmap
        .create_file_with_data(
            root,
            "seek64k.bin",
            &seek_data,
            CompressionMode::Seekable {
                chunk_size: 65536,
                level: 1,
            },
        )
        .unwrap();

    // Cold read of chunk 0 via background pool (using block_on).
    let cold_seek_task = dm_mmap.read_at_async(seek_id, 0, 4096);
    let cold_seek_bytes = futures::executor::block_on(cold_seek_task).unwrap();
    assert_eq!(cold_seek_bytes, &seek_data[0..4096]);

    // Chunk 0 is now warm in ChunkReadCache: reading another slice inside chunk 0 must be inline-ready!
    let warm_seek_task = dm_mmap.read_at_async(seek_id, 8192, 4096);
    assert!(
        warm_seek_task.is_ready(),
        "read_at_async on warm ChunkReadCache must be ready immediately on Inline Fast-Path"
    );
    assert_eq!(warm_seek_task.wait().unwrap(), &seek_data[8192..12288]);

    // 3. Stream-compressed file: cold read via background pool -> warms DecodedBlobCache -> inline-ready!
    let stream_data: Vec<u8> = (0..24_576).map(|i| ((i * 3) % 127) as u8).collect();
    let stream_id = dm_mmap
        .create_file_with_data(
            root,
            "stream.bin",
            &stream_data,
            CompressionMode::Stream { level: 1 },
        )
        .unwrap();

    // Cold read_data_async via `.wait()` and cold read_at_async via `block_on`.
    let cold_stream_at = dm_mmap.read_at_async(stream_id, 0, 4096);
    assert_eq!(cold_stream_at.wait().unwrap(), &stream_data[0..4096]);

    // DecodedBlobCache is now warm for `stream_id`: both read_at_async and read_data_async must be inline-ready!
    let warm_stream_at = dm_mmap.read_at_async(stream_id, 4096, 8192);
    assert!(
        warm_stream_at.is_ready(),
        "read_at_async on warm DecodedBlobCache must be ready immediately on Inline Fast-Path"
    );
    assert_eq!(warm_stream_at.wait().unwrap(), &stream_data[4096..12288]);

    let warm_stream_full = dm_mmap.read_data_async(stream_id);
    assert!(
        warm_stream_full.is_ready(),
        "read_data_async on warm DecodedBlobCache must be ready immediately on Inline Fast-Path"
    );
    assert_eq!(
        futures::executor::block_on(warm_stream_full).unwrap(),
        stream_data
    );

    // 4. StorageMode::FileIo: cold read (background pool) -> warms DataPageCache -> warm read is inline-ready!
    let fio_path = dir.path().join("async_file_io.img");
    let dm_fio =
        DiskManager::open_with_storage_mode(&fio_path, 16 * 1024 * 1024, StorageMode::FileIo)
            .unwrap();
    let fio_root = dm_fio.superblock().root_inode;
    let fio_id = dm_fio
        .create_file_with_data(fio_root, "fio_raw.bin", &raw_data, CompressionMode::Never)
        .unwrap();

    // Newly written FileIo blocks are not yet in DataPageCache: cold read completes via background pool.
    let cold_fio_task = dm_fio.read_data_async(fio_id);
    assert_eq!(
        futures::executor::block_on(cold_fio_task).unwrap(),
        raw_data
    );

    // Now the 4 pages (16 KiB) are resident in DataPageCache: both read_at_async and read_data_async
    // must hit the Inline Fast-Path (`is_ready() == true` immediately upon return)!
    let warm_fio_at = dm_fio.read_at_async(fio_id, 2048, 4096);
    assert!(
        warm_fio_at.is_ready(),
        "read_at_async on warm FileIo DataPageCache must be ready immediately on Inline Fast-Path"
    );
    assert_eq!(warm_fio_at.wait().unwrap(), &raw_data[2048..6144]);

    let warm_fio_full = dm_fio.read_data_async(fio_id);
    assert!(
        warm_fio_full.is_ready(),
        "read_data_async on warm FileIo DataPageCache must be ready immediately on Inline Fast-Path"
    );
    let mut warm_fio_full = warm_fio_full;
    assert_eq!(warm_fio_full.try_take().unwrap().unwrap(), raw_data);
}

#[test]
fn test_group_commit_strict_durability_mmap_and_file_io_wal() {
    let dir = tempdir().unwrap();

    for (mode, label) in [
        (StorageMode::Mmap, "mmap_wal"),
        (StorageMode::FileIo, "file_io_wal"),
    ] {
        let img_path = dir.path().join(format!("group_commit_{label}.img"));
        let dm =
            DiskManager::open_with_options(&img_path, 32 * 1024 * 1024, None, true, mode).unwrap();
        dm.set_durability_mode(DurabilityMode::Strict);
        let root = dm.superblock().root_inode;

        // Launch 40 concurrent create_file_with_data_async tasks.
        let num_files = 40usize;
        let mut handles = Vec::with_capacity(num_files);
        let barrier = Arc::new(std::sync::Barrier::new(num_files));

        for i in 0..num_files {
            let dm_clone = dm.clone();
            let bar = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let name = format!("gc_file_{i:02}.bin");
                let payload: Vec<u8> = (0..2048).map(|b| ((b + i * 19) % 251) as u8).collect();
                bar.wait();
                let task = dm_clone.create_file_with_data_async(
                    root,
                    name,
                    payload,
                    CompressionMode::Never,
                );
                futures::executor::block_on(task).unwrap()
            }));
        }

        let mut created_inodes = Vec::with_capacity(num_files);
        for h in handles {
            created_inodes.push(h.join().unwrap());
        }
        assert_eq!(created_inodes.len(), num_files);

        // Verify fsck integrity before closing.
        let report = dm.verify_integrity().unwrap();
        assert!(
            report.is_clean,
            "verify_integrity failed after 40 concurrent Strict group-commit writes on {label}: {report:?}"
        );
        drop(dm);

        // Reopen the image and verify all 40 files and their exact payloads survived.
        let dm_reopen = DiskManager::open_with_options(&img_path, 0, None, true, mode).unwrap();
        let report_reopen = dm_reopen.verify_integrity().unwrap();
        assert!(report_reopen.is_clean);

        for i in 0..num_files {
            let name = format!("/gc_file_{i:02}.bin");
            let id = dm_reopen.resolve_path(&name).unwrap();
            let expected: Vec<u8> = (0..2048).map(|b| ((b + i * 19) % 251) as u8).collect();
            assert_eq!(dm_reopen.read_data(id).unwrap(), expected);
        }
    }
}

#[test]
fn test_c_ffi_session_zero_copy_out_of_order_drop_and_async_uaf_safety() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("ffi_full_test.img");
    let c_img_path = CString::new(img_path.to_str().unwrap()).unwrap();

    // 1. oifs_open_with_options (Mmap + WAL) & durability configuration.
    let handle = oifs_open_with_options(
        c_img_path.as_ptr(),
        16 * 1024 * 1024,
        ptr::null(),
        OIFS_STORAGE_MODE_MMAP,
        1,
    );
    assert!(!handle.is_null());
    assert_eq!(oifs_set_durability_mode(handle, 0), 0);
    assert_eq!(oifs_set_file_io_cache_bytes(handle, 8 * 1024 * 1024), 0);

    // 2. oifs_create_file_with_data, oifs_resolve_path, oifs_read_at_inode.
    let c_path1 = CString::new("/alpha.bin").unwrap();
    let payload1: Vec<u8> = (0..8192).map(|i| (i % 241) as u8).collect();
    let inode1 = oifs_create_file_with_data(
        handle,
        c_path1.as_ptr(),
        payload1.as_ptr(),
        payload1.len() as u64,
        OIFS_WRITE_POLICY_RAW,
        0,
    );
    assert!(inode1 > 0);
    assert_eq!(oifs_resolve_path(handle, c_path1.as_ptr()), inode1);

    let mut read_buf = vec![0u8; 4096];
    let n_read = oifs_read_at_inode(
        handle,
        inode1 as u64,
        1024,
        read_buf.as_mut_ptr(),
        read_buf.len() as u64,
    );
    assert_eq!(n_read, 4096);
    assert_eq!(&read_buf[..], &payload1[1024..5120]);

    // Create a second compressed file for batch read and fallback testing.
    let c_path2 = CString::new("/beta_comp.bin").unwrap();
    let payload2: Vec<u8> = (0..16384).map(|i| ((i * 5) % 113) as u8).collect();
    let inode2 = oifs_create_file_with_data(
        handle,
        c_path2.as_ptr(),
        payload2.as_ptr(),
        payload2.len() as u64,
        OIFS_WRITE_POLICY_STREAM,
        1,
    );
    assert!(inode2 > 0);

    // 3. oifs_read_at_batch across both files.
    let mut batch_buf1 = vec![0u8; 2048];
    let mut batch_buf2 = vec![0u8; 4096];
    let mut batch_items = [
        OIFSReadBatchItem {
            inode_id: inode1 as u64,
            offset: 512,
            buf: batch_buf1.as_mut_ptr(),
            buf_size: batch_buf1.len() as u64,
            result: -1,
        },
        OIFSReadBatchItem {
            inode_id: inode2 as u64,
            offset: 2048,
            buf: batch_buf2.as_mut_ptr(),
            buf_size: batch_buf2.len() as u64,
            result: -1,
        },
    ];
    assert_eq!(
        oifs_read_at_batch(handle, batch_items.as_mut_ptr(), batch_items.len()),
        0
    );
    assert_eq!(batch_items[0].result, 2048);
    assert_eq!(batch_items[1].result, 4096);
    assert_eq!(&batch_buf1[..], &payload1[512..2560]);
    assert_eq!(&batch_buf2[..], &payload2[2048..6144]);

    // 4. oifs_begin_read, oifs_session_open_file, oifs_file_mmap_slice, and
    //    OUT-OF-ORDER oifs_end_read BEFORE oifs_file_close (must not UAF!).
    let session = oifs_begin_read(handle);
    assert!(!session.is_null());

    let fh1 = oifs_session_open_file(session, c_path1.as_ptr());
    assert!(!fh1.is_null());
    assert_eq!(oifs_file_size(fh1), payload1.len() as u64);
    assert_eq!(oifs_file_inode_id(fh1), inode1 as u64);

    let fh2 = oifs_session_open_inode(session, inode2 as u64);
    assert!(!fh2.is_null());

    // Destroy the session BEFORE using and closing fh1 and fh2!
    oifs_end_read(session);

    // fh1 is still valid because OIFSFileHandle holds its own Arc<FfiSharedSession>.
    let mut mmap_ptr: *const u8 = ptr::null();
    let mut mmap_len: u64 = 0;
    let slice_rc = oifs_file_mmap_slice(fh1, 256, 4096, &mut mmap_ptr, &mut mmap_len);
    assert_eq!(
        slice_rc, 0,
        "oifs_file_mmap_slice must return 0 on Mmap raw file"
    );
    assert!(!mmap_ptr.is_null());
    assert_eq!(mmap_len, 4096);
    let zero_copy_bytes = unsafe { std::slice::from_raw_parts(mmap_ptr, mmap_len as usize) };
    assert_eq!(zero_copy_bytes, &payload1[256..4352]);

    // fh2 is compressed, so oifs_file_mmap_slice must return 1 (fallback signal) and oifs_file_read_at must work.
    let mut comp_ptr: *const u8 = ptr::null();
    let mut comp_len: u64 = 0;
    assert_eq!(
        oifs_file_mmap_slice(fh2, 0, 1024, &mut comp_ptr, &mut comp_len),
        1
    );
    let mut fh2_buf = vec![0u8; 1024];
    assert_eq!(
        oifs_file_read_at(fh2, 100, fh2_buf.as_mut_ptr(), fh2_buf.len() as u64),
        1024
    );
    assert_eq!(&fh2_buf[..], &payload2[100..1124]);

    oifs_file_close(fh1);
    oifs_file_close(fh2);

    // 5. Async FFI: oifs_read_at_async inline fast-path + immediate oifs_async_free UAF drop barrier.
    let mut async_hot_buf = vec![0u8; 1024];
    let hot_fut = oifs_read_at_async(
        handle,
        inode1 as u64,
        0,
        async_hot_buf.as_mut_ptr(),
        async_hot_buf.len() as u64,
    );
    assert!(!hot_fut.is_null());
    assert_eq!(
        oifs_async_poll(hot_fut),
        1,
        "Mmap raw oifs_read_at_async must be ready immediately"
    );
    assert_eq!(oifs_async_wait(hot_fut), 1024);
    assert_eq!(&async_hot_buf[..], &payload1[..1024]);
    oifs_async_free(hot_fut);

    // Create a larger uncached compressed file (128 KiB) so oifs_read_at_async dispatches
    // to the background worker pool, then immediately call oifs_async_free(fut) without
    // oifs_async_wait and poison/drop the buffer right after oifs_async_free returns.
    let large_comp_data: Vec<u8> = (0..131_072).map(|i| ((i * 31 + 7) % 251) as u8).collect();
    for round in 0..16 {
        let c_large = CString::new(format!("/uaf_barrier_{round}.bin")).unwrap();
        let large_id = oifs_create_file_with_data(
            handle,
            c_large.as_ptr(),
            large_comp_data.as_ptr(),
            large_comp_data.len() as u64,
            OIFS_WRITE_POLICY_STREAM,
            1,
        );
        assert!(large_id > 0);

        let mut temp_buf = vec![0u8; large_comp_data.len()];
        let slow_fut = oifs_read_at_async(
            handle,
            large_id as u64,
            0,
            temp_buf.as_mut_ptr(),
            temp_buf.len() as u64,
        );
        assert!(!slow_fut.is_null());
        // Drop immediately without calling oifs_async_wait: wait_on_drop MUST block until
        // the background thread finishes writing into temp_buf.
        oifs_async_free(slow_fut);
        // Immediately mutate and free temp_buf to catch any post-free write race.
        temp_buf.fill(0xAA);
        drop(temp_buf);
    }

    // 6. oifs_create_file_with_data_async, oifs_write_file_async, oifs_flush_async.
    let c_async_file = CString::new("/async_created.bin").unwrap();
    let create_fut = oifs_create_file_with_data_async(
        handle,
        c_async_file.as_ptr(),
        payload1.as_ptr(),
        payload1.len() as u64,
        OIFS_WRITE_POLICY_RAW,
        0,
    );
    assert!(!create_fut.is_null());
    let async_inode = oifs_async_wait(create_fut);
    assert!(async_inode > 0);
    oifs_async_free(create_fut);

    let patch = b"PATCHED_BYTES";
    let write_fut = oifs_write_file_async(
        handle,
        c_async_file.as_ptr(),
        0,
        patch.as_ptr(),
        patch.len() as u64,
        OIFS_WRITE_POLICY_RAW,
        0,
    );
    assert!(!write_fut.is_null());
    assert_eq!(oifs_async_wait(write_fut), async_inode);
    oifs_async_free(write_fut);

    let flush_fut = oifs_flush_async(handle);
    assert!(!flush_fut.is_null());
    assert_eq!(oifs_async_wait(flush_fut), 0);
    oifs_async_free(flush_fut);

    // 7. Callback APIs: oifs_read_at_async_cb and oifs_create_file_with_data_async_cb.
    #[derive(Default)]
    struct CbState {
        done: bool,
        status: i64,
        data: Vec<u8>,
    }

    extern "C" fn on_read_done(
        status_or_len: i64,
        data: *const u8,
        data_len: usize,
        user_data: *mut c_void,
    ) {
        let pair = unsafe { &*(user_data as *const (Mutex<CbState>, Condvar)) };
        let mut guard = pair.0.lock().unwrap();
        guard.status = status_or_len;
        if status_or_len > 0 && !data.is_null() {
            guard.data = unsafe { std::slice::from_raw_parts(data, data_len) }.to_vec();
        }
        guard.done = true;
        pair.1.notify_all();
    }

    extern "C" fn on_create_done(status_or_inode: i64, user_data: *mut c_void) {
        let pair = unsafe { &*(user_data as *const (Mutex<CbState>, Condvar)) };
        let mut guard = pair.0.lock().unwrap();
        guard.status = status_or_inode;
        guard.done = true;
        pair.1.notify_all();
    }

    let create_cb_sync = Arc::new((Mutex::new(CbState::default()), Condvar::new()));
    let c_cb_file = CString::new("/cb_created.bin").unwrap();
    let rc_create_cb = oifs_create_file_with_data_async_cb(
        handle,
        c_cb_file.as_ptr(),
        payload1.as_ptr(),
        payload1.len() as u64,
        OIFS_WRITE_POLICY_RAW,
        0,
        Some(on_create_done),
        Arc::as_ptr(&create_cb_sync) as *mut c_void,
    );
    assert_eq!(rc_create_cb, 0);
    let cb_inode = {
        let mut g = create_cb_sync.0.lock().unwrap();
        while !g.done {
            g = create_cb_sync.1.wait(g).unwrap();
        }
        g.status
    };
    assert!(cb_inode > 0);

    let read_cb_sync = Arc::new((Mutex::new(CbState::default()), Condvar::new()));
    let rc_read_cb = oifs_read_at_async_cb(
        handle,
        cb_inode as u64,
        128,
        1024,
        Some(on_read_done),
        Arc::as_ptr(&read_cb_sync) as *mut c_void,
    );
    assert_eq!(rc_read_cb, 0);
    {
        let mut g = read_cb_sync.0.lock().unwrap();
        while !g.done {
            g = read_cb_sync.1.wait(g).unwrap();
        }
        assert_eq!(g.status, 1024);
        assert_eq!(&g.data[..], &payload1[128..1152]);
    }

    assert_eq!(oifs_flush(handle), 0);
    oifs_close(handle);
}
