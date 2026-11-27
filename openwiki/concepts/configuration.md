---
type: "Reference"
title: "Configuration"
description: "Documents all configuration knobs: Cargo features, CLI flags, environment variables, session modes, compression modes, durability modes, auto-defrag, storage modes, and their interactions."
tags: ["configuration", "cli", "environment", "runtime", "storage", "compression", "durability", "defragmentation", "session", "ipc"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-26T17:56:01.542Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-ea9e30b0c99ad48bf309d4ab
    resource: repo://src/io_engine.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-26T17:56:01.542Z" }
---

OIFS provides multiple configuration mechanisms to tailor functionality for different use cases, from embedded systems to high-performance clusters. Configuration occurs at build time via Cargo features, at runtime via CLI flags and environment variables, and through persistent filesystem properties stored in the superblock.

## Build-Time Features

Optional functionality is controlled through Cargo features in `Cargo.toml`:

| Feature | Description | Default |
|---------|-------------|---------|
| `default` | Enables `cli` and `io_uring` features | Yes |
| `cli` | Builds the `oifs` CLI binary (requires `clap`) | Yes (via default) |
| `mcp` | Adds MCP (Model Context Protocol) server capabilities via the `oifs_mcp` binary; required for the `--network` / `-n` CLI flag | No |
| `io_uring` | Enables Linux io_uring backend for accelerated I/O (Linux-only, requires kernel ≥ 5.15 or RHEL 9.3+ backport) | Yes (via default, Linux-only) |
| `blosc2` | Enables native C-Blosc2 interop wrappers (`filters::blosc2_compress`/`blosc2_decompress`) | No |

**Encryption dependencies** (`chacha20poly1305`, `argon2`, `rand`, `zeroize`, `blake2`, `base64ct`) and **compression dependencies** (`zstd`, `rayon`) are always linked; they are not behind feature flags.

The `io_uring` feature is conditional: it only links the `io-uring` dependency on Linux targets (`cfg(target_os = "linux")`). On other platforms or when disabled, the io_uring backend compiles to nothing and falls back to `pread`.

## Runtime Configuration

### Environment Variables

| Variable | Purpose | Values |
|----------|---------|--------|
| `OIFS_IO_BACKEND` | Selects I/O backend for payload reads | `mmap`, `pread`, `io_uring` (aliases: `uring`, `iouring`, `io-uring`), case-insensitive |
| `OIFS_PASSWORD` | Provides password for encrypted filesystems | String (overridden by `--password` flag unless in `--json` mode) |
| `OIFS_ALLOW_PRE_5_15` | Allows io_uring on kernels older than 5.15 / RHEL 9.3 backport (Linux only) | Any non-empty value (e.g., `1`, `true`) |
| `OIFS_STORAGE_MODE` | Selects default storage mode for new/open images | `mmap`, `pread`, `file`, `file_io`, `no-mmap` (case-insensitive) |
| `OIFS_FILE_IO_CACHE_MB` | User-space data page cache budget for `StorageMode::FileIo` in MiB (`0` disables) | Integer, default `32`, max `4096` (4 GiB) |

#### I/O Backend Resolution

The I/O backend selection follows this precedence:

1. Explicit request via `DiskManager::set_io_backend(IoBackend)` (API)
2. `OIFS_IO_BACKEND` environment variable (parsed case-insensitively)
3. Default: `IoBackend::Mmap`

Requested backends that are unavailable (e.g., `io_uring` on non-Linux, `io_uring` feature disabled, kernel too old, `io_uring_disabled` sysctl, seccomp-filtered container) silently fall back to `IoBackend::Pread`. The actual backend in use can be queried via `IoBackend::resolve()`.

**Kernel version gating:** The io_uring backend requires Linux ≥ 5.15 or a RHEL 9.3+ backport (kernel `5.14.0-362+` with `.el9` in the release string). The check is enforced by `uring_supported()` in `src/io_engine.rs` unless `OIFS_ALLOW_PRE_5_15` is set in the environment.

#### Storage Mode Resolution

The storage mode selection follows this precedence:

1. Explicit request via `DiskManager::open_with_storage_mode()` or `set_default_storage_mode()` (API)
2. `OIFS_STORAGE_MODE` environment variable (parsed case-insensitively)
3. Default: `StorageMode::Mmap`

### CLI Flags

Global flags affecting all commands (defined in `src/bin/oifs.rs`):

| Flag | Description |
|------|-------------|
| `--image`, `-i <path>` | Path to OIFS image file |
| `--password <pwd>` | Password for encrypted filesystem (overrides `OIFS_PASSWORD`) |
| `--ipc` | Enable local IPC (Inter-Process Communication) mode for multi-process concurrency on the same machine (Unix Domain Socket in `/tmp`) |
| `--network`, `-n` | Enable cross-machine/network mode (for NFS, Lustre, or multi-node clusters); requires `mcp` feature |
| `--bind <addr>` | Custom bind address for network mode (e.g., `0.0.0.0:9050` or `127.0.0.1:0`) |
| `--allow-remote-ipc` | Allow the IPC master to bind a non-loopback address (required when using `--bind` with a non-loopback address; only safe when traffic is tunnelled via WireGuard, IPsec, SSH port forwarding) |
| `--json` | Output results as minified JSON (suppresses interactive password prompts) |
| `--auto-defrag[=THRESHOLD]` | Enable the background automatic defragmentation coordinator. Optional fragmentation threshold (0.0–1.0); defaults to 0.25. Ratio is `(free_runs - 1) / (free_blocks - 1)`. |
| `--defrag-interval <seconds>` | Interval in seconds between background fragmentation checks; defaults to 300 (minimum 10 ms enforced internally) |
| `--no-mmap` | Use pure positional `pread`/`pwrite` file I/O instead of mmap (eliminates `SIGBUS` and mmap hazards); sets `StorageMode::FileIo` |
| `--read-only` | Open the image in read-only mode (shared `F_RDLCK` lock, zero writes, works on 0444 images) |
| `--dict <path>` | Custom Zstd dictionary file(s) to register (first is also set as default compressor) |
| `--auto-grow[=MAX_MB]` | Automatically expand image capacity on block or inode exhaustion (optional hard cap in MiB, e.g. `--auto-grow` or `--auto-grow=64`) |

Command-specific configuration:

- `create`: `--size <MB>` (default 10), `--encrypt`, `--journal` (enables metadata WAL)
- `put`: `--compress`, `--no-compress`, `--chunked` (seekable 64KB chunked compression), `--filter <type>` (`none`, `delta`, `shuffle`, `bitshuffle`, `both`, `auto`), `--typesize <bytes>` (1, 2, 4, 8)
- `defrag`: `--mode <safe|inplace>` (default `safe`)

### Session Mode Selection

The session mode determines how processes coordinate access to the same image. It is selected via CLI flags or the `OifsSession` API:

| Mode | CLI Flag | API Method | Transport | Use Case |
|------|----------|------------|-----------|----------|
| `Standalone` | (none) | `get_or_open` | None (direct `mmap`) | Single process, zero overhead |
| `Local` | `--ipc` | `open_local` | Unix Domain Socket in `/tmp` | Multi-process on one machine |
| `Network` | `--network` / `-n` | `open_network` | TCP + rendezvous file (`.image.master`) | NFS, Lustre, multi-node clusters |

**Network mode requires the `mcp` feature** (it pulls in `tokio` and `rmcp`). The `oifs_mcp` binary also requires the `mcp` feature.

### Password Resolution

Encrypted filesystems require a password. Resolution precedence:

1. `--password` CLI flag
2. `OIFS_PASSWORD` environment variable
3. Interactive prompt (suppressed in `--json` mode to keep stdout pure for JSON pipelines)

The password is stored in a `Zeroizing<String>` buffer and wiped on drop.

## Persistent Filesystem Properties (Superblock)

The superblock (block 0) stores immutable and mutable filesystem properties that persist across mounts. Key feature flags and fields:

| Field | Type | Description |
|-------|------|-------------|
| `encrypted` | `bool` | Whether the filesystem uses encryption |
| `encryption_version` | `u8` | Encryption scheme: `1` = legacy (XChaCha20-Poly1305 without AAD), `2` = context-bound (AAD binds ciphertext to inode/chunk) |
| `format_version` | `u32` | On-disk format: `1` = legacy bincode inodes, `2` = fixed 256-byte inodes (`INODE_FORMAT_V2`), `3` = v3 layout with multi-block bitmaps |
| `migration_cursor` | `u64` | In-place format migration progress cursor (0 = no migration) |
| Journal layout | Derived | Detected via `has_journal_layout()`: true when inode table starts after data bitmap + `JOURNAL_RESERVED_BLOCKS` (33 blocks: 1 header + 128 KB ring) |

**No `case_insensitive` or `chunked` flags exist in the superblock.** Case sensitivity is not a filesystem-level setting. Chunked compression is a per-write `CompressionMode::Seekable` choice, not a persistent property.

### Encryption Schemes

- **Legacy (`ENCRYPTION_SCHEME_LEGACY = 1`)**: XChaCha20-Poly1305 without associated data. Files can be swapped or rolled back without detection.
- **Context-bound (`ENCRYPTION_SCHEME_CONTEXT_BOUND = 2`)**: AEAD with associated data binding ciphertext to inode ID (and for chunks, to chunk index + flags). Prevents ciphertext relocation, reordering, and rollback attacks. New images use this scheme.

### Format Versions

- **v1 (Legacy)**: Packed `bincode` inode records. Read-only; existing images stay usable but keep writing v1 records until migrated.
- **v2 (Fixed Inode)**: 256-byte fixed-width inode records with explicit little-endian fields (`INODE_FORMAT_V2`).
- **v3 (Layout)**: Multi-block bitmaps that scale with image size (removes the 128 MiB / 32,768 block cap of v1/v2). `FORMAT_VERSION_V3_LAYOUT = 3`.

### Journaling

When `--journal` is passed to `create`, the superblock reserves 33 blocks (1 header + 128 KB ring) between the data bitmap and the inode table. Uncommitted transactions are replayed automatically on the next mount, making `fsck` unnecessary after power loss. The presence of the journal is detected by layout geometry, not a dedicated boolean flag.

## Compression Modes

`CompressionMode` (from `src/disk.rs`) controls when and how files are compressed:

| Mode | Behavior |
|------|----------|
| `Always` | Always compress, regardless of file size (stream mode) |
| `Never` | Never compress |
| `Auto` (default) | Compress files ≥ 8 KB |
| `Stream { level }` | Full-file stream compression with explicit Zstd level (1–19, 0 = default 3) |
| `Seekable { chunk_size, level }` | Seekable chunked compression with independent chunks (default 64 KB chunks, level 1 recommended) |

The `--chunked` CLI flag selects `Seekable { chunk_size: 64KB, level: 3 }`.

### Pre-compression Filters

Applied before Zstd to improve compression ratio on structured data:

| Filter | CLI `--filter` Value | Description |
|--------|---------------------|-------------|
| None | `none` | No filter |
| Delta | `delta` | First-order delta encoding (good for time-series, monotonic data) |
| Byte Shuffle | `shuffle` | Groups identical significance bytes (good for AoS structs) |
| Bit Shuffle | `bitshuffle` | Bit-level transpose (good for sparse bits, boolean masks) |
| Numeric (Delta + Shuffle) | `both` or `numeric` | Delta then byte shuffle |
| Auto | `auto` | Analyzes file content and selects best filter |

Element size (`--typesize`) controls filter granularity: 1, 2, 4, or 8 bytes (default 4).

## Durability Modes

`DurabilityMode` (from `src/disk.rs`) controls `msync` behavior:

| Mode | Value | Behavior |
|------|-------|----------|
| `Lazy` (default) | `0` | Mutations update shared mmap + OS page cache without per-mutation `msync`. Visible to all processes immediately; survives process crashes. Persistence across power loss guaranteed on explicit `flush()`, `Drop`, or kernel writeback. ~45x faster bulk writes. |
| `Full` | `1` | Each mutation issues `msync(MS_SYNC)`. Maximum power-loss resilience; slower. |

## Auto-Defragmentation

Background coordination configured via `--auto-defrag[=THRESHOLD]` and `--defrag-interval`:

- **Threshold** (default 0.25): Fragmentation ratio `(free_runs - 1) / (free_blocks - 1)`. 0.0 = fully contiguous, 1.0 = every free block isolated.
- **Interval** (default 300 s): Time between fragmentation checks (min 10 ms).
- Runs only on the IPC master (Standalone or master in Local/Network mode). Disabled in `--read-only` mode and on proxy (client) sessions.

## Storage Modes

`StorageMode` (from `src/storage.rs`) controls the file access strategy:

| Mode | Description |
|------|-------------|
| `Mmap` (default) | Memory-mapped I/O |
| `Pread` | Positional `pread`/`pwrite` |
| `FileIo` | User-space cached `pread`/`pwrite` with `OIFS_FILE_IO_CACHE_MB` budget |
| `NoMmap` | Alias for `FileIo` (set by `--no-mmap`) |

## Configuration Interactions

- `--no-mmap` overrides `OIFS_STORAGE_MODE` and sets `StorageMode::FileIo`.
- `--network` requires the `mcp` feature (build-time).
- `io_uring` backend requires Linux + `io_uring` feature + kernel ≥ 5.15 (or `OIFS_ALLOW_PRE_5_15`).
- `--auto-defrag` only takes effect on the master process; proxy sessions warn and ignore it.
- `--read-only` disables auto-defrag, auto-grow, and writes; opens with shared `F_RDLCK`.
- Encryption and compression are independent: encrypted files may still be compressed (compression runs before encryption).
- Password is required for `create --encrypt`, and for opening any encrypted image.
