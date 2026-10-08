use oifs::disk::{CompressionMode, DiskManager};
use oifs::inode::{CHUNK_SIZE_64K, INODE_FLAG_SEEKABLE_64K};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::tempdir;

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_image_path() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let count = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = dir.path().join(format!("test_chunked_{count}.img"));
    (dir, path)
}

#[test]
fn test_seekable_chunked_write_and_read_roundtrip() {
    let (_dir, img_path) = unique_image_path();
    let dm = DiskManager::open(&img_path, 30 * 1024 * 1024).unwrap();

    let root_id = dm.superblock().root_inode;
    let inode_id = dm.create_file(root_id, "seekable_test.bin").unwrap();

    // 256KB payload (exactly 4 chunks of 64KB)
    let mut payload = vec![0u8; 4 * CHUNK_SIZE_64K];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = ((i * 17 + 3) % 251) as u8;
    }

    dm.write_data(
        inode_id,
        0,
        &payload,
        CompressionMode::Seekable {
            chunk_size: CHUNK_SIZE_64K as u32,
            level: 1,
        },
    )
    .unwrap();

    let inode = dm.read_inode(inode_id).unwrap();
    assert_eq!(inode.size, payload.len() as u64);
    assert_ne!(
        inode.flags & INODE_FLAG_SEEKABLE_64K,
        0,
        "INODE_FLAG_SEEKABLE_64K must be set"
    );
    assert!(
        inode.compressed_size > 0 && inode.compressed_size < inode.size,
        "Compressed size should be smaller than uncompressed: comp={} orig={}",
        inode.compressed_size,
        inode.size
    );

    // 1. Full read
    let read_back = dm.read_data(inode_id).unwrap();
    assert_eq!(read_back, payload);

    // 2. read_at checks
    // Chunk 0 head
    let mut buf = vec![0u8; 100];
    let n = dm.read_at(inode_id, 0, &mut buf).unwrap();
    assert_eq!(n, 100);
    assert_eq!(buf, &payload[0..100]);

    // Cross boundary: chunk 0 -> chunk 1 (offset 65530, len 20)
    let mut buf_cross = vec![0u8; 20];
    let n = dm.read_at(inode_id, 65530, &mut buf_cross).unwrap();
    assert_eq!(n, 20);
    assert_eq!(buf_cross, &payload[65530..65550]);

    // Chunk 2 start
    let mut buf_c2 = vec![0u8; 500];
    let n = dm
        .read_at(inode_id, 2 * CHUNK_SIZE_64K as u64, &mut buf_c2)
        .unwrap();
    assert_eq!(n, 500);
    assert_eq!(
        buf_c2,
        &payload[2 * CHUNK_SIZE_64K..2 * CHUNK_SIZE_64K + 500]
    );

    // Near EOF (last 10 bytes)
    let mut buf_tail = vec![0u8; 20];
    let n = dm
        .read_at(inode_id, (payload.len() - 10) as u64, &mut buf_tail)
        .unwrap();
    assert_eq!(n, 10);
    assert_eq!(&buf_tail[..10], &payload[payload.len() - 10..]);

    // Beyond EOF
    let mut buf_eof = vec![0u8; 10];
    let n = dm
        .read_at(inode_id, payload.len() as u64 + 100, &mut buf_eof)
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn test_seekable_chunked_random_rewind_partial_overwrite() {
    let (_dir, img_path) = unique_image_path();
    let dm = DiskManager::open(&img_path, 30 * 1024 * 1024).unwrap();

    let root_id = dm.superblock().root_inode;
    let inode_id = dm.create_file(root_id, "rewind_test.bin").unwrap();

    // 192KB (3 chunks)
    let mut data = vec![0xAAu8; 3 * CHUNK_SIZE_64K];
    dm.write_data(
        inode_id,
        0,
        &data,
        CompressionMode::Seekable {
            chunk_size: CHUNK_SIZE_64K as u32,
            level: 1,
        },
    )
    .unwrap();

    // Rewind & Overwrite 16 bytes in chunk 1 (offset 70,000)
    let patch = [0x55u8; 16];
    dm.write_data(inode_id, 70_000, &patch, CompressionMode::Auto)
        .unwrap();
    data[70_000..70_016].copy_from_slice(&patch);

    let read_back = dm.read_data(inode_id).unwrap();
    assert_eq!(read_back, data);

    // Overwrite across chunk boundary (60,000 to 75,000 - crossing chunk 0 into chunk 1)
    let patch_cross = vec![0x77u8; 15_000];
    dm.write_data(inode_id, 60_000, &patch_cross, CompressionMode::Auto)
        .unwrap();
    data[60_000..75_000].copy_from_slice(&patch_cross);

    let read_back2 = dm.read_data(inode_id).unwrap();
    assert_eq!(read_back2, data);

    // Verify small read_at exactly matches
    let mut slice_buf = vec![0u8; 32];
    dm.read_at(inode_id, 69_990, &mut slice_buf).unwrap();
    assert_eq!(slice_buf, &data[69_990..70_022]);
}

#[test]
fn test_seekable_chunked_anti_inflation_fallback() {
    let (_dir, img_path) = unique_image_path();
    let dm = DiskManager::open(&img_path, 30 * 1024 * 1024).unwrap();

    let root_id = dm.superblock().root_inode;
    let inode_id = dm.create_file(root_id, "uncompressible.bin").unwrap();

    // High entropy 64KB chunk (pseudorandom noise)
    let mut noise = vec![0u8; CHUNK_SIZE_64K];
    let mut state: u64 = 0x853c49e6748fea9b;
    for b in noise.iter_mut() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        *b = (state >> 33) as u8;
    }

    dm.write_chunked_64k(inode_id, 0, &noise, 1).unwrap();

    let read_back = dm.read_data(inode_id).unwrap();
    assert_eq!(read_back, noise);

    let mut buf = vec![0u8; 128];
    dm.read_at(inode_id, 1000, &mut buf).unwrap();
    assert_eq!(buf, &noise[1000..1128]);
}

#[test]
fn test_seekable_chunked_truncate_shrink_and_expand() {
    let (_dir, img_path) = unique_image_path();
    let dm = DiskManager::open(&img_path, 30 * 1024 * 1024).unwrap();

    let root_id = dm.superblock().root_inode;
    let inode_id = dm.create_file(root_id, "trunc_test.bin").unwrap();

    // 192KB (3 chunks)
    let mut data = vec![0x42u8; 3 * CHUNK_SIZE_64K];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i % 256) as u8;
    }
    dm.write_chunked_64k(inode_id, 0, &data, 1).unwrap();

    // 1. Truncate shrink to 100KB (chunk 0: 64KB, chunk 1: 36KB, chunk 2: freed)
    dm.truncate(inode_id, 100 * 1024).unwrap();
    let inode_shrunk = dm.read_inode(inode_id).unwrap();
    assert_eq!(inode_shrunk.size, 100 * 1024);

    let read_shrunk = dm.read_data(inode_id).unwrap();
    assert_eq!(read_shrunk.len(), 100 * 1024);
    assert_eq!(read_shrunk, &data[..100 * 1024]);

    // 2. Truncate expand to 250KB (chunk 2 and 3 become sparse holes)
    dm.truncate(inode_id, 250 * 1024).unwrap();
    let inode_expanded = dm.read_inode(inode_id).unwrap();
    assert_eq!(inode_expanded.size, 250 * 1024);

    let mut hole_buf = vec![0xFFu8; 100];
    dm.read_at(inode_id, 200 * 1024, &mut hole_buf).unwrap();
    assert_eq!(
        hole_buf,
        vec![0u8; 100],
        "Sparse hole should read back as zeroes"
    );

    // 3. Truncate to 0 (all chunks freed)
    dm.truncate(inode_id, 0).unwrap();
    let inode_zero = dm.read_inode(inode_id).unwrap();
    assert_eq!(inode_zero.size, 0);
    assert_eq!(inode_zero.compressed_size, 0);
    assert_eq!(inode_zero.blocks, [0; 12]);
    assert_eq!(dm.read_data(inode_id).unwrap(), Vec::<u8>::new());
}

#[test]
fn test_seekable_chunked_persistence_across_reopen() {
    let (_dir, img_path) = unique_image_path();
    let mut payload = vec![0u8; 150_000];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i ^ 0x5A) as u8;
    }

    let inode_id = {
        let dm = DiskManager::open(&img_path, 30 * 1024 * 1024).unwrap();
        let root_id = dm.superblock().root_inode;
        let id = dm.create_file(root_id, "persist.bin").unwrap();
        dm.write_chunked_64k(id, 0, &payload, 1).unwrap();
        dm.flush().unwrap();
        id
    };

    // Reopen filesystem
    let dm_reopened = DiskManager::open(&img_path, 30 * 1024 * 1024).unwrap();
    let inode = dm_reopened.read_inode(inode_id).unwrap();
    assert_eq!(inode.size, payload.len() as u64);
    assert_ne!(inode.flags & INODE_FLAG_SEEKABLE_64K, 0);

    let read_back = dm_reopened.read_data(inode_id).unwrap();
    assert_eq!(read_back, payload);

    let mut buf = vec![0u8; 64];
    dm_reopened.read_at(inode_id, 70_000, &mut buf).unwrap();
    assert_eq!(buf, &payload[70_000..70_064]);
}

#[test]
fn test_seekable_chunked_no_leaked_blocks() {
    let (_dir, img_path) = unique_image_path();
    let dm = DiskManager::open(&img_path, 30 * 1024 * 1024).unwrap();

    let root_id = dm.superblock().root_inode;
    let stats_before = dm.analyze_fragmentation().unwrap();

    let inode_id = dm.create_file(root_id, "lifecycle.bin").unwrap();
    let data = vec![0x33u8; 200 * 1024]; // ~3.1 chunks
    dm.write_chunked_64k(inode_id, 0, &data, 1).unwrap();

    // Partial rewrites
    dm.write_chunked_64k(inode_id, 1000, &[0x99u8; 5000], 1)
        .unwrap();
    dm.write_chunked_64k(inode_id, 65000, &[0x88u8; 10000], 1)
        .unwrap();

    // Truncate to shrink
    dm.truncate(inode_id, 50 * 1024).unwrap();

    // Truncate to 0
    dm.truncate(inode_id, 0).unwrap();

    // Delete file
    dm.delete_file(root_id, "lifecycle.bin").unwrap();

    let stats_after = dm.analyze_fragmentation().unwrap();
    assert_eq!(
        stats_before.free_blocks, stats_after.free_blocks,
        "Every allocated block must be reclaimed after deletion"
    );
}

#[test]
fn test_seekable_chunked_ffi_api() {
    use oifs::ffi::*;
    use std::ffi::CString;

    let (_dir, img_path) = unique_image_path();
    let c_path = CString::new(img_path.to_str().unwrap()).unwrap();
    let handle = oifs_open(c_path.as_ptr(), 30 * 1024 * 1024);
    assert!(!handle.is_null());

    let filename = CString::new("ffi_seekable.dat").unwrap();
    let mut payload = vec![0u8; 130 * 1024]; // 2+ chunks
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }

    // Write with OIFS_WRITE_POLICY_SEEKABLE_64K
    let res = oifs_write_file_with_policy(
        handle,
        filename.as_ptr(),
        0,
        payload.as_ptr(),
        payload.len() as u64,
        OIFS_WRITE_POLICY_SEEKABLE_64K,
        1,
    );
    assert_eq!(res, 0);

    // Read full file
    let mut read_buf = vec![0u8; payload.len()];
    let n = oifs_read_file(
        handle,
        filename.as_ptr(),
        read_buf.as_mut_ptr(),
        read_buf.len() as u64,
    );
    assert_eq!(n as usize, payload.len());
    assert_eq!(read_buf, payload);

    // Overwrite at offset 70,000
    let patch = [0xEEu8; 20];
    let res = oifs_write_file_with_policy(
        handle,
        filename.as_ptr(),
        70_000,
        patch.as_ptr(),
        patch.len() as u64,
        OIFS_WRITE_POLICY_SEEKABLE_64K,
        1,
    );
    assert_eq!(res, 0);
    payload[70_000..70_020].copy_from_slice(&patch);

    // Read at offset 69990
    let mut at_buf = vec![0u8; 40];
    let n = oifs_read_at(
        handle,
        filename.as_ptr(),
        69990,
        at_buf.as_mut_ptr(),
        at_buf.len() as u64,
    );
    assert_eq!(n, 40);
    assert_eq!(at_buf, &payload[69990..70030]);

    oifs_close(handle);
}
