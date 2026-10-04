use oifs::BLOCK_SIZE;
use oifs::inode::FileType;
use oifs::superblock::SuperBlock;
use serde::Serialize;
use std::io::{Seek, SeekFrom, Write};
use tempfile::NamedTempFile;

#[derive(Serialize)]
struct V1SuperBlock {
    magic: u32,
    block_size: u32,
    block_count: u64,
    inode_bitmap_block: u64,
    data_bitmap_block: u64,
    inode_table_block: u64,
    inode_count: u64,
    data_block_start: u64,
    root_inode: u64,
}

#[derive(Serialize)]
#[allow(dead_code)]
enum V1FileType {
    File,
    Directory,
}

#[derive(Serialize)]
struct V1Inode {
    mode: V1FileType,
    size: u64,
    compressed_size: u64,
    created_at: u64,
    modified_at: u64,
    blocks: [u64; 12],
}

#[test]
fn test_v1_image_compatibility() {
    let mut file = NamedTempFile::new().unwrap();
    let zero_block = [0u8; BLOCK_SIZE];

    // Initialize entire 10MB (2560 blocks) with zeroes
    for _ in 0..2560 {
        file.write_all(&zero_block).unwrap();
    }

    // Write V1 Superblock at Block 0
    let v1_sb = V1SuperBlock {
        magic: SuperBlock::MAGIC,
        block_size: BLOCK_SIZE as u32,
        block_count: 2560,
        inode_bitmap_block: 1,
        data_bitmap_block: 2,
        inode_table_block: 3,
        inode_count: 32768,
        data_block_start: 1027,
        root_inode: 0,
    };
    let sb_bytes = bincode::serialize(&v1_sb).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&sb_bytes).unwrap();

    // Mark inode 0 in inode bitmap (Block 1)
    let mut inode_bitmap = [0u8; BLOCK_SIZE];
    inode_bitmap[0] = 0x01; // Inode 0 allocated
    file.seek(SeekFrom::Start(BLOCK_SIZE as u64)).unwrap();
    file.write_all(&inode_bitmap).unwrap();

    // Mark root directory data block 1027 in data bitmap (Block 2)
    let mut data_bitmap = [0u8; BLOCK_SIZE];
    data_bitmap[0] = 0x01; // Block 1027 allocated
    file.seek(SeekFrom::Start(2 * BLOCK_SIZE as u64)).unwrap();
    file.write_all(&data_bitmap).unwrap();

    // Write V1 Root Inode at Inode Table (Block 3, inode 0)
    let mut root_blocks = [0u64; 12];
    root_blocks[0] = 1027;
    let v1_root = V1Inode {
        mode: V1FileType::Directory,
        size: 0,
        compressed_size: 0,
        created_at: 1000,
        modified_at: 1000,
        blocks: root_blocks,
    };
    let root_bytes = bincode::serialize(&v1_root).unwrap();
    let inode_table_offset = 3 * BLOCK_SIZE as u64;
    file.seek(SeekFrom::Start(inode_table_offset)).unwrap();
    file.write_all(&root_bytes).unwrap();
    file.flush().unwrap();

    // Open with the current DiskManager!
    let path = file.path().to_path_buf();
    let dm = oifs::disk::DiskManager::open(&path, 0).expect("Must successfully open V1 image");

    let sb = dm.superblock();
    assert_eq!(sb.magic, SuperBlock::MAGIC);
    assert_eq!(sb.block_count, 2560);
    assert!(
        !sb.encrypted,
        "V1 image must deserialize with encrypted=false"
    );
    assert_eq!(sb.encryption_version, 0);

    let root_inode = dm.read_inode(0).expect("Must read V1 root inode");
    assert_eq!(root_inode.mode, FileType::Directory);
    assert!(
        !root_inode.encrypted,
        "V1 inode must deserialize with encrypted=false"
    );
    assert_eq!(
        root_inode.filter_typesize, 0,
        "V1 inode must have filter_typesize=0"
    );
    assert!(!root_inode.filter_delta);
    assert!(!root_inode.filter_shuffle);
    assert!(!root_inode.filter_bitshuffle);

    // Can we create a new file and write/read in this legacy V1 image?
    let new_file = dm
        .create_file(0, "test_v1_append.txt")
        .expect("create file on v1");
    dm.write_data(
        new_file,
        0,
        b"Hello from new binary on V1 image!",
        oifs::disk::CompressionMode::Always,
    )
    .expect("write data");
    let read_back = dm.read_data(new_file).expect("read data");
    assert_eq!(read_back, b"Hello from new binary on V1 image!");
}
