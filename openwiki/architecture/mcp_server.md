---
type: architecture
title: MCP Server Integration
description: How the oifs_mcp binary exposes the OIFS storage engine as Model Context Protocol (MCP) tools over stdio, providing AI agents with an isolated, single-file virtual filesystem for persistent memory.
tags: [mcp, rmcp, ai-agents, cursor, claude-desktop, sandbox, stdio, json-schema]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-22T10:29:12.696Z
sources:
  - id: openwiki-source-1d8ff572dc201d9ae2619645
    resource: repo://src/bin/oifs_mcp.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-22T10:29:12.696Z" }
---

## Responsibility and ownership

The [`oifs_mcp`](repo://src/bin/oifs_mcp.rs) binary exposes the OIFS filesystem engine to AI agents (such as Claude Desktop, Cursor, and autonomous agent loops) via the **Model Context Protocol (MCP)**.

Rather than giving AI agents unrestricted access to the host operating system filesystem, `oifs_mcp` provisions a self-contained, sandboxed `.img` container. All file operations, directory hierarchies, append logs, and storage metrics are fully confined within this single file image.

## Architecture and framework (rmcp)

`oifs_mcp` is built on top of [`rmcp`](repo://src/bin/oifs_mcp.rs#L10-L13) (the Rust MCP SDK), implementing an asynchronous JSON-RPC protocol over standard input/output (`stdio`):

<!-- openwiki: mermaid parse failed and this diagram was converted to a text fence so it does not break rendering. Fix the diagram source and restore the mermaid fence. Parser error: Parse error on line 2: ... A[AI Agent Host (Cursor / Claude Des Expecting 'SQE', 'DOUBLECIRCLEEND', 'PE', '-)', 'STADIUMEND', 'SUBROUTINEEND', 'PIPE', 'CYLINDEREND', 'DIAMOND_STOP', 'TAGEND', 'TRAPEND', 'INVTRAPEND', 'UNICODE_TEXT', 'TEXT', 'TAGSTART', got 'PS' -->
```text
flowchart TD
    A[AI Agent Host (Cursor / Claude Desktop)] -->|JSON-RPC (stdio transport)| B[rmcp Service Handler (ServerHandler)]
    B -->|ToolRouter dispatch & JsonSchema validation| C[OifsMcpServer]
    C -->|async mutex lock| D[TokioMutex<DiskManager>]
    D -->|mmap & storage calls| E[OIFS Engine (DiskManager / Inode / Bitmap)]
```

### The OifsMcpServer structure

Server state is encapsulated by [`OifsMcpServer`](repo://src/bin/oifs_mcp.rs#L27-L32):

```rust
#[derive(Clone)]
struct OifsMcpServer {
    dm: Arc<TokioMutex<DiskManager>>,
    image_path: PathBuf,
    tool_router: ToolRouter<Self>,
}
```

- **Thread and Async Safety**: Because `rmcp` operates in an asynchronous Tokio runtime while `DiskManager` performs synchronous mmap I/O, `DiskManager` is wrapped in `Arc<TokioMutex<DiskManager>>`. Each incoming MCP tool invocation acquires this mutex to ensure sequential transaction isolation.
  - **Why TokioMutex is acceptable for agent workloads**: AI agent interactions are inherently sequential from the perspective of a single conversation — an LLM emits one tool call, waits for the result, then emits the next. The mutex serializes these naturally sequential requests. There is no high-concurrency fan-out from a single agent; concurrent access would only occur if multiple agent hosts connected to the same server process, which the stdio transport prevents by design (one stdin/stdout pair per process). For workloads requiring parallelism, the IPC/daemon path ([`src/ipc.rs`](repo://src/ipc.rs)) provides a multi-threaded alternative.
- **Bootstrapping**: Constructed via [`OifsMcpServer::new`](repo://src/bin/oifs_mcp.rs#L33-L47). If the target image file does not yet exist and an `OIFS_PASSWORD` environment variable is detected, it automatically initializes an encrypted image via `DiskManager::create_encrypted`; otherwise, it calls `DiskManager::open_with_password`.
- **Defaults**: Defaults to `agent_memory.img` with a default size of 10 MB ([`DEFAULT_IMAGE`](repo://src/bin/oifs_mcp.rs#L23), [`DEFAULT_SIZE_MB`](repo://src/bin/oifs_mcp.rs#L24)), configurable via `--size <MB>` and positional CLI arguments.

## Server capabilities and initialization

The server implements [`ServerHandler`](repo://src/bin/oifs_mcp.rs#L339-L354) to announce capabilities during the MCP handshake:

```rust
#[rmcp::tool_handler]
impl ServerHandler for OifsMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "OIFS Memory Sandbox — A sandboxed inode filesystem for AI agent memory. \
                 All reads/writes are isolated inside a .img file."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
```

By enabling `.enable_tools()`, the server advertises its available tool registry and JSON schemas to connected LLMs.

## Single-image sandbox model and path resolution

The sandbox model is deliberately simple:

1. **One image per server process**: The CLI accepts a single image path (default: `agent_memory.img`). All tools operate exclusively within this image.
2. **Path resolution is always relative to the image root**: Paths like `"notes/todo.txt"` or `"."` are resolved by `DiskManager::resolve_path` ([`disk.rs`](repo://src/disk.rs#L5213)) starting from the root inode (inode 1). There is no concept of "current working directory" — every path is absolute within the sandbox.
3. **Parent directories must exist**: `write_file`, `mkdir`, and `append_file` use `resolve_parent` to locate the parent directory inode, then create the child. They do **not** auto-create intermediate directories (no `mkdir -p` semantics). The agent must create the directory hierarchy first with `mkdir` calls.
4. **Encryption is image-scoped**: If `OIFS_PASSWORD` is set at startup, the entire image is encrypted with AES-256-XTS. The same password must be supplied on every subsequent launch to open the image. The MCP server itself has no key-management logic — it simply forwards the environment variable to `DiskManager`.

## Available MCP tools

All tools are registered via the [`tool_router`](repo://src/bin/oifs_mcp.rs#L157) macro. Each tool acquires `self.dm.lock().await`, executes the operation, and returns a JSON string built with `serde_json` (never hand-rolled `format!`) to guarantee correct escaping.

### Tool response convention

- **Success**: `{"ok": true, ...}` with tool-specific fields.
- **Failure**: `{"ok": false, "error": "..."}` — a single JSON object with a human-readable error message.

### `write_file`

```json
{
  "name": "write_file",
  "description": "Write (create or overwrite) a file inside the OIFS sandbox image. Parent directories must already exist.",
  "parameters": {
    "type": "object",
    "properties": {
      "path": { "type": "string", "description": "Path inside the OIFS image (e.g. \"notes/todo.txt\")" },
      "content": { "type": "string", "description": "UTF-8 content to write" }
    },
    "required": ["path", "content"]
  }
}
```

**Implementation** ([`repo://src/bin/oifs_mcp.rs#L157-L184`](repo://src/bin/oifs_mcp.rs#L157-L184)):
- Resolves parent directory, creates file if missing, writes with `CompressionMode::Auto` (zstd compression applied automatically for compressible content).
- Returns: `{"ok": true, "inode": <u64>, "bytes": <usize>}`

### `read_file`

```json
{
  "name": "read_file",
  "description": "Read the contents of a file inside the OIFS sandbox image. Returns UTF-8 text content.",
  "parameters": {
    "type": "object",
    "properties": {
      "path": { "type": "string", "description": "Path inside the OIFS image (e.g. \"notes/todo.txt\")" }
    },
    "required": ["path"]
  }
}
```

**Implementation** ([`repo://src/bin/oifs_mcp.rs#L186-L205`](repo://src/bin/oifs_mcp.rs#L186-L205)):
- Resolves path, reads raw bytes via `DiskManager::read_data`, validates UTF-8.
- Returns: raw UTF-8 string content on success (not wrapped in JSON — the MCP protocol delivers the tool result as a text content block containing the string).
- Errors: `{"ok": false, "error": "File contains non-UTF-8 data"}` or path resolution errors.

### `list_dir` (JSONL streaming)

```json
{
  "name": "list_dir",
  "description": "List files and directories at a given path inside the OIFS sandbox image. Returns one JSON object per line (JSONL).",
  "parameters": {
    "type": "object",
    "properties": {
      "path": { "type": "string", "description": "Path inside the OIFS image (e.g. \".\" for root, \"notes\" for a subdirectory)" }
    },
    "required": ["path"]
  }
}
```

**Implementation** ([`repo://src/bin/oifs_mcp.rs#L207-L230`](repo://src/bin/oifs_mcp.rs#L207-L230)):
- Resolves directory inode, calls `list_entries_inner` to gather child entries.
- Each entry serialized as a separate JSON object: `{"name": "...", "kind": "file|dir", "size": <u64>}`.
- **Output format**: newline-delimited JSON (JSONL) — one line per entry. Empty directory returns `{"entries": 0}`.
- This streaming format allows agents to process large directories incrementally without loading the entire listing into a single JSON array.

### `mkdir`

```json
{
  "name": "mkdir",
  "description": "Create a directory inside the OIFS sandbox image. Parent directories must exist.",
  "parameters": {
    "type": "object",
    "properties": {
      "path": { "type": "string", "description": "Path of the new directory (e.g. \"notes/drafts\")" }
    },
    "required": ["path"]
  }
}
```

**Implementation** ([`repo://src/bin/oifs_mcp.rs#L232-L250`](repo://src/bin/oifs_mcp.rs#L232-L250)):
- Resolves parent, creates directory inode via `DiskManager::create_directory`.
- Returns: `{"ok": true, "inode": <u64>}`

### `delete_file`

```json
{
  "name": "delete_file",
  "description": "Delete a file from the OIFS sandbox image.",
  "parameters": {
    "type": "object",
    "properties": {
      "path": { "type": "string", "description": "Path of the file to delete (e.g. \"notes/old.txt\")" }
    },
    "required": ["path"]
  }
}
```

**Implementation** ([`repo://src/bin/oifs_mcp.rs#L252-L267`](repo://src/bin/oifs_mcp.rs#L252-L267)):
- Resolves parent, calls `DiskManager::delete_file` (which removes the directory entry and frees data blocks/inode).
- Returns: `{"ok": true}`

### `truncate_file`

```json
{
  "name": "truncate_file",
  "description": "Truncate or extend a file in the OIFS sandbox image to the specified size in bytes.",
  "parameters": {
    "type": "object",
    "properties": {
      "path": { "type": "string", "description": "Path inside the OIFS image (e.g. \"notes/todo.txt\")" },
      "size": { "type": "integer", "description": "Target file size in bytes" }
    },
    "required": ["path", "size"]
  }
}
```

**Implementation** ([`repo://src/bin/oifs_mcp.rs#L269-L286`](repo://src/bin/oifs_mcp.rs#L269-L286)):
- Calls `DiskManager::truncate_path` which adjusts file size (allocating zero-filled blocks if extending, freeing blocks if shrinking).
- Returns: `{"ok": true, "size": <u64>}`

### `append_file` (uncompressed JSONL logs)

```json
{
  "name": "append_file",
  "description": "Append a line to a file inside the OIFS sandbox image. Creates the file if it does not exist. Ideal for JSONL memory logs.",
  "parameters": {
    "type": "object",
    "properties": {
      "path": { "type": "string", "description": "Path inside the OIFS image (e.g. \"logs/memory.jsonl\")" },
      "content": { "type": "string", "description": "UTF-8 content to append (a newline is auto-added if missing)" }
    },
    "required": ["path", "content"]
  }
}
```

**Implementation** ([`repo://src/bin/oifs_mcp.rs#L288-L315`](repo://src/bin/oifs_mcp.rs#L288-L315)):
- Resolves parent, creates file if missing.
- Reads existing content, appends new content, ensures trailing newline.
- Writes with `CompressionMode::Never` — **explicitly uncompressed** to keep log files seekable and line-oriented for JSONL processing.
- Returns: `{"ok": true, "inode": <u64>, "total_bytes": <usize>}`

### `status`

```json
{
  "name": "status",
  "description": "Show filesystem status: image path, total/used/free blocks, and fragmentation ratio.",
  "parameters": { "type": "object", "properties": {} }
}
```

**Implementation** ([`repo://src/bin/oifs_mcp.rs#L317-L337`](repo://src/bin/oifs_mcp.rs#L317-L337)):
- Calls `DiskManager::analyze_fragmentation` ([`disk.rs`](repo://src/disk.rs#L137-L162)).
- Returns:
```json
{
  "ok": true,
  "image": "<path>",
  "total_blocks": <u64>,
  "used_blocks": <u64>,
  "free_blocks": <u64>,
  "fragmentation": <f64>
}
```
- The `fragmentation` ratio is `1.0 - (largest_free_run / free_blocks)`, indicating how scattered free space is.

## JSON error responses and escaping

All tool responses are constructed via the internal `json_object` helper ([`repo://src/bin/oifs_mcp.rs#L115-L130`](repo://src/bin/oifs_mcp.rs#L115-L130)) which uses `serde_json::Map` and `to_string()`. This guarantees:

- File names containing quotes, backslashes, or control characters cannot inject malformed JSON.
- Error messages from the engine are safely embedded.
- The test `test_mcp_tool_response_escapes_hostile_names` ([`repo://tests/mcp_server_test.rs#L101-L167`](repo://tests/mcp_server_test.rs#L101-L167)) validates this by writing a file with a hostile name (`inject\"},\"evil\":1,\"x.json`) and verifying every response line parses as valid JSON.

## CLI entry point

The `main` function ([`repo://src/bin/oifs_mcp.rs#L356-L421`](repo://src/bin/oifs_mcp.rs#L356-L421)):

1. Parses `--size <MB>` and positional `IMAGE_PATH`.
2. Reads `OIFS_PASSWORD` from environment.
3. Computes `size_bytes` with checked multiplication to prevent overflow.
4. Constructs `OifsMcpServer::new`.
5. Serves over `rmcp::transport::stdio()` and awaits `service.waiting()`.

Logging to stderr: `[oifs-mcp] image=<path> size=<MB> encrypted=<bool>`.

## Feature-gated compilation

The MCP server is gated behind the `mcp` feature flag in `Cargo.toml` ([`repo://Cargo.toml#L42-L45`](repo://Cargo.toml#L42-L45)):

```toml
# Optional: MCP server stack. Only pulled in when the `mcp` feature is on,
# which keeps the CLI binary buildable on older rustc toolchains.
rmcp = { version = "0.16", features = ["server", "macros", "transport-async-rw", "transport-io"], optional = true }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "io-std"], optional = true }
schemars = { version = "1", optional = true }

[features]
default = ["mcp", "io_uring"]
mcp = ["dep:rmcp", "dep:tokio", "dep:schemars"]
```

```toml
[[bin]]
name = "oifs_mcp"
path = "src/bin/oifs_mcp.rs"
required-features = ["mcp"]
```

This design keeps the core `oifs` CLI binary lightweight and buildable without pulling in the async MCP runtime dependencies.

## Testing

Integration tests in [`tests/mcp_server_test.rs`](repo://tests/mcp_server_test.rs) spawn the binary, speak JSON-RPC over stdio, and verify:

- `test_mcp_cli_help_and_arguments`: CLI help and argument validation.
- `test_mcp_stdio_initialize_and_tools_list`: MCP handshake (`initialize` + `notifications/initialized`) and `tools/list` response.
- `test_mcp_tool_response_escapes_hostile_names`: JSON escaping correctness for tool calls and results.

Run with `cargo test --test mcp_server_test`.

## Related pages

- [MCP Integration Guide](../integrations/mcp_integration.md) — configuring Claude Desktop, Cursor, and other agents.
- [MCP Server Workflow](../workflows/mcp_workflow.md) — step-by-step usage patterns for agent memory.
- [FFI Interface](../architecture/ffi_interface.md) — the C API alternative for non-MCP embeddings.
