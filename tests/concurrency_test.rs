use oifs::disk::DiskManager;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// Test: Intra-process threading (Threads share DiskManager via Arc, logic protected by Mutex)
#[test]
fn test_intra_process_threading() {
    let image_path = "threading_test.img";
    if Path::new(image_path).exists() {
        fs::remove_file(image_path).unwrap();
    }

    Command::new(env!("CARGO_BIN_EXE_oifs"))
        .args(["--image", image_path, "create", "--size", "10"])
        .status()
        .expect("Cmd failed");

    let dm = DiskManager::open(image_path, 0).expect("Open failed");
    let dm = Arc::new(dm); // Now DiskManager is Clone/ThreadSafe

    let mut handles = vec![];

    // 4 Threads, each writing to a DIFFERENT file
    for i in 0..4 {
        let dm_clone = dm.clone();
        let handle = thread::spawn(move || {
            let root = dm_clone.resolve_path(".").unwrap();
            let filename = format!("thread_file_{}.txt", i);

            // Create file
            let inode_id = dm_clone
                .create_file(root, &filename)
                .expect("Create failed");

            let mut iter = 0;
            let mut last_data = Vec::new();
            let mut current_offset = 0;

            // Write loop
            // Max file size is 48KB (12 blocks). We write 40KB total (10 * 4KB).
            let chunk_size = 4096; // 4KB
            let max_iters = 10;

            for _ in 0..max_iters {
                let mut data = vec![0u8; chunk_size];
                for b in data.iter_mut() {
                    *b = (iter % 255) as u8;
                }

                dm_clone
                    .write_data(
                        inode_id,
                        current_offset,
                        &data,
                        oifs::disk::CompressionMode::Auto,
                    )
                    .expect("Write failed");
                current_offset += data.len() as u64;

                last_data.extend_from_slice(&data);
                iter += 1;

                if iter % 5 == 0 {
                    let _ = dm_clone.flush();
                }
                thread::sleep(Duration::from_millis(10));
            }
            (filename, last_data)
        });
        handles.push(handle);
    }

    let mut results = vec![];
    for h in handles {
        results.push(h.join().unwrap());
    }

    // Verify Integrity
    let root = dm.resolve_path(".").unwrap();
    for (filename, expected_data) in &results {
        let inode_id = dm.lookup(root, filename).expect("File missing");
        let actual_data = dm.read_data(inode_id).expect("Read failed");

        let len = expected_data.len();
        assert!(
            actual_data.len() >= len,
            "File truncated? {:?} vs {:?}",
            actual_data.len(),
            len
        );
        assert_eq!(
            &actual_data[..len],
            &expected_data[..],
            "Content mismatch for {}",
            filename
        );
    }
    dm.flush().unwrap();
    drop(dm);

    // List files via CLI to stdout
    println!("--- Files after concurrency test ---");
    let output = Command::new(env!("CARGO_BIN_EXE_oifs"))
        .args(["--image", image_path, "ls", "-r"])
        .output()
        .expect("LS Failed");
    println!("{}", String::from_utf8_lossy(&output.stdout));
    println!("------------------------------------");

    fs::remove_file(image_path).unwrap();
}

#[test]
fn test_inter_process_locking() {
    let image_path = "test_process.img";
    if Path::new(image_path).exists() {
        fs::remove_file(image_path).unwrap();
    }

    Command::new(env!("CARGO_BIN_EXE_oifs"))
        .args(["--image", image_path, "create", "--size", "10"])
        .status()
        .expect("Cmd failed");

    let py_script = r#"
import fcntl
import time
import sys
import struct

path = sys.argv[1]
f = open(path, "r+")
# Lock it
try:
    lock_data = struct.pack('hhllh', fcntl.F_WRLCK, 0, 0, 0, 0)
    fcntl.fcntl(f.fileno(), fcntl.F_SETLK, lock_data)
    print("LOCKED", flush=True)
    time.sleep(3)
except Exception as e:
    print(f"FAIL: {e}")
"#;
    fs::write("lock_holder.py", py_script).unwrap();

    let mut child = Command::new("python3")
        .arg("lock_holder.py")
        .arg(image_path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn python");

    let stdout = child.stdout.take().unwrap();
    use std::io::{BufRead, BufReader};
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();

    if line.contains("LOCKED") {
        let res = DiskManager::open(Path::new(image_path), 0);
        assert!(res.is_err(), "Should fail to acquire lock");
    } else {
        // Maybe python failing? CI environment?
        // Skip if cannot setup
    }

    let _ = child.wait();
    let _ = fs::remove_file("lock_holder.py");
    let _ = fs::remove_file(image_path);
}
