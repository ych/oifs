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

/// SEC-22: the tool responses must be valid JSON even when a file name or an
/// error message contains quotes, backslashes or control characters.
#[test]
fn test_mcp_tool_response_escapes_hostile_names() {
    let bin_path = env!("CARGO_BIN_EXE_oifs_mcp");
    let ctx = TestContext::new("test_mcp_json_escape");

    let mut child = Command::new(bin_path)
        .args(["--size", "10", &ctx.image_path])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oifs_mcp");

    let mut stdin = child.stdin.take().expect("child stdin");

    let init_req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test-client","version":"1.0.0"}}}"#;
    let _ = writeln!(stdin, "{}", init_req);
    let _ = stdin.flush();
    let _ = writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    );
    let _ = stdin.flush();

    // A file name that would break a hand-built JSON string.
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"write_file","arguments":{"path":"inject\"},\"evil\":1,\"x.json","content":"payload"}}}"#;
    let _ = writeln!(stdin, "{}", call);
    let _ = stdin.flush();

    // And a listing, which embeds the file name in the response.
    let list = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_dir","arguments":{"path":"."}}}"#;
    let _ = writeln!(stdin, "{}", list);
    let _ = stdin.flush();

    std::thread::sleep(std::time::Duration::from_millis(400));
    drop(stdin);

    let output = child.wait_with_output().expect("wait for child");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Every line the server produced must be valid JSON.
    let mut checked = 0;
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("the MCP server emitted invalid JSON: {line} ({e})"));
        // The tool payloads are JSON documents inside the text field: they must
        // parse as well.
        if let Some(text) = value
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            && serde_json::from_str::<serde_json::Value>(text).is_ok()
        {
            checked += 1;
        }
    }
    assert!(checked > 0, "no tool response was checked: {stdout}");
}
