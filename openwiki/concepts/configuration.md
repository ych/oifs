---
type: concept
title: Configuration and Durability Policies
description: Build-time features (mcp, compression, encryption), runtime options (durability mode, io backend selection), and environment variables affecting OIFS behavior.
tags: [configuration, features, environment-variables, cli-options, build-flags]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-09T15:04:29.410Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-ea9e30b0c99ad48bf309d4ab
    resource: repo://src/io_engine.rs
  - id: openwiki-source-ef0afc6eaf5c925c9975314d
    resource: repo://src/ipc.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-09T15:04:29.410Z" }
---

OIFS provides multiple configuration mechanisms to tailor functionality for different use cases, from embedded systems to high-performance clusters. Configuration occurs at build time via Cargo features, at runtime via CLI flags and environment variables, and through persistent filesystem properties.

## Build-Time Features

Optional functionality is controlled through Cargo features in `Cargo.toml`:

| Feature | Description | Default |
|---------|-------------|---------|
| `default` | Enables `mcp` and `io_uring` features | Yes |
| `mcp` | Adds MCP (Model Context Protocol) server capabilities | Yes (via default) |
| `io_uring` | Enables Linux io_uring backend for accelerated I/O | Yes (via default, Linux-only) |
| Encryption deps | `chacha20poly1305`, `argon2`, `rand`, `zeroize`, `blake2`, `base64ct` | Always linked |
| Compression deps | `zstd`, `blosc2`, `rayon` | Always linked |

The `io_uring` feature is conditional: it only links the `io-uring` dependency on Linux targets. On other platforms or when disabled, the io_uring backend compiles to nothing and falls back to `pread`.

## Runtime Configuration

### Environment Variables

| Variable | Purpose | Values |
|----------|---------|--------|
| `OIFS_IO_BACKEND` | Selects I/O backend for payload reads | `mmap`, `pread`, `io_uring` (or aliases `uring`, `iouring`) |
| `OIFS_PASSWORD` | Provides password for encrypted filesystems | String (overrides `--password` flag unless in `--json` mode) |

The I/O backend selection follows this precedence:
1. Explicit request via `IoEngine` construction
2. `OIFS_IO_BACKEND` environment variable (parsed case-insensitively)
3. Default: `IoBackend::Mmap`

Requested backends that are unavailable (e.g., `io_uring` on non-Linux, missing kernel support, or feature disabled) silently fall back to `IoBackend::Pread`. The actual backend in use can be queried via `IoBackend::resolve()`.

### CLI Flags

Global flags affecting all commands:
- `--image <path>`: Path to OIFS image file
- `--password <pwd>`: Password for encrypted filesystem (overrides `OIFS_PASSWORD`)
- `--network` / `-n`: Enable cross-machine/network mode
- `--bind <addr>`: Custom bind address for network mode (e.g., `0.0.0.0:9050`)
- `--json`: Output results as minified JSON (suppresses interactive password prompts)

Command-specific configuration:
- `create`: `--size <MB>`, `--encrypt`
- `put`: `--compress`, `--no-compress`, `--filter <type>`, `--typesize <bytes>`
- `defrag`: `--mode <safe\|inplace>`

### Filesystem Properties

Properties stored in the superblock and configurable via CLI or API:

#### Compression Mode
Controls when files are automatically compressed using Zstandard:
- `Always`: Compress all files regardless of size
- `Never`: Disable compression entirely
- `Auto` (default): Compress files ≥ 8KB
- `Stream`: Full-file stream compression with explicit Zstd compression level
- `Seekable`: Seekable chunked compression (64KB independent chunks)
- `Stream`: Full-file stream compression with explicit Zstd compression level
- `Seekable`: Seekable chunked compression (64KB independent chunks)

Set via `DiskManager::set_compression_mode()` or CLI `put` command flags.

#### Defragmentation Mode
Determines how defragmentation operations are performed:
- `Safe` (default): Create new image and replace original after success
- `InPlace`: Directly modify original image (faster but risks corruption on failure)

#### Durability Mode
<!-- openwiki: broken internal link [../architecture/async_io_and_engines.md#durability-modes] heading anchor "durability-modes" does not exist in "../architecture/async_io_and_engines.md". Fix the href or restore the target, then delete this comment. -->
Balances power-loss resilience against write throughput ([detailed explanation](../architecture/async_io_and_engines.md#durability-modes)):
- `Lazy` (default): Updates mmap/page cache without per-mutation `msync`; survives process crashes; requires explicit `flush()` for power-loss safety
- `RangeAsync`: Asynchronously flushes only modified byte ranges via `msync(MS_ASYNC)`
- `Strict`: Synchronously flushes modified byte ranges via `msync(MS_SYNC)` before returning
- `LegacyWholeMmapAsync`: Asynchronously flushes entire virtual memory map after each mutation (pre-P3.3 behavior)

Set via `DiskManager::set_durability_mode()`; current mode readable via `durability_mode()`.

## Feature Interaction Notes

- Encryption and compression are independent: encrypted files may still be compressed
- Network mode does not require the `mcp` feature flag; it uses standard TCP transport
- io_uring backend provides greatest benefit with `RangeAsync` or `Strict` durability modes where asynchronous msync can overlap with I/O submissions
- In `--json` mode, interactive password prompts are disabled; encryption requires `--password` or `OIFS_PASSWORD`
