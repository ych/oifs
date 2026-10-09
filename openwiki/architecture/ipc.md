---
type: architecture
title: Inter-Process Communication (IPC)
description: The IPC subsystem enables transparent master-proxy coordination with dual transport backends (Unix Domain Sockets and Network TCP) and provides master election, framed RPC, and a request dispatcher for OIFS file system operations.
tags: [ipc, master-proxy, uds, tcp, framing, rendezvous, session]
verified:
  - by: openwiki/0.6.1
    at: 2026-10-09T15:04:29.410Z
sources:
  - id: openwiki-source-ef0afc6eaf5c925c9975314d
    resource: repo://src/ipc.rs
generated: { by: "openwiki/0.6.1", at: "2026-10-09T15:04:29.410Z" }
---

## Responsibility and ownership

The IPC layer owns the transport mechanism for master-proxy communication. It provides:
- Dual-mode transport: Local (Unix Domain Socket in `/tmp`) and Network (TCP with rendezvous file)
- Master election protocol (`bind_or_connect`) that deterministically chooses a master process per image
- Length-prefixed Bincode framing for RPC safety
- Request/response type definitions covering all DiskManager operations
- A background `IpcServer` on the master that dispatches requests to the `DiskManager`
- An `IpcClient` used by proxies to forward requests to the master

<!-- openwiki: broken internal link [../concurrency_and_session.md] file "../concurrency_and_session.md" does not exist. Fix the href or restore the target, then delete this comment. -->
The actual coordination logic (deciding direct vs. remote mode, session registry, failover) resides in the session layer ([Concurrency, Sessions, and IPConcurrency, Sessions, and IPC](../concurrency_and_session.md)), while IPC focuses strictly on reliable message exchange.

## Transport modes

### Local mode (Unix Domain Socket)
- Default mode for single-machine use
- Socket path derived from image path via `get_socket_path`: deterministic hash in `/tmp/oifs_<basename>_<hash>.sock`
- Master election: processes first attempt to connect; if socket absent or unresponsive, they bind and become master; stale sockets are cleaned up after failed connection attempts

### Network mode (TCP with rendezvous file)
- Enables multi-node clusters over NFS/Lustre
- Uses a `.image.master` rendezvous file in the image directory containing `MasterInfo { addr, pid }`
- Master election: processes check for existing rendezvous file; if present and master alive (via clock-free active ping probe), they connect; otherwise they atomically `create_new` the file to become master and bind TCP
- Active ping probe: lightweight TCP connect + Ping/Pong exchange to detect dead masters without relying on clock synchronization

## Framing protocol

Every RPC exchange uses a length-prefixed frame:
1. 4-byte little-endian length (uint32)
2. Bincode-serialized payload (max 128 MiB)
3. `write_framed` serializes, prefixes length, writes; `read_framed` reads length, validates size, deserializes payload

This framing ensures:
- Bounded memory usage (size limit prevents OOM)
- Resilience to partial reads/writes (atomic frame exchange)
- Compatibility across Unix and TCP streams via the `IpcStream` enum

## Request and response types

### IpcRequest (client → master)
Enumerates all file system operations proxyable over IPC:
- Basic node operations: `CreateFile`, `CreateDirectory`, `Lookup`, `ReadInode`, `DeleteFile`, `Truncate`
- Path resolution: `ResolvePath`, `ResolveParent`
- Directory listing: `ListDir`
- Data access: `ReadData`, `ReadAt`, `WriteData` (with compression/filter configuration)
- Metadata: `GetSuperblock`, `Flush`
- Maintenance: `AnalyzeFragmentation`, `Defragment`, `VerifyIntegrity`, `Migrate`, `GetBlockCopy`
- Liveness: `Ping`

Each request variant has a `name()` method returning a static string for lightweight logging.

### IpcResponse (master → client)
- `Success(IpcResponseData)`: typed payload per request type (e.g., `InodeId`, `Data(Vec<u8>)`, `Superblock`, `DirectoryEntries`, etc.)
- `Error(String)`: error message propagated as `io::Error` on client side

### IpcResponseData
Rich enum matching each successful request:
- `Pong`, `InodeId(u64)`, `Unit` (for void operations)
- `Data(Vec<u8>)` for read operations
- Structured responses: `ResolveParent`, `Inode`, `DirectoryEntries`, `Superblock`, `FragmentationStats`, `DefragStats`, `FsckReport`, `MigrationStats`, `BlockCopy`

## IpcServer (master background dispatcher)

Runs on the master process after winning election:
- Accepts connections on the transport listener (UDS or TCP)
- Uses non-blocking polling (`libc::poll`) on the listener fd with 50ms timeout
- For each accepted connection:
  1. Assigns monotonically increasing peer ID
  2. Spawns a worker thread to handle that peer
  3. Notifies session layer via `SessionEvent::PeerConnected` (if event channel provided)
- Worker thread loop:
  - Reads framed `IpcRequest` from peer
  - Dispatches to `handle_request` which calls appropriate `DiskManager` method
  - Writes framed `IpcResponse` back to peer
  - On success, emits `SessionEvent::RequestHandled`
  - On read/write error or clean disconnect, emits `SessionEvent::PeerDisconnected`
- Tracks active peers and total served via atomic counters
- Graceful shutdown on drop: waits for active peers (≤1.5s), signals shutdown, removes socket/rendezvous file

## IpcClient (proxy handle)

Used by remote processes to communicate with the master:
- Wraps a mutexed `IpcStream` (Unix or TCP) and target path
- `send` method:
  - Locks stream (deliberately does not use `lock_unpoisoned` to avoid desynchronization on panic)
  - Writes framed request, reads framed response
  - Maps `IpcResponse::Error` to `io::Error::other`
- Provides `target_path` access for diagnostics
- Designed for single-threaded use per session; multiple threads sharing a client must externalize synchronization

## Lifecycle and cleanup

- Master election occurs during `OifsSession` initialization via `bind_or_connect`
- Master process retains `IpcListener` and launches `IpcServer` in background
- Each proxy process holds an `IpcClient` connected to the master
- On master termination (crash or normal exit):
  - `IpcServer::Drop` removes the transport endpoint (socket or rendezvous file)
  - Proxies detect failure on next `send` and trigger master re-election via session layer failover
- On normal proxy exit, client connection is closed; master detects EOF and cleans up peer worker

## Integration with session layer

<!-- openwiki: broken internal link [../concurrency_and_session.md] file "../concurrency_and_session.md" does not exist. Fix the href or restore the target, then delete this comment. -->
As detailed in [Concurrency, Sessions, and IPC](../concurrency_and_session.md):
- `OifsSession` uses IPC transport but owns the master/proxy state machine
- Session registry deduplicates by canonical image path
- Failover logic: proxies retry master election on connection errors and may promote themselves to master
- Event channel (`Sender<SessionEvent>`) flows from IPC server to session layer for monitoring

## Error handling and resilience

- Master election tolerates stale endpoints (removes and retries)
- Connection retries with exponential backoff during election
- Active ping probe in network mode detects dead masters without false positives from clock skew
- Framing size limit (128 MiB) prevents resource exhaustion
- Worker threads detach cleanly on peer disconnect or master shutdown
- Mutex poisoning is accepted: a panic during send leaves stream locked, but subsequent panic is preferred to silent corruption
