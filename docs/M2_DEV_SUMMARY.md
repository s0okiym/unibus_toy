# M2 Milestone Development Summary

**Milestone**: M2 — Device Abstraction + MR Registration + Memory Semantics
**Date**: 2026-04-14
**Status**: COMPLETED
**Preceding Milestone**: M1 (two-node control plane, 73 unit tests)

---

## 1. Overview

M2 implements the complete data plane: any node can register CPU/NPU memory as a Memory Region (MR), broadcast its existence to all peers via MR_PUBLISH, and any other node can read/write/atomically operate on that memory via UB address. This milestone bridges the gap between the M1 control plane and actual cross-node data operations.

**Key achievement**: Two unibusd processes can now exchange data — node A registers memory, node B discovers it via MR_PUBLISH, and B can write to / read from / atomically operate on A's memory through the data plane.

---

## 2. New Features

### 2.1 MR_PUBLISH Send-Side Integration

- `MrTable.register()` now emits `MrPublishEvent::Publish` via an optional `mpsc` channel
- `MrTable.deregister()` emits `MrPublishEvent::Revoke`
- Control plane consumes these events and broadcasts `MR_PUBLISH` / `MR_REVOKE` to all peers
- On new peer join (Hello/HelloAck), existing MRs are re-published to the new peer

### 2.2 DataPlaneEngine (new `ub-dataplane` crate)

- **Responder**: Receives DATA frames, dispatches verb operations (Write, ReadReq, AtomicCas, AtomicFaa) on local MRs, sends response frames
- **Client**: Remote verb functions for cross-node operations:
  - `ub_write_remote(addr, data)` — fire-and-forget write
  - `ub_read_remote(addr, len)` — request-response read with 2s timeout
  - `ub_atomic_cas_remote(addr, expect, new)` — request-response atomic CAS
  - `ub_atomic_faa_remote(addr, add)` — request-response atomic FAA
- **Peer management**: `connect_peer(node_id, data_addr)` — registers outbound send channel via `fabric.send_to()`
- **Pending request tracking**: `DashMap<u64, oneshot::Sender<VerbResponse>>` for request-response correlation via opaque IDs

### 2.3 Wire Protocol Extensions

- `AtomicCasResp = 8` — response verb type for atomic CAS
- `AtomicFaaResp = 9` — response verb type for atomic FAA
- Response payload format: `[old_value: u64 BE][status: u32 BE][reserved: u32]`

### 2.4 Admin API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/admin/mr/register` | POST | Register CPU/NPU MR, returns handle + ub_addr |
| `/admin/mr/deregister` | POST | Deregister MR by handle |
| `/admin/mr/list` | GET | List all local MRs with device type |
| `/admin/mr/cache` | GET | List all remote MR cache entries |
| `/admin/verb/write` | POST | Cross-node write: `{ub_addr, data}` |
| `/admin/verb/read` | POST | Cross-node read: `{ub_addr, len}` |
| `/admin/verb/atomic-cas` | POST | Cross-node CAS: `{ub_addr, expect, new}` |
| `/admin/verb/atomic-faa` | POST | Cross-node FAA: `{ub_addr, add}` |

### 2.5 unibusctl Subcommands

| Command | Description |
|---------|-------------|
| `mr-list` | List local MRs (Handle, DeviceKind, UbAddr, Len, Perms, State) |
| `mr-cache` | List remote MR cache entries (OwnerNode, Handle, UbAddr, Len, DeviceKind) |

### 2.6 Peer Discovery Coordination

- Control plane exposes `peer_change_rx: watch::Receiver<u16>` to notify data plane of new peers
- unibusd spawns a task that subscribes to this channel and calls `dataplane.connect_peer()`

---

## 3. Code Changes

### 3.1 New Files

| File | Lines | Description |
|------|-------|-------------|
| `crates/ub-dataplane/Cargo.toml` | 16 | Crate manifest (depends on ub-core, ub-wire, ub-fabric, tokio, bytes, dashmap, byteorder) |
| `crates/ub-dataplane/src/lib.rs` | 907 | DataPlaneEngine implementation — responder, client, frame construction, error handling |
| `tests/m2_e2e.sh` | 242 | E2E test: MR registration, cross-node write/read, atomics |
| `docs/M2_IMPL_PLAN.md` | 217 | Implementation plan (6 steps with dependency graph) |
| `docs/M2_TEST_REPORT.md` | 116 | Test report with bug findings and fixes |
| `docs/M2_DEV_SUMMARY.md` | — | This document |

### 3.2 Modified Files

| File | Change Summary |
|------|---------------|
| `Cargo.toml` | Added `ub-dataplane` to workspace members and dependencies |
| `Cargo.lock` | Updated lock file with new crate |
| `crates/ub-core/src/mr.rs` | Added `MrPublishInfo`, `MrPublishEvent` enum, `new_with_channel()` constructor, event emission in `register()`/`deregister()`, updated `check_perms` for AtomicCasResp/AtomicFaaResp |
| `crates/ub-core/src/types.rs` | Added `AtomicCasResp=8`, `AtomicFaaResp=9` to Verb enum, updated `from_u8()` and roundtrip test |
| `crates/ub-control/src/control.rs` | Added `mr_table`/`mr_event_rx` fields, `set_mr_table()`, `peer_change_tx/rx` watch channel, MR event drain task, `send_existing_mrs()` on peer join, heartbeat fix using `active_peer_senders()` |
| `crates/ub-control/src/member.rs` | Added `active_peer_senders()` method for internal broadcast (returns actual tx channels) |
| `crates/ub-fabric/src/udp.rs` | Added `send_to()` method for direct UDP send without Session |
| `crates/unibusd/Cargo.toml` | Added `ub-dataplane` and `serde` (with derive) dependencies |
| `crates/unibusd/src/main.rs` | Added DataPlaneEngine integration, Device init, MR admin routes, verb routes, peer coordination task, request/response structs |
| `crates/unibusctl/src/main.rs` | Added `MrList` and `MrCache` subcommands with formatted output |

### 3.3 Line Count Summary

| Category | Lines Changed |
|----------|--------------|
| New code (ub-dataplane) | ~907 |
| Modified code | ~587 (+/-) |
| Test scripts | ~242 |
| Documentation | ~350 |
| **Total new/changed** | **~2,086** |

---

## 4. Unit Tests

### 4.1 Test Summary

| Crate | M1 Tests | M2 Tests | Total |
|-------|----------|----------|-------|
| ub-core | 49 | +2 | 51 |
| ub-control | 12 | +0 | 12 |
| ub-dataplane | 0 | +5 | 5 |
| ub-wire | 11 | +0 | 11 |
| ub-fabric | 2 | +0 | 2 |
| **Total** | **74** | **+7** | **81** |

Note: M1 had 73 tests reported; ub-core added 2 more from MR publish event tests, and ub-wire's verb roundtrip test was updated for the new verb types.

### 4.2 New Unit Tests

| Test | Crate | Description |
|------|-------|-------------|
| `test_mr_publish_event_on_register` | ub-core | Verifies MrTable with channel sends `MrPublishEvent::Publish` containing correct mr_handle, owner_node, base_ub_addr, len, perms, device_kind |
| `test_mr_publish_event_on_deregister` | ub-core | Verifies MrTable with channel sends `MrPublishEvent::Revoke` with correct owner_node and mr_handle |
| `test_mr_table_without_channel_works` | ub-core | Verifies MrTable without channel (backward compat) — register/deregister work without events |
| `test_build_data_frame` | ub-dataplane | Constructs a DATA frame and verifies decoded FrameHeader (src_node, dst_node) and DataExtHeader (verb, mr_handle, opaque) match input |
| `test_status_to_error` | ub-dataplane | Verifies status code mapping: 1→AddrInvalid, 2→PermDenied, 3→Alignment |
| `test_dataplane_write_and_read` | ub-dataplane | Two in-process DataPlaneEngines on loopback UDP: engine B writes to engine A's MR, local read verifies data |
| `test_dataplane_atomic_cas_and_faa` | ub-dataplane | Cross-node CAS (expect=0, new=42 → old=0), CAS retry (expect=0 → old=42), FAA (add=1 → old=42) |
| `test_concurrent_cas_serialization` | ub-dataplane | 8 concurrent tokio tasks CAS same address from 0 to their task ID; exactly one returns old=0 |

---

## 5. E2E Tests

### 5.1 M2 E2E Test (`tests/m2_e2e.sh`)

10-step test covering the full cross-node data path:

| Step | Test | Result |
|------|------|--------|
| 1-2 | Start two unibusd nodes, wait for HELLO exchange | PASS |
| 3 | Register CPU MR on node0 via admin API | PASS (handle=1, addr=0x0001:0001:0000:...) |
| 4 | MR_PUBLISH propagates to node1 cache | PASS (1 cache entry) |
| 5 | `unibusctl mr-list` shows MR with handle | PASS |
| 6 | Cross-node write (node1 → node0 MR) via verb API | PASS |
| 7 | Cross-node read (node1 → node0 MR) returns written data | PASS (5 bytes: [72,101,108,108,111] = "Hello") |
| 8 | Register NPU MR, cross-node write + read | PASS |
| 9 | Cross-node atomic CAS: expect=0, new=42 → old=0 | PASS |
| 9 | Cross-node atomic CAS retry: expect=0, new=99 → old=42 (CAS failed) | PASS |
| 10 | Cross-node atomic FAA: add=1 → old=42 | PASS |

### 5.2 M1 E2E Regression Test (`tests/m1_e2e.sh`)

| Test | Result |
|------|--------|
| Two nodes form SuperPod | PASS |
| Both nodes Active | PASS |
| unibusctl node-list works | PASS |
| Fault detection (kill node1) | PASS (Suspect) |

---

## 6. Key Bugs Found and Fixed

### 6.1 Heartbeat/MR_PUBLISH Broadcast Failure

**Symptom**: Nodes marked each other as down after ~6 seconds. MR_PUBLISH never reached peers.

**Root Cause**: `MemberTable::list()` strips `tx` channels (sets to `None`) for the admin API. The heartbeat loop and MR event drain loop both used `list()` to get peers, resulting in `tx: None` — messages were silently dropped instead of being sent.

**Fix**: Added `MemberTable::active_peer_senders()` method that returns `(node_id, Sender)` pairs with actual channels. Updated heartbeat and MR event loops to use this method for sending.

**Files**: `crates/ub-control/src/member.rs`, `crates/ub-control/src/control.rs`

### 6.2 DataPlaneEngine Response Not Received

**Symptom**: Remote atomic operations (CAS, FAA) timed out. Write worked (fire-and-forget), but read and atomics didn't return.

**Root Cause**: `connect_peer()` used `fabric.dial()` which creates a demux entry for the peer. Responses from the peer were routed to the dial session's channel, but `connect_peer()` only spawned a write loop — no read loop consumed the responses.

**Initial attempted fix**: Spawned a recv loop alongside the write loop, sharing the Session via `Arc<Mutex<>>`. This caused deadlock because the recv loop held the mutex lock while waiting, blocking writes.

**Final fix**: Replaced `fabric.dial()` with `fabric.send_to()` in `connect_peer()`. Outbound frames are sent directly via the UDP socket. Responses arrive through the listener's `accept()` path, where `handle_incoming_frame` dispatches response verbs to `complete_pending_request()`. Added `UdpFabric::send_to()` method.

**Files**: `crates/ub-dataplane/src/lib.rs`, `crates/ub-fabric/src/udp.rs`

### 6.3 Opaque ID Collision in Cloned Engines

**Symptom**: Concurrent CAS test failed — all 8 tasks timed out.

**Root Cause**: `DataPlaneEngine::clone()` created a new `AtomicU64` for `next_opaque`, initialized from the current value. Cloned engines could generate the same opaque IDs, causing `pending_requests` DashMap collisions where one task's response overwrites another's oneshot channel.

**Fix**: Changed `next_opaque` from `AtomicU64` to `Arc<AtomicU64>` so all clones share the same counter via `fetch_add()`.

**Files**: `crates/ub-dataplane/src/lib.rs`

---

## 7. Architecture Decisions

### 7.1 ub-dataplane as Separate Crate

**Problem**: DataPlaneEngine depends on ub-core (MR, addr, device), ub-wire (frame codec), and ub-fabric (transport). Adding ub-wire and ub-fabric as dependencies of ub-core creates a cycle: ub-core → ub-fabric → ub-core.

**Decision**: Created `ub-dataplane` as a separate crate that depends on ub-core, ub-wire, and ub-fabric. This follows the existing crate layering and avoids circular dependencies.

### 7.2 Peer Communication via send_to()

**Problem**: The `Session` trait requires `&mut self` for both `send()` and `recv()`, preventing concurrent read+write on a single session. Using `Arc<Mutex<Session>>` causes deadlock when the recv loop holds the lock while waiting for data.

**Decision**: Use `UdpFabric::send_to()` for outbound frames instead of `dial()` + `Session`. This decouples sending from receiving:
- **Outbound**: `connect_peer()` spawns a write loop that reads from an `mpsc::channel` and calls `fabric.send_to()`
- **Inbound**: Responses arrive through the listener's `accept()` path and are processed by the existing recv loop in `start()`

### 7.3 MrPublishEvent Channel Bridge

**Problem**: `MrTable::register()` is a synchronous method (called from admin API handler), but MR_PUBLISH needs to be broadcast asynchronously over TCP.

**Decision**: Added an optional `mpsc::UnboundedSender<MrPublishEvent>` to `MrTable`. The `register()`/`deregister()` methods send events synchronously. The control plane's MR event drain task consumes these events asynchronously and broadcasts to all peers.

---

## 8. Verification

```bash
cargo test                    # 81 tests pass
cargo build --release         # clean build
bash tests/m1_e2e.sh          # M1 regression — PASS
bash tests/m2_e2e.sh          # M2 E2E — PASS
```

---

## 9. Milestone Progress

| Milestone | Status | Unit Tests | E2E |
|-----------|--------|-----------|-----|
| M1 | DONE | 73 | 1 |
| M2 | DONE | 81 | 2 |
| M3 | — | — | — |
| M4 | — | — | — |
| M5 | — | — | — |
