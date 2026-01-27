---
description: "Rule for running and verifying cargo tests"
---

When running `cargo test`, you MUST:
1.  Check the **Exit Code**. If it is not 0, the test failed.
2.  Scan the output for "error[" strings. If these exist, compilation failed.
3.  Scan the output for "FAILED" or "test result: FAILED".
4.  If multiple failures occur, prioritize fixing Compilation Errors (`error[...]`) first, as they block tests from running.
5.  Check `tests/` directory for any test files that might have become stale due to API changes (e.g. signature updates) and update them.
