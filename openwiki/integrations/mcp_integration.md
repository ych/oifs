---
type: integration guide
title: MCP Integration Guide
description: Guide to integrating AI agents and IDEs with OIFS via the Model Context Protocol server.
tags: [mcp, integration, ai-agents, ide]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-04T10:12:53.730Z
sources:
  - id: openwiki-source-1d8ff572dc201d9ae2619645
    resource: repo://src/bin/oifs_mcp.rs
  - id: openwiki-source-b0f9117e3c34930435de9b17
    resource: repo://tests/mcp_server_test.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-04T10:12:53.730Z" }
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

## Connecting from IDEs and AI Agents

### Claude Desktop

1. Install Claude Desktop.
2. Open Settings → Developer → MCP Servers.
3. Click "Add New Server".
4. Configure:
   - **Name**: OIFS Memory Sandbox
   - **Command**: `cargo` (or path to cargo binary)
   - **Arguments**: `run --bin oifs_mcp -- [IMAGE_PATH]`
   - **Environment Variables**: 
     - `OIFS_PASSWORD` (if using encryption)
5. Save and restart Claude Desktop.

### Cursor

1. Install Cursor.
2. Open Settings → MCP Servers.
3. Click "Add New MCP Server".
4. Configure similarly to Claude Desktop, using the `cargo run` command.
5. Save and reload Cursor.

### Other MCP-Compatible Agents

Any agent that supports MCP over stdio can connect by spawning the `oifs_mcp` binary and communicating via JSON-RPC 2.0. Refer to the agent's documentation for MCP configuration.

## Available Tools

Once connected, the MCP server exposes the following tools for file operations within the OIFS sandbox image:

### `write_file`
Write (create or overwrite) a file. Parent directories must already exist.
- **Parameters**:
  - `path`: Path inside the OIFS image (e.g., `"notes/todo.txt"`)
  - `content`: UTF-8 content to write
- **Returns**: JSON with `ok`, `inode`, and `bytes` on success; or `ok: false` and `error` on failure.

### `read_file`
Read the contents of a file.
- **Parameters**:
  - `path`: Path inside the OIFS image (e.g., `"notes/todo.txt"`)
- **Returns**: File content as UTF-8 string on success; or error JSON on failure.

### `list_dir`
List files and directories at a given path (returns JSONL, one object per line).
- **Parameters**:
  - `path`: Path inside the OIFS image (e.g., `"."` for root, `"notes"` for subdirectory)
- **Returns**: Newline-separated JSON objects with `name`, `kind` (`"file"` or `"dir"`), and `size`; or `{"entries":0}` if empty.

### `mkdir`
Create a directory. Parent directories must exist.
- **Parameters**:
  - `path`: Path of the new directory (e.g., `"notes/drafts"`)
- **Returns**: JSON with `ok` and `inode` on success; or error JSON on failure.

### `delete_file`
Delete a file.
- **Parameters**:
  - `path`: Path of the file to delete (e.g., `"notes/old.txt"`)
- **Returns**: JSON with `ok` on success; or error JSON on failure.

### `append_file`
Append a line to a file (creates the file if it does not exist). Ideal for JSONL memory logs.
- **Parameters**:
  - `path`: Path inside the OIFS image (e.g., `"logs/memory.jsonl"`)
  - `content`: UTF-8 content to append (a newline is auto-added if missing)
- **Returns**: JSON with `ok`, `inode`, and `total_bytes` on success; or error JSON on failure.

### `status`
Show filesystem status: image path, total/used/free blocks, and fragmentation ratio.
- **Parameters**: None
- **Returns**: JSON with `image`, `total_blocks`, `used_blocks`, `free_blocks`, and `fragmentation` ratio.

## Configuration Notes

### Disk Image Persistence

The MCP server operates on a disk image file (`.img` by default). All changes are persisted to this image. To preserve agent memory across sessions, reuse the same image path.

### Encryption

When `OIFS_PASSWORD` is set, the image is encrypted using AES-256-XTS. The same password must be provided to open an existing encrypted image. Loss of the password results in irreversible data loss.

### Size Management

The image size is fixed at creation. If the image fills up, write operations will fail. To increase size, back up the existing image, create a new larger image, and copy data over (outside the MCP server).

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

These tests spawn the server, send JSON-RPC initialize and tools/list requests, and validate responses.

## Conclusion

By integrating OIFS via the MCP server, AI agents gain a persistent, sandboxed filesystem for memory storage and file-based workflows. The stdio-based MCP interface allows seamless compatibility with standard MCP clients like Claude Desktop and Cursor, while the toolset provides familiar file operations within a secure, isolated environment.
