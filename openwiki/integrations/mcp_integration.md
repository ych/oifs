---
type: integration guide
title: MCP Integration Guide
description: Guide to integrating AI agents and IDEs with OIFS via the Model Context Protocol server. Covers stdio transport, tool schemas, encrypted sandboxes, and client configuration.
tags: [mcp, integration, ai-agents, ide, filesystem]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-1d8ff572dc201d9ae2619645
    resource: repo://src/bin/oifs_mcp.rs
  - id: openwiki-source-b0f9117e3c34930435de9b17
    resource: repo://tests/mcp_server_test.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---

# MCP Integration Guide

This guide explains how to integrate AI agents and IDEs with the OIFS filesystem using the Model Context Protocol (MCP) server. The MCP server exposes OIFS as a set of tools that AI agents can use to read, write, and manage files within a sandboxed disk image.

## Running the MCP Server

The MCP server is provided as a standalone binary: `oifs_mcp`. It can be run directly with Cargo or installed via `cargo install`.

### Basic Usage

```bash
cargo run --bin oifs_mcp [IMAGE_PATH]
```

### Options

- `--size <MB>`: Set the initial size of the disk image when creating a new image (default: 10 MB).
- `-h, --help`: Show help message.

### Environment Variables

- `OIFS_PASSWORD`: If set, the disk image is opened (or created) as encrypted using this password. Required to open an existing encrypted image.

### Example

```bash
# Run with default image (agent_memory.img) and size (10 MB)
cargo run --bin oifs_mcp

# Run with custom image and size
cargo run --bin oifs_mcp -- --size 20 my_agent.img

# Run with encryption (requires OIFS_PASSWORD set)
OIFS_PASSWORD="secret" cargo run --bin oifs_mcp -- encrypted.img
```

The server communicates via stdio (standard input/output) and expects JSON-RPC 2.0 messages, making it compatible with any MCP client.

## Sandbox Isolation Model

Each `oifs_mcp` server instance operates on **exactly one disk image**. This creates a strong isolation boundary:

- **One image per server process** — All file operations are confined to the single `.img` file specified at startup.
- **No cross-image access** — An agent connected to one MCP server cannot access files in another agent's image.
- **Persistent state** — The disk image persists on disk. Reusing the same image path across sessions preserves agent memory.
- **Encryption per image** — When `OIFS_PASSWORD` is set, the entire image is encrypted with AES-256-XTS. The same password must be provided to reopen it.

This model maps naturally to "one sandbox per agent session" or "one sandbox per project."

## Connecting from IDEs and AI Agents

The MCP server uses the **stdio transport** with **JSON-RPC 2.0**. Any MCP-compatible client can spawn the binary and communicate over stdin/stdout.

### Claude Desktop

1. Install Claude Desktop.
2. Open **Settings → Developer → MCP Servers**.
3. Click **Add New Server**.
4. Configure:

   ```json
   {
     "name": "OIFS Memory Sandbox",
     "command": "cargo",
     "args": ["run", "--bin", "oifs_mcp", "--", "agent_memory.img"],
     "env": {
       "OIFS_PASSWORD": "your-encryption-password"
     }
   }
   ```

   - Use the full path to `cargo` if it's not on the system PATH (e.g., `/home/user/.cargo/bin/cargo`).
   - Omit `env.OIFS_PASSWORD` if you don't need encryption.
   - Replace `agent_memory.img` with your desired image path (relative to the working directory or absolute).

5. Save and restart Claude Desktop.

### Cursor

1. Install Cursor.
2. Open **Settings → MCP Servers** (or `Ctrl+Shift+P` → "MCP: Open Configuration").
3. Add a new server entry in `mcp.json`:

   ```json
   {
     "mcpServers": {
       "oifs-memory": {
         "command": "cargo",
         "args": ["run", "--bin", "oifs_mcp", "--", "agent_memory.img"],
         "env": {
           "OIFS_PASSWORD": "your-encryption-password"
         }
       }
     }
   }
   ```

4. Save and reload Cursor (or restart).

### VS Code (with MCP Extension)

If using an MCP-compatible extension (e.g., `mcp-vscode`):

```json
{
  "mcp": {
    "servers": {
      "oifs": {
        "command": "cargo",
        "args": ["run", "--bin", "oifs_mcp", "--", "agent_memory.img"],
        "env": { "OIFS_PASSWORD": "optional-password" }
      }
    }
  }
}
```

### Generic JSON-RPC 2.0 Client

Any client that can spawn a process and speak JSON-RPC 2.0 over stdio:

```python
import subprocess, json

proc = subprocess.Popen(
    ["cargo", "run", "--bin", "oifs_mcp", "--", "agent_memory.img"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True
)

# Initialize
init = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "my-client", "version": "1.0"}}}
proc.stdin.write(json.dumps(init) + "\n")
proc.stdin.flush()

# List tools
tools = {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}
proc.stdin.write(json.dumps(tools) + "\n")
proc.stdin.flush()

# Read response
for line in proc.stdout:
    print(line)
```

## Available Tools

Once connected, the MCP server exposes the following tools for file operations within the OIFS sandbox image. All tool parameters follow JSON Schema; examples are provided below.

### Common Response Format

All tools return a JSON object. Success responses include `"ok": true` with tool-specific fields. Errors return:

```json
{ "ok": false, "error": "human-readable error message" }
```

---

### `write_file`

Write (create or overwrite) a file. Parent directories must already exist.

**Parameters (JSON Schema):**

```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path inside the OIFS image (e.g. \"notes/todo.txt\")" },
    "content": { "type": "string", "description": "UTF-8 content to write" }
  },
  "required": ["path", "content"]
}
```

**Returns on success:**

```json
{ "ok": true, "inode": 123, "bytes": 42 }
```

---

### `read_file`

Read the contents of a file. Returns raw UTF-8 text (not wrapped in JSON).

**Parameters (JSON Schema):**

```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path inside the OIFS image (e.g. \"notes/todo.txt\")" }
  },
  "required": ["path"]
}
```

**Returns on success:** The file content as a plain UTF-8 string (not JSON-encoded).

**Returns on error:** JSON error object `{ "ok": false, "error": "..." }`.

---

### `list_dir`

List files and directories at a given path. Returns **JSONL** (one JSON object per line).

**Parameters (JSON Schema):**

```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path inside the OIFS image (e.g. \".\" for root, \"notes\" for subdirectory)" }
  },
  "required": ["path"]
}
```

**Returns on success:** Newline-separated JSON objects, each with:

```json
{ "name": "todo.txt", "kind": "file", "size": 42 }
{ "name": "drafts", "kind": "dir", "size": 0 }
```

If the directory is empty, returns: `{"entries":0}`.

**Returns on error:** JSON error object.

---

### `mkdir`

Create a directory. Parent directories must exist.

**Parameters (JSON Schema):**

```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path of the new directory (e.g. \"notes/drafts\")" }
  },
  "required": ["path"]
}
```

**Returns on success:**

```json
{ "ok": true, "inode": 124 }
```

---

### `delete_file`

Delete a file from the OIFS sandbox image.

**Parameters (JSON Schema):**

```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path of the file to delete (e.g. \"notes/old.txt\")" }
  },
  "required": ["path"]
}
```

**Returns on success:**

```json
{ "ok": true }
```

---

### `truncate_file`

Truncate or extend a file to the specified size in bytes.

**Parameters (JSON Schema):**

```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path inside the OIFS image (e.g. \"data/file.bin\")" },
    "size": { "type": "integer", "minimum": 0, "description": "New size in bytes" }
  },
  "required": ["path", "size"]
}
```

**Returns on success:**

```json
{ "ok": true, "size": 1024 }
```

---

### `append_file`

Append a line to a file (creates the file if it does not exist). **Ideal for JSONL memory logs** — each call appends the content plus a newline, producing valid JSONL when each payload is a JSON object.

**Parameters (JSON Schema):**

```json
{
  "type": "object",
  "properties": {
    "path": { "type": "string", "description": "Path inside the OIFS image (e.g. \"logs/memory.jsonl\")" },
    "content": { "type": "string", "description": "UTF-8 content to append (a newline is auto-added if missing)" }
  },
  "required": ["path", "content"]
}
```

**Returns on success:**

```json
{ "ok": true, "inode": 125, "total_bytes": 2048 }
```

**Agent Memory Log Example:**

An agent can maintain a structured memory log by appending JSON objects:

```python
# Agent writes a memory entry
append_file({ "path": "memory.jsonl", "content": '{"type":"observation","timestamp":"2025-01-15T10:30:00Z","summary":"User asked about project structure"}' })

# Later, read the entire log with read_file and parse each line as JSON
```

Because each call adds a newline, the file remains valid JSONL — one JSON object per line — suitable for streaming parsing.

---

### `status`

Show filesystem status: image path, total/used/free blocks, and fragmentation ratio.

**Parameters:** None (empty object `{}`).

**Returns on success:**

```json
{
  "image": "/home/user/agent_memory.img",
  "total_blocks": 2560,
  "used_blocks": 42,
  "free_blocks": 2518,
  "fragmentation": 0.023
}
```

---

## Configuration Notes

### Disk Image Persistence

The MCP server operates on a disk image file (`.img` by default). All changes are persisted to this image. To preserve agent memory across sessions, reuse the same image path.

### Encryption

When `OIFS_PASSWORD` is set, the image is encrypted using AES-256-XTS. The same password must be provided to open an existing encrypted image. **Loss of the password results in irreversible data loss.**

### Size Management

The image size is fixed at creation (default 10 MB, configurable via `--size`). If the image fills up, write operations will fail with an error. To increase size, back up the existing image, create a new larger image, and copy data over (outside the MCP server).

## Troubleshooting

### Connection Issues

- Ensure the `oifs_mcp` binary is built and accessible in the PATH (or provide full path to cargo/binary).
- Verify stdio communication: the agent should spawn the process and exchange JSON-RPC messages.
- Check agent logs for MCP connection errors.

### Permission Errors

- The server requires read/write access to the image file path.
- On encrypted images, ensure `OIFS_PASSWORD` matches the one used to create the image.

### Tool Failures

- Most tool errors return JSON with `ok: false` and an `error` string.
- Common errors: parent directory missing (for write/mkdir), file not found (for read/delete), or invalid paths.
- Use `list_dir` to verify directory structure before operations.

## Workflow Examples

See the related workflow guide for common agent memory patterns: [MCP Workflow](../workflows/mcp_workflow.md).

For architectural details of the MCP server implementation: [MCP Server Architecture](../architecture/mcp_server.md).

## Testing

The MCP server includes integration tests that verify stdio communication and tool functionality. To run tests:

```bash
cargo test --tests
```

These tests spawn the server, send JSON-RPC initialize and tools/list requests, and validate responses. They also verify that tool responses are valid JSON even when file names contain quotes, backslashes, or control characters (the server uses `serde_json` for all response serialization).

## Conclusion

By integrating OIFS via the MCP server, AI agents gain a persistent, sandboxed filesystem for memory storage and file-based workflows. The stdio-based MCP interface allows seamless compatibility with standard MCP clients like Claude Desktop and Cursor, while the toolset provides familiar file operations within a secure, isolated environment.
