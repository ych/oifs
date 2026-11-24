---
type: "Reference"
title: "Configuration"
description: "Documents all configuration knobs: CLI flags, environment variables, session modes, compression modes, durability modes, auto-defrag, storage modes, and their interactions."
tags: ["configuration", "cli", "environment", "runtime", "storage", "compression", "durability", "defragmentation", "session", "ipc"]
verified:
  - by: openwiki/0.9.0
    at: 2026-11-23T20:55:55.458Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-ea9e30b0c99ad48bf309d4ab
    resource: repo://src/io_engine.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-23T20:55:55.458Z" }
---

OIFS provides multiple configuration mechanisms to tailor functionality for different use cases, from embedded systems to high-performance clusters. Configuration occurs at build time via Cargo features, at runtime via CLI flags and environment variables, and through persistent filesystem properties stored in the superblock.

## Build-Time Features

Optional functionality is controlled through Cargo features in `Cargo.toml`:

| Feature | Description | Default |
|---------|-------------|---------|
| `default` | Enables `mcp` and `io_uring` features | Yes |
| `mcp` | Adds MCP (Model Context Protocol) server capabilities; required for `--network` mode | Yes (via default) |
| `io_uring` | Enables Linux io_uring backend for accelerated I/O (Linux-only, requires kernel ≥ 5.15 or RHEL 9.3+ backport) | Yes (via default, Linux-only) |
| `blosc2` | Enables native C-Blosc2 interop wrappers (`filters::blosc2_compress`/`blosc2_decompress`) | No |
| Encryption deps | `chacha20poly1305`, `argon2`, `rand`, `zeroize`, `blake2`, `base64ct` | Always linked |
| Compression deps | `zstd`, `blosc2` (optional), `rayon` | Always linked |

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

Global flags affecting all commands:

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

Command-specific configuration:

- `create`: `--size <MB>` (default 10), `--encrypt`, `--journal` (enables metadata WAL)
- `put`: `--compress`, `--no-compress`, `--chunked` (seekable 64KB chunked compression), `--filter <type>` (`none`, `delta`, `shuffle`, `bitshuffle`, `both`, `auto`), `--typesize <bytes>` (1, 2, 4, 8)
- `defrag`: `--mode <safe|inplace>` (default `safe`)

### Session Mode Selection

The session mode determines how processes coordinate access to the same image. It is selected via CLI flags or the `OifsSession` API:

| Mode | CLI Flag | API Method | Transport | Use Case |
|------|----------|------------|-----------|----------|
| `Standalone` | (none) | `get_or_open` | None (direct `mmap`) | Single process, zero overhead |
| `Local` | `--ipc` | `get_or_open_local` | Unix Domain Socket (`/tmp`) | Multi-process on same machine |
| `Network` | `--network` / `-n` (optionally `--bind`) | `get_or_open_network` | TCP + rendezvous file (`.image.master`) | NFS, Lustre, multi-node clusters |

In `Network` mode, a rendezvous file `.image.master` is created in the image directory containing the master's bind address, PID, and an authentication token. The transport is plain TCP without TLS; non-loopback binds require `--allow-remote-ipc` and should only be used over encrypted tunnels.

### Storage Mode Selection

OIFS supports two image storage backends over the exact same on-disk format:

| Mode | Description | Trade-offs |
|------|-------------|------------|
| `Mmap` (default) | Maps the entire `.img` container with `memmap2::MmapMut` for zero-copy memory access (40–58 GB/s on resident pages) | Risk of `SIGBUS` on storage I/O errors; uses `unsafe` mmap |
| `FileIo` | 100% `mmap`-free positional `pread(2)`/`pwrite(2)` I/O backed by in-memory write-through bitmaps, an in-memory WAL mirror (when journaled), and a bounded 16-shard 4 KiB metadata block cache | Eliminates `SIGBUS` crashes and `unsafe` memory-mapping hazards; all I/O strictly in Safe Rust |

The `FileIo` mode also includes a 64-shard bounded Second-Chance Clock user-space 4 KiB data page cache (default 32 MiB, max 4 GiB, configurable via `OIFS_FILE_IO_CACHE_MB`). Large streaming extents > 64 KiB bypass the cache to preserve sequential throughput and prevent scan pollution.

## Filesystem Properties (Superblock-Persistent)

Properties stored in the superblock and configurable via CLI or API (`DiskManager` methods).

### Compression Mode (`CompressionMode`)

Controls when files are compressed using Zstandard:

| Variant | Description |
|---------|-------------|
| `Always` | Compress all files regardless of size (full-file stream) |
| `Never` | Disable compression entirely |
| `Auto` (default) | Compress files ≥ 8KB |
| `Stream { level: i32 }` | Full-file stream compression with explicit Zstd compression level (1–19, 0 = default level 3) |
| `Seekable { chunk_size: u32, level: i32 }` | Seekable chunked compression with independent chunks (default chunk 64KB, level 1 recommended) |

Set via `DiskManager::set_compression_mode()` or CLI `put` flags (`--compress`, `--no-compress`, `--chunked`).

### Defragmentation Mode (`DefragMode`)

Determines how defragmentation operations are performed:

| Variant | Description |
|---------|-------------|
| `Safe` (default) | Create new image and replace original after success |
| `InPlace` | Directly modify original image (faster but risks corruption on failure) |

Set via `DiskManager::defragment()` or CLI `defrag --mode`.

### Durability Mode (`DurabilityMode`)

Balances power-loss resilience against write throughput. The default is `Lazy`.

| Mode | Value | Behavior | Throughput (Bulk Writes) |
|------|-------|----------|--------------------------|
| `Lazy` (default) | 0 | Updates mmap/page cache without per-mutation `msync`. Survives process crashes; requires explicit `flush()` for power-loss safety. | Baseline (~45× faster than `Strict`) |
| `RangeAsync` | 1 | Asynchronously flushes only modified byte ranges via `msync(MS_ASYNC)` on each mutation, avoiding full VM scans. | Near-`Lazy` for small writes; overhead grows with block count |
| `Strict` | 2 | Synchronously flushes modified byte ranges via `msync(MS_SYNC)` on every mutation. Guarantees physical persistence before return. | Slowest (~45× slower than `Lazy`) |
| `LegacyWholeMmapAsync` | 3 | Asynchronously flushes entire virtual memory map after every mutation (pre-P3.3 behavior). | Deprecated |

Set via `DiskManager::set_durability_mode()`; current mode readable via `durability_mode()`.

**Benchmark context:** The ~45× throughput difference between `Lazy` and `Strict` is measured in `tests/journal_bench.rs` (ignored bench `bench_durability_mode_cost`) using 500 files × 512 B on a journaled image. `RangeAsync` scales with the number of dirty blocks per mutation; deferred coalescing batches up to 64 ops or 4 MiB before issuing `msync(MS_ASYNC)`.

**Coalescing details:**
- `RangeAsync` bridges gaps up to 1 MiB between dirty ranges, collapsing metadata and nearby data blocks into a single `msync(MS_ASYNC)`.
- `Strict` bridges gaps up to 256 KiB, collapsing separate bitmap and inode `msync(MS_SYNC)` calls across the 132 KiB journal region.

### Filter Configuration (`FilterConfig`)

Pre-compression filters applied to file data before Zstandard compression:

| Filter | Description | Typesize |
|--------|-------------|----------|
| `none` | No filter | N/A |
| `delta` | First-order delta encoding | 1, 2, 4, 8 bytes |
| `shuffle` | Byte shuffle (groups identical significance bytes) | 1, 2, 4, 8 bytes |
| `bitshuffle` | Bit-level shuffle (aggregates identical bits) | 1, 2, 4, 8 bytes |
| `both` / `numeric` | Delta + byte shuffle | 1, 2, 4, 8 bytes |
| `auto` | Analyze content and pick best filter | Auto |

Set via CLI `put --filter --typesize` or API `write_data_with_filters`.

### Auto-Defragmentation Configuration (`AutoDefragConfig`)

Controls the background automatic defragmentation coordinator:

| Parameter | Description | Default | Sanitization |
|-----------|-------------|---------|--------------|
| `enabled` | Whether the coordinator runs | `false` | — |
| `fragmentation_threshold` | Ratio `(free_runs - 1) / (free_blocks - 1)` above which a pass triggers | 0.25 | `NaN` → 0.25; clamped to `[0.0, 1.0]` |
| `check_interval` | Time between fragmentation measurements | 300 s | Raised to 10 ms minimum (`MIN_CHECK_INTERVAL`) |
| `min_free_blocks` | Minimum free data blocks required before a pass | 0 | — |
| `allow_journaled` | Allow defrag on WAL-enabled images | `true` | — |

The `AutoDefragConfig::sanitized()` method normalizes the configuration: it replaces `NaN` thresholds with the default (since comparisons against `NaN` are always false, which would cause defrag on every tick), clamps the threshold to `[0.0, 1.0]`, and raises sub-10 ms intervals to 10 ms to prevent a hot loop.

The coordinator runs on a daemon thread that exits when the `DiskManager` is dropped. Every pass takes the inner **write** lock, so it serializes against metadata mutations exactly like any other writer; no two defrag passes can overlap. On WAL-enabled images, background defragmentation is fully journaled and atomic: payload moves are ordered before metadata commits, and metadata updates are recorded in the WAL with batched checkpointing.

## Superblock Geometry Constraints

The on-disk superblock layout imposes the following constraints (defined in `src/superblock.rs`):

- **Block size**: Fixed at 4096 bytes (`BLOCK_SIZE`).
- **Inode size**: Fixed at 256 bytes per inode.
- **Superblock**: Single block at offset 0 (block 0).
- **Inode bitmap**: Starts at block 1; number of blocks = `ceil(total_inodes / (8 * BLOCK_SIZE))`.
- **Data bitmap**: Follows inode bitmap; number of blocks = `ceil(total_data_blocks / (8 * BLOCK_SIZE))`.
- **Journal (optional)**: If enabled, reserves 33 blocks (1 header + 128 KiB ring) between the data bitmap and inode table.
- **Inode table**: Starts after bitmaps (and journal if present); 32 blocks by default (8192 inodes).
- **Data area**: All remaining blocks.

Total image size must accommodate all regions. Minimum recommended size is ~10 MB for a usable filesystem with journaling.

## Feature Interaction Notes

- Encryption and compression are independent: encrypted files may still be compressed (compression runs before encryption).
- Network mode (`--network`) requires the `mcp` feature flag (enabled by default).
- io_uring backend provides greatest benefit with `RangeAsync` or `Strict` durability modes where asynchronous `msync` can overlap with I/O submissions.
- In `--json` mode, interactive password prompts are disabled; encryption requires `--password` or `OIFS_PASSWORD`.
- The `blosc2` feature is optional and only needed for the C-Blosc2 interop wrappers; OIFS ships its own pure-Rust filter pipeline.
- `StorageMode::FileIo` is fully compatible with journaling (WAL), encryption, and all compression modes.
- Auto-defragmentation only runs on the Master session (Direct mode); client sessions cannot start the coordinator.

## Configuration Source Mapping

| Configuration Axis | Environment Variable | CLI Flag | API Method | Superblock (Persistent) |
|--------------------|---------------------|----------|------------|-------------------------|
| I/O Backend | `OIFS_IO_BACKEND` | — | `DiskManager::set_io_backend()` | No |
| Password | `OIFS_PASSWORD` | `--password` | `open_with_mode(password)` / `open_with_password()` | No (derived at mount) |
| Kernel override | `OIFS_ALLOW_PRE_5_15` | — | — | No |
| Session mode | — | `--ipc`, `--network`, `--bind` | `OifsSession::get_or_open_*()` | No |
| JSON output | — | `--json` | — | No |
| Auto-defrag enable/threshold | — | `--auto-defrag[=THRESHOLD]` | `session.set_auto_defrag(config)` | No |
| Defrag interval | — | `--defrag-interval` | `AutoDefragConfig::with_check_interval()` | No |
| Storage mode | `OIFS_STORAGE_MODE` | `--no-mmap` | `open_with_storage_mode()` / `set_default_storage_mode()` | No |
| FileIo cache budget | `OIFS_FILE_IO_CACHE_MB` | — | `DataPageCache::set_capacity_bytes()` | No |
| Compression mode | — | `put --compress/--no-compress/--chunked` | `DiskManager::set_compression_mode()` | No (per-write) |
| Defrag mode | — | `defrag --mode` | `DiskManager::defragment(mode)` | No (per-operation) |
| Durability mode | — | — | `DiskManager::set_durability_mode()` | No (runtime) |
| Filter config | — | `put --filter --typesize` | `write_data_with_filters()` | No (per-write) |
| Journal enable | — | `create --journal` | `open_journaled()` / `create_encrypted_journaled()` | Yes (superblock flag) |
| Encryption | — | `create --encrypt` | `create_encrypted()` | Yes (superblock flag) |
