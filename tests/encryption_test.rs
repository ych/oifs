use oifs::disk::{CompressionMode, DiskManager};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_create_encrypted_filesystem() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");

    // Create encrypted filesystem
    let password = "test_password_123";
    let size = 10 * 1024 * 1024; // 10MB

    let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();

    // Verify superblock has encryption enabled
    let sb = dm.superblock();
    assert!(sb.encrypted, "Superblock should have encrypted flag set");
    assert_ne!(sb.encryption_salt, [0u8; 16], "Salt should be non-zero");
    assert_eq!(
        sb.encryption_version,
        oifs::encryption::ENCRYPTION_SCHEME_CONTEXT_BOUND,
        "New images use the context-bound encryption scheme"
    );
}

#[test]
fn test_encrypted_write_read_roundtrip() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");

    let password = "secure_password";
    let size = 10 * 1024 * 1024;

    // Create encrypted filesystem
    let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();

    // Create a file
    let root_inode = dm.superblock().root_inode;
    let file_inode = dm.create_file(root_inode, "secret.txt").unwrap();

    // Write encrypted data
    let plaintext = b"This is secret data that should be encrypted!";
    dm.write_data(file_inode, 0, plaintext, CompressionMode::Never)
        .unwrap();

    // Read back and verify
    let decrypted = dm.read_data(file_inode).unwrap();
    assert_eq!(decrypted, plaintext, "Decrypted data should match original");

    // Verify the inode is marked as encrypted
    let inode = dm.read_inode(file_inode).unwrap();
    assert!(inode.encrypted, "Inode should be marked as encrypted");
    assert_ne!(
        inode.encryption_nonce, [0u8; 24],
        "Nonce should be non-zero"
    );
}

#[test]
fn test_wrong_password_fails() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");

    let correct_password = "correct_password";
    let wrong_password = "wrong_password";
    let size = 10 * 1024 * 1024;

    // Create encrypted filesystem with correct password
    let file_inode;
    {
        let dm = DiskManager::create_encrypted(&image_path, size, correct_password).unwrap();
        let root_inode = dm.superblock().root_inode;
        file_inode = dm.create_file(root_inode, "file.txt").unwrap();
        dm.write_data(file_inode, 0, b"secret", CompressionMode::Never)
            .unwrap();
    }

    // Try to open with wrong password - should succeed in opening disk manager
    let dm = DiskManager::open_with_password(&image_path, 0, Some(wrong_password)).unwrap();

    // With filename encryption , lookup with wrong password cannot match the encrypted filename
    let lookup_result = dm.lookup(0, "file.txt");
    assert!(
        lookup_result.is_err(),
        "Lookup with wrong password must fail to protect filename confidentiality"
    );

    // And reading data directly by inode with wrong password fails with DecryptionFailed
    let read_result = dm.read_data(file_inode);
    assert!(
        read_result.is_err(),
        "Reading with wrong password should fail"
    );

    // Verify it's a decryption error
    match read_result {
        Err(oifs::disk::DiskManagerError::DecryptionFailed) => {
            // Expected error
        }
        _ => panic!("Expected DecryptionFailed error, got: {:?}", read_result),
    }
}

#[test]
fn test_encryption_with_compression() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");

    let password = "compress_and_encrypt";
    let size = 10 * 1024 * 1024;

    let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
    let root_inode = dm.superblock().root_inode;
    let file_inode = dm.create_file(root_inode, "compressible.txt").unwrap();

    // Create highly compressible data (10KB of repeated pattern)
    let plaintext = vec![b'A'; 10 * 1024];

    // Write with compression enabled
    dm.write_data(file_inode, 0, &plaintext, CompressionMode::Always)
        .unwrap();

    // Verify inode shows both compression and encryption
    let inode = dm.read_inode(file_inode).unwrap();
    assert!(inode.encrypted, "File should be encrypted");
    assert!(inode.compressed_size > 0, "File should be compressed");
    assert_eq!(
        inode.size,
        plaintext.len() as u64,
        "Logical size should match original"
    );

    // Read back and verify
    let decrypted = dm.read_data(file_inode).unwrap();
    assert_eq!(
        decrypted, plaintext,
        "Decrypted+decompressed data should match original"
    );
}

#[test]
fn test_multiple_files_unique_nonces() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");

    let password = "multi_file_test";
    let size = 10 * 1024 * 1024;

    let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
    let root_inode = dm.superblock().root_inode;

    // Create multiple files
    let mut nonces = Vec::new();
    for i in 0..5 {
        let filename = format!("file{}.txt", i);
        let file_inode = dm.create_file(root_inode, &filename).unwrap();
        dm.write_data(file_inode, 0, b"data", CompressionMode::Never)
            .unwrap();

        let inode = dm.read_inode(file_inode).unwrap();
        nonces.push(inode.encryption_nonce);
    }

    // Verify all nonces are unique
    for i in 0..nonces.len() {
        for j in (i + 1)..nonces.len() {
            assert_ne!(
                nonces[i], nonces[j],
                "Nonces should be unique for different files"
            );
        }
    }
}

#[test]
fn test_cannot_open_encrypted_without_password() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");

    let password = "required_password";
    let size = 10 * 1024 * 1024;

    // Create encrypted filesystem
    {
        let _dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
    }

    // Try to open without password using regular open()
    let result = DiskManager::open(&image_path, 0);
    assert!(
        result.is_err(),
        "Opening encrypted filesystem without password should fail"
    );

    // Verify it's specifically a PasswordRequired error
    match result {
        Err(oifs::disk::DiskManagerError::PasswordRequired) => {
            // Expected error
        }
        _ => panic!("Expected PasswordRequired error"),
    }
}

#[test]
fn test_data_is_actually_encrypted_on_disk() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("encrypted.img");

    let password = "encryption_test";
    let size = 10 * 1024 * 1024;
    let plaintext = b"VERY_DISTINCTIVE_SECRET_TEXT_THAT_SHOULD_NOT_APPEAR_IN_RAW_DISK";

    let secret_filename = "VERY_DISTINCTIVE_SECRET_FILENAME_XYZ12345.TXT";

    // Create encrypted filesystem and write data
    {
        let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
        let root_inode = dm.superblock().root_inode;
        let file_inode = dm.create_file(root_inode, secret_filename).unwrap();
        dm.write_data(file_inode, 0, plaintext, CompressionMode::Never)
            .unwrap();
    }

    // Read raw disk image
    let raw_disk = fs::read(&image_path).unwrap();

    // Convert plaintext to string for searching
    let plaintext_str = String::from_utf8_lossy(plaintext);
    let raw_disk_str = String::from_utf8_lossy(&raw_disk);

    // Verify plaintext data does NOT appear in raw disk
    assert!(
        !raw_disk_str.contains(plaintext_str.as_ref()),
        "Plaintext should not be visible in raw disk image - data is not encrypted!"
    );

    // Verify filename does NOT appear in raw disk (Option 2: Filename Encryption)
    assert!(
        !raw_disk_str.contains(secret_filename),
        "Filename should not be visible in raw disk image - filename is not encrypted!"
    );

    // Verify we can still read it correctly with password
    // Need to reopen the file since we closed it above
    let dm = DiskManager::open_with_password(&image_path, size, Some(password)).unwrap();
    let root_inode = dm.superblock().root_inode;
    let file_inode = dm.lookup(root_inode, secret_filename).unwrap();
    let decrypted = dm.read_data(file_inode).unwrap();
    assert_eq!(
        decrypted, plaintext,
        "Should be able to decrypt with correct password"
    );
}

#[test]
fn test_unencrypted_filesystem_still_works() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("unencrypted.img");

    let size = 10 * 1024 * 1024;

    // Create normal (unencrypted) filesystem
    let dm = DiskManager::open(&image_path, size).unwrap();

    // Verify superblock shows not encrypted
    let sb = dm.superblock();
    assert!(!sb.encrypted, "Superblock should not have encrypted flag");

    // Write and read data normally
    let root_inode = sb.root_inode;
    let file_inode = dm.create_file(root_inode, "normal.txt").unwrap();
    let data = b"Normal unencrypted data";
    dm.write_data(file_inode, 0, data, CompressionMode::Never)
        .unwrap();

    let read_data = dm.read_data(file_inode).unwrap();
    assert_eq!(read_data, data);

    // Verify inode is not marked as encrypted
    let inode = dm.read_inode(file_inode).unwrap();
    assert!(!inode.encrypted, "Inode should not be encrypted");
}

#[test]
fn test_encrypted_compressed_append() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("enc_append.img");
    let size = 10 * 1024 * 1024;
    let password = "append_secret_pwd";

    let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
    let root_inode = dm.superblock().root_inode;
    let file_inode = dm.create_file(root_inode, "encrypted_log.txt").unwrap();

    // 1. Initial write with compression + encryption
    dm.write_data(
        file_inode,
        0,
        b"Part 1: Encrypted initial\n",
        CompressionMode::Always,
    )
    .unwrap();

    // 2. Append to compressed + encrypted file
    dm.write_data(
        file_inode,
        26,
        b"Part 2: Encrypted appended\n",
        CompressionMode::Always,
    )
    .unwrap();

    // 3. Read back and verify
    let read_back = dm.read_data(file_inode).unwrap();
    assert_eq!(
        read_back,
        b"Part 1: Encrypted initial\nPart 2: Encrypted appended\n"
    );
}

fn patch_image_bytes(path: &std::path::Path, offset: u64, bytes: &[u8]) {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(bytes).unwrap();
}

fn recompute_inode_v3_crc(path: &std::path::Path, slot_offset: u64) {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::OpenOptions::new().read(true).open(path).unwrap();
    f.seek(SeekFrom::Start(slot_offset)).unwrap();
    let mut prefix = [0u8; oifs::inode_format::INODE_V2_CRC32C_OFFSET];
    f.read_exact(&mut prefix).unwrap();
    let crc = oifs::journal::crc32c(&prefix);
    patch_image_bytes(
        path,
        slot_offset + oifs::inode_format::INODE_V2_CRC32C_OFFSET as u64,
        &crc.to_le_bytes(),
    );
}

#[test]
fn test_encrypted_image_rejects_unencrypted_inode_injection() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("inject_inode.img");
    let size = 8 * 1024 * 1024;
    let password = "strong_password";

    let (itb, raw_ino, raw_blk, seek_ino);
    {
        let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
        let root = dm.superblock().root_inode;

        raw_ino = dm.create_file(root, "genuine.txt").unwrap();
        dm.write_data(raw_ino, 0, b"genuine secret", CompressionMode::Never)
            .unwrap();

        seek_ino = dm.create_file(root, "seekable.bin").unwrap();
        dm.write_data(
            seek_ino,
            0,
            &vec![0x5Au8; 128 * 1024],
            CompressionMode::Seekable {
                chunk_size: 65536,
                level: 1,
            },
        )
        .unwrap();

        itb = dm.superblock().inode_table_block;
        raw_blk = dm.read_inode(raw_ino).unwrap().blocks[0];
        dm.flush().unwrap();
    }

    // Tamper raw file inode: clear `encrypted` byte (offset 129), write plaintext into its block,
    // and recompute the unkeyed v3 CRC32C ([171..175)) as an active disk attacker could, so reads
    // exercise the `!inode.encrypted` -> `DecryptionFailed` guard rather than failing on ChecksumMismatch.
    let raw_slot = itb * 4096 + raw_ino * 256;
    let payload = b"ATTACKER CONTROLLED";
    patch_image_bytes(&image_path, raw_blk * 4096, payload);
    patch_image_bytes(
        &image_path,
        raw_slot + 1,
        &(payload.len() as u64).to_le_bytes(),
    );
    patch_image_bytes(&image_path, raw_slot + 9, &0u64.to_le_bytes());
    patch_image_bytes(&image_path, raw_slot + 129, &[0]);
    recompute_inode_v3_crc(&image_path, raw_slot);

    // Tamper seekable file inode: clear `encrypted` byte (offset 129) and recompute v3 CRC32C.
    let seek_slot = itb * 4096 + seek_ino * 256;
    patch_image_bytes(&image_path, seek_slot + 129, &[0]);
    recompute_inode_v3_crc(&image_path, seek_slot);

    let dm = DiskManager::open_with_password(&image_path, 0, Some(password)).unwrap();

    // Both full read and positional read must reject the unencrypted inode.
    assert!(
        matches!(
            dm.read_data(raw_ino),
            Err(oifs::DiskManagerError::DecryptionFailed)
        ),
        "read_data must reject unencrypted file on an encrypted image"
    );
    let mut buf = [0u8; 32];
    assert!(
        matches!(
            dm.read_at(raw_ino, 0, &mut buf),
            Err(oifs::DiskManagerError::DecryptionFailed)
        ),
        "read_at must reject unencrypted file on an encrypted image"
    );

    // Seekable read_data and read_at must also reject the unencrypted inode.
    assert!(
        matches!(
            dm.read_data(seek_ino),
            Err(oifs::DiskManagerError::DecryptionFailed)
        ),
        "seekable read_data must reject unencrypted file on an encrypted image"
    );
    assert!(
        matches!(
            dm.read_at(seek_ino, 0, &mut buf),
            Err(oifs::DiskManagerError::DecryptionFailed)
        ),
        "seekable read_at must reject unencrypted file on an encrypted image"
    );
}

#[test]
fn test_encrypted_image_rejects_plaintext_directory_entry_injection() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("inject_dir.img");
    let size = 8 * 1024 * 1024;
    let password = "strong_password";

    let (dir_blk, genuine_ino);
    {
        let dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
        let root = dm.superblock().root_inode;
        genuine_ino = dm.create_file(root, "real.txt").unwrap();
        dm.write_data(genuine_ino, 0, b"real", CompressionMode::Never)
            .unwrap();
        dir_blk = dm.read_inode(root).unwrap().blocks[0];
        dm.flush().unwrap();
    }

    // Append an unauthenticated plaintext directory entry "injected.txt" directly into root's dir block.
    let img = fs::read(&image_path).unwrap();
    let blk_off = (dir_blk * 4096) as usize;
    let insert_off = oifs::directory::find_insert_offset_in_block(&img[blk_off..blk_off + 4096]);
    let injected = oifs::directory::DirectoryEntry {
        inode: genuine_ino,
        hash: oifs::directory::hash_filename("injected.txt"),
        name: "injected.txt".to_string(),
    };
    let mut encoded = Vec::new();
    injected.serialize_into(&mut encoded).unwrap();
    patch_image_bytes(&image_path, (blk_off + insert_off) as u64, &encoded);

    let dm = DiskManager::open_with_password(&image_path, 0, Some(password)).unwrap();
    let root = dm.superblock().root_inode;

    // Plaintext entry must not be found by lookup, delete_file, or list_dir.
    assert!(
        dm.lookup(root, "injected.txt").is_err(),
        "lookup must reject plaintext directory entry on encrypted image"
    );
    assert!(
        dm.delete_file(root, "injected.txt").is_err(),
        "delete_file must reject plaintext directory entry on encrypted image"
    );
    let listed: Vec<String> = dm
        .list_dir(root)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(listed, vec!["real.txt".to_string()]);
}

#[test]
fn test_password_rejected_when_superblock_encrypted_flag_downgraded() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("downgrade.img");
    let size = 8 * 1024 * 1024;
    let password = "strong_password";

    {
        let _dm = DiskManager::create_encrypted(&image_path, size, password).unwrap();
    }

    // 1. Flipping only `encrypted` (byte 64) leaves `encryption_version == 2` inconsistent,
    //    which SuperBlock::validate rejects immediately as CorruptedSuperblock.
    patch_image_bytes(&image_path, 64, &[0]);
    let res = DiskManager::open_with_password(&image_path, 0, Some(password));
    assert!(
        matches!(res, Err(oifs::DiskManagerError::CorruptedSuperblock(_))),
        "inconsistent encrypted=false with non-zero encryption_version must fail validation"
    );

    // 2. Even if an attacker also zeroes `encryption_version` (byte 81) to make the
    //    superblock look like a clean unencrypted image, open_with_password must refuse
    //    with NotEncrypted instead of silently writing plaintext.
    patch_image_bytes(&image_path, 81, &[0]);
    let res = DiskManager::open_with_password(&image_path, 0, Some(password));
    assert!(
        matches!(res, Err(oifs::DiskManagerError::NotEncrypted)),
        "open_with_password must refuse to open a downgraded/unencrypted image with a password"
    );
}

#[test]
fn test_open_with_password_on_new_path_creates_encrypted_image() {
    let temp_dir = TempDir::new().unwrap();
    let image_path = temp_dir.path().join("new_with_password.img");
    let size = 8 * 1024 * 1024;
    let password = "brand_new_password";

    {
        let dm = DiskManager::open_with_password(&image_path, size, Some(password)).unwrap();
        assert!(
            dm.is_encrypted(),
            "open_with_password on a new path must create an encrypted image"
        );
        let root = dm.superblock().root_inode;
        let ino = dm.create_file(root, "secret_file.txt").unwrap();
        dm.write_data(ino, 0, b"TOP_SECRET_PAYLOAD_12345", CompressionMode::Never)
            .unwrap();
        dm.flush().unwrap();
    }

    let raw = fs::read(&image_path).unwrap();
    assert!(
        !raw.windows(24).any(|w| w == b"TOP_SECRET_PAYLOAD_12345"),
        "Payload must not appear in plaintext on disk"
    );
    assert!(
        !raw.windows(15).any(|w| w == b"secret_file.txt"),
        "Filename must not appear in plaintext on disk"
    );

    // Reopening with the same password succeeds and returns the original data.
    let dm = DiskManager::open_with_password(&image_path, 0, Some(password)).unwrap();
    let ino = dm.lookup(0, "secret_file.txt").unwrap();
    assert_eq!(dm.read_data(ino).unwrap(), b"TOP_SECRET_PAYLOAD_12345");
}

#[cfg(unix)]
#[test]
fn test_new_image_has_owner_only_permissions_and_create_encrypted_refuses_symlink() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let temp_dir = TempDir::new().unwrap();
    let plain_img = temp_dir.path().join("plain_0600.img");
    let enc_img = temp_dir.path().join("enc_0600.img");

    {
        let _dm = DiskManager::open(&plain_img, 4 * 1024 * 1024).unwrap();
        let _edm = DiskManager::create_encrypted(&enc_img, 4 * 1024 * 1024, "pw").unwrap();
    }

    let plain_mode = fs::metadata(&plain_img).unwrap().permissions().mode() & 0o777;
    let enc_mode = fs::metadata(&enc_img).unwrap().permissions().mode() & 0o777;
    assert_eq!(plain_mode, 0o600, "plain image must be created 0600");
    assert_eq!(enc_mode, 0o600, "encrypted image must be created 0600");

    // create_encrypted uses truncate(true) and must refuse to follow a symlink.
    let victim = temp_dir.path().join("victim.txt");
    fs::write(&victim, b"do not truncate me").unwrap();
    let link = temp_dir.path().join("evil_link.img");
    symlink(&victim, &link).unwrap();

    let res = DiskManager::create_encrypted(&link, 4 * 1024 * 1024, "pw");
    assert!(
        res.is_err(),
        "create_encrypted must refuse to follow symlinks (O_NOFOLLOW)"
    );
    assert_eq!(
        fs::read(&victim).unwrap(),
        b"do not truncate me",
        "symlink target must remain untouched"
    );
}
