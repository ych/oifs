use oifs::disk::{CompressionMode, DiskManager};
use std::fs;
use std::path::Path;

struct TestContext {
    image_path: String,
}

impl TestContext {
    fn new(name: &str, _size_mb: u64) -> Self {
        let image_path = format!("{}.img", name);
        if Path::new(&image_path).exists() {
            let _ = fs::remove_file(&image_path);
        }
        Self { image_path }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        if Path::new(&self.image_path).exists() {
            let _ = fs::remove_file(&self.image_path);
        }
    }
}

#[test]
fn test_direct_and_single_indirect_blocks() {
    let ctx = TestContext::new("test_single_indirect", 20);
    let dm = DiskManager::open(&ctx.image_path, 20 * 1024 * 1024).expect("open dm");

    let root_id = dm.superblock().root_inode;
    let file_id = dm.create_file(root_id, "large_single_indirect.bin").expect("create file");

    // 10 direct blocks = 40KB. Single indirect starts at block 10.
    // Let's write 80KB (20 blocks total: 10 direct + 1 single indirect index + 10 single indirect data).
    let data_size = 80 * 1024;
    let payload: Vec<u8> = (0..data_size).map(|i| (i % 251) as u8).collect();

    dm.write_data(file_id, 0, &payload, CompressionMode::Never).expect("write data");

    // Read back and verify exact byte match
    let read_back = dm.read_data(file_id).expect("read data");
    assert_eq!(read_back.len(), data_size);
    assert_eq!(read_back, payload);

    // Verify inode metadata
    let inode = dm.read_inode(file_id).expect("read inode");
    assert_eq!(inode.size, data_size as u64);
    assert_ne!(inode.blocks[0], 0);
    assert_ne!(inode.blocks[9], 0);
    assert_ne!(inode.blocks[10], 0); // Single indirect block allocated

    // Delete file and verify all blocks are returned
    dm.delete_file(root_id, "large_single_indirect.bin").expect("delete file");
    let fsck = dm.verify_integrity().expect("verify integrity");
    assert!(fsck.is_clean, "fsck should be clean after deleting single-indirect file: {:?}", fsck);
}

#[test]
fn test_double_indirect_blocks_and_sparse_offsets() {
    let ctx = TestContext::new("test_double_indirect", 40);
    let dm = DiskManager::open(&ctx.image_path, 40 * 1024 * 1024).expect("open dm");

    let root_id = dm.superblock().root_inode;
    let file_id = dm.create_file(root_id, "double_indirect.bin").expect("create file");

    // Direct: 10 blocks (0..10) = 40KB
    // Single indirect: 512 blocks = 2048KB = 2MB (indices 10..522)
    // Double indirect: logical block >= 522 (offset >= 522 * 4096 = 2,138,112 bytes)
    let double_indirect_offset = 530 * 4096; // ~2.17MB
    let chunk_size = 16 * 1024; // 16KB write
    let chunk_data: Vec<u8> = (0..chunk_size).map(|i| (i % 256) as u8).collect();

    // Write at direct block offset (0)
    dm.write_data(file_id, 0, b"HEADER_DATA", CompressionMode::Never).expect("write header");

    // Write across double-indirect block boundary
    dm.write_data(file_id, double_indirect_offset, &chunk_data, CompressionMode::Never).expect("write double indirect");

    // Read back full file
    let read_back = dm.read_data(file_id).expect("read data");
    assert_eq!(read_back.len() as u64, double_indirect_offset + chunk_size as u64);

    // Verify header
    assert_eq!(&read_back[..11], b"HEADER_DATA");
    // Verify unwritten sparse gap is zeroed
    assert_eq!(&read_back[11..double_indirect_offset as usize], vec![0u8; double_indirect_offset as usize - 11]);
    // Verify double indirect data chunk
    assert_eq!(&read_back[double_indirect_offset as usize..], chunk_data.as_slice());

    // Verify inode has double indirect block set (blocks[11])
    let inode = dm.read_inode(file_id).expect("read inode");
    assert_ne!(inode.blocks[11], 0);

    // Delete and verify clean fsck
    dm.delete_file(root_id, "double_indirect.bin").expect("delete file");
    let fsck = dm.verify_integrity().expect("verify integrity");
    assert!(fsck.is_clean, "fsck should be clean after deleting double-indirect file: {:?}", fsck);
}

#[test]
fn test_triple_indirect_blocks_and_sparse_offsets() {
    let ctx = TestContext::new("test_triple_indirect", 40);
    let dm = DiskManager::open(&ctx.image_path, 40 * 1024 * 1024).expect("open dm");

    let root_id = dm.superblock().root_inode;
    let file_id = dm.create_file(root_id, "triple_indirect.bin").expect("create file");

    // Double indirect limit is 262666 blocks (10 + 512 + 512*512) = 1,075,879,936 bytes.
    // Triple indirect starts at block 262666.
    let triple_indirect_blk = 262666 + 10;
    let triple_indirect_offset = triple_indirect_blk * 4096;
    let chunk_size = 8 * 1024; // 8KB write (2 blocks)
    let chunk_data: Vec<u8> = (0..chunk_size).map(|i| ((i * 7) % 256) as u8).collect();

    // Write at direct block offset (0)
    dm.write_data(file_id, 0, b"TRIPLE_HEADER", CompressionMode::Never).expect("write header");

    // Write past 1GB into triple indirect blocks
    dm.write_data(file_id, triple_indirect_offset, &chunk_data, CompressionMode::Never).expect("write triple indirect");

    // Verify inode metadata
    let inode = dm.read_inode(file_id).expect("read inode");
    assert_ne!(inode.triple_indirect, 0, "triple_indirect block pointer must be allocated");
    assert_eq!(inode.size, triple_indirect_offset + chunk_size as u64);

    // Verify read back
    let read_back = dm.read_data(file_id).expect("read data");
    assert_eq!(read_back.len() as u64, triple_indirect_offset + chunk_size as u64);
    assert_eq!(&read_back[..13], b"TRIPLE_HEADER");
    assert_eq!(&read_back[triple_indirect_offset as usize..], chunk_data.as_slice());

    // Delete and verify clean fsck
    dm.delete_file(root_id, "triple_indirect.bin").expect("delete file");
    let fsck = dm.verify_integrity().expect("verify integrity");
    assert!(fsck.is_clean, "fsck should be clean after deleting triple-indirect file: {:?}", fsck);
}

