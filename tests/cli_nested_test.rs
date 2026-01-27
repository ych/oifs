use std::process::Command;
use std::path::Path;
use std::fs;

#[test]
fn test_cli_nested_flow() {
    let image_path = "test_cli_nested.img";
    let host_file = "test_nested.txt";
    let extracted_file = "extracted_nested.txt";

    if Path::new(image_path).exists() { fs::remove_file(image_path).unwrap(); }
    if Path::new(host_file).exists() { fs::remove_file(host_file).unwrap(); }
    if Path::new(extracted_file).exists() { fs::remove_file(extracted_file).unwrap(); }

    // 1. Create Image
    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "create", "--size", "10"])
        .status().expect("Cmd failed");
    assert!(status.success());

    // 2. Mkdir nested (one by one)
    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "mkdir", "a"])
        .status().expect("Cmd failed");
    assert!(status.success());

    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "mkdir", "a/b"])
        .status().expect("Cmd failed");
    assert!(status.success());
    
    // 3. Put file into nested
    let content = "Hello Nested World!";
    fs::write(host_file, content).unwrap();

    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "put", host_file, "a/b/file.txt"])
        .status().expect("Cmd failed");
    assert!(status.success());

    // 4. Ls nested
    let output = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "ls", "a/b"])
        .output().expect("Cmd failed");
    let stdout = String::from_utf8(output.stdout).unwrap();
    println!("{}", stdout);
    assert!(stdout.contains("file.txt"));

    // 5. Get file from nested
    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "get", "a/b/file.txt", extracted_file])
        .status().expect("Cmd failed");
    assert!(status.success());
    
    let extracted_content = fs::read_to_string(extracted_file).unwrap();
    assert_eq!(content, extracted_content);

    fs::remove_file(image_path).unwrap();
    fs::remove_file(host_file).unwrap();
    fs::remove_file(extracted_file).unwrap();
}
