---
type: "Reference"
title: "NFSv3 Server"
description: "Pure-Rust local loopback NFSv3 server implementing ONC RPC v2, XDR, MOUNT v3, and NFS v3 procedures for zero-kext mounting on macOS and unprivileged Linux."
tags: ["nfs", "rpc", "xdr", "mount", "loopback", "network-filesystem"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
  - id: openwiki-source-3631336be3d6ebf7c8d4dd97
    resource: repo://src/nfs.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

# NFS v3 Server

OIFS embeds a pure-Rust, stateless NFSv3 server (`src/nfs.rs`) that enables **zero-dependency, kext-free, reboot-free native OS mounting** on macOS (via the built-in `mount_nfs` client) and on Linux (via `mount -t nfs`). Both **MOUNT v3** (program `100005`) and **NFS v3** (program `100003`) are multiplexed over a single loopback TCP listener (`127.0.0.1:<port>`) without any `rpcbind`/`portmap` daemon.

## Design Goals

| Goal | Implementation |
|------|----------------|
| **Zero external C/Go dependencies** | Pure Rust ONC RPC v2 (RFC 5531), XDR (RFC 4506), MOUNT v3 (RFC 1813 Appendix I), NFS v3 (RFC 1813) |
| **Single loopback TCP port** | Ephemeral port on `127.0.0.1` shared by both MOUNT and NFS programs |
| **No root / no kext** | macOS `mount_nfs -o port=P,mountport=P 127.0.0.1:/ <mp>` works as unprivileged user |
| **Single OS FD preserved** | Stateless 16-byte file handles (`[fs_generation, inode_id]`) map directly to OIFS inodes without per-file OS descriptors |
| **Full OIFS parity** | Works with `Mmap`/`FileIo`, plain/journaled/encrypted, compressed files, strict `--read-only` mounts |

## Why NFSv3 Over NFSv4 for Local Loopback

| Dimension | NFSv3 (Chosen) | NFSv4 |
|-----------|----------------|-------|
| **State model** | Stateless (`filehandle + offset + count`) | Stateful (`OPEN`, `stateid`, leases, grace periods) |
| **Complexity** | ~4× simpler to implement and verify | Requires `SETCLIENTID`, `OPEN_CONFIRM`, `seqid` |
| **Loopback latency** | 1 RPC/op; loopback RTT ~10–20 µs | `COMPOUND` helps WAN, marginal on loopback |
| **File locking** | `nolocks` → local kernel enforces `flock`/`fcntl` | Built-in `LOCK`/`LOCKU` for multi-host NAS |
| **macOS client** | Battle-tested `mount_nfs` reliability | v4.0 only; idmapping/Finder edge cases |
| **Formal verification** | Small bounded wire format | State-space explosion impractical |

> The ONC RPC v2 framing and XDR codec are shared, leaving NFSv4 extensibility open if multi-host mounting is ever required.

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                    OifsNfsServer (src/nfs.rs)                   │
├─────────────────────────────────────────────────────────────────┤
│  ┌─────────────────┐    ┌──────────────────────────────────┐   │
│  │  TCP Listener   │───▶│  Accept Loop (background thread) │   │
│  │  127.0.0.1:port │    │  (MAX_CONCURRENT_NFS_CONNS=256)  │   │
│  └─────────────────┘    └──────────────────────────────────┘   │
│                              │                                  │
│                    ┌─────────┴─────────┐                        │
│                    ▼                   ▼                        │
│          ┌─────────────────┐   ┌─────────────────┐             │
│          │ Worker Thread 1 │   │ Worker Thread N │             │
│          │ (per-connection)│   │ (per-connection)│             │
│          └────────┬────────┘   └────────┬────────┘             │
│                   │                     │                       │
│                   └──────────┬──────────┘                       │
│                              ▼                                  │
│                 ┌──────────────────────┐                        │
│                 │ OifsNfsFs (dispatch) │                        │
│                 │  Arc<DiskManager>    │                        │
│                 └──────────────────────┘                        │
└─────────────────────────────────────────────────────────────────┘
```

### Key Types

| Type | Purpose |
|------|---------|
| `OifsNfsServer` | Embedded loopback TCP server; binds `127.0.0.1:port`, spawns accept + worker threads |
| `OifsNfsFs` | Stateless protocol dispatcher implementing MOUNT v3 + NFS v3 procedures |
| `NfsMountOptions` | Configuration (read-only, port, compression, macOS metadata filtering, uid/gid) |
| `NfsFileHandle` | 16-byte stateless handle: `[fs_generation: u64 LE, inode_id: u64 LE]` |
| `XdrReader` / `XdrWriter` | Panic-free bounded XDR codec with 4-byte alignment and 2 MiB frame cap |
| `RpcCallHeader` | Parsed ONC RPC v2 CALL header (xid, rpcvers=2, prog, vers, proc, cred) |

---

## ONC RPC v2 / XDR Foundation

### XDR Codec (`XdrReader` / `XdrWriter`)

The codec enforces **panic-free bounds checking** on every read/write:

- **4-byte alignment**: `(len + 3) & !3` padding for all opaque/string payloads
- **Hard caps**: `NFS_MAX_FRAME_LEN = 2 MiB` per reassembled RPC record; `NFS_MAX_IO_SIZE = 1 MiB` per READ/WRITE
- **Malformed input handling**: Returns `None`/`GARBAGE_ARGS` instead of panicking

```rust
// Example: bounded opaque read with alignment
pub fn read_opaque(&mut self, max_len: usize) -> Option<&'a [u8]> {
    let len = self.read_u32()? as usize;
    if len > max_len { return None; }
    self.read_fixed_opaque(len)  // advances past padded boundary
}
```

### TCP Record Marking (RFC 5531)

Each RPC message is framed as one or more fragments:

```
┌─────────────────────────┐
│ Fragment Header (4 B)   │  bit 31 = last_fragment, bits 0..30 = length
├─────────────────────────┤
│ Fragment Data (len)     │
└─────────────────────────┘
```

`read_rpc_record` reassembles up to `MAX_RPC_FRAGMENTS = 64` fragments into a single message ≤ `NFS_MAX_FRAME_LEN`.

### CALL Header Parsing

`RpcCallHeader::parse` extracts and validates:

```rust
pub struct RpcCallHeader {
    pub xid: u32,
    pub rpcvers: u32,        // must be 2
    pub prog: u32,           // NFS_PROGRAM (100003) or MOUNT_PROGRAM (100005)
    pub vers: u32,           // must be 3
    pub proc_num: u32,       // procedure number
    pub cred_flavor: u32,    // AUTH_NONE (0) or AUTH_SYS (1)
}
```

- Credentials/verifiers are consumed (up to 400 bytes each per RFC 5531) but only `AUTH_NONE` and `AUTH_SYS` flavors are accepted.
- `AUTH_SYS` (UNIX-style uid/gid) is parsed but not yet enforced for authorization; the server trusts loopback client identity.
- Invalid/malformed records → `RPC_ACCEPT_GARBAGE_ARGS`; wrong `rpcvers` → `RPC_REJECT_RPC_MISMATCH`; unknown program → `RPC_ACCEPT_PROG_UNAVAIL`.

---

## Stateless 16-Byte File Handle

```
┌────────────────────────────────────────────────────────┐
│  NfsFileHandle (16 bytes)                              │
├────────────────────────────────────────────────────────┤
│  Bytes 0..7   : fs_generation (u64 LE)  ── per-mount   │
│  Bytes 8..15  : inode_id      (u64 LE)  ── OIFS inode  │
└────────────────────────────────────────────────────────┘
```

- **`fs_generation`**: Generated once per mount session via `generate_session_generation()` (nanoseconds ^ PID ^ constant). Prevents stale-handle collisions across remounts → `NFS3ERR_STALE`.
- **Root handle**: `inode_id = 0`.
- **Round-trip**: `to_bytes()` / `from_bytes()` is a lossless bijection verified by Kani proofs.
- **Validation on decode**: Checks `fs_generation` matches current session AND `DiskManager::is_inode_allocated(inode_id)`.

---

## MOUNT v3 (Program 100005, Version 3)

| Procedure | Number | Behavior |
|-----------|--------|----------|
| `MNTPROC3_NULL` | 0 | Health check, returns `RPC_ACCEPT_SUCCESS` |
| `MNTPROC3_MNT` | 1 | Validates export path (`/` or `/oifs`); returns `MNT3_OK` + root file handle + `[AUTH_NONE, AUTH_SYS]` flavors |
| `MNTPROC3_DUMP` | 2 | Returns empty mountlist |
| `MNTPROC3_UMNT` | 3 | Consumes path argument, returns success |
| `MNTPROC3_UMNTALL` | 4 | Returns success |
| `MNTPROC3_EXPORT` | 5 | Advertises single export entry `/` with no group restrictions |

> The server exports **only `/`** (or `/oifs` for backwards compatibility) and binds strictly to `127.0.0.1`.

---

## NFS v3 Procedures (Program 100003, Version 3)

All procedures validate the file handle (`decode_handle` → `NFS3ERR_STALE`/`NFS3ERR_BADHANDLE`) and enforce **read-only mode** (`NFS3ERR_ROFS` on mutations) before delegating to `DiskManager`.

### Core Attribute & Lookup Procedures

| Proc | Name | Key Behavior |
|------|------|--------------|
| `NFSPROC3_NULL` | 0 | Health check |
| `NFSPROC3_GETATTR` | 1 | Returns `fattr3` mapping OIFS `Inode` → type, mode, nlink, uid, gid, size, used, fsid, fileid, atime/mtime/ctime |
| `NFSPROC3_SETATTR` | 2 | Supports `size` (truncate), `mode`, `mtime`, `atime`; rejects with `NFS3ERR_ROFS` in read-only mode |
| `NFSPROC3_LOOKUP` | 3 | **UTF-8 NFD→NFC normalization**; filters macOS metadata (`._*`, `.DS_Store`) with `NFS3ERR_NOENT`; handles `.` and `..`; records parent in `parent_map` |
| `NFSPROC3_ACCESS` | 4 | Returns allowed bits (`READ`/`LOOKUP`/`MODIFY`/`EXTEND`/`DELETE`/`EXECUTE`); masks write bits in read-only mode |
| `NFSPROC3_READ` | 6 | Serves offset/count via `DiskManager::read_at`; returns `eof` flag; caps at `NFS_MAX_IO_SIZE` |
| `NFSPROC3_WRITE` | 7 | Writes at offset via `DiskManager::write_data`; honors `UNSTABLE`/`DATA_SYNC`/`FILE_SYNC`; returns session `writeverf3` (8 bytes from `fs_generation`) |

### Directory & File Creation

| Proc | Name | Key Behavior |
|------|------|--------------|
| `NFSPROC3_CREATE` | 8 | Modes: `UNCHECKED` (create/truncate), `GUARDED` (fail if exists), `EXCLUSIVE` (verifier match); optional `sattr3` for size |
| `NFSPROC3_MKDIR` | 9 | Creates subdirectory; records parent; returns handle + attributes |

### Removal & Rename

| Proc | Name | Key Behavior |
|------|------|--------------|
| `NFSPROC3_REMOVE` | 12 | Unlinks file; returns `NFS3ERR_ISDIR` if target is directory |
| `NFSPROC3_RMDIR` | 13 | Removes empty directory; returns `NFS3ERR_NOTEMPTY` if not empty, `NFS3ERR_NOTDIR` if file |
| `NFSPROC3_RENAME` | 14 | Atomic cross-directory rename via `DiskManager::rename`; updates `parent_map` for moved directory; forgets overwritten inode's parent |

### Directory Reading

| Proc | Name | Key Behavior |
|------|------|--------------|
| `NFSPROC3_READDIR` | 16 | Cookie-based pagination; respects `count` byte budget; returns `cookieverf3`; emits `.` (cookie=1), `..` (cookie=2), then sorted entries |
| `NFSPROC3_READDIRPLUS` | 17 | Same as `READDIR` plus `post_op_attr` and `post_op_fh` for each entry; honors separate `dircount`/`maxcount` budgets |

### Filesystem Info

| Proc | Name | Key Behavior |
|------|------|--------------|
| `NFSPROC3_FSSTAT` | 18 | Reports total/free/available bytes and inodes; `avail = 0` in read-only mode |
| `NFSPROC3_FSINFO` | 19 | Advertises `rtmax`/`rtpref`/`wtmax`/`wtpref` = 128 KiB, `dtpref` = 32 KiB, `maxfilesize` = 512 GiB, `FSF3_HOMOGENEOUS \| FSF3_CANSETTIME` |
| `NFSPROC3_PATHCONF` | 20 | Reports `linkmax=1`, `name_max=255`, `no_trunc=true`, `chown_restricted=true`, `case_insensitive=false`, `case_preserving=true` |
| `NFSPROC3_COMMIT` | 21 | Flushes buffered writes via `DiskManager::flush`; returns `writeverf3` |

### Unsupported Procedures (Clean `NFS3ERR_NOTSUPP`)

`READLINK`, `SYMLINK`, `MKNOD`, `LINK` → return `NFS3ERR_NOTSUPP` with proper XDR reply structure.

---

## Concurrency & Security Model

### Loopback-Only Binding

- Default `TcpListener::bind(("127.0.0.1", port))` — **never** `0.0.0.0` unless explicit opt-in added.
- `mount_nfs`/`mount -t nfs` uses `port=<P>,mountport=<P>` to reach both programs on the same port.

### Multi-Connection Server

```
accept_thread (background)
    │
    ├─▶ poll(listener_fd, 50ms) ──▶ accept()
    │       │
    │       ├─▶ active_conns < MAX_CONCURRENT_NFS_CONNS (256) ?
    │       │       │
    │       │       └─▶ spawn worker thread per connection
    │       │
    │       └─▶ else drop connection (backpressure)
    │
    └─▶ shutdown flag → join all workers
```

- Each worker calls `serve_tcp_connection`:
  1. `poll_fd_readable(fd, 50ms)` → checks `shutdown` flag
  2. `read_rpc_record` → reassembles full RPC message
  3. `OifsNfsFs::dispatch_rpc` → produces reply payload
  4. `write_rpc_record` → sends single last-fragment frame

### Read-Only Enforcement

Every mutating procedure begins with:

```rust
if self.is_read_only() {
    w.write_u32(NFS3ERR_ROFS);
    self.write_wcc_data(&mut w, inode_id, pre.as_ref(), pre.as_ref());
    return w.into_bytes();
}
```

- `OifsNfsFs::is_read_only()` returns `options.read_only || dm.is_read_only()`.
- `DiskManager::is_read_only()` is set by `open_read_only*` factories and enforced at the OS file descriptor, mmap protection, and API guard levels.

### Authentication

| Flavor | Value | Handling |
|--------|-------|----------|
| `AUTH_NONE` | 0 | Accepted; anonymous access |
| `AUTH_SYS` | 1 | Parsed (uid/gid/groups); extracted but not yet used for ACL decisions; loopback trust model |

Future work: map `AUTH_SYS` uid/gid to `DiskManager` permission checks.

---

## Integration with DiskManager

`OifsNfsFs` wraps `Arc<DiskManager>` and delegates all storage operations:

| NFS Operation | DiskManager Call |
|---------------|------------------|
| `GETATTR` / `LOOKUP` / `ACCESS` | `read_allocated_inode`, `lookup`, `list_dir` |
| `READ` | `read_at(inode_id, offset, buf)` |
| `WRITE` | `write_data(inode_id, offset, data, compression_mode)` |
| `CREATE` / `MKDIR` | `create_file`, `create_directory` |
| `REMOVE` / `RMDIR` | `delete_file` |
| `RENAME` | `rename(from_dir, from_name, to_dir, to_name)` |
| `SETATTR` (truncate) | `truncate(inode_id, new_size)` |
| `COMMIT` | `flush()` |
| `FSSTAT` | `superblock()`, `analyze_fragmentation()` |

**Read sessions**: `READDIR`/`READDIRPLUS` use `DiskManager::begin_read()` for a consistent snapshot, with an in-memory `dir_snapshots` cache keyed by `(dir_id, write_gen, size, modified_at)` for repeated pagination.

**Parent map**: `OifsNfsFs` maintains a `parent_map` (fast array + fallback `FxHashMap`) to resolve `..` lookups without scanning directories. Updated on `CREATE`/`MKDIR`/`RENAME`/`RMDIR`.

---

## CLI Integration (`src/bin/oifs.rs`)

```bash
oifs mount <mountpoint> [--backend auto|nfs|fuse] [--port <u16>] [--read-only]
```

| Platform | `--backend auto` Default |
|----------|--------------------------|
| macOS | **NFSv3** (zero kext, works out of the box) |
| Linux | **FUSE** (`/dev/fuse`); `--backend nfs` for unprivileged containers |

### Mount Flow (NFS backend)

1. `OifsNfsServer::bind(dm, options)` → binds `127.0.0.1:port`, returns bound port.
2. `mount_os_nfs(port, mountpoint, read_only)`:
   - **macOS**: `mount_nfs -o nolocks,vers=3,tcp,rsize=131072,wsize=131072,port=P,mountport=P 127.0.0.1:/ <mp>`
   - **Linux**: `mount -t nfs -o nolock,vers=3,proto=tcp,rsize=131072,wsize=131072,port=P,mountport=P 127.0.0.1:/ <mp>`
3. `mount_and_serve_nfs` blocks until:
   - External unmount detected via `is_mounted(mountpoint)` (dev comparison)
   - `SIGINT`/`SIGTERM` → sets shutdown flag, unmounts, stops server, flushes `DiskManager` (if not read-only).

### Graceful Shutdown

```rust
// Signal handler (set_interrupt_handler)
move || {
    shutdown_flag.store(true, Release);
    let _ = fuse::unmount(&mountpoint);  // or nfs unmount
    if !read_only { let _ = dm.flush(); }
    std::process::exit(0);
}
```

---

## Verification & Testing

### Protocol Integration Suite (`tests/nfs_test.rs`)

| Test | Coverage |
|------|----------|
| `test_nfs_mount_and_all_v3_procedures_over_tcp` | Full MOUNT v3 + all 18 NFS v3 procedures via raw TCP + XDR |
| `test_nfs_readdir_and_readdirplus_pagination_over_220_entries` | Multi-page `READDIR` (512 B budget) and `READDIRPLUS` (1 KiB budget) |
| `test_nfs_read_only_enforces_rofs_and_byte_immutability` | Every mutating proc returns `NFS3ERR_ROFS`; image byte-for-byte identical after mount |
| `test_nfs_file_io_and_encrypted_parity_plus_malformed_fuzzing` | `FileIo` + encrypted parity; malformed/truncated/oversized frames never panic |

### Kani Formal Verification (`#[cfg(kani)]`)

| Proof | Property Verified |
|-------|-------------------|
| `kani_xdr_padding_and_bounds` | `xdr_padded_len` aligns to 4B without overflow; `XdrReader::read_opaque` never panics, respects `max_len` |
| `kani_rpc_record_marking_framing` | `parse_record_marking_header` and `RpcCallHeader::parse` are panic-free for arbitrary bytes |
| `kani_nfs_filehandle_roundtrip` | `NfsFileHandle` serialization is a lossless bijection; rejects non-16-byte slices |

---

## Configuration Reference

```rust
pub struct NfsMountOptions {
    pub read_only: bool,                    // Enforce NFS3ERR_ROFS on mutations
    pub port: u16,                          // 0 = OS-assigned ephemeral
    pub compression_mode: CompressionMode,  // WRITE compression policy
    pub filter_macos_metadata: bool,        // Reject ._*, .DS_Store with NFS3ERR_NOENT
    pub uid: u32,                           // Effective UID in fattr3 (default: geteuid)
    pub gid: u32,                           // Effective GID in fattr3 (default: getegid)
}
```

---

## Related Pages

- [DiskManager & Persistence](disk_manager_and_persistence.md) — storage engine backing NFS procedures
- [Concurrency & Session](concurrency_and_session.md) — thread model, locks, and session lifecycle
- [Architecture Overview](overview.md) — high-level component map
