---
type: task_routing_map
title: Quick Start
description: Refresh task-routing map to reflect updated pages and provide guided navigation through the OIFS documentation hierarchy.
tags: [quickstart, navigation, documentation, guide, tutorial]
sources:
  - id: openwiki-source-651d1fb6c9e49916a916ab51
    resource: repo://Cargo.toml
  - id: openwiki-source-23775c3de52f3ab95a13cb8b
    resource: repo://README.md
  - id: openwiki-source-c4c0d1a8305275c15968c047
    resource: repo://src/bin/oifs.rs
verified:
  - by: openwiki/0.6.1
    at: 2026-10-06T14:52:27.072Z
generated: { by: "openwiki/0.6.1", at: "2026-10-06T14:52:27.072Z" }
---

# Task Routing Map

This page provides a routing map to recently updated documentation and the overall OIFS documentation hierarchy.

## Recently Updated Pages

- [Async I/O and Engines](architecture/async_io_and_engines.md) — Update to address stale claim and reflect current I/O engine implementation.
- [Concurrency and Session Management](architecture/concurrency_and_session.md) — Update to address stale and unresolved claims regarding IPC, session registry, and block-level merge policy.
- [Disk Manager and Persistence](architecture/disk_manager_and_persistence.md) — Update to address stale and unresolved claims regarding block allocation, persistence, and metadata updates.
- [Crash Safety Testing](testing/crash_safety_testing.md) — Update to address unresolved claim regarding metadata mutation sync-on-write semantics.

## Documentation Hierarchy

- [Architecture](architecture/index.md) — Core subsystem designs and implementations.
- [Concepts](concepts/index.md) — Fundamental ideas and configuration.
- [Integrations](integrations/index.md) — External tool and service integrations.
- [Operations](operations/index.md) — CLI usage, testing, and verification.
- [Testing](testing/index.md) — Testing methodologies and verification.
- [Workflows](workflows/index.md) — Step-by-step guides for common tasks.
