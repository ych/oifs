//! Unit, regression, concurrency, and benchmark tests for whole-image CoW cloning
//! (`DiskManager::clone_image`), 3-tier cross-image fast file transfer
//! (`DiskManager::transfer_file_to`), and sub-range cross-image splice/move
//! (`DiskManager::splice_range_to`).

use oifs::disk::{
    CloneMethod, CompressionMode, DiskManager, DiskManagerError, LogicalRun, ReflinkFallback,
    SpliceOptions, SpliceTrimMode, TransferMethod, TransferOptions, can_splice_fast_path,
    canonical_file_id, classify_reflink_errno, clear_unsupported_reflink_cache,
    coalesce_logical_runs, combine_transfer_tier, is_reflink_device_pair_unsupported,
    mark_reflink_device_pair_unsupported, plan_splice_partition, punch_table_slot,
    push_logical_block_run, rebase_chunk_entry, shift_table_collapse, should_use_raw_fast_path,
    slot_reads_as_zero, transfer_byte_range,
};
use oifs::filters::FilterConfig;
use oifs::inode::{CHUNK_SIZE_64K, ChunkEntry, FileType, INODE_FLAG_SEEKABLE_64K, Inode};
use oifs::storage::StorageMode;
use std::os::unix::io::AsRawFd;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;
use tempfile::tempdir;

// ============================================================================
// 1. Unit Tests for Pure State-Machine & Layout Helpers
// ============================================================================

#[test]
fn test_unit_should_use_raw_fast_path() {
    assert!(should_use_raw_fast_path(false, false));
    assert!(!should_use_raw_fast_path(true, false));
    assert!(!should_use_raw_fast_path(false, true));
    assert!(!should_use_raw_fast_path(true, true));
}

#[test]
fn test_unit_classify_reflink_errno_and_device_pair_cache() {
    for err in [libc::EOPNOTSUPP, libc::ENOTTY, libc::EXDEV, libc::ENOSYS] {
        assert_eq!(
            classify_reflink_errno(err),
            ReflinkFallback::CacheUnsupportedDevicePair
        );
    }
    #[cfg(not(target_os = "linux"))]
    assert_eq!(
        classify_reflink_errno(libc::ENOTSUP),
        ReflinkFallback::CacheUnsupportedDevicePair
    );

    for err in [libc::EINVAL, libc::EPERM, libc::ETXTBSY] {
        assert_eq!(
            classify_reflink_errno(err),
            ReflinkFallback::FallbackThisExtentOnly
        );
    }

    for err in [libc::ENOSPC, libc::EIO, libc::EBADF] {
        assert_eq!(classify_reflink_errno(err), ReflinkFallback::Fatal);
    }

    clear_unsupported_reflink_cache();
    assert!(!is_reflink_device_pair_unsupported(1001, 2002));
    mark_reflink_device_pair_unsupported(1001, 2002);
    assert!(is_reflink_device_pair_unsupported(1001, 2002));
    assert!(!is_reflink_device_pair_unsupported(2002, 1001));
    clear_unsupported_reflink_cache();
    assert!(!is_reflink_device_pair_unsupported(1001, 2002));

    let dir = tempdir().expect("tempdir");
    let f1 = std::fs::File::create(dir.path().join("f1.bin")).expect("create f1");
    let f2 = std::fs::File::create(dir.path().join("f2.bin")).expect("create f2");
    let id1 = canonical_file_id(f1.as_raw_fd()).expect("canonical_file_id f1");
    let id2 = canonical_file_id(f2.as_raw_fd()).expect("canonical_file_id f2");
    assert_ne!(id1, id2);
    assert_eq!(id1, canonical_file_id(f1.as_raw_fd()).unwrap());
}

#[test]
fn test_unit_combine_transfer_tier_lattice() {
    let all = [
        TransferMethod::Reflink,
        TransferMethod::CopyFileRange,
        TransferMethod::RawBlockCopy,
        TransferMethod::LogicalReencode,
    ];

    for &a in &all {
        // Idempotence, identity (Reflink), and top (LogicalReencode).
        assert_eq!(combine_transfer_tier(a, a), a);
        assert_eq!(combine_transfer_tier(a, TransferMethod::Reflink), a);
        assert_eq!(
            combine_transfer_tier(a, TransferMethod::LogicalReencode),
            TransferMethod::LogicalReencode
        );

        for &b in &all {
            // Commutativity and monotonicity.
            let ab = combine_transfer_tier(a, b);
            assert_eq!(ab, combine_transfer_tier(b, a));
            assert!(ab.tier_rank() >= a.tier_rank());
            assert!(ab.tier_rank() >= b.tier_rank());

            for &c in &all {
                // Associativity.
                assert_eq!(
                    combine_transfer_tier(ab, c),
                    combine_transfer_tier(a, combine_transfer_tier(b, c))
                );
            }
        }
    }
}

#[test]
fn test_unit_rebase_chunk_entry() {
    assert_eq!(
        rebase_chunk_entry(ChunkEntry::EMPTY, 1234),
        ChunkEntry::EMPTY
    );

    let orig = ChunkEntry::new(42, 7, ChunkEntry::FLAG_COMPRESSED, 19_876);
    let rebased = rebase_chunk_entry(orig, 987_654);
    assert_eq!(rebased.start_block(), 987_654);
    assert_eq!(rebased.block_count(), 7);
    assert_eq!(rebased.flags(), ChunkEntry::FLAG_COMPRESSED);
    assert_eq!(rebased.compressed_len(), 19_876);
}

#[test]
fn test_unit_coalesce_logical_runs() {
    assert!(coalesce_logical_runs(&[]).is_empty());
    assert!(coalesce_logical_runs(&[(0, 0), (1, 0), (5, 0)]).is_empty());

    let input = [
        (0, 100),
        (1, 101),
        (2, 102),
        (3, 0),   // sparse hole
        (4, 104), // logical gap after hole -> new run
        (5, 105),
        (6, 200), // physical discontinuity -> new run
        (8, 201), // logical discontinuity -> new run
    ];
    let runs = coalesce_logical_runs(&input);
    assert_eq!(
        runs,
        vec![
            LogicalRun {
                logical_start: 0,
                phys_start: 100,
                count: 3,
            },
            LogicalRun {
                logical_start: 4,
                phys_start: 104,
                count: 2,
            },
            LogicalRun {
                logical_start: 6,
                phys_start: 200,
                count: 1,
            },
            LogicalRun {
                logical_start: 8,
                phys_start: 201,
                count: 1,
            },
        ]
    );

    let mut incremental = Vec::new();
    for &(l, p) in &input {
        push_logical_block_run(&mut incremental, l, p);
    }
    assert_eq!(incremental, runs);
}

#[test]
fn test_unit_transfer_byte_range_edge_cases_and_multiblock() {
    let dir = tempdir().expect("tempdir");
    let src_path = dir.path().join("src.bin");
    let dst_path = dir.path().join("dst.bin");

    let total_len = 640 * 1024 + 321;
    let src_bytes: Vec<u8> = (0..total_len)
        .map(|i| ((i * 131 + 17) & 0xFF) as u8)
        .collect();
    std::fs::write(&src_path, &src_bytes).expect("write src");
    std::fs::write(&dst_path, vec![0xA5u8; total_len + 4096]).expect("write dst");

    let src_file = std::fs::File::open(&src_path).expect("open src");
    let dst_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&dst_path)
        .expect("open dst");

    // 1. Zero-length transfer leaves destination untouched.
    let m0 = transfer_byte_range(src_file.as_raw_fd(), 0, dst_file.as_raw_fd(), 128, 0)
        .expect("zero-len transfer");
    assert_ne!(m0, TransferMethod::LogicalReencode);
    let dst_after_zero = std::fs::read(&dst_path).expect("read dst");
    assert!(dst_after_zero.iter().all(|&b| b == 0xA5));

    // 2. Unaligned sub-block range.
    transfer_byte_range(src_file.as_raw_fd(), 37, dst_file.as_raw_fd(), 91, 777)
        .expect("unaligned transfer");
    let dst_after_unaligned = std::fs::read(&dst_path).expect("read dst");
    assert_eq!(&dst_after_unaligned[91..91 + 777], &src_bytes[37..37 + 777]);
    assert_eq!(dst_after_unaligned[90], 0xA5);
    assert_eq!(dst_after_unaligned[91 + 777], 0xA5);

    // 3. Aligned single-block transfer.
    transfer_byte_range(src_file.as_raw_fd(), 4096, dst_file.as_raw_fd(), 8192, 4096)
        .expect("aligned block transfer");
    let dst_after_aligned = std::fs::read(&dst_path).expect("read dst");
    assert_eq!(
        &dst_after_aligned[8192..8192 + 4096],
        &src_bytes[4096..8192]
    );

    // 4. Multi-block > 256 KiB internal buffer transfer.
    transfer_byte_range(
        src_file.as_raw_fd(),
        0,
        dst_file.as_raw_fd(),
        4096,
        total_len as u64,
    )
    .expect("multiblock transfer");
    let dst_after_multi = std::fs::read(&dst_path).expect("read dst");
    assert_eq!(&dst_after_multi[4096..4096 + total_len], &src_bytes[..]);
}

#[test]
fn test_unit_splice_helpers() {
    let raw_inode = Inode::new(FileType::File);
    assert_eq!(
        can_splice_fast_path(&raw_inode, &raw_inode, false, false),
        Some(4096)
    );
    assert_eq!(
        can_splice_fast_path(&raw_inode, &raw_inode, true, false),
        None
    );
    assert_eq!(
        can_splice_fast_path(&raw_inode, &raw_inode, false, true),
        None
    );

    let dir_inode = Inode::new(FileType::Directory);
    assert_eq!(
        can_splice_fast_path(&dir_inode, &raw_inode, false, false),
        None
    );

    let mut stream_inode = Inode::new(FileType::File);
    stream_inode.compressed_size = 1024;
    assert_eq!(
        can_splice_fast_path(&stream_inode, &raw_inode, false, false),
        None
    );

    let mut seek_a = Inode::new(FileType::File);
    seek_a.flags = INODE_FLAG_SEEKABLE_64K;
    seek_a.filter_typesize = 4;
    seek_a.filter_delta = true;
    seek_a.filter_shuffle = true;

    let seek_b = seek_a;
    assert_eq!(
        can_splice_fast_path(&seek_a, &seek_b, false, false),
        Some(CHUNK_SIZE_64K as u64)
    );
    assert_eq!(
        can_splice_fast_path(&seek_a, &raw_inode, false, false),
        None
    );

    let mut seek_diff_filter = seek_a;
    seek_diff_filter.filter_bitshuffle = true;
    assert_eq!(
        can_splice_fast_path(&seek_a, &seek_diff_filter, false, false),
        None
    );

    // Overflow checks in `plan_splice_partition`.
    assert_eq!(plan_splice_partition(u64::MAX - 5, 0, 10, 4096), None);
    assert_eq!(plan_splice_partition(0, u64::MAX - 5, 10, 4096), None);

    // Mismatched alignment -> entire range stays in head.
    let mis = plan_splice_partition(100, 200, 16_384, 4096).unwrap();
    assert_eq!(mis.head_len, 16_384);
    assert_eq!(mis.body_len, 0);
    assert_eq!(mis.tail_len, 0);

    // Unaligned head + aligned body + unaligned tail.
    let part = plan_splice_partition(500, 4596, 12_000, 4096).unwrap();
    assert_eq!(part.head_len + part.body_len + part.tail_len, 12_000);
    assert_eq!(part.head_len, 3596);
    assert_eq!(part.body_len, 8192);
    assert_eq!(part.tail_len, 212);
    assert_eq!(part.body_src_off % 4096, 0);
    assert_eq!(part.body_dst_off % 4096, 0);

    // `shift_table_collapse` & slot zeroing.
    let mut tbl = vec![10, 0, 30, 40, 50];
    let freed = shift_table_collapse(&mut tbl, 1, 2);
    assert_eq!(freed, vec![30]);
    assert_eq!(tbl, vec![10, 40, 50]);

    let mut slot = 777u64;
    assert!(!slot_reads_as_zero(slot, false));
    assert_eq!(punch_table_slot(&mut slot), Some(777));
    assert_eq!(slot, 0);
    assert!(slot_reads_as_zero(slot, false));
    assert!(slot_reads_as_zero(ChunkEntry::EMPTY.0, true));
}

// ============================================================================
// 2. Full Matrix Whole-Image & File Transfer Regression Tests
// ============================================================================

#[test]
fn test_clone_image_cow_fork_independence() {
    let dir = tempdir().expect("tempdir");
    let src_img = dir.path().join("src.img");
    let dst_img = dir.path().join("fork.img");

    {
        let dm = DiskManager::open_with_options(
            &src_img,
            16 * 1024 * 1024,
            None,
            true,
            StorageMode::Mmap,
        )
        .expect("create src");
        dm.create_file_with_data(0, "base.txt", b"original base data", CompressionMode::Never)
            .expect("seed base");
        dm.flush().expect("flush src");
    }

    let method = DiskManager::clone_image(&src_img, &dst_img).expect("clone_image");
    #[cfg(target_os = "macos")]
    assert_eq!(
        method,
        CloneMethod::ApfsClonefile,
        "macOS APFS should use O(1) clonefile(2)"
    );
    #[cfg(not(target_os = "macos"))]
    let _ = method;

    // Cloning onto an existing path must fail with AlreadyExists.
    assert!(DiskManager::clone_image(&src_img, &dst_img).is_err());

    // Mutate the fork and verify the source image is completely unaffected.
    {
        let fork = DiskManager::open_with_options(
            &dst_img,
            16 * 1024 * 1024,
            None,
            true,
            StorageMode::Mmap,
        )
        .expect("open fork");
        let base_ino = fork.lookup(0, "base.txt").expect("lookup base in fork");
        assert_eq!(fork.read_data(base_ino).unwrap(), b"original base data");
        fork.write_data(base_ino, 0, b"mutated fork data!", CompressionMode::Never)
            .expect("mutate fork");
        fork.create_file_with_data(0, "only_in_fork.txt", b"forked", CompressionMode::Never)
            .expect("create fork-only file");
        fork.flush().expect("flush fork");
        assert!(fork.verify_integrity().unwrap().is_clean);
    }

    {
        let src = DiskManager::open_read_only(&src_img).expect("open src ro");
        let base_ino = src.lookup(0, "base.txt").expect("lookup base in src");
        assert_eq!(src.read_data(base_ino).unwrap(), b"original base data");
        assert!(src.lookup(0, "only_in_fork.txt").is_err());
        assert!(src.verify_integrity().unwrap().is_clean);
    }
}

#[test]
fn test_transfer_file_matrix_plain_zstd_filtered_seekable_and_sparse() {
    let matrix = [
        (StorageMode::Mmap, StorageMode::Mmap, true),
        (StorageMode::Mmap, StorageMode::Mmap, false),
        (StorageMode::FileIo, StorageMode::FileIo, true),
        (StorageMode::FileIo, StorageMode::FileIo, false),
        (StorageMode::Mmap, StorageMode::FileIo, true),
        (StorageMode::Mmap, StorageMode::FileIo, false),
        (StorageMode::FileIo, StorageMode::Mmap, true),
        (StorageMode::FileIo, StorageMode::Mmap, false),
    ];

    for (src_mode, dst_mode, journaled) in matrix {
        let dir = tempdir().expect("tempdir");
        let src_img = dir.path().join("a.img");
        let dst_img = dir.path().join("b.img");

        let src =
            DiskManager::open_with_options(&src_img, 32 * 1024 * 1024, None, journaled, src_mode)
                .expect("open src");
        let dst =
            DiskManager::open_with_options(&dst_img, 32 * 1024 * 1024, None, journaled, dst_mode)
                .expect("open dst");

        // 1. Plain uncompressed multi-block file (128 KiB).
        let raw_data: Vec<u8> = (0..(128 * 1024))
            .map(|i| ((i * 29 + 11) & 0xFF) as u8)
            .collect();
        src.create_file_with_data(0, "raw.bin", &raw_data, CompressionMode::Never)
            .expect("create raw");

        // 2. Zstd stream compressed file (256 KiB repetitive).
        let zstd_data = vec![0x42u8; 256 * 1024];
        src.create_file_with_data(0, "stream.zst", &zstd_data, CompressionMode::Always)
            .expect("create zstd");

        // 3. SIMD-filtered (Delta + Shuffle) + Zstd file.
        let mut filtered_data = Vec::with_capacity(64 * 1024);
        for v in 0u32..(16 * 1024) {
            filtered_data.extend_from_slice(&v.to_le_bytes());
        }
        let filters = FilterConfig::numeric(4);
        src.create_file_with_data_with_filters(
            0,
            "filtered.bin",
            &filtered_data,
            CompressionMode::Always,
            filters,
        )
        .expect("create filtered");

        // 4. BitShuffle-filtered + Zstd file.
        let mut bitshuffle_data = Vec::with_capacity(64 * 1024);
        for v in 0u32..(16 * 1024) {
            bitshuffle_data.extend_from_slice(&(v << 3).to_le_bytes());
        }
        src.create_file_with_data_with_filters(
            0,
            "bitshuffle.bin",
            &bitshuffle_data,
            CompressionMode::Always,
            FilterConfig::bitshuffle_only(4),
        )
        .expect("create bitshuffle");

        // 5. Seekable 64K multi-chunk file (256 KiB = 4 chunks, mixed compressed + raw).
        let mut seekable_data = vec![0x77u8; 128 * 1024];
        seekable_data.extend((0..(128 * 1024)).map(|i| ((i * 197 + 53) & 0xFF) as u8));
        src.create_file_with_data(
            0,
            "seekable.bin",
            &seekable_data,
            CompressionMode::Seekable {
                chunk_size: CHUNK_SIZE_64K as u32,
                level: 1,
            },
        )
        .expect("create seekable");

        // 6. Sparse file with a large unallocated zero hole (8 MiB logical, only 8 KiB allocated).
        let sparse_ino = src.create_file(0, "sparse.bin").expect("create sparse");
        src.write_data(sparse_ino, 0, b"HEAD_MAGIC", CompressionMode::Never)
            .expect("write head");
        src.write_data(
            sparse_ino,
            8 * 1024 * 1024 - 10,
            b"TAIL_MAGIC",
            CompressionMode::Never,
        )
        .expect("write tail");
        let sparse_expected = src.read_data(sparse_ino).expect("read sparse");

        // Transfer all 6 files into `dst` without decompression/recompression.
        let dst_sub = dst.create_directory(0, "imported").expect("mkdir imported");
        let used_before_sparse = dst.analyze_fragmentation().unwrap().used_blocks;

        for (name, expected) in [
            ("raw.bin", raw_data.as_slice()),
            ("stream.zst", zstd_data.as_slice()),
            ("filtered.bin", filtered_data.as_slice()),
            ("bitshuffle.bin", bitshuffle_data.as_slice()),
            ("seekable.bin", seekable_data.as_slice()),
        ] {
            let stats = src
                .transfer_file_to(0, name, &dst, dst_sub, name, TransferOptions::default())
                .unwrap_or_else(|e| panic!("transfer {name} failed: {e}"));
            assert_ne!(
                stats.method,
                TransferMethod::LogicalReencode,
                "unencrypted transfer of {name} must use raw/copy_file_range/reflink"
            );
            assert_eq!(stats.logical_bytes, expected.len() as u64);
            let got = dst.read_data(stats.dst_inode).expect("read transferred");
            assert_eq!(got, expected, "content mismatch after transferring {name}");
        }

        // Sparse hole-skipping check.
        let used_before_sparse_only = dst.analyze_fragmentation().unwrap().used_blocks;
        let sparse_stats = src
            .transfer_file_to(
                0,
                "sparse.bin",
                &dst,
                dst_sub,
                "sparse.bin",
                TransferOptions::default(),
            )
            .expect("transfer sparse");
        let used_after_sparse = dst.analyze_fragmentation().unwrap().used_blocks;
        assert_ne!(sparse_stats.method, TransferMethod::LogicalReencode);
        assert_eq!(sparse_stats.logical_bytes, 8 * 1024 * 1024);
        assert!(
            sparse_stats.physical_bytes <= 16 * 1024,
            "sparse holes must be skipped, got physical_bytes={}",
            sparse_stats.physical_bytes
        );
        assert!(
            used_after_sparse - used_before_sparse_only <= 16,
            "sparse transfer must not materialize 2048 hole blocks (grew by {})",
            used_after_sparse - used_before_sparse_only
        );
        assert!(used_after_sparse >= used_before_sparse);
        assert_eq!(
            dst.read_data(sparse_stats.dst_inode).unwrap(),
            sparse_expected
        );

        // Verify Zstd stream file stayed compressed (compressed_size preserved).
        let src_zst_inode = src
            .read_inode(src.lookup(0, "stream.zst").unwrap())
            .unwrap();
        let dst_zst_inode = dst
            .read_inode(dst.lookup(dst_sub, "stream.zst").unwrap())
            .unwrap();
        assert_eq!(dst_zst_inode.compressed_size, src_zst_inode.compressed_size);
        assert!(dst_zst_inode.compressed_size > 0 && dst_zst_inode.compressed_size < 4096);

        // Verify SIMD filter flags preserved on destination inodes.
        let dst_filt_inode = dst
            .read_inode(dst.lookup(dst_sub, "filtered.bin").unwrap())
            .unwrap();
        assert_eq!(dst_filt_inode.filter_typesize, 4);
        assert!(dst_filt_inode.filter_delta);
        assert!(dst_filt_inode.filter_shuffle);

        let dst_bshuf_inode = dst
            .read_inode(dst.lookup(dst_sub, "bitshuffle.bin").unwrap())
            .unwrap();
        assert_eq!(dst_bshuf_inode.filter_typesize, 4);
        assert!(dst_bshuf_inode.filter_bitshuffle);

        // Verify Seekable 64K random `read_at` works on transferred file.
        let dst_seek_ino = dst.lookup(dst_sub, "seekable.bin").unwrap();
        let mut slice = vec![0u8; 4096];
        let n = dst
            .read_at(dst_seek_ino, 130 * 1024, &mut slice)
            .expect("read_at seekable");
        assert_eq!(n, 4096);
        assert_eq!(&slice[..], &seekable_data[130 * 1024..134 * 1024]);

        // Overwrite rejection when overwrite == false, and replacement when overwrite == true.
        let err = src
            .transfer_file_to(
                0,
                "raw.bin",
                &dst,
                dst_sub,
                "raw.bin",
                TransferOptions {
                    move_file: false,
                    overwrite: false,
                },
            )
            .unwrap_err();
        assert!(matches!(
            err,
            DiskManagerError::Io(ref io_err) if io_err.kind() == std::io::ErrorKind::AlreadyExists
        ));

        let move_stats = src
            .transfer_file_to(
                0,
                "raw.bin",
                &dst,
                dst_sub,
                "raw.bin",
                TransferOptions {
                    move_file: true,
                    overwrite: true,
                },
            )
            .expect("move with overwrite");
        assert!(
            src.lookup(0, "raw.bin").is_err(),
            "moved source must be deleted"
        );
        assert_eq!(dst.read_data(move_stats.dst_inode).unwrap(), raw_data);

        // Both images must pass `fsck` with 0 errors.
        src.flush().expect("flush src");
        dst.flush().expect("flush dst");
        assert!(
            src.verify_integrity().unwrap().is_clean,
            "src fsck failed for ({src_mode:?}, {dst_mode:?}, journaled={journaled})"
        );
        assert!(
            dst.verify_integrity().unwrap().is_clean,
            "dst fsck failed for ({src_mode:?}, {dst_mode:?}, journaled={journaled})"
        );
    }
}

#[test]
fn test_transfer_encrypted_boundary_all_directions_and_read_only() {
    let dir = tempdir().expect("tempdir");
    let enc_a_img = dir.path().join("enc_a.img");
    let enc_b_img = dir.path().join("enc_b.img");
    let plain_img = dir.path().join("plain.img");

    let enc_a = DiskManager::open_with_options(
        &enc_a_img,
        16 * 1024 * 1024,
        Some("enc-pass-a-123!"),
        true,
        StorageMode::FileIo,
    )
    .expect("open enc_a");
    let enc_b = DiskManager::open_with_options(
        &enc_b_img,
        16 * 1024 * 1024,
        Some("enc-pass-b-456!"),
        true,
        StorageMode::Mmap,
    )
    .expect("open enc_b");
    let plain = DiskManager::open_with_options(
        &plain_img,
        16 * 1024 * 1024,
        None,
        true,
        StorageMode::FileIo,
    )
    .expect("open plain");

    let secret = b"secret cross-image payload with enough length to verify encryption boundary";
    enc_a
        .create_file_with_data(0, "secret.txt", secret, CompressionMode::Always)
        .expect("create secret");

    // 1. Encrypted -> Plain must force LogicalReencode.
    let s1 = enc_a
        .transfer_file_to(
            0,
            "secret.txt",
            &plain,
            0,
            "exported.txt",
            TransferOptions::default(),
        )
        .expect("transfer enc -> plain");
    assert_eq!(s1.method, TransferMethod::LogicalReencode);
    assert_eq!(plain.read_data(s1.dst_inode).unwrap(), secret);

    // 2. Plain -> Encrypted must force LogicalReencode.
    let s2 = plain
        .transfer_file_to(
            0,
            "exported.txt",
            &enc_b,
            0,
            "imported.txt",
            TransferOptions::default(),
        )
        .expect("transfer plain -> enc");
    assert_eq!(s2.method, TransferMethod::LogicalReencode);
    assert_eq!(enc_b.read_data(s2.dst_inode).unwrap(), secret);

    // 3. Encrypted -> Encrypted must force LogicalReencode (independent salts + inode-bound AAD).
    let s3 = enc_a
        .transfer_file_to(
            0,
            "secret.txt",
            &enc_b,
            0,
            "reencrypted.txt",
            TransferOptions::default(),
        )
        .expect("transfer enc -> enc");
    assert_eq!(s3.method, TransferMethod::LogicalReencode);
    assert_eq!(enc_b.read_data(s3.dst_inode).unwrap(), secret);

    assert!(enc_a.verify_integrity().unwrap().is_clean);
    assert!(enc_b.verify_integrity().unwrap().is_clean);
    assert!(plain.verify_integrity().unwrap().is_clean);
    plain.flush().expect("flush plain");
    drop(plain);

    // Read-only source: copy succeeds and leaves source 100% immutable; move fails with ReadOnly.
    let bytes_before = std::fs::read(&plain_img).expect("snapshot before");
    let plain_ro = DiskManager::open_read_only(&plain_img).expect("open plain ro");
    let dst2_img = dir.path().join("dst2.img");
    let dst2 = DiskManager::open(&dst2_img, 16 * 1024 * 1024).expect("open dst2");

    let ro_copy = plain_ro
        .transfer_file_to(
            0,
            "exported.txt",
            &dst2,
            0,
            "copied.txt",
            TransferOptions::default(),
        )
        .expect("copy from read-only source must succeed");
    assert_eq!(dst2.read_data(ro_copy.dst_inode).unwrap(), secret);

    let ro_move = plain_ro.transfer_file_to(
        0,
        "exported.txt",
        &dst2,
        0,
        "moved.txt",
        TransferOptions {
            move_file: true,
            overwrite: false,
        },
    );
    assert!(matches!(ro_move, Err(DiskManagerError::ReadOnly)));

    // Read-only destination rejects transfer with ReadOnly.
    let to_ro = dst2.transfer_file_to(
        0,
        "copied.txt",
        &plain_ro,
        0,
        "nope.txt",
        TransferOptions::default(),
    );
    assert!(matches!(to_ro, Err(DiskManagerError::ReadOnly)));

    drop(plain_ro);
    assert_eq!(
        bytes_before,
        std::fs::read(&plain_img).expect("snapshot after"),
        "read-only source image must remain 100% byte-for-byte immutable"
    );
}

#[test]
fn test_transfer_fragmented_destination_greedy_extent_splitting() {
    let dir = tempdir().expect("tempdir");
    let src_img = dir.path().join("src_frag.img");
    let dst_img = dir.path().join("dst_frag.img");

    let src =
        DiskManager::open_with_options(&src_img, 8 * 1024 * 1024, None, true, StorageMode::Mmap)
            .expect("open src");
    let dst =
        DiskManager::open_with_options(&dst_img, 4 * 1024 * 1024, None, true, StorageMode::FileIo)
            .expect("open dst");

    // Fill destination with small 2-block (8 KiB) files until nearly full, then delete every
    // odd file so free space consists strictly of 2-block holes.
    let chunk_8k = vec![0xEEu8; 8 * 1024];
    let mut created_names = Vec::new();
    for i in 0..500 {
        let name = format!("pad_{i:04}.bin");
        match dst.create_file_with_data(0, &name, &chunk_8k, CompressionMode::Never) {
            Ok(_) => created_names.push(name),
            Err(_) => break,
        }
    }
    assert!(
        created_names.len() >= 40,
        "expected enough padding files to fragment dst"
    );
    for (i, name) in created_names.iter().enumerate() {
        if i % 2 == 1 {
            dst.delete_file(0, name).expect("delete odd pad");
        }
    }

    // Create a contiguous 128 KiB (32-block) file in src and transfer it to fragmented dst.
    let payload: Vec<u8> = (0..(128 * 1024))
        .map(|i| ((i * 73 + 19) & 0xFF) as u8)
        .collect();
    src.create_file_with_data(0, "big.bin", &payload, CompressionMode::Never)
        .expect("create big.bin");

    let stats = src
        .transfer_file_to(
            0,
            "big.bin",
            &dst,
            0,
            "big_split.bin",
            TransferOptions::default(),
        )
        .expect("transfer into fragmented dst");
    assert_ne!(stats.method, TransferMethod::LogicalReencode);
    assert!(
        stats.extents_transferred > 1,
        "expected greedy extent splitting into >1 extent, got {}",
        stats.extents_transferred
    );
    assert_eq!(dst.read_data(stats.dst_inode).unwrap(), payload);
    assert!(src.verify_integrity().unwrap().is_clean);
    assert!(dst.verify_integrity().unwrap().is_clean);
}

#[test]
fn test_transfer_same_image_move_and_copy() {
    let dir = tempdir().expect("tempdir");
    let img = dir.path().join("same.img");
    let dm = DiskManager::open_with_options(&img, 16 * 1024 * 1024, None, true, StorageMode::Mmap)
        .expect("open dm");

    let sub = dm.create_directory(0, "sub").expect("mkdir sub");
    let payload = b"same-image transfer payload".repeat(200);
    let orig_ino = dm
        .create_file_with_data(0, "orig.bin", &payload, CompressionMode::Always)
        .expect("create orig");

    // Same-image copy creates an independent inode.
    let copy_stats = dm
        .transfer_file_to(
            0,
            "orig.bin",
            &dm,
            sub,
            "copy.bin",
            TransferOptions::default(),
        )
        .expect("same-image copy");
    assert_ne!(copy_stats.dst_inode, orig_ino);
    assert_eq!(dm.read_data(copy_stats.dst_inode).unwrap(), payload);

    // Same-image move with existing dest & overwrite=false -> AlreadyExists.
    let err = dm
        .transfer_file_to(
            0,
            "orig.bin",
            &dm,
            sub,
            "copy.bin",
            TransferOptions {
                move_file: true,
                overwrite: false,
            },
        )
        .unwrap_err();
    assert!(matches!(
        err,
        DiskManagerError::Io(ref io_err) if io_err.kind() == std::io::ErrorKind::AlreadyExists
    ));

    // Same-image move performs O(1) metadata-only rename keeping inode ID.
    let move_stats = dm
        .transfer_file_to(
            0,
            "orig.bin",
            &dm,
            sub,
            "moved.bin",
            TransferOptions {
                move_file: true,
                overwrite: false,
            },
        )
        .expect("same-image move");
    assert_eq!(move_stats.src_inode, orig_ino);
    assert_eq!(move_stats.dst_inode, orig_ino);
    assert_eq!(move_stats.extents_transferred, 0);
    assert!(dm.lookup(0, "orig.bin").is_err());
    assert_eq!(dm.read_data(orig_ino).unwrap(), payload);
    assert!(dm.verify_integrity().unwrap().is_clean);
}

#[test]
fn test_cli_clone_and_transfer_smoke() {
    let bin = env!("CARGO_BIN_EXE_oifs");
    let dir = tempdir().expect("tempdir");
    let src_img = dir.path().join("cli_src.img");
    let dst_img = dir.path().join("cli_dst.img");
    let cloned_img = dir.path().join("cli_cloned.img");
    let sample_file = dir.path().join("sample.bin");

    let payload: Vec<u8> = (0..128 * 1024).map(|i| (i & 0xFF) as u8).collect();
    std::fs::write(&sample_file, &payload).expect("write sample");

    for img in [&src_img, &dst_img] {
        let out = Command::new(bin)
            .args(["--image", img.to_str().unwrap(), "create", "--size", "16"])
            .output()
            .expect("oifs create");
        assert!(out.status.success(), "{:?}", out);
    }

    let out = Command::new(bin)
        .args([
            "--image",
            src_img.to_str().unwrap(),
            "put",
            sample_file.to_str().unwrap(),
            "sample.bin",
        ])
        .output()
        .expect("oifs put");
    assert!(out.status.success(), "{:?}", out);

    // CLI whole-image clone (without needing `--image`).
    let out = Command::new(bin)
        .args([
            "clone",
            src_img.to_str().unwrap(),
            cloned_img.to_str().unwrap(),
        ])
        .output()
        .expect("oifs clone");
    assert!(out.status.success(), "{:?}", out);
    {
        let cloned = DiskManager::open_read_only(&cloned_img).expect("open cloned");
        let ino = cloned.lookup(0, "sample.bin").expect("lookup in cloned");
        assert_eq!(cloned.read_data(ino).unwrap(), payload);
        assert!(cloned.verify_integrity().unwrap().is_clean);
    }

    // CLI cross-image transfer (copy + json).
    let out = Command::new(bin)
        .args([
            "--json",
            "--image",
            src_img.to_str().unwrap(),
            "transfer",
            "--to",
            dst_img.to_str().unwrap(),
            "sample.bin",
            "--dest",
            "copied.bin",
        ])
        .output()
        .expect("oifs transfer copy");
    assert!(out.status.success(), "{:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"ok\":true"), "unexpected json: {stdout}");

    // CLI cross-image sub-range splice with `--trim-mode punch-hole`.
    let out = Command::new(bin)
        .args([
            "--image",
            src_img.to_str().unwrap(),
            "transfer",
            "--to",
            dst_img.to_str().unwrap(),
            "sample.bin",
            "--dest",
            "spliced.bin",
            "--offset",
            "4096",
            "--len",
            "16384",
            "--dst-offset",
            "0",
            "--trim-mode",
            "punch-hole",
        ])
        .output()
        .expect("oifs transfer splice");
    assert!(out.status.success(), "{:?}", out);

    // CLI cross-image move with overwrite.
    let out = Command::new(bin)
        .args([
            "--image",
            src_img.to_str().unwrap(),
            "transfer",
            "--to",
            dst_img.to_str().unwrap(),
            "sample.bin",
            "--dest",
            "copied.bin",
            "--move",
            "--overwrite",
        ])
        .output()
        .expect("oifs transfer move");
    assert!(out.status.success(), "{:?}", out);

    let src_ro = DiskManager::open_read_only(&src_img).expect("open src ro");
    let dst_ro = DiskManager::open_read_only(&dst_img).expect("open dst ro");
    assert!(src_ro.lookup(0, "sample.bin").is_err());
    let spliced_ino = dst_ro.lookup(0, "spliced.bin").expect("lookup spliced");
    assert_eq!(
        dst_ro.read_data(spliced_ino).unwrap(),
        &payload[4096..20480]
    );
    let copied_ino = dst_ro.lookup(0, "copied.bin").expect("lookup copied");
    let copied_data = dst_ro.read_data(copied_ino).unwrap();
    assert_eq!(copied_data.len(), payload.len());
    // The 16 KiB range [4096..20480] was punched to zero prior to `--move`.
    assert!(copied_data[4096..20480].iter().all(|&b| b == 0));
    assert!(src_ro.verify_integrity().unwrap().is_clean);
    assert!(dst_ro.verify_integrity().unwrap().is_clean);
}

// ============================================================================
// 3. Cross-Image Sub-Range Splice / Move Regression Tests
// ============================================================================

#[test]
fn test_splice_raw_sandwich_head_body_tail_punch_hole_and_collapse() {
    for (mode, journaled) in [
        (StorageMode::Mmap, true),
        (StorageMode::FileIo, true),
        (StorageMode::Mmap, false),
        (StorageMode::FileIo, false),
    ] {
        let dir = tempdir().expect("tempdir");
        let src_img = dir.path().join("src_splice.img");
        let dst_img = dir.path().join("dst_splice.img");

        let src = DiskManager::open_with_options(&src_img, 16 * 1024 * 1024, None, journaled, mode)
            .expect("open src");
        let dst = DiskManager::open_with_options(&dst_img, 16 * 1024 * 1024, None, journaled, mode)
            .expect("open dst");

        // 1. Unaligned sandwich split (Head + Body + Tail) on raw file.
        let src_bytes: Vec<u8> = (0..64 * 1024)
            .map(|i| ((i * 31 + 7) & 0xFF) as u8)
            .collect();
        let src_ino = src
            .create_file_with_data(0, "raw_src.bin", &src_bytes, CompressionMode::Never)
            .expect("create src raw");
        let dst_seed = vec![0xBBu8; 64 * 1024];
        let dst_ino = dst
            .create_file_with_data(0, "raw_dst.bin", &dst_seed, CompressionMode::Never)
            .expect("create dst raw");

        let off = 1000u64;
        let len = 32 * 1024 + 2000; // head=3096, body=28672 (7 blocks), tail=2000 -> wait: 34768 total
        let stats = src
            .splice_range_to(
                src_ino,
                off,
                &dst,
                dst_ino,
                off,
                len,
                SpliceOptions {
                    trim_mode: SpliceTrimMode::Keep,
                },
            )
            .expect("sandwich splice");
        assert_eq!(stats.bytes_spliced, len);
        assert!(stats.logical_rmw_bytes > 0);
        assert!(stats.body_fast_path_bytes >= 28 * 1024);
        assert_ne!(stats.method, TransferMethod::LogicalReencode);

        let mut expected_dst = dst_seed.clone();
        expected_dst[off as usize..(off + len) as usize]
            .copy_from_slice(&src_bytes[off as usize..(off + len) as usize]);
        assert_eq!(dst.read_data(dst_ino).unwrap(), expected_dst);
        assert_eq!(src.read_data(src_ino).unwrap(), src_bytes);

        // 2. PunchHole on aligned raw range keeps size, zeroes range, and frees blocks.
        let punch_dst = dst
            .create_file(0, "punched_dst.bin")
            .expect("create punch dst");
        let used_before_punch = src.analyze_fragmentation().unwrap().used_blocks;
        let punch_stats = src
            .splice_range_to(
                src_ino,
                8192,
                &dst,
                punch_dst,
                0,
                24 * 1024,
                SpliceOptions {
                    trim_mode: SpliceTrimMode::PunchHole,
                },
            )
            .expect("punch hole splice");
        let used_after_punch = src.analyze_fragmentation().unwrap().used_blocks;
        assert_eq!(punch_stats.src_blocks_freed, 6);
        assert!(used_after_punch < used_before_punch);
        assert_eq!(
            dst.read_data(punch_dst).unwrap(),
            &src_bytes[8192..8192 + 24 * 1024]
        );

        let src_after_punch = src.read_data(src_ino).unwrap();
        assert_eq!(src_after_punch.len(), src_bytes.len());
        assert_eq!(&src_after_punch[..8192], &src_bytes[..8192]);
        assert!(
            src_after_punch[8192..8192 + 24 * 1024]
                .iter()
                .all(|&b| b == 0)
        );
        assert_eq!(
            &src_after_punch[8192 + 24 * 1024..],
            &src_bytes[8192 + 24 * 1024..]
        );

        // 3. CollapseRange on aligned raw range removes slice and shifts trailing pointers.
        let collapse_src_data: Vec<u8> = (0..48 * 1024)
            .map(|i| ((i * 59 + 3) & 0xFF) as u8)
            .collect();
        let col_src_ino = src
            .create_file_with_data(0, "col_src.bin", &collapse_src_data, CompressionMode::Never)
            .expect("create col src");
        let col_dst_ino = dst.create_file(0, "col_dst.bin").expect("create col dst");

        let col_stats = src
            .splice_range_to(
                col_src_ino,
                8192,
                &dst,
                col_dst_ino,
                0,
                16 * 1024,
                SpliceOptions {
                    trim_mode: SpliceTrimMode::CollapseRange,
                },
            )
            .expect("collapse range splice");
        assert_eq!(
            col_stats.src_blocks_freed, 5,
            "4 data blocks + 1 single-indirect pointer block freed when shrinking 12 -> 8 blocks"
        );
        assert_eq!(
            dst.read_data(col_dst_ino).unwrap(),
            &collapse_src_data[8192..24 * 1024]
        );
        let mut expected_collapsed = collapse_src_data[..8192].to_vec();
        expected_collapsed.extend_from_slice(&collapse_src_data[24 * 1024..]);
        assert_eq!(src.read_data(col_src_ino).unwrap(), expected_collapsed);

        assert!(src.verify_integrity().unwrap().is_clean);
        assert!(dst.verify_integrity().unwrap().is_clean);
    }
}

#[test]
fn test_splice_seekable_64k_collapse_filter_guard_stream_and_encrypted_fallback() {
    let dir = tempdir().expect("tempdir");
    let src_img = dir.path().join("src_seek.img");
    let dst_img = dir.path().join("dst_seek.img");
    let enc_img = dir.path().join("enc_seek.img");

    let src =
        DiskManager::open_with_options(&src_img, 32 * 1024 * 1024, None, true, StorageMode::Mmap)
            .expect("open src");
    let dst =
        DiskManager::open_with_options(&dst_img, 32 * 1024 * 1024, None, true, StorageMode::FileIo)
            .expect("open dst");
    let enc = DiskManager::open_with_options(
        &enc_img,
        16 * 1024 * 1024,
        Some("splice-enc-pass!"),
        true,
        StorageMode::Mmap,
    )
    .expect("open enc");

    // 5 x 64 KiB monotonic u32 chunks with Delta+Shuffle filters.
    let mut seek_data = Vec::with_capacity(5 * CHUNK_SIZE_64K);
    for v in 0u32..(5 * CHUNK_SIZE_64K as u32 / 4) {
        seek_data.extend_from_slice(&v.to_le_bytes());
    }
    let filters = FilterConfig::numeric(4);
    let seek_mode = CompressionMode::Seekable {
        chunk_size: CHUNK_SIZE_64K as u32,
        level: 1,
    };

    let src_ino = src
        .create_file_with_data_with_filters(0, "seek_src.bin", &seek_data, seek_mode, filters)
        .expect("create seek src");
    let dst_seed = vec![0u8; 4 * CHUNK_SIZE_64K];
    let dst_matching_ino = dst
        .create_file_with_data_with_filters(0, "seek_dst.bin", &dst_seed, seek_mode, filters)
        .expect("create seek dst matching");

    // 1. Aligned Seekable 64K + matching filters + CollapseRange -> fast path, zero RMW.
    let off = CHUNK_SIZE_64K as u64;
    let len = (3 * CHUNK_SIZE_64K) as u64;
    let stats = src
        .splice_range_to(
            src_ino,
            off,
            &dst,
            dst_matching_ino,
            off,
            len,
            SpliceOptions {
                trim_mode: SpliceTrimMode::CollapseRange,
            },
        )
        .expect("seekable splice collapse");
    assert_eq!(stats.logical_rmw_bytes, 0);
    assert_eq!(stats.body_fast_path_bytes, len);
    assert_ne!(stats.method, TransferMethod::LogicalReencode);
    assert!(stats.src_blocks_freed > 0);

    let mut expected_dst = dst_seed.clone();
    expected_dst[off as usize..(off + len) as usize]
        .copy_from_slice(&seek_data[off as usize..(off + len) as usize]);
    assert_eq!(dst.read_data(dst_matching_ino).unwrap(), expected_dst);

    let mut expected_src_after = seek_data[..CHUNK_SIZE_64K].to_vec();
    expected_src_after.extend_from_slice(&seek_data[4 * CHUNK_SIZE_64K..]);
    assert_eq!(src.read_data(src_ino).unwrap(), expected_src_after);

    // 2. Mode mismatch (Seekable 64K -> Raw uncompressed) forces LogicalReencode.
    let dst_mismatch_ino = dst
        .create_file_with_data(0, "raw_mismatch.bin", &dst_seed, CompressionMode::Never)
        .expect("create raw dst mismatch");
    let mismatch_stats = src
        .splice_range_to(
            src_ino,
            0,
            &dst,
            dst_mismatch_ino,
            0,
            CHUNK_SIZE_64K as u64,
            SpliceOptions::default(),
        )
        .expect("mismatch mode splice");
    assert_eq!(mismatch_stats.method, TransferMethod::LogicalReencode);
    assert_eq!(
        &dst.read_data(dst_mismatch_ino).unwrap()[..CHUNK_SIZE_64K],
        &expected_src_after[..CHUNK_SIZE_64K]
    );

    // 3. Stream-compressed source forces LogicalReencode.
    let stream_data = vec![0x5Au8; 64 * 1024];
    let stream_ino = src
        .create_file_with_data(0, "stream.zst", &stream_data, CompressionMode::Always)
        .expect("create stream");
    let stream_dst_ino = dst
        .create_file(0, "stream_dst.bin")
        .expect("create stream dst");
    let stream_stats = src
        .splice_range_to(
            stream_ino,
            4096,
            &dst,
            stream_dst_ino,
            0,
            16 * 1024,
            SpliceOptions::default(),
        )
        .expect("stream fallback splice");
    assert_eq!(stream_stats.method, TransferMethod::LogicalReencode);
    assert_eq!(
        dst.read_data(stream_dst_ino).unwrap(),
        &stream_data[4096..20 * 1024]
    );

    // 4. Encrypted boundary forces LogicalReencode.
    let enc_ino = enc.create_file(0, "enc_dst.bin").expect("create enc dst");
    let enc_stats = src
        .splice_range_to(
            src_ino,
            0,
            &enc,
            enc_ino,
            0,
            16 * 1024,
            SpliceOptions::default(),
        )
        .expect("encrypted splice");
    assert_eq!(enc_stats.method, TransferMethod::LogicalReencode);
    assert_eq!(
        enc.read_data(enc_ino).unwrap(),
        &expected_src_after[..16 * 1024]
    );

    // 5. Read-only protection on source with PunchHole / CollapseRange.
    src.flush().expect("flush src");
    let src_ro = DiskManager::open_read_only(&src_img).expect("open src ro");
    let ro_trim = src_ro.splice_range_to(
        src_ino,
        0,
        &dst,
        stream_dst_ino,
        0,
        4096,
        SpliceOptions {
            trim_mode: SpliceTrimMode::PunchHole,
        },
    );
    assert!(matches!(ro_trim, Err(DiskManagerError::ReadOnly)));
    drop(src_ro);

    assert!(src.verify_integrity().unwrap().is_clean);
    assert!(dst.verify_integrity().unwrap().is_clean);
    assert!(enc.verify_integrity().unwrap().is_clean);
}

// ============================================================================
// 4. Concurrent AB-BA Deadlock Regression Test
// ============================================================================

#[test]
fn test_transfer_concurrent_ab_ba_deadlock_regression() {
    let dir = tempdir().expect("tempdir");
    let a_img = dir.path().join("a_concurrent.img");
    let b_img = dir.path().join("b_concurrent.img");

    let a = Arc::new(
        DiskManager::open_with_options(&a_img, 64 * 1024 * 1024, None, true, StorageMode::Mmap)
            .expect("open A"),
    );
    let b = Arc::new(
        DiskManager::open_with_options(&b_img, 64 * 1024 * 1024, None, true, StorageMode::FileIo)
            .expect("open B"),
    );

    let mut expected_from_a = Vec::new();
    let mut expected_from_b = Vec::new();
    for i in 0..16 {
        let data_a: Vec<u8> = (0..128 * 1024)
            .map(|k| ((k + i * 17) & 0xFF) as u8)
            .collect();
        let mode = if i % 3 == 0 {
            CompressionMode::Seekable {
                chunk_size: CHUNK_SIZE_64K as u32,
                level: 1,
            }
        } else if i % 3 == 1 {
            CompressionMode::Always
        } else {
            CompressionMode::Never
        };
        a.create_file_with_data(0, &format!("from_a_{i}.bin"), &data_a, mode)
            .expect("seed A");
        expected_from_a.push(data_a);

        let data_b: Vec<u8> = (0..128 * 1024)
            .map(|k| ((k * 3 + i * 29) & 0xFF) as u8)
            .collect();
        b.create_file_with_data(0, &format!("from_b_{i}.bin"), &data_b, mode)
            .expect("seed B");
        expected_from_b.push(data_b);
    }

    let mut handles = Vec::new();
    for tid in 0..4 {
        let a_ref = Arc::clone(&a);
        let b_ref = Arc::clone(&b);
        handles.push(std::thread::spawn(move || {
            for iter in 0..4 {
                let idx = tid * 4 + iter;
                let src_name = format!("from_a_{idx}.bin");
                let dst_name = format!("to_b_{idx}.bin");
                a_ref
                    .transfer_file_to(
                        0,
                        &src_name,
                        &b_ref,
                        0,
                        &dst_name,
                        TransferOptions::default(),
                    )
                    .expect("A -> B transfer");
            }
        }));
    }
    for tid in 0..4 {
        let a_ref = Arc::clone(&a);
        let b_ref = Arc::clone(&b);
        handles.push(std::thread::spawn(move || {
            for iter in 0..4 {
                let idx = tid * 4 + iter;
                let src_name = format!("from_b_{idx}.bin");
                let dst_name = format!("to_a_{idx}.bin");
                b_ref
                    .transfer_file_to(
                        0,
                        &src_name,
                        &a_ref,
                        0,
                        &dst_name,
                        TransferOptions::default(),
                    )
                    .expect("B -> A transfer");
            }
        }));
    }

    for h in handles {
        h.join().expect("worker thread panicked");
    }

    for (i, expected) in expected_from_a.iter().enumerate() {
        let ino = b.lookup(0, &format!("to_b_{i}.bin")).expect("lookup to_b");
        assert_eq!(b.read_data(ino).unwrap(), *expected);
    }
    for (i, expected) in expected_from_b.iter().enumerate() {
        let ino = a.lookup(0, &format!("to_a_{i}.bin")).expect("lookup to_a");
        assert_eq!(a.read_data(ino).unwrap(), *expected);
    }

    a.flush().expect("flush A");
    b.flush().expect("flush B");
    assert!(a.verify_integrity().unwrap().is_clean);
    assert!(b.verify_integrity().unwrap().is_clean);
}

// ============================================================================
// 5. Before / After Performance Benchmark (#[ignore])
// ============================================================================

#[test]
#[ignore]
fn bench_cross_image_transfer_performance() {
    let dir = tempdir().expect("tempdir");
    let src_img = dir.path().join("bench_src.img");
    let dst_img = dir.path().join("bench_dst.img");

    let src =
        DiskManager::open_with_options(&src_img, 256 * 1024 * 1024, None, true, StorageMode::Mmap)
            .expect("open bench src");
    let dst =
        DiskManager::open_with_options(&dst_img, 256 * 1024 * 1024, None, true, StorageMode::Mmap)
            .expect("open bench dst");

    struct Row {
        label: &'static str,
        logical_mb: f64,
        before_ms: f64,
        after_ms: f64,
        method: String,
    }
    let mut rows = Vec::new();

    // 1. 16 MiB Zstd-compressed file.
    let zstd_16m: Vec<u8> = (0..(16 * 1024 * 1024))
        .map(|i| ((i / 64) & 0xFF) as u8)
        .collect();
    let zstd_ino = src
        .create_file_with_data(0, "zstd_16m.bin", &zstd_16m, CompressionMode::Always)
        .expect("seed zstd 16m");
    src.flush().unwrap();

    let t0 = Instant::now();
    let data = src.read_data(zstd_ino).unwrap();
    dst.create_file_with_data(0, "old_zstd.bin", &data, CompressionMode::Always)
        .unwrap();
    let before_ms = t0.elapsed().as_secs_f64() * 1000.0;
    dst.flush().unwrap();

    let t1 = Instant::now();
    let stats = src
        .transfer_file_to(
            0,
            "zstd_16m.bin",
            &dst,
            0,
            "new_zstd.bin",
            TransferOptions::default(),
        )
        .unwrap();
    let after_ms = t1.elapsed().as_secs_f64() * 1000.0;
    rows.push(Row {
        label: "16 MiB Zstd stream",
        logical_mb: 16.0,
        before_ms,
        after_ms,
        method: format!("{:?}", stats.method),
    });

    // 2. 16 MiB SIMD-filtered (Delta + Shuffle) + Zstd file.
    let mut filt_16m = Vec::with_capacity(16 * 1024 * 1024);
    for v in 0u32..(4 * 1024 * 1024) {
        filt_16m.extend_from_slice(&v.to_le_bytes());
    }
    let filters = FilterConfig::numeric(4);
    let filt_ino = src
        .create_file_with_data_with_filters(
            0,
            "filt_16m.bin",
            &filt_16m,
            CompressionMode::Always,
            filters,
        )
        .unwrap();
    src.flush().unwrap();

    let t0 = Instant::now();
    let data = src.read_data(filt_ino).unwrap();
    dst.create_file_with_data_with_filters(
        0,
        "old_filt.bin",
        &data,
        CompressionMode::Always,
        filters,
    )
    .unwrap();
    let before_ms = t0.elapsed().as_secs_f64() * 1000.0;
    dst.flush().unwrap();

    let t1 = Instant::now();
    let stats = src
        .transfer_file_to(
            0,
            "filt_16m.bin",
            &dst,
            0,
            "new_filt.bin",
            TransferOptions::default(),
        )
        .unwrap();
    let after_ms = t1.elapsed().as_secs_f64() * 1000.0;
    rows.push(Row {
        label: "16 MiB Delta+Shuffle+Zstd",
        logical_mb: 16.0,
        before_ms,
        after_ms,
        method: format!("{:?}", stats.method),
    });

    // 3. 16 MiB Seekable 64K compressed file.
    let seek_mode = CompressionMode::Seekable {
        chunk_size: CHUNK_SIZE_64K as u32,
        level: 3,
    };
    let seek_ino = src
        .create_file_with_data(0, "seek_16m.bin", &zstd_16m, seek_mode)
        .unwrap();
    src.flush().unwrap();

    let t0 = Instant::now();
    let data = src.read_data(seek_ino).unwrap();
    dst.create_file_with_data(0, "old_seek.bin", &data, seek_mode)
        .unwrap();
    let before_ms = t0.elapsed().as_secs_f64() * 1000.0;
    dst.flush().unwrap();

    let t1 = Instant::now();
    let stats = src
        .transfer_file_to(
            0,
            "seek_16m.bin",
            &dst,
            0,
            "new_seek.bin",
            TransferOptions::default(),
        )
        .unwrap();
    let after_ms = t1.elapsed().as_secs_f64() * 1000.0;
    rows.push(Row {
        label: "16 MiB Seekable 64K",
        logical_mb: 16.0,
        before_ms,
        after_ms,
        method: format!("{:?}", stats.method),
    });

    // 4. 64 MiB Sparse file (only 64 KiB allocated).
    let sparse_ino = src.create_file(0, "sparse_64m.bin").unwrap();
    let active = vec![0xABu8; 32 * 1024];
    src.write_data(sparse_ino, 0, &active, CompressionMode::Never)
        .unwrap();
    src.write_data(
        sparse_ino,
        64 * 1024 * 1024 - 32 * 1024,
        &active,
        CompressionMode::Never,
    )
    .unwrap();
    src.flush().unwrap();

    let t0 = Instant::now();
    let data = src.read_data(sparse_ino).unwrap();
    let old_sparse_ino = dst.create_file(0, "old_sparse.bin").unwrap();
    for (idx, chunk) in data.chunks(16 * 1024 * 1024).enumerate() {
        dst.write_data(
            old_sparse_ino,
            (idx * 16 * 1024 * 1024) as u64,
            chunk,
            CompressionMode::Never,
        )
        .unwrap();
    }
    let before_ms = t0.elapsed().as_secs_f64() * 1000.0;
    dst.flush().unwrap();

    let t1 = Instant::now();
    let stats = src
        .transfer_file_to(
            0,
            "sparse_64m.bin",
            &dst,
            0,
            "new_sparse.bin",
            TransferOptions::default(),
        )
        .unwrap();
    let after_ms = t1.elapsed().as_secs_f64() * 1000.0;
    rows.push(Row {
        label: "64 MiB Sparse (64 KiB alloc)",
        logical_mb: 64.0,
        before_ms,
        after_ms,
        method: format!("{:?}", stats.method),
    });

    // 5. 8 MiB Seekable 64K sub-range splice (`CollapseRange`) vs logical RMW.
    let splice_seed = vec![0x44u8; 16 * 1024 * 1024];
    let sp_old_src = src
        .create_file_with_data(0, "sp_old_src.bin", &splice_seed, seek_mode)
        .unwrap();
    let sp_old_dst = dst
        .create_file_with_data(0, "sp_old_dst.bin", &splice_seed, seek_mode)
        .unwrap();
    let sp_new_src = src
        .create_file_with_data(0, "sp_new_src.bin", &splice_seed, seek_mode)
        .unwrap();
    let sp_new_dst = dst
        .create_file_with_data(0, "sp_new_dst.bin", &splice_seed, seek_mode)
        .unwrap();
    src.flush().unwrap();
    dst.flush().unwrap();

    let splice_len = 8 * 1024 * 1024u64;
    let t0 = Instant::now();
    let mut sub = vec![0u8; splice_len as usize];
    src.read_at(sp_old_src, 64 * 1024, &mut sub).unwrap();
    dst.write_data(sp_old_dst, 64 * 1024, &sub, seek_mode)
        .unwrap();
    let mut full_rem = src.read_data(sp_old_src).unwrap();
    full_rem.drain(64 * 1024..(64 * 1024 + splice_len as usize));
    src.write_data(sp_old_src, 0, &full_rem, seek_mode).unwrap();
    src.truncate(sp_old_src, full_rem.len() as u64).unwrap();
    let before_ms = t0.elapsed().as_secs_f64() * 1000.0;
    src.flush().unwrap();
    dst.flush().unwrap();

    let t1 = Instant::now();
    let sp_stats = src
        .splice_range_to(
            sp_new_src,
            64 * 1024,
            &dst,
            sp_new_dst,
            64 * 1024,
            splice_len,
            SpliceOptions {
                trim_mode: SpliceTrimMode::CollapseRange,
            },
        )
        .unwrap();
    let after_ms = t1.elapsed().as_secs_f64() * 1000.0;
    rows.push(Row {
        label: "8 MiB Seekable Splice+Collapse",
        logical_mb: 8.0,
        before_ms,
        after_ms,
        method: format!("{:?}", sp_stats.method),
    });

    // 6. 64 MiB Whole-image `clone_image` vs `std::fs::copy`.
    let clone_src = dir.path().join("clone_64m_src.img");
    let clone_copy_dst = dir.path().join("clone_64m_copy.img");
    let clone_cow_dst = dir.path().join("clone_64m_cow.img");
    {
        let c_dm = DiskManager::open(&clone_src, 64 * 1024 * 1024).unwrap();
        c_dm.create_file_with_data(0, "payload.bin", &zstd_16m, CompressionMode::Never)
            .unwrap();
        c_dm.flush().unwrap();
    }

    let t0 = Instant::now();
    std::fs::copy(&clone_src, &clone_copy_dst).unwrap();
    let before_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t1 = Instant::now();
    let clone_method = DiskManager::clone_image(&clone_src, &clone_cow_dst).unwrap();
    let after_ms = t1.elapsed().as_secs_f64() * 1000.0;
    rows.push(Row {
        label: "64 MiB Whole-Image Clone",
        logical_mb: 64.0,
        before_ms,
        after_ms,
        method: format!("{clone_method:?}"),
    });

    println!(
        "\n| Benchmark Workload | Logical Re-encode / Copy (ms) | New 3-Tier / CoW (ms) | Throughput (MB/s) | Speedup | Method |"
    );
    println!("| :--- | ---: | ---: | ---: | ---: | :--- |");
    for r in &rows {
        let mb_s = r.logical_mb / (r.after_ms / 1000.0).max(1e-9);
        let speedup = r.before_ms / r.after_ms.max(1e-6);
        println!(
            "| {} | {:.2} | {:.2} | {:.1} | {:.2}x | {} |",
            r.label, r.before_ms, r.after_ms, mb_s, speedup, r.method
        );
    }
}
