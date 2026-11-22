//! Live kernel FUSE mount end-to-end integration tests.
//!
//! These tests mount real OIFS `.img` files onto temporary mountpoints through
//! the host OS kernel FUSE stack (`macFUSE` on macOS, `fusermount3`/FUSE on Linux)
//! and exercise genuine POSIX syscalls via `std::fs`.
//!
//! Marked `#[ignore]` so headless CI environments without FUSE installed remain
//! green. Run explicitly on a FUSE-capable host:
//!
//! ```bash
//! cargo test --release --test fuse_live_mount_test -- --ignored --nocapture --test-threads=1
//! ```

use oifs::disk::{CompressionMode, DiskManager};
use oifs::fuse::{FuseMountOptions, OifsFuseFs, unmount};
use oifs::storage::StorageMode;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::tempdir;

/// Checks whether a live kernel FUSE helper is installed on this host.
fn require_live_fuse_environment() {
    #[cfg(target_os = "macos")]
    {
        let macfuse = Path::new("/Library/Filesystems/macfuse.fs/Contents/Resources/mount_macfuse");
        let osxfuse = Path::new("/Library/Filesystems/osxfuse.fs/Contents/Resources/mount_osxfuse");
        assert!(
            macfuse.exists() || osxfuse.exists(),
            "macFUSE / osxfuse mount helper not found; install macFUSE to run live mount tests"
        );
    }
    #[cfg(target_os = "linux")]
    {
        assert!(
            Path::new("/dev/fuse").exists(),
            "/dev/fuse not found; FUSE kernel support is required to run live mount tests"
        );
    }
}

/// RAII guard that mounts an `OifsFuseFs` on a background thread, waits until
/// the kernel mount is active (`st_dev` differs from the parent directory), and
/// guarantees unmount + thread join on `Drop` even if a test panics.
struct LiveMountGuard {
    mountpoint: PathBuf,
    mount_failed: Arc<AtomicBool>,
    handle: Option<JoinHandle<std::io::Result<()>>>,
}

impl LiveMountGuard {
    fn mount(dm: DiskManager, opts: FuseMountOptions, mountpoint: PathBuf) -> Self {
        let parent_dev = fs::metadata(
            mountpoint
                .parent()
                .expect("mountpoint must have a parent directory"),
        )
        .expect("stat parent directory")
        .dev();

        let mountpoint_bg = mountpoint.clone();
        let mount_failed = Arc::new(AtomicBool::new(false));
        let mount_failed_bg = Arc::clone(&mount_failed);

        let handle = thread::spawn(move || {
            let fuse_fs = OifsFuseFs::new(dm, opts);
            let res = fuse_fs.mount(&mountpoint_bg);
            if res.is_err() {
                mount_failed_bg.store(true, Ordering::Release);
            }
            res
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut mounted = false;
        while Instant::now() < deadline {
            if mount_failed.load(Ordering::Acquire) {
                break;
            }
            if let Ok(meta) = fs::metadata(&mountpoint)
                && meta.dev() != parent_dev
            {
                mounted = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        if !mounted {
            let err = if handle.is_finished() {
                format!("{:?}", handle.join())
            } else {
                force_unmount(&mountpoint);
                format!("{:?}", handle.join())
            };
            panic!(
                "live FUSE mount at {} did not become active within timeout: {err}",
                mountpoint.display()
            );
        }

        Self {
            mountpoint,
            mount_failed,
            handle: Some(handle),
        }
    }

    fn path(&self) -> &Path {
        &self.mountpoint
    }

    fn unmount_now(mut self) {
        self.cleanup();
    }

    fn cleanup(&mut self) {
        if let Some(handle) = self.handle.take() {
            force_unmount(&self.mountpoint);
            let join_res = handle.join();
            assert!(
                !self.mount_failed.load(Ordering::Acquire),
                "FUSE mount thread reported an error: {join_res:?}"
            );
            let mount_res = join_res.expect("FUSE mount thread panicked");
            assert!(
                mount_res.is_ok(),
                "FUSE mount returned error on shutdown: {mount_res:?}"
            );
        }
    }
}

impl Drop for LiveMountGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            force_unmount(&self.mountpoint);
            let _ = handle.join();
        }
    }
}

fn force_unmount(mountpoint: &Path) {
    if unmount(mountpoint).is_ok() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("diskutil")
            .args(["unmount", "force"])
            .arg(mountpoint)
            .status();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("fusermount3")
            .args(["-uz"])
            .arg(mountpoint)
            .status();
    }
}

#[test]
#[ignore = "requires live kernel FUSE (macFUSE on macOS or /dev/fuse on Linux)"]
fn test_live_fuse_mount_rw_lifecycle_large_file_dirs_and_macos_unicode() {
    require_live_fuse_environment();

    let dir = tempdir().expect("tempdir");
    let img_path = dir.path().join("live_rw.img");
    let mnt_path = dir.path().join("mnt_rw");
    fs::create_dir(&mnt_path).expect("create mountpoint");

    let dm =
        DiskManager::open_with_options(&img_path, 32 * 1024 * 1024, None, true, StorageMode::Mmap)
            .expect("create journaled image");

    let guard = LiveMountGuard::mount(
        dm,
        FuseMountOptions {
            volname: Some("OIFSLiveRW".to_string()),
            ..FuseMountOptions::default()
        },
        mnt_path.clone(),
    );

    // 1. Create, read, overwrite, append, and truncate via standard POSIX std::fs.
    let hello_path = guard.path().join("hello.txt");
    fs::write(&hello_path, b"hello live fuse\n").expect("write hello.txt");
    assert_eq!(
        fs::read(&hello_path).expect("read hello.txt"),
        b"hello live fuse\n"
    );

    {
        let mut f = OpenOptions::new()
            .append(true)
            .open(&hello_path)
            .expect("open append");
        f.write_all(b"second line\n").expect("append line");
        f.sync_all().expect("fsync");
    }
    assert_eq!(
        fs::read(&hello_path).expect("read appended"),
        b"hello live fuse\nsecond line\n"
    );

    // Truncate shorter and then extend with zero fill.
    {
        let f = OpenOptions::new()
            .write(true)
            .open(&hello_path)
            .expect("open for truncate");
        f.set_len(5).expect("truncate to 5");
    }
    assert_eq!(fs::read(&hello_path).expect("read truncated"), b"hello");

    {
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&hello_path)
            .expect("open for extend");
        f.set_len(12).expect("extend to 12");
        f.seek(SeekFrom::Start(0)).expect("seek 0");
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).expect("read extended");
        assert_eq!(buf, b"hello\0\0\0\0\0\0\0");
    }

    // 2. Large file (1.5 MiB) exercising multi-packet FUSE_WRITE / FUSE_READ.
    let large_path = guard.path().join("large_1_5mb.bin");
    let large_payload: Vec<u8> = (0..(1536 * 1024))
        .map(|i| ((i * 31 + 17) & 0xFF) as u8)
        .collect();
    fs::write(&large_path, &large_payload).expect("write 1.5 MiB file");
    let large_readback = fs::read(&large_path).expect("read 1.5 MiB file");
    assert_eq!(large_readback.len(), large_payload.len());
    assert_eq!(large_readback, large_payload);

    // 3. Nested directories + >200 files to exercise kernel READDIR pagination.
    let nested = guard.path().join("docs").join("many");
    fs::create_dir_all(&nested).expect("create nested dirs");
    for i in 0..210 {
        let p = nested.join(format!("item_{i:03}.txt"));
        fs::write(&p, format!("payload-{i}")).expect("write paginated entry");
    }
    let listed: Vec<String> = fs::read_dir(&nested)
        .expect("read_dir nested")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        listed.len(),
        210,
        "expected all 210 paginated directory entries"
    );
    for i in 0..210 {
        assert!(listed.contains(&format!("item_{i:03}.txt")));
    }

    // Delete a subset and a temporary directory.
    let temp_dir = guard.path().join("ephemeral");
    fs::create_dir(&temp_dir).expect("mkdir ephemeral");
    let temp_file = temp_dir.join("gone.txt");
    fs::write(&temp_file, b"bye").expect("write gone.txt");
    fs::remove_file(&temp_file).expect("unlink gone.txt");
    fs::remove_dir(&temp_dir).expect("rmdir ephemeral");
    assert!(!temp_dir.exists());

    // 3b. Atomic POSIX rename (same-dir, cross-dir, and overwrite) via live kernel FUSE.
    let rename_src = guard.path().join("rename_src.txt");
    let rename_mid = guard.path().join("rename_mid.txt");
    let rename_dst_dir = guard.path().join("rename_dir");
    fs::create_dir(&rename_dst_dir).expect("mkdir rename_dir");
    let rename_dst = rename_dst_dir.join("rename_final.txt");
    fs::write(&rename_src, b"atomic rename payload").expect("write rename_src");
    fs::write(&rename_dst, b"old victim payload").expect("write victim");
    fs::rename(&rename_src, &rename_mid).expect("same-dir live FUSE rename");
    assert!(!rename_src.exists());
    fs::rename(&rename_mid, &rename_dst).expect("cross-dir overwrite live FUSE rename");
    assert!(!rename_mid.exists());
    assert_eq!(
        fs::read(&rename_dst).expect("read renamed file"),
        b"atomic rename payload"
    );

    // 4. UTF-8 accented + CJK filename (NFC creation & NFD lookup on macOS).
    let nfc_name = "caf\u{00E9}_測試.txt";
    let nfc_path = guard.path().join(nfc_name);
    fs::write(&nfc_path, "你好 macFUSE!\n".as_bytes()).expect("write unicode file");
    assert_eq!(
        fs::read(&nfc_path).expect("read NFC file"),
        "你好 macFUSE!\n".as_bytes()
    );
    #[cfg(target_os = "macos")]
    {
        let nfd_path = guard.path().join("cafe\u{0301}_測試.txt");
        assert_eq!(
            fs::read(&nfd_path).expect("read NFD-normalized file on macOS"),
            "你好 macFUSE!\n".as_bytes()
        );
    }

    // 5. macOS Finder metadata noise should be rejected and never pollute `.img`.
    #[cfg(target_os = "macos")]
    {
        let ds_store = guard.path().join(".DS_Store");
        assert!(
            fs::metadata(&ds_store).is_err(),
            ".DS_Store lookup should return ENOENT"
        );
    }

    // Unmount cleanly and verify persistence + fsck directly on `.img`.
    guard.unmount_now();

    let dm_verify = DiskManager::open(&img_path, 32 * 1024 * 1024).expect("reopen image");
    let hello_ino = dm_verify.lookup(0, "hello.txt").expect("lookup hello.txt");
    assert_eq!(
        dm_verify.read_data(hello_ino).expect("read hello.txt"),
        b"hello\0\0\0\0\0\0\0"
    );
    let large_ino = dm_verify
        .lookup(0, "large_1_5mb.bin")
        .expect("lookup large file");
    assert_eq!(
        dm_verify.read_data(large_ino).expect("read large file"),
        large_payload
    );
    let unicode_ino = dm_verify.lookup(0, nfc_name).expect("lookup unicode file");
    assert_eq!(
        dm_verify.read_data(unicode_ino).expect("read unicode file"),
        "你好 macFUSE!\n".as_bytes()
    );
    assert!(
        dm_verify.lookup(0, ".DS_Store").is_err(),
        ".DS_Store must not exist in image"
    );

    let report = dm_verify.verify_integrity().expect("fsck verify_integrity");
    assert!(
        report.is_clean,
        "image failed fsck after live FUSE mount: {report:?}"
    );
}

#[test]
#[ignore = "requires live kernel FUSE (macFUSE on macOS or /dev/fuse on Linux)"]
fn test_live_fuse_mount_read_only_immutability() {
    require_live_fuse_environment();

    let dir = tempdir().expect("tempdir");
    let img_path = dir.path().join("live_ro.img");
    let mnt_path = dir.path().join("mnt_ro");
    fs::create_dir(&mnt_path).expect("create mountpoint");

    // Seed the image, then close and snapshot exact bytes.
    {
        let dm = DiskManager::open(&img_path, 8 * 1024 * 1024).expect("create ro seed image");
        dm.create_file_with_data(0, "immutable.txt", b"do not mutate", CompressionMode::Never)
            .expect("seed file");
        let sub = dm.create_directory(0, "sub").expect("seed dir");
        dm.create_file_with_data(
            sub,
            "nested.txt",
            b"nested content",
            CompressionMode::Always,
        )
        .expect("seed nested");
        dm.flush().expect("flush seed");
    }
    let bytes_before = fs::read(&img_path).expect("snapshot image before mount");

    let dm_ro = DiskManager::open_read_only(&img_path).expect("open_read_only");
    let guard = LiveMountGuard::mount(
        dm_ro,
        FuseMountOptions {
            read_only: true,
            volname: Some("OIFSLiveRO".to_string()),
            ..FuseMountOptions::default()
        },
        mnt_path.clone(),
    );

    // Reads must succeed.
    assert_eq!(
        fs::read(guard.path().join("immutable.txt")).expect("read immutable.txt"),
        b"do not mutate"
    );
    assert_eq!(
        fs::read(guard.path().join("sub").join("nested.txt")).expect("read nested.txt"),
        b"nested content"
    );

    // Every mutating POSIX operation must fail.
    assert!(
        fs::write(guard.path().join("new.txt"), b"nope").is_err(),
        "creating a file on a read-only FUSE mount must fail"
    );
    assert!(
        OpenOptions::new()
            .write(true)
            .open(guard.path().join("immutable.txt"))
            .is_err(),
        "opening a file O_WRONLY on a read-only FUSE mount must fail"
    );
    assert!(
        fs::create_dir(guard.path().join("newdir")).is_err(),
        "mkdir on a read-only FUSE mount must fail"
    );
    assert!(
        fs::remove_file(guard.path().join("immutable.txt")).is_err(),
        "unlink on a read-only FUSE mount must fail"
    );

    guard.unmount_now();

    let bytes_after = fs::read(&img_path).expect("snapshot image after unmount");
    assert_eq!(
        bytes_before, bytes_after,
        "read-only live FUSE mount must keep .img 100% byte-for-byte identical"
    );
}

#[test]
#[ignore = "requires live kernel FUSE (macFUSE on macOS or /dev/fuse on Linux)"]
fn test_live_fuse_mount_encrypted_file_io() {
    require_live_fuse_environment();

    let dir = tempdir().expect("tempdir");
    let img_path = dir.path().join("live_enc.img");
    let mnt_path = dir.path().join("mnt_enc");
    fs::create_dir(&mnt_path).expect("create mountpoint");

    let password = "live-fuse-strong-password-123!";
    let dm = DiskManager::open_with_options(
        &img_path,
        16 * 1024 * 1024,
        Some(password),
        true,
        StorageMode::FileIo,
    )
    .expect("create encrypted FileIo image");

    let guard = LiveMountGuard::mount(
        dm,
        FuseMountOptions {
            volname: Some("OIFSLiveEnc".to_string()),
            ..FuseMountOptions::default()
        },
        mnt_path.clone(),
    );

    let secret_path = guard.path().join("secret.txt");
    let secret_data = b"top-secret encrypted payload through macFUSE + FileIo";
    fs::write(&secret_path, secret_data).expect("write secret.txt");
    assert_eq!(
        fs::read(&secret_path).expect("read secret.txt"),
        secret_data
    );

    guard.unmount_now();

    let dm_verify = DiskManager::open_with_options(
        &img_path,
        16 * 1024 * 1024,
        Some(password),
        true,
        StorageMode::FileIo,
    )
    .expect("reopen encrypted FileIo image");
    let ino = dm_verify
        .lookup(0, "secret.txt")
        .expect("lookup secret.txt");
    assert_eq!(
        dm_verify.read_data(ino).expect("read secret.txt"),
        secret_data
    );
    let report = dm_verify.verify_integrity().expect("fsck encrypted image");
    assert!(
        report.is_clean,
        "encrypted image failed fsck after live FUSE mount: {report:?}"
    );
}

fn bench_dir_workload(label: &str, root: &Path) {
    let small_dir = root.join("bench_small");
    fs::create_dir(&small_dir).expect("mkdir bench_small");
    let small_payload = vec![0x5Au8; 4096];
    let n_small = 500usize;

    let t0 = Instant::now();
    for i in 0..n_small {
        fs::write(small_dir.join(format!("f_{i:04}.bin")), &small_payload).expect("write small");
    }
    let small_w = t0.elapsed();

    let t0 = Instant::now();
    let mut sum = 0u64;
    for i in 0..n_small {
        let data = fs::read(small_dir.join(format!("f_{i:04}.bin"))).expect("read small");
        sum += data[0] as u64;
    }
    let small_r = t0.elapsed();

    let t0 = Instant::now();
    for i in 0..n_small {
        fs::remove_file(small_dir.join(format!("f_{i:04}.bin"))).expect("delete small");
    }
    let small_d = t0.elapsed();
    let _ = fs::remove_dir(&small_dir);

    // 16 MiB sequential write + read + 2,000 random 4 KiB reads
    let large_path = root.join("bench_16m.bin");
    let large_payload: Vec<u8> = (0..(16 * 1024 * 1024))
        .map(|i| ((i * 131 + 7) & 0xFF) as u8)
        .collect();

    let t0 = Instant::now();
    fs::write(&large_path, &large_payload).expect("write 16M");
    let seq_w = t0.elapsed();

    let t0 = Instant::now();
    let readback = fs::read(&large_path).expect("read 16M");
    let seq_r = t0.elapsed();
    assert_eq!(readback.len(), large_payload.len());

    let mut f = fs::File::open(&large_path).expect("open 16M for random read");
    let mut buf = [0u8; 4096];
    let n_rnd = 2000usize;
    let t0 = Instant::now();
    for i in 0..n_rnd {
        let off = (((i * 7919) % 4096) * 4096) as u64;
        f.seek(SeekFrom::Start(off)).expect("seek");
        f.read_exact(&mut buf).expect("read 4K");
        sum += buf[0] as u64;
    }
    let rnd_r = t0.elapsed();
    drop(f);
    let _ = fs::remove_file(&large_path);

    let seq_w_mbs = 16.0 / seq_w.as_secs_f64();
    let seq_r_mbs = 16.0 / seq_r.as_secs_f64();
    let small_w_ops = n_small as f64 / small_w.as_secs_f64();
    let small_r_ops = n_small as f64 / small_r.as_secs_f64();
    let small_d_ops = n_small as f64 / small_d.as_secs_f64();
    let rnd_ops = n_rnd as f64 / rnd_r.as_secs_f64();

    println!(
        "{label:<26} | 500x4K W: {:6.1}ms ({:6.0}/s) | R: {:5.1}ms ({:7.0}/s) | D: {:5.1}ms ({:6.0}/s) | 16M W: {:6.1}ms ({:6.1} MB/s) | 16M R: {:5.1}ms ({:7.1} MB/s) | 2k4K Rnd: {:5.1}ms ({:7.0}/s) [chk={sum}]",
        small_w.as_secs_f64() * 1000.0,
        small_w_ops,
        small_r.as_secs_f64() * 1000.0,
        small_r_ops,
        small_d.as_secs_f64() * 1000.0,
        small_d_ops,
        seq_w.as_secs_f64() * 1000.0,
        seq_w_mbs,
        seq_r.as_secs_f64() * 1000.0,
        seq_r_mbs,
        rnd_r.as_secs_f64() * 1000.0,
        rnd_ops,
    );
}

#[test]
#[ignore = "requires live kernel FUSE; run with --ignored --nocapture --test-threads=1"]
fn bench_live_fuse_mount_performance() {
    require_live_fuse_environment();

    let dir = tempdir().expect("tempdir");
    println!();

    // 1. Native host filesystem baseline (APFS on macOS)
    let native_dir = dir.path().join("native");
    fs::create_dir(&native_dir).expect("mkdir native");
    for round in 1..=3 {
        bench_dir_workload(&format!("APFS (r{round})"), &native_dir);
    }

    // 2. Live FUSE A/B comparison: 128 KiB vs 1 MiB max_write for Mmap+WAL and FileIo+WAL
    for (storage_label, mode) in [
        ("Mmap+WAL", StorageMode::Mmap),
        ("FileIo+WAL", StorageMode::FileIo),
    ] {
        for (mw_label, max_write) in [("128K", 128 * 1024), ("1M", 1024 * 1024)] {
            let tag = format!("{storage_label}_{mw_label}");
            let img_path = dir.path().join(format!("bench_{tag}.img"));
            let mnt_path = dir.path().join(format!("mnt_{tag}"));
            fs::create_dir(&mnt_path).expect("mkdir mnt");
            let dm = DiskManager::open_with_options(&img_path, 64 * 1024 * 1024, None, true, mode)
                .expect("open bench image");
            let guard = LiveMountGuard::mount(
                dm,
                FuseMountOptions {
                    volname: Some(format!("OIFS{mw_label}")),
                    max_write,
                    ..FuseMountOptions::default()
                },
                mnt_path,
            );
            for round in 1..=3 {
                bench_dir_workload(
                    &format!("{storage_label} {mw_label} (r{round})"),
                    guard.path(),
                );
            }
            guard.unmount_now();
        }
    }
}
