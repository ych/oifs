use oifs::allocator::AllocatorError;
use oifs::disk::CompressionMode;
use oifs::zstd_pool::train_dictionary;
use oifs::{
    AutoGrowConfig, DiskManager, DiskManagerError, ResizeMethod, StorageMode, VacuumOptions,
};
use std::process::Command;
use tempfile::tempdir;

fn deterministic_incompressible(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        out.push((state >> 32) as u8);
    }
    out
}

fn deterministic_samples(count: usize, each_len: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| {
            format!(
                "{{\"service\":\"oifs-resize-test\",\"seq\":{},\"region\":\"ap-east-1\",\"payload\":\"{}\" }}\n",
                i,
                "repeated_json_schema_token_".repeat(each_len / 28 + 1)
            )
            .into_bytes()
        })
        .collect()
}

#[test]
fn test_in_place_grow_and_shrink_matrix() {
    let dir = tempdir().unwrap();

    for mode in [StorageMode::Mmap, StorageMode::FileIo] {
        for journaled in [false, true] {
            for encrypted in [false, true] {
                let tag = format!("{mode:?}_{journaled}_{encrypted}");
                let img = dir.path().join(format!("inplace_{tag}.img"));
                let password = encrypted.then_some("Passw0rd!Resize");

                let initial_bytes = 4 * 1024 * 1024u64;
                let grown_bytes = 32 * 1024 * 1024u64;
                let shrunk_bytes = 8 * 1024 * 1024u64;

                let dm =
                    DiskManager::open_with_options(&img, initial_bytes, password, journaled, mode)
                        .unwrap();

                let p1 = deterministic_incompressible(96 * 1024, 11);
                let ino1 = dm
                    .create_file_with_data(0, "alpha.bin", &p1, CompressionMode::Never)
                    .unwrap();

                // Grow within the same 128 MiB bitmap boundary -> InPlace O(1)
                let grow_stats = dm.grow(grown_bytes).unwrap();
                assert_eq!(grow_stats.method, ResizeMethod::InPlace, "{tag}");
                assert_eq!(grow_stats.bytes_after, grown_bytes, "{tag}");
                assert_eq!(std::fs::metadata(&img).unwrap().len(), grown_bytes, "{tag}");
                assert_eq!(dm.image_size_bytes(), grown_bytes, "{tag}");

                // Write a second large file far into the newly grown tail
                let p2 = deterministic_incompressible(5 * 1024 * 1024, 22);
                let ino2 = dm
                    .create_file_with_data(0, "tail.bin", &p2, CompressionMode::Never)
                    .unwrap();
                assert_eq!(dm.read_data(ino2).unwrap(), p2, "{tag}");

                // Delete tail.bin and shrink back down to 8 MiB
                dm.delete_file(0, "tail.bin").unwrap();

                // Also place a small file at tail-safe or potentially high block offset before shrink
                let p3 = deterministic_incompressible(32 * 1024, 33);
                let ino3 = dm
                    .create_file_with_data(0, "beta.bin", &p3, CompressionMode::Never)
                    .unwrap();

                let shrink_stats = dm.resize(shrunk_bytes).unwrap();
                assert!(
                    matches!(
                        shrink_stats.method,
                        ResizeMethod::InPlace | ResizeMethod::AtomicRelayout
                    ),
                    "{tag}"
                );
                assert_eq!(shrink_stats.bytes_after, shrunk_bytes, "{tag}");
                assert_eq!(
                    std::fs::metadata(&img).unwrap().len(),
                    shrunk_bytes,
                    "{tag}"
                );
                assert_eq!(dm.read_data(ino1).unwrap(), p1, "{tag}");
                assert_eq!(dm.read_data(ino3).unwrap(), p3, "{tag}");

                let report = dm.verify_integrity().unwrap();
                assert!(report.is_clean, "{tag}: fsck errors {:?}", report);
                drop(dm);

                // Reopen and verify persistence
                let reopened =
                    DiskManager::open_with_options(&img, shrunk_bytes, password, journaled, mode)
                        .unwrap();
                assert_eq!(reopened.image_size_bytes(), shrunk_bytes, "{tag}");
                assert_eq!(reopened.read_data(ino1).unwrap(), p1, "{tag}");
                assert_eq!(reopened.read_data(ino3).unwrap(), p3, "{tag}");
                assert!(reopened.verify_integrity().unwrap().is_clean, "{tag}");
            }
        }
    }
}

#[test]
fn test_cross_boundary_128mb_bitmap_grow_and_vacuum_reclaim() {
    let dir = tempdir().unwrap();

    for mode in [StorageMode::Mmap, StorageMode::FileIo] {
        let img = dir.path().join(format!("cross_128m_{mode:?}.img"));
        let password = Some("Strong#Cross128!");

        // Create small 4 MiB encrypted + journaled image
        let dm =
            DiskManager::open_with_options(&img, 4 * 1024 * 1024, password, true, mode).unwrap();

        // 1. Standard file
        let standard_payload = deterministic_incompressible(150 * 1024, 101);
        let ino_std = dm
            .create_file_with_data(0, "std.bin", &standard_payload, CompressionMode::Never)
            .unwrap();

        // 2. Seekable 64K compressed + encrypted file
        let seekable_payload = vec![b'S'; 192 * 1024]
            .into_iter()
            .enumerate()
            .map(|(i, b)| b ^ ((i % 13) as u8))
            .collect::<Vec<u8>>();
        let ino_seek = dm.create_file(0, "seek64k.bin").unwrap();
        dm.write_chunked_64k(ino_seek, 0, &seekable_payload, 3)
            .unwrap();

        // 3. Zstd dictionary-compressed file (then close and reopen WITHOUT dict registered
        //    to verify AtomicRelayout copies opaque compressed frames without needing the dict!)
        let samples = deterministic_samples(64, 512);
        let dict_bytes = train_dictionary(&samples, 16 * 1024).unwrap();
        dm.register_and_use_zstd_dict(&dict_bytes).unwrap();
        let dict_payload = samples[0..8].concat();
        let ino_dict = dm
            .create_file_with_data(0, "logs.dict.json", &dict_payload, CompressionMode::Always)
            .unwrap();
        dm.flush().unwrap();
        drop(dm);

        // Reopen WITHOUT registering dict_bytes yet, grow past 128 MiB -> 144 MiB
        let dm2 =
            DiskManager::open_with_options(&img, 4 * 1024 * 1024, password, true, mode).unwrap();
        let grow_target = 144 * 1024 * 1024u64;
        let grow_stats = dm2.grow(grow_target).unwrap();
        assert_eq!(grow_stats.method, ResizeMethod::AtomicRelayout);
        assert_eq!(grow_stats.bytes_after, grow_target);
        assert_eq!(dm2.superblock().data_bitmap_blocks(), 2);
        assert_eq!(std::fs::metadata(&img).unwrap().len(), grow_target);

        // Verify all inode IDs preserved
        assert_eq!(dm2.resolve_path("/std.bin").unwrap(), ino_std);
        assert_eq!(dm2.resolve_path("/seek64k.bin").unwrap(), ino_seek);
        assert_eq!(dm2.resolve_path("/logs.dict.json").unwrap(), ino_dict);

        // Standard and seekable are immediately readable; dict file becomes readable after registering dict
        assert_eq!(dm2.read_data(ino_std).unwrap(), standard_payload);
        assert_eq!(dm2.read_data(ino_seek).unwrap(), seekable_payload);
        dm2.register_zstd_dict(&dict_bytes).unwrap();
        assert_eq!(dm2.read_data(ino_dict).unwrap(), dict_payload);

        // Write > 132 MiB worth of data (proving blocks beyond the first 128 MiB bitmap block work!)
        let chunk_34m = vec![0x5Au8; 34 * 1024 * 1024];
        let mut bulk_names = Vec::new();
        for i in 0..4 {
            let name = format!("bulk_{i}.bin");
            dm2.create_file_with_data(0, &name, &chunk_34m, CompressionMode::Never)
                .unwrap();
            bulk_names.push(name);
        }
        assert!(dm2.verify_integrity().unwrap().is_clean);

        // Delete bulk files and run Vacuum -> shrinks back below 128 MiB boundary (data_bitmap_blocks == 1)
        for name in &bulk_names {
            dm2.delete_file(0, name).unwrap();
        }
        let vac_stats = dm2.vacuum(VacuumOptions::default()).unwrap();
        assert_eq!(vac_stats.method, ResizeMethod::AtomicRelayout);
        assert_eq!(dm2.superblock().data_bitmap_blocks(), 1);
        assert!(vac_stats.bytes_after < 16 * 1024 * 1024);
        assert!(vac_stats.bytes_reclaimed > 120 * 1024 * 1024);
        assert_eq!(
            std::fs::metadata(&img).unwrap().len(),
            vac_stats.bytes_after
        );

        // Verify all surviving inodes kept IDs and payloads after vacuum relayout
        assert_eq!(dm2.resolve_path("/std.bin").unwrap(), ino_std);
        assert_eq!(dm2.resolve_path("/seek64k.bin").unwrap(), ino_seek);
        assert_eq!(dm2.resolve_path("/logs.dict.json").unwrap(), ino_dict);
        assert_eq!(dm2.read_data(ino_std).unwrap(), standard_payload);
        assert_eq!(dm2.read_data(ino_seek).unwrap(), seekable_payload);
        assert_eq!(dm2.read_data(ino_dict).unwrap(), dict_payload);
        assert!(dm2.verify_integrity().unwrap().is_clean);

        drop(dm2);
        let reopened =
            DiskManager::open_with_options(&img, vac_stats.bytes_after, password, true, mode)
                .unwrap();
        reopened.register_zstd_dict(&dict_bytes).unwrap();
        assert_eq!(reopened.superblock().data_bitmap_blocks(), 1);
        assert_eq!(reopened.read_data(ino_std).unwrap(), standard_payload);
        assert_eq!(reopened.read_data(ino_seek).unwrap(), seekable_payload);
        assert_eq!(reopened.read_data(ino_dict).unwrap(), dict_payload);
        assert!(reopened.verify_integrity().unwrap().is_clean);
    }
}

#[test]
fn test_transparent_auto_grow_data_exhaustion_across_paths() {
    let dir = tempdir().unwrap();

    for (mode, journaled, encrypted) in [
        (StorageMode::Mmap, false, false),
        (StorageMode::Mmap, true, true),
        (StorageMode::FileIo, true, false),
        (StorageMode::FileIo, false, true),
    ] {
        let img = dir.path().join(format!(
            "autogrow_data_{mode:?}_{journaled}_{encrypted}.img"
        ));
        let password = encrypted.then_some("AutoGrow#123");

        let dm = DiskManager::open_with_options(&img, 2 * 1024 * 1024, password, journaled, mode)
            .unwrap();
        dm.set_auto_grow(AutoGrowConfig::enabled());

        // 1. write_data
        let p_write = deterministic_incompressible(3 * 1024 * 1024, 1);
        let i_write = dm.create_file(0, "write_data.bin").unwrap();
        dm.write_data(i_write, 0, &p_write, CompressionMode::Never)
            .unwrap();

        // 2. create_file_with_data
        let p_cfwd = deterministic_incompressible(4 * 1024 * 1024, 2);
        let i_cfwd = dm
            .create_file_with_data(0, "cfwd.bin", &p_cfwd, CompressionMode::Never)
            .unwrap();

        // 3. Seekable 64K
        let p_seek = deterministic_incompressible(3 * 1024 * 1024, 3);
        let i_seek = dm.create_file(0, "seek.bin").unwrap();
        dm.write_chunked_64k(i_seek, 0, &p_seek, 0).unwrap();

        // 4. WriteBatch
        let p_batch1 = deterministic_incompressible(1500 * 1024, 4);
        let p_batch2 = deterministic_incompressible(1500 * 1024, 5);
        let mut batch = dm.begin_write_batch();
        batch.create_file_with_data(0, "b1.bin", p_batch1.clone(), CompressionMode::Never);
        batch.create_file_with_data(0, "b2.bin", p_batch2.clone(), CompressionMode::Never);
        let batch_res = batch.commit().unwrap();
        let i_b1 = batch_res.created_inodes[0];
        let i_b2 = batch_res.created_inodes[1];

        assert!(dm.image_size_bytes() > 12 * 1024 * 1024);
        assert_eq!(dm.read_data(i_write).unwrap(), p_write);
        assert_eq!(dm.read_data(i_cfwd).unwrap(), p_cfwd);
        assert_eq!(dm.read_data(i_seek).unwrap(), p_seek);
        assert_eq!(dm.read_data(i_b1).unwrap(), p_batch1);
        assert_eq!(dm.read_data(i_b2).unwrap(), p_batch2);
        assert!(dm.verify_integrity().unwrap().is_clean);
    }
}

#[test]
fn test_transparent_auto_grow_inode_table_expansion() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("autogrow_inodes.img");

    // 1 MiB image starts with ~32-64 usable inode slots depending on geometry;
    // creating 1,500 files must expand the inode table cleanly while keeping earlier IDs intact.
    let dm =
        DiskManager::open_with_options(&img, 1024 * 1024, None, true, StorageMode::Mmap).unwrap();
    let initial_usable_inodes = dm.superblock().usable_inode_count();
    assert!(initial_usable_inodes < 1500);

    dm.set_auto_grow(AutoGrowConfig::enabled());
    let anchor_data = b"early-anchor-inode-payload".to_vec();
    let anchor_id = dm
        .create_file_with_data(0, "anchor_0.txt", &anchor_data, CompressionMode::Never)
        .unwrap();

    for idx in 1..1500usize {
        let name = format!("f_{idx:04}.txt");
        let body = format!("payload_{idx}");
        dm.create_file_with_data(0, &name, body.as_bytes(), CompressionMode::Never)
            .unwrap();
    }

    assert!(dm.superblock().usable_inode_count() >= 1501);
    assert_eq!(dm.resolve_path("/anchor_0.txt").unwrap(), anchor_id);
    assert_eq!(dm.read_data(anchor_id).unwrap(), anchor_data);
    assert_eq!(
        dm.read_data(dm.resolve_path("/f_1499.txt").unwrap())
            .unwrap(),
        b"payload_1499"
    );
    assert!(dm.verify_integrity().unwrap().is_clean);
}

#[test]
fn test_auto_grow_cap_fixed_default_and_read_only_safety() {
    let dir = tempdir().unwrap();

    // 1. Default AutoGrowConfig is disabled -> deterministic NoSpace
    let img_fixed = dir.path().join("fixed_default.img");
    let dm_fixed = DiskManager::open(&img_fixed, 1024 * 1024).unwrap();
    assert!(!dm_fixed.auto_grow_config().enabled);
    let big = deterministic_incompressible(2 * 1024 * 1024, 99);
    let err = dm_fixed
        .create_file_with_data(0, "too_big.bin", &big, CompressionMode::Never)
        .unwrap_err();
    assert!(matches!(
        err,
        DiskManagerError::Allocator(AllocatorError::NoSpace)
    ));
    assert!(dm_fixed.verify_integrity().unwrap().is_clean);
    drop(dm_fixed);

    // 2. Hard max_size_bytes cap enforcement
    let img_cap = dir.path().join("capped.img");
    let dm_cap = DiskManager::open(&img_cap, 2 * 1024 * 1024).unwrap();
    dm_cap.set_auto_grow(AutoGrowConfig::enabled().with_max_size_bytes(6 * 1024 * 1024));

    let keep_data = deterministic_incompressible(512 * 1024, 7);
    let keep_ino = dm_cap
        .create_file_with_data(0, "keep.bin", &keep_data, CompressionMode::Never)
        .unwrap();

    // Compressible 8 MiB payload still succeeds under 6 MiB image cap (best-effort preemptive grow)
    let compressible = vec![b'A'; 8 * 1024 * 1024];
    let comp_ino = dm_cap
        .create_file_with_data(0, "zeros.bin", &compressible, CompressionMode::Always)
        .unwrap();
    assert!(dm_cap.image_size_bytes() <= 6 * 1024 * 1024);

    // Incompressible 8 MiB payload hits max_size_bytes cap cleanly
    let too_big = deterministic_incompressible(8 * 1024 * 1024, 88);
    let err = dm_cap
        .create_file_with_data(0, "overflow.bin", &too_big, CompressionMode::Never)
        .unwrap_err();
    assert!(matches!(
        err,
        DiskManagerError::Allocator(AllocatorError::NoSpace)
    ));
    assert!(dm_cap.image_size_bytes() <= 6 * 1024 * 1024);
    assert_eq!(dm_cap.read_data(keep_ino).unwrap(), keep_data);
    assert_eq!(dm_cap.read_data(comp_ino).unwrap(), compressible);
    assert!(dm_cap.verify_integrity().unwrap().is_clean);
    drop(dm_cap);

    // 3. Read-only mount rejects grow / resize / vacuum with DiskManagerError::ReadOnly
    let dm_ro = DiskManager::open_read_only(&img_cap).unwrap();
    assert!(matches!(
        dm_ro.grow(10 * 1024 * 1024).unwrap_err(),
        DiskManagerError::ReadOnly
    ));
    assert!(matches!(
        dm_ro.resize(4 * 1024 * 1024).unwrap_err(),
        DiskManagerError::ReadOnly
    ));
    assert!(matches!(
        dm_ro.vacuum(VacuumOptions::default()).unwrap_err(),
        DiskManagerError::ReadOnly
    ));
}

#[test]
fn test_cli_autogrow_resize_and_vacuum_e2e() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("cli_resize.img");
    let src_file = dir.path().join("big_src.bin");
    let out_file = dir.path().join("big_out.bin");
    let bin = env!("CARGO_BIN_EXE_oifs");

    let payload = deterministic_incompressible(5 * 1024 * 1024, 777);
    std::fs::write(&src_file, &payload).unwrap();

    // 1. Create 2 MiB image
    let status = Command::new(bin)
        .args(["-i", img.to_str().unwrap(), "create", "--size", "2"])
        .status()
        .unwrap();
    assert!(status.success());

    // 2. `put` 5 MiB incompressible file using --auto-grow --no-compress
    let status = Command::new(bin)
        .args([
            "-i",
            img.to_str().unwrap(),
            "--auto-grow",
            "put",
            src_file.to_str().unwrap(),
            "/big.bin",
            "--no-compress",
        ])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(std::fs::metadata(&img).unwrap().len() >= 5 * 1024 * 1024);

    // 3. Read back via `get`
    let status = Command::new(bin)
        .args([
            "-i",
            img.to_str().unwrap(),
            "get",
            "/big.bin",
            out_file.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(&out_file).unwrap(), payload);

    // 4. `resize --size 32`
    let out = Command::new(bin)
        .args([
            "-i",
            img.to_str().unwrap(),
            "--json",
            "resize",
            "--size",
            "32",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "resize failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let resize_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(resize_json["ok"], true);
    assert_eq!(resize_json["bytes_after"], 32 * 1024 * 1024);
    assert_eq!(std::fs::metadata(&img).unwrap().len(), 32 * 1024 * 1024);

    // 5. Delete /big.bin and `vacuum --json`
    let status = Command::new(bin)
        .args(["-i", img.to_str().unwrap(), "rm", "/big.bin"])
        .status()
        .unwrap();
    assert!(status.success());

    let out = Command::new(bin)
        .args(["-i", img.to_str().unwrap(), "--json", "vacuum"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "vacuum failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let vac_json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(vac_json["ok"], true);
    assert!(vac_json["bytes_reclaimed"].as_u64().unwrap() > 25 * 1024 * 1024);
    assert!(std::fs::metadata(&img).unwrap().len() < 4 * 1024 * 1024);
}
