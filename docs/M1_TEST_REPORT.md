# M1 Milestone Test Report

- **Commit**: `ac4b229` feat: implement M1 two-node control plane
- **Date**: 2026-04-14
- **Rust Toolchain**: 1.94.1
- **Profile**: release (optimized)

---

## 1. M1 Scope

Two nodes form a SuperPod via TCP control plane, exchange HELLO/HEARTBEAT, expose admin API, and detect node failures.

## 2. Crates Implemented

| Crate | Description | Key Files |
|-------|-------------|-----------|
| `ub-core` | Core types, address, device, MR, verbs, config, error | addr.rs, device/{memory,npu}.rs, mr.rs, verbs.rs, config.rs, types.rs, error.rs |
| `ub-wire` | Wire protocol frame codec | frame.rs, codec.rs |
| `ub-fabric` | Fabric abstraction + UDP implementation | fabric.rs, udp.rs, peer_addr.rs |
| `ub-control` | TCP control plane engine | control.rs, member.rs, message.rs |
| `unibusd` | Daemon (control plane + axum admin API) | main.rs |
| `unibusctl` | CLI client for admin API | main.rs |
| `ub-transport` | Placeholder for future transport layer | lib.rs |
| `ub-obs` | Placeholder for observability/metrics | lib.rs, metrics.rs |
| `ub-managed` | Placeholder for managed memory layer | lib.rs |

**Total**: 43 files, 6820 lines of Rust code.

## 3. Code Audit — Issues Found and Fixed

During the audit phase, 6 issues were identified and corrected before the final test run.

### 3.1 Critical: HeartbeatPayload missing node_id

- **Problem**: `HeartbeatPayload` had only a `timestamp` field, no `node_id`. The heartbeat handler called `members.update_last_seen(hb.node_id, ...)` but there was no `node_id` to read, causing a compile error or wrong behavior.
- **Fix**: Added `node_id: u16` field to `HeartbeatPayload`, updated encode/decode, and ensured both Heartbeat and HeartbeatAck handlers use the correct node_id.

### 3.2 Critical: HelloAck sent local_epoch: 0

- **Problem**: When responding to a HELLO, the HelloAck was constructed with `local_epoch: 0` instead of the node's actual epoch, because the local_epoch value was not threaded through the function chain.
- **Fix**: Passed `local_epoch` from `ControlPlane::start()` through to `handle_peer_connection()` and `process_control_message()`, so HelloAck carries the real epoch.

### 3.3 Critical: MR permission enforcement missing

- **Problem**: There was no check that an MR's permissions allow the requested verb (e.g., writing to a read-only MR would succeed).
- **Fix**: Added `MrEntry::check_perms(&self, verb: Verb)` method that maps verbs to required `MrPerms` bits and returns `Err(UbError::PermDenied)` if the permission is missing. The verbs module calls this before any device operation.

### 3.4 Critical: Missing verbs module

- **Problem**: No local short-circuit verb helper functions existed. The design doc requires `ub_read_sync`, `ub_write_sync`, `ub_atomic_cas_sync`, `ub_atomic_faa_sync` for same-node operations.
- **Fix**: Created `ub-core/src/verbs.rs` with four functions that: look up MR by address, check permissions, verify alignment (for atomics), and call the Device trait method.

### 3.5 Medium: UDP Fabric first-packet loss

- **Problem**: When a UDP packet arrives before `accept()` creates a session for that peer, the packet was dropped because the demux table had no entry yet.
- **Fix**: Added a `pending: DashMap<SocketAddr, Vec<BytesMut>>` buffer. The recv loop stores early packets in pending; `accept()` drains them into the new session channel after registration.

### 3.6 Medium: NpuDevice write lock for atomic reads

- **Problem**: `NpuDevice::atomic_cas` and `atomic_faa` acquired a write lock (`write()`) on the internal `RwLock<Vec<u8>>`, but atomic reads only need a read lock since `AtomicU64` provides interior mutability.
- **Fix**: Changed to `read()` lock for both atomic operations, allowing concurrent atomic operations on different addresses.

## 4. Unit Test Results

**73 unit tests, all passing.**

### 4.1 ub-core (48 tests)

| Category | Test | Validates |
|----------|------|-----------|
| Address | `test_ub_addr_roundtrip` | UbAddr u128 bit-field construction and field accessors |
| Address | `test_ub_addr_bytes_roundtrip` | UbAddr to_bytes/from_bytes roundtrip |
| Address | `test_ub_addr_text_roundtrip` | UbAddr text format "0x{pod}:{node}:{dev}:{off}:{res}" roundtrip |
| Address | `test_ub_addr_example` | Known example value matches expected text |
| Address | `test_ub_addr_with_offset` | with_offset() changes offset while preserving other fields |
| Address | `test_ub_addr_parse_errors` | Invalid text format returns error |
| Config | `test_config_defaults` | Default NodeConfig values match spec |
| Config | `test_config_missing_node_id` | Config without node_id fails validation |
| Config | `test_config_roundtrip` | YAML serialize/deserialize roundtrip |
| MemoryDevice | `test_memory_device_read_write` | Basic read/write at offset |
| MemoryDevice | `test_memory_device_out_of_bounds` | Out-of-bounds access returns error |
| MemoryDevice | `test_memory_device_kind_and_id` | DeviceKind::Memory and device_id=0 |
| MemoryDevice | `test_memory_device_atomic_cas_success` | CAS with matching expected succeeds |
| MemoryDevice | `test_memory_device_atomic_cas_failure` | CAS with mismatched expected fails (returns old value) |
| MemoryDevice | `test_memory_device_atomic_faa` | FAA increments and returns old value |
| MemoryDevice | `test_memory_device_atomic_alignment_error` | Non-8-aligned atomic returns error |
| MemoryDevice | `test_memory_device_atomic_out_of_bounds` | Out-of-bounds atomic returns error |
| NpuDevice | `test_npu_device_read_write` | Basic read/write on NPU |
| NpuDevice | `test_npu_device_kind_and_id` | DeviceKind::Npu and auto-incremented device_id |
| NpuDevice | `test_npu_device_atomic_cas` | CAS on NPU device |
| NpuDevice | `test_npu_device_atomic_faa` | FAA on NPU device |
| NpuDevice | `test_npu_alloc_no_overlap` | Two allocations have non-overlapping offsets |
| NpuDevice | `test_npu_alloc_alignment` | Allocated offsets are 8-byte aligned |
| NpuDevice | `test_ub_npu_open` | Global NPU device counter increments |
| MR | `test_mr_register_and_lookup` | Register MR, lookup by handle, verify fields |
| MR | `test_mr_two_registrations_no_overlap` | Two MRs on same device have non-overlapping offsets |
| MR | `test_mr_deregister` | Deregister removes MR from table |
| MR | `test_mr_deregister_nonexistent` | Deregistering unknown handle returns error |
| MR | `test_mr_lookup_by_addr` | Lookup MR by UB address, get offset within MR |
| MR | `test_mr_lookup_by_addr_out_of_range` | Address outside MR range returns None |
| MR | `test_mr_inflight_refcount` | try_inflight_inc/dec work correctly |
| MR | `test_mr_inflight_blocked_when_revoking` | try_inflight_inc fails when MR is Revoking |
| MR | `test_mr_check_perms` | Permission check: READ allows ReadReq, denies Write; READ|WRITE|ATOMIC allows all |
| MR | `test_mr_cache_table` | Remote MR cache insert, lookup_by_addr, remove |
| Types | `test_mr_perms_bits` | MrPerms bitflags: READ=1, WRITE=2, ATOMIC=4 |
| Types | `test_verb_roundtrip` | Verb from_u8/u8 roundtrip |
| Types | `test_device_kind_roundtrip` | DeviceKind from_u8/u8 roundtrip |
| Types | `test_error_code_values` | UbStatus error codes match spec values |
| Types | `test_node_state_display` | NodeState Display trait output |
| Verbs | `test_ub_read_sync_ok` | Local read verb succeeds with correct MR |
| Verbs | `test_ub_read_sync_addr_invalid` | Local read returns error for unknown address |
| Verbs | `test_ub_read_sync_perm_denied` | Local read denied on write-only MR |
| Verbs | `test_ub_write_sync_perm_denied` | Local write denied on read-only MR |
| Verbs | `test_ub_atomic_cas_sync_ok` | Local CAS succeeds with correct MR |
| Verbs | `test_ub_atomic_cas_sync_alignment` | Local CAS fails on non-8-aligned address |
| Verbs | `test_ub_atomic_cas_sync_perm_denied` | Local CAS denied without ATOMIC permission |
| Verbs | `test_ub_atomic_faa_sync_ok` | Local FAA succeeds and returns old value |
| Verbs | `test_ub_atomic_faa_sync_perm_denied` | Local FAA denied without ATOMIC permission |

### 4.2 ub-control (12 tests)

| Test | Validates |
|------|-----------|
| `test_ctrl_framing_roundtrip` | Length(4B)+MsgType(1B)+Payload framing encode/decode |
| `test_hello_roundtrip` | HelloPayload encode/decode with node_id, version, epoch, credits, data_addr |
| `test_heartbeat_roundtrip` | HeartbeatPayload encode/decode with node_id, timestamp |
| `test_mr_publish_roundtrip` | MrPublishPayload encode/decode with UbAddr, len, perms, device_kind |
| `test_mr_revoke_roundtrip` | MrRevokePayload encode/decode |
| `test_member_down_roundtrip` | MemberDownPayload encode/decode |
| `test_control_msg_full_roundtrip` | ControlMsg enum encode/decode roundtrip |
| `test_member_table_upsert` | NodeInfo insert and update in MemberTable |
| `test_member_table_mark_down` | mark_down transitions to Down state |
| `test_member_table_mark_suspect_then_active` | Suspect→Active recovery path |
| `test_member_table_active_peers` | List only active peers, exclude self |
| `test_now_millis` | Timestamp generation produces non-zero value |

### 4.3 ub-fabric (2 tests)

| Test | Validates |
|------|-----------|
| `test_udp_fabric_kind` | UdpFabric.kind() returns "udp" |
| `test_udp_fabric_send_recv` | Two UdpFabric nodes: dial, send, accept, recv — verifies pending-buffer fix |

### 4.4 ub-wire (11 tests)

| Test | Validates |
|------|-----------|
| `test_frame_type_roundtrip` | FrameType from_u8/u8 roundtrip |
| `test_frame_flags` | FrameFlags bitflags construction |
| `test_ext_flags` | ExtFlags bitflags construction |
| `test_data_frame_roundtrip` | Full data frame (header + ext header + payload) encode/decode |
| `test_data_frame_with_imm` | Data frame with immediate data field |
| `test_fragment_frame` | Fragmented frame (FRAG_FIRST/MIDDLE/LAST) encoding |
| `test_invalid_magic` | Wrong magic bytes rejected on decode |
| `test_version_mismatch` | Wrong version rejected on decode |
| `test_ack_payload_roundtrip` | AckPayload with SACK blocks encode/decode |
| `test_ack_payload_no_sack` | AckPayload without SACK blocks |
| `test_credit_payload_roundtrip` | CreditPayload encode/decode |

## 5. E2E Test Results

Script: `tests/m1_e2e.sh`

| Step | Description | Result |
|------|-------------|--------|
| 1/6 | Start node0 (unibusd --config node0.yaml) | PASS |
| 2/6 | Start node1 (unibusd --config node1.yaml) | PASS |
| 3/6 | Wait for HELLO exchange (4s) | PASS |
| 4/6 | Admin API: both nodes see >=2 nodes, both Active | PASS |
| 5/6 | unibusctl node-list shows both Active nodes with ports 7910/7911 | PASS |
| 6/6 | Kill node1 → node0 detects node2 as Down | PASS |

**E2E test exit code: 0 (all steps passed)**

## 6. Design Compliance

| Design Requirement (IMPL_PLAN) | Status |
|-------------------------------|--------|
| Step 0.1: Cargo workspace with 9 crates | Done |
| Step 0.2: ub-core error/types/addr | Done |
| Step 0.3: ub-wire frame/codec | Done |
| Step 1.1: Device trait + MemoryDevice | Done |
| Step 1.2: NpuDevice with alloc + atomic | Done |
| Step 1.3: MrTable register/deregister/lookup | Done |
| Step 2.1: Fabric/Listener/Session traits | Done |
| Step 2.2: UdpFabric implementation | Done |
| Step 3.1: Control message types + framing | Done |
| Step 3.2: MemberTable with state machine | Done |
| Step 3.3: ControlPlane (HELLO/HEARTBEAT/peer mgmt) | Done |
| Step 3.4: unibusd daemon + admin API + unibusctl | Done |
| HeartbeatPayload includes node_id | Done (audit fix) |
| HelloAck carries actual local_epoch | Done (audit fix) |
| MR permission enforcement | Done (audit fix) |
| UDP first-packet buffering | Done (audit fix) |
