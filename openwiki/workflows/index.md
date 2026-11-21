# Files

- [Basic File/Directory Operations](basic_operations.md) - End-to-end flows for create_file, create_directory, lookup, open_file (OpenExisting/CreateNew/CreateOrOpen), read_data, read_at, read_at_batch, write_data (with filters/compression), write_data_with_filters, truncate, delete_file, list_dir, resolve_path, resolve_parent, and flush. Shows both Direct (DiskManager) and Remote (IPC) paths.
- [Analyze a host file (no image required)](compression_workflows.md)
- [Defragmentation Workflow](defragmentation_workflow.md) - Step-by-step guide to analyzing filesystem fragmentation, executing safe defragmentation, running in-place WAL-atomic defragmentation, and configuring the background auto-defrag coordinator while maintaining data integrity.
- [Encrypted Filesystem Workflows](encrypted_workflows.md) - Step-by-step guide for creating, opening, and using encrypted OIFS images including password management, key derivation, context-bound AEAD, filename encryption (SIV), IPC authentication, and the key hierarchy.
- [FFI Usage Workflow](ffi_workflow.md) - Step-by-step guide to using the C FFI interface for integrating OIFS with C/C++ applications, covering version handshake, lifecycle management, file/directory operations, error handling, encryption, I/O backend configuration, and session reuse.
- [Fsck Workflow](fsck_workflow.md) - Step-by-step guide to running OIFS fsck (CLI and API), interpreting the consistency report (orphan inodes, leaked blocks, missing blocks, cross-linked blocks), when manual fsck is needed versus journaled auto-recovery on mount, and the meaning of the is_clean flag.
- [MCP Workflow](mcp_workflow.md)
