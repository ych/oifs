---
type: workflow
title: Encrypted Filesystem Workflows
description: Step-by-step guide for creating, opening, and using encrypted OIFS images including password management, key derivation, and transparent encryption/decryption.
tags: [encryption, workflow, cli, security]
sources:
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
  - id: openwiki-source-88657ea41344918d5e874716
    resource: repo://src/encryption.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-04T10:12:53.730Z" }
verified:
  - by: openwiki/0.6.1
    at: 2026-10-05T16:55:45.523Z
---
# Encrypted Filesystem Workflows

This guide covers the complete workflow for working with encrypted OIFS (Optimized Image File System) images, from initial creation through daily operations.

## Creating an Encrypted Filesystem

To create a new encrypted OIFS image, use the `create` command with the `--encrypt` flag:

```bash
oifs -i myimage.img create --size 100 --encrypt
```

During creation, you'll be prompted for a password (unless provided via `--password` or `OIFS_PASSWORD` environment variable). The system will:
1. Generate a cryptographically secure random salt (16 bytes) stored in the superblock
2. Derive a 256-bit encryption key from your password using Argon2id
3. Initialize the filesystem with XChaCha20-Poly1305 encryption for data and ChaCha20-Poly1305 with synthetic IV for filenames

**Password Requirements:**
- Cannot be empty
- Warning issued if shorter than 8 characters (non-JSON mode only)
- In `--json` mode, password must be provided via `--password` or `OIFS_PASSWORD` (no interactive prompt)

## Opening an Encrypted Filesystem

When opening an existing image, the system automatically detects encryption by attempting to open without credentials first. If a password is required, it follows this precedence:
1. `--password` command-line argument
2. `OIFS_PASSWORD` environment variable
3. Interactive prompt (suppressed in `--json` mode)

Example opening for various operations:
```bash
# Interactive password prompt
oifs -i myimage.img ls

# Using password flag
oifs -i myimage.img --password mysecret put hostfile.txt

# Using environment variable
export OIFS_PASSWORD=mysecret
oifs -i myimage.img get remote.txt

# JSON mode (requires explicit password)
oifs -i myimage.img --json --password mysecret ls
```

## Using Encrypted Filesystems

Once successfully opened, all filesystem operations work transparently:
- **Data Encryption**: File contents are encrypted/decrypted on-the-fly using XChaCha20-Poly1305 with unique nonces
- **Filename Encryption**: Filenames are encrypted deterministically using ChaCha20-Poly1305 with synthetic IV (parent inode as tweak), producing `_e_` prefixed Base64URL names
- **Directory Operations**: Directory listings show decrypted filenames while maintaining encrypted storage
- **All Commands Work**: `put`, `get`, `ls`, `mkdir`, `append`, `analyze`, `defrag`, `fsck` function identically to unencrypted filesystems

## Password Management

### Sources
Passwords can be supplied through three mechanisms (in order of precedence):
1. Command-line flag: `--password <password>`
2. Environment variable: `OIFS_PASSWORD`
3. Interactive terminal prompt (hidden input)

### Best Practices
- Use environment variables or password prompts to avoid exposing passwords in process lists
- In automated environments, consider using `--password` with secure credential passing
- For JSON mode integrations, explicit password provision is required
- The system prevents empty passwords and warns about short passwords (<8 chars) in interactive mode

## Encryption Details

### Key Derivation
- Algorithm: Argon2id (via `argon2` crate)
- Parameters: Default settings (adjustable in future versions)
- Salt: 16-byte cryptographically random value stored in filesystem superblock
- Output: 256-bit key for XChaCha20-Poly1305

### Data Encryption
- Cipher: XChaCha20-Poly1305 AEAD
- Key: 256-bit derived key
- Nonce: 192-bit unique value generated per encryption operation
- Authentication: 16-bit tag included with ciphertext

### Filename Encryption
- Cipher: ChaCha20-Poly1305 AEAD
- Mode: Synthetic IV (SIV) for deterministic encryption
- Tweak: Parent inode number ensures same name in different directories encrypts differently
- Format: `_e_` + Base64URL-encoded (nonce || ciphertext || tag)
- Already encrypted names (starting with `_e_`) are left unchanged for compatibility

## Workflow Examples

### Creating and Populating
```bash
# Create 1GB encrypted image
oifs -i secure.img create --size 1024 --encrypt
# Enter password when prompted

# Add files (password prompted again)
oifs -i secure.img put document.pdf
oifs -i secure.img put --compress photo.png

# Create directory structure
oifs -i secure.img mkdir projects
oifs -i secure.img put --remote-name projects/source.zip source.zip
```

### Daily Usage
```bash
# List contents (password prompt)
oifs -i secure.img ls

# Retrieve file
oifs -i secure.img get projects/source.zip ./source.zip

# Append to log
oifs -i secure.img append log.txt "$(date): Backup completed"

# Verify integrity
oifs -i secure.img fsck
```

### Automated Backup Script
```bash
#!/bin/bash
export OIFS_PASSWORD=$(cat /secure/backup_password)
oifs -i backup.img --password "$OIFS_PASSWORD" put --compress database.sql
oifs -i backup.img --password "$OIFS_PASSWORD" put --compress logs.tar.gz
```

## Related Information

- For architectural details of the encryption implementation, see [Encryption Architecture](../architecture/encryption.md)
- For basic filesystem operations, refer to [Basic Operations Workflow](../workflows/basic_operations.md)
- For FFI integration details, see [FFI Interface](../architecture/ffi_interface.md)

> **Note**: Encrypted filesystems cannot be converted to/from unencrypted format. Backup your password securely—there is no recovery mechanism.
