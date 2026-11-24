//! Regression tests for Batch 4 IPC, NFSv3, FUSE, FFI, SuperBlock, and defrag stability fixes.

use oifs::disk::{CompressionMode, DefragMode, DiskManager, DiskManagerError};
use oifs::ffi::{
    OIFSReadBatchItem, oifs_close, oifs_create_file_with_data, oifs_last_error, oifs_open,
    oifs_read_at_batch, oifs_read_at_inode,
};
use oifs::fuse::{
    FUSE_RENAME2, FUSE_RENAME_NOREPLACE, FuseAbiPlatform, FuseMountOptions, MACOS_RENAME_EXCL,
    OifsFuseState, disk_error_to_errno,
};
use oifs::ipc::{PRE_AUTH_MAX_FRAME_BYTES, read_framed_with_limit};
use oifs::nfs::read_rpc_record;
use oifs::superblock::SuperBlock;
use std::ffi::{CStr, CString};
use std::io::Cursor;
use tempfile::tempdir;

#[test]
fn test_ipc_pre_auth_frame_limit_rejects_before_allocating() {
    let oversized_len = (PRE_AUTH_MAX_FRAME_BYTES as u32) + 1;
    let mut wire = Vec::new();
    wire.extend_from_slice(&oversized_len.to_le_bytes());
    wire.extend_from_slice(&[0xAAu8; 16]);

    let mut cur = Cursor::new(wire);
    let err = read_framed_with_limit::<_, Vec<u8>>(&mut cur, PRE_AUTH_MAX_FRAME_BYTES).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn test_nfs_read_rpc_record_rejects_zero_len_nonfinal_and_excessive_fragments() {
    // 1. Zero-length non-final fragment (header = 0x0000_0000)
    let mut z = Vec::new();
    z.extend_from_slice(&0x0000_0000u32.to_be_bytes());
    z.extend_from_slice(&0x8000_0004u32.to_be_bytes());
    z.extend_from_slice(&[1, 2, 3, 4]);
    let err = read_rpc_record(&mut Cursor::new(z)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

    // 2. > 64 fragments (65 x 4-byte fragments)
    let mut many = Vec::new();
    for i in 0..65usize {
        let last_bit = if i == 64 { 0x8000_0000u32 } else { 0 };
        many.extend_from_slice(&(last_bit | 4).to_be_bytes());
        many.extend_from_slice(&[1, 2, 3, 4]);
    }
    let err = read_rpc_record(&mut Cursor::new(many)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

fn build_fuse_frame(opcode: u32, unique: u64, nodeid: u64, payload: &[u8]) -> Vec<u8> {
    let total_len = (40 + payload.len()) as u32;
    let mut out = Vec::with_capacity(total_len as usize);
    out.extend_from_slice(&total_len.to_le_bytes());
    out.extend_from_slice(&opcode.to_le_bytes());
    out.extend_from_slice(&unique.to_le_bytes());
    out.extend_from_slice(&nodeid.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // uid
    out.extend_from_slice(&0u32.to_le_bytes()); // gid
    out.extend_from_slice(&0u32.to_le_bytes()); // pid
    out.extend_from_slice(&0u32.to_le_bytes()); // padding
    out.extend_from_slice(payload);
    out
}

fn parse_fuse_reply_error(resp: &[u8]) -> i32 {
    assert!(resp.len() >= 16);
    i32::from_le_bytes(resp[4..8].try_into().unwrap())
}

#[test]
fn test_fuse_disk_error_to_errno_and_macos_rename2_flags() {
    assert_eq!(
        disk_error_to_errno(&DiskManagerError::Io(std::io::Error::other(
            "Cannot write data to non-file inode"
        ))),
        -libc::EISDIR
    );
    assert_eq!(
        disk_error_to_errno(&DiskManagerError::Io(std::io::Error::other(
            "Cannot truncate non-file inode"
        ))),
        -libc::EISDIR
    );

    let dir = tempdir().unwrap();
    let img = dir.path().join("fuse_rename_flags.img");
    let dm = DiskManager::open(&img, 4 * 1024 * 1024).unwrap();
    let root = dm.superblock().root_inode;
    let _src = dm.create_file(root, "src.txt").unwrap();
    let _dst = dm.create_file(root, "dst.txt").unwrap();

    let state = OifsFuseState::new(FuseMountOptions::default());

    let mut payload = Vec::new();
    payload.extend_from_slice(&1u64.to_le_bytes()); // newdir = FUSE_ROOT_ID (1)
    payload.extend_from_slice(&1u32.to_le_bytes()); // macOS RENAME_SECLUDE (= 1, NOT EXCL!)
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(b"src.txt\0dst.txt\0");
    let frame = build_fuse_frame(FUSE_RENAME2, 10, 1, &payload);
    let resp = state.dispatch_frame_for_platform(&dm, &frame, FuseAbiPlatform::MacOs);
    assert_eq!(parse_fuse_reply_error(&resp), -libc::EINVAL);

    // MACOS_RENAME_EXCL (1 << 2) translates to FUSE_RENAME_NOREPLACE -> EEXIST on dst.txt.
    payload[8..12].copy_from_slice(&MACOS_RENAME_EXCL.to_le_bytes());
    let frame = build_fuse_frame(FUSE_RENAME2, 11, 1, &payload);
    let resp = state.dispatch_frame_for_platform(&dm, &frame, FuseAbiPlatform::MacOs);
    assert_eq!(parse_fuse_reply_error(&resp), -libc::EEXIST);

    // Linux NOREPLACE = 1 on Linux platform -> EEXIST on dst.txt.
    payload[8..12].copy_from_slice(&FUSE_RENAME_NOREPLACE.to_le_bytes());
    let frame = build_fuse_frame(FUSE_RENAME2, 12, 1, &payload);
    let resp = state.dispatch_frame_for_platform(&dm, &frame, FuseAbiPlatform::Linux);
    assert_eq!(parse_fuse_reply_error(&resp), -libc::EEXIST);
}

#[test]
fn test_ffi_read_at_batch_rejects_overlap_and_isize_max_bounds() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("ffi_bounds.img");
    let c_img = CString::new(img.to_str().unwrap()).unwrap();
    let handle = oifs_open(c_img.as_ptr(), 4 * 1024 * 1024);
    assert!(!handle.is_null());

    let c_file = CString::new("/data.bin").unwrap();
    let data = b"0123456789abcdef0123456789abcdef";
    let ino = oifs_create_file_with_data(
        handle,
        c_file.as_ptr(),
        data.as_ptr(),
        data.len() as u64,
        0,
        0,
    );
    assert!(ino >= 0);

    // Overlapping output buffers must be rejected before creating multiple &mut slices.
    let mut shared = [0u8; 32];
    let mut items = [
        OIFSReadBatchItem {
            inode_id: ino as u64,
            offset: 0,
            buf: shared.as_mut_ptr(),
            buf_size: 16,
            result: 0,
        },
        OIFSReadBatchItem {
            inode_id: ino as u64,
            offset: 8,
            buf: unsafe { shared.as_mut_ptr().add(8) },
            buf_size: 16,
            result: 0,
        },
    ];
    let rc = oifs_read_at_batch(handle, items.as_mut_ptr(), items.len());
    assert_eq!(rc, -1);
    let mut err_buf = [0i8; 256];
    let err_rc = oifs_last_error(handle, err_buf.as_mut_ptr(), err_buf.len() as u32);
    assert_eq!(err_rc, 0);
    let err_str = unsafe { CStr::from_ptr(err_buf.as_ptr()) }.to_string_lossy();
    assert!(
        err_str.contains("Overlapping"),
        "expected overlap error, got: {err_str}"
    );

    // Size > isize::MAX must fail cleanly rather than causing UB in slice::from_raw_parts_mut.
    let mut dummy = [0u8; 8];
    let big = (isize::MAX as u64).saturating_add(1);
    assert_eq!(
        oifs_read_at_inode(handle, ino as u64, 0, dummy.as_mut_ptr(), big),
        -1
    );

    oifs_close(handle);
}

#[test]
fn test_superblock_new_v3_min_blocks_and_defragment_safe_zero_byte_encrypted() {
    let sb = SuperBlock::new_v3(5, false);
    assert!(sb.inode_count >= 16);
    assert!(sb.validate(5 * oifs::BLOCK_SIZE).is_ok());

    let dir = tempdir().unwrap();
    let img = dir.path().join("enc_zero_defrag.img");
    let out = dir.path().join("enc_zero_defrag_out.img");
    let dm = DiskManager::create_encrypted(&img, 4 * 1024 * 1024, "pw-defrag").unwrap();
    let root = dm.superblock().root_inode;
    let zero_id = dm.create_file(root, "zero_enc.bin").unwrap();
    // Write 0-byte encrypted payload (occupies 1 block for 16-byte AEAD tag while size == 0).
    dm.write_data(zero_id, 0, b"", CompressionMode::Never)
        .unwrap();
    let non_zero_id = dm.create_file(root, "normal.bin").unwrap();
    dm.write_data(non_zero_id, 0, b"hello", CompressionMode::Never)
        .unwrap();

    let stats = dm
        .defragment(
            img.to_str().unwrap(),
            DefragMode::Safe,
            Some(out.to_str().unwrap()),
        )
        .unwrap();
    assert!(stats.files_processed >= 2);
    drop(dm);

    let dm_out = DiskManager::open_with_password(&img, 0, Some("pw-defrag")).unwrap();
    assert_eq!(dm_out.read_data(zero_id).unwrap(), b"");
    assert_eq!(dm_out.read_data(non_zero_id).unwrap(), b"hello");
    let report = dm_out.verify_integrity().unwrap();
    assert!(report.is_clean, "unhealthy defrag output: {report:?}");
}
