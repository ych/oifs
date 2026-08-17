use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use oifs::disk::{CompressionMode, DefragMode};
use oifs::ipc::{get_master_info_path, SessionMode};
use oifs::session::OifsSession;

#[test]
fn test_empty_zero_byte_files_across_ipc() {
    let img_path = "test_edge_empty.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let master = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Master open failed");
    let client = OifsSession::open(img_path, 0).expect("Client open failed");

    let root = client.resolve_path(".").expect("Resolve root failed");
    let empty_fid = client.create_file(root, "empty.txt").expect("Create empty file failed");

    // Read 0-byte file
    let empty_data = client.read_data(empty_fid).expect("Read empty file failed");
    assert!(empty_data.is_empty(), "0-byte file must return empty Vec");

    // Write empty slice
    client.write_data(empty_fid, 0, &[], CompressionMode::Auto).expect("Write empty data failed");

    // Verify via master
    let master_read = master.read_data(empty_fid).expect("Master read failed");
    assert!(master_read.is_empty());

    // List dir
    let entries = client.list_dir(root).expect("List dir failed");
    let entry = entries.iter().find(|e| e.name == "empty.txt").expect("empty.txt must be listed");
    assert_eq!(entry.inode, empty_fid);

    drop(client);
    drop(master);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_deep_nested_directory_tree_and_path_resolution_across_ipc() {
    let img_path = "test_edge_nested.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let master = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Master open failed");
    let client = OifsSession::open(img_path, 0).expect("Client open failed");

    // Build hierarchy: lvl1/lvl2/lvl3/lvl4/lvl5
    let mut current_dir = client.resolve_path(".").unwrap();
    let levels = ["lvl1", "lvl2", "lvl3", "lvl4", "lvl5"];
    for lvl in &levels {
        current_dir = client.create_directory(current_dir, lvl).expect("Create dir failed");
    }

    // Create file inside deep directory
    let deep_file_id = client.create_file(current_dir, "deep_data.bin").expect("Create deep file failed");
    let payload = b"Deeply nested file content accessible via transparent IPC proxy";
    client.write_data(deep_file_id, 0, payload, CompressionMode::Auto).expect("Write deep file failed");

    // Resolve parent of deep path
    let (parent_id, name) = client.resolve_parent("lvl1/lvl2/lvl3/lvl4/lvl5/deep_data.bin").expect("Resolve parent failed");
    assert_eq!(parent_id, current_dir);
    assert_eq!(name, "deep_data.bin");

    // Resolve full path
    let resolved_id = client.resolve_path("lvl1/lvl2/lvl3/lvl4/lvl5/deep_data.bin").expect("Resolve path failed");
    assert_eq!(resolved_id, deep_file_id);

    // Master reads resolved file
    let data = master.read_data(resolved_id).expect("Master read failed");
    assert_eq!(data, payload);

    let fsck = master.verify_integrity().expect("Fsck failed");
    assert!(fsck.is_clean);

    drop(client);
    drop(master);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_invalid_lookups_and_nonexistent_operations_across_ipc() {
    let img_path = "test_edge_conflicts.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let master = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Master open failed");
    let client = OifsSession::open(img_path, 0).expect("Client open failed");

    let root = client.resolve_path(".").unwrap();
    let valid_fid = client.create_file(root, "valid.txt").expect("Create valid file failed");

    // 1. Lookup non-existent file name should fail gracefully with Error
    let lookup_res = client.lookup(root, "non_existent_file.xyz");
    assert!(lookup_res.is_err(), "Looking up missing file must return error");

    // 2. Reading non-existent inode ID should fail cleanly over IPC without crashing server
    let bad_read_res = client.read_data(999_999);
    assert!(bad_read_res.is_err(), "Reading invalid inode must return error");

    // 3. Creating file in non-existent directory inode ID should fail cleanly
    let bad_parent_res = client.create_file(888_888, "ghost.txt");
    assert!(bad_parent_res.is_err(), "Creating file in invalid parent must return error");

    // 4. Resolving non-existent multi-level path should fail cleanly
    let bad_path_res = client.resolve_path("no_such_folder/no_such_file.txt");
    assert!(bad_path_res.is_err(), "Resolving non-existent path must return error");

    // 5. Subsequent valid operation should still succeed flawlessly
    client.write_data(valid_fid, 0, b"Still working normally", CompressionMode::Auto).expect("Write should succeed");
    let read_back = client.read_data(valid_fid).expect("Read should succeed");
    assert_eq!(read_back, b"Still working normally");

    drop(client);
    drop(master);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_file_deletion_lifecycle_and_reuse_across_ipc() {
    let img_path = "test_edge_deletion.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let master = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Master open failed");
    let client = OifsSession::open(img_path, 0).expect("Client open failed");

    let root = client.resolve_path(".").unwrap();
    let fid1 = client.create_file(root, "reusable.txt").unwrap();
    client.write_data(fid1, 0, b"Version 1 of file data", CompressionMode::Auto).unwrap();

    // Verify existence
    assert_eq!(client.lookup(root, "reusable.txt").unwrap(), fid1);

    // Delete file via client
    client.delete_file(root, "reusable.txt").expect("Delete file failed");

    // Verify lookup fails now
    assert!(client.lookup(root, "reusable.txt").is_err(), "Lookup after delete must fail");

    // Recreate file with same name
    let fid2 = client.create_file(root, "reusable.txt").expect("Recreate file failed");
    client.write_data(fid2, 0, b"Version 2 - fresh content", CompressionMode::Auto).unwrap();

    // Verify new content
    let read_v2 = client.read_data(fid2).unwrap();
    assert_eq!(read_v2, b"Version 2 - fresh content");

    let master_read = master.read_data(fid2).unwrap();
    assert_eq!(master_read, b"Version 2 - fresh content");

    let fsck = master.verify_integrity().unwrap();
    assert!(fsck.is_clean, "Filesystem must remain clean after delete and recreate");

    drop(client);
    drop(master);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_large_multi_block_boundary_payloads_across_ipc() {
    let img_path = "test_edge_large.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let master = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Master open failed");
    let client = OifsSession::open(img_path, 0).expect("Client open failed");

    let root = client.resolve_path(".").unwrap();

    // 1. 40KB file (10 blocks) with Auto compression
    let fid_auto = client.create_file(root, "large_auto.bin").unwrap();
    let payload_40k: Vec<u8> = (0..40 * 1024).map(|i| (i % 251) as u8).collect();
    client.write_data(fid_auto, 0, &payload_40k, CompressionMode::Auto).expect("Write 40KB auto failed");
    let read_auto = client.read_data(fid_auto).expect("Read 40KB auto failed");
    assert_eq!(read_auto, payload_40k);

    // 2. 40KB repetitive file with Always compression
    let fid_comp = client.create_file(root, "large_comp.txt").unwrap();
    let repetitive_40k: Vec<u8> = vec![b'A'; 40 * 1024];
    client.write_data(fid_comp, 0, &repetitive_40k, CompressionMode::Always).expect("Write 40KB compressed failed");
    let read_comp = client.read_data(fid_comp).expect("Read 40KB compressed failed");
    assert_eq!(read_comp, repetitive_40k);

    // 3. 40KB file with Never compression
    let fid_raw = client.create_file(root, "large_raw.bin").unwrap();
    client.write_data(fid_raw, 0, &payload_40k, CompressionMode::Never).expect("Write 40KB raw failed");
    let read_raw = master.read_data(fid_raw).expect("Master read 40KB raw failed");
    assert_eq!(read_raw, payload_40k);

    let fsck = master.verify_integrity().unwrap();
    assert!(fsck.is_clean);

    drop(client);
    drop(master);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_corrupted_rendezvous_file_self_healing() {
    let img_path = "test_edge_corrupt_rendezvous.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    // 1. Create image
    {
        let session = OifsSession::open_network(img_path, 10 * 1024 * 1024, None).unwrap();
        drop(session);
    }

    // 2. Intentionally inject corrupted garbage bytes into .image.master file
    let master_file = get_master_info_path(img_path);
    fs::write(&master_file, b"!!!MALFORMED_GARBAGE_JSON_BYTES_XYZ12345!!!").unwrap();
    assert!(master_file.exists());

    // 3. Client opens via open_network -> Must detect corrupt file, remove it, and become Master!
    let session = OifsSession::open_network(img_path, 0, None)
        .expect("Open with corrupt rendezvous file must recover");
    assert!(session.is_direct(), "Recovered session must be Master");

    // 4. Verify healthy operation
    let root = session.resolve_path(".").unwrap();
    let fid = session.create_file(root, "healed.txt").unwrap();
    session.write_data(fid, 0, b"Self-healed from corrupt rendezvous", CompressionMode::Auto).unwrap();
    let read_back = session.read_data(fid).unwrap();
    assert_eq!(read_back, b"Self-healed from corrupt rendezvous");

    drop(session);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_high_concurrency_burst_operations_across_ipc() {
    let img_path = "test_edge_burst.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let master = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Master open failed");
    let master_arc = Arc::new(master);

    let num_threads = 6;
    let ops_per_thread = 15;
    let mut handles = Vec::new();

    let img_path_str = img_path.to_string();

    for t_idx in 0..num_threads {
        let path_clone = img_path_str.clone();
        let handle = thread::spawn(move || {
            let client = OifsSession::open(&path_clone, 0).expect("Client open failed");
            let root = client.resolve_path(".").unwrap();
            let thread_dir_name = format!("dir_thread_{}", t_idx);
            let thread_dir = client.create_directory(root, &thread_dir_name).expect("Create thread dir failed");

            for op_idx in 0..ops_per_thread {
                let filename = format!("file_{}.txt", op_idx);
                let fid = client.create_file(thread_dir, &filename).expect("Create file in burst failed");
                let content = format!("Burst content from thread {} iteration {}", t_idx, op_idx);
                client.write_data(fid, 0, content.as_bytes(), CompressionMode::Auto).expect("Write file in burst failed");
                
                let read_back = client.read_data(fid).expect("Read file in burst failed");
                assert_eq!(read_back, content.as_bytes());

                let looked_up = client.lookup(thread_dir, &filename).expect("Lookup in burst failed");
                assert_eq!(looked_up, fid);
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().unwrap();
    }

    // Master verifies all directories and files
    let root = master_arc.resolve_path(".").unwrap();
    let root_entries = master_arc.list_dir(root).expect("List root failed");
    assert_eq!(root_entries.len(), num_threads, "All thread directories must be present");

    for entry in root_entries {
        let files = master_arc.list_dir(entry.inode).expect("List thread dir failed");
        assert_eq!(files.len(), ops_per_thread, "All files within directory must be present");
    }

    let fsck = master_arc.verify_integrity().expect("Fsck failed");
    assert!(fsck.is_clean, "Filesystem must remain consistent after high concurrency burst");

    drop(master_arc);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_remote_defragmentation_and_fsck_across_ipc() {
    let img_path = "test_edge_remote_defrag.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let master = OifsSession::open(img_path, 10 * 1024 * 1024).expect("Master open failed");
    let client = OifsSession::open(img_path, 0).expect("Client open failed");

    let root = client.resolve_path(".").unwrap();

    // Create several files to create data blocks
    for i in 0..10 {
        let fid = client.create_file(root, &format!("defrag_file_{}.dat", i)).unwrap();
        let payload = vec![(i as u8) * 17; 4096];
        client.write_data(fid, 0, &payload, CompressionMode::Never).unwrap();
    }

    // Remote client triggers fragmentation analysis
    let frag_stats = client.analyze_fragmentation().expect("Remote analyze fragmentation failed");
    assert!(frag_stats.total_blocks > 0);
    assert_eq!(frag_stats.total_blocks, 1533);
    assert!(frag_stats.used_blocks >= 10);

    // Remote client triggers defragmentation
    let defrag_stats = client.defragment(img_path, DefragMode::Safe, None)
        .expect("Remote defragment failed");
    assert!(defrag_stats.files_processed >= 10);

    // Remote client triggers integrity verification (fsck)
    let fsck_report = client.verify_integrity().expect("Remote fsck failed");
    assert!(fsck_report.is_clean);

    drop(client);
    drop(master);
    let _ = fs::remove_file(img_path);
}

#[test]
fn test_interleaved_local_and_network_modes_on_different_images() {
    let img_local = "test_interleaved_local.img";
    let img_net = "test_interleaved_net.img";

    if Path::new(img_local).exists() { let _ = fs::remove_file(img_local); }
    if Path::new(img_net).exists() { let _ = fs::remove_file(img_net); }

    // Start local session on img_local
    let local_master = OifsSession::open(img_local, 10 * 1024 * 1024).unwrap();
    let local_client = OifsSession::open(img_local, 0).unwrap();
    assert_eq!(local_master.mode(), &SessionMode::Local);

    // Start network session on img_net
    let net_master = OifsSession::open_network(img_net, 10 * 1024 * 1024, None).unwrap();
    let net_client = OifsSession::open_network(img_net, 0, None).unwrap();
    assert!(matches!(net_master.mode(), SessionMode::Network { .. }));

    // Interleaved write operations
    let r_local = local_client.resolve_path(".").unwrap();
    let fid_local = local_client.create_file(r_local, "loc.txt").unwrap();
    local_client.write_data(fid_local, 0, b"Local mode payload", CompressionMode::Auto).unwrap();

    let r_net = net_client.resolve_path(".").unwrap();
    let fid_net = net_client.create_file(r_net, "net.txt").unwrap();
    net_client.write_data(fid_net, 0, b"Network mode payload", CompressionMode::Auto).unwrap();

    // Verify independent reads
    assert_eq!(local_master.read_data(fid_local).unwrap(), b"Local mode payload");
    assert_eq!(net_master.read_data(fid_net).unwrap(), b"Network mode payload");

    drop(local_client);
    drop(local_master);
    drop(net_client);
    drop(net_master);

    let _ = fs::remove_file(img_local);
    let _ = fs::remove_file(img_net);
}
