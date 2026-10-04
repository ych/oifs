use std::fs;
use std::process::Command;
use std::thread;
use tempfile::tempdir;

#[test]
fn test_cli_concurrent_processes() {
    let dir = tempdir().unwrap();
    let img_path = dir.path().join("test_cli_concurrency.img");
    let img_str = img_path.to_str().unwrap().to_string();

    let bin_path = env!("CARGO_BIN_EXE_oifs");

    // 1. Create image
    let status = Command::new(bin_path)
        .args(["--image", &img_str, "create", "--size", "10"])
        .status()
        .expect("Create failed");
    assert!(status.success());

    // Prepare 4 test payload files
    for i in 0..4 {
        let payload_path = dir.path().join(format!("payload_{}.txt", i));
        fs::write(&payload_path, format!("Payload data from process {}", i)).unwrap();
    }

    // 2. Launch 4 independent CLI processes concurrently putting files into the same image
    let mut handles = Vec::new();
    for i in 0..4 {
        let bin = bin_path.to_string();
        let img = img_str.clone();
        let payload_path = dir.path().join(format!("payload_{}.txt", i));
        let handle = thread::spawn(move || {
            let remote = format!("remote_{}.txt", i);
            let output = Command::new(bin)
                .args([
                    "--image",
                    &img,
                    "put",
                    payload_path.to_str().unwrap(),
                    &remote,
                ])
                .output()
                .expect("CLI put failed");
            assert!(
                output.status.success(),
                "Process {} put failed: {}",
                i,
                String::from_utf8_lossy(&output.stderr)
            );
        });
        handles.push(handle);
    }

    for h in handles {
        h.join().unwrap();
    }

    // 3. Run ls and verify all 4 files are present
    let ls_output = Command::new(bin_path)
        .args(["--image", &img_str, "ls"])
        .output()
        .expect("CLI ls failed");
    assert!(ls_output.status.success());
    let ls_stdout = String::from_utf8_lossy(&ls_output.stdout);

    for i in 0..4 {
        assert!(
            ls_stdout.contains(&format!("remote_{}.txt", i)),
            "Missing remote_{}.txt in ls output:\n{}",
            i,
            ls_stdout
        );
    }

    // 4. Concurrently get files back
    let mut get_handles = Vec::new();
    for i in 0..4 {
        let bin = bin_path.to_string();
        let img = img_str.clone();
        let downloaded_path = dir.path().join(format!("downloaded_{}.txt", i));
        let handle = thread::spawn(move || {
            let remote = format!("remote_{}.txt", i);
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
        get_handles.push(handle);
    }

    for h in get_handles {
        h.join().unwrap();
    }

    // 5. Verify downloaded file contents
    for i in 0..4 {
        let downloaded_path = dir.path().join(format!("downloaded_{}.txt", i));
        let content = fs::read_to_string(&downloaded_path).unwrap();
        assert_eq!(content, format!("Payload data from process {}", i));
    }
}
