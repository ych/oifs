---
type: quickstart_guide
title: Quick Start Guide
description: Step-by-step instructions to build OIFS, create disk images, and perform basic file operations.
tags: [quickstart, getting-started, tutorial, cli]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-07T12:20:29.772Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-23775c3de52f3ab95a13cb8b
    resource: repo://README.md
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-07T12:20:29.772Z" }
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

## File Import and Export

Import a file from the host into the image:

```bash
cargo run --bin oifs -- -i disk.img put dataset.bin
```

Export a file from the image to the host:

```bash
cargo run --bin oifs -- -i disk.img get dataset.bin extracted.bin
```

## Next Steps

For more information, see:
<!-- openwiki: broken internal link [../workflows/basic_operations.md] file "../workflows/basic_operations.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Basic Operations](../workflows/basic_operations.md)
<!-- openwiki: broken internal link [../operations/index.md] file "../operations/index.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- [Operations](../operations/index.md)
