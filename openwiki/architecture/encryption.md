---
type: architecture
title: Encryption Subsystem
description: How OIFS provides authenticated at-rest data confidentiality and deterministic directory privacy using XChaCha20-Poly1305 AEAD, Argon2id key derivation, Zeroize key hygiene, and Blake2b-tweak SIV filename encryption.
tags: [encryption, security, aead, xchacha20-poly1305, argon2id, siv, zeroize]
verified:
  - by: openwiki/0.6.1
    at: 2026-09-29T16:14:34.721Z
sources:
  - id: openwiki-source-88657ea41344918d5e874716
    resource: repo://src/encryption.rs
generated: { by: "pi", at: "2026-09-29T16:14:34.721Z" }
---

## Responsibility and ownership

<!-- openwiki: broken internal link [src/encryption.rs] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
The encryption subsystem ([`src/encryption.rs`](src/encryption.rs)) provides cryptographic privacy and authenticated integrity for stored files, directory entries, and on-disk metadata.

It isolates cryptographic primitives into three clean layers:
<!-- openwiki: broken internal link [src/encryption.rs#L13-L34] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
1. **Key Management and Hygiene**: Passphrase hashing and automatic secret erasing via [`EncryptionKey`](src/encryption.rs#L13-L34).
<!-- openwiki: broken internal link [src/encryption.rs#L71-L93] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/encryption.rs#L95-L117] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
2. **File Payload AEAD**: Authenticated payload confidentiality via [`encrypt_data`](src/encryption.rs#L71-L93) and [`decrypt_data`](src/encryption.rs#L95-L117).
<!-- openwiki: broken internal link [src/encryption.rs#L142-L178] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
<!-- openwiki: broken internal link [src/encryption.rs#L184-L232] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
3. **Deterministic Filename Privacy**: Directory structure and file naming privacy via Synthetic Initialization Vector (SIV) encryption in [`encrypt_filename`](src/encryption.rs#L142-L178) and [`decrypt_filename`](src/encryption.rs#L184-L232).

## Key derivation and memory hygiene

### Memory hygiene with ZeroizeOnDrop

<!-- openwiki: broken internal link [src/encryption.rs#L13-L34] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Cryptographic keys must never linger in memory or bleed into swap space or crash dumps. The [`EncryptionKey`](src/encryption.rs#L13-L34) struct wraps a 256-bit (32-byte) key with `zeroize` macros:

```rust
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct EncryptionKey {
    key: [u8; 32],
}
```

- When an `EncryptionKey` instance goes out of scope, the `ZeroizeOnDrop` trait automatically executes volatile memory zeroization on `key`.
- The `fmt::Debug` implementation explicitly overrides formatted output with `EncryptionKey([REDACTED])` (`src/encryption.rs#L30-L34`), preventing secrets from accidentally leaking into log outputs or panic strings.

### Argon2id password-based key derivation (PBKDF)

<!-- openwiki: broken internal link [src/encryption.rs#L44-L69] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
Keys are derived from user-supplied passphrases using Argon2id via [`derive_key`](src/encryption.rs#L44-L69):
- **Salt Management**: A cryptographically secure 16-byte random salt is generated with `OsRng` via `generate_salt()` (`src/encryption.rs#L128-L133`). This salt is permanently stored in plaintext within the filesystem's `SuperBlock::encryption_salt` (`src/superblock.rs`).
- **Rainbow Table and GPU Resistance**: Argon2id combines data-dependent and data-independent memory access passes, providing state-of-the-art resistance against both side-channel cache timing attacks and GPU/ASIC password cracking.
- The 256-bit output hash is copied into `EncryptionKey::from_bytes(key)`.

## File payload AEAD: XChaCha20-Poly1305

File content encryption uses the extended-nonce variant of ChaCha20-Poly1305 (`XChaCha20Poly1305`, `src/encryption.rs#L71-L117`):

### 192-bit per-file nonces

Standard ChaCha20-Poly1305 uses a 96-bit nonce, which risks catastrophic key/nonce reuse collisions if nonces are generated randomly over billions of files. In contrast, XChaCha20-Poly1305 features an extended **192-bit (24-byte)** nonce:
- Each encrypted file receives a unique 24-byte nonce generated via `generate_nonce()` using system entropy (`OsRng`, `src/encryption.rs#L120-L125`).
- The 192-bit nonce size eliminates the birthday bound risk for random nonce generation ($2^{96}$ operations required for a 50% collision probability).
- The nonce is stored directly in the file's metadata header: `Inode::encryption_nonce` (`src/inode.rs`).

### Authenticated encryption with associated data (AEAD)

`encrypt_data` computes a 16-byte Poly1305 Message Authentication Code (MAC) tag appended to the ciphertext (`src/encryption.rs#L79`):
- Any tampering, byte corruption, bit flips, or truncation of the on-disk ciphertext immediately causes `decrypt_data` to fail authentication with `EncryptionError::DecryptionFailed`.
- Plaintext is only returned once the Poly1305 MAC tag is cryptographically verified against the key and nonce.

## Pipeline ordering: filter $\to$ compress $\to$ encrypt

<!-- openwiki: broken internal link [src/disk.rs#L953-L1040] file "src/disk.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
In [`DiskManager::write_data_from_start_internal`](src/disk.rs#L953-L1040), the write pipeline strictly enforces that **pre-compression filtering and compression execute before encryption**:

```
Plaintext ──> [Filter (Delta/Shuffle)] ──> [Zstd Compress] ──> [XChaCha20 Encrypt] ──> On-Disk Blocks
                                                                        ▲
                                                      Nonce (OsRng) ────┘
```

### Cryptographic rationale

1. **Shannon Entropy Flattening**: Secure ciphers like XChaCha20 output ciphertext indistinguishable from pure pseudorandom noise, with Shannon entropy approaching the theoretical maximum ($\approx 8.0$ bits per byte).
2. **Compressibility Destruction**: Lossless compressors (LZ77, Huffman, FSE) rely entirely on recurring byte patterns and non-uniform symbol distributions. If encryption occurred first, compression would achieve 0% reduction or expand in size.
3. **Security Invariance**: Compressing before encryption preserves maximum storage efficiency while the subsequent XChaCha20-Poly1305 pass ensures complete cryptographic confidentiality and integrity across the physical stored blocks.

## Deterministic SIV filename encryption

Plain directory entry storage exposes sensitive filename patterns, extensions, and directory topologies. To protect directory listings while still allowing $O(1)$ fast lookups without decrypting every entry in a directory block, OIFS implements Synthetic Initialization Vector (SIV) filename encryption (`src/encryption.rs#L142-L232`).

### SIV construction and directory tweak

<!-- openwiki: broken internal link [src/encryption.rs#L142-L178] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`encrypt_filename`](src/encryption.rs#L142-L178) deterministically generates a synthetic nonce using Blake2b-512 over a domain-separated context:

```
Synthetic Nonce = Blake2b-512("OIFS_SIV_FILENAME_V1" || key || parent_inode || filename)[0..12]
```

1. **Deterministic Lookups**: For a given `parent_inode` and `filename`, the synthetic nonce is strictly deterministic. Looking up `"secret.docx"` in parent inode 42 always hashes to the exact same encrypted string, allowing `DiskManager::lookup` to find directory records directly.
2. **Directory Tweak Isolation**: Including `parent_inode.to_le_bytes()` in the PRF input acts as a cryptographic tweak. If two different directories contain a file with the identical name `"data.csv"`, their resulting ciphertexts are completely distinct and uncorrelated.
3. **Payload Construction**: ChaCha20-Poly1305 encrypts `name.as_bytes()` using the 12-byte synthetic nonce. The 12-byte nonce and ciphertext/tag are combined and formatted as unpadded Base64URL (`base64ct::Base64UrlUnpadded`).
4. **Prefix and Special Entries**:
   - Encrypted filenames are prepended with `_e_` (`FILENAME_ENC_PREFIX = "_e_"`).
   - Standard directory navigation links `.` and `..` are explicitly preserved in plaintext (`src/encryption.rs#L147-L149`).
   - If an input string already starts with `_e_`, it is recognized as already encrypted and returned unmodified.

### Decryption and verification

<!-- openwiki: broken internal link [src/encryption.rs#L184-L232] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`decrypt_filename`](src/encryption.rs#L184-L232) decodes the Base64URL string, extracts the 12-byte synthetic nonce, verifies the Poly1305 authentication tag, and re-computes the Blake2b hash to ensure the synthetic nonce matches `(key, parent_inode, plaintext)`:
- If a filename does not start with `_e_` or fails authentication, it gracefully returns the original name (`src/encryption.rs#L189-L191`), maintaining backward compatibility with unencrypted images.

## Error handling

<!-- openwiki: broken internal link [src/encryption.rs#L236-L248] file "src/encryption.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
All cryptographic failures map to [`EncryptionError`](src/encryption.rs#L236-L248):

| Error Variant | Meaning | Trigger Condition |
| :--- | :--- | :--- |
| `KeyDerivationFailed` | PBKDF Failure | Argon2id salt encoding or hash generation failed |
| `InvalidKey` | Cipher Initialization Error | Key slice length or formatting invalid |
| `EncryptionFailed` | AEAD Failure | Cipher failed during block processing |
| `DecryptionFailed` | Auth / Secret Error | Wrong passphrase, corrupted blocks, or modified Poly1305 MAC tag |
