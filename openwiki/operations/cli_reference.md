---
type: operations
title: CLI Reference
description: Comprehensive reference for the oifs command-line interface, detailing global flags, password precedence rules, JSON pipelines, multi-process network coordination, and command specifications.
tags: [cli, commands, clap, json-mode, network-mode, password-precedence, fsck, defrag]
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
generated: { by: "pi", at: "2026-09-29T16:14:34.721Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-09-29T18:26:58.313Z
---

## Overview

<!-- openwiki: broken internal link [src/bin/oifs.rs] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The `oifs` executable ([`src/bin/oifs.rs`](src/bin/oifs.rs)) provides a unified command-line tool for creating, inspecting, modifying, and diagnosing OIFS filesystem images.

<!-- openwiki: broken internal link [Cargo.toml#L16] file "Cargo.toml" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/bin/oifs.rs#L10-L34] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/bin/oifs.rs#L36-L111] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/session.rs] file "src/session.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
It is built with [`clap`](Cargo.toml#L16) using the derive pattern ([`Cli`](src/bin/oifs.rs#L10-L34) and [`Commands`](src/bin/oifs.rs#L36-L111)). The CLI transparently routes operations through the [`OifsSession`](src/session.rs) layer, meaning CLI commands automatically participate in multi-process Master-Proxy concurrency.

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

<!-- openwiki: broken internal link [src/bin/oifs.rs#L149-L183] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
When interacting with encrypted filesystems, `oifs` determines passphrases according to a strict 3-tier precedence hierarchy ([`src/bin/oifs.rs#L149-L183`](src/bin/oifs.rs#L149-L183)):

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
<!-- openwiki: broken internal link [src/bin/oifs.rs#L116-L147] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
3. **Interactive Masked Prompt**: If neither flag nor environment variable is provided, `read_password` ([`src/bin/oifs.rs#L116-L147`](src/bin/oifs.rs#L116-L147)) disables terminal echo (`libc::ECHO`) via `libc::tcgetattr` / `libc::tcsetattr`, safely reading the password while masking input against shoulder-surfing. Prompts are printed to `stderr` so stdout remains clean for shell pipes.
4. **Non-Interactive `--json` Enforcement**: In `--json` mode, interactive prompts are forbidden. If an encrypted image is opened without `--password` or `OIFS_PASSWORD`, the process immediately aborts with:
   ```json
   {"ok": false, "error": "Encrypted filesystem requires --password or OIFS_PASSWORD env in --json mode"}
   ```

### Password confirmation on creation

When creating an encrypted image (`oifs -i disk.img create --encrypt`):
- Interactive mode prompts twice (`Enter password: ` and `Confirm password: `) to prevent accidental typos (`src/bin/oifs.rs#L294-L298`).
- If the password is fewer than 8 characters, a non-blocking warning is emitted to stderr: `⚠️ Warning: Password is shorter than 8 characters` (`src/bin/oifs.rs#L301-L303`).

## JSON output mode (`--json`)

The `--json` flag guarantees machine-readable outputs for automation, AI agent tools, and monitoring scripts:
- **Success Responses**: Emits JSON objects (e.g. `{"ok": true, "message": "..."}`) or formatted diagnostic reports.
<!-- openwiki: broken internal link [src/bin/oifs.rs#L188-L195] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- **Failure Responses**: Intercepts panics and errors in `main` ([`src/bin/oifs.rs#L188-L195`](src/bin/oifs.rs#L188-L195)), emitting `{"ok": false, "error": "<msg>"}` to stdout and exiting with status code 1.

## Network mode and Master-Proxy coordination

By default, OIFS runs in local IPC mode, creating a Unix domain socket under `/tmp/oifs_<name>_<hash>.sock`.

When operating over networked shared storage (NFS, Lustre, AWS EFS, or multi-node clusters):
<!-- openwiki: broken internal link [src/bin/oifs.rs#L208-L214] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
- Adding `-n` / `--network` ([`src/bin/oifs.rs#L208-L214`](src/bin/oifs.rs#L208-L214)) switches to `SessionMode::Network`.
- The first process creates an atomic `<image>.master` rendezvous file containing the TCP address and PID of the Master.
- Secondary processes read the rendezvous file, probe connectivity using ping/pong packets, and connect over TCP.
- The optional `--bind <ADDR>` flag allows binding to explicit network interfaces or port ranges.

## Command reference

### 1. `create`
<!-- openwiki: broken internal link [src/bin/oifs.rs#L47-L54] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Creates a fresh OIFS container image file ([`src/bin/oifs.rs#L47-L54`](src/bin/oifs.rs#L47-L54)):
```bash
oifs -i <image.img> create [--size <MB>] [--encrypt]
```
- `--size <MB>`: Image capacity in megabytes (default: 10 MB).
- `--encrypt`: Enables Argon2id + XChaCha20-Poly1305 encryption.

### 2. `put`
<!-- openwiki: broken internal link [src/bin/oifs.rs#L56-L73] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Imports a file from the host filesystem into the image ([`src/bin/oifs.rs#L56-L73`](src/bin/oifs.rs#L56-L73)):
```bash
oifs -i <image.img> put <host_path> [remote_name] [OPTIONS]
```
- `[remote_name]`: Destination path inside OIFS (defaults to host filename).
- `--compress`: Forces Zstd compression regardless of payload size.
- `--no-compress`: Disables compression completely.
- `--filter <none|delta|shuffle|both|auto>`: Pre-compression data filter.
- `--typesize <1|2|4|8>`: Element width for shuffle/delta transformations.

### 3. `get`
<!-- openwiki: broken internal link [src/bin/oifs.rs#L80-L85] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Exports a file from the image to the host filesystem ([`src/bin/oifs.rs#L80-L85`](src/bin/oifs.rs#L80-L85)):
```bash
oifs -i <image.img> get <remote_name> [host_path]
```

### 4. `append`
<!-- openwiki: broken internal link [src/bin/oifs.rs#L87-L95] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Appends content to a file inside the image ([`src/bin/oifs.rs#L87-L95`](src/bin/oifs.rs#L87-L95)):
```bash
oifs -i <image.img> append <remote_name> <content> [--no-newline]
```
- Automatically creates the file if it does not exist. Appends a trailing newline unless `--no-newline` is passed.

### 5. `mkdir`
<!-- openwiki: broken internal link [src/bin/oifs.rs#L97-L100] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Creates a directory inside the image ([`src/bin/oifs.rs#L97-L100`](src/bin/oifs.rs#L97-L100)):
```bash
oifs -i <image.img> mkdir <dir_name>
```

### 6. `ls`
<!-- openwiki: broken internal link [src/bin/oifs.rs#L39-L45] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Lists entries in a directory ([`src/bin/oifs.rs#L39-L45`](src/bin/oifs.rs#L39-L45)):
```bash
oifs -i <image.img> ls [path] [-r/--recursive]
```
- Displays name, kind (file/dir), logical size, physical compressed size, and ISO-8601 modification time.

### 7. `analyze`
<!-- openwiki: broken internal link [src/bin/oifs.rs#L102] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Measures free-block scatter and fragmentation ratio ([`src/bin/oifs.rs#L102`](src/bin/oifs.rs#L102)):
```bash
oifs -i <image.img> analyze
```

### 8. `defrag`
<!-- openwiki: broken internal link [src/bin/oifs.rs#L104-L108] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Performs safe out-of-place defragmentation ([`src/bin/oifs.rs#L104-L108`](src/bin/oifs.rs#L104-L108)):
```bash
oifs -i <image.img> defrag [--mode <safe|inplace>]
```
- `--mode safe` (default): Executes 3-step atomic rename with `.old` backup.

### 9. `fsck`
<!-- openwiki: broken internal link [src/bin/oifs.rs#L110] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Executes structural consistency scanner ([`src/bin/oifs.rs#L110`](src/bin/oifs.rs#L110)):
```bash
oifs -i <image.img> fsck [--json]
```
- Scans for orphan inodes, leaked blocks, missing blocks, and cross-linked blocks.

### 10. `filter-analyze`
<!-- openwiki: broken internal link [src/bin/oifs.rs#L75-L78] file "src/bin/oifs.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Standalone tool that analyzes host data Shannon entropy and evaluates 14 candidate Blosc2 pipelines in parallel ([`src/bin/oifs.rs#L75-L78`](src/bin/oifs.rs#L75-L78)):
```bash
oifs filter-analyze <host_path> [--json]
```
- Does not require `-i / --image`. Recommends optimal `--filter` and `--typesize` parameters based on entropy reduction.
