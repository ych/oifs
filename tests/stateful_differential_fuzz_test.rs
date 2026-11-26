//! Stateful Differential / Model-Based Fuzzing (`fsx`-style & full-filesystem oracle).
//!
//! Executes long, deterministic pseudo-random interleaved filesystem workloads against an
//! in-memory reference oracle (`ModelFs`), verifying full payload parity, directory hierarchy
//! parity, random `read_at` slices, cross-mode `Mmap <-> FileIo` reopens, and `verify_integrity()`.

use oifs::BLOCK_SIZE;
use oifs::disk::{
    AutoGrowConfig, CompressionMode, DefragMode, DiskManager, DiskManagerError, DurabilityMode,
    SpliceOptions, SpliceTrimMode, TransferOptions, VacuumOptions,
};
use oifs::filters::FilterConfig;
use oifs::inode::FileType;
use oifs::storage::StorageMode;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
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

    fn fill_pattern(&mut self, len: usize, compressible: bool) -> Vec<u8> {
        let mut out = vec![0u8; len];
        if compressible {
            let pat = [
                (self.next_u64() & 0xFF) as u8,
                ((self.next_u64() >> 8) & 0xFF) as u8,
                0x20,
                0x7B,
            ];
            for (i, b) in out.iter_mut().enumerate() {
                *b = pat[i % pat.len()].wrapping_add(((i / 64) & 3) as u8);
            }
        } else {
            for b in &mut out {
                *b = (self.next_u64() & 0xFF) as u8;
            }
        }
        out
    }
}

#[derive(Default, Clone)]
struct ModelFs {
    dirs: BTreeSet<String>,
    files: BTreeMap<String, Vec<u8>>,
}

impl ModelFs {
    fn new() -> Self {
        Self::default()
    }

    fn split_parent(path: &str) -> (&str, &str) {
        match path.rfind('/') {
            Some(pos) => (&path[..pos], &path[pos + 1..]),
            None => ("", path),
        }
    }

    fn has_dir(&self, dir: &str) -> bool {
        dir.is_empty() || self.dirs.contains(dir)
    }

    fn direct_children(&self, dir: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        for d in &self.dirs {
            if let Some(rest) = d.strip_prefix(&prefix)
                && !rest.is_empty()
                && !rest.contains('/')
            {
                out.insert(rest.to_string());
            }
        }
        for f in self.files.keys() {
            if let Some(rest) = f.strip_prefix(&prefix)
                && !rest.is_empty()
                && !rest.contains('/')
            {
                out.insert(rest.to_string());
            }
        }
        out
    }

    fn is_dir_empty(&self, dir: &str) -> bool {
        self.direct_children(dir).is_empty()
    }

    fn write(&mut self, path: &str, offset: usize, data: &[u8]) {
        let buf = self.files.get_mut(path).expect("file must exist in model");
        let end = offset + data.len();
        if end > buf.len() {
            buf.resize(end, 0);
        }
        buf[offset..end].copy_from_slice(data);
    }

    fn truncate(&mut self, path: &str, new_len: usize) {
        let buf = self.files.get_mut(path).expect("file must exist in model");
        buf.resize(new_len, 0);
    }

    fn rename_entry(&mut self, old_path: &str, new_path: &str) -> Result<(), &'static str> {
        let (new_parent, _) = Self::split_parent(new_path);
        if !self.has_dir(new_parent) {
            return Err("missing target parent");
        }
        if old_path == new_path {
            return Ok(());
        }
        if self.files.contains_key(old_path) {
            if self.dirs.contains(new_path) {
                return Err("cannot overwrite dir with file");
            }
            let payload = self.files.remove(old_path).unwrap();
            self.files.insert(new_path.to_string(), payload);
            return Ok(());
        }
        if self.dirs.contains(old_path) {
            let old_prefix = format!("{old_path}/");
            if new_path.starts_with(&old_prefix) {
                return Err("cycle");
            }
            if self.files.contains_key(new_path) {
                return Err("cannot overwrite file with dir");
            }
            if self.dirs.contains(new_path) && !self.is_dir_empty(new_path) {
                return Err("target dir not empty");
            }
            let mut moved_dirs = Vec::new();
            for d in &self.dirs {
                if d == old_path || d.starts_with(&old_prefix) {
                    moved_dirs.push(d.clone());
                }
            }
            for d in &moved_dirs {
                self.dirs.remove(d);
            }
            for d in moved_dirs {
                let suffix = &d[old_path.len()..];
                self.dirs.insert(format!("{new_path}{suffix}"));
            }
            let mut moved_files = Vec::new();
            for (f, data) in &self.files {
                if f.starts_with(&old_prefix) {
                    moved_files.push((f.clone(), data.clone()));
                }
            }
            for (f, _) in &moved_files {
                self.files.remove(f);
            }
            for (f, data) in moved_files {
                let suffix = &f[old_path.len()..];
                self.files.insert(format!("{new_path}{suffix}"), data);
            }
            return Ok(());
        }
        Err("missing source")
    }

    fn verify_against(&self, dm: &DiskManager, context: &str) {
        let root = dm.superblock().root_inode;
        let report = dm
            .verify_integrity()
            .unwrap_or_else(|e| panic!("[{context}] verify_integrity error: {e}"));
        assert!(
            report.is_clean,
            "[{context}] fsck failed: orphans={:?}, leaked={:?}, missing={:?}, cross={:?}, corrupted={:?}",
            report.orphan_inodes,
            report.leaked_blocks,
            report.missing_blocks,
            report.cross_linked_blocks,
            report.corrupted_inodes
        );

        let mut actual_dirs = BTreeSet::new();
        let mut actual_files = BTreeMap::new();
        collect_actual_tree(dm, root, "", &mut actual_dirs, &mut actual_files, context);

        assert_eq!(
            actual_dirs, self.dirs,
            "[{context}] directory tree mismatch"
        );
        assert_eq!(
            actual_files.keys().collect::<Vec<_>>(),
            self.files.keys().collect::<Vec<_>>(),
            "[{context}] file key set mismatch"
        );
        for (path, expected) in &self.files {
            let got = &actual_files[path];
            assert_eq!(
                got.len(),
                expected.len(),
                "[{context}] length mismatch for '{path}': got {} expected {}",
                got.len(),
                expected.len()
            );
            if got != expected {
                let diff_pos = got
                    .iter()
                    .zip(expected.iter())
                    .position(|(a, b)| a != b)
                    .unwrap_or(0);
                panic!(
                    "[{context}] payload mismatch for '{path}' at byte {diff_pos}: got {:02x?} vs expected {:02x?}",
                    &got[diff_pos..got.len().min(diff_pos + 16)],
                    &expected[diff_pos..expected.len().min(diff_pos + 16)]
                );
            }
        }
    }
}

fn collect_actual_tree(
    dm: &DiskManager,
    dir_ino: u64,
    prefix: &str,
    dirs: &mut BTreeSet<String>,
    files: &mut BTreeMap<String, Vec<u8>>,
    context: &str,
) {
    let entries = dm
        .list_dir(dir_ino)
        .unwrap_or_else(|e| panic!("[{context}] list_dir({dir_ino}, '{prefix}') failed: {e}"));
    for entry in entries {
        let full = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        let ino = dm
            .read_inode(entry.inode)
            .unwrap_or_else(|e| panic!("[{context}] read_inode({}) for '{full}': {e}", entry.inode));
        match ino.mode {
            FileType::Directory => {
                assert!(dirs.insert(full.clone()), "[{context}] duplicate dir '{full}'");
                collect_actual_tree(dm, entry.inode, &full, dirs, files, context);
            }
            FileType::File => {
                let data = dm
                    .read_data(entry.inode)
                    .unwrap_or_else(|e| panic!("[{context}] read_data('{full}') failed: {e}"));
                assert!(
                    files.insert(full.clone(), data).is_none(),
                    "[{context}] duplicate file '{full}'"
                );
            }
        }
    }
}

fn resolve_dir_inode(dm: &DiskManager, dir: &str) -> u64 {
    if dir.is_empty() {
        dm.superblock().root_inode
    } else {
        dm.resolve_path(dir).unwrap()
    }
}

fn open_mode(
    path: &Path,
    size: u64,
    password: Option<&str>,
    journaled: bool,
    mode: StorageMode,
) -> DiskManager {
    DiskManager::open_with_options(path, size, password, journaled, mode).unwrap()
}

fn pick_compression_mode(rng: &mut XorShift64) -> CompressionMode {
    match rng.next_usize(4) {
        0 => CompressionMode::Never,
        1 => CompressionMode::Auto,
        2 => CompressionMode::Always,
        _ => CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        },
    }
}

#[test]
fn test_stateful_differential_fsx_single_file_modes() {
    let configs = [
        (StorageMode::Mmap, false, None, 0xF5A1_0001u64),
        (StorageMode::FileIo, true, None, 0xF5A1_0002u64),
        (StorageMode::Mmap, true, Some("fsx_pw_mmap"), 0xF5A1_0003u64),
        (StorageMode::FileIo, true, Some("fsx_pw_fio"), 0xF5A1_0004u64),
    ];

    for (mut cur_mode, journaled, password, seed) in configs {
        let dir = tempdir().unwrap();
        let img_path = dir.path().join("fsx_single.img");
        let mut rng = XorShift64::new(seed);
        let max_file = 320 * 1024usize;

        let mut dm = open_mode(&img_path, 32 * 1024 * 1024, password, journaled, cur_mode);
        let derived_key = password.map(|pw| {
            let sb = dm.superblock();
            oifs::encryption::derive_key(pw, &sb.encryption_salt).unwrap()
        });
        let root = dm.superblock().root_inode;
        let mut fid = dm.create_file(root, "target.bin").unwrap();
        let mut oracle: Vec<u8> = Vec::new();
        let mut history: Vec<String> = Vec::new();

        for step in 0..250usize {
            match rng.next_usize(10) {
                // Random write (including boundary crossings: 4K blocks, 40K SIB, 64K chunks)
                0..=4 => {
                    let offset = if rng.next_usize(5) == 0 {
                        0
                    } else {
                        rng.next_usize(max_file.saturating_sub(4096))
                    };
                    let len = if rng.next_usize(6) == 0 {
                        (1 + rng.next_usize(4)) * 4096
                    } else {
                        1 + rng.next_usize(24 * 1024)
                    };
                    let len = len.min(max_file - offset);
                    let comp = pick_compression_mode(&mut rng);
                    let use_filter =
                        offset == 0 && len >= 64 && len.is_multiple_of(4) && rng.next_usize(4) == 0;
                    let filter = if use_filter {
                        if rng.next_usize(2) == 0 {
                            FilterConfig::numeric(4)
                        } else {
                            FilterConfig::bitshuffle_only(4)
                        }
                    } else {
                        FilterConfig::none()
                    };
                    let payload = rng.fill_pattern(len, comp != CompressionMode::Never);
                    history.push(format!(
                        "step={step} write(off={offset}, len={len}, comp={comp:?}, filter={filter:?})"
                    ));
                    if use_filter {
                        dm.write_data_with_filters(fid, offset as u64, &payload, comp, filter)
                            .unwrap_or_else(|e| panic!("seed={seed} {}: {e}", history.last().unwrap()));
                    } else {
                        dm.write_data(fid, offset as u64, &payload, comp)
                            .unwrap_or_else(|e| panic!("seed={seed} {}: {e}", history.last().unwrap()));
                    }
                    let end = offset + len;
                    if end > oracle.len() {
                        oracle.resize(end, 0);
                    }
                    oracle[offset..end].copy_from_slice(&payload);
                }
                // Truncate (shrink or sparse expand)
                5..=6 => {
                    let new_len = match rng.next_usize(4) {
                        0 => 0,
                        1 => rng.next_usize(oracle.len().max(1)),
                        2 => oracle.len() + rng.next_usize(32 * 1024),
                        _ => (1 + rng.next_usize(4)) * 65536,
                    }
                    .min(max_file);
                    history.push(format!("step={step} truncate({new_len})"));
                    dm.truncate(fid, new_len as u64)
                        .unwrap_or_else(|e| panic!("seed={seed} {}: {e}", history.last().unwrap()));
                    oracle.resize(new_len, 0);
                }
                // Random positional read_at (including past-EOF & cross-boundary)
                7..=8 => {
                    let max_off = oracle.len() + 2048;
                    let off = rng.next_usize(max_off);
                    let rlen = rng.next_usize(48 * 1024);
                    let mut buf = vec![0xA5u8; rlen];
                    let got = dm
                        .read_at(fid, off as u64, &mut buf)
                        .unwrap_or_else(|e| panic!("seed={seed} step={step} read_at({off}, {rlen}): {e}"));
                    let exp_n = oracle.len().saturating_sub(off).min(rlen);
                    assert_eq!(
                        got, exp_n,
                        "seed={seed} step={step} read_at len mismatch (off={off}, req={rlen}, oracle={})",
                        oracle.len()
                    );
                    if got > 0 {
                        assert_eq!(
                            &buf[..got],
                            &oracle[off..off + got],
                            "seed={seed} step={step} read_at data mismatch at off={off}, got={got}, history={:?}",
                            &history[history.len().saturating_sub(12)..]
                        );
                    }
                }
                // Defragment + close/reopen swapping StorageMode
                _ => {
                    let path_s = dm.backing_path().to_str().unwrap().to_string();
                    history.push(format!("step={step} defrag+reopen({cur_mode:?})"));
                    dm.defragment(&path_s, DefragMode::InPlace, None).unwrap();
                    dm.flush().unwrap();
                    drop(dm);
                    cur_mode = match cur_mode {
                        StorageMode::Mmap => StorageMode::FileIo,
                        StorageMode::FileIo => StorageMode::Mmap,
                    };
                    dm = if let Some(ref k) = derived_key {
                        DiskManager::open_with_key_and_storage_mode(
                            &img_path,
                            0,
                            Some(k.clone()),
                            cur_mode,
                        )
                        .unwrap()
                    } else {
                        open_mode(&img_path, 0, None, journaled, cur_mode)
                    };
                    fid = dm.lookup(dm.superblock().root_inode, "target.bin").unwrap();
                    let full = dm.read_data(fid).unwrap();
                    assert_eq!(
                        full,
                        oracle,
                        "seed={seed} step={step} full read mismatch after defrag+reopen, history={:?}",
                        &history[history.len().saturating_sub(12)..]
                    );
                }
            }

            if step % 25 == 24 {
                let full = dm.read_data(fid).unwrap();
                assert_eq!(full.len(), oracle.len(), "seed={seed} step={step} len");
                assert_eq!(full, oracle, "seed={seed} step={step} content");
                assert!(dm.verify_integrity().unwrap().is_clean);
            }
        }

        let final_bytes = dm.read_data(fid).unwrap();
        assert_eq!(final_bytes, oracle);
        assert!(dm.verify_integrity().unwrap().is_clean);
        if let Some(pw) = password {
            drop(dm);
            let dm_pw = open_mode(&img_path, 0, Some(pw), journaled, cur_mode);
            let pw_fid = dm_pw.lookup(dm_pw.superblock().root_inode, "target.bin").unwrap();
            assert_eq!(dm_pw.read_data(pw_fid).unwrap(), oracle);
        }
    }
}

#[test]
fn test_stateful_differential_full_filesystem_oracle_plain_and_journaled() {
    for (mut cur_mode, journaled, seed) in [
        (StorageMode::Mmap, true, 0xD1FF_1001u64),
        (StorageMode::FileIo, true, 0xD1FF_1002u64),
        (StorageMode::Mmap, false, 0xD1FF_1003u64),
    ] {
        let dir = tempdir().unwrap();
        let img_path = dir.path().join("full_oracle.img");
        let mut rng = XorShift64::new(seed);
        let mut dm = open_mode(&img_path, 6 * 1024 * 1024, None, journaled, cur_mode);
        dm.set_auto_grow(
            AutoGrowConfig::enabled()
                .with_growth_factor(1.5)
                .with_min_increment_bytes(2 * 1024 * 1024)
                .with_max_size_bytes(24 * 1024 * 1024),
        );

        let mut model = ModelFs::new();
        let mut seq = 0u64;

        for step in 0..350usize {
            let op = rng.next_usize(10);
            match op {
                // Mkdir in root or random existing dir
                0 => {
                    if model.dirs.len() < 18 {
                        let parent_rel = if model.dirs.is_empty() || rng.next_usize(2) == 0 {
                            String::new()
                        } else {
                            let idx = rng.next_usize(model.dirs.len());
                            model.dirs.iter().nth(idx).unwrap().clone()
                        };
                        seq += 1;
                        let name = format!("d_{seq}");
                        let full = if parent_rel.is_empty() {
                            name.clone()
                        } else {
                            format!("{parent_rel}/{name}")
                        };
                        let p_ino = resolve_dir_inode(&dm, &parent_rel);
                        dm.create_directory(p_ino, &name).unwrap();
                        model.dirs.insert(full);
                    }
                }
                // Create file with data
                1..=2 => {
                    if model.files.len() < 48 {
                        let parent_rel = if model.dirs.is_empty() || rng.next_usize(2) == 0 {
                            String::new()
                        } else {
                            let idx = rng.next_usize(model.dirs.len());
                            model.dirs.iter().nth(idx).unwrap().clone()
                        };
                        seq += 1;
                        let name = format!("f_{seq}.dat");
                        let full = if parent_rel.is_empty() {
                            name.clone()
                        } else {
                            format!("{parent_rel}/{name}")
                        };
                        let comp = pick_compression_mode(&mut rng);
                        let len = if rng.next_usize(5) == 0 {
                            0
                        } else if matches!(comp, CompressionMode::Seekable { .. }) {
                            16 * 1024 + rng.next_usize(90 * 1024)
                        } else {
                            1 + rng.next_usize(20 * 1024)
                        };
                        let compressible = rng.next_usize(2) == 0;
                        let data = rng.fill_pattern(len, compressible);
                        let p_ino = resolve_dir_inode(&dm, &parent_rel);
                        dm.create_file_with_data(p_ino, &name, &data, comp).unwrap();
                        model.files.insert(full, data);
                    }
                }
                // Write at offset on existing file
                3 => {
                    if !model.files.is_empty() {
                        let f_idx = rng.next_usize(model.files.len());
                        let f_path = model.files.keys().nth(f_idx).unwrap().clone();
                        let cur_len = model.files[&f_path].len();
                        let off = rng.next_usize(cur_len + 4096).min(180 * 1024);
                        let len = (1 + rng.next_usize(12 * 1024)).min(200 * 1024 - off);
                        let comp = pick_compression_mode(&mut rng);
                        let payload = rng.fill_pattern(len, comp != CompressionMode::Never);
                        let ino = dm.resolve_path(&f_path).unwrap();
                        dm.write_data(ino, off as u64, &payload, comp).unwrap();
                        model.write(&f_path, off, &payload);
                    }
                }
                // Truncate existing file
                4 => {
                    if !model.files.is_empty() {
                        let f_idx = rng.next_usize(model.files.len());
                        let f_path = model.files.keys().nth(f_idx).unwrap().clone();
                        let cur_len = model.files[&f_path].len();
                        let new_len = match rng.next_usize(3) {
                            0 => 0,
                            1 => rng.next_usize(cur_len.max(1)),
                            _ => (cur_len + rng.next_usize(8192)).min(160 * 1024),
                        };
                        let ino = dm.resolve_path(&f_path).unwrap();
                        dm.truncate(ino, new_len as u64).unwrap();
                        model.truncate(&f_path, new_len);
                    }
                }
                // Rename / Move (plus overwrite & self-rename)
                5 => {
                    if !model.files.is_empty() {
                        let f_idx = rng.next_usize(model.files.len());
                        let old_path = model.files.keys().nth(f_idx).unwrap().clone();
                        let (old_parent_rel, old_name) = ModelFs::split_parent(&old_path);
                        let old_p_ino = resolve_dir_inode(&dm, old_parent_rel);
                        if rng.next_usize(8) == 0 {
                            // Self rename must be a clean no-op
                            dm.rename(old_p_ino, old_name, old_p_ino, old_name).unwrap();
                        } else {
                            let new_parent_rel = if model.dirs.is_empty() || rng.next_usize(2) == 0 {
                                String::new()
                            } else {
                                let d_idx = rng.next_usize(model.dirs.len());
                                model.dirs.iter().nth(d_idx).unwrap().clone()
                            };
                            let new_p_ino = resolve_dir_inode(&dm, &new_parent_rel);
                            let new_name = if model.files.len() > 2 && rng.next_usize(4) == 0 {
                                // Overwrite existing file in same or target dir
                                let candidates: Vec<String> = model
                                    .files
                                    .keys()
                                    .filter(|p| {
                                        ModelFs::split_parent(p).0 == new_parent_rel
                                            && *p != &old_path
                                    })
                                    .map(|p| ModelFs::split_parent(p).1.to_string())
                                    .collect();
                                if let Some(victim) = candidates.first() {
                                    victim.clone()
                                } else {
                                    seq += 1;
                                    format!("mv_{seq}.dat")
                                }
                            } else {
                                seq += 1;
                                format!("mv_{seq}.dat")
                            };
                            let new_path = if new_parent_rel.is_empty() {
                                new_name.clone()
                            } else {
                                format!("{new_parent_rel}/{new_name}")
                            };
                            dm.rename(old_p_ino, old_name, new_p_ino, &new_name).unwrap();
                            model.rename_entry(&old_path, &new_path).unwrap();
                        }
                    }
                }
                // Delete file or empty dir
                6 => {
                    if !model.files.is_empty() && rng.next_usize(4) != 0 {
                        let f_idx = rng.next_usize(model.files.len());
                        let f_path = model.files.keys().nth(f_idx).unwrap().clone();
                        let (p_rel, name) = ModelFs::split_parent(&f_path);
                        let p_ino = resolve_dir_inode(&dm, p_rel);
                        dm.delete_file(p_ino, name).unwrap();
                        model.files.remove(&f_path);
                    } else if let Some(empty_dir) =
                        model.dirs.iter().find(|d| model.is_dir_empty(d)).cloned()
                    {
                        let (p_rel, name) = ModelFs::split_parent(&empty_dir);
                        let p_ino = resolve_dir_inode(&dm, p_rel);
                        dm.delete_file(p_ino, name).unwrap();
                        model.dirs.remove(&empty_dir);
                    }
                }
                // WriteBatch (mixed ops + periodic failing rollback batch)
                7 => {
                    let parent_rel = if model.dirs.is_empty() || rng.next_usize(2) == 0 {
                        String::new()
                    } else {
                        let idx = rng.next_usize(model.dirs.len());
                        model.dirs.iter().nth(idx).unwrap().clone()
                    };
                    let p_ino = resolve_dir_inode(&dm, &parent_rel);
                    let fail_batch = rng.next_usize(4) == 0;
                    let mut wb = dm.begin_write_batch();
                    let mut staged_files = Vec::new();
                    for _ in 0..3 {
                        seq += 1;
                        let name = format!("b_{seq}.dat");
                        let full = if parent_rel.is_empty() {
                            name.clone()
                        } else {
                            format!("{parent_rel}/{name}")
                        };
                        let b_len = 128 + rng.next_usize(4096);
                        let data = rng.fill_pattern(b_len, true);
                        wb.create_file_with_data(p_ino, &name, data.clone(), CompressionMode::Auto);
                        staged_files.push((full, data));
                    }
                    if fail_batch {
                        wb.delete(p_ino, "definitely_missing_rollback_target.xyz");
                        assert!(wb.commit().is_err());
                    } else {
                        let res = wb.commit().unwrap();
                        assert_eq!(res.ops_executed, 3);
                        for (full, data) in staged_files {
                            model.files.insert(full, data);
                        }
                    }
                }
                // Defragment / Vacuum / Grow
                8 => {
                    let backing = dm.backing_path().to_str().unwrap().to_string();
                    dm.defragment(&backing, DefragMode::InPlace, None).unwrap();
                    if rng.next_usize(2) == 0 {
                        dm.vacuum(
                            VacuumOptions::default()
                                .with_min_headroom_bytes(1024 * 1024)
                                .with_min_image_bytes(4 * 1024 * 1024),
                        )
                        .unwrap();
                    }
                }
                // Close & Reopen alternating StorageMode
                _ => {
                    dm.flush().unwrap();
                    let saved_ag = dm.auto_grow_config();
                    drop(dm);
                    cur_mode = match cur_mode {
                        StorageMode::Mmap => StorageMode::FileIo,
                        StorageMode::FileIo => StorageMode::Mmap,
                    };
                    dm = open_mode(&img_path, 0, None, journaled, cur_mode);
                    dm.set_auto_grow(saved_ag);
                }
            }

            // Periodic read_at spot check
            if !model.files.is_empty() && step.is_multiple_of(5) {
                let f_idx = rng.next_usize(model.files.len());
                let (f_path, expected) = model.files.iter().nth(f_idx).unwrap();
                let ino = dm.resolve_path(f_path).unwrap();
                let off = if expected.is_empty() {
                    0
                } else {
                    rng.next_usize(expected.len())
                };
                let rlen = 256 + rng.next_usize(4096);
                let mut got = vec![0u8; rlen];
                let n = dm.read_at(ino, off as u64, &mut got).unwrap();
                let exp_n = expected.len().saturating_sub(off).min(rlen);
                assert_eq!(n, exp_n);
                assert_eq!(&got[..n], &expected[off..off + n]);
            }

            if step % 35 == 34 {
                model.verify_against(&dm, &format!("seed={seed} step={step}"));
            }
        }

        // Negative directory cycle & non-empty directory rename invariants
        if let Some(non_empty_dir) = model.dirs.iter().find(|d| !model.is_dir_empty(d)).cloned() {
            let root = dm.superblock().root_inode;
            let victim_dir = dm.create_directory(root, "empty_src_for_neg").unwrap();
            let (np_rel, n_name) = ModelFs::split_parent(&non_empty_dir);
            let np_ino = resolve_dir_inode(&dm, np_rel);
            assert!(matches!(
                dm.rename(root, "empty_src_for_neg", np_ino, n_name),
                Err(DiskManagerError::Io(_))
            ));
            let _ = victim_dir;
            dm.delete_file(root, "empty_src_for_neg").unwrap();
        }

        model.verify_against(&dm, &format!("seed={seed} final"));
    }
}

#[test]
fn test_stateful_differential_encrypted_with_dict_and_reopen() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("enc_dict_oracle.img");
    let password = "OracleEncryptedDict#2026";
    let mut rng = XorShift64::new(0xE9C0_D1C7u64);

    // Train a deterministic dictionary
    let samples: Vec<Vec<u8>> = (0..24u32)
        .map(|i| {
            format!(
                "{{\"kind\":\"agent_trace\",\"step\":{i},\"zone\":\"us-west-2\",\"status\":\"ok\",\"payload\":\"repeated_json_structure\"}}"
            )
            .into_bytes()
        })
        .collect();
    let dict = DiskManager::train_zstd_dict(&samples, 4096).unwrap();

    let mut cur_mode = StorageMode::Mmap;
    let mut dm = DiskManager::create_encrypted_journaled(&img_path, 16 * 1024 * 1024, password).unwrap();
    let enc_key = oifs::encryption::derive_key(password, &dm.superblock().encryption_salt).unwrap();
    let dict_id = dm.register_and_use_zstd_dict(&dict).unwrap();

    let mut model = ModelFs::new();
    let mut seq = 0u64;

    for step in 0..200usize {
        match rng.next_usize(7) {
            0 => {
                if model.dirs.len() < 10 {
                    let parent_rel = if model.dirs.is_empty() || rng.next_usize(2) == 0 {
                        String::new()
                    } else {
                        model
                            .dirs
                            .iter()
                            .nth(rng.next_usize(model.dirs.len()))
                            .unwrap()
                            .clone()
                    };
                    seq += 1;
                    let name = format!("ed_{seq}");
                    let full = if parent_rel.is_empty() {
                        name.clone()
                    } else {
                        format!("{parent_rel}/{name}")
                    };
                    let p_ino = resolve_dir_inode(&dm, &parent_rel);
                    dm.create_directory(p_ino, &name).unwrap();
                    model.dirs.insert(full);
                }
            }
            1..=2 => {
                if model.files.len() < 36 {
                    let parent_rel = if model.dirs.is_empty() || rng.next_usize(2) == 0 {
                        String::new()
                    } else {
                        model
                            .dirs
                            .iter()
                            .nth(rng.next_usize(model.dirs.len()))
                            .unwrap()
                            .clone()
                    };
                    seq += 1;
                    let name = format!("ef_{seq}.json");
                    let full = if parent_rel.is_empty() {
                        name.clone()
                    } else {
                        format!("{parent_rel}/{name}")
                    };
                    let data = if rng.next_usize(2) == 0 {
                        samples[rng.next_usize(samples.len())].clone()
                    } else {
                        let p_len = 256 + rng.next_usize(80 * 1024);
                        rng.fill_pattern(p_len, true)
                    };
                    let comp = if data.len() > 32 * 1024 {
                        CompressionMode::Seekable {
                            chunk_size: 65536,
                            level: 1,
                        }
                    } else {
                        CompressionMode::Auto
                    };
                    let p_ino = resolve_dir_inode(&dm, &parent_rel);
                    dm.create_file_with_data(p_ino, &name, &data, comp).unwrap();
                    model.files.insert(full, data);
                }
            }
            3 => {
                if !model.files.is_empty() {
                    let f_path = model
                        .files
                        .keys()
                        .nth(rng.next_usize(model.files.len()))
                        .unwrap()
                        .clone();
                    let cur_len = model.files[&f_path].len();
                    let off = rng.next_usize(cur_len + 2048).min(120 * 1024);
                    let len = 1 + rng.next_usize(8 * 1024);
                    let payload = rng.fill_pattern(len, true);
                    let ino = dm.resolve_path(&f_path).unwrap();
                    dm.write_data(ino, off as u64, &payload, CompressionMode::Auto)
                        .unwrap();
                    model.write(&f_path, off, &payload);
                }
            }
            4 => {
                if !model.files.is_empty() {
                    let old_path = model
                        .files
                        .keys()
                        .nth(rng.next_usize(model.files.len()))
                        .unwrap()
                        .clone();
                    let new_parent_rel = if model.dirs.is_empty() || rng.next_usize(2) == 0 {
                        String::new()
                    } else {
                        model
                            .dirs
                            .iter()
                            .nth(rng.next_usize(model.dirs.len()))
                            .unwrap()
                            .clone()
                    };
                    seq += 1;
                    let new_name = format!("ren_{seq}.json");
                    let new_path = if new_parent_rel.is_empty() {
                        new_name.clone()
                    } else {
                        format!("{new_parent_rel}/{new_name}")
                    };
                    let (old_p_rel, old_name) = ModelFs::split_parent(&old_path);
                    let old_p = resolve_dir_inode(&dm, old_p_rel);
                    let new_p = resolve_dir_inode(&dm, &new_parent_rel);
                    dm.rename(old_p, old_name, new_p, &new_name).unwrap();
                    model.rename_entry(&old_path, &new_path).unwrap();
                }
            }
            5 => {
                if !model.files.is_empty() {
                    let f_path = model
                        .files
                        .keys()
                        .nth(rng.next_usize(model.files.len()))
                        .unwrap()
                        .clone();
                    let (p_rel, name) = ModelFs::split_parent(&f_path);
                    let p_ino = resolve_dir_inode(&dm, p_rel);
                    dm.delete_file(p_ino, name).unwrap();
                    model.files.remove(&f_path);
                }
            }
            _ => {
                let backing = dm.backing_path().to_str().unwrap().to_string();
                dm.defragment(&backing, DefragMode::InPlace, None).unwrap();
                dm.flush().unwrap();
                drop(dm);
                cur_mode = match cur_mode {
                    StorageMode::Mmap => StorageMode::FileIo,
                    StorageMode::FileIo => StorageMode::Mmap,
                };
                dm = DiskManager::open_with_key_and_storage_mode(
                    &img_path,
                    0,
                    Some(enc_key.clone()),
                    cur_mode,
                )
                .unwrap();
                assert_eq!(dm.register_and_use_zstd_dict(&dict).unwrap(), dict_id);
            }
        }

        if step % 25 == 24 {
            model.verify_against(&dm, &format!("enc_dict step={step}"));
        }
    }

    model.verify_against(&dm, "enc_dict final");
    drop(dm);
    let dm_pw = open_mode(&img_path, 0, Some(password), true, cur_mode);
    assert_eq!(dm_pw.register_and_use_zstd_dict(&dict).unwrap(), dict_id);
    model.verify_against(&dm_pw, "enc_dict password_reopen");
}

#[test]
fn test_stateful_differential_cross_image_transfer_and_splice() {
    let dir = tempdir().unwrap();
    let src_path = dir.path().join("xfer_src.img");
    let dst_path = dir.path().join("xfer_dst.img");
    let mut rng = XorShift64::new(0x5B1C_CAFEu64);

    let src_dm = DiskManager::open_journaled(&src_path, 16 * 1024 * 1024).unwrap();
    let dst_dm = DiskManager::open_journaled(&dst_path, 16 * 1024 * 1024).unwrap();
    src_dm.set_durability_mode(DurabilityMode::Strict);
    dst_dm.set_durability_mode(DurabilityMode::Strict);

    let mut src_model = ModelFs::new();
    let mut dst_model = ModelFs::new();
    let src_root = src_dm.superblock().root_inode;
    let dst_root = dst_dm.superblock().root_inode;

    for i in 0..8usize {
        let name = format!("f_{i}.bin");
        let len = (2 + (i % 4)) * BLOCK_SIZE * 2 + 513;
        let data = rng.fill_pattern(len, i.is_multiple_of(2));
        let comp = if i.is_multiple_of(3) {
            CompressionMode::Seekable {
                chunk_size: 65536,
                level: 1,
            }
        } else if i.is_multiple_of(2) {
            CompressionMode::Auto
        } else {
            CompressionMode::Never
        };
        src_dm
            .create_file_with_data(src_root, &name, &data, comp)
            .unwrap();
        src_model.files.insert(name, data);
    }

    for step in 0..100usize {
        match rng.next_usize(4) {
            // Transfer random source file -> dst (copy / move / overwrite)
            0..=1 if !src_model.files.is_empty() => {
                let s_name = src_model
                    .files
                    .keys()
                    .nth(rng.next_usize(src_model.files.len()))
                    .unwrap()
                    .clone();
                let do_move = src_model.files.len() > 3 && rng.next_usize(4) == 0;
                let (d_name, overwrite) = if !dst_model.files.is_empty() && rng.next_usize(3) == 0 {
                    (
                        dst_model
                            .files
                            .keys()
                            .nth(rng.next_usize(dst_model.files.len()))
                            .unwrap()
                            .clone(),
                        true,
                    )
                } else {
                    (format!("t_{step}.bin"), false)
                };
                src_dm
                    .transfer_file_to(
                        src_root,
                        &s_name,
                        &dst_dm,
                        dst_root,
                        &d_name,
                        TransferOptions {
                            move_file: do_move,
                            overwrite,
                        },
                    )
                    .unwrap();
                let payload = if do_move {
                    src_model.files.remove(&s_name).unwrap()
                } else {
                    src_model.files[&s_name].clone()
                };
                dst_model.files.insert(d_name, payload);
            }
            // Splice sub-range across images (Keep / PunchHole / CollapseRange)
            2 if !src_model.files.is_empty() && !dst_model.files.is_empty() => {
                let s_name = src_model
                    .files
                    .keys()
                    .nth(rng.next_usize(src_model.files.len()))
                    .unwrap()
                    .clone();
                let d_name = dst_model
                    .files
                    .keys()
                    .nth(rng.next_usize(dst_model.files.len()))
                    .unwrap()
                    .clone();
                let s_len = src_model.files[&s_name].len();
                if s_len > 0 {
                    let s_off = rng.next_usize(s_len);
                    let len = 1 + rng.next_usize(s_len - s_off).min(32 * 1024);
                    let d_len = dst_model.files[&d_name].len();
                    let d_off = rng.next_usize(d_len + 4096);
                    let trim_mode = match rng.next_usize(3) {
                        0 => SpliceTrimMode::Keep,
                        1 => SpliceTrimMode::PunchHole,
                        _ => SpliceTrimMode::CollapseRange,
                    };
                    let s_ino = src_dm.lookup(src_root, &s_name).unwrap();
                    let d_ino = dst_dm.lookup(dst_root, &d_name).unwrap();
                    src_dm
                        .splice_range_to(
                            s_ino,
                            s_off as u64,
                            &dst_dm,
                            d_ino,
                            d_off as u64,
                            len as u64,
                            SpliceOptions { trim_mode },
                        )
                        .unwrap();

                    let slice = src_model.files[&s_name][s_off..s_off + len].to_vec();
                    dst_model.write(&d_name, d_off, &slice);
                    match trim_mode {
                        SpliceTrimMode::Keep => {}
                        SpliceTrimMode::PunchHole => {
                            src_model.files.get_mut(&s_name).unwrap()[s_off..s_off + len].fill(0);
                        }
                        SpliceTrimMode::CollapseRange => {
                            src_model
                                .files
                                .get_mut(&s_name)
                                .unwrap()
                                .drain(s_off..s_off + len);
                        }
                    }
                }
            }
            // Keep source file pool rich with occasional overwrite
            _ => {
                let s_name = format!("seed_{}.bin", step % 6);
                let len = 4096 + rng.next_usize(64 * 1024);
                let data = rng.fill_pattern(len, true);
                if src_model.files.contains_key(&s_name) {
                    let ino = src_dm.lookup(src_root, &s_name).unwrap();
                    src_dm
                        .write_data_truncated(ino, &data, CompressionMode::Auto)
                        .unwrap();
                } else {
                    src_dm
                        .create_file_with_data(src_root, &s_name, &data, CompressionMode::Auto)
                        .unwrap();
                }
                src_model.files.insert(s_name, data);
            }
        }
    }

    src_model.verify_against(&src_dm, "transfer_splice src");
    dst_model.verify_against(&dst_dm, "transfer_splice dst");
}
