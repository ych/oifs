//! Crash VFS Power-Loss Simulation & Real `SIGKILL` Recovery Tests (SQLite/littlefs-inspired).
//!
//! 1. Uses `oifs::storage::crash_sim` to record every exact `pwrite(2)`, `set_len(2)`, and
//!    `sync_data(2)` barrier issued by `StorageMode::FileIo + DurabilityMode::Strict`.
//! 2. Replays exhaustive prefix cuts (`0..=N`), adversarial intra-barrier write reordering and
//!    subset drops, and 512-byte torn-sector / torn-WAL-header truncations across all major
//!    `Strict` WAL operations (inline create, large ordered-mode write, Seekable 64K, `WriteBatch`,
//!    `truncate`, `defragment`, `grow`) on both plain and encrypted images.
//! 3. Spawns a live child worker writing self-checksumming payloads in `Strict` mode (`Mmap` and
//!    `FileIo`) and abruptly sends `SIGKILL`, verifying WAL recovery and `fsck` cleanliness.

use oifs::BLOCK_SIZE;
use oifs::disk::{CompressionMode, DefragMode, DiskManager, DurabilityMode};
use oifs::inode::FileType;
use oifs::journal::{JOURNAL_RING_BLOCKS, crc32c};
use oifs::storage::StorageMode;
use oifs::storage::crash_sim::{FileIoTraceEvent, start_file_io_trace, take_file_io_trace};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FsSnapshot {
    dirs: BTreeSet<String>,
    files: BTreeMap<String, Vec<u8>>,
}

fn collect_snapshot(
    dm: &DiskManager,
    dir_ino: u64,
    prefix: &str,
    dirs: &mut BTreeSet<String>,
    files: &mut BTreeMap<String, Vec<u8>>,
) {
    let entries = dm.list_dir(dir_ino).expect("list_dir after recovery");
    for entry in entries {
        let full = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        let ino = dm.read_inode(entry.inode).expect("read_inode");
        match ino.mode {
            FileType::Directory => {
                assert!(dirs.insert(full.clone()), "duplicate directory {full}");
                collect_snapshot(dm, entry.inode, &full, dirs, files);
            }
            FileType::File => {
                let data = dm.read_data(entry.inode).expect("read_data");
                assert!(files.insert(full.clone(), data).is_none(), "duplicate file {full}");
            }
        }
    }
}

fn snapshot_fs(dm: &DiskManager) -> FsSnapshot {
    let mut dirs = BTreeSet::new();
    let mut files = BTreeMap::new();
    collect_snapshot(dm, dm.superblock().root_inode, "", &mut dirs, &mut files);
    FsSnapshot { dirs, files }
}

fn apply_trace_event(img: &mut Vec<u8>, ev: &FileIoTraceEvent) {
    match ev {
        FileIoTraceEvent::Write { offset, data } => {
            let start = *offset as usize;
            let end = start + data.len();
            if end > img.len() {
                img.resize(end, 0);
            }
            img[start..end].copy_from_slice(data);
        }
        FileIoTraceEvent::SetLen { new_len } => {
            img.resize(*new_len as usize, 0);
        }
        FileIoTraceEvent::Sync => {}
    }
}

fn assert_crash_image_valid_with_dm(dm: DiskManager, valid_states: &[FsSnapshot], label: &str) {
    let report = dm
        .verify_integrity()
        .unwrap_or_else(|e| panic!("[{label}] verify_integrity failed: {e}"));
    assert!(
        report.is_clean,
        "[{label}] fsck not clean after crash recovery: orphans={:?}, leaked={:?}, missing={:?}, cross={:?}, corrupted={:?}",
        report.orphan_inodes,
        report.leaked_blocks,
        report.missing_blocks,
        report.cross_linked_blocks,
        report.corrupted_inodes
    );

    let recovered = snapshot_fs(&dm);
    assert!(
        valid_states.iter().any(|st| st == &recovered),
        "[{label}] recovered filesystem state does not match any valid atomic commit point!\nRecovered dirs={:?}, files={:?}",
        recovered.dirs,
        recovered.files.keys().collect::<Vec<_>>()
    );
}

fn assert_crash_image_valid(
    crash_bytes: &[u8],
    password: Option<&str>,
    valid_states: &[FsSnapshot],
    label: &str,
) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("crashed.img");
    fs::write(&path, crash_bytes).unwrap();

    let dm = DiskManager::open_with_options(&path, 0, password, true, StorageMode::FileIo)
        .unwrap_or_else(|e| panic!("[{label}] failed to recover crashed image: {e}"));
    assert_crash_image_valid_with_dm(dm, valid_states, label);
}

fn assert_crash_image_valid_with_key(
    crash_bytes: &[u8],
    key: Option<&oifs::encryption::EncryptionKey>,
    valid_states: &[FsSnapshot],
    label: &str,
) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("crashed_key.img");
    fs::write(&path, crash_bytes).unwrap();

    let dm = DiskManager::open_with_key_and_storage_mode(&path, 0, key.cloned(), StorageMode::FileIo)
        .unwrap_or_else(|e| panic!("[{label}] failed to recover crashed image with key: {e}"));
    assert_crash_image_valid_with_dm(dm, valid_states, label);
}

fn split_sync_epochs(events: &[FileIoTraceEvent]) -> Vec<Vec<FileIoTraceEvent>> {
    let mut epochs = Vec::new();
    let mut cur = Vec::new();
    for ev in events {
        if matches!(ev, FileIoTraceEvent::Sync) {
            if !cur.is_empty() {
                epochs.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ev.clone());
        }
    }
    if !cur.is_empty() {
        epochs.push(cur);
    }
    epochs
}

fn run_strict_workload_with_trace(
    img_path: &Path,
    password: Option<&str>,
) -> (
    Vec<u8>,
    oifs::superblock::SuperBlock,
    Vec<FsSnapshot>,
    Vec<FileIoTraceEvent>,
) {
    let (base_img, sb, mut valid_states) = {
        let dm = if let Some(pw) = password {
            DiskManager::create_encrypted_file_io_journaled(img_path, 4 * 1024 * 1024, pw).unwrap()
        } else {
            DiskManager::open_file_io_journaled(img_path, 4 * 1024 * 1024).unwrap()
        };
        dm.set_durability_mode(DurabilityMode::Strict);
        let root = dm.superblock().root_inode;
        let f_seed = dm
            .create_file_with_data(root, "baseline.txt", b"initial_committed_state", CompressionMode::Never)
            .unwrap();
        let _ = f_seed;
        dm.flush().unwrap();
        let sb = dm.superblock();
        let snap0 = snapshot_fs(&dm);
        drop(dm);
        (fs::read(img_path).unwrap(), sb, vec![snap0])
    };

    let _trace_guard = start_file_io_trace(img_path).unwrap();
    {
        let dm = DiskManager::open_with_options(img_path, 0, password, true, StorageMode::FileIo)
            .unwrap();
        dm.set_durability_mode(DurabilityMode::Strict);
        let root = dm.superblock().root_inode;

        // 1. Small WAL-inline create_file_with_data (<= 8 KiB)
        dm.create_file_with_data(
            root,
            "inline_small.txt",
            b"small-inline-wal-payload",
            CompressionMode::Auto,
        )
        .unwrap();
        valid_states.push(snapshot_fs(&dm));

        // 2. Ordered-mode large write (> 8 KiB) + crash-atomic write_data_truncated
        let big = vec![0x5Au8; 28 * 1024];
        let large_id = dm
            .create_file_with_data(root, "ordered_large.bin", &big, CompressionMode::Never)
            .unwrap();
        valid_states.push(snapshot_fs(&dm));

        dm.write_data_truncated(
            large_id,
            b"atomic_truncated_replacement_payload",
            CompressionMode::Auto,
        )
        .unwrap();
        valid_states.push(snapshot_fs(&dm));

        // 3. Seekable 64K write (create + payload write are two distinct Strict commit points)
        let seek_payload: Vec<u8> = (0..(140 * 1024)).map(|i| (i % 199) as u8).collect();
        let seek_id = dm.create_file(root, "seek64k.bin").unwrap();
        valid_states.push(snapshot_fs(&dm));
        dm.write_data(
            seek_id,
            0,
            &seek_payload,
            CompressionMode::Seekable {
                chunk_size: 65536,
                level: 1,
            },
        )
        .unwrap();
        valid_states.push(snapshot_fs(&dm));

        // 4. Explicit WriteBatch (mkdir + create + rename + delete)
        let sub_id = dm.create_directory(root, "work").unwrap();
        valid_states.push(snapshot_fs(&dm));

        let mut wb = dm.begin_write_batch();
        wb.create_file_with_data(sub_id, "draft.txt", b"draft_v1", CompressionMode::Never)
            .rename(root, "baseline.txt", sub_id, "baseline_moved.txt");
        wb.commit().unwrap();
        valid_states.push(snapshot_fs(&dm));

        // 5. Truncate shrink & expand
        dm.truncate(seek_id, 70 * 1024).unwrap();
        valid_states.push(snapshot_fs(&dm));

        // 6. Online defragmentation
        let backing = dm.backing_path().to_str().unwrap().to_string();
        dm.defragment(&backing, DefragMode::InPlace, None).unwrap();
        valid_states.push(snapshot_fs(&dm));

        // 7. In-place grow + post-grow write
        dm.grow(6 * 1024 * 1024).unwrap();
        valid_states.push(snapshot_fs(&dm));
        dm.create_file_with_data(root, "post_grow.txt", b"after_grow_ok", CompressionMode::Auto)
            .unwrap();
        valid_states.push(snapshot_fs(&dm));

        dm.flush().unwrap();
        valid_states.push(snapshot_fs(&dm));
    }

    let events = take_file_io_trace();
    (base_img, sb, valid_states, events)
}

#[test]
fn test_crash_vfs_exhaustive_prefix_cuts_all_strict_operations() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("crash_vfs_prefix.img");
    let (base_img, _sb, valid_states, events) = run_strict_workload_with_trace(&img_path, None);
    assert!(
        events.len() >= 15,
        "expected rich FileIo trace events, got {}",
        events.len()
    );

    let mut cur_img = base_img;
    assert_crash_image_valid(&cur_img, None, &valid_states, "prefix_cut=0");
    for (idx, ev) in events.iter().enumerate() {
        apply_trace_event(&mut cur_img, ev);
        // Only need to mount/check after state-mutating events (Skip duplicate Sync-only checks)
        if !matches!(ev, FileIoTraceEvent::Sync) {
            assert_crash_image_valid(
                &cur_img,
                None,
                &valid_states,
                &format!("prefix_cut={}/{}", idx + 1, events.len()),
            );
        }
    }
}

#[test]
fn test_crash_vfs_intra_barrier_reordering_drops_and_512b_torn_sectors() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("crash_vfs_reorder.img");
    let (base_img, sb, valid_states, events) = run_strict_workload_with_trace(&img_path, None);
    let epochs = split_sync_epochs(&events);
    assert!(!epochs.is_empty());

    let wal_ring_start = (sb.journal_header_block() + 1) * BLOCK_SIZE as u64;
    let wal_ring_end = wal_ring_start + JOURNAL_RING_BLOCKS * BLOCK_SIZE as u64;

    let mut durable_img = base_img;
    let mut prng: u64 = 0xC4A5_9001;
    let mut next_u64 = || {
        prng ^= prng << 13;
        prng ^= prng >> 7;
        prng ^= prng << 17;
        prng
    };

    for (ep_idx, epoch) in epochs.iter().enumerate() {
        if epoch.len() >= 2 {
            // Case A: Only the LAST write in the barrier hits disk (all prior writes in epoch dropped!)
            let mut last_only = durable_img.clone();
            apply_trace_event(&mut last_only, epoch.last().unwrap());
            assert_crash_image_valid(
                &last_only,
                None,
                &valid_states,
                &format!("epoch={ep_idx} last_only"),
            );

            // Case B: Random subsets (reordered flush) within the epoch
            for sub in 0..6 {
                let mut reordered = durable_img.clone();
                let mut indices: Vec<usize> = (0..epoch.len()).collect();
                for i in (1..indices.len()).rev() {
                    let j = (next_u64() as usize) % (i + 1);
                    indices.swap(i, j);
                }
                let keep_cnt = 1 + ((next_u64() as usize) % indices.len());
                for &k in &indices[..keep_cnt] {
                    apply_trace_event(&mut reordered, &epoch[k]);
                }
                assert_crash_image_valid(
                    &reordered,
                    None,
                    &valid_states,
                    &format!("epoch={ep_idx} subset={sub}"),
                );
            }
        }

        // Case C: 512-byte atomic disk sector torn writes (+ 32-byte sub-sector torn WAL ring frames)
        for (w_idx, ev) in epoch.iter().enumerate() {
            if let FileIoTraceEvent::Write { offset, data } = ev {
                let is_wal_ring = *offset >= wal_ring_start && *offset < wal_ring_end;
                let tear_points: &[usize] = if is_wal_ring { &[32, 512] } else { &[512] };
                for &prefix_len in tear_points {
                    if data.len() > prefix_len {
                        for fill_byte in [0x00u8, 0xFF] {
                            let mut torn = durable_img.clone();
                            let start = *offset as usize;
                            let end = start + data.len();
                            if end > torn.len() {
                                torn.resize(end, 0);
                            }
                            torn[start..start + prefix_len].copy_from_slice(&data[..prefix_len]);
                            torn[start + prefix_len..end].fill(fill_byte);
                            assert_crash_image_valid(
                                &torn,
                                None,
                                &valid_states,
                                &format!("epoch={ep_idx} w={w_idx} torn={prefix_len}/{fill_byte:#x}"),
                            );
                        }
                    }
                }
            }
        }

        for ev in epoch {
            apply_trace_event(&mut durable_img, ev);
        }
    }
}

#[test]
fn test_crash_vfs_encrypted_wal_strict_power_loss() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("crash_vfs_enc.img");
    let pw = "CrashVfsEncrypted#2026";
    let (base_img, sb, valid_states, events) = run_strict_workload_with_trace(&img_path, Some(pw));
    let epochs = split_sync_epochs(&events);
    let enc_key = oifs::encryption::derive_key(pw, &sb.encryption_salt).unwrap();

    let wal_ring_start = (sb.journal_header_block() + 1) * BLOCK_SIZE as u64;
    let wal_ring_end = wal_ring_start + JOURNAL_RING_BLOCKS * BLOCK_SIZE as u64;

    // Verify first & final states through full Argon2id password derivation
    assert_crash_image_valid(&base_img, Some(pw), &valid_states, "enc_pw_first");

    let mut cur_img = base_img.clone();
    for (idx, ev) in events.iter().enumerate() {
        apply_trace_event(&mut cur_img, ev);
        if !matches!(ev, FileIoTraceEvent::Sync) {
            assert_crash_image_valid_with_key(
                &cur_img,
                Some(&enc_key),
                &valid_states,
                &format!("enc_prefix={}/{}", idx + 1, events.len()),
            );
        }
    }
    assert_crash_image_valid(&cur_img, Some(pw), &valid_states, "enc_pw_last");

    // Replayed intra-barrier reordering, drops, and torn sectors directly against `base_img`
    let mut durable_img = base_img;
    let mut prng: u64 = 0xE9C0_C4A5u64;
    let mut next_u64 = || {
        prng ^= prng << 13;
        prng ^= prng >> 7;
        prng ^= prng << 17;
        prng
    };

    for (ep_idx, epoch) in epochs.iter().enumerate() {
        if epoch.len() >= 2 {
            let mut drop_head = durable_img.clone();
            apply_trace_event(&mut drop_head, epoch.last().unwrap());
            assert_crash_image_valid_with_key(
                &drop_head,
                Some(&enc_key),
                &valid_states,
                &format!("enc epoch={ep_idx} last_only"),
            );

            for sub in 0..4 {
                let mut reordered = durable_img.clone();
                let mut indices: Vec<usize> = (0..epoch.len()).collect();
                for i in (1..indices.len()).rev() {
                    let j = (next_u64() as usize) % (i + 1);
                    indices.swap(i, j);
                }
                let keep_cnt = 1 + ((next_u64() as usize) % indices.len());
                for &k in &indices[..keep_cnt] {
                    apply_trace_event(&mut reordered, &epoch[k]);
                }
                assert_crash_image_valid_with_key(
                    &reordered,
                    Some(&enc_key),
                    &valid_states,
                    &format!("enc epoch={ep_idx} subset={sub}"),
                );
            }
        }

        for (w_idx, ev) in epoch.iter().enumerate() {
            if let FileIoTraceEvent::Write { offset, data } = ev {
                let is_wal_ring = *offset >= wal_ring_start && *offset < wal_ring_end;
                let tear_points: &[usize] = if is_wal_ring { &[32, 512] } else { &[512] };
                for &prefix_len in tear_points {
                    if data.len() > prefix_len {
                        let fill_byte = if is_wal_ring && prefix_len == 32 {
                            0x00u8
                        } else {
                            0xFFu8
                        };
                        let mut torn = durable_img.clone();
                        let start = *offset as usize;
                        let end = start + data.len();
                        if end > torn.len() {
                            torn.resize(end, 0);
                        }
                        torn[start..start + prefix_len].copy_from_slice(&data[..prefix_len]);
                        torn[start + prefix_len..end].fill(fill_byte);
                        assert_crash_image_valid_with_key(
                            &torn,
                            Some(&enc_key),
                            &valid_states,
                            &format!("enc epoch={ep_idx} w={w_idx} torn={prefix_len}/{fill_byte:#x}"),
                        );
                    }
                }
            }
        }

        for ev in epoch {
            apply_trace_event(&mut durable_img, ev);
        }
    }
}

fn format_self_check_payload(seq: u64, len: usize) -> Vec<u8> {
    let body_len = len.max(64);
    let mut body = vec![0u8; body_len];
    for (i, b) in body.iter_mut().enumerate() {
        *b = ((seq as usize).wrapping_mul(31).wrapping_add(i) & 0xFF) as u8;
    }
    let csum = crc32c(&body);
    let mut out = format!("OIFS_CHK:{seq:08}:{csum:08x}:").into_bytes();
    out.extend_from_slice(&body);
    out
}

fn verify_self_check_payload(bytes: &[u8]) {
    let header_slice = &bytes[..bytes.len().min(27)];
    let text_prefix = std::str::from_utf8(header_slice).unwrap_or("");
    assert!(
        bytes.len() >= 27 && text_prefix.starts_with("OIFS_CHK:"),
        "corrupted header prefix in surviving file (len={}): {header_slice:02x?}",
        bytes.len()
    );
    let mut parts = bytes.splitn(4, |&b| b == b':');
    let magic = parts.next().unwrap();
    let seq_b = parts.next().unwrap();
    let crc_b = parts.next().unwrap();
    let body = parts.next().unwrap();
    assert_eq!(magic, b"OIFS_CHK");
    let _seq: u64 = std::str::from_utf8(seq_b).unwrap().parse().unwrap();
    let expected_crc = u32::from_str_radix(std::str::from_utf8(crc_b).unwrap(), 16).unwrap();
    assert_eq!(crc32c(body), expected_crc, "CRC32C mismatch in surviving file");
}

#[test]
fn sigkill_worker_helper() {
    let Some(img_path) = std::env::var_os("OIFS_SIGKILL_WORKER_IMG") else {
        return;
    };
    let ready_path = std::env::var_os("OIFS_SIGKILL_WORKER_READY").unwrap();
    let use_file_io = std::env::var("OIFS_SIGKILL_WORKER_MODE").as_deref() == Ok("file_io");

    let mode = if use_file_io {
        StorageMode::FileIo
    } else {
        StorageMode::Mmap
    };
    let dm = DiskManager::open_with_options(&img_path, 0, None, true, mode).unwrap();
    dm.set_durability_mode(DurabilityMode::Strict);
    let root = dm.superblock().root_inode;

    fs::write(ready_path, b"ready").unwrap();

    let mut seq = 1u64;
    loop {
        let slot = (seq % 12) as usize;
        let name = format!("live_{slot}.chk");
        let payload = format_self_check_payload(seq, 1024 + ((seq as usize % 5) * 4096));
        match dm.lookup(root, &name) {
            Ok(id) => {
                let _ = dm.write_data_truncated(id, &payload, CompressionMode::Auto);
            }
            Err(_) => {
                let _ = dm.create_file_with_data(root, &name, &payload, CompressionMode::Auto);
            }
        }
        if seq.is_multiple_of(4) {
            let mut wb = dm.begin_write_batch();
            let bname = format!("batch_{}.chk", seq % 8);
            if dm.lookup(root, &bname).is_ok() {
                wb.delete(root, &bname);
            }
            wb.create_file_with_data(
                root,
                &bname,
                format_self_check_payload(seq + 1000, 512),
                CompressionMode::Never,
            );
            let _ = wb.commit();
        }
        seq += 1;
    }
}

#[cfg(unix)]
#[test]
fn test_real_subprocess_sigkill_strict_wal_recovery() {
    let exe = std::env::current_exe().unwrap();
    let dir = tempdir().unwrap();

    for (round, mode_str, storage_mode) in [
        (0, "mmap", StorageMode::Mmap),
        (1, "mmap", StorageMode::Mmap),
        (2, "mmap", StorageMode::Mmap),
        (3, "file_io", StorageMode::FileIo),
        (4, "file_io", StorageMode::FileIo),
        (5, "file_io", StorageMode::FileIo),
    ] {
        let img_path = dir.path().join(format!("sigkill_{round}.img"));
        let ready_path = dir.path().join(format!("sigkill_{round}.ready"));
        {
            let dm = DiskManager::open_with_options(
                &img_path,
                16 * 1024 * 1024,
                None,
                true,
                storage_mode,
            )
            .unwrap();
            let root = dm.superblock().root_inode;
            dm.create_file_with_data(
                root,
                "anchor.chk",
                &format_self_check_payload(0, 512),
                CompressionMode::Never,
            )
            .unwrap();
            dm.flush().unwrap();
        }

        let mut child = std::process::Command::new(&exe)
            .args(["sigkill_worker_helper", "--exact", "--nocapture"])
            .env("OIFS_SIGKILL_WORKER_IMG", &img_path)
            .env("OIFS_SIGKILL_WORKER_READY", &ready_path)
            .env("OIFS_SIGKILL_WORKER_MODE", mode_str)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sigkill worker");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready_path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(ready_path.exists(), "worker did not signal readiness");

        std::thread::sleep(std::time::Duration::from_millis(25 + (round as u64 % 3) * 15));
        let pid = child.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        let _ = child.wait();

        // Recover & validate integrity + payload self-checksums
        let dm = DiskManager::open_with_options(&img_path, 0, None, true, storage_mode)
            .unwrap_or_else(|e| panic!("round={round} ({mode_str}) open after SIGKILL failed: {e}"));
        let report = dm.verify_integrity().unwrap();
        assert!(
            report.is_clean,
            "round={round} ({mode_str}) fsck failed after SIGKILL: {report:?}"
        );
        let root = dm.superblock().root_inode;
        let entries = dm.list_dir(root).unwrap();
        assert!(
            entries.iter().any(|e| e.name == "anchor.chk"),
            "anchor file vanished after SIGKILL"
        );
        for entry in entries {
            let data = dm.read_data(entry.inode).unwrap();
            verify_self_check_payload(&data);
        }
    }
}
