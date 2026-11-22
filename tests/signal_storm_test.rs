//! High-frequency POSIX signal storm & `EINTR` resilience test suite.
//!
//! Simulates running OIFS inside a host process subject to frequent asynchronous signals
//! (`SIGALRM`, `SIGPROF`, `SIGVTALRM`, `SIGUSR1`, `SIGUSR2`) installed **without**
//! `SA_RESTART`, so interruptible kernel syscalls (`poll`, `accept`, `connect`, `msync`,
//! `fdatasync`, `fsync`, `fcntl`, `fstat`, `recvmsg`, `sendmsg`, `pread`, `pwrite`,
//! `clonefile`/`FICLONE`, `copy_file_range`) actually observe `EINTR`.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use oifs::disk::{
    CompressionMode, DiskManager, DurabilityMode, SpliceOptions, SpliceTrimMode, TransferOptions,
};
use oifs::fuse::{
    FUSE_DESTROY, FUSE_GETATTR, FUSE_IN_HEADER_SIZE, FUSE_INIT, FuseMountOptions, OifsFuseFs,
    recv_fd_scm_rights, send_fd_scm_rights,
};
use oifs::session::OifsSession;
use oifs::storage::StorageMode;
use tempfile::tempdir;

static SIGALRM_COUNT: AtomicUsize = AtomicUsize::new(0);
static SIGPROF_COUNT: AtomicUsize = AtomicUsize::new(0);
static SIGVTALRM_COUNT: AtomicUsize = AtomicUsize::new(0);
static SIGUSR1_COUNT: AtomicUsize = AtomicUsize::new(0);
static SIGUSR2_COUNT: AtomicUsize = AtomicUsize::new(0);

extern "C" fn storm_signal_handler(sig: libc::c_int) {
    match sig {
        libc::SIGALRM => {
            SIGALRM_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        libc::SIGPROF => {
            SIGPROF_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        libc::SIGVTALRM => {
            SIGVTALRM_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        libc::SIGUSR1 => {
            SIGUSR1_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        libc::SIGUSR2 => {
            SIGUSR2_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

/// Process-wide serialization lock because POSIX signal dispositions and `setitimer`
/// timers are shared across the entire process.
fn storm_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug, Clone, Copy)]
struct StormCounts {
    alrm: usize,
    prof: usize,
    vtalrm: usize,
    usr1: usize,
    usr2: usize,
}

impl StormCounts {
    fn total(self) -> usize {
        self.alrm + self.prof + self.vtalrm + self.usr1 + self.usr2
    }
}

/// RAII harness that installs non-restarting (`sa_flags = 0`) handlers for `SIGALRM`,
/// `SIGPROF`, `SIGVTALRM`, `SIGUSR1`, and `SIGUSR2`, starts interval timers (`ITIMER_REAL`,
/// `ITIMER_PROF`, `ITIMER_VIRTUAL`), and spawns a background thread firing `SIGUSR1`/`SIGUSR2`
/// directly at registered threads (and the process).
struct SignalStorm {
    _guard: std::sync::MutexGuard<'static, ()>,
    prev_actions: Vec<(libc::c_int, libc::sigaction)>,
    stop: Arc<AtomicBool>,
    targets: Arc<Mutex<Vec<libc::pthread_t>>>,
    bomber: Option<thread::JoinHandle<()>>,
    start_counts: StormCounts,
}

impl SignalStorm {
    fn snapshot() -> StormCounts {
        StormCounts {
            alrm: SIGALRM_COUNT.load(Ordering::Relaxed),
            prof: SIGPROF_COUNT.load(Ordering::Relaxed),
            vtalrm: SIGVTALRM_COUNT.load(Ordering::Relaxed),
            usr1: SIGUSR1_COUNT.load(Ordering::Relaxed),
            usr2: SIGUSR2_COUNT.load(Ordering::Relaxed),
        }
    }

    fn start(interval_us: i64) -> Self {
        let guard = storm_lock().lock().unwrap_or_else(|p| p.into_inner());
        let signals = [
            libc::SIGALRM,
            libc::SIGPROF,
            libc::SIGVTALRM,
            libc::SIGUSR1,
            libc::SIGUSR2,
        ];
        let mut prev_actions = Vec::with_capacity(signals.len());

        // Install handlers WITHOUT SA_RESTART (`sa_flags = 0`) so interruptible syscalls
        // genuinely return EINTR.
        for sig in signals {
            unsafe {
                let mut new_act = std::mem::zeroed::<libc::sigaction>();
                let mut old_act = std::mem::zeroed::<libc::sigaction>();
                new_act.sa_sigaction = storm_signal_handler as *const () as usize;
                new_act.sa_flags = 0; // Deliberately omit SA_RESTART!
                libc::sigemptyset(&mut new_act.sa_mask);
                assert_eq!(
                    libc::sigaction(sig, &new_act, &mut old_act),
                    0,
                    "sigaction failed for signal {sig}"
                );
                prev_actions.push((sig, old_act));
            }
        }

        let start_counts = Self::snapshot();

        // Arm ITIMER_REAL (SIGALRM), ITIMER_PROF (SIGPROF), and ITIMER_VIRTUAL (SIGVTALRM).
        let timer = libc::itimerval {
            it_interval: libc::timeval {
                tv_sec: 0,
                tv_usec: interval_us as _,
            },
            it_value: libc::timeval {
                tv_sec: 0,
                tv_usec: interval_us as _,
            },
        };
        unsafe {
            assert_eq!(
                libc::setitimer(libc::ITIMER_REAL, &timer, std::ptr::null_mut()),
                0,
                "setitimer(ITIMER_REAL) failed"
            );
            assert_eq!(
                libc::setitimer(libc::ITIMER_PROF, &timer, std::ptr::null_mut()),
                0,
                "setitimer(ITIMER_PROF) failed"
            );
            assert_eq!(
                libc::setitimer(libc::ITIMER_VIRTUAL, &timer, std::ptr::null_mut()),
                0,
                "setitimer(ITIMER_VIRTUAL) failed"
            );
        }

        let stop = Arc::new(AtomicBool::new(false));
        let targets = Arc::new(Mutex::new(vec![unsafe { libc::pthread_self() }]));
        let stop_bg = Arc::clone(&stop);
        let targets_bg = Arc::clone(&targets);
        let pid = unsafe { libc::getpid() };

        let bomber = thread::spawn(move || {
            let mut tick = 0usize;
            while !stop_bg.load(Ordering::Relaxed) {
                let sig = if tick.is_multiple_of(2) {
                    libc::SIGUSR1
                } else {
                    libc::SIGUSR2
                };
                // Send directly to registered threads so their blocking syscalls get EINTR,
                // plus process-wide delivery so internal background threads (IpcServer,
                // group-commit, worker pool) also receive signals.
                let tids: Vec<libc::pthread_t> = targets_bg
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                for tid in tids {
                    unsafe {
                        libc::pthread_kill(tid, sig);
                    }
                }
                unsafe {
                    libc::kill(pid, sig);
                }
                tick = tick.wrapping_add(1);
                thread::sleep(Duration::from_micros(300));
            }
        });

        Self {
            _guard: guard,
            prev_actions,
            stop,
            targets,
            bomber: Some(bomber),
            start_counts,
        }
    }

    fn register_current_thread(&self) {
        let tid = unsafe { libc::pthread_self() };
        let mut guard = self.targets.lock().unwrap_or_else(|p| p.into_inner());
        if !guard.contains(&tid) {
            guard.push(tid);
        }
    }

    fn unregister_current_thread(&self) {
        let tid = unsafe { libc::pthread_self() };
        let mut guard = self.targets.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|&t| t != tid);
    }

    fn delivered(&self) -> StormCounts {
        let now = Self::snapshot();
        StormCounts {
            alrm: now.alrm.saturating_sub(self.start_counts.alrm),
            prof: now.prof.saturating_sub(self.start_counts.prof),
            vtalrm: now.vtalrm.saturating_sub(self.start_counts.vtalrm),
            usr1: now.usr1.saturating_sub(self.start_counts.usr1),
            usr2: now.usr2.saturating_sub(self.start_counts.usr2),
        }
    }
}

impl Drop for SignalStorm {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.bomber.take() {
            let _ = h.join();
        }

        // Disarm all interval timers before restoring handlers.
        let zero = libc::itimerval {
            it_interval: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            it_value: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        };
        unsafe {
            let _ = libc::setitimer(libc::ITIMER_REAL, &zero, std::ptr::null_mut());
            let _ = libc::setitimer(libc::ITIMER_PROF, &zero, std::ptr::null_mut());
            let _ = libc::setitimer(libc::ITIMER_VIRTUAL, &zero, std::ptr::null_mut());
        }

        // Drain any pending signals briefly before restoring previous dispositions.
        thread::sleep(Duration::from_millis(5));

        for (sig, prev) in &self.prev_actions {
            unsafe {
                let _ = libc::sigaction(*sig, prev, std::ptr::null_mut());
            }
        }
    }
}

#[test]
fn test_signal_storm_disk_manager_mmap_and_file_io_strict_wal_encrypted() {
    let storm = SignalStorm::start(500);
    let dir = tempdir().unwrap();

    for (label, mode, durability, encrypted) in [
        ("mmap_strict_wal", StorageMode::Mmap, DurabilityMode::Strict, false),
        ("fileio_strict_wal", StorageMode::FileIo, DurabilityMode::Strict, false),
        (
            "mmap_range_async_enc",
            StorageMode::Mmap,
            DurabilityMode::RangeAsync,
            true,
        ),
        (
            "fileio_strict_enc",
            StorageMode::FileIo,
            DurabilityMode::Strict,
            true,
        ),
    ] {
        let img = dir.path().join(format!("{label}.img"));
        let dm = DiskManager::open_with_options(
            &img,
            16 * 1024 * 1024,
            if encrypted { Some("sigpwd") } else { None },
            true,
            mode,
        )
        .expect("open_with_options");
        dm.set_durability_mode(durability);

        let root = dm.resolve_path(".").unwrap();
        let sub = dm.create_directory(root, "work").unwrap();

        let payload_small: Vec<u8> = (0usize..4096).map(|i| (i * 31 % 251) as u8).collect();
        let payload_chunked: Vec<u8> = (0usize..(192 * 1024))
            .map(|i| ((i / 64).wrapping_mul(17) % 251) as u8)
            .collect();

        for i in 0usize..15 {
            let name = format!("f_{i}.bin");
            let ino = dm
                .create_file_with_data(sub, &name, &payload_small, CompressionMode::Never)
                .unwrap_or_else(|e| panic!("{label}: create_file_with_data {i} failed: {e}"));

            let mut buf = vec![0u8; payload_small.len()];
            let n = dm
                .read_at(ino, 0, &mut buf)
                .unwrap_or_else(|e| panic!("{label}: read_at {i} failed: {e}"));
            assert_eq!(n, payload_small.len());
            assert_eq!(buf, payload_small);

            if i.is_multiple_of(3) {
                dm.write_data(
                    ino,
                    0,
                    &payload_chunked,
                    CompressionMode::Seekable {
                        chunk_size: 64 * 1024,
                        level: 1,
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: seekable write {i} failed: {e}"));
                let got = dm
                    .read_data(ino)
                    .unwrap_or_else(|e| panic!("{label}: seekable read {i} failed: {e}"));
                assert_eq!(got, payload_chunked);

                dm.truncate(ino, 70_000)
                    .unwrap_or_else(|e| panic!("{label}: truncate {i} failed: {e}"));
                let trunc_got = dm.read_data(ino).unwrap();
                assert_eq!(trunc_got, &payload_chunked[..70_000]);
            }

            if i.is_multiple_of(5) {
                let renamed = format!("renamed_{i}.bin");
                dm.rename(sub, &name, sub, &renamed)
                    .unwrap_or_else(|e| panic!("{label}: rename {i} failed: {e}"));
                dm.delete_file(sub, &renamed)
                    .unwrap_or_else(|e| panic!("{label}: delete {i} failed: {e}"));
            }
        }

        dm.flush()
            .unwrap_or_else(|e| panic!("{label}: flush failed: {e}"));
        let report = dm
            .verify_integrity()
            .unwrap_or_else(|e| panic!("{label}: verify_integrity failed: {e}"));
        assert!(
            report.is_clean,
            "{label}: integrity check failed: {report:?}"
        );
    }

    let counts = storm.delivered();
    eprintln!("[SignalStorm] DiskManager delivered: {counts:?}");
    assert!(
        counts.total() >= 20,
        "Expected heavy signal delivery during DiskManager storm, got {counts:?}"
    );
    assert!(
        (counts.alrm > 0 || counts.prof > 0) && counts.usr1 > 0 && counts.usr2 > 0,
        "Expected timer + SIGUSR1 + SIGUSR2 deliveries, got {counts:?}"
    );
}

#[test]
fn test_signal_storm_cross_image_clone_transfer_and_splice() {
    let storm = SignalStorm::start(500);
    let dir = tempdir().unwrap();

    let src_path = dir.path().join("src.img");
    let dst_path = dir.path().join("dst.img");
    let src = DiskManager::open_journaled(&src_path, 16 * 1024 * 1024).unwrap();
    let dst = DiskManager::open_journaled(&dst_path, 16 * 1024 * 1024).unwrap();
    src.set_durability_mode(DurabilityMode::Strict);
    dst.set_durability_mode(DurabilityMode::Strict);

    let raw_data: Vec<u8> = (0..(128 * 1024)).map(|i| (i % 251) as u8).collect();
    let seek_data: Vec<u8> = (0..(256 * 1024)).map(|i| ((i / 128) % 199) as u8).collect();

    let _s_raw = src
        .create_file_with_data(0, "raw.bin", &raw_data, CompressionMode::Never)
        .unwrap();
    let s_seek = src
        .create_file_with_data(
            0,
            "seek.bin",
            &seek_data,
            CompressionMode::Seekable {
                chunk_size: 64 * 1024,
                level: 1,
            },
        )
        .unwrap();
    src.flush().unwrap();

    // Repeated whole-image clones under signal storm
    for i in 0..8 {
        let clone_path = dir.path().join(format!("clone_{i}.img"));
        DiskManager::clone_image(&src_path, &clone_path)
            .unwrap_or_else(|e| panic!("clone_image {i} failed under signal storm: {e}"));
    }

    // Repeated cross-image whole-file transfers + sub-range splices under signal storm
    for i in 0..6 {
        let dst_raw_name = format!("t_raw_{i}.bin");
        let stats_raw = src
            .transfer_file_to(0, "raw.bin", &dst, 0, &dst_raw_name, TransferOptions::default())
            .unwrap_or_else(|e| panic!("transfer raw {i} failed under storm: {e}"));
        assert_eq!(dst.read_data(stats_raw.dst_inode).unwrap(), raw_data);

        let dst_seek_name = format!("t_seek_{i}.bin");
        let stats_seek = src
            .transfer_file_to(0, "seek.bin", &dst, 0, &dst_seek_name, TransferOptions::default())
            .unwrap_or_else(|e| panic!("transfer seekable {i} failed under storm: {e}"));
        assert_eq!(dst.read_data(stats_seek.dst_inode).unwrap(), seek_data);

        let splice_target = dst
            .create_file_with_data(
                0,
                &format!("splice_{i}.bin"),
                &vec![0xAAu8; 256 * 1024],
                CompressionMode::Seekable {
                    chunk_size: 64 * 1024,
                    level: 1,
                },
            )
            .unwrap();
        let splice_opts = SpliceOptions {
            trim_mode: SpliceTrimMode::Keep,
        };
        src.splice_range_to(
            s_seek,
            64 * 1024,
            &dst,
            splice_target,
            64 * 1024,
            128 * 1024,
            splice_opts,
        )
        .unwrap_or_else(|e| panic!("splice {i} failed under storm: {e}"));

        let mut verify = vec![0u8; 128 * 1024];
        dst.read_at(splice_target, 64 * 1024, &mut verify).unwrap();
        assert_eq!(verify, &seek_data[64 * 1024..192 * 1024]);
    }

    assert!(src.verify_integrity().unwrap().is_clean);
    assert!(dst.verify_integrity().unwrap().is_clean);

    let counts = storm.delivered();
    eprintln!("[SignalStorm] Transfer/Splice delivered: {counts:?}");
    assert!(
        counts.total() >= 10,
        "Expected signal delivery during transfer/splice storm, got {counts:?}"
    );
}

#[test]
fn test_signal_storm_ipc_local_and_network_survives_eintr() {
    let storm = SignalStorm::start(400);
    let dir = tempdir().unwrap();

    // Test both Local UDS and Network TCP IPC modes under intense EINTR.
    for is_network in [false, true] {
        let img = dir
            .path()
            .join(if is_network { "ipc_net.img" } else { "ipc_uds.img" });
        let master = if is_network {
            OifsSession::open_network(&img, 16 * 1024 * 1024, None).unwrap()
        } else {
            OifsSession::open_local(&img, 16 * 1024 * 1024).unwrap()
        };
        assert!(master.is_direct());

        // Repeatedly open fresh client sessions mid-storm to prove IpcServer's accept()
        // loop NEVER exits on EINTR, and run concurrent RPCs across pooled secondary streams.
        for round in 0..5 {
            let client = Arc::new(if is_network {
                OifsSession::open_network(&img, 0, None)
                    .unwrap_or_else(|e| panic!("network client open round {round} failed: {e}"))
            } else {
                OifsSession::open_local(&img, 0)
                    .unwrap_or_else(|e| panic!("local client open round {round} failed: {e}"))
            });
            assert!(!client.is_direct());

            let mut workers = Vec::new();
            for tidx in 0..4 {
                let c = Arc::clone(&client);
                let targets = Arc::clone(&storm.targets);
                workers.push(thread::spawn(move || {
                    let tid = unsafe { libc::pthread_self() };
                    targets.lock().unwrap_or_else(|p| p.into_inner()).push(tid);
                    let root = c.resolve_path(".").unwrap();
                    let name = format!("r{round}_t{tidx}.bin");
                    let ino = c
                        .create_or_open_file(root, &name)
                        .unwrap_or_else(|e| panic!("create_or_open_file failed: {e}"));
                    // Second create_or_open_file exercises Remote AlreadyExists path
                    let ino2 = c
                        .create_or_open_file(root, &name)
                        .unwrap_or_else(|e| panic!("second create_or_open_file failed: {e}"));
                    assert_eq!(ino, ino2);

                    let payload = vec![(round * 16 + tidx) as u8; 8192];
                    c.write_data(ino, 0, &payload, CompressionMode::Never)
                        .unwrap_or_else(|e| panic!("write_data failed: {e}"));
                    let read_back = c
                        .read_data(ino)
                        .unwrap_or_else(|e| panic!("read_data failed: {e}"));
                    assert_eq!(read_back, payload);

                    targets
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .retain(|&t| t != tid);
                }));
            }
            for w in workers {
                w.join().unwrap();
            }
            client.flush().unwrap();
            drop(client);
        }

        drop(master);
    }

    let counts = storm.delivered();
    eprintln!("[SignalStorm] IPC Local+Network delivered: {counts:?}");
    assert!(
        counts.total() >= 10,
        "Expected heavy signal delivery during IPC storm, got {counts:?}"
    );
}

#[test]
fn test_signal_storm_fuse_scm_rights_and_stream_dispatch() {
    let storm = SignalStorm::start(400);
    let dir = tempdir().unwrap();

    // 1. Hammer send_fd_scm_rights / recv_fd_scm_rights across a Unix socketpair
    // while both sender and receiver threads are targeted with non-restarting signals.
    let (sock_a, sock_b) = UnixStream::pair().unwrap();
    let sample_file = std::fs::File::create(dir.path().join("fd_payload.txt")).unwrap();
    let targets = Arc::clone(&storm.targets);

    let sender_fd = sock_a.as_raw_fd();
    let payload_fd = sample_file.as_raw_fd();
    let sender = thread::spawn(move || {
        let tid = unsafe { libc::pthread_self() };
        targets.lock().unwrap_or_else(|p| p.into_inner()).push(tid);
        for i in 0usize..40 {
            if i.is_multiple_of(5) {
                // Brief pause forces receiver to block inside `recvmsg`, making EINTR very likely.
                thread::sleep(Duration::from_millis(2));
            }
            send_fd_scm_rights(sender_fd, payload_fd)
                .unwrap_or_else(|e| panic!("send_fd_scm_rights #{i} failed under storm: {e}"));
        }
        targets
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|&t| t != tid);
    });

    for i in 0..40 {
        let owned = recv_fd_scm_rights(sock_b.as_raw_fd())
            .unwrap_or_else(|e| panic!("recv_fd_scm_rights #{i} failed under storm: {e}"));
        assert!(owned.as_raw_fd() >= 0);
    }
    sender.join().unwrap();

    // 2. Hammer OifsFuseFs::serve_stream over UnixStream::pair() with FUSE frames
    let img = dir.path().join("fuse_storm.img");
    let dm = Arc::new(DiskManager::open(&img, 8 * 1024 * 1024).unwrap());
    let fs = Arc::new(OifsFuseFs::from_arc(dm, FuseMountOptions::default()));
    let (mut server_stream, mut client_stream) = UnixStream::pair().unwrap();

    let fs_srv = Arc::clone(&fs);
    let targets2 = Arc::clone(&storm.targets);
    let srv_thread = thread::spawn(move || {
        let tid = unsafe { libc::pthread_self() };
        targets2.lock().unwrap_or_else(|p| p.into_inner()).push(tid);
        fs_srv
            .serve_stream(&mut server_stream)
            .expect("serve_stream must survive signal storm");
        targets2
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|&t| t != tid);
    });

    fn build_fuse_frame(opcode: u32, unique: u64, nodeid: u64, payload: &[u8]) -> Vec<u8> {
        let total = (FUSE_IN_HEADER_SIZE + payload.len()) as u32;
        let mut frame = vec![0u8; total as usize];
        frame[0..4].copy_from_slice(&total.to_le_bytes());
        frame[4..8].copy_from_slice(&opcode.to_le_bytes());
        frame[8..16].copy_from_slice(&unique.to_le_bytes());
        frame[16..24].copy_from_slice(&nodeid.to_le_bytes());
        frame[FUSE_IN_HEADER_SIZE..].copy_from_slice(payload);
        frame
    }

    fn read_fuse_reply(stream: &mut UnixStream) -> (i32, Vec<u8>) {
        let mut hdr = [0u8; 16];
        stream.read_exact(&mut hdr).unwrap();
        let len = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
        let err = i32::from_le_bytes(hdr[4..8].try_into().unwrap());
        let mut rest = vec![0u8; len.saturating_sub(16)];
        if !rest.is_empty() {
            stream.read_exact(&mut rest).unwrap();
        }
        (err, rest)
    }

    // FUSE_INIT (ABI 7.28)
    let mut init_payload = [0u8; 16];
    init_payload[0..4].copy_from_slice(&7u32.to_le_bytes());
    init_payload[4..8].copy_from_slice(&28u32.to_le_bytes());
    let init_frame = build_fuse_frame(FUSE_INIT, 1, 0, &init_payload);
    client_stream.write_all(&init_frame).unwrap();
    let (err, _) = read_fuse_reply(&mut client_stream);
    assert_eq!(err, 0);

    // 100 FUSE_GETATTR frames with occasional pauses so server blocks inside read() under signals
    for i in 0..100u64 {
        if i.is_multiple_of(20) {
            thread::sleep(Duration::from_millis(2));
        }
        let getattr_frame = build_fuse_frame(FUSE_GETATTR, 2 + i, 1, &[0u8; 16]);
        client_stream.write_all(&getattr_frame).unwrap();
        let (err, _) = read_fuse_reply(&mut client_stream);
        assert_eq!(err, 0, "FUSE_GETATTR #{i} failed with errno {err}");
    }

    let destroy_frame = build_fuse_frame(FUSE_DESTROY, 9999, 0, &[]);
    client_stream.write_all(&destroy_frame).unwrap();
    let (err, _) = read_fuse_reply(&mut client_stream);
    assert_eq!(err, 0);

    drop(client_stream);
    srv_thread.join().unwrap();

    let counts = storm.delivered();
    eprintln!("[SignalStorm] FUSE SCM_RIGHTS+Stream delivered: {counts:?}");
    assert!(
        counts.total() >= 10,
        "Expected signal delivery during FUSE storm, got {counts:?}"
    );
}

#[test]
fn test_signal_storm_async_read_pool_and_group_commit() {
    let storm = SignalStorm::start(400);
    storm.register_current_thread();

    let dir = tempdir().unwrap();
    let img = dir.path().join("async_storm.img");
    let dm = Arc::new(DiskManager::open_journaled(&img, 16 * 1024 * 1024).unwrap());
    dm.set_durability_mode(DurabilityMode::Strict);

    let mut inodes = Vec::new();
    for i in 0..8 {
        inodes.push(dm.create_file(0, &format!("async_{i}.bin")).unwrap());
    }

    // Launch concurrent async Group-Commit writes + async reads under signal storm
    let mut handles = Vec::new();
    for (idx, &ino) in inodes.iter().enumerate() {
        let dm_c = Arc::clone(&dm);
        let targets = Arc::clone(&storm.targets);
        handles.push(thread::spawn(move || {
            let tid = unsafe { libc::pthread_self() };
            targets.lock().unwrap_or_else(|p| p.into_inner()).push(tid);
            for round in 0..10u8 {
                let payload = vec![(idx as u8).wrapping_add(round); 4096];
                dm_c.write_data_async(ino, 0, payload.clone(), CompressionMode::Never)
                    .wait()
                    .unwrap_or_else(|e| {
                        panic!("write_data_async idx={idx} round={round} failed: {e}")
                    });
                let read_back = dm_c
                    .read_at_async(ino, 0, 4096)
                    .wait()
                    .unwrap_or_else(|e| {
                        panic!("read_at_async idx={idx} round={round} failed: {e}")
                    });
                assert_eq!(read_back, payload);
            }
            targets
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|&t| t != tid);
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    dm.flush().unwrap();
    assert!(dm.verify_integrity().unwrap().is_clean);
    storm.unregister_current_thread();

    let counts = storm.delivered();
    eprintln!("[SignalStorm] Async ReadPool+GroupCommit delivered: {counts:?}");
    assert!(
        counts.total() >= 10,
        "Expected signal delivery during async storm, got {counts:?}"
    );
}
