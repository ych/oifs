use std::process::Command;
use std::path::Path;
use std::fs;

#[test]
fn test_cli_extended_flow() {
    let image_path = "test_cli_ext.img";
    let host_file = "test_put.txt";
    let extracted_file = "extracted.txt";

    // Clean up
    if Path::new(image_path).exists() { fs::remove_file(image_path).unwrap(); }
    if Path::new(host_file).exists() { fs::remove_file(host_file).unwrap(); }
    if Path::new(extracted_file).exists() { fs::remove_file(extracted_file).unwrap(); }

    // 1. Create Image
    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "create", "--size", "10"])
        .status().expect("Cmd failed");
    assert!(status.success());
    assert!(Path::new(image_path).exists());

    // 2. Create host file
    let content = "Hello OIFS World!";
    fs::write(host_file, content).unwrap();

    // 3. Put file
    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "put", host_file])
        .status().expect("Cmd failed");
    assert!(status.success());

    // Verify LS
    let output = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "ls"])
        .output().expect("Cmd failed");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("test_put.txt"));

    // 4. Get file
    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "get", "test_put.txt", extracted_file])
        .status().expect("Cmd failed");
    assert!(status.success());
    
    // Verify content
    let extracted_content = fs::read_to_string(extracted_file).unwrap();
    assert_eq!(content, extracted_content);

    // Clean up
    fs::remove_file(image_path).unwrap();
    fs::remove_file(host_file).unwrap();
    fs::remove_file(extracted_file).unwrap();
}
