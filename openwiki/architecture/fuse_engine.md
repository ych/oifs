---
type: architecture
title: FUSE Kernel Protocol Engine
description: Pure-Rust FUSE ABI v7.28 implementation with frame parser/dispatcher, cross-platform mount runners (Linux /dev/fuse + fusermount3, macOS FUSE-T/macFUSE via SCM_RIGHTS), dynamic max_write negotiation, handle table, read-only enforcement, UTF-8 NFD->NFC normalization, and macOS Finder metadata storm protection.
tags: [fuse, abi, kernel-protocol, mount, linux, macos, scm_rights, read-only, normalization, metadata-storm]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-22T10:29:12.696Z
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-062270e1dcd4de6ec5011c98
    resource: repo://src/fuse.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-22T10:29:12.696Z" }
---

# FUSE Kernel Protocol Engine

The OIFS FUSE engine is a **pure-Rust implementation of the FUSE kernel ABI v7.28** (major 7, minor 28) with zero external C dependencies. It provides a complete wire parser, frame dispatcher, stateful open-handle table, and cross-platform kernel mount runners for Linux and macOS. The engine multiplexes any number of open FUSE file handles over the single underlying OIFS image file descriptor owned by the `DiskManager`.

## Key Properties

- **Endianness-portable**: Every wire structure uses explicit little-endian `to_le_bytes` / `from_le_bytes` serialization.
- **Dual ABI support**: Simultaneously supports standard Linux 88-byte `FuseAttr` and macOS/Darwin 104-byte `FuseAttr` (with `crtime`, `crtimensec`, `flags`), plus both Linux 8-byte and macOS 16-byte xattr request headers.
- **Single-FD container**: The `FuseHandleTable` multiplexes thousands of FUSE `fh` values over one `.img` OS file descriptor.
- **Strict read-only enforcement**: When mounted read-only (via `DiskManager::is_read_only()` or `FuseMountOptions::read_only`), **every mutating operation returns `-EROFS` with zero disk writes**.
- **Panic-free frame dispatch**: `OifsFuseFs::dispatch_frame` validates every header and payload boundary and never panics on truncated or malformed input.

## FUSE ABI Constants

The engine targets **FUSE kernel protocol version 7.28** with the following wire sizes:

| Structure | Linux ABI Size | macOS ABI Size |
|-----------|----------------|----------------|
| `FuseInHeader` | 40 B | 40 B |
| `FuseOutHeader` | 16 B | 16 B |
| `FuseAttr` | 88 B | 104 B |
| `FuseEntryOut` | 128 B | 144 B |
| `FuseAttrOut` | 104 B | 120 B |
| `FuseSetAttrIn` | 88 B | 128 B (extended) |
| `FuseInitIn` | 16 B | 16 B |
| `FuseInitOut` | 64 B (ABI v7.28) | 64 B |
| `fuse_setxattr_in` / `fuse_getxattr_in` | 8 B | 16 B (adds `position`) |
| `fuse_dirent` header | 24 B | 24 B |

```rust
pub const FUSE_KERNEL_VERSION: u32 = 7;
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 28;
pub const FUSE_ROOT_ID: u64 = 1;
```

The platform-dependent layout is selected via `FuseAbiPlatform`:

```rust
pub enum FuseAbiPlatform {
    Linux,   // 88-byte FuseAttr, 128-byte FuseEntryOut, 104-byte FuseAttrOut
    MacOs,   // 104-byte FuseAttr, 144-byte FuseEntryOut, 120-byte FuseAttrOut
}
```

## Frame Parser & Dispatcher

### Request/Response Headers

`FuseInHeader` (40 bytes) and `FuseOutHeader` (16 bytes) are parsed with explicit bounds checking:

```rust
let Some(hdr) = FuseInHeader::parse(request) else {
    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
};
let payload = &request[FUSE_IN_HEADER_SIZE..];
```

The dispatcher validates:
- `buf.len() >= 40`
- `header.len as usize == buf.len()`
- Opcode is supported (`FuseOpcode::from_u32`)
- Payload length matches declared sizes for each opcode

### Opcode Dispatch

`dispatch_frame_for_platform` matches on `FuseOpcode` and routes to typed handlers:

```rust
match opcode {
    FuseOpcode::Init => { /* handshake */ }
    FuseOpcode::Lookup => { /* path lookup */ }
    FuseOpcode::Getattr => { /* attribute query */ }
    FuseOpcode::Setattr => { /* size/truncate, rejected if read-only */ }
    FuseOpcode::Mkdir => { /* directory creation, rejected if read-only */ }
    FuseOpcode::Unlink => { /* file deletion, rejected if read-only */ }
    FuseOpcode::Rmdir => { /* directory deletion, rejected if read-only */ }
    FuseOpcode::Rename => { /* rename/move, rejected if read-only */ }
    FuseOpcode::Open => { /* open file, write flags checked */ }
    FuseOpcode::Create => { /* create file, rejected if read-only */ }
    FuseOpcode::Read => { /* read data */ }
    FuseOpcode::Write => { /* write data, rejected if read-only */ }
    FuseOpcode::Flush => { /* flush dirty pages */ }
    FuseOpcode::Fsync => { /* sync data/metadata */ }
    FuseOpcode::Setxattr => { /* returns ENOTSUP */ }
    FuseOpcode::Getxattr => { /* returns ENOTSUP */ }
    FuseOpcode::Listxattr => { /* returns ENOTSUP */ }
    FuseOpcode::Removexattr => { /* returns ENOTSUP / EROFS */ }
    FuseOpcode::Release => { /* close file handle */ }
    FuseOpcode::Opendir => { /* open directory */ }
    FuseOpcode::Readdir => { /* list directory */ }
    FuseOpcode::Releasedir => { /* close dir handle */ }
    FuseOpcode::Statfs => { /* filesystem stats */ }
    FuseOpcode::Access => { /* permission check, write rejected if read-only */ }
    FuseOpcode::Destroy => { /* cleanup */ }
    FuseOpcode::Forget | FuseOpcode::BatchForget => { /* no reply */ }
    ...
}
```

**One-way notifications** (`FUSE_FORGET`, `FUSE_BATCH_FORGET`) return an empty `Vec` — no response frame is sent.

### Payload Parsing Safety

Every payload struct implements a `parse` / `parse_for` method that:
- Checks minimum buffer length
- Uses `read_u{16,32,64}_le` helpers with bounds-checked slice access
- Returns `None` on truncation (mapped to `-EINVAL` by dispatcher)

Example for `FuseWriteIn` (40-byte header followed by `size` payload bytes):

```rust
let Some(write_in) = FuseWriteIn::parse(payload) else {
    return FuseOutHeader::error_frame(unique, -libc::EINVAL);
};
let data_len = write_in.size as usize;
let data_end = FUSE_WRITE_IN_SIZE.checked_add(data_len)
    .filter(|&e| e <= payload.len())
    .ok_or_else(|| FuseOutHeader::error_frame(unique, -libc::EINVAL))?;
let data = &payload[FUSE_WRITE_IN_SIZE..data_end];
```

## Cross-Platform Mount Runners

### Linux: `/dev/fuse` + `fusermount3`

The primary Linux mount path uses `fusermount3` (falling back to `fusermount`) with the `_FUSE_COMMFD` handshake over `SCM_RIGHTS`:

```mermaid
sequenceDiagram
    participant App as OIFS Process
    participant FS as fusermount3
    participant Kernel as Linux FUSE Kernel Module
    App->>FS: socketpair(AF_UNIX, SOCK_STREAM)
    App->>FS: clear FD_CLOEXEC on child_sock
    App->>FS: spawn fusermount3 with _FUSE_COMMFD=child_fd
    App->>FS: drop child_sock
    App->>App: recv_fd_scm_rights(parent_sock) -> /dev/fuse fd
    FS->>Kernel: mount(2) with fd=...
    Kernel->>App: FUSE_INIT request on /dev/fuse fd
    App->>Kernel: FUSE_INIT reply (negotiated max_write, max_pages)
    App->>Kernel: serve_fuse_fd() loop (read/write frames)
```

The `SCM_RIGHTS` fd passing uses `sendmsg`/`recvmsg` with `CMSG_SPACE(sizeof(int))`:

```rust
pub fn recv_fd_scm_rights(sock_fd: RawFd) -> std::io::Result<OwnedFd> {
    let mut cmsg_buf = vec![0u8; unsafe { libc::CMSG_SPACE(4) as usize }];
    // msghdr setup with iov + control buffer
    let n = libc::recvmsg(sock_fd, &mut msg, 0);
    // iterate CMSG headers, match SOL_SOCKET + SCM_RIGHTS
    // copy received_fd from CMSG_DATA
}
```

### Fallback: Direct `/dev/fuse` + `mount(2)`

For privileged containers (root), the engine opens `/dev/fuse` directly and invokes `mount(2)`:

```rust
let dev_file = OpenOptions::new().read(true).write(true).open("/dev/fuse")?;
let data_str = format!(
    "fd={fd},rootmode=40000,user_id={},group_id={},default_permissions{}",
    opts.uid, opts.gid, if opts.allow_other { ",allow_other" } else { "" }
);
let rc = unsafe {
    libc::mount(
        c"oifs".as_ptr(),
        target.as_ptr(),
        c"fuse.oifs".as_ptr(),
        MS_NOSUID | MS_NODEV | if read_only { MS_RDONLY } else { 0 },
        data.as_ptr().cast()
    )
};
```

### macOS: `FUSE-T` / `macFUSE` / `osxfuse`

macOS mount discovers the available backend and uses the appropriate helper:

```mermaid
flowchart TD
    A[discover_macos_fuse_backend] --> B{FUSE-T found?}
    B -->|yes| C[Error: FUSE-T uses NFSv4 daemon]
    B -->|no| D{macFUSE / osxfuse?}
    D -->|yes| E[socketpair + SCM_RIGHTS handshake]
    D -->|no| F[Try fusermount in PATH]
    F --> G{success?}
    G -->|yes| H[serve_fuse_fd MacOs]
    G -->|no| I[Return error: install macFUSE]
    E --> J[spawn mount_macfuse/osxfuse with _FUSE_CALL_BY_LIB=1]
    J --> K[recv_fd_scm_rights -> /dev/macfuseN fd]
    K --> L[serve_fuse_fd_with_platform MacOs]
```

The macOS handshake sets these environment variables before spawning the mount helper:

```rust
std::process::Command::new(&mount_helper)
    .env("_FUSE_CALL_BY_LIB", "1")
    .env("_FUSE_COMMFD", child_fd.to_string())
    .env("_FUSE_COMMVERS", "2")
    .env("_FUSE_DAEMON_PATH", current_exe())
    .arg("-o").arg(opts)
    .arg(mountpoint)
    .spawn()
```

## Dynamic `max_write` Negotiation

The engine negotiates `max_write` during `FUSE_INIT` based on kernel capabilities and sysfs configuration.

### Constants

```rust
pub const FUSE_MIN_MAX_WRITE: u32 = 4096;           // 4 KiB minimum
pub const FUSE_LEGACY_MAX_WRITE: u32 = 128 * 1024;  // 128 KiB (Linux < 4.20)
pub const FUSE_DEFAULT_MAX_WRITE: u32 = 1024 * 1024; // 1 MiB default
pub const FUSE_MAX_SUPPORTED_WRITE: u32 = 4 * 1024 * 1024; // 4 MiB max
```

### Negotiation Logic

```rust
pub fn negotiate_max_write(
    requested: u32,
    init_flags: u32,
    platform: FuseAbiPlatform,
    page_size: u32,
    linux_max_pages_limit: u32,
) -> (u32, u16) {
    match platform {
        FuseAbiPlatform::Linux => {
            if (init_flags & FUSE_MAX_PAGES) == 0 {
                // Linux < 4.20: hard cap at 32 pages (128 KiB on 4K pages)
                let legacy_cap = 32u32.saturating_mul(page_size).max(FUSE_LEGACY_MAX_WRITE);
                (requested.min(legacy_cap), 0)
            } else {
                // Linux 4.20+: cap by max_pages_limit * page_size
                let limit_pages = linux_max_pages_limit.clamp(32, 1024);
                let kernel_cap = limit_pages.saturating_mul(page_size);
                let max_write = requested.min(kernel_cap);
                let max_pages = max_write.div_ceil(page_size).clamp(1, limit_pages) as u16;
                (max_write, max_pages)
            }
        }
        FuseAbiPlatform::MacOs => {
            if (init_flags & FUSE_BIG_WRITES) == 0 {
                (FUSE_MIN_MAX_WRITE, 0)
            } else {
                let max_pages = if (init_flags & FUSE_MAX_PAGES) != 0 {
                    max_write_to_pages(requested, page_size)
                } else { 0 };
                (requested, max_pages)
            }
        }
    }
}
```

### Linux Kernel `max_pages_limit` Detection

On Linux 6.13+, the sysfs parameter `/sys/module/fuse/parameters/max_pages_limit` controls the maximum (defaults to 256, configurable up to 1024):

```rust
pub fn detect_linux_fuse_max_pages_limit() -> u32 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(raw) = std::fs::read_to_string("/sys/module/fuse/parameters/max_pages_limit")
            && let Ok(parsed) = raw.trim().parse::<u32>()
        {
            return parsed.clamp(32, 1024);
        }
    }
    256 // Linux 4.20–6.12 or non-Linux fallback
}
```

**Resulting caps:**
- Linux < 4.20: **128 KiB** (32 pages × 4 KiB)
- Linux 4.20–6.12: **1 MiB** (256 pages × 4 KiB)
- Linux 6.13+ (default): **1 MiB** (256 pages × 4 KiB)
- Linux 6.13+ (tuned): up to **4 MiB** (1024 pages × 4 KiB)

The negotiated `max_write` is stored in `OifsFuseState.max_write` and enforced on every `FUSE_WRITE` request.

## FuseHandleTable: Handle Multiplexing

The `FuseHandleTable` maps FUSE `fh: u64` values to `FuseFileHandle` entries, allowing thousands of concurrent open files/directories over a single `.img` fd:

```rust
pub struct FuseFileHandle {
    pub inode_id: u64,      // OIFS inode backing this handle
    pub flags: u32,         // POSIX open flags (O_RDONLY, O_WRONLY, O_APPEND, etc.)
    pub is_dir: bool,       // true for FUSE_OPENDIR handles
}

pub struct FuseHandleTable {
    next_fh: AtomicU64,                    // monotonically increasing handle IDs
    handles: RwLock<FxHashMap<u64, FuseFileHandle>>,
}
```

**Operations:**
- `insert(handle) -> u64`: Allocates next non-zero `fh`, stores handle, returns `fh`
- `get(fh) -> Option<FuseFileHandle>`: Read-locked lookup
- `remove(fh) -> Option<FuseFileHandle>`: Write-locked removal (on `RELEASE`/`RELEASEDIR`)
- `clear()`: Drops all handles (on `FUSE_DESTROY`)

**Key invariant:** All I/O (`read_at`, `write_data`, `truncate`) goes through the single `DiskManager` instance — no per-handle OS file descriptors are opened.

## Read-Only Enforcement

Read-only semantics are enforced at **two levels**:

1. **DiskManager level**: `DiskManager::is_read_only()` returns `true` when the image was opened with `oifs_open_read_only` or the file has `0444` permissions.
2. **FUSE mount level**: `FuseMountOptions::read_only` (set via CLI `--read-only` or `mount -o ro`).

The combined check in `OifsFuseState`:

```rust
pub fn is_read_only(&self, dm: &DiskManager) -> bool {
    dm.is_read_only() || self.options.read_only
}
```

**Every mutating operation checks this first:**

```rust
pub fn mkdir(&self, dm: &DiskManager, parent: u64, name: &str, _mode: u32) -> Result<FuseEntryOut, i32> {
    if self.is_read_only(dm) {
        return Err(-libc::EROFS);
    }
    // ... actual mkdir logic
}

pub fn write(&self, dm: &DiskManager, nodeid: u64, fh: u64, offset: u64, data: &[u8], flags: u32) -> Result<u32, i32> {
    if self.is_read_only(dm) {
        return Err(-libc::EROFS);
    }
    // ... actual write logic
}

pub fn setattr_size(&self, dm: &DiskManager, nodeid: u64, new_size: u64) -> Result<FuseAttrOut, i32> {
    if self.is_read_only(dm) {
        return Err(-libc::EROFS);
    }
    // ... truncate logic
}

pub fn access(&self, dm: &DiskManager, nodeid: u64, mask: u32) -> Result<(), i32> {
    if self.is_read_only(dm) && (mask & libc::W_OK) != 0 {
        return Err(-libc::EROFS);
    }
    Ok(())
}
```

Even `setxattr`/`removexattr` return `-EROFS` before `-ENOTSUP`:

```rust
pub fn removexattr(&self, dm: &DiskManager, nodeid: u64, _name: &str) -> Result<(), i32> {
    if self.is_read_only(dm) {
        return Err(-libc::EROFS);
    }
    Err(-libc::ENOTSUP)
}
```

## UTF-8 NFD → NFC Normalization

macOS APFS and Finder store filenames in **NFD** (decomposed) form, while Linux and OIFS directory indices expect **NFC** (composed). The engine normalizes on ingress when `FuseMountOptions::normalize_nfc = true` (default):

```rust
#[inline]
fn normalize_name<'a>(&self, name: &'a str) -> Cow<'a, str> {
    if self.options.normalize_nfc {
        normalize_filename_nfc(name)
    } else {
        Cow::Borrowed(name)
    }
}
```

### Implementation: `normalize_filename_nfc`

The function features a **zero-allocation fast path** for ASCII and already-composed strings:

```rust
pub fn normalize_filename_nfc(name: &str) -> Cow<'_, str> {
    if name.is_ascii() {
        return Cow::Borrowed(name); // fast path
    }
    // Quick scan for composable sequences
    let mut needs_compose = false;
    let mut prev = first_char;
    for ch in name.chars().skip(1) {
        if compose_pair(prev, ch).is_some() {
            needs_compose = true;
            break;
        }
        prev = ch;
    }
    if !needs_compose {
        return Cow::Borrowed(name); // already NFC
    }
    // Slow path: build composed String
    let mut out = String::with_capacity(name.len());
    // ... streaming composition using compose_pair
    Cow::Owned(out)
}
```

### Supported Composition Rules (`compose_pair`)

| Category | Examples |
|----------|----------|
| **Korean Hangul Jamo** | `L + V → LV`, `LV + T → LVT` (algorithmic per Unicode §3.12) |
| **Japanese Kana** | `hiragana/katakana + U+3099 (dakuten) → voiced`, `+ U+309A (handakuten) → semi-voiced` |
| **Latin diacritics** | `A + U+0300 → À`, `e + U+0301 → é`, `n + U+0303 → ñ`, etc. |
| **Cyrillic** | `Е + U+0308 → Ё`, `и + U+0306 → й` |
| **Combining marks** | Grave, acute, circumflex, tilde, macron, breve, dot above, diaeresis, ring above, double acute, caron, cedilla, ogonek |

This pure-Rust implementation avoids ICU or `unicode-normalization` crate dependencies.

## macOS Finder Metadata Storm Protection

macOS Finder aggressively issues `getxattr`/`setxattr`/`listxattr` on metadata files (`.DS_Store`, `._*`, `.Spotlight-V100`, etc.), which would:
1. Clutter the OIFS container with meaningless entries
2. Cause "metadata storms" — thousands of xattr syscalls per directory visit

### Fast Rejection

When `FuseMountOptions::reject_macos_metadata = true` (default), these filenames are rejected at **lookup entry** with `-ENOENT`:

```rust
pub fn is_macos_metadata_filename(name: &str) -> bool {
    name.starts_with("._")
        || matches!(
            name,
            ".DS_Store" | ".Spotlight-V100" | ".Trashes" | ".fseventsd" | ".TemporaryItems"
        )
}

#[inline]
fn reject_if_macos_metadata(&self, name: &str) -> Result<(), i32> {
    if self.options.reject_macos_metadata && is_macos_metadata_filename(name) {
        return Err(-libc::ENOENT);
    }
    Ok(())
}
```

### xattr Storm Throttling

Even when metadata files slip through (e.g., on Linux), the xattr handlers return **`-ENOTSUP`** instead of `-ENOSYS` or `-EINVAL`:

```rust
pub fn setxattr(...) -> Result<(), i32> {
    if self.is_read_only(dm) { return Err(-libc::EROFS); }
    let _ = self.resolve_file_inode(dm, nodeid)?;
    Err(-libc::ENOTSUP) // critical: ENOTSUP stops Finder retry loops
}

pub fn getxattr(...) -> Result<Vec<u8>, i32> { Err(-libc::ENOTSUP) }
pub fn listxattr(...) -> Result<Vec<u8>, i32> { Err(-libc::ENOTSUP) }
pub fn removexattr(...) -> Result<(), i32> { Err(-libc::ENOTSUP) }
```

**Why `-ENOTSUP`?** macOS Finder treats `-ENOSYS` (not implemented) and `-EINVAL` (invalid argument) as "try harder" signals and retries with different parameters. `-ENOTSUP` (operation not supported on this filesystem) cleanly terminates the storm.

## Mount Option Generation

`FuseMountOptions::build_mount_options_for_os` produces OS-filtered `-o` strings:

### Linux Options
```text
fsname=oifs,default_permissions,subtype=oifs[,ro][,allow_other]
```
- Strips macOS-only options (`noappledouble`, `noapplexattr`, `volname`, `local`, `iosize`)

### macOS Options
```text
fsname=oifs,default_permissions,noappledouble,noapplexattr,local,iosize=<max_write>[,ro][,allow_other][,volname=<name>]
```
- `noappledouble` / `noapplexattr`: disable AppleDouble/EA handling in macFUSE
- `local`: marks as local filesystem (disables network filesystem heuristics)
- `iosize`: sets I/O chunk size to negotiated `max_write`
- `volname`: sets volume display name in Finder sidebar

## VFS Facade: High-Level API

`OifsFuseFs` wraps `OifsFuseState + Arc<DiskManager>` and exposes typed methods matching FUSE ops:

```rust
pub struct OifsFuseFs {
    dm: Arc<DiskManager>,
    state: OifsFuseState,
}

impl OifsFuseFs {
    // Binary frame dispatcher (used by serve_fuse_fd, serve_stream, tests)
    pub fn dispatch_frame(&self, request: &[u8]) -> Vec<u8>;
    pub fn dispatch_frame_for_platform(&self, request: &[u8], platform: FuseAbiPlatform) -> Vec<u8>;

    // High-level VFS API (used by FFI, tests, IPC)
    pub fn lookup(&self, parent: u64, name: &str) -> Result<FuseEntryOut, i32>;
    pub fn getattr(&self, nodeid: u64) -> Result<FuseAttrOut, i32>;
    pub fn mkdir(&self, parent: u64, name: &str, mode: u32) -> Result<FuseEntryOut, i32>;
    pub fn create(&self, parent: u64, name: &str, flags: u32, mode: u32) -> Result<(FuseEntryOut, FuseOpenOut), i32>;
    pub fn read(&self, nodeid: u64, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32>;
    pub fn write(&self, nodeid: u64, fh: u64, offset: u64, data: &[u8], flags: u32) -> Result<u32, i32>;
    // ... unlink, rmdir, rename, open, opendir, readdir, statfs, access, xattr, etc.
}
```

## Formal Verification (Kani)

The engine includes Kani proofs for critical invariants:

```rust
#[cfg(kani)]
mod kani_proofs {
    /// Bijection: inode_id <-> nodeid (root 0 <-> 1, 0 invalid)
    #[kani::proof]
    fn proof_fuse_nodeid_inode_bijection_and_root_mapping() { ... }

    /// fuse_dirent_align: 8-byte alignment, no overflow for name_len <= 255
    #[kani::proof]
    fn proof_fuse_dirent_align_and_record_bounds() { ... }

    /// FuseInHeader::parse: never panics, rejects len < 40, rejects mismatched len
    #[kani::proof]
    fn proof_fuse_frame_header_parse_no_panic_and_bounds() { ... }
}
```

## Integration Points

| Component | Role |
|-----------|------|
| `DiskManager` | Single source of truth for on-disk state; all FUSE ops delegate to it |
| `FuseMountOptions` | Configures ABI platform, read-only, normalization, metadata filtering, `max_write` |
| `OifsFuseState` | Protocol state (handles, parent map, init/destroy flags) — reusable by FFI |
| `OifsFuseFs` | High-level wrapper owning `Arc<DiskManager>`; used by CLI mount, IPC, FFI |
| `src/bin/oifs.rs` | CLI `mount` subcommand — opens image, builds options, calls `mount()` |
| `include/oifs.h` / `src/ffi.rs` | C FFI exposes `OIFSHandle`, `oifs_is_read_only`; FUSE not directly exposed |

## Failure Modes & Invariants

| Scenario | Behavior |
|----------|----------|
| Truncated frame (`len < 40`) | `-EINVAL` response |
| Declared `len` ≠ `buf.len()` | `-EINVAL` response |
| Unsupported opcode | `-ENOSYS` response |
| Read-only mount + mutating op | `-EROFS` response, **zero disk writes** |
| `ENODEV` / EOF on `/dev/fuse` | Clean `Ok(())` exit from `serve_fuse_fd` |
| `FUSE_DESTROY` received | `handles.clear()`, `dm.flush()` if not read-only, exit loop |
| macOS FUSE backend missing | Clear error: install macFUSE or use `serve_fuse_fd` / `dispatch_frame` |
| `normalize_nfc` + invalid UTF-8 | `-EINVAL` at parse_fuse_name (NUL-terminated) |

## Extension Points

1. **Custom `FuseAbiPlatform`**: Embedders can call `dispatch_frame_for_platform` with `Linux` or `MacOs` for cross-platform testing.
2. **`FuseMountOptions` tuning**: `max_write`, `attr_ttl_secs`, `entry_ttl_secs`, `reject_macos_metadata`, `normalize_nfc`, `volname`.
3. **Alternative transports**: `serve_fuse_fd` (packet device), `serve_stream` (Unix stream), `dispatch_frame` (in-process).
4. **FFI reuse**: `OifsFuseState` can be embedded in C-compatible handles that already own a `DiskManager`.

---

*This page documents the FUSE kernel protocol engine as implemented in `src/fuse.rs` (4332 lines), with mount runners in `src/bin/oifs.rs` and read-only semantics rooted in `src/disk.rs::DiskManager::is_read_only()`.*
