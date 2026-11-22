//! Comprehensive integration tests for atomic POSIX `DiskManager::rename` and
//! FUSE `FUSE_RENAME` / `FUSE_RENAME2`.

use oifs::disk::{CompressionMode, DiskManager, DiskManagerError};
use oifs::fuse::{
    FUSE_OUT_HEADER_SIZE, FUSE_RENAME, FUSE_RENAME_NOREPLACE, FUSE_RENAME2, FUSE_ROOT_ID,
    FuseInHeader, FuseMountOptions, FuseOutHeader, OifsFuseFs, inode_to_nodeid,
};
use oifs::storage::StorageMode;
use tempfile::tempdir;

fn parse_reply_err(resp: &[u8]) -> i32 {
    let hdr = FuseOutHeader::parse(resp).expect("valid FuseOutHeader");
    assert_eq!(hdr.len as usize, resp.len());
    assert!(resp.len() >= FUSE_OUT_HEADER_SIZE);
    hdr.error
}

fn build_rename_payload(newdir: u64, old_name: &str, new_name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&newdir.to_le_bytes());
    out.extend_from_slice(old_name.as_bytes());
    out.push(0);
    out.extend_from_slice(new_name.as_bytes());
    out.push(0);
    out
}

fn build_rename2_payload(newdir: u64, flags: u32, old_name: &str, new_name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&newdir.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(old_name.as_bytes());
    out.push(0);
    out.extend_from_slice(new_name.as_bytes());
    out.push(0);
    out
}

#[test]
fn test_rename_matrix_plain_journaled_and_file_io() {
    for (storage_mode, journaled) in [
        (StorageMode::Mmap, true),
        (StorageMode::Mmap, false),
        (StorageMode::FileIo, true),
        (StorageMode::FileIo, false),
    ] {
        let dir = tempdir().expect("tempdir");
        let img = dir.path().join("rename.img");
        let dm =
            DiskManager::open_with_options(&img, 16 * 1024 * 1024, None, journaled, storage_mode)
                .expect("open image");

        // 1. Same-directory rename preserves inode ID and payload (metadata-only).
        let payload = vec![0xABu8; 64 * 1024];
        let src_ino = dm
            .create_file_with_data(0, "alpha.bin", &payload, CompressionMode::Never)
            .expect("create alpha.bin");
        let src_blocks_before = dm.read_inode(src_ino).expect("read inode").blocks;

        dm.rename(0, "alpha.bin", 0, "beta.bin")
            .expect("same-dir rename");
        assert!(dm.lookup(0, "alpha.bin").is_err());
        assert_eq!(dm.lookup(0, "beta.bin").expect("lookup beta.bin"), src_ino);
        assert_eq!(
            dm.read_inode(src_ino).expect("read inode").blocks,
            src_blocks_before,
            "rename must not touch or reallocate payload blocks"
        );
        assert_eq!(dm.read_data(src_ino).expect("read beta.bin"), payload);

        // 2. No-op self-rename succeeds cleanly.
        dm.rename(0, "beta.bin", 0, "beta.bin")
            .expect("self-rename noop");
        assert_eq!(dm.lookup(0, "beta.bin").unwrap(), src_ino);

        // 3. Cross-directory move.
        let dir_a = dm.create_directory(0, "dir_a").expect("mkdir dir_a");
        let dir_b = dm.create_directory(0, "dir_b").expect("mkdir dir_b");
        dm.rename(0, "beta.bin", dir_a, "moved.bin")
            .expect("cross-dir move");
        assert!(dm.lookup(0, "beta.bin").is_err());
        assert_eq!(dm.lookup(dir_a, "moved.bin").unwrap(), src_ino);
        assert_eq!(dm.read_data(src_ino).unwrap(), payload);

        // 4. Overwrite existing regular file frees victim inode & blocks atomically.
        let victim_ino = dm
            .create_file_with_data(
                dir_b,
                "target.bin",
                &[0xCDu8; 32 * 1024],
                CompressionMode::Never,
            )
            .expect("create victim");
        assert_ne!(victim_ino, src_ino);
        dm.rename(dir_a, "moved.bin", dir_b, "target.bin")
            .expect("overwrite file");
        assert!(dm.lookup(dir_a, "moved.bin").is_err());
        assert_eq!(dm.lookup(dir_b, "target.bin").unwrap(), src_ino);
        assert_eq!(dm.read_data(src_ino).unwrap(), payload);

        // 5. Overwrite empty directory succeeds; non-empty directory fails.
        let src_dir = dm.create_directory(0, "src_dir").expect("mkdir src_dir");
        let child_in_src = dm
            .create_file_with_data(src_dir, "keep.txt", b"keep me", CompressionMode::Never)
            .expect("create keep.txt");
        let _empty_dst_dir = dm
            .create_directory(0, "empty_dst")
            .expect("mkdir empty_dst");
        let nonempty_dst_dir = dm
            .create_directory(0, "nonempty_dst")
            .expect("mkdir nonempty_dst");
        dm.create_file_with_data(
            nonempty_dst_dir,
            "blocker.txt",
            b"block",
            CompressionMode::Never,
        )
        .expect("create blocker");

        assert!(
            dm.rename(0, "src_dir", 0, "nonempty_dst").is_err(),
            "must reject overwriting non-empty directory"
        );
        dm.rename(0, "src_dir", 0, "empty_dst")
            .expect("overwrite empty directory");
        assert!(dm.lookup(0, "src_dir").is_err());
        assert_eq!(dm.lookup(0, "empty_dst").unwrap(), src_dir);
        assert_eq!(dm.lookup(src_dir, "keep.txt").unwrap(), child_in_src);

        // 6. Type mismatch checks (dir -> file and file -> dir).
        assert!(
            dm.rename(0, "empty_dst", dir_b, "target.bin").is_err(),
            "cannot overwrite file with directory"
        );
        assert!(
            dm.rename(dir_b, "target.bin", 0, "empty_dst").is_err(),
            "cannot overwrite directory with file"
        );

        // 7. Cycle prevention: cannot move directory into itself or its descendant.
        let sub = dm.create_directory(src_dir, "sub").expect("mkdir sub");
        let deep = dm.create_directory(sub, "deep").expect("mkdir deep");
        assert!(
            dm.rename(0, "empty_dst", src_dir, "cycle1").is_err(),
            "cannot move dir into itself"
        );
        assert!(
            dm.rename(0, "empty_dst", deep, "cycle2").is_err(),
            "cannot move dir into descendant"
        );

        // 8. Integrity check (`fsck`) must be 100% clean (no leaked/orphan inodes or blocks).
        dm.flush().expect("flush");
        let report = dm.verify_integrity().expect("fsck");
        assert!(
            report.is_clean,
            "fsck failed for ({storage_mode:?}, journaled={journaled}): {report:?}"
        );
    }
}

#[test]
fn test_rename_encrypted_cross_directory_recomputes_siv_without_touching_payload() {
    let dir = tempdir().expect("tempdir");
    let img = dir.path().join("enc_rename.img");
    let password = "rename-enc-password-456!";
    let dm = DiskManager::open_with_options(
        &img,
        16 * 1024 * 1024,
        Some(password),
        true,
        StorageMode::FileIo,
    )
    .expect("open encrypted image");

    let d1 = dm.create_directory(0, "vault_a").expect("mkdir vault_a");
    let d2 = dm.create_directory(0, "vault_b").expect("mkdir vault_b");
    let secret = b"encrypted payload bound to inode_id, never re-encrypted on rename";
    let ino = dm
        .create_file_with_data(d1, "secret_old.txt", secret, CompressionMode::Never)
        .expect("create secret");
    let blocks_before = dm.read_inode(ino).expect("read inode").blocks;

    // Cross-directory rename changes parent_inode -> SIV filename ciphertext changes,
    // while payload blocks remain completely untouched.
    dm.rename(d1, "secret_old.txt", d2, "secret_new.txt")
        .expect("encrypted cross-dir rename");
    assert!(dm.lookup(d1, "secret_old.txt").is_err());
    assert_eq!(dm.lookup(d2, "secret_new.txt").unwrap(), ino);
    assert_eq!(dm.read_inode(ino).unwrap().blocks, blocks_before);
    assert_eq!(dm.read_data(ino).unwrap(), secret);
    dm.flush().expect("flush");
    drop(dm);

    // Reopen from disk and verify decryption + fsck.
    let dm2 = DiskManager::open_with_options(
        &img,
        16 * 1024 * 1024,
        Some(password),
        true,
        StorageMode::FileIo,
    )
    .expect("reopen encrypted image");
    let d2_re = dm2.lookup(0, "vault_b").expect("lookup vault_b");
    let ino_re = dm2
        .lookup(d2_re, "secret_new.txt")
        .expect("lookup secret_new.txt");
    assert_eq!(ino_re, ino);
    assert_eq!(dm2.read_data(ino_re).unwrap(), secret);
    assert!(dm2.verify_integrity().unwrap().is_clean);
}

#[test]
fn test_rename_read_only_and_fuse_wire_protocol() {
    let dir = tempdir().expect("tempdir");
    let img = dir.path().join("fuse_rename.img");
    let dm = DiskManager::open_with_options(&img, 8 * 1024 * 1024, None, true, StorageMode::Mmap)
        .expect("open image");
    let sub = dm.create_directory(0, "sub").expect("mkdir sub");
    dm.create_file_with_data(0, "a.txt", b"hello-a", CompressionMode::Never)
        .expect("create a.txt");
    dm.create_file_with_data(0, "b.txt", b"hello-b", CompressionMode::Never)
        .expect("create b.txt");

    let fs = OifsFuseFs::new(dm, FuseMountOptions::default());
    let sub_nodeid = inode_to_nodeid(sub).unwrap();

    // FUSE_RENAME2 with RENAME_NOREPLACE onto existing b.txt -> -EEXIST.
    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_RENAME2,
        1,
        FUSE_ROOT_ID,
        &build_rename2_payload(FUSE_ROOT_ID, FUSE_RENAME_NOREPLACE, "a.txt", "b.txt"),
    ));
    assert_eq!(parse_reply_err(&resp), -libc::EEXIST);

    // FUSE_RENAME2 with unsupported flag (e.g. RENAME_EXCHANGE = 2) -> -EINVAL.
    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_RENAME2,
        2,
        FUSE_ROOT_ID,
        &build_rename2_payload(FUSE_ROOT_ID, 2, "a.txt", "b.txt"),
    ));
    assert_eq!(parse_reply_err(&resp), -libc::EINVAL);

    // FUSE_RENAME across directories succeeds.
    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_RENAME,
        3,
        FUSE_ROOT_ID,
        &build_rename_payload(sub_nodeid, "a.txt", "moved_a.txt"),
    ));
    assert_eq!(parse_reply_err(&resp), 0);
    assert!(fs.lookup(FUSE_ROOT_ID, "a.txt").is_err());
    assert!(fs.lookup(sub_nodeid, "moved_a.txt").is_ok());
    fs.disk_manager().flush().expect("flush");
    drop(fs);

    // Read-only mount rejects both DiskManager::rename and FUSE_RENAME with EROFS.
    let bytes_before = std::fs::read(&img).expect("snapshot before");
    let dm_ro = DiskManager::open_read_only(&img).expect("open ro");
    assert!(matches!(
        dm_ro.rename(0, "b.txt", 0, "c.txt"),
        Err(DiskManagerError::ReadOnly)
    ));
    let fs_ro = OifsFuseFs::new(
        dm_ro,
        FuseMountOptions {
            read_only: true,
            ..FuseMountOptions::default()
        },
    );
    let resp = fs_ro.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_RENAME,
        4,
        FUSE_ROOT_ID,
        &build_rename_payload(FUSE_ROOT_ID, "b.txt", "c.txt"),
    ));
    assert_eq!(parse_reply_err(&resp), -libc::EROFS);
    drop(fs_ro);
    assert_eq!(bytes_before, std::fs::read(&img).expect("snapshot after"));
}
