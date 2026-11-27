# Security Policy & Threat Model

## Supported Versions

| Version | Supported | Security Hardening |
| :--- | :---: | :--- |
| `1.7.x` (`master`) | ✅ Yes | XAES-256-GCM + AES-256-GCM-SIV (`v3`), SuperBlock `header_mac`, bounded allocations, 135 Kani proofs |
| `1.6.x` | ⚠️ Maintenance | Legacy `v2` XChaCha20-Poly1305 & dynamic Auto-Grow/Vacuum support |

## Security Architecture & Guarantees

1. **Authenticated Cryptography & Anti-Downgrade (`src/encryption.rs`)**:
   - Key derivation uses **OWASP / RFC 9106 Argon2id** (`64 MiB` memory, `t=3` iterations, `p=4` lanes) with **BLAKE2b-512** domain-separated subkeys.
   - Format **v3 encryption** (default) uses single-pass hardware-accelerated **C2SP XAES-256-GCM** (24-byte nonces, `inode_id` / `chunk_idx` / `flags` bound in AAD) and **RFC 8452 AES-256-GCM-SIV** deterministic filenames (`parent_inode` bound in nonce and AAD). Legacy `v2` XChaCha20-Poly1305 remains supported; legacy `v1` mounts read-only automatically to prevent write-side AAD downgrade.
   - Encrypted containers sign the SuperBlock via keyed BLAKE2b-256 `header_mac` verified at mount time to detect cipher/salt/format tampering.
   - Freshly allocated data blocks and extension slack are zero-initialized before handoff, and Unix containers are created with owner-only `0600` permissions (`O_NOFOLLOW`).

2. **Resource-Exhaustion & Anti-Token-Drain Defenses**:
   - `SuperBlock::validate` enforces format/encryption allow-lists and table/region bounds; allocators bound pointers to valid on-disk ranges.
   - Stream decompression bounds (`MAX_READ_DATA_BYTES = 2 GiB`, `MAX_DECOMPRESSED_STREAM_BYTES = 256 MiB`, fallible `try_reserve_exact`) block forged-size OOM and Zstd bombs.
   - The **`oifs_mcp` stdio server** executes 100% locally with **zero outbound network requests and zero telemetry**, enforces anti-token-drain defaults (`256 KiB` / max `4 MiB` per `read_file`; `500` / max `5,000` entries per `list_dir`), and supports OS-level `fcntl(F_RDLCK)` + `PROT_READ` immutable `--read-only` mounts.

3. **Multi-Process IPC Hardening (`src/ipc.rs`)**:
   - IPC authentication uses keyed BLAKE2b challenge-response (passphrases never cross the wire).
   - Unix domain sockets and rendezvous files use `0700`/`0600` permissions, concurrent peer slots are bounded, and TCP IPC binds `127.0.0.1` loopback by default unless `--allow-remote-ipc` is explicitly supplied.

4. **Formal Verification & Testing**:
   - Verified continuously with **135 AWS Kani (CBMC) formal proofs**, Bolero fuzzing targets, Shuttle DPOR concurrency scheduling tests, and 80+ integration test suites.

## Reporting a Vulnerability

Please report suspected security issues privately rather than opening a public issue:

- **Email**: `ych@ychuang.org`
- **GitHub**: Open a private Security Advisory on [`ych/oifs`](https://github.com/ych/oifs/security/advisories)

Please include reproduction steps, affected version/commit, and impact assessment. Confirmed vulnerabilities are prioritized for patch release and Kani regression proof coverage.
