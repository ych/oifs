# Files

- [Basic File/Directory Operations](basic_operations.md)
- [Compression Workflows](compression_workflows.md) - Guide for compression usage: filter analysis, mode selection (auto/always/never/stream/seekable), numerical data optimization, and performance tuning.
- [Defragmentation Workflow](defragmentation_workflow.md) - Step-by-step guide to analyzing filesystem fragmentation, executing safe defragmentation, running in-place WAL-atomic defragmentation, and configuring the background auto-defrag coordinator while maintaining data integrity.
- [Encrypted Filesystem Workflows](encrypted_workflows.md) - Step-by-step guide for creating, opening, and using encrypted OIFS images including password management, key derivation, context-bound AEAD, filename encryption (SIV), IPC authentication, and the key hierarchy.
- [FFI Usage Workflow](ffi_workflow.md) - Step-by-step guide to using the C FFI interface for integrating OIFS with C/C++ applications, covering version handshake, lifecycle management, file/directory operations, error handling, encryption, I/O backend configuration, session reuse, async I/O, zero-copy reads, cross-image transfers, SIMD filters, diagnostics, and FUSE frame dispatch.
- [Fsck Workflow](fsck_workflow.md) - Step-by-step guide to running OIFS fsck (CLI and API), interpreting the consistency report (orphan inodes, leaked blocks, missing blocks, cross-linked blocks), when manual fsck is needed versus journaled auto-recovery on mount, and the meaning of the is_clean flag.
- [MCP Workflow](mcp_workflow.md)
