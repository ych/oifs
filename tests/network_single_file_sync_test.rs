use oifs::disk::CompressionMode;
use oifs::session::OifsSession;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

struct TestContext {
    image_path: String,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let image_path = format!("{}.img", name);
        if Path::new(&image_path).exists() {
            let _ = fs::remove_file(&image_path);
        }
        let master_file = format!("{}.master", image_path);
        if Path::new(&master_file).exists() {
            let _ = fs::remove_file(&master_file);
        }
        Self { image_path }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        if Path::new(&self.image_path).exists() {
            let _ = fs::remove_file(&self.image_path);
        }
        let master_file = format!("{}.master", self.image_path);
        if Path::new(&master_file).exists() {
            let _ = fs::remove_file(&master_file);
        }
    }
}

#[test]
fn test_network_multi_node_concurrent_slice_writes_to_single_file() {
    let ctx = TestContext::new("test_net_single_file_slices");
    let num_nodes = 8;
    let slice_size = 4096;

    // Node 0 (Master) initializes the filesystem over TCP Network Mode
    let master_session = OifsSession::open_network(
        &ctx.image_path,
        20 * 1024 * 1024,
        Some("127.0.0.1:0".to_string()),
    )
    .expect("open master");

    let root_id = master_session.superblock().expect("sb").root_inode;
    let file_id = master_session
        .create_file(root_id, "shared_matrix.dat")
        .expect("create file");

    // Spawn N concurrent simulated network nodes
    let mut handles = Vec::new();
    let image_path = ctx.image_path.clone();

    for node_idx in 0..num_nodes {
        let path = image_path.clone();
        let handle = thread::spawn(move || {
            let session = OifsSession::open_network(&path, 0, None).expect("client open");

            // Generate unique byte pattern for this node
            let byte_val = (node_idx as u8 + 1) * 17;
            let payload = vec![byte_val; slice_size];
            let offset = (node_idx * slice_size) as u64;

            // Write to designated slice in the single shared file
            session
                .write_data(file_id, offset, &payload, CompressionMode::Never)
                .expect("write slice");
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().expect("join client thread");
    }

    // Verify final file state through a new client node
    let verify_session =
        OifsSession::open_network(&ctx.image_path, 0, None).expect("verify session");
    let full_content = verify_session
        .read_data(file_id)
        .expect("read full content");

    assert_eq!(
        full_content.len(),
        num_nodes * slice_size,
        "Full file length must match total slices"
    );

    for node_idx in 0..num_nodes {
        let expected_val = (node_idx as u8 + 1) * 17;
        let start = node_idx * slice_size;
        let end = start + slice_size;
        let slice = &full_content[start..end];
        assert!(
            slice.iter().all(|&b| b == expected_val),
            "Node {}'s slice (offset {}..{}) got corrupted or overwritten",
            node_idx,
            start,
            end
        );
    }
}

#[test]
fn test_network_multi_node_concurrent_partitioned_records_in_single_file() {
    let ctx = TestContext::new("test_net_single_file_records");
    let num_nodes = 6;
    let records_per_node = 30;
    let record_len = 32;

    // Node 0 (Master) initializes the file
    let master = OifsSession::open_network(
        &ctx.image_path,
        20 * 1024 * 1024,
        Some("127.0.0.1:0".to_string()),
    )
    .expect("open master");
    let root_id = master.superblock().expect("sb").root_inode;
    let file_id = master
        .create_file(root_id, "shared_audit.log")
        .expect("create audit log");

    let mut handles = Vec::new();
    let image_path = ctx.image_path.clone();

    for node_idx in 0..num_nodes {
        let path = image_path.clone();
        let handle = thread::spawn(move || {
            let session = OifsSession::open_network(&path, 0, None).expect("client open");

            for seq in 0..records_per_node {
                // Fixed 32-byte formatted log entry
                let entry = format!("NODE:{:02}:SEQ:{:04}:DATA:OK!======\n", node_idx, seq);
                assert_eq!(entry.len(), record_len);

                // Write into node's dedicated record partition slot
                let slot = node_idx * records_per_node + seq;
                let offset = (slot * record_len) as u64;
                session
                    .write_data(file_id, offset, entry.as_bytes(), CompressionMode::Never)
                    .expect("write entry");
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().expect("join thread");
    }

    // Read full log and verify
    let data = master.read_data(file_id).expect("read audit log");
    let log_str = String::from_utf8(data).expect("utf8 string");
    let lines: Vec<&str> = log_str.lines().collect();

    assert_eq!(
        lines.len(),
        num_nodes * records_per_node,
        "Total log lines must equal num_nodes * records_per_node"
    );

    for node_idx in 0..num_nodes {
        for seq in 0..records_per_node {
            let slot = node_idx * records_per_node + seq;
            let expected = format!("NODE:{:02}:SEQ:{:04}:DATA:OK!======", node_idx, seq);
            assert_eq!(
                lines[slot], expected,
                "Slot {} must match Node {} seq {}",
                slot, node_idx, seq
            );
        }
    }
}

#[test]
fn test_network_multi_node_concurrent_readers_and_writers_on_single_file() {
    let ctx = TestContext::new("test_net_readers_writers");
    let num_writers = 4;
    let num_readers = 4;
    let iterations = 20;

    let master = OifsSession::open_network(
        &ctx.image_path,
        20 * 1024 * 1024,
        Some("127.0.0.1:0".to_string()),
    )
    .expect("open master");
    let root_id = master.superblock().expect("sb").root_inode;
    let file_id = master
        .create_file(root_id, "state_record.bin")
        .expect("create file");

    // Initialize with 256 bytes of zeros
    master
        .write_data(file_id, 0, &[0u8; 256], CompressionMode::Never)
        .expect("init write");

    let successful_reads = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    // Writers
    for writer_id in 0..num_writers {
        let path = ctx.image_path.clone();
        let handle = thread::spawn(move || {
            let session = OifsSession::open_network(&path, 0, None).expect("open writer");

            for i in 0..iterations {
                let val = ((writer_id + 1) * 10 + (i % 10)) as u8;
                let payload = vec![val; 64];
                let offset = (writer_id * 64) as u64;
                session
                    .write_data(file_id, offset, &payload, CompressionMode::Never)
                    .expect("writer write");
                thread::yield_now();
            }
        });
        handles.push(handle);
    }

    // Readers
    for _ in 0..num_readers {
        let path = ctx.image_path.clone();
        let counter = Arc::clone(&successful_reads);
        let handle = thread::spawn(move || {
            let session = OifsSession::open_network(&path, 0, None).expect("open reader");

            for _ in 0..iterations {
                if let Ok(data) = session.read_data(file_id) {
                    assert_eq!(data.len(), 256);
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                thread::yield_now();
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().expect("join thread");
    }

    assert!(
        successful_reads.load(Ordering::SeqCst) > 0,
        "Readers must have succeeded concurrently"
    );
}
