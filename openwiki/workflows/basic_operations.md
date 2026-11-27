---
type: "Reference"
title: "Basic File/Directory Operations"
openwiki_generated: true
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---


# Basic File/Directory Operations

This guide documents the end-to-end flows for core filesystem operations in OIFS, covering both **Direct** mode (single-process `DiskManager` with `mmap`) and **Remote** mode (transparent IPC proxy to a Master process). The same high-level API is provided by `OifsSession`; the implementation path is selected at session creation time.

> **Related pages:** [Concurrency & Session](../architecture/concurrency_and_session.md), [Disk Manager & Persistence](../architecture/disk_manager_and_persistence.md), [Inode & Directory](../architecture/inode_and_directory.md), [CLI Reference](../operations/cli_reference.md)

---

## Session Modes

| Mode | Entry Point | Transport | Use Case |
|------|-------------|-----------|----------|
| **Standalone** | `OifsSession::open` / `oifs` CLI without `--ipc`/`--network` | None (direct `mmap`) | Single process, zero overhead |
| **Local (Master)** | `OifsSession::open_local` / `oifs --ipc` | Unix Domain Socket (`/tmp/oifs-<hash>.sock`) | Multi-process on same host |
| **Network (Master)** | `OifsSession::open_network` / `oifs --network` | TCP via rendezvous file (`.<image>.master`) | Multi-node / NFS / Lustre clusters |
| **Remote (Client)** | Same entry points; auto-detects existing Master | UDS or TCP | Additional processes attaching to an already-open image |

`OifsSession` is an enum (`Direct { dm, server?, … }` | `Remote { client, … }`) that implements the full operation set by delegating either to the local `Arc<DiskManager>` or by sending framed `IpcRequest` messages to the Master.

---

## Creating a Filesystem Image

### CLI

```bash
# Create a 10 MB unencrypted image
oifs -i disk.img create --size 10

# Create with metadata journaling (WAL)
oifs -i disk.img create --size 10 --journal

# Create encrypted (prompts for password)
oifs -i disk.img create --size 10 --encrypt

# Create encrypted with journaling
oifs -i disk.img create --size 10 --encrypt --journal

# Non-interactive password via env (CI/CD safe)
OIFS_PASSWORD=secret oifs -i disk.img create --size 10 --encrypt --json
```

### Programmatic (Session API)

```rust
use oifs::{OifsSession, session::SessionMode};

// Unencrypted, no journal
let session = OifsSession::open_with_mode("disk.img", 10 * 1024 * 1024, SessionMode::Standalone, None, false)?;

// Unencrypted, with journal
let session = OifsSession::open_journaled_with_mode("disk.img", 10 * 1024 * 1024, SessionMode::Standalone, None, false)?;

// Encrypted, no journal (password required)
let session = OifsSession::open_with_mode("disk.img", 10 * 1024 * 1024, SessionMode::Standalone, Some("password"), true)?;

// Encrypted, with journal
let session = OifsSession::open_journaled_with_mode("disk.img", 10 * 1024 * 1024, SessionMode::Standalone, Some("password"), true)?;
```

### Key Details

| Option | Description |
|--------|-------------|
| `--size <MB>` | Total image capacity in megabytes (default: 10). Must be ≥ 4 MB for viable layout. |
| `--journal` | Enables metadata WAL. Reserves 33 blocks (1 header + 128 KB ring) between the data bitmap and inode table. Uncommitted transactions replay automatically on next mount — `fsck` not required after power loss. |
| `--encrypt` | Enables XChaCha20-Poly1305 encryption. Password derived via Argon2id. If `--journal` is also set, the WAL region is encrypted too. |
| **Password precedence** | `--password` flag > `OIFS_PASSWORD` env > interactive masked prompt (disabled in `--json` mode). |

---

## Read-Only Mount Workflow

Open an image read-only (shared `F_RDLCK`, zero writes, works on `0444` files):

```bash
# CLI
oifs -i disk.img --read-only info
oifs -i disk.img --read-only cat /file.txt
oifs -i disk.img --read-only get /file.txt ./output.txt

# JSON mode requires env or flag (no interactive prompt)
OIFS_PASSWORD=secret oifs -i encrypted.img --read-only --json cat /file.txt
```

```rust
use oifs::{OifsSession, DiskManager};

// Via DiskManager
let dm = DiskManager::open_read_only("disk.img")?;
let data = dm.read_data(inode_id)?;

// Via OifsSession (read-only via global --read-only flag at CLI layer)
let session = OifsSession::open_with_mode("disk.img", 0, SessionMode::Standalone, None, false)?;
```

---

## Core Operations Walkthrough

This section walks through the complete lifecycle: **create → put → list → stat → get → rename → remove**.

### 1. Create Image

```bash
oifs -i demo.img create --size 20 --journal
```

```rust
let session = OifsSession::open_journaled_with_mode("demo.img", 20 * 1024 * 1024, SessionMode::Standalone, None, false)?;
```

### 2. Import Files (`put`)

```bash
# Basic import (auto-compresses files ≥ 8 KB)
oifs -i demo.img put ./local.txt

# Force compression regardless of size
oifs -i demo.img put ./small.txt --compress

# Never compress
oifs -i demo.img put ./data.bin --no-compress

# Seekable 64 KB chunked compression (random reads, partial updates)
oifs -i demo.img put ./large.log --chunked

# With pre-compression filter (delta for time-series, shuffle for struct arrays)
oifs -i demo.img put ./metrics.bin --filter delta --typesize 8

# Multiple files (batch loop)
for f in *.log; do oifs -i demo.img put "$f"; done
```

```rust
// Single file
let inode = session.put("local.txt", None, CompressionMode::Auto, FilterConfig::none())?;

// Batch import (loop over entries)
for entry in std::fs::read_dir(".")? {
    let path = entry?.path();
    if path.is_file() {
        session.put(&path, None, CompressionMode::Auto, FilterConfig::none())?;
    }
}
```

### 3. List Directory (`ls`)

```bash
# Root directory
oifs -i demo.img ls

# Subdirectory (recursive)
oifs -i demo.img ls /logs --recursive

# JSON output for parsing
oifs -i demo.img ls --json
```

```rust
let entries = session.list_dir(0)?; // root inode = 0
for e in entries {
    println!("{}  {}  {}", e.kind, e.name, e.size);
}
```

### 4. Inspect Metadata (`stat`)

```bash
oifs -i demo.img stat /local.txt
```

```rust
let stat = session.stat("/local.txt")?;
println!("inode={} size={} compressed={} encrypted={} created={} modified={}",
    stat.inode, stat.size, stat.compressed_size, stat.encrypted,
    stat.created_at, stat.modified_at);
```

### 5. Export Files (`get`)

```bash
# Export to current directory with same name
oifs -i demo.img get local.txt

# Export to specific destination
oifs -i demo.img get local.txt ./backup/local.txt

# Export to directory (name preserved)
oifs -i demo.img get local.txt ./backup/
```

```rust
let bytes_written = session.get("/local.txt", "./output.txt")?;
```

### 6. View File Contents (`cat`)

```bash
oifs -i demo.img cat /local.txt
```

```rust
let content = session.cat("/local.txt")?;
// or
let data = session.read_data(inode_id)?;
```

### 7. Rename / Move (`rename`)

```bash
# Rename within same directory
oifs -i demo.img rename /old.txt /new.txt

# Move to subdirectory (directory must exist)
oifs -i demo.img rename /file.txt /subdir/file.txt
```

```rust
session.rename("/old.txt", "/new.txt")?;
```

### 8. Remove Files/Directories (`rm`)

```bash
# Remove file
oifs -i demo.img rm /local.txt

# Remove non-empty directory (recursive)
oifs -i demo.img rm /logs --recursive

# JSON mode for scripting
oifs -i demo.img rm /old.txt --json
```

```rust
session.remove("/local.txt")?;
session.remove_dir("/logs", true)?; // recursive = true
```

---

## Multi-File Batch Operations

### Batch Import with Pattern Matching

```bash
# Import all .txt files
for f in *.txt; do oifs -i demo.img put "$f"; done

# Import with custom names
oifs -i demo.img put ./data.csv --compress
oifs -i demo.img put ./config.json --no-compress
```

### Batch Export

```bash
# Export all files (requires listing first)
oifs -i demo.img ls --json | jq -r '.[] | select(.kind=="f") | .name' | while read name; do
    oifs -i demo.img get "$name" "./export/$name"
done
```

### Programmatic Batch Operations

```rust
use oifs::{OifsSession, CompressionMode, Filters::FilterConfig};
use std::path::Path;

let session = OifsSession::open("demo.img", 0)?;

// Batch put with per-file compression decisions
let files = vec![
    ("small.txt", CompressionMode::Never, FilterConfig::none()),
    ("large.log", CompressionMode::Auto, FilterConfig::none()),
    ("metrics.bin", CompressionMode::Auto, FilterConfig { delta: true, typesize: 8, ..Default::default() }),
];

for (name, comp_mode, filter) in files {
    let path = Path::new(name);
    session.put(path, None, comp_mode, filter)?;
}

// Batch get
let entries = session.list_dir(0)?;
for entry in entries {
    if entry.kind == FileType::File {
        let dest = format!("./export/{}", entry.name);
        session.get(&format!("/{}", entry.name), &dest)?;
    }
}
```

---

## Verification: fsck & Integrity

### CLI

```bash
# Full structural consistency check
oifs -i demo.img fsck

# JSON output for monitoring
oifs -i demo.img fsck --json
```

### Programmatic

```rust
// Via DiskManager
let report = dm.fsck()?;
if !report.is_clean {
    eprintln!("Orphan inodes: {:?}", report.orphan_inodes);
    eprintln!("Leaked blocks: {:?}", report.leaked_blocks);
    eprintln!("Missing blocks: {:?}", report.missing_blocks);
    eprintln!("Cross-linked blocks: {:?}", report.cross_linked_blocks);
    eprintln!("Corrupted inodes: {:?}", report.corrupted_inodes);
}

// Via OifsSession
let report = session.fsck()?;
```

### What fsck Checks

| Check | Description |
|-------|-------------|
| **Orphan inodes** | Allocated inodes not referenced by any directory entry |
| **Leaked blocks** | Data bitmap marks block allocated but no inode maps it |
| **Missing blocks** | Inode maps a block that data bitmap marks free |
| **Cross-linked blocks** | Same block referenced by multiple inodes |
| **Corrupted inodes** | Inode fails format decoding or CRC32C verification |

---

## JSON Output Mode

All subcommands support `--json` for machine-readable output (CI/CD, automation, AI agents):

```bash
# Success format
{"ok": true, "inode": 42, "bytes": 1024}

# Error format (exit code 1)
{"ok": false, "error": "File not found: /missing.txt"}
```

Key behaviors:
- **No interactive prompts** — encrypted images require `--password` or `OIFS_PASSWORD`
- **Structured errors** — consistent `{"ok": false, "error": "..."}` schema
- **All commands emit to stdout** — stderr reserved for warnings

---

## Durability Modes

Control write persistence vs. throughput trade-off:

```rust
use oifs::DurabilityMode;

dm.set_durability_mode(DurabilityMode::Lazy);         // Default: process-safe, ~45x faster bulk writes
dm.set_durability_mode(DurabilityMode::RangeAsync);   // Async range msync (balanced)
dm.set_durability_mode(DurabilityMode::Strict);       // Sync msync per mutation (power-loss safe)
```

| Mode | Sync Behavior | Throughput | Power-Loss Safety |
|------|---------------|------------|-------------------|
| `Lazy` (default) | No per-op sync; OS writeback | Highest | On explicit `flush()` / `Drop` |
| `RangeAsync` | Async `msync(MS_ASYNC)` per mutation | High | Pages scheduled for early writeback |
| `Strict` | Sync `msync(MS_SYNC)` per mutation | Lowest | Guaranteed on physical storage before return |

---

## Complete Example: End-to-End Workflow

```bash
#!/bin/bash
set -e

IMG="workflow.img"

# 1. Create journaled image
oifs -i "$IMG" create --size 50 --journal

# 2. Import files
oifs -i "$IMG" put ./README.md
oifs -i "$IMG" put ./src/main.rs --compress
oifs -i "$IMG" put ./data/metrics.bin --filter delta --typesize 8

# 3. List and inspect
oifs -i "$IMG" ls --recursive
oifs -i "$IMG" stat /README.md

# 4. Export for verification
mkdir -p ./verify
oifs -i "$IMG" get /README.md ./verify/README.md
oifs -i "$IMG" get /main.rs ./verify/main.rs

# 5. Rename
oifs -i "$IMG" rename /README.md /DOCS.md

# 6. Verify fsck clean
oifs -i "$IMG" fsck

# 7. Cleanup (remove image)
rm "$IMG"
```

```rust
use oifs::{OifsSession, SessionMode, CompressionMode, Filters::FilterConfig};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session = OifsSession::open_journaled_with_mode(
        "workflow.img", 50 * 1024 * 1024, SessionMode::Standalone, None, false)?;

    // Put
    session.put(Path::new("README.md"), None, CompressionMode::Auto, FilterConfig::none())?;
    session.put(Path::new("src/main.rs"), None, CompressionMode::Always, FilterConfig::none())?;
    session.put(
        Path::new("data/metrics.bin"),
        None,
        CompressionMode::Auto,
        FilterConfig { delta: true, typesize: 8, ..Default::default() }
    )?;

    // List
    for e in session.list_dir(0)? {
        println!("{} {}", e.kind, e.name);
    }

    // Stat
    let stat = session.stat("/README.md")?;
    println!("size={} compressed={}", stat.size, stat.compressed_size);

    // Get
    session.get("/README.md", "./verify/README.md")?;
    session.get("/main.rs", "./verify/main.rs")?;

    // Rename
    session.rename("/README.md", "/DOCS.md")?;

    // Verify
    let report = session.fsck()?;
    assert!(report.is_clean, "fsck found corruption: {:?}", report);

    // Cleanup
    drop(session);
    std::fs::remove_file("workflow.img")?;
    Ok(())
}
```

---

## Error Handling Quick Reference

| Error | Cause | Resolution |
|-------|-------|------------|
| `PasswordRequired` | Encrypted image opened without password | Provide `--password` or `OIFS_PASSWORD` |
| `DecryptionFailed` | Wrong password or corrupted data | Verify password; check image integrity |
| `NotEncrypted` | Password given for unencrypted image | Remove `--password` / `OIFS_PASSWORD` |
| `ReadOnly` | Mutation attempted on `--read-only` mount | Remove `--read-only` flag |
| `AlreadyExists` | Create file/dir with existing name | Use different name or remove first |
| `NotFound` | Lookup of missing path | Verify path exists with `ls`/`stat` |
| `NoSpace` | Image full (blocks or inodes) | Enable `--auto-grow` or run `vacuum` |
| `Locking` | Another process holds exclusive lock | Wait for other process; check `lsof` |

---

## See Also

- [CLI Reference](../operations/cli_reference.md) — Complete command reference
- [Compression Workflows](../workflows/compression_workflows.md) — Filter pipelines, chunked compression
- [Encrypted Workflows](../workflows/encrypted_workflows.md) — Key management, re-encryption
- [Architecture: Disk Manager](../architecture/disk_manager_and_persistence.md) — Internals of `DiskManager`
- [Architecture: Concurrency & Session](../architecture/concurrency_and_session.md) — IPC, Master/Proxy model
