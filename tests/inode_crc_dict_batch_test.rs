use oifs::disk::{CompressionMode, DiskManager, DiskManagerError, DurabilityMode};
use oifs::inode_format::{
    INODE_SLOT_SIZE, INODE_V2_CRC32C_OFFSET, INODE_V2_RECORD_VERSION,
    INODE_V2_RECORD_VERSION_NO_CRC, INODE_V2_RECORD_VERSION_OFFSET, INODE_V2_RESERVED_START,
};
use oifs::superblock::SuperBlock;
use oifs::{SpliceOptions, SpliceTrimMode, StorageMode, TransferMethod, TransferOptions};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const MB: u64 = 1024 * 1024;

struct TempImg {
    path: PathBuf,
}

impl TempImg {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        p.push(format!("oifs_feat_test_{tag}_{pid}_{n}.img"));
        let _ = fs::remove_file(&p);
        Self { path: p }
    }
}

impl Drop for TempImg {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct TempHostFile {
    path: PathBuf,
}

impl TempHostFile {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        p.push(format!("oifs_host_file_{tag}_{pid}_{n}.bin"));
        let _ = fs::remove_file(&p);
        Self { path: p }
    }
}

impl Drop for TempHostFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn make_json_samples(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|i| {
            format!(
                r#"{{"service":"oifs-telemetry","region":"ap-northeast-1","env":"prod","request_id":"req-{i:06}","latency_us":{},"status":200,"cache_hit":true,"tags":["storage","inode","zstd","crc32c"]}}"#,
                120 + (i % 73)
            )
            .into_bytes()
        })
        .collect()
}

fn deterministic_pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x >> 24) as u8);
    }
    out
}

fn raw_inode_slot_offset(img_path: &Path, inode_id: u64) -> u64 {
    let mut f = OpenOptions::new().read(true).open(img_path).unwrap();
    let mut sb_buf = [0u8; 4096];
    f.read_exact(&mut sb_buf).unwrap();
    let sb: SuperBlock = bincode::deserialize(&sb_buf).unwrap();
    sb.inode_table_block * 4096 + inode_id * INODE_SLOT_SIZE as u64
}

// ===========================================================================
// 1. Main-Table Inode CRC32C Checksum (`record_version == 3`)
// ===========================================================================

#[test]
fn test_inode_crc32c_stamps_v3_detects_bitflip_and_keeps_v2_compat() {
    let img = TempImg::new("inode_crc");
    let file_id;
    let legacy_id;
    let payload_a = b"inode crc32c integrity payload alpha";
    let payload_b = b"legacy v2 inode backward compat payload beta";

    {
        let dm = DiskManager::open(&img.path, 16 * MB).expect("open");
        file_id = dm
            .create_file_with_data(0, "protected.bin", payload_a, CompressionMode::Never)
            .expect("create a");
        legacy_id = dm
            .create_file_with_data(0, "legacy.bin", payload_b, CompressionMode::Never)
            .expect("create b");
        dm.flush().unwrap();
    }

    // Verify on-disk record stamps record_version == 3, zeroed reserved tail, and non-zero CRC32C.
    let slot_off_a = raw_inode_slot_offset(&img.path, file_id);
    let slot_off_b = raw_inode_slot_offset(&img.path, legacy_id);
    {
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&img.path)
            .unwrap();
        let mut slot = [0u8; INODE_SLOT_SIZE];
        f.seek(SeekFrom::Start(slot_off_a)).unwrap();
        f.read_exact(&mut slot).unwrap();
        assert_eq!(
            slot[INODE_V2_RECORD_VERSION_OFFSET],
            INODE_V2_RECORD_VERSION
        );
        assert_eq!(INODE_V2_RECORD_VERSION, 3);
        assert!(slot[INODE_V2_RESERVED_START..].iter().all(|&b| b == 0));

        // Flip 1 bit in `file_id` payload metadata (size low byte at offset 1).
        f.seek(SeekFrom::Start(slot_off_a + 1)).unwrap();
        f.write_all(&[slot[1] ^ 0x01]).unwrap();

        // Downgrade `legacy_id` to record_version == 2 with zeroed [171..175] CRC slot.
        let mut slot_b = [0u8; INODE_SLOT_SIZE];
        f.seek(SeekFrom::Start(slot_off_b)).unwrap();
        f.read_exact(&mut slot_b).unwrap();
        slot_b[INODE_V2_RECORD_VERSION_OFFSET] = INODE_V2_RECORD_VERSION_NO_CRC;
        slot_b[INODE_V2_CRC32C_OFFSET..INODE_V2_RESERVED_START].fill(0);
        f.seek(SeekFrom::Start(slot_off_b)).unwrap();
        f.write_all(&slot_b).unwrap();
        f.sync_all().unwrap();
    }

    let dm = DiskManager::open(&img.path, 16 * MB).expect("reopen");
    // Reading corrupted v3 inode fails cleanly with checksum error.
    let err = dm
        .read_data(file_id)
        .expect_err("corrupted inode must fail");
    let err_str = err.to_string();
    assert!(
        err_str.contains("CRC32C") || err_str.contains("Checksum"),
        "unexpected error message: {err_str}"
    );

    // Legacy v2 inode remains completely readable.
    assert_eq!(dm.read_data(legacy_id).expect("legacy v2 read"), payload_b);

    // fsck (`verify_integrity`) catches the corrupted inode in `corrupted_inodes`.
    let report = dm.verify_integrity().expect("fsck");
    assert!(!report.is_clean);
    assert!(
        report.corrupted_inodes.contains(&file_id),
        "expected corrupted_inodes to contain {file_id}, got {:?}",
        report.corrupted_inodes
    );
    assert!(!report.corrupted_inodes.contains(&legacy_id));
}

// ===========================================================================
// 2. Zstd Custom Dictionary Support
// ===========================================================================

#[test]
fn test_zstd_dict_auto_small_files_anti_inflation_and_variants() {
    let samples = make_json_samples(120);
    let dict = DiskManager::train_zstd_dict(&samples, 32 * 1024).expect("train dict");

    // A) Standard Mmap image: < 8 KiB Auto threshold bypass + anti-inflation guard
    let img = TempImg::new("dict_mmap");
    {
        let dm = DiskManager::open(&img.path, 32 * MB).expect("open");
        let small_json = &samples[0]; // ~170 bytes (< 8192 B)
        let raw_id = dm
            .create_file_with_data(0, "no_dict.json", small_json, CompressionMode::Auto)
            .unwrap();
        assert_eq!(
            dm.read_inode(raw_id).unwrap().compressed_size,
            0,
            "without dictionary, <8 KiB Auto stays uncompressed"
        );

        let dict_id = dm.register_and_use_zstd_dict(&dict).expect("register dict");
        assert_eq!(dm.default_zstd_dict_id(), Some(dict_id));

        let dict_file_id = dm
            .create_file_with_data(0, "with_dict.json", small_json, CompressionMode::Auto)
            .unwrap();
        let dict_inode = dm.read_inode(dict_file_id).unwrap();
        assert!(
            dict_inode.compressed_size > 0 && dict_inode.compressed_size < small_json.len() as u64,
            "expected dict Auto compression of <8K payload, got compressed_size={} vs orig={}",
            dict_inode.compressed_size,
            small_json.len()
        );
        assert_eq!(dm.read_data(dict_file_id).unwrap(), *small_json);

        // Incompressible random < 8 KiB bytes must fall back to raw (anti-inflation).
        let random = deterministic_pseudo_random(512, 0xCAFE_BABE);
        let rand_id = dm
            .create_file_with_data(0, "rand.bin", &random, CompressionMode::Auto)
            .unwrap();
        assert_eq!(dm.read_inode(rand_id).unwrap().compressed_size, 0);
        assert_eq!(dm.read_data(rand_id).unwrap(), random);

        // Stream & Always modes + multi-frame compressed append
        let always_id = dm
            .create_file_with_data(0, "always.json", &samples[1], CompressionMode::Always)
            .unwrap();
        dm.write_data(
            always_id,
            samples[1].len() as u64,
            &samples[2],
            CompressionMode::Stream { level: 3 },
        )
        .unwrap();
        let mut expected_concat = samples[1].clone();
        expected_concat.extend_from_slice(&samples[2]);
        assert_eq!(dm.read_data(always_id).unwrap(), expected_concat);

        // Seekable 64K with dictionary: multi-chunk write, read_at, partial overwrite, truncate
        let mut big_json = Vec::new();
        for s in &samples {
            big_json.extend_from_slice(s);
            big_json.push(b'\n');
        }
        while big_json.len() < 150 * 1024 {
            big_json.extend_from_slice(&samples[3]);
            big_json.push(b'\n');
        }
        let seek_id = dm
            .create_file_with_data(
                0,
                "seek.jsonl",
                &big_json,
                CompressionMode::Seekable {
                    chunk_size: 65536,
                    level: 3,
                },
            )
            .unwrap();
        let patch = b"PATCH_MARKER_AT_70K";
        dm.write_data(
            seek_id,
            70_000,
            patch,
            CompressionMode::Seekable {
                chunk_size: 65536,
                level: 3,
            },
        )
        .unwrap();
        big_json[70_000..70_000 + patch.len()].copy_from_slice(patch);

        let mut slice_buf = vec![0u8; 256];
        let n = dm.read_at(seek_id, 69_950, &mut slice_buf).unwrap();
        assert_eq!(n, 256);
        assert_eq!(&slice_buf, &big_json[69_950..69_950 + 256]);
        assert_eq!(dm.read_data(seek_id).unwrap(), big_json);

        dm.truncate(seek_id, 80_000).unwrap();
        assert_eq!(dm.read_data(seek_id).unwrap(), &big_json[..80_000]);

        dm.flush().unwrap();
    }

    // B) Reopen without registering dictionary -> MissingDictionary { dict_id }
    {
        let dm_ro = DiskManager::open_read_only(&img.path).expect("open ro");
        let f_auto = dm_ro.lookup(0, "with_dict.json").unwrap();
        let f_seek = dm_ro.lookup(0, "seek.jsonl").unwrap();
        assert!(matches!(
            dm_ro.read_data(f_auto),
            Err(DiskManagerError::MissingDictionary { .. })
        ));
        assert!(matches!(
            dm_ro.read_data(f_seek),
            Err(DiskManagerError::MissingDictionary { .. })
        ));

        // Registering dict on Read-Only mount enables decoding!
        dm_ro.register_zstd_dict(&dict).expect("register on RO");
        assert_eq!(dm_ro.read_data(f_auto).unwrap(), samples[0]);
        assert_eq!(dm_ro.read_data(f_seek).unwrap().len(), 80_000);
    }

    // C) Multi-dict coexistence + train_zstd_dict_from_dir + FileIo + Encrypted image
    let samples_b: Vec<Vec<u8>> = (0..100)
        .map(|i| {
            format!(
                "LOG_REC|host=db-{i:03}|level=INFO|component=replication|msg=checkpoint_done_{i}\n"
            )
            .into_bytes()
        })
        .collect();
    let img_enc = TempImg::new("dict_enc_fileio");
    {
        let dm = DiskManager::create_encrypted_file_io(&img_enc.path, 32 * MB, "hunter2-dict-pass")
            .expect("create encrypted file_io");
        assert_eq!(dm.storage_mode(), StorageMode::FileIo);
        let sample_dir = dm.create_directory(0, "samples").unwrap();
        for (i, s) in samples_b.iter().enumerate() {
            dm.create_file_with_data(sample_dir, &format!("s_{i}.log"), s, CompressionMode::Never)
                .unwrap();
        }
        let dict_b = dm
            .train_zstd_dict_from_dir(sample_dir, 16 * 1024, 100)
            .expect("train from dir");
        let id_a = dm.register_and_use_zstd_dict(&dict).unwrap();
        let f_a = dm
            .create_file_with_data(0, "a.json", &samples[5], CompressionMode::Auto)
            .unwrap();

        let id_b = dm.register_and_use_zstd_dict(&dict_b).unwrap();
        assert_ne!(id_a, id_b);
        let f_b = dm
            .create_file_with_data(0, "b.log", &samples_b[5], CompressionMode::Auto)
            .unwrap();

        // Seekable 64K + Encrypted + Dict + FileIo
        let big_log: Vec<u8> = samples_b.concat();
        let f_seek_enc = dm
            .create_file_with_data(
                0,
                "seek_enc.log",
                &big_log,
                CompressionMode::Seekable {
                    chunk_size: 65536,
                    level: 3,
                },
            )
            .unwrap();

        assert_eq!(dm.read_data(f_a).unwrap(), samples[5]);
        assert_eq!(dm.read_data(f_b).unwrap(), samples_b[5]);
        assert_eq!(dm.read_data(f_seek_enc).unwrap(), big_log);
    }
}

#[test]
fn test_zstd_dict_cross_image_transfer_and_splice_fallback() {
    let samples = make_json_samples(100);
    let dict = DiskManager::train_zstd_dict(&samples, 32 * 1024).unwrap();
    let payload: Vec<u8> = samples.concat();

    let src_img = TempImg::new("dict_xfer_src");
    let dst_nodict_img = TempImg::new("dict_xfer_dst_nodict");
    let dst_samedict_img = TempImg::new("dict_xfer_dst_same");

    let src = DiskManager::open(&src_img.path, 32 * MB).unwrap();
    src.register_and_use_zstd_dict(&dict).unwrap();
    let src_ino = src
        .create_file_with_data(
            0,
            "events.jsonl",
            &payload,
            CompressionMode::Seekable {
                chunk_size: 65536,
                level: 3,
            },
        )
        .unwrap();
    let src_stream_ino = src
        .create_file_with_data(0, "small.json", &samples[0], CompressionMode::Auto)
        .unwrap();
    src.flush().unwrap();

    // 1. Destination without dictionary must fall back to LogicalReencode so `dst` stays readable.
    let dst_no = DiskManager::open(&dst_nodict_img.path, 32 * MB).unwrap();
    let st_file = src
        .transfer_file_to(
            0,
            "small.json",
            &dst_no,
            0,
            "small.json",
            TransferOptions::default(),
        )
        .unwrap();
    assert_eq!(st_file.method, TransferMethod::LogicalReencode);
    assert_eq!(dst_no.read_data(st_file.dst_inode).unwrap(), samples[0]);

    let dst_seek_no = dst_no
        .create_file_with_data(
            0,
            "target.jsonl",
            &vec![0u8; payload.len()],
            CompressionMode::Seekable {
                chunk_size: 65536,
                level: 3,
            },
        )
        .unwrap();
    let sp_no = src
        .splice_range_to(
            src_ino,
            0,
            &dst_no,
            dst_seek_no,
            0,
            payload.len() as u64,
            SpliceOptions {
                trim_mode: SpliceTrimMode::Keep,
            },
        )
        .unwrap();
    assert_eq!(sp_no.method, TransferMethod::LogicalReencode);
    assert_eq!(dst_no.read_data(dst_seek_no).unwrap(), payload);

    // 2. Destination WITH matching dictionary is eligible for fast path (not LogicalReencode).
    let dst_same = DiskManager::open(&dst_samedict_img.path, 32 * MB).unwrap();
    dst_same.register_and_use_zstd_dict(&dict).unwrap();
    let st_same = src
        .transfer_file_to(
            0,
            "events.jsonl",
            &dst_same,
            0,
            "events.jsonl",
            TransferOptions::default(),
        )
        .unwrap();
    assert_ne!(st_same.method, TransferMethod::LogicalReencode);
    assert_eq!(dst_same.read_data(st_same.dst_inode).unwrap(), payload);

    // Check unused variable silence cleanly
    let _ = src_stream_ino;
}

#[test]
fn test_zstd_dict_cli_train_put_and_cat() {
    let oifs_bin = env!("CARGO_BIN_EXE_oifs");
    let img = TempImg::new("cli_dict");
    let dict_file = TempHostFile::new("cli_dict_zst");
    let put_input = TempHostFile::new("cli_dict_payload");

    let samples = make_json_samples(100);
    {
        let dm = DiskManager::open(&img.path, 16 * MB).unwrap();
        let dir = dm.create_directory(0, "samples").unwrap();
        for (i, s) in samples.iter().enumerate() {
            dm.create_file_with_data(dir, &format!("sample_{i}.json"), s, CompressionMode::Never)
                .unwrap();
        }
        dm.flush().unwrap();
    }
    fs::write(&put_input.path, &samples[42]).unwrap();

    // 1. `oifs -i img dict train --dir /samples --output dict_file`
    let train_out = Command::new(oifs_bin)
        .arg("-i")
        .arg(&img.path)
        .args(["--json", "dict", "train", "--dir", "/samples", "--output"])
        .arg(&dict_file.path)
        .args(["--max-size", "16384"])
        .output()
        .expect("run dict train");
    assert!(
        train_out.status.success(),
        "dict train failed: {}",
        String::from_utf8_lossy(&train_out.stderr)
    );
    assert!(fs::metadata(&dict_file.path).unwrap().len() >= 256);

    // 2. `oifs -i img --dict dict_file put put_input item.json`
    let put_out = Command::new(oifs_bin)
        .arg("-i")
        .arg(&img.path)
        .arg("--dict")
        .arg(&dict_file.path)
        .arg("put")
        .arg(&put_input.path)
        .arg("item.json")
        .output()
        .expect("run put with --dict");
    assert!(
        put_out.status.success(),
        "put --dict failed: {}",
        String::from_utf8_lossy(&put_out.stderr)
    );

    // Verify compressed_size > 0 on the imported <8K item
    {
        let dm = DiskManager::open_read_only(&img.path).unwrap();
        let id = dm.lookup(0, "item.json").unwrap();
        let inode = dm.read_inode(id).unwrap();
        assert!(inode.compressed_size > 0);
    }

    // 3. `oifs -i img --dict dict_file cat item.json`
    let cat_out = Command::new(oifs_bin)
        .arg("-i")
        .arg(&img.path)
        .arg("--dict")
        .arg(&dict_file.path)
        .args(["cat", "item.json"])
        .output()
        .expect("run cat with --dict");
    assert!(
        cat_out.status.success(),
        "cat --dict failed: {}",
        String::from_utf8_lossy(&cat_out.stderr)
    );
    assert_eq!(cat_out.stdout, samples[42]);
}

// ===========================================================================
// 3. Explicit Multi-Op Write Batch (`WriteBatch`)
// ===========================================================================

#[test]
fn test_write_batch_500_files_journaled_coalesced_and_bulk_delete() {
    let img = TempImg::new("batch_500");
    {
        let dm = DiskManager::open_journaled(&img.path, 32 * MB).expect("open journaled");
        dm.set_durability_mode(DurabilityMode::Strict);
        let mut batch = dm.begin_write_batch();
        for i in 0..500 {
            let body = format!("batch-payload-{i:04}-hello-oifs");
            batch.create_file_with_data(0, format!("f_{i:04}.txt"), body, CompressionMode::Auto);
        }
        assert_eq!(batch.len(), 500);
        let res = batch.commit().expect("commit 500 creates");
        assert_eq!(res.ops_executed, 500);
        assert_eq!(res.created_inodes.len(), 500);
        assert!(
            res.wal_transactions >= 1 && res.wal_transactions < 20,
            "expected heavily coalesced WAL txs (<20 for 500 files), got {}",
            res.wal_transactions
        );
        assert!(dm.verify_integrity().unwrap().is_clean);
    }

    // Reopen and verify all 500 files survived cleanly, then bulk-delete 200 files in 1 batch.
    {
        let dm = DiskManager::open_journaled(&img.path, 32 * MB).expect("reopen");
        for i in 0..500 {
            let id = dm.lookup(0, &format!("f_{i:04}.txt")).unwrap();
            let got = dm.read_data(id).unwrap();
            assert_eq!(got, format!("batch-payload-{i:04}-hello-oifs").into_bytes());
        }

        let mut del_batch = dm.begin_write_batch();
        for i in 0..200 {
            del_batch.delete(0, format!("f_{i:04}.txt"));
        }
        let del_res = del_batch.commit().expect("commit 200 deletes");
        assert_eq!(del_res.ops_executed, 200);
        assert!(del_res.created_inodes.is_empty());
        assert!(del_res.wal_transactions >= 1);
        assert!(dm.verify_integrity().unwrap().is_clean);

        for i in 0..200 {
            assert!(dm.lookup(0, &format!("f_{i:04}.txt")).is_err());
        }
        for i in 200..500 {
            assert!(dm.lookup(0, &format!("f_{i:04}.txt")).is_ok());
        }
    }
}

#[test]
fn test_write_batch_mixed_ops_seekable_dict_encrypted_and_modes() {
    let samples = make_json_samples(80);
    let dict = DiskManager::train_zstd_dict(&samples, 16 * 1024).unwrap();
    let big_seek: Vec<u8> = samples.concat();

    // Test matrix: (journaled, encrypted, storage_mode)
    let configs = [
        (true, false, StorageMode::Mmap),
        (true, true, StorageMode::Mmap),
        (false, false, StorageMode::Mmap),
        (true, true, StorageMode::FileIo),
        (false, false, StorageMode::FileIo),
    ];

    for (idx, &(journaled, encrypted, mode)) in configs.iter().enumerate() {
        let img = TempImg::new(&format!("batch_mixed_{idx}"));
        {
            let dm = if encrypted {
                let password = Some("batch-secret-pass");
                DiskManager::open_with_options(&img.path, 32 * MB, password, journaled, mode)
                    .expect("open encrypted")
            } else {
                DiskManager::open_with_options(&img.path, 32 * MB, None, journaled, mode)
                    .expect("open plain")
            };
            dm.register_and_use_zstd_dict(&dict).unwrap();

            // Pre-existing dir and victim files for mixed create/mkdir/rename/delete/overwrite
            let sub_id = dm.create_directory(0, "sub").unwrap();
            dm.create_file_with_data(0, "old_target.txt", b"overwrite-me", CompressionMode::Auto)
                .unwrap();

            let mut batch = dm.begin_write_batch();
            batch
                .create_file_with_data(0, "tiny.json", samples[0].clone(), CompressionMode::Auto)
                .create_file_with_data(
                    0,
                    "big_seek.jsonl",
                    big_seek.clone(),
                    CompressionMode::Seekable {
                        chunk_size: 65536,
                        level: 3,
                    },
                )
                .mkdir(0, "new_dir")
                .mkdir(0, "gone_dir")
                .create_file_with_data(
                    sub_id,
                    "temp.txt",
                    b"will be deleted in same batch".to_vec(),
                    CompressionMode::Never,
                )
                .delete(sub_id, "temp.txt")
                .delete(0, "gone_dir")
                .rename(0, "tiny.json", sub_id, "moved_tiny.json")
                .create_file_with_data(
                    0,
                    "src_over.txt",
                    b"winner".to_vec(),
                    CompressionMode::Never,
                )
                .rename(0, "src_over.txt", 0, "old_target.txt");

            let res = batch.commit().expect("mixed batch commit");
            assert_eq!(res.ops_executed, 10);
            assert_eq!(res.created_inodes.len(), 6);
            if journaled {
                assert!(res.wal_transactions >= 1);
            } else {
                assert_eq!(res.wal_transactions, 0);
            }

            assert!(dm.verify_integrity().unwrap().is_clean);
            assert!(dm.lookup(0, "tiny.json").is_err());
            assert!(dm.lookup(0, "gone_dir").is_err());
            assert!(dm.lookup(sub_id, "temp.txt").is_err());
            assert!(dm.lookup(0, "src_over.txt").is_err());

            let moved_id = dm.lookup(sub_id, "moved_tiny.json").unwrap();
            assert_eq!(dm.read_data(moved_id).unwrap(), samples[0]);
            let winner_id = dm.lookup(0, "old_target.txt").unwrap();
            assert_eq!(dm.read_data(winner_id).unwrap(), b"winner");
            let seek_id = dm.lookup(0, "big_seek.jsonl").unwrap();
            assert_eq!(dm.read_data(seek_id).unwrap(), big_seek);
            let mut sub_slice = [0u8; 64];
            dm.read_at(seek_id, 500, &mut sub_slice).unwrap();
            assert_eq!(&sub_slice, &big_seek[500..564]);
        }

        // Reopen cleanly and re-verify
        let dm2 = if encrypted {
            DiskManager::open_with_options(
                &img.path,
                32 * MB,
                Some("batch-secret-pass"),
                journaled,
                mode,
            )
            .unwrap()
        } else {
            DiskManager::open_with_options(&img.path, 32 * MB, None, journaled, mode).unwrap()
        };
        dm2.register_and_use_zstd_dict(&dict).unwrap();
        assert!(dm2.verify_integrity().unwrap().is_clean);
        let sub_id = dm2.lookup(0, "sub").unwrap();
        let moved_id = dm2.lookup(sub_id, "moved_tiny.json").unwrap();
        assert_eq!(dm2.read_data(moved_id).unwrap(), samples[0]);
    }
}

#[test]
fn test_write_batch_atomic_rollback_on_validation_error() {
    for &journaled in &[true, false] {
        let img = TempImg::new(&format!("batch_rollback_{journaled}"));
        let dm =
            DiskManager::open_with_options(&img.path, 16 * MB, None, journaled, StorageMode::Mmap)
                .unwrap();
        let non_empty = dm.create_directory(0, "keep_dir").unwrap();
        dm.create_file_with_data(non_empty, "child.txt", b"stay", CompressionMode::Never)
            .unwrap();

        // Batch with 5 valid ops followed by deleting a non-empty directory -> MUST roll back 100%.
        let mut batch = dm.begin_write_batch();
        for i in 0..5 {
            batch.create_file_with_data(
                0,
                format!("ghost_{i}.txt"),
                b"ghost".to_vec(),
                CompressionMode::Never,
            );
        }
        batch.delete(0, "keep_dir");
        let err = batch
            .commit()
            .expect_err("non-empty dir delete must fail batch");
        assert!(err.to_string().contains("not empty"));

        // Zero partial commits allowed: none of ghost_0..5 exist and fsck is spotless.
        for i in 0..5 {
            assert!(
                dm.lookup(0, &format!("ghost_{i}.txt")).is_err(),
                "ghost_{i}.txt should not exist after rollback"
            );
        }
        let child = dm.lookup(non_empty, "child.txt").unwrap();
        assert_eq!(dm.read_data(child).unwrap(), b"stay");
        assert!(dm.verify_integrity().unwrap().is_clean);

        // Also reject duplicate filename introduced inside the same batch.
        let mut dup_batch = dm.begin_write_batch();
        dup_batch
            .create_file_with_data(0, "once.txt", b"1".to_vec(), CompressionMode::Never)
            .create_file_with_data(0, "once.txt", b"2".to_vec(), CompressionMode::Never);
        assert!(dup_batch.commit().is_err());
        assert!(dm.lookup(0, "once.txt").is_err());
        assert!(dm.verify_integrity().unwrap().is_clean);
    }
}
