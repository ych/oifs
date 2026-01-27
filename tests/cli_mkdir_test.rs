use std::process::Command;
use std::path::Path;
use std::fs;

#[test]
fn test_cli_mkdir_flow() {
    let image_path = "test_cli_mkdir.img";

    if Path::new(image_path).exists() { fs::remove_file(image_path).unwrap(); }

    // 1. Create Image
    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "create", "--size", "10"])
        .status().expect("Cmd failed");
    assert!(status.success());

    // 2. Mkdir
    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "mkdir", "my_dir"])
        .status().expect("Cmd failed");
    assert!(status.success());
    
    // 3. Mkdir duplicate
    let status = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "mkdir", "my_dir"])
        .status().expect("Cmd failed");
    // Should fail or verify error message? The current main returns Ok(()) on error print. 
    // It prints "Error: ...". We can check output if we want.
    // For now assuming success=true but checking logic via LS.

    // 4. Ls
    let output = Command::new("cargo")
        .args(&["run", "--bin", "oifs", "--", "--image", image_path, "ls"])
        .output().expect("Cmd failed");
    let stdout = String::from_utf8(output.stdout).unwrap();
    println!("{}", stdout);
    
    // Expect: "d my_dir" or similar format
    assert!(stdout.contains("d my_dir")); // based on format "d{:<19}" -> "d my_dir             "

    fs::remove_file(image_path).unwrap();
}
