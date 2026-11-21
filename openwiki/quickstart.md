---
type: "Reference"
title: "Quickstart"
openwiki_generated: true
verified:
  - by: openwiki/0.9.0
    at: 2026-11-20T22:07:36.625Z
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-111508bcc7826871de86b6c4
    resource: repo://scripts/release_check.sh
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
  - id: openwiki-source-ef0afc6eaf5c925c9975314d
    resource: repo://src/ipc.rs
  - id: openwiki-source-ed8bf05e307c6278442542c2
    resource: repo://src/lib.rs
  - id: openwiki-source-c1e8d5f8bb6497980a6b4166
    resource: repo://src/session.rs
  - id: openwiki-source-ff3c4fb65b984fb93a3255ec
    resource: repo://src/superblock.rs
  - id: openwiki-source-4dd40261c9252cfa88de26ff
    resource: repo://tests/bolero_fuzz_test.rs
  - id: openwiki-source-5b30823597738f668b07d33c
    resource: repo://tests/shuttle_concurrency_test.rs
generated: { by: "openwiki/0.9.0", at: "2026-11-20T22:07:36.625Z" }
---


# Quickstart

This guide gets you from zero to a working OIFS image in seconds, then points you to the authoritative reference for every domain.

## Prerequisites

- Rust toolchain (Edition 2024): `rustup install stable`
- Linux for `io_uring` backend (optional feature)

## Build

```bash
# Default: builds CLI (oifs), C shared library (liboifs.so/.dylib), and MCP server (oifs_mcp)
cargo build --release

# Without MCP server (smaller binary, works on older rustc)
cargo build --release --no-default-features

# With all optional features (io_uring backend, blosc2 C-library wrappers)
cargo build --release --features io_uring,blosc2
```

**Artifacts produced:**
| Binary | Path | Purpose |
|---|---|---|
| `oifs` | `target/release/oifs` | Main CLI for all filesystem operations |
| `liboifs.so` / `liboifs.dylib` | `target/release/` | C FFI shared library (`cdylib`) |
| `oifs_mcp` | `target/release/oifs_mcp` | Model Context Protocol server (requires `mcp` feature) |

## Session Modes — Choose One

Every command runs in one of three **session modes**. The mode determines how processes coordinate access to the same `.img` file.

| Mode | Flag | Transport | When to Use |
|---|---|---|---|
| **Standalone** | (default, no flag) | Direct `mmap` — zero sockets, zero background threads | Single-process use, maximum throughput, no concurrency needed |
| **Local IPC** | `--ipc` | Unix Domain Socket in `/tmp/oifs_<hash>.sock` | Multi-process on the same machine (containers, CI, parallel jobs) |
| **Network** | `--network [ADDR]` or `--bind ADDR` | TCP + rendezvous file `.img.master` in image directory | NFS/Lustre/shared storage, multi-node clusters, remote development |

**Selection rules:**
- **Standalone**: Default. Use for scripts, tests, and single-writer workloads.
- **Local (`--ipc`)**: First process becomes **Master** (holds `fcntl` lock on image, runs IPC server); subsequent processes become **Proxies** and forward operations over UDS. Transparent failover: if Master exits, a Proxy promotes itself.
- **Network (`--network`)**: Uses a `.img.master` rendezvous file containing the Master's TCP address and an auth token. Processes probe with ping/pong before connecting. Bind to `0.0.0.0:PORT` for multi-node; add `--allow-remote-ipsc` only inside a tunnel (WireGuard/IPsec/SSH) — the IPC protocol is **not encrypted**.

```bash
# Standalone (default)
oifs -i disk.img ls

# Local multi-process
oifs -i disk.img --ipc put data.bin    # Process 1: becomes Master
oifs -i disk.img --ipc get data.bin    # Process 2: becomes Proxy

# Network mode (NFS/shared storage)
oifs -i /mnt/shared/disk.img --network 127.0.0.1:8989 ls   # Master
oifs -i /mnt/shared/disk.img --network 127.0.0.1:8989 put data.bin  # Proxy
```

<!-- openwiki: broken internal link [../architecture/concurrency_and_session.md] file "../architecture/concurrency_and_session.md" does not exist. Fix the href or restore the target, then delete this comment. -->
Details: [Concurrency & Session Model](../architecture/concurrency_and_session.md)

## Minimal Round-Trips

### 1. Plain Image (no encryption, no journal)

```bash
# Create 10 MiB image
oifs -i disk.img create --size 10

# Import a file
oifs -i disk.img put dataset.bin

# Export it back
oifs -i disk.img get dataset.bin extracted.bin

# Verify
diff dataset.bin extracted.bin
```

### 2. Encrypted Image

```bash
# Create with encryption (prompts twice for password confirmation)
oifs -i secret.img create --size 10 --encrypt

# Import (prompts for password, or use OIFS_PASSWORD env / --password flag)
oifs -i secret.img put dataset.bin

# Export
oifs -i secret.img get dataset.bin extracted.bin
```

**Password precedence** (highest → lowest):
1. `--password <pwd>` CLI flag
2. `OIFS_PASSWORD` environment variable
3. Interactive masked prompt (disabled in `--json` mode)

<!-- openwiki: broken internal link [../workflows/encrypted_workflows.md] file "../workflows/encrypted_workflows.md" does not exist. Fix the href or restore the target, then delete this comment. -->
Details: [Encrypted Filesystem Workflows](../workflows/encrypted_workflows.md)

### 3. Journaled Image (Crash-Safe Metadata)

```bash
# Create with WAL journaling (reserves 33 blocks for circular ring buffer)
oifs -i journal.img create --size 10 --journal

# Import / export work identically
oifs -i journal.img put dataset.bin
oifs -i journal.img get dataset.bin extracted.bin

# After power loss: mount replays WAL automatically — no fsck needed
```

<!-- openwiki: broken internal link [../architecture/journal.md] file "../architecture/journal.md" does not exist. Fix the href or restore the target, then delete this comment. -->
Details: [Metadata WAL / Journaling](../architecture/journal.md)

### 4. Encrypted + Journaled (Maximum Safety)

```bash
oifs -i safe.img create --size 10 --encrypt --journal
```

## Common Tasks — Command → Reference Map

| Task | Command | Reference |
|---|---|---|
<!-- openwiki: broken internal link [../operations/cli_reference.md#create] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Create image** | `oifs -i disk.img create --size 10 [--encrypt] [--journal]` | [CLI: create](../operations/cli_reference.md#create) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#put] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Import file** | `oifs -i disk.img put host.bin [dest.bin] [--compress/--no-compress/--chunked] [--filter auto\|delta\|shuffle\|both] [--typesize 4]` | [CLI: put](../operations/cli_reference.md#put) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#get] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Export file** | `oifs -i disk.img get name.bin [out.bin]` | [CLI: get](../operations/cli_reference.md#get) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#append] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Append text** | `oifs -i disk.img append log.txt "new line" [--no-newline]` | [CLI: append](../operations/cli_reference.md#append) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#mkdir] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Create directory** | `oifs -i disk.img mkdir dirname` | [CLI: mkdir](../operations/cli_reference.md#mkdir) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#ls] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **List files** | `oifs -i disk.img ls [path] [-r/--recursive]` | [CLI: ls](../operations/cli_reference.md#ls) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#rm] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Delete** | `oifs -i disk.img rm path [-r/--recursive]` | [CLI: rm](../operations/cli_reference.md#rm) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#analyze] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Analyze fragmentation** | `oifs -i disk.img analyze` | [CLI: analyze](../operations/cli_reference.md#analyze) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#defrag] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Defragment** | `oifs -i disk.img defrag [--mode safe\|inplace]` | [CLI: defrag](../operations/cli_reference.md#defrag) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#fsck] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Integrity check (fsck)** | `oifs -i disk.img fsck [--json]` | [CLI: fsck](../operations/cli_reference.md#fsck) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#migrate] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Migrate legacy v1→v2** | `oifs -i legacy.img migrate` | [CLI: migrate](../operations/cli_reference.md#migrate) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#truncate] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Truncate/extend file** | `oifs -i disk.img truncate path --size BYTES` | [CLI: truncate](../operations/cli_reference.md#truncate) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#filter-analyze] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Recommend compression filters** | `oifs filter-analyze dataset.bin` | [CLI: filter-analyze](../operations/cli_reference.md#filter-analyze) |
<!-- openwiki: broken internal link [../operations/cli_reference.md#global-options] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Enable auto-defrag** | `oifs -i disk.img --auto-defrag[=0.3] --defrag-interval 300 ls` | [CLI: global options](../operations/cli_reference.md#global-options) |
<!-- openwiki: broken internal link [../workflows/mcp_workflow.md] file "../workflows/mcp_workflow.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Run MCP server** | `oifs_mcp [--size MB] [--image PATH]` | [MCP Workflow](../workflows/mcp_workflow.md) |

**Global flags** (work with every subcommand):
```
-i, --image PATH        Target image file (required)
-p, --password STR      Passphrase for encrypted images
--ipc                   Local IPC mode (UDS)
-n, --network           Network TCP mode
--bind ADDR             Custom bind address (e.g. 0.0.0.0:9050)
--allow-remote-ipsc     Allow non-loopback bind (requires tunnel)
--json                  Machine-readable output, no interactive prompts
--auto-defrag [THRESH]  Background defrag coordinator (default threshold 0.25)
--defrag-interval SEC   Check interval (default 300s)
```

<!-- openwiki: broken internal link [../operations/cli_reference.md] file "../operations/cli_reference.md" does not exist. Fix the href or restore the target, then delete this comment. -->
Full reference: [CLI Reference](../operations/cli_reference.md)

## Running the Test Suite

OIFS uses a multi-tiered verification strategy. Run the layer appropriate for your change:

```bash
# 1. Unit + integration tests (all 16 test files, ~68 suites)
cargo test --release

# 2. With io_uring backend (Linux only)
cargo test --release --features io_uring

# 3. Property-based fuzzing (Bolero, 16 harnesses)
cargo test --test bolero_fuzz_test --release

# 4. Continuous fuzzing (libFuzzer/AFL/honggfuzz/Kani backends)
cargo bolero test --test bolero_fuzz_test blockpath_roundtrip
cargo bolero test --test bolero_fuzz_test --engine afl filter_pipeline_roundtrip

# 5. Concurrency model checking (Shuttle, DPOR)
cargo test --test shuttle_concurrency_test --release -- --nocapture

# 6. Formal verification (Kani, ~80 proofs)
cargo kani                    # all proofs
cargo kani -j 8 --output-format=terse   # parallel, terse
cargo kani --harness proof_chunk_entry_roundtrip_all  # single harness

# 7. Data-race detection (ThreadSanitizer)
cargo test --release --features tsan

# 8. Full pre-release gate (fmt, clippy, build, test, shuttle, kani, coverage)
./scripts/release_check.sh --quick
```

| Layer | Command | What It Catches |
|---|---|---|
| Empirical | `cargo test` | Logic bugs, API regressions, end-to-end I/O |
| Bolero | `cargo test --test bolero_fuzz_test` | Edge-case panics, roundtrip invariants, parser robustness |
| Shuttle | `cargo test --test shuttle_concurrency_test` | Heisenbugs, data races, deadlocks under adversarial scheduling |
| Kani | `cargo kani` | **All** symbolic inputs: arithmetic safety, layout invariants, bijections, WAL idempotence |
| TSAN | `cargo test --features tsan` | Actual data races in compiled binary |

<!-- openwiki: broken internal link [../operations/testing_and_verification.md] file "../operations/testing_and_verification.md" does not exist. Fix the href or restore the target, then delete this comment. -->
Details: [Testing & Verification](../operations/testing_and_verification.md)

## Architecture Map — Where to Go Next

| Domain | Page | Covers |
|---|---|---|
<!-- openwiki: broken internal link [../architecture/overview.md] file "../architecture/overview.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **System Overview** | [Architecture Overview](../architecture/overview.md) | Crate structure, public API surface, runtime domains |
<!-- openwiki: broken internal link [../architecture/concurrency_and_session.md] file "../architecture/concurrency_and_session.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Session Modes & IPC** | [Concurrency & Session Model](../architecture/concurrency_and_session.md) | Master/Proxy election, failover, UDS/TCP, session registry |
<!-- openwiki: broken internal link [../architecture/superblock_and_layout.md] file "../architecture/superblock_and_layout.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **On-Disk Layout** | [Superblock & Layout](../architecture/superblock_and_layout.md) | Block 0 superblock, v3 journaled layout, bitmap/inode/data regions |
<!-- openwiki: broken internal link [../architecture/block_allocation.md] file "../architecture/block_allocation.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Block/Inode Allocation** | [Block & Inode Allocation](../architecture/block_allocation.md) | Two-bitmap allocator, search hints, AllocSim for journaled ops |
<!-- openwiki: broken internal link [../architecture/inode_and_directory.md] file "../architecture/inode_and_directory.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Inodes & Directories** | [Inodes & Directories](../architecture/inode_and_directory.md) | 256-byte inode v2, triple-indirect, sharded dir cache, SIV filenames |
<!-- openwiki: broken internal link [../architecture/journal.md] file "../architecture/journal.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Journaling (WAL)** | [Metadata WAL / Journaling](../architecture/journal.md) | Ring buffer, frame format, coalescing, idempotent replay, checkpointing |
<!-- openwiki: broken internal link [../architecture/encryption.md] file "../architecture/encryption.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Encryption** | [Encryption & Key Hierarchy](../architecture/encryption.md) | Argon2id, BLAKE2b subkeys, XChaCha20-Poly1305, context-bound AAD |
<!-- openwiki: broken internal link [../architecture/compression_and_filters.md] file "../architecture/compression_and_filters.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Compression & Filters** | [Compression & Filters](../architecture/compression_and_filters.md) | Stream vs Seekable 64K, Delta/Shuffle/BitShuffle, DecodedBlobCache |
<!-- openwiki: broken internal link [../architecture/async_io_and_engines.md] file "../architecture/async_io_and_engines.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **I/O Engines** | [Async I/O & Read Engines](../architecture/async_io_and_engines.md) | Mmap, Pread, IoUring backends, extent coalescing, `OIFS_IO_BACKEND` |
<!-- openwiki: broken internal link [../architecture/defragmentation.md] file "../architecture/defragmentation.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Defragmentation** | [Defragmentation](../architecture/defragmentation.md) | COW passes, auto-defrag coordinator, journaled metadata deferral |
<!-- openwiki: broken internal link [../architecture/integrity_checking_fsck.md] file "../architecture/integrity_checking_fsck.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Fsck** | [Integrity Checking](../architecture/integrity_checking_fsck.md) | Orphan/leaked/missing/cross-linked detection, journal reduces need |
<!-- openwiki: broken internal link [../architecture/ffi_interface.md] file "../architecture/ffi_interface.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **FFI / C API** | [FFI / C Interface](../architecture/ffi_interface.md) | Opaque handle, create/open/close, CRUD, async callbacks, version handshake |
<!-- openwiki: broken internal link [../architecture/mcp_server.md] file "../architecture/mcp_server.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **MCP Server** | [MCP Server](../architecture/mcp_server.md) | Tool router, JSONL output, stdio transport, encrypted sandbox |
<!-- openwiki: broken internal link [../concepts/configuration.md] file "../concepts/configuration.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| **Configuration Knobs** | [Configuration](../concepts/configuration.md) | Cargo features, env vars, DurabilityMode, CompressionMode, OpenMode |

## Workflow Deep-Dives

| Workflow | Page |
|---|---|
<!-- openwiki: broken internal link [../workflows/basic_operations.md] file "../workflows/basic_operations.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| Basic file/directory operations (create, lookup, read, write, truncate, delete, list) | [Basic Operations](../workflows/basic_operations.md) |
<!-- openwiki: broken internal link [../workflows/compression_workflows.md] file "../workflows/compression_workflows.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| Stream vs Seekable 64K compression, filter recommendation, cache interaction | [Compression Workflows](../workflows/compression_workflows.md) |
<!-- openwiki: broken internal link [../workflows/defragmentation_workflow.md] file "../workflows/defragmentation_workflow.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| Online defrag, auto-defrag, fragmentation metrics, crash recovery | [Defragmentation Workflow](../workflows/defragmentation_workflow.md) |
<!-- openwiki: broken internal link [../workflows/encrypted_workflows.md] file "../workflows/encrypted_workflows.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| Encrypted image creation, password/key handling, IPC auth, key hierarchy | [Encrypted Workflows](../workflows/encrypted_workflows.md) |
<!-- openwiki: broken internal link [../workflows/ffi_workflow.md] file "../workflows/ffi_workflow.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| C API from Python/Node/Go/Rust, handle lifecycle, async callbacks | [FFI Workflow](../workflows/ffi_workflow.md) |
<!-- openwiki: broken internal link [../workflows/fsck_workflow.md] file "../workflows/fsck_workflow.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| Fsck invocation, report interpretation, when it's needed | [Fsck Workflow](../workflows/fsck_workflow.md) |
<!-- openwiki: broken internal link [../workflows/mcp_workflow.md] file "../workflows/mcp_workflow.md" does not exist. Fix the href or restore the target, then delete this comment. -->
| MCP server as agent sandbox, tool schemas, encrypted sandbox | [MCP Workflow](../workflows/mcp_workflow.md) |

---

<!-- openwiki: broken internal link [../workflows/basic_operations.md] file "../workflows/basic_operations.md" does not exist. Fix the href or restore the target, then delete this comment. -->
**Next:** Pick a workflow above, or start with [Basic Operations](../workflows/basic_operations.md) for the canonical end-to-end flow.
