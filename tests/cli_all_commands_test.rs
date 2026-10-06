use std::fs;
use std::path::Path;
use std::process::Command;

struct TestContext {
    image_path: String,
    temp_files: Vec<String>,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let image_path = format!("{}.img", name);
        if Path::new(&image_path).exists() {
            let _ = fs::remove_file(&image_path);
        }
        Self {
            image_path,
            temp_files: Vec::new(),
        }
    }

    fn create_temp_file(&mut self, name: &str, content: &[u8]) -> String {
        let path = format!("{}_{}", self.image_path, name);
        fs::write(&path, content).expect("write temp file");
        self.temp_files.push(path.clone());
        path
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        if Path::new(&self.image_path).exists() {
            let _ = fs::remove_file(&self.image_path);
        }
        for file in &self.temp_files {
            if Path::new(file).exists() {
                let _ = fs::remove_file(file);
            }
        }
    }
}

#[test]
fn test_cli_full_lifecycle_and_json_outputs() {
    let bin_path = env!("CARGO_BIN_EXE_oifs");
    let mut ctx = TestContext::new("test_cli_lifecycle");

    // 1. Create image
    let output = Command::new(bin_path)
        .args(["--image", &ctx.image_path, "create", "--size", "10"])
        .output()
        .expect("create image");
    assert!(output.status.success());

    // 2. Mkdir
    let output = Command::new(bin_path)
        .args(["--image", &ctx.image_path, "mkdir", "docs"])
        .output()
        .expect("mkdir");
    assert!(output.status.success());

    // 3. Put files with different compression modes
    let f1 = ctx.create_temp_file("input1.txt", b"Regular plain text content");
    let output = Command::new(bin_path)
        .args([
            "--image",
            &ctx.image_path,
            "put",
            "--no-compress",
            &f1,
            "docs/plain.txt",
        ])
        .output()
        .expect("put no-compress");
    assert!(output.status.success());

    let f2 = ctx.create_temp_file("input2.txt", "COMPRESSIBLE_CHUNK_".repeat(300).as_bytes());
    let output = Command::new(bin_path)
        .args([
            "--image",
            &ctx.image_path,
            "put",
            "--compress",
            &f2,
            "docs/compressed.txt",
        ])
        .output()
        .expect("put compress");
    assert!(output.status.success());

    // 4. Append
    let f_app = ctx.create_temp_file("app.txt", b"First line\n");
    let output = Command::new(bin_path)
        .args(["--image", &ctx.image_path, "put", &f_app, "log.txt"])
        .output()
        .expect("put initial log");
    assert!(output.status.success());

    let output = Command::new(bin_path)
        .args([
            "--image",
            &ctx.image_path,
            "append",
            "log.txt",
            "Second line\n",
        ])
        .output()
        .expect("append to log");
    assert!(output.status.success());

    // 5. Get and verify
    let downloaded_log = format!("{}_downloaded_log.txt", ctx.image_path);
    ctx.temp_files.push(downloaded_log.clone());
    let output = Command::new(bin_path)
        .args([
            "--image",
            &ctx.image_path,
            "get",
            "log.txt",
            &downloaded_log,
        ])
        .output()
        .expect("get log");
    assert!(output.status.success());

    let read_log = fs::read(&downloaded_log).expect("read downloaded log");
    assert_eq!(read_log, b"First line\nSecond line\n");

    // 6. List with --json flag
    let output = Command::new(bin_path)
        .args(["--image", &ctx.image_path, "--json", "ls", "-r"])
        .output()
        .expect("ls json");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("docs"));
    assert!(stdout.contains("log.txt"));

    // 7. Analyze with --json flag
    let output = Command::new(bin_path)
        .args(["--image", &ctx.image_path, "--json", "analyze"])
        .output()
        .expect("analyze json");
    assert!(output.status.success());
    let analyze_json = String::from_utf8_lossy(&output.stdout);
    assert!(analyze_json.contains("\"total_blocks\""));
    assert!(analyze_json.contains("\"fragmentation_ratio\""));

    // 8. Defragment
    // 8. Defragment (safe mode)
    let output = Command::new(bin_path)
        .args(["--image", &ctx.image_path, "defrag"])
        .output()
        .expect("defrag");
    assert!(output.status.success());

    // 8b. Defragment (in-place mode via JSON)
    let output = Command::new(bin_path)
        .args([
            "--image",
            &ctx.image_path,
            "--json",
            "defrag",
            "--mode",
            "inplace",
        ])
        .output()
        .expect("defrag inplace json");
    assert!(output.status.success());
    let defrag_json = String::from_utf8_lossy(&output.stdout);
    assert!(defrag_json.contains("\"ok\": true") || defrag_json.contains("\"ok\":true"));

    // 9. Fsck with --json flag
    let output = Command::new(bin_path)
        .args(["--image", &ctx.image_path, "--json", "fsck"])
        .output()
        .expect("fsck json");
    assert!(output.status.success());
    let fsck_json = String::from_utf8_lossy(&output.stdout);
    assert!(fsck_json.contains("\"is_clean\": true") || fsck_json.contains("\"is_clean\":true"));
}

#[test]
fn test_cli_encrypted_filesystem_workflow() {
    let bin_path = env!("CARGO_BIN_EXE_oifs");
    let mut ctx = TestContext::new("test_cli_encrypted");

    let pwd = "SuperSecretPassword123!";

    // 1. Create encrypted image
    let output = Command::new(bin_path)
        .args([
            "--image",
            &ctx.image_path,
            "--password",
            pwd,
            "create",
            "--size",
            "10",
            "--encrypt",
        ])
        .output()
        .expect("create encrypted");
    assert!(output.status.success());

    // 2. Put file with correct password
    let host_file = ctx.create_temp_file("secret.txt", b"Classified secret content");
    let output = Command::new(bin_path)
        .args([
            "--image",
            &ctx.image_path,
            "--password",
            pwd,
            "put",
            &host_file,
            "classified.txt",
        ])
        .output()
        .expect("put encrypted");
    assert!(output.status.success());

    // 3. Try to read without password -> must fail
    let out_file = format!("{}_out.txt", ctx.image_path);
    ctx.temp_files.push(out_file.clone());
    let output = Command::new(bin_path)
        .args([
            "--image",
            &ctx.image_path,
            "get",
            "classified.txt",
            &out_file,
        ])
        .output()
        .expect("get without pwd");
    assert!(!output.status.success());

    // 4. Try to read with wrong password -> must fail
    let output = Command::new(bin_path)
        .args([
            "--image",
            &ctx.image_path,
            "--password",
            "WrongPassword",
            "get",
            "classified.txt",
            &out_file,
        ])
        .output()
        .expect("get with wrong pwd");
    assert!(!output.status.success());

    // 5. Read with correct password -> must succeed
    let output = Command::new(bin_path)
        .args([
            "--image",
            &ctx.image_path,
            "--password",
            pwd,
            "get",
            "classified.txt",
            &out_file,
        ])
        .output()
        .expect("get with correct pwd");
    assert!(output.status.success());

    let content = fs::read(&out_file).expect("read extracted content");
    assert_eq!(content, b"Classified secret content");
}
