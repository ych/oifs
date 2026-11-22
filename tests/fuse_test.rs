use oifs::disk::{CompressionMode, DiskManager};
use oifs::fuse::{
    FATTR_MTIME, FATTR_SIZE, FUSE_ACCESS, FUSE_ASYNC_READ, FUSE_ATOMIC_O_TRUNC,
    FUSE_ATTR_MACOS_SIZE, FUSE_ATTR_OUT_MACOS_SIZE, FUSE_ATTR_OUT_SIZE, FUSE_ATTR_SIZE,
    FUSE_BATCH_FORGET, FUSE_BIG_WRITES, FUSE_CREATE, FUSE_DESTROY, FUSE_DT_DIR, FUSE_DT_REG,
    FUSE_ENTRY_OUT_MACOS_SIZE, FUSE_ENTRY_OUT_SIZE, FUSE_FLUSH, FUSE_FORGET, FUSE_FSYNC,
    FUSE_GETATTR, FUSE_GETXATTR, FUSE_INIT, FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION,
    FUSE_LISTXATTR, FUSE_LOOKUP, FUSE_MAX_PAGES, FUSE_MKDIR, FUSE_OPEN, FUSE_OPENDIR,
    FUSE_OUT_HEADER_SIZE, FUSE_READ, FUSE_READDIR, FUSE_RELEASE, FUSE_RELEASEDIR, FUSE_REMOVEXATTR,
    FUSE_RMDIR, FUSE_ROOT_ID, FUSE_SETATTR, FUSE_SETXATTR, FUSE_STATFS, FUSE_UNLINK, FUSE_WRITE,
    FuseAbiPlatform, FuseAccessIn, FuseAttr, FuseAttrOut, FuseCreateIn, FuseDirEntry, FuseEntryOut,
    FuseFlushIn, FuseFsyncIn, FuseGetXattrIn, FuseInHeader, FuseInitIn, FuseInitOut, FuseKStatfs,
    FuseMkdirIn, FuseMountOptions, FuseOpenIn, FuseOpenOut, FuseOutHeader, FuseReadIn,
    FuseReleaseIn, FuseSetAttrIn, FuseSetXattrIn, FuseWriteIn, FuseWriteOut, OifsFuseFs,
    fuse_dirent_align, inode_to_nodeid, is_macos_metadata_filename, nodeid_to_inode,
    normalize_filename_nfc, recv_fd_scm_rights, send_fd_scm_rights,
};
use oifs::storage::StorageMode;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Wire frame helpers for tests
// ---------------------------------------------------------------------------

fn build_cstr_payload(prefix: &[u8], name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + name.len() + 1);
    out.extend_from_slice(prefix);
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out
}

fn parse_reply(resp: &[u8]) -> (FuseOutHeader, &[u8]) {
    let hdr = FuseOutHeader::parse(resp).expect("valid FuseOutHeader");
    assert_eq!(hdr.len as usize, resp.len(), "reply len header mismatch");
    (hdr, &resp[FUSE_OUT_HEADER_SIZE..])
}

fn send_recv_frame(stream: &mut UnixStream, req: &[u8]) -> Vec<u8> {
    stream.write_all(req).expect("write request frame");
    let mut hdr_buf = [0u8; FUSE_OUT_HEADER_SIZE];
    stream
        .read_exact(&mut hdr_buf)
        .expect("read response header");
    let hdr = FuseOutHeader::parse(&hdr_buf).expect("parse response header");
    let total = hdr.len as usize;
    assert!(total >= FUSE_OUT_HEADER_SIZE);
    let mut full = vec![0u8; total];
    full[..FUSE_OUT_HEADER_SIZE].copy_from_slice(&hdr_buf);
    if total > FUSE_OUT_HEADER_SIZE {
        stream
            .read_exact(&mut full[FUSE_OUT_HEADER_SIZE..])
            .expect("read response body");
    }
    full
}

fn send_recv_packet_fd(fd: std::os::unix::io::RawFd, req: &[u8]) -> Vec<u8> {
    // SAFETY: `req` is a valid readable slice.
    let sent = unsafe { libc::write(fd, req.as_ptr().cast(), req.len()) };
    assert_eq!(sent as usize, req.len(), "packet write failed");
    let mut buf = vec![0u8; 64 * 1024];
    // SAFETY: `buf` is a valid writable slice.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    assert!(
        n >= FUSE_OUT_HEADER_SIZE as isize,
        "packet read failed: {n}"
    );
    buf.truncate(n as usize);
    buf
}

fn count_open_os_fds() -> usize {
    #[cfg(target_os = "macos")]
    let fd_dir = "/dev/fd";
    #[cfg(target_os = "linux")]
    let fd_dir = "/proc/self/fd";
    std::fs::read_dir(fd_dir)
        .expect("read open fd directory")
        .count()
}

fn count_fds_for_path(target: &std::path::Path) -> usize {
    let canonical = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    #[cfg(target_os = "macos")]
    {
        let mut count = 0usize;
        if let Ok(entries) = std::fs::read_dir("/dev/fd") {
            for entry in entries.flatten() {
                let Ok(fd) = entry.file_name().to_string_lossy().parse::<libc::c_int>() else {
                    continue;
                };
                let mut buf = [0u8; libc::PATH_MAX as usize];
                // SAFETY: `F_GETPATH` writes a NUL-terminated path into `buf` of length `PATH_MAX`.
                if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } == 0 {
                    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                    if let Ok(p) = std::str::from_utf8(&buf[..len])
                        && std::path::Path::new(p) == canonical
                    {
                        count += 1;
                    }
                }
            }
        }
        count
    }
    #[cfg(target_os = "linux")]
    {
        let mut count = 0usize;
        if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
            for entry in entries.flatten() {
                if let Ok(link) = std::fs::read_link(entry.path())
                    && link == canonical
                {
                    count += 1;
                }
            }
        }
        count
    }
}

// ---------------------------------------------------------------------------
// 1. Full VFS lifecycle (high-level + binary wire) across Mmap/FileIo,
//    plain / journaled / encrypted
// ---------------------------------------------------------------------------

fn run_full_lifecycle(dm: DiskManager) {
    let fs = OifsFuseFs::new(
        dm,
        FuseMountOptions {
            compression_mode: CompressionMode::Auto,
            ..FuseMountOptions::default()
        },
    );

    let root_attr = fs.getattr(FUSE_ROOT_ID).expect("getattr root");
    assert_eq!(root_attr.attr.ino, FUSE_ROOT_ID);
    assert_eq!(root_attr.attr.mode & 0o170000, 0o040000);

    let dir_entry = fs.mkdir(FUSE_ROOT_ID, "alpha", 0o755).expect("mkdir alpha");
    assert!(dir_entry.nodeid > FUSE_ROOT_ID);
    assert_eq!(dir_entry.attr.mode & 0o170000, 0o040000);

    let (file_entry, open_out) = fs
        .create(dir_entry.nodeid, "hello.txt", libc::O_RDWR as u32, 0o644)
        .expect("create hello.txt");
    assert_eq!(fs.open_handle_count(), 1);

    let payload = b"Pure-Rust FUSE Kernel ABI v7.28 over OIFS!";
    let written = fs
        .write(file_entry.nodeid, open_out.fh, 0, payload, 0)
        .expect("write payload");
    assert_eq!(written as usize, payload.len());

    fs.flush(file_entry.nodeid, open_out.fh).expect("flush");
    fs.fsync(file_entry.nodeid, open_out.fh, false)
        .expect("fsync");

    let got = fs
        .read(file_entry.nodeid, open_out.fh, 0, 4096)
        .expect("read payload");
    assert_eq!(got, payload);

    let slice = fs
        .read(file_entry.nodeid, open_out.fh, 10, 11)
        .expect("positional read");
    assert_eq!(slice, &payload[10..21]);

    let looked_up = fs
        .lookup(dir_entry.nodeid, "hello.txt")
        .expect("lookup hello.txt");
    assert_eq!(looked_up.nodeid, file_entry.nodeid);
    assert_eq!(looked_up.attr.size, payload.len() as u64);

    fs.release(file_entry.nodeid, open_out.fh).expect("release");
    assert_eq!(fs.open_handle_count(), 0);

    let init_in = FuseInitIn {
        major: FUSE_KERNEL_VERSION,
        minor: FUSE_KERNEL_MINOR_VERSION,
        max_readahead: 256 * 1024,
        flags: FUSE_ASYNC_READ | FUSE_BIG_WRITES | FUSE_ATOMIC_O_TRUNC | FUSE_MAX_PAGES,
    };
    let req = FuseInHeader::build_frame(FUSE_INIT, 1, 0, &init_in.to_bytes());
    let resp = fs.dispatch_frame(&req);
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(hdr.unique, 1);
    let init_out = FuseInitOut::parse(body).expect("parse FuseInitOut");
    assert_eq!(init_out.major, FUSE_KERNEL_VERSION);
    assert_eq!(init_out.minor, FUSE_KERNEL_MINOR_VERSION);
    assert!(init_out.max_write > 0);
    assert_ne!(init_out.flags & FUSE_MAX_PAGES, 0);

    let create_in = FuseCreateIn {
        flags: (libc::O_RDWR | libc::O_CREAT | libc::O_EXCL) as u32,
        mode: 0o100644,
        umask: 0o022,
        padding: 0,
    };
    let create_payload = build_cstr_payload(&create_in.to_bytes(), "wire.bin");
    let req = FuseInHeader::build_frame(FUSE_CREATE, 2, dir_entry.nodeid, &create_payload);
    let resp = fs.dispatch_frame(&req);
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    let wire_entry = FuseEntryOut::parse(&body[..FUSE_ENTRY_OUT_SIZE]).expect("entry");
    let wire_open = FuseOpenOut::parse(&body[FUSE_ENTRY_OUT_SIZE..]).expect("open_out");
    assert!(wire_open.fh != 0);

    let big_data: Vec<u8> = (0..16384u32).map(|i| (i % 251) as u8).collect();
    let write_in = FuseWriteIn {
        fh: wire_open.fh,
        offset: 0,
        size: big_data.len() as u32,
        write_flags: 0,
        lock_owner: 0,
        flags: 0,
        padding: 0,
    };
    let mut wpayload = Vec::with_capacity(40 + big_data.len());
    wpayload.extend_from_slice(&write_in.to_bytes());
    wpayload.extend_from_slice(&big_data);
    let req = FuseInHeader::build_frame(FUSE_WRITE, 3, wire_entry.nodeid, &wpayload);
    let resp = fs.dispatch_frame(&req);
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    let write_out = FuseWriteOut::parse(body).expect("write_out");
    assert_eq!(write_out.size as usize, big_data.len());

    let read_in = FuseReadIn {
        fh: wire_open.fh,
        offset: 0,
        size: big_data.len() as u32,
        ..FuseReadIn::default()
    };
    let req = FuseInHeader::build_frame(FUSE_READ, 4, wire_entry.nodeid, &read_in.to_bytes());
    let resp = fs.dispatch_frame(&req);
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(body, big_data.as_slice());

    let req = FuseInHeader::build_frame(FUSE_GETATTR, 5, wire_entry.nodeid, &[]);
    let resp = fs.dispatch_frame(&req);
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    let attr_out = FuseAttrOut::parse(body).expect("attr_out");
    assert_eq!(attr_out.attr.size, big_data.len() as u64);

    let rel_in = FuseReleaseIn {
        fh: wire_open.fh,
        ..FuseReleaseIn::default()
    };
    let req = FuseInHeader::build_frame(FUSE_RELEASE, 6, wire_entry.nodeid, &rel_in.to_bytes());
    let resp = fs.dispatch_frame(&req);
    let (hdr, _) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);

    let req = FuseInHeader::build_frame(
        FUSE_UNLINK,
        7,
        dir_entry.nodeid,
        &build_cstr_payload(&[], "wire.bin"),
    );
    assert_eq!(parse_reply(&fs.dispatch_frame(&req)).0.error, 0);
    let req = FuseInHeader::build_frame(
        FUSE_UNLINK,
        8,
        dir_entry.nodeid,
        &build_cstr_payload(&[], "hello.txt"),
    );
    assert_eq!(parse_reply(&fs.dispatch_frame(&req)).0.error, 0);
    let req = FuseInHeader::build_frame(
        FUSE_RMDIR,
        9,
        FUSE_ROOT_ID,
        &build_cstr_payload(&[], "alpha"),
    );
    assert_eq!(parse_reply(&fs.dispatch_frame(&req)).0.error, 0);
}

#[test]
fn test_fuse_lifecycle_mmap_and_file_io_plain_journaled_encrypted() {
    let dir = tempdir().expect("tempdir");
    let img_size = 8 * 1024 * 1024;

    let dm = DiskManager::open_with_storage_mode(
        dir.path().join("mmap_plain.img"),
        img_size,
        StorageMode::Mmap,
    )
    .expect("open mmap plain");
    run_full_lifecycle(dm);

    let dm = DiskManager::open_file_io(dir.path().join("fileio_plain.img"), img_size)
        .expect("open fileio plain");
    run_full_lifecycle(dm);

    let dm = DiskManager::open_with_options(
        dir.path().join("mmap_journal.img"),
        img_size,
        None,
        true,
        StorageMode::Mmap,
    )
    .expect("open mmap journaled");
    run_full_lifecycle(dm);

    let dm = DiskManager::open_file_io_journaled(dir.path().join("fileio_journal.img"), img_size)
        .expect("open fileio journaled");
    run_full_lifecycle(dm);

    let dm = DiskManager::create_encrypted(
        dir.path().join("mmap_enc.img"),
        img_size,
        "fuse-secret-pass",
    )
    .expect("open mmap encrypted");
    run_full_lifecycle(dm);

    let dm = DiskManager::create_encrypted_file_io(
        dir.path().join("fileio_enc.img"),
        img_size,
        "fuse-secret-pass",
    )
    .expect("open fileio encrypted");
    run_full_lifecycle(dm);
}

// ---------------------------------------------------------------------------
// 2. "1,000 open FUSE files = 1 OS FD" single-container property
// ---------------------------------------------------------------------------

#[test]
fn test_fuse_1000_open_files_zero_extra_os_fds() {
    let dir = tempdir().expect("tempdir");
    let img_path = dir.path().join("thousand_files.img");
    let dm = DiskManager::open_with_storage_mode(&img_path, 32 * 1024 * 1024, StorageMode::Mmap)
        .expect("open image");
    let fs = OifsFuseFs::new(
        dm,
        FuseMountOptions {
            compression_mode: CompressionMode::Never,
            ..FuseMountOptions::default()
        },
    );

    let fds_before = count_open_os_fds();
    let img_fds_before = count_fds_for_path(&img_path);
    assert!(
        img_fds_before >= 1,
        "expected backing .img file to have an active OS FD"
    );
    let mut handles: Vec<(u64, u64)> = Vec::with_capacity(1000);

    for i in 0..1000usize {
        let name = format!("f_{i:04}.dat");
        if i < 500 {
            let create_in = FuseCreateIn {
                flags: (libc::O_RDWR | libc::O_CREAT | libc::O_EXCL) as u32,
                mode: 0o100644,
                umask: 0,
                padding: 0,
            };
            let req = FuseInHeader::build_frame(
                FUSE_CREATE,
                (i + 1) as u64,
                FUSE_ROOT_ID,
                &build_cstr_payload(&create_in.to_bytes(), &name),
            );
            let resp = fs.dispatch_frame(&req);
            let (hdr, body) = parse_reply(&resp);
            assert_eq!(hdr.error, 0, "FUSE_CREATE failed for {name}");
            let entry = FuseEntryOut::parse(&body[..FUSE_ENTRY_OUT_SIZE]).unwrap();
            let open_out = FuseOpenOut::parse(&body[FUSE_ENTRY_OUT_SIZE..]).unwrap();
            handles.push((entry.nodeid, open_out.fh));
        } else {
            let (entry, first_open) = fs
                .create(FUSE_ROOT_ID, &name, libc::O_RDWR as u32, 0o644)
                .expect("create");
            fs.release(entry.nodeid, first_open.fh).expect("release");
            let open_in = FuseOpenIn {
                flags: libc::O_RDWR as u32,
                unused: 0,
            };
            let req = FuseInHeader::build_frame(
                FUSE_OPEN,
                (i + 1) as u64,
                entry.nodeid,
                &open_in.to_bytes(),
            );
            let resp = fs.dispatch_frame(&req);
            let (hdr, body) = parse_reply(&resp);
            assert_eq!(hdr.error, 0, "FUSE_OPEN failed for {name}");
            let open_out = FuseOpenOut::parse(body).unwrap();
            handles.push((entry.nodeid, open_out.fh));
        }
    }

    assert_eq!(fs.open_handle_count(), 1000);
    let img_fds_while_open = count_fds_for_path(&img_path);
    assert_eq!(
        img_fds_while_open, img_fds_before,
        "opening 1,000 FUSE handles must reuse the single container OS FD without opening extra FDs"
    );
    let fds_while_open = count_open_os_fds();
    assert!(
        fds_while_open < fds_before + 64,
        "opening 1,000 FUSE handles must not allocate per-file OS FDs (before={fds_before}, after={fds_while_open})"
    );

    for (idx, &(nodeid, fh)) in handles.iter().enumerate() {
        let data = format!("payload-for-handle-{idx:04}-fh-{fh}");
        let written = fs
            .write(nodeid, fh, 0, data.as_bytes(), 0)
            .expect("write handle");
        assert_eq!(written as usize, data.len());
    }
    for (idx, &(nodeid, fh)) in handles.iter().enumerate() {
        let expected = format!("payload-for-handle-{idx:04}-fh-{fh}");
        let got = fs.read(nodeid, fh, 0, 256).expect("read handle");
        assert_eq!(got, expected.as_bytes());
    }

    assert_eq!(
        count_fds_for_path(&img_path),
        img_fds_before,
        "I/O across 1,000 FUSE handles must not leak OS FDs"
    );

    for (idx, &(nodeid, fh)) in handles.iter().enumerate() {
        let rel = FuseReleaseIn {
            fh,
            ..FuseReleaseIn::default()
        };
        let req =
            FuseInHeader::build_frame(FUSE_RELEASE, (2000 + idx) as u64, nodeid, &rel.to_bytes());
        assert_eq!(parse_reply(&fs.dispatch_frame(&req)).0.error, 0);
    }
    assert_eq!(fs.open_handle_count(), 0);
}

// ---------------------------------------------------------------------------
// 3. FUSE_READDIR pagination with `.` and `..` + multi-block directories
// ---------------------------------------------------------------------------

#[test]
fn test_fuse_readdir_pagination_multiblock_and_alignment() {
    let dir = tempdir().expect("tempdir");
    let dm = DiskManager::open_with_storage_mode(
        dir.path().join("readdir.img"),
        16 * 1024 * 1024,
        StorageMode::Mmap,
    )
    .expect("open");
    let fs = OifsFuseFs::new(dm, FuseMountOptions::default());

    let sub = fs
        .mkdir(FUSE_ROOT_ID, "bigdir", 0o755)
        .expect("mkdir bigdir");
    let mut expected: BTreeMap<String, (u64, u32)> = BTreeMap::new();
    expected.insert(".".to_string(), (sub.nodeid, FUSE_DT_DIR));
    expected.insert("..".to_string(), (FUSE_ROOT_ID, FUSE_DT_DIR));

    for i in 0..250usize {
        let name = format!("entry_{i:03}_{}", "x".repeat(i % 13));
        let (e, o) = fs
            .create(sub.nodeid, &name, libc::O_RDWR as u32, 0o644)
            .expect("create entry");
        fs.release(e.nodeid, o.fh).expect("release");
        expected.insert(name, (e.nodeid, FUSE_DT_REG));
    }
    for i in 0..10usize {
        let name = format!("subdir_{i:02}");
        let e = fs.mkdir(sub.nodeid, &name, 0o755).expect("mkdir sub");
        expected.insert(name, (e.nodeid, FUSE_DT_DIR));
    }

    let open_in = FuseOpenIn {
        flags: libc::O_RDONLY as u32,
        unused: 0,
    };
    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_OPENDIR,
        10,
        sub.nodeid,
        &open_in.to_bytes(),
    ));
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    let dir_fh = FuseOpenOut::parse(body).unwrap().fh;

    let mut offset = 0u64;
    let mut seen: BTreeMap<String, (u64, u32)> = BTreeMap::new();
    let mut pages = 0usize;
    loop {
        let read_in = FuseReadIn {
            fh: dir_fh,
            offset,
            size: 192,
            ..FuseReadIn::default()
        };
        let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
            FUSE_READDIR,
            100 + pages as u64,
            sub.nodeid,
            &read_in.to_bytes(),
        ));
        let (hdr, body) = parse_reply(&resp);
        assert_eq!(hdr.error, 0);
        assert!(body.len() <= 192, "reply exceeded requested buffer");
        assert_eq!(
            body.len() % 8,
            0,
            "fuse_dirent stream must be 8-byte aligned"
        );
        if body.is_empty() {
            break;
        }
        let entries = FuseDirEntry::parse_stream(body).expect("parse dirents");
        assert!(!entries.is_empty());
        for ent in entries {
            assert_eq!(ent.wire_len(), fuse_dirent_align(24 + ent.name.len()));
            assert!(ent.off > offset, "dirent offset must strictly advance");
            offset = ent.off;
            let prev = seen.insert(ent.name.clone(), (ent.ino, ent.file_type));
            assert!(prev.is_none(), "duplicate dirent returned: {}", ent.name);
        }
        pages += 1;
    }
    assert!(
        pages > 20,
        "expected multi-page readdir pagination, got {pages}"
    );
    assert_eq!(seen, expected);

    let rel_in = FuseReleaseIn {
        fh: dir_fh,
        ..FuseReleaseIn::default()
    };
    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_RELEASEDIR,
        999,
        sub.nodeid,
        &rel_in.to_bytes(),
    ));
    assert_eq!(parse_reply(&resp).0.error, 0);
}

// ---------------------------------------------------------------------------
// 4. SETATTR (FATTR_SIZE), O_TRUNC, O_APPEND, UNLINK, RMDIR (ENOTEMPTY), STATFS
// ---------------------------------------------------------------------------

#[test]
fn test_fuse_setattr_trunc_append_unlink_rmdir_statfs() {
    let dir = tempdir().expect("tempdir");
    let dm = DiskManager::open_with_storage_mode(
        dir.path().join("ops.img"),
        8 * 1024 * 1024,
        StorageMode::FileIo,
    )
    .expect("open");
    let fs = OifsFuseFs::new(
        dm,
        FuseMountOptions {
            compression_mode: CompressionMode::Never,
            ..FuseMountOptions::default()
        },
    );

    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_STATFS,
        1,
        FUSE_ROOT_ID,
        &[],
    ));
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    let st_before = FuseKStatfs::parse(body).expect("statfs before");
    assert!(st_before.blocks > 0);
    assert!(st_before.bfree > 0);
    assert!(
        st_before.bavail * (st_before.frsize as u64) > 0,
        "writable mount must report non-zero available space for macOS Finder drag-and-drop"
    );
    assert_eq!(st_before.bsize, 4096);
    assert_eq!(st_before.frsize, 4096);
    assert_eq!(st_before.namelen, 255);

    let (file_ent, open_append) = fs
        .create(
            FUSE_ROOT_ID,
            "log.txt",
            (libc::O_RDWR | libc::O_APPEND) as u32,
            0o644,
        )
        .expect("create log.txt");

    for chunk in [
        b"first-".as_slice(),
        b"second-".as_slice(),
        b"third".as_slice(),
    ] {
        let wi = FuseWriteIn {
            fh: open_append.fh,
            offset: 0,
            size: chunk.len() as u32,
            flags: libc::O_APPEND as u32,
            ..FuseWriteIn::default()
        };
        let mut p = Vec::with_capacity(40 + chunk.len());
        p.extend_from_slice(&wi.to_bytes());
        p.extend_from_slice(chunk);
        let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
            FUSE_WRITE,
            2,
            file_ent.nodeid,
            &p,
        ));
        let (hdr, body) = parse_reply(&resp);
        assert_eq!(hdr.error, 0);
        assert_eq!(
            FuseWriteOut::parse(body).unwrap().size as usize,
            chunk.len()
        );
    }
    assert_eq!(
        fs.read(file_ent.nodeid, open_append.fh, 0, 128).unwrap(),
        b"first-second-third"
    );

    let setattr_shrink = FuseSetAttrIn {
        valid: FATTR_SIZE,
        size: 5,
        ..FuseSetAttrIn::default()
    };
    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_SETATTR,
        3,
        file_ent.nodeid,
        &setattr_shrink.to_bytes(),
    ));
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(FuseAttrOut::parse(body).unwrap().attr.size, 5);
    assert_eq!(
        fs.read(file_ent.nodeid, open_append.fh, 0, 128).unwrap(),
        b"first"
    );

    let setattr_grow = FuseSetAttrIn {
        valid: FATTR_SIZE,
        size: 10,
        ..FuseSetAttrIn::default()
    };
    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_SETATTR,
        4,
        file_ent.nodeid,
        &setattr_grow.to_bytes(),
    ));
    assert_eq!(parse_reply(&resp).0.error, 0);
    assert_eq!(
        fs.read(file_ent.nodeid, open_append.fh, 0, 128).unwrap(),
        b"first\0\0\0\0\0"
    );
    fs.release(file_ent.nodeid, open_append.fh).unwrap();

    let open_trunc = FuseOpenIn {
        flags: (libc::O_RDWR | libc::O_TRUNC) as u32,
        unused: 0,
    };
    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_OPEN,
        5,
        file_ent.nodeid,
        &open_trunc.to_bytes(),
    ));
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    let trunc_fh = FuseOpenOut::parse(body).unwrap().fh;
    assert_eq!(fs.getattr(file_ent.nodeid).unwrap().attr.size, 0);
    fs.release(file_ent.nodeid, trunc_fh).unwrap();

    let st_after_create = fs.statfs(FUSE_ROOT_ID).unwrap();
    assert_eq!(st_after_create.ffree, st_before.ffree - 1);

    let sub = fs.mkdir(FUSE_ROOT_ID, "folder", 0o755).unwrap();
    let (child, child_o) = fs
        .create(sub.nodeid, "inside.txt", libc::O_RDWR as u32, 0o644)
        .unwrap();
    fs.release(child.nodeid, child_o.fh).unwrap();

    assert_eq!(fs.unlink(FUSE_ROOT_ID, "folder"), Err(-libc::EISDIR));
    assert_eq!(fs.rmdir(FUSE_ROOT_ID, "log.txt"), Err(-libc::ENOTDIR));
    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_RMDIR,
        6,
        FUSE_ROOT_ID,
        &build_cstr_payload(&[], "folder"),
    ));
    assert_eq!(parse_reply(&resp).0.error, -libc::ENOTEMPTY);

    fs.unlink(sub.nodeid, "inside.txt").unwrap();
    fs.rmdir(FUSE_ROOT_ID, "folder").unwrap();
    fs.unlink(FUSE_ROOT_ID, "log.txt").unwrap();

    let st_after_delete = fs.statfs(FUSE_ROOT_ID).unwrap();
    assert_eq!(st_after_delete.ffree, st_before.ffree);
}

// ---------------------------------------------------------------------------
// 5. Read-only FUSE mount: EROFS on every mutating opcode & byte-for-byte immutability
// ---------------------------------------------------------------------------

#[test]
fn test_fuse_read_only_erofs_and_immutability() {
    let dir = tempdir().expect("tempdir");
    let img_path = dir.path().join("readonly.img");

    let (file_nodeid, dir_nodeid) = {
        let dm = DiskManager::open_with_storage_mode(&img_path, 4 * 1024 * 1024, StorageMode::Mmap)
            .expect("create image");
        let fs = OifsFuseFs::new(dm, FuseMountOptions::default());
        let d = fs.mkdir(FUSE_ROOT_ID, "docs", 0o755).unwrap();
        let (f, o) = fs
            .create(d.nodeid, "readme.txt", libc::O_RDWR as u32, 0o644)
            .unwrap();
        fs.write(f.nodeid, o.fh, 0, b"immutable data", 0).unwrap();
        fs.fsync(f.nodeid, o.fh, false).unwrap();
        fs.release(f.nodeid, o.fh).unwrap();
        (f.nodeid, d.nodeid)
    };

    let bytes_before = std::fs::read(&img_path).expect("snapshot image before");

    for use_dm_ro in [true, false] {
        let (dm, opts) = if use_dm_ro {
            (
                DiskManager::open_read_only(&img_path).expect("open_read_only"),
                FuseMountOptions::default(),
            )
        } else {
            (
                DiskManager::open_read_only_with_storage_mode(&img_path, None, StorageMode::FileIo)
                    .expect("open_read_only file_io"),
                FuseMountOptions {
                    read_only: true,
                    ..FuseMountOptions::default()
                },
            )
        };
        let fs = OifsFuseFs::new(dm, opts);
        assert!(fs.is_read_only());

        let attr = fs.getattr(file_nodeid).expect("getattr ro");
        assert_eq!(attr.attr.size, 14);
        assert_eq!(attr.attr.mode & 0o777, 0o444);

        let open_ro = fs
            .open(file_nodeid, libc::O_RDONLY as u32)
            .expect("open O_RDONLY");
        assert_eq!(
            fs.read(file_nodeid, open_ro.fh, 0, 64).unwrap(),
            b"immutable data"
        );
        fs.flush(file_nodeid, open_ro.fh).expect("flush ro noop");
        fs.fsync(file_nodeid, open_ro.fh, false)
            .expect("fsync ro noop");
        fs.access(file_nodeid, libc::R_OK as u32)
            .expect("access R_OK");
        let dir_o = fs.opendir(dir_nodeid, libc::O_RDONLY as u32).unwrap();
        let ents = fs.readdir(dir_nodeid, dir_o.fh, 0, 4096).unwrap();
        assert_eq!(ents.len(), 3);
        fs.releasedir(dir_nodeid, dir_o.fh).unwrap();

        let st_ro = fs.statfs(FUSE_ROOT_ID).unwrap();
        assert!(st_ro.bfree > 0);
        assert_eq!(st_ro.bavail, 0, "read-only statfs must report bavail == 0");

        let create_in = FuseCreateIn {
            flags: (libc::O_RDWR | libc::O_CREAT) as u32,
            mode: 0o100644,
            umask: 0,
            padding: 0,
        };
        let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
            FUSE_CREATE,
            1,
            FUSE_ROOT_ID,
            &build_cstr_payload(&create_in.to_bytes(), "new.txt"),
        ));
        assert_eq!(parse_reply(&resp).0.error, -libc::EROFS);

        let write_in = FuseWriteIn {
            fh: open_ro.fh,
            offset: 0,
            size: 4,
            ..FuseWriteIn::default()
        };
        let mut wp = Vec::from(write_in.to_bytes());
        wp.extend_from_slice(b"evil");
        let resp = fs.dispatch_frame(&FuseInHeader::build_frame(FUSE_WRITE, 2, file_nodeid, &wp));
        assert_eq!(parse_reply(&resp).0.error, -libc::EROFS);

        let mkdir_in = FuseMkdirIn {
            mode: 0o755,
            umask: 0,
        };
        let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
            FUSE_MKDIR,
            3,
            FUSE_ROOT_ID,
            &build_cstr_payload(&mkdir_in.to_bytes(), "newdir"),
        ));
        assert_eq!(parse_reply(&resp).0.error, -libc::EROFS);

        let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
            FUSE_UNLINK,
            4,
            dir_nodeid,
            &build_cstr_payload(&[], "readme.txt"),
        ));
        assert_eq!(parse_reply(&resp).0.error, -libc::EROFS);

        let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
            FUSE_RMDIR,
            5,
            FUSE_ROOT_ID,
            &build_cstr_payload(&[], "docs"),
        ));
        assert_eq!(parse_reply(&resp).0.error, -libc::EROFS);

        for valid in [FATTR_SIZE, FATTR_MTIME] {
            let sa = FuseSetAttrIn {
                valid,
                size: 0,
                ..FuseSetAttrIn::default()
            };
            let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
                FUSE_SETATTR,
                6,
                file_nodeid,
                &sa.to_bytes(),
            ));
            assert_eq!(parse_reply(&resp).0.error, -libc::EROFS);
        }

        for flag in [libc::O_WRONLY, libc::O_RDWR, libc::O_CREAT, libc::O_TRUNC] {
            let oi = FuseOpenIn {
                flags: flag as u32,
                unused: 0,
            };
            let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
                FUSE_OPEN,
                7,
                file_nodeid,
                &oi.to_bytes(),
            ));
            assert_eq!(parse_reply(&resp).0.error, -libc::EROFS);
        }

        let acc = FuseAccessIn {
            mask: libc::W_OK as u32,
            padding: 0,
        };
        let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
            FUSE_ACCESS,
            8,
            file_nodeid,
            &acc.to_bytes(),
        ));
        assert_eq!(parse_reply(&resp).0.error, -libc::EROFS);

        fs.release(file_nodeid, open_ro.fh).unwrap();
    }

    let bytes_after = std::fs::read(&img_path).expect("snapshot image after");
    assert_eq!(
        bytes_before, bytes_after,
        "read-only FUSE mount must preserve 100% byte-for-byte image immutability"
    );
}

// ---------------------------------------------------------------------------
// 6. Live FUSE protocol over Unix stream socketpair (serve_stream)
// ---------------------------------------------------------------------------

#[test]
fn test_fuse_live_unix_socketpair_session() {
    let dir = tempdir().expect("tempdir");
    let dm = DiskManager::open_with_storage_mode(
        dir.path().join("socketpair.img"),
        4 * 1024 * 1024,
        StorageMode::Mmap,
    )
    .expect("open");
    let fs = Arc::new(OifsFuseFs::new(dm, FuseMountOptions::default()));
    let (mut client, mut server) = UnixStream::pair().expect("UnixStream::pair");

    let server_fs = Arc::clone(&fs);
    let server_thread = std::thread::spawn(move || server_fs.serve_stream(&mut server));

    let init_in = FuseInitIn {
        major: FUSE_KERNEL_VERSION,
        minor: FUSE_KERNEL_MINOR_VERSION,
        max_readahead: 65536,
        flags: FUSE_ASYNC_READ | FUSE_MAX_PAGES,
    };
    let resp = send_recv_frame(
        &mut client,
        &FuseInHeader::build_frame(FUSE_INIT, 1, 0, &init_in.to_bytes()),
    );
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(hdr.unique, 1);
    let init_out = FuseInitOut::parse(body).unwrap();
    assert_eq!(init_out.major, 7);
    assert_eq!(init_out.minor, 28);

    let ci = FuseCreateIn {
        flags: (libc::O_RDWR | libc::O_CREAT) as u32,
        mode: 0o100644,
        umask: 0,
        padding: 0,
    };
    let resp = send_recv_frame(
        &mut client,
        &FuseInHeader::build_frame(
            FUSE_CREATE,
            2,
            FUSE_ROOT_ID,
            &build_cstr_payload(&ci.to_bytes(), "stream.txt"),
        ),
    );
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    let created = FuseEntryOut::parse(&body[..FUSE_ENTRY_OUT_SIZE]).unwrap();
    let opened = FuseOpenOut::parse(&body[FUSE_ENTRY_OUT_SIZE..]).unwrap();

    let msg = b"hello over unix socketpair FUSE ABI v7.28!";
    let wi = FuseWriteIn {
        fh: opened.fh,
        offset: 0,
        size: msg.len() as u32,
        ..FuseWriteIn::default()
    };
    let mut wp = Vec::from(wi.to_bytes());
    wp.extend_from_slice(msg);
    let resp = send_recv_frame(
        &mut client,
        &FuseInHeader::build_frame(FUSE_WRITE, 3, created.nodeid, &wp),
    );
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(FuseWriteOut::parse(body).unwrap().size as usize, msg.len());

    let fi = FuseFlushIn {
        fh: opened.fh,
        ..FuseFlushIn::default()
    };
    let resp = send_recv_frame(
        &mut client,
        &FuseInHeader::build_frame(FUSE_FLUSH, 4, created.nodeid, &fi.to_bytes()),
    );
    assert_eq!(parse_reply(&resp).0.error, 0);

    let fsi = FuseFsyncIn {
        fh: opened.fh,
        fsync_flags: 0,
        padding: 0,
    };
    let resp = send_recv_frame(
        &mut client,
        &FuseInHeader::build_frame(FUSE_FSYNC, 5, created.nodeid, &fsi.to_bytes()),
    );
    assert_eq!(parse_reply(&resp).0.error, 0);

    let ri = FuseReadIn {
        fh: opened.fh,
        offset: 0,
        size: 1024,
        ..FuseReadIn::default()
    };
    let resp = send_recv_frame(
        &mut client,
        &FuseInHeader::build_frame(FUSE_READ, 6, created.nodeid, &ri.to_bytes()),
    );
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(body, msg);

    let resp = send_recv_frame(
        &mut client,
        &FuseInHeader::build_frame(FUSE_GETATTR, 7, created.nodeid, &[]),
    );
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(
        FuseAttrOut::parse(body).unwrap().attr.size,
        msg.len() as u64
    );

    let oi = FuseOpenIn {
        flags: libc::O_RDONLY as u32,
        unused: 0,
    };
    let resp = send_recv_frame(
        &mut client,
        &FuseInHeader::build_frame(FUSE_OPENDIR, 8, FUSE_ROOT_ID, &oi.to_bytes()),
    );
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    let dfh = FuseOpenOut::parse(body).unwrap().fh;

    let dri = FuseReadIn {
        fh: dfh,
        offset: 0,
        size: 4096,
        ..FuseReadIn::default()
    };
    let resp = send_recv_frame(
        &mut client,
        &FuseInHeader::build_frame(FUSE_READDIR, 9, FUSE_ROOT_ID, &dri.to_bytes()),
    );
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    let dirents = FuseDirEntry::parse_stream(body).unwrap();
    assert!(
        dirents
            .iter()
            .any(|e| e.name == "stream.txt" && e.ino == created.nodeid)
    );

    let rdi = FuseReleaseIn {
        fh: dfh,
        ..FuseReleaseIn::default()
    };
    let resp = send_recv_frame(
        &mut client,
        &FuseInHeader::build_frame(FUSE_RELEASEDIR, 10, FUSE_ROOT_ID, &rdi.to_bytes()),
    );
    assert_eq!(parse_reply(&resp).0.error, 0);

    let forget_req =
        FuseInHeader::build_frame(FUSE_FORGET, 11, created.nodeid, &1u64.to_le_bytes());
    client.write_all(&forget_req).unwrap();
    let batch_forget_req = FuseInHeader::build_frame(FUSE_BATCH_FORGET, 12, 0, &[0u8; 8]);
    client.write_all(&batch_forget_req).unwrap();

    let resp = send_recv_frame(
        &mut client,
        &FuseInHeader::build_frame(FUSE_DESTROY, 13, 0, &[]),
    );
    let (hdr, _) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(hdr.unique, 13);

    server_thread
        .join()
        .expect("server thread panicked")
        .expect("serve_stream returned error");
    assert_eq!(fs.open_handle_count(), 0);
}

// ---------------------------------------------------------------------------
// 7. Dual Linux (88B) & macOS (104B) ABI layout verification on every host
// ---------------------------------------------------------------------------

#[test]
fn test_fuse_dual_linux_and_macos_abi_layouts() {
    let dir = tempdir().expect("tempdir");
    let dm = DiskManager::open_with_storage_mode(
        dir.path().join("dual_abi.img"),
        4 * 1024 * 1024,
        StorageMode::Mmap,
    )
    .expect("open");

    let sample = FuseAttr {
        ino: 42,
        size: 12345,
        blocks: 25,
        atime: 1_700_000_001,
        mtime: 1_700_000_002,
        ctime: 1_700_000_003,
        crtime: 1_700_000_004,
        atimensec: 11,
        mtimensec: 22,
        ctimensec: 33,
        crtimensec: 44,
        mode: 0o100644,
        nlink: 1,
        uid: 501,
        gid: 20,
        rdev: 0,
        flags: 0x0000_0002,
        blksize: 4096,
        padding: 0,
    };
    let linux_bytes = sample.to_bytes();
    assert_eq!(linux_bytes.len(), FUSE_ATTR_SIZE);
    let parsed_linux = FuseAttr::parse(&linux_bytes).expect("parse Linux FuseAttr");
    assert_eq!(parsed_linux.ino, 42);
    assert_eq!(parsed_linux.size, 12345);
    assert_eq!(parsed_linux.mode, 0o100644);
    assert_eq!(parsed_linux.blksize, 4096);

    let macos_bytes = sample.to_macos_bytes();
    assert_eq!(macos_bytes.len(), FUSE_ATTR_MACOS_SIZE);
    let parsed_macos = FuseAttr::parse_macos(&macos_bytes).expect("parse macOS FuseAttr");
    assert_eq!(parsed_macos, sample);

    let fs_macos = OifsFuseFs::new(
        dm,
        FuseMountOptions {
            abi_platform: FuseAbiPlatform::MacOs,
            ..FuseMountOptions::default()
        },
    );

    let ci = FuseCreateIn {
        flags: (libc::O_RDWR | libc::O_CREAT) as u32,
        mode: 0o100644,
        umask: 0,
        padding: 0,
    };
    let resp = fs_macos.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_CREATE,
        1,
        FUSE_ROOT_ID,
        &build_cstr_payload(&ci.to_bytes(), "darwin.txt"),
    ));
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(body.len(), FUSE_ENTRY_OUT_MACOS_SIZE + 16);
    let mac_entry = FuseEntryOut::parse_macos(&body[..FUSE_ENTRY_OUT_MACOS_SIZE]).unwrap();
    let mac_open = FuseOpenOut::parse(&body[FUSE_ENTRY_OUT_MACOS_SIZE..]).unwrap();
    assert!(mac_entry.attr.crtime > 0);
    assert_eq!(mac_entry.attr.blksize, 4096);

    fs_macos
        .write(mac_entry.nodeid, mac_open.fh, 0, b"cross-platform", 0)
        .unwrap();

    let resp_mac = fs_macos.dispatch_frame_for_platform(
        &FuseInHeader::build_frame(FUSE_GETATTR, 2, mac_entry.nodeid, &[]),
        FuseAbiPlatform::MacOs,
    );
    let (hdr_mac, body_mac) = parse_reply(&resp_mac);
    assert_eq!(hdr_mac.error, 0);
    assert_eq!(body_mac.len(), FUSE_ATTR_OUT_MACOS_SIZE);
    let ao_mac = FuseAttrOut::parse_macos(body_mac).unwrap();
    assert_eq!(ao_mac.attr.size, 14);
    assert_eq!(ao_mac.attr.crtime, mac_entry.attr.crtime);

    let resp_lin = fs_macos.dispatch_frame_for_platform(
        &FuseInHeader::build_frame(FUSE_GETATTR, 3, mac_entry.nodeid, &[]),
        FuseAbiPlatform::Linux,
    );
    let (hdr_lin, body_lin) = parse_reply(&resp_lin);
    assert_eq!(hdr_lin.error, 0);
    assert_eq!(body_lin.len(), FUSE_ATTR_OUT_SIZE);
    let ao_lin = FuseAttrOut::parse(body_lin).unwrap();
    assert_eq!(ao_lin.attr.size, 14);
    assert_eq!(ao_lin.attr.ino, mac_entry.nodeid);
    assert_eq!(ao_lin.attr.mode, ao_mac.attr.mode);

    fs_macos.release(mac_entry.nodeid, mac_open.fh).unwrap();
}

// ---------------------------------------------------------------------------
// 8. macOS Finder metadata storm filter, xattr (-ENOTSUP), NFD->NFC,
//    geteuid/getegid defaults, and OS-filtered mount options
// ---------------------------------------------------------------------------

#[test]
fn test_fuse_macos_metadata_storm_xattr_nfc_and_os_mount_options() {
    // 8a. macOS metadata filename predicate
    for junk in [
        ".DS_Store",
        "._report.pdf",
        "._.",
        ".Spotlight-V100",
        ".Trashes",
        ".fseventsd",
        ".TemporaryItems",
    ] {
        assert!(is_macos_metadata_filename(junk), "expected junk: {junk}");
    }
    for legit in ["notes.txt", ".gitignore", ".env", "..hidden", "cafe.txt"] {
        assert!(
            !is_macos_metadata_filename(legit),
            "expected legit: {legit}"
        );
    }

    // 8b. UTF-8 NFD -> NFC normalization (ASCII fast-path, Latin, Kana, Hangul)
    assert!(matches!(
        normalize_filename_nfc("plain_ascii.txt"),
        Cow::Borrowed("plain_ascii.txt")
    ));
    assert_eq!(
        normalize_filename_nfc("cafe\u{0301}.txt"),
        "caf\u{00E9}.txt"
    );
    assert_eq!(
        normalize_filename_nfc("n\u{0303}o\u{0308}a\u{030A}"),
        "\u{00F1}\u{00F6}\u{00E5}"
    );
    // Japanese Kana dakuten & handakuten: が (か + ゙), パ (ハ + ゚)
    assert_eq!(
        normalize_filename_nfc("\u{304B}\u{3099}\u{30CF}\u{309A}.txt"),
        "\u{304C}\u{30D1}.txt"
    );
    // Korean Hangul Jamo L+V+T: 한 (\u{1112}\u{1161}\u{11AB} -> \u{D55C}), 글 (\u{1100}\u{1173}\u{11AF} -> \u{AE00})
    assert_eq!(
        normalize_filename_nfc("\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}.md"),
        "\u{D55C}\u{AE00}.md"
    );

    let dir = tempdir().expect("tempdir");
    let dm = DiskManager::open_with_storage_mode(
        dir.path().join("macos_compat.img"),
        4 * 1024 * 1024,
        StorageMode::Mmap,
    )
    .expect("open");
    let opts = FuseMountOptions {
        volname: Some("OifsVol".to_string()),
        allow_other: true,
        ..FuseMountOptions::default()
    };
    // Verify default uid/gid match effective process uid/gid
    // SAFETY: `geteuid`/`getegid` are always safe.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    assert_eq!(opts.uid, euid);
    assert_eq!(opts.gid, egid);
    assert!(opts.reject_macos_metadata);
    assert!(opts.normalize_nfc);

    let fs = OifsFuseFs::new(dm, opts);

    // 8c. Finder metadata storm fast rejection
    for junk in [
        ".DS_Store",
        "._foo.txt",
        ".Spotlight-V100",
        ".Trashes",
        ".fseventsd",
        ".TemporaryItems",
    ] {
        assert_eq!(fs.lookup(FUSE_ROOT_ID, junk), Err(-libc::ENOENT));
        assert_eq!(
            fs.create(FUSE_ROOT_ID, junk, libc::O_RDWR as u32, 0o644),
            Err(-libc::ENOENT)
        );
        assert_eq!(fs.mkdir(FUSE_ROOT_ID, junk, 0o755), Err(-libc::ENOENT));
    }

    // 8d. NFD <-> NFC round-trip across create, lookup, read, unlink
    let (cafe_entry, cafe_open) = fs
        .create(FUSE_ROOT_ID, "caf\u{00E9}.txt", libc::O_RDWR as u32, 0o644)
        .expect("create NFC cafe");
    fs.write(cafe_entry.nodeid, cafe_open.fh, 0, b"espresso", 0)
        .unwrap();
    fs.release(cafe_entry.nodeid, cafe_open.fh).unwrap();

    // macOS Finder sends NFD "cafe\u{0301}.txt" -> must find NFC "caf\u{00E9}.txt"!
    let found_nfd = fs
        .lookup(FUSE_ROOT_ID, "cafe\u{0301}.txt")
        .expect("NFD lookup matches NFC file");
    assert_eq!(found_nfd.nodeid, cafe_entry.nodeid);

    // Create with Hangul NFD, lookup with Hangul NFC
    let (hangul_entry, hangul_open) = fs
        .create(
            FUSE_ROOT_ID,
            "\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}.txt",
            libc::O_RDWR as u32,
            0o644,
        )
        .expect("create Hangul NFD");
    fs.release(hangul_entry.nodeid, hangul_open.fh).unwrap();
    let found_hangul = fs
        .lookup(FUSE_ROOT_ID, "\u{D55C}\u{AE00}.txt")
        .expect("NFC Hangul lookup matches NFD-created file");
    assert_eq!(found_hangul.nodeid, hangul_entry.nodeid);

    // Create with Kana NFD, lookup with Kana NFC
    let (kana_entry, kana_open) = fs
        .create(
            FUSE_ROOT_ID,
            "\u{304B}\u{3099}\u{30CF}\u{309A}.txt",
            libc::O_RDWR as u32,
            0o644,
        )
        .expect("create Kana NFD");
    fs.release(kana_entry.nodeid, kana_open.fh).unwrap();
    let found_kana = fs
        .lookup(FUSE_ROOT_ID, "\u{304C}\u{30D1}.txt")
        .expect("NFC Kana lookup matches NFD-created file");
    assert_eq!(found_kana.nodeid, kana_entry.nodeid);

    // 8e. Extended attributes MUST return -ENOTSUP (never -ENOSYS or -EINVAL)
    //     for both Linux (8B) and macOS (16B with `position`) wire headers!
    let getx_lin = FuseGetXattrIn {
        size: 64,
        ..FuseGetXattrIn::default()
    };
    let resp = fs.dispatch_frame_for_platform(
        &FuseInHeader::build_frame(
            FUSE_GETXATTR,
            1,
            cafe_entry.nodeid,
            &build_cstr_payload(&getx_lin.to_bytes(), "com.apple.quarantine"),
        ),
        FuseAbiPlatform::Linux,
    );
    assert_eq!(parse_reply(&resp).0.error, -libc::ENOTSUP);

    let getx_mac = FuseGetXattrIn {
        size: 64,
        padding: 0,
        position: 0,
        padding2: 0,
    };
    let resp = fs.dispatch_frame_for_platform(
        &FuseInHeader::build_frame(
            FUSE_GETXATTR,
            2,
            cafe_entry.nodeid,
            &build_cstr_payload(&getx_mac.to_macos_bytes(), "com.apple.FinderInfo"),
        ),
        FuseAbiPlatform::MacOs,
    );
    assert_eq!(parse_reply(&resp).0.error, -libc::ENOTSUP);

    let setx_lin = FuseSetXattrIn {
        size: 4,
        flags: 0,
        position: 0,
        padding: 0,
    };
    let mut setx_lin_payload = build_cstr_payload(&setx_lin.to_bytes(), "user.test");
    setx_lin_payload.extend_from_slice(b"val1");
    let resp = fs.dispatch_frame_for_platform(
        &FuseInHeader::build_frame(FUSE_SETXATTR, 3, cafe_entry.nodeid, &setx_lin_payload),
        FuseAbiPlatform::Linux,
    );
    assert_eq!(parse_reply(&resp).0.error, -libc::ENOTSUP);

    let mut setx_mac_payload =
        build_cstr_payload(&setx_lin.to_macos_bytes(), "com.apple.quarantine");
    setx_mac_payload.extend_from_slice(b"val1");
    let resp = fs.dispatch_frame_for_platform(
        &FuseInHeader::build_frame(FUSE_SETXATTR, 4, cafe_entry.nodeid, &setx_mac_payload),
        FuseAbiPlatform::MacOs,
    );
    assert_eq!(parse_reply(&resp).0.error, -libc::ENOTSUP);

    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_LISTXATTR,
        5,
        cafe_entry.nodeid,
        &getx_lin.to_bytes(),
    ));
    assert_eq!(parse_reply(&resp).0.error, -libc::ENOTSUP);

    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_REMOVEXATTR,
        6,
        cafe_entry.nodeid,
        &build_cstr_payload(&[], "user.test"),
    ));
    assert_eq!(parse_reply(&resp).0.error, -libc::ENOTSUP);

    // 8f. OS-filtered mount options: Linux must strip macOS-only options; macOS must include them.
    let macos_opts = fs.build_mount_options_for_os("macos");
    assert!(macos_opts.contains(&"default_permissions".to_string()));
    assert!(macos_opts.contains(&"allow_other".to_string()));
    assert!(macos_opts.contains(&"noappledouble".to_string()));
    assert!(macos_opts.contains(&"noapplexattr".to_string()));
    assert!(macos_opts.contains(&"local".to_string()));
    assert!(macos_opts.contains(&"iosize=1048576".to_string()));
    assert!(macos_opts.contains(&"volname=OifsVol".to_string()));
    assert!(!macos_opts.contains(&"ro".to_string()));

    let linux_opts = fs.build_mount_options_for_os("linux");
    assert!(linux_opts.contains(&"default_permissions".to_string()));
    assert!(linux_opts.contains(&"allow_other".to_string()));
    assert!(linux_opts.contains(&"subtype=oifs".to_string()));
    for forbidden in [
        "noappledouble",
        "noapplexattr",
        "local",
        "iosize=1048576",
        "volname=OifsVol",
    ] {
        assert!(
            !linux_opts
                .iter()
                .any(|o| o == forbidden || o.starts_with("volname=") || o.starts_with("iosize=")),
            "Linux mount options must not include macOS-only option {forbidden}: {linux_opts:?}"
        );
    }

    // 8g. Dynamic Linux / macOS max_write negotiation based on kernel capabilities.
    use oifs::fuse::{FUSE_DEFAULT_MAX_WRITE, FUSE_LEGACY_MAX_WRITE, negotiate_max_write};
    // Linux < 4.20 (no FUSE_MAX_PAGES): automatically capped at 128 KiB, max_pages = 0.
    assert_eq!(
        negotiate_max_write(
            FUSE_DEFAULT_MAX_WRITE,
            FUSE_BIG_WRITES,
            FuseAbiPlatform::Linux,
            4096,
            256
        ),
        (FUSE_LEGACY_MAX_WRITE, 0)
    );
    // Linux 4.20-6.12 (FUSE_MAX_PAGES, limit=256, 4K page): 1 MiB, 256 pages.
    assert_eq!(
        negotiate_max_write(
            FUSE_DEFAULT_MAX_WRITE,
            FUSE_BIG_WRITES | FUSE_MAX_PAGES,
            FuseAbiPlatform::Linux,
            4096,
            256
        ),
        (1024 * 1024, 256)
    );
    // Linux 4.20-6.12 requesting 4 MiB still capped to kernel limit 256 * 4K = 1 MiB.
    assert_eq!(
        negotiate_max_write(
            4 * 1024 * 1024,
            FUSE_BIG_WRITES | FUSE_MAX_PAGES,
            FuseAbiPlatform::Linux,
            4096,
            256
        ),
        (1024 * 1024, 256)
    );
    // Linux 6.13+ (max_pages_limit=1024, 4K page) requesting 4 MiB -> 4 MiB, 1024 pages.
    assert_eq!(
        negotiate_max_write(
            4 * 1024 * 1024,
            FUSE_BIG_WRITES | FUSE_MAX_PAGES,
            FuseAbiPlatform::Linux,
            4096,
            1024
        ),
        (4 * 1024 * 1024, 1024)
    );
    // Linux ARM64 16K page requesting 1 MiB -> 1 MiB, 64 pages.
    assert_eq!(
        negotiate_max_write(
            1024 * 1024,
            FUSE_BIG_WRITES | FUSE_MAX_PAGES,
            FuseAbiPlatform::Linux,
            16384,
            256
        ),
        (1024 * 1024, 64)
    );
    // macOS macFUSE with FUSE_BIG_WRITES -> 1 MiB even without FUSE_MAX_PAGES.
    assert_eq!(
        negotiate_max_write(
            1024 * 1024,
            FUSE_BIG_WRITES,
            FuseAbiPlatform::MacOs,
            16384,
            256
        ),
        (1024 * 1024, 0)
    );

    // Read-only flag emits "ro" on both OSes
    let ro_opts = FuseMountOptions::default().build_mount_options_for_os("linux", true);
    assert!(ro_opts.contains(&"ro".to_string()));
    let ro_mac_opts = FuseMountOptions::default().build_mount_options_for_os("macos", true);
    assert!(ro_mac_opts.contains(&"ro".to_string()));
}

// ---------------------------------------------------------------------------
// 9. Atomic packet device I/O (serve_fuse_fd over AF_UNIX SOCK_DGRAM) &
//    SCM_RIGHTS real file descriptor passing
// ---------------------------------------------------------------------------

#[test]
fn test_fuse_serve_fuse_fd_packet_channel_and_scm_rights() {
    let dir = tempdir().expect("tempdir");
    let token_path = dir.path().join("scm_token.txt");
    {
        let mut f = std::fs::File::create(&token_path).expect("create token file");
        f.write_all(b"fd-passed-via-scm-rights!").expect("write");
    }
    let orig_file = std::fs::File::open(&token_path).expect("open token file");

    let mut stream_fds = [0 as libc::c_int; 2];
    // SAFETY: valid two-element array for `socketpair(2)`.
    assert_eq!(
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, stream_fds.as_mut_ptr()) },
        0
    );
    // SAFETY: freshly created descriptors.
    let sender_sock = unsafe { OwnedFd::from_raw_fd(stream_fds[0]) };
    let receiver_sock = unsafe { OwnedFd::from_raw_fd(stream_fds[1]) };

    send_fd_scm_rights(sender_sock.as_raw_fd(), orig_file.as_raw_fd()).expect("send_fd_scm_rights");
    let received_fd = recv_fd_scm_rights(receiver_sock.as_raw_fd()).expect("recv_fd_scm_rights");
    let mut received_file = std::fs::File::from(received_fd);
    received_file.seek(SeekFrom::Start(0)).unwrap();
    let mut content = String::new();
    received_file.read_to_string(&mut content).unwrap();
    assert_eq!(content, "fd-passed-via-scm-rights!");

    let dm = DiskManager::open_with_storage_mode(
        dir.path().join("packet_dev.img"),
        4 * 1024 * 1024,
        StorageMode::Mmap,
    )
    .expect("open");
    let fs = Arc::new(OifsFuseFs::new(dm, FuseMountOptions::for_host_kernel()));

    let mut dgram_fds = [0 as libc::c_int; 2];
    // SAFETY: valid two-element array for `socketpair(2)`.
    assert_eq!(
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, dgram_fds.as_mut_ptr()) },
        0
    );
    // SAFETY: freshly created descriptors.
    let client_pkt = unsafe { OwnedFd::from_raw_fd(dgram_fds[0]) };
    let server_pkt = unsafe { OwnedFd::from_raw_fd(dgram_fds[1]) };

    let server_fs = Arc::clone(&fs);
    let server_thread = std::thread::spawn(move || {
        let res = server_fs.serve_fuse_fd(server_pkt.as_raw_fd());
        drop(server_pkt);
        res
    });

    let host_abi = FuseAbiPlatform::host();

    let init_in = FuseInitIn {
        major: FUSE_KERNEL_VERSION,
        minor: FUSE_KERNEL_MINOR_VERSION,
        max_readahead: 65536,
        flags: FUSE_ASYNC_READ | FUSE_MAX_PAGES,
    };
    let resp = send_recv_packet_fd(
        client_pkt.as_raw_fd(),
        &FuseInHeader::build_frame(FUSE_INIT, 10, 0, &init_in.to_bytes()),
    );
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(
        FuseInitOut::parse(body).unwrap().minor,
        FUSE_KERNEL_MINOR_VERSION
    );

    let ci = FuseCreateIn {
        flags: (libc::O_RDWR | libc::O_CREAT) as u32,
        mode: 0o100644,
        umask: 0,
        padding: 0,
    };
    let resp = send_recv_packet_fd(
        client_pkt.as_raw_fd(),
        &FuseInHeader::build_frame(
            FUSE_CREATE,
            11,
            FUSE_ROOT_ID,
            &build_cstr_payload(&ci.to_bytes(), "packet.dat"),
        ),
    );
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    let entry_sz = host_abi.entry_out_size();
    let entry = FuseEntryOut::parse_for(&body[..entry_sz], host_abi).unwrap();
    let open_out = FuseOpenOut::parse(&body[entry_sz..]).unwrap();

    let data = b"atomic packet device payload";
    let wi = FuseWriteIn {
        fh: open_out.fh,
        offset: 0,
        size: data.len() as u32,
        ..FuseWriteIn::default()
    };
    let mut wp = Vec::from(wi.to_bytes());
    wp.extend_from_slice(data);
    let resp = send_recv_packet_fd(
        client_pkt.as_raw_fd(),
        &FuseInHeader::build_frame(FUSE_WRITE, 12, entry.nodeid, &wp),
    );
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(FuseWriteOut::parse(body).unwrap().size as usize, data.len());

    let ri = FuseReadIn {
        fh: open_out.fh,
        offset: 0,
        size: 128,
        ..FuseReadIn::default()
    };
    let resp = send_recv_packet_fd(
        client_pkt.as_raw_fd(),
        &FuseInHeader::build_frame(FUSE_READ, 13, entry.nodeid, &ri.to_bytes()),
    );
    let (hdr, body) = parse_reply(&resp);
    assert_eq!(hdr.error, 0);
    assert_eq!(body, data);

    let resp = send_recv_packet_fd(
        client_pkt.as_raw_fd(),
        &FuseInHeader::build_frame(FUSE_DESTROY, 14, 0, &[]),
    );
    assert_eq!(parse_reply(&resp).0.error, 0);

    server_thread
        .join()
        .expect("serve_fuse_fd thread panicked")
        .expect("serve_fuse_fd failed");
    assert_eq!(fs.open_handle_count(), 0);
}

// ---------------------------------------------------------------------------
// 10. Malformed frame robustness, NodeID bijection, and mount error reporting
// ---------------------------------------------------------------------------

#[test]
fn test_fuse_malformed_frames_bijection_and_mount_error() {
    assert_eq!(inode_to_nodeid(0), Some(FUSE_ROOT_ID));
    assert_eq!(nodeid_to_inode(0), None);
    assert_eq!(nodeid_to_inode(FUSE_ROOT_ID), Some(0));
    assert_eq!(inode_to_nodeid(u64::MAX), None);
    for id in [0u64, 1, 42, 65535, u64::MAX - 1] {
        let nid = inode_to_nodeid(id).unwrap();
        assert_eq!(nodeid_to_inode(nid), Some(id));
    }

    let dir = tempdir().expect("tempdir");
    let dm = DiskManager::open_with_storage_mode(
        dir.path().join("robust.img"),
        2 * 1024 * 1024,
        StorageMode::Mmap,
    )
    .expect("open");
    let fs = OifsFuseFs::new(dm, FuseMountOptions::default());

    for len in 0..40usize {
        let garbage = vec![0xABu8; len];
        let resp = fs.dispatch_frame(&garbage);
        let (hdr, _) = parse_reply(&resp);
        assert_eq!(hdr.error, -libc::EINVAL);
    }

    let mut bad_len = FuseInHeader::build_frame(FUSE_GETATTR, 77, FUSE_ROOT_ID, &[]);
    bad_len[0..4].copy_from_slice(&999u32.to_le_bytes());
    let (hdr, _) = parse_reply(&fs.dispatch_frame(&bad_len));
    assert_eq!(hdr.error, -libc::EINVAL);
    assert_eq!(hdr.unique, 77);

    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(9999, 88, FUSE_ROOT_ID, &[]));
    assert_eq!(parse_reply(&resp).0.error, -libc::ENOSYS);

    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(FUSE_GETATTR, 89, 0, &[]));
    assert_eq!(parse_reply(&resp).0.error, -libc::ENOENT);

    let bad_read = FuseReadIn {
        fh: 999_999,
        offset: 0,
        size: 16,
        ..FuseReadIn::default()
    };
    let resp = fs.dispatch_frame(&FuseInHeader::build_frame(
        FUSE_READ,
        90,
        FUSE_ROOT_ID,
        &bad_read.to_bytes(),
    ));
    assert_eq!(parse_reply(&resp).0.error, -libc::EBADF);

    for opcode in [
        FUSE_INIT,
        FUSE_OPEN,
        FUSE_CREATE,
        FUSE_MKDIR,
        FUSE_READ,
        FUSE_WRITE,
        FUSE_SETATTR,
        FUSE_RELEASE,
        FUSE_FLUSH,
        FUSE_FSYNC,
        FUSE_SETXATTR,
        FUSE_GETXATTR,
        FUSE_LISTXATTR,
        FUSE_REMOVEXATTR,
        FUSE_OPENDIR,
        FUSE_READDIR,
        FUSE_RELEASEDIR,
        FUSE_ACCESS,
        FUSE_LOOKUP,
        FUSE_UNLINK,
        FUSE_RMDIR,
    ] {
        let resp = fs.dispatch_frame(&FuseInHeader::build_frame(opcode, 91, FUSE_ROOT_ID, &[]));
        let (hdr, _) = parse_reply(&resp);
        assert_eq!(hdr.error, -libc::EINVAL, "opcode {opcode} with empty body");
    }

    let err = fs
        .mount(dir.path().join("does_not_exist"))
        .expect_err("missing mountpoint");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}
