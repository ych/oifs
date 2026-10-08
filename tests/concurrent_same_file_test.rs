use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use oifs::disk::{CompressionMode, DiskManager, DiskManagerError};
use oifs::filters::FilterConfig;
use oifs::inode::CHUNK_SIZE_64K;
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
// 1. Exact Same Filename Creation Race: Mutual Exclusion Verification
// =========================================================================

#[test]
fn test_concurrent_create_exact_same_filename_mutual_exclusion() {
    let ctx = TempImageContext::new("test_same_file_mutex");
    let dm = Arc::new(DiskManager::open(&ctx.path, 20 * 1024 * 1024).expect("open"));
    let root = dm.superblock().root_inode;

    let num_threads = 16;
    let barrier = Arc::new(Barrier::new(num_threads));
    let successes = Arc::new(AtomicUsize::new(0));
    let already_exists_errors = Arc::new(AtomicUsize::new(0));
    let created_inode_id = Arc::new(Mutex::new(None));

    let mut handles = Vec::new();

    for _thread_idx in 0..num_threads {
        let dm_clone = Arc::clone(&dm);
        let b = Arc::clone(&barrier);
        let s_count = Arc::clone(&successes);
        let e_count = Arc::clone(&already_exists_errors);
        let created_id_slot = Arc::clone(&created_inode_id);

        let h = thread::spawn(move || {
            // Align all threads to hit create_file simultaneously
            b.wait();

            match dm_clone.create_file(root, "race_same_name.txt") {
                Ok(inode_id) => {
                    s_count.fetch_add(1, Ordering::SeqCst);
                    let mut lock = created_id_slot.lock().unwrap();
                    *lock = Some(inode_id);
                }
                Err(DiskManagerError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    e_count.fetch_add(1, Ordering::SeqCst);
                }
                Err(other) => {
                    panic!("Unexpected error from create_file: {:?}", other);
                }
            }
        });
        handles.push(h);
    }

    for h in handles {
        h.join().unwrap();
    }

    // Exactly 1 thread must have succeeded
    assert_eq!(
        successes.load(Ordering::SeqCst),
        1,
        "Exactly one thread must succeed in creating the file"
    );

    // Exactly 15 threads must have received AlreadyExists
    assert_eq!(
        already_exists_errors.load(Ordering::SeqCst),
        num_threads - 1,
        "All other threads must receive AlreadyExists error"
    );

    let winner_inode_id = created_inode_id.lock().unwrap().unwrap();

    // Verify lookup resolves to the single created file
    let lookup_id = dm
        .lookup(root, "race_same_name.txt")
        .expect("lookup winner");
    assert_eq!(lookup_id, winner_inode_id);

    // Verify directory entries: exactly 1 entry for this name
    let entries = dm.list_dir(root).expect("list_dir");
    let matching_entries: Vec<_> = entries
        .into_iter()
        .filter(|e| e.name == "race_same_name.txt")
        .collect();
    assert_eq!(
        matching_entries.len(),
        1,
        "Directory must contain exactly 1 entry for race_same_name.txt"
    );

    // Verify fsck: 0 leaked inodes, 0 leaked blocks
    let fsck = dm.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "FSCK must be completely clean: {:?}", fsck);
}

// =========================================================================
// 2. Concurrent create_or_open_file: All Threads Safely Open the Same File
// =========================================================================

#[test]
fn test_concurrent_create_or_open_same_filename_and_sliced_writes() {
    let ctx = TempImageContext::new("test_same_file_create_or_open");
    let dm = Arc::new(DiskManager::open(&ctx.path, 20 * 1024 * 1024).expect("open"));
    let root = dm.superblock().root_inode;

    let num_threads = 16;
    let slice_size = 4096;
    let barrier = Arc::new(Barrier::new(num_threads));
    let mut handles = Vec::new();

    for thread_idx in 0..num_threads {
        let dm_clone = Arc::clone(&dm);
        let b = Arc::clone(&barrier);

        let h = thread::spawn(move || {
            b.wait();

            // All threads simultaneously open/create the same file
            let inode_id = dm_clone
                .create_or_open_file(root, "shared_document.dat")
                .expect("create_or_open_file");

            // Each thread writes its dedicated slice
            let offset = (thread_idx * slice_size) as u64;
            let tag = (thread_idx as u8 + 1) * 11;
            let payload = vec![tag; slice_size];

            dm_clone
                .write_data_with_filters(
                    inode_id,
                    offset,
                    &payload,
                    CompressionMode::Never,
                    FilterConfig::none(),
                )
                .expect("write slice");

            (thread_idx, inode_id, tag)
        });
        handles.push(h);
    }

    let mut first_inode_id = None;
    let mut thread_tags = Vec::new();

    for h in handles {
        let (t_idx, inode_id, tag) = h.join().unwrap();
        if let Some(expected_id) = first_inode_id {
            assert_eq!(
                inode_id, expected_id,
                "All threads must receive the identical Inode ID"
            );
        } else {
            first_inode_id = Some(inode_id);
        }
        thread_tags.push((t_idx, tag));
    }

    let target_inode_id = first_inode_id.unwrap();

    // Verify whole file content
    let full_data = dm.read_data(target_inode_id).expect("read full data");
    assert_eq!(full_data.len(), num_threads * slice_size);

    for (t_idx, expected_tag) in thread_tags {
        let start = t_idx * slice_size;
        let end = start + slice_size;
        let slice = &full_data[start..end];
        assert!(
            slice.iter().all(|&b| b == expected_tag),
            "Thread {} slice {}..{} corrupted",
            t_idx,
            start,
            end
        );
    }

    let fsck = dm.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "FSCK must be clean: {:?}", fsck);
}

// =========================================================================
// 3. Concurrent create_or_open with Seekable 64KB Chunked Compression
// =========================================================================

#[test]
fn test_concurrent_create_or_open_same_filename_with_seekable_compression() {
    let ctx = TempImageContext::new("test_same_file_seekable");
    let dm = Arc::new(DiskManager::open(&ctx.path, 30 * 1024 * 1024).expect("open"));
    let root = dm.superblock().root_inode;

    let num_threads = 8;
    let barrier = Arc::new(Barrier::new(num_threads));
    let mut handles = Vec::new();

    for thread_idx in 0..num_threads {
        let dm_clone = Arc::clone(&dm);
        let b = Arc::clone(&barrier);

        let h = thread::spawn(move || {
            b.wait();

            // Simultaneously create or open the same seekable compressed file
            let inode_id = dm_clone
                .create_or_open_file(root, "shared_matrix_64k.dat")
                .expect("create_or_open seekable");

            // Write dedicated 64KB chunk
            let offset = (thread_idx * CHUNK_SIZE_64K) as u64;
            let tag = format!("THREAD_{:02}_SEEKABLE_CHUNK_DATA_STREAM_", thread_idx);
            let mut payload = tag.repeat(CHUNK_SIZE_64K / tag.len() + 1).into_bytes();
            payload.truncate(CHUNK_SIZE_64K);

            dm_clone
                .write_data_with_filters(
                    inode_id,
                    offset,
                    &payload,
                    CompressionMode::Seekable {
                        chunk_size: CHUNK_SIZE_64K as u32,
                        level: 1,
                    },
                    FilterConfig::none(),
                )
                .expect("write seekable chunk");

            (thread_idx, inode_id)
        });
        handles.push(h);
    }

    let mut common_inode = None;
    for h in handles {
        let (_t_idx, inode_id) = h.join().unwrap();
        if let Some(expected) = common_inode {
            assert_eq!(inode_id, expected);
        } else {
            common_inode = Some(inode_id);
        }
    }

    let file_id = common_inode.unwrap();
    let full_data = dm.read_data(file_id).expect("read full data");
    assert_eq!(full_data.len(), num_threads * CHUNK_SIZE_64K);

    // Verify all 8 chunks
    for thread_idx in 0..num_threads {
        let start = thread_idx * CHUNK_SIZE_64K;
        let end = start + CHUNK_SIZE_64K;
        let slice = &full_data[start..end];

        let tag = format!("THREAD_{:02}_SEEKABLE_CHUNK_DATA_STREAM_", thread_idx);
        let mut expected = tag.repeat(CHUNK_SIZE_64K / tag.len() + 1).into_bytes();
        expected.truncate(CHUNK_SIZE_64K);

        assert_eq!(slice, expected.as_slice());
    }

    let fsck = dm.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "FSCK must be clean: {:?}", fsck);
}

// =========================================================================
// 4. Nested Subdirectory Same Filename Isolation
// =========================================================================

#[test]
fn test_concurrent_create_same_filename_in_nested_subdirectories() {
    let ctx = TempImageContext::new("test_same_file_nested");
    let dm = Arc::new(DiskManager::open(&ctx.path, 20 * 1024 * 1024).expect("open"));
    let root = dm.superblock().root_inode;

    // Create 4 subdirectories
    let mut dir_ids = Vec::new();
    for d in 0..4 {
        let dir_id = dm.create_directory(root, &format!("dir_{}", d)).unwrap();
        dir_ids.push(dir_id);
    }

    let threads_per_dir = 4;
    let total_threads = dir_ids.len() * threads_per_dir;
    let barrier = Arc::new(Barrier::new(total_threads));
    let mut handles = Vec::new();

    // In each directory, 4 threads race to create "common.txt"
    for (dir_idx, &target_dir) in dir_ids.iter().enumerate() {
        for _t in 0..threads_per_dir {
            let dm_clone = Arc::clone(&dm);
            let b = Arc::clone(&barrier);

            let h = thread::spawn(move || {
                b.wait();

                let res = dm_clone.create_file(target_dir, "common.txt");
                let inode_id = match res {
                    Ok(id) => id,
                    Err(DiskManagerError::Io(e))
                        if e.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        dm_clone.lookup(target_dir, "common.txt").unwrap()
                    }
                    Err(e) => panic!("Unexpected error: {:?}", e),
                };
                (dir_idx, inode_id)
            });
            handles.push(h);
        }
    }

    let mut dir_created_inodes = vec![None; dir_ids.len()];
    for h in handles {
        let (dir_idx, inode_id) = h.join().unwrap();
        if let Some(expected) = dir_created_inodes[dir_idx] {
            assert_eq!(inode_id, expected, "Mismatch within directory {}", dir_idx);
        } else {
            dir_created_inodes[dir_idx] = Some(inode_id);
        }
    }

    // Verify all 4 directories have distinct Inode IDs for "common.txt"
    let mut unique_inodes: Vec<u64> = dir_created_inodes.into_iter().map(|o| o.unwrap()).collect();
    let original_len = unique_inodes.len();
    unique_inodes.sort();
    unique_inodes.dedup();
    assert_eq!(
        unique_inodes.len(),
        original_len,
        "Each directory must have a distinct Inode ID for common.txt"
    );

    let fsck = dm.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "FSCK must be clean: {:?}", fsck);
}

// =========================================================================
// 5. OifsSession create_or_open_file Multi-Thread Concurrency
// =========================================================================

#[test]
fn test_session_concurrent_create_or_open_same_filename() {
    let ctx = TempImageContext::new("test_same_file_session");
    let session = Arc::new(OifsSession::open(&ctx.path, 20 * 1024 * 1024).expect("open session"));
    let root = session.resolve_path(".").expect("resolve root");

    let num_threads = 8;
    let barrier = Arc::new(Barrier::new(num_threads));
    let mut handles = Vec::new();

    for t_idx in 0..num_threads {
        let sess_clone = Arc::clone(&session);
        let b = Arc::clone(&barrier);

        let h = thread::spawn(move || {
            b.wait();

            let inode_id = sess_clone
                .create_or_open_file(root, "session_shared.dat")
                .expect("session create_or_open_file");

            let data = format!("CONTENT_FROM_THREAD_{:02}\n", t_idx);
            let offset = (t_idx * data.len()) as u64;
            sess_clone
                .write_data(inode_id, offset, data.as_bytes(), CompressionMode::Never)
                .expect("session write");

            inode_id
        });
        handles.push(h);
    }

    let mut common_id = None;
    for h in handles {
        let id = h.join().unwrap();
        if let Some(expected) = common_id {
            assert_eq!(id, expected);
        } else {
            common_id = Some(id);
        }
    }

    let file_id = common_id.unwrap();
    let read_back = session.read_data(file_id).expect("read data");
    assert!(!read_back.is_empty());

    let fsck = session.verify_integrity().expect("fsck");
    assert!(fsck.is_clean, "FSCK must be clean: {:?}", fsck);
}

// =========================================================================
// 6. OpenMode::CreateNew Strict Mutual Exclusion & EEXIST Guarantee
// =========================================================================

#[test]
fn test_multithread_open_mode_create_new_eexist_guarantee() {
    use oifs::disk::OpenMode;

    let ctx = TempImageContext::new("test_same_file_open_mode_create_new");
    let dm = Arc::new(DiskManager::open(&ctx.path, 20 * 1024 * 1024).expect("open"));
    let root = dm.superblock().root_inode;

    let num_threads = 16;
    let barrier = Arc::new(Barrier::new(num_threads));
    let successes = Arc::new(AtomicUsize::new(0));
    let eexist_count = Arc::new(AtomicUsize::new(0));
    let created_inode_id = Arc::new(Mutex::new(None));
    let mut handles = Vec::new();

    // 16 threads concurrently request OpenMode::CreateNew on a non-existent filename
    for _ in 0..num_threads {
        let dm_clone = Arc::clone(&dm);
        let b = Arc::clone(&barrier);
        let s_count = Arc::clone(&successes);
        let e_count = Arc::clone(&eexist_count);
        let created_id_slot = Arc::clone(&created_inode_id);

        let h = thread::spawn(move || {
            b.wait();

            match dm_clone.open_file(root, "exclusive_posix.dat", OpenMode::CreateNew) {
                Ok(inode_id) => {
                    s_count.fetch_add(1, Ordering::SeqCst);
                    let mut lock = created_id_slot.lock().unwrap();
                    *lock = Some(inode_id);
                }
                Err(DiskManagerError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    e_count.fetch_add(1, Ordering::SeqCst);
                }
                Err(other) => {
                    panic!("Unexpected error from open_file CreateNew: {:?}", other);
                }
            }
        });
        handles.push(h);
    }

    for h in handles {
        h.join().unwrap();
    }

    // Must guarantee: exactly 1 thread succeeded, all other 15 threads received AlreadyExists (EEXIST)
    assert_eq!(
        successes.load(Ordering::SeqCst),
        1,
        "Exactly one thread must succeed in OpenMode::CreateNew"
    );
    assert_eq!(
        eexist_count.load(Ordering::SeqCst),
        num_threads - 1,
        "All other threads must fail with AlreadyExists (EEXIST)"
    );

    let winner_id = created_inode_id.lock().unwrap().unwrap();
    let lookup_id = dm
        .open_file(root, "exclusive_posix.dat", OpenMode::OpenExisting)
        .expect("open existing");
    assert_eq!(lookup_id, winner_id);

    let fsck = dm.verify_integrity().expect("fsck");
    assert!(fsck.is_clean);
}

// =========================================================================
// 7. OpenMode::OpenExisting Fails with ENOENT When Absent, Succeeds When Present
// =========================================================================

#[test]
fn test_multithread_open_mode_open_existing_enoent_before_creation() {
    use oifs::disk::OpenMode;

    let ctx = TempImageContext::new("test_same_file_open_existing");
    let dm = Arc::new(DiskManager::open(&ctx.path, 20 * 1024 * 1024).expect("open"));
    let root = dm.superblock().root_inode;

    let num_threads = 8;
    let barrier = Arc::new(Barrier::new(num_threads));
    let enoent_count = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    // 1. When file does not exist, all threads requesting OpenExisting must get NotFound (ENOENT)
    for _ in 0..num_threads {
        let dm_clone = Arc::clone(&dm);
        let b = Arc::clone(&barrier);
        let not_found_count = Arc::clone(&enoent_count);

        let h = thread::spawn(move || {
            b.wait();

            match dm_clone.open_file(root, "must_exist.dat", OpenMode::OpenExisting) {
                Ok(_) => panic!("File should not exist yet"),
                Err(DiskManagerError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    not_found_count.fetch_add(1, Ordering::SeqCst);
                }
                Err(other) => panic!("Unexpected error: {:?}", other),
            }
        });
        handles.push(h);
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        enoent_count.load(Ordering::SeqCst),
        num_threads,
        "All threads must get NotFound when file does not exist"
    );

    // 2. File is created
    let created_id = dm
        .open_file(root, "must_exist.dat", OpenMode::CreateNew)
        .expect("create");

    // 3. Now all threads requesting OpenExisting succeed
    let mut open_handles = Vec::new();
    let barrier2 = Arc::new(Barrier::new(num_threads));

    for _ in 0..num_threads {
        let dm_clone = Arc::clone(&dm);
        let b = Arc::clone(&barrier2);

        let h = thread::spawn(move || {
            b.wait();
            dm_clone
                .open_file(root, "must_exist.dat", OpenMode::OpenExisting)
                .expect("open existing")
        });
        open_handles.push(h);
    }

    for h in open_handles {
        let id = h.join().unwrap();
        assert_eq!(id, created_id);
    }
}
