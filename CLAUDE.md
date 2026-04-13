# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Status

**Pre-implementation.** The repository currently contains only design documents — no source code, build system, or tests exist yet. All work so far is in `docs/`.

## What This Project Is

UniBus Toy is a pure-software implementation of a unified-addressing interconnect protocol for Scale-Up domains, inspired by Huawei's Lingqu (UnifiedBus / UB). It provides global unified addressing + memory/message semantics + peer-to-peer access across node processes, implemented entirely in userspace.

## Language & Runtime

- **Rust** with **tokio** multi-threaded async runtime
- Config format: **YAML** (serde + serde_yaml)
- Wire encoding: **hand-written binary** using `byteorder` crate — NOT protobuf or serde serialization
- Async traits via `async-trait` crate
- Buffer management via `bytes` crate (Bytes/BytesMut)
- Error types via `thiserror`
- Sync primitives: `parking_lot` (Mutex/RwLock), `tokio::sync` for async
- Lock-free queues: `crossbeam::queue::ArrayQueue` (for JFS MPSC)
- Metrics: `metrics` + `metrics-exporter-prometheus`
- HTTP admin: `axum` (serves both `/metrics` and `/admin/*`)
- Benchmarking: `criterion`

## Build & Test Commands (once code exists)

```bash
cargo build                  # build all crates
cargo test                   # run all tests
cargo test -p ub-core        # run tests for a single crate
cargo test test_name         # run a single test
cargo bench                  # run criterion benchmarks
cargo fmt --check            # check formatting
cargo clippy                 # lint
```

## Architecture

Two-layer design with **strict one-way dependency**: Managed layer calls Verbs layer APIs; Verbs layer has no knowledge of Managed layer.

```
┌─────────────────────────────────────────────────┐
│ Application (KV demo M5, KV cache demo M7)      │
├─────────────────────────────────────────────────┤
│ ub-managed (M7) — Global allocator, Placer,     │
│   Region table, Coherence SM, Cache tier         │
├─────────────────────────────────────────────────┤
│ ub-core — verbs / jetty / mr / addr / device    │
├─────────────────────────────────────────────────┤
│ ub-transport — sequencing, ACK/SACK, retransmit │
├─────────────────────────────────────────────────┤
│ ub-fabric — UDP(default) / TCP / UDS            │
└─────────────────────────────────────────────────┘
         ▲
         │ ub-control — hub, heartbeat, MR directory, membership
```

Key points:
- **Control plane spans both layers**: Verbs-layer messages (`MR_PUBLISH`, `HEARTBEAT`) and Managed-layer messages (`ALLOC_REQ`, `INVALIDATE`) share the same `ub-control` channel (different enum variants)
- **Control/data plane isolation**: separate tokio tasks and sockets; control RPC must not block data path
- **M1–M5 independently deliverable**: applications can use verbs APIs (`ub_read/write/atomic/send` + NodeID addressing) without Managed layer

## Planned Workspace Crates

| Crate | Purpose |
|---|---|
| `ub-core` | addr, mr, jetty, verbs, device (memory + npu backends) |
| `ub-wire` | Wire protocol encoding/decoding (fixed header + payload + optional ext headers) |
| `ub-transport` | Reliable transport, flow control, fragmentation/reassembly |
| `ub-fabric` | UDP/TCP/UDS Fabric/Session trait implementations |
| `ub-control` | Membership, MR directory, heartbeat, node state machine |
| `ub-obs` | Counters, logging, /metrics, tracing hooks |
| `ub-managed` | Global allocator, placer, region table, coherence (M7) |
| `unibusd` | Daemon binary (process entrypoint, reads YAML config, starts tokio runtime) |
| `unibusctl` | CLI binary (subcommands via local HTTP admin API on same port as /metrics) |

## UB Address Format

128-bit: `[PodID:16 | NodeID:16 | DeviceID:16 | Offset:64 | Reserved:16]`

## Milestones

| Milestone | Scope |
|---|---|
| **M1** | ub-fabric + ub-control (heartbeat, node state machine, unibusd skeleton) |
| **M2** | ub-core (MR, addr, device with CPU + simulated NPU), verbs read/write/atomic, e2e test |
| **M3** | Jetty + send/recv/write_with_imm |
| **M4** | Full reliable transport (FR-REL) + flow control (FR-FLOW) + failure detection (FR-FAIL) |
| **M5** | Observability (FR-OBS) + benchmark + distributed KV demo |
| **M6** (optional) | Multi-hop / source routing / simplified nD-Mesh topology |
| **M7** (vision) | Managed layer: GVA + placer + region cache + SWMR coherence + 3-node KV cache demo |

## Authoritative Documents

- `docs/REQUIREMENTS.md` — functional requirements, user stories, acceptance criteria
- `docs/DESIGN.md` — detailed design in 3 parts: Part I (Verbs layer, M1–M5), Part II (Managed layer, M7), Part III (shared deliverables/tests/milestones)

## Test Strategy (from DESIGN.md)

- Unit tests via `cargo test`
- Integration tests: two-process local UDP/TCP, 1% random packet loss injection
- Concurrency tests: multi-task atomic_cas, multi-task ub_send on same JFS
- Fault injection: kill peer, peer restart with new epoch, MR deregister during inflight
- Performance: criterion bench, target ≥ 50K ops/s for 1KiB write, P50 latency < 200µs
- E2E: shell script launching 3× unibusd + unibusctl assertions
