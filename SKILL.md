---
name: oifs-sandbox
description: Use the OIFS single-file POSIX filesystem CLI (`oifs`) and MCP server (`oifs_mcp`) for isolated, encrypted, crash-safe AI agent workspaces, O(1) CoW state handoff (`oifs clone`), bounded pagination reads, and native OS mounting (`oifs mount`).
---

# OIFS Agent Sandbox & Memory Skill

OIFS packs an entire Unix-like filesystem into a single portable `.img` container file with **zero network calls**, **zero vendor lock-in**, and optional **XAES-256-GCM + AES-256-GCM-SIV** authenticated encryption.

## When to Use This Skill

- Isolate an AI agent's scratchpad, logs, structured memory, or build artifacts inside a single rollbackable `.img` container instead of scattering files across the host OS.
- Hand off workspace state cleanly between agents using O(1) CoW cloning (`oifs clone`) or shared `F_RDLCK` read-only mounts (`--read-only`).
- Compress repetitive telemetry or agent logs using SIMD filters (`delta`, `shuffle`, `bitshuffle`), Seekable 64 KiB Zstd chunks, or trained Zstd dictionaries (`train_zstd_dict`).

## Connect `oifs_mcp` to Your Agent Client

Build or install the MCP server binary with `--features mcp` (enabled by default):

```bash
cargo build --release --features mcp --bin oifs_mcp
```

- **Claude Code**:
  ```bash
  claude mcp add oifs-sandbox -- /path/to/oifs_mcp /path/to/agent_workspace.img --size 64 --journal --auto-grow=512
  ```
- **Claude Desktop / Cursor / Codex / Gemini CLI (`mcpServers` JSON)**:
  ```json
  {
    "mcpServers": {
      "oifs-sandbox": {
        "command": "/path/to/oifs_mcp",
        "args": ["/path/to/agent_workspace.img", "--size", "64", "--journal", "--auto-grow=512"],
        "env": {
          "OIFS_PASSWORD": "optional-vault-password"
        }
      }
    }
  }
  ```

Pass `--read-only` to share an immutable verification mount across multiple concurrent agents via `fcntl(F_RDLCK)`.

## Available MCP Tools (18 Tools)

1. **Read & Inspect (Bounded Anti-Token-Drain)**:
   - `read_file` (`path`, optional `offset`, `max_bytes`): Defaults to a 256 KiB cap (max 4 MiB) with UTF-8 boundary safety and continuation hints.
   - `list_dir` (`path`, optional `offset`, `limit`): Returns JSONL entries paginated at 500 entries by default (max 5,000).
   - `stat_path` (`path`): Returns inode ID, type, logical/compressed bytes, timestamps, and flags.
   - `status`: Container size, encryption status/cipher, auto-grow state, Zstd dictionaries, block usage, and fragmentation ratio.
   - `verify_integrity`: Online structural `fsck` verification.
2. **Write, Batch & Atomic Operations**:
   - `write_file`, `append_file` (JSONL-safe), `mkdir`, `delete_file`, `truncate_file`, `rename_path`.
   - `write_batch`: Atomically commits mixed `create_file`, `mkdir`, `delete`, and `rename` ops in one WAL transaction.
3. **Capacity & Dictionary Tuning**:
   - `resize_image`, `vacuum_image`, `configure_auto_grow`, `train_zstd_dict`, `load_zstd_dict`, `set_default_zstd_dict`.

## Multi-Agent Baton Relay & Clean Exit

```bash
# Fork an agent workspace instantaneously in O(1) via APFS clonefile / Linux FICLONE
oifs clone agent_a.img agent_b_handoff.img

# Inspect integrity or mount natively on Linux/macOS with zero vendor lock-in
oifs -i agent_b_handoff.img --read-only mount ./mnt --foreground
```

## Safety Guarantees

- **Zero Telemetry / Zero External Network**: `oifs_mcp` communicates exclusively over stdio JSON-RPC and modifies only the target `.img` file.
- **Anti-Token-Drain Bounds**: `read_file` and `list_dir` enforce hard response caps and offsets so large logs cannot exhaust agent context budgets.
- **Clean Exit**: Standard POSIX tree extractable via `oifs mount`, `oifs cat`, C FFI (`include/oifs.h`), or Modern C++17/20 (`include/oifs.hpp`).
