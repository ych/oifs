//! Compatibility tests for the inode on-disk format bump (v1 `bincode` -> v2 fixed).
//!
//! A v1 image predates `format_version` on the superblock and stores inodes as
//! packed `bincode` records. These tests synthesize such an image by rewriting a
//! freshly created one into v1 form, then assert that it remains fully functional
//! and can be migrated in place.

use oifs::disk::CompressionMode;
use oifs::inode_format::{INODE_SLOT_SIZE, decode_v1, decode_v2, encode_v2};
use oifs::superblock::SuperBlock;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

struct Img {
    path: PathBuf,
}

impl Img {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("oifs_fmt_{name}.img"));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }
}

impl Drop for Img {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

const MB: u64 = 1024 * 1024;

fn read_block(path: &Path, block: u64) -> Vec<u8> {
    let mut f = File::open(path).expect("open");
    f.seek(SeekFrom::Start(block * 4096)).expect("seek");
    let mut buf = vec![0u8; 4096];
    f.read_exact(&mut buf).expect("read");
    buf
}

fn write_block(path: &Path, block: u64, data: &[u8]) {
    let mut f = OpenOptions::new().write(true).open(path).expect("open rw");
    f.seek(SeekFrom::Start(block * 4096)).expect("seek");
    f.write_all(data).expect("write");
}

/// Rewrite an image from format v2 into a genuine format v1 image.
///
/// This mirrors what a pre-bump build would have produced: the superblock without
/// `format_version` (its bytes read back as 0 from block 0's zero padding) and
/// every allocated inode slot holding a packed `bincode` record followed by
/// arbitrary stale bytes.
fn downgrade_to_v1(path: &Path) {
    let sb: SuperBlock = bincode::deserialize(&read_block(path, 0)[..]).expect("parse sb");

    // 1. Superblock: serialize *without* format_version, zero-pad the rest of block 0.
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct SuperBlockV1 {
        magic: u32,
        block_size: u32,
        block_count: u64,
        inode_bitmap_block: u64,
        data_bitmap_block: u64,
        inode_table_block: u64,
        inode_count: u64,
        data_block_start: u64,
        root_inode: u64,
        encrypted: bool,
        encryption_salt: [u8; 16],
        encryption_version: u8,
    }
    let v1 = SuperBlockV1 {
        magic: sb.magic,
        block_size: sb.block_size,
        block_count: sb.block_count,
        inode_bitmap_block: sb.inode_bitmap_block,
        data_bitmap_block: sb.data_bitmap_block,
        inode_table_block: sb.inode_table_block,
        inode_count: sb.inode_count,
        data_block_start: sb.data_block_start,
        root_inode: sb.root_inode,
        encrypted: sb.encrypted,
        encryption_salt: sb.encryption_salt,
        encryption_version: sb.encryption_version,
    };
    let payload = bincode::serialize(&v1).expect("serialize v1 sb");
    let mut block0 = vec![0u8; 4096];
    block0[..payload.len()].copy_from_slice(&payload);
    write_block(path, 0, &block0);

    // 2. Every allocated inode slot: v2 record -> bincode record + stale tail.
    let inode_bitmap = read_block(path, sb.inode_bitmap_block);
    let mut image = std::fs::read(path).expect("read image");
    for inode_id in 0..sb.inode_count {
        let bit = inode_id as usize;
        if inode_bitmap[bit / 8] & (1 << (bit % 8)) == 0 {
            continue;
        }
        let off = (sb.inode_table_block * 4096) as usize + inode_id as usize * INODE_SLOT_SIZE;
        let slot = &image[off..off + INODE_SLOT_SIZE];
        let inode = oifs::inode_format::decode_v2(slot).expect("decode v2");
        let bin = bincode::serialize(&inode).expect("serialize v1 inode");
        // v1 writers left the tail untouched, so emulate stale bytes there.
        image[off + bin.len()..off + INODE_SLOT_SIZE].fill(0x5A);
        image[off..off + bin.len()].copy_from_slice(&bin);
    }
    std::fs::write(path, &image).expect("write v1 image");
}

// ---------------------------------------------------------------------------

#[test]
fn test_v1_image_is_detected_and_stays_usable() {
    let img = Img::new("v1_usable");
    {
        let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create v2 image");
        let root = dm.superblock().root_inode;
        let f = dm.create_file(root, "hello.txt").expect("create");
        dm.write_data(f, 0, b"legacy payload", CompressionMode::Never)
            .expect("write");
    }

    downgrade_to_v1(&img.path);

    // The image must open, report the legacy format, and remain fully functional.
    let dm = oifs::DiskManager::open(&img.path, 0).expect("open v1 image");
    assert!(
        dm.superblock().is_legacy_inode_format(),
        "a v1 image must be detected as legacy"
    );
    assert_eq!(
        dm.superblock().inode_record_version(),
        SuperBlock::FORMAT_VERSION_LEGACY
    );

    let root = dm.superblock().root_inode;
    let f = dm.lookup(root, "hello.txt").expect("lookup on v1 image");
    assert_eq!(
        dm.read_data(f).expect("read on v1 image"),
        b"legacy payload",
        "v1 inode content must decode correctly"
    );

    // Writes on a v1 image must keep producing v1 records (no mixed encodings).
    let g = dm.create_file(root, "new.txt").expect("create on v1 image");
    dm.write_data(g, 0, b"still legacy", CompressionMode::Never)
        .expect("write on v1 image");
    assert!(dm.superblock().is_legacy_inode_format());
}

#[test]
fn test_v1_image_survives_reopen() {
    let img = Img::new("v1_reopen");
    {
        let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        dm.create_file(root, "a").expect("create a");
        dm.create_file(root, "b").expect("create b");
    }
    downgrade_to_v1(&img.path);

    // Mutate, drop, and reopen: everything must persist in the v1 layout.
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("open");
        let root = dm.superblock().root_inode;
        dm.delete_file(root, "a").expect("delete on v1 image");
        let c = dm.create_file(root, "c").expect("create c");
        dm.write_data(c, 0, b"c data", CompressionMode::Never)
            .expect("write c");
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("reopen v1 image");
        assert!(dm.superblock().is_legacy_inode_format());
        let root = dm.superblock().root_inode;
        assert!(dm.lookup(root, "a").is_err(), "delete persisted");
        dm.lookup(root, "b").expect("b persists");
        let c = dm.lookup(root, "c").expect("c persists");
        assert_eq!(dm.read_data(c).expect("read c"), b"c data");
    }
}

#[test]
fn test_v1_image_passes_fsck() {
    let img = Img::new("v1_fsck");
    {
        let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        dm.create_directory(root, "dir").expect("mkdir");
        for i in 0..10 {
            dm.create_file(root, &format!("f{i}")).expect("create");
        }
    }
    downgrade_to_v1(&img.path);
    let dm = oifs::DiskManager::open(&img.path, 0).expect("open");
    let report = dm.verify_integrity().expect("fsck on v1 image");
    assert!(report.is_clean, "v1 image must pass fsck: {report:?}");
}

#[test]
fn test_v1_records_are_not_readable_as_v2() {
    // Guards the reason the superblock has to select the decoder: a v1 record
    // decoded as v2 would produce plausible-looking but wrong values.
    let img = Img::new("v1_not_v2");
    {
        let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        let f = dm.create_file(root, "x").expect("create");
        dm.write_data(f, 0, b"0123456789", CompressionMode::Never)
            .expect("write");
    }
    downgrade_to_v1(&img.path);

    let bytes = std::fs::read(&img.path).expect("read");
    let sb: SuperBlock = bincode::deserialize(&bytes[..4096]).expect("parse");
    let off = (sb.inode_table_block * 4096) as usize + INODE_SLOT_SIZE; // inode 1
    let slot = &bytes[off..off + INODE_SLOT_SIZE];

    let as_v1 = decode_v1(slot).expect("v1 decode works");
    assert_eq!(as_v1.size, 10);

    // Decoding the same bytes as v2 must not silently "work" with the right value.
    match oifs::inode_format::decode_v2(slot) {
        Err(_) => {}
        Ok(v2) => assert_ne!(
            v2.size, as_v1.size,
            "if v2 happens to parse a v1 record, the values must still differ"
        ),
    }
}

#[test]
fn test_v2_records_carry_version_and_zeroed_reserved_space() {
    let img = Img::new("v2_layout");
    {
        let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        let f = dm.create_file(root, "y").expect("create");
        dm.write_data(f, 0, b"abcd", CompressionMode::Never)
            .expect("write");
    }
    let bytes = std::fs::read(&img.path).expect("read");
    let sb: SuperBlock = bincode::deserialize(&bytes[..4096]).expect("parse");
    assert!(!sb.is_legacy_inode_format());

    let off = (sb.inode_table_block * 4096) as usize + INODE_SLOT_SIZE;
    let slot = &bytes[off..off + INODE_SLOT_SIZE];
    assert_eq!(
        slot[oifs::inode_format::INODE_V2_RECORD_VERSION_OFFSET],
        oifs::inode_format::INODE_V2_RECORD_VERSION,
        "on-disk record must be stamped with record_version"
    );
    assert!(
        slot[oifs::inode_format::INODE_V2_RESERVED_START..]
            .iter()
            .all(|b| *b == 0),
        "reserved space must be zeroed, not left as stale bytes"
    );
    assert_eq!(encode_v2(&decode_v2(slot).expect("d")), *slot);
}

#[test]
fn test_migration_upgrades_v1_image_in_place() {
    let img = Img::new("migrate_ok");
    {
        let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        dm.create_directory(root, "dir").expect("mkdir");
        for i in 0..12 {
            let f = dm.create_file(root, &format!("f{i}")).expect("create");
            dm.write_data(
                f,
                0,
                format!("payload {i}").as_bytes(),
                CompressionMode::Never,
            )
            .expect("write");
        }
        dm.delete_file(root, "f3").expect("delete");
    }
    downgrade_to_v1(&img.path);

    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("open v1");
        assert!(dm.needs_migration(), "must report migration needed");

        let stats = dm.migrate().expect("migrate");
        assert!(!stats.already_current);
        assert_eq!(
            stats.from_version,
            SuperBlock::FORMAT_VERSION_LEGACY,
            "must start from the legacy format"
        );
        assert_eq!(
            stats.to_version,
            SuperBlock::FORMAT_VERSION_FIXED_INODE,
            "must end at the current format"
        );
        assert!(stats.inodes_rewritten > 0, "must rewrite allocated inodes");
        assert!(!dm.needs_migration(), "no migration needed afterwards");
        assert!(!dm.migration_in_progress(), "cursor must be cleared");

        // Everything still works on the upgraded image.
        let root = dm.superblock().root_inode;
        assert!(dm.lookup(root, "f3").is_err(), "delete preserved");
        let f = dm.lookup(root, "f7").expect("lookup");
        assert_eq!(dm.read_data(f).expect("read"), b"payload 7");
    }
    {
        // And it survives a remount, with the superblock change persisted.
        let dm = oifs::DiskManager::open(&img.path, 0).expect("reopen");
        assert!(!dm.needs_migration());
        assert!(!dm.superblock().is_legacy_inode_format());
        let report = dm.verify_integrity().expect("fsck");
        assert!(report.is_clean, "migrated image must pass fsck: {report:?}");
    }
}

#[test]
fn test_migration_is_idempotent() {
    let img = Img::new("migrate_idem");
    {
        let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        dm.create_file(root, "a").expect("create");
    }
    downgrade_to_v1(&img.path);
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("open");
        dm.migrate().expect("first migrate");
        let again = dm.migrate().expect("second migrate");
        assert!(again.already_current, "second migrate must be a no-op");
        assert_eq!(again.inodes_rewritten, 0);
    }
}

#[test]
fn test_migration_on_current_image_is_noop() {
    let img = Img::new("migrate_noop");
    let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create");
    assert!(!dm.needs_migration());
    let stats = dm.migrate().expect("migrate on current image");
    assert!(stats.already_current);
    assert_eq!(stats.inodes_rewritten, 0);
}

#[test]
fn test_interrupted_migration_cursor_keeps_image_readable() {
    // The crash-safety argument: an image holding a *mix* of v1 and v2 records must
    // still read correctly, because the cursor tells us which decoder to use.
    let img = Img::new("migrate_cursor");
    {
        let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create");
        let root = dm.superblock().root_inode;
        for i in 0..8 {
            let f = dm.create_file(root, &format!("c{i}")).expect("create");
            dm.write_data(f, 0, format!("data{i}").as_bytes(), CompressionMode::Never)
                .expect("write");
        }
    }
    downgrade_to_v1(&img.path);

    // Simulate a crash halfway: rewrite only inodes [0, 5) to v2 and record that
    // progress in the cursor, leaving format_version at the legacy value.
    {
        let bytes = std::fs::read(&img.path).expect("read");
        let sb: SuperBlock = bincode::deserialize(&bytes[..4096]).expect("parse");
        let inode_bitmap = read_block(&img.path, sb.inode_bitmap_block);
        let mut image = bytes;
        let cursor = 5u64;
        for inode_id in 0..cursor {
            let bit = inode_id as usize;
            if inode_bitmap[bit / 8] & (1 << (bit % 8)) == 0 {
                continue;
            }
            let off = (sb.inode_table_block * 4096) as usize + inode_id as usize * INODE_SLOT_SIZE;
            let inode = decode_v1(&image[off..off + INODE_SLOT_SIZE]).expect("v1 decode");
            let v2 = encode_v2(&inode);
            image[off..off + INODE_SLOT_SIZE].copy_from_slice(&v2);
        }
        // Record progress the way migrate() does.
        let mut sb2 = sb;
        sb2.format_version = SuperBlock::FORMAT_VERSION_LEGACY;
        sb2.migration_cursor = cursor;
        let ser = bincode::serialize(&sb2).expect("serialize");
        let mut block0 = vec![0u8; 4096];
        block0[..ser.len()].copy_from_slice(&ser);
        image[..4096].copy_from_slice(&block0);
        std::fs::write(&img.path, &image).expect("write partial");
    }

    // The mixed image must mount and read every entry correctly.
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("open mixed image");
        assert!(dm.superblock().is_legacy_inode_format());
        assert!(dm.superblock().is_migration_in_progress());
        assert!(dm.migration_in_progress());
        let root = dm.superblock().root_inode;
        for i in 0..8 {
            let f = dm
                .lookup(root, &format!("c{i}"))
                .expect("lookup in mixed image");
            assert_eq!(
                dm.read_data(f).expect("read in mixed image"),
                format!("data{i}").as_bytes(),
                "inode {i} must decode correctly regardless of which side of the cursor it is on"
            );
        }
    }

    // Resuming the migration must complete it and leave everything intact.
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("reopen");
        let stats = dm.migrate().expect("resume migration");
        assert!(!stats.already_current);
        assert!(!dm.needs_migration());
        assert!(!dm.migration_in_progress());
    }
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("verify");
        let root = dm.superblock().root_inode;
        for i in 0..8 {
            let f = dm
                .lookup(root, &format!("c{i}"))
                .expect("lookup after migration");
            assert_eq!(
                dm.read_data(f).expect("read after migration"),
                format!("data{i}").as_bytes()
            );
        }
        let report = dm.verify_integrity().expect("fsck");
        assert!(report.is_clean, "resumed migration: {report:?}");
    }
}

#[test]
fn test_v1_image_cannot_be_journaled() {
    // A genuine v1 image predates journaling entirely, so the combination
    // "v1 inodes + a journal holding records" is unreachable through the public API.
    //
    // It is *not* unreachable by corrupting an image on disk, which is why migrate()
    // must checkpoint the journal before switching formats: a replayed WriteInode op
    // would otherwise push v2-encoded bytes into a v1 slot and destroy the record.
    // This test pins the reachable half of that invariant.
    let img = Img::new("v1_no_journal");
    {
        let dm = oifs::DiskManager::open(&img.path, 20 * MB).expect("create");
        assert!(!dm.has_journal());
        let root = dm.superblock().root_inode;
        dm.create_file(root, "x").expect("create");
    }
    downgrade_to_v1(&img.path);
    {
        let dm = oifs::DiskManager::open(&img.path, 0).expect("open");
        assert!(
            !dm.has_journal(),
            "a downgraded image must not report journaling"
        );
        assert!(dm.superblock().is_legacy_inode_format());
    }
}
