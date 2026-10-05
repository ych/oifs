---
type: architecture
title: Concurrency, Sessions, and IPC
description: How OIFS allows multiple processes to share one image through a Master-Proxy design — a single Master holds the mmap and a DiskManager mutex while Proxies forward framed requests over UDS or TCP — coordinated by the OifsSession registry with self-healing master-failover.
tags: [ipc, master-proxy, session, concurrency, uds, tcp, failover]
sources:
  - id: openwiki-source-f9183fa58bb2f10bacc5bd4c
    resource: repo://src/disk.rs
  - id: openwiki-source-ef0afc6eaf5c925c9975314d
    resource: repo://src/ipc.rs
  - id: openwiki-source-c1e8d5f8bb6497980a6b4166
    resource: repo://src/session.rs
generated: { by: "antigravity", at: "2026-10-03T11:29:24.571Z" }
---

## Responsibility and ownership

The session layer owns the process coordination contract while the IPC layer
<!-- openwiki: broken internal link [`src/session.rs#L59-L78`] file "`src/session.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
owns the transport. [`OifsSession`](`src/session.rs#L59-L78`) decides whether a
given process runs in **Direct (Master)** or **Remote (Proxy)** mode and forwards
<!-- openwiki: broken internal link [`src/ipc.rs`] file "`src/ipc.rs`" does not exist. Fix the href or restore the target, then delete this comment. -->
every operation to the right place. [`ipc.rs`](`src/ipc.rs`) provides the
`bind_or_connect` master election, the `IpcServer` request dispatcher, and the
length-prefixed framing used on every hop. The actual mutation of the image
happens only inside a single Master's [`DiskManager`] (`Arc<Mutex<DiskManagerInner>>`),
which is the real serialization point.

## Master-Proxy architecture

Only one process may hold the `mmap` and mutate the bitmap/inode tables. On open,
one process wins master status and runs in `Direct` mode: it opens the
`DiskManager` directly (zero IPC overhead for its own calls) and starts a
background `IpcServer`. Every other process wins nothing and runs in `Remote`
mode: each public method forwards the request through `send_request` to the
Master.

Master election is transport-dependent and implemented in `bind_or_connect`
(`src/ipc.rs#L332-L477`):

- **Local (UDS)** — `get_socket_path` (`src/ipc.rs#L114`) derives a deterministic
  `/tmp/oifs_<name>_<hash>.sock` from the canonical image path. A process first
  tries to connect; if the socket is unreachable it atomically `bind`s as master,
  retrying and cleaning up stale sockets.
- **Network (TCP)** — a `<image>.master` rendezvous file in the image directory
  holds a `MasterInfo { addr, pid }` (`src/ipc.rs#L45`). The probe
  `is_tcp_master_alive` (`src/ipc.rs#L150`) runs a clock-free active Ping/Pong to
  detect a dead master on NFS/Lustre. The winner atomically
  `create_new`s the rendezvous file and binds TCP; losers connect.

## Process-level session registry

<!-- openwiki: broken internal link [`src/session.rs#L77`] file "`src/session.rs" does not exist. Fix the href or restore the target, then delete this comment. -->
[`SESSION_REGISTRY`](`src/session.rs#L77`) is a process-wide
`Mutex<HashMap<PathBuf, OifsSession>>` keyed by the canonicalized absolute path.
`get_or_open_with_mode` (`src/session.rs#L195`) returns a `Clone` of the existing
session if one is cached, otherwise opens one and inserts it. Because
`OifsSession` is `Clone` built on `Arc` fields, multiple threads share one Master
with no duplication and natural reference counting. `canonicalize_path`
(`src/session.rs#L85`) resolves symlinks (up to 32 hops) so links pointing at the
same image deduplicate to the same registry key.

## Transport: framed requests

Every RPC is a length-prefixed Bincode frame (`src/ipc.rs#L281`) with a 128 MB
safety cap. `IpcRequest` / `IpcResponseData` (`src/ipc.rs`) enumerate all
supported operations (create/read/write/delete/resolve/list/fsck/defrag/flush).
`IpcServer::handle_request` (`src/ipc.rs#L648`) is the single dispatcher that
maps each request to a `DiskManager` call, so logic is never duplicated across
the Direct and Remote arms.

The server thread (`src/ipc.rs#L479`) keeps the listener non-blocking and uses
`libc::poll` on the listener fd; each accepted peer gets its own worker thread
that loops reading requests and emitting `SessionEvent::PeerConnected`,
`RequestHandled`, and `PeerDisconnected`. Drop performs a graceful shutdown and
removes the socket or rendezvous file.

## Self-healing master failover

A Remote session must survive the Master dying (crash, power loss). In
`send_request` (`src/session.rs#L424-L490`) a Remote call first checks a cached
`dm_fallback` local DiskManager, then retries up to five times. On a connection
error (broken pipe, reset, etc. per `is_connection_error`, `src/session.rs#L490`),
it re-runs master election; if it turns out the retrying process is now the
Master, it pins the local `DiskManager` as `dm_fallback` and serves the request
locally. This makes a Proxy transparently promote itself after a master loss.

## Block-level merge policy

The merge policy described in the project docs is enforced structurally rather
than by special code: all peers funnel into the one Master's
`Mutex<DiskManagerInner>`. Under that mutex:

- **Disjoint byte offsets in the same block** merge in place — each write
  `copy_from_slice`s only its own byte range, leaving untouched bytes intact.
- **Overlapping offsets** are Last-Writer-Wins because the mutex serializes
  writes into POSIX `pwrite` semantics with no torn writes.
- **Compressed files** use zstd multi-frame append at EOF, or transparent
  read-modify-recompress for random offsets, as documented in the compression
  page.

There is no cross-process locking of individual blocks; mutual exclusion is the
coarse-grained Master mutex, which is what the Shuttle concurrency proofs
rely on.

## Extension seams

The `OifsSession` enum's two variants and its method-by-method dispatch are the
seam for adding new operations: a new `IpcRequest` arm plus matching Direct and
Remote branches, and a new `DiskManager` method. Transport is swappable through
the `IpcStream`/`IpcListener` abstractions in `bind_or_connect`, and failover
behavior lives entirely in `send_request`, so a new transport only needs to
produce a connectable stream.
