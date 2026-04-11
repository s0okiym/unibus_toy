# UniBus Toy Vision：Managed Memory Layer

> 在现有 verbs 层之上叠加一个「分布式 OS 风格」的内存管理层，让应用完全不感知 NodeID。
> 对标物：CXL 3.0 pooled memory + coherency 的软件 toy 版、FaRM + placer、Alluxio 的内存语义版本。

| 项 | 值 |
|---|---|
| **文档状态** | Draft / **Exploratory**（vision，未排入 M1–M5 主线） |
| **最后更新** | 2026-04-11 |
| **前置文档** | [REQUIREMENTS.md](./REQUIREMENTS.md)、[DESIGN.md](./DESIGN.md) |
| **定位** | 叠在现有 verbs 层之上的独立 track（M7），不推翻任何已对齐的 FR |

---

## 0. TL;DR

把 SuperPod 内所有 MR（CPU 内存、NPU 模拟显存、未来的 CXL/SSD）组织成**一个带静态性能画像的全局堆**。应用调用 `ub_alloc(size, hints)` 拿到不透明 `ub_va_t`，**代码里再也不出现 NodeID**——UB 内部由中心 placer 按「调用者位置 + device 画像 + 应用 hint」选放置位置，并通过 region 级 cache + 单写多读 invalidation 协议维护一致性。KV cache / 模型权重这类「多读单写 + 异构分层」负载是这套机制的原生适配场景。

---

## 1. 动机：为什么现有 verbs 层不够

现有 `ub_mr_register + ub_read/write + ub_atomic_*`（REQUIREMENTS §5.2–5.3）本质是 IB verbs 的翻版：

- **应用 node-aware**：必须知道 `owner_node`、自己选 device、自己决定数据放 CPU 还是 NPU。
- **手动生命周期**：应用自己 `malloc` → `register` → 算 UB 地址 → 广播给 peer → 用完 `dereg`。
- **拓扑变化应用要管**：节点上下线、device 画像变化、热点迁移，全部由应用代码响应。

而 AI 训练/推理里最典型的两类数据——**KV cache** 和 **模型权重**——的访问模式高度一致：

- 大对象，字节级随机访问
- **单一 writer**（prefill / update 阶段）、**多个 reader**（decode / attention 阶段）
- 读者分布在多个 node，延迟敏感
- 存储天然分层：HBM（小而快）< DRAM（中）< CXL / NVMe（大而慢），数据应按热度流动

让应用代码直接管这些事情是不合理的——就像让应用程序决定"这个变量存在哪条 DIMM 上"一样。传统 OS 用**虚拟内存 + page cache + NUMA 调度**把这件事封装了；分布式场景需要对应的一层。

---

## 2. 一句话定义

> **UB Managed Memory Layer** = 把 SuperPod 内所有 verbs 层 MR 抽象成**一个全局堆**，通过**中心 placer**做首次放置、通过 **region 级 single-writer-multi-reader 缓存协议**做多点读取，让应用用不透明 `ub_va_t` 访问数据而**无需知道 NodeID / DeviceID / 拓扑**。

---

## 3. 和现有设计的分层关系

**原则：叠层，不替换**。底层 verbs 照 REQUIREMENTS/DESIGN 继续做，本层只在上面加调度与缓存。

```
┌──────────────────────────────────────────────────────┐
│ Application (KV cache demo, 模型权重 serving, …)    │
│  ── 只见 ub_va_t，代码里没有 NodeID                 │
├──────────────────────────────────────────────────────┤
│ ub-managed                       （本文新增，M7）    │
│  ├─ Global allocator & placer                        │
│  ├─ Device capability registry                       │
│  ├─ Region table + coherence state machine           │
│  └─ Cache tier controller / LRU eviction             │
├──────────────────────────────────────────────────────┤
│ ub-core verbs (M2/M3, 已对齐)                        │
│  ── ub_mr_register / ub_read / ub_write / ub_atomic  │
│  ── Device trait (memory / npu)                      │
├──────────────────────────────────────────────────────┤
│ ub-transport / ub-fabric (M4, 已对齐)                │
└──────────────────────────────────────────────────────┘
```

**边界规则**（首版）：

1. **应用只选一层**。使用 managed 层的应用**不得**持有 verbs 层 `UB Address`；要混用留到后续里程碑。
2. Managed 层对下**只调用** verbs 层的公开接口；verbs 层**不感知** managed 层存在，保持可独立测试。
3. Managed 层的每个 region 在内部一定对应 1 个或多个 verbs 层 MR（可能分布在多个 node 上——home 有权威副本，其他 node 有 cache 副本各自注册为本地 MR）。
4. Managed 层的 **首次 alloc 性能** 允许慢（走控制面 RPC），但**稳态数据面**（本地 cache 命中）必须直达 verbs 层零拷贝路径。

---

## 4. 核心概念

| 概念 | 说明 | 类比 |
|---|---|---|
| **`ub_va_t`** | 全局逻辑地址，`u128` 不透明 handle，**不编码 NodeID**；内部是 `[region_id:64 \| offset:64]` | Plan 9 全局 namespace / CUDA UM 指针 |
| **Region** | 一次 `ub_alloc` 的粒度；迁移、cache、失效都以 region 为单位；首版不做 page 级 | RADOS object / Alluxio block |
| **Device Capability Profile** | 每个 verbs 层 device 的静态 + 动态元数据：带宽、时延、容量、tier、当前利用率 | NUMA distance matrix |
| **Placement Policy** | `ub_alloc` 时 placer 用 cost function 选 home device | OS 调度器 / 数据库 optimizer |
| **Home Node** | 一个 region 的权威副本所在 node；所有写操作直达 home | Directory-based DSM home |
| **Cache Replica** | Reader 本地的只读副本，按需从 home 拉；本地 LRU 淘汰 | CDN edge / CPU L2 cache |
| **Coherence Protocol** | 首版：**Single-Writer Multi-Reader + write-through + invalidation**；写前 writer 锁住 home，home 批量 invalidate 所有 reader | 简化 MESI 的 M/S/I 三态 |
| **Writer Guard** | 应用显式 `ub_acquire_writer(va)` 拿到的独占写许可；带 lease，崩溃超时自动释放 | 分布式锁 |
| **Epoch** | 每次 writer release 时 home 单调递增；reader 副本带 epoch，invalidate 用 epoch 区分新旧 | Raft term / Lamport 版本号 |
| **Migration** | Home 节点迁移（冷数据下沉、热数据上浮）；首版**不做**，留到 M7 后续 | LSM compaction |

---

## 5. 应用层 API sketch

> 只是为了把语义对齐清楚，不是最终签名；具体签名等 M7 启动时再定。

```rust
// ---------- 句柄 ----------

/// 全局虚拟地址。对应用不透明，内部 u128。
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct UbVa(u128);

// ---------- 分配 hint ----------

pub enum AccessPattern {
    ReadMostly,            // KV cache 权重类
    SingleWriterReadOften, // KV cache decode 段
    Scratch,               // 临时缓冲，不需跨节点一致
}

pub enum LatencyClass  { Critical, Normal, Bulk }
pub enum CapacityClass { Small, Large, Huge }

pub struct AllocHints {
    pub access: AccessPattern,
    pub latency_class: LatencyClass,
    pub capacity_class: CapacityClass,
    /// 逃生门：强制绑到指定 device 类型（FR 第 4 点）。
    /// 首版允许 `Some(DeviceKind::Npu)` / `Some(DeviceKind::Memory)`。
    pub pin: Option<DeviceSelector>,
    /// 可选：预告 reader 集合，让 placer 更好地选 home 位置
    pub expected_readers: Option<Vec<NodeId>>,
}

// ---------- 分配 / 释放 ----------

pub async fn ub_alloc(size: u64, hints: AllocHints) -> Result<UbVa, UbError>;
pub async fn ub_free(va: UbVa)                      -> Result<(), UbError>;

// ---------- 读写 ----------

/// 通用读：本地若有 Shared 副本则本地命中；否则触发 FETCH。
pub async fn ub_read_va (va: UbVa, offset: u64, buf: &mut [u8]) -> Result<(), UbError>;

/// 通用写：必须先 acquire_writer；写路径直达 home，顺带 invalidate。
pub async fn ub_write_va(va: UbVa, offset: u64, buf: &[u8])     -> Result<(), UbError>;

/// 快速路径：本地已有副本时借出指针（零拷贝）；否则返回 None。
pub fn ub_try_local_map(va: UbVa, perms: Perms) -> Result<Option<LocalView<'_>>, UbError>;

// ---------- 一致性同步 ----------

/// 显式 acquire writer 锁；drop guard = release，期间任意 ub_write_va 被允许。
/// 多个 acquire_writer 在同一 region 上串行化；lease 默认 5s。
pub async fn ub_acquire_writer(va: UbVa) -> Result<WriterGuard, UbError>;

/// Release 一致性的同步点：保证此前所有本地 ub_write_va 对其他 reader 变成可见。
/// Drop WriterGuard 时自动调用，一般应用不需要直接用。
pub async fn ub_sync(va: UbVa) -> Result<(), UbError>;
```

**要点**：

- `ub_alloc` 异步，因为要走控制面 RPC 到 placer。稳态数据面 `ub_read_va` / `ub_write_va` 尽量 fast path。
- `ub_try_local_map` 是给 "应用想自己用裸指针访问" 的逃生门，只在本地已有 `Shared` 副本时成功；配合 zero-copy 场景。
- 应用代码里**没有任何 NodeID**。`expected_readers` 是唯一可选的拓扑 hint，并且是软提示。

---

## 6. 内部机制

### 6.1 Device Capability Registry

每个 node 启动时，扫描本地所有 verbs device（CPU memory / 模拟 NPU / 未来的 CXL），生成 profile：

```rust
pub struct DeviceProfile {
    pub device_key: (NodeId, DeviceId),   // 内部用，不暴露给应用
    pub kind: DeviceKind,                  // Memory | Npu | ...
    pub tier: StorageTier,                 // Hot | Warm | Cold
    pub capacity_bytes: u64,
    pub peak_read_bw_mbps:   u32,
    pub peak_write_bw_mbps:  u32,
    pub read_latency_ns_p50: u32,
    pub write_latency_ns_p50: u32,
    // 动态部分（heartbeat piggyback 更新）
    pub used_bytes: u64,
    pub recent_rps: u32,
}
```

- **静态字段来源**：首版写死静态表（CPU memory 一组默认值、模拟 NPU 一组默认值——反正是 toy，不需要真去 bench）。未来可以启动时做短暂 micro-bench 自动填。
- **广播**：通过 `DEVICE_PROFILE_PUBLISH` 控制面消息（和 `MR_PUBLISH` 同级，见 DESIGN §7.2）广播到 placer。
- **动态字段**：piggyback 在心跳里，周期 1s。

### 6.2 中心 Placer

- **部署**：placer 是 **hub node** 上的一个 tokio task（复用 DESIGN §7.1 的 hub 基础设施，单点）。非 hub node 通过控制面 TCP 连接 placer。
- **首版不做 failover**：hub 挂掉 = managed 层不可用；主线 verbs 层仍可工作。
- **`ub_alloc` 流程**：
  1. 应用 local agent → 发 `ALLOC_REQ(size, hints, requester_node)` 到 placer。
  2. Placer 查 device registry，跑 cost function：
     ```
     score(d) = w_lat  * latency_estimate(d, requester_node, hints.access)
              + w_cap  * (1 - free_ratio(d))          # 容量压力
              + w_tier * tier_mismatch(hints.latency_class, d.tier)
              + w_load * d.recent_rps / d.peak_rps    # 动态负载
     ```
     权重系数放配置；选 score 最小者。
  3. Placer 向选中的 home node 发 `REGION_CREATE(region_id, size)`；home node 在本地 pool（见下）里切出一段 verbs 层 MR，回 `REGION_CREATE_OK(mr_handle, base_offset)`。
  4. Placer 记录 `region_id → home_node/mr_handle/base_offset`，回 `ALLOC_RESP(ub_va)` 给 requester。
- **本地 pool**：每个 node 启动时对每个 device **预先 `ub_mr_register` 一个大 MR**（比如 NPU 的 1/2 容量、CPU 的 256 MiB），managed 层后续的 region 都是在这个大 MR 内部做 sub-allocation（free-list / bump allocator）。好处：避免每次 alloc 都做一次控制面 `MR_PUBLISH` 广播；代价：device 用量上限受预留影响。首版接受这个取舍。

### 6.3 Coherence 协议（SWMR + write-through + invalidate）

**Region 状态**（每个副本持有方本地记录）：

| 状态 | 含义 |
|---|---|
| `Invalid` | 本地无数据 |
| `Shared` | 本地有只读副本（带 epoch） |
| `Home` | 本节点是 home，持有权威副本；当前可能有也可能没有 writer guard |

**Home 节点额外维护**：

```rust
struct RegionHomeState {
    mr_handle: u32,
    base_offset: u64,
    len: u64,
    epoch: u64,                       // 单调递增，writer release 时 +1
    readers: HashSet<NodeId>,         // 持有 Shared 副本的 node
    writer: Option<(NodeId, Instant)>,// 当前 writer + lease deadline
}
```

**5 个协议事件**：

1. **Reader 首次 `ub_read_va`，本地 `Invalid`**：
   - Local agent 查本地 region table → miss → 向 placer 查 `region_id → home`（可缓存）。
   - 向 home 发 `FETCH(region_id, offset, len)`。
   - Home 若处于 writer-held 状态：**阻塞** FETCH 直到 writer release（toy 首版选阻塞，最简单；更好的选择是返回旧 epoch 快照，后续可优化）。
   - Home 回 `FETCH_RESP(epoch, payload)`；同时把 requester 加入 `readers` set。
   - Requester 把 payload 存入本地 cache tier（DRAM 中的一个 verbs MR，managed 层预留的 cache pool），state → `Shared(epoch)`。

2. **Reader 本地命中 `Shared`**：
   - 直接从本地 cache MR 读。**这是稳态热路径**，全程不走网络，零拷贝（`ub_try_local_map` 的背后）。

3. **Writer `ub_acquire_writer(va)`**：
   - 向 home 发 `WRITE_LOCK_REQ(region_id)`。
   - Home 检查：若已有 writer → 排队（FIFO）或返回 `UB_ERR_WOULD_BLOCK`（首版选排队 + 默认 5s 超时）。
   - Home 对所有 `readers` 发 `INVALIDATE(region_id, new_epoch = epoch + 1)`。
   - 每个 reader 收到后：本地 state → `Invalid`，回 `INVALIDATE_ACK`。
   - Home 收齐 ACK（或 reader 超时被踢出 readers set）后，`epoch += 1`，`writer = Some((writer_node, deadline))`，回 `WRITE_LOCK_GRANTED(epoch)`。

4. **Writer 持锁期间 `ub_write_va`**：
   - **Write-through**：若 writer 就在 home 上 → 本地写 verbs MR。否则向 home 发 `WRITE(region_id, offset, payload)`，home 执行本地写后回 ACK。
   - 持锁期间所有写都走 home，不在 writer 侧积累脏数据（不做 write-back）。

5. **Writer drop guard / `ub_sync`**：
   - 向 home 发 `WRITE_UNLOCK(region_id)`。
   - Home 清空 `writer`，readers set 此时为空（都在第 3 步被清了）。
   - 新一轮 FETCH 会看到 `epoch` 后的数据。

**Writer lease**：默认 5s，writer 崩溃 / 网络隔离时自动释放，home 丢弃该 writer，后续 acquire 可继续。lease 过期不会让已执行的 writes 回滚——它们已经 apply 到 home，只是 reader 可能看到的是 partial update；这是本模型的**一致性弱点**，但对 KV cache 这种幂等重算的场景可以接受。

**并发 FETCH 去重**：同一 region 的多个本地 task 并发 miss 时，只发出一个 `FETCH`，其余挂在 `tokio::sync::Notify` 上等。

### 6.4 Migration 与 Eviction

- **Reader 侧 eviction**：本地 cache MR 满时按 LRU 淘汰 `Shared` 副本；淘汰**不通知 home**——home 的 readers set 允许 stale（下次 invalidate 过去 reader 本地没这 region 也无妨，回 ACK 即可）。
- **Home 迁移**：**首版不做**。文档留扩展点：触发条件 = "home 负载 > 阈值 and 某 reader 命中率 > 阈值 → home 迁到该 reader"。迁移协议大致是 Raft leader transfer 的简化版（双锁交接），难度中等，M7 后续可加。
- **Region 删除**：`ub_free` 向 placer 发 `REGION_DELETE`，placer 通知 home + 所有 readers，home 释放 sub-allocation。

---

## 7. 一致性模型选型

### 候选方案

| 方案 | 优点 | 缺点 |
|---|---|---|
| (a) 最终一致 | 实现最简单（reader 自己按 TTL 拉新） | KV cache 的 writer update 可能几秒都看不到，对 decode step 不可接受 |
| **(b) SWMR + write-through + invalidate** ✓ | 原生匹配 KV cache 负载；writer release 后严格可见；实现只需 3 个状态 + 5 个事件 | 不支持多 writer；writer 持锁期间 reader 阻塞或看旧值 |
| (c) 完全线性化 | 语义最强 | 一切写都变 RPC 到 home，热路径崩溃；且需要全局序号或 Raft |

**选 (b)** 的理由：

- KV cache / 模型权重的 **"一个 writer 阶段 + 若干 reader 阶段"** 周期正是 SWMR 的典型；
- Write-through 避免了 dirty writeback 的复杂度，writer 崩溃也不会丢"本地脏页"；
- Invalidation 以 region 为单位，批次小，广播只发给 `readers` set 内的 node（不是全网）；
- 实现量 = 一个 `HashMap<RegionId, RegionHomeState>` + 5 个消息类型，toy 可控。

### 本模型**保证**

1. `WriterGuard` drop 后，任何**新发起**的 `ub_read_va` 都能看到该 writer 写入的值。
2. 同一 writer 在持锁期间的写按提交顺序 apply 到 home。
3. Reader 本地 `Shared` 副本在被 invalidate 前是内部一致的快照。

### 本模型**不保证**

1. **跨 region 写入顺序**：两个 region 各自的写无全局序；应用要靠自己用一个专门的 "同步 region" 做版本号。
2. **持锁中间可见性**：writer 持锁期间写了一半，其他 reader 不能"偷看"——acquire/release 是 release consistency 的 barrier。
3. **崩溃原子性**：writer lease 超时 = 部分写已 apply，reader 可能看到 partial update。对幂等 workload 可接受；对需要原子 update 的 workload 未来要加"版本号 region 指针 swap"。
4. **多 writer**：完全不支持。尝试并发 acquire 会排队或超时。

---

## 8. 与已有分布式系统的对比

| 系统 | 粒度 | 一致性 | 放置 | 与本项目差异 |
|---|---|---|---|---|
| **Treadmarks / Munin / Argo (DSM)** | 页级 | Release / Entry consistency | 静态 | Page 级透明 → false sharing 致命；本项目 region 级规避 |
| **UPC / OpenSHMEM / Chapel (PGAS)** | 数组切片 | 语言级 fence | 编译期静态切分 | 无动态 placer，本项目由 placer 自动选 |
| **FaRM (MSR)** | object | 2PC + 1-sided RDMA | 应用指定 | 最接近本思路；FaRM 无 placer，应用管 primary；本项目加 placer 层 |
| **Ceph RADOS** | object | 多副本强一致 | CRUSH 哈希 | 面向持久存储；本项目纯内存层，一致性弱一档 |
| **Alluxio** | block | 最终一致 | 按 worker locality | 面向数据编排/文件；本项目字节级、region 级 |
| **CUDA Unified Memory** | page | 驱动内 migrate | 按 fault | 内核驱动实现；本项目纯用户态、显式 alloc |
| **CXL 3.0 Type 3 + coherency** | cache line | 硬件 MESI | 池化 | 硬件方向；本项目是它的软件 toy 模拟 |
| **Grappa** | task + data | — | 动态 | 迁移计算到数据；本项目只迁移数据不迁移计算 |

**本项目的定位缝隙**：region 级 + SWMR + 中心 placer + 纯用户态 + 异构 tier。这是一个 **未被任何成熟开源项目占据** 的坐标，但每一条边都有成熟子系统可以借鉴，**所以"难但不险"**。

---

## 9. 最小端到端 Demo：3 节点 KV Cache

```
┌──────────── N0 (hub + placer) ────────────┐
│  Device: CPU 256 MiB + 模拟 NPU 256 MiB    │
│  Role: 控制面 hub、placer、region home     │
└────────────────────────────────────────────┘
       ▲                       ▲
       │ control               │ data
       │                       │
┌──── N1 (writer) ────┐  ┌──── N2 (reader) ────┐
│  KV cache producer  │  │  KV cache consumer  │
│  (模拟 decode step) │  │  (模拟 attention)   │
└─────────────────────┘  └─────────────────────┘
```

**步骤**：

1. N0 启动，扫描本地 device，向自身 placer 注册 CPU + NPU profile。
2. N1、N2 启动并连接 N0，报告自己的 device（CPU DRAM 作为 cache pool）。
3. N1 调 `ub_alloc(4 MiB, { access: SingleWriterReadOften, latency_class: Critical, capacity_class: Small })`。
4. Placer 选中 N0 NPU（`Critical` + `Small` 命中 NPU tier），在 N0 NPU 预留 pool 里切 4 MiB，返回 `UbVa`。
5. N1 `ub_acquire_writer(va)` → 向 N0 home 发 WRITE_LOCK_REQ → readers 空 → 直接授权。
6. N1 `ub_write_va(va, 0, payload_4mib)` → write-through 到 N0 NPU MR。
7. N1 drop WriterGuard → N0 epoch +1。
8. N2 `ub_read_va(va, 0, buf)`：
   - 本地 Invalid → 查 home = N0 → FETCH → 读到 N2 本地 DRAM cache → state = Shared(epoch=1)。
9. N2 再次 `ub_read_va` → 本地 `Shared` 命中，**不走网络**。
10. N1 再 `ub_acquire_writer` → N0 向 N2 发 INVALIDATE(epoch=2) → N2 ACK → granted。
11. N1 写新 payload、release。
12. N2 再读 → 本地 Invalid → 重新 FETCH → Shared(epoch=2)。

**验收条件**：

- [ ] Demo 应用代码**出现 0 次 `NodeId`**（用 `grep` 强制检查）。
- [ ] `unibusctl region list` 能看到 region、home node、readers set、当前 epoch。
- [ ] metrics 端点暴露：`placement_decision_total{tier=...}`, `cache_hit_total`, `cache_miss_total`, `invalidate_sent_total`, `write_lock_wait_ms`。
- [ ] N2 第 2 次读的延迟 < 第 1 次读的 1/5（本地命中效果）。
- [ ] Kill N2 → N0 / N1 正常继续；重启 N2 → 重新 FETCH 从 epoch 拿最新。
- [ ] Kill N1 持锁中 → N0 lease 超时释放，readers 后续 acquire 成功。

---

## 10. 对现有里程碑的影响

**主线 M1–M5 不变**，也不引入新前置依赖。本 vision 挂在 **M7**（M6 继续留给多跳/源路由），依赖 M2（MR + Device）、M3（控制面 + 消息语义）、M4（可靠传输 + 流控）。

**M7 子里程碑拆分**（把风险分散掉）：

| 子 MS | 内容 | 验收 |
|---|---|---|
| **M7.1** | Device capability profile + registry + 控制面 `DEVICE_PROFILE_PUBLISH` 广播 | `unibusctl device list` 能看到所有 node 上所有 device 的静态画像 |
| **M7.2** | Placer + `ub_alloc` / `ub_free`；**无 cache**，读写直达 home | 跨节点 `ub_read_va` / `ub_write_va` 通；应用代码无 NodeID |
| **M7.3** | 本地 read cache + FETCH 协议（**无 invalidate**，加 epoch 对比让 reader 自判过期） | cache 命中率可观测；写一次后 reader 重拉新值 |
| **M7.4** | Writer guard + invalidation 协议（SWMR 完成） | 第 9 节 demo 步骤 5–12 全部通过 |
| **M7.5** | LRU eviction + 本地 cache pool 容量管理 | cache pool 满时正确淘汰，不 OOM |
| **M7.6** | 3 节点 KV cache demo + CLI/metrics 完善 | 第 9 节全部验收条件满足 |

**逃生保底**：如果 M7.4（invalidate 协议）实现受阻，M7.3 停下来也能给出一个 "弱一致版 managed layer" 的可演示结果——这是为什么把 cache 和 invalidate 拆成两步。

**Managed 层完全不做也不阻塞主线**：M1–M5 的 verbs 层 KV demo 本身是可交付的（就是应用代码里有 NodeID），vision 可随时 park。

---

## 11. 开放问题

| # | 问题 | 当前倾向 / 备选 |
|---|---|---|
| V1 | `UbVa` 编码方式：`[region_id:64 \| offset:64]` 够了还是要留 type/version bit？ | 倾向 `[region_id:64 \| offset:64]`，region_id 高 16 bit 留 reserved |
| V2 | Placer 故障？ | 首版单点，hub 挂 = managed 层不可用；M7 之后再考虑 Raft/backup placer |
| V3 | Region 上限？ | 首版 1 GiB；>1 GiB 的需求走应用分片 |
| V4 | Readers set 持久化？ | 不持久化；placer 重启丢失 = 所有 readers 变"黑名单"不在集合里，下次 invalidate 对他们是 no-op，由 reader 自己下次 FETCH 时重新加入 |
| V5 | `ub_acquire_writer` 排队 vs 立即失败？ | 默认排队 + 5s lease，应用也可以传 `try_acquire` 选立即失败 |
| V6 | Pool 预留比例？ | 首版 NPU 50% / CPU 25%，配置可调 |
| V7 | Hub node 本身既是 placer 又是某 region 的 home，是否会形成全局热点？ | 首版允许，验收 demo 里观察；真热时 placer 会把冷 region 分到其他 node，但本身已是瓶颈点 |
| V8 | 对 verbs 层的 FR 有反向修改诉求吗？ | 有一条：verbs 层要允许**一次性注册大 MR 后在其内部做 sub-range 失效**（managed 层的 region 是 sub-range，但 verbs 层 `ub_mr_deregister` 是整个 MR）——M7.2 启动时再正式写 FR |
| V9 | Managed 层的 cache 本身要不要暴露给 verbs 层做 zero-copy 目的地？ | 暂不——边界规则第 1 条 "只选一层"；未来允许混用时再考虑 |
| V10 | NPU 显存作为 cache tier 时的语义：NPU 上的 cache replica 能直接被本地算子访问吗？ | 首版可以（因为 NPU 是纯软件模拟，就是一段字节数组）；真实 NPU 下这涉及 HBM 驻留，留作未来硬件适配点 |

---

## 12. 下一步

本文档为 **exploratory vision**，不立即排期。在主线 M1–M5 交付过程中，M7 track 可并行做以下准备，但不阻塞主线：

1. V8 的 sub-range dereg 提案——等 M2 实现时顺便评估可行性；
2. Device capability profile 的默认值表——可以在 M2 落 NPU 模拟时顺手先写到 YAML；
3. 把本文第 9 节 demo 的 "0 次 NodeId" grep 断言先写成测试骨架。

如 reviewer 对第 11 节任何问题有强意见，直接在这里标记 / 留 comment；我会把正式决定追加到本文件并在 M7 启动时同步到 REQUIREMENTS.md（新增 FR-GVA-* 段）与 DESIGN.md（新增 §17 Managed Layer 实现）。

---

## 13. 参考资料（互联网可查）

> 下列链接仅做路线参考，2026-04 月验证可访问。

- ⭐ FaRM: Fast Remote Memory (NSDI '14) — https://www.microsoft.com/en-us/research/publication/farm-fast-remote-memory/
- ⭐ Treadmarks: Distributed Shared Memory on Standard Workstations — https://www.usenix.org/conference/usenix-winter-1994-technical-conference/treadmarks-distributed-shared-memory-standard
- ⭐ Argo DSM — https://www.it.uu.se/research/group/uart/argo
- 📖 Ceph RADOS / CRUSH — https://ceph.io/en/
- 📖 Alluxio — https://www.alluxio.io/
- 📖 CXL 3.0 Specification overview — https://www.computeexpresslink.org/
- 📖 PGAS / OpenSHMEM — http://www.openshmem.org/
- 📖 Grappa: latency-tolerant runtime for DSM — https://grappa.io/
