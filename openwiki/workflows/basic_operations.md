---
type: workflow
title: Basic File Operations Workflow
description: End-to-end guide for creating images, importing files, directory operations, and common filesystem tasks in OIFS.
tags: [filesystem, cli, tutorial, basic-operations]
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-04T10:12:53.730Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-05T16:55:45.523Z
---
# Basic File Operations Workflow

This guide walks through the essential operations for managing an OIFS (Optimized In-place File System) image: creating an image, importing files with compression options, managing directories, and performing common file tasks like appending, exporting, and maintenance.

## Creating an OIFS Image

To start, create a new OIFS image file. The `create` command initializes the filesystem structures.

```bash
# Create a 10MB image (default size)
oifs -i myfs.img create

# Create an encrypted image (will prompt for password)
oifs -i secret.img create --encrypt

# Create a larger image (size in MB)
oifs -i large.img create --size 100
```

**Notes:**
- If the image file already exists, the command will fail to prevent overwriting.
- For encrypted images in JSON mode (`--json`), provide the password via `--password` or `OIFS_PASSWORD` environment variable.
- The session automatically handles encryption detection and password prompting when needed.

## Importing Files

Use the `put` command to import files from the host into the OIFS image. This command supports various compression and filtering options.

```bash
# Import a file with automatic compression decision
oifs -i myfs.img put ./host-file.txt

# Import without compression (store as-is)
oifs -i myfs.img put --no-compress ./host-file.txt

# Force compression regardless of size
oifs -i myfs.img put --compress ./host-file.txt

# Specify a custom name in the image
oifs -i myfs.img put ./host-file.txt --remote-name stored-as.txt

# Apply a specific compression filter (e.g., shuffle)
oifs -i myfs.img put --filter shuffle --typesize 4 ./data.bin

# Let OIFS automatically analyze and select the best filter
oifs -i myfs.img put --filter auto ./data.bin
```

**Compression Modes:**
- `auto` (default): Uses heuristics to decide whether to compress based on content.
- `always` (`--compress`): Always compress the file.
- `never` (`--no-compress`): Store the file uncompressed.

**Filters:**
- `none`: No pre-filter (raw compression).
- `delta`: Delta filter (good for sequential data).
- `shuffle`: Byte shuffle (good for structured data).
- `bitshuffle`: Bit shuffle (good for sparse data).
- `both`/`numeric`: Combines delta and shuffle.
- `auto`: Analyzes the data and recommends the optimal filter.

**Import Process:**
1. Validate that both the image and host file exist.
2. Resolve the parent directory and check for name conflicts.
3. Create an inode for the new file.
4. Read the host file content.
5. Apply selected filter and compression mode.
6. Write the data to the filesystem.

## Directory Operations

Organize files within the OIFS image using directories.

```bash
# Create a directory
oifs -i myfs.img mkdir documents

# Create nested directories (requires intermediate dirs to exist)
oifs -i myfs.img mkdir documents/2024/reports
```

**Notes:**
- The `mkdir` command fails if a file or directory with the same name already exists.
- For nested paths, ensure parent directories exist or create them first.

## Listing Contents

View the contents of the image with the `ls` command.

```bash
# List root directory
oifs -i myfs.img ls

# List a specific path
oifs -i myfs.img ls documents

# List recursively
oifs -i myfs.img ls -r

# Output as JSON for scripting
oifs -i myfs.img --json ls -r
```

**Output Fields:**
- `Name`: File or directory name.
- `Size`: Uncompressed size in bytes.
- `CompSize`: Compressed size (if applicable, otherwise `-`).
- `Modified`: Last modification timestamp.
- Kind is indicated by a leading `d` for directories or `-` for files in the detailed view.

## Common File Tasks

### Appending to Files

Add content to an existing file without rewriting the entire file.

```bash
# Append a string to a file
oifs -i myfs.img append log.txt "New log entry\n"

# Append without adding a newline
oifs -i myfs.img append data.csv ",extra_value" --no-newline
```

**Behavior:**
- If the file does not exist, it is created.
- Existing content is preserved and new content is added to the end.
- By default, a newline is appended unless `--no-newline` is specified.

### Exporting Files

Retrieve files from the OIFS image to the host filesystem.

```bash
# Export a file to the current directory with the same name
oifs -i myfs.img get config.ini

# Export to a specific host path
oifs -i myfs.img get config.ini ./backup/config.ini

# Export and rename
oifs -i myfs.img get config.ini ./backup/application.conf
```

**Notes:**
- The destination directory is created automatically if it does not exist.
- If no host path is provided, the file is saved with its OIFS name in the current directory.

## Maintenance and Analysis

### Fragmentation Analysis

Check how fragmented the filesystem is.

```bash
# Analyze fragmentation
oifs -i myfs.img analyze

# JSON output for programmatic use
oifs -i myfs.img --json analyze
```

**Metrics:**
- Total, used, and free blocks.
- Number of free runs (contiguous free block sequences).
- Largest free run size.
- Average gap size between used blocks.
- Fragmentation ratio (higher indicates more fragmentation).

### Defragmentation

Reduce fragmentation to improve performance.

```bash
# Defragment using safe mode (default)
oifs -i myfs.img defrag

# Defragment using inplace mode (faster but riskier on interruption)
oifs -i myfs.img defrag --mode inplace
```

**Modes:**
- `safe`: Copy data to new locations before freeing old ones (survives interruptions).
- `inplace`: Move data directly (faster but may leave filesystem inconsistent if interrupted).

### Filesystem Check

Verify structural consistency.

```bash
# Check filesystem integrity
oifs -i myfs.img fsck

# JSON output
oifs -i myfs.img --json fsck
```

**Checks:**
- Validates inode and directory structures.
- Ensures allocation bitmap consistency.
- Reports whether the filesystem is clean.

## Example Workflow

Here's a complete example demonstrating typical usage:

```bash
# 1. Create a 20MB encrypted image
oifs -i project.img create --size 20 --encrypt
# (Enter password when prompted)

# 2. Create directories for organization
oifs -i project.img mkdir src
oifs -i project.img mkdir docs
oifs -i project.img mkdir build

# 3. Import source code with compression
oifs -i project.img put --compress ./main.c src/main.c
oifs -i project.img put --compress ./utils.h src/utils.h

# 4. Import documentation without compression (already compressed formats)
oifs -i project.img put --no-compress ./design.pdf docs/design.pdf
oifs -i project.img put --no-compress ./readme.md docs/readme.md

# 5. Append to a log file
oifs -i project.img put --compress ./initial.log build/build.log
oifs -i project.img append build/build.log "Compilation started at $(date)\n"

# 6. List contents to verify
oifs -i project.img ls -r

# 7. Check for fragmentation after many writes
oifs -i project.img analyze

# 8. Defragment if needed
oifs -i project.img defrag

# 9. Final integrity check
oifs -i project.img fsck
```

## Additional Notes

### JSON Mode
Use `--json` for machine-readable output from commands like `ls`, `put`, `get`, `append`, `analyze`, `defrag`, and `fsck`. This facilitates scripting and integration with other tools.

### Encryption
Encrypted images require a password for all operations after creation. Provide it via:
- `--password` flag (visible in process list)
- `OIFS_PASSWORD` environment variable
- Interactive prompt (default when needed and not in JSON mode)

### Error Handling
Most commands will fail with a clear error message if:
- The image file does not exist.
- A host file to import does not exist.
- A file or directory already exists when creating.
- Password is missing or incorrect for encrypted images.
- The filesystem is corrupted (detected by `fsck` or failed operations).

### Cross-Machine Access
<!-- openwiki: broken internal link [../encrypted_workflows.md] file "../encrypted_workflows.md" does not exist. Fix the href or restore the target, then delete this comment. -->
For network or multi-node usage, see the [encrypted workflows](../encrypted_workflows.md) and use `--network` or `--bind` options to enable IPC server mode.

---

*This workflow covers the core functionality for day-to-day OIFS usage. For advanced features like filter analysis, refer to the CLI reference or experiment with the `filter-analyze` command.*
