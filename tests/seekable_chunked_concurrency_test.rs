use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;
use tempfile::tempdir;

use oifs::disk::{CompressionMode, DiskManager};
use oifs::filters::FilterConfig;
use oifs::inode::{CHUNK_SIZE_64K, INODE_FLAG_SEEKABLE_64K};
use oifs::session::OifsSession;

struct TempImageContext {
    path: String,
}

impl TempImageContext {
    fn new(name: &str) -> Self {
        let path = format!("{}.img", name);
        if Path::new(&path).exists() {
            let _ = fs::remove_file(&path);
        }
        let master = format!("{}.master", path);
        if Path::new(&master).exists() {
            let _ = fs::remove_file(&master);
        }
        Self { path }
    }
}

impl Drop for TempImageContext {
    fn drop(&mut self) {
        if Path::new(&self.path).exists() {
            let _ = fs::remove_file(&self.path);
        }
        let master = format!("{}.master", self.path);
        if Path::new(&master).exists() {
            let _ = fs::remove_file(&master);
        }
    }
}

// =========================================================================
// 1. Multithread: Concurrent read_at on Seekable 64KB Chunked File
// =========================================================================

#[test]
fn test_multithread_concurrent_chunked_read_at() {
    let ctx = TempImageContext::new("test_mt_chunked_read_at");
    let dm = Arc::new(DiskManager::open(&ctx.path, 30 * 1024 * 1024).expect("open"));
    let root = dm.superblock().root_inode;
    let file_id = dm.create_file(root, "chunked_shared.dat").expect("create");

    // 16 chunks = 1MB of predictable structured data
    let num_chunks = 16;
    let total_size = num_chunks * CHUNK_SIZE_64K;
    let mut initial_data = Vec::with_capacity(total_size);
    for c in 0..num_chunks {
        let pattern = format!("CHUNK_{:03}_PATTERN_HEADER_DATA_PADDING_", c);
        let mut chunk = pattern
            .repeat(CHUNK_SIZE_64K / pattern.len() + 1)
            .into_bytes();
        chunk.truncate(CHUNK_SIZE_64K);
        initial_data.extend_from_slice(&chunk);
    }
    assert_eq!(initial_data.len(), total_size);

    dm.write_data_with_filters(
        file_id,
        0,
        &initial_data,
        CompressionMode::Seekable {
            chunk_size: CHUNK_SIZE_64K as u32,
            level: 3,
        },
        FilterConfig::none(),
    )
    .expect("write seekable");

    let inode = dm.read_inode(file_id).expect("read inode");
    assert_eq!(
        inode.flags & INODE_FLAG_SEEKABLE_64K,
        INODE_FLAG_SEEKABLE_64K
    );

    let num_threads = 16;
    let ops_per_thread = 50;
    let barrier = Arc::new(Barrier::new(num_threads));
    let initial_data_arc = Arc::new(initial_data);
    let mut handles = Vec::new();

    for thread_idx in 0..num_threads {
        let dm_clone = Arc::clone(&dm);
        let b = Arc::clone(&barrier);
        let data_ref = Arc::clone(&initial_data_arc);

        let handle = thread::spawn(move || {
            b.wait();

            for op in 0..ops_per_thread {
                // Test varied offsets: aligned, unaligned, cross-chunk boundaries
                let offset = match (thread_idx + op) % 5 {
                    0 => (op % num_chunks) * CHUNK_SIZE_64K, // chunk boundary
                    1 => (op % num_chunks) * CHUNK_SIZE_64K + 1024, // inside chunk
                    2 => (op % (num_chunks - 1)) * CHUNK_SIZE_64K + 65500, // straddling chunk boundary
                    3 => 0,                                                // beginning
                    _ => total_size - 512,                                 // near end
                } as u64;

                let len = match (thread_idx + op) % 4 {
                    0 => 512,
                    1 => 4096,
                    2 => 65536,
                    _ => 128,
                };

                let mut buf = vec![0u8; len];
                let bytes_read = dm_clone
                    .read_at(file_id, offset, &mut buf)
                    .expect("read_at");

                let expected_avail = (total_size as u64).saturating_sub(offset) as usize;
                let expected_len = len.min(expected_avail);
                assert_eq!(bytes_read, expected_len);

                let expected_slice = &data_ref[offset as usize..offset as usize + expected_len];
                assert_eq!(
                    &buf[..bytes_read],
                    expected_slice,
                    "Thread {} op {} mismatch at offset {} len {}",
                    thread_idx,
                    op,
                    offset,
                    len
                );
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().expect("thread join");
    }

    // Verify fsck
    let fsck = dm.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "FSCK should be clean: {:?}", fsck);
}

// =========================================================================
// 2. Multithread: Parallel Partitioned Chunk Writes to Single File
// =========================================================================

#[test]
fn test_multithread_concurrent_partitioned_chunk_writes() {
    let ctx = TempImageContext::new("test_mt_chunk_writes");
    let dm = Arc::new(DiskManager::open(&ctx.path, 30 * 1024 * 1024).expect("open"));
    let root = dm.superblock().root_inode;
    let file_id = dm.create_file(root, "partitioned.dat").expect("create");

    let num_chunks = 16;
    let total_size = num_chunks * CHUNK_SIZE_64K;

    // Initialize file as 16 empty seekable chunks
    let empty_payload = vec![0u8; total_size];
    dm.write_data_with_filters(
        file_id,
        0,
        &empty_payload,
        CompressionMode::Seekable {
            chunk_size: CHUNK_SIZE_64K as u32,
            level: 1,
        },
        FilterConfig::none(),
    )
    .expect("init write");

    let barrier = Arc::new(Barrier::new(num_chunks));
    let mut handles = Vec::new();

    // 16 threads, each writes exactly its dedicated chunk
    for chunk_idx in 0..num_chunks {
        let dm_clone = Arc::clone(&dm);
        let b = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            let offset = (chunk_idx * CHUNK_SIZE_64K) as u64;
            let tag = format!(
                "WORKER_{:02}_CHUNK_REPETITIVE_CONTENT_VERIFICATION",
                chunk_idx
            );
            let mut payload = tag.repeat(CHUNK_SIZE_64K / tag.len() + 1).into_bytes();
            payload.truncate(CHUNK_SIZE_64K);

            b.wait();

            dm_clone
                .write_data_with_filters(
                    file_id,
                    offset,
                    &payload,
                    CompressionMode::Seekable {
                        chunk_size: CHUNK_SIZE_64K as u32,
                        level: 1,
                    },
                    FilterConfig::none(),
                )
                .expect("write partitioned chunk");
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().expect("thread join");
    }

    // Verify all chunks in the file
    let full_data = dm.read_data(file_id).expect("read full data");
    assert_eq!(full_data.len(), total_size);

    for chunk_idx in 0..num_chunks {
        let offset = chunk_idx * CHUNK_SIZE_64K;
        let chunk_slice = &full_data[offset..offset + CHUNK_SIZE_64K];

        let tag = format!(
            "WORKER_{:02}_CHUNK_REPETITIVE_CONTENT_VERIFICATION",
            chunk_idx
        );
        let mut expected = tag.repeat(CHUNK_SIZE_64K / tag.len() + 1).into_bytes();
        expected.truncate(CHUNK_SIZE_64K);

        assert_eq!(
            chunk_slice,
            expected.as_slice(),
            "Chunk {} data corrupted or overwritten by other threads",
            chunk_idx
        );
    }

    let fsck = dm.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "FSCK should be clean: {:?}", fsck);
}

// =========================================================================
// 3. Multithread: Concurrent Readers vs Continuous Writer Stress Race
// =========================================================================

#[test]
fn test_multithread_concurrent_readers_and_writers_race() {
    let ctx = TempImageContext::new("test_mt_rw_race");
    let dm = Arc::new(DiskManager::open(&ctx.path, 30 * 1024 * 1024).expect("open"));
    let root = dm.superblock().root_inode;
    let file_id = dm.create_file(root, "race_target.dat").expect("create");

    let num_chunks = 8;
    let total_size = num_chunks * CHUNK_SIZE_64K;
    let initial_zeros = vec![0u8; total_size];

    dm.write_data_with_filters(
        file_id,
        0,
        &initial_zeros,
        CompressionMode::Seekable {
            chunk_size: CHUNK_SIZE_64K as u32,
            level: 1,
        },
        FilterConfig::none(),
    )
    .expect("init write");

    let running = Arc::new(AtomicBool::new(true));
    let read_count = Arc::new(AtomicUsize::new(0));

    // Spawn 6 reader threads continuously reading random chunk slices
    let mut reader_handles = Vec::new();
    for r_id in 0..6 {
        let dm_clone = Arc::clone(&dm);
        let running_clone = Arc::clone(&running);
        let read_counter = Arc::clone(&read_count);

        let h = thread::spawn(move || {
            let mut iter = 0;
            while running_clone.load(Ordering::Relaxed) {
                let chunk_idx = (r_id + iter) % num_chunks;
                let offset = (chunk_idx * CHUNK_SIZE_64K) as u64;
                let mut buf = vec![0u8; 1024];

                let bytes = dm_clone
                    .read_at(file_id, offset, &mut buf)
                    .expect("read_at");
                assert_eq!(bytes, 1024);

                // Check that data is either zeros or has a valid header structure
                if buf[0] != 0 {
                    let header_str = String::from_utf8_lossy(&buf[..32]);
                    assert!(
                        header_str.starts_with("GEN:"),
                        "Reader saw torn/corrupted chunk: {:?}",
                        header_str
                    );
                }

                read_counter.fetch_add(1, Ordering::Relaxed);
                iter += 1;
                thread::yield_now();
            }
        });
        reader_handles.push(h);
    }

    // Writer thread performs 40 rapid chunk updates
    for generation in 1..=40 {
        let target_chunk = generation % num_chunks;
        let offset = (target_chunk * CHUNK_SIZE_64K) as u64;

        let pattern = format!(
            "GEN:{:04}:CHUNK:{:02}:DATA_BLOCK_CONTENT_PATTERN_PAD",
            generation, target_chunk
        );
        let mut payload = pattern
            .repeat(CHUNK_SIZE_64K / pattern.len() + 1)
            .into_bytes();
        payload.truncate(CHUNK_SIZE_64K);

        dm.write_data_with_filters(
            file_id,
            offset,
            &payload,
            CompressionMode::Seekable {
                chunk_size: CHUNK_SIZE_64K as u32,
                level: 1,
            },
            FilterConfig::none(),
        )
        .expect("writer update");

        thread::sleep(Duration::from_millis(1));
    }

    running.store(false, Ordering::Relaxed);
    for h in reader_handles {
        h.join().expect("reader join");
    }

    assert!(
        read_count.load(Ordering::Relaxed) > 100,
        "Readers should have completed many iterations concurrently"
    );

    let fsck = dm.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "FSCK should be clean: {:?}", fsck);
}

// =========================================================================
// 4. Multithread: Truncate vs read_at Boundary Race
// =========================================================================

#[test]
fn test_multithread_concurrent_truncate_and_read_at() {
    let ctx = TempImageContext::new("test_mt_truncate_race");
    let dm = Arc::new(DiskManager::open(&ctx.path, 30 * 1024 * 1024).expect("open"));
    let root = dm.superblock().root_inode;
    let file_id = dm.create_file(root, "dynamic_size.dat").expect("create");

    // Initialize with 4 chunks
    let initial_data = vec![0x55u8; 4 * CHUNK_SIZE_64K];
    dm.write_data_with_filters(
        file_id,
        0,
        &initial_data,
        CompressionMode::Seekable {
            chunk_size: CHUNK_SIZE_64K as u32,
            level: 1,
        },
        FilterConfig::none(),
    )
    .expect("init write");

    let running = Arc::new(AtomicBool::new(true));

    // 4 reader threads continuously read across variable offsets
    let mut reader_handles = Vec::new();
    for r_idx in 0..4 {
        let dm_clone = Arc::clone(&dm);
        let running_clone = Arc::clone(&running);

        let h = thread::spawn(move || {
            let mut iter = 0;
            while running_clone.load(Ordering::Relaxed) {
                let offset = ((r_idx + iter) % 8 * 32 * 1024) as u64;
                let mut buf = vec![0u8; 4096];
                // Must not panic or segfault regardless of concurrent size mutations
                let _ = dm_clone.read_at(file_id, offset, &mut buf);
                iter += 1;
                thread::yield_now();
            }
        });
        reader_handles.push(h);
    }

    // Mutator thread alternates shrinking and expanding
    let sizes = [
        CHUNK_SIZE_64K as u64,
        3 * CHUNK_SIZE_64K as u64 + 128,
        6 * CHUNK_SIZE_64K as u64,
        2 * CHUNK_SIZE_64K as u64,
        5 * CHUNK_SIZE_64K as u64,
        1024,
        4 * CHUNK_SIZE_64K as u64,
    ];

    for &new_size in &sizes {
        dm.truncate(file_id, new_size).expect("truncate");
        thread::sleep(Duration::from_millis(2));
    }

    running.store(false, Ordering::Relaxed);
    for h in reader_handles {
        h.join().expect("reader join");
    }

    let fsck = dm.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "FSCK should be clean: {:?}", fsck);
}

// =========================================================================
// 5. Multiprocess: Real Process CLI Concurrency with Seekable Chunked Files
// =========================================================================

#[test]
fn test_multiprocess_cli_chunked_concurrency() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("test_mp_cli_chunked.img");
    let img_str = img_path.to_str().unwrap().to_string();
    let bin_path = env!("CARGO_BIN_EXE_oifs");

    // 1. Create image via CLI (30MB)
    let status = Command::new(bin_path)
        .args(["--image", &img_str, "create", "--size", "30"])
        .status()
        .expect("CLI create failed");
    assert!(status.success());

    // 2. Prepare 4 distinct files (128KB - 256KB each, multiple chunks)
    let num_procs = 4;
    for i in 0..num_procs {
        let payload_path = dir.path().join(format!("host_chunked_{}.dat", i));
        let chunk_count = 2 + i; // 2, 3, 4, 5 chunks
        let total_bytes = chunk_count * CHUNK_SIZE_64K;

        let tag = format!("PROCESS_{}_COMPRESSIBLE_CHUNK_DATA_PAYLOAD_", i);
        let mut data = tag.repeat(total_bytes / tag.len() + 1).into_bytes();
        data.truncate(total_bytes);
        fs::write(&payload_path, &data).unwrap();
    }

    // 3. Concurrently launch 4 independent OS processes running `oifs put --chunked`
    let mut put_handles = Vec::new();
    for i in 0..num_procs {
        let bin = bin_path.to_string();
        let img = img_str.clone();
        let payload_path = dir.path().join(format!("host_chunked_{}.dat", i));

        let h = thread::spawn(move || {
            let remote = format!("remote_chunked_{}.dat", i);
            let output = Command::new(bin)
                .args([
                    "--image",
                    &img,
                    "put",
                    "--chunked",
                    payload_path.to_str().unwrap(),
                    &remote,
                ])
                .output()
                .expect("CLI put failed");

            assert!(
                output.status.success(),
                "Process {} put --chunked failed: {}",
                i,
                String::from_utf8_lossy(&output.stderr)
            );
        });
        put_handles.push(h);
    }

    for h in put_handles {
        h.join().unwrap();
    }

    // 4. Verify directory listing via CLI
    let ls_output = Command::new(bin_path)
        .args(["--image", &img_str, "ls"])
        .output()
        .expect("CLI ls failed");
    assert!(ls_output.status.success());
    let ls_stdout = String::from_utf8_lossy(&ls_output.stdout);

    for i in 0..num_procs {
        assert!(
            ls_stdout.contains(&format!("remote_chunked_{}.dat", i)),
            "Missing remote_chunked_{}.dat in ls:\n{}",
            i,
            ls_stdout
        );
    }

    // 5. Concurrently launch 4 independent OS processes running `oifs get`
    let mut get_handles = Vec::new();
    for i in 0..num_procs {
        let bin = bin_path.to_string();
        let img = img_str.clone();
        let downloaded_path = dir.path().join(format!("downloaded_{}.dat", i));

        let h = thread::spawn(move || {
            let remote = format!("remote_chunked_{}.dat", i);
            let output = Command::new(bin)
                .args([
                    "--image",
                    &img,
                    "get",
                    &remote,
                    downloaded_path.to_str().unwrap(),
                ])
                .output()
                .expect("CLI get failed");

            assert!(
                output.status.success(),
                "Process {} get failed: {}",
                i,
                String::from_utf8_lossy(&output.stderr)
            );
        });
        get_handles.push(h);
    }

    for h in get_handles {
        h.join().unwrap();
    }

    // 6. Verify downloaded contents byte-for-byte against original host files
    for i in 0..num_procs {
        let host_orig = fs::read(dir.path().join(format!("host_chunked_{}.dat", i))).unwrap();
        let downloaded = fs::read(dir.path().join(format!("downloaded_{}.dat", i))).unwrap();
        assert_eq!(
            host_orig, downloaded,
            "Downloaded file {} did not match original",
            i
        );
    }

    // 7. Verify fsck via CLI
    let fsck_output = Command::new(bin_path)
        .args(["--image", &img_str, "fsck"])
        .output()
        .expect("CLI fsck failed");
    assert!(
        fsck_output.status.success(),
        "CLI fsck failed: {}",
        String::from_utf8_lossy(&fsck_output.stderr)
    );
}

// =========================================================================
// 6. Multiprocess: Remote IPC Proxy Concurrent Sliced read_at & Writes
// =========================================================================

#[test]
fn test_multiprocess_ipc_chunked_read_at_and_writes() {
    let ctx = TempImageContext::new("test_mp_ipc_chunked");
    let num_chunks = 8;
    let total_size = num_chunks * CHUNK_SIZE_64K;

    // Master opens session
    let master = OifsSession::open(&ctx.path, 30 * 1024 * 1024).expect("master open");
    assert!(master.is_direct(), "Master must be Direct mode");

    let root = master.resolve_path(".").expect("resolve root");
    let file_id = master
        .create_file(root, "ipc_shared_chunked.dat")
        .expect("create file");

    let mut initial_data = Vec::with_capacity(total_size);
    for c in 0..num_chunks {
        let tag = format!("IPC_CHUNK_{:02}_INITIAL_PAYLOAD_STRING_PATTERN", c);
        let mut chunk = tag.repeat(CHUNK_SIZE_64K / tag.len() + 1).into_bytes();
        chunk.truncate(CHUNK_SIZE_64K);
        initial_data.extend_from_slice(&chunk);
    }

    master
        .write_data_with_filters(
            file_id,
            0,
            &initial_data,
            CompressionMode::Seekable {
                chunk_size: CHUNK_SIZE_64K as u32,
                level: 2,
            },
            FilterConfig::none(),
        )
        .expect("master write");

    // Spawn 4 concurrent client threads connecting via Remote IPC proxy
    let num_clients = 4;
    let barrier = Arc::new(Barrier::new(num_clients));
    let path_clone = ctx.path.clone();
    let initial_arc = Arc::new(initial_data);
    let mut handles = Vec::new();

    for client_idx in 0..num_clients {
        let p = path_clone.clone();
        let b = Arc::clone(&barrier);
        let data_ref = Arc::clone(&initial_arc);

        let h = thread::spawn(move || {
            let client = OifsSession::open(&p, 0).expect("client open");
            assert!(!client.is_direct(), "Client must be Remote mode");

            b.wait();

            // 1. Sliced read_at over IPC
            let target_chunk = client_idx * 2;
            let offset = (target_chunk * CHUNK_SIZE_64K + 500) as u64;
            let mut buf = vec![0u8; 2048];
            let read_n = client
                .read_at(file_id, offset, &mut buf)
                .expect("read_at via IPC");
            assert_eq!(read_n, 2048);

            let expected_slice = &data_ref[offset as usize..offset as usize + 2048];
            assert_eq!(&buf[..read_n], expected_slice);

            // 2. Client overwrites a dedicated chunk over IPC
            let my_chunk = client_idx * 2 + 1;
            let my_offset = (my_chunk * CHUNK_SIZE_64K) as u64;
            let updated_tag = format!("UPDATED_BY_CLIENT_{}_OVER_IPC_CHANNEL", client_idx);
            let mut updated_payload = updated_tag
                .repeat(CHUNK_SIZE_64K / updated_tag.len() + 1)
                .into_bytes();
            updated_payload.truncate(CHUNK_SIZE_64K);

            client
                .write_data_with_filters(
                    file_id,
                    my_offset,
                    &updated_payload,
                    CompressionMode::Seekable {
                        chunk_size: CHUNK_SIZE_64K as u32,
                        level: 2,
                    },
                    FilterConfig::none(),
                )
                .expect("write over IPC");
        });
        handles.push(h);
    }

    for h in handles {
        h.join().expect("client thread join");
    }

    // Master verifies updated chunks
    let final_data = master.read_data(file_id).expect("master read");
    assert_eq!(final_data.len(), total_size);

    for client_idx in 0..num_clients {
        let my_chunk = client_idx * 2 + 1;
        let my_offset = my_chunk * CHUNK_SIZE_64K;
        let chunk_slice = &final_data[my_offset..my_offset + CHUNK_SIZE_64K];

        let updated_tag = format!("UPDATED_BY_CLIENT_{}_OVER_IPC_CHANNEL", client_idx);
        let mut expected = updated_tag
            .repeat(CHUNK_SIZE_64K / updated_tag.len() + 1)
            .into_bytes();
        expected.truncate(CHUNK_SIZE_64K);

        assert_eq!(chunk_slice, expected.as_slice());
    }

    let fsck = master.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "FSCK should be clean: {:?}", fsck);
}

// =========================================================================
// 7. Multiprocess: Network Mode Multi-Node Concurrent Partitioned Writes
// =========================================================================

#[test]
fn test_multiprocess_network_single_file_chunked_writes() {
    let ctx = TempImageContext::new("test_mp_net_chunked");
    let num_nodes = 4;
    let total_size = num_nodes * CHUNK_SIZE_64K;

    // Node 0 (Master) initializes session over TCP Network Mode
    let master =
        OifsSession::open_network(&ctx.path, 30 * 1024 * 1024, Some("127.0.0.1:0".to_string()))
            .expect("open network master");

    let root = master.resolve_path(".").expect("resolve root");
    let file_id = master
        .create_file(root, "network_matrix.dat")
        .expect("create file");

    // Initialize with zeroed seekable chunks
    let empty_payload = vec![0u8; total_size];
    master
        .write_data_with_filters(
            file_id,
            0,
            &empty_payload,
            CompressionMode::Seekable {
                chunk_size: CHUNK_SIZE_64K as u32,
                level: 1,
            },
            FilterConfig::none(),
        )
        .expect("master init write");

    let barrier = Arc::new(Barrier::new(num_nodes));
    let path_clone = ctx.path.clone();
    let mut handles = Vec::new();

    // 4 concurrent nodes connecting via network mode
    for node_idx in 0..num_nodes {
        let p = path_clone.clone();
        let b = Arc::clone(&barrier);

        let h = thread::spawn(move || {
            let session = OifsSession::open_network(&p, 0, None).expect("open network client");

            let offset = (node_idx * CHUNK_SIZE_64K) as u64;
            let tag = format!("NETWORK_NODE_{:02}_CHUNKED_DATA_TRANSMISSION", node_idx);
            let mut payload = tag.repeat(CHUNK_SIZE_64K / tag.len() + 1).into_bytes();
            payload.truncate(CHUNK_SIZE_64K);

            b.wait();

            session
                .write_data_with_filters(
                    file_id,
                    offset,
                    &payload,
                    CompressionMode::Seekable {
                        chunk_size: CHUNK_SIZE_64K as u32,
                        level: 1,
                    },
                    FilterConfig::none(),
                )
                .expect("network write chunk");

            // Also test sliced read_at over network socket
            let mut verify_buf = vec![0u8; 128];
            let n = session
                .read_at(file_id, offset, &mut verify_buf)
                .expect("network read_at");
            assert_eq!(n, 128);
            assert_eq!(&verify_buf[..n], &payload[..128]);
        });
        handles.push(h);
    }

    for h in handles {
        h.join().expect("node join");
    }

    // Master verifies full matrix
    let full_data = master.read_data(file_id).expect("read full data");
    assert_eq!(full_data.len(), total_size);

    for node_idx in 0..num_nodes {
        let offset = node_idx * CHUNK_SIZE_64K;
        let chunk_slice = &full_data[offset..offset + CHUNK_SIZE_64K];

        let tag = format!("NETWORK_NODE_{:02}_CHUNKED_DATA_TRANSMISSION", node_idx);
        let mut expected = tag.repeat(CHUNK_SIZE_64K / tag.len() + 1).into_bytes();
        expected.truncate(CHUNK_SIZE_64K);

        assert_eq!(chunk_slice, expected.as_slice());
    }

    let fsck = master.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "FSCK should be clean: {:?}", fsck);
}
