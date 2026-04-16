# M4 Implementation Plan: Reliable Transport + Flow Control + Failure Detection

**Milestone**: M4 — 可靠传输 + 流控 + 故障检测
**Date**: 2026-04-16
**Preceding Milestone**: M3 (Jetty + Send/Recv/Write_with_Imm, 96 unit tests, 3 E2E tests)

---

## Context

M1-M3 已完成，数据面以 fire-and-forget 模式运行在 UDP 上：Write/Send/WriteImm 发出即忘，Read/Atomic 使用 2s 硬超时无重传。1% 丢包即可导致操作失败或语义违反。M4 引入可靠传输（FR-REL）、流控（FR-FLOW）和故障检测集成（FR-FAIL），使数据面在丢包环境下仍保持"至多一次"语义和正确背压。

**已有基础设施（可直接复用）**：
- Wire 协议已完整实现：FrameType::Data/Ack/Credit、AckPayload（ack_seq + credit_grant + SACK bitmap）、CreditPayload
- FrameHeader.stream_seq 字段已定义（当前恒为 0）、FrameFlags::ACK_REQ/HAS_SACK 已定义
- TransportConfig（rto_ms=200, max_retries=8, sack_bitmap_bits=256, reassembly_budget_bytes=64MiB）已解析但未使用
- FlowConfig.initial_credits=64、HeartbeatConfig（interval_ms=1000, fail_after=3）已解析
- ub-transport crate 已创建，依赖链完整，当前为空壳
- HelloPayload 已携带 epoch:u32 和 initial_credits:u32

**关键差距**：
1. 无序号分配（stream_seq 恒为 0）
2. ACK/Credit 帧被静默丢弃（handle_incoming_frame 仅处理 Data 帧）
3. 无重传队列 / RTO 定时器
4. 无去重窗口 / 写路径幂等 / 读响应缓存
5. 无信用追踪（credit 窗口）
6. 无分片重组
7. 控制面 Suspect→Active 恢复 bug（心跳恢复后未调用 mark_active）
8. 控制面仅通知新节点加入，不通知 Suspect/Down 状态变化
9. 数据面 peer_senders 无清理机制（节点 Down 后条目残留）

---

## Architecture Decision: Transport in ub-transport crate

**决策**：可靠传输逻辑放在 `ub-transport` crate，DataPlaneEngine 委托给 TransportManager。

**理由**：
- 职责分离：传输层（序号/ACK/重传/流控/去重）与数据面（动词分发/MR操作/Jetty处理）解耦
- 已有 crate 边界：ub-transport 已有正确的依赖链（ub-core, ub-wire, ub-fabric, ub-obs）
- 可测试性：传输层可独立测试，无需 MR/Jetty 基础设施

**集成边界**：

```
App → DataPlaneEngine.ub_write_remote()
       → build_data_frame()
       → TransportManager.send(remote_node, frame, is_fire_and_forget, opaque)
         → ReliableSession: 分配 stream_seq, 设 ACK_REQ, 存入重传队列, 扣减 credit
         → peer_senders → fabric.send_to()

Fabric listener → raw bytes
  → TransportManager.handle_incoming(raw)
    → Data 帧: 去重 → 通过 inbound_tx 传递给 DataPlaneEngine → 回 ACK(credit_grant)
    → Ack 帧: 更新重传队列 / 恢复 credit
    → Credit 帧: 恢复 credit
```

---

## Deferred Features（明确推迟）

1. **分片/重组**：当前 MAX_PAYLOAD_SIZE=1320 够用，wire 字段已定义，逻辑推迟到 M4 后迭代
2. **AIMD 拥塞控制**：仅实现信用流控即可满足 FR-FLOW-1/2，AIMD 推迟到细化迭代
3. **EWMA RTT 估算**：使用固定 RTO + 指数退避，Karn 算法推迟
4. **SACK 快速重传**（3 重复 SACK 触发）：生成 SACK bitmap 但推迟快速重传触发逻辑
5. **Header CRC**：当前恒为 0，验证推迟

---

## Implementation Steps

### Step M4.0: Control Plane Bug Fixes + State Change Notification

**目标**：修复 Suspect→Active 恢复 bug，增加 PeerChangeEvent 通知。

#### M4.0.1: 修复 Suspect→Active 恢复

**文件**: `crates/ub-control/src/control.rs` (line 274)

当前逻辑：`missed_count.remove(&node.node_id)` 仅重置计数器，不恢复状态。

修改为：
```rust
} else {
    if missed_count.remove(&node.node_id).is_some() {
        if node.state == NodeState::Suspect {
            members_hb.mark_active(node.node_id);
        }
    }
}
```

#### M4.0.2: PeerChangeEvent watch channel

**文件**: `crates/ub-core/src/types.rs`, `crates/ub-control/src/control.rs`

新增类型（放在 ub-core/types.rs 以避免循环依赖）：
```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerChangeEvent {
    pub node_id: u16,
    pub new_state: NodeState,
    pub epoch: u32,
    pub initial_credits: u32,
    pub data_addr: String,
}
```

修改 ControlPlane：
- `peer_change_tx: watch::Sender<u16>` → `watch::Sender<PeerChangeEvent>`
- `peer_change_rx: watch::Receiver<u16>` → `watch::Receiver<PeerChangeEvent>`
- 在以下时机发送事件：
  - Hello/HelloAck 处理时（节点变 Active）→ 发送 `PeerChangeEvent { new_state: Active, epoch, initial_credits, data_addr }`
  - mark_suspect 后 → 发送 `{ new_state: Suspect }`
  - mark_down 后 → 发送 `{ new_state: Down }`
  - mark_active 后 → 发送 `{ new_state: Active }`

修改 unibusd peer-connector task：
- 匹配 `event.new_state` 而非仅 `node_id != 0`
- Active: connect_peer + create_session
- Down: transport.notify_peer_down
- Suspect: 仅日志

**测试**：
- `test_suspect_to_active_recovery` — 模拟心跳恢复，验证 mark_active 调用
- `test_peer_change_event_active` — 验证 Hello 触发 Active 事件
- `test_peer_change_event_suspect_down` — 验证心跳丢失触发 Suspect/Down 事件

---

### Step M4.1: DedupWindow (ub-transport)

**文件**: `crates/ub-transport/src/dedup.rs` (新建)

滑动窗口去重，窗口大小 W=1024。使用 bitset（16 个 u64 = 1024 bit）跟踪已接收序号。

```rust
pub enum DedupResult {
    AlreadySeen,    // seq < next_expected 或 bitset 中已标记
    InOrder,        // seq == next_expected
    OutOfOrder,     // next_expected < seq < next_expected + W, 未标记
    BeyondWindow,   // seq >= next_expected + W
}

pub struct DedupWindow {
    next_expected: u64,
    seen: [u64; 16],  // 1024-bit bitset
    window_size: usize,
}
```

关键方法：
- `check(&self, seq: u64) -> DedupResult` — 判断序号状态
- `mark_and_advance(&mut self, seq: u64) -> u64` — 标记已见并推进 next_expected
- `is_seen(&self, seq: u64) -> bool` — 检查窗口内序号是否已标记
- `reset(&mut self)` — 清空（epoch 变化时使用）

**测试**（6 个）：
- 顺序接收返回 InOrder
- 重复序号返回 AlreadySeen
- 乱序返回 OutOfOrder
- 超出窗口返回 BeyondWindow
- 窗口滑动后旧序号返回 AlreadySeen
- gap 填补后 next_expected 跳跃推进

---

### Step M4.2: ReadResponseCache (ub-transport)

**文件**: `crates/ub-transport/src/read_cache.rs` (新建)

LRU 缓存，键为 `(src_node: u16, opaque: u64)`，值为响应帧字节。容量 1024 条目，TTL 5s。

```rust
pub struct ReadResponseCache {
    entries: LinkedHashMap<(u16, u64), CacheEntry>,
    capacity: usize,
    ttl: Duration,
}

struct CacheEntry {
    response: Vec<u8>,
    inserted_at: Instant,
}
```

方法：
- `get(&mut self, key: (u16, u64)) -> Option<&[u8]>` — 命中则返回缓存的响应帧，同时更新 LRU
- `insert(&mut self, key: (u16, u64), response: Vec<u8>)` — 插入，超容量则驱逐最旧
- `remove_expired(&mut self)` — 清除过期条目

使用 `linked_hash_map` crate 或手写 LRU（VecDeque + HashMap）。为减少依赖，手写简单 LRU。

**测试**（3 个）：
- 插入后可命中
- 容量满时驱逐最旧
- 过期条目不可命中

---

### Step M4.3: ReliableSession (ub-transport)

**文件**: `crates/ub-transport/src/session.rs` (新建)

```rust
pub struct SessionKey {
    pub local_node: u16,
    pub remote_node: u16,
    pub epoch: u32,
}

pub enum SessionState { Active, Dead }

pub struct RetransmitEntry {
    pub seq: u64,
    pub frame: Vec<u8>,
    pub first_sent_at: Instant,
    pub rto_deadline: Instant,
    pub retry_count: u32,
    pub is_fire_and_forget: bool,
    pub opaque: u64,
}

pub enum ReceiveAction {
    Deliver,          // 新的、按序帧
    Duplicate,        // 已见，仅 ACK
    OutOfOrder,       // 有 gap，缓冲后 ACK+SACK
    BeyondWindow,     // 超出窗口，丢弃
}

pub struct ReliableSession {
    pub key: SessionKey,
    pub state: SessionState,

    // === 发送侧 ===
    pub next_seq: u64,
    pub available_credits: u32,
    pub retransmit_queue: VecDeque<RetransmitEntry>,
    pub rto_base_ms: u64,        // 初始 RTO
    pub max_retries: u32,

    // === 接收侧 ===
    pub dedup: DedupWindow,
    pub execution_records: VecDeque<(u64, bool)>,  // (seq, executed)
    pub read_cache: ReadResponseCache,
    pub credits_to_grant: u32,    // 待返还给对方的信用
    pub frames_since_last_ack: u32,  // 距上次 ACK 接收的帧数

    // === RTT 估算 (M4 暂用固定 RTO，预留字段) ===
    pub srtt_ms: Option<u64>,
    pub rttvar_ms: Option<u64>,
}
```

关键方法：

| 方法 | 签名 | 描述 |
|------|------|------|
| `new` | `(key, rto_ms, max_retries, initial_credits) -> Self` | 创建新会话 |
| `assign_seq` | `(&mut self, frame: &mut [u8], is_fire_and_forget: bool, opaque: u64) -> u64` | 分配 stream_seq，设置 ACK_REQ，存入重传队列 |
| `process_ack` | `(&mut self, ack: &AckPayload) -> Vec<RetransmitEntry>` | 移除已确认条目，返回被清理的条目 |
| `receive_frame` | `(&mut self, seq: u64) -> ReceiveAction` | 去重检查，返回处理动作 |
| `mark_executed` | `(&mut self, seq: u64)` | 记录写路径已执行 |
| `is_executed` | `(&self, seq: u64) -> bool` | 检查写路径是否已执行 |
| `add_credits` | `(&mut self, credits: u32)` | 增加可用信用 |
| `consume_credit` | `(&mut self) -> Result<(), UbError>` | 扣减信用，0 则返回 NoResources |
| `kill` | `(&mut self) -> Vec<RetransmitEntry>` | 标记 Dead，返回所有未确认条目 |
| `build_ack_payload` | `(&mut self, has_gap: bool) -> AckPayload` | 构造 ACK（含 SACK bitmap 和 credit_grant） |
| `check_rto` | `(&mut self, now: Instant) -> Vec<u64>` | 返回超时的 seq 列表，更新 deadline 和 retry_count |

**SACK bitmap 构造**：
从 dedup.seen bitset 提取。bit i 表示 `ack_seq + 1 + i` 已收到。仅当 has_gap=true 时填充。

**写路径幂等性**：
`execution_records` 与 dedup 窗口大小相同（1024）。按 seq 排序的 VecDeque。接收端收到写类动词时：
1. `is_executed(seq)` 返回 true → 仅 ACK，不重执行
2. 返回 false → 执行动词，调用 `mark_executed(seq)`

**测试**（11 个）：
- assign_seq 递增序号
- process_ack 移除已确认条目
- credit 扣减和恢复
- credit 耗尽返回 NoResources
- receive_frame 顺序/重复/乱序/超窗口
- mark_executed 幂等检查
- kill 返回全部未确认条目
- build_ack_payload 正确的 ack_seq + credit_grant
- SACK bitmap 反映 gap
- check_rto 检测超时条目
- check_rto 超过 max_retries 后会话变 Dead

---

### Step M4.4: TransportManager (ub-transport)

**文件**: `crates/ub-transport/src/manager.rs` (新建)

```rust
pub struct InboundFrame {
    pub header: FrameHeader,
    pub ext: DataExtHeader,
    pub payload: Vec<u8>,
    pub seq: u64,  // 传输层序号，用于执行记录
}

pub struct TransportManager {
    local_node_id: u16,
    sessions: DashMap<SessionKey, parking_lot::Mutex<ReliableSession>>,
    peer_senders: DashMap<u16, mpsc::Sender<Vec<u8>>>,
    inbound_tx: mpsc::UnboundedSender<InboundFrame>,
    rto_task: Option<JoinHandle<()>>,
    config: TransportConfig,
    flow_config: FlowConfig,
}
```

关键方法：

| 方法 | 签名 | 描述 |
|------|------|------|
| `new` | `(local_node_id, config, flow_config, inbound_tx) -> Self` | 构造 |
| `create_session` | `(&self, remote_node, epoch, initial_credits) -> Result` | 创建 ReliableSession |
| `invalidate_session` | `(&self, remote_node, old_epoch) -> Vec<RetransmitEntry>` | 作废旧会话 |
| `send` | `(&self, remote_node, frame_data, is_fire_and_forget, opaque) -> Result` | 主发送路径 |
| `handle_incoming` | `(&self, raw: &[u8])` | 接收调度（Data/Ack/Credit） |
| `register_peer_sender` | `(&self, node_id, sender)` | 注册出站通道 |
| `remove_peer_sender` | `(&self, node_id)` | 移除出站通道 |
| `notify_peer_down` | `(&self, node_id) -> Vec<RetransmitEntry>` | 节点 Down 通知 |
| `mark_executed` | `(&self, remote_node, seq)` | 标记写路径已执行 |
| `grant_credits` | `(&self, remote_node, count)` | CQE 消费后返还信用 |

#### 发送路径 (send)

1. 查找 session: `(local_node_id, remote_node, epoch)`
2. 调用 `session.consume_credit()` — 0 则返回 NoResources
3. 调用 `session.assign_seq(frame_data, is_fire_and_forget, opaque)` — 修改 stream_seq、设 ACK_REQ
4. 通过 `peer_senders[remote_node]` 发送修改后的帧

#### 接收路径 (handle_incoming)

1. `decode_frame(raw)` → `(header, ext, payload)`
2. **Data 帧**：
   - 查找 session
   - `session.receive_frame(header.stream_seq)` → ReceiveAction
   - Deliver: 通过 `inbound_tx` 发送 `InboundFrame`
   - Duplicate: 仅发 ACK（写路径不重执行）
   - OutOfOrder: 发 ACK + SACK
   - 递增 `session.credits_to_grant`，每 8 帧或 50ms 发 ACK
3. **Ack 帧**：
   - `decode_ack_payload(payload, header.flags.contains(HAS_SACK))`
   - `session.process_ack(ack)` → 移除已确认的重传条目
   - `session.add_credits(ack.credit_grant)`
4. **Credit 帧**：
   - `decode_credit_payload(payload)`
   - `session.add_credits(credit.credits)`

#### ACK 发送策略

- 每 8 个 Data 帧发一次 ACK（`frames_since_last_ack >= 8`）
- 每次接收乱序/重复帧时立即发 ACK（带 SACK）
- ACK 携带 `credit_grant = session.credits_to_grant`，发送后重置为 0
- 如果 `credits_to_grant > 8` 且无待发 ACK，发独立 CREDIT 帧

#### RTO 重传循环

每 `rto_base_ms / 2`（100ms）tick 一次：
1. 遍历所有 Active session
2. 调用 `session.check_rto(now)` → 返回超时 seq 列表
3. 对每个超时条目：
   - `retry_count < max_retries`：重发帧，`rto_deadline = now + rto_base_ms * 2^retry_count`
   - `retry_count >= max_retries`：`session.kill()` → 收集未确认条目 → 通过回调通知 DataPlaneEngine

**测试**（10 个）：
- create_session 后可查找
- send 分配 stream_seq
- handle_incoming Data 帧被传递给 inbound_tx
- handle_incoming 重复帧被丢弃
- ACK 清理重传队列
- Credit 恢复信用
- RTO 超时触发重传
- max_retries 后会话 Dead
- notify_peer_down 杀死会话
- 信用耗尽 send 返回 NoResources

---

### Step M4.5: DataPlaneEngine Integration

**文件**: `crates/ub-dataplane/src/lib.rs` (修改)

这是最侵入性的修改——DataPlaneEngine 必须从直接操作 peer_senders/fabric 切换为通过 TransportManager。

#### M4.5.1: 添加 ub-transport 依赖

`crates/ub-dataplane/Cargo.toml` 添加：
```toml
ub-transport = { workspace = true }
```

#### M4.5.2: 重构 DataPlaneEngine 结构

```rust
pub struct DataPlaneEngine {
    local_node_id: u16,
    mr_table: Arc<MrTable>,
    mr_cache: Arc<MrCacheTable>,
    jetty_table: Arc<JettyTable>,
    fabric: Arc<UdpFabric>,
    transport: Arc<TransportManager>,       // 新增，替代 peer_senders
    inbound_rx: mpsc::UnboundedReceiver<InboundFrame>,  // 新增
    pending_requests: Arc<DashMap<u64, oneshot::Sender<VerbResponse>>>,
    next_opaque: Arc<AtomicU64>,
    tasks: Vec<JoinHandle<()>>,
    // 移除: peer_senders (由 TransportManager 管理)
}
```

#### M4.5.3: 修改构造函数

`DataPlaneEngine::new` 签名变更：
```rust
pub fn new(
    local_node_id: u16,
    mr_table: Arc<MrTable>,
    mr_cache: Arc<MrCacheTable>,
    jetty_table: Arc<JettyTable>,
    fabric: Arc<UdpFabric>,
    transport: Arc<TransportManager>,
    inbound_rx: mpsc::UnboundedReceiver<InboundFrame>,
) -> Self
```

#### M4.5.4: 修改 start() — 接收路径

当前 `start()` 的 listener accept 循环直接调用 `handle_incoming_frame()`。修改为：

1. accept 循环读取 raw bytes → `transport.handle_incoming(raw)`
2. 新增 inbound processing task：从 `inbound_rx` 读取 `InboundFrame` → 调用动词处理器
3. 动词处理器签名从 `handle_incoming_frame(raw, ...)` 改为 `handle_verb_frame(InboundFrame, ...)`
4. 写路径动词（Write, WriteImm, AtomicCas, AtomicFaa, Send）执行后调用 `transport.mark_executed(remote_node, seq)`

#### M4.5.5: 修改 connect_peer()

```rust
pub async fn connect_peer(&self, node_id: u16, data_addr: SocketAddr) -> Result<(), UbError> {
    let (tx, rx) = mpsc::channel(256);
    self.transport.register_peer_sender(node_id, tx);
    // 启动写循环（同 M3）
    let fabric = Arc::clone(&self.fabric);
    tokio::spawn(async move {
        let mut rx = rx;
        while let Some(data) = rx.recv().await {
            let _ = fabric.send_to(data_addr, &data).await;
        }
    });
    Ok(())
}
```

注意：`create_session` 由 unibusd 的 peer-connector task 调用（根据 PeerChangeEvent）。

#### M4.5.6: 修改发送方法

所有 `ub_*_remote` 方法中的发送逻辑：

```rust
// 之前:
sender.send(frame.to_vec()).await.map_err(|_| UbError::LinkDown)?;

// 之后:
self.transport.send(remote_node, frame.to_vec(), is_fire_and_forget, opaque)?;
```

is_fire_and_forget 映射：
- Write / Send / SendWithImm / WriteImm → `true`
- Read / AtomicCas / AtomicFaa → `false`

注意：`transport.send()` 是同步的（仅入队），所以 `await` 去掉。信用不足时返回 `UbError::NoResources`。

#### M4.5.7: 会话死亡处理

当 TransportManager 检测到会话死亡（max_retries 耗尽或 peer Down），返回 `Vec<RetransmitEntry>`。DataPlaneEngine 需要一个回调处理：

```rust
// 在 TransportManager 中注册回调
transport.set_on_session_dead(Arc::new(|entries: Vec<RetransmitEntry>, pending: &DashMap<u64, oneshot::Sender<VerbResponse>>| {
    for entry in entries {
        if !entry.is_fire_and_forget {
            if let Some((_, tx)) = pending.remove(&entry.opaque) {
                let _ = tx.send(VerbResponse { status: UB_ERR_LINK_DOWN as u32, data: vec![] });
            }
        }
    }
}));
```

#### M4.5.8: 读响应缓存集成

接收端处理 ReadReq 时：
1. 检查 session.read_cache.get((src_node, opaque))
2. 命中：直接发送缓存的响应帧（不重新执行读操作）
3. 未命中：执行读操作，将响应帧缓存，发送

**测试**（5 个）：
- write 通过 transport 发送
- read 通过 transport 发送并缓存响应
- 重复写不重执行
- 重复读返回缓存
- 会话死亡触发 LinkDown

---

### Step M4.6: unibusd Integration

**文件**: `crates/unibusd/src/main.rs` (修改)

#### M4.6.1: 创建 TransportManager

```rust
let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
let transport = Arc::new(TransportManager::new(
    config.node_id,
    config.transport.clone(),
    config.flow.clone(),
    inbound_tx,
));
```

#### M4.6.2: 传递给 DataPlaneEngine

```rust
let mut dataplane = DataPlaneEngine::new(
    config.node_id,
    Arc::clone(&mr_table),
    Arc::clone(&mr_cache),
    Arc::clone(&jetty_table),
    fabric,
    Arc::clone(&transport),
    inbound_rx,
);
```

#### M4.6.3: 更新 peer-connector task

```rust
tokio::spawn(async move {
    let mut rx = peer_change_rx;
    while rx.changed().await.is_ok() {
        let event = rx.borrow().clone();
        match event.new_state {
            NodeState::Active => {
                // 连接数据面
                if let Ok(data_addr) = event.data_addr.parse::<SocketAddr>() {
                    let dp = dataplane_peer.lock().await;
                    let _ = dp.connect_peer(event.node_id, data_addr).await;
                }
                // 创建传输会话
                transport.create_session(event.node_id, event.epoch, event.initial_credits);
            }
            NodeState::Suspect => {
                tracing::warn!("node {} is suspect", event.node_id);
            }
            NodeState::Down => {
                let entries = transport.notify_peer_down(event.node_id);
                // 处理未确认条目 → 完成 pending_requests
                for entry in entries {
                    if !entry.is_fire_and_forget {
                        // 通过 dataplane 的 pending_requests 完成
                    }
                }
                transport.remove_peer_sender(event.node_id);
            }
            _ => {}
        }
    }
});
```

#### M4.6.4: Credit Return on CQE Consumption

Jetty poll_cqe 时需要返还信用。在 `jetty_poll_cqe` handler 中：

```rust
if let Some(cqe) = jetty.poll_cqe() {
    // 通知传输层返还信用
    transport.grant_credits(cqe_src_node, 1);
    // ... 返回 CQE JSON
}
```

需要知道 CQE 来自哪个远端节点。当前 CQE 结构中没有 src_node 字段。可选方案：
- 在 JFC 中存储 src_node 信息（修改 Cqe 结构）
- 或在 push_cqe 时由 handle_send/handle_write_imm 传递 src_node

**决策**：在 Cqe 结构中添加 `src_node: u16` 字段。修改 push_cqe 调用处传入 src_node。

**测试**：通过 M1/M2/M3 E2E 回归测试验证集成正确性。

---

### Step M4.7: Packet Loss Resilience + E2E Test

**文件**: `crates/ub-fabric/src/udp.rs` (修改), `tests/m4_e2e.sh` (新建)

#### M4.7.1: 丢包注入

在 UdpFabric 中增加可选丢包率：

```rust
pub struct UdpFabric {
    // ... 现有字段 ...
    loss_rate: f64,  // 0.0~1.0, 仅测试用
}

impl UdpFabric {
    pub fn bind_with_loss(addr: SocketAddr, loss_rate: f64) -> Result<Self, UbError> {
        // 同 bind(), 但设置 loss_rate
    }
}
```

在 recv 循环中：`if self.loss_rate > 0.0 && rand::random::<f64>() < self.loss_rate { continue; }`

#### M4.7.2: M4 E2E 测试 (tests/m4_e2e.sh)

| 步骤 | 测试 | 验证 |
|------|------|------|
| 1-2 | 启动两个节点，等待 HELLO | 两节点互相可见 |
| 3 | M3 全部操作回归 | Send/Recv/WriteImm 正常 |
| 4 | 跨节点 Write + Read | 数据正确 |
| 5 | 跨节点 Atomic CAS + FAA | 原子语义正确 |
| 6 | 杀死 node1 | node0 在 fail_after*2 内标记 node1 为 Down |
| 7 | node0 对 node1 的操作返回 error | LinkDown 或 Timeout |
| 8 | 重启 node1 | node0 检测到 epoch 变化，重建会话 |
| 9 | 跨节点操作恢复正常 | 新会话可用 |

---

### Step M4.8: Loss Injection Integration Test

**文件**: `crates/ub-dataplane/src/lib.rs` (添加测试)

1% 丢包环境下的集成测试：
- 使用 `UdpFabric::bind_with_loss("127.0.0.1:0", 0.01)`
- 执行 100 次 Write + Read，全部成功
- 执行 50 次 AtomicCAS，全部成功
- 验证写入数据正确（至多一次语义）

---

## Dependency Graph

```
M4.0 (Control plane fixes)  ─────────────────────────┐
                                                       │
M4.1 (DedupWindow)     ─┐                              │
M4.2 (ReadCache)       ─┤                              │
M4.3 (ReliableSession) ─┤                              │
                         ↓                              │
M4.4 (TransportManager) ─┤                              │
                         ↓                              │
M4.5 (DataPlaneEngine)  ─┤                              │
                         ↓                              ↓
M4.6 (unibusd integration) ───────────────────────────┘
                         │
                         ↓
M4.7 (E2E test + loss injection)
                         │
                         ↓
M4.8 (Loss integration test)
```

M4.0 和 M4.1/M4.2 可并行。M4.3 依赖 M4.1+M4.2。M4.4 依赖 M4.3。M4.5 依赖 M4.4。M4.6 依赖 M4.0+M4.5。M4.7+M4.8 依赖 M4.6。

---

## File Summary

### New Files

| File | Purpose | Est. Lines |
|------|---------|-----------|
| `crates/ub-transport/src/dedup.rs` | DedupWindow 滑动窗口去重 | ~120 |
| `crates/ub-transport/src/read_cache.rs` | LRU 读响应缓存 | ~100 |
| `crates/ub-transport/src/session.rs` | ReliableSession + RetransmitEntry + 执行记录 | ~400 |
| `crates/ub-transport/src/manager.rs` | TransportManager 发送/接收/ACK/RTO 调度 | ~500 |
| `crates/ub-transport/src/ack.rs` | ACK/SACK 帧构造辅助函数 | ~80 |
| `tests/m4_e2e.sh` | M4 E2E 测试脚本 | ~250 |

### Modified Files

| File | Changes |
|------|---------|
| `crates/ub-transport/src/lib.rs` | 添加 `pub mod dedup/read_cache/session/manager/ack` |
| `crates/ub-transport/src/transport.rs` | 删除占位注释 |
| `crates/ub-transport/Cargo.toml` | 添加 dashmap, parking_lot, rand 依赖 |
| `crates/ub-core/src/types.rs` | 添加 PeerChangeEvent, Cqe 增加 src_node 字段 |
| `crates/ub-control/src/control.rs` | 修复 Suspect→Active, PeerChangeEvent 通知, epoch 变化检测 |
| `crates/ub-dataplane/Cargo.toml` | 添加 ub-transport 依赖 |
| `crates/ub-dataplane/src/lib.rs` | TransportManager 集成，重构发送/接收路径 |
| `crates/ub-fabric/src/udp.rs` | 添加 bind_with_loss 丢包注入 |
| `crates/unibusd/src/main.rs` | TransportManager 创建/传递, peer-connector 更新, credit 返还 |

---

## Verification

```bash
cargo test                          # 134+ 单元测试通过
cargo build --release               # 干净构建
bash tests/m1_e2e.sh                # M1 回归 — PASS
bash tests/m2_e2e.sh                # M2 回归 — PASS
bash tests/m3_e2e.sh                # M3 回归 — PASS
bash tests/m4_e2e.sh                # M4 E2E — PASS (含故障检测)
# 1% 丢包注入下端到端测试通过
# 链路失效后操作返回 UB_ERR_LINK_DOWN
# 信用耗尽时发送端正确背压
```

---

## Risk Mitigations

| 风险 | 缓解措施 |
|------|---------|
| 去重 + 写执行记录竞态 | ReliableSession 用 Mutex 保护，mark_executed 在同一锁内完成 |
| 信用饥饿（ACK 丢失） | 周期性 ACK（每 50ms 或每 8 帧）+ 独立 CREDIT 帧 |
| Epoch 变更时 in-flight 操作 | 旧会话 kill → pending_requests 完成 LinkDown → 新会话从 seq=0 开始 |
| M1-M3 回归 | TransportManager 在 0% 丢包 + 充裕信用下行为与 fire-and-forget 等价 |
| Mutex 竞争 | M4 toy 实现可接受；如需优化可拆分 send/receive 锁 + AtomicU64 next_seq |
| ACK 开销 | 每 8 帧 + 50ms 定时 ACK，读请求立即 ACK |