---
type: workflow
title: Basic File/Directory Operations
description: End-to-end flows for create/read/update/delete files, directory management, metadata inspection, and truncate/extend. Covers Direct (DiskManager) and Remote (IPC) paths, image creation options, read-only mounts, and JSON output mode.
tags: [filesystem, cli, tutorial, basic-operations, architecture]
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
verified:
  - by: openwiki/0.9.0
    at: 2026-11-24T19:09:23.676Z
generated: { by: "openwiki/0.9.0", at: "2026-11-23T20:55:55.458Z" }
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
// Programmatic
let session = OifsSession::open_with_mode(
    "disk.img",
    0,                    // total_size ignored for read-only
    SessionMode::Standalone,
    Some("password"),     // if encrypted
    false,
)?;
// Or use the dedicated read-only opener:
let dm = DiskManager::open_read_only_with_storage_mode(
    "disk.img",
    Some("password"),     // None for unencrypted
    StorageMode::default(),
)?;
let session = OifsSession::Direct { dm: Arc::new(dm), server: None, event_rx: Arc::new(Mutex::new(None)), mode: SessionMode::Standalone };
```

**Constraints:** All mutating operations (`put`, `mkdir`, `rm`, `append`, `truncate`, `write`, `create_file`, etc.) return `DiskManagerError::ReadOnly`. Background auto-defrag is disabled in read-only mode.

---

## Core Operations

All operations are available via both the **CLI** (`oifs` subcommands) and the **Session API** (`OifsSession`). The Session API works identically in Direct and Remote modes.

### Path Resolution

Paths use `/` as separator. No `..` traversal is supported (explicitly rejected). Path components are validated via `directory::validate_filename` — empty names, `.`, `..`, `/`, `\`, and null bytes are rejected; max length 255 bytes.

| API | Purpose |
|-----|---------|
| `resolve_path(path: &str) -> Result<u64>` | Resolves full path (e.g., `"docs/file.txt"`) to inode ID. Splits on `/`, filters empty and `.` components, walks from root. |
| `resolve_parent(path: &str) -> Result<(u64, String)>` | Returns `(parent_inode_id, filename)` for creating/looking up entries. `resolve_parent("a/b/c") → (inode_of_b, "c")`. Root parent is root inode. |

Both APIs exist on `OifsSession` and `DiskManager`. In Remote mode they are IPC requests (`IpcRequest::ResolvePath`, `IpcRequest::ResolveParent`).

---

### `put` — Import a Host File into OIFS

```bash
# CLI: import file.txt to /file.txt (auto-detects dest name)
oifs -i disk.img put ./file.txt

# Import to specific name
oifs -i disk.img put ./file.txt /data/myfile.txt

# Compression control
oifs -i disk.img put ./file.txt --compress        # force Zstd
oifs -i disk.img put ./file.txt --no-compress     # store raw
oifs -i disk.img put ./file.txt --chunked         # seekable 64 KiB chunks

# Pre-compression filters (for structured/columnar data)
oifs -i disk.img put ./data.bin --filter delta --typesize 8
oifs -i disk.img put ./data.bin --filter shuffle --typesize 4
oifs -i disk.img put ./data.bin --filter both --typesize 4
oifs -i disk.img put ./data.bin --filter auto     # analyze & pick best

# JSON output
oifs -i disk.img --json put ./file.txt
# {"ok": true, "inode": 123, "bytes": 4096}
```

```rust
// Session API
let (parent, name) = session.resolve_parent("/data/myfile.txt")?;
let inode_id = session.create_file(parent, &name)?;
let data = std::fs::read("./file.txt")?;
session.write_data_with_filters(
    inode_id,
    0,
    &data,
    CompressionMode::Auto,              // or Always, Never, Seekable { chunk_size, level }
    FilterConfig::none(),               // or FilterConfig::delta(8), .shuffle(4), .numeric(4), .auto()
)?;
session.flush()?;
```

**Details:**

- Creates the file atomically via `create_file_with_data_with_filters` (journaled images: single WAL transaction; small payloads ≤ 8 KiB are data-journaled).
- Compression modes: `Auto` (heuristic), `Always`, `Never`, `Seekable { chunk_size: 64 KiB, level }`, `Stream { level }`.
- Filter pipeline runs **before** compression: delta → shuffle/bitshuffle → Zstd.
- `--filter auto` runs `filters::recommend_filters` on the input bytes and selects the best config (delta, shuffle, both/bitshuffle, or none).

---

### `get` — Export a File from OIFS to Host

```bash
# CLI
oifs -i disk.img get /file.txt                    # writes ./file.txt
oifs -i disk.img get /file.txt ./out.txt          # explicit destination
oifs -i disk.img get /dir/file.txt ./outdir/      # destination is directory
oifs -i disk.img --json get /file.txt
# {"ok": true, "bytes": 4096, "dest": "file.txt"}
```

```rust
// Session API
let inode_id = session.resolve_path("/file.txt")?;
let data = session.read_data(inode_id)?;
std::fs::write("./out.txt", data)?;
```

**Details:** Automatically decompresses, decrypts, and un-applies filters. Validates the remote filename component (rejects path separators, null bytes, etc.).

---

### `cat` — Print File Contents to Stdout

```bash
# CLI (raw bytes to stdout, no newline added)
oifs -i disk.img cat /file.txt

# JSON mode wraps in object with UTF-8 lossy content
oifs -i disk.img --json cat /file.txt
# {"ok": true, "path": "/file.txt", "bytes": 123, "content": "hello world"}
```

```rust
let inode_id = session.resolve_path("/file.txt")?;
let data = session.read_data(inode_id)?;
std::io::stdout().lock().write_all(&data)?;
```

---

### `append` — Append Text to a File

```bash
# CLI (adds newline unless --no-newline)
oifs -i disk.img append /log.txt "new line"
oifs -i disk.img append /log.txt "no newline" --no-newline

# Creates file if it doesn't exist
oifs -i disk.img --json append /new.txt "first line"
```

```rust
let (parent, name) = session.resolve_parent("/log.txt")?;
let inode_id = match session.lookup(parent, &name) {
    Ok(id) => id,
    Err(_) => session.create_file(parent, &name)?,
};
let mut existing = session.read_data(inode_id).unwrap_or_default();
existing.extend_from_slice(b"new line\n");
session.write_data(inode_id, 0, &existing, CompressionMode::Auto)?;
session.flush()?;
```

**Details:** Reads entire file, appends in memory, rewrites from offset 0 (whole-file overwrite). Not an efficient streaming append — use `write_data` with offset for that.

---

### `mkdir` — Create a Directory

```bash
# CLI
oifs -i disk.img mkdir /data
oifs -i disk.img mkdir /data/subdir
oifs -i disk.img --json mkdir /data
# {"ok": true, "inode": 456}
```

```rust
let (parent, name) = session.resolve_parent("/data/subdir")?;
let dir_id = session.create_directory(parent, &name)?;
session.flush()?;
```

**Details:** Validates parent is a directory, name doesn't exist. Creates inode with `FileType::Directory`, zero size, empty block pointers.

---

### `ls` — List Directory Entries

```bash
# CLI (flat, non-recursive by default)
oifs -i disk.img ls
oifs -i disk.img ls /data
oifs -i disk.img ls /data --recursive

# JSON output (array of entries)
oifs -i disk.img --json ls /data
# [
#   {"name": "file.txt", "kind": "f", "size": 1024, "comp_size": 512, "modified": "2024-01-15 14:30:00"},
#   {"name": "subdir", "kind": "d", "size": 0, "comp_size": null, "modified": "2024-01-15 14:25:00"}
# ]
```

```rust
let target = session.resolve_path("/data")?;
let entries = session.list_dir(target)?;
for e in entries {
    let inode = session.read_inode(e.inode)?;
    println!("{}  {}  {}", e.name, inode.size, if inode.mode == FileType::Directory { "d" } else { "f" });
}
```

**Details:** Returns `Vec<DirectoryEntry>` (`inode`, `hash`, `name`). If the directory's `DirIndex` cache is `complete`, results are served from memory without disk reads. First `ls` on a cold directory triggers a full scan and promotes the index to `complete`.

---

### `stat` — Inspect Inode Metadata

```bash
# CLI
oifs -i disk.img stat /file.txt
oifs -i disk.img stat /           # root directory
oifs -i disk.img --json stat /file.txt
# {
#   "ok": true,
#   "path": "/file.txt",
#   "inode": 123,
#   "kind": "file",
#   "size": 1024,
#   "compressed_size": 512,
#   "encrypted": false,
#   "seekable_64k": false,
#   "created": "2024-01-15 14:30:00",
#   "modified": "2024-01-15 14:35:00"
# }
```

```rust
let inode_id = session.resolve_path("/file.txt")?;
let inode = session.read_inode(inode_id)?;
println!("inode: {}, size: {}, compressed: {}, encrypted: {}",
    inode_id, inode.size, inode.compressed_size, inode.encrypted);
```

**Details:** Reads the on-disk inode record. Shows logical size (`size`), compressed on-disk size (`compressed_size`, 0 if uncompressed), encryption flag, seekable-64K flag, timestamps.

---

### `rm` — Delete File or Directory

```bash
# CLI (fails on non-empty directory unless --recursive)
oifs -i disk.img rm /file.txt
oifs -i disk.img rm /empty_dir
oifs -i disk.img rm /non_empty_dir --recursive
oifs -i disk.img --json rm /file.txt
# {"ok": true, "removed": "/file.txt", "inode": 123, "recursive": false}
```

```rust
let (parent, name) = session.resolve_parent("/file.txt")?;
let target = session.lookup(parent, &name)?;
let inode = session.read_inode(target)?;

if inode.mode == FileType::Directory {
    // Check empty (or recursively delete contents)
    let entries = session.list_dir(target)?;
    if !entries.is_empty() {
        // Implement recursive traversal or error
    }
}
session.delete_file(parent, &name)?;
session.flush()?;
```

**Details:** `delete_file` removes the directory entry, frees the target inode and all its data blocks, and invalidates parent & target `DirIndex` cache entries. Journaled images commit via WAL; legacy images write in-place. Root directory deletion is refused.

---

### `truncate` — Resize a File

```bash
# CLI (size in bytes)
oifs -i disk.img truncate /file.txt --size 0        # empty the file
oifs -i disk.img truncate /file.txt --size 1048576  # extend to 1 MiB (sparse for raw)
oifs -i disk.img truncate /file.txt --size 500      # shrink
oifs -i disk.img --json truncate /file.txt --size 0
# {"ok": true, "path": "/file.txt", "size": 0}
```

```rust
session.truncate_path("/file.txt", 0)?;       // by path
session.truncate(inode_id, 1048576)?;         // by inode ID
session.flush()?;
```

**Details:**

| Case | Behavior |
|------|----------|
| **Shrink (raw)** | Frees whole blocks beyond new size; zeros trailing slack in last surviving block. Prunes empty indirect blocks. |
| **Extend (raw)** | Sparse extension — new logical blocks read as zero until written. No data blocks allocated. |
| **Compressed / Encrypted / Filtered** | Reads full decoded content, resizes in memory, re-encodes from offset 0 (whole-file rewrite). |
| **Seekable 64K** | Special chunked path that can truncate individual chunks without full recompression. |

---

## JSON Output Mode (`--json`)

All subcommands emit structured JSON on success or `{"ok": false, "error": "..."}` on failure. Stdout is pure JSON (no prompts, no ANSI).

| Subcommand | Success Schema |
|------------|----------------|
| `create` | `{"ok": true, "message": "...", "journal": boolean}` |
| `put` | `{"ok": true, "inode": <u64>, "bytes": <usize>}` |
| `get` | `{"ok": true, "bytes": <usize>, "dest": "<path>"}` |
| `cat` | `{"ok": true, "path": "...", "bytes": <usize>, "content": "<utf8_lossy>"}` |
| `append` | `{"ok": true, "inode": <u64>, "total_bytes": <usize>}` |
| `mkdir` | `{"ok": true, "inode": <u64>}` |
| `rm` | `{"ok": true, "removed": "<path>", "inode": <u64>, "recursive": boolean}` |
| `ls` | Array of `{"name": "...", "kind": "f\|d", "size": <u64>, "comp_size": <u64|null>, "modified": "YYYY-MM-DD HH:MM:SS"}` |
| `stat` | `{"ok": true, "path": "...", "inode": <u64>, "kind": "file\|directory", "size": <u64>, "compressed_size": <u64>, "encrypted": <bool>, "seekable_64k": <bool>, "created": "...", "modified": "..."}` |
| `info` | `{"ok": true, "image": "...", "block_size": <u32>, "block_count": <u64>, "total_bytes": <u64>, "inode_count": <u64>, "data_block_start": <u64>, "format_version": <u32>, "encrypted": <bool>, "journaled": <bool>, "read_only": <bool>}` |
| `truncate` | `{"ok": true, "path": "...", "size": <u64>}` |

---

## Flush / Persistence

```bash
# CLI: most mutating commands auto-flush; explicit flush not exposed as subcommand
# But you can ensure durability via:
oifs -i disk.img --json info   # forces session flush in CLI
```

```rust
// Session API
session.flush()?;   // blocks until msync/WAL checkpoint completes
```

**Details:** `flush()` acquires a global `sync_mutex` to serialize flushes, snapshots the WAL head, calls `sync_journal_region` (WAL first!), then `backing.flush()`. On journaled images, it checkpoints the WAL up to the snapshot head after the data flush. In Remote mode, `IpcRequest::Flush` is forwarded to the Master.

---

## Directory Index Caching (`DirIndex`)

OIFS maintains a sharded, thread-safe `DirIndex` cache mapping `dir_inode_id → DirIndex`. Each index stores:

| Field | Purpose |
|-------|---------|
| `complete: bool` | `true` once all on-disk entries are in `stored` (and `plain` on encrypted FS). Hits/misses are O(1). |
| `cold_scans: u32` | After 8 cold scans, a full directory scan promotes the index to `complete`. |
| `stored: HashMap<String, (inode, phys_blk, hash)>` | On-disk (ciphertext on encrypted FS) name → location. |
| `plain: HashMap<String, (inode, hash)>` | Plaintext name → location (encrypted FS only; avoids re-encryption on lookup). |
| `tail_offset` | Cached append position for `create_file` fast path. |

**Promotion:** `lookup` caches positive hits immediately. Negative lookups and `list_dir` trigger cold scans; after 8 scans the index becomes `complete`. `create_file`/`delete_file` invalidate the parent's cache (`may_have_holes`, `tail_offset`) and drop the target's index.

---

## Write Data Decision Tree (Advanced)

`DiskManager::write_data_with_filters` selects one of four **WriteCase** variants in a lock-free Stage 1, then commits under exclusive lock in Stage 3:

```text
WriteCase::FullOverwrite      → replace entire payload from offset 0
WriteCase::CompressedAppend   → append fresh Zstd frame to compressed stream (unencrypted, EOF, no filters)
WriteCase::Recompress         → read-modify-rebuild: decode, splice, re-encode from 0
WriteCase::Raw                → direct memcpy into existing/allocated blocks
```

**Stages:**
1. **Stage 1 (shared read lock):** Inspect inode, resolve encryption/filters, decide `WriteCase`, build compressed/encrypted buffer (CPU-heavy).
2. **Stage 2 (zero locks):** Filter → Zstd → XChaCha20-Poly1305 encryption (AAD binds to inode ID).
3. **Stage 3 (exclusive write lock):** Allocate/free blocks, write buffer, update inode metadata, commit (WAL transaction on journaled images).

Seekable 64K chunked files and encrypted partial upgrades use a separate chunked pipeline.

---

## Summary: CLI vs API Mapping

| Operation | CLI Subcommand | Session API |
|-----------|----------------|-------------|
| Create image | `create` | `OifsSession::open_with_mode` / `open_journaled_with_mode` |
| Import file | `put` | `create_file` + `write_data_with_filters` + `flush` |
| Export file | `get` | `resolve_path` + `read_data` |
| Print file | `cat` | `resolve_path` + `read_data` + write to stdout |
| Append text | `append` | `lookup`/`create_file` + `read_data` + `write_data` + `flush` |
| Create dir | `mkdir` | `resolve_parent` + `create_directory` + `flush` |
| List dir | `ls` | `resolve_path` + `list_dir` |
| Inspect metadata | `stat` | `resolve_path` + `read_inode` |
| Delete | `rm` | `resolve_parent` + `lookup` + `delete_file` + `flush` |
| Resize | `truncate` | `truncate_path` / `truncate` + `flush` |
| Path → inode | (implicit) | `resolve_path` |
| Path → parent+name | (implicit) | `resolve_parent` |

All APIs work identically in Direct and Remote (IPC) modes.
