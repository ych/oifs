//! Protocol integration tests for OIFS Pure-Rust Local NFSv3 Server (`src/nfs.rs`).
//!
//! Exercises ONC RPC v2 TCP record marking, XDR encoding/decoding, MOUNT v3, all 18 NFS v3
//! procedures, multi-page `READDIR`/`READDIRPLUS` pagination (>200 entries), read-only
//! byte-for-byte immutability, `Mmap`/`FileIo`/Journaled/Encrypted parity, and malformed
//! frame fuzzing.

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;

use oifs::disk::{CompressionMode, DiskManager};
use oifs::nfs::{
    ACCESS3_DELETE, ACCESS3_EXECUTE, ACCESS3_EXTEND, ACCESS3_LOOKUP, ACCESS3_MODIFY, ACCESS3_READ,
    CREATE_EXCLUSIVE, CREATE_GUARDED, CREATE_UNCHECKED, FILE_SYNC, MNT3_OK, MNT3ERR_NOENT,
    MNTPROC3_DUMP, MNTPROC3_EXPORT, MNTPROC3_MNT, MNTPROC3_NULL, MNTPROC3_UMNT, MNTPROC3_UMNTALL,
    MOUNT_PROGRAM, MOUNT_VERSION_3, NFS_FILE_HANDLE_LEN, NFS_MAX_FRAME_LEN, NFS_PROGRAM,
    NFS_VERSION_3, NFS3_OK, NFS3ERR_EXIST, NFS3ERR_ISDIR, NFS3ERR_NOENT, NFS3ERR_NOTDIR,
    NFS3ERR_NOTEMPTY, NFS3ERR_NOTSUPP, NFS3ERR_ROFS, NFS3ERR_STALE, NFSPROC3_ACCESS,
    NFSPROC3_COMMIT, NFSPROC3_CREATE, NFSPROC3_FSINFO, NFSPROC3_FSSTAT, NFSPROC3_GETATTR,
    NFSPROC3_LINK, NFSPROC3_LOOKUP, NFSPROC3_MKDIR, NFSPROC3_MKNOD, NFSPROC3_NULL,
    NFSPROC3_PATHCONF, NFSPROC3_READ, NFSPROC3_READDIR, NFSPROC3_READDIRPLUS, NFSPROC3_READLINK,
    NFSPROC3_REMOVE, NFSPROC3_RENAME, NFSPROC3_RMDIR, NFSPROC3_SETATTR, NFSPROC3_SYMLINK,
    NFSPROC3_WRITE, NfsFileHandle, NfsMountOptions, OifsNfsFs, OifsNfsServer, RPC_ACCEPT_SUCCESS,
    RPC_AUTH_NONE, RPC_MSG_CALL, RPC_MSG_REPLY, RPC_REPLY_MSG_ACCEPTED, RPC_VERSION_2, UNSTABLE,
    XdrReader, XdrWriter, read_rpc_record, write_rpc_record,
};
use oifs::storage::StorageMode;
use tempfile::tempdir;

fn build_rpc_call(xid: u32, prog: u32, vers: u32, proc_num: u32, args: &[u8]) -> Vec<u8> {
    let mut w = XdrWriter::with_capacity(48 + args.len());
    w.write_u32(xid);
    w.write_u32(RPC_MSG_CALL);
    w.write_u32(RPC_VERSION_2);
    w.write_u32(prog);
    w.write_u32(vers);
    w.write_u32(proc_num);
    // cred: AUTH_NONE
    w.write_u32(RPC_AUTH_NONE);
    w.write_u32(0);
    // verf: AUTH_NONE
    w.write_u32(RPC_AUTH_NONE);
    w.write_u32(0);
    let mut bytes = w.into_bytes();
    bytes.extend_from_slice(args);
    bytes
}

fn rpc_roundtrip(
    stream: &mut TcpStream,
    xid: u32,
    prog: u32,
    vers: u32,
    proc_num: u32,
    args: &[u8],
) -> Vec<u8> {
    let call = build_rpc_call(xid, prog, vers, proc_num, args);
    write_rpc_record(stream, &call).expect("write RPC call");
    let reply = read_rpc_record(stream)
        .expect("read RPC reply")
        .expect("non-EOF RPC reply");

    let mut r = XdrReader::new(&reply);
    assert_eq!(r.read_u32(), Some(xid));
    assert_eq!(r.read_u32(), Some(RPC_MSG_REPLY));
    assert_eq!(r.read_u32(), Some(RPC_REPLY_MSG_ACCEPTED));
    let _verf_flavor = r.read_u32().unwrap();
    let _verf_body = r.read_opaque(400).unwrap();
    assert_eq!(r.read_u32(), Some(RPC_ACCEPT_SUCCESS));
    reply[r.position()..].to_vec()
}

fn write_empty_sattr3(w: &mut XdrWriter, set_size: Option<u64>) {
    w.write_bool(false); // mode
    w.write_bool(false); // uid
    w.write_bool(false); // gid
    if let Some(sz) = set_size {
        w.write_bool(true);
        w.write_u64(sz);
    } else {
        w.write_bool(false);
    }
    w.write_u32(0); // atime: DONT_CHANGE
    w.write_u32(0); // mtime: DONT_CHANGE
}

fn skip_fattr3(r: &mut XdrReader<'_>) {
    // fattr3 is 21 u32-equivalent words = 84 bytes
    let _ = r.read_fixed_opaque(84).expect("valid fattr3");
}

fn skip_post_op_attr(r: &mut XdrReader<'_>) -> bool {
    let present = r.read_bool().expect("post_op_attr present flag");
    if present {
        skip_fattr3(r);
    }
    present
}

fn skip_wcc_data(r: &mut XdrReader<'_>) {
    if r.read_bool().expect("wcc pre flag") {
        let _ = r.read_fixed_opaque(24).expect("wcc_attr");
    }
    let _ = skip_post_op_attr(r);
}

fn mount_root_via_tcp(stream: &mut TcpStream) -> Vec<u8> {
    let mut args = XdrWriter::new();
    args.write_string("/");
    let body = rpc_roundtrip(
        stream,
        10,
        MOUNT_PROGRAM,
        MOUNT_VERSION_3,
        MNTPROC3_MNT,
        &args.into_bytes(),
    );
    let mut r = XdrReader::new(&body);
    assert_eq!(r.read_u32(), Some(MNT3_OK));
    let fh = r.read_opaque(64).unwrap().to_vec();
    assert_eq!(fh.len(), NFS_FILE_HANDLE_LEN);
    let n_flavors = r.read_u32().unwrap();
    assert!(n_flavors >= 1);
    fh
}

#[test]
fn test_nfs_mount_and_all_v3_procedures_over_tcp() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("nfs_full.img");
    let dm = Arc::new(DiskManager::open_journaled(&img, 16 * 1024 * 1024).unwrap());
    let mut server = OifsNfsServer::bind(Arc::clone(&dm), NfsMountOptions::default()).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();

    // 1. MOUNT v3: NULL, EXPORT, DUMP, bad MNT, good MNT
    let _ = rpc_roundtrip(&mut stream, 1, MOUNT_PROGRAM, MOUNT_VERSION_3, MNTPROC3_NULL, &[]);
    let exp = rpc_roundtrip(&mut stream, 2, MOUNT_PROGRAM, MOUNT_VERSION_3, MNTPROC3_EXPORT, &[]);
    let mut er = XdrReader::new(&exp);
    assert_eq!(er.read_bool(), Some(true));
    assert_eq!(er.read_string(64), Some("/"));
    let _ = rpc_roundtrip(&mut stream, 3, MOUNT_PROGRAM, MOUNT_VERSION_3, MNTPROC3_DUMP, &[]);

    let mut bad_mnt = XdrWriter::new();
    bad_mnt.write_string("/nonexistent");
    let bad_body = rpc_roundtrip(
        &mut stream,
        4,
        MOUNT_PROGRAM,
        MOUNT_VERSION_3,
        MNTPROC3_MNT,
        &bad_mnt.into_bytes(),
    );
    assert_eq!(XdrReader::new(&bad_body).read_u32(), Some(MNT3ERR_NOENT));

    let root_fh = mount_root_via_tcp(&mut stream);

    // 2. NFS NULL, GETATTR, FSSTAT, FSINFO, PATHCONF, ACCESS
    let _ = rpc_roundtrip(&mut stream, 11, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_NULL, &[]);

    let mut fh_arg = XdrWriter::new();
    fh_arg.write_opaque(&root_fh);
    let fh_arg_bytes = fh_arg.into_bytes();

    let ga = rpc_roundtrip(&mut stream, 12, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_GETATTR, &fh_arg_bytes);
    let mut r = XdrReader::new(&ga);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    assert_eq!(r.read_u32(), Some(2)); // NF3DIR

    let fsstat = rpc_roundtrip(&mut stream, 13, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_FSSTAT, &fh_arg_bytes);
    let mut r = XdrReader::new(&fsstat);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    assert!(skip_post_op_attr(&mut r));
    let tbytes = r.read_u64().unwrap();
    let fbytes = r.read_u64().unwrap();
    assert!(tbytes > 0 && fbytes > 0);

    let fsinfo = rpc_roundtrip(&mut stream, 14, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_FSINFO, &fh_arg_bytes);
    assert_eq!(XdrReader::new(&fsinfo).read_u32(), Some(NFS3_OK));

    let pc = rpc_roundtrip(&mut stream, 15, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_PATHCONF, &fh_arg_bytes);
    assert_eq!(XdrReader::new(&pc).read_u32(), Some(NFS3_OK));

    let mut acc_arg = XdrWriter::new();
    acc_arg.write_opaque(&root_fh);
    acc_arg.write_u32(
        ACCESS3_READ | ACCESS3_LOOKUP | ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_DELETE | ACCESS3_EXECUTE,
    );
    let acc = rpc_roundtrip(&mut stream, 16, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_ACCESS, &acc_arg.into_bytes());
    let mut r = XdrReader::new(&acc);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    skip_post_op_attr(&mut r);
    assert_eq!(
        r.read_u32(),
        Some(ACCESS3_READ | ACCESS3_LOOKUP | ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_DELETE | ACCESS3_EXECUTE)
    );

    // 3. MKDIR "docs" + CREATE (GUARDED / EXCLUSIVE / UNCHECKED)
    let mut mk = XdrWriter::new();
    mk.write_opaque(&root_fh);
    mk.write_string("docs");
    write_empty_sattr3(&mut mk, None);
    let mk_res = rpc_roundtrip(&mut stream, 17, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_MKDIR, &mk.into_bytes());
    let mut r = XdrReader::new(&mk_res);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    assert_eq!(r.read_bool(), Some(true));
    let docs_fh = r.read_opaque(64).unwrap().to_vec();

    // CREATE GUARDED "hello.txt"
    let mut cr = XdrWriter::new();
    cr.write_opaque(&docs_fh);
    cr.write_string("hello.txt");
    cr.write_u32(CREATE_GUARDED);
    write_empty_sattr3(&mut cr, None);
    let cr_res = rpc_roundtrip(&mut stream, 18, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_CREATE, &cr.into_bytes());
    let mut r = XdrReader::new(&cr_res);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    assert_eq!(r.read_bool(), Some(true));
    let file_fh = r.read_opaque(64).unwrap().to_vec();

    // Duplicate GUARDED create -> NFS3ERR_EXIST
    let mut cr_dup = XdrWriter::new();
    cr_dup.write_opaque(&docs_fh);
    cr_dup.write_string("hello.txt");
    cr_dup.write_u32(CREATE_GUARDED);
    write_empty_sattr3(&mut cr_dup, None);
    let cr_dup_res = rpc_roundtrip(&mut stream, 19, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_CREATE, &cr_dup.into_bytes());
    assert_eq!(XdrReader::new(&cr_dup_res).read_u32(), Some(NFS3ERR_EXIST));

    // CREATE EXCLUSIVE "excl.txt"
    let mut cr_ex = XdrWriter::new();
    cr_ex.write_opaque(&docs_fh);
    cr_ex.write_string("excl.txt");
    cr_ex.write_u32(CREATE_EXCLUSIVE);
    cr_ex.write_fixed_opaque(&[1, 2, 3, 4, 5, 6, 7, 8]);
    let cr_ex_res = rpc_roundtrip(&mut stream, 20, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_CREATE, &cr_ex.into_bytes());
    assert_eq!(XdrReader::new(&cr_ex_res).read_u32(), Some(NFS3_OK));

    // 4. WRITE (UNSTABLE + FILE_SYNC), COMMIT, READ, SETATTR truncate
    let payload = b"Hello OIFS NFSv3 world!";
    let mut wr = XdrWriter::new();
    wr.write_opaque(&file_fh);
    wr.write_u64(0);
    wr.write_u32(payload.len() as u32);
    wr.write_u32(UNSTABLE);
    wr.write_opaque(payload);
    let wr_res = rpc_roundtrip(&mut stream, 21, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_WRITE, &wr.into_bytes());
    let mut r = XdrReader::new(&wr_res);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    skip_wcc_data(&mut r);
    assert_eq!(r.read_u32(), Some(payload.len() as u32));
    assert_eq!(r.read_u32(), Some(UNSTABLE));

    let mut cm = XdrWriter::new();
    cm.write_opaque(&file_fh);
    cm.write_u64(0);
    cm.write_u32(0);
    let cm_res = rpc_roundtrip(&mut stream, 22, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_COMMIT, &cm.into_bytes());
    assert_eq!(XdrReader::new(&cm_res).read_u32(), Some(NFS3_OK));

    let mut rd = XdrWriter::new();
    rd.write_opaque(&file_fh);
    rd.write_u64(0);
    rd.write_u32(4096);
    let rd_bytes = rd.into_bytes();
    let rd_res = rpc_roundtrip(&mut stream, 23, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_READ, &rd_bytes);
    let mut r = XdrReader::new(&rd_res);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    skip_post_op_attr(&mut r);
    assert_eq!(r.read_u32(), Some(payload.len() as u32));
    assert_eq!(r.read_bool(), Some(true)); // eof
    assert_eq!(r.read_opaque(4096), Some(payload.as_slice()));

    // SETATTR truncate to 5 bytes
    let mut sa = XdrWriter::new();
    sa.write_opaque(&file_fh);
    write_empty_sattr3(&mut sa, Some(5));
    sa.write_bool(false); // guard
    let sa_res = rpc_roundtrip(&mut stream, 24, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_SETATTR, &sa.into_bytes());
    assert_eq!(XdrReader::new(&sa_res).read_u32(), Some(NFS3_OK));

    let rd_res2 = rpc_roundtrip(&mut stream, 25, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_READ, &rd_bytes);
    let mut r = XdrReader::new(&rd_res2);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    skip_post_op_attr(&mut r);
    assert_eq!(r.read_u32(), Some(5));
    assert_eq!(r.read_bool(), Some(true));
    assert_eq!(r.read_opaque(4096), Some(&b"Hello"[..]));

    // 5. LOOKUP Unicode NFD -> NFC + macOS metadata filtering + ".."
    // Create file with NFC "café.txt" (U+00E9) and look up with NFD "cafe\u{0301}.txt"
    let mut cr_uni = XdrWriter::new();
    cr_uni.write_opaque(&docs_fh);
    cr_uni.write_string("caf\u{00e9}.txt");
    cr_uni.write_u32(CREATE_UNCHECKED);
    write_empty_sattr3(&mut cr_uni, None);
    let _ = rpc_roundtrip(&mut stream, 26, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_CREATE, &cr_uni.into_bytes());

    let mut lk_nfd = XdrWriter::new();
    lk_nfd.write_opaque(&docs_fh);
    lk_nfd.write_string("cafe\u{0301}.txt");
    let lk_res = rpc_roundtrip(&mut stream, 27, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_LOOKUP, &lk_nfd.into_bytes());
    assert_eq!(XdrReader::new(&lk_res).read_u32(), Some(NFS3_OK));

    let mut lk_meta = XdrWriter::new();
    lk_meta.write_opaque(&docs_fh);
    lk_meta.write_string("._hello.txt");
    let lk_meta_res = rpc_roundtrip(&mut stream, 28, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_LOOKUP, &lk_meta.into_bytes());
    assert_eq!(XdrReader::new(&lk_meta_res).read_u32(), Some(NFS3ERR_NOENT));

    let mut lk_dotdot = XdrWriter::new();
    lk_dotdot.write_opaque(&docs_fh);
    lk_dotdot.write_string("..");
    let lk_dd_res = rpc_roundtrip(&mut stream, 29, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_LOOKUP, &lk_dotdot.into_bytes());
    let mut r = XdrReader::new(&lk_dd_res);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    assert_eq!(r.read_opaque(64), Some(root_fh.as_slice()));

    // 6. Atomic RENAME across directories ("docs/hello.txt" -> "/moved.txt")
    let mut rn = XdrWriter::new();
    rn.write_opaque(&docs_fh);
    rn.write_string("hello.txt");
    rn.write_opaque(&root_fh);
    rn.write_string("moved.txt");
    let rn_res = rpc_roundtrip(&mut stream, 30, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_RENAME, &rn.into_bytes());
    assert_eq!(XdrReader::new(&rn_res).read_u32(), Some(NFS3_OK));

    // Verify moved.txt exists at root and has "Hello"
    let moved_ino = dm.lookup(0, "moved.txt").unwrap();
    assert_eq!(dm.read_data(moved_ino).unwrap(), b"Hello");

    // 7. REMOVE directory fails with ISDIR; RMDIR non-empty fails with NOTEMPTY; RMDIR file fails with NOTDIR
    let mut rm_dir_as_file = XdrWriter::new();
    rm_dir_as_file.write_opaque(&root_fh);
    rm_dir_as_file.write_string("docs");
    let res = rpc_roundtrip(&mut stream, 31, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_REMOVE, &rm_dir_as_file.into_bytes());
    assert_eq!(XdrReader::new(&res).read_u32(), Some(NFS3ERR_ISDIR));

    let mut rmdir_nonempty = XdrWriter::new();
    rmdir_nonempty.write_opaque(&root_fh);
    rmdir_nonempty.write_string("docs");
    let res = rpc_roundtrip(&mut stream, 32, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_RMDIR, &rmdir_nonempty.into_bytes());
    assert_eq!(XdrReader::new(&res).read_u32(), Some(NFS3ERR_NOTEMPTY));

    let mut rmdir_file = XdrWriter::new();
    rmdir_file.write_opaque(&root_fh);
    rmdir_file.write_string("moved.txt");
    let res = rpc_roundtrip(&mut stream, 33, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_RMDIR, &rmdir_file.into_bytes());
    assert_eq!(XdrReader::new(&res).read_u32(), Some(NFS3ERR_NOTDIR));

    // Remove remaining files in docs, then RMDIR docs succeeds
    for f in ["excl.txt", "caf\u{00e9}.txt"] {
        let mut rm = XdrWriter::new();
        rm.write_opaque(&docs_fh);
        rm.write_string(f);
        let res = rpc_roundtrip(&mut stream, 34, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_REMOVE, &rm.into_bytes());
        assert_eq!(XdrReader::new(&res).read_u32(), Some(NFS3_OK));
    }
    let mut rmdir_ok = XdrWriter::new();
    rmdir_ok.write_opaque(&root_fh);
    rmdir_ok.write_string("docs");
    let res = rpc_roundtrip(&mut stream, 35, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_RMDIR, &rmdir_ok.into_bytes());
    assert_eq!(XdrReader::new(&res).read_u32(), Some(NFS3_OK));

    // 8. Unsupported procedures return NFS3ERR_NOTSUPP cleanly
    for proc_num in [NFSPROC3_READLINK, NFSPROC3_SYMLINK, NFSPROC3_MKNOD, NFSPROC3_LINK] {
        let res = rpc_roundtrip(&mut stream, 40 + proc_num, NFS_PROGRAM, NFS_VERSION_3, proc_num, &[]);
        assert_eq!(XdrReader::new(&res).read_u32(), Some(NFS3ERR_NOTSUPP));
    }

    // 9. Stale generation handle returns NFS3ERR_STALE
    let stale_fh = NfsFileHandle::new(server.fs().fs_generation() ^ 0xFFFF, 0).to_bytes();
    let mut st = XdrWriter::new();
    st.write_opaque(&stale_fh);
    let st_res = rpc_roundtrip(&mut stream, 50, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_GETATTR, &st.into_bytes());
    assert_eq!(XdrReader::new(&st_res).read_u32(), Some(NFS3ERR_STALE));

    // 10. UMNT & UMNTALL
    let mut um = XdrWriter::new();
    um.write_string("/");
    let _ = rpc_roundtrip(&mut stream, 60, MOUNT_PROGRAM, MOUNT_VERSION_3, MNTPROC3_UMNT, &um.into_bytes());
    let _ = rpc_roundtrip(&mut stream, 61, MOUNT_PROGRAM, MOUNT_VERSION_3, MNTPROC3_UMNTALL, &[]);

    drop(stream);
    server.stop();
    assert!(dm.verify_integrity().unwrap().is_clean);
}

#[test]
fn test_nfs_readdir_and_readdirplus_pagination_over_220_entries() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("nfs_many.img");
    let dm = Arc::new(DiskManager::open(&img, 16 * 1024 * 1024).unwrap());

    for i in 0..220 {
        dm.create_file(0, &format!("entry_{i:04}.txt")).unwrap();
    }

    let mut server = OifsNfsServer::bind(Arc::clone(&dm), NfsMountOptions::default()).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    let root_fh = mount_root_via_tcp(&mut stream);

    // 1. Multi-page READDIR with small 512-byte budget
    let mut collected_rd = Vec::new();
    let mut cookie = 0u64;
    let mut pages = 0usize;
    loop {
        let mut args = XdrWriter::new();
        args.write_opaque(&root_fh);
        args.write_u64(cookie);
        args.write_fixed_opaque(&[0u8; 8]);
        args.write_u32(512);
        let body = rpc_roundtrip(
            &mut stream,
            100 + pages as u32,
            NFS_PROGRAM,
            NFS_VERSION_3,
            NFSPROC3_READDIR,
            &args.into_bytes(),
        );
        let mut r = XdrReader::new(&body);
        assert_eq!(r.read_u32(), Some(NFS3_OK));
        skip_post_op_attr(&mut r);
        let _verf = r.read_fixed_opaque(8).unwrap();
        while r.read_bool().unwrap() {
            let fileid = r.read_u64().unwrap();
            assert!(fileid >= 1, "NFS fileid must never be 0");
            let name = r.read_string(256).unwrap().to_string();
            cookie = r.read_u64().unwrap();
            collected_rd.push(name);
        }
        let eof = r.read_bool().unwrap();
        pages += 1;
        assert!(pages < 200, "READDIR pagination loop did not terminate");
        if eof {
            break;
        }
    }
    assert!(pages > 5, "Expected multi-page READDIR pagination, got {pages} pages");
    assert_eq!(collected_rd.len(), 222); // "." + ".." + 220 files

    // 2. Multi-page READDIRPLUS with small 1024-byte maxcount budget
    let mut collected_plus = Vec::new();
    let mut cookie = 0u64;
    let mut plus_pages = 0usize;
    loop {
        let mut args = XdrWriter::new();
        args.write_opaque(&root_fh);
        args.write_u64(cookie);
        args.write_fixed_opaque(&[0u8; 8]);
        args.write_u32(512); // dircount
        args.write_u32(1024); // maxcount
        let body = rpc_roundtrip(
            &mut stream,
            500 + plus_pages as u32,
            NFS_PROGRAM,
            NFS_VERSION_3,
            NFSPROC3_READDIRPLUS,
            &args.into_bytes(),
        );
        let mut r = XdrReader::new(&body);
        assert_eq!(r.read_u32(), Some(NFS3_OK));
        skip_post_op_attr(&mut r);
        let _verf = r.read_fixed_opaque(8).unwrap();
        while r.read_bool().unwrap() {
            let fileid = r.read_u64().unwrap();
            assert!(fileid >= 1);
            let name = r.read_string(256).unwrap().to_string();
            cookie = r.read_u64().unwrap();
            assert!(skip_post_op_attr(&mut r));
            assert_eq!(r.read_bool(), Some(true));
            let fh = r.read_opaque(64).unwrap();
            assert_eq!(fh.len(), NFS_FILE_HANDLE_LEN);
            collected_plus.push(name);
        }
        let eof = r.read_bool().unwrap();
        plus_pages += 1;
        assert!(plus_pages < 200);
        if eof {
            break;
        }
    }
    assert!(plus_pages > 15, "Expected multi-page READDIRPLUS, got {plus_pages} pages");
    assert_eq!(collected_plus, collected_rd);

    drop(stream);
    server.stop();
}

#[test]
fn test_nfs_read_only_enforces_rofs_and_byte_immutability() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("nfs_ro.img");
    {
        let dm = DiskManager::open_journaled(&img, 8 * 1024 * 1024).unwrap();
        dm.create_file_with_data(0, "fixed.txt", b"immutable data", CompressionMode::Auto)
            .unwrap();
        dm.flush().unwrap();
    }
    let before = std::fs::read(&img).unwrap();

    let ro_dm = Arc::new(DiskManager::open_read_only(&img).unwrap());
    let mut server = OifsNfsServer::bind(
        Arc::clone(&ro_dm),
        NfsMountOptions {
            read_only: true,
            ..NfsMountOptions::default()
        },
    )
    .unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    let root_fh = mount_root_via_tcp(&mut stream);

    // Lookup and READ succeed
    let mut lk = XdrWriter::new();
    lk.write_opaque(&root_fh);
    lk.write_string("fixed.txt");
    let lk_res = rpc_roundtrip(&mut stream, 1, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_LOOKUP, &lk.into_bytes());
    let mut r = XdrReader::new(&lk_res);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    let file_fh = r.read_opaque(64).unwrap().to_vec();

    let mut rd = XdrWriter::new();
    rd.write_opaque(&file_fh);
    rd.write_u64(0);
    rd.write_u32(64);
    let rd_res = rpc_roundtrip(&mut stream, 2, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_READ, &rd.into_bytes());
    let mut r = XdrReader::new(&rd_res);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    skip_post_op_attr(&mut r);
    assert_eq!(r.read_u32(), Some(14));
    assert_eq!(r.read_bool(), Some(true));
    assert_eq!(r.read_opaque(64), Some(&b"immutable data"[..]));

    // ACCESS masks off MODIFY/EXTEND/DELETE
    let mut acc = XdrWriter::new();
    acc.write_opaque(&root_fh);
    acc.write_u32(ACCESS3_READ | ACCESS3_LOOKUP | ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_DELETE);
    let acc_res = rpc_roundtrip(&mut stream, 3, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_ACCESS, &acc.into_bytes());
    let mut r = XdrReader::new(&acc_res);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    skip_post_op_attr(&mut r);
    assert_eq!(r.read_u32(), Some(ACCESS3_READ | ACCESS3_LOOKUP));

    // Every mutating proc returns NFS3ERR_ROFS
    let mut wr = XdrWriter::new();
    wr.write_opaque(&file_fh);
    wr.write_u64(0);
    wr.write_u32(4);
    wr.write_u32(FILE_SYNC);
    wr.write_opaque(b"hack");
    assert_eq!(
        XdrReader::new(&rpc_roundtrip(&mut stream, 4, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_WRITE, &wr.into_bytes())).read_u32(),
        Some(NFS3ERR_ROFS)
    );

    let mut cr = XdrWriter::new();
    cr.write_opaque(&root_fh);
    cr.write_string("new.txt");
    cr.write_u32(CREATE_UNCHECKED);
    write_empty_sattr3(&mut cr, None);
    assert_eq!(
        XdrReader::new(&rpc_roundtrip(&mut stream, 5, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_CREATE, &cr.into_bytes())).read_u32(),
        Some(NFS3ERR_ROFS)
    );

    let mut mk = XdrWriter::new();
    mk.write_opaque(&root_fh);
    mk.write_string("dir");
    write_empty_sattr3(&mut mk, None);
    assert_eq!(
        XdrReader::new(&rpc_roundtrip(&mut stream, 6, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_MKDIR, &mk.into_bytes())).read_u32(),
        Some(NFS3ERR_ROFS)
    );

    let mut rm = XdrWriter::new();
    rm.write_opaque(&root_fh);
    rm.write_string("fixed.txt");
    assert_eq!(
        XdrReader::new(&rpc_roundtrip(&mut stream, 7, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_REMOVE, &rm.into_bytes())).read_u32(),
        Some(NFS3ERR_ROFS)
    );

    let mut rn = XdrWriter::new();
    rn.write_opaque(&root_fh);
    rn.write_string("fixed.txt");
    rn.write_opaque(&root_fh);
    rn.write_string("other.txt");
    assert_eq!(
        XdrReader::new(&rpc_roundtrip(&mut stream, 8, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_RENAME, &rn.into_bytes())).read_u32(),
        Some(NFS3ERR_ROFS)
    );

    let mut sa = XdrWriter::new();
    sa.write_opaque(&file_fh);
    write_empty_sattr3(&mut sa, Some(0));
    sa.write_bool(false);
    assert_eq!(
        XdrReader::new(&rpc_roundtrip(&mut stream, 9, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_SETATTR, &sa.into_bytes())).read_u32(),
        Some(NFS3ERR_ROFS)
    );

    drop(stream);
    server.stop();
    drop(ro_dm);

    let after = std::fs::read(&img).unwrap();
    assert_eq!(before, after, "Read-only NFS mount must be 100% byte-for-byte immutable");
}

#[test]
fn test_nfs_file_io_and_encrypted_parity_plus_malformed_fuzzing() {
    let dir = tempdir().unwrap();
    let img = dir.path().join("nfs_enc_fileio.img");
    let dm = Arc::new(
        DiskManager::open_with_options(
            &img,
            16 * 1024 * 1024,
            Some("nfs-secret-pass"),
            true,
            StorageMode::FileIo,
        )
        .unwrap(),
    );

    let fs = OifsNfsFs::from_arc(Arc::clone(&dm), NfsMountOptions::default());

    // Malformed / truncated / garbage frames must never panic
    for bad in [
        &[][..],
        &[0x01][..],
        &[0xFF; 4][..],
        &[0x00; 24][..],
        &[0xFF; 64][..],
        &build_rpc_call(7, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_WRITE, &[0xAA; 12])[..],
        &build_rpc_call(8, NFS_PROGRAM, 99, NFSPROC3_NULL, &[])[..],
        &build_rpc_call(9, 999999, 1, 0, &[])[..],
    ] {
        let reply = fs.dispatch_rpc(bad);
        assert!(reply.len() >= 16);
    }

    // Multi-fragment TCP record reassembly + encrypted FileIo read/write roundtrip
    let mut server = OifsNfsServer::bind(Arc::clone(&dm), NfsMountOptions::default()).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
    let root_fh = mount_root_via_tcp(&mut stream);

    // Send a CREATE RPC split across 2 TCP record-marking fragments!
    let mut cr = XdrWriter::new();
    cr.write_opaque(&root_fh);
    cr.write_string("secret.bin");
    cr.write_u32(CREATE_GUARDED);
    write_empty_sattr3(&mut cr, None);
    let call = build_rpc_call(77, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_CREATE, &cr.into_bytes());
    let mid = call.len() / 2;
    let frag1_hdr = (mid as u32).to_be_bytes(); // bit 31 = 0 (more fragments)
    let frag2_hdr = (0x8000_0000u32 | ((call.len() - mid) as u32)).to_be_bytes();
    stream.write_all(&frag1_hdr).unwrap();
    stream.write_all(&call[..mid]).unwrap();
    stream.write_all(&frag2_hdr).unwrap();
    stream.write_all(&call[mid..]).unwrap();
    stream.flush().unwrap();

    let reply = read_rpc_record(&mut stream).unwrap().unwrap();
    let mut r = XdrReader::new(&reply);
    assert_eq!(r.read_u32(), Some(77));
    assert_eq!(r.read_u32(), Some(RPC_MSG_REPLY));
    assert_eq!(r.read_u32(), Some(RPC_REPLY_MSG_ACCEPTED));
    let _ = r.read_u32();
    let _ = r.read_opaque(400);
    assert_eq!(r.read_u32(), Some(RPC_ACCEPT_SUCCESS));
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    assert_eq!(r.read_bool(), Some(true));
    let secret_fh = r.read_opaque(64).unwrap().to_vec();

    // Write & read 64 KiB encrypted payload over FileIo
    let data: Vec<u8> = (0usize..65536).map(|i| (i * 29 % 251) as u8).collect();
    let mut wr = XdrWriter::new();
    wr.write_opaque(&secret_fh);
    wr.write_u64(0);
    wr.write_u32(data.len() as u32);
    wr.write_u32(FILE_SYNC);
    wr.write_opaque(&data);
    let wr_res = rpc_roundtrip(&mut stream, 78, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_WRITE, &wr.into_bytes());
    assert_eq!(XdrReader::new(&wr_res).read_u32(), Some(NFS3_OK));

    let mut rd = XdrWriter::new();
    rd.write_opaque(&secret_fh);
    rd.write_u64(0);
    rd.write_u32(data.len() as u32);
    let rd_res = rpc_roundtrip(&mut stream, 79, NFS_PROGRAM, NFS_VERSION_3, NFSPROC3_READ, &rd.into_bytes());
    let mut r = XdrReader::new(&rd_res);
    assert_eq!(r.read_u32(), Some(NFS3_OK));
    skip_post_op_attr(&mut r);
    assert_eq!(r.read_u32(), Some(data.len() as u32));
    assert_eq!(r.read_bool(), Some(true));
    assert_eq!(r.read_opaque(128 * 1024), Some(data.as_slice()));

    // Oversized fragment header (> NFS_MAX_FRAME_LEN) closes connection safely without OOM
    let huge_hdr = (0x8000_0000u32 | ((NFS_MAX_FRAME_LEN as u32) + 4096)).to_be_bytes();
    let _ = stream.write_all(&huge_hdr);
    let _ = stream.flush();
    assert!(read_rpc_record(&mut stream).unwrap_or(None).is_none());

    drop(stream);
    server.stop();
    assert!(dm.verify_integrity().unwrap().is_clean);
}
