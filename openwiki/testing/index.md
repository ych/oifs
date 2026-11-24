# Files

- [Crash Safety Testing](crash_safety_testing.md) - Documents crash safety validation including power-loss simulation, WAL replay verification, fsck after unclean shutdown, durability mode guarantees, and test scenarios for concurrent operations during crash.
- [Integration Testing](integration_testing.md) - Documents end-to-end integration tests for OIFS: FUSE mount/unmount, NFS server, CLI workflows, FFI/C++ API, multi-process IPC, async/FFI, and cross-image operations.
- [Kani Formal Verification](kani_verification.md) - AWS Kani Rust Verifier is used to mathematically prove safety and correctness properties across the entire input space for core OIFS components, with 90+ formal proofs of bijectivity, arithmetic safety, layout validity, and filter roundtrip fidelity across 17 modules.
- [Thread Sanitizer (TSAN) Testing](tsan_testing.md) - Document TSAN-based concurrency testing: data race detection in sharded caches, IPC, session management, and async task pipeline.
