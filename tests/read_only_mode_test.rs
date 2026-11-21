use oifs::auto_defrag::AutoDefragConfig;
use oifs::disk::{
    CompressionMode, DefragMode, DiskManager, DiskManagerError, OpenMode, ReadRequest,
};
use oifs::filters::{FilterConfig, FilterType};
use oifs::storage::StorageMode;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempImg {
    path: PathBuf,
}

impl TempImg {
    fn new(tag: &str) -> Self {
        let id = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "oifs_ro_test_{}_{}_{}.img",
            tag,
            std::process::id(),
            id
        ));
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn set_mode(&self, mode: u32) {
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(mode))
            .expect("set_permissions");
    }
}

impl Drop for TempImg {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::remove_file(&self.path);
    }
}

struct FixturePayloads {
    raw: Vec<u8>,
    stream: Vec<u8>,
    seekable: Vec<u8>,
    filtered: Vec<u8>,
    small: Vec<u8>,
    nested: Vec<u8>,
}

fn build_fixture_payloads() -> FixturePayloads {
    let raw: Vec<u8> = (0usize..(128 * 1024))
        .map(|i| ((i.wrapping_mul(31) ^ (i >> 5)) & 0xFF) as u8)
        .collect();
    let stream: Vec<u8> = b"OIFS read-only zstd stream payload! "
        .iter()
        .copied()
        .cycle()
        .take(64 * 1024)
        .collect();
    let mut seekable = Vec::with_capacity(192 * 1024);
    for chunk in 0..3u8 {
        for i in 0..(64 * 1024) {
            seekable.push(chunk.wrapping_mul(53).wrapping_add((i % 251) as u8));
        }
    }
    let mut filtered = Vec::with_capacity(32 * 1024);
    for i in 0u64..(32 * 1024 / 8) {
        let val = 1_000_000i64 + (i as i64) * 17;
        filtered.extend_from_slice(&val.to_le_bytes());
    }
    let small = b"small read-only hello world".to_vec();
    let nested = b"nested directory file contents 1234567890".to_vec();
    FixturePayloads {
        raw,
        stream,
        seekable,
        filtered,
        small,
        nested,
    }
}

fn populate_fixture_image(path: &Path, password: Option<&str>, journaled: bool) -> FixturePayloads {
    let payloads = build_fixture_payloads();
    let dm = DiskManager::open_with_options(
        path,
        16 * 1024 * 1024,
        password,
        journaled,
        StorageMode::Mmap,
    )
    .expect("create fixture image");

    let root = dm.superblock().root_inode;
    dm.create_file_with_data(root, "raw.bin", &payloads.raw, CompressionMode::Never)
        .expect("create raw.bin");
    dm.create_file_with_data(
        root,
        "stream.zst",
        &payloads.stream,
        CompressionMode::Always,
    )
    .expect("create stream.zst");
    dm.create_file_with_data(
        root,
        "seekable.bin",
        &payloads.seekable,
        CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        },
    )
    .expect("create seekable.bin");
    let mut filter_cfg = FilterConfig::none();
    filter_cfg.typesize = 8;
    filter_cfg.delta = true;
    filter_cfg.shuffle = true;
    assert!(filter_cfg.is_active());
    let _ = FilterType::Delta;
    dm.create_file_with_data_with_filters(
        root,
        "filtered.i64",
        &payloads.filtered,
        CompressionMode::Always,
        filter_cfg,
    )
    .expect("create filtered.i64");
    dm.create_file_with_data(root, "small.txt", &payloads.small, CompressionMode::Never)
        .expect("create small.txt");

    let sub_id = dm.create_directory(root, "sub").expect("create sub dir");
    dm.create_file_with_data(
        sub_id,
        "nested.txt",
        &payloads.nested,
        CompressionMode::Never,
    )
    .expect("create nested.txt");

    // Flush cleanly (and checkpoint WAL if journaled) before closing.
    dm.flush().expect("flush fixture image");
    drop(dm);
    payloads
}

fn verify_full_read_matrix(
    dm: &DiskManager,
    payloads: &FixturePayloads,
    expected_mode: StorageMode,
    encrypted: bool,
    journaled: bool,
) {
    assert!(dm.is_read_only());
    assert_eq!(dm.storage_mode(), expected_mode);
    assert_eq!(dm.is_encrypted(), encrypted);

    let root = dm.superblock().root_inode;
    let entries = dm.list_dir(root).expect("list_dir root");
    let mut names: Vec<String> = entries.into_iter().map(|e| e.name).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "filtered.i64",
            "raw.bin",
            "seekable.bin",
            "small.txt",
            "stream.zst",
            "sub",
        ]
    );

    let raw_id = dm.lookup(root, "raw.bin").expect("lookup raw.bin");
    let stream_id = dm.resolve_path("/stream.zst").expect("resolve /stream.zst");
    let seekable_id = dm
        .resolve_path("/seekable.bin")
        .expect("resolve /seekable.bin");
    let filtered_id = dm
        .resolve_path("/filtered.i64")
        .expect("resolve /filtered.i64");
    let small_id = dm.resolve_path("/small.txt").expect("resolve /small.txt");
    let nested_id = dm
        .resolve_path("/sub/nested.txt")
        .expect("resolve /sub/nested.txt");

    // 1. Whole-file `read_data`
    assert_eq!(dm.read_data(raw_id).expect("read_data raw"), payloads.raw);
    assert_eq!(
        dm.read_data(stream_id).expect("read_data stream"),
        payloads.stream
    );
    assert_eq!(
        dm.read_data(seekable_id).expect("read_data seekable"),
        payloads.seekable
    );
    assert_eq!(
        dm.read_data(filtered_id).expect("read_data filtered"),
        payloads.filtered
    );
    assert_eq!(
        dm.read_data(small_id).expect("read_data small"),
        payloads.small
    );
    assert_eq!(
        dm.read_data(nested_id).expect("read_data nested"),
        payloads.nested
    );

    // 2. Positional `read_at` across block and 64K chunk boundaries
    let mut buf = vec![0u8; 5000];
    let n = dm.read_at(raw_id, 4000, &mut buf).expect("read_at raw");
    assert_eq!(n, 5000);
    assert_eq!(&buf[..], &payloads.raw[4000..9000]);

    let mut sbuf = vec![0u8; 8192];
    let n = dm
        .read_at(seekable_id, 65000, &mut sbuf)
        .expect("read_at seekable cross-chunk");
    assert_eq!(n, 8192);
    assert_eq!(&sbuf[..], &payloads.seekable[65000..65000 + 8192]);

    let mut fbuf = vec![0u8; 1024];
    let n = dm
        .read_at(filtered_id, 512, &mut fbuf)
        .expect("read_at filtered");
    assert_eq!(n, 1024);
    assert_eq!(&fbuf[..], &payloads.filtered[512..1536]);

    // 3. `read_at_batch`
    let mut b_raw = vec![0u8; 256];
    let mut b_stream = vec![0u8; 256];
    let mut b_seek = vec![0u8; 256];
    let mut b_nested = vec![0u8; payloads.nested.len()];
    let mut reqs = [
        ReadRequest {
            inode_id: raw_id,
            offset: 1234,
            buf: &mut b_raw,
        },
        ReadRequest {
            inode_id: stream_id,
            offset: 100,
            buf: &mut b_stream,
        },
        ReadRequest {
            inode_id: seekable_id,
            offset: 70000,
            buf: &mut b_seek,
        },
        ReadRequest {
            inode_id: nested_id,
            offset: 0,
            buf: &mut b_nested,
        },
    ];
    let batch_res = dm.read_at_batch(&mut reqs);
    assert_eq!(batch_res.len(), 4);
    assert_eq!(batch_res[0].as_ref().unwrap(), &256);
    assert_eq!(batch_res[1].as_ref().unwrap(), &256);
    assert_eq!(batch_res[2].as_ref().unwrap(), &256);
    assert_eq!(batch_res[3].as_ref().unwrap(), &payloads.nested.len());
    assert_eq!(&b_raw[..], &payloads.raw[1234..1234 + 256]);
    assert_eq!(&b_stream[..], &payloads.stream[100..356]);
    assert_eq!(&b_seek[..], &payloads.seekable[70000..70256]);
    assert_eq!(&b_nested[..], &payloads.nested[..]);

    // 4. Async reads (`read_data_async`, `read_at_async`, `read_at_batch_async`)
    let async_raw_task = dm.read_at_async(raw_id, 500, 2048);
    if expected_mode == StorageMode::Mmap && !encrypted {
        assert!(
            async_raw_task.is_ready(),
            "read_at_async on read-only Mmap raw file should complete inline"
        );
    }
    assert_eq!(
        async_raw_task.wait().expect("read_at_async raw"),
        &payloads.raw[500..2548]
    );
    assert_eq!(
        dm.read_data_async(stream_id)
            .wait()
            .expect("read_data_async stream"),
        payloads.stream
    );
    assert_eq!(
        dm.read_data_async(seekable_id)
            .wait()
            .expect("read_data_async seekable"),
        payloads.seekable
    );
    let batch_async = dm
        .read_at_batch_async(vec![(raw_id, 0, 128), (small_id, 0, payloads.small.len())])
        .wait();
    assert_eq!(batch_async[0].as_ref().unwrap(), &payloads.raw[..128]);
    assert_eq!(batch_async[1].as_ref().unwrap(), &payloads.small[..]);

    // 5. `begin_read` -> `FileReadHandle::read_at` and true zero-copy `mmap_slice`
    {
        let session = dm.begin_read();
        let raw_handle = session.open_file(raw_id).expect("open_file raw");
        let expected_raw_inode_size = if encrypted {
            payloads.raw.len() + 16
        } else {
            payloads.raw.len()
        };
        assert_eq!(raw_handle.size() as usize, expected_raw_inode_size);
        let mut hbuf = vec![0u8; 4096];
        let n = raw_handle.read_at(8192, &mut hbuf).expect("handle read_at");
        assert_eq!(n, 4096);
        assert_eq!(&hbuf[..], &payloads.raw[8192..8192 + 4096]);

        let zc_direct = raw_handle
            .mmap_slice(0, 40 * 1024)
            .expect("mmap_slice raw direct blocks");
        let small_handle = session.open_file(small_id).expect("open_file small");
        let zc_small = small_handle
            .mmap_slice(0, payloads.small.len())
            .expect("mmap_slice small");
        if expected_mode == StorageMode::Mmap && !encrypted {
            let direct_slice = zc_direct
                .expect("expected 0-copy mmap_slice on PROT_READ MmapReadOnly direct blocks");
            assert_eq!(direct_slice, &payloads.raw[..40 * 1024]);
            let small_slice =
                zc_small.expect("expected 0-copy mmap_slice on PROT_READ MmapReadOnly small file");
            assert_eq!(small_slice, &payloads.small[..]);
            if !journaled {
                // Non-journaled fresh multi-block writes place indirect metadata after payload,
                // so the entire 128 KiB payload is physically contiguous for a single 0-copy slice.
                let full_slice = raw_handle
                    .mmap_slice(0, payloads.raw.len())
                    .expect("mmap_slice full raw")
                    .expect("expected full 128 KiB 0-copy mmap_slice on MmapReadOnly");
                assert_eq!(full_slice, &payloads.raw[..]);
            }
        } else {
            assert!(
                zc_direct.is_none(),
                "expected mmap_slice None on FileIo or encrypted file"
            );
            assert!(
                zc_small.is_none(),
                "expected small mmap_slice None on FileIo or encrypted file"
            );
        }

        // Compressed files never return a raw mmap_slice even under MmapReadOnly.
        let stream_handle = session.open_file(stream_id).expect("open_file stream");
        assert!(
            stream_handle
                .mmap_slice(0, 1024)
                .expect("mmap_slice stream")
                .is_none()
        );
        let seek_handle = session.open_file(seekable_id).expect("open_file seekable");
        assert!(
            seek_handle
                .mmap_slice(0, 1024)
                .expect("mmap_slice seekable")
                .is_none()
        );
    }

    // 6. Read-only diagnostics (`analyze_fragmentation`, `verify_integrity`)
    let frag = dm
        .analyze_fragmentation()
        .expect("analyze_fragmentation on read-only");
    assert!(frag.used_blocks > 0);
    assert!(frag.total_blocks >= frag.used_blocks);

    let report = dm
        .verify_integrity()
        .expect("verify_integrity (fsck) on read-only");
    assert!(report.is_clean, "fsck report not clean: {:?}", report);
}

#[test]
fn test_read_only_full_matrix_on_0444_images() {
    let cases: [(&str, Option<&str>, bool, StorageMode); 6] = [
        ("plain_mmap", None, false, StorageMode::Mmap),
        ("plain_fileio", None, false, StorageMode::FileIo),
        ("journaled_mmap", None, true, StorageMode::Mmap),
        ("journaled_fileio", None, true, StorageMode::FileIo),
        (
            "enc_journaled_mmap",
            Some("ro-secret-pass!"),
            true,
            StorageMode::Mmap,
        ),
        (
            "enc_journaled_fileio",
            Some("ro-secret-pass!"),
            true,
            StorageMode::FileIo,
        ),
    ];

    for (label, password, journaled, mode) in cases {
        let img = TempImg::new(label);
        let payloads = populate_fixture_image(img.path(), password, journaled);
        img.set_mode(0o444);

        let dm = DiskManager::open_read_only_with_storage_mode(img.path(), password, mode)
            .unwrap_or_else(|e| panic!("open_read_only failed for {label}: {e}"));
        verify_full_read_matrix(&dm, &payloads, mode, password.is_some(), journaled);

        // Also test convenience constructors on plain and encrypted `0444` images.
        if mode == StorageMode::Mmap {
            if password.is_none() {
                let dm2 = DiskManager::open_read_only(img.path())
                    .expect("open_read_only convenience on 0444");
                assert!(dm2.is_read_only());
                let raw_id = dm2.resolve_path("/raw.bin").unwrap();
                assert_eq!(dm2.read_data(raw_id).unwrap(), payloads.raw);
                // Providing a password on an unencrypted image must fail with NotEncrypted.
                assert!(matches!(
                    DiskManager::open_read_only_with_password(img.path(), Some("unexpected")),
                    Err(DiskManagerError::NotEncrypted)
                ));
            } else {
                let dm2 = DiskManager::open_read_only_with_password(img.path(), password)
                    .expect("open_read_only_with_password on 0444");
                assert!(dm2.is_read_only());
                let raw_id = dm2.resolve_path("/raw.bin").unwrap();
                assert_eq!(dm2.read_data(raw_id).unwrap(), payloads.raw);
                // Omitting password on an encrypted image must fail with PasswordRequired.
                assert!(matches!(
                    DiskManager::open_read_only(img.path()),
                    Err(DiskManagerError::PasswordRequired)
                ));
            }
        }
    }
}

#[test]
fn test_exhaustive_mutation_gate_and_byte_for_byte_immutability() {
    let cases = [
        ("mut_plain_mmap_0600", false, StorageMode::Mmap, 0o600),
        ("mut_plain_fileio_0600", false, StorageMode::FileIo, 0o600),
        ("mut_journal_mmap_0600", true, StorageMode::Mmap, 0o600),
        ("mut_journal_fileio_0600", true, StorageMode::FileIo, 0o600),
        ("mut_journal_mmap_0444", true, StorageMode::Mmap, 0o444),
        ("mut_journal_fileio_0444", true, StorageMode::FileIo, 0o444),
    ];

    for (label, journaled, mode, perms) in cases {
        let img = TempImg::new(label);
        let payloads = populate_fixture_image(img.path(), None, journaled);
        let before_bytes = std::fs::read(img.path()).expect("read image bytes before");
        img.set_mode(perms);

        {
            let dm = DiskManager::open_read_only_with_storage_mode(img.path(), None, mode)
                .unwrap_or_else(|e| panic!("open_read_only {label}: {e}"));
            assert!(dm.is_read_only());

            let root = dm.superblock().root_inode;
            let raw_id = dm.lookup(root, "raw.bin").expect("lookup raw.bin");
            let seek_id = dm
                .lookup(root, "seekable.bin")
                .expect("lookup seekable.bin");
            let raw_inode = dm.read_inode(raw_id).expect("read_inode raw.bin");

            // Read-only opens on existing entries succeed.
            assert_eq!(
                dm.open_file(root, "raw.bin", OpenMode::OpenExisting)
                    .expect("OpenExisting"),
                raw_id
            );
            assert_eq!(
                dm.open_file(root, "raw.bin", OpenMode::CreateOrOpen)
                    .expect("CreateOrOpen existing"),
                raw_id
            );
            assert_eq!(
                dm.create_or_open_file(root, "raw.bin")
                    .expect("create_or_open_file existing"),
                raw_id
            );

            // Every mutating API MUST return Err(DiskManagerError::ReadOnly).
            assert!(matches!(
                dm.create_file(root, "new_file.txt"),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.create_file_with_data(root, "new_data.txt", b"abc", CompressionMode::Never),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.create_file_with_data_with_filters(
                    root,
                    "new_filt.txt",
                    b"abcdefgh",
                    CompressionMode::Always,
                    FilterConfig::none(),
                ),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.create_file_with_data_async(
                    root,
                    "new_async.txt".to_string(),
                    b"async".to_vec(),
                    CompressionMode::Never,
                )
                .wait(),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.create_or_open_file(root, "missing_entry.txt"),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.open_file(root, "raw.bin", OpenMode::CreateNew),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.open_file(root, "missing_create_or_open.txt", OpenMode::CreateOrOpen),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.create_directory(root, "new_dir"),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.delete_file(root, "raw.bin"),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.write_data(raw_id, 0, b"overwrite", CompressionMode::Never),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.write_data_with_filters(
                    raw_id,
                    0,
                    b"overwrite",
                    CompressionMode::Never,
                    FilterConfig::none(),
                ),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.write_data_truncated(raw_id, b"short", CompressionMode::Never),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.write_data_async(raw_id, 0, b"async".to_vec(), CompressionMode::Never)
                    .wait(),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.write_chunked_64k(seek_id, 0, b"chunked", 1),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.write_inode(raw_id, &raw_inode),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.truncate(raw_id, 10),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.truncate_path("/raw.bin", 10),
                Err(DiskManagerError::ReadOnly)
            ));
            let path_str = img.path().to_str().unwrap();
            assert!(matches!(
                dm.defragment(path_str, DefragMode::Safe, None),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.defragment(path_str, DefragMode::InPlace, None),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(dm.migrate(), Err(DiskManagerError::ReadOnly)));
            assert!(matches!(
                dm.checkpoint_journal_now(),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.allocate_contiguous_blocks(2),
                Err(DiskManagerError::ReadOnly)
            ));
            assert!(matches!(
                dm.free_contiguous_blocks(dm.superblock().data_block_start, 1),
                Err(DiskManagerError::ReadOnly)
            ));

            // Enabling background auto-defrag must be suppressed on read-only mounts.
            dm.set_auto_defrag(
                AutoDefragConfig::new(0.0).with_check_interval(Duration::from_millis(10)),
            );
            assert!(!dm.auto_defrag_config().enabled);
            assert_eq!(dm.auto_defrag_stats().passes, 0);

            // Flush helpers are safe no-ops on read-only mounts (zero disk writes).
            dm.flush().expect("flush no-op");
            dm.flush_async().expect("flush_async no-op");
            dm.flush_exclusive_legacy()
                .expect("flush_exclusive_legacy no-op");
            dm.flush_async_task()
                .wait()
                .expect("flush_async_task no-op");

            // Data still reads back intact.
            assert_eq!(dm.read_data(raw_id).unwrap(), payloads.raw);
        }

        // After dropping `dm`, the image on disk must be 100% byte-for-byte identical.
        let after_bytes = std::fs::read(img.path()).expect("read image bytes after");
        assert_eq!(
            before_bytes, after_bytes,
            "Image bytes mutated on read-only mount for case {label}"
        );
    }
}

#[test]
fn test_dirty_unrecovered_wal_refused_until_rw_recovery() {
    for (label, mode) in [
        ("dirty_wal_mmap", StorageMode::Mmap),
        ("dirty_wal_fileio", StorageMode::FileIo),
    ] {
        let img = TempImg::new(label);
        let payload = b"uncheckpointed journaled payload data 42";
        {
            let dm = DiskManager::open_with_options(
                img.path(),
                4 * 1024 * 1024,
                None,
                true,
                StorageMode::Mmap,
            )
            .expect("create journaled image");
            let root = dm.superblock().root_inode;
            // Write under default Lazy mode without calling `flush()` / `checkpoint_journal_now()`,
            // leaving committed transactions pending in the WAL ring.
            dm.create_file_with_data(root, "pending.bin", payload, CompressionMode::Never)
                .expect("create pending.bin");
            // Drop without flushing/checkpointing the WAL.
        }

        let before_bytes = std::fs::read(img.path()).expect("read dirty image bytes");

        // Read-only mount must reject the image without modifying a single byte.
        let ro_err = DiskManager::open_read_only_with_storage_mode(img.path(), None, mode)
            .err()
            .expect("expected read-only mount to refuse dirty WAL");
        match ro_err {
            DiskManagerError::Io(e) => {
                assert!(
                    e.to_string().contains("unrecovered transactions"),
                    "unexpected error message: {e}"
                );
            }
            other => panic!("expected DiskManagerError::Io for dirty WAL, got {other:?}"),
        }

        let after_failed_ro = std::fs::read(img.path()).expect("read image bytes after failed RO");
        assert_eq!(
            before_bytes, after_failed_ro,
            "failed read-only mount must not modify dirty WAL image"
        );

        // Open read-write once to recover and mark journal clean, then close.
        {
            let dm_rw = DiskManager::open_with_options(
                img.path(),
                4 * 1024 * 1024,
                None,
                true,
                StorageMode::Mmap,
            )
            .expect("open RW to recover WAL");
            let id = dm_rw
                .resolve_path("/pending.bin")
                .expect("lookup recovered");
            assert_eq!(dm_rw.read_data(id).expect("read recovered"), payload);
        }

        // Now read-only mount succeeds (even after `chmod 0444`)!
        img.set_mode(0o444);
        let dm_ro = DiskManager::open_read_only_with_storage_mode(img.path(), None, mode)
            .expect("open read-only after RW recovery");
        assert!(dm_ro.is_read_only());
        let id = dm_ro.resolve_path("/pending.bin").expect("lookup RO");
        assert_eq!(dm_ro.read_data(id).expect("read RO"), payload);
    }
}

fn spawn_lock_child(
    action: &str,
    img_path: &Path,
    mode: &str,
) -> (Child, BufReader<std::process::ChildStdout>) {
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(exe)
        .arg("--exact")
        .arg("child_lock_helper")
        .arg("--nocapture")
        .env("OIFS_RO_CHILD_ACTION", action)
        .env("OIFS_RO_CHILD_IMG", img_path)
        .env("OIFS_RO_CHILD_MODE", mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn child process");
    let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
    (child, stdout)
}

fn read_child_marker(reader: &mut BufReader<std::process::ChildStdout>, expected: &str) {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).expect("read child stdout");
        assert!(n > 0, "child exited before emitting marker `{expected}`");
        if line.trim() == expected {
            return;
        }
    }
}

fn release_child(mut child: Child) {
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "EXIT");
        let _ = stdin.flush();
    }
    let status = child.wait().expect("wait child");
    assert!(status.success(), "child process failed: {status:?}");
}

#[test]
fn child_lock_helper() {
    let Ok(action) = std::env::var("OIFS_RO_CHILD_ACTION") else {
        return;
    };
    let img_path = PathBuf::from(std::env::var("OIFS_RO_CHILD_IMG").expect("OIFS_RO_CHILD_IMG"));
    let mode = match std::env::var("OIFS_RO_CHILD_MODE")
        .unwrap_or_else(|_| "mmap".to_string())
        .as_str()
    {
        "fileio" => StorageMode::FileIo,
        _ => StorageMode::Mmap,
    };

    match action.as_str() {
        "hold_ro" => {
            let dm = DiskManager::open_read_only_with_storage_mode(&img_path, None, mode)
                .expect("child open_read_only");
            let id = dm.resolve_path("/small.txt").expect("child lookup");
            let data = dm.read_data(id).expect("child read_data");
            assert_eq!(data, b"small read-only hello world");
            println!("LOCKED");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            drop(dm);
        }
        "hold_rw" => {
            let dm = DiskManager::open_with_storage_mode(&img_path, 4 * 1024 * 1024, mode)
                .expect("child open RW");
            println!("LOCKED");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            drop(dm);
        }
        "try_rw_expect_locking" => {
            let res = DiskManager::open_with_storage_mode(&img_path, 4 * 1024 * 1024, mode);
            assert!(
                matches!(res, Err(DiskManagerError::Locking(_))),
                "expected DiskManagerError::Locking when acquiring F_WRLCK against F_RDLCK, got {:?}",
                res.err()
            );
            println!("LOCKING_ERR");
            let _ = std::io::stdout().flush();
        }
        "try_ro_expect_locking" => {
            let res = DiskManager::open_read_only_with_storage_mode(&img_path, None, mode);
            assert!(
                matches!(res, Err(DiskManagerError::Locking(_))),
                "expected DiskManagerError::Locking when acquiring F_RDLCK against F_WRLCK, got {:?}",
                res.err()
            );
            println!("LOCKING_ERR");
            let _ = std::io::stdout().flush();
        }
        other => panic!("unknown OIFS_RO_CHILD_ACTION: {other}"),
    }
}

#[test]
fn test_multiprocess_shared_rdlck_and_exclusive_wrlck_matrix() {
    let img = TempImg::new("multiproc_lock");
    let payloads = populate_fixture_image(img.path(), None, false);

    // Phase 1: Parent holds F_RDLCK while TWO independent child processes also hold F_RDLCK
    // simultaneously (one Mmap, one FileIo).
    {
        let parent_ro =
            DiskManager::open_read_only_with_storage_mode(img.path(), None, StorageMode::Mmap)
                .expect("parent open_read_only");
        let small_id = parent_ro.resolve_path("/small.txt").unwrap();
        assert_eq!(parent_ro.read_data(small_id).unwrap(), payloads.small);

        let (child1, mut out1) = spawn_lock_child("hold_ro", img.path(), "mmap");
        read_child_marker(&mut out1, "LOCKED");

        let (child2, mut out2) = spawn_lock_child("hold_ro", img.path(), "fileio");
        read_child_marker(&mut out2, "LOCKED");

        // While readers hold F_RDLCK, a writer process attempting F_WRLCK must fail with Locking.
        let (mut writer_probe, mut wout) =
            spawn_lock_child("try_rw_expect_locking", img.path(), "mmap");
        read_child_marker(&mut wout, "LOCKING_ERR");
        assert!(writer_probe.wait().unwrap().success());

        // Release both reader children and the parent reader.
        release_child(child1);
        release_child(child2);
        drop(parent_ro);
    }

    // Phase 2: A child writer acquires F_WRLCK; both parent and another child attempting
    // F_RDLCK must fail with DiskManagerError::Locking.
    let (writer_child, mut wout) = spawn_lock_child("hold_rw", img.path(), "mmap");
    read_child_marker(&mut wout, "LOCKED");

    assert!(
        matches!(
            DiskManager::open_read_only_with_storage_mode(img.path(), None, StorageMode::Mmap),
            Err(DiskManagerError::Locking(_))
        ),
        "parent F_RDLCK must fail while child holds F_WRLCK"
    );
    assert!(
        matches!(
            DiskManager::open_read_only_with_storage_mode(img.path(), None, StorageMode::FileIo),
            Err(DiskManagerError::Locking(_))
        ),
        "parent FileIo F_RDLCK must fail while child holds F_WRLCK"
    );

    let (mut ro_probe, mut ro_out) = spawn_lock_child("try_ro_expect_locking", img.path(), "mmap");
    read_child_marker(&mut ro_out, "LOCKING_ERR");
    assert!(ro_probe.wait().unwrap().success());

    // Phase 3: Once writer releases F_WRLCK, readers can immediately acquire F_RDLCK again.
    release_child(writer_child);

    let parent_ro2 =
        DiskManager::open_read_only_with_storage_mode(img.path(), None, StorageMode::FileIo)
            .expect("parent F_RDLCK after writer exits");
    let (child3, mut out3) = spawn_lock_child("hold_ro", img.path(), "mmap");
    read_child_marker(&mut out3, "LOCKED");

    let small_id = parent_ro2.resolve_path("/small.txt").unwrap();
    assert_eq!(parent_ro2.read_data(small_id).unwrap(), payloads.small);

    release_child(child3);
    drop(parent_ro2);
}
