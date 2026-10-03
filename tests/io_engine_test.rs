//! P3.2: payload-read engine tests.
//!
//! Every available backend (`Mmap`, `Pread`, and `IoUring` where the kernel supports it)
//! must return byte-identical results for every file shape and API.

use oifs::disk::{CompressionMode, DiskManager, DurabilityMode, ReadRequest};
use oifs::{FilterConfig, IoBackend};
use std::fs;
use std::path::Path;

struct TestDisk {
    path: String,
}

impl TestDisk {
    fn new(name: &str) -> Self {
        let path = format!("target/{}_{}.img", name, std::process::id());
        if Path::new(&path).exists() {
            let _ = fs::remove_file(&path);
        }
        Self { path }
    }
}

impl Drop for TestDisk {
    fn drop(&mut self) {
        if Path::new(&self.path).exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Backends that can actually run here (IoUring only on capable Linux kernels).
fn backends() -> Vec<IoBackend> {
    IoBackend::ALL
        .into_iter()
        .filter(|b| b.is_supported())
        .collect()
}

/// Deterministic, non-repeating-per-block payload.
fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// Builds an image with files covering every addressing level and encoding.
/// Returns `(name, inode, expected logical content)`.
fn populate(dm: &DiskManager) -> Vec<(&'static str, u64, Vec<u8>)> {
    let root = dm.superblock().root_inode;
    let mut files = Vec::new();

    let mut add = |name: &'static str, data: Vec<u8>, mode: CompressionMode| {
        let id = dm.create_file(root, name).unwrap();
        dm.write_data(id, 0, &data, mode).unwrap();
        files.push((name, id, data));
    };
    add("empty.bin", Vec::new(), CompressionMode::Never);
    add("tiny.bin", pattern(17, 1), CompressionMode::Never);
    add(
        "direct.bin",
        pattern(10 * 4096 - 3, 2),
        CompressionMode::Never,
    );
    add(
        "single_indirect.bin",
        pattern(1024 * 1024 + 123, 3),
        CompressionMode::Never,
    );
    add(
        "double_indirect.bin",
        pattern(3 * 1024 * 1024 + 777, 4),
        CompressionMode::Never,
    );
    let compressible: Vec<u8> = (0..300_000u32).map(|i| (i / 100) as u8).collect();
    add("compressed.bin", compressible, CompressionMode::Always);

    // Sparse file: leading hole of 5 blocks, data, then hole, then tail.
    let id = dm.create_file(root, "sparse.bin").unwrap();
    let mid = pattern(4096 * 2 + 10, 5);
    let tail = pattern(4000, 6);
    dm.write_data(id, 5 * 4096 + 100, &mid, CompressionMode::Never)
        .unwrap();
    let tail_off = 40 * 4096 + 7;
    dm.write_data(id, tail_off, &tail, CompressionMode::Never)
        .unwrap();
    let mut sparse = vec![0u8; tail_off as usize + tail.len()];
    sparse[5 * 4096 + 100..5 * 4096 + 100 + mid.len()].copy_from_slice(&mid);
    sparse[tail_off as usize..].copy_from_slice(&tail);
    files.push(("sparse.bin", id, sparse));

    // Filtered (delta + shuffle) + compressed.
    let id = dm.create_file(root, "filtered.bin").unwrap();
    let floats: Vec<u8> = (0..20_000u32)
        .flat_map(|i| (i as f32 * 0.5).to_le_bytes())
        .collect();
    dm.write_data_with_filters(
        id,
        0,
        &floats,
        CompressionMode::Always,
        FilterConfig {
            typesize: 4,
            delta: true,
            shuffle: true,
            bitshuffle: false,
        },
    )
    .unwrap();
    files.push(("filtered.bin", id, floats));

    files
}

#[test]
fn test_default_backend_is_mmap() {
    // The environment override must not leak into this assertion.
    if std::env::var(oifs::io_engine::IO_BACKEND_ENV).is_ok() {
        return;
    }
    let td = TestDisk::new("io_default_backend");
    let dm = DiskManager::open(&td.path, 8 * 1024 * 1024).unwrap();
    assert_eq!(dm.io_backend(), IoBackend::Mmap);
    assert_eq!(dm.requested_io_backend(), IoBackend::Mmap);
}

#[test]
fn test_backend_switching_and_fallback() {
    let td = TestDisk::new("io_backend_switching");
    let dm = DiskManager::open(&td.path, 8 * 1024 * 1024).unwrap();

    assert_eq!(dm.set_io_backend(IoBackend::Pread), IoBackend::Pread);
    assert_eq!(dm.io_backend(), IoBackend::Pread);

    let eff = dm.set_io_backend(IoBackend::IoUring);
    assert_eq!(dm.requested_io_backend(), IoBackend::IoUring);
    if IoBackend::IoUring.is_supported() {
        assert_eq!(eff, IoBackend::IoUring);
    } else {
        // Non-Linux, feature off, or kernel without io_uring: graceful fallback.
        assert_eq!(eff, IoBackend::Pread);
    }
    assert_eq!(dm.io_backend(), eff);

    let dm = dm.with_io_backend(IoBackend::Mmap);
    assert_eq!(dm.io_backend(), IoBackend::Mmap);
}

#[test]
fn test_read_data_identical_across_backends() {
    let td = TestDisk::new("io_read_data_equiv");
    let dm = DiskManager::open(&td.path, 64 * 1024 * 1024).unwrap();
    let files = populate(&dm);

    for backend in backends() {
        dm.set_io_backend(backend);
        for (name, id, expected) in &files {
            let got = dm.read_data(*id).unwrap();
            assert!(
                got == *expected,
                "read_data mismatch: backend={backend} file={name} (got {} bytes, want {})",
                got.len(),
                expected.len()
            );
        }
    }
}

#[test]
fn test_read_at_identical_across_backends() {
    let td = TestDisk::new("io_read_at_equiv");
    let dm = DiskManager::open(&td.path, 64 * 1024 * 1024).unwrap();
    let files = populate(&dm);

    // Offsets/lengths chosen to straddle block boundaries, holes, and EOF.
    let probes: &[(u64, usize)] = &[
        (0, 1),
        (0, 4096),
        (1, 4096),
        (4095, 2),
        (4096 * 5 + 50, 300),
        (4096 * 9 + 4000, 8192),
        (4096 * 10 - 1, 4096 * 3),
        (4096 * 521 + 17, 4096 * 4),
        (100_000, 2_000_000),
        (u64::MAX / 2, 10),
    ];

    for backend in backends() {
        dm.set_io_backend(backend);
        for (name, id, expected) in &files {
            for &(off, len) in probes {
                let mut buf = vec![0xEEu8; len];
                let n = dm.read_at(*id, off, &mut buf).unwrap();
                let start = (off as usize).min(expected.len());
                let end = start.saturating_add(len).min(expected.len());
                assert_eq!(
                    n,
                    end - start,
                    "length mismatch backend={backend} file={name} off={off} len={len}"
                );
                assert!(
                    buf[..n] == expected[start..end],
                    "content mismatch backend={backend} file={name} off={off} len={len}"
                );
                assert!(
                    buf[n..].iter().all(|&b| b == 0xEE),
                    "bytes past the returned length were modified (backend={backend} file={name})"
                );
            }
        }
    }
}

#[test]
fn test_read_at_batch_mixed_requests() {
    let td = TestDisk::new("io_read_at_batch");
    let dm = DiskManager::open(&td.path, 64 * 1024 * 1024).unwrap();
    let files = populate(&dm);
    let root = dm.superblock().root_inode;
    let dir = dm.create_directory(root, "a_dir").unwrap();

    for backend in backends() {
        dm.set_io_backend(backend);

        // One request per file at a few offsets, plus two that must fail
        // (a directory and a nonexistent inode beyond the table).
        let mut specs: Vec<(u64, u64, usize)> = Vec::new();
        for (_, id, _) in &files {
            specs.push((*id, 0, 5000));
            specs.push((*id, 4096 * 3 + 11, 70_000));
        }
        let dir_idx = specs.len();
        specs.push((dir, 0, 16));

        let mut bufs: Vec<Vec<u8>> = specs.iter().map(|&(_, _, l)| vec![0u8; l]).collect();
        let mut reqs: Vec<ReadRequest<'_>> = specs
            .iter()
            .zip(bufs.iter_mut())
            .map(|(&(inode_id, offset, _), buf)| ReadRequest {
                inode_id,
                offset,
                buf: buf.as_mut_slice(),
            })
            .collect();
        let results = dm.read_at_batch(&mut reqs);
        drop(reqs);
        assert_eq!(results.len(), specs.len());

        for (i, (&(id, off, len), res)) in specs.iter().zip(results.iter()).enumerate() {
            if i == dir_idx {
                assert!(res.is_err(), "directory read must fail (backend={backend})");
                continue;
            }
            let expected = &files.iter().find(|f| f.1 == id).unwrap().2;
            let start = (off as usize).min(expected.len());
            let end = (start + len).min(expected.len());
            let n = *res.as_ref().unwrap();
            assert_eq!(
                n,
                end - start,
                "batch length mismatch backend={backend} req={i}"
            );
            assert!(
                bufs[i][..n] == expected[start..end],
                "batch content mismatch backend={backend} req={i}"
            );
        }

        // Each request must agree with the single-shot API.
        for (i, &(id, off, len)) in specs.iter().enumerate() {
            if i == dir_idx {
                continue;
            }
            let mut single = vec![0u8; len];
            let n = dm.read_at(id, off, &mut single).unwrap();
            assert_eq!(&single[..n], &bufs[i][..n]);
        }
    }

    // Empty batch is a no-op.
    assert!(dm.read_at_batch(&mut []).is_empty());
}

#[test]
fn test_encrypted_filesystem_across_backends() {
    let td = TestDisk::new("io_encrypted");
    let dm = DiskManager::create_encrypted(&td.path, 16 * 1024 * 1024, "hunter2").unwrap();
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "secret.bin").unwrap();
    let data = pattern(200_000, 9);
    dm.write_data(id, 0, &data, CompressionMode::Never).unwrap();

    for backend in backends() {
        dm.set_io_backend(backend);
        assert!(dm.read_data(id).unwrap() == data, "backend={backend}");
        let mut buf = vec![0u8; 10_000];
        let n = dm.read_at(id, 50_000, &mut buf).unwrap();
        assert_eq!(n, 10_000);
        assert_eq!(&buf[..], &data[50_000..60_000]);
    }
}

/// Writes go through the shared mmap; syscall-based backends read the same page cache,
/// so un-msynced (Lazy) writes must be visible immediately.
#[test]
fn test_mmap_writes_visible_to_syscall_backends() {
    let td = TestDisk::new("io_coherence");
    let dm = DiskManager::open(&td.path, 16 * 1024 * 1024)
        .unwrap()
        .with_durability_mode(DurabilityMode::Lazy);
    let root = dm.superblock().root_inode;

    for backend in backends() {
        dm.set_io_backend(backend);
        // Fresh file per backend: offset-0 writes overwrite but do not truncate, so the
        // payload only grows across rounds.
        let id = dm.create_file(root, &format!("f_{backend}.bin")).unwrap();
        for round in 0..4u32 {
            let data = pattern(50_000 + round as usize * 4096, round + 100);
            dm.write_data(id, 0, &data, CompressionMode::Never).unwrap();
            assert!(
                dm.read_data(id).unwrap() == data,
                "backend={backend} round={round}"
            );

            // In-place overwrite of a middle range, then positional read.
            let patch = vec![round as u8 ^ 0x5A; 3000];
            dm.write_data(id, 10_000, &patch, CompressionMode::Never)
                .unwrap();
            let mut buf = vec![0u8; 3000];
            assert_eq!(dm.read_at(id, 10_000, &mut buf).unwrap(), 3000);
            assert_eq!(buf, patch, "backend={backend} round={round}");
        }
    }
}

#[test]
fn test_concurrent_readers_on_syscall_backends() {
    let td = TestDisk::new("io_concurrent");
    let dm = DiskManager::open(&td.path, 64 * 1024 * 1024).unwrap();
    let files = std::sync::Arc::new(populate(&dm));

    for backend in backends() {
        dm.set_io_backend(backend);
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let dm = dm.clone();
                let files = files.clone();
                std::thread::spawn(move || {
                    for iter in 0..20usize {
                        let (_, id, expected) = &files[(t + iter) % files.len()];
                        assert!(dm.read_data(*id).unwrap() == *expected);
                        if !expected.is_empty() {
                            let off = (iter * 7919) % expected.len();
                            let mut buf = vec![0u8; 9000];
                            let n = dm.read_at(*id, off as u64, &mut buf).unwrap();
                            let end = (off + 9000).min(expected.len());
                            assert_eq!(&buf[..n], &expected[off..end]);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("reader thread panicked");
        }
    }
}

#[test]
fn test_backend_persists_across_reopen_via_env_only() {
    // A reopened DiskManager starts from OIFS_IO_BACKEND (or Mmap), not the previous
    // instance's runtime choice: the backend is a process-local policy, not on-disk state.
    let td = TestDisk::new("io_reopen");
    {
        let dm = DiskManager::open(&td.path, 8 * 1024 * 1024).unwrap();
        dm.set_io_backend(IoBackend::Pread);
    }
    let dm = DiskManager::open(&td.path, 0).unwrap();
    let expected = IoBackend::from_env().unwrap_or_default().resolve();
    assert_eq!(dm.io_backend(), expected);
}
