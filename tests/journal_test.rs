//! Integration tests for the optional Metadata WAL (journaling) subsystem.
//!
//! These cover the create → use → reopen → recover lifecycle of a journaled image
//! and the backward-compatibility guarantee that legacy images are untouched.

use oifs::disk::CompressionMode;
use oifs::journal::{
    JOURNAL_HEADER_BLOCK, JOURNAL_RESERVED_BLOCKS, JournalRing, JournalState, MetadataOp,
    apply_op_in_place, crc32c, decode_frame, encode_frame, encode_ops, journal_ring_bytes,
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

// ---------------------------------------------------------------------------
// M3: write-path integration
// ---------------------------------------------------------------------------

/// Count the transaction frames currently sitting in the journal ring.
fn ring_tx_count(image: &[u8]) -> usize {
    let start = (JOURNAL_HEADER_BLOCK as usize) * 4096;
    let region_len = (JOURNAL_RESERVED_BLOCKS as usize) * 4096;
    let mut snapshot = image[start..start + region_len].to_vec();
    let ring = JournalRing::open(&mut snapshot, 4096).expect("open ring");
    let state = ring.state();
    let mut count = 0usize;
    let mut cursor = state.tail % journal_ring_bytes(4096);
    let ring_slice = ring_snapshot(&snapshot, 4096);
    while cursor != state.head % journal_ring_bytes(4096) {
        match decode_frame(&ring_slice[cursor as usize..]) {
            Some((_, len)) => {
                cursor = (cursor + len as u64) % journal_ring_bytes(4096);
                count += 1;
            }
            None => break,
        }
    }
    count
}

fn ring_snapshot(region: &[u8], block_size: u64) -> Vec<u8> {
    let bs = usize::try_from(block_size).unwrap();
    region[bs..].to_vec()
}

#[test]
fn test_journaled_delete_writes_transaction() {
    let img = Img::new("delete_tx");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        for n in ["x.txt", "y.txt", "z.txt"] {
            dm.create_file(root, n).expect("create");
        }
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("mount");
        let root = dm.superblock().root_inode;

        // Mount-time recovery replays and then resets the ring, so measure the
        // baseline now rather than from the pre-mount image.
        let txs_before = ring_tx_count(&fs::read(&img.path).expect("read"));

        dm.delete_file(root, "y.txt").expect("journaled delete");

        // The delete must add exactly one transaction.
        let txs_after = ring_tx_count(&fs::read(&img.path).expect("read"));
        assert_eq!(
            txs_after,
            txs_before + 1,
            "delete must commit exactly one WAL transaction"
        );
    }
}

#[test]
fn test_journaled_delete_is_atomic_and_recoverable() {
    let img = Img::new("delete_recover");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        for n in ["a", "b", "c", "d"] {
            dm.create_file(root, n).expect("create");
        }
        dm.delete_file(root, "b").expect("delete b");
    }
    // Remount: the delete must be visible exactly once, with no double-apply.
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("remount");
        let root = dm.superblock().root_inode;
        assert!(dm.lookup(root, "b").is_err(), "b must stay deleted");
        for n in ["a", "c", "d"] {
            dm.lookup(root, n).expect("sibling must survive");
        }
        // The freed inode must be reusable.
        let reused = dm.create_file(root, "e").expect("recreate");
        assert!(reused < 8, "freed inode slot should be reused");
    }
}

#[test]
fn test_journaled_delete_frees_data_blocks() {
    let img = Img::new("delete_blocks");
    let (freed_before, freed_after);
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        let f = dm.create_file(root, "big.dat").expect("create");
        let payload = vec![0xABu8; 100 * 1024];
        dm.write_data(f, 0, &payload, CompressionMode::Never)
            .expect("write 100KB");
        let sb = dm.superblock();
        let stats_before = dm.analyze_fragmentation().expect("stats");
        freed_before = stats_before.used_blocks;

        dm.delete_file(root, "big.dat").expect("delete");
        let stats_after = dm.analyze_fragmentation().expect("stats");
        freed_after = stats_after.used_blocks;
        assert!(
            freed_after < freed_before,
            "deleting a 100KB file must release data blocks ({freed_before} -> {freed_after})"
        );
        let _ = sb;
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("remount");
        let root = dm.superblock().root_inode;
        assert!(dm.lookup(root, "big.dat").is_err());
    }
}

#[test]
fn test_legacy_delete_path_untouched_by_journal() {
    // The legacy path must still work and must NOT create journal transactions.
    let img = Img::new("legacy_delete");
    let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create legacy");
    let root = dm.superblock().root_inode;
    dm.create_file(root, "keep").expect("create");
    dm.create_file(root, "drop").expect("create");
    dm.delete_file(root, "drop").expect("legacy delete");
    assert!(dm.lookup(root, "drop").is_err());
    assert!(dm.lookup(root, "keep").is_ok());
}

#[test]
fn test_journaled_repeated_delete_reuse_cycle() {
    // Exercises allocator hints, inode reuse and dir-cache invalidation across
    // many journaled transactions.
    let img = Img::new("reuse_cycle");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        for i in 0..25 {
            let name = format!("f{i}");
            let fid = dm.create_file(root, &name).expect("create");
            dm.write_data(fid, 0, &vec![i as u8; 512], CompressionMode::Never)
                .expect("write");
            dm.delete_file(root, &name).expect("delete");
            assert!(dm.lookup(root, &name).is_err(), "{name} must be gone");
        }
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("remount");
        let root = dm.superblock().root_inode;
        assert!(
            dm.list_dir(root).expect("list").is_empty(),
            "root must be empty"
        );
    }
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

// ---------------------------------------------------------------------------
// M3: journaled create (create_file / mkdir) + checkpoint
// ---------------------------------------------------------------------------

#[test]
fn test_journaled_mkdir_creates_directory_with_block() {
    let img = Img::new("mkdir_j");
    let dir_id = {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        let d = dm.create_directory(root, "docs").expect("mkdir");
        let ino = dm.read_inode(d).expect("read dir inode");
        assert_eq!(ino.mode, oifs::inode::FileType::Directory);
        assert_ne!(ino.blocks[0], 0, "a directory must own a data block");
        assert_eq!(ino.size, 4096);
        d
    };
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("remount");
        let root = dm.superblock().root_inode;
        let d = dm.lookup(root, "docs").expect("docs survives");
        assert_eq!(d, dir_id);
        assert_ne!(dm.read_inode(d).expect("inode").blocks[0], 0);
    }
}

#[test]
fn test_journaled_create_write_delete_roundtrip_large() {
    let img = Img::new("crud_large");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        for i in 0..40 {
            let name = format!("file{i}.bin");
            let fid = dm.create_file(root, &name).expect("create");
            let payload: Vec<u8> = (0..2048).map(|k| ((i + k) % 251) as u8).collect();
            dm.write_data(fid, 0, &payload, CompressionMode::Never)
                .expect("write");
        }
        for i in 0..40 {
            dm.delete_file(root, &format!("file{i}.bin"))
                .expect("delete");
        }
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("remount");
        let root = dm.superblock().root_inode;
        assert!(
            dm.list_dir(root).expect("list").is_empty(),
            "all entries must be gone after remount"
        );
    }
}

#[test]
fn test_journaled_multi_block_directory_growth() {
    // 300 entries forces the directory past 10 blocks, exercising the simulated
    // single-indirect allocation path.
    let img = Img::new("dir_growth");
    let n = 3000usize;
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        for i in 0..n {
            dm.create_file(root, &format!("e{i:04}"))
                .expect("create in large dir");
        }
        let ino = dm.read_inode(root).expect("root inode");
        let blocks = oifs::disk::DiskManager::dir_num_blocks(&ino);
        assert!(
            blocks > 10,
            "directory must grow past the direct limit (blocks={blocks})"
        );
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("remount");
        let root = dm.superblock().root_inode;
        let entries = dm.list_dir(root).expect("list after remount");
        assert_eq!(entries.len(), n, "every entry must survive the remount");
        for i in 0..n {
            dm.lookup(root, &format!("e{i:04}")).expect("lookup");
        }
    }
}

#[test]
fn test_journaled_create_duplicate_name_rejected() {
    let img = Img::new("dup_name");
    let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
    let root = dm.superblock().root_inode;
    dm.create_file(root, "dup").expect("first create");
    let err = dm
        .create_file(root, "dup")
        .expect_err("duplicate must be rejected");
    assert!(
        err.to_string().contains("already exists"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_journaled_checkpoint_discards_applied_transactions() {
    let img = Img::new("ckpt");
    let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
    let root = dm.superblock().root_inode;
    for i in 0..5 {
        dm.create_file(root, &format!("c{i}")).expect("create");
    }
    let before = ring_tx_count(&fs::read(&img.path).expect("read"));
    assert!(before > 0, "creates must leave transactions pending");

    let discarded = dm.checkpoint_journal_now().expect("checkpoint");
    assert_eq!(
        discarded, before,
        "checkpoint must discard every pending tx"
    );
    assert_eq!(
        ring_tx_count(&fs::read(&img.path).expect("read")),
        0,
        "ring must be empty after a checkpoint"
    );
    assert_eq!(dm.list_dir(root).expect("list").len(), 5);
}

#[test]
fn test_journaled_checkpoint_is_idempotent_and_mount_safe() {
    let img = Img::new("ckpt_safe");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        dm.create_file(root, "keep").expect("create");
        dm.checkpoint_journal_now().expect("checkpoint");
        assert_eq!(
            dm.checkpoint_journal_now().expect("second checkpoint"),
            0,
            "checkpointing an empty ring discards nothing"
        );
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("remount");
        let root = dm.superblock().root_inode;
        dm.lookup(root, "keep")
            .expect("entry survives checkpoint + remount");
        assert_eq!(dm.list_dir(root).expect("list").len(), 1);
    }
}

#[test]
fn test_journaled_checkpoint_strict_mode_reclaims_ring() {
    use oifs::DurabilityMode;
    let img = Img::new("ckpt_strict");
    let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB)
        .expect("create")
        .with_durability_mode(DurabilityMode::Strict);
    let root = dm.superblock().root_inode;
    for i in 0..10 {
        dm.create_file(root, &format!("s{i}")).expect("create");
    }
    assert_eq!(
        ring_tx_count(&fs::read(&img.path).expect("read")),
        0,
        "Strict mode must checkpoint after every transaction"
    );
    assert_eq!(dm.list_dir(root).expect("list").len(), 10);
}

#[test]
fn test_journaled_recovery_after_checkpoint_is_noop() {
    let img = Img::new("ckpt_noop");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        let f = dm.create_file(root, "data.txt").expect("create");
        dm.write_data(f, 0, b"payload", CompressionMode::Never)
            .expect("write");
        dm.checkpoint_journal_now().expect("checkpoint");
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("remount");
        let root = dm.superblock().root_inode;
        let f = dm.lookup(root, "data.txt").expect("lookup");
        assert_eq!(
            dm.read_data(f).expect("read"),
            b"payload",
            "content must be intact after checkpoint + remount"
        );
    }
}

#[test]
fn test_journaled_create_leaks_no_data_blocks() {
    let img = Img::new("alloc_hints");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        for i in 0..15 {
            dm.create_file(root, &format!("h{i}")).expect("create");
        }
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("remount");
        let root = dm.superblock().root_inode;
        for i in 0..15 {
            dm.lookup(root, &format!("h{i}")).expect("lookup");
        }
        let stats = dm.analyze_fragmentation().expect("stats");
        assert_eq!(
            stats.used_blocks, 1,
            "only the root directory's own data block should be in use"
        );
    }
}

#[test]
fn test_journaled_create_after_delete_reuses_inode() {
    let img = Img::new("reuse_inode");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        let a = dm.create_file(root, "a").expect("create a");
        dm.delete_file(root, "a").expect("delete a");
        let b = dm.create_file(root, "b").expect("create b");
        assert_eq!(a, b, "the freed inode must be reused");
        assert!(dm.lookup(root, "a").is_err(), "old name must be gone");
        assert!(dm.lookup(root, "b").is_ok());
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("remount");
        let root = dm.superblock().root_inode;
        assert!(dm.lookup(root, "a").is_err());
        dm.lookup(root, "b").expect("b survives remount");
    }
}

#[test]
fn test_journaled_create_matches_legacy_geometry() {
    // A journaled create must consume the same data blocks as a legacy create,
    // otherwise the two layouts would drift apart.
    let jimg = Img::new("geom_j");
    let limg = Img::new("geom_l");

    let jstats = {
        let dm = oifs::DiskManager::open_journaled(&jimg.path, 20 * MB).expect("j create");
        let root = dm.superblock().root_inode;
        for i in 0..8 {
            dm.create_directory(root, &format!("d{i}"))
                .expect("j mkdir");
        }
        dm.analyze_fragmentation().expect("j stats").used_blocks
    };
    let lstats = {
        let dm = oifs::DiskManager::open(&limg.path, 20 * MB).expect("l create");
        let root = dm.superblock().root_inode;
        for i in 0..8 {
            dm.create_directory(root, &format!("d{i}"))
                .expect("l mkdir");
        }
        dm.analyze_fragmentation().expect("l stats").used_blocks
    };

    assert_eq!(
        jstats, lstats,
        "journaled and legacy creates must consume identical data blocks"
    );
}

#[test]
fn test_journaled_fsck_clean_after_creates_and_deletes() {
    let img = Img::new("fsck_j");
    {
        let dm = oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        dm.create_directory(root, "dir").expect("mkdir");
        for i in 0..25 {
            dm.create_file(root, &format!("f{i}")).expect("create");
        }
        dm.delete_file(root, "f0").expect("delete");
        dm.delete_file(root, "f1").expect("delete");
    }
    let dm = oifs::DiskManager::open(&img.path, 0).expect("remount");
    let report = dm.verify_integrity().expect("fsck");
    assert!(
        report.is_clean,
        "journaled image must pass fsck: {report:?}"
    );
}

#[test]
fn test_journaled_concurrent_creates_and_deletes() {
    let img = Img::new("concurrent");
    let dm =
        std::sync::Arc::new(oifs::DiskManager::open_journaled(&img.path, 20 * MB).expect("create"));
    let root = dm.superblock().root_inode;
    let mut handles = Vec::new();
    for t in 0..4 {
        let dm = dm.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..15 {
                let name = format!("t{t}_f{i}");
                if let Ok(id) = dm.create_file(root, &name) {
                    let _ = dm.write_data(id, 0, b"x", CompressionMode::Never);
                    if i % 2 == 0 {
                        let _ = dm.delete_file(root, &name);
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("join");
    }
    let entries = dm.list_dir(root).expect("list");
    for e in &entries {
        dm.lookup(root, &e.name).expect("lookup survives");
    }
    drop(dm);
    let dm2 = oifs::DiskManager::open(&img.path, 0).expect("remount");
    let report = dm2.verify_integrity().expect("fsck");
    assert!(
        report.is_clean,
        "concurrent journaled writes must stay consistent: {report:?}"
    );
}
