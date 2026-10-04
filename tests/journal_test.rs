//! Integration tests for the optional Metadata WAL (journaling) subsystem.
//!
//! These cover the create → use → reopen → recover lifecycle of a journaled image
//! and the backward-compatibility guarantee that legacy images are untouched.

use oifs::disk::CompressionMode;
use oifs::journal::{
    JOURNAL_HEADER_BLOCK, JOURNAL_RESERVED_BLOCKS, JournalRing, JournalState, MetadataOp,
    apply_op_in_place, crc32c, decode_frame, encode_frame, encode_ops,
};
use oifs::superblock::SuperBlock;
use std::fs;
use std::path::PathBuf;

/// Simple scratch-path helper that removes the image on drop.
struct Img {
    path: PathBuf,
}

impl Img {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("oifs_journal_{name}.img"));
        let _ = fs::remove_file(&path);
        Self { path }
    }
}

impl Drop for Img {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

const MB: u64 = 1024 * 1024;

#[test]
fn test_journaled_image_creates_and_reopens() {
    let img = Img::new("create_reopen");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create journaled");
        assert!(dm.has_journal(), "image must be journaled");
        let sb = dm.superblock();
        assert!(
            sb.inode_table_block >= 3 + JOURNAL_RESERVED_BLOCKS,
            "inode table must start after the reserved journal region"
        );
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("reopen via plain open()");
        assert!(
            dm.has_journal(),
            "journaled layout must be detected on a plain reopen"
        );
    }
}

#[test]
fn test_journaled_image_basic_file_lifecycle() {
    let img = Img::new("lifecycle");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        let fid = dm.create_file(root, "hello.txt").expect("create file");
        dm.write_data(fid, 0, b"hello journal", CompressionMode::Never)
            .expect("write");
        assert_eq!(dm.read_data(fid).expect("read"), b"hello journal");
    }
    {
        // Reopen and verify durability of file content and directory entry.
        let dm = oifs::DiskManager::open(&img.path, 0).expect("reopen");
        let root = dm.superblock().root_inode;
        let fid = dm
            .lookup(root, "hello.txt")
            .expect("lookup survives reopen");
        assert_eq!(
            dm.read_data(fid).expect("read after reopen"),
            b"hello journal"
        );
    }
}

#[test]
fn test_journaled_create_and_delete_roundtrip() {
    let img = Img::new("crud");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        let a = dm.create_file(root, "a.txt").expect("create a");
        let b = dm.create_file(root, "b.txt").expect("create b");
        dm.write_data(a, 0, b"AAA", CompressionMode::Never)
            .expect("write a");
        dm.write_data(b, 0, b"BBB", CompressionMode::Never)
            .expect("write b");
        dm.delete_file(root, "a.txt").expect("delete a");
        assert!(dm.lookup(root, "a.txt").is_err(), "a.txt must be gone");
        assert!(dm.lookup(root, "b.txt").is_ok(), "b.txt must remain");
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("reopen");
        let root = dm.superblock().root_inode;
        assert!(dm.lookup(root, "a.txt").is_err(), "delete persists");
        let b = dm.lookup(root, "b.txt").expect("b survives");
        assert_eq!(dm.read_data(b).expect("read b"), b"BBB");
    }
}

#[test]
fn test_legacy_image_is_unchanged_by_journal_feature() {
    let img = Img::new("legacy");
    let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create legacy");
    let sb = dm.superblock();
    assert!(!dm.has_journal(), "default image must NOT be journaled");
    assert_eq!(
        sb.inode_table_block, 3,
        "legacy inode table block unchanged"
    );
}

#[test]
fn test_journal_header_written_at_block_three() {
    let img = Img::new("header");
    {
        let _dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
    }
    // A freshly formatted image has an empty ring and is not yet marked clean:
    // it has been created and mounted, not cleanly unmounted.
    {
        let bytes = fs::read(&img.path).expect("read image");
        let off = (JOURNAL_HEADER_BLOCK as usize) * 4096;
        let state = JournalState::decode(&bytes[off..off + 4096]).expect("journal header present");
        assert_eq!(state.head, 0, "fresh ring must be empty");
        assert_eq!(state.tail, 0, "fresh ring must be empty");
        assert!(
            !state.cleanly_unmounted,
            "fresh mount is not a clean shutdown"
        );
    }
    // Reopening replays (an empty ring) and records a clean state.
    {
        let _dm = oifs::DiskManager::open(&img.path, 0).expect("reopen");
    }
    {
        let bytes = fs::read(&img.path).expect("read image");
        let off = (JOURNAL_HEADER_BLOCK as usize) * 4096;
        let state = JournalState::decode(&bytes[off..off + 4096]).expect("journal header present");
        assert!(state.cleanly_unmounted, "reopen must record a clean marker");
        assert_eq!(state.head, 0);
        assert_eq!(state.tail, 0);
    }
}

#[test]
fn test_crash_recovery_replays_durable_transaction() {
    // Simulate a crash between "WAL transaction is durable" and "ops applied in
    // place" by driving the journal ring directly, then mounting the image and
    // asserting the redo took effect.
    let img = Img::new("recovery");
    {
        let _dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
    }

    // Append a transaction that marks a data block allocated, but do NOT apply it
    // to the image — exactly the state a crash between commit and apply leaves.
    {
        let mut image = fs::read(&img.path).expect("read image");
        let sb = SuperBlock::new_journaled(image.len() as u64 / 4096);

        let ops = vec![MetadataOp::SetDataBitmap {
            block_id: sb.data_block_start + 3,
            allocated: true,
        }];
        {
            let start = (JOURNAL_HEADER_BLOCK as usize) * 4096;
            let region_len = (JOURNAL_RESERVED_BLOCKS as usize) * 4096;
            let mut ring =
                JournalRing::open(&mut image[start..start + region_len], 4096).expect("open ring");
            ring.append(&ops).expect("append durable tx");
        }
        // Deliberately do not apply `ops` to `image`.
        fs::write(&img.path, &image).expect("write crashed image");
    }

    // Mounting must replay the transaction.
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("mount triggers recovery");
        let sb = dm.superblock();
        let dm_bitmap = dm
            .get_block_copy(sb.data_bitmap_block)
            .expect("data bitmap");
        // Mirror SimpleBlockAllocator: bit index is relative to data_block_start.
        let bit = 3usize;
        let byte = bit / 8;
        let mask = 1u8 << (bit % 8);
        assert_eq!(
            dm_bitmap[byte] & mask,
            mask,
            "recovered transaction must have set the data bitmap bit"
        );
    }
}

#[test]
fn test_torn_transaction_is_discarded_on_mount() {
    let img = Img::new("torn");
    {
        let _dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
    }
    // Corrupt the transaction so its CRC fails, then mount: the tx must be ignored
    // and the filesystem must still be usable.
    {
        let mut image = fs::read(&img.path).expect("read image");
        let start = (JOURNAL_HEADER_BLOCK as usize) * 4096;
        let region_len = (JOURNAL_RESERVED_BLOCKS as usize) * 4096;
        {
            let mut ring =
                JournalRing::open(&mut image[start..start + region_len], 4096).expect("open ring");
            ring.append(&[MetadataOp::SetInodeBitmap {
                inode_id: 7,
                allocated: true,
            }])
            .expect("append");
        }
        // Flip a bit inside the appended frame's payload region.
        image[start + 4096 + 24] ^= 0xFF;
        fs::write(&img.path, &image).expect("write torn image");
    }
    // Mount must succeed and ignore the torn transaction.
    let dm = oifs::DiskManager::open(&img.path, 0).expect("mount tolerates torn tx");
    let root = dm.superblock().root_inode;
    // Filesystem remains fully usable.
    dm.create_file(root, "after_torn.txt")
        .expect("usable after torn tx");
}

#[test]
fn test_apply_op_idempotent_in_place() {
    // Directly assert the redo primitive is idempotent at the image level.
    let sb = SuperBlock::new_journaled(3000);
    let mut image = vec![0u8; 3000 * 4096];
    let op = MetadataOp::SetInodeBitmap {
        inode_id: 11,
        allocated: true,
    };
    apply_op_in_place(&mut image, &sb, &op).expect("apply");
    let once = image.clone();
    apply_op_in_place(&mut image, &sb, &op).expect("re-apply");
    assert_eq!(image, once, "redo must be idempotent");
}

#[test]
fn test_crc32c_matches_reference_vector() {
    // Standard CRC32C check value.
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
}

#[test]
fn test_encode_frame_detects_single_bit_corruption() {
    let ops = vec![MetadataOp::SetInodeBitmap {
        inode_id: 1,
        allocated: true,
    }];
    let frame = encode_frame(1, &ops);
    assert!(decode_frame(&frame).is_some(), "clean frame decodes");

    // Flip every single bit in turn; each flip must be caught by the CRC.
    for byte_idx in 0..frame.len() {
        for bit in 0..8 {
            let mut corrupt = frame.clone();
            corrupt[byte_idx] ^= 1 << bit;
            assert!(
                decode_frame(&corrupt).is_none(),
                "corruption at byte {byte_idx} bit {bit} must be rejected"
            );
        }
    }
}

#[test]
fn test_metadata_ops_encode_decode_roundtrip() {
    let ops = vec![
        MetadataOp::SetInodeBitmap {
            inode_id: 5,
            allocated: true,
        },
        MetadataOp::SetDataBitmap {
            block_id: 42,
            allocated: false,
        },
    ];
    let encoded = encode_ops(&ops);
    let decoded = oifs::journal::MetadataOp::decode_all(&encoded).expect("decode");
    assert_eq!(decoded, ops);
}
