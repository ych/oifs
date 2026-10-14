---
type: quickstart_guide
title: Quick Start Guide
description: Step-by-step instructions to build OIFS, create a disk image, and perform basic file operations.
tags: [quickstart, getting-started, tutorial, cli]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-13T15:40:56.857Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-23775c3de52f3ab95a13cb8b
    resource: repo://README.md
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-13T15:40:56.857Z" }
---

# Quick Start Guide

This guide provides a quick introduction to building and using OIFS.

## Build

To build the project in release mode:

```bash
cargo build --release
```

## Create an Image

Create a standard 10MB image:

```bash
cargo run --bin oifs -- -i disk.img create --size 10
```

Create an encrypted image (prompts for password securely):

```bash
cargo run --bin oifs -- -i encrypted.img create --size 10 --encrypt
```

Create an image with journaling enabled for crash safety:

```bash
cargo run --bin oifs -- -i journal.img create --size 10 --journal
```

## Basic File Operations

Import a file from the host into the image:

```bash
cargo run --bin oifs -- -i disk.img put dataset.bin
```

Export a file from the image to the host:

```bash
cargo run --bin oifs -- -i disk.img get dataset.bin extracted.bin
```

Append content to a file in the image:

```bash
cargo run --bin oifs -- -i disk.img append log.txt "New log entry"
```

Create a directory:

```bash
cargo run --bin oifs -- -i disk.img mkdir data
```

List files in the image:

```bash
cargo run --bin oifs -- -i disk.img ls
```

List files recursively:

```bash
cargo run --bin oifs -- -i disk.img ls --recursive
```

## Analysis and Maintenance

Analyze a file to recommend optimal compression filters:

```bash
cargo run --bin oifs -- -i disk.img filter-analyze dataset.bin
```

Check filesystem structural consistency:

```bash
cargo run --bin oifs -- -i disk.img fsck
```

Analyze disk fragmentation:

```bash
cargo run --bin oifs -- -i disk.img analyze
```

Defragment the filesystem (safe mode):

```bash
cargo run --bin oifs -- -i disk.img defrag
```

Defragment the filesystem (inplace mode):

```bash
cargo run --bin oifs -- -i disk.img defrag --mode inplace
```

## Next Steps

For more information, see:
<!-- openwiki: broken internal link [../workflows/basic_operations.md] file "../workflows/basic_operations.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Basic Operations](../workflows/basic_operations.md)
<!-- openwiki: broken internal link [../operations/index.md] file "../operations/index.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Operations Reference](../operations/index.md)
<!-- openwiki: broken internal link [../workflows/encrypted_workflows.md] file "../workflows/encrypted_workflows.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Encrypted Workflows](../workflows/encrypted_workflows.md)
<!-- openwiki: broken internal link [../workflows/compression_workflows.md] file "../workflows/compression_workflows.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Compression Workflows](../workflows/compression_workflows.md)
