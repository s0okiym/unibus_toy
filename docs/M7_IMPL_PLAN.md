# M7 实施计划：Managed 全局虚拟地址层

**日期**: 2026-04-17
**前置里程碑**: M6（164 单元测试，Verbs 层功能补全）
**目标**: 实现 Managed 层（FR-GVA-1 ~ FR-GVA-6），提供 `ub_alloc`/`ub_free`/`ub_read_va`/`ub_write_va` 全局虚拟地址接口，应用无需持有 NodeID

---

## 0. 总体架构回顾

```
┌───────────────────────────────────────────────────────────────┐
│ Application (3-node KV Cache Demo, §19.2)                     │
├───────────────────────────────────────────────────────────────┤
│ ub-managed                                                     │
│   DeviceProfile Registry · Placer · RegionTable               │
│   Coherence SM (Invalid/Shared/Home) · WriterGuard + Lease    │
│   FetchAgent · CachePool (LRU eviction)                       │
│   SubAllocator (bump allocator within pool MR)                │
├───────────────────────────────────────────────────────────────┤
│ ub-core (verbs: ub_read/write/atomic/send)                    │
├───────────────────────────────────────────────────────────────┤
│ ub-transport (AIMD + RTT + SACK)                               │
├───────────────────────────────────────────────────────────────┤
│ ub-fabric (UDP)                                                │
└───────────────────────────────────────────────────────────────┘
         ▲
         │ ub-control (hub + heartbeat + MR directory)
         │   M7 新增: DEVICE_PROFILE_PUBLISH, ALLOC_REQ/RESP,
         │          REGION_CREATE/DELETE, FETCH/FETCH_RESP,
         │          WRITE_LOCK_REQ/GRANTED/UNLOCK,
         │          INVALIDATE/INVALIDATE_ACK
```

**关键边界规则**（§13.3）:
1. 应用只选一层（verbs 或 managed），不混用
2. Managed 层只调用 verbs 层公开接口，verbs 层不感知 managed 层
3. 每个 region 内部对应 1 个 verbs MR（home 持权威副本，其他 node 持 cache 副本各自注册为本地 MR）
4. 首次 alloc 允许慢（控制面 RPC），稳态数据面直达零拷贝

---

## 1. 子里程碑拆分

依据 DESIGN §21.2，M7 拆为 6 个子里程碑。**M7.3 结束即可停**（弱一致版可演示），M7.6 为完整交付。

| 子 MS | 内容 | 验收 |
|---|---|---|
| **M7.1** | Device capability profile + registry + `DEVICE_PROFILE_PUBLISH` 广播 | `unibusctl device list` 显示所有设备画像 |
| **M7.2** | Placer + `ub_alloc`/`ub_free`；无 cache，读写直达 home | 跨节点 `ub_read_va`/`ub_write_va` 通；应用代码无 NodeID |
| **M7.3** | 本地 read cache + FETCH 协议（无 invalidate，epoch 自判过期） | cache 命中率可观测；写后 reader 重拉新值 |
| **M7.4** | Writer guard + invalidation 协议（SWMR 完成） | §19.2 步骤 5–12 全部通过 |
| **M7.5** | LRU eviction + 本地 cache pool 容量管理 | cache pool 满时正确淘汰，不 OOM |
| **M7.6** | 3 节点 KV cache demo + CLI/metrics 完善 | §19.2 全部验收条件满足 |

---

## 2. 详细步骤

### Step 7.1: Device Capability Profile + Registry

**需求**: FR-GVA-2 — Device/MR 注册需携带静态 capability profile（带宽、时延、容量、tier 标签）。

**当前状态**: `Device` trait 有 `kind()`/`device_id()`/`capacity()` 但无带宽/时延/tier 字段。`ManagedConfig` 已在 config.rs 定义。控制面无 `DEVICE_PROFILE_PUBLISH` 消息。

**改动**:

1. **`crates/ub-core/src/types.rs`**:
   - 添加 `StorageTier` 枚举：`Hot`/`Warm`/`Cold`
   - 添加 `DeviceProfile` 结构体：
     ```rust
     pub struct DeviceProfile {
         pub device_key: (u16, u16),  // (node_id, device_id)
         pub kind: DeviceKind,
         pub tier: StorageTier,
         pub capacity_bytes: u64,
         pub peak_read_bw_mbps: u32,
         pub peak_write_bw_mbps: u32,
         pub read_latency_ns_p50: u32,
         pub write_latency_ns_p50: u32,
         pub used_bytes: u64,
         pub recent_rps: u32,
     }
     ```
   - 静态默认值：Memory → `{tier: Warm, peak_bw: 10000, latency_p50: 200, ...}`，NPU → `{tier: Hot, peak_bw: 50000, latency_p50: 50, ...}`

2. **`crates/ub-core/src/device.rs`**:
   - `Device` trait 添加 `tier() -> StorageTier` 方法（默认实现返回 `StorageTier::Warm`）
   - `MemoryDevice` 返回 `Warm`，`NpuDevice` 返回 `Hot`
   - `Device` trait 添加 `peak_read_bw_mbps() -> u32`、`peak_write_bw_mbps() -> u32`、`read_latency_ns_p50() -> u32`、`write_latency_ns_p50() -> u32`（默认实现提供合理值）

3. **`crates/ub-control/src/message.rs`**:
   - 添加 `CtrlMsgType::DeviceProfilePublish` (0x11)
   - 添加 `DeviceProfilePublishPayload`（Vec of `DeviceProfileEntry`），含编解码
   - `DeviceProfileEntry`：node_id + device_id + kind(u8) + tier(u8) + capacity_bytes + bw/latency 字段 + used_bytes + recent_rps
   - 添加 `ControlMsg::DeviceProfilePublish` 变体

4. **`crates/ub-control/src/control.rs`**:
   - 启动时：扫描本地 device，生成 `DeviceProfile` 列表，向 hub 发送 `DEVICE_PROFILE_PUBLISH`
   - Hub 收到后：存入本地 registry（`HashMap<(NodeId, DeviceId), DeviceProfile>`）
   - Hub 广播 profile 到所有节点
   - 新节点加入（`Join`）时：hub 回复中附带当前所有 profile

5. **`crates/ub-managed/src/lib.rs`**:
   - 添加 `DeviceRegistry` 结构体：
     ```rust
     pub struct DeviceRegistry {
         profiles: RwLock<HashMap<(u16, u16), DeviceProfile>>,
     }
     ```
   - 方法：`register()`/`get()`/`list()`/`update_dynamic()`/`best_device()`

6. **`crates/ub-obs/src/metrics.rs`**:
   - 添加 M7 相关 metrics 描述

7. **`crates/unibusd/src/main.rs`**:
   - `managed.enabled` 为 true 时：启动时注册 pool MR，发送 device profile
   - 添加 `/admin/device/list` endpoint

8. **`crates/unibusctl/src/main.rs`**:
   - 添加 `DeviceList` 子命令

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| `test_storage_tier_roundtrip` | 单元 | StorageTier 枚举编解码 |
| `test_device_profile_entry_roundtrip` | 单元 | DeviceProfileEntry 编解码 |
| `test_device_profile_publish_roundtrip` | 单元 | DEVICE_PROFILE_PUBLISH 控制面消息 roundtrip |
| `test_device_registry_register_and_list` | 单元 | 注册 profile 后可查询 |
| `test_device_registry_best_device_selection` | 单元 | best_device 按 tier/容量选最优 |
| `test_npu_device_tier_is_hot` | 单元 | NPU tier = Hot |
| `test_memory_device_tier_is_warm` | 单元 | Memory tier = Warm |

**验收**: 7 个单元测试通过；`unibusctl device list` 在 3 节点 E2E 中显示所有设备画像。

---

### Step 7.2: Placer + ub_alloc/ub_free（无 cache，读写直达 home）

**需求**: FR-GVA-1 (ub_alloc/ub_free) + FR-GVA-3 (home 选择) + FR-GVA-5 (生命周期由 UB 管理)。

**当前状态**: 无 `UbVa` 类型、无 Placer、无 RegionTable、无 `ALLOC_REQ/RESP`/`REGION_CREATE/DELETE` 控制面消息。

**改动**:

1. **`crates/ub-core/src/types.rs`**:
   - 添加 `UbVa(u128)` 类型，内部编码 `[region_id:64 | offset:64]`
   - 添加 `RegionId(u64)` 类型
   - 添加 `AllocHints` 结构体（access、latency_class、capacity_class、pin、expected_readers）
   - 添加 `AccessPattern`/`LatencyClass`/`CapacityClass` 枚举
   - 添加 `RegionState` 枚举：`Invalid`/`Shared(epoch: u64)`/`Home`
   - 添加 `RegionInfo` 结构体（region_id, home_node_id, device_id, mr_handle, base_offset, len, epoch, state）

2. **`crates/ub-control/src/message.rs`**:
   - 添加控制面消息类型：
     - `ALLOC_REQ` (0x12): region_id, size, hints, requester_node_id
     - `ALLOC_RESP` (0x13): region_id, ub_va, home_node_id, mr_handle, base_offset, error_code
     - `REGION_CREATE` (0x14): region_id, size, device_kind（placer → home node）
     - `REGION_CREATE_OK` (0x15): region_id, mr_handle, base_offset（home node → placer）
     - `REGION_DELETE` (0x16): region_id（placer → home node + readers）
   - 各 payload 编解码实现
   - `ControlMsg` 添加对应变体

3. **`crates/ub-managed/src/lib.rs`**:
   - 添加 `Placer` 结构体：
     ```rust
     pub struct Placer {
         registry: Arc<DeviceRegistry>,
         regions: RwLock<HashMap<RegionId, RegionPlacement>>,
         next_region_id: AtomicU64,
         config: ManagedConfig,
     }
     ```
   - `RegionPlacement`: region_id, home_node_id, device_id, mr_handle, base_offset, len, requester_node_id
   - `place()` 方法：cost function 选 device → 发 `REGION_CREATE` → 记录映射 → 返回 UbVa
   - Cost function:
     ```
     score(d) = w_lat * latency_estimate(d, requester, access)
              + w_cap * (1 - free_ratio(d))
              + w_tier * tier_mismatch(hints.latency_class, d.tier)
              + w_load * d.recent_rps / d.peak_rps
     ```
   - `free()` 方法：发 `REGION_DELETE` → 清理映射

   - 添加 `SubAllocator` 结构体（在大 pool MR 内部做 sub-allocation）：
     ```rust
     pub struct SubAllocator {
         base_offset: u64,
         total_len: u64,
         allocated: AtomicU64,  // bump allocator
     }
     ```
   - `alloc(size) -> Option<u64>`：bump 分配，返回偏移
   - `free(offset, size)`：首版为 no-op（bump 不回收），accept 简化

   - 添加 `RegionTable` 结构体（每个 node 本地的 region 视图）：
     ```rust
     pub struct RegionTable {
         entries: RwLock<HashMap<RegionId, RegionEntry>>,
     }
     ```
   - `RegionEntry`: region_id, state(RegionState), home_node_id, mr_handle, base_offset, len, local_mr_handle(Option)

   - 添加 Managed Layer API（async 封装）：
     ```rust
     pub async fn ub_alloc(size: u64, hints: AllocHints) -> Result<UbVa, UbError>;
     pub async fn ub_free(va: UbVa) -> Result<(), UbError>;
     pub async fn ub_read_va(va: UbVa, offset: u64, buf: &mut [u8]) -> Result<(), UbError>;
     pub async fn ub_write_va(va: UbVa, offset: u64, buf: &[u8]) -> Result<(), UbError>;
     ```

4. **`crates/unibusd/src/main.rs`**:
   - `managed.enabled` 为 true 时：
     - 启动时预注册 pool MR（按 pool.memory_reserve_ratio / npu_reserve_ratio）
     - 创建 `DeviceRegistry`、`Placer`、`RegionTable`
     - 处理 `ALLOC_REQ`/`REGION_CREATE`/`REGION_DELETE` 控制面消息
   - 添加 `/admin/region/alloc` endpoint（对应 ub_alloc）
   - 添加 `/admin/region/free` endpoint（对应 ub_free）
   - 添加 `/admin/region/list` endpoint
   - 添加 `/admin/verb/read-va` endpoint（对应 ub_read_va）
   - 添加 `/admin/verb/write-va` endpoint（对应 ub_write_va）

5. **`crates/unibusctl/src/main.rs`**:
   - 添加 `RegionAlloc`/`RegionFree`/`RegionList` 子命令
   - 添加 `VaRead`/`VaWrite` 子命令

6. **`crates/ub-managed/Cargo.toml`**:
   - 添加 `ub-dataplane` 依赖（ub_read_va/ub_write_va 需要调用 data plane）

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| `test_ub_va_encoding` | 单元 | UbVa 的 region_id/offset 编解码 roundtrip |
| `test_alloc_hints_default` | 单元 | AllocHints 默认值合理 |
| `test_placement_cost_function` | 单元 | cost function 对不同 tier/容量/负载给出正确排序 |
| `test_placement_prefers_local_device` | 单元 | 同 tier 下优先选请求方本节点（latency_estimate 最低） |
| `test_placement_respects_pin_hint` | 单元 | pin=Npu 时只选 NPU 设备 |
| `test_sub_allocator_bump` | 单元 | 连续 alloc 返回不重叠偏移 |
| `test_sub_allocator_exhaustion` | 单元 | 空间不足返回 None |
| `test_region_table_crud` | 单元 | insert/lookup/remove 正常工作 |
| `test_region_state_transitions` | 单元 | Invalid→Home/Home→Invalid 状态转移 |
| `test_alloc_req_resp_roundtrip` | 单元 | ALLOC_REQ/ALLOC_RESP 编解码 roundtrip |
| `test_region_create_delete_roundtrip` | 单元 | REGION_CREATE/DELETE 编解码 roundtrip |

**验收**: 11 个单元测试通过；E2E: 2 节点 managed.enabled=true，ub_alloc → 跨节点 ub_read_va/ub_write_va 成功。

---

### Step 7.3: 本地 Read Cache + FETCH 协议（无 invalidate，epoch 自判过期）

**需求**: FR-GVA-4 (M7.3 阶段) — 可选本地 read cache + FETCH，无 invalidate，reader 用 epoch 自判过期。

**当前状态**: 无 FETCH/FETCH_RESP 控制面消息，无 CachePool，无 epoch 自判逻辑。

**改动**:

1. **`crates/ub-control/src/message.rs`**:
   - 添加控制面消息类型：
     - `FETCH` (0x17): region_id, offset, len, requester_node_id
     - `FETCH_RESP` (0x18): region_id, epoch, payload, error_code
   - 编解码实现
   - `ControlMsg` 添加对应变体

2. **`crates/ub-managed/src/lib.rs`**:
   - 添加 `CachePool` 结构体：
     ```rust
     pub struct CachePool {
         mr_handle: u32,       // pool MR handle（启动时注册）
         allocator: SubAllocator,
         entries: RwLock<HashMap<RegionId, CacheEntry>>,
         max_bytes: u64,
         used_bytes: AtomicU64,
     }
     ```
   - `CacheEntry`: region_id, pool_offset, len, epoch, last_access(Instant)
   - `fetch_and_cache()`: 向 home 发 FETCH → 存入 pool → 返回
   - `lookup()`: 本地缓存命中 → 返回；miss → 触发 fetch_and_cache
   - `evict_one()`: 超容量时 LRU 淘汰（简化：首版仅标记释放，不回收 SubAllocator）

   - 添加 `FetchAgent` 结构体：
     ```rust
     pub struct FetchAgent {
         region_table: Arc<RegionTable>,
         cache_pool: Arc<CachePool>,
         in_flight: Mutex<HashMap<RegionId, Arc<Notify>>>,  // 并发 FETCH 去重
     }
     ```
   - `read_va()`: lookup local cache → hit → read; miss → FETCH → cache → read
   - 并发去重：同一 region 的多个 task miss 时，只发一个 FETCH，其余挂 Notify 等待

   - 修改 `ub_read_va()`:
     - 本地 Shared → 直接从 cache MR 读取
     - 本地 Invalid → FetchAgent.fetch_and_cache() → 读取
     - 本地 Home → 直接从 home MR 读取

   - 修改 `ub_write_va()`（M7.2 直达 home 模式不变，无一致性协议）

   - 添加 epoch 自判逻辑：
     - 读取时，若 Shared 的 epoch < home 已知最新 epoch，自动重新 FETCH

3. **`crates/unibusd/src/main.rs`**:
   - `managed.enabled` 为 true 时：注册 cache pool MR，创建 CachePool/FetchAgent
   - 处理 `FETCH` 控制面消息：home node 从本地 MR 读取数据，回复 FETCH_RESP
   - 修改 `/admin/verb/read-va` endpoint：通过 FetchAgent 读取

4. **`crates/ub-obs/src/metrics.rs`**:
   - 添加 `unibus_region_cache_hit_total` 计数器
   - 添加 `unibus_region_cache_miss_total` 计数器
   - 添加 `unibus_region_count{state=home|shared|invalid}` gauge

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| `test_fetch_payload_roundtrip` | 单元 | FETCH/FETCH_RESP 编解码 roundtrip |
| `test_cache_pool_insert_and_lookup` | 单元 | 插入后可 lookup |
| `test_cache_pool_miss` | 单元 | 不存在的 region 返回 None |
| `test_cache_pool_eviction` | 单元 | 超容量时 LRU 淘汰最久未访问 |
| `test_fetch_agent_dedup` | 单元 | 并发 FETCH 同一 region 只发一次 |
| `test_epoch_self_check` | 单元 | epoch 过期时自动重新 FETCH |
| `test_region_state_invalid_to_shared` | 单元 | FETCH 成功后 state = Shared |

**验收**: 7 个单元测试通过；E2E: N2 读 region → miss → FETCH → Shared → 二次读 hit（延迟显著降低）。

---

### Step 7.4: Writer Guard + Invalidation 协议（SWMR 完成）

**需求**: FR-GVA-4 (M7.4 阶段) — 写入需获得写锁并向其他 cache 发送 invalidate。

**当前状态**: 无 `WRITE_LOCK_REQ/GRANTED/UNLOCK`/`INVALIDATE/INVALIDATE_ACK` 消息，无 `WriterGuard`，无 `RegionHomeState`。

**改动**:

1. **`crates/ub-control/src/message.rs`**:
   - 添加控制面消息类型：
     - `WRITE_LOCK_REQ` (0x19): region_id, writer_node_id
     - `WRITE_LOCK_GRANTED` (0x1A): region_id, epoch, error_code
     - `WRITE_UNLOCK` (0x1B): region_id, writer_node_id
     - `INVALIDATE` (0x1C): region_id, new_epoch
     - `INVALIDATE_ACK` (0x1D): region_id
   - 编解码实现
   - `ControlMsg` 添加对应变体

2. **`crates/ub-managed/src/lib.rs`**:
   - 添加 `RegionHomeState` 结构体：
     ```rust
     pub struct RegionHomeState {
         mr_handle: u32,
         base_offset: u64,
         len: u64,
         epoch: AtomicU64,
         readers: RwLock<HashSet<u16>>,     // NodeId set
         writer: RwLock<Option<WriterInfo>>, // 当前 writer + lease deadline
     }
     pub struct WriterInfo {
         writer_node_id: u16,
         lease_deadline: Instant,
     }
     ```

   - 添加 `WriterGuard` 结构体：
     ```rust
     pub struct WriterGuard {
         va: UbVa,
         region_id: RegionId,
         home_node_id: u16,
         managed: Arc<ManagedLayer>,
     }
     ```
   - Drop 时自动发 `WRITE_UNLOCK`

   - 添加 `CoherenceManager` 结构体：
     ```rust
     pub struct CoherenceManager {
         home_states: RwLock<HashMap<RegionId, RegionHomeState>>,
         writer_lease_ms: u64,
     }
     ```
   - `acquire_writer()`: 发 WRITE_LOCK_REQ → home invalidate 所有 readers → 收齐 ACK → 授权
   - `release_writer()`: 发 WRITE_UNLOCK → home 清除 writer
   - `handle_invalidate()`: reader 收到 INVALIDATE → 本地 Shared → Invalid，回 ACK
   - `check_lease_expiry()`: 定期检查 writer lease，超时自动释放

   - 添加 `ub_acquire_writer()` API
   - 修改 `ub_write_va()`: 必须持有 WriterGuard 才能写

3. **`crates/unibusd/src/main.rs`**:
   - 添加 `/admin/verb/acquire-writer` endpoint
   - 添加 `/admin/verb/sync` endpoint（对应 ub_sync）
   - 处理 `WRITE_LOCK_REQ`/`WRITE_UNLOCK`/`INVALIDATE`/`INVALIDATE_ACK` 控制面消息
   - 启动 writer lease 检查定时器

4. **`crates/ub-obs/src/metrics.rs`**:
   - 添加 `unibus_invalidate_sent_total` 计数器
   - 添加 `unibus_write_lock_wait_ms` histogram

5. **`crates/unibusctl/src/main.rs`**:
   - 添加 `AcquireWriter`/`ReleaseWriter` 子命令

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| `test_write_lock_req_roundtrip` | 单元 | WRITE_LOCK_REQ/GRANTED 编解码 roundtrip |
| `test_invalidate_roundtrip` | 单元 | INVALIDATE/INVALIDATE_ACK 编解码 roundtrip |
| `test_coherence_acquire_writer_no_readers` | 单元 | 无 readers 时直接授权 |
| `test_coherence_acquire_writer_with_readers` | 单元 | 有 readers 时先 invalidate 再授权 |
| `test_coherence_invalidate_state_transition` | 单元 | Shared → Invalid on INVALIDATE |
| `test_coherence_writer_lease_expiry` | 单元 | lease 超时后 writer 自动释放 |
| `test_writer_guard_drop_sends_unlock` | 单元 | WriterGuard drop 触发 WRITE_UNLOCK |
| `test_concurrent_acquire_queues` | 单元 | 多 writer 串行排队 |

**验收**: 8 个单元测试通过；E2E: §19.2 步骤 5–12 全部通过。

---

### Step 7.5: LRU Eviction + Cache Pool 容量管理

**需求**: FR-GVA-4 (缓存容量管理与逐出策略由 UB 负责，对应用透明)。

**当前状态**: Step 7.3 的 CachePool 有基础 eviction，但缺少完善的 LRU 数据结构和容量管理。

**改动**:

1. **`crates/ub-managed/src/lib.rs`**:
   - 重构 `CachePool` 的 LRU 实现：
     ```rust
     pub struct LruEntry {
         region_id: RegionId,
         pool_offset: u64,
         len: u64,
         epoch: u64,
         last_access: Instant,
     }
     ```
   - 使用 `LinkedHashMap` 风格（或 `IndexMap` + 手动 LRU list）实现 O(1) 的 access/evict
   - `evict_if_needed()`: 插入前检查 used_bytes + entry.len > max_bytes → LRU 淘汰
   - eviction 时：将 region state 改为 Invalid，释放 SubAllocator 空间（bump allocator 下为 no-op，但更新 used_bytes）
   - 淘汰不通知 home（home 的 readers set 允许 stale）

2. **`crates/unibusd/src/main.rs`**:
   - 修改 read-va 流程：每次 cache hit 时更新 LRU 访问时间
   - 修改 fetch_and_cache 流程：插入前调用 evict_if_needed

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| `test_lru_eviction_order` | 单元 | 淘汰顺序为最久未访问 |
| `test_lru_access_updates_order` | 单元 | 访问后条目不被淘汰 |
| `test_cache_pool_capacity_enforcement` | 单元 | 超容量时自动淘汰 |
| `test_cache_pool_eviction_invalidates_region` | 单元 | 淘汰后 region state = Invalid |
| `test_cache_pool_zero_max_bytes` | 单元 | max_bytes=0 时所有读都走 FETCH（无缓存） |

**验收**: 5 个单元测试通过；E2E: 小容量 cache pool 下，反复读多个 region 时缓存正确淘汰和重新拉取。

---

### Step 7.6: 3 节点 KV Cache Demo + CLI/Metrics 完善

**需求**: §19.2 全部验收条件满足。

**当前状态**: 需要集成测试脚本和 demo 配置。

**改动**:

1. **`configs/`**:
   - 添加 M7 三节点配置文件：`m7_node0.yaml`（hub + placer）、`m7_node1.yaml`（writer）、`m7_node2.yaml`（reader）
   - 每个配置设置 `managed.enabled: true`，配置 pool/cache/cost_weights

2. **`tests/m7_e2e.sh`**:
   - 完整 12 步 §19.2 demo 脚本：
     1. 启动 3 个 unibusd
     2. 等待集群形成
     3. unibusctl device list → 验证画像
     4. N1 ub_alloc → 获取 UbVa
     5. N1 acquire_writer → 授权
     6. N1 write_va → 写 payload
     7. N1 release_writer
     8. N2 read_va → miss → FETCH → 读到数据
     9. N2 二次 read_va → cache hit（验证延迟降低）
     10. N1 再 acquire_writer → N2 收 INVALIDATE → Invalid
     11. N1 写新 payload、release
     12. N2 再 read_va → miss → FETCH → 读到新数据
   - 验证条件：
     - 应用代码 grep NodeId = 0 命中
     - `unibusctl region list` 显示 region 信息
     - `/metrics` 包含 M7 counters
     - N2 第 2 次读延迟 < 第 1 次读的 1/5

3. **`crates/ub-obs/src/metrics.rs`**:
   - 添加 `unibus_placement_decision_total{tier=...}` counter

4. **`crates/unibusctl/src/main.rs`**:
   - 完善 `RegionList` 输出：显示 region_id、home_node、readers、epoch、state
   - 完善 `DeviceList` 输出：显示 tier、capacity、bandwidth、latency

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| m7_e2e.sh 全 12 步 | E2E | §19.2 场景完整通过 |
| 故障: kill N2 持锁中 | 故障注入 | N0/N1 正常；重启 N2 → re-FETCH 拿最新 |
| 故障: kill N1 持锁中 | 故障注入 | N0 lease 超时释放；后续 acquire 成功 |
| 故障: kill N0 (placer) | 故障注入 | 新 ub_alloc 失败；现有 region 读写继续 |
| 性能: cache hit vs miss | 性能 | 本地 Shared hit 延迟 < 首次 FETCH 的 1/5 |

**验收**: E2E 12 步 + 3 个故障注入场景通过；性能指标达标。

---

## 3. 控制面消息 ID 分配

| ID | 消息类型 | 来源 | Step |
|---|---|---|---|
| 0x01–0x10 | 已分配（M1–M6） | — | — |
| 0x11 | `DEVICE_PROFILE_PUBLISH` | 任意 node → hub | 7.1 |
| 0x12 | `ALLOC_REQ` | 任意 node → placer | 7.2 |
| 0x13 | `ALLOC_RESP` | placer → requester | 7.2 |
| 0x14 | `REGION_CREATE` | placer → home node | 7.2 |
| 0x15 | `REGION_CREATE_OK` | home node → placer | 7.2 |
| 0x16 | `REGION_DELETE` | placer → home node | 7.2 |
| 0x17 | `FETCH` | reader → home node | 7.3 |
| 0x18 | `FETCH_RESP` | home node → reader | 7.3 |
| 0x19 | `WRITE_LOCK_REQ` | writer → home node | 7.4 |
| 0x1A | `WRITE_LOCK_GRANTED` | home node → writer | 7.4 |
| 0x1B | `WRITE_UNLOCK` | writer → home node | 7.4 |
| 0x1C | `INVALIDATE` | home node → readers | 7.4 |
| 0x1D | `INVALIDATE_ACK` | reader → home node | 7.4 |

---

## 4. 依赖关系

```
Step 7.1 (Device Profile) ──── 无依赖（基础设施）
Step 7.2 (Placer + ub_alloc) ── 依赖 7.1（DeviceRegistry）
Step 7.3 (Cache + FETCH) ────── 依赖 7.2（RegionTable + Placer）
Step 7.4 (Writer Guard) ─────── 依赖 7.3（CachePool + FETCH）
Step 7.5 (LRU Eviction) ─────── 依赖 7.3（CachePool）
Step 7.6 (Demo + CLI) ───────── 依赖 7.4 + 7.5

Step 7.1 → 7.2 必须串行（7.2 需要 DeviceRegistry）
Step 7.3 → 7.4 必须串行（7.4 需要 CachePool）
Step 7.5 可与 7.4 并行开发，但集成测试需 7.4 完成
Step 7.6 必须最后
```

建议开发顺序：**7.1 → 7.2 → 7.3 → 7.4 → 7.5 → 7.6**

---

## 5. 文件变更预估

| 文件 | Step | 改动量 |
|------|------|-------|
| `crates/ub-core/src/types.rs` | 7.1, 7.2 | 大（UbVa、RegionId、AllocHints、StorageTier、DeviceProfile、RegionState） |
| `crates/ub-core/src/device.rs` | 7.1 | 小（添加 tier/bw/latency 默认方法） |
| `crates/ub-core/src/device/memory.rs` | 7.1 | 小（实现 tier 方法） |
| `crates/ub-core/src/device/npu.rs` | 7.1 | 小（实现 tier 方法） |
| `crates/ub-control/src/message.rs` | 7.1, 7.2, 7.3, 7.4 | 大（14 个新消息类型 + payload） |
| `crates/ub-control/src/control.rs` | 7.1, 7.2, 7.3, 7.4 | 大（所有新消息的处理逻辑） |
| `crates/ub-managed/src/lib.rs` | 7.1–7.5 | 很大（DeviceRegistry、Placer、SubAllocator、RegionTable、CachePool、FetchAgent、CoherenceManager、WriterGuard、Managed Layer API） |
| `crates/ub-managed/Cargo.toml` | 7.2 | 小（添加 ub-dataplane 依赖） |
| `crates/ub-obs/src/metrics.rs` | 7.1, 7.3, 7.4, 7.6 | 小 |
| `crates/unibusd/src/main.rs` | 7.1–7.6 | 大（新 endpoint、managed 集成、控制面消息处理） |
| `crates/unibusctl/src/main.rs` | 7.1, 7.2, 7.4, 7.6 | 中（新子命令） |
| `configs/m7_node*.yaml` | 7.6 | 小 |
| `tests/m7_e2e.sh` | 7.6 | 中 |

---

## 6. 设计决策固化

依据 DESIGN §22.2 的开放问题，首版决策如下：

| # | 问题 | 决策 |
|---|---|---|
| V1 | UbVa 编码 | `[region_id:64 \| offset:64]`，region_id 高 16 bit 保留（置 0） |
| V2 | Placer 故障 | 单点，hub 挂 = managed 层不可用；verbs 层不受影响 |
| V3 | Region 上限 | 首版 1 GiB；更大需求走应用分片 |
| V4 | Readers set 持久化 | 不持久化；placer 重启丢失，reader 下次 FETCH 重新加入 |
| V5 | acquire_writer 排队 | 默认排队 + 5s lease；首版不实现 try_acquire |
| V6 | Pool 预留比例 | NPU 50% / CPU 25%，配置可调 |
| V7 | Hub 热点 | 首版允许，验收 demo 里观察 |
| V8 | MR 注销 | 不修改 verbs 层；managed 层 region 在大 MR 内 sub-allocate，verbs MR 永不注销 |
| V9 | Cache 暴露给 verbs | 不暴露；边界规则"只选一层" |
| V10 | NPU cache 访问 | 允许（NPU 是模拟字节数组） |

---

## 7. 验收总标准

- **Step 7.1–7.5 每步完成后**：该步新增单元测试全部通过
- **Step 7.6 完成后**：
  - 单元测试总数 >= 200
  - §19.2 12 步 E2E 全部通过
  - 应用代码 grep NodeId = 0 命中
  - `unibusctl region list` 正确显示 region/home/readers/epoch
  - `unibusctl device list` 正确显示设备画像
  - `/metrics` 包含所有 M7 计数器
  - N2 cache hit 延迟 < 首次 FETCH 的 1/5
  - Writer 持锁中 kill → lease 超时释放
  - M1–M6 全部 164 个现有测试不受影响