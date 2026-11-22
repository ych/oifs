//! Live OS kernel `mount_nfs` E2E tests for OIFS Pure-Rust Local NFSv3 Server.
//!
//! Zero third-party drivers / zero kext / zero reboot required on macOS!
//! Ignored by default so CI runners without mount permissions skip it; run directly via:
//!   `cargo test --test nfs_live_mount_test -- --ignored --nocapture`

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use oifs::disk::{CompressionMode, DiskManager};
use oifs::fuse::unmount;
use oifs::nfs::{NfsMountOptions, OifsNfsServer, is_mounted, mount_os_nfs};
use tempfile::tempdir;

struct LiveNfsMountGuard {
    mountpoint: PathBuf,
    server: OifsNfsServer,
}

impl LiveNfsMountGuard {
    fn mount(dm: Arc<DiskManager>, mountpoint: &Path, read_only: bool) -> Self {
        let server = OifsNfsServer::bind(
            dm,
            NfsMountOptions {
                read_only,
                ..NfsMountOptions::default()
            },
        )
        .expect("bind local NFSv3 server");
        mount_os_nfs(server.port(), mountpoint, read_only).expect("mount_os_nfs failed");
        assert!(
            is_mounted(mountpoint),
            "mountpoint {:?} should report mounted",
            mountpoint
        );
        Self {
            mountpoint: mountpoint.to_path_buf(),
            server,
        }
    }
}

impl Drop for LiveNfsMountGuard {
    fn drop(&mut self) {
        let _ = unmount(&self.mountpoint);
        self.server.stop();
    }
}

#[test]
#[ignore = "Requires live OS NFS mount privileges (run with -- --ignored --nocapture)"]
fn test_live_nfs_mount_full_posix_lifecycle_and_fsck() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("live_nfs.img");
    let mp = dir.path().join("mnt");
    fs::create_dir_all(&mp).unwrap();

    let dm = Arc::new(DiskManager::open_journaled(&img, 32 * 1024 * 1024).unwrap());
    let guard = LiveNfsMountGuard::mount(Arc::clone(&dm), &mp, false);

    // 1. Create, write, read, append, seek-write, truncate
    let f1 = mp.join("hello.txt");
    fs::write(&f1, b"Hello via zero-kext NFSv3!").unwrap();
    assert_eq!(fs::read(&f1).unwrap(), b"Hello via zero-kext NFSv3!");

    {
        let mut f = OpenOptions::new().append(true).open(&f1).unwrap();
        f.write_all(b" Appended!").unwrap();
        f.sync_all().unwrap();
    }
    assert_eq!(
        fs::read(&f1).unwrap(),
        b"Hello via zero-kext NFSv3! Appended!"
    );

    {
        let f = OpenOptions::new().write(true).open(&f1).unwrap();
        f.set_len(5).unwrap();
    }
    assert_eq!(fs::read(&f1).unwrap(), b"Hello");

    // 2. Large 2 MiB file write + random read
    let large = mp.join("large_2m.bin");
    let pattern: Vec<u8> = (0usize..(2 * 1024 * 1024))
        .map(|i| ((i.wrapping_mul(37) ^ (i >> 8)) & 0xFF) as u8)
        .collect();
    fs::write(&large, &pattern).unwrap();
    assert_eq!(fs::read(&large).unwrap(), pattern);

    {
        let mut f = fs::File::open(&large).unwrap();
        f.seek(SeekFrom::Start(1_048_576)).unwrap();
        let mut slice = vec![0u8; 4096];
        f.read_exact(&mut slice).unwrap();
        assert_eq!(slice, &pattern[1_048_576..1_048_576 + 4096]);
    }

    // 3. Nested directories, atomic cross-directory rename, Unicode NFC/NFD, readdir
    let sub1 = mp.join("alpha/beta");
    let sub2 = mp.join("gamma");
    fs::create_dir_all(&sub1).unwrap();
    fs::create_dir_all(&sub2).unwrap();

    let src_file = sub1.join("caf\u{00e9}.txt");
    fs::write(&src_file, b"unicode payload").unwrap();
    let dst_file = sub2.join("moved_caf\u{00e9}.txt");
    fs::rename(&src_file, &dst_file).unwrap();
    assert!(!src_file.exists());
    assert_eq!(fs::read(&dst_file).unwrap(), b"unicode payload");

    let entries: Vec<String> = fs::read_dir(&mp)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(entries.contains(&"hello.txt".to_string()));
    assert!(entries.contains(&"large_2m.bin".to_string()));
    assert!(entries.contains(&"alpha".to_string()));
    assert!(entries.contains(&"gamma".to_string()));

    // 4. Delete files and remove empty directories
    fs::remove_file(&f1).unwrap();
    fs::remove_dir(&sub1).unwrap();
    assert!(!sub1.exists());

    drop(guard);
    dm.flush().unwrap();
    let report = dm.verify_integrity().unwrap();
    assert!(
        report.is_clean,
        "OIFS image must pass fsck after live NFSv3 unmount: {report:?}"
    );
}

#[test]
#[ignore = "Requires live OS NFS mount privileges (run with -- --ignored --nocapture)"]
fn test_live_nfs_read_only_mount_immutability() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("live_nfs_ro.img");
    let mp = dir.path().join("mnt_ro");
    fs::create_dir_all(&mp).unwrap();

    {
        let dm = DiskManager::open_journaled(&img, 8 * 1024 * 1024).unwrap();
        dm.create_file_with_data(0, "readme.txt", b"read-only via NFSv3", CompressionMode::Auto)
            .unwrap();
        dm.flush().unwrap();
    }
    let before = fs::read(&img).unwrap();

    let ro_dm = Arc::new(DiskManager::open_read_only(&img).unwrap());
    let guard = LiveNfsMountGuard::mount(Arc::clone(&ro_dm), &mp, true);

    assert_eq!(
        fs::read(mp.join("readme.txt")).unwrap(),
        b"read-only via NFSv3"
    );
    assert!(fs::write(mp.join("forbidden.txt"), b"nope").is_err());

    drop(guard);
    drop(ro_dm);

    let after = fs::read(&img).unwrap();
    assert_eq!(before, after, "Read-only live NFS mount must not mutate image");
}
