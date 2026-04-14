# M2 Implementation Plan: Device Abstraction + MR Registration + Memory Semantics

## Context

M1 is complete (two-node control plane, 73 unit tests, E2E test passing). M2 adds the data plane: any node can register CPU/NPU memory as an MR, broadcast its existence, and any other node can read/write/atomically operate on that memory via UB address. The core gap is: no data-plane listener, no remote verb execution, no MR admin API, and MR_PUBLISH is only handled on the receive side.

## What Already Exists (from M1)

- Device trait + MemoryDevice + NpuDevice (complete)
- MrTable (local) + MrCacheTable (remote) (complete)
- Local sync verbs: ub_read_sync, ub_write_sync, ub_atomic_cas_sync, ub_atomic_faa_sync (complete)
- MR_PUBLISH/MR_REVOKE message encode/decode + receive-side handling (complete)
- Wire frame codec: FrameHeader(32B) + DataExtHeader(48B) with encode_frame/decode_frame (complete)
- Fabric trait + UdpFabric with DashMap demux and pending buffer (complete)
- Control plane: TCP HELLO/HEARTBEAT, MemberTable with data_addr per node (complete)
- unibusd: control plane + axum admin API (/admin/node/list, /admin/node/info)
- unibusctl: node-list, node-info subcommands

## Key Gaps for M2

1. No send-side MR_PUBLISH trigger when MrTable.register() is called
2. No re-broadcast of existing MRs when a new node joins
3. No data-plane UDP listener in unibusd
4. No local MrTable in unibusd (only MrCacheTable for remote MRs)
5. No remote verb functions (construct DATA frame, send via fabric, receive response)
6. No responder-side verb dispatch (receive frame, execute locally, send response)
7. No MR admin API endpoints or unibusctl mr-list
8. No AtomicCasResp/AtomicFaaResp verb types in the Verb enum

## Implementation Steps

### Step M2.1: MR_PUBLISH Send-Side Integration

**What**: Wire MrTable.register()/deregister() to trigger MR_PUBLISH/MR_REVOKE broadcasts. Handle "new peer join" case: send existing MRs to the new peer.

**Files to modify**:
- `crates/ub-core/src/mr.rs` — Add `MrPublishEvent` enum, add optional `mpsc::UnboundedSender<MrPublishEvent>` to MrTable, send events in register()/deregister()
- `crates/ub-core/src/lib.rs` — Export new types
- `crates/ub-control/src/control.rs` — Add `Arc<MrTable>` to ControlPlane, add event-drain task that reads MrPublishEvent and broadcasts to peers, add "send existing MRs to new peer" logic on Hello/HelloAck
- `crates/unibusd/src/main.rs` — Pass MrTable to ControlPlane constructor

**Tests**:
- Unit: MrTable with channel sends MrPublishEvent::Publish on register, MrPublishEvent::Revoke on deregister
- Unit: MrTable without channel still works (backward compat)
- Integration: two in-process control planes, register MR on node A, verify node B's MrCacheTable has the entry

### Step M2.2: DataPlaneEngine Skeleton + Responder

**What**: Create `ub_core::dataplane` module with DataPlaneEngine that starts a UDP listener, receives DATA frames, decodes them, dispatches verb operations on local MRs. Focus on receive/respond path.

**Files to create**:
- `crates/ub-core/src/dataplane.rs` — DataPlaneEngine struct, responder task, verb dispatch

**Files to modify**:
- `crates/ub-core/src/lib.rs` — Add `pub mod dataplane`
- `crates/ub-core/src/types.rs` — Add AtomicCasResp=8, AtomicFaaResp=9 to Verb enum
- `crates/ub-core/Cargo.toml` — Add ub-wire and ub-fabric as dependencies

**Key design**:
- `DataPlaneEngine` owns: `Arc<MrTable>`, `Arc<MrCacheTable>`, `Arc<MemberTable>`, `Arc<UdpFabric>`, `Arc<DashMap<u64, oneshot::Sender<VerbResponse>>>` for pending requests, `AtomicU64` for opaque IDs
- `start()`: bind UdpFabric, call listen(), spawn responder task (accept + recv loop)
- Responder: for each frame, decode_frame(), match verb (Write/ReadReq/AtomicCas/AtomicFaa -> dispatch locally, ReadResp/AtomicCasResp/AtomicFaaResp -> complete pending oneshot)
- Frame construction helper: `build_data_frame(src_node, dst_node, verb, mr_handle, ub_addr, opaque, payload) -> BytesMut`
- Error response: set ExtFlags::ERR_RESP, payload = [status: u32 BE]

**Tests**:
- Unit: DataPlaneEngine creation with MrTable and UdpFabric
- Unit: Construct WRITE frame, send through local UdpFabric, verify device was written
- Unit: Construct READ_REQ frame, send, verify response contains correct data
- Unit: Construct ATOMIC_CAS frame, send, verify response contains old value
- Unit: Invalid MR handle returns error response
- Unit: Permission denied returns error response

### Step M2.3: Remote Verb Functions (Client Side)

**What**: Implement ub_write_remote, ub_read_remote, ub_atomic_cas_remote, ub_atomic_faa_remote in DataPlaneEngine. These look up remote MR in MrCacheTable, construct DATA frame, send via fabric session, and (for reads/atomics) await response via oneshot.

**Files to modify**:
- `crates/ub-core/src/dataplane.rs` — Add remote verb methods, add connect_peer() method

**Key design**:
- Write (fire-and-forget): lookup addr in MrCacheTable -> resolve peer data_addr from MemberTable -> build Write frame (opaque=0) -> session.send()
- Read/Atomic (request-response): lookup -> resolve -> allocate opaque -> create oneshot -> insert into pending_requests -> build frame -> session.send() -> await oneshot with timeout (2s)
- connect_peer(): call fabric.dial(PeerAddr::Inet(data_addr)), store session
- The responder task handles both incoming requests AND responses: check if verb is a response type and complete the pending oneshot

**Tests**:
- Integration: two DataPlaneEngine instances on loopback, cross-node write then local read
- Integration: cross-node read
- Integration: cross-node atomic_cas
- Integration: cross-node atomic_faa
- Unit: unknown address returns AddrInvalid
- Unit: timeout returns Timeout error

### Step M2.4: unibusd Integration + MR Admin API + unibusctl

**What**: Wire DataPlaneEngine into unibusd. Add MR admin API endpoints. Add unibusctl mr-list. Add verb admin API endpoints for triggering remote operations.

**Files to modify**:
- `crates/unibusd/src/main.rs` — Create MrTable, DataPlaneEngine, start data plane, add Device initialization, add MR admin routes, add verb routes, coordinate peer connections via watch channel from ControlPlane
- `crates/unibusctl/src/main.rs` — Add MrList, MrRegister, MrDeregister subcommands
- `crates/ub-control/src/control.rs` — Add watch channel for peer discovery notification

**New admin API endpoints**:
- `POST /admin/mr/register` — Body: `{ "device_kind": "memory"|"npu", "len": 1024, "perms": "rw" }` -> returns handle + ub_addr
- `POST /admin/mr/deregister` — Body: `{ "mr_handle": 1 }`
- `GET /admin/mr/list` — returns all local MRs with device type
- `GET /admin/mr/cache` — returns all remote MR cache entries
- `POST /admin/verb/write` — Body: `{ "ub_addr": "0x...", "data": [1,2,3,4] }` -> ub_write_remote
- `POST /admin/verb/read` — Body: `{ "ub_addr": "0x...", "len": 4 }` -> ub_read_remote
- `POST /admin/verb/atomic-cas` — Body: `{ "ub_addr": "0x...", "expect": 0, "new": 42 }`
- `POST /admin/verb/atomic-faa` — Body: `{ "ub_addr": "0x...", "add": 1 }`

**Peer connection coordination**: ControlPlane publishes new peer data_addr via a watch channel. DataPlaneEngine subscribes and dials new peers.

**Tests**:
- Integration: POST /admin/mr/register returns valid handle and address
- Integration: GET /admin/mr/list shows registered MRs with device type
- Integration: POST /admin/mr/deregister removes the MR
- E2E: two unibusd processes, register MR on node A, verify node B shows it in mr-cache

### Step M2.5: Cross-Node Write/Read E2E Test

**What**: Shell script E2E test demonstrating cross-node write/read for both CPU and NPU MR types.

**Files to create**:
- `tests/m2_e2e.sh`

**Test flow**:
1. Start two unibusd processes
2. Wait for HELLO exchange
3. Register CPU MR on node A via admin API
4. Wait for MR_PUBLISH
5. Write from node B to node A's MR via verb API
6. Read from node A locally, verify data matches
7. Read from node B remotely, verify data matches
8. Repeat with NPU MR
9. Test unibusctl mr-list shows device type

### Step M2.6: Cross-Node Atomic Operations + Concurrency Test

**What**: Test cross-node atomic_cas and atomic_faa, including concurrent CAS serialization test.

**Files to modify**:
- `tests/m2_e2e.sh` — Add atomic test sections
- `crates/ub-core/src/dataplane.rs` — Any fixes needed

**Test flow**:
1. Register MR with ATOMIC permission
2. Initialize target to 0
3. Cross-node CAS(addr, expect=0, new=42) -> succeeds, returns 0
4. Cross-node CAS(addr, expect=0, new=99) -> fails, returns 42
5. Cross-node FAA(addr, add=1) -> returns 42, value becomes 43
6. Concurrent: 8 tokio tasks CAS same address, exactly one succeeds
7. Non-aligned address returns Alignment error
8. CAS without ATOMIC permission returns PermDenied

## Dependency Graph

```
M2.1 (MR_PUBLISH send-side)
  |
  v
M2.2 (DataPlaneEngine responder)
  |
  v
M2.3 (Remote verb functions)
  |
  v
M2.4 (unibusd + admin API + unibusctl)
  |
  v
M2.5 (E2E write/read)
  |
  v
M2.6 (E2E atomics + concurrency)
```

## Critical Files

| File | Action |
|------|--------|
| `crates/ub-core/src/dataplane.rs` | **NEW** — data-plane engine (most important new code) |
| `crates/ub-core/src/mr.rs` | MODIFY — add MrPublishEvent channel |
| `crates/ub-core/src/types.rs` | MODIFY — add AtomicCasResp=8, AtomicFaaResp=9 |
| `crates/ub-core/src/lib.rs` | MODIFY — add `pub mod dataplane` |
| `crates/ub-core/Cargo.toml` | MODIFY — add ub-wire, ub-fabric deps |
| `crates/ub-control/src/control.rs` | MODIFY — add MrTable ref, event drain, re-publish on join, peer watch channel |
| `crates/unibusd/src/main.rs` | MODIFY — add DataPlaneEngine, MrTable, Device init, MR/verb admin routes |
| `crates/unibusctl/src/main.rs` | MODIFY — add mr-list subcommand |
| `tests/m2_e2e.sh` | **NEW** — E2E test script |

## Frame Construction Convention

| Field | Write | ReadReq | ReadResp | AtomicCas | AtomicFaa | AtomicCasResp | AtomicFaaResp |
|-------|-------|---------|----------|-----------|-----------|---------------|---------------|
| verb  | 3     | 1       | 2        | 4         | 5         | 8             | 9             |
| mr_handle | remote | remote | same as req | remote | remote | same as req | same as req |
| ub_addr | target | target | same as req | target | target | same as req | same as req |
| opaque | 0     | unique ID | same as req | unique ID | unique ID | same as req | same as req |

- Write payload: raw bytes to write
- ReadReq payload: [read_len: u32 BE]
- ReadResp payload: raw bytes of the read result
- AtomicCas payload (16B): [expect: u64 BE][new: u64 BE]
- AtomicFaa payload (8B): [add: u64 BE]
- AtomicCasResp/AtomicFaaResp payload (16B): [old_value: u64 BE][status: u32 BE][reserved: u32]
- Error response (ExtFlags::ERR_RESP set): payload = [status: u32 BE]

## Verification

1. `cargo test` — all unit + integration tests pass (target: ~100+ tests)
2. `tests/m2_e2e.sh` — exit code 0
3. Two unibusd processes: register MR on node A, write from node B, read back matches
4. Cross-node atomic_cas succeeds/fails correctly
5. Concurrent CAS test proves serialization
6. `unibusctl mr-list` shows device type column
