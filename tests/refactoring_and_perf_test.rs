use oifs::bitmap::{Bitmap, BitmapRef};
use oifs::disk::{CompressionMode, DiskManager};
use oifs::filters::{delta_decode_inplace, delta_encode_inplace, trunc_precision_encode_inplace};
use oifs::ipc::IpcRequest;
use std::fs;
use std::path::Path;

struct TempTestContext {
    path: String,
}

impl TempTestContext {
    fn new(name: &str) -> Self {
        let path = format!("{}.img", name);
        if Path::new(&path).exists() {
            let _ = fs::remove_file(&path);
        }
        Self { path }
    }
}

impl Drop for TempTestContext {
    fn drop(&mut self) {
        if Path::new(&self.path).exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[test]
fn test_bitmap_ref_parity_and_bounds() {
    let mut data1 = [0x55u8, 0xAAu8, 0x00, 0xFF]; // 01010101, 10101010, 00000000, 11111111
    let data2 = [0x55u8, 0xAAu8, 0x00, 0xFF];
    let bm_mut = Bitmap::new(&mut data1);
    let bm_ref = BitmapRef::new(&data2);

    // Verify bit-by-bit parity between Bitmap and BitmapRef
    for i in 0..32 {
        assert_eq!(bm_mut.get(i), bm_ref.get(i), "Mismatch at bit {}", i);
    }

    // Out of bounds safety
    assert!(!bm_ref.get(32));
    assert!(!bm_ref.get(1000));

    // find_first_free parity
    assert_eq!(bm_mut.find_first_free(), bm_ref.find_first_free());
    assert_eq!(bm_ref.find_first_free(), Some(1)); // bit 1 is 0 in 0x55 (01010101)

    // All ones bitmap
    let all_ones = [0xFFu8; 16];
    let bm_full = BitmapRef::new(&all_ones);
    assert_eq!(bm_full.find_first_free(), None);

    // All zeroes bitmap
    let all_zeroes = [0x00u8; 16];
    let bm_empty = BitmapRef::new(&all_zeroes);
    assert_eq!(bm_empty.find_first_free(), Some(0));
}

#[test]
fn test_delta_inplace_roundtrip_all_typesizes() {
    // Test typesize = 1 (u8)
    let original_u8: Vec<u8> = (0..255).map(|i| (i * 13 % 256) as u8).collect();
    let mut buf_u8 = original_u8.clone();
    delta_encode_inplace(&mut buf_u8, 1);
    assert_ne!(
        buf_u8, original_u8,
        "Encoded data should differ from original"
    );
    delta_decode_inplace(&mut buf_u8, 1);
    assert_eq!(
        buf_u8, original_u8,
        "Decoded u8 must match original exactly"
    );

    // Test typesize = 2 (u16) with unaligned tail
    let mut original_u16 = Vec::new();
    for i in 0..100u16 {
        original_u16.extend_from_slice(&(i * 300).to_le_bytes());
    }
    original_u16.push(0x42); // 1 tail byte (unaligned)
    let mut buf_u16 = original_u16.clone();
    delta_encode_inplace(&mut buf_u16, 2);
    assert_eq!(
        *buf_u16.last().unwrap(),
        0x42,
        "Tail byte must be preserved"
    );
    delta_decode_inplace(&mut buf_u16, 2);
    assert_eq!(
        buf_u16, original_u16,
        "Decoded u16 must match original exactly"
    );

    // Test typesize = 4 (u32)
    let mut original_u32 = Vec::new();
    for i in 0..50u32 {
        original_u32.extend_from_slice(&(i * 100_000 + 42).to_le_bytes());
    }
    original_u32.extend_from_slice(&[0x11, 0x22]); // 2 tail bytes
    let mut buf_u32 = original_u32.clone();
    delta_encode_inplace(&mut buf_u32, 4);
    delta_decode_inplace(&mut buf_u32, 4);
    assert_eq!(
        buf_u32, original_u32,
        "Decoded u32 must match original exactly"
    );

    // Test typesize = 8 (u64)
    let mut original_u64 = Vec::new();
    for i in 0..30u64 {
        original_u64.extend_from_slice(&(i * 1_000_000_000_000 + 7).to_le_bytes());
    }
    let mut buf_u64 = original_u64.clone();
    delta_encode_inplace(&mut buf_u64, 8);
    delta_decode_inplace(&mut buf_u64, 8);
    assert_eq!(
        buf_u64, original_u64,
        "Decoded u64 must match original exactly"
    );

    // Edge cases: empty, 1 element
    let mut empty: Vec<u8> = Vec::new();
    delta_encode_inplace(&mut empty, 4);
    delta_decode_inplace(&mut empty, 4);
    assert!(empty.is_empty());

    let mut single = vec![42u8, 0, 0, 0];
    delta_encode_inplace(&mut single, 4);
    assert_eq!(single, vec![42u8, 0, 0, 0]);
    delta_decode_inplace(&mut single, 4);
    assert_eq!(single, vec![42u8, 0, 0, 0]);
}

#[test]
fn test_trunc_precision_inplace_logic() {
    let f32_val = 1.2345678f32;
    let mut buf = f32_val.to_le_bytes().to_vec();

    // Truncate to 10 bits precision
    trunc_precision_encode_inplace(&mut buf, 4, 10);
    let truncated = f32::from_le_bytes(buf[..4].try_into().unwrap());
    assert!(
        (truncated - f32_val).abs() < 0.01,
        "Truncated float should be close to original"
    );
    let bits = u32::from_le_bytes(buf[..4].try_into().unwrap());
    let mask = (1u32 << (23 - 10)) - 1;
    assert_eq!(bits & mask, 0, "Lower mantissa bits must be zeroed");

    // f64 test
    let f64_val = 3.141592653589793f64;
    let mut buf64 = f64_val.to_le_bytes().to_vec();
    trunc_precision_encode_inplace(&mut buf64, 8, 20);
    let truncated64 = f64::from_le_bytes(buf64[..8].try_into().unwrap());
    assert!((truncated64 - f64_val).abs() < 1e-5);
    let bits64 = u64::from_le_bytes(buf64[..8].try_into().unwrap());
    let mask64 = (1u64 << (52 - 20)) - 1;
    assert_eq!(bits64 & mask64, 0, "Lower f64 mantissa bits must be zeroed");
}

#[test]
fn test_ipc_request_names_coverage() {
    assert_eq!(IpcRequest::Ping.name(), "Ping");
    assert_eq!(
        IpcRequest::CreateFile {
            parent_inode_id: 0,
            name: "f.txt".into()
        }
        .name(),
        "CreateFile"
    );
    assert_eq!(
        IpcRequest::CreateDirectory {
            parent_inode_id: 0,
            name: "d".into()
        }
        .name(),
        "CreateDirectory"
    );
    assert_eq!(
        IpcRequest::Lookup {
            parent_inode_id: 0,
            name: "f".into()
        }
        .name(),
        "Lookup"
    );
    assert_eq!(IpcRequest::ReadData { inode_id: 1 }.name(), "ReadData");
    assert_eq!(
        IpcRequest::WriteData {
            inode_id: 1,
            file_offset: 0,
            data: vec![1, 2, 3],
            compression_mode: CompressionMode::Auto,
            filter_config: Default::default()
        }
        .name(),
        "WriteData"
    );
    assert_eq!(
        IpcRequest::DeleteFile {
            parent_inode_id: 0,
            name: "f".into()
        }
        .name(),
        "DeleteFile"
    );
    assert_eq!(
        IpcRequest::ResolvePath {
            path: "/a/b".into()
        }
        .name(),
        "ResolvePath"
    );
    assert_eq!(
        IpcRequest::ResolveParent {
            path: "/a/b".into()
        }
        .name(),
        "ResolveParent"
    );
    assert_eq!(IpcRequest::ReadInode { inode_id: 0 }.name(), "ReadInode");
    assert_eq!(IpcRequest::ListDir { dir_inode_id: 0 }.name(), "ListDir");
    assert_eq!(IpcRequest::GetSuperblock.name(), "GetSuperblock");
    assert_eq!(IpcRequest::Flush.name(), "Flush");
    assert_eq!(
        IpcRequest::AnalyzeFragmentation.name(),
        "AnalyzeFragmentation"
    );
    assert_eq!(
        IpcRequest::Defragment {
            source_path: "a.img".into(),
            mode: Default::default(),
            output_path: None
        }
        .name(),
        "Defragment"
    );
    assert_eq!(IpcRequest::VerifyIntegrity.name(), "VerifyIntegrity");
    assert_eq!(
        IpcRequest::GetBlockCopy { block_id: 0 }.name(),
        "GetBlockCopy"
    );
}

#[test]
fn test_created_at_timestamp_population() {
    let ctx = TempTestContext::new("test_created_at_timestamps");
    let dm = DiskManager::open(&ctx.path, 10 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    let file_id = dm.create_file(root, "sample.txt").expect("create file");
    let file_inode = dm.read_inode(file_id).expect("read file inode");

    assert!(file_inode.created_at > 0, "created_at must be non-zero");
    assert!(file_inode.modified_at > 0, "modified_at must be non-zero");
    assert!(
        file_inode.created_at <= file_inode.modified_at,
        "created_at must be <= modified_at"
    );

    let dir_id = dm.create_directory(root, "sample_dir").expect("create dir");
    let dir_inode = dm.read_inode(dir_id).expect("read dir inode");

    assert!(dir_inode.created_at > 0, "dir created_at must be non-zero");
    assert!(
        dir_inode.modified_at > 0,
        "dir modified_at must be non-zero"
    );
    assert!(
        dir_inode.created_at <= dir_inode.modified_at,
        "dir created_at must be <= modified_at"
    );
}

#[test]
fn test_write_buffer_and_collection_consistency() {
    let ctx = TempTestContext::new("test_write_buffer_consistency");
    let dm = DiskManager::open(&ctx.path, 20 * 1024 * 1024).expect("open dm");
    let root = dm.superblock().root_inode;

    let file_id = dm
        .create_file(root, "multi_block.dat")
        .expect("create file");

    // Write 3 separate chunks across block boundaries
    let chunk1 = vec![0xAAu8; 3000];
    let chunk2 = vec![0xBBu8; 2000];
    let chunk3 = vec![0xCCu8; 4096];

    dm.write_data(file_id, 0, &chunk1, CompressionMode::Never)
        .expect("write 1");
    dm.write_data(file_id, 3000, &chunk2, CompressionMode::Never)
        .expect("write 2");
    dm.write_data(file_id, 5000, &chunk3, CompressionMode::Never)
        .expect("write 3");

    let read_back = dm.read_data(file_id).expect("read data");
    assert_eq!(read_back.len(), 5000 + 4096);
    assert_eq!(&read_back[..3000], chunk1.as_slice());
    assert_eq!(&read_back[3000..5000], chunk2.as_slice());
    assert_eq!(&read_back[5000..], chunk3.as_slice());

    // Verify integrity is 100% clean
    let fsck = dm.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "Filesystem should be clean: {:?}", fsck);

    // Delete and verify clean cleanup
    dm.delete_file(root, "multi_block.dat")
        .expect("delete file");
    let fsck2 = dm.verify_integrity().expect("fsck after delete");
    assert!(fsck2.is_clean, "Filesystem should be clean after deletion");
}
