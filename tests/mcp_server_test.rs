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
    assert!(stderr.contains("--journal"));
    assert!(stderr.contains("--no-mmap"));
    assert!(stderr.contains("--read-only"));
    assert!(stderr.contains("--auto-grow"));
    assert!(stderr.contains("--dict"));

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

/// The tool responses must be valid JSON even when a file name or an
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

fn parse_mcp_tool_text_json(resp_line: &str) -> serde_json::Value {
    let outer: serde_json::Value =
        serde_json::from_str(resp_line).unwrap_or_else(|e| panic!("invalid JSON-RPC: {resp_line} ({e})"));
    let text = outer["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("missing result.content[0].text in: {resp_line}"));
    serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("tool text is not valid JSON: {text} ({e})"))
}

fn send_mcp_requests(args: &[&str], requests: &[serde_json::Value]) -> Vec<serde_json::Value> {
    use std::io::{BufRead, BufReader};

    let bin_path = env!("CARGO_BIN_EXE_oifs_mcp");
    let mut child = Command::new(bin_path)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oifs_mcp");

    let mut stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);

    let read_next_json = |reader: &mut BufReader<std::process::ChildStdout>| -> serde_json::Value {
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).expect("read stdout line");
            assert!(n > 0, "oifs_mcp closed stdout unexpectedly");
            if !line.trim().is_empty() {
                return serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("invalid JSON line: {line} ({e})"));
            }
        }
    };

    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test-client", "version": "1.0.0"}
        }
    });
    writeln!(stdin, "{init_req}").unwrap();
    stdin.flush().unwrap();
    let mut lines = vec![read_next_json(&mut reader)];

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    for req in requests {
        writeln!(stdin, "{req}").unwrap();
        stdin.flush().unwrap();
        lines.push(read_next_json(&mut reader));
    }
    drop(stdin);

    let status = child.wait().expect("wait oifs_mcp");
    assert!(status.success(), "oifs_mcp exited with {status}");
    lines
}

#[test]
fn test_mcp_v161_tools_batch_resize_vacuum_dict_and_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let img_path = dir.path().join("mcp_v161.img");
    let img_str = img_path.to_str().unwrap();

    // Seed 12 corpus JSON files inside a batch + exercise rename, stat, resize, auto_grow,
    // train_zstd_dict, load_zstd_dict, set_default_zstd_dict, vacuum, verify_integrity, status.
    let mut batch_ops = vec![
        serde_json::json!({"op": "mkdir", "path": "extra_dir"}),
        serde_json::json!({
            "op": "create_file",
            "path": "tmp/draft.txt",
            "content": "hello batch mcp",
            "compression": "raw"
        }),
        serde_json::json!({"op": "rename", "old_path": "tmp/draft.txt", "new_path": "corpus/final.txt"}),
        serde_json::json!({
            "op": "create_file",
            "path": "tmp/remove.txt",
            "content": "gone",
            "compression": "seekable"
        }),
        serde_json::json!({"op": "delete", "path": "tmp/remove.txt"}),
    ];
    for i in 0..10 {
        batch_ops.push(serde_json::json!({
            "op": "create_file",
            "path": format!("corpus/rec_{i}.json"),
            "content": format!(r#"{{"svc":"mcp","id":{i},"region":"ap-east-1","status":"ok"}}"#),
            "compression": "raw"
        }));
    }

    let reqs = vec![
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        serde_json::json!({"jsonrpc":"2.0","id":100,"method":"tools/call","params":{"name":"write_batch","arguments":{"ops":[
            {"op":"mkdir","path":"corpus"},
            {"op":"mkdir","path":"tmp"}
        ]}}}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"write_batch","arguments":{"ops": batch_ops}}}),
        // Atomic rollback check
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"write_batch","arguments":{"ops":[
            {"op":"create_file","path":"never.txt","content":"no"},
            {"op":"delete","path":"non_existent.txt"}
        ]}}}),
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"rename_path","arguments":{"old_path":"corpus/final.txt","new_path":"final_moved.txt"}}}),
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"stat_path","arguments":{"path":"final_moved.txt"}}}),
        serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"configure_auto_grow","arguments":{"enabled":true,"max_size_bytes":16777216}}}),
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"resize_image","arguments":{"size_bytes":8388608,"grow_only":true}}}),
        serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"train_zstd_dict","arguments":{"dir_path":"corpus","save_path":"trained.dict","max_dict_bytes":4096,"max_samples":32,"activate":true}}}),
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"set_default_zstd_dict","arguments":{"dict_id":null}}}),
        serde_json::json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"load_zstd_dict","arguments":{"path":"trained.dict","activate":true}}}),
        serde_json::json!({"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"vacuum_image","arguments":{"min_headroom_bytes":524288,"headroom_percent":10,"min_image_bytes":2097152}}}),
        serde_json::json!({"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"verify_integrity","arguments":{}}}),
        serde_json::json!({"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"status","arguments":{}}}),
    ];

    let responses = send_mcp_requests(&["--size", "4", "--journal", "--no-mmap", img_str], &reqs);
    let by_id = |target_id: i64| -> serde_json::Value {
        responses
            .iter()
            .find(|v| v["id"].as_i64() == Some(target_id))
            .unwrap_or_else(|| panic!("missing response for id={target_id}: {responses:?}"))
            .clone()
    };

    let list_json = by_id(2).to_string();
    for expected_tool in [
        "rename_path",
        "stat_path",
        "write_batch",
        "resize_image",
        "vacuum_image",
        "configure_auto_grow",
        "train_zstd_dict",
        "load_zstd_dict",
        "set_default_zstd_dict",
        "verify_integrity",
    ] {
        assert!(
            list_json.contains(expected_tool),
            "tools/list missing {expected_tool}: {list_json}"
        );
    }

    let r3 = parse_mcp_tool_text_json(&by_id(3).to_string());
    assert_eq!(r3["ok"], true, "r3={r3}");
    assert_eq!(r3["ops_executed"], 15);

    let r4 = parse_mcp_tool_text_json(&by_id(4).to_string());
    assert_eq!(r4["ok"], false, "rollback batch must fail: {r4}");

    let r5 = parse_mcp_tool_text_json(&by_id(5).to_string());
    assert_eq!(r5["ok"], true, "r5={r5}");

    let r6 = parse_mcp_tool_text_json(&by_id(6).to_string());
    assert_eq!(r6["ok"], true, "r6={r6}");
    assert_eq!(r6["kind"], "file");
    assert_eq!(r6["size"], "hello batch mcp".len());

    let r7 = parse_mcp_tool_text_json(&by_id(7).to_string());
    assert_eq!(r7["enabled"], true, "r7={r7}");

    let r8 = parse_mcp_tool_text_json(&by_id(8).to_string());
    assert_eq!(r8["bytes_after"], 8_388_608, "r8={r8}");

    let r9 = parse_mcp_tool_text_json(&by_id(9).to_string());
    assert_eq!(r9["ok"], true, "r9={r9}");
    let dict_id = r9["dict_id"].as_u64().unwrap();
    assert!(dict_id > 0);
    assert_eq!(r9["activated"], true);
    assert_eq!(r9["saved_path"], "trained.dict");

    let r10 = parse_mcp_tool_text_json(&by_id(10).to_string());
    assert_eq!(r10["ok"], true);
    assert!(r10["default_zstd_dict_id"].is_null());

    let r11 = parse_mcp_tool_text_json(&by_id(11).to_string());
    assert_eq!(r11["ok"], true);
    assert_eq!(r11["dict_id"], dict_id);

    let r12 = parse_mcp_tool_text_json(&by_id(12).to_string());
    assert_eq!(r12["ok"], true, "r12={r12}");

    let r13 = parse_mcp_tool_text_json(&by_id(13).to_string());
    assert_eq!(r13["is_clean"], true, "r13={r13}");
    assert_eq!(r13["corrupted_inodes"], 0);

    let r14 = parse_mcp_tool_text_json(&by_id(14).to_string());
    assert_eq!(r14["read_only"], false);
    assert_eq!(r14["auto_grow_enabled"], true);
    assert_eq!(r14["default_zstd_dict_id"], dict_id);

    // Reopen strictly read-only with --auto-grow must fail cleanly at CLI validation
    let ro_ag_out = Command::new(env!("CARGO_BIN_EXE_oifs_mcp"))
        .args(["--read-only", "--auto-grow", img_str])
        .output()
        .expect("oifs_mcp --read-only --auto-grow");
    assert!(!ro_ag_out.status.success());

    // Read-only MCP session: read/inspect succeeds, mutations return {"ok":false,...}
    let ro_reqs = vec![
        serde_json::json!({"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"status","arguments":{}}}),
        serde_json::json!({"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"final_moved.txt"}}}),
        serde_json::json!({"jsonrpc":"2.0","id":22,"method":"tools/call","params":{"name":"configure_auto_grow","arguments":{"enabled":true}}}),
        serde_json::json!({"jsonrpc":"2.0","id":23,"method":"tools/call","params":{"name":"write_batch","arguments":{"ops":[{"op":"mkdir","path":"ro_fail"}]}}}),
    ];
    let ro_resp = send_mcp_requests(&["--read-only", img_str], &ro_reqs);
    let ro_by_id = |target_id: i64| -> serde_json::Value {
        ro_resp
            .iter()
            .find(|v| v["id"].as_i64() == Some(target_id))
            .unwrap_or_else(|| panic!("missing RO response id={target_id}: {ro_resp:?}"))
            .clone()
    };
    let r20 = parse_mcp_tool_text_json(&ro_by_id(20).to_string());
    assert_eq!(r20["read_only"], true);
    assert_eq!(
        ro_by_id(21)["result"]["content"][0]["text"],
        "hello batch mcp"
    );
    assert_eq!(parse_mcp_tool_text_json(&ro_by_id(22).to_string())["ok"], false);
    assert_eq!(parse_mcp_tool_text_json(&ro_by_id(23).to_string())["ok"], false);
}
