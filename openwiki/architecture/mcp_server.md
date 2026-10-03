---
type: architecture
title: MCP Server Integration
description: How the oifs_mcp binary exposes the OIFS storage engine as Model Context Protocol (MCP) tools over stdio, providing AI agents with an isolated, single-file virtual filesystem for persistent memory.
tags: [mcp, rmcp, ai-agents, cursor, claude-desktop, sandbox, stdio, json-schema]
sources:
  - id: openwiki-source-1d8ff572dc201d9ae2619645
    resource: repo://src/bin/oifs_mcp.rs
generated: { by: "antigravity", at: "2026-10-03T11:29:24.571Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-03T08:18:49.684Z
---

## Responsibility and ownership

<!-- openwiki: broken internal link [src/bin/oifs_mcp.rs] file "src/bin/oifs_mcp.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The [`oifs_mcp`](src/bin/oifs_mcp.rs) binary exposes the OIFS filesystem engine to AI agents (such as Claude Desktop, Cursor, and autonomous agent loops) via the **Model Context Protocol (MCP)**.

Rather than giving AI agents unrestricted access to the host operating system filesystem, `oifs_mcp` provisions a self-contained, sandboxed `.img` container. All file operations, directory hierarchies, append logs, and storage metrics are fully confined within this single file image.

## Architecture and framework (rmcp)

<!-- openwiki: broken internal link [src/bin/oifs_mcp.rs#L10-L13] file "src/bin/oifs_mcp.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
`oifs_mcp` is built on top of [`rmcp`](src/bin/oifs_mcp.rs#L10-L13) (the Rust MCP SDK), implementing an asynchronous JSON-RPC protocol over standard input/output (`stdio`):

```
┌──────────────────────────────────────────────┐
│  AI Agent Host (Cursor / Claude Desktop)      │
└──────────────────────┬───────────────────────┘
                       │ JSON-RPC (stdio transport)
                       ▼
┌──────────────────────────────────────────────┐
│  rmcp Service Handler (ServerHandler)        │
│  - ToolRouter dispatch & JsonSchema validation│
└──────────────────────┬───────────────────────┘
                       │ async mutex lock
                       ▼
┌──────────────────────────────────────────────┐
│  OifsMcpServer (src/bin/oifs_mcp.rs)         │
│  - TokioMutex<DiskManager>                   │
│  - Isolated container (agent_memory.img)     │
└──────────────────────┬───────────────────────┘
                       │ mmap & storage calls
                       ▼
┌──────────────────────────────────────────────┐
│  OIFS Engine (DiskManager / Inode / Bitmap)  │
└──────────────────────────────────────────────┘
```

### The OifsMcpServer structure

<!-- openwiki: broken internal link [src/bin/oifs_mcp.rs#L27-L32] file "src/bin/oifs_mcp.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Server state is encapsulated by [`OifsMcpServer`](src/bin/oifs_mcp.rs#L27-L32):

```rust
#[derive(Clone)]
struct OifsMcpServer {
    dm: Arc<TokioMutex<DiskManager>>,
    image_path: PathBuf,
    tool_router: ToolRouter<Self>,
}
```

- **Thread and Async Safety**: Because `rmcp` operates in an asynchronous Tokio runtime while `DiskManager` performs synchronous mmap I/O, `DiskManager` is wrapped in `Arc<TokioMutex<DiskManager>>`. Each incoming MCP tool invocation acquires this mutex to ensure sequential transaction isolation.
<!-- openwiki: broken internal link [src/bin/oifs_mcp.rs#L35-L47] file "src/bin/oifs_mcp.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **Bootstrapping**: Constructed via [`OifsMcpServer::new`](src/bin/oifs_mcp.rs#L35-L47). If the target image file does not yet exist and an `OIFS_PASSWORD` environment variable is detected, it automatically initializes an encrypted image via `DiskManager::create_encrypted`; otherwise, it calls `DiskManager::open_with_password`.
- **Defaults**: Defaults to `agent_memory.img` with a default size of 10 MB (`src/bin/oifs_mcp.rs#L23-L24`), configurable via `--size <MB>` and positional CLI arguments.

## Server capabilities and initialization

<!-- openwiki: broken internal link [src/bin/oifs_mcp.rs#L259-L272] file "src/bin/oifs_mcp.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The server implements [`ServerHandler`](src/bin/oifs_mcp.rs#L259-L272) to announce capabilities during the MCP handshake:

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

## Available MCP tools

<!-- openwiki: broken internal link [src/bin/oifs_mcp.rs#L134-L255] file "src/bin/oifs_mcp.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/bin/oifs_mcp.rs#L14] file "src/bin/oifs_mcp.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The server registers 7 specialized tools via the `#[tool_router]` macro ([`src/bin/oifs_mcp.rs#L134-L255`](src/bin/oifs_mcp.rs#L134-L255)). Input parameters derive [`schemars::JsonSchema`](src/bin/oifs_mcp.rs#L14) to emit standard OpenAPI/JSON-Schema definitions.

### 1. `write_file`
- **Description**: `"Write (create or overwrite) a file inside the OIFS sandbox image. Parent directories must already exist."`
- **Parameters**:
  - `path: String` (e.g. `"notes/todo.txt"`)
  - `content: String` (UTF-8 payload)
- **Engine Call**: Resolves parent via `dm.resolve_parent`, looks up or creates the inode, and writes data with `CompressionMode::Auto`.
- **Response**: `{"ok":true,"inode":1,"bytes":128}` or `{"ok":false,"error":"..."}`.

### 2. `read_file`
- **Description**: `"Read the contents of a file inside the OIFS sandbox image. Returns UTF-8 text content."`
- **Parameters**:
  - `path: String`
- **Engine Call**: Resolves path via `dm.resolve_path`, calls `dm.read_data(inode_id)`, and converts raw bytes to a UTF-8 string.
- **Response**: Plain text file content on success, or JSON error `{"ok":false,"error":"..."}`.

### 3. `list_dir`
- **Description**: `"List files and directories at a given path inside the OIFS sandbox image. Returns one JSON object per line (JSONL)."`
- **Parameters**:
  - `path: String` (e.g. `"."` or `"notes"`)
<!-- openwiki: broken internal link [src/directory.rs] file "src/directory.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **Engine Call**: Resolves target directory inode, reads block data via `dm.get_block_copy`, and iterates entries via [`DirectoryIterator`](src/directory.rs).
- **Response**: Formatted as newline-delimited JSON (JSONL):
  ```json
  {"name":"todo.txt","kind":"file","size":420}
  {"name":"drafts","kind":"dir","size":4096}
  ```
  Returns `{"entries":0}` if the directory is empty.

### 4. `append_file`
- **Description**: `"Append a line to a file inside the OIFS sandbox image. Creates the file if it does not exist. Ideal for JSONL memory logs."`
- **Parameters**:
  - `path: String` (e.g. `"logs/memory.jsonl"`)
  - `content: String`
- **Engine Call**: Looks up or creates file, appends content (automatically adding a trailing newline if missing), and persists using `CompressionMode::Never` for fast sequential logging.
- **Response**: `{"ok":true,"inode":2,"total_bytes":850}`.

### 5. `mkdir`
- **Description**: `"Create a directory inside the OIFS sandbox image. Parent directories must exist."`
- **Parameters**:
  - `path: String`
- **Engine Call**: Resolves parent and calls `dm.create_directory(parent_id, &name)`.
- **Response**: `{"ok":true,"inode":3}`.

### 6. `delete_file`
- **Description**: `"Delete a file from the OIFS sandbox image."`
- **Parameters**:
  - `path: String`
- **Engine Call**: Resolves parent and calls `dm.delete_file(parent_id, &name)`.
- **Response**: `{"ok":true}`.

### 7. `status`
- **Description**: `"Show filesystem status: image path, total/used/free blocks, and fragmentation ratio."`
- **Parameters**: None.
- **Engine Call**: Invokes `dm.analyze_fragmentation()`.
- **Response**: JSON status report:
  ```json
  {"image":"agent_memory.img","total_blocks":2560,"used_blocks":12,"free_blocks":2548,"fragmentation":0.000}
  ```

## CLI execution and environment configuration

The server binary is executed from the terminal or configured directly within agent desktop environments:

```bash
cargo run --bin oifs_mcp -- [OPTIONS] [IMAGE_PATH]
```

- `--size <MB>`: Allocates initial disk image size when creating a new file (default: 10 MB).
- `OIFS_PASSWORD`: Environment variable used to open or initialize password-encrypted containers.

### Example client configuration (Cursor / Claude Desktop)

In `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "oifs": {
      "command": "/path/to/oifs/target/release/oifs_mcp",
      "args": ["--size", "50", "/path/to/agent_sandbox.img"],
      "env": {
        "OIFS_PASSWORD": "secret-agent-key"
      }
    }
  }
}
```
