use oifs::disk::CompressionMode;
use oifs::filters::FilterConfig;
use oifs::{BLOCK_SIZE, DiskManager, DurabilityMode, StorageMode};
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;

fn open_plain(path: &Path, size: u64, journal: bool, mode: StorageMode) -> DiskManager {
    match (journal, mode) {
        (false, m) => DiskManager::open_with_storage_mode(path, size, m).unwrap(),
        (true, StorageMode::Mmap) => DiskManager::open_journaled(path, size).unwrap(),
        (true, StorageMode::FileIo) => DiskManager::open_file_io_journaled(path, size).unwrap(),
    }
}

#[test]
fn test_create_file_with_data_matrix() {
    let dir = tempdir().unwrap();

    for &mode in &[StorageMode::Mmap, StorageMode::FileIo] {
        for &journal in &[false, true] {
            for &durability in &[DurabilityMode::Lazy, DurabilityMode::Strict] {
                let path = dir.path().join(format!(
                    "create_with_data_{mode:?}_{journal}_{durability:?}.img"
                ));
                let dm = open_plain(&path, 16 * 1024 * 1024, journal, mode);
                dm.set_durability_mode(durability);
                let root = dm.superblock().root_inode;

                // Test empty, sub-block, exact 4 KiB, 4 KiB + 1, exact 8 KiB, and multi-block (128 KiB).
                let sizes = [
                    0usize,
                    137,
                    BLOCK_SIZE,
                    BLOCK_SIZE + 17,
                    2 * BLOCK_SIZE,
                    128 * 1024,
                ];
                for (i, &sz) in sizes.iter().enumerate() {
                    let payload: Vec<u8> =
                        (0..sz).map(|b| ((b * 31 + i * 17) & 0xFF) as u8).collect();
                    let name = format!("raw_{i}.bin");
                    let id = dm
                        .create_file_with_data(root, &name, &payload, CompressionMode::Never)
                        .unwrap();
                    assert_eq!(dm.lookup(root, &name).unwrap(), id);
                    assert_eq!(dm.read_data(id).unwrap(), payload);
                }

                // Duplicate filename must fail with AlreadyExists without overwriting existing content.
                let err = dm
                    .create_file_with_data(root, "raw_2.bin", b"clobber", CompressionMode::Never)
                    .unwrap_err();
                assert!(
                    matches!(err, oifs::DiskManagerError::Io(ref e) if e.kind() == std::io::ErrorKind::AlreadyExists),
                    "expected AlreadyExists, got {err:?}"
                );
                let raw2_id = dm.lookup(root, "raw_2.bin").unwrap();
                assert_eq!(dm.read_data(raw2_id).unwrap().len(), BLOCK_SIZE);

                // Compressed, filtered, and seekable modes.
                let compressible: Vec<u8> = (0..32 * 1024).map(|b| (b % 13) as u8).collect();
                let zstd_id = dm
                    .create_file_with_data(root, "zstd.bin", &compressible, CompressionMode::Always)
                    .unwrap();
                assert_eq!(dm.read_data(zstd_id).unwrap(), compressible);

                let auto_id = dm
                    .create_file_with_data(root, "auto.bin", &compressible, CompressionMode::Auto)
                    .unwrap();
                assert_eq!(dm.read_data(auto_id).unwrap(), compressible);

                let filtered: Vec<u8> = (0..4096u32)
                    .flat_map(|v| (v * 1000).to_le_bytes())
                    .collect();
                let filt_cfg = FilterConfig {
                    typesize: 4,
                    delta: true,
                    shuffle: true,
                    bitshuffle: false,
                };
                let filt_id = dm
                    .create_file_with_data_with_filters(
                        root,
                        "filtered.bin",
                        &filtered,
                        CompressionMode::Always,
                        filt_cfg,
                    )
                    .unwrap();
                assert_eq!(dm.read_data(filt_id).unwrap(), filtered);

                let seek_id = dm
                    .create_file_with_data(
                        root,
                        "seekable.bin",
                        &compressible,
                        CompressionMode::Seekable {
                            level: 1,
                            chunk_size: 65536,
                        },
                    )
                    .unwrap();
                assert_eq!(dm.read_data(seek_id).unwrap(), compressible);

                dm.flush().unwrap();
                let report = dm.verify_integrity().unwrap();
                assert!(
                    report.is_clean,
                    "fsck failed for {mode:?} journal={journal} durability={durability:?}: {report:?}"
                );
                drop(dm);

                // Reopen and verify full persistence.
                let reopened = open_plain(&path, 0, journal, mode);
                for (i, &sz) in sizes.iter().enumerate() {
                    let expected: Vec<u8> =
                        (0..sz).map(|b| ((b * 31 + i * 17) & 0xFF) as u8).collect();
                    let id = reopened.lookup(root, &format!("raw_{i}.bin")).unwrap();
                    assert_eq!(reopened.read_data(id).unwrap(), expected);
                }
                assert_eq!(
                    reopened
                        .read_data(reopened.lookup(root, "filtered.bin").unwrap())
                        .unwrap(),
                    filtered
                );
                assert_eq!(
                    reopened
                        .read_data(reopened.lookup(root, "seekable.bin").unwrap())
                        .unwrap(),
                    compressible
                );
            }
        }
    }
}

#[test]
fn test_create_file_with_data_encrypted_and_strict_wal() {
    let dir = tempdir().unwrap();

    for &mode in &[StorageMode::Mmap, StorageMode::FileIo] {
        for &journal in &[false, true] {
            let path = dir.path().join(format!("enc_{mode:?}_{journal}.img"));
            // Create the encrypted container first, then reopen in the target StorageMode.
            {
                let init = if journal {
                    DiskManager::create_encrypted_journaled(&path, 10 * 1024 * 1024, "secret-pw")
                        .unwrap()
                } else {
                    DiskManager::create_encrypted(&path, 10 * 1024 * 1024, "secret-pw").unwrap()
                };
                init.flush().unwrap();
            }
            let dm = match mode {
                StorageMode::Mmap => {
                    DiskManager::open_with_password(&path, 0, Some("secret-pw")).unwrap()
                }
                StorageMode::FileIo => {
                    DiskManager::open_file_io_with_password(&path, 0, Some("secret-pw")).unwrap()
                }
            };
            dm.set_durability_mode(DurabilityMode::Strict);
            let root = dm.superblock().root_inode;

            // Empty encrypted file, exact 4 KiB (4,112 B encrypted = 2 WAL-inlined blocks), and 32 KiB.
            let cases: &[(&str, Vec<u8>)] = &[
                ("empty.enc", vec![]),
                ("small.enc", vec![0xA5; 512]),
                (
                    "fourk.enc",
                    (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect(),
                ),
                (
                    "large.enc",
                    (0..32 * 1024).map(|i| ((i * 7) % 251) as u8).collect(),
                ),
            ];
            for (name, data) in cases {
                let id = dm
                    .create_file_with_data(root, name, data, CompressionMode::Never)
                    .unwrap();
                assert_eq!(dm.read_data(id).unwrap(), *data);
            }
            dm.flush().unwrap();
            assert!(dm.verify_integrity().unwrap().is_clean);
            drop(dm);

            let reopened = match mode {
                StorageMode::Mmap => {
                    DiskManager::open_with_password(&path, 0, Some("secret-pw")).unwrap()
                }
                StorageMode::FileIo => {
                    DiskManager::open_file_io_with_password(&path, 0, Some("secret-pw")).unwrap()
                }
            };
            for (name, data) in cases {
                let id = reopened.lookup(root, name).unwrap();
                assert_eq!(reopened.read_data(id).unwrap(), *data);
            }
        }
    }
}

#[test]
fn test_create_file_with_data_no_space_leaves_no_orphan() {
    let dir = tempdir().unwrap();

    for &mode in &[StorageMode::Mmap, StorageMode::FileIo] {
        for &journal in &[false, true] {
            let path = dir.path().join(format!("nospace_{mode:?}_{journal}.img"));
            let dm = open_plain(&path, 512 * 1024, journal, mode);
            let root = dm.superblock().root_inode;

            // Attempt to create a file larger than the entire container.
            let huge = vec![0x55u8; 2 * 1024 * 1024];
            let err = dm
                .create_file_with_data(root, "too_big.bin", &huge, CompressionMode::Never)
                .unwrap_err();
            assert!(matches!(err, oifs::DiskManagerError::Allocator(_)));

            // Directory must not contain the failed file, and fsck must report zero leaks.
            assert!(dm.lookup(root, "too_big.bin").is_err());
            assert!(dm.list_dir(root).unwrap().is_empty());
            assert!(dm.verify_integrity().unwrap().is_clean);

            // Subsequent valid file creation must still succeed cleanly.
            let ok_id = dm
                .create_file_with_data(root, "ok.bin", b"hello", CompressionMode::Never)
                .unwrap();
            assert_eq!(dm.read_data(ok_id).unwrap(), b"hello");
            assert!(dm.verify_integrity().unwrap().is_clean);
        }
    }
}

#[test]
fn test_read_session_and_file_read_handle_concurrent() {
    let dir = tempdir().unwrap();

    for &mode in &[StorageMode::Mmap, StorageMode::FileIo] {
        let path = dir.path().join(format!("read_session_{mode:?}.img"));
        let dm = Arc::new(open_plain(&path, 32 * 1024 * 1024, true, mode));
        let root = dm.superblock().root_inode;

        // 4 MiB file exercises direct (0..10), single-indirect (10..522), and double-indirect (522..1024).
        let file_size = 4 * 1024 * 1024;
        let payload: Arc<Vec<u8>> = Arc::new(
            (0usize..file_size)
                .map(|i| ((i.wrapping_mul(131) ^ (i >> 8)) & 0xFF) as u8)
                .collect(),
        );
        let raw_id = dm
            .create_file_with_data(root, "big.bin", &payload, CompressionMode::Never)
            .unwrap();

        // Sparse file with a hole between block 0 and block 20.
        let sparse_id = dm.create_file(root, "sparse.bin").unwrap();
        dm.write_data(sparse_id, 0, b"HEAD", CompressionMode::Never)
            .unwrap();
        dm.write_data(
            sparse_id,
            (20 * BLOCK_SIZE) as u64,
            b"TAIL",
            CompressionMode::Never,
        )
        .unwrap();

        // Compressed file via FileReadHandle.
        let comp_data = vec![0x3Cu8; 32 * 1024];
        let comp_id = dm
            .create_file_with_data(root, "comp.bin", &comp_data, CompressionMode::Always)
            .unwrap();

        // Single-threaded session verification.
        {
            let session = dm.begin_read();
            assert_eq!(session.lookup(root, "big.bin").unwrap(), raw_id);
            assert_eq!(session.list_dir(root).unwrap().len(), 3);
            assert_eq!(session.read_inode(raw_id).unwrap().size, file_size as u64);

            let handle = session.open_file(raw_id).unwrap();
            assert_eq!(handle.inode_id(), raw_id);
            assert_eq!(handle.size(), file_size as u64);

            // Direct, single-indirect, double-indirect, unaligned cross-block, and EOF reads.
            let mut buf = vec![0u8; BLOCK_SIZE];
            for &blk in &[0usize, 9, 10, 521, 522, 1023] {
                let off = (blk * BLOCK_SIZE) as u64;
                assert_eq!(handle.read_at(off, &mut buf).unwrap(), BLOCK_SIZE);
                assert_eq!(buf, payload[blk * BLOCK_SIZE..(blk + 1) * BLOCK_SIZE]);
            }

            let mut cross = vec![0u8; 6000];
            let cross_off = 521 * BLOCK_SIZE + 1234;
            assert_eq!(handle.read_at(cross_off as u64, &mut cross).unwrap(), 6000);
            assert_eq!(cross, payload[cross_off..cross_off + 6000]);

            let mut eof_buf = [0xFFu8; 64];
            assert_eq!(
                handle
                    .read_at((file_size - 10) as u64, &mut eof_buf)
                    .unwrap(),
                10
            );
            assert_eq!(&eof_buf[..10], &payload[file_size - 10..]);
            assert_eq!(handle.read_at(file_size as u64, &mut eof_buf).unwrap(), 0);

            // Sparse hole reads back zeroed.
            let sparse_h = session.open_file(sparse_id).unwrap();
            let mut hole = vec![0xEEu8; BLOCK_SIZE];
            assert_eq!(
                sparse_h
                    .read_at((5 * BLOCK_SIZE) as u64, &mut hole)
                    .unwrap(),
                BLOCK_SIZE
            );
            assert!(hole.iter().all(|&b| b == 0));

            // Compressed file through FileReadHandle.
            let comp_h = session.open_file(comp_id).unwrap();
            assert_eq!(comp_h.read_all().unwrap(), comp_data);
            let mut slice = [0u8; 256];
            assert_eq!(comp_h.read_at(1000, &mut slice).unwrap(), 256);
            assert_eq!(&slice[..], &comp_data[1000..1256]);
        }

        // 4 concurrent threads each holding a ReadSession + FileReadHandle.
        let mut threads = Vec::new();
        for t in 0..4usize {
            let dm = Arc::clone(&dm);
            let payload = Arc::clone(&payload);
            threads.push(std::thread::spawn(move || {
                let session = dm.begin_read();
                let handle = session.open_file(raw_id).unwrap();
                let mut buf = vec![0u8; BLOCK_SIZE];
                let mut rng = 0x1234_5678_9ABC_DEF0u64 ^ (t as u64 + 1);
                let num_blocks = file_size / BLOCK_SIZE;
                for _ in 0..500 {
                    rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let blk = (rng as usize) % num_blocks;
                    let off = (blk * BLOCK_SIZE) as u64;
                    let n = handle.read_at(off, &mut buf).unwrap();
                    assert_eq!(n, BLOCK_SIZE);
                    assert_eq!(buf, payload[blk * BLOCK_SIZE..(blk + 1) * BLOCK_SIZE]);
                }
            }));
        }
        for th in threads {
            th.join().unwrap();
        }
    }
}

#[test]
fn test_file_io_data_page_cache_coherence_and_sizing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data_cache_coherence.img");

    let dm = open_plain(&path, 16 * 1024 * 1024, true, StorageMode::FileIo);
    let root = dm.superblock().root_inode;

    assert!(dm.file_io_cache_bytes() > 0);
    assert_eq!(dm.file_io_cache_used_bytes(), 0);

    // 1. Write a 512 KiB file. Streaming writes should not bloat the cache.
    let v1: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
    let id = dm
        .create_file_with_data(root, "hot.bin", &v1, CompressionMode::Never)
        .unwrap();
    assert_eq!(dm.file_io_cache_used_bytes(), 0);

    // 2. Large (> 64 KiB) streaming read_data bypasses cache (scan resistance).
    assert_eq!(dm.read_data(id).unwrap(), v1);
    assert_eq!(dm.file_io_cache_used_bytes(), 0);

    // 3. Small 4 KiB read_at populates the data page cache.
    let mut blk_buf = [0u8; BLOCK_SIZE];
    for blk in 0..32usize {
        dm.read_at(id, (blk * BLOCK_SIZE) as u64, &mut blk_buf)
            .unwrap();
        assert_eq!(&blk_buf[..], &v1[blk * BLOCK_SIZE..(blk + 1) * BLOCK_SIZE]);
    }
    assert_eq!(dm.file_io_cache_used_bytes(), 32 * BLOCK_SIZE);

    // 4. Overwrite in place: cached pages MUST reflect new bytes (no stale reads!).
    let patch = vec![0x99u8; BLOCK_SIZE * 2];
    dm.write_data(id, (4 * BLOCK_SIZE) as u64, &patch, CompressionMode::Never)
        .unwrap();
    dm.read_at(id, (4 * BLOCK_SIZE) as u64, &mut blk_buf)
        .unwrap();
    assert_eq!(&blk_buf[..], &patch[..BLOCK_SIZE]);
    dm.read_at(id, (5 * BLOCK_SIZE) as u64, &mut blk_buf)
        .unwrap();
    assert_eq!(&blk_buf[..], &patch[BLOCK_SIZE..]);

    // 5. Truncate + delete + reuse blocks: zero stale data leakage.
    dm.truncate(id, (2 * BLOCK_SIZE) as u64).unwrap();
    dm.delete_file(root, "hot.bin").unwrap();
    let fresh_id = dm
        .create_file_with_data(root, "fresh.bin", b"NEW_DATA", CompressionMode::Never)
        .unwrap();
    let mut small = [0u8; 8];
    assert_eq!(dm.read_at(fresh_id, 0, &mut small).unwrap(), 8);
    assert_eq!(&small, b"NEW_DATA");

    // 6. Runtime resize & disable (`0`).
    dm.set_file_io_cache_bytes(256 * 1024);
    assert_eq!(dm.file_io_cache_bytes(), 256 * 1024);
    assert!(dm.file_io_cache_used_bytes() <= 256 * 1024);

    dm.set_file_io_cache_bytes(0);
    assert_eq!(dm.file_io_cache_bytes(), 0);
    assert_eq!(dm.file_io_cache_used_bytes(), 0);

    // Reads continue working transparently with cache disabled.
    assert_eq!(dm.read_data(fresh_id).unwrap(), b"NEW_DATA");
    assert_eq!(dm.file_io_cache_used_bytes(), 0);
}
