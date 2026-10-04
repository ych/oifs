# Files

- [Crash Safety Testing](crash_safety_testing.md) - Guide to crash safety test scenarios that verify durability against power loss and system crashes in the OIFS file system.
- [Integration Testing Guide](integration_testing.md) - A guide to the end-to-end integration tests for the OIFS file system, covering disk persistence, crash safety, and network synchronization.
- [Kani Formal Verification](kani_verification.md) - AWS Kani Rust Verifier is used to mathematically prove safety and correctness properties across the entire input space for core OIFS components, covering 49 formal proofs of bijectivity, arithmetic safety, layout validity, and filter roundtrip fidelity.
- [ThreadSanitizer Testing](tsan_testing.md) - Details on ThreadSanitizer integration for detecting data races and ensuring memory safety under concurrency.
