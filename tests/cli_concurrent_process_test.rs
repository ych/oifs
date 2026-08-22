use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;

#[test]
fn test_cli_concurrent_processes() {
    let img_path = "test_cli_concurrency.img";
    if Path::new(img_path).exists() {
        let _ = fs::remove_file(img_path);
    }

    let bin_path = env!("CARGO_BIN_EXE_oifs");

    // 1. Create image
    let status = Command::new(bin_path)
        .args(["--image", img_path, "create", "--size", "10"])
        .status()
        .expect("Create failed");
    assert!(status.success());

    // Prepare 4 test payload files
    for i in 0..4 {
        let filename = format!("payload_{}.txt", i);
        fs::write(&filename, format!("Payload data from process {}", i)).unwrap();
    }

    // 2. Launch 4 independent CLI processes concurrently putting files into the same image
    let mut handles = Vec::new();
    for i in 0..4 {
        let bin = bin_path.to_string();
        let img = img_path.to_string();
        let handle = thread::spawn(move || {
            let payload = format!("payload_{}.txt", i);
            let remote = format!("remote_{}.txt", i);
            let output = Command::new(bin)
                .args(["--image", &img, "put", &payload, &remote])
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
        .args(["--image", img_path, "ls"])
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
        let img = img_path.to_string();
        let handle = thread::spawn(move || {
            let remote = format!("remote_{}.txt", i);
            let downloaded = format!("downloaded_{}.txt", i);
            let output = Command::new(bin)
                .args(["--image", &img, "get", &remote, &downloaded])
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
        let downloaded = format!("downloaded_{}.txt", i);
        let content = fs::read_to_string(&downloaded).unwrap();
        assert_eq!(content, format!("Payload data from process {}", i));
        let _ = fs::remove_file(downloaded);
        let _ = fs::remove_file(format!("payload_{}.txt", i));
    }

    let _ = fs::remove_file(img_path);
}
