use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use oifs::OifsSession;
use oifs::disk::CompressionMode;

#[test]
fn test_get_or_open_concurrent_threads_same_path() {
    let image_path = "test_get_open_threads.img";
    OifsSession::unregister_from_registry(image_path);
    if Path::new(image_path).exists() {
        let _ = fs::remove_file(image_path);
    }

    let total_size = 10 * 1024 * 1024; // 10MB
    let num_threads = 8;
    let mut handles = vec![];
    let ready_counter = Arc::new(AtomicUsize::new(0));

    for i in 0..num_threads {
        let ready = ready_counter.clone();
        let handle = thread::spawn(move || {
            ready.fetch_add(1, Ordering::SeqCst);
            // Wait for all threads to align
            while ready.load(Ordering::SeqCst) < num_threads {
                thread::sleep(Duration::from_millis(5));
            }

            // All threads concurrently invoke get_or_open
            let session =
                OifsSession::get_or_open(image_path, total_size).expect("get_or_open failed");

            // Verify all threads get Direct (Master) mode with zero IPC overhead
            assert!(session.is_direct(), "Session should be Direct mode");

            // Write a thread-specific file
            let root_id = session.resolve_path(".").expect("resolve root");
            let file_name = format!("thread_data_{}.bin", i);
            let inode_id = session
                .create_file(root_id, &file_name)
                .expect("create file");

            let data: Vec<u8> = (0..2048).map(|x| ((x + i) % 256) as u8).collect();
            session
                .write_data(inode_id, 0, &data, CompressionMode::Auto)
                .expect("write data");

            (file_name, data)
        });
        handles.push(handle);
    }

    let mut written_files = vec![];
    for handle in handles {
        written_files.push(handle.join().unwrap());
    }

    // Verify all files from an independent get_or_open handle
    let verify_session =
        OifsSession::get_or_open(image_path, total_size).expect("verify session get_or_open");
    let root_id = verify_session.resolve_path(".").expect("resolve root");

    for (file_name, expected_data) in written_files {
        let inode_id = verify_session
            .lookup(root_id, &file_name)
            .expect("file should exist");
        let actual_data = verify_session.read_data(inode_id).expect("read data");
        assert_eq!(
            actual_data, expected_data,
            "Data mismatch for {}",
            file_name
        );
    }

    // Clean up
    OifsSession::unregister_from_registry(image_path);
    let _ = fs::remove_file(image_path);
}

#[test]
fn test_get_or_open_concurrent_with_symlinks() {
    let real_image = "test_symlink_target.img";
    let link1 = "test_symlink_1.img";
    let link2 = "test_symlink_2.img";
    let link3 = "test_symlink_3.img";

    for p in &[real_image, link1, link2, link3] {
        OifsSession::unregister_from_registry(p);
        let _ = fs::remove_file(p);
    }

    let total_size = 10 * 1024 * 1024;

    // First create the real image via get_or_open
    let _init_session = OifsSession::get_or_open(real_image, total_size).expect("init open");

    // Create 3 symbolic links pointing to the real image
    symlink(real_image, link1).expect("create symlink 1");
    symlink(real_image, link2).expect("create symlink 2");
    symlink(real_image, link3).expect("create symlink 3");

    let paths = vec![
        real_image.to_string(),
        link1.to_string(),
        link2.to_string(),
        link3.to_string(),
        format!("./{}", link1),
        format!("./{}", real_image),
    ];

    let mut handles = vec![];
    for (i, p) in paths.into_iter().enumerate() {
        let handle = thread::spawn(move || {
            // Each thread uses a different path / symlink
            let session = OifsSession::get_or_open(&p, total_size).expect("open through symlink");

            // Since it's in the same process, canonicalize resolves to the same key
            assert!(session.is_direct(), "Should be Direct mode for path: {}", p);

            let root_id = session.resolve_path(".").expect("resolve root");
            let file_name = format!("file_from_thread_{}.txt", i);
            let inode_id = session
                .create_file(root_id, &file_name)
                .expect("create file");

            let content = format!("Written via path {} by thread {}", p, i);
            session
                .write_data(inode_id, 0, content.as_bytes(), CompressionMode::Auto)
                .expect("write content");

            (file_name, content)
        });
        handles.push(handle);
    }

    let mut expected_entries = vec![];
    for handle in handles {
        expected_entries.push(handle.join().unwrap());
    }

    // Now open via any of the symlinks to verify all entries are visible
    let check_session = OifsSession::get_or_open(link2, total_size).expect("check session");
    let root_id = check_session.resolve_path(".").expect("resolve root");

    for (file_name, expected_content) in expected_entries {
        let inode_id = check_session
            .lookup(root_id, &file_name)
            .expect("lookup file");
        let data = check_session.read_data(inode_id).expect("read content");
        assert_eq!(String::from_utf8(data).unwrap(), expected_content);
    }

    // Clean up
    OifsSession::unregister_from_registry(real_image);
    for p in &[real_image, link1, link2, link3] {
        let _ = fs::remove_file(p);
    }
}

#[test]
fn test_get_or_open_encrypted_and_registry_lifecycle() {
    let enc_image = "test_registry_enc.img";
    OifsSession::unregister_from_registry(enc_image);
    if Path::new(enc_image).exists() {
        let _ = fs::remove_file(enc_image);
    }

    let total_size = 5 * 1024 * 1024;
    let password = "SecretPassword123!";

    // Create encrypted session
    let session1 = OifsSession::get_or_create_encrypted(enc_image, total_size, password)
        .expect("create encrypted");
    let root1 = session1.resolve_path(".").expect("resolve root");
    let file1 = session1
        .create_file(root1, "secret.txt")
        .expect("create file");
    session1
        .write_data(file1, 0, b"Top secret data", CompressionMode::Auto)
        .expect("write data");

    // Another thread retrieves the session with password
    let session2 = OifsSession::get_or_open_with_password(enc_image, total_size, Some(password))
        .expect("get encrypted");
    let root2 = session2.resolve_path(".").expect("resolve root");
    let lookup_id = session2.lookup(root2, "secret.txt").expect("lookup secret");
    let content = session2.read_data(lookup_id).expect("read secret");
    assert_eq!(&content, b"Top secret data");

    // Test unregister
    let removed = OifsSession::unregister_from_registry(enc_image);
    assert!(removed.is_some());

    // Unregistering again returns None
    let removed_again = OifsSession::unregister_from_registry(enc_image);
    assert!(removed_again.is_none());

    // Clean up
    let _ = fs::remove_file(enc_image);
}

#[test]
fn test_nested_and_chained_symlinks() {
    let tree_base = "test_symlink_tree";
    let dir_a = format!("{}/dir_a", tree_base);
    let dir_b = format!("{}/dir_b", dir_a);
    let dir_c = format!("{}/dir_c", dir_b);

    let _ = fs::remove_dir_all(tree_base);
    fs::create_dir_all(&dir_c).expect("create dir_c");

    let real_img = format!("{}/actual_disk.img", dir_c);
    let link_c = format!("{}/link_c.img", dir_b);
    let link_b = format!("{}/link_b.img", dir_a);
    let link_a = format!("{}/link_a.img", tree_base);
    let link_root = "root_chained_link.img";

    OifsSession::unregister_from_registry(&real_img);
    let _ = fs::remove_file(link_root);

    let total_size = 10 * 1024 * 1024;
    // 1. Create real image
    let _init = OifsSession::get_or_open(&real_img, total_size).expect("init real image");

    // 2. Build multi-hop chained symlinks:
    // link_root -> tree_base/link_a -> dir_a/link_b -> dir_b/link_c -> dir_c/actual_disk.img
    symlink("dir_c/actual_disk.img", &link_c).expect("symlink c");
    symlink("dir_b/link_c.img", &link_b).expect("symlink b");
    symlink("dir_a/link_b.img", &link_a).expect("symlink a");
    symlink(format!("{}/link_a.img", tree_base), link_root).expect("symlink root");

    let hop_paths = vec![
        real_img.clone(),
        link_c.clone(),
        link_b.clone(),
        link_a.clone(),
        link_root.to_string(),
    ];

    let mut handles = vec![];
    for (i, p) in hop_paths.into_iter().enumerate() {
        let handle = thread::spawn(move || {
            let session = OifsSession::get_or_open(&p, total_size).expect("open chained hop");
            assert!(session.is_direct(), "Hop {} at '{}' should be Direct", i, p);

            let root_id = session.resolve_path(".").expect("resolve root");
            let file_name = format!("hop_{}_file.txt", i);
            let inode_id = session
                .create_file(root_id, &file_name)
                .expect("create file");

            let data = format!("Payload from chain hop {} ({})", i, p);
            session
                .write_data(inode_id, 0, data.as_bytes(), CompressionMode::Auto)
                .expect("write data");

            (file_name, data)
        });
        handles.push(handle);
    }

    let mut results = vec![];
    for h in handles {
        results.push(h.join().unwrap());
    }

    // Verify all 5 files from the outermost root link
    let check_session = OifsSession::get_or_open(link_root, total_size).expect("check root link");
    let root_id = check_session.resolve_path(".").expect("resolve root");

    for (fname, expected) in results {
        let inode_id = check_session.lookup(root_id, &fname).expect("lookup file");
        let data = check_session.read_data(inode_id).expect("read data");
        assert_eq!(String::from_utf8(data).unwrap(), expected);
    }

    // Clean up
    OifsSession::unregister_from_registry(&real_img);
    let _ = fs::remove_file(link_root);
    let _ = fs::remove_dir_all(tree_base);
}

#[test]
fn test_concurrent_race_creation_via_different_symlinks() {
    let real_img = "test_race_actual.img";
    let link1 = "test_race_link1.img";
    let link2 = "test_race_link2.img";
    let link3 = "test_race_link3.img";

    for p in &[real_img, link1, link2, link3] {
        OifsSession::unregister_from_registry(p);
        let _ = fs::remove_file(p);
    }

    // Create dangling symlinks pointing to target that does not exist yet!
    symlink(real_img, link1).expect("symlink 1");
    symlink(real_img, link2).expect("symlink 2");
    symlink(real_img, link3).expect("symlink 3");

    let total_size = 10 * 1024 * 1024;
    let paths = vec![
        real_img.to_string(),
        link1.to_string(),
        link2.to_string(),
        link3.to_string(),
        format!("./{}", link1),
        format!("./{}", link2),
        format!("./{}", link3),
        format!("./{}", real_img),
    ];

    let num_threads = paths.len();
    let barrier_counter = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    for (i, p) in paths.into_iter().enumerate() {
        let counter = barrier_counter.clone();
        let handle = thread::spawn(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            while counter.load(Ordering::SeqCst) < num_threads {
                thread::sleep(Duration::from_millis(5));
            }

            // All threads simultaneously race to get_or_open
            let session = OifsSession::get_or_open(&p, total_size).expect("race get_or_open");
            assert!(
                session.is_direct(),
                "Thread {} with path '{}' must be Direct",
                i,
                p
            );

            let root_id = session.resolve_path(".").expect("resolve root");
            let file_name = format!("race_winner_{}.dat", i);
            let inode_id = session
                .create_file(root_id, &file_name)
                .expect("create file");

            let data = vec![(i % 255) as u8; 1024];
            session
                .write_data(inode_id, 0, &data, CompressionMode::Auto)
                .expect("write data");

            (file_name, data)
        });
        handles.push(handle);
    }

    let mut written = vec![];
    for h in handles {
        written.push(h.join().unwrap());
    }

    // Verify all files from link3
    let verify = OifsSession::get_or_open(link3, total_size).expect("verify session");
    let root_id = verify.resolve_path(".").expect("resolve root");
    for (fname, expected_data) in written {
        let inode_id = verify.lookup(root_id, &fname).expect("lookup file");
        let data = verify.read_data(inode_id).expect("read data");
        assert_eq!(data, expected_data);
    }

    // Clean up
    OifsSession::unregister_from_registry(real_img);
    for p in &[real_img, link1, link2, link3] {
        let _ = fs::remove_file(p);
    }
}

#[test]
fn test_symlink_concurrent_readers_and_writers_stress() {
    let real_img = "test_stress_actual.img";
    let link_w1 = "test_stress_w1.img";
    let link_w2 = "test_stress_w2.img";
    let link_r1 = "test_stress_r1.img";
    let link_r2 = "test_stress_r2.img";

    for p in &[real_img, link_w1, link_w2, link_r1, link_r2] {
        OifsSession::unregister_from_registry(p);
        let _ = fs::remove_file(p);
    }

    let total_size = 15 * 1024 * 1024;
    let _init = OifsSession::get_or_open(real_img, total_size).expect("init open");

    symlink(real_img, link_w1).expect("symlink w1");
    symlink(real_img, link_w2).expect("symlink w2");
    symlink(real_img, link_r1).expect("symlink r1");
    symlink(real_img, link_r2).expect("symlink r2");

    let num_iterations = 25;
    let mut writer_handles = vec![];
    let writer_paths = vec![
        real_img.to_string(),
        link_w1.to_string(),
        link_w2.to_string(),
    ];

    // Spawn writers
    for (w_idx, path) in writer_paths.into_iter().enumerate() {
        let handle = thread::spawn(move || {
            let session = OifsSession::get_or_open(&path, total_size).expect("writer open");
            let root_id = session.resolve_path(".").expect("resolve root");
            let fname = format!("writer_{}.log", w_idx);
            let inode_id = session
                .create_file(root_id, &fname)
                .expect("create writer file");

            for iter in 0..num_iterations {
                let chunk = format!("[W{}:iter{}] ", w_idx, iter);
                let offset = (iter * chunk.len()) as u64;
                session
                    .write_data(inode_id, offset, chunk.as_bytes(), CompressionMode::Never)
                    .expect("write chunk");
                thread::sleep(Duration::from_millis(2));
            }
            fname
        });
        writer_handles.push(handle);
    }

    // Spawn concurrent readers
    let reader_paths = vec![
        link_r1.to_string(),
        link_r2.to_string(),
        real_img.to_string(),
    ];
    let mut reader_handles = vec![];

    for (r_idx, path) in reader_paths.into_iter().enumerate() {
        let handle = thread::spawn(move || {
            let session = OifsSession::get_or_open(&path, total_size).expect("reader open");
            let root_id = session.resolve_path(".").expect("resolve root");

            // Periodically list and read directory entries
            for _ in 0..num_iterations {
                let entries = session.list_dir(root_id).unwrap_or_default();
                for entry in entries {
                    if entry.name != "." && entry.name != ".." {
                        let _ = session.read_data(entry.inode);
                    }
                }
                thread::sleep(Duration::from_millis(3));
            }
            r_idx
        });
        reader_handles.push(handle);
    }

    // Wait for all writers and readers
    let mut written_files = vec![];
    for wh in writer_handles {
        written_files.push(wh.join().unwrap());
    }
    for rh in reader_handles {
        let _ = rh.join().unwrap();
    }

    // Verify all writer files have complete logs
    let verify_session = OifsSession::get_or_open(link_r1, total_size).expect("verify open");
    let root_id = verify_session.resolve_path(".").expect("resolve root");

    for (w_idx, fname) in written_files.into_iter().enumerate() {
        let inode_id = verify_session
            .lookup(root_id, &fname)
            .expect("lookup writer file");
        let data = verify_session
            .read_data(inode_id)
            .expect("read writer data");
        let text = String::from_utf8(data).expect("utf8 string");

        for iter in 0..num_iterations {
            let expected_tag = format!("[W{}:iter{}]", w_idx, iter);
            assert!(
                text.contains(&expected_tag),
                "Missing expected tag {} in {}",
                expected_tag,
                fname
            );
        }
    }

    // Clean up
    OifsSession::unregister_from_registry(real_img);
    for p in &[real_img, link_w1, link_w2, link_r1, link_r2] {
        let _ = fs::remove_file(p);
    }
}

#[test]
fn test_symlink_inter_process_cli_and_session() {
    use std::process::Command;

    let real_img = "test_proc_real.img";
    let link_master = "test_proc_link_master.img";
    let link_cli = "test_proc_link_cli.img";

    for p in &[real_img, link_master, link_cli] {
        OifsSession::unregister_from_registry(p);
        let _ = fs::remove_file(p);
    }

    let total_size = 10 * 1024 * 1024;
    // 1. Create real image and symlinks via get_or_open
    let _master_session = OifsSession::get_or_open(real_img, total_size).expect("create master");

    symlink(real_img, link_master).expect("symlink master");
    symlink(real_img, link_cli).expect("symlink cli");

    // 2. Open Direct Session via link_master in this process
    let master_session =
        OifsSession::get_or_open(link_master, total_size).expect("open via link_master");
    assert!(
        master_session.is_direct(),
        "Master session should be Direct"
    );

    // 3. Subprocess CLI accesses the filesystem via link_cli
    let test_file = "payload_from_cli.txt";
    fs::write(test_file, b"Cross-process data written via symlink").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_oifs"))
        .args(["--image", link_cli, "put", test_file])
        .output()
        .expect("CLI put execution failed");

    assert!(
        output.status.success(),
        "CLI put failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    // 4. In-process Master session via link_master immediately verifies the new file
    let root_id = master_session.resolve_path(".").expect("resolve root");
    let inode_id = master_session
        .lookup(root_id, test_file)
        .expect("Master should find file put by CLI");
    let content = master_session.read_data(inode_id).expect("read content");
    assert_eq!(&content, b"Cross-process data written via symlink");

    // Clean up
    OifsSession::unregister_from_registry(real_img);
    let _ = fs::remove_file(test_file);
    for p in &[real_img, link_master, link_cli] {
        let _ = fs::remove_file(p);
    }
}
