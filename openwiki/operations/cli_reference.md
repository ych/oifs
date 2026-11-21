---
type: "Reference"
title: "This will FAIL with an error mentioning --allow-remote-ipsc"
openwiki_generated: true
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---


## Overview

The `oifs` executable ([src/bin/oifs.rs](../../src/bin/oifs.rs)) provides a unified command-line tool for creating, inspecting, modifying, and diagnosing OIFS filesystem images.

Built with [`clap`](https://crates.io/crates/clap) using the derive pattern ([`Cli`](repo://src/bin/oifs.rs#L100-L154) and [`Commands`](repo://src/bin/oifs.rs#L156-L212)). The CLI transparently routes operations through the [`OifsSession`](repo://src/session.rs) layer, meaning CLI commands automatically participate in multi-process Master-Proxy concurrency.

## Global options

Global options apply across all subcommands:

| Flag | Long Option | Description |
| :--- | :--- | :--- |
| `-i` | `--image <PATH>` | Path to the target `.img` container file (required for all image commands). |
| `-p` | `--password <STR>` | Passphrase for encrypted images. |
| | `--ipc` | Enables local IPC (Unix Domain Sockets) mode for multi-process concurrency. |
| `-n` | `--network` | Enables cross-machine Network TCP mode instead of local UDS (for NFS, Lustre, multi-node clusters). |
| | `--bind <ADDR>` | Custom host:port to bind in network mode (e.g. `0.0.0.0:9050` or `127.0.0.1:0`). |
| | `--allow-remote-ipsc` | Allow the IPC master to bind a non-loopback address. Only safe when traffic is tunnelled (WireGuard, IPsec, SSH port forwarding); the IPC transport is plain TCP without TLS. |
| | `--json` | Formats all command outputs as structured JSON and disables interactive stdin prompts. |
| | `--auto-defrag [THRESHOLD]` | Enable background automatic defragmentation coordinator. Optional fragmentation threshold (ratio of `(free_runs - 1) / (free_blocks - 1)`) above which a defragmentation pass is triggered. Default: `0.25`. Range: `0.0` (all free space contiguous) to `1.0` (every free block isolated). |
| | `--defrag-interval <SECONDS>` | Interval in seconds between background fragmentation checks. Default: `300` (5 minutes). Minimum: `1`. |
| `-h` | `--help` | Prints help information. |
| `-V` | `--version` | Prints version. |

## Password precedence and security model

When interacting with encrypted filesystems, `oifs` determines passphrases according to a strict 3-tier precedence hierarchy ([repo://src/bin/oifs.rs#L307-L342]):

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
3. **Interactive Masked Prompt**: If neither flag nor environment variable is provided, `read_password` ([repo://src/bin/oifs.rs#L218-L256]) disables terminal echo (`libc::ECHO`) via `libc::tcgetattr` / `libc::tcsetattr`, safely reading the password while masking input against shoulder-surfing. Prompts are printed to `stderr` so stdout remains clean for shell pipes.
4. **Non-Interactive `--json` Enforcement**: In `--json` mode, interactive prompts are forbidden. If an encrypted image is opened without `--password` or `OIFS_PASSWORD`, the process immediately aborts with:
   ```json
   {"ok": false, "error": "Encrypted filesystem requires --password or OIFS_PASSWORD env in --json mode"}
   ```

### Password confirmation on creation

When creating an encrypted image (`oifs -i disk.img create --encrypt`):
- Interactive mode prompts twice (`Enter password: ` and `Confirm password: `) to prevent accidental typos ([repo://src/bin/oifs.rs#L519-L531]).
- If the password is fewer than 8 characters, a non-blocking warning is emitted to stderr: `⚠️ Warning: Password is shorter than 8 characters` ([repo://src/bin/oifs.rs#L533-L535]).

## JSON output mode (`--json`)

The `--json` flag guarantees machine-readable outputs for automation, AI agent tools, and monitoring scripts:

- **Success Responses**: Emits JSON objects (e.g. `{"ok": true, "message": "..."}`) or formatted diagnostic reports.
- **Failure Responses**: Intercepts panics and errors in `main` ([repo://src/bin/oifs.rs#L358-L364]), emitting `{"ok": false, "error": "<msg>"}` to stdout and exiting with status code 1.

### Common JSON schemas

| Subcommand | Success Schema | Notes |
| :--- | :--- | :--- |
| `create` | `{"ok": true, "message": "Filesystem created", "journal": boolean}` | `journal` true if `--journal` was specified |
| `put` | `{"ok": true, "inode": <u64>, "bytes": <usize>}` | |
| `get` | `{"ok": true, "bytes": <usize>, "dest": "<path>"}` | |
| `append` | `{"ok": true, "inode": <u64>, "total_bytes": <usize>}` | |
| `mkdir` | `{"ok": true, "inode": <u64>}` | |
| `rm` | `{"ok": true, "removed": "<path>", "inode": <u64>, "recursive": boolean}` | |
| `ls` | `[{"name": "...", "kind": "f\|d", "size": <u64>, "comp_size": <u64|null>, "modified": "YYYY-MM-DD HH:MM:SS"}, ...]` | Array of entries |
| `analyze` | `{"ok": true, "total_blocks": <u64>, "used_blocks": <u64>, "free_blocks": <u64>, "free_runs": <u64>, "largest_free_run": <u64>, "avg_gap_size": <f64>, "fragmentation_ratio": <f64>}` | |
| `defrag` | `{"ok": true, "files_processed": <u64>, "bytes_moved": <u64>, "frag_before": <f64>, "frag_after": <f64>}` | |
| `fsck` | Full `IntegrityReport` (see [fsck workflow](../workflows/fsck_workflow.md)) | |
| `migrate` | `{"ok": true, "from_version": <u32>, "to_version": <u32>, "inodes_rewritten": <u64>, "already_current": boolean}` | |
| `truncate` | `{"ok": true, "path": "<path>", "size": <u64>}` | |
| `filter-analyze` | Pretty-printed `FilterRecommendation` (see below) | |
| Error (any) | `{"ok": false, "error": "<message>"}` | Exit code 1 |

## Network mode and Master-Proxy coordination

By default, OIFS runs in **Standalone** mode (no IPC). With `--ipc`, it runs in **Local** mode, creating a Unix domain socket under `/tmp/oifs_<name>_<hash>.sock`.

When operating over networked shared storage (NFS, Lustre, AWS EFS, or multi-node clusters):
- Adding `-n` / `--network` or specifying `--bind` ([repo://src/bin/oifs.rs#L402-L410]) switches to `SessionMode::Network`.
- The first process creates an atomic `<image>.master` rendezvous file containing the TCP address and PID of the Master.
- Secondary processes read the rendezvous file, probe connectivity using ping/pong packets, and connect over TCP.
- The optional `--bind <ADDR>` flag allows binding to explicit network interfaces or port ranges.

### `--allow-remote-ipsc` security boundary

The IPC transport is **plain TCP without TLS**. Binding to a non-loopback address (e.g. `0.0.0.0:9050`) exposes the master socket to the network. The CLI **refuses** to bind a non-loopback address unless `--allow-remote-ipsc` is explicitly provided ([repo://src/bin/oifs.rs#L370-L379]):

```bash
# This will FAIL with an error mentioning --allow-remote-ipsc
oifs --image /shared/disk.img --network --bind 0.0.0.0:9050 ls

# This SUCCEEDS (operator explicitly acknowledges the risk)
oifs --image /shared/disk.img --network --bind 0.0.0.0:9050 --allow-remote-ipsc ls
```

**Only use `--allow-remote-ipsc` when the traffic is tunnelled** (WireGuard, IPsec, SSH port forwarding).

## Auto-defragmentation (`--auto-defrag`, `--defrag-interval`)

The background auto-defragmentation coordinator runs on the Master process and periodically evaluates filesystem fragmentation:

- **Threshold** (`--auto-defrag [THRESHOLD]`): Fragmentation ratio above which a defragmentation pass is triggered. The ratio is `(free_runs - 1) / (free_blocks - 1)`. Default: `0.25`. Value of `0.0` means all free space is contiguous; `1.0` means every free block is isolated.
- **Interval** (`--defrag-interval <SECONDS>`): Seconds between checks. Default: `300` (5 minutes). Minimum: `1`.

Example:
```bash
# Run in background with auto-defrag enabled, checking every 60s, triggering at 30% fragmentation
oifs -i disk.img --auto-defrag 0.30 --defrag-interval 60 --ipc ls &
```

The coordinator only runs on the Master. If invoked on a Proxy (client) session, a warning is emitted to stderr (unless `--json`).

## Command reference

### 1. `create`
Creates a fresh OIFS container image file ([repo://src/bin/oifs.rs#L498-L575]):
```bash
oifs -i <image.img> create [--size <MB>] [--encrypt] [--journal]
```
| Argument | Description |
| :--- | :--- |
| `--size <MB>` | Image capacity in megabytes (default: `10`). |
| `--encrypt` | Enables Argon2id + XChaCha20-Poly1305 encryption. |
| `--journal` | Enable the metadata WAL (journaling) for crash-safe metadata updates. Reserves 33 blocks (1 header + 128 KB ring) between the data bitmap and the inode table. |

**Examples:**
```bash
# Basic 100MB image
oifs -i disk.img create --size 100

# Encrypted image with journaling (prompts for password interactively)
oifs -i secure.img create --size 50 --encrypt --journal

# Encrypted image for CI/CD (password from env)
OIFS_PASSWORD="secret" oifs -i ci.img create --size 20 --encrypt --json
```

---

### 2. `put`
Imports a file from the host filesystem into the image ([repo://src/bin/oifs.rs#L577-L708]):
```bash
oifs -i <image.img> put <host_path> [remote_name] [OPTIONS]
```
| Argument | Description |
| :--- | :--- |
| `host_path` | File path on host (required). |
| `remote_name` | Destination path inside OIFS (defaults to host filename). |
| `--compress` | Forces Zstd compression regardless of payload size. |
| `--no-compress` | Disables compression completely. |
| `--chunked` / `--seekable` / `--chunked-64k` | Seekable 64KB chunked compression (Blosc2 seekable frames). |
| `--filter <none\|delta\|shuffle\|bitshuffle\|both\|numeric\|auto>` | Pre-compression data filter (default: `none`). |
| `--typesize <1\|2\|4\|8>` | Element width for shuffle/delta transformations (default: `4`). |

**Filter modes:**
- `none` — No pre-filter.
- `delta` — First-order difference (good for time-series, monotonic data).
- `shuffle` — Byte shuffle (good for structured records / AoS layouts).
- `bitshuffle` — Bit-level transpose (good for sparse bitmasks, boolean arrays).
- `both` / `numeric` — Delta + byte shuffle combined.
- `auto` — Analyze content and pick the best filter automatically.

**Examples:**
```bash
# Simple import
oifs -i disk.img put ./data.bin

# Import with explicit name and forced compression
oifs -i disk.img put ./data.bin archive.dat --compress

# Import with auto-selected filter
oifs -i disk.img put ./sensors.csv --filter auto

# Import with specific filter and typesize
oifs -i disk.img put ./array.dat --filter shuffle --typesize 8
```

---

### 3. `filter-analyze`
Standalone tool that analyzes host data Shannon entropy and evaluates 14 candidate Blosc2 pipelines in parallel ([repo://src/bin/oifs.rs#L170-L198], [repo://src/bin/oifs.rs#L413-L495]):
```bash
oifs filter-analyze <host_path> [--json]
```
- Does **not** require `-i / --image`.
- Recommends optimal `--filter` and `--typesize` parameters based on entropy reduction.

**Example output (human-readable):**
```
=== OIFS Filter Recommendation Report for "data.bin" ===
Original Size:        1048576 bytes
Baseline Zstd Size:   128450 bytes (Entropy: 0.981 bits/byte)

Filter Pipeline              Entropy   Zstd Size    Ratio   Savings
--------------------------------------------------------------------------------
none                         0.981     128450      8.16x   87.8%
delta (ts=4)                 0.112      12450     84.22x   98.8% [*RECOMMENDED*]
shuffle (ts=4)               0.892      98200     10.68x   90.6%
...

Recommendation Rationale: Data displays strong linear/temporal correlation; first-order delta collapses dynamic range, shrinking entropy.
Recommended Blosc2 Filter(s): ["blosc2::Filter::Delta"]
Command to import with recommended filter:
  oifs -i <image.img> put "data.bin" --filter delta --typesize 4
```

**JSON output (`--json`):** Returns a `FilterRecommendation` object with `original_size`, `baseline_compressed_size`, `baseline_entropy`, `candidates` (array of pipeline results), and `best_config`.

---

### 4. `get`
Exports a file from the image to the host filesystem ([repo://src/bin/oifs.rs#L710-L780]):
```bash
oifs -i <image.img> get <remote_name> [host_path]
```
| Argument | Description |
| :--- | :--- |
| `remote_name` | Filename or path inside OIFS (required). |
| `host_path` | Destination path on host (optional). If omitted, uses the remote filename in the current directory. If a directory is specified (trailing slash or existing dir), the file is placed inside it. |

**Examples:**
```bash
# Export to current directory with same name
oifs -i disk.img get data.bin

# Export to specific path
oifs -i disk.img get data.bin /tmp/exported.bin

# Export to a directory (creates data.bin inside it)
oifs -i disk.img get data.bin /tmp/out/
```

---

### 5. `append`
Appends content to a file inside the image ([repo://src/bin/oifs.rs#L782-L835]):
```bash
oifs -i <image.img> append <remote_name> <content> [--no-newline]
```
| Argument | Description |
| :--- | :--- |
| `remote_name` | Destination filename in OIFS (created if it doesn't exist). |
| `content` | String content to append. |
| `--no-newline` | Do not append a trailing newline automatically. |

**Examples:**
```bash
# Append with automatic newline
oifs -i disk.img append log.txt "New log entry"

# Append without newline
oifs -i disk.img append log.txt "Partial line" --no-newline
```

---

### 6. `mkdir`
Creates a directory inside the image ([repo://src/bin/oifs.rs#L837-L870]):
```bash
oifs -i <image.img> mkdir <dir_name>
```
| Argument | Description |
| :--- | :--- |
| `dir_name` | Directory path to create (parent directories must exist). |

**Example:**
```bash
oifs -i disk.img mkdir docs
oifs -i disk.img mkdir docs/2024
```

---

### 7. `rm`
Deletes a file or directory inside the image ([repo://src/bin/oifs.rs#L872-L980]):
```bash
oifs -i <image.img> rm <path> [-r/--recursive]
```
| Argument | Description |
| :--- | :--- |
| `path` | Path of the file or directory to delete. |
| `-r`, `--recursive` | Allow deleting a directory that still has contents (recursive traversal). |

**Safety guards:**
- Refuses to delete the filesystem root.
- For non-empty directories, requires `--recursive`.
- Cycle detection with max traversal depth of 256.

**Example:**
```bash
oifs -i disk.img rm old_file.txt
oifs -i disk.img rm old_dir --recursive
```

---

### 8. `ls`
Lists entries in a directory ([repo://src/bin/oifs.rs#L982-L1090]):
```bash
oifs -i <image.img> ls [path] [-r/--recursive]
```
| Argument | Description |
| :--- | :--- |
| `path` | Path to list (optional, defaults to root). |
| `-r`, `--recursive` | List recursively. |

**Output columns (human mode):** Name, Type (d/f), Logical Size, Compressed Size (or `-`), Modified (ISO-8601 local time).

**JSON output:** Array of objects with fields `name`, `kind` (`"f"` or `"d"`), `size` (u64), `comp_size` (u64 or null), `modified` (`"YYYY-MM-DD HH:MM:SS"`).

**Example:**
```bash
oifs -i disk.img ls -r
oifs -i disk.img --json ls docs
```

---

### 9. `analyze`
Measures free-block scatter and fragmentation ratio ([repo://src/bin/oifs.rs#L1092-L1145]):
```bash
oifs -i <image.img> analyze
```
**Output:** Total blocks, used blocks, free blocks, free runs (gaps), largest free run, average gap size, fragmentation ratio (percentage).

**JSON output:** Object with fields `total_blocks`, `used_blocks`, `free_blocks`, `free_runs`, `largest_free_run`, `avg_gap_size`, `fragmentation_ratio`.

---

### 10. `defrag`
Performs safe out-of-place defragmentation ([repo://src/bin/oifs.rs#L1147-L1215]):
```bash
oifs -i <image.img> defrag [--mode <safe|inplace>]
```
| Argument | Description |
| :--- | :--- |
| `--mode safe` (default) | Executes 3-step atomic rename with `.old` backup (crash-safe). |
| `--mode inplace` | Directly modifies the image in place. **WARNING:** Data loss may occur if interrupted. Requires 3-second confirmation delay in interactive mode. |

**JSON output:** Object with `files_processed`, `bytes_moved`, `frag_before`, `frag_after`.

---

### 11. `fsck`
Executes structural consistency scanner ([repo://src/bin/oifs.rs#L1278-L1330]):
```bash
oifs -i <image.img> fsck [--json]
```
Scans for:
- Orphan inodes (allocated but not reachable from root)
- Leaked blocks (marked used but not referenced by any inode)
- Missing blocks (referenced by inode but marked free)
- Cross-linked blocks (referenced by multiple inodes)

**JSON output:** Full `IntegrityReport` with boolean `is_clean` and arrays of offending IDs.

**Exit codes:** 0 if clean, 1 if corrupted (unless `--json` where exit code is always 0 and `is_clean` field indicates status).

---

### 12. `migrate`
Upgrades a legacy image to the current inode format ([repo://src/bin/oifs.rs#L1217-L1275]):
```bash
oifs -i <image.img> migrate
```
- Resumes interrupted migrations automatically.
- No-op if already at current format version.

**JSON output:** Object with `from_version`, `to_version`, `inodes_rewritten`, `already_current`.

---

### 13. `truncate`
Truncates or extends a file to the specified size ([repo://src/bin/oifs.rs#L1332-L1345]):
```bash
oifs -i <image.img> truncate <path> --size <bytes>
```
| Argument | Description |
| :--- | :--- |
| `path` | Remote path in OIFS. |
| `--size <bytes>` | New size in bytes (required). |

**Example:**
```bash
# Truncate to 1KB
oifs -i disk.img truncate large.dat --size 1024

# Extend with zero-padding
oifs -i disk.img truncate grow.dat --size 1048576
```

**JSON output:** `{"ok": true, "path": "<path>", "size": <bytes>}`

## Exit codes

| Code | Meaning |
| :--- | :--- |
| `0` | Success (or clean fsck). |
| `1` | General error (invalid arguments, I/O error, corruption detected by fsck, password mismatch, etc.). |
| `>1` | Not used by `oifs` (reserved for signal termination). |

## Related documentation

- [Configuration](../concepts/configuration.md) — Environment variables and session modes.
- [Quickstart](../quickstart.md) — End-to-end tutorial.
- [Basic Operations](../workflows/basic_operations.md) — Common workflows.
- [Compression Workflows](../workflows/compression_workflows.md) — Filter selection and tuning.
- [Defragmentation Workflow](../workflows/defragmentation_workflow.md) — Analyze, defrag, auto-defrag.
- [Encrypted Workflows](../workflows/encrypted_workflows.md) — Password management and key derivation.
- [FSCK Workflow](../workflows/fsck_workflow.md) — Integrity report schema and repair strategies.
