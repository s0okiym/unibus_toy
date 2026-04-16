# M4 Milestone Development Summary

**Milestone**: M4 — Reliable Transport + Flow Control + Failure Detection
**Date**: 2026-04-16
**Status**: COMPLETED
**Preceding Milestone**: M3 (Jetty + Send/Recv/Write_with_Imm, 96 unit tests, 3 E2E tests)

---

## 1. Overview

M4 adds reliable transport (FR-REL), flow control (FR-FLOW), and failure detection integration (FR-FAIL) to the UniBus data plane. Prior to M4, all data-plane operations used fire-and-forget delivery over UDP — a single dropped packet could cause Read/Atomic operations to time out, and Write/Send operations to be silently lost. M4 introduces sequencing, ACK/SACK-based retransmission, credit-based flow control, sliding-window dedup, write-path idempotency, and read-response caching, making the data plane resilient to packet loss while preserving at-most-once semantics.

**Key achievement**: The data plane operates correctly under 1% packet loss — 100 writes + reads and 50 atomic operations all succeed with correct at-most-once semantics verified. Failed peers are detected, sessions are torn down, and restarted peers re-establish sessions with epoch-guaranteed freshness.

---

## 2. New Features

### 2.1 DedupWindow (`ub-transport::dedup`)

Sliding-window deduplication for received frames. Uses a 1024-bit bitset (16 × u64) to track which sequence numbers within the window have been seen.

| Method | Description |
|--------|-------------|
| `check(seq) -> DedupResult` | Classify a received seq as InOrder / AlreadySeen / OutOfOrder / BeyondWindow |
| `mark_and_advance(seq)` | Mark seq as seen and advance `next_expected` if gaps are filled |
| `is_seen(seq) -> bool` | Check if a specific seq is marked in the bitset |
| `reset()` | Clear the window (used on epoch change) |
| `reset_to(start)` | Reset window with a new starting seq |

### 2.2 ReadResponseCache (`ub-transport::read_cache`)

LRU cache for server-side read/atomic responses, keyed by `(remote_node, opaque)`. Capacity 1024 entries, TTL 5 seconds. On duplicate request retransmission, the cached response is re-sent without re-executing the operation.

| Method | Description |
|--------|-------------|
| `get(key) -> Option<&[u8]>` | Look up cached response; updates LRU order |
| `insert(key, response)` | Insert response; evicts oldest if over capacity |
| `remove_expired()` | Purge entries past TTL |
| `clear()` | Reset cache |

### 2.3 ReliableSession (`ub-transport::session`)

Per-peer reliable session managing both send-side and receive-side state.

**Send side**: Sequence number assignment, retransmit queue, credit tracking, RTO with exponential backoff.

**Receive side**: Dedup window, execution records (for write-path idempotency), read response cache, credit grant accumulator.

Key methods:

| Method | Description |
|--------|-------------|
| `assign_seq(frame, is_fire_and_forget, opaque) -> u64` | Assign stream_seq, set ACK_REQ flag, store in retransmit queue |
| `process_ack(ack) -> Vec<RetransmitEntry>` | Remove ACKed fire-and-forget entries; add credits; keep request-response entries |
| `complete_request(opaque) -> bool` | Remove a request-response entry when its response is confirmed received |
| `receive_frame(seq) -> ReceiveAction` | Dedup check; returns Deliver/Duplicate/OutOfOrder/BeyondWindow |
| `mark_executed(seq)` | Record write-verb execution (idempotency guard) |
| `is_executed(seq) -> bool` | Check if write-verb already executed |
| `consume_credit() -> Result` | Deduct one credit; returns NoResources if exhausted |
| `add_credits(n)` | Restore credits (from ACK/CREDIT frames) |
| `build_ack_payload(has_gap) -> AckPayload` | Build ACK with SACK bitmap and credit_grant |
| `check_rto(now) -> Vec<u64>` | Check for retransmission timeouts; kills session if max_retries exceeded |
| `kill() -> Vec<RetransmitEntry>` | Mark Dead and return all pending entries |
| `should_send_ack() -> bool` | Returns true every 8 frames or when credits_to_grant >= 8 |

### 2.4 TransportManager (`ub-transport::manager`)

Central orchestrator for reliable sessions, ACK/CREDIT framing, and RTO retransmission.

| Method | Description |
|--------|-------------|
| `new(local_node_id, config, flow_config, inbound_tx)` | Construct with config and inbound channel |
| `create_session(remote_node, epoch, initial_credits)` | Create a new ReliableSession |
| `invalidate_session(remote_node, old_epoch)` | Kill old-epoch sessions |
| `send(remote_node, frame, is_fire_and_forget, opaque)` | Main send path: assign seq, deduct credit, send via peer sender |
| `handle_incoming(raw)` | Dispatch: Data → dedup+deliver, Ack → process_ack, Credit → add_credits |
| `register_peer_sender(node_id, sender)` | Register outbound channel for a peer |
| `remove_peer_sender(node_id)` | Remove outbound channel |
| `notify_peer_down(node_id) -> Vec<RetransmitEntry>` | Kill all sessions for a downed peer |
| `complete_request(remote_node, opaque)` | Remove request-response entry after response received |
| `mark_executed(remote_node, seq)` | Mark write-verb as executed |
| `is_executed(remote_node, seq) -> bool` | Check if write-verb already executed |
| `read_cache_get(remote_node, key)` | Get cached response for duplicate request |
| `read_cache_insert(remote_node, key, response)` | Cache response for potential retransmission |
| `grant_credits(remote_node, count)` | Return credits after CQE consumption |
| `start_rto_task()` | Launch background RTO retransmission loop |
| `send_frame_to_peer(remote_node, frame)` | Send a raw frame (for cached response re-sends) |

**ACK sending strategy**:
- Every 8 Data frames (`frames_since_last_ack >= 8`)
- Immediately on duplicate or out-of-order frames (with SACK bitmap)
- ACK carries `credit_grant` = accumulated credits to return to sender
- Independent CREDIT frame when `credits_to_grant >= 8` and no pending ACK

**RTO retransmission loop** (background tokio task):
- Ticks every `rto_base_ms / 2` (100ms)
- Iterates all Active sessions, checks `check_rto(now)`
- Retransmits timed-out frames with exponential backoff (`RTO × 2^retry_count`)
- Kills session and notifies if `retry_count > max_retries`

### 2.5 UdpFabric Loss Injection (`ub-fabric::udp`)

`bind_with_loss(addr, loss_rate)` creates a UdpFabric that randomly drops a fraction of received packets. Used for integration testing only.

### 2.6 DataPlaneEngine Integration

DataPlaneEngine refactored from direct `peer_senders` to `TransportManager` delegation:

- **Send path**: All `ub_*_remote` methods now call `transport.send()` instead of directly pushing to peer sender channels
- **Receive path**: Fabric listener → `transport.handle_incoming(raw)` → dedup → `inbound_rx` → `handle_verb_frame()`
- **Fire-and-forget vs request-response**: Write/Send/SendWithImm/WriteImm marked `is_fire_and_forget=true`; Read/AtomicCas/AtomicFaa marked `false`
- **Write idempotency**: Write/AtomicCas/AtomicFaa check `transport.is_executed()` before execution; `transport.mark_executed()` after
- **Read response caching**: Read/Atomic responses cached; on duplicate, cached response re-sent
- **Request completion**: `complete_pending_request()` calls `transport.complete_request()` to clean retransmit queue
- **Duplicate delivery**: Request-response duplicates (Read/Atomic) delivered to handler for cached response re-sending

---

## 3. Code Changes

### 3.1 New Files

| File | Lines | Description |
|------|-------|-------------|
| `crates/ub-transport/src/dedup.rs` | 281 | DedupWindow sliding-window deduplication, 10 unit tests |
| `crates/ub-transport/src/read_cache.rs` | 192 | LRU read response cache with TTL, 5 unit tests |
| `crates/ub-transport/src/session.rs` | 557 | ReliableSession + RetransmitEntry + execution records, 13 unit tests |
| `crates/ub-transport/src/manager.rs` | 896 | TransportManager orchestration, 14 unit tests |
| `tests/m4_e2e.sh` | 342 | M4 E2E test: cross-node ops, failure detection, peer restart recovery |

### 3.2 Modified Files

| File | Change Summary |
|------|---------------|
| `crates/ub-transport/src/lib.rs` | Added `pub mod dedup/read_cache/session/manager` |
| `crates/ub-transport/Cargo.toml` | Added `dashmap`, `parking_lot`, `rand` dependencies |
| `crates/ub-control/src/control.rs` | Fixed Suspect→Active recovery; added PeerChangeEvent notification (Active/Suspect/Down); epoch change detection in Hello handler |
| `crates/ub-control/src/member.rs` | Added `mark_active()` method to MemberTable |
| `crates/ub-core/src/types.rs` | Added `PeerChangeEvent` struct; added `src_node: u16` field to `Cqe` |
| `crates/ub-dataplane/Cargo.toml` | Added `ub-transport` dependency |
| `crates/ub-dataplane/src/lib.rs` | Replaced `peer_senders` with `Arc<TransportManager>`; new `inbound_rx` processing loop; `handle_verb_frame()` with dedup/idempotency; `complete_pending_request()` with `transport.complete_request()`; atomic response caching; duplicate delivery for request-response verbs; `test_loss_resilience_write_read_and_atomic` |
| `crates/ub-fabric/Cargo.toml` | Added `rand` dependency |
| `crates/ub-fabric/src/udp.rs` | Added `bind_with_loss(addr, loss_rate)` for test loss injection |
| `crates/unibusd/src/main.rs` | TransportManager creation and wiring; updated peer-connector task with PeerChangeEvent handling (Active→create_session, Down→notify_peer_down); credit return on CQE poll |

### 3.3 Line Count Summary

| Category | Lines |
|----------|-------|
| New code (ub-transport: dedup + read_cache + session + manager) | 1,926 |
| New code (m4_e2e.sh) | 342 |
| Modified code (ub-dataplane) | ~619 net additions |
| Modified code (unibusd) | ~63 net additions |
| Modified code (ub-control) | ~80 net additions |
| Modified code (ub-core types.rs) | ~34 net additions |
| Modified code (ub-fabric udp.rs) | ~13 net additions |
| **Total new/changed** | **~3,077** |

---

## 4. Unit Tests

### 4.1 Test Summary

| Crate | M3 Tests | M4 Tests | Total |
|-------|----------|----------|-------|
| ub-core | 60 | +1 | 61 |
| ub-control | 12 | +2 | 14 |
| ub-transport | 0 | +42 | 42 |
| ub-dataplane | 11 | +1 | 12 |
| ub-wire | 11 | +0 | 11 |
| ub-fabric | 2 | +0 | 2 |
| **Total** | **96** | **+46** | **142** |

### 4.2 New Unit Tests — ub-transport

**DedupWindow (10 tests)**:

| Test | Description |
|------|-------------|
| `test_in_order_reception` | Sequential seqs return InOrder |
| `test_duplicate_detection` | Receiving same seq again returns AlreadySeen |
| `test_out_of_order` | Gap in seq returns OutOfOrder |
| `test_beyond_window` | seq beyond window returns BeyondWindow |
| `test_window_slide_old_seqs_already_seen` | After sliding, old seqs become AlreadySeen |
| `test_gap_fill_advances_next_expected` | When gap is filled, next_expected jumps |
| `test_is_seen` | is_seen correctly reports bitset state |
| `test_reset` | reset() clears all state |
| `test_reset_to` | reset_to(start) sets new base |
| `test_custom_window_size` | Non-default window size works correctly |

**ReadResponseCache (5 tests)**:

| Test | Description |
|------|-------------|
| `test_insert_and_get` | Insert then get returns cached response |
| `test_capacity_eviction` | Over-capacity insertion evicts oldest |
| `test_expired_entries` | TTL-expired entries return None |
| `test_remove_expired` | Purge expired entries from cache |
| `test_clear` | clear() empties cache |

**ReliableSession (13 tests)**:

| Test | Description |
|------|-------------|
| `test_assign_seq_increments` | assign_seq returns 1, 2, 3... and enqueues |
| `test_process_ack_removes_entries` | ACK removes fire-and-forget entries only |
| `test_complete_request_removes_entry` | complete_request removes request-response entry by opaque |
| `test_credit_consume_and_add` | Credit deduct and restore |
| `test_credit_exhausted` | NoResources when credits = 0 |
| `test_receive_frame_in_order` | In-order delivery |
| `test_receive_frame_duplicate` | Duplicate detection |
| `test_receive_frame_out_of_order` | Out-of-order with gap |
| `test_receive_frame_beyond_window` | Beyond window rejection |
| `test_mark_executed_idempotency` | mark_executed + is_executed idempotency |
| `test_kill_returns_all_entries` | kill() marks Dead and returns all entries |
| `test_build_ack_payload` | ACK payload has correct ack_seq + credit_grant |
| `test_build_ack_payload_with_sack` | SACK bitmap reflects received gaps |
| `test_check_rto_detects_timeout` | RTO timeout detection |
| `test_check_rto_max_retries_kills_session` | Session killed after max retries |
| `test_should_send_ack` | ACK trigger every 8 frames |

**TransportManager (14 tests)**:

| Test | Description |
|------|-------------|
| `test_create_session_and_find` | Session created and findable by key |
| `test_send_assigns_seq` | send() assigns stream_seq to frame |
| `test_handle_data_frame_delivers` | In-order Data frame delivered to inbound_tx |
| `test_handle_ack_clears_retransmit` | ACK clears fire-and-forget retransmit entries |
| `test_handle_ack_keeps_request_response` | ACK keeps request-response entries (until complete_request) |
| `test_mark_executed` | mark_executed works through TransportManager |
| `test_credit_exhaustion` | send() returns NoResources when credits exhausted |
| `test_grant_credits` | grant_credits restores available credits |
| `test_notify_peer_down_kills_sessions` | notify_peer_down kills all sessions for peer |
| `test_build_ack_frame` | ACK frame encoding |
| `test_build_credit_frame` | CREDIT frame encoding |

### 4.3 New Unit Tests — ub-dataplane

| Test | Description |
|------|-------------|
| `test_loss_resilience_write_read_and_atomic` | 1% loss injection: 100 writes+reads and 50 FAAs all succeed with at-most-once semantics verified |

### 4.4 New Unit Tests — ub-control

| Test | Description |
|------|-------------|
| `test_suspect_to_active_recovery` | Heartbeat recovery triggers mark_active |
| `test_peer_change_event` | PeerChangeEvent emitted on state transitions |

### 4.5 New Unit Tests — ub-core

| Test | Description |
|------|-------------|
| `test_peer_change_event_serde` | PeerChangeEvent serializes/deserializes correctly |

---

## 5. E2E Tests

### 5.1 M4 E2E Test (`tests/m4_e2e.sh`)

10-step test covering cross-node operations, failure detection, and peer restart recovery:

| Step | Test | Result |
|------|------|--------|
| 1 | Start two unibusd nodes | PASS |
| 2 | Wait for HELLO exchange, verify both see 2 nodes | PASS |
| 3 | M3 regression: Create Jettys on both nodes | PASS |
| 4 | Cross-node Write + Read | PASS |
| 5 | Cross-node Atomic CAS + FAA | PASS |
| 6 | Send/Recv + write_with_imm regression | PASS |
| 7 | Kill node1 — failure detection | PASS (node0 detects node1 as Down) |
| 8 | Verify dead node state | PASS |
| 9 | Restart node1 — epoch change, session rebuild | PASS (node0 sees node1 as Active again) |
| 10 | Cross-node operations after restart | PASS |

### 5.2 Regression Tests

| Test | Result |
|------|--------|
| M1 E2E (`tests/m1_e2e.sh`) | PASS |
| M2 E2E (`tests/m2_e2e.sh`) | PASS |
| M3 E2E (`tests/m3_e2e.sh`) | PASS |

---

## 6. Loss Resilience Integration Test

### 6.1 Test Design

The `test_loss_resilience_write_read_and_atomic` test creates two DataPlaneEngine instances connected via `UdpFabric::bind_with_loss(0.01)` (1% random packet loss), with the RTO retransmission task active.

**Test sequence**:
1. Register MR on node A
2. Publish MR so node B can resolve the address
3. Perform 100 cross-node Write + Read cycles from B to A
4. Write 8 bytes of zero to initialize atomic counter
5. Perform 50 Atomic FAA operations from B to A (each adds 1)
6. Read back the counter and verify it equals 50 (at-most-once semantics)

**At-most-once verification**: If any FAA were re-executed (duplicated), the final counter would exceed 50. The test asserts `final_value == 50`, confirming every FAA was executed exactly once despite packet loss and retransmission.

### 6.2 Test Results

| Loss Rate | Runs | Pass | Fail | Notes |
|-----------|------|------|------|-------|
| 1% | 10 | 10 | 0 | Stable; all writes/reads succeed, at-most-once verified |
| 5% | 5 | 2 | 3 | Flaky — some operations exceed RTO max_retries under heavy loss |

### 6.3 Conclusions

1. **Reliable transport works at 1% loss**: All 150 operations (100 writes + 50 FAAs) complete successfully with correct semantics.
2. **At-most-once semantics preserved**: FAA counter exactly equals expected value, confirming no duplicate execution.
3. **RTO retransmission recovers lost frames**: Dropped request and response frames are retransmitted and eventually delivered.
4. **Read response cache prevents redundant work**: Duplicate read/atomic requests receive cached responses without re-execution.
5. **5% loss exceeds design threshold**: At 5% loss, the probability of losing both the original and all retries within the RTO window becomes non-trivial. The M4 plan specified 1% loss as the target; higher loss rates would require congestion control (deferred).

---

## 7. Key Bugs Found and Fixed

### 7.1 Arc<DashMap> Sharing — RTO Task Sees Zero Sessions

**Symptom**: RTO retransmission task reported `sessions=0` despite sessions being created by the main task.

**Root Cause**: `sessions` and `peer_senders` were plain `DashMap` (not `Arc<DashMap>`). `DashMap::clone()` creates an independent copy — the RTO task's clone had no entries because `create_session` was called on the original.

**Fix**: Wrapped `sessions: Arc<DashMap<SessionKey, Mutex<ReliableSession>>>` and `peer_senders: Arc<DashMap<u16, mpsc::Sender<Vec<u8>>>>`. Both the main task and RTO task now share the same map via `Arc::clone()`.

**File**: `crates/ub-transport/src/manager.rs`

### 7.2 ACK Clears Request-Response Retransmit Entries Prematurely

**Symptom**: Under packet loss, Read/Atomic operations time out even though ACKs are received.

**Root Cause**: `process_ack()` removed ALL entries with `seq <= ack_seq`, including request-response entries. But ACK only confirms the receiver got the *request* — the *response* may still be lost. The sender must keep retransmitting until it receives the response.

**Fix**: `process_ack()` now only removes `is_fire_and_forget` entries. Request-response entries remain until `complete_request(opaque)` is called when the response is received by the original sender.

**Files**: `crates/ub-transport/src/session.rs`, `crates/ub-transport/src/manager.rs`

### 7.3 Duplicate Request-Response Frames Silently Dropped

**Symptom**: Under loss, duplicate retransmitted request-response frames (Read/Atomic) were dropped by `ReceiveAction::Duplicate`, so the server never re-sent the cached response.

**Root Cause**: The `handle_data_frame` method only delivered `ReceiveAction::Deliver` frames to the inbound channel. Duplicates were ACKed but silently discarded.

**Fix**: For request-response verbs (ReadReq, AtomicCas, AtomicFaa), duplicate frames are now also delivered to the inbound channel, so the handler can look up the cached response and re-send it.

**File**: `crates/ub-transport/src/manager.rs`

### 7.4 Atomic Duplicate Check Returns Without Resending Response

**Symptom**: Under loss, duplicate Atomic requests were detected (`is_executed` returns true) but the response was never re-sent — the handler returned early.

**Root Cause**: The `is_executed` check for AtomicCas/AtomicFaa in `handle_verb_frame` returned early without looking up and resending the cached response.

**Fix**: On `is_executed` hit, look up the read cache by `(remote_node, opaque)` and resend the cached response frame via `transport.send_frame_to_peer()`.

**File**: `crates/ub-dataplane/src/lib.rs`

### 7.5 Atomic Responses Not Cached

**Symptom**: Even after fixing duplicate delivery, the read cache had no entries for Atomic operations — responses were generated but never stored.

**Root Cause**: `handle_atomic_cas()` and `handle_atomic_faa()` built and sent the response frame but never called `transport.read_cache_insert()`.

**Fix**: Added `transport.read_cache_insert(remote_node, (remote_node, ext.opaque), response_frame)` after building the response in both atomic handlers.

**File**: `crates/ub-dataplane/src/lib.rs`

### 7.6 Atomic Byte Order Mismatch in Loss Test

**Symptom**: Loss resilience test failed with `final_value != 50` — the FAA counter returned unexpected values.

**Root Cause**: Test initialization used `u64::to_be_bytes()` for the counter but `u64::from_be_bytes()` for reading back, while the device backend stores atomics in native byte order (`to_ne_bytes()`/`from_ne_bytes()`).

**Fix**: Changed test initialization and verification to use `to_ne_bytes()`/`from_ne_bytes()`, matching the existing test pattern.

**File**: `crates/ub-dataplane/src/lib.rs`

---

## 8. Architecture Decisions

### 8.1 Transport Logic in ub-transport Crate

**Decision**: Reliable transport (sequencing, ACK/SACK, retransmission, dedup, flow control) lives in `ub-transport`, with DataPlaneEngine delegating to TransportManager.

**Rationale**: Separation of concerns — transport layer is independently testable without MR/Jetty infrastructure. The crate boundary was already established in the workspace.

### 8.2 ACK Confirms Receipt, Not Response Delivery

**Decision**: ACK frames only clear fire-and-forget entries from the retransmit queue. Request-response entries are kept until the response is confirmed received via `complete_request()`.

**Rationale**: ACK acknowledges that the receiver got the request frame. The response frame (traveling in the opposite direction) may still be lost. The sender must retransmit the request until it receives the response, not just until the receiver acknowledges the request.

### 8.3 Duplicate Request-Response Delivery to Handler

**Decision**: For request-response verbs, duplicate frames (from RTO retransmission) are delivered to the handler so cached responses can be re-sent.

**Rationale**: The handler needs to know about duplicates to re-send the cached response. Silently dropping duplicates would leave the sender waiting for a response that the server already generated but lost in transit.

### 8.4 Fixed RTO with Exponential Backoff (No RTT Estimation)

**Decision**: Use fixed RTO (200ms) with exponential backoff (`RTO × 2^retry_count`), capped at 10 doublings. No EWMA RTT estimation.

**Rationale**: Simpler to implement and sufficient for the toy's LAN deployment. Karn algorithm and EWMA RTT are deferred to a future iteration.

### 8.5 1% Loss Target (Not Higher)

**Decision**: The reliable transport is designed and tested for 1% packet loss. 5% loss may exceed retry budget.

**Rationale**: M4 plan specified 1% loss as the target. Higher loss rates would require AIMD congestion control and more aggressive retry budgets, which are deferred.

---

## 9. Verification

```bash
cargo test                    # 142 tests pass
cargo build --release         # clean build
bash tests/m1_e2e.sh          # M1 regression — PASS
bash tests/m2_e2e.sh          # M2 regression — PASS
bash tests/m3_e2e.sh          # M3 regression — PASS
bash tests/m4_e2e.sh          # M4 E2E — PASS (failure detection + restart recovery)
# Loss resilience test: 10/10 passes at 1% loss
```

---

## 10. Milestone Progress

| Milestone | Status | Unit Tests | E2E |
|-----------|--------|-----------|-----|
| M1 | DONE | — | PASS |
| M2 | DONE | 81 | PASS |
| M3 | DONE | 96 (+15) | PASS |
| M4 | DONE | 142 (+46) | PASS |
| M5 | — | — | — |
