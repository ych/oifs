---
type: architecture
title: Asynchronous I/O and Pluggable Engines
description: Pluggable payload read engine supporting Mmap, Pread, and io_uring with ExtentList coalescing, ring pooling, and kernel gating.
tags: [io_engine, async_io, io_uring, pread, extent_coalescing, kani, performance]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-03T11:29:24.571Z
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-ea9e30b0c99ad48bf309d4ab
    resource: repo://src/io_engine.rs
generated: { by: "antigravity", at: "2026-10-03T11:29:24.571Z" }
---

# Asynchronous I/O and Pluggable Engines

The OIFS file system separates metadata operations from payload data reading. While filesystem metadata (superblocks, bitmaps, inodes, indirect pointer blocks, and directory indices) is kept on the shared memory map (`MAP_SHARED`) for zero-copy, low-overhead access, file payloads can be large, cold, and scattered across physical blocks.

In P3.2, OIFS introduced a pluggable data-block read engine (`src/io_engine.rs`) that decouples physical payload I/O from memory-mapped page faults, enabling applications to select between `Mmap`, positional `Pread`, and high-concurrency Linux `io_uring` backends.

<!-- openwiki: mermaid parse failed and this diagram was converted to a text fence so it does not break rendering. Fix the diagram source and restore the mermaid fence. Parser error: Heuristic: an unescaped angle bracket inside a label breaks rendering; rephrase the label. -->
```text
flowchart TD
    UserReq["DiskManager::read_at / read_at_batch"] --> Guard["Acquire Read Lock (inner.read())"]
    Guard --> Prepare["read_at_prepare (Block Traversal)"]
    Prepare --> Extents["ExtentList (Contiguous Coalescing)"]
    Extents --> EngineSelect{"IoBackend Selection"}
    
    EngineSelect -->|"Mmap (Default)"| MmapPath["read_mmap: Direct slice copy from mmap"]
    EngineSelect -->|"Pread"| PreadPath["read_pread: Positional pread(2) per extent"]
    EngineSelect -->|"IoUring (Linux >= 5.15)"| RingCheckout["Checkout Ring from RingPool"]
    
    RingCheckout --> SubQueue["Populate SQEs (up to 64 in-flight)"]
    SubQueue --> KernelWait["submit_and_wait(1)"]
    KernelWait --> ReapCQE["Reap CQEs & Check In Ring"]
```

## 1. IoBackend Architecture and Selection

The `IoBackend` enum defines three distinct data-block retrieval strategies:

1. **`IoBackend::Mmap` (Default, value `0`)**:
   Payload bytes are copied directly from the shared memory-mapped region via `memcpy`. This path provides optimal latency when data is already hot in the OS page cache, but causes synchronous thread-blocking page faults when reading uncached cold blocks.
2. **`IoBackend::Pread` (Value `1`)**:
   Executes positional `pread(2)` system calls for each coalesced physical extent. This eliminates uncoordinated single-page faults by issuing bulk multi-block reads directly to the kernel, which can invoke readahead algorithms.
3. **`IoBackend::IoUring` (Value `2`)**:
   Submits all physical extents across a single read request or a batch (`read_at_batch`) asynchronously into a Linux `io_uring` ring, enabling high queue-depth hardware parallelization.

All backends read through the standard OS page cache without `O_DIRECT`, ensuring that recently written dirty pages (including uncommitted mutations under `DurabilityMode::Lazy`) are immediately visible across all backends.

Backend selection can be configured dynamically via:
- The environment variable `OIFS_IO_BACKEND=mmap|pread|io_uring`.
- Programmatic Rust APIs: `DiskManager::set_io_backend(backend)` or `DiskManager::with_io_backend(backend)`.
- Foreign Function Interface (FFI): `oifs_set_io_backend(handle, backend)` and `oifs_get_io_backend(handle)`.

```rust
// Querying and configuring the backend
let dm = DiskManager::open("data.oifs", 10 * 1024 * 1024)?;
let effective = dm.set_io_backend(IoBackend::IoUring);
assert_eq!(dm.io_backend(), effective);
```

## 2. ExtentList Block Coalescing

Non-contiguous files incur high system call or SQE submission overhead if dispatched one 4KB block at a time. OIFS solves this through the `ExtentList` abstraction.

When traversing indirect pointer blocks, contiguous physical disk runs and destination buffer offsets are merged sequentially inside `ExtentList::push`:

```rust
if let Some(last) = self.extents.last_mut()
    && last.disk_offset.checked_add(last.len as u64) == Some(disk_offset)
    && last.buf_offset.checked_add(last.len) == Some(buf_offset)
{
    last.len += len;
    return;
}
```

Files whose blocks were allocated contiguously by the allocator collapse into a single `ReadExtent`. A 100MB contiguous read is executed as a single `pread` or single `io_uring` SQE rather than 25,600 distinct operations.

This coalescing behavior is formally proven correct by Kani model checking (`proof_extent_push_preserves_coverage`), verifying that coverage is strictly preserved and no bytes are lost or fabricated.

## 3. Kernel Gating and Graceful Fallback

Because `io_uring` experienced kernel instability and security restrictions in earlier releases, OIFS implements strict kernel version gating:

- **Baseline Requirement**: Mainline Linux kernel version $\ge 5.15$.
- **Enterprise Linux Support**: Automatically detects RHEL 9.3+ backports (`5.14.0-362+` on `.el9`) by parsing the release string via `libc::uname` and caching the result with `OnceLock`.
- **Sysctl and Sandbox Protection**: Probes ring creation and checks for `IORING_OP_READ` support. If `io_uring` is disabled by sysctl (`kernel.io_uring_disabled`), seccomp rules, or missing kernel features, `IoBackend::resolve()` automatically falls back to `IoBackend::Pread`.

Applications calling `dm.set_io_backend(IoBackend::IoUring)` receive the effective backend (`IoBackend::Pread` if unsupported), guaranteeing that configuration never triggers runtime errors.

## 4. IoUring Ring Pooling and Memory Safety

To support concurrent readers under `DiskManager`'s reader-writer lock (`Arc<RwLock<DiskManagerInner>>`), the `io_uring` engine maintains an internal `RingPool`:

- Each ring is configured with a submission queue depth of 64 entries (`RING_ENTRIES = 64`).
- Extents exceeding `MAX_OP_BYTES` (1MB) are split across multiple SQEs, allowing the kernel to service chunks in parallel.
- Idle rings are cached up to `MAX_IDLE_RINGS` (16) to eliminate teardown and initialization overhead.
- **In-flight Lifetime Invariant**: The submission and reaping loop in `uring::run` guarantees that the function never returns while SQEs remain in flight. If `submit_and_wait` encounters an unrecoverable non-transient error, the process aborts to prevent releasing buffer pointers that the kernel may still be asynchronously writing to.

## 5. Batched Vector I/O (`read_at_batch`)

For workloads performing multiple random reads (such as multi-threaded index lookups or database record queries), `DiskManager::read_at_batch` executes multiple requests under a single lock acquisition:

```rust
let mut req1 = ReadRequest { inode_id: 1, offset: 0, buf: &mut buf1 };
let mut req2 = ReadRequest { inode_id: 2, offset: 4096, buf: &mut buf2 };
let mut requests = [req1, req2];

let results = dm.read_at_batch(&mut requests);
```

When operating with `IoBackend::IoUring`, all extents across all requests in the batch are aggregated into a single vector of `ReadTarget` structures and submitted simultaneously to the ring, maximizing hardware I/O queue depth.

## 6. Formal Verification

The I/O engine is formally verified using Kani proofs in `src/io_engine.rs`:
- `proof_extent_push_preserves_coverage`: Proves that `ExtentList::push` maintains exact byte counts and merges only when both disk offset and buffer offset are contiguous.
- `proof_io_backend_from_u8_soundness`: Proves that discriminant conversion via `from_u8` is total and safely maps all out-of-range `u8` values to `IoBackend::Mmap`.
