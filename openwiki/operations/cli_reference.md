---
type: operations
title: CLI Reference
description: Comprehensive reference for the oifs command-line interface, detailing global flags, password precedence rules, JSON pipelines, multi-process network coordination, and command specifications.
tags: [cli, commands, clap, json-mode, network-mode, password-precedence, fsck, defrag]
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-05T16:55:45.523Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-05T16:55:45.523Z
---

## Overview

The `oifs` executable ([src/bin/oifs.rs](../..//src/bin/oifs.rs)) provides a unified command-line tool for creating, inspecting, modifying, and diagnosing OIFS filesystem images.

<!-- openwiki: broken internal link [Cargo.toml#L16] file "Cargo.toml" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/bin/oifs.rs#L10-L34] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/bin/oifs.rs#L36-L128] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/session.rs] file "src/session.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
It is built with [`clap`](Cargo.toml#L16) using the derive pattern ([`Cli`](src/bin/oifs.rs#L10-L34)) and [`Commands`](src/bin/oifs.rs#L36-L128)). The CLI transparently routes operations through the [`OifsSession`](src/session.rs) layer, meaning CLI commands automatically participate in multi-process Master-Proxy concurrency.

## Global options

Global options apply across all subcommands:

| Flag | Long Option | Description |
| :--- | :--- | :--- |
| `-i` | `--image <PATH>` | Path to the target `.img` container file (required for all image commands). |
| `-p` | `--password <STR>`| Passphrase for encrypted images. |
| `-n` | `--network` | Enables cross-machine Network TCP mode instead of local Unix Domain Sockets (UDS). |
| | `--bind <ADDR>` | Custom host:port to bind in network mode (e.g. `0.0.0.0:9050` or `127.0.0.1:0`). |
| | `--json` | Formats all command outputs as structured JSON and disables interactive stdin prompts. |
| `-h` | `--help` | Prints help information. |
| `-V` | `--version` | Prints version. |

## Password precedence and security model

When interacting with encrypted filesystems, `oifs` determines passphrases according to a strict 3-tier precedence hierarchy ([src/bin/oifs.rs#L166-L192]):

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
3. **Interactive Masked Prompt**: If neither flag nor environment variable is provided, `read_password` ([src/bin/oifs.rs#L130-L164]) disables terminal echo (`libc::ECHO`) via `libc::tcgetattr` / `libc::tcsetattr`, safely reading the password while masking input against shoulder-surfing. Prompts are printed to `stderr` so stdout remains clean for shell pipes.
4. **Non-Interactive `--json` Enforcement**: In `--json` mode, interactive prompts are forbidden. If an encrypted image is opened without `--password` or `OIFS_PASSWORD`, the process immediately aborts with:
   ```json
   {"ok": false, "error": "Encrypted filesystem requires --password or OIFS_PASSWORD env in --json mode"}
   ```

### Password confirmation on creation

When creating an encrypted image (`oifs -i disk.img create --encrypt`):
- Interactive mode prompts twice (`Enter password: ` and `Confirm password: `) to prevent accidental typos ([src/bin/oifs.rs#L352-L378]).
- If the password is fewer than 8 characters, a non-blocking warning is emitted to stderr: `⚠️ Warning: Password is shorter than 8 characters` ([src/bin/oifs.rs#L380-L382]).

## JSON output mode (`--json`)

The `--json` flag guarantees machine-readable outputs for automation, AI agent tools, and monitoring scripts:
- **Success Responses**: Emits JSON objects (e.g. `{"ok": true, "message": "..."}`) or formatted diagnostic reports.
- **Failure Responses**: Intercepts panics and errors in `main` ([src/bin/oifs.rs#L218-L228]), emitting `{"ok": false, "error": "<msg>"}` to stdout and exiting with status code 1.

## Network mode and Master-Proxy coordination

By default, OIFS runs in local IPC mode, creating a Unix domain socket under `/tmp/oifs_<name>_<hash>.sock`.

When operating over networked shared storage (NFS, Lustre, AWS EFS, or multi-node clusters):
- Adding `-n` / `--network` ([src/bin/oifs.rs#L241-L247]) switches to `SessionMode::Network`.
- The first process creates an atomic `<image>.master` rendezvous file containing the TCP address and PID of the Master.
- Secondary processes read the rendezvous file, probe connectivity using ping/pong packets, and connect over TCP.
- The optional `--bind <ADDR>` flag allows binding to explicit network interfaces or port ranges.

## Command reference

### 1. `create`
Creates a fresh OIFS container image file ([src/bin/oifs.rs#L342-L438]):
```bash
oifs -i <image.img> create [--size <MB>] [--encrypt] [--journal]
```
- `--size <MB>`: Image capacity in megabytes (default: 10 MB).
- `--encrypt`: Enables Argon2id + XChaCha20-Poly1305 encryption.
- `--journal`: Enable the metadata WAL (journaling) for crash-safe metadata updates.

### 2. `put`
Imports a file from the host filesystem into the image ([src/bin/oifs.rs#L440-L512]):
```bash
oifs -i <image.img> put <host_path> [remote_name] [OPTIONS]
```
- `[remote_name]`: Destination path inside OIFS (defaults to host filename).
- `--compress`: Forces Zstd compression regardless of payload size.
- `--no-compress`: Disables compression completely.
- `--filter <none|delta|shuffle|both|auto>`: Pre-compression data filter.
- `--typesize <1|2|4|8>`: Element width for shuffle/delta transformations.

### 3. `get`
Exports a file from the image to the host filesystem ([src/bin/oifs.rs#L559-L586]):
```bash
oifs -i <image.img> get <remote_name> [host_path]
```

### 4. `append`
Appends content to a file inside the image ([src/bin/oifs.rs#L514-L557]):
```bash
oifs -i <image.img> append <remote_name> <content> [--no-newline]
```
- Automatically creates the file if it does not exist. Appends a trailing newline unless `--no-newline` is passed.

### 5. `mkdir`
Creates a directory inside the image ([src/bin/oifs.rs#L588-L615]):
```bash
oifs -i <image.img> mkdir <dir_name>
```

### 6. `ls`
Lists entries in a directory ([src/bin/oifs.rs#L38-L45]):
```bash
oifs -i <image.img> ls [path] [-r/--recursive]
```
- Displays name, kind (file/dir), logical size, physical compressed size, and ISO-8601 modification time.

### 7. `analyze`
Measures free-block scatter and fragmentation ratio ([src/bin/oifs.rs#L116-L117]):
```bash
oifs -i <image.img> analyze
```

### 8. `defrag`
Performs safe out-of-place defragmentation ([src/bin/oifs.rs#L118-L122]):
```bash
oifs -i <image.img> defrag [--mode <safe|inplace>]
```
- `--mode safe` (default): Executes 3-step atomic rename with `.old` backup.

### 9. `fsck`
Executes structural consistency scanner ([src/bin/oifs.rs#L123-L125]):
```bash
oifs -i <image.img> fsck [--json]
```
- Scans for orphan inodes, leaked blocks, missing blocks, and cross-linked blocks.

### 10. `filter-analyze`
Standalone tool that analyzes host data Shannon entropy and evaluates 14 candidate Blosc2 pipelines in parallel ([src/bin/oifs.rs#L249-L332]):
```bash
oifs filter-analyze <host_path> [--json]
```
- Does not require `-i / --image`. Recommends optimal `--filter` and `--typesize` parameters based on entropy reduction.

### 11. `rm`
Deletes a file or directory inside the image ([src/bin/oifs.rs#L108-L115]):
```bash
oifs -i <image.img> rm <path> [-r/--recursive]
```
- `--recursive`: Allow deleting a directory that still has contents.

### 12. `migrate`
Upgrades a legacy image to the current inode format ([src/bin/oifs.rs#L126-L127]):
```bash
oifs -i <image.img> migrate
```
