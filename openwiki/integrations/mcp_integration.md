---
type: integration guide
title: MCP Integration Guide
description: Guide to integrating AI agents and IDEs with OIFS via the Model Context Protocol server. Covers stdio transport, tool schemas, encrypted sandboxes, and client configuration.
tags: [mcp, integration, ai-agents, ide, filesystem]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-23775c3de52f3ab95a13cb8b
    resource: repo://README.md
  - id: openwiki-source-1d8ff572dc201d9ae2619645
    resource: repo://src/bin/oifs_mcp.rs
  - id: openwiki-source-b0f9117e3c34930435de9b17
    resource: repo://tests/mcp_server_test.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
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

### Startup Behavior

1. **Image creation**: If the image file doesn't exist and `OIFS_PASSWORD` is set, `DiskManager::create_encrypted` is called. Otherwise `DiskManager::open_with_password` auto-creates a plain image.
2. **Size handling**: The `--size` value (megabytes) is multiplied by `1024 * 1024` with checked arithmetic to prevent silent overflow.
3. **Single image per process**: The CLI accepts exactly one positional image argument. All tools operate exclusively inside that image.
4. **Logging**: On startup the server logs to **stderr**: `[oifs-mcp] image=agent_memory.img size=10MB encrypted=false`.

### MCP Protocol Handshake

When an MCP client spawns the server, the following sequence occurs over stdio:

1. Client sends `initialize` request with protocol version and client info
2. Client sends `notifications/initialized` notification
3. Client calls `tools/list` to discover available tools
4. Subsequent `tools/call` invocations perform file operations

The server implements the MCP `ServerHandler` interface and advertises tool capabilities during initialization. It does **not** expose MCP resources (resource URIs, templates, or `resources/list`/`resources/read` handlers) — all filesystem access is mediated exclusively through the registered tools.

## Sandbox Isolation Model

Each `oifs_mcp` server instance operates on **exactly one disk image**. This creates a strong isolation boundary:

- **One image per server process** — All file operations are confined to the single `.img` file specified at startup.
- **No cross-image access** — An agent connected to one MCP server cannot access files in another agent's image.
- **Persistent state** — The disk image persists on disk. Reusing the same image path across sessions preserves agent memory.
- **Encryption per image** — When `OIFS_PASSWORD` is set, the entire image is encrypted with AES-256-XTS (with Argon2id key derivation). The same password must be provided to reopen it.

This model maps naturally to "one sandbox per agent session" or "one sandbox per project."

### Session Isolation for Multi-Tenant Use

The underlying `OifsSession` abstraction (in `src/session.rs`) provides three session modes that affect how multiple processes share an image:

| Mode | Transport | Use Case |
|------|-----------|----------|
| **Standalone** | Direct `mmap` — zero sockets, zero background threads | Single-process use (default for MCP server) |
| **Local IPC** | Unix Domain Socket in `/tmp` | Multi-process on the same machine |
| **Network** | TCP + rendezvous file (`.img.master`) | NFS/Lustre/shared storage, multi-node clusters |

The MCP server currently operates exclusively in **Standalone mode** (direct `mmap` access, zero IPC overhead). Each MCP server process gets its own `DiskManager` instance. For multi-tenant scenarios where multiple agents need isolated sandboxes, run separate `oifs_mcp` processes with different image paths. The process-wide session registry in `OifsSession` ensures that within a single process, multiple handles to the same canonicalized image path share one `DiskManager`.

**Authentication** for encrypted images is handled entirely via the `OIFS_PASSWORD` environment variable. The MCP server forwards this to `DiskManager`; it has no independent key-management logic. In IPC/Network modes, the Master process holds the encryption credentials and Proxies authenticate via a session secret or the password.

## Connecting from IDEs and AI Agents

The MCP server uses the **stdio transport** with **JSON-RPC 2.0**. Any MCP-compatible client can spawn the binary and communicate over stdin/stdout.

### Client Configuration: mcp.json

Both **Claude Desktop** and **Cursor** support an `mcp.json` (or `claude_desktop_config.json`) file to declare MCP servers. Add an entry for `oifs_mcp`:

```json
{
  "mcpServers": {
    "oifs-sandbox": {
      "command": "oifs_mcp",
      "args": ["/absolute/path/to/agent_workspace.img", "--size", "64"],
      "env": {
        "OIFS_PASSWORD": "optional-vault-password"
      }
    }
  }
}
```

- **`command`**: Path to the `oifs_mcp` binary (must be on `PATH` or use absolute path).
- **`args`**: Positional arguments passed to the server. The first argument is the image path; `--size <MB>` sets initial size when creating a new image.
- **`env`**: Environment variables for the child process. Use `OIFS_PASSWORD` to enable encryption. Omit for an unencrypted image.

**Claude Desktop**: Place the config at `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows).

**Cursor**: Place the config at `~/.cursor/mcp.json` (macOS/Linux) or `%USERPROFILE%\.cursor\mcp.json` (Windows).

Restart the IDE after editing the config file. The server will appear in the MCP servers list.

### Direct JSON-RPC Testing

You can test the server manually by piping JSON-RPC requests to its stdin:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}' | ./target/release/oifs_mcp --size 10 test.img
```

## Available MCP Tools

The server registers **eight tools** via `rmcp`'s `#[tool_router]` macro. Every tool:

- Clones the `DiskManager`
- Executes the operation inside `tokio::task::spawn_blocking` (bridging async `rmcp` to sync mmap I/O)
- Returns a **JSON string** built with `serde_json` (never hand-rolled `format!`) to guarantee correct escaping

### Response Convention

- **Success**: `{"ok": true, ...}` with tool-specific fields.
- **Failure**: `{"ok": false, "error": "..."}`.

### Tool Reference

| Tool | Description | Parameters |
|------|-------------|------------|
| `write_file` | Create or overwrite a file. Parent directories must already exist. | `path: string`, `content: string` |
| `read_file` | Read a file's contents as UTF-8 text. | `path: string` |
| `list_dir` | List entries at a path. Returns **JSON Lines (JSONL)** — one JSON object per line. | `path: string` (e.g., `"."` for root) |
| `mkdir` | Create a directory. Parent directories must exist (no `mkdir -p`). | `path: string` |
| `delete_file` | Delete a file. | `path: string` |
| `truncate_file` | Truncate or extend a file to a specific byte size. | `path: string`, `size: number` (bytes) |
| `append_file` | Append a line to a file (creates file if missing). Auto-adds trailing newline. Ideal for **JSONL memory logs**. | `path: string`, `content: string` |
| `status` | Show filesystem stats: image path, total/used/free blocks, fragmentation ratio. | *(none)* |

### Path Semantics

- All paths are **relative to the image root** (inode 1). There is no working-directory concept.
- Paths like `"notes/todo.txt"` or `"."` are resolved by `DiskManager::resolve_path` starting from root.
- `write_file`, `mkdir`, and `append_file` use `resolve_parent` to locate the parent directory inode, then create the child. They **do not** auto-create intermediate directories — the agent must call `mkdir` for each level first.
- Path components are not URL-encoded; they are used as raw directory entry names.

### `list_dir` JSONL Output

`list_dir` returns **one JSON object per line** (not a JSON array):

```json
{"name":"notes","kind":"dir","size":0}
{"name":"logs","kind":"dir","size":0}
{"name":"hello.txt","kind":"file","size":13}
```

This streaming-friendly format lets an agent process large directories incrementally.

## Resource URI Scheme: `oifs://<image>/<path>`

**Note**: The current `oifs_mcp` implementation does **not** expose MCP resources (the `resources/list` and `resources/read` handlers are not implemented). All filesystem access goes through tools only.

If future versions add resource support, the URI scheme would follow the pattern:

```
oifs://<image-name>/<path-inside-image>
```

Where `<image-name>` is the base filename of the `.img` file (e.g., `agent_memory`), and `<path-inside-image>` is the path relative to the image root (e.g., `notes/todo.txt`).

## Tool Allowlists and Sandbox Directory Restrictions

The MCP server does **not** implement a configurable tool allowlist or directory-level permissions within the sandbox. The sandbox boundary is the **entire disk image**:

- All eight tools are always available when the server starts.
- There is no mechanism to disable specific tools per-client.
- There is no path-prefix restriction inside the image — any path under the image root is accessible.

To achieve finer-grained isolation, run **separate `oifs_mcp` processes** with **different image files** per agent/project. Each process provides a completely independent filesystem with its own tool namespace.

## Typical Agent Loop

A common memory pattern for an agent across a conversation:

```text
1. write_file  →  create/overwrite a structured note (e.g., "context/project_plan.md")
2. append_file →  stream JSONL observations to a log (e.g., "logs/memory.jsonl")
3. list_dir    →  browse the sandbox to discover what exists (JSONL output)
4. read_file   →  recall a specific note or log file
```

Because the `.img` file persists on disk, **the same image path reused in a later session restores all files**. The agent's memory survives process restarts, host restarts, and machine reboots.

### Example: JSONL Log with `append_file`

**Request:**
```json
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"append_file","arguments":{"path":"logs/memory.jsonl","content":"{\"event\":\"observation\",\"ts\":1720000000,\"data\":{\"key\":\"value\"}}"}}}
```

**Response (success):**
```json
{"ok":true,"inode":42,"total_bytes":1024}
```

Each call appends one line (the tool auto-adds `\n` if missing). The resulting `logs/memory.jsonl` is valid JSONL — one complete JSON object per line — ready for streaming parsers.

## Encrypted Sandbox Workflow

1. **Set the password** in the host environment (or in the MCP client config `env` section):
   ```bash
   export OIFS_PASSWORD="strong-passphrase"
   ```
2. **Launch the server** (first run creates the encrypted image):
   ```bash
   ./target/release/oifs_mcp --size 50 agent_memory.img
   ```
   Startup log shows `encrypted=true`.
3. **Reopen later** — the same `OIFS_PASSWORD` must be present in the environment, otherwise `DiskManager::open_with_password` returns `PasswordRequired`.

The encryption is **image-scoped**: the entire `.img` file (superblock, inode table, data blocks, directory entries, filenames) is encrypted. Filenames use AES-SIV (deterministic authenticated encryption) so directory listings don't leak name lengths.

## Concurrency Model

`OifsMcpServer` holds `DiskManager` directly (not behind a `TokioMutex` — each blocking task gets its own clone, and `DiskManager` uses internal `RwLock`/`Mutex` for synchronization). An additional `append_lock: Arc<std::sync::Mutex<()>>` serializes concurrent `append_file` calls so that multiple agents appending to the same log file interleave cleanly line-by-line.

### Why This Is Acceptable for Agent Workloads

- **MCP over stdio is inherently sequential** from a single agent's perspective: an LLM emits one tool call, waits for the result, then emits the next.
- **No fan-out from a single host** — the stdio transport pairs one stdin/stdout with one server process. Concurrent access would only occur if multiple agent hosts connected to the same process, which the transport prevents by design.
- **Throughput implication**: The async–sync bridge adds negligible latency (microseconds) per tool call. For workloads requiring true parallelism (e.g., batch indexing, multi-agent coordination), the IPC/daemon path (`src/ipc.rs`) provides a multi-threaded alternative with a dedicated master process.

## Related Pages

- [MCP Server Architecture](../architecture/mcp_server.md) — internal structure, `rmcp` integration, tool registration.
- [MCP Workflow](../workflows/mcp_workflow.md) — detailed tool usage patterns and examples.
- [Quickstart](../quickstart.md) — building and running the MCP server.
- [Encrypted Workflows](../workflows/encrypted_workflows.md) — Argon2id parameters, key hierarchy, IPC authentication.
