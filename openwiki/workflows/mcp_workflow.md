---
type: workflow
title: MCP Server Workflow
description: A step-by-step guide to using the OIFS MCP server for AI agent integration, including setup, tool usage, and common operations.
tags: [mcp, workflow, ai-agents, claude-desktop, cursor, tools]
sources:
  - id: openwiki-source-1d8ff572dc201d9ae2619645
    resource: repo://src/bin/oifs_mcp.rs
  - id: openwiki-source-b0f9117e3c34930435de9b17
    resource: repo://tests/mcp_server_test.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-09T15:04:29.410Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-09T15:04:29.410Z
---

# MCP Server Workflow

The OIFS MCP server (`oifs_mcp`) exposes the OIFS filesystem as a set of tools via the Model Context Protocol (MCP) over standard input/output (stdio). This allows AI agents (such as Claude Desktop, Cursor, or custom agent loops) to interact with an isolated, sandboxed filesystem image (`.img`) for persistent memory and file operations.

## Prerequisites

1. **Build with MCP support**: Ensure the `mcp` feature is enabled when building OIFS.
   ```bash
   cargo build --release --features mcp
   ```
   This produces the `target/release/oifs_mcp` binary.

2. **Choose an image path**: Decide on a filesystem image file (e.g., `agent_memory.img`). The server will create it if it doesn't exist.

3. **Optional encryption**: Set the `OIFS_PASSWORD` environment variable to use an encrypted image.

## Starting the Server

Launch the MCP server in a terminal. The server communicates via stdio, so it is intended to be launched by an AI agent host (like Claude Desktop) or manually for testing.

```bash
# Start with default image (agent_memory.img) and size (10 MB)
./target/release/oifs_mcp

# Custom image and size
./target/release/oifs_mcp --size 20 custom_agent.img

# Encrypted image (requires OIFS_PASSWORD set in environment)
OIFS_PASSWORD=secret ./target/release/oifs_mcp encrypted.img
```

Upon startup, the server logs the image path, size, and encryption status to stderr.

## Connecting an AI Agent

AI agent hosts (Claude Desktop, Cursor, etc.) launch `oifs_mcp` as a subprocess and communicate via JSON-RPC over stdio. The agent host is responsible for:

1. Spawning the `oifs_mcp` process with appropriate arguments.
2. Sending an MCP `initialize` request.
3. Sending an `initialized` notification.
4. Subsequently sending tool invocations (`tools/call`) and receiving responses.

See the [MCP server test](repo://tests/mcp_server_test.rs) for a minimal example of this interaction.

## Available Tools

The server exposes eight tools, each with JSON-Schema validated parameters. Tools return a JSON string indicating success (`{ "ok": true, ... }`) or failure (`{ "ok": false, "error": "..." }`).

### 1. `write_file`
Write (create or overwrite) a file. Parent directories must exist.

**Parameters**:
- `path`: File path inside the image (e.g., `"notes/todo.txt"`)
- `content`: UTF-8 content to write

**Example**:
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "write_file",
    "arguments": { "path": "hello.txt", "content": "Hello, OIFS!" }
  }
}
```

### 2. `read_file`
Read a file's contents as UTF-8 text.

**Parameters**:
- `path`: File path inside the image

**Example**:
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "tools/call",
  "params": {
    "name": "read_file",
    "arguments": { "path": "hello.txt" }
  }
}
```

### 3. `list_dir`
List files and directories at a given path. Returns one JSON object per line (JSONL).

**Parameters**:
- `path`: Directory path (e.g., `"."` for root, `"notes"` for a subdirectory)

**Example**:
```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "tools/call",
  "params": {
    "name": "list_dir",
    "arguments": { "path": "." }
  }
}
```

### 4. `mkdir`
Create a directory. Parent directories must exist.

**Parameters**:
- `path`: Path of the new directory (e.g., `"notes/drafts"`)

### 5. `delete_file`
Delete a file.

**Parameters**:
- `path`: Path of the file to delete

### 6. `truncate_file`
Truncate or extend a file to the specified size in bytes.

**Parameters**:
- `path`: File path inside the image (e.g., `"notes/todo.txt"`)
- `size`: Target file size in bytes

### 7. `append_file`
Append a line to a file (creates the file if it does not exist). Ideal for JSONL memory logs.

**Parameters**:
- `path`: File path inside the image
- `content`: UTF-8 content to append (a newline is auto-added if missing)

### 8. `status`
Show filesystem status: image path, total/used/free blocks, and fragmentation ratio.

**Parameters**: None

## Example Workflow

Here is an end-to-end example of using the MCP tools to store and retrieve agent memory.

1. **Start the server** (in a terminal or via agent host):
   ```bash
   ./target/release/oifs_mcp agent_memory.img
   ```

2. **Agent host initializes the connection** (JSON-RPC):
   ```json
   {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test-agent","version":"1.0.0"}}}
   {"jsonrpc":"2.0","method":"notifications/initialized"}
   ```

3. **Create a directory for logs**:
   ```json
   {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mkdir","arguments":{"path":"logs"}}}
   ```

4. **Append a memory entry** (JSONL format):
   ```json
   {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"append_file","arguments":{"path":"logs/memory.jsonl","content":"{\"task\":\"explored MCP workspace\",\"result\":\"success\"}"}}}
   ```

5. **Read the log to verify**:
   ```json
   {"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"logs/memory.jsonl"}}}
   ```
   Response:
   ```json
   {"jsonrpc":"2.0","id":4,"result":"{\"ok\":true,\"inode\":3,\"total_bytes\":52}"}
   ```
   (The actual content is returned as the raw string; the test checks for the JSON structure.)

6. **Check filesystem status**:
   ```json
   {"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"status","arguments":{}}}
   ```
   Response:
   ```json
   {"jsonrpc":"2.0","id":5,"result":"{\"image\":\"agent_memory.img\",\"total_blocks\":2560,\"used_blocks\":8,\"free_blocks\":2552,\"fragmentation\":0.003}"}
   ```

## Notes

- **Error handling**: Tool failures return a JSON string with `ok: false` and an `error` message.
- **Paths**: All paths are relative to the root of the OIFS image. Use `.` for the root directory.
- **Concurrency**: The server uses a `TokioMutex` to serialize access to the underlying `DiskManager`, ensuring transaction isolation per tool invocation.
- **Encryption**: If `OIFS_PASSWORD` is set, the image is opened (or created) as encrypted. The same password must be provided to open an existing encrypted image.

## Integration

<!-- openwiki: broken internal link [./../../architecture/mcp_server.md] file "./../../architecture/mcp_server.md" does not exist. Fix the href or restore the target, then delete this comment. -->
Refer to the [MCP server architecture](./../../architecture/mcp_server.md) for details on the internal structure and the [`rmcp`](https://github.com/agentixlabs/rmcp) framework.

<!-- openwiki: broken internal link [./../../quickstart.md] file "./../../quickstart.md" does not exist. Fix the href or restore the target, then delete this comment. -->
For a quick start with the `oifs` CLI (non-MCP), see the [Quickstart guide](./../../quickstart.md).
