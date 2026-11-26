//! Full-Image Random Bit-Flip & Hostile `.img` End-to-End Bombardment Tests.
//!
//! Validates that arbitrary single-bit and multi-byte corruptions across real OIFS images
//! (plain, WAL, encrypted, Seekable 64K, Zstd dictionary, multi-block directories, and indirect
//! trees), as well as CRC-valid forged hostile inodes, never trigger panics, OOMs, or hangs
//! across `open` -> `list_dir` -> `read_data` -> `read_at` -> `verify_integrity` -> `defragment` -> `vacuum`.

use oifs::BLOCK_SIZE;
use oifs::disk::{CompressionMode, DefragMode, DiskManager, VacuumOptions};
use oifs::filters::FilterConfig;
use oifs::inode::{FileType, INODE_FLAG_SEEKABLE_64K};
use oifs::inode_format::{INODE_SLOT_SIZE, INODE_V2_CRC32C_OFFSET, encode_v2};
use oifs::journal::crc32c;
use oifs::storage::StorageMode;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use tempfile::tempdir;

#[derive(Clone)]
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() as usize) % bound
        }
    }
}

fn build_rich_image(
    encrypted: bool,
    password: &str,
) -> (Vec<u8>, Option<Vec<u8>>, oifs::superblock::SuperBlock, u64, u64) {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("rich.img");

    let samples: Vec<Vec<u8>> = (0..16u32)
        .map(|i| {
            format!("{{\"evt\":\"bitflip\",\"worker\":{i},\"zone\":\"tw-1\",\"ok\":true}}")
                .into_bytes()
        })
        .collect();
    let dict = DiskManager::train_zstd_dict(&samples, 4096).unwrap();

    let (sb, raw_ino, seek_ino);
    {
        let dm = if encrypted {
            DiskManager::create_encrypted_journaled(&img_path, 4 * 1024 * 1024, password).unwrap()
        } else {
            DiskManager::open_journaled(&img_path, 4 * 1024 * 1024).unwrap()
        };
        dm.register_and_use_zstd_dict(&dict).unwrap();
        let root = dm.superblock().root_inode;
        let sub = dm.create_directory(root, "nested").unwrap();

        // 1. Raw file with sparse hole + indirect pointer block (> 40 KiB)
        raw_ino = dm.create_file(sub, "raw_indirect.bin").unwrap();
        dm.write_data(raw_ino, 0, &[0x11u8; 4096], CompressionMode::Never)
            .unwrap();
        dm.write_data(raw_ino, 11 * 4096, &[0x22u8; 8192], CompressionMode::Never)
            .unwrap();

        // 2. Dictionary-compressed small file
        dm.create_file_with_data(root, "dict_small.json", &samples[3], CompressionMode::Auto)
            .unwrap();

        // 3. Zstd Stream + Numeric Filter
        let nums: Vec<u8> = (0..1024u32).flat_map(|n| n.to_le_bytes()).collect();
        dm.create_file_with_data_with_filters(
            root,
            "filtered.bin",
            &nums,
            CompressionMode::Always,
            FilterConfig::numeric(4),
        )
        .unwrap();

        // 4. Seekable 64K multi-chunk file
        let seek_data: Vec<u8> = (0..(150 * 1024)).map(|i| (i % 173) as u8).collect();
        seek_ino = dm
            .create_file_with_data(
                sub,
                "seek64k.bin",
                &seek_data,
                CompressionMode::Seekable {
                    chunk_size: 65536,
                    level: 1,
                },
            )
            .unwrap();

        sb = dm.superblock();
        dm.flush().unwrap();

        // Leave a clean committed WAL transaction in the ring as well (no final flush)
        dm.create_file_with_data(root, "wal_pending.txt", b"pending_wal_tx", CompressionMode::Never)
            .unwrap();
    }

    (
        fs::read(&img_path).unwrap(),
        Some(dict),
        sb,
        raw_ino,
        seek_ino,
    )
}

fn walk_limited(dm: &DiskManager, dir_ino: u64, depth: usize, visited: &mut usize) {
    if depth == 0 || *visited >= 32 {
        return;
    }
    let Ok(entries) = dm.list_dir(dir_ino) else {
        return;
    };
    for entry in entries.into_iter().take(16) {
        if *visited >= 32 {
            break;
        }
        *visited += 1;
        let Ok(inode) = dm.read_inode(entry.inode) else {
            continue;
        };
        match inode.mode {
            FileType::Directory => {
                walk_limited(dm, entry.inode, depth - 1, visited);
            }
            FileType::File => {
                let _ = dm.read_data(entry.inode);
                let mut buf = [0u8; 4096];
                let _ = dm.read_at(entry.inode, 0, &mut buf);
                if inode.size > 512 && inode.size < (64 * 1024 * 1024) {
                    let _ = dm.read_at(entry.inode, inode.size / 2, &mut buf);
                    let _ = dm.read_at(entry.inode, inode.size - 128, &mut buf);
                }
            }
        }
    }
}

fn exercise_hostile_image_with_open<F>(open_fn: F, dict: Option<&[u8]>, label: &str)
where
    F: Fn(&std::path::Path, bool) -> Result<DiskManager, oifs::disk::DiskManagerError>,
{
    let res = catch_unwind(AssertUnwindSafe(|| {
        let dir = tempdir().unwrap();
        let ro_path = dir.path().join("hostile_ro.img");
        let rw_path = dir.path().join("hostile_rw.img");

        if let Ok(ro) = open_fn(&ro_path, true) {
            if let Some(d) = dict {
                let _ = ro.register_zstd_dict(d);
            }
            let _ = ro.analyze_fragmentation();
            let _ = ro.verify_integrity();
            let mut visited = 0;
            walk_limited(&ro, ro.superblock().root_inode, 4, &mut visited);
        }

        if let Ok(dm) = open_fn(&rw_path, false) {
            if let Some(d) = dict {
                let _ = dm.register_and_use_zstd_dict(d);
            }
            let _ = dm.analyze_fragmentation();
            let _ = dm.verify_integrity();
            let mut visited = 0;
            walk_limited(&dm, dm.superblock().root_inode, 4, &mut visited);
            if let Some(p) = dm.backing_path().to_str() {
                let path_s = p.to_string();
                let _ = dm.defragment(&path_s, DefragMode::InPlace, None);
            }
            let _ = dm.vacuum(VacuumOptions::default());
        }
    }));
    assert!(res.is_ok(), "[{label}] panicked on corrupted image!");
}

fn exercise_hostile_image(
    bytes: &[u8],
    password: Option<&str>,
    mode: StorageMode,
    dict: Option<&[u8]>,
    label: &str,
) {
    exercise_hostile_image_with_open(
        |path, is_ro| {
            fs::write(path, bytes).unwrap();
            if is_ro {
                DiskManager::open_read_only_with_storage_mode(path, password, mode)
            } else {
                DiskManager::open_with_options(path, 0, password, true, mode)
            }
        },
        dict,
        label,
    );
}

fn exercise_hostile_image_with_key(
    bytes: &[u8],
    key: Option<oifs::encryption::EncryptionKey>,
    mode: StorageMode,
    dict: Option<&[u8]>,
    label: &str,
) {
    exercise_hostile_image_with_open(
        |path, is_ro| {
            fs::write(path, bytes).unwrap();
            if is_ro {
                DiskManager::open_read_only_with_key_and_storage_mode(path, key.clone(), mode)
            } else {
                DiskManager::open_with_key_and_storage_mode(path, 0, key.clone(), mode)
            }
        },
        dict,
        label,
    );
}

#[test]
fn test_targeted_region_bitflips_plain_and_wal_no_panic() {
    let (base, dict_opt, sb, raw_ino, seek_ino) = build_rich_image(false, "");
    let dict = dict_opt.as_deref();

    let regions: &[(&str, usize)] = &[
        ("superblock_magic", 0),
        ("superblock_fields", 24),
        ("inode_bitmap", sb.inode_bitmap_block as usize * BLOCK_SIZE),
        ("data_bitmap", sb.data_bitmap_block as usize * BLOCK_SIZE),
        ("wal_header", sb.journal_header_block() as usize * BLOCK_SIZE),
        ("wal_ring", (sb.journal_header_block() as usize + 1) * BLOCK_SIZE + 12),
        (
            "raw_inode",
            sb.inode_table_block as usize * BLOCK_SIZE + raw_ino as usize * INODE_SLOT_SIZE + 12,
        ),
        (
            "seek_inode",
            sb.inode_table_block as usize * BLOCK_SIZE + seek_ino as usize * INODE_SLOT_SIZE + 168,
        ),
        ("root_dir_block", sb.data_block_start as usize * BLOCK_SIZE + 8),
        ("data_area_blk1", (sb.data_block_start as usize + 2) * BLOCK_SIZE + 64),
    ];

    for &(region_name, off) in regions {
        for bit in [0x01u8, 0x80] {
            if off < base.len() {
                let mut mutated = base.clone();
                mutated[off] ^= bit;
                for mode in [StorageMode::Mmap, StorageMode::FileIo] {
                    exercise_hostile_image(
                        &mutated,
                        None,
                        mode,
                        dict,
                        &format!("plain {region_name} bit={bit:#x} mode={mode:?}"),
                    );
                }
            }
        }
    }
}

#[test]
fn test_targeted_region_bitflips_encrypted_with_dict_no_panic() {
    let pw = "BitFlipEncrypted#2026";
    let (base, dict_opt, sb, raw_ino, seek_ino) = build_rich_image(true, pw);
    let dict = dict_opt.as_deref();
    let valid_key = oifs::encryption::derive_key(pw, &sb.encryption_salt).unwrap();
    let wrong_key = oifs::encryption::derive_key("wrong_pw", &sb.encryption_salt).unwrap();

    // Locate first payload block cleanly even when `raw_ino` transparently upgraded to Seekable 64K.
    let dir = tempdir().unwrap();
    let check_path = dir.path().join("enc_inspect.img");
    fs::write(&check_path, &base).unwrap();
    let dm_clean = DiskManager::open_with_key_and_storage_mode(
        &check_path,
        0,
        Some(valid_key.clone()),
        StorageMode::FileIo,
    )
    .unwrap();
    let raw_inode = dm_clean.read_inode(raw_ino).unwrap();
    let raw_blk0 = if raw_inode.flags & INODE_FLAG_SEEKABLE_64K != 0 {
        oifs::inode::ChunkEntry(raw_inode.blocks[0]).start_block() as u64
    } else {
        raw_inode.blocks[0]
    };
    assert!(raw_blk0 >= sb.data_block_start && raw_blk0 < sb.block_count);
    drop(dm_clean);

    // 1. Flip a byte in raw_ino's encrypted payload block -> must fail decryption cleanly
    let mut ct_flip = base.clone();
    ct_flip[raw_blk0 as usize * BLOCK_SIZE + 5] ^= 0x40;
    let ct_path = dir.path().join("ct_flip.img");
    fs::write(&ct_path, &ct_flip).unwrap();
    let dm_ct = DiskManager::open_with_key_and_storage_mode(
        &ct_path,
        0,
        Some(valid_key.clone()),
        StorageMode::FileIo,
    )
    .unwrap();
    assert!(dm_ct.read_data(raw_ino).is_err());
    let mut b = [0u8; 64];
    assert!(dm_ct.read_at(raw_ino, 0, &mut b).is_err());
    drop(dm_ct);

    // 2. Flip a byte inside seek_ino's v3 inode `[0..171)` -> read_inode fails & fsck reports corrupted_inodes
    let seek_slot = sb.inode_table_block as usize * BLOCK_SIZE + seek_ino as usize * INODE_SLOT_SIZE;
    let mut ino_flip = base.clone();
    ino_flip[seek_slot + 20] ^= 0x04;
    let ino_path = dir.path().join("ino_flip.img");
    fs::write(&ino_path, &ino_flip).unwrap();
    let dm_ino = DiskManager::open_with_key_and_storage_mode(
        &ino_path,
        0,
        Some(valid_key.clone()),
        StorageMode::Mmap,
    )
    .unwrap();
    assert!(dm_ino.read_inode(seek_ino).is_err());
    let rep = dm_ino.verify_integrity().unwrap();
    assert!(!rep.is_clean);
    assert!(rep.corrupted_inodes.contains(&seek_ino));
    drop(dm_ino);

    // 2b. Explicitly exercise Argon2id password path on corrupted salt/superblock bytes
    let mut salt_flip = base.clone();
    salt_flip[81] ^= 0x20;
    for pass in [Some(pw), Some("wrong_pw"), None] {
        exercise_hostile_image(
            &salt_flip,
            pass,
            StorageMode::Mmap,
            dict,
            &format!("enc pw_salt off=81 pass={}", pass.is_some()),
        );
    }

    // 3. Key-pinned no-panic sweep across encrypted regions + wrong key / no key
    for off in [
        0usize,
        64,
        81,
        sb.inode_table_block as usize * BLOCK_SIZE + raw_ino as usize * INODE_SLOT_SIZE + 135,
        sb.data_block_start as usize * BLOCK_SIZE + 20,
        raw_blk0 as usize * BLOCK_SIZE + 30,
    ] {
        let mut mutated = base.clone();
        mutated[off] ^= 0x20;
        for (k_lbl, key_opt) in [
            ("valid", Some(valid_key.clone())),
            ("wrong", Some(wrong_key.clone())),
            ("none", None),
        ] {
            for mode in [StorageMode::Mmap, StorageMode::FileIo] {
                exercise_hostile_image_with_key(
                    &mutated,
                    key_opt.clone(),
                    mode,
                    dict,
                    &format!("enc off={off} key={k_lbl} mode={mode:?}"),
                );
            }
        }
    }
}

fn write_forged_inode_v3(img: &mut [u8], slot_offset: usize, inode: &oifs::inode::Inode) {
    let mut encoded = encode_v2(inode);
    let crc = crc32c(&encoded[..INODE_V2_CRC32C_OFFSET]);
    encoded[INODE_V2_CRC32C_OFFSET..INODE_V2_CRC32C_OFFSET + 4]
        .copy_from_slice(&crc.to_le_bytes());
    img[slot_offset..slot_offset + INODE_SLOT_SIZE].copy_from_slice(&encoded);
}

#[test]
fn test_crc_valid_forged_hostile_inodes_no_panic_or_oom() {
    let (base, dict_opt, sb, raw_ino, seek_ino) = build_rich_image(false, "");
    let dict = dict_opt.as_deref();

    let raw_slot = sb.inode_table_block as usize * BLOCK_SIZE + raw_ino as usize * INODE_SLOT_SIZE;
    let seek_slot = sb.inode_table_block as usize * BLOCK_SIZE + seek_ino as usize * INODE_SLOT_SIZE;

    let forged_cases = [
        ("huge_size_raw", raw_slot, {
            let mut ino = oifs::inode::Inode::new(FileType::File);
            ino.size = u64::MAX;
            ino.compressed_size = 0;
            ino.blocks[0] = sb.data_block_start;
            ino
        }),
        ("huge_compressed_size", raw_slot, {
            let mut ino = oifs::inode::Inode::new(FileType::File);
            ino.size = u64::MAX - 1;
            ino.compressed_size = u64::MAX;
            ino.blocks[0] = sb.data_block_start;
            ino
        }),
        ("oob_direct_and_indirect", raw_slot, {
            let mut ino = oifs::inode::Inode::new(FileType::File);
            ino.size = 100 * 4096;
            ino.blocks = [u64::MAX - 7; 12];
            ino.triple_indirect = u64::MAX;
            ino
        }),
        ("bogus_filter_typesize", raw_slot, {
            let mut ino = oifs::inode::Inode::new(FileType::File);
            ino.size = 4096;
            ino.compressed_size = 128;
            ino.blocks[0] = sb.data_block_start;
            ino.filter_typesize = 255;
            ino.filter_delta = true;
            ino.filter_shuffle = true;
            ino.filter_bitshuffle = true;
            ino
        }),
        ("hostile_seekable_chunks", seek_slot, {
            let mut ino = oifs::inode::Inode::new(FileType::File);
            ino.size = 1024 * 1024;
            ino.compressed_size = 4096;
            ino.flags = INODE_FLAG_SEEKABLE_64K;
            ino.blocks = [u64::MAX; 12];
            ino.mode = FileType::File;
            ino
        }),
    ];

    for (name, slot, ino) in forged_cases {
        let mut mutated = base.clone();
        write_forged_inode_v3(&mut mutated, slot, &ino);
        for mode in [StorageMode::Mmap, StorageMode::FileIo] {
            exercise_hostile_image(
                &mutated,
                None,
                mode,
                dict,
                &format!("forged={name} mode={mode:?}"),
            );
        }
    }
}

#[test]
fn test_random_full_image_multi_bitflip_bombardment() {
    let pw = "BombardmentPw#2026";
    let (plain_base, plain_dict, _, _, _) = build_rich_image(false, "");
    let (enc_base, enc_dict, enc_sb, _, _) = build_rich_image(true, pw);
    let enc_key = oifs::encryption::derive_key(pw, &enc_sb.encryption_salt).unwrap();
    let mut rng = XorShift64::new(0xB17F_1199u64);

    for iter in 0..120usize {
        let use_enc = iter % 2 == 1;
        let (base, dict, key_opt) = if use_enc {
            (&enc_base, enc_dict.as_deref(), Some(enc_key.clone()))
        } else {
            (&plain_base, plain_dict.as_deref(), None)
        };
        let mut mutated = base.clone();
        let n_flips = 1 + rng.next_usize(8);
        for _ in 0..n_flips {
            let off = rng.next_usize(mutated.len());
            let mask = (1u8 << rng.next_usize(8)).max(1);
            mutated[off] ^= mask;
        }
        let mode = if iter % 4 < 2 {
            StorageMode::Mmap
        } else {
            StorageMode::FileIo
        };
        exercise_hostile_image_with_key(
            &mutated,
            key_opt,
            mode,
            dict,
            &format!("bombardment iter={iter} enc={use_enc} flips={n_flips} mode={mode:?}"),
        );
    }
}
