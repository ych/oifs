//! Regression tests for Batch 3 WAL crash-safety, async polling, Stage-1/Stage-3
//! concurrency, and session failover / canonicalization fixes.

use oifs::OifsSession;
use oifs::disk::{CompressionMode, DiskManager, DurabilityMode};
use std::future::Future as _;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use tempfile::tempdir;

struct ThreadUnparkWaker {
    thread: std::thread::Thread,
    woken: AtomicBool,
}

impl Wake for ThreadUnparkWaker {
    fn wake(self: Arc<Self>) {
        self.woken.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

#[test]
fn test_write_data_journaled_cow_preserves_old_data_on_enospc() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("cow_enospc.img");

    // Journal reserves 33 blocks + metadata overhead (~41 blocks = 168 KiB).
    // Give ~256 KiB total (~190 KiB usable data blocks) so an 80 KiB file fits once,
    // while a full-overwrite with 160 KiB incompressible bytes cannot allocate new COW blocks.
    let dm = DiskManager::open_journaled(&img, 64 * 4096).unwrap();
    let root = dm.superblock().root_inode;
    let fid = dm.create_file(root, "cow_target.bin").unwrap();

    let original: Vec<u8> = (0..80 * 1024)
        .map(|i| ((i * 131 + 17) ^ (i >> 3)) as u8)
        .collect();
    dm.write_data(fid, 0, &original, CompressionMode::Never)
        .unwrap();

    let too_large: Vec<u8> = (0..180 * 1024)
        .map(|i| ((i * 73 + 91) ^ (i >> 5)) as u8)
        .collect();
    let res = dm.write_data(fid, 0, &too_large, CompressionMode::Never);
    assert!(
        res.is_err(),
        "oversized COW overwrite must fail cleanly with ENOSPC"
    );

    // Crucially, COW must NOT have overwritten the file's live blocks prior to commit.
    assert_eq!(dm.read_data(fid).unwrap(), original);
    drop(dm);

    // Reopening from disk must also see intact original data and pass fsck.
    let dm2 = DiskManager::open_journaled(&img, 0).unwrap();
    assert_eq!(dm2.read_data(fid).unwrap(), original);
    let report = dm2.verify_integrity().unwrap();
    assert!(report.is_clean, "unhealthy image: {report:?}");
}

#[test]
fn test_strict_journal_ring_wrap_checkpoints_instead_of_dropping() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("strict_wrap.img");
    let crashed = dir.path().join("strict_wrap_crashed.img");

    // Use a large enough image for 450 small files; Strict durability must auto-checkpoint
    // before wrapping over uncheckpointed frames even with zero explicit `flush()` calls.
    let dm = DiskManager::open_journaled(&img, 8 * 1024 * 1024).unwrap();
    dm.set_durability_mode(DurabilityMode::Strict);
    let root = dm.superblock().root_inode;

    let count = 450usize;
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let name = format!("f_{i:04}.txt");
        let id = dm.create_file(root, &name).unwrap();
        let payload = format!("strict-payload-{i:04}");
        dm.write_data(id, 0, payload.as_bytes(), CompressionMode::Never)
            .unwrap();
        ids.push((id, name, payload));
    }

    // Snapshot raw bytes while open (simulating abrupt power loss / process kill before Drop),
    // wait: remember under Strict mode, deferred in-place writes are synced on checkpoint/wrap,
    // while committed WAL frames + payloads before commit are durable on disk.
    let raw_snapshot = std::fs::read(&img).unwrap();
    drop(dm);

    std::fs::write(&crashed, &raw_snapshot).unwrap();
    let dm_rec = DiskManager::open_journaled(&crashed, 0).unwrap();
    let rec_root = dm_rec.superblock().root_inode;
    for (id, name, expected) in &ids {
        let looked = dm_rec.lookup(rec_root, name).unwrap();
        assert_eq!(looked, *id, "missing {name} after strict WAL recovery");
        assert_eq!(dm_rec.read_data(*id).unwrap(), expected.as_bytes());
    }
    let report = dm_rec.verify_integrity().unwrap();
    assert!(report.is_clean, "unhealthy after recovery: {report:?}");
}

#[test]
fn test_async_task_poll_panics_cleanly_on_worker_panic() {
    let mut task = oifs::disk::OifsAsyncTask::<u64>::spawn(|| {
        panic!("simulated background worker panic");
    });

    let state = Arc::new(ThreadUnparkWaker {
        thread: std::thread::current(),
        woken: AtomicBool::new(false),
    });
    let waker: Waker = state.clone().into();
    let mut cx = Context::from_waker(&waker);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            match Pin::new(&mut task).poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "OifsAsyncTask::poll hung in Pending after worker panic"
                    );
                    std::thread::park_timeout(std::time::Duration::from_millis(50));
                }
            }
        }
    }));

    assert!(
        outcome.is_err(),
        "polling a panicked OifsAsyncTask must panic cleanly rather than hanging"
    );
}

#[test]
fn test_concurrent_same_second_writers_and_async_blob_cache_consistency() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("concurrent_writers.img");
    let dm = Arc::new(DiskManager::open(&img, 16 * 1024 * 1024).unwrap());
    let root = dm.superblock().root_inode;

    let std_id = dm.create_file(root, "standard_comp.bin").unwrap();
    let seek_id = dm.create_file(root, "seekable.bin").unwrap();

    let v0 = vec![0xA0u8; 32 * 1024];
    dm.write_data(std_id, 0, &v0, CompressionMode::Always)
        .unwrap();
    dm.write_data(
        seek_id,
        0,
        &vec![0xB0u8; 128 * 1024],
        CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        },
    )
    .unwrap();

    // Writers overwrite at varied offsets within < 1 second so modified_at stays identical.
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for tid in 0..3u8 {
        let dm_w = Arc::clone(&dm);
        let stop_w = Arc::clone(&stop);
        handles.push(std::thread::spawn(move || {
            for iter in 0..20usize {
                if stop_w.load(Ordering::Relaxed) {
                    break;
                }
                let tag = 0x10u8.wrapping_add(tid).wrapping_add((iter & 0x0F) as u8);
                let payload = vec![tag; 16 * 1024 + (iter % 5) * 1024];
                dm_w.write_data(std_id, 0, &payload, CompressionMode::Always)
                    .unwrap();
                let s_payload = vec![tag; 64 * 1024];
                dm_w.write_data(
                    seek_id,
                    ((iter % 2) * 32 * 1024) as u64,
                    &s_payload,
                    CompressionMode::Seekable {
                        chunk_size: 65536,
                        level: 1,
                    },
                )
                .unwrap();
            }
        }));
    }

    for _ in 0..40 {
        let data = dm.read_data_async(std_id).wait().unwrap();
        assert!(!data.is_empty());
        // Full overwrites always store a uniform byte fill.
        let first = data[0];
        assert!(
            data.iter().all(|&b| b == first),
            "observed mixed/stale blob bytes for standard compressed file"
        );
        let seek_head = dm.read_at_async(seek_id, 0, 8192).wait().unwrap();
        assert_eq!(seek_head.len(), 8192);
    }

    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
    let report = dm.verify_integrity().unwrap();
    assert!(report.is_clean, "unhealthy image: {report:?}");
}

#[test]
fn test_session_failover_keeps_promoted_ipc_server_alive() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("failover_server_alive.img");

    let master = OifsSession::open_local(&img, 4 * 1024 * 1024).unwrap();
    assert!(master.is_direct());
    let root = master.resolve_path(".").unwrap();
    let fid = master.create_file(root, "kept.txt").unwrap();
    master
        .write_data(fid, 0, b"before-failover", CompressionMode::Never)
        .unwrap();

    let client1 = OifsSession::open_local(&img, 0).unwrap();
    let client2 = OifsSession::open_local(&img, 0).unwrap();
    assert!(!client1.is_direct());
    assert!(!client2.is_direct());

    // Master exits.
    drop(master);

    // client1 issues an operation and triggers failover (promoting to Master or reconnecting).
    assert_eq!(client1.read_data(fid).unwrap(), b"before-failover");
    client1
        .write_data(fid, 0, b"after-failover!", CompressionMode::Never)
        .unwrap();

    // Both client2 (pre-existing client) and a brand-new client3 must still talk to the
    // surviving Master IPC server cleanly.
    assert_eq!(client2.read_data(fid).unwrap(), b"after-failover!");
    let client3 = OifsSession::open_local(&img, 0).unwrap();
    assert_eq!(client3.read_data(fid).unwrap(), b"after-failover!");
}
