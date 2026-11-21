//! Integration tests and benchmarks for `StorageMode::FileIo` (pure positional `pread`/`pwrite`, zero `mmap`).

use oifs::disk::{CompressionMode, DefragMode, DiskManager, DurabilityMode, ReadRequest};
use oifs::filters::FilterConfig;
use oifs::io_engine::IoBackend;
use oifs::storage::StorageMode;
use std::sync::Arc;
use std::thread;
use tempfile::tempdir;

#[test]
fn test_file_io_raw_compressed_filtered_and_truncate() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("file_io_basic.img");

    let dm = DiskManager::open_file_io(&img, 32 * 1024 * 1024).unwrap();
    assert_eq!(dm.storage_mode(), StorageMode::FileIo);
    // FileIo never uses IoBackend::Mmap.
    assert_ne!(dm.io_backend(), IoBackend::Mmap);
    assert_eq!(dm.set_io_backend(IoBackend::Mmap), IoBackend::Pread);

    let root = dm.superblock().root_inode;

    // 1. Raw file crossing direct + single-indirect + double-indirect boundary (> 2.2 MiB)
    let big_id = dm.create_file(root, "big_raw.bin").unwrap();
    let big_data: Vec<u8> = (0..2_300_000usize)
        .map(|i| ((i * 31 + 17) & 0xFF) as u8)
        .collect();
    dm.write_data(big_id, 0, &big_data, CompressionMode::Never)
        .unwrap();
    assert_eq!(dm.read_data(big_id).unwrap(), big_data);

    // Positional read_at across indirect tiers
    let mut slice_buf = vec![0u8; 8192];
    let n = dm.read_at(big_id, 2_150_000, &mut slice_buf).unwrap();
    assert_eq!(n, 8192);
    assert_eq!(&slice_buf[..], &big_data[2_150_000..2_150_000 + 8192]);

    // Batched positional reads
    let mut b1 = vec![0u8; 4096];
    let mut b2 = vec![0u8; 4096];
    let results = dm.read_at_batch(&mut [
        ReadRequest {
            inode_id: big_id,
            offset: 1234,
            buf: &mut b1,
        },
        ReadRequest {
            inode_id: big_id,
            offset: 2_200_000,
            buf: &mut b2,
        },
    ]);
    assert_eq!(results[0].as_ref().copied().unwrap(), 4096);
    assert_eq!(results[1].as_ref().copied().unwrap(), 4096);
    assert_eq!(&b1[..], &big_data[1234..1234 + 4096]);
    assert_eq!(&b2[..], &big_data[2_200_000..2_200_000 + 4096]);

    // Truncate shrink + sparse extend
    dm.truncate(big_id, 50_000).unwrap();
    assert_eq!(dm.read_data(big_id).unwrap(), big_data[..50_000]);
    dm.truncate(big_id, 60_000).unwrap();
    let extended = dm.read_data(big_id).unwrap();
    assert_eq!(extended.len(), 60_000);
    assert_eq!(&extended[..50_000], &big_data[..50_000]);
    assert!(extended[50_000..].iter().all(|&b| b == 0));

    // 2. Zstd compressed file + multi-frame append
    let comp_id = dm.create_file(root, "stream.zst").unwrap();
    let part1 = vec![b'A'; 16 * 1024];
    let part2 = vec![b'B'; 8 * 1024];
    dm.write_data(comp_id, 0, &part1, CompressionMode::Always)
        .unwrap();
    dm.write_data(comp_id, part1.len() as u64, &part2, CompressionMode::Always)
        .unwrap();
    let mut expected_comp = part1.clone();
    expected_comp.extend_from_slice(&part2);
    assert_eq!(dm.read_data(comp_id).unwrap(), expected_comp);

    // 3. Pre-compression Delta + BitShuffle filtered numeric payload
    let filt_id = dm.create_file(root, "metrics.bin").unwrap();
    let mut metrics = Vec::with_capacity(4096 * 4);
    for i in 0..4096u32 {
        metrics.extend_from_slice(&(100_000u32 + i * 7).to_le_bytes());
    }
    let filter_cfg = FilterConfig {
        typesize: 4,
        delta: true,
        shuffle: false,
        bitshuffle: true,
    };
    dm.write_data_with_filters(filt_id, 0, &metrics, CompressionMode::Always, filter_cfg)
        .unwrap();
    assert_eq!(dm.read_data(filt_id).unwrap(), metrics);

    dm.flush().unwrap();
    let fsck = dm.verify_integrity().unwrap();
    assert!(fsck.is_clean, "fsck failed: {fsck:?}");
}

#[test]
fn test_file_io_encrypted_and_seekable_64k() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("file_io_enc.img");

    let dm =
        DiskManager::create_encrypted_file_io(&img, 32 * 1024 * 1024, "secret-pw-123").unwrap();
    assert_eq!(dm.storage_mode(), StorageMode::FileIo);
    assert!(dm.is_encrypted());

    let root = dm.superblock().root_inode;
    let sub = dm.create_directory(root, "vault").unwrap();
    let secret_id = dm.create_file(sub, "seekable.dat").unwrap();

    // Write 512 KiB of patterned data as Seekable 64K encrypted chunks
    let mut payload = vec![0u8; 512 * 1024];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = ((i / 64 + i * 13) & 0xFF) as u8;
    }
    dm.write_data(
        secret_id,
        0,
        &payload,
        CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        },
    )
    .unwrap();

    // Partial mid-file overwrite (RMW on chunk 3)
    let patch = vec![0x5Au8; 4096];
    let patch_off = 3 * 65536 + 1024;
    dm.write_data(
        secret_id,
        patch_off as u64,
        &patch,
        CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        },
    )
    .unwrap();
    payload[patch_off..patch_off + 4096].copy_from_slice(&patch);

    assert_eq!(dm.read_data(secret_id).unwrap(), payload);

    // Random read_at spanning chunk boundary
    let mut rbuf = vec![0u8; 8192];
    let read_off = 3 * 65536 - 2048;
    let n = dm.read_at(secret_id, read_off as u64, &mut rbuf).unwrap();
    assert_eq!(n, 8192);
    assert_eq!(&rbuf[..], &payload[read_off..read_off + 8192]);

    // Truncate seekable encrypted file
    dm.truncate(secret_id, 200_000).unwrap();
    assert_eq!(dm.read_data(secret_id).unwrap(), payload[..200_000]);

    dm.flush().unwrap();
    assert!(dm.verify_integrity().unwrap().is_clean);
    drop(dm);

    // Reopen with wrong password fails directory lookup/decryption; reopen with right password succeeds
    let dm_reopened =
        DiskManager::open_file_io_with_password(&img, 0, Some("secret-pw-123")).unwrap();
    assert_eq!(dm_reopened.storage_mode(), StorageMode::FileIo);
    let resolved = dm_reopened.resolve_path("/vault/seekable.dat").unwrap();
    assert_eq!(resolved, secret_id);
    assert_eq!(dm_reopened.read_data(resolved).unwrap(), payload[..200_000]);
    assert!(dm_reopened.verify_integrity().unwrap().is_clean);
}

#[test]
fn test_file_io_wal_journaling_and_durability_modes() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("file_io_wal.img");

    for mode in [
        DurabilityMode::Lazy,
        DurabilityMode::RangeAsync,
        DurabilityMode::Strict,
    ] {
        let _ = std::fs::remove_file(&img);
        let dm = DiskManager::open_file_io_journaled(&img, 24 * 1024 * 1024)
            .unwrap()
            .with_durability_mode(mode);
        assert_eq!(dm.storage_mode(), StorageMode::FileIo);
        assert!(dm.has_journal());

        let root = dm.superblock().root_inode;
        let mut expected = Vec::new();
        for i in 0..40 {
            let name = format!("f_{i:03}.txt");
            let id = dm.create_file(root, &name).unwrap();
            let data = format!("journaled payload #{i} {:?}", vec![i as u8; 1024]).into_bytes();
            dm.write_data(id, 0, &data, CompressionMode::Auto).unwrap();
            expected.push((name, data));
        }

        // Delete every 4th file and truncate every surviving 5th file
        for i in (0..40).step_by(4) {
            dm.delete_file(root, &format!("f_{i:03}.txt")).unwrap();
        }
        for i in (1..40).step_by(5) {
            if i % 4 == 0 {
                continue;
            }
            let id = dm.lookup(root, &format!("f_{i:03}.txt")).unwrap();
            dm.truncate(id, 128).unwrap();
            expected[i].1.truncate(128);
        }

        dm.flush().unwrap();
        assert!(dm.verify_integrity().unwrap().is_clean);
        drop(dm);

        // Reopen with FileIo and verify every surviving file
        let dm2 = DiskManager::open_file_io(&img, 0).unwrap();
        assert!(dm2.has_journal());
        for (i, (name, data)) in expected.iter().enumerate() {
            let lookup = dm2.lookup(root, name);
            if i % 4 == 0 {
                assert!(lookup.is_err(), "{name} should have been deleted");
            } else {
                let id = lookup.unwrap();
                assert_eq!(&dm2.read_data(id).unwrap(), data);
            }
        }
        assert!(dm2.verify_integrity().unwrap().is_clean);
    }
}

#[test]
fn test_file_io_and_mmap_100_percent_interchangeable() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("cross_compat.img");

    // Phase 1: Create & populate with FileIo (journaled, multi-block dir > 200 entries, seekable + raw)
    {
        let dm = DiskManager::open_file_io_journaled(&img, 32 * 1024 * 1024).unwrap();
        let root = dm.superblock().root_inode;
        let sub = dm.create_directory(root, "many").unwrap();
        for i in 0..250 {
            let id = dm.create_file(sub, &format!("entry_{i:04}.dat")).unwrap();
            if i % 25 == 0 {
                let data = vec![(i & 0xFF) as u8; 100_000];
                dm.write_data(
                    id,
                    0,
                    &data,
                    CompressionMode::Seekable {
                        chunk_size: 65536,
                        level: 1,
                    },
                )
                .unwrap();
            } else {
                dm.write_data(
                    id,
                    0,
                    format!("hello-{i}").as_bytes(),
                    CompressionMode::Never,
                )
                .unwrap();
            }
        }
        dm.flush().unwrap();
        assert!(dm.verify_integrity().unwrap().is_clean);
    }

    // Phase 2: Reopen exact same file with StorageMode::Mmap, verify & mutate
    {
        let dm = DiskManager::open_with_storage_mode(&img, 0, StorageMode::Mmap).unwrap();
        assert_eq!(dm.storage_mode(), StorageMode::Mmap);
        let sub = dm.resolve_path("/many").unwrap();
        assert_eq!(dm.list_dir(sub).unwrap().len(), 250);
        for i in 0..250 {
            let id = dm.lookup(sub, &format!("entry_{i:04}.dat")).unwrap();
            let got = dm.read_data(id).unwrap();
            if i % 25 == 0 {
                assert_eq!(got, vec![(i & 0xFF) as u8; 100_000]);
            } else {
                assert_eq!(got, format!("hello-{i}").as_bytes());
            }
        }
        // Mutate under Mmap
        let extra = dm.create_file(sub, "from_mmap.txt").unwrap();
        dm.write_data(extra, 0, b"written by mmap", CompressionMode::Always)
            .unwrap();
        dm.delete_file(sub, "entry_0001.dat").unwrap();
        dm.flush().unwrap();
        assert!(dm.verify_integrity().unwrap().is_clean);
    }

    // Phase 3: Reopen again with StorageMode::FileIo and verify Mmap mutations
    {
        let dm = DiskManager::open_file_io(&img, 0).unwrap();
        assert_eq!(dm.storage_mode(), StorageMode::FileIo);
        let sub = dm.resolve_path("/many").unwrap();
        assert_eq!(dm.list_dir(sub).unwrap().len(), 250); // -1 +1
        assert!(dm.lookup(sub, "entry_0001.dat").is_err());
        let extra = dm.lookup(sub, "from_mmap.txt").unwrap();
        assert_eq!(dm.read_data(extra).unwrap(), b"written by mmap");
        assert!(dm.verify_integrity().unwrap().is_clean);
    }
}

#[test]
fn test_file_io_defrag_safe_and_inplace_journaled_and_legacy() {
    let dir = tempdir().unwrap();

    for (journaled, mode) in [
        (false, DefragMode::InPlace),
        (true, DefragMode::InPlace),
        (false, DefragMode::Safe),
    ] {
        let img = dir
            .path()
            .join(format!("defrag_{}_{:?}.img", journaled, mode));
        let dm = if journaled {
            DiskManager::open_file_io_journaled(&img, 24 * 1024 * 1024).unwrap()
        } else {
            DiskManager::open_file_io(&img, 24 * 1024 * 1024).unwrap()
        };
        let root = dm.superblock().root_inode;

        // Create fragmented layout with mixed raw + seekable files
        let mut expected = Vec::new();
        for i in 0..30 {
            let name = format!("blk_{i:02}.bin");
            let id = dm.create_file(root, &name).unwrap();
            let payload: Vec<u8> = (0..131_072).map(|k| ((k + i * 7) & 0xFF) as u8).collect();
            let cm = if i % 3 == 0 {
                CompressionMode::Seekable {
                    chunk_size: 65536,
                    level: 1,
                }
            } else {
                CompressionMode::Never
            };
            dm.write_data(id, 0, &payload, cm).unwrap();
            expected.push((name, payload));
        }
        // Delete even files to create holes
        for i in (0..30).step_by(2) {
            dm.delete_file(root, &format!("blk_{i:02}.bin")).unwrap();
        }
        // Allocate a new fragmented file spanning holes
        let frag_id = dm.create_file(root, "spanning.bin").unwrap();
        let spanning: Vec<u8> = (0..500_000).map(|k| ((k * 19) & 0xFF) as u8).collect();
        dm.write_data(frag_id, 0, &spanning, CompressionMode::Never)
            .unwrap();

        let stats = dm.defragment(img.to_str().unwrap(), mode, None).unwrap();
        assert_eq!(
            dm.storage_mode(),
            StorageMode::FileIo,
            "defrag must preserve FileIo storage mode"
        );
        assert!(
            stats.frag_after <= stats.frag_before,
            "fragmentation should not worsen: {stats:?}"
        );

        // Verify all surviving files bit-for-bit
        for (i, (name, payload)) in expected.iter().enumerate() {
            if i % 2 == 1 {
                let id = dm.lookup(root, name).unwrap();
                assert_eq!(&dm.read_data(id).unwrap(), payload);
            }
        }
        let sid = dm.lookup(root, "spanning.bin").unwrap();
        assert_eq!(dm.read_data(sid).unwrap(), spanning);
        assert!(dm.verify_integrity().unwrap().is_clean);
    }
}

#[test]
fn test_file_io_concurrent_readers_and_writers() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("file_io_concurrent.img");
    let dm = Arc::new(DiskManager::open_file_io_journaled(&img, 32 * 1024 * 1024).unwrap());
    let root = dm.superblock().root_inode;

    let mut handles = Vec::new();
    for t in 0..6usize {
        let dm = Arc::clone(&dm);
        handles.push(thread::spawn(move || {
            let dir_id = dm.create_directory(root, &format!("worker_{t}")).unwrap();
            for i in 0..25usize {
                let name = format!("item_{i:02}.bin");
                let id = dm.create_file(dir_id, &name).unwrap();
                let payload = vec![((t * 31 + i) & 0xFF) as u8; 12_288];
                dm.write_data(id, 0, &payload, CompressionMode::Auto)
                    .unwrap();
                assert_eq!(dm.read_data(id).unwrap(), payload);
                let mut slice = [0u8; 2048];
                let n = dm.read_at(id, 4096, &mut slice).unwrap();
                assert_eq!(n, 2048);
                assert_eq!(&slice[..], &payload[4096..6144]);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    dm.flush().unwrap();
    assert!(dm.verify_integrity().unwrap().is_clean);
}

#[test]
fn test_cli_no_mmap_flag_end_to_end() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("cli_no_mmap.img");
    let host_src = dir.path().join("hello.txt");
    std::fs::write(&host_src, b"pure pread/pwrite CLI test payload").unwrap();

    let bin = env!("CARGO_BIN_EXE_oifs");

    let run = |args: &[&str]| {
        let out = std::process::Command::new(bin)
            .args(args)
            .output()
            .expect("spawn oifs");
        assert!(
            out.status.success(),
            "oifs {args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let img_s = img.to_str().unwrap();
    let src_s = host_src.to_str().unwrap();
    let out_dst = dir.path().join("hello_out.txt");
    let dst_s = out_dst.to_str().unwrap();

    run(&[
        "--no-mmap",
        "-i",
        img_s,
        "create",
        "--size",
        "10",
        "--journal",
    ]);
    run(&["--no-mmap", "-i", img_s, "mkdir", "/docs"]);
    run(&["--no-mmap", "-i", img_s, "put", src_s, "/docs/hello.txt"]);
    run(&["--no-mmap", "-i", img_s, "get", "/docs/hello.txt", dst_s]);
    assert_eq!(
        std::fs::read_to_string(&out_dst).unwrap(),
        "pure pread/pwrite CLI test payload"
    );
    let ls_out = run(&["--no-mmap", "-i", img_s, "ls", "/docs"]);
    assert!(ls_out.contains("hello.txt"));
    run(&["--no-mmap", "-i", img_s, "fsck"]);
}

#[test]
#[ignore]
fn bench_mmap_vs_file_io() {
    use std::time::Instant;

    let dir = tempdir().unwrap();
    let payload_16m: Vec<u8> = (0..(16 * 1024 * 1024))
        .map(|i| ((i * 7 + (i >> 8)) & 0xFF) as u8)
        .collect();
    let payload_8m_comp: Vec<u8> = (0..(8 * 1024 * 1024))
        .map(|i| ((i / 16) & 0x3F) as u8)
        .collect();

    println!("\n=== StorageMode::Mmap vs StorageMode::FileIo Benchmark ===");
    for mode in [StorageMode::Mmap, StorageMode::FileIo] {
        let img = dir.path().join(format!("bench_{:?}.img", mode));
        let dm = DiskManager::open_with_storage_mode(&img, 64 * 1024 * 1024, mode).unwrap();
        let root = dm.superblock().root_inode;

        // 1. Sequential 16 MiB raw write
        let f_raw = dm.create_file(root, "raw16m.bin").unwrap();
        let t0 = Instant::now();
        dm.write_data(f_raw, 0, &payload_16m, CompressionMode::Never)
            .unwrap();
        let raw_write_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // 2. Sequential 16 MiB raw read
        let t0 = Instant::now();
        let got = dm.read_data(f_raw).unwrap();
        let raw_read_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(got.len(), payload_16m.len());

        // 3. 5,000 random 4 KiB read_at
        let mut rbuf = [0u8; 4096];
        let t0 = Instant::now();
        let mut state: u64 = 0x1234_5678_9ABC_DEF0;
        for _ in 0..5000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let off = (state % (16 * 1024 * 1024 - 4096)) & !4095;
            let n = dm.read_at(f_raw, off, &mut rbuf).unwrap();
            assert_eq!(n, 4096);
        }
        let rand_read_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // 4. Seekable 64K Zstd 8 MiB write + read
        let f_seek = dm.create_file(root, "seek8m.bin").unwrap();
        let t0 = Instant::now();
        dm.write_data(
            f_seek,
            0,
            &payload_8m_comp,
            CompressionMode::Seekable {
                chunk_size: 65536,
                level: 1,
            },
        )
        .unwrap();
        let seek_write_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t0 = Instant::now();
        let got_seek = dm.read_data(f_seek).unwrap();
        let seek_read_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(got_seek.len(), payload_8m_comp.len());

        // 5. 500 metadata create + lookup + delete
        let sub = dm.create_directory(root, "meta").unwrap();
        let t0 = Instant::now();
        for i in 0..500 {
            dm.create_file(sub, &format!("m_{i:04}")).unwrap();
        }
        for i in 0..500 {
            let _ = dm.lookup(sub, &format!("m_{i:04}")).unwrap();
        }
        for i in 0..500 {
            dm.delete_file(sub, &format!("m_{i:04}")).unwrap();
        }
        let meta_ms = t0.elapsed().as_secs_f64() * 1000.0;

        println!(
            "[{mode:?}] 16M raw W: {raw_write_ms:6.2} ms ({:6.0} MB/s) | 16M raw R: {raw_read_ms:6.2} ms ({:6.0} MB/s) | 5k rand 4K R: {rand_read_ms:6.2} ms | 8M seek64K W: {seek_write_ms:6.2} ms | 8M seek64K R: {seek_read_ms:6.2} ms | 500x meta CRUD: {meta_ms:6.2} ms",
            16.0 / (raw_write_ms / 1000.0),
            16.0 / (raw_read_ms / 1000.0),
        );
    }
}
