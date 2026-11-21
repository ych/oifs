---
type: "Reference"
title: "MCP Workflow"
openwiki_generated: true
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


# MCP Workflow

The OIFS MCP server (`oifs_mcp`) exposes the OIFS filesystem as a set of tools via the Model Context Protocol (MCP) over standard input/output (stdio). This gives AI agents (Claude Desktop, Cursor, custom agent loops) a persistent, sandboxed memory store contained in a single `.img` file.

---

## Prerequisites

1. **Build with the `mcp` feature**  
   ```bash
   cargo build --release --features mcp
   ```
   Produces `target/release/oifs_mcp`.

2. **Choose an image path**  
   Example: `agent_memory.img`. The server creates the image on first run if it doesn't exist.

3. **Optional encryption**  
   Set `OIFS_PASSWORD` in the environment to open or create an encrypted image (AES-256-XTS with Argon2id key derivation).

---

## Server Startup

The binary communicates over stdio and is intended to be spawned by an MCP host (Claude Desktop, Cursor, or a test harness). Launch manually for debugging:

```bash
# Defaults: agent_memory.img, 10 MB, unencrypted
./target/release/oifs_mcp

# Custom image and size (MB)
./target/release/oifs_mcp --size 20 custom_agent.img

# Encrypted image (OIFS_PASSWORD must be set)
OIFS_PASSWORD=secret ./target/release/oifs_mcp encrypted.img
```

On startup the server logs to **stderr**:
```
[oifs-mcp] image=agent_memory.img size=10MB encrypted=false
```

The server then enters the MCP `stdio` transport loop, waiting for JSON-RPC 2.0 requests.

### Startup Behavior (from source)

- **Image creation**: If the image file doesn't exist and `OIFS_PASSWORD` is set, `DiskManager::create_encrypted` is called. Otherwise `DiskManager::open_with_password` auto-creates a plain image.
- **Size handling**: The `--size` value (megabytes) is multiplied by `1024 * 1024` with checked arithmetic to prevent silent overflow.
- **Single image per process**: The CLI accepts exactly one positional image argument. All tools operate exclusively inside that image.

---

## Connecting an AI Agent Host

MCP hosts launch `oifs_mcp` as a subprocess and speak JSON-RPC over the child's stdin/stdout. The standard handshake:

1. Host spawns `oifs_mcp` with desired arguments.
2. Host sends `initialize` request (protocol version, client info).
3. Host sends `notifications/initialized` notification.
4. Host calls `tools/list` to discover available tools.
5. Subsequent `tools/call` invocations perform file operations.

See [`tests/mcp_server_test.rs`](repo://tests/mcp_server_test.rs#L1-L50) for a minimal working example of this sequence.

---

## Available Tools

Eight tools are registered via the `#[tool_router]` macro. Every tool acquires `self.dm.lock().await` (a `TokioMutex<DiskManager>`), executes the operation, and returns a **JSON string** built with `serde_json` (never hand-rolled `format!`) to guarantee correct escaping.

**Response convention**  
- Success: `{"ok": true, ...}` with tool-specific fields.  
- Failure: `{"ok": false, "error": "..."}`.

| Tool | Purpose |
|------|---------|
| `write_file` | Create or overwrite a file. Parent directories must already exist. |
| `read_file` | Read a file's contents as UTF-8 text. |
| `list_dir` | List entries at a path. Emits **JSON Lines (JSONL)** — one JSON object per line. |
| `mkdir` | Create a directory. Parent directories must exist (no `mkdir -p`). |
| `delete_file` | Delete a file. |
| `truncate_file` | Truncate or extend a file to a specific byte size. |
| `append_file` | Append a line to a file (creates file if missing). Auto-adds trailing newline. Ideal for **JSONL memory logs**. |
| `status` | Show filesystem stats: image path, total/used/free blocks, fragmentation ratio. |

### Path Semantics

- All paths are **relative to the image root** (inode 1). There is no working-directory concept.
- Paths like `"notes/todo.txt"` or `"."` are resolved by `DiskManager::resolve_path` starting from root.
- `write_file`, `mkdir`, and `append_file` use `resolve_parent` to locate the parent directory inode, then create the child. They **do not** auto-create intermediate directories — the agent must call `mkdir` for each level first.

---

## Typical Agent Loop: write → append → list → read

A common memory pattern for an agent across a conversation:

```text
1. write_file  →  create/overwrite a structured note (e.g., "context/project_plan.md")
2. append_file →  stream JSONL observations to a log (e.g., "logs/memory.jsonl")
3. list_dir    →  browse the sandbox to discover what exists (JSONL output)
4. read_file   →  recall a specific note or log file
```

Because the `.img` file persists on disk, **the same image path reused in a later session restores all files**. The agent's memory survives process restarts, host restarts, and machine reboots.

### Example JSONL Log with `append_file`

```json
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"append_file","arguments":{"path":"logs/memory.jsonl","content":"{\"event\":\"observation\",\"ts\":1720000000,\"data\":{\"key\":\"value\"}}"}}}
```

Server response (success):
```json
{"ok":true,"inode":42,"total_bytes":1024}
```

Each call appends one line (the tool auto-adds `\n` if missing). The resulting `logs/memory.jsonl` is valid JSONL — one complete JSON object per line — ready for streaming parsers.

### `list_dir` JSONL Output

`list_dir` returns **one JSON object per line** (not a JSON array):

```json
{"name":"notes","kind":"dir","size":0}
{"name":"logs","kind":"dir","size":0}
{"name":"hello.txt","kind":"file","size":13}
```

This streaming-friendly format lets an agent process large directories incrementally.

---

## Sandbox Isolation Model

- **One image per server process** — the CLI accepts a single image path. All file operations are confined to that `.img` file.
- **No cross-image access** — an agent connected to one MCP server cannot reach another agent's image.
- **Persistent state** — the image file persists on disk. Reusing the same path across sessions preserves agent memory.
- **Encryption per image** — if `OIFS_PASSWORD` is set at startup, the entire image is encrypted (AES-256-XTS, Argon2id key derivation with salt stored in the superblock). The same password must be supplied on every subsequent launch. The MCP server itself has no key-management logic; it simply forwards the environment variable to `DiskManager`.

This model maps naturally to **one sandbox per agent session** or **one sandbox per project**.

---

## Concurrency: Single-Threaded TokioMutex

`OifsMcpServer` wraps `DiskManager` in `Arc<TokioMutex<DiskManager>>`:

```rust
struct OifsMcpServer {
    dm: Arc<TokioMutex<DiskManager>>,
    image_path: PathBuf,
    tool_router: ToolRouter<Self>,
}
```

### Why This Is Acceptable for Agent Workloads

- **MCP over stdio is inherently sequential** from a single agent's perspective: an LLM emits one tool call, waits for the result, then emits the next. The mutex serializes these naturally sequential requests.
- **No fan-out from a single host** — the stdio transport pairs one stdin/stdout with one server process. Concurrent access would only occur if multiple agent hosts connected to the same process, which the transport prevents by design.
- **Throughput implication**: The mutex adds negligible latency (microseconds) per tool call. For workloads requiring true parallelism (e.g., batch indexing, multi-agent coordination), the IPC/daemon path (`src/ipc.rs`) provides a multi-threaded alternative with a dedicated master process.

---

## Encrypted Sandbox Workflow

1. **Set the password** in the host environment:
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

---

## Quick Reference: Tool Parameters

| Tool | Required Parameters | Notes |
|------|---------------------|-------|
| `write_file` | `path: string`, `content: string` | Parent dir must exist. Overwrites. |
| `read_file` | `path: string` | Returns UTF-8 content or error if non-UTF-8. |
| `list_dir` | `path: string` (e.g., `"."` or `"notes"`) | Returns JSONL (one object per line). |
| `mkdir` | `path: string` | Parent dir must exist. |
| `delete_file` | `path: string` | |
| `truncate_file` | `path: string`, `size: number` (bytes) | |
| `append_file` | `path: string`, `content: string` | Creates file if missing. Auto-adds `\n`. |
| `status` | *(none)* | Returns image path, block counts, fragmentation. |

---

## Related Pages

- [MCP Server Architecture](../architecture/mcp_server.md) — internal structure, `rmcp` integration, tool registration.
- [MCP Integration Guide](../integrations/mcp_integration.md) — Claude Desktop, Cursor, and custom client configuration.
- [Basic Operations](../workflows/basic_operations.md) — lower-level `DiskManager` API and direct-mode flows.
- [Encrypted Workflows](../workflows/encrypted_workflows.md) — Argon2id parameters, key hierarchy, IPC authentication.
