---
type: concept
title: ThreadSanitizer Testing
description: Details on ThreadSanitizer integration for detecting data races and ensuring memory safety under concurrency.
tags: [testing, concurrency, ThreadSanitizer, memory safety]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-04T10:12:53.730Z
sources:
  - id: openwiki-source-120f0cc600d844dc98f88d46
    resource: repo://tests/concurrency_stress_test.rs
  - id: openwiki-source-26a4a5705f3958b08e71a42f
    resource: repo://tests/concurrency_test.rs
  - id: openwiki-source-89dfa6f9ae01afe2c6645cb0
    resource: repo://tests/rwlock_concurrency_test.rs
  - id: openwiki-source-4dd96a71b25353481dd09366
    resource: repo://tests/session_ipc_test.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-04T10:12:53.730Z" }
---

ThreadSanitizer (TSan) is a dynamic data race detector integrated into the OIFS testing suite to identify memory safety issues arising from concurrent access. The project includes specialized stress tests designed to expose data races, atomicity violations, and other concurrency-related bugs when executed under TSan's instrumentation.

## TSan Integration Approach

OIFS employs ThreadSanitizer through its continuous integration pipeline and dedicated stress test binaries. Rather than modifying standard unit tests, the project maintains explicit TSan-focused tests that maximize thread interleaving and contention to increase the likelihood of race detection. These tests:

- Exercise shared DiskManager instances across multiple threads
- Combine read/write operations with concurrent integrity checks (fsck)
- Test indirect block allocation under contention
- Validate session-level IPC concurrency in multi-process scenarios

## Key TSaN Test Files

### concurrency_stress_test.rs
The primary ThreadSanitizer validation suite containing three high-stress scenarios:

1. **Multithreaded Read/Write Integrity** (`test_stress_multithread_read_write_integrity`)
   - 6 concurrent threads performing 20 iterations each of 2KB writes
   - Periodic flushing and self-verification during write operations
   - Final structural integrity check via `verify_integrity()`

2. **Concurrent Mutations with Live Fsck** (`test_stress_concurrent_mutations_with_live_fsck`)
   - Background thread continuously running fsck while worker threads create/delete files
   - Validates filesystem structural integrity during active mutations
   - Ensures no false positives from TSan due to legitimate concurrent operations

3. **Indirect Block Expansion Under Contention** (`test_stress_large_file_indirect_block_expansion`)
   - 4 threads expanding files across direct/indirect block boundary (40KB → 80KB)
   - Tests indirect table allocation safety, pointer initialization, and block indexing
   - Targets a known complex code path prone to race conditions

### Supporting Concurrency Tests

While not exclusively designed for TSan, these tests additionally run under ThreadSanitizer in CI to broaden coverage:

- `concurrency_test.rs`: Validates intra-process threading (shared DiskManager via Arc) and inter-process locking
- `rwlock_concurrency_test.rs`: Tests read-write lock scaling with mixed reader/writer workloads and concurrent flush operations
- `session_ipc_test.rs`: Verifies multi-process transparent proxy behavior and concurrent writes from multiple clients

## Running TSan Locally

ThreadSanitizer tests can be executed locally using the standard cargo test command with the `tsan` feature:

```bash
cargo test --features tsan --test concurrency_stress
```

This builds the test binary with ThreadSanitizer instrumentation enabled (via `-Zsanitizer=thread`) and executes all stress test scenarios. The CI pipeline runs equivalent configurations on every pull request to prevent regressions.

## TSan-Specific Considerations

When analyzing TSan output in OIFS tests:

- **Expected Synchronization**: All shared state access in tests is protected by DiskManager's internal locking mechanisms (mutexes/rwlocks). TSan reports indicate potential missing synchronization in the implementation, not test design flaws.
- **Flush Operation Coordination**: Tests deliberately intermix synchronous (`flush()`) and asynchronous (`flush_async()`) flush operations with concurrent readers to validate correct locking hierarchy.
- **Atomic Operations**: Tests use `AtomicBool` and `AtomicUsize` with `Ordering::Relaxed` for coordination flags and counters—these are intentionally race-free by design and should not trigger TSan warnings.

## Relationship to Other Verification Methods

ThreadSanitizer complements OIFS' formal verification approach:

<!-- openwiki: broken internal link [../kani_verification.md] file "../kani_verification.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- Unlike [Kani proofs](../kani_verification.md) which verify absence of specific properties under all thread interleavings, TSan provides empirical evidence of race freedom observed during test execution.
<!-- openwiki: broken internal link [../shuttle_concurrency.md] file "../shuttle_concurrency.md" does not exist. Fix the href or restore the target, then delete this comment. -->
- While [Shuttle model checking](../shuttle_concurrency.md) systematically explores thread schedules, TSan detects races that occur in the specific schedules exercised by its stress tests.
- TSan is particularly effective at catching complex interaction bugs involving multiple synchronization primitives that may be difficult to model in verification tools.

## Maintenance Guidelines

When adding new concurrent functionality:

1. Consider whether the change warrants a dedicated TSan stress test (especially for core DiskManager or IPC components)
2. Ensure new tests follow the pattern of maximizing thread contention while maintaining deterministic verification
3. Avoid using `std::thread::yield_now()` or sleep-based synchronization in tests, as these can mask race conditions
4. Validate that any new synchronization primitives correctly express happens-before relationships to both the implementation and TSan
