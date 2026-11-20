//! Tests that block pointers and WAL replay must never reach outside
//! the data area.

use oifs::disk::{CompressionMode, DiskManager};
use oifs::journal::MetadataOp;
use oifs::superblock::SuperBlock;
use std::fs;
use std::path::Path;

struct TestImage {
    path: String,
}

impl TestImage {
    fn new(name: &str) -> Self {
        let path = format!("target/{}_{}.img", name, std::process::id());
        if Path::new(&path).exists() {
            let _ = fs::remove_file(&path);
        }
        Self { path }
    }
}

impl Drop for TestImage {
    fn drop(&mut self) {
        if Path::new(&self.path).exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[test]
fn test_reading_a_file_never_exposes_metadata_blocks() {
    let img = TestImage::new("test_sec20_read");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "evil.bin").expect("create file");
    dm.write_data(id, 0, b"legitimate payload", CompressionMode::Never)
        .expect("write data");

    // Point the file at the superblock (block 0) and at the inode bitmap
    // (block 1): a corrupted or hostile image must not turn those blocks
    // into file content.
    let mut inode = dm.read_inode(id).expect("read inode");
    inode.blocks[0] = 0; // the superblock
    dm.write_inode(id, &inode).expect("write inode");
    drop(dm);

    let dm = DiskManager::open(&img.path, 0).expect("reopen dm");
    let data = dm.read_data(id).expect("read data");
    assert!(
        !data.windows(4).any(|w| w == b"OIFS"),
        "the superblock must not be readable as file content"
    );

    let mut inode = dm.read_inode(id).expect("read inode");
    inode.blocks[0] = 1; // the inode bitmap
    dm.write_inode(id, &inode).expect("write inode");
    drop(dm);

    let dm = DiskManager::open(&img.path, 0).expect("reopen dm");
    let data = dm.read_data(id).expect("read data");
    assert!(
        data.len() <= 4096,
        "a pointer outside the data area must not expose other blocks"
    );
}

#[test]
fn test_writing_through_a_forged_pointer_cannot_corrupt_the_filesystem() {
    let img = TestImage::new("test_sec20_write");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;
    let id = dm.create_file(root, "evil.bin").expect("create file");
    dm.write_data(id, 0, b"legitimate payload", CompressionMode::Never)
        .expect("write data");

    // Point the file at the inode bitmap (block 1): a partial write that
    // lands in the first block would otherwise overwrite the bitmap.
    let mut inode = dm.read_inode(id).expect("read inode");
    inode.blocks[0] = 1;
    dm.write_inode(id, &inode).expect("write inode");
    drop(dm);

    let dm = DiskManager::open(&img.path, 0).expect("reopen dm");
    let result = dm.write_data(id, 100, b"OVERWRITE", CompressionMode::Never);
    assert!(result.is_err(), "a forged pointer must not be writable");
    drop(dm);

    // The image must still be mountable and usable: the superblock and the
    // inode bitmap were not touched by the rejected write.
    let dm = DiskManager::open(&img.path, 0).expect("reopen after the rejected write");
    let sb = dm.superblock();
    assert_eq!(sb.magic, SuperBlock::MAGIC, "the superblock must be intact");
    let root = sb.root_inode;
    let new_id = dm
        .create_file(root, "after.bin")
        .expect("the filesystem must still be usable");
    dm.write_data(new_id, 0, b"still working", CompressionMode::Never)
        .expect("the filesystem must still be writable");
    assert_eq!(dm.read_data(new_id).unwrap(), b"still working");
}

#[test]
fn test_wal_replay_refuses_blocks_outside_the_data_area() {
    let img = TestImage::new("test_sec20_wal");
    let dm = DiskManager::open_journaled(&img.path, 10 * 1024 * 1024).expect("open journaled dm");
    let sb = dm.superblock();
    drop(dm);

    // A crafted WAL transaction that tries to overwrite the superblock, the
    // inode bitmap and an out-of-range inode.
    let mut image = fs::read(&img.path).expect("read image");

    let err = oifs::journal::apply_op_in_place(
        &mut image,
        &sb,
        &MetadataOp::WriteBlockSlice {
            block_id: 0,
            offset: 0,
            data: vec![0xFF; 16],
        },
    );
    assert!(
        err.is_err(),
        "the superblock must not be writable from the WAL"
    );

    let err = oifs::journal::apply_op_in_place(
        &mut image,
        &sb,
        &MetadataOp::WriteBlockSlice {
            block_id: sb.inode_bitmap_block,
            offset: 0,
            data: vec![0xFF; 16],
        },
    );
    assert!(
        err.is_err(),
        "the inode bitmap must not be writable from the WAL"
    );

    let err = oifs::journal::apply_op_in_place(
        &mut image,
        &sb,
        &MetadataOp::WriteInode {
            inode_id: sb.inode_count + 1000,
            inode_bytes: Box::new([0u8; 256]),
        },
    );
    assert!(err.is_err(), "an out-of-range inode must not be writable");

    let err = oifs::journal::apply_op_in_place(
        &mut image,
        &sb,
        &MetadataOp::SetDataBitmap {
            block_id: 1,
            allocated: true,
        },
    );
    assert!(
        err.is_err(),
        "the data bitmap must only describe the data area"
    );

    // A legitimate data-area write still works.
    let ok = oifs::journal::apply_op_in_place(
        &mut image,
        &sb,
        &MetadataOp::WriteBlockSlice {
            block_id: sb.data_block_start + 1,
            offset: 0,
            data: vec![0x42; 16],
        },
    );
    assert!(ok.is_ok(), "a data-area write must still be accepted");
}

#[test]
fn test_read_at_chunked_and_truncate_never_touch_metadata_blocks() {
    let img = TestImage::new("test_sec28_bounds");
    let dm = DiskManager::open(&img.path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    // 1. Raw file with forged block[0] = 0 (SuperBlock):
    //    read_at must return zeros (not SuperBlock bytes), and truncate
    //    (both shrink and expand) must not zero out SuperBlock slack bytes.
    let raw_id = dm.create_file(root, "forged_raw.bin").expect("create raw");
    dm.write_data(raw_id, 0, b"hello world", CompressionMode::Never)
        .expect("write raw");
    let mut raw_inode = dm.read_inode(raw_id).expect("read raw inode");
    raw_inode.blocks[0] = 0; // SuperBlock
    dm.write_inode(raw_id, &raw_inode).expect("write raw inode");

    let mut buf = [0xAAu8; 64];
    let n = dm.read_at(raw_id, 0, &mut buf).expect("read_at forged");
    assert_eq!(n, 11);
    assert!(
        buf[..n].iter().all(|&b| b == 0),
        "read_at must treat a forged metadata pointer as a zero hole"
    );

    dm.truncate(raw_id, 4).expect("truncate shrink forged");
    dm.truncate(raw_id, 4096).expect("truncate expand forged");

    // 2. Seekable 64K chunked file with forged ChunkEntry pointing to block 0:
    let chunk_id = dm
        .create_file(root, "forged_chunk.bin")
        .expect("create chunked");
    dm.write_data(
        chunk_id,
        0,
        b"seekable payload",
        CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        },
    )
    .expect("write chunked");
    let mut chunk_inode = dm.read_inode(chunk_id).expect("read chunk inode");
    chunk_inode.blocks[0] =
        oifs::inode::ChunkEntry::new(0, 1, oifs::inode::ChunkEntry::FLAG_RAW, 16).0;
    dm.write_inode(chunk_id, &chunk_inode)
        .expect("write chunk inode");

    assert!(
        dm.read_data(chunk_id).is_err(),
        "seekable chunk read must reject start_block < data_block_start"
    );
    let mut cbuf = [0u8; 16];
    assert!(
        dm.read_at(chunk_id, 0, &mut cbuf).is_err(),
        "seekable read_at must reject start_block < data_block_start"
    );

    drop(dm);

    // SuperBlock on disk must remain completely intact after reopen.
    let dm = DiskManager::open(&img.path, 0).expect("reopen dm after checks");
    assert_eq!(dm.superblock().magic, SuperBlock::MAGIC);
}

#[test]
fn test_forged_huge_inode_size_and_zstd_bomb_rejected_without_oom() {
    let img = TestImage::new("forged_size_oom");
    let dm = DiskManager::open(&img.path, 8 * 1024 * 1024).expect("create dm");
    let root = dm.superblock().root_inode;

    // 1. Raw file with forged 64 TiB size must return an error, never abort with OOM.
    let raw_id = dm.create_file(root, "huge_raw.bin").unwrap();
    dm.write_data(raw_id, 0, b"hello", CompressionMode::Never)
        .unwrap();
    let mut raw_inode = dm.read_inode(raw_id).unwrap();
    raw_inode.size = 1u64 << 46;
    dm.write_inode(raw_id, &raw_inode).unwrap();
    assert!(
        dm.read_data(raw_id).is_err(),
        "64 TiB raw inode.size must be rejected cleanly"
    );

    // 2. Raw file with forged u64::MAX size must return an error, never panic.
    raw_inode.size = u64::MAX;
    dm.write_inode(raw_id, &raw_inode).unwrap();
    assert!(
        dm.read_data(raw_id).is_err(),
        "u64::MAX raw inode.size must be rejected cleanly"
    );

    // 3. Seekable 64K file with forged 16 TiB size must return an error, never OOM.
    let seek_id = dm.create_file(root, "huge_seekable.bin").unwrap();
    dm.write_data(
        seek_id,
        0,
        b"seekable",
        CompressionMode::Seekable {
            chunk_size: 65536,
            level: 1,
        },
    )
    .unwrap();
    let mut seek_inode = dm.read_inode(seek_id).unwrap();
    seek_inode.size = 1u64 << 44;
    dm.write_inode(seek_id, &seek_inode).unwrap();
    assert!(
        dm.read_data(seek_id).is_err(),
        "16 TiB seekable inode.size must be rejected cleanly"
    );

    // 4. Compressed file with forged compressed_size exceeding data area must be rejected.
    let comp_id = dm.create_file(root, "forged_comp.bin").unwrap();
    dm.write_data(comp_id, 0, &vec![0u8; 64 * 1024], CompressionMode::Always)
        .unwrap();
    let mut comp_inode = dm.read_inode(comp_id).unwrap();
    comp_inode.compressed_size = 100 * 1024 * 1024;
    dm.write_inode(comp_id, &comp_inode).unwrap();
    assert!(
        dm.read_data(comp_id).is_err(),
        "compressed_size larger than filesystem data area must be rejected"
    );

    // 5. Zstd decompression bomb (512 MiB of zeros compressed to ~16 KB) with forged size
    //    must be rejected cleanly without expanding past MAX_READ_DATA_BYTES.
    let bomb = zstd::bulk::compress(&vec![0u8; 300 * 1024 * 1024], 1).unwrap();
    let bomb_id = dm.create_file(root, "zstd_bomb.bin").unwrap();
    dm.write_data(bomb_id, 0, &bomb, CompressionMode::Never)
        .unwrap();
    let mut bomb_inode = dm.read_inode(bomb_id).unwrap();
    bomb_inode.compressed_size = bomb.len() as u64;
    // Case A: forged logical size > MAX_READ_DATA_BYTES
    bomb_inode.size = 300 * 1024 * 1024;
    dm.write_inode(bomb_id, &bomb_inode).unwrap();
    assert!(
        dm.read_data(bomb_id).is_err(),
        "Zstd bomb with forged logical size > MAX_READ_DATA_BYTES must be rejected"
    );
    // Case B: forged small logical size (1 KiB) masking a 300 MiB Zstd stream
    bomb_inode.size = 1024;
    dm.write_inode(bomb_id, &bomb_inode).unwrap();
    assert!(
        dm.read_data(bomb_id).is_err(),
        "Zstd bomb exceeding expected_size + slack must be rejected"
    );
}
