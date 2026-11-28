//! Regression tests for Batch 2 encryption, authentication, locking, and
//! stale-handle access control fixes.

use oifs::OifsSession;
use oifs::disk::{CompressionMode, DiskManager, DiskManagerError};
use oifs::fuse::{FuseMountOptions, OifsFuseState, inode_to_nodeid};
use oifs::nfs::{
    MNT3_OK, MNT3ERR_ACCES, MNTPROC3_EXPORT, MNTPROC3_MNT, MOUNT_PROGRAM, MOUNT_VERSION_3,
    NFS_FILE_HANDLE_LEN, NFS_PROGRAM, NFS_VERSION_3, NFS3_OK, NFS3ERR_ACCES, NFS3ERR_STALE,
    NFSPROC3_GETATTR, NFSPROC3_READ, NFSPROC3_SETATTR, NFSPROC3_WRITE, NfsFileHandle,
    NfsMountOptions, OifsNfsFs, RPC_ACCEPT_SUCCESS, RPC_AUTH_NONE, RPC_AUTH_SYS, RPC_AUTH_TOOWEAK,
    RPC_MSG_CALL, RPC_MSG_REPLY, RPC_REJECT_AUTH_ERROR, RPC_REPLY_MSG_ACCEPTED,
    RPC_REPLY_MSG_DENIED, RPC_VERSION_2, UNSTABLE, XdrReader, XdrWriter,
};
use std::io::Read as _;
use std::sync::Arc;
use tempfile::tempdir;

fn build_rpc_call(xid: u32, prog: u32, vers: u32, proc_num: u32, args: &[u8]) -> Vec<u8> {
    let mut w = XdrWriter::with_capacity(48 + args.len());
    w.write_u32(xid);
    w.write_u32(RPC_MSG_CALL);
    w.write_u32(RPC_VERSION_2);
    w.write_u32(prog);
    w.write_u32(vers);
    w.write_u32(proc_num);
    w.write_u32(RPC_AUTH_NONE);
    w.write_u32(0);
    w.write_u32(RPC_AUTH_NONE);
    w.write_u32(0);
    let mut bytes = w.into_bytes();
    bytes.extend_from_slice(args);
    bytes
}

fn nfs_reply_status(reply: &[u8]) -> u32 {
    let mut r = XdrReader::new(reply);
    let _xid = r.read_u32().unwrap();
    let _msg_type = r.read_u32().unwrap();
    let _reply_stat = r.read_u32().unwrap();
    let _verf_flavor = r.read_u32().unwrap();
    let _ = r.read_opaque(64).unwrap();
    let accept_stat = r.read_u32().unwrap();
    assert_eq!(accept_stat, 0);
    r.read_u32().unwrap()
}

#[test]
fn test_encrypted_filename_e_prefix_is_encrypted_and_tampered_entries_rejected() {
    use oifs::EncryptionCipher;
    use oifs::storage::StorageMode;

    let dir = tempdir().unwrap();

    for (tag, cipher, tok_len, tamper_off) in [
        // v3 AES-256-GCM-SIV: 3 + base64_unpadded(32 + 16 = 48) = 67 bytes
        ("v3", EncryptionCipher::Aes256Gcm, 67usize, 60usize),
        // v2 XChaCha20-SIV: 3 + base64_unpadded(24 + 32 + 16 = 72) = 99 bytes
        ("v2", EncryptionCipher::XChaCha20Poly1305, 99usize, 90usize),
    ] {
        let img = dir.path().join(format!("enc_filenames_{tag}.img"));

        let (dir_a_id, dir_b_id) = {
            let dm = DiskManager::create_encrypted_with_cipher(
                &img,
                4 * 1024 * 1024,
                "pw123",
                false,
                StorageMode::Mmap,
                cipher,
            )
            .unwrap();
            let root = dm.superblock().root_inode;

            // 1. Filename starting literally with `_e_` must still be encrypted on disk.
            let lit = "_e_very_confidential_literal_marker_xyz";
            let f = dm.create_file(root, lit).unwrap();
            dm.write_data(f, 0, b"payload", CompressionMode::Auto)
                .unwrap();
            assert_eq!(dm.lookup(root, lit).unwrap(), f);
            let root_list = dm.list_dir(root).unwrap();
            assert!(root_list.iter().any(|e| e.name == lit));

            let dir_a = dm.create_directory(root, "dir_a").unwrap();
            let dir_b = dm.create_directory(root, "dir_b").unwrap();

            dm.create_file(dir_a, "legit_inside_dir_a_file_0001.bin")
                .unwrap();
            dm.create_file(dir_a, "forged_target_entry_name_002.bin")
                .unwrap();
            dm.create_file(dir_a, "replay_target_entry_name_003.bin")
                .unwrap();

            dm.create_file(dir_b, "unique_b_entry_for_replay_04.bin")
                .unwrap();
            dm.flush().unwrap();
            (dir_a, dir_b)
        };

        // Inspect raw disk image: plaintext `_e_very_confidential_literal_marker_xyz` must NOT appear.
        let mut raw = std::fs::read(&img).unwrap();
        assert!(
            !raw.windows(b"very_confidential_literal_marker".len())
                .any(|w| w == b"very_confidential_literal_marker"),
            "_e_-prefixed filename leaked in plaintext on disk for {tag}!"
        );

        // Find all `_e_` tokens belonging to dir_a/dir_b entries (32-byte plaintext -> `tok_len`).
        let is_b64url = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
        let mut tokens: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut i = 0;
        while i + tok_len <= raw.len() {
            if &raw[i..i + 3] == b"_e_"
                && raw[i + 3..i + tok_len].iter().all(|&b| is_b64url(b))
                && (i + tok_len == raw.len() || !is_b64url(raw[i + tok_len]))
            {
                tokens.push((i, raw[i..i + tok_len].to_vec()));
                i += tok_len;
            } else {
                i += 1;
            }
        }
        assert_eq!(
            tokens.len(),
            4,
            "expected exactly 4 encrypted 32-byte filenames on disk for {tag}, found {}",
            tokens.len()
        );

        let dir_a_tok0_off = tokens[0].0;
        let dir_a_tok1_off = tokens[1].0;
        let dir_b_tok_bytes = tokens.last().unwrap().1.clone();

        // Tamper tag byte of dir_a token 0 (flip 'A'<->'B' within valid base64url alphabet).
        let tampered_byte = if raw[dir_a_tok0_off + tamper_off] == b'A' {
            b'B'
        } else {
            b'A'
        };
        raw[dir_a_tok0_off + tamper_off] = tampered_byte;
        // Replay dir_b's valid ciphertext into dir_a's slot 1 (same length, wrong parent_inode AAD).
        raw[dir_a_tok1_off..dir_a_tok1_off + tok_len].copy_from_slice(&dir_b_tok_bytes);
        std::fs::write(&img, &raw).unwrap();

        // Reopen image: list_dir(dir_a) must return ONLY the 1 valid remaining entry (neither tampered nor replayed!),
        // and list_dir(dir_b) still sees its legitimate entry.
        let dm = DiskManager::open_with_password(&img, 0, Some("pw123")).unwrap();
        let entries_a = dm.list_dir(dir_a_id).unwrap();
        assert_eq!(
            entries_a.len(),
            1,
            "tampered and cross-directory replayed entries must be rejected for {tag}, got: {:?}",
            entries_a.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        assert!(entries_a[0].name.ends_with(".bin"));
        assert!(!entries_a[0].name.starts_with("_e_Tampered"));

        // Verify dir_cache wasn't poisoned with raw ciphertext names either.
        let replayed_str = String::from_utf8(dir_b_tok_bytes).unwrap();
        assert!(dm.lookup(dir_a_id, &replayed_str).is_err());

        let entries_b = dm.list_dir(dir_b_id).unwrap();
        assert_eq!(entries_b.len(), 1);
        assert_eq!(entries_b[0].name, "unique_b_entry_for_replay_04.bin");
    }
}

#[test]
fn test_session_registry_enforces_password_on_cached_encrypted_session() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("reg_pw_check.img");
    OifsSession::unregister_from_registry(&img);

    let s1 = OifsSession::get_or_create_encrypted(&img, 4 * 1024 * 1024, "correct_pw").unwrap();
    let root = s1.resolve_path(".").unwrap();
    let fid = s1.create_file(root, "secret.txt").unwrap();
    s1.write_data(fid, 0, b"confidential", CompressionMode::Auto)
        .unwrap();

    // Missing password on cached encrypted session must fail with PasswordRequired
    assert!(matches!(
        OifsSession::get_or_open(&img, 0),
        Err(oifs::SessionError::DiskManager(
            DiskManagerError::PasswordRequired
        ))
    ));

    // Wrong password on cached encrypted session must fail with DecryptionFailed
    assert!(matches!(
        OifsSession::get_or_open_with_password(&img, 0, Some("wrong_pw")),
        Err(oifs::SessionError::DiskManager(
            DiskManagerError::DecryptionFailed
        ))
    ));

    // Correct password succeeds and shares access cleanly
    let s2 = OifsSession::get_or_open_with_password(&img, 0, Some("correct_pw")).unwrap();
    assert_eq!(s2.read_data(fid).unwrap(), b"confidential");

    OifsSession::unregister_from_registry(&img);
}

#[test]
fn child_lock_holder_helper() {
    let Ok(img_path) = std::env::var("OIFS_SEC_LOCK_CHILD_IMG") else {
        return;
    };
    let _dm = DiskManager::open(&img_path, 0).expect("child open image");
    println!("LOCKED");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

#[test]
fn test_create_encrypted_does_not_truncate_live_locked_image_and_rejects_tiny_size() {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::process::{Command, Stdio};

    let dir = tempdir().unwrap();
    let img = dir.path().join("live_locked.img");

    let payload = b"important live data";
    let fid = {
        let dm = DiskManager::open(&img, 2 * 1024 * 1024).unwrap();
        let root = dm.superblock().root_inode;
        let fid = dm.create_file(root, "precious.txt").unwrap();
        dm.write_data(fid, 0, payload, CompressionMode::Never)
            .unwrap();
        dm.flush().unwrap();
        fid
    };

    // Hold an OS-level `fcntl(F_WRLCK)` lock from a separate process (POSIX `fcntl` locks are per-process).
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .arg("--exact")
        .arg("child_lock_holder_helper")
        .arg("--nocapture")
        .env("OIFS_SEC_LOCK_CHILD_IMG", &img)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        let n = stdout.read_line(&mut line).unwrap();
        assert!(n > 0, "lock-holder child exited before signaling LOCKED");
        if line.trim() == "LOCKED" {
            break;
        }
    }

    // Competing create_encrypted while live lock is held must fail with Locking AND never truncate the image.
    let err = DiskManager::create_encrypted(&img, 4 * 1024 * 1024, "pw")
        .err()
        .expect("competing create_encrypted should fail while image lock is held");
    assert!(
        matches!(err, DiskManagerError::Locking(_)),
        "expected Locking error, got {err:?}"
    );

    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "DONE");
    }
    assert!(child.wait().unwrap().success());

    assert_eq!(std::fs::metadata(&img).unwrap().len(), 2 * 1024 * 1024);
    let dm = DiskManager::open(&img, 0).unwrap();
    assert_eq!(dm.read_data(fid).unwrap(), payload);

    // Tiny image sizes must return FileTooSmall cleanly rather than panicking in SuperBlock::new_v3.
    let tiny = dir.path().join("tiny.img");
    assert!(matches!(
        DiskManager::open(&tiny, 4 * 4096),
        Err(DiskManagerError::FileTooSmall)
    ));
    let tiny_j = dir.path().join("tiny_j.img");
    assert!(matches!(
        DiskManager::open_journaled(&tiny_j, 38 * 4096),
        Err(DiskManagerError::FileTooSmall)
    ));
}

#[test]
fn test_fuse_open_then_unlink_rejects_stale_handle_read_and_write() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("fuse_stale.img");
    let dm = DiskManager::open(&img, 2 * 1024 * 1024).unwrap();
    let fuse = OifsFuseState::new(FuseMountOptions::default());

    let (entry1, open1) = fuse
        .create(&dm, 1, "temp.txt", libc::O_RDWR as u32, 0o644)
        .unwrap();
    assert_eq!(
        fuse.write(&dm, entry1.attr.ino, open1.fh, 0, b"first_file_bytes", 0)
            .unwrap(),
        16
    );

    // Unlink while fh is open must invalidate fh.
    fuse.unlink(&dm, 1, "temp.txt").unwrap();

    // New file likely reuses inode 1 / freed data blocks.
    let (entry2, open2) = fuse
        .create(&dm, 1, "new.txt", libc::O_RDWR as u32, 0o644)
        .unwrap();
    assert_eq!(
        fuse.write(&dm, entry2.attr.ino, open2.fh, 0, b"SECOND_SECRET", 0)
            .unwrap(),
        13
    );

    // Old handle must be rejected (-EBADF) and never read or overwrite `new.txt`!
    assert_eq!(
        fuse.read(&dm, entry1.attr.ino, open1.fh, 0, 16),
        Err(-libc::EBADF)
    );
    assert_eq!(
        fuse.write(&dm, entry1.attr.ino, open1.fh, 0, b"CORRUPT", 0),
        Err(-libc::EBADF)
    );
    assert_eq!(
        fuse.read(&dm, entry2.attr.ino, open2.fh, 0, 13).unwrap(),
        b"SECOND_SECRET"
    );

    // Direct nodeid access on an unallocated inode (fh=0) must return -ESTALE.
    let bogus_nodeid = inode_to_nodeid(50).unwrap();
    assert_eq!(fuse.getattr(&dm, bogus_nodeid), Err(-libc::ESTALE));
    assert_eq!(fuse.read(&dm, bogus_nodeid, 0, 0, 16), Err(-libc::ESTALE));
}

#[test]
fn test_nfs_forged_and_deleted_handles_return_nfs3err_stale() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("nfs_stale.img");
    let dm = Arc::new(DiskManager::open(&img, 2 * 1024 * 1024).unwrap());
    let root = dm.superblock().root_inode;
    let fid = dm.create_file(root, "victim.txt").unwrap();
    dm.write_data(fid, 0, b"secret_victim", CompressionMode::Never)
        .unwrap();

    let nfs = OifsNfsFs::from_arc(Arc::clone(&dm), NfsMountOptions::default());

    // 1. MNT "/" to obtain session fs_generation
    let mut mnt_args = XdrWriter::new();
    mnt_args.write_string("/");
    let mnt_reply = nfs.dispatch_rpc(&build_rpc_call(
        1,
        MOUNT_PROGRAM,
        MOUNT_VERSION_3,
        MNTPROC3_MNT,
        &mnt_args.into_bytes(),
    ));
    assert_eq!(nfs_reply_status(&mnt_reply), MNT3_OK);

    // Delete victim.txt (inode `fid` is now unallocated)
    dm.delete_file(root, "victim.txt").unwrap();

    // Try both the deleted handle and an unallocated forged handle with valid fs_generation
    for bad_inode in [fid, 99u64] {
        let bad_fh = NfsFileHandle::new(nfs.fs_generation(), bad_inode).to_bytes();

        // GETATTR
        let mut ga = XdrWriter::with_capacity(NFS_FILE_HANDLE_LEN + 8);
        ga.write_opaque(&bad_fh);
        let ga_reply = nfs.dispatch_rpc(&build_rpc_call(
            2,
            NFS_PROGRAM,
            NFS_VERSION_3,
            NFSPROC3_GETATTR,
            &ga.into_bytes(),
        ));
        assert_eq!(nfs_reply_status(&ga_reply), NFS3ERR_STALE);

        // READ
        let mut rd = XdrWriter::with_capacity(NFS_FILE_HANDLE_LEN + 24);
        rd.write_opaque(&bad_fh);
        rd.write_u64(0);
        rd.write_u32(64);
        let rd_reply = nfs.dispatch_rpc(&build_rpc_call(
            3,
            NFS_PROGRAM,
            NFS_VERSION_3,
            NFSPROC3_READ,
            &rd.into_bytes(),
        ));
        assert_eq!(nfs_reply_status(&rd_reply), NFS3ERR_STALE);

        // WRITE
        let mut wr = XdrWriter::with_capacity(NFS_FILE_HANDLE_LEN + 32);
        wr.write_opaque(&bad_fh);
        wr.write_u64(0);
        wr.write_u32(4);
        wr.write_u32(UNSTABLE);
        wr.write_opaque(b"PWN!");
        let wr_reply = nfs.dispatch_rpc(&build_rpc_call(
            4,
            NFS_PROGRAM,
            NFS_VERSION_3,
            NFSPROC3_WRITE,
            &wr.into_bytes(),
        ));
        assert_eq!(nfs_reply_status(&wr_reply), NFS3ERR_STALE);

        // SETATTR
        let mut sa = XdrWriter::with_capacity(NFS_FILE_HANDLE_LEN + 48);
        sa.write_opaque(&bad_fh);
        for _ in 0..6 {
            sa.write_bool(false);
        }
        sa.write_bool(false); // no guard
        let sa_reply = nfs.dispatch_rpc(&build_rpc_call(
            5,
            NFS_PROGRAM,
            NFS_VERSION_3,
            NFSPROC3_SETATTR,
            &sa.into_bytes(),
        ));
        assert_eq!(nfs_reply_status(&sa_reply), NFS3ERR_STALE);
    }

    // Verify creating another file right after deletion was never touched by stale NFS ops.
    let f2 = dm.create_file(root, "fresh.txt").unwrap();
    assert_eq!(dm.read_data(f2).unwrap().len(), 0);
    let _ = std::io::empty().read(&mut [0u8; 1]);
}

fn build_auth_sys_cred(uid: u32, gid: u32) -> Vec<u8> {
    let mut w = XdrWriter::with_capacity(64);
    w.write_u32(1); // stamp
    w.write_string("localhost");
    w.write_u32(uid);
    w.write_u32(gid);
    w.write_u32(1); // gids.len()
    w.write_u32(gid);
    w.into_bytes()
}

fn build_rpc_call_auth_sys(
    xid: u32,
    prog: u32,
    vers: u32,
    proc_num: u32,
    uid: u32,
    gid: u32,
    args: &[u8],
) -> Vec<u8> {
    let cred = build_auth_sys_cred(uid, gid);
    let mut w = XdrWriter::with_capacity(64 + cred.len() + args.len());
    w.write_u32(xid);
    w.write_u32(RPC_MSG_CALL);
    w.write_u32(RPC_VERSION_2);
    w.write_u32(prog);
    w.write_u32(vers);
    w.write_u32(proc_num);
    w.write_u32(RPC_AUTH_SYS);
    w.write_opaque(&cred);
    w.write_u32(RPC_AUTH_NONE);
    w.write_u32(0);
    let mut bytes = w.into_bytes();
    bytes.extend_from_slice(args);
    bytes
}

#[test]
fn test_ipc_rejects_symlinked_runtime_dir_and_verifies_uds_peer_uid() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    let tmp = tempfile::tempdir().unwrap();
    let euid = unsafe { libc::geteuid() };

    // 1. Overly broad 0755 own runtime dir must be tightened to 0700.
    let broad = tmp.path().join("broad_runtime");
    std::fs::create_dir(&broad).unwrap();
    std::fs::set_permissions(&broad, std::fs::Permissions::from_mode(0o755)).unwrap();
    let resolved =
        oifs::ipc::resolve_private_runtime_dir_with(Some(broad.as_os_str()), tmp.path(), euid)
            .expect("owned 0755 dir should tighten cleanly to 0700");
    assert_eq!(
        std::fs::symlink_metadata(&broad).unwrap().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::symlink_metadata(&resolved).unwrap().mode() & 0o777,
        0o700
    );

    // 2. Symlinked runtime directory (e.g. XDG_RUNTIME_DIR or /tmp/oifs-<uid> hijack) must fail.
    let decoy = tmp.path().join("decoy_target");
    std::fs::create_dir(&decoy).unwrap();
    let link_dir = tmp.path().join("symlink_runtime");
    symlink(&decoy, &link_dir).unwrap();
    let err =
        oifs::ipc::resolve_private_runtime_dir_with(Some(link_dir.as_os_str()), tmp.path(), euid)
            .expect_err("symlinked runtime dir must be rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);

    let tmp_sub = tmp.path().join("temp_base");
    std::fs::create_dir(&tmp_sub).unwrap();
    let uid_dir_link = tmp_sub.join(format!("oifs-{euid}"));
    symlink(&decoy, &uid_dir_link).unwrap();
    let fallback_err = oifs::ipc::resolve_private_runtime_dir_with(None, &tmp_sub, euid)
        .expect_err("symlinked /tmp/oifs-<uid> fallback dir must be rejected");
    assert_eq!(fallback_err.kind(), std::io::ErrorKind::PermissionDenied);

    // 3. Real Local UDS session succeeds, verifies peer UID, and sets 0600 on token file.
    let img = tmp.path().join("uds_hardened.img");
    let master = OifsSession::open_ipc(&img, 2 * 1024 * 1024).unwrap();
    assert!(master.is_direct());
    let proxy = OifsSession::open_ipc(&img, 0).unwrap();
    assert!(!proxy.is_direct());
    let fid = proxy.create_file(0, "verified_peer.txt").unwrap();
    proxy
        .write_data(fid, 0, b"peer-uid-ok", CompressionMode::Never)
        .unwrap();
    assert_eq!(master.read_data(fid).unwrap(), b"peer-uid-ok");

    let tok_path = oifs::ipc::get_token_path(&img);
    let tok_meta = std::fs::symlink_metadata(&tok_path).unwrap();
    assert!(!tok_meta.file_type().is_symlink());
    assert_eq!(tok_meta.uid(), euid);
    assert_eq!(tok_meta.mode() & 0o077, 0);
}

#[test]
fn test_nfs_enforces_auth_sys_uid_seal_mount_and_csprng_handles() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("nfs_uid_gate.img");
    let dm = Arc::new(DiskManager::open(&img, 2 * 1024 * 1024).unwrap());

    let nfs_a = OifsNfsFs::from_arc(
        Arc::clone(&dm),
        NfsMountOptions {
            uid: 1000,
            gid: 1000,
            allow_other: false,
            require_auth_sys: true,
            ..NfsMountOptions::default()
        },
    );
    let nfs_b = OifsNfsFs::from_arc(Arc::clone(&dm), NfsMountOptions::default());
    assert_ne!(
        nfs_a.fs_generation(),
        nfs_b.fs_generation(),
        "NFS session generation must use CSPRNG entropy"
    );

    let mut mnt_args = XdrWriter::new();
    mnt_args.write_string("/");
    let mnt_bytes = mnt_args.into_bytes();

    // Non-owner AUTH_SYS (uid=2000) is denied MNT even before seal_mount().
    let denied_mnt = nfs_a.dispatch_rpc(&build_rpc_call_auth_sys(
        1,
        MOUNT_PROGRAM,
        MOUNT_VERSION_3,
        MNTPROC3_MNT,
        2000,
        2000,
        &mnt_bytes,
    ));
    assert_eq!(nfs_reply_status(&denied_mnt), MNT3ERR_ACCES);

    // Owner AUTH_SYS (uid=1000) succeeds before seal_mount().
    let ok_mnt = nfs_a.dispatch_rpc(&build_rpc_call_auth_sys(
        2,
        MOUNT_PROGRAM,
        MOUNT_VERSION_3,
        MNTPROC3_MNT,
        1000,
        1000,
        &mnt_bytes,
    ));
    let mut mr = XdrReader::new(&ok_mnt);
    let _ = mr.read_u32(); // xid
    let _ = mr.read_u32(); // REPLY
    assert_eq!(mr.read_u32(), Some(RPC_REPLY_MSG_ACCEPTED));
    let _ = mr.read_u32(); // verf_flavor
    let _ = mr.read_opaque(400);
    assert_eq!(mr.read_u32(), Some(RPC_ACCEPT_SUCCESS));
    assert_eq!(mr.read_u32(), Some(MNT3_OK));
    let root_fh = mr.read_opaque(64).unwrap().to_vec();

    // Seal mount after OS mount handshake: MNT must reject and EXPORT must be empty.
    nfs_a.seal_mount();
    let sealed_mnt = nfs_a.dispatch_rpc(&build_rpc_call_auth_sys(
        3,
        MOUNT_PROGRAM,
        MOUNT_VERSION_3,
        MNTPROC3_MNT,
        1000,
        1000,
        &mnt_bytes,
    ));
    assert_eq!(nfs_reply_status(&sealed_mnt), MNT3ERR_ACCES);

    let sealed_export = nfs_a.dispatch_rpc(&build_rpc_call_auth_sys(
        4,
        MOUNT_PROGRAM,
        MOUNT_VERSION_3,
        MNTPROC3_EXPORT,
        1000,
        1000,
        &[],
    ));
    let mut er = XdrReader::new(&sealed_export);
    let _ = er.read_u32();
    let _ = er.read_u32();
    assert_eq!(er.read_u32(), Some(RPC_REPLY_MSG_ACCEPTED));
    let _ = er.read_u32();
    let _ = er.read_opaque(400);
    assert_eq!(er.read_u32(), Some(RPC_ACCEPT_SUCCESS));
    assert_eq!(er.read_bool(), Some(false));

    // NFS GETATTR checks AUTH_NONE -> AUTH_TOOWEAK, other UID -> NFS3ERR_ACCES, owner UID -> NFS3_OK.
    let mut ga_args = XdrWriter::with_capacity(NFS_FILE_HANDLE_LEN + 8);
    ga_args.write_opaque(&root_fh);
    let ga_bytes = ga_args.into_bytes();

    let auth_none_ga = nfs_a.dispatch_rpc(&build_rpc_call(
        5,
        NFS_PROGRAM,
        NFS_VERSION_3,
        NFSPROC3_GETATTR,
        &ga_bytes,
    ));
    let mut dr = XdrReader::new(&auth_none_ga);
    assert_eq!(dr.read_u32(), Some(5));
    assert_eq!(dr.read_u32(), Some(RPC_MSG_REPLY));
    assert_eq!(dr.read_u32(), Some(RPC_REPLY_MSG_DENIED));
    assert_eq!(dr.read_u32(), Some(RPC_REJECT_AUTH_ERROR));
    assert_eq!(dr.read_u32(), Some(RPC_AUTH_TOOWEAK));

    let other_uid_ga = nfs_a.dispatch_rpc(&build_rpc_call_auth_sys(
        6,
        NFS_PROGRAM,
        NFS_VERSION_3,
        NFSPROC3_GETATTR,
        2000,
        2000,
        &ga_bytes,
    ));
    assert_eq!(nfs_reply_status(&other_uid_ga), NFS3ERR_ACCES);

    let owner_uid_ga = nfs_a.dispatch_rpc(&build_rpc_call_auth_sys(
        7,
        NFS_PROGRAM,
        NFS_VERSION_3,
        NFSPROC3_GETATTR,
        1000,
        1000,
        &ga_bytes,
    ));
    assert_eq!(nfs_reply_status(&owner_uid_ga), NFS3_OK);

    // Explicit allow_other: true permits non-owner UID 2000.
    let nfs_shared = OifsNfsFs::from_arc(
        Arc::clone(&dm),
        NfsMountOptions {
            uid: 1000,
            gid: 1000,
            allow_other: true,
            require_auth_sys: true,
            ..NfsMountOptions::default()
        },
    );
    let shared_fh = nfs_shared.root_handle().to_bytes();
    let mut shared_ga_args = XdrWriter::with_capacity(NFS_FILE_HANDLE_LEN + 8);
    shared_ga_args.write_opaque(&shared_fh);
    let shared_ga = nfs_shared.dispatch_rpc(&build_rpc_call_auth_sys(
        8,
        NFS_PROGRAM,
        NFS_VERSION_3,
        NFSPROC3_GETATTR,
        2000,
        2000,
        &shared_ga_args.into_bytes(),
    ));
    assert_eq!(nfs_reply_status(&shared_ga), NFS3_OK);
}
