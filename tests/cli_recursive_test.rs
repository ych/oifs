use std::process::Command;
use std::path::Path;
use std::fs;

#[test]
fn test_cli_recursive_and_limits() {
    let image_path = "test_cli_rec.img";
    let host_file = "test_rec.txt";
    
    if Path::new(image_path).exists() { fs::remove_file(image_path).unwrap(); }
    if Path::new(host_file).exists() { fs::remove_file(host_file).unwrap(); }

    // 1. Create Image
    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "create", "--size", "10"])
        .status().expect("Cmd failed");
    assert!(status.success());

    // 2. Create hierarchy
    Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "mkdir", "a"])
        .status().unwrap();
    Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "mkdir", "a/b"])
        .status().unwrap();
    
    fs::write(host_file, "content").unwrap();
    Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "put", host_file, "a/b/file.txt"])
        .status().unwrap();

    // 3. Test Recursive LS
    let output = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "ls", "-r"])
        .output().expect("Cmd failed");
    let stdout = String::from_utf8(output.stdout).unwrap();
    println!("Recursive LS Output:\n{}", stdout);

    assert!(stdout.contains("a/b/file.txt"));
    assert!(stdout.contains("a/b"));

    // 4. Test Filename Limit
    let long_name = "a".repeat(256);
    let output_err = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "put", host_file, &long_name])
        .output().expect("Cmd failed");
    
    let stdout_err = String::from_utf8(output_err.stdout).unwrap();
    // let stderr_err = String::from_utf8(output_err.stderr).unwrap();
    
    assert!(!output_err.status.success() || !stdout_err.contains("Imported"));
    
    // We expect it to FAIL to import.
    assert!(!stdout_err.contains("Imported"));
    // Or check stderr for specific error if we returned one, or just that it didn't succeed.
    
    fs::remove_file(image_path).unwrap();
    fs::remove_file(host_file).unwrap();
}
