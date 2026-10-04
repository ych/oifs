---
type: guide
title: Quickstart
description: A step-by-step beginner's guide to building OIFS, creating your first container filesystem image, performing basic file and directory operations, using encryption, and exploring compression filters.
tags: [quickstart, guide, tutorial, build, cli, encryption, compression, io_engine]
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-23775c3de52f3ab95a13cb8b
    resource: repo://README.md
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
generated: { by: "antigravity", at: "2026-10-03T11:29:24.571Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-04T06:56:29.774Z
---

# Quickstart

**OIFS (O's Inode File System)** is a single-file containerized Unix-like inode filesystem implemented in Rust. It packages an entire filesystem—complete with permissions, directories, compression, and encryption—into a portable `.img` container file.

This quickstart guide walks you through:
1. Building OIFS from source.
2. Creating and inspecting your first filesystem image.
3. Importing, exporting, and managing files and directories.
4. Working with encrypted images.
5. Next steps and documentation links.

## 1. Prerequisites and Building

OIFS requires the Rust toolchain (edition 2024 / Rust 1.85+).

### Standard Build

Clone the repository and build the release binaries:

```bash
cargo build --release
```

This compiles:
- `target/release/oifs`: The primary command-line tool.
- `target/release/liboifs.so` (or `liboifs.dylib` on macOS): The C-compatible FFI shared library.

### Building with AI Agent MCP Support

To include the Model Context Protocol (MCP) server for integration with AI coding assistants (Claude Desktop, Cursor, Antigravity):

```bash
cargo build --release --features mcp
```

This additionally produces `target/release/oifs_mcp`.

## 2. Creating Your First Filesystem Image

Every OIFS filesystem lives inside a single `.img` file. Let's create a 10MB filesystem image named `my_disk.img`:

```bash
# Using cargo run
cargo run --release --bin oifs -- -i my_disk.img create --size 10

# Or using the compiled binary directly
./target/release/oifs -i my_disk.img create --size 10
```

Output:
```text
Created image "my_disk.img" with size 10MB
```

The tool allocates a 10MB container on your host machine, formats Block 0 as the SuperBlock, initializes the inode and data block bitmaps, and allocates the root directory (inode 0).

## 3. Basic File and Directory Operations

All filesystem commands require `-i <image_path>`.

### Creating Directories (`mkdir`)

Create a subdirectory named `documents` inside the image:

```bash
./target/release/oifs -i my_disk.img mkdir documents
```

### Importing Files (`put`)

Create a sample text file on your host machine:

```bash
echo "Hello from OIFS container!" > sample.txt
```

Import `sample.txt` into the root of the image:

```bash
./target/release/oifs -i my_disk.img put sample.txt
```

You can also import files directly into subdirectories or specify a new remote name:

```bash
./target/release/oifs -i my_disk.img put sample.txt documents/notes.txt
```

### Listing Directory Contents (`ls`)

List files in the root directory:

```bash
./target/release/oifs -i my_disk.img ls
```

Output displays permissions, size, modification timestamp, and filename.

### Reading and Exporting Files (`cat` and `get`)

View file contents directly to standard output:

```bash
./target/release/oifs -i my_disk.img cat sample.txt
```

Export a file from the image back to your host machine:

```bash
./target/release/oifs -i my_disk.img get sample.txt restored.txt
```

### Deleting Files (`rm`)

```bash
./target/release/oifs -i my_disk.img rm sample.txt
```

## 4. Working with Encrypted Images

OIFS provides military-grade at-rest encryption powered by XChaCha20-Poly1305 AEAD, Argon2id key derivation, and deterministic SIV filename encryption.

### Creating an Encrypted Image

```bash
./target/release/oifs -i secure.img create --size 20 --password "my-secret-passphrase"
```

### Accessing Encrypted Images

When running commands against an encrypted image, OIFS automatically detects encryption and securely prompts for your passphrase:

```bash
./target/release/oifs -i secure.img put sample.txt
# 🔒 Encrypted filesystem detected. Enter password: [hidden]
```

### Non-Interactive / Scripted Password Handling

For scripts and CI/CD pipelines, provide the password using the environment variable or `--password` argument:

```bash
# Using environment variable (recommended)
export OIFS_PASSWORD="my-secret-passphrase"
./target/release/oifs -i secure.img ls

# Using command-line argument
./target/release/oifs -i secure.img --password "my-secret-passphrase" ls
```

## 5. Pre-Compression Filters and Analysis

OIFS features Blosc2-style data filters (Delta, ByteShuffle, BitShuffle) that collapse Shannon entropy before Zstd compression:

### Analyzing Host Data Before Import

Analyze numerical or tabular data to find the optimal filter configuration:

```bash
./target/release/oifs filter-analyze data.bin
```

Output recommends the ideal pipeline (e.g. `--filter both --typesize 4` for 32-bit floats), boosting compression ratios up to 390x.

### Importing with Filters

```bash
./target/release/oifs -i my_disk.img put data.bin --filter both --typesize 4 --compress
```

## 6. Structural Diagnostics (`fsck`)

Verify that the image is structurally intact:

```bash
./target/release/oifs -i my_disk.img fsck
```

Output:
```text
=== Filesystem Consistency Check (fsck) ===
Status:              ✅ CLEAN
Orphan Inodes:       0
Leaked Blocks:       0
Missing Blocks:      0
Cross-linked Blocks: 0
```

## Next Steps & Documentation Links

<!-- openwiki: broken internal link [openwiki/architecture/overview.md] file "openwiki/architecture/overview.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [OIFS Architecture Overview](openwiki/architecture/overview.md) — Subsystem layout, memory-mapped storage engine, and block allocation.
<!-- openwiki: broken internal link [openwiki/architecture/async_io_and_engines.md] file "openwiki/architecture/async_io_and_engines.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Asynchronous I/O and Pluggable Engines](openwiki/architecture/async_io_and_engines.md) — Deep dive into Mmap, Pread, Linux io_uring backends, ExtentList coalescing, and kernel gating.
<!-- openwiki: broken internal link [openwiki/architecture/disk_manager_and_persistence.md] file "openwiki/architecture/disk_manager_and_persistence.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [DiskManager and Persistence Model](openwiki/architecture/disk_manager_and_persistence.md) — Storage engine coordinator, durability policies, and read/write pipelines.
<!-- openwiki: broken internal link [openwiki/architecture/compression_and_filters.md] file "openwiki/architecture/compression_and_filters.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Compression and Filters](openwiki/architecture/compression_and_filters.md) — Deep dive into Blosc2 filters, Delta encoding, and Zstd multi-frame streams.
<!-- openwiki: broken internal link [openwiki/architecture/encryption.md] file "openwiki/architecture/encryption.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Encryption Subsystem](openwiki/architecture/encryption.md) — Cryptographic design, 192-bit nonces, and deterministic SIV filename privacy.
<!-- openwiki: broken internal link [openwiki/architecture/ffi_interface.md] file "openwiki/architecture/ffi_interface.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [C FFI Interface](openwiki/architecture/ffi_interface.md) — Shared library bindings, handle lifecycle, and I/O backend selection.
<!-- openwiki: broken internal link [openwiki/operations/cli_reference.md] file "openwiki/operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [CLI Reference](openwiki/operations/cli_reference.md) — Complete manual for all subcommands, options, and JSON pipelines.
<!-- openwiki: broken internal link [openwiki/operations/testing_and_verification.md] file "openwiki/operations/testing_and_verification.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Testing and Formal Verification](openwiki/operations/testing_and_verification.md) — Guide to running integration tests, Shuttle concurrency permutations, and Kani proofs.
