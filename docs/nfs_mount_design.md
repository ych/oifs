# OIFS Pure-Rust Local NFSv3 Server Design

## 1. Motivation & Goals

OIFS already provides a Pure-Rust FUSE ABI v7.28 engine (`src/fuse.rs`) supporting Linux (`/dev/fuse` + `fusermount3`) and macOS (`macFUSE` / `osxfuse`). However, on modern macOS (Apple Silicon / macOS 15+), kernel FUSE requires installing a third-party System Extension (`macFUSE`), approving it in System Settings, and rebooting.

To provide **zero-dependency, kext-free, reboot-free native OS mounting on macOS** (as well as unprivileged container mounting on Linux where `/dev/fuse` is unavailable), OIFS will implement an embedded, Pure-Rust **Local NFSv3 Server (`src/nfs.rs`)** that the host OS mounts directly with its built-in `mount_nfs` client.

### Core Goals
- **Zero external C/Go dependencies**: Pure Rust implementation of ONC RPC v2 (RFC 5531), XDR (RFC 4506), MOUNT v3 (RFC 1813 Appendix I), and NFS v3 (RFC 1813).
- **Single loopback TCP port, no `rpcbind`/`portmap`**: Bind `127.0.0.1:0` (ephemeral port) and multiplex both `MOUNT v3` (program `100005`) and `NFS v3` (program `100003`) over the same TCP listener.
- **No root privileges or kext required**: macOS mounts directly via:
  ```bash
  mount_nfs -o nolocks,vers=3,tcp,rsize=131072,wsize=131072,port=<P>,mountport=<P> 127.0.0.1:/ <mountpoint>
  ```
- **Single OS FD preservation**: Stateless 16-byte NFSv3 file handles map directly to OIFS `inode_id` without opening per-file OS descriptors.
- **Full OIFS feature parity**: Works seamlessly over `StorageMode::Mmap` and `StorageMode::FileIo`, plain/journaled/encrypted images, compressed files, and strict `--read-only` immutable mounts.

---

## 2. Why NFSv3 Over NFSv4 for Local Loopback Mounts

| Dimension | NFSv3 (RFC 1813) | NFSv4.0 / v4.1 (RFC 7530 / 8881) |
| :--- | :--- | :--- |
| **State Model** | **Stateless** (`filehandle + offset + count`). Simple, restart-safe, zero state leaks. | **Stateful** (`SETCLIENTID`, `OPEN`/`OPEN_CONFIRM`, `stateid`, `seqid`, leases, grace periods). 4–6× implementation complexity. |
| **Loopback Porting** | Multiplexing MOUNT v3 + NFS v3 on one TCP port (`port=P,mountport=P`) gives single-port, `rpcbind`-free mounting. | Native single TCP port with pseudo-fs. |
| **Loopback Latency** | 1 RPC per operation; on `127.0.0.1` (RTT ~10–20 µs) with OS page cache, overhead is minimal. | `COMPOUND` RPC helps high-latency LAN/WAN, marginal gain on loopback. |
| **File Locking** | `nolocks` lets the local OS kernel enforce `flock`/`fcntl` locks across processes on the same host. | Built-in `LOCK`/`LOCKU` needed primarily for multi-host shared NAS. |
| **macOS Client Maturity** | Battle-tested, most reliable path in macOS `mount_nfs`. | macOS supports only NFSv4.0 (no v4.1); `FATTR4` bitmaps, `user@domain` idmapping, and Finder edge cases are frequent failure sources. |
| **Windows Client** | Windows built-in NFS client supports NFSv3 natively. | Not supported by default. |
| **Formal Verification** | Small, bounded wire format amenable to Kani proofs. | State-space explosion makes full verification impractical. |

**Architecture Note**: The ONC RPC v2 framing layer and XDR codec are shared between NFSv3 and NFSv4. Starting with NFSv3 delivers maximum stability and minimal attack surface while keeping future NFSv4 extensibility open if multi-host network mounting is ever required.

---

## 3. Architecture & Module Breakdown

### 3.1 `src/nfs.rs` — Pure-Rust ONC RPC v2 / XDR / MOUNT v3 / NFS v3

1. **XDR Codec (RFC 4506)**:
   - Safe `XdrReader<'a>` and `XdrWriter` with explicit bounds checks, 4-byte alignment padding (`(len + 3) & !3`), and hard maximum payload caps (e.g. `NFS_MAX_FRAME_LEN = 2 MiB`) to prevent OOM or panics on malformed input.
2. **ONC RPC v2 TCP Record Marking (RFC 5531)**:
   - 4-byte big-endian fragment header (`bit 31 = last_fragment`, `bits 0..30 = fragment_length`).
   - Reassembles multi-fragment RPC messages up to `NFS_MAX_FRAME_LEN`.
   - Parses `CALL` (`xid`, `rpcvers=2`, `prog`, `vers`, `proc`, `cred`, `verf`) and encodes `REPLY` (`MSG_ACCEPTED` / `MSG_DENIED`).
   - Accepts `AUTH_NONE` and `AUTH_SYS` (extracting `uid`/`gid` for newly created metadata if needed).
3. **Stateless File Handle (`NfsFileHandle`, 16 bytes)**:
   - Layout: `[fs_generation: u64 LE, inode_id: u64 LE]`.
   - `fs_generation` is generated per mount session (or derived from superblock salt/UUID), preventing stale handle collisions across remounts (`NFS3ERR_STALE`).
   - Root handle uses `inode_id = 0`.
4. **MOUNT v3 (Program `100005`, Version `3`)**:
   - `MNTPROC3_NULL (0)`: Health check.
   - `MNTPROC3_MNT (1)`: Validates export path (`/`), returns `MNT3_OK` + root `NfsFileHandle` + `[AUTH_NONE, AUTH_SYS]`.
   - `MNTPROC3_UMNT (3)` / `MNTPROC3_UMNTALL (4)`: Acknowledges unmount.
   - `MNTPROC3_EXPORT (5)`: Advertises `/` export.
5. **NFS v3 (Program `100003`, Version `3`)**:
   - `NFSPROC3_NULL (0)`
   - `NFSPROC3_GETATTR (1)` — maps OIFS `Inode` to `fattr3` (type, mode, nlink, uid, gid, size, used, fsid, fileid, atime/mtime/ctime).
   - `NFSPROC3_SETATTR (2)` — supports `size` (truncate), `mode`/`mtime`/`atime` updates; rejects with `NFS3ERR_ROFS` in read-only mode.
   - `NFSPROC3_LOOKUP (3)` — UTF-8 NFD→NFC normalization, macOS Finder metadata filtering (`._*`, `.DS_Store`), returns `NfsFileHandle` + post-op `fattr3`.
   - `NFSPROC3_ACCESS (4)` — returns allowed access bits (`READ`/`LOOKUP`/`MODIFY`/`EXTEND`/`DELETE`/`EXECUTE`), masking write bits in read-only mode.
   - `NFSPROC3_READ (6)` — serves offset/count reads via `DiskManager::read_at` with `eof` flag.
   - `NFSPROC3_WRITE (7)` — writes at arbitrary offset; handles `UNSTABLE` / `DATA_SYNC` / `FILE_SYNC` stability levels and returns a mount-session `writeverf3` (8 bytes).
   - `NFSPROC3_CREATE (8)` — supports `UNCHECKED`, `GUARDED`, and `EXCLUSIVE` modes.
   - `NFSPROC3_MKDIR (9)` — creates subdirectory and returns handle + attributes.
   - `NFSPROC3_REMOVE (12)` / `NFSPROC3_RMDIR (13)` — unlinks files / removes empty directories.
   - `NFSPROC3_RENAME (14)` — atomic rename (requires adding or wiring `DiskManager::rename` with WAL support; returns `NFS3ERR_NOTSUPP` until cross-directory rename is finalized if phased).
   - `NFSPROC3_READDIR (16)` & `NFSPROC3_READDIRPLUS (17)` — cookie-based pagination respecting `dircount`/`maxcount` byte budgets, returning `.`, `..`, and directory entries with `cookieverf3`.
   - `NFSPROC3_FSSTAT (18)` — reports total/free/available bytes and inode slots (`avail = 0` in read-only mode).
   - `NFSPROC3_FSINFO (19)` — advertises `rtmax`/`rtpref`/`wtmax`/`wtpref` = 128 KiB, `dtpref` = 32 KiB, `maxfilesize`, `FSF3_HOMOGENEOUS | FSF3_CANSETTIME`.
   - `NFSPROC3_PATHCONF (20)` — reports `linkmax`, `name_max = 255`, `no_trunc = true`, `chown_restricted = true`, `case_insensitive = false`, `case_preserving = true`.
   - `NFSPROC3_COMMIT (21)` — flushes buffered writes via `DiskManager::flush` and returns `writeverf3`.
   - Unsupported procedures (`SYMLINK`, `MKNOD`, `LINK`) return `NFS3ERR_NOTSUPP` cleanly.

### 3.2 Concurrency & Security Model
- **Loopback-only binding**: Defaults strictly to `127.0.0.1:0` (never `0.0.0.0` unless an explicit opt-in flag is added in the future).
- **Multi-connection server**: Accepts concurrent TCP connections from the kernel NFS client (each connection served on a worker thread sharing `Arc<OifsNfsFs>`).
- **Read-only enforcement**: Every mutating NFSv3 procedure checks `is_read_only()` before calling `DiskManager` and returns `NFS3ERR_ROFS`, preserving 100% byte-for-byte image immutability.
- **Shared VFS helpers**: Reuse `normalize_filename_nfc` and `is_macos_metadata_filename` (extracted into a shared internal helper or re-exported from `src/fuse.rs`).

### 3.3 CLI Integration (`src/bin/oifs.rs`)
Extend `oifs mount <mountpoint>`:
- `--backend <auto|nfs|fuse>` (default: `auto`)
  - **macOS `auto`**: Prefer built-in **NFSv3** (zero kext, works out of the box); fall back or allow `--backend fuse` for macFUSE.
  - **Linux `auto`**: Prefer native **FUSE** (`/dev/fuse`); allow `--backend nfs` for unprivileged containers.
- `--port <u16>` (optional, default `0` for OS-assigned ephemeral port).
- Existing flags (`--read-only`, `--no-mmap`, `--foreground`, `--allow-other`) remain supported.
- Graceful shutdown on `SIGINT`/`SIGTERM`: unmounts the mountpoint (`unmount(2)` / `umount`), stops the TCP listener, and flushes `DiskManager`.

---

## 4. Verification & Testing Plan

1. **Protocol Integration Suite (`tests/nfs_test.rs`)**:
   - Direct loopback `TcpStream` client sending raw ONC RPC v2 / XDR frames.
   - Covers `MOUNT v3` (`MNT`, `EXPORT`, `UMNT`) and all 18 `NFS v3` procedures across `Mmap`, `FileIo`, journaled, and encrypted images.
   - Read-only `NFS3ERR_ROFS` + byte-for-byte immutability check.
   - `READDIR` / `READDIRPLUS` multi-page pagination with > 200 entries.
   - Malformed / truncated / oversized XDR and RPC fragment fuzzing (must never panic).
2. **Live OS `mount_nfs` E2E Test (`tests/nfs_live_mount_test.rs`)**:
   - Mounts a temporary `.img` via real `mount_nfs` on macOS (zero third-party drivers required).
   - Exercises standard `std::fs` read/write/append/truncate/large-file/nested-dir/Unicode/read-only flows, unmounts via RAII guard, and runs `DiskManager::verify_integrity()` (`fsck`).
3. **Formal Verification (Kani Proofs in `src/nfs.rs`)**:
   - `kani_xdr_padding_and_bounds`: Proves XDR 4-byte alignment never overflows or reads past buffer bounds.
   - `kani_rpc_record_marking_framing`: Proves fragment header decoding and `NFS_MAX_FRAME_LEN` enforcement are panic-free for arbitrary bytes.
   - `kani_nfs_filehandle_roundtrip`: Proves `(fs_generation, inode_id) <-> [u8; 16]` is a lossless bijection.
