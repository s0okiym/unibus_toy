# M2 Milestone Test Report

**Date**: 2026-04-14
**Milestone**: M2 — Device Abstraction + MR Registration + Memory Semantics
**Status**: PASSED

## Summary

M2 adds the complete data plane: any node can register CPU/NPU memory as an MR, broadcast its existence via MR_PUBLISH, and any other node can read/write/atomically operate on that memory via UB address. All unit tests (81) and E2E tests pass.

## Unit Tests

| Crate | Tests | Status |
|-------|-------|--------|
| ub-core | 51 | PASS |
| ub-control | 12 | PASS |
| ub-dataplane | 5 | PASS |
| ub-wire | 11 | PASS |
| ub-fabric | 2 | PASS |
| **Total** | **81** | **PASS** |

### New M2 Unit Tests

| Test | Crate | Description |
|------|-------|-------------|
| `test_mr_publish_event_on_register` | ub-core | MrTable sends MrPublishEvent::Publish on register() |
| `test_mr_publish_event_on_deregister` | ub-core | MrTable sends MrPublishEvent::Revoke on deregister() |
| `test_mr_table_without_channel_works` | ub-core | MrTable without channel is backward compatible |
| `test_build_data_frame` | ub-dataplane | Wire frame construction with FrameHeader + DataExtHeader |
| `test_status_to_error` | ub-dataplane | Status code to UbError mapping |
| `test_dataplane_write_and_read` | ub-dataplane | Two-engine cross-node write then local verify |
| `test_dataplane_atomic_cas_and_faa` | ub-dataplane | Cross-node atomic CAS/FAA with success and failure cases |
| `test_concurrent_cas_serialization` | ub-dataplane | 8 concurrent CAS tasks, exactly one succeeds |

## E2E Tests

### M2 E2E Test (`tests/m2_e2e.sh`)

| Step | Test | Result |
|------|------|--------|
| 1-2 | Two unibusd nodes start, HELLO exchange | PASS |
| 3 | Register CPU MR on node0 | PASS (handle=1) |
| 4 | MR_PUBLISH propagates to node1 cache | PASS |
| 5 | unibusctl mr-list shows MR | PASS |
| 6 | Cross-node write (node1 → node0 MR) | PASS |
| 7 | Cross-node read (node1 → node0 MR) | PASS (5 bytes: "Hello") |
| 8 | Register NPU MR, cross-node write+read | PASS |
| 9 | Cross-node atomic CAS (expect=0, new=42) → old=0 | PASS |
| 9 | Cross-node atomic CAS retry (expect=0, new=99) → old=42 | PASS |
| 10 | Cross-node atomic FAA (add=1) → old=42 | PASS |

### M1 E2E Regression Test (`tests/m1_e2e.sh`)

| Test | Result |
|------|--------|
| Two nodes form SuperPod | PASS |
| Both nodes Active | PASS |
| unibusctl node-list | PASS |
| Fault detection (kill node1) | PASS (Suspect) |

## Key Bugs Found and Fixed

### 1. Heartbeat/MR_PUBLISH Broadcast Failure

**Symptom**: Nodes marked each other as down after ~6 seconds. MR_PUBLISH never reached peers.

**Root Cause**: `MemberTable::list()` strips `tx` channels (sets to `None`) for the admin API. The heartbeat loop and MR event drain loop both used `list()` to get peers, resulting in `tx: None` — messages were silently dropped.

**Fix**: Added `MemberTable::active_peer_senders()` method that returns `(node_id, Sender)` pairs with actual channels. Updated heartbeat and MR event loops to use this method.

### 2. DataPlaneEngine Response Not Received

**Symptom**: Remote atomic operations (CAS, FAA) timed out. Write worked (fire-and-forget), but read and atomics didn't return.

**Root Cause**: `connect_peer()` used `fabric.dial()` which creates a `demux` entry for the peer. Responses from the peer were routed to the dial session's channel, but `connect_peer()` only spawned a write loop — no read loop consumed the responses.

**Fix**: Replaced `fabric.dial()` with `fabric.send_to()` in `connect_peer()`. Outbound frames are sent directly via the UDP socket. Responses arrive through the listener's `accept()` path, where `handle_incoming_frame` dispatches response verbs to `complete_pending_request()`.

### 3. Opaque ID Collision in Cloned Engines

**Symptom**: Concurrent CAS test failed — all 8 tasks timed out.

**Root Cause**: `DataPlaneEngine::clone()` created a new `AtomicU64` for `next_opaque`, initialized from the current value. Cloned engines could generate the same opaque IDs, causing `pending_requests` DashMap collisions.

**Fix**: Changed `next_opaque` from `AtomicU64` to `Arc<AtomicU64>` so all clones share the same counter.

## Architecture Decisions

### ub-dataplane as Separate Crate

Initially attempted to place DataPlaneEngine in ub-core, but this created a cyclic dependency (ub-core → ub-fabric → ub-core). Created `ub-dataplane` crate that depends on ub-core, ub-wire, and ub-fabric.

### Peer Communication via send_to()

Instead of `dial()` + `Session` for outbound connections, uses `fabric.send_to()` directly. This avoids the Session trait's `&mut self` constraint on `send`/`recv` which prevents concurrent read+write. Responses naturally flow through the listener's accept loop.

### Verb Types Added

- `AtomicCasResp = 8` — response to AtomicCas, payload: [old_value:u64 BE][status:u32 BE][reserved:u32]
- `AtomicFaaResp = 9` — response to AtomicFaa, payload: [old_value:u64 BE][status:u32 BE][reserved:u32]

## Test Counts

| Milestone | Unit Tests | E2E Tests |
|-----------|-----------|-----------|
| M1 | 73 | 1 (6 steps) |
| M2 | 81 (+8) | 2 (16 steps + regression) |

## Verification Commands

```bash
cargo test                    # 81 tests pass
cargo build --release         # clean build
bash tests/m1_e2e.sh          # M1 regression
bash tests/m2_e2e.sh          # M2 E2E
```
