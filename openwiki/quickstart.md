---
type: guide
title: Quickstart
description: A step-by-step beginner's guide to building OIFS, creating your first container filesystem image, performing basic file and directory operations, using encryption, and exploring compression filters.
tags: [quickstart, guide, tutorial, build, cli, encryption, compression]
verified:
  - by: openwiki/0.6.1
    at: 2026-09-29T16:14:34.721Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-23775c3de52f3ab95a13cb8b
    resource: repo://README.md
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
generated: { by: "pi", at: "2026-09-29T16:14:34.721Z" }
---

## Introduction

**OIFS (O's Inode File System)** is a single-file containerized Unix-like inode filesystem implemented in Rust. It packages an entire filesystem—complete with permissions, directories, compression, and encryption—into a portable `.img` container file.

This quickstart guide walks you through:
1. Building OIFS from source.
2. Creating and inspecting your first filesystem image.
3. Importing, exporting, and managing files and directories.
4. Working with encrypted images.
5. Next steps and documentation links.

## 1. Prerequisites and building

OIFS requires the Rust toolchain (edition 2024 / Rust 1.85+).

### Standard build

Clone the repository and build the release binaries:

```bash
cargo build --release
```

This compiles:
- `target/release/oifs`: The primary command-line tool.
- `target/release/liboifs.so` (or `liboifs.dylib` on macOS): The C-compatible FFI shared library.

### Building with AI Agent MCP support

To include the Model Context Protocol (MCP) server for integration with AI coding assistants (Claude Desktop, Cursor, Antigravity):

```bash
cargo build --release --features mcp
```

This additionally produces `target/release/oifs_mcp`.

## 2. Creating your first filesystem image

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

## 3. Basic file and directory operations

All filesystem commands require `-i <image_path>`.

### Creating directories (`mkdir`)

Create a subdirectory named `documents` inside the image:

```bash
./target/release/oifs -i my_disk.img mkdir documents
```

### Importing files (`put`)

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

### Listing directory contents (`ls`)

List the contents of the image:

```bash
./target/release/oifs -i my_disk.img ls
```

Output:
```text
=== Directory Listing: "/" ===
NAME                 KIND       SIZE COMP_SIZE MODIFIED
--------------------------------------------------------------------------------
documents            dir        4096         - 2026-09-30 01:30:00
sample.txt           file         27         - 2026-09-30 01:30:15
```

To view nested contents recursively:

```bash
./target/release/oifs -i my_disk.img ls -r
```

### Appending content (`append`)

Append text to an existing or new file in the container:

```bash
./target/release/oifs -i my_disk.img append documents/notes.txt "Appending line 2."
```

### Exporting files (`get`)

Read and export files back from the image to your host machine:

```bash
./target/release/oifs -i my_disk.img get documents/notes.txt exported_notes.txt
cat exported_notes.txt
```

Output:
```text
Hello from OIFS container!
Appending line 2.
```

## 4. Encrypted filesystems 🔒

OIFS includes end-to-end authenticated encryption via XChaCha20-Poly1305 and Argon2id.

### Creating an encrypted image

Add the `--encrypt` flag when creating the image:

```bash
./target/release/oifs -i secure.img create --size 10 --encrypt
```

Terminal prompt:
```text
Enter password: [hidden]
Confirm password: [hidden]
✅ Encrypted filesystem created: "secure.img"
```

### Accessing encrypted images

When running commands against an encrypted image, OIFS automatically detects encryption and securely prompts for your passphrase:

```bash
./target/release/oifs -i secure.img put sample.txt
# 🔒 Encrypted filesystem detected. Enter password: [hidden]
```

### Non-interactive / scripted password handling

For scripts and CI/CD pipelines, provide the password using the environment variable or `--password` argument:

```bash
# Using environment variable (recommended)
export OIFS_PASSWORD="my-secret-passphrase"
./target/release/oifs -i secure.img ls

# Using command-line argument
./target/release/oifs -i secure.img --password "my-secret-passphrase" ls
```

## 5. Pre-compression filters and analysis

OIFS features Blosc2-style data filters (Delta, ByteShuffle, BitShuffle) that collapse Shannon entropy before Zstd compression:

### Analyzing host data before import

Analyze numerical or tabular data to find the optimal filter configuration:

```bash
./target/release/oifs filter-analyze data.bin
```

Output recommends the ideal pipeline (e.g. `--filter both --typesize 4` for 32-bit floats), boosting compression ratios up to 390x.

### Importing with filters

```bash
./target/release/oifs -i my_disk.img put data.bin --filter both --typesize 4 --compress
```

## 6. Structural diagnostics (`fsck`)

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

## Next steps & documentation links

<!-- openwiki: broken internal link [openwiki/architecture/overview.md] file "openwiki/architecture/overview.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [OIFS Architecture Overview](openwiki/architecture/overview.md) — Subsystem layout, memory-mapped storage engine, and block allocation.
<!-- openwiki: broken internal link [openwiki/architecture/compression_and_filters.md] file "openwiki/architecture/compression_and_filters.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Compression and Filters](openwiki/architecture/compression_and_filters.md) — Deep dive into Blosc2 filters, Delta encoding, and Zstd multi-frame streams.
<!-- openwiki: broken internal link [openwiki/architecture/encryption.md] file "openwiki/architecture/encryption.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Encryption Subsystem](openwiki/architecture/encryption.md) — Cryptographic design, 192-bit nonces, and deterministic SIV filename privacy.
<!-- openwiki: broken internal link [openwiki/operations/cli_reference.md] file "openwiki/operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [CLI Reference](openwiki/operations/cli_reference.md) — Complete manual for all subcommands, options, and JSON pipelines.
<!-- openwiki: broken internal link [openwiki/operations/testing_and_verification.md] file "openwiki/operations/testing_and_verification.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Testing and Formal Verification](openwiki/operations/testing_and_verification.md) — Guide to running integration tests, Shuttle concurrency permutations, and Kani proofs.
