---
type: "Reference"
title: "CLI Reference"
description: "Complete reference for the oifs and oifs_mcp CLI commands: global flags, subcommands, arguments, JSON output, and exit codes."
tags: ["cli", "reference", "operations", "oifs", "oifs_mcp"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-1d8ff572dc201d9ae2619645
    resource: repo://src/bin/oifs_mcp.rs
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

## Overview

The OIFS project provides two command-line executables:

| Binary | Source | Purpose |
| :--- | :--- | :--- |
| `oifs` | [`src/bin/oifs.rs`](../../src/bin/oifs.rs) | Full-featured CLI for creating, inspecting, modifying, and diagnosing OIFS filesystem images. |
| `oifs_mcp` | [`src/bin/oifs_mcp.rs`](../../src/bin/oifs_mcp.rs) | MCP (Model Context Protocol) server that exposes OIFS as tools for AI agents via stdio. |

Both binaries are built with [`clap`](https://crates.io/crates/clap) using the derive pattern. The `oifs` CLI routes operations through the [`OifsSession`](../architecture/overview.md) layer, meaning CLI commands automatically participate in multi-process Master-Proxy concurrency when IPC is enabled.

---

## `oifs` — Complete Command Reference

### Global Options

Global options apply across all subcommands and must appear **before** the subcommand:

| Flag | Long Option | Description |
| :--- | :--- | :--- |
| `-i` | `--image <PATH>` | Path to the target `.img` container file (required for all image commands except `filter-analyze` and `clone`). |
| `-p` | `--password <STR>` | Passphrase for encrypted images. **Highest precedence** in password resolution. |
| | `--ipc` | Enables local IPC (Unix Domain Sockets) mode for multi-process concurrency on the same machine. |
| `-n` | `--network` | Enables cross-machine Network TCP mode (for NFS, Lustre, multi-node clusters). Implies network mode if `--bind` is also set. |
| | `--bind <ADDR>` | Custom host:port to bind in network mode (e.g., `0.0.0.0:9050` or `127.0.0.1:0`). |
| | `--allow-remote-ipc` | Allow the IPC master to bind a non-loopback address. **Only safe when traffic is tunnelled** (WireGuard, IPsec, SSH port forwarding); the IPC transport is plain TCP without TLS. |
| | `--json` | Formats all command outputs as structured JSON and **disables interactive stdin prompts**. |
| | `--auto-defrag [THRESHOLD]` | Enable background automatic defragmentation coordinator. Optional fragmentation threshold (ratio of `(free_runs - 1) / (free_blocks - 1)`) above which a defragmentation pass is triggered. Default: `0.25`. Range: `0.0` (all free space contiguous) to `1.0` (every free block isolated). |
| | `--defrag-interval <SECONDS>` | Interval in seconds between background fragmentation checks. Default: `300` (5 minutes). Minimum: `1`. |
| | `--no-mmap` | Use pure positional `pread`/`pwrite` file I/O instead of `mmap` (eliminates SIGBUS and mmap hazards on network filesystems). |
| | `--read-only` | Open the image in read-only mode (shared `F_RDLCK` lock, zero writes, works on `0444` images). |
| | `--dict <PATH>` | Custom Zstd dictionary file(s) to register (first is also set as default compressor). Can be specified multiple times. |
| | `--auto-grow[=MAX_MB]` | Automatically expand image capacity on block or inode exhaustion (optional hard cap in MiB, e.g. `--auto-grow` or `--auto-grow=64`). Requires `=` when specifying a value. |
| `-h` | `--help` | Prints help information. |
| `-V` | `--version` | Prints version. |

### Password Resolution (3-Tier Hierarchy)

When interacting with encrypted filesystems, `oifs` determines passphrases according to a strict precedence hierarchy ([`src/bin/oifs.rs#L307-L342`](../../src/bin/oifs.rs#L307-L342)):

```
1. CLI Argument (--password <PWD>)
       │ (Present?) ── Yes ──> Use CLI argument
       No
2. Environment Variable (OIFS_PASSWORD)
       │ (Present?) ── Yes ──> Use environment variable
       No
3. Execution Mode Check (--json)
       ├── Enabled  ──> Abort immediately with JSON error
       └── Disabled ──> Secure Interactive Terminal Prompt (libc::termios)
```

1. **CLI Flag (`--password <pwd>`)**: Highest precedence. Useful for scripts and automated integration tests.
2. **Environment Variable (`OIFS_PASSWORD`)**: Preferred for CI/CD environments and background jobs to prevent secrets from appearing in `ps aux` process tables.
3. **Interactive Masked Prompt**: If neither flag nor environment variable is provided, `read_password` ([`src/bin/oifs.rs#L218-L256`](../../src/bin/oifs.rs#L218-L256)) disables terminal echo (`libc::ECHO`) via `libc::tcgetattr` / `libc::tcsetattr`, safely reading the password while masking input. Prompts are printed to `stderr` so stdout remains clean for shell pipes.
4. **Non-Interactive `--json` Enforcement**: In `--json` mode, interactive prompts are forbidden. If an encrypted image is opened without `--password` or `OIFS_PASSWORD`, the process immediately aborts with:
   ```json
   {"ok": false, "error": "Encrypted filesystem requires --password or OIFS_PASSWORD env in --json mode"}
   ```

**Password confirmation on creation** (`oifs -i disk.img create --encrypt`):
- Interactive mode prompts twice (`Enter password: ` and `Confirm password: `) to prevent accidental typos.
- If the password is fewer than 8 characters, a non-blocking warning is emitted to stderr: `⚠️ Warning: Password is shorter than 8 characters`.

---

### JSON Output Mode (`--json`)

The `--json` flag guarantees machine-readable outputs for automation, AI agent tools, and monitoring scripts:

- **Success Responses**: Emits JSON objects (e.g., `{"ok": true, "message": "..."}`) or formatted diagnostic reports.
- **Failure Responses**: Intercepts panics and errors in `main` ([`src/bin/oifs.rs#L358-L364`](../../src/bin/oifs.rs#L358-L364)), emitting `{"ok": false, "error": "<msg>"}` to stdout and exiting with status code 1.

#### Common JSON Success Schemas

| Subcommand | Success Schema | Notes |
| :--- | :--- | :--- |
| `create` | `{"ok": true, "message": "Filesystem created", "journal": boolean}` | `journal` true if `--journal` was specified |
| `put` | `{"ok": true, "inode": <u64>, "bytes": <usize>}` | |
| `get` | `{"ok": true, "bytes": <usize>, "dest": "<path>"}` | |
| `cat` | `{"ok": true, "path": "<name>", "bytes": <usize>, "content": "<utf8_lossy>"}` | |
| `append` | `{"ok": true, "inode": <u64>, "total_bytes": <usize>}` | |
| `mkdir` | `{"ok": true, "inode": <u64>}` | |
| `rm` | `{"ok": true, "removed": "<path>", "inode": <u64>, "recursive": boolean}` | |
| `ls` | `[{"name": "...", "kind": "f\|d", "size": <u64>, "comp_size": <u64\|null>, "modified": "YYYY-MM-DD HH:MM:SS"}, ...]` | Array of entries |
| `stat` | `{"ok": true, "path": "...", "inode": <u64>, "kind": "file\|directory", "size": <u64>, "compressed_size": <u64>, "encrypted": <bool>, "seekable_64k": <bool>, "created": "YYYY-MM-DD HH:MM:SS", "modified": "YYYY-MM-DD HH:MM:SS"}` | |
| `info` | `{"ok": true, "image": "...", "block_size": <u32>, "block_count": <u64>, "total_bytes": <u64>, "inode_count": <u64>, "data_block_start": <u64>, "format_version": <u32>, "encrypted": <bool>, "journaled": <bool>, "read_only": <bool>}` | |
| `analyze` | `{"ok": true, "total_blocks": <u64>, "used_blocks": <u64>, "free_blocks": <u64>, "free_runs": <u64>, "largest_free_run": <u64>, "avg_gap_size": <f64>, "fragmentation_ratio": <f64>}` | |
| `defrag` | `{"ok": true, "files_processed": <u64>, "bytes_moved": <u64>, "frag_before": <f64>, "frag_after": <f64>}` | |
| `fsck` | Full `IntegrityReport` (see [fsck workflow](../workflows/fsck_workflow.md)) | |
| `migrate` | `{"ok": true, "from_version": <u32>, "to_version": <u32>, "inodes_rewritten": <u64>, "already_current": boolean}` | |
| `truncate` | `{"ok": true, "path": "<path>", "size": <u64>}` | |
| `mount` | `{"ok": true, "backend": "nfs\|fuse", "mountpoint": "...", "read_only": <bool>, "foreground": <bool>, "allow_other": <bool>}` | |
| `clone` | `{"ok": true, "src": "...", "dst": "...", "method": "..."}` | Method reflects OS CoW reflink type |
| `transfer` | See [Transfer JSON](#transfer-json-schemas) below | |
| `filter-analyze` | Pretty-printed `FilterRecommendation` (see below) | |
| `dict train` | `{"ok": true, "dict_id": <u32>, "bytes": <usize>, "output": "..."}` | |
| `resize` | `{"ok": true, "bytes_before": <u64>, "bytes_after": <u64>, "blocks_before": <u64>, "blocks_after": <u64>, "inodes_before": <u64>, "inodes_after": <u64>, "method": "..."}` | |
| `vacuum` | `{"ok": true, "bytes_before": <u64>, "bytes_after": <u64>, "bytes_reclaimed": <u64>, "blocks_before": <u64>, "blocks_after": <u64>, "blocks_relocated": <u64>, "method": "..."}` | |
| Error (any) | `{"ok": false, "error": "<message>"}` | Exit code 1 |

---

### Subcommands

#### `create` — Create a new OIFS image

```bash
oifs -i <image.img> create [--size <MB>] [--encrypt] [--journal]
```

| Option | Description |
| :--- | :--- |
| `--size <MB>` | Size in megabytes (default: 10). |
| `--encrypt` | Enable encryption (will prompt for password via 3-tier hierarchy). |
| `--journal` | Enable the metadata WAL (journaling) for crash-safe metadata updates. Reserves 33 blocks (1 header + 128 KB ring) between the data bitmap and the inode table. Uncommitted transactions are replayed automatically on the next mount, so `oifs fsck` is no longer needed after a power loss. |

**JSON success**: `{"ok": true, "message": "Filesystem created", "journal": true}`

---

#### `put` — Import a file into the image

```bash
oifs -i <image.img> put <host_path> [remote_name] [--compress] [--no-compress] [--chunked] [--filter <mode>] [--typesize <N>]
```

| Argument / Option | Description |
| :--- | :--- |
| `host_path` | File path on host (required). |
| `remote_name` | Destination filename in OIFS (optional, defaults to host filename). |
| `--compress` | Always compress the file regardless of size. |
| `--no-compress` | Never compress the file. |
| `--chunked` / `--seekable` / `--chunked-64k` | Seekable 64KB chunked compression (sets `INODE_FLAG_SEEKABLE_64K`). |
| `--filter <mode>` | Pre-compression filter: `none`, `delta`, `shuffle`, `bitshuffle`, `both` (alias `numeric`), or `auto`. Default: `none`. |
| `--typesize <N>` | Element size in bytes for filter (1, 2, 4, 8). Default: 0 (auto = 4). |

**Compression mode precedence**: `--chunked` > `--compress` > `--no-compress` > `Auto`.

**Filter modes**:
- `none` — No pre-filter.
- `delta` — First-order delta (good for time-series, linear data).
- `shuffle` — Byte shuffle (good for AoS struct arrays).
- `bitshuffle` — Bit-level transpose (good for boolean masks, sparse bits).
- `both` / `numeric` — Delta + byte shuffle (good for numeric arrays).
- `auto` — Analyze file content and recommend optimal filter.

**JSON success**: `{"ok": true, "inode": <u64>, "bytes": <usize>}`

---

#### `filter-analyze` — Analyze a file and recommend optimal compression filters

```bash
oifs filter-analyze <host_path> [--json]
```

Analyzes a host file (no image required) and prints a detailed filter recommendation report including entropy measurements, compression ratios, and the exact `oifs put` command to use.

**JSON output**: Pretty-printed `FilterRecommendation` struct with fields:
- `original_size`, `baseline_compressed_size`, `baseline_entropy`
- `candidates[]` with `label`, `entropy`, `compressed_size`, `compression_ratio`, `space_savings_percent`
- `best_config` — the recommended `FilterConfig`

---

#### `get` — Export a file from the image

```bash
oifs -i <image.img> get <remote_name> [host_path]
```

| Argument | Description |
| :--- | :--- |
| `remote_name` | Filename in OIFS (required). |
| `host_path` | Destination path on host (optional, defaults to remote filename). If host_path is a directory or ends with `/`, the remote filename is appended. |

**JSON success**: `{"ok": true, "bytes": <usize>, "dest": "<resolved_path>"}`

---

#### `cat` — Print file contents to stdout

```bash
oifs -i <image.img> cat <remote_name>
```

| Argument | Description |
| :--- | :--- |
| `remote_name` | Filename in OIFS (required). |

**Behavior**: Writes raw bytes to stdout (no newline added). In `--json` mode, returns a JSON object with the content as UTF-8 lossy string.

**JSON success**: `{"ok": true, "path": "...", "bytes": <usize>, "content": "<utf8_lossy>"}`

---

#### `info` — Display filesystem superblock and configuration

```bash
oifs -i <image.img> info
```

Shows block size, total blocks/bytes, inode capacity, format version, encryption status, journaling status, and read-only state.

---

#### `stat` — Display inode metadata for a file or directory

```bash
oifs -i <image.img> stat <path>
```

| Argument | Description |
| :--- | :--- |
| `path` | Path in OIFS (required). Use `/` for root directory. |

Shows inode number, type (file/directory), size, compressed size, encryption flag, seekable-64k flag, created/modified timestamps.

---

#### `append` — Append content to a file in the image

```bash
oifs -i <image.img> append <remote_name> <content> [--no-newline]
```

| Argument / Option | Description |
| :--- | :--- |
| `remote_name` | Destination filename in OIFS (creates if not exists). |
| `content` | Content to append (required). |
| `--no-newline` | Do not append a newline automatically if content doesn't end with one. |

**JSON success**: `{"ok": true, "inode": <u64>, "total_bytes": <usize>}`

---

#### `mkdir` — Create a directory

```bash
oifs -i <image.img> mkdir <dir_name>
```

| Argument | Description |
| :--- | :--- |
| `dir_name` | Directory name/path (parent must exist). |

**JSON success**: `{"ok": true, "inode": <u64>}`

---

#### `rm` — Delete a file or directory

```bash
oifs -i <image.img> rm <path> [-r|--recursive]
```

| Argument / Option | Description |
| :--- | :--- |
| `path` | Path of the file or directory to delete. |
| `-r`, `--recursive` | Allow deleting a directory that still has contents. |

**Safety**: Never allows deleting the filesystem root. Non-empty directories require `--recursive`.

**JSON success**: `{"ok": true, "removed": "<path>", "inode": <u64>, "recursive": <bool>}`

---

#### `ls` — List files in the root directory or specified path

```bash
oifs -i <image.img> ls [path] [-r|--recursive]
```

| Argument / Option | Description |
| :--- | :--- |
| `path` | Path to list (optional, defaults to root). |
| `-r`, `--recursive` | List recursively. |

**Output columns** (non-JSON): `Type`, `Name`, `Size`, `CompSize`, `Modified`. Type: `d` = directory, `-` = file.

**JSON success**: Array of entry objects (see [JSON schemas](#common-json-success-schemas)).

---

#### `analyze` — Analyze disk fragmentation

```bash
oifs -i <image.img> analyze
```

Reports total/used/free blocks, free runs (gaps), largest free run, average gap size, and fragmentation ratio.

---

#### `defrag` — Defragment the filesystem

```bash
oifs -i <image.img> defrag [--mode <safe|inplace>]
```

| Option | Description |
| :--- | :--- |
| `--mode <safe|inplace>` | Defragmentation mode (default: `safe`). `safe` uses copy-on-write; `inplace` modifies the image directly (⚠️ data loss risk if interrupted). |

**JSON success**: `{"ok": true, "files_processed": <u64>, "bytes_moved": <u64>, "frag_before": <f64>, "frag_after": <f64>}`

---

#### `fsck` — Check filesystem structural consistency

```bash
oifs -i <image.img> fsck
```

Performs a full structural integrity check: orphan inodes, leaked blocks, missing blocks, cross-linked blocks. Returns non-zero exit code if corruption is found.

**JSON output**: Full `IntegrityReport` object with `is_clean` boolean and arrays of issue IDs.

---

#### `migrate` — Upgrade a legacy image to current inode format

```bash
oifs -i <image.img> migrate
```

Upgrades legacy v1 inode format to current fixed-width format. Resumes interrupted migrations automatically.

**JSON success**: `{"ok": true, "from_version": <u32>, "to_version": <u32>, "inodes_rewritten": <u64>, "already_current": <bool>}`

---

#### `truncate` — Truncate or extend a file to specified size

```bash
oifs -i <image.img> truncate <path> --size <bytes>
```

| Argument / Option | Description |
| :--- | :--- |
| `path` | Remote path in OIFS. |
| `-s`, `--size <bytes>` | New size in bytes (required). |

**JSON success**: `{"ok": true, "path": "<path>", "size": <u64>}`

---

#### `mount` — Mount the OIFS image as a FUSE/NFS filesystem

```bash
oifs -i <image.img> mount <mountpoint> [--backend <auto|nfs|fuse>] [--port <N>] [--foreground] [--allow-other]
```

| Argument / Option | Description |
| :--- | :--- |
| `mountpoint` | Directory where the filesystem will be mounted (required). |
| `--backend` | Mount backend: `auto` (macOS: nfs, Linux: fuse), `nfs` (built-in local NFSv3, zero kext), or `fuse`. Default: `auto`. |
| `--port` | Loopback TCP port for NFSv3 backend (0 = OS-assigned ephemeral port). Default: 0. |
| `--foreground` | Run attached in the foreground and log mount details to stderr. |
| `--allow-other` | Allow other users to access the mounted filesystem (`allow_other`, FUSE backend only). |

**Read-only behavior**: If the image was opened with `--read-only` or the filesystem is read-only, the mount is read-only regardless of backend options.

**JSON success**: `{"ok": true, "backend": "nfs\|fuse", "mountpoint": "...", "read_only": <bool>, "foreground": <bool>, "allow_other": <bool>}`

---

#### `clone` — Clone an entire OIFS image using OS CoW reflink

```bash
oifs clone <src> <dst>
```

| Argument | Description |
| :--- | :--- |
| `src` | Source OIFS image path (defaults to `--image` if omitted). |
| `dst` | Destination OIFS image path (must not already exist). |

Uses `APFS clonefile` (macOS) or `FICLONE` (Linux) for instant copy-on-write clones.

**JSON success**: `{"ok": true, "src": "...", "dst": "...", "method": "..."}`

---

#### `transfer` — Fast cross-image file copy or move

```bash
oifs -i <src.img> transfer --to <dst.img> <src_path> [--dest <dst_path>] [--move] [--overwrite] [--to-password <pwd>] [--offset <N>] [--len <N>] [--dst-offset <N>] [--trim-mode <keep|punch-hole|collapse>]
```

**Three-tier transfer mechanism** (Direct sessions only):
1. **FICLONERANGE** — OS reflink for aligned extents (zero-copy).
2. **copy_file_range** — Kernel copy without user-space roundtrip.
3. **Raw block copy** — Direct block-level copy (bypasses decompression/recompression).

Falls back to logical re-encode (read + write) if either session is a Remote IPC proxy.

| Argument / Option | Description |
| :--- | :--- |
| `--to <PATH>` | Destination OIFS image path (required). |
| `src_path` | Source file path inside the source image (required). |
| `--dest <PATH>` | Destination file path inside `--to` image (defaults to `src_path`). |
| `--move` | Remove source file after destination commits and flushes. |
| `--overwrite` | Overwrite existing file at destination. |
| `--to-password <PWD>` | Password for encrypted destination image (optional). |
| `--offset <N>` | Sub-range byte offset in source file (enables `splice_range_to`). |
| `--len <N>` | Sub-range byte length to splice (defaults to remaining source bytes from `--offset`). |
| `--dst-offset <N>` | Sub-range byte offset in destination file (defaults to `--offset` or 0). |
| `--trim-mode <MODE>` | Source trim mode for sub-range splice: `keep` (default), `punch-hole`, `collapse`. |

##### Transfer JSON Schemas

**Full file transfer**:
```json
{
  "ok": true,
  "src_inode": <u64>,
  "dst_inode": <u64>,
  "logical_bytes": <u64>,
  "physical_bytes": <u64>,
  "extents_transferred": <usize>,
  "method": "Ficlonerange|CopyFileRange|RawBlockCopy|LogicalReencode",
  "moved": <bool>
}
```

**Sub-range splice transfer**:
```json
{
  "ok": true,
  "src_inode": <u64>,
  "dst_inode": <u64>,
  "bytes_spliced": <u64>,
  "logical_rmw_bytes": <u64>,
  "body_fast_path_bytes": <u64>,
  "physical_bytes": <u64>,
  "extents_transferred": <u64>,
  "src_blocks_freed": <u64>,
  "method": "Ficlonerange|CopyFileRange|RawBlockCopy|LogicalReencode",
  "trim_mode": "keep|punch-hole|collapse"
}
```

---

#### `dict` — Zstd dictionary utilities

```bash
oifs -i <image.img> dict train [--dir <path>] --output <dict_path> [--max-size <bytes>] [--max-samples <N>]
```

| Subcommand | Description |
| :--- | :--- |
| `train` | Train a Zstd dictionary from sample files stored inside an OIFS directory. |

**`train` options**:
| Option | Description |
| :--- | :--- |
| `--dir <path>` | Directory path inside the image (e.g. "/" or "/logs"). Default: `/`. |
| `-o`, `--output <PATH>` | Host output path for the trained Zstd dictionary (required). |
| `--max-size <bytes>` | Target dictionary size in bytes (256..=16777216). Default: 112640. |
| `--max-samples <N>` | Maximum number of regular files sampled from `--dir`. Default: 256. |

**JSON success**: `{"ok": true, "dict_id": <u32>, "bytes": <usize>, "output": "..."}`

---

#### `resize` — Resize (grow or shrink) an OIFS image container

```bash
oifs -i <image.img> resize [--size <MiB> | --bytes <N>]
```

| Option | Description |
| :--- | :--- |
| `--size <MiB>` | New target image size in MiB. |
| `--bytes <N>` | New target image size in exact bytes (rounded up to 4 KiB blocks). |

**Mutually exclusive**: `--size` and `--bytes` cannot be used together. One is required.

**JSON success**: `{"ok": true, "bytes_before": <u64>, "bytes_after": <u64>, "blocks_before": <u64>, "blocks_after": <u64>, "inodes_before": <u64>, "inodes_after": <u64>, "method": "..."}`

---

#### `vacuum` — Compact used blocks and shrink the image to reclaim unused space

```bash
oifs -i <image.img> vacuum [--headroom-mb <N>] [--headroom-pct <N>] [--min-size-mb <N>] [--no-relayout]
```

| Option | Description |
| :--- | :--- |
| `--headroom-mb <N>` | Minimum free headroom to retain in MiB (default: 1). |
| `--headroom-pct <N>` | Free headroom percentage relative to used data blocks 0..=100 (default: 15). |
| `--min-size-mb <N>` | Minimum image size floor in MiB (0 = no floor, default: 0). |
| `--no-relayout` | Disallow atomic cross-boundary re-layout (in-place shrink only). |

**JSON success**: `{"ok": true, "bytes_before": <u64>, "bytes_after": <u64>, "bytes_reclaimed": <u64>, "blocks_before": <u64>, "blocks_after": <u64>, "blocks_relocated": <u64>, "method": "..."}`

---

### Exit Codes

| Code | Meaning |
| :--- | :--- |
| `0` | Success. |
| `1` | General error (invalid arguments, I/O error, filesystem corruption, etc.). In `--json` mode, error details are printed to stdout as `{"ok": false, "error": "..."}`. |

---

### Error Message Conventions

- **Human mode** (default): Errors printed to `stderr` with `Error: ` prefix. Warnings/notes printed to `stderr` without prefix (e.g., `⚠️ Warning: ...`, `✅ Created...`).
- **JSON mode** (`--json`): All output to `stdout` as JSON. Errors are `{"ok": false, "error": "<message>"}`. Interactive prompts are **suppressed**; missing passwords for encrypted images cause immediate abort with JSON error.
- **Panic handling**: `main` wraps `run_cli` and catches any error, ensuring JSON error format in `--json` mode.

---

## `oifs_mcp` — MCP Server Reference

The `oifs_mcp` binary runs as an MCP (Model Context Protocol) server over stdio, exposing OIFS filesystem operations as tools for AI agents (Cursor, Claude Desktop, etc.).

### Usage

```bash
oifs_mcp [--size <MB>] [IMAGE_PATH]
```

| Option / Argument | Description |
| :--- | :--- |
| `--size <MB>` | Initial size when creating a new image (default: 10 MB). |
| `IMAGE_PATH` | Path to the OIFS image file (default: `agent_memory.img`). |
| `-h`, `--help` | Show usage. |

### Environment

| Variable | Description |
| :--- | :--- |
| `OIFS_PASSWORD` | If set, opens (or creates) the image as encrypted. Required to open an existing encrypted image. |

### Exposed MCP Tools

The server exposes **8 tools** over stdio ([`src/bin/oifs_mcp.rs`](../../src/bin/oifs_mcp.rs)):

| Tool | Description | Parameters |
| :--- | :--- | :--- |
| `write_file` | Write (create or overwrite) a file inside the OIFS sandbox image. Parent directories must already exist. | `path: string`, `content: string` |
| `read_file` | Read the contents of a file inside the OIFS sandbox image. Returns UTF-8 text content. | `path: string` |
| `list_dir` | List files and directories at a given path. Returns JSONL (one JSON object per line). | `path: string` |
| `mkdir` | Create a directory inside the OIFS sandbox image. Parent directories must exist. | `path: string` |
| `delete_file` | Delete a file from the OIFS sandbox image. | `path: string` |
| `truncate_file` | Truncate or extend a file to the specified size in bytes. | `path: string`, `size: number` |
| `append_file` | Append a line to a file (creates if not exists). Ideal for JSONL memory logs. Auto-adds newline if missing. | `path: string`, `content: string` |
| `status` | Show filesystem status: image path, total/used/free blocks, and fragmentation ratio. | (none) |

### MCP Tool Response Format

All tools return JSON strings. Successful responses include `"ok": true` plus tool-specific fields. Errors return `{"ok": false, "error": "..."}`. Responses are built with `serde_json` to ensure proper escaping.

#### Examples

**write_file success**:
```json
{"ok": true, "inode": 123, "bytes": 42}
```

**read_file success**: Returns raw UTF-8 string content (not JSON-wrapped).

**list_dir success** (JSONL):
```json
{"name": "notes", "kind": "dir", "size": 0}
{"name": "todo.txt", "kind": "file", "size": 128}
```

**status success**:
```json
{"image": "agent_memory.img", "total_blocks": 2560, "used_blocks": 100, "free_blocks": 2460, "fragmentation": 0.02}
```

**Error (any tool)**:
```json
{"ok": false, "error": "File not found: notes/missing.txt"}
```

---

## Network Mode and Master-Proxy Coordination

By default, OIFS runs in **Standalone** mode (no IPC). With `--ipc`, it runs in **Local** mode, creating a Unix domain socket under `/tmp/oifs_<name>_<hash>.sock`.

When operating over networked shared storage (NFS, Lustre, AWS EFS, or multi-node clusters):
- Adding `-n` / `--network` or specifying `--bind` switches to `SessionMode::Network`.
- The master binds to the specified address (default `127.0.0.1:0` for random ephemeral port).
- Proxies connect to the master via TCP.
- **Security**: The IPC transport is plain TCP without TLS. Binding to non-loopback addresses requires `--allow-remote-ipc` and should only be done inside a secure tunnel (WireGuard, IPsec, SSH).

---

## Related Pages

- [Configuration](../concepts/configuration.md) — Configuration file and environment variables
- [Quickstart](../quickstart.md) — Getting started with OIFS
- [Basic Operations](../workflows/basic_operations.md) — Common workflow examples
- [FSCK Workflow](../workflows/fsck_workflow.md) — Filesystem consistency checking
- [Architecture](../architecture/overview.md) — System architecture and Session modes
