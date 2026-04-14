# M3 Milestone Development Summary

**Milestone**: M3 — Jetty + Send/Recv/Write_with_Imm
**Date**: 2026-04-14
**Status**: COMPLETED
**Preceding Milestone**: M2 (data plane with cross-node read/write/atomic, 81 unit tests)

---

## 1. Overview

M3 adds message semantics to the UniBus data plane: the Jetty abstraction (JFS/JFR/JFC queues) and the Send/Recv/Write_with_Imm verbs. With M3, nodes can exchange messages bilaterally (send/recv) and combine unilateral writes with notifications (write_with_imm), enabling higher-level protocols on top of the data plane.

**Key achievement**: Two unibusd processes can now exchange messages — node A creates a Jetty and posts recv buffers, node B sends messages to A's Jetty and A polls CQEs. B can also write to A's MR and simultaneously notify A's Jetty with an immediate value.

---

## 2. New Features

### 2.1 Jetty Data Structure (`ub-core::jetty`)

The Jetty is a connectionless send/recv unit containing three internal queues:

- **JFS (Jetty Send Queue)**: MPSC queue where application tasks post WorkRequests (pending send operations)
- **JFR (Jetty Recv Queue)**: Pre-posted receive buffers that match against incoming Send messages
- **JFC (Jetty Completion Queue)**: Completion events for both send and recv operations

Key data structures:

| Struct | Description |
|--------|-------------|
| `RecvBuffer` | Pre-posted receive buffer with `buf: Vec<u8>` and `wr_id: WrId` |
| `WorkRequest` | Pending send operation with `wr_id`, `dst_jetty: JettyAddr`, `data`, `imm`, `verb`, `ub_addr`, `mr_handle` |
| `Jetty` | Core struct containing JFS/JFR/JFC queues with configurable depths |
| `JettyTable` | Global table mapping `JettyHandle → Arc<Jetty>` with auto-increment handle allocator |
| `JettyInfo` | Summary info for admin API (handle, node_id, queue depths, counts) |

Key methods on `Jetty`:

| Method | Description |
|--------|-------------|
| `post_recv(buf, wr_id)` | Push receive buffer into JFR; returns `NoResources` if JFR full |
| `post_send(wr)` | Push WorkRequest into JFS; returns `NoResources` if JFS full or JFC at high watermark |
| `pop_send()` | Pop WorkRequest from JFS (for worker task) |
| `pop_recv()` | Pop RecvBuffer from JFR (for matching incoming Send) |
| `push_cqe(cqe)` | Push completion entry onto JFC; returns `NoResources` if JFC full |
| `poll_cqe()` | Non-blocking CQE poll from JFC |
| `close()` | Flush all pending WRs/recv buffers with `CQE(status=FLUSHED)` |

### 2.2 DataPlaneEngine Jetty Integration

DataPlaneEngine extended with `jetty_table: Arc<JettyTable>` field and two new incoming frame handlers:

**`handle_send(payload, ext, jetty_table)`**:
1. Look up `ext.jetty_dst` in local JettyTable → get Jetty
2. Pop a RecvBuffer from JFR (if empty, drop frame + log warning)
3. Copy payload into RecvBuffer (truncate if needed)
4. Push CQE onto JFC: `Cqe { wr_id, status: UB_OK, imm: ext.imm, byte_len, jetty_id: ext.jetty_dst, verb: Verb::Send }`

**`handle_write_imm(payload, ext, mr_table, jetty_table)`**:
1. Write payload to MR at `ext.ub_addr` (same as handle_write)
2. Look up `ext.jetty_dst` in local JettyTable → get Jetty
3. Push CQE onto JFC: `Cqe { wr_id: 0, status: UB_OK, imm: ext.imm, byte_len, jetty_id: ext.jetty_dst, verb: Verb::WriteImm }`

### 2.3 Remote Send/WriteImm Client Functions

Three new fire-and-forget client methods on DataPlaneEngine:

| Method | Verb | Description |
|--------|------|-------------|
| `ub_send_remote(jetty_src, dst_jetty, data)` | Send | Send message to remote Jetty |
| `ub_send_with_imm_remote(jetty_src, dst_jetty, data, imm)` | Send | Send message with immediate value |
| `ub_write_imm_remote(addr, data, imm, jetty_src, dst_jetty)` | WriteImm | Write to remote MR + notify Jetty with imm |

All three:
1. Look up peer in `peer_senders` by dst node_id
2. Build frame via `build_data_frame_ex()` with jetty IDs and optional imm
3. Send via peer sender channel (fire-and-forget, no response expected)

### 2.4 Wire Frame Extension

New helper function `build_data_frame_ex()` extends `build_data_frame()` with:

- `jetty_src: u32` — source Jetty handle
- `jetty_dst: u32` — destination Jetty handle
- `imm: Option<u64>` — immediate value

When `imm.is_some()`: sets `ExtFlags::HAS_IMM` flag and writes the 8-byte immediate value into the extended header.

### 2.5 Admin API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/admin/jetty/create` | POST | Create a new Jetty, returns `{jetty_handle}` |
| `/admin/jetty/list` | GET | List all local Jettys with queue depths and counts |
| `/admin/jetty/post-recv` | POST | Post recv buffer: `{jetty_handle, len}` |
| `/admin/jetty/poll-cqe` | POST | Poll CQE from JFC: `{jetty_handle}` |
| `/admin/verb/send` | POST | Send message: `{dst_node_id, dst_jetty_id, data}` |
| `/admin/verb/send-with-imm` | POST | Send with immediate: `{dst_node_id, dst_jetty_id, data, imm}` |
| `/admin/verb/write-imm` | POST | Write + notify: `{ub_addr, data, imm, dst_node_id, dst_jetty_id}` |

### 2.6 unibusctl Subcommands

| Command | Description |
|---------|-------------|
| `jetty-create` | Create a new Jetty on the target node |
| `jetty-list` | List all Jettys (Handle, NodeID, queue depths/counts) |
| `jetty-post-recv <handle> <len>` | Post a receive buffer to a Jetty |
| `jetty-poll-cqe <handle>` | Poll a CQE from a Jetty's JFC |

---

## 3. Code Changes

### 3.1 New Files

| File | Lines | Description |
|------|-------|-------------|
| `crates/ub-core/src/jetty.rs` | 586 | Jetty struct, JFS/JFR/JFC queues, JettyTable, JettyInfo, 9 unit tests |
| `tests/m3_e2e.sh` | 305 | E2E test: Jetty CRUD, send/recv, send_with_imm, write_with_imm, ordering |

### 3.2 Modified Files

| File | Change Summary |
|------|---------------|
| `crates/ub-core/src/lib.rs` | Added `pub mod jetty;` |
| `crates/ub-core/src/types.rs` | Added `serde::Serialize, serde::Deserialize` derives to `JettyHandle` |
| `crates/ub-dataplane/src/lib.rs` | Added `jetty_table: Arc<JettyTable>` to DataPlaneEngine; added `handle_send()`, `handle_write_imm()` incoming handlers; added `build_data_frame_ex()` with jetty/imm support; added `ub_send_remote()`, `ub_send_with_imm_remote()`, `ub_write_imm_remote()` client methods; added `Verb::Send` and `Verb::WriteImm` match arms in `handle_incoming_frame`; added 6 new tests (send_and_recv, send_with_imm, write_imm, build_data_frame_ex_with_imm, message_ordering, concurrent_sends) |
| `crates/unibusd/src/main.rs` | Added `jetty_table: Arc<JettyTable>` to AppState; created JettyTable at startup; passed to DataPlaneEngine::new(); added 4 Jetty admin routes + 3 verb routes; added 7 handler functions (`jetty_create`, `jetty_list`, `jetty_post_recv`, `jetty_poll_cqe`, `verb_send`, `verb_send_with_imm`, `verb_write_imm`); added `JettyAddr` import; added request structs (`JettyPostRecvRequest`, `JettyPollCqeRequest`, `VerbSendRequest`, `VerbWriteImmRequest`) |
| `crates/unibusctl/src/main.rs` | Added 4 Jetty subcommands: `JettyCreate`, `JettyList`, `JettyPostRecv`, `JettyPollCqe` with formatted output |

### 3.3 Line Count Summary

| Category | Lines |
|----------|-------|
| New code (ub-core jetty.rs) | 586 |
| New code (m3_e2e.sh) | 305 |
| Modified code (ub-dataplane) | ~548 additional |
| Modified code (unibusd) | ~131 additional |
| Modified code (unibusctl) | ~80 additional |
| Modified code (ub-core types.rs) | ~1 line change |
| **Total new/changed** | **~1,651** |

---

## 4. Unit Tests

### 4.1 Test Summary

| Crate | M2 Tests | M3 Tests | Total |
|-------|----------|----------|-------|
| ub-core | 51 | +9 | 60 |
| ub-control | 12 | +0 | 12 |
| ub-dataplane | 5 | +6 | 11 |
| ub-wire | 11 | +0 | 11 |
| ub-fabric | 2 | +0 | 2 |
| **Total** | **81** | **+15** | **96** |

### 4.2 New Unit Tests — ub-core (Jetty)

| Test | Description |
|------|-------------|
| `test_jetty_post_recv_and_pop` | Post 3 recv buffers (different sizes/wr_ids), pop in FIFO order, verify buf.len and wr_id match |
| `test_jetty_post_send_and_pop` | Post 2 WorkRequests (one with imm=Some(42)), pop in FIFO order, verify wr_id and imm |
| `test_jetty_cqe_push_and_poll` | Push 2 CQEs (one with imm=Some(99)), poll in FIFO order, verify wr_id and imm; verify poll returns None when empty |
| `test_jetty_jfc_high_watermark_backpressure` | Fill JFC to high_watermark (12), verify post_send returns NoResources; poll 1 CQE to free space, verify post_send succeeds |
| `test_jetty_close_flushes_pending` | Post 3 WRs + 3 recv buffers, close Jetty, verify JFS/JFR empty, verify 6 FLUSHED CQEs in JFC |
| `test_jetty_table_create_lookup_deregister` | Create 2 Jettys (different handles), lookup by handle, list returns 2, deregister one, verify removed |
| `test_jetty_jfr_depth_limit` | JFR depth=2, post 2 recv buffers OK, 3rd returns NoResources |
| `test_jetty_jfs_depth_limit` | JFS depth=2, post 2 WRs OK, 3rd returns NoResources |
| `test_jetty_jfc_backpressure_recovery` | Fill JFC to high_watermark, post_send blocked; poll 1 CQE, post_send succeeds again |

### 4.3 New Unit Tests — ub-dataplane

| Test | Description |
|------|-------------|
| `test_build_data_frame_ex_with_imm` | Build frame with jetty_src=1, jetty_dst=2, imm=Some(42), verify decoded DataExtHeader has HAS_IMM flag, jetty fields, and imm value |
| `test_dataplane_send_and_recv` | Two engines on loopback: B creates Jetty, A creates Jetty + posts recv, B sends to A's Jetty, A polls CQE with correct verb=Send |
| `test_dataplane_send_with_imm` | Same as above but with imm=Some(42), verify CQE has imm=Some(42) |
| `test_dataplane_write_imm` | B writes 4 bytes to A's MR + notifies A's Jetty with imm=42, verify MR data and CQE imm=Some(42), verb=WriteImm |
| `test_message_ordering_same_jetty_pair` | Post 2 recv buffers (wr_id=100, 101), send 2 messages sequentially, verify CQEs arrive in order (wr_id=100 first, 101 second) |
| `test_concurrent_sends_same_jetty` | Post 4 recv buffers, spawn 4 concurrent tokio tasks that each send, verify all 4 CQEs eventually arrive |

---

## 5. E2E Tests

### 5.1 M3 E2E Test (`tests/m3_e2e.sh`)

11-step test covering the full cross-node Jetty + message semantics path:

| Step | Test | Result |
|------|------|--------|
| 1 | Start two unibusd nodes | PASS |
| 2 | Wait for HELLO exchange, verify both see 2 nodes | PASS |
| 3 | Create Jetty on node0 | PASS (handle=1) |
| 4 | Verify Jetty list on node0, unibusctl jetty-list | PASS (1 Jetty, unibusctl shows handle) |
| 5 | Create Jetty on node1 | PASS (handle=1) |
| 6 | Post recv buffer on node0's Jetty (len=256) | PASS |
| 7 | Cross-node send (node1 → node0 Jetty, data=[72,101,108,108,111]) | PASS |
| 8 | Poll CQE on node0, verify verb=Send | PASS (wr_id=1, verb=Send) |
| 9 | Cross-node send_with_imm (node1 → node0, imm=42) | PASS, CQE has imm=42 |
| 10 | Cross-node write_with_imm (node1 → node0 MR + Jetty, imm=99) | PASS — MR data verified, CQE imm=99, verb=WriteImm |
| 11 | Message ordering: 2 sends on same (src,dst) pair | PASS — 2 CQEs received in order |

### 5.2 Regression Tests

| Test | Result |
|------|--------|
| M1 E2E (`tests/m1_e2e.sh`) | PASS |
| M2 E2E (`tests/m2_e2e.sh`) | PASS |

---

## 6. Key Bugs Found and Fixed

### 6.1 UB_OK Import Scope in jetty.rs Tests

**Symptom**: Compilation error — `UB_OK` not found in test module scope after removing it from the crate-level import.

**Root Cause**: `UB_OK` was imported at crate level in `jetty.rs` but not used in non-test code (only in test code). Removing the crate-level import broke the test module.

**Fix**: Moved `use crate::error::UB_OK;` into the `#[cfg(test)] mod tests` block.

**File**: `crates/ub-core/src/jetty.rs`

### 6.2 JettyHandle Missing Serialize/Deserialize

**Symptom**: Compilation error — `JettyInfo` derives `serde::Serialize` but `JettyHandle` field doesn't implement it.

**Root Cause**: `JettyHandle(u32)` was a plain newtype without serde derives, but `JettyInfo` (used in admin API JSON serialization) contained it.

**Fix**: Added `#[derive(serde::Serialize, serde::Deserialize)]` to `JettyHandle` in `types.rs`.

**File**: `crates/ub-core/src/types.rs`

### 6.3 Unreachable Wildcard in handle_incoming_frame Match

**Symptom**: After adding `Verb::Send` and `Verb::WriteImm` match arms, all 9 Verb variants were covered, making the `_ =>` wildcard unreachable.

**Fix**: Removed the `_ =>` wildcard arm from the match expression.

**File**: `crates/ub-dataplane/src/lib.rs`

---

## 7. Architecture Decisions

### 7.1 Jetty Queues Use Mutex<VecDeque> Instead of Lock-Free

**Problem**: The original plan specified `crossbeam::ArrayQueue` for JFS (MPSC). However, ArrayQueue has fixed capacity and doesn't support variable-depth configuration from `JettyConfig`.

**Decision**: Used `parking_lot::Mutex<VecDeque<>>` for all three queues (JFS, JFR, JFC). This is simpler, supports configurable depths, and sufficient for the toy implementation's performance requirements. The `Notify` primitive provides async wake-up for JFS consumers.

### 7.2 Fire-and-Forget for Send/WriteImm

**Problem**: Send and WriteImm are unilateral operations (no response expected from the receiver), unlike Read/Atomic which are request-response.

**Decision**: `ub_send_remote()`, `ub_send_with_imm_remote()`, and `ub_write_imm_remote()` use the peer sender channel directly without registering pending requests. The receiver processes the frame, delivers data/CQE locally, and no response frame is sent back. This matches the InfiniBand semantics where Send and WriteImm are "fire-and-forget" from the requester's perspective.

### 7.3 Source Jetty Discovery in Admin API

**Problem**: The admin API send/write_imm endpoints need a `jetty_src` for the sender, but clients shouldn't need to know which local Jetty to use.

**Decision**: The admin API handlers automatically use the first Jetty from `jetty_table.list()` as the source. This is a simplification for the toy implementation — a real API would let the client specify the source Jetty.

---

## 8. Verification

```bash
cargo test                    # 96 tests pass
cargo build --release         # clean build
bash tests/m1_e2e.sh          # M1 regression — PASS
bash tests/m2_e2e.sh          # M2 regression — PASS
bash tests/m3_e2e.sh          # M3 E2E — PASS
```

---

## 9. Milestone Progress

| Milestone | Status | Unit Tests | E2E |
|-----------|--------|-----------|-----|
| M1 | DONE | — | PASS |
| M2 | DONE | 81 | PASS |
| M3 | DONE | 96 (+15) | PASS |
| M4 | — | — | — |
| M5 | — | — | — |
