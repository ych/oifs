use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

struct TestContext {
    image_path: String,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let image_path = format!("{}.img", name);
        if Path::new(&image_path).exists() {
            let _ = fs::remove_file(&image_path);
        }
        Self { image_path }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        if Path::new(&self.image_path).exists() {
            let _ = fs::remove_file(&self.image_path);
        }
    }
}

#[test]
fn test_mcp_cli_help_and_arguments() {
    let bin_path = env!("CARGO_BIN_EXE_oifs_mcp");

    // 1. --help output
    let output = Command::new(bin_path)
        .arg("--help")
        .output()
        .expect("oifs_mcp --help");
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Usage: oifs_mcp"));
    assert!(stderr.contains("--size"));

    // 2. Invalid --size value
    let output = Command::new(bin_path)
        .args(["--size", "not_a_number"])
        .output()
        .expect("oifs_mcp invalid size");
    assert!(!output.status.success());
}

#[test]
fn test_mcp_stdio_initialize_and_tools_list() {
    let bin_path = env!("CARGO_BIN_EXE_oifs_mcp");
    let ctx = TestContext::new("test_mcp_jsonrpc");

    let mut child = Command::new(bin_path)
        .args(["--size", "10", &ctx.image_path])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oifs_mcp");

    let mut stdin = child.stdin.take().expect("child stdin");

    // Send standard MCP initialize request
    let init_req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test-client","version":"1.0.0"}}}"#;
    let _ = writeln!(stdin, "{}", init_req);
    let _ = stdin.flush();

    // Send initialized notification
    let init_notif = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let _ = writeln!(stdin, "{}", init_notif);
    let _ = stdin.flush();

    // Send tools/list request
    let tools_req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    let _ = writeln!(stdin, "{}", tools_req);
    let _ = stdin.flush();

    std::thread::sleep(std::time::Duration::from_millis(200));

    // Drop stdin to close stream
    drop(stdin);

    let output = child.wait_with_output().expect("wait for child");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify MCP initialize response and capabilities
    assert!(
        stdout.contains("\"jsonrpc\":\"2.0\""),
        "MCP stdout must contain valid jsonrpc response: {}",
        stdout
    );
    assert!(
        stdout.contains("OIFS Memory Sandbox"),
        "MCP stdout must contain server instructions: {}",
        stdout
    );
    assert!(
        stdout.contains("\"tools\""),
        "MCP capabilities must include tools: {}",
        stdout
    );
}
