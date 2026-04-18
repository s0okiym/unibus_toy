# UniBus Toy 部署与配置指南

本文档详细说明如何从零开始部署 UniBus Toy 集群，涵盖环境准备、编译构建、配置文件编写、集群启动、功能验证和故障排查。

---

## 一、环境要求

| 项 | 要求 |
|---|---|
| 操作系统 | Linux（已验证 EL8 / kernel 4.18+） |
| Rust | 1.75+（edition 2021） |
| Python | 3.x（E2E 测试 JSON 解析用） |
| 网络 | 本机多端口可达（默认使用 127.0.0.1 上的多个端口） |

安装 Rust：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
rustc --version   # 确认 >= 1.75
```

---

## 二、获取与编译

### 2.1 获取源码

```bash
git clone <repo-url> unibus_toy
cd unibus_toy
```

### 2.2 编译

```bash
# Debug 构建（开发调试用）
cargo build

# Release 构建（生产部署和性能测试用，推荐）
cargo build --release
```

编译产物：

| 二进制 | 路径 | 说明 |
|---|---|---|
| `unibusd` | `target/release/unibusd` | 守护进程，每个节点运行一个实例 |
| `unibusctl` | `target/release/unibusctl` | CLI 管理工具，通过 HTTP 与 unibusd 交互 |

### 2.3 运行单元测试

```bash
# 全量测试
cargo test --workspace

# 单 crate 测试
cargo test -p ub-core
cargo test -p ub-transport
cargo test -p ub-managed
```

---

## 三、配置文件详解

每个 `unibusd` 实例需要一个 YAML 配置文件。配置文件定义了节点的身份、网络监听、传输参数、Managed 层等全部运行时行为。

### 3.1 完整配置模板

以下是一个包含所有配置项的完整模板，每项均有详细注释：

```yaml
# ============================================================
# 基础标识
# ============================================================
pod_id: 1                    # SuperPod 标识。同一集群所有节点必须相同。
                             # 类型: u16, 默认值: 1

node_id: 1                   # 本节点在 SuperPod 中的唯一标识。
                             # 类型: u16, 必填, 无默认值。
                             # node_id=0 的节点作为 Hub（协调节点），
                             # 其他节点启动时向 Hub 发送 JOIN 请求。
                             # 集群内每个节点的 node_id 必须不同。

# ============================================================
# 控制面配置
# ============================================================
control:
  listen: "0.0.0.0:7900"    # 控制面 TCP 监听地址。
                             # 格式: "IP:PORT"
                             # 默认值: "0.0.0.0:7900"
                             # 每个节点必须使用不同的端口。

  bootstrap: static          # 集群引导方式。当前仅支持 "static"。
                             # static 模式下，节点通过 peers 列表发现彼此。

  peers:                     # 已知对端的控制面地址列表。
    - "127.0.0.1:7901"      # 节点启动时会向这些地址发送 HELLO 消息。
    - "127.0.0.1:7902"      # Hub 节点可以不填 peers；非 Hub 节点至少填 Hub 地址。

  seed_addrs: []             # （预留，当前未使用）种子节点地址。

  hub_node_id: 0             # Hub 节点的 node_id。
                             # 默认值: 0
                             # Hub 负责转发成员变更通知（SNAPSHOT）给所有节点。

# ============================================================
# 数据面配置
# ============================================================
data:
  listen: "0.0.0.0:7910"    # 数据面 UDP 监听地址。
                             # 格式: "IP:PORT"
                             # 默认值: "0.0.0.0:7901"
                             # verbs 操作（read/write/atomic/send）通过此端口收发。
                             # 每个节点必须使用不同的端口。

  fabric: udp                # Fabric 传输协议。当前仅支持 "udp"。
                             # TCP/UDS 已在 trait 中预留，尚未实现。

  mtu: 1400                  # 最大传输单元（字节）。
                             # 默认值: 1400
                             # 有效载荷最大约 1320 字节（扣除帧头）。
                             # 超过此大小的数据需要分片（分片逻辑尚未实现，
                             # 因此单个 verb 请求不要超过 ~1320 字节）。

# ============================================================
# MR（内存区域）配置
# ============================================================
mr:
  deregister_timeout_ms: 500 # MR 注销的等待超时（毫秒）。
                             # 默认值: 500
                             # MR 注销时如果还有 inflight 操作，会进入 Revoking
                             # 状态等待操作完成。超时后强制进入 Released 状态。
                             # 增大此值可以减少 inflight 操作失败的概率。

# ============================================================
# 设备配置
# ============================================================
device:
  npu:
    enabled: true             # 是否启用模拟 NPU 设备。
                              # 默认值: true
                              # NPU 设备提供模拟显存，与 CPU 内存共存于统一编址空间。
                              # 设为 false 则只有 CPU 内存设备。

    mem_size_mib: 64          # NPU 模拟显存大小（MiB）。
                              # 默认值: 256
                              # 启动时会分配指定大小的内存作为 NPU 设备的存储。

# ============================================================
# 传输层配置
# ============================================================
transport:
  rto_ms: 200                # 初始重传超时（Retransmission Timeout，毫秒）。
                             # 默认值: 200
                             # 发送数据帧后，如果在 RTO 时间内未收到 ACK，
                             # 触发重传。运行时 RTO 会根据 EWMA RTT 动态调整。
                             # 低延迟网络可设小（如 100），高延迟网络应增大。

  max_retries: 8             # 单帧最大重传次数。
                             # 默认值: 8
                             # 超过此次数后放弃该帧，通知上层传输失败。
                             # 每次重传 RTO 指数退避。

  sack_bitmap_bits: 256      # SACK 位图位数。
                             # 默认值: 256
                             # 用于 ACK 帧中标记已收到的乱序帧序号。
                             # 通常不需要修改。

  reassembly_budget_bytes: 67108864  # 重组缓冲区预算（字节）。
                                     # 默认值: 67108864 (64 MiB)
                                     # 预留字段，当前分片/重组逻辑未实现。

# ============================================================
# 流控配置
# ============================================================
flow:
  initial_credits: 64        # 初始信用（credit）数量。
                             # 默认值: 64
                             # 每个对端会话的初始发送窗口大小。
                             # 增大可提高吞吐，但占用更多内存。
                             # 对端通过 Credit 帧归还信用。

# ============================================================
# Jetty 配置
# ============================================================
jetty:
  jfs_depth: 1024            # JFS（发送队列）深度。
                             # 默认值: 1024
                             # 每个 Jetty 的 JFS 最多缓冲的发送请求数。

  jfr_depth: 1024            # JFR（接收队列）深度。
                             # 默认值: 1024
                             # 每个 Jetty 的 JFR 最多缓冲的接收请求数。

  jfc_depth: 1024            # JFC（完成队列）深度。
                             # 默认值: 1024
                             # 每个 Jetty 的 JFC 最多缓冲的完成事件数。

  jfc_high_watermark: 896    # JFC 高水位线。
                             # 默认值: 896
                             # 当 JFC 中的 CQE 数量超过此值时，产生流控反压。
                             # 应小于 jfc_depth，建议为 jfc_depth 的 87.5%。

# ============================================================
# 可观测配置
# ============================================================
obs:
  metrics_listen: "127.0.0.1:9090"  # HTTP 监听地址（同时服务 /metrics 和 /admin/*）。
                                     # 格式: "IP:PORT"
                                     # 默认值: "127.0.0.1:9090"
                                     # 每个节点必须使用不同的端口。
                                     # 此端口同时提供：
                                     #   GET  /metrics          — Prometheus 指标
                                     #   GET  /admin/node/list  — 集群成员列表
                                     #   POST /admin/verb/write — 远程写
                                     #   ... (共 31 个端点)

# ============================================================
# 心跳配置
# ============================================================
heartbeat:
  interval_ms: 1000          # 心跳发送间隔（毫秒）。
                             # 默认值: 1000
                             # 每个节点每隔此时间向所有已知对端发送 HEARTBEAT。
                             # 减小可加快故障检测，但增加网络开销。

  fail_after: 3              # 连续未收到心跳的次数阈值。
                             # 默认值: 3
                             # 连续 fail_after 次未收到某节点的心跳后，
                             # 判定该节点故障，触发 PeerChangeEvent::Leave 通知。
                             # 实际故障检测时间 = interval_ms × fail_after。
                             # 默认配置下为 3 秒。

# ============================================================
# Managed 层配置（M7）
# ============================================================
managed:
  enabled: false             # 是否启用 Managed 层。
                             # 默认值: false
                             # 设为 true 后，节点将提供：
                             #   - ub_alloc / ub_free（全局区域分配/释放）
                             #   - read_va / write_va（按虚拟地址读写）
                             #   - acquire_writer / release_writer（SWMR 写锁）
                             #   - region cache + INVALIDATE/re-FETCH
                             # 纯 Verbs 层使用（M1–M5）设为 false 即可。

  placer_node_id: 1          # 执行放置决策的节点 ID。
                             # 默认值: 0
                             # 只有此节点上的 Placer 会执行 region 分配。
                             # 其他节点收到 alloc 请求会转发到此节点。
                             # 通常设为第一个非 Hub 节点（如 node_id=1）。

  pool:
    memory_reserve_ratio: 0.25  # 内存池预留比例。
                                # 默认值: 0.25
                                # 内存池总大小为 256 MiB。
                                # 预留部分不参与分配，用于系统开销。
                                # 可分配部分 = 256 MiB × (1 - memory_reserve_ratio)。

    npu_reserve_ratio: 0.50     # NPU 池预留比例。
                                # 默认值: 0.50
                                # NPU 池可分配部分 = npu_mem_size × (1 - npu_reserve_ratio)。

  writer_lease_ms: 5000      # 写锁租约时长（毫秒）。
                             # 默认值: 5000
                             # 获取写锁后，如果在租约期内未释放，
                             # 系统会自动过期该写锁，防止死锁。
                             # 增大可容忍更长的写操作，但故障恢复更慢。

  fetch_block_on_writer: true  # FETCH 时是否阻塞等待写锁释放。
                               # 默认值: true
                               # true:  读操作遇到写锁时等待写释放后再 FETCH。
                               # false: 读操作遇到写锁时立即返回错误。

  cache:
    max_bytes: 8388608        # 缓存池最大字节数。
                              # 默认值: 536870912 (512 MiB)
                              # 远端 region 的本地缓存容量上限。
                              # 超出时按淘汰策略逐出旧条目。
                              # 测试环境建议 8 MiB (8388608)。

    eviction: lru             # 缓存淘汰策略。
                              # 默认值: "lru"
                              # 当前仅支持 LRU（Least Recently Used）。

  cost_weights:               # Placer 放置决策的代价权重。
                              # Placer 使用加权评分选择最优设备：
                              #   score = latency×w1 + capacity×w2 + tier_match×w3 + load×w4
                              # 分数越低越优先。
    latency: 1.0              # 延迟类权重。默认值: 1.0
    capacity: 0.3             # 容量类权重。默认值: 0.3
    tier_match: 0.5           # 层级匹配权重。默认值: 0.5
    load: 0.2                 # 负载权重。默认值: 0.2
```

### 3.2 配置项速查表

| 配置项 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `pod_id` | u16 | 1 | SuperPod ID，集群内所有节点必须相同 |
| `node_id` | u16 | **必填** | 节点唯一 ID，0 为 Hub |
| `control.listen` | String | `"0.0.0.0:7900"` | 控制面 TCP 监听地址 |
| `control.bootstrap` | String | `"static"` | 引导方式（仅 static） |
| `control.peers` | Vec\<String\> | `[]` | 已知对端控制面地址 |
| `control.hub_node_id` | u16 | 0 | Hub 节点 ID |
| `data.listen` | String | `"0.0.0.0:7901"` | 数据面 UDP 监听地址 |
| `data.fabric` | String | `"udp"` | 传输协议（仅 udp） |
| `data.mtu` | u16 | 1400 | 最大传输单元 |
| `mr.deregister_timeout_ms` | u64 | 500 | MR 注销超时 |
| `device.npu.enabled` | bool | true | 是否启用 NPU 模拟设备 |
| `device.npu.mem_size_mib` | u64 | 256 | NPU 显存大小 (MiB) |
| `transport.rto_ms` | u64 | 200 | 初始重传超时 |
| `transport.max_retries` | u32 | 8 | 最大重传次数 |
| `transport.sack_bitmap_bits` | u16 | 256 | SACK 位图位数 |
| `transport.reassembly_budget_bytes` | u64 | 67108864 | 重组缓冲区预算 |
| `flow.initial_credits` | u32 | 64 | 初始信用数 |
| `jetty.jfs_depth` | u32 | 1024 | JFS 队列深度 |
| `jetty.jfr_depth` | u32 | 1024 | JFR 队列深度 |
| `jetty.jfc_depth` | u32 | 1024 | JFC 队列深度 |
| `jetty.jfc_high_watermark` | u32 | 896 | JFC 高水位线 |
| `obs.metrics_listen` | String | `"127.0.0.1:9090"` | HTTP 监听地址 |
| `heartbeat.interval_ms` | u64 | 1000 | 心跳间隔 |
| `heartbeat.fail_after` | u32 | 3 | 心跳失败阈值 |
| `managed.enabled` | bool | false | 是否启用 Managed 层 |
| `managed.placer_node_id` | u16 | 0 | 放置决策节点 ID |
| `managed.pool.memory_reserve_ratio` | f64 | 0.25 | 内存池预留比例 |
| `managed.pool.npu_reserve_ratio` | f64 | 0.50 | NPU 池预留比例 |
| `managed.writer_lease_ms` | u64 | 5000 | 写锁租约时长 |
| `managed.fetch_block_on_writer` | bool | true | FETCH 是否等待写锁 |
| `managed.cache.max_bytes` | u64 | 536870912 | 缓存池最大字节数 |
| `managed.cache.eviction` | String | `"lru"` | 淘汰策略 |
| `managed.cost_weights.latency` | f64 | 1.0 | 延迟权重 |
| `managed.cost_weights.capacity` | f64 | 0.3 | 容量权重 |
| `managed.cost_weights.tier_match` | f64 | 0.5 | 层级匹配权重 |
| `managed.cost_weights.load` | f64 | 0.2 | 负载权重 |

### 3.3 关键约束

1. **node_id 唯一**：集群内每个节点的 `node_id` 必须不同。
2. **pod_id 一致**：同一集群所有节点的 `pod_id` 必须相同。
3. **端口不冲突**：同一机器上运行的多个节点，`control.listen`、`data.listen`、`obs.metrics_listen` 三个端口必须各不相同。
4. **peers 互指**：每个节点的 `peers` 列表应包含集群内其他节点的控制面地址。
5. **Hub 角色**：`hub_node_id` 指向的节点（通常 node_id=0）负责转发成员变更。该节点应先启动。
6. **Managed 层**：只有 `managed.enabled: true` 的节点才能执行 region 操作。同一集群的 Managed 节点应使用相同的 `placer_node_id`。

---

## 四、集群部署

### 4.1 场景一：Verbs 层 3 节点集群（M1–M5）

#### 步骤 1：编写配置文件

**configs/node0.yaml**（Hub 节点）

```yaml
pod_id: 1
node_id: 1
control:
  listen: "0.0.0.0:7900"
  bootstrap: static
  peers:
    - "127.0.0.1:7901"
  hub_node_id: 0
data:
  listen: "0.0.0.0:7910"
  fabric: udp
  mtu: 1400
mr:
  deregister_timeout_ms: 500
device:
  npu:
    enabled: true
    mem_size_mib: 64
transport:
  rto_ms: 200
  max_retries: 8
flow:
  initial_credits: 64
jetty:
  jfs_depth: 1024
  jfr_depth: 1024
  jfc_depth: 1024
  jfc_high_watermark: 896
obs:
  metrics_listen: "127.0.0.1:9090"
heartbeat:
  interval_ms: 1000
  fail_after: 3
managed:
  enabled: false
```

**configs/node1.yaml**

```yaml
pod_id: 1
node_id: 2
control:
  listen: "0.0.0.0:7901"
  bootstrap: static
  peers:
    - "127.0.0.1:7900"
  hub_node_id: 0
data:
  listen: "0.0.0.0:7911"
  fabric: udp
  mtu: 1400
mr:
  deregister_timeout_ms: 500
device:
  npu:
    enabled: true
    mem_size_mib: 64
transport:
  rto_ms: 200
  max_retries: 8
flow:
  initial_credits: 64
jetty:
  jfs_depth: 1024
  jfr_depth: 1024
  jfc_depth: 1024
  jfc_high_watermark: 896
obs:
  metrics_listen: "127.0.0.1:9091"
heartbeat:
  interval_ms: 1000
  fail_after: 3
managed:
  enabled: false
```

**configs/node2.yaml**

```yaml
pod_id: 1
node_id: 3
control:
  listen: "0.0.0.0:7902"
  bootstrap: static
  peers:
    - "127.0.0.1:7900"
    - "127.0.0.1:7901"
  hub_node_id: 0
data:
  listen: "0.0.0.0:7912"
  fabric: udp
  mtu: 1400
mr:
  deregister_timeout_ms: 500
device:
  npu:
    enabled: true
    mem_size_mib: 64
transport:
  rto_ms: 200
  max_retries: 8
flow:
  initial_credits: 64
jetty:
  jfs_depth: 1024
  jfr_depth: 1024
  jfc_depth: 1024
  jfc_high_watermark: 896
obs:
  metrics_listen: "127.0.0.1:9092"
heartbeat:
  interval_ms: 1000
  fail_after: 3
managed:
  enabled: false
```

#### 步骤 2：编译

```bash
cargo build --release
```

#### 步骤 3：启动集群

```bash
# 终端 1 — Node 1 (Hub)
./target/release/unibusd --config configs/node0.yaml

# 终端 2 — Node 2
./target/release/unibusd --config configs/node1.yaml

# 终端 3 — Node 3
./target/release/unibusd --config configs/node2.yaml
```

也可后台启动：

```bash
./target/release/unibusd --config configs/node0.yaml > /tmp/node0.log 2>&1 &
./target/release/unibusd --config configs/node1.yaml > /tmp/node1.log 2>&1 &
./target/release/unibusd --config configs/node2.yaml > /tmp/node2.log 2>&1 &
```

#### 步骤 4：验证集群

```bash
# 等待集群形成（约 6 秒）
sleep 6

# 查看节点列表
curl -s http://127.0.0.1:9090/admin/node/list | python3 -m json.tool

# 预期输出：nodes 数组包含 3 个节点
# {
#   "nodes": [
#     {"node_id": 1, "state": "Alive", ...},
#     {"node_id": 2, "state": "Alive", ...},
#     {"node_id": 3, "state": "Alive", ...}
#   ]
# }
```

#### 步骤 5：基本操作

```bash
# 注册 MR
curl -s -X POST http://127.0.0.1:9090/admin/mr/register \
  -H "Content-Type: application/json" \
  -d '{"device_kind":"memory","len":1048576,"perms":"rw"}'

# 查看本地 MR
curl -s http://127.0.0.1:9090/admin/mr/list

# 远程写（从 node2 写入 node1 的 MR）
curl -s -X POST http://127.0.0.1:9091/admin/verb/write \
  -H "Content-Type: application/json" \
  -d '{"ub_addr":"0x0001000X000000000000000000000000","data":[72,101,108,108,111]}'

# 远程读
curl -s -X POST http://127.0.0.1:9091/admin/verb/read \
  -H "Content-Type: application/json" \
  -d '{"ub_addr":"0x0001000X000000000000000000000000","len":5}'

# 创建 Jetty
curl -s -X POST http://127.0.0.1:9090/admin/jetty/create

# 查看 metrics
curl http://127.0.0.1:9090/metrics
```

#### 步骤 6：停止集群

```bash
# 如果是前台运行，在各终端按 Ctrl+C

# 如果是后台运行
pkill -f unibusd
```

### 4.2 场景二：Managed 层 3 节点集群（M7）

Managed 层集群使用独立的端口配置，避免与 Verbs 层集群冲突。

#### 步骤 1：编写配置文件

**configs/m7_node0.yaml**

```yaml
pod_id: 1
node_id: 1
control:
  listen: "0.0.0.0:7920"
  bootstrap: static
  peers:
    - "127.0.0.1:7921"
    - "127.0.0.1:7922"
  hub_node_id: 0
data:
  listen: "0.0.0.0:7930"
  fabric: udp
  mtu: 1400
mr:
  deregister_timeout_ms: 500
device:
  npu:
    enabled: true
    mem_size_mib: 64
transport:
  rto_ms: 200
  max_retries: 8
flow:
  initial_credits: 64
jetty:
  jfs_depth: 1024
  jfr_depth: 1024
  jfc_depth: 1024
  jfc_high_watermark: 896
obs:
  metrics_listen: "127.0.0.1:9190"
heartbeat:
  interval_ms: 1000
  fail_after: 3
managed:
  enabled: true
  placer_node_id: 1
  pool:
    memory_reserve_ratio: 0.25
    npu_reserve_ratio: 0.50
  writer_lease_ms: 5000
  fetch_block_on_writer: true
  cache:
    max_bytes: 8388608
    eviction: lru
  cost_weights:
    latency: 1.0
    capacity: 0.3
    tier_match: 0.5
    load: 0.2
```

**configs/m7_node1.yaml** — 修改 `node_id: 2`、`control.listen: "0.0.0.0:7921"`、`data.listen: "0.0.0.0:7931"`、`obs.metrics_listen: "127.0.0.1:9191"`，其余与 m7_node0.yaml 相同。

**configs/m7_node2.yaml** — 修改 `node_id: 3`、`control.listen: "0.0.0.0:7922"`、`data.listen: "0.0.0.0:7932"`、`obs.metrics_listen: "127.0.0.1:9192"`，其余与 m7_node0.yaml 相同。

#### 步骤 2：启动集群

```bash
./target/release/unibusd --config configs/m7_node0.yaml > /tmp/m7_node0.log 2>&1 &
./target/release/unibusd --config configs/m7_node1.yaml > /tmp/m7_node1.log 2>&1 &
./target/release/unibusd --config configs/m7_node2.yaml > /tmp/m7_node2.log 2>&1 &
sleep 6
```

#### 步骤 3：Managed 层完整操作

```bash
ADMIN0="http://127.0.0.1:9190"
ADMIN1="http://127.0.0.1:9191"
ADMIN2="http://127.0.0.1:9192"

# 1. 查看设备 profile
curl -s "$ADMIN1/admin/device/list"

# 2. 分配 region
ALLOC_RESP=$(curl -s -X POST "$ADMIN1/admin/region/alloc" \
  -H "Content-Type: application/json" \
  -d '{"size":4096,"latency_class":"normal","capacity_class":"small"}')
echo "$ALLOC_RESP"
VA=$(echo "$ALLOC_RESP" | python3 -c "import sys,json; print(json.load(sys.stdin)['va'])")
REGION_ID=$(echo "$ALLOC_RESP" | python3 -c "import sys,json; print(json.load(sys.stdin)['region_id'])")
HOME_NODE=$(echo "$ALLOC_RESP" | python3 -c "import sys,json; print(json.load(sys.stdin)['home_node_id'])")

# 3. 获取写锁
curl -s -X POST "$ADMIN1/admin/verb/acquire-writer" \
  -H "Content-Type: application/json" \
  -d "{\"va\":\"$VA\"}"

# 4. 写入数据 "Hello" = [72,101,108,108,111]
curl -s -X POST "$ADMIN1/admin/verb/write-va" \
  -H "Content-Type: application/json" \
  -d "{\"va\":\"$VA\",\"offset\":0,\"data\":[72,101,108,108,111]}"

# 5. 释放写锁
curl -s -X POST "$ADMIN1/admin/verb/release-writer" \
  -H "Content-Type: application/json" \
  -d "{\"va\":\"$VA\"}"

# 6. 在 node3 注册远端 region
curl -s -X POST "$ADMIN2/admin/region/register-remote" \
  -H "Content-Type: application/json" \
  -d "{\"region_id\":$REGION_ID,\"home_node_id\":$HOME_NODE,\"device_id\":0,\"mr_handle\":0,\"base_offset\":0,\"len\":4096,\"epoch\":0}"

# 7. 远端读取（首次，cache miss -> FETCH）
curl -s -X POST "$ADMIN2/admin/verb/read-va" \
  -H "Content-Type: application/json" \
  -d "{\"va\":\"$VA\",\"offset\":0,\"len\":5}"
# 预期: source=fetch, data=[72,101,108,108,111]

# 8. 再次读取（cache hit）
curl -s -X POST "$ADMIN2/admin/verb/read-va" \
  -H "Content-Type: application/json" \
  -d "{\"va\":\"$VA\",\"offset\":0,\"len\":5}"
# 预期: source=cache

# 9. 释放时获取写锁，触发 invalidation
ACQ2=$(curl -s -X POST "$ADMIN1/admin/verb/acquire-writer" \
  -H "Content-Type: application/json" \
  -d "{\"va\":\"$VA\"}")
NEW_EPOCH=$(echo "$ACQ2" | python3 -c "import sys,json; print(json.load(sys.stdin)['epoch'])")

# 10. 在 node3 执行 invalidation
curl -s -X POST "$ADMIN2/admin/region/invalidate" \
  -H "Content-Type: application/json" \
  -d "{\"region_id\":$REGION_ID,\"new_epoch\":$NEW_EPOCH}"

# 11. 写入新数据 "World" = [87,111,114,108,100]
curl -s -X POST "$ADMIN1/admin/verb/write-va" \
  -H "Content-Type: application/json" \
  -d "{\"va\":\"$VA\",\"offset\":0,\"data\":[87,111,114,108,100]}"

# 12. 释放写锁
curl -s -X POST "$ADMIN1/admin/verb/release-writer" \
  -H "Content-Type: application/json" \
  -d "{\"va\":\"$VA\"}"

# 13. 远端再次读取（re-FETCH，获得新数据）
curl -s -X POST "$ADMIN2/admin/verb/read-va" \
  -H "Content-Type: application/json" \
  -d "{\"va\":\"$VA\",\"offset\":0,\"len\":5}"
# 预期: source=fetch, data=[87,111,114,108,100]

# 14. 查看区域列表
curl -s "$ADMIN1/admin/region/list"

# 15. 查看指标
curl -s "$ADMIN2/metrics"
```

---

## 五、使用 unibusctl CLI

`unibusctl` 是命令行管理工具，通过 HTTP 与指定节点的 admin API 交互。

### 5.1 通用参数

| 参数 | 说明 | 默认值 |
|---|---|---|
| `-a` / `--addr` | 目标节点 admin API 地址 | `http://127.0.0.1:9090` |

### 5.2 子命令一览

| 子命令 | 说明 | 关键参数 |
|---|---|---|
| `node-start` | 启动新的 unibusd 进程 | `--config <path>` |
| `node-list` | 列出集群所有成员 | — |
| `node-info` | 查看本地节点详细信息 | — |
| `mr-list` | 列出本地 MR | — |
| `mr-cache` | 列出远端 MR 缓存 | — |
| `jetty-create` | 创建 Jetty | — |
| `jetty-list` | 列出本地 Jetty | — |
| `jetty-post-recv` | 预投递接收缓冲区 | `<jetty_handle> <len>` |
| `jetty-poll-cqe` | 轮询完成事件 | `<jetty_handle>` |
| `bench` | 运行写/读/原子基准测试 | `--ub_addr`, `-n`, `-s` |
| `kv-init` | 初始化 KV store | `-s`, `--device_kind` |
| `kv-put` | KV 写入 | `<ub_addr> <slot> <key> <value>` |
| `kv-get` | KV 读取 | `<ub_addr> <slot> <key>` |
| `kv-cas` | KV 原子 CAS | `<ub_addr> <slot> <key> <expect_version> <value>` |
| `device-list` | 列出设备 profile | — |
| `region-alloc` | 分配全局 region | `-s`, `--latency_class`, `--capacity_class`, `--pin` |
| `region-free` | 释放全局 region | `<region_id>` |
| `region-list` | 列出所有 region | — |
| `va-read` | 按虚拟地址读 | `<va> -l <len> --offset` |
| `va-write` | 按虚拟地址写 | `<va> <data> --offset` |
| `acquire-writer` | 获取写锁 | `<va>` |
| `release-writer` | 释放写锁 | `<va>` |
| `region-register-remote` | 注册远端 region | `<region_id> <home_node_id> <device_id> <mr_handle> --len` |
| `region-invalidate` | 使缓存失效 | `<region_id> --new_epoch` |

### 5.3 常用示例

```bash
# 查看集群成员
./target/release/unibusctl -a http://127.0.0.1:9090 node-list

# 查看本地 MR
./target/release/unibusctl -a http://127.0.0.1:9090 mr-list

# 创建 Jetty
./target/release/unibusctl -a http://127.0.0.1:9090 jetty-create

# 运行 benchmark（100 次写/读/原子，1KiB 数据）
./target/release/unibusctl -a http://127.0.0.1:9090 bench --ub_addr 0x... -n 100 -s 1024

# Managed 层操作（端口 9190）
./target/release/unibusctl -a http://127.0.0.1:9190 device-list
./target/release/unibusctl -a http://127.0.0.1:9190 region-alloc -s 4096 --latency_class normal
./target/release/unibusctl -a http://127.0.0.1:9190 region-list
```

---

## 六、Admin HTTP API 参考

所有 API 通过 `obs.metrics_listen` 端口提供，请求和响应均为 JSON 格式。

### 6.1 集群与节点

| Endpoint | Method | 请求体 | 响应字段 | 说明 |
|---|---|---|---|---|
| `/admin/node/list` | GET | — | `nodes[]` | 列出集群所有成员 |
| `/admin/node/info` | GET | — | `node_id`, `state`, ... | 查询本地节点信息 |

### 6.2 MR（内存区域）

| Endpoint | Method | 请求体 | 响应字段 | 说明 |
|---|---|---|---|---|
| `/admin/mr/list` | GET | — | `mrs[]` | 列出本地 MR |
| `/admin/mr/cache` | GET | — | `cache[]` | 列出远端 MR 缓存 |
| `/admin/mr/register` | POST | `{"device_kind":"memory","len":1048576,"perms":"rw"}` | `ub_addr`, `mr_handle` | 注册本地 MR |
| `/admin/mr/deregister` | POST | `{"mr_handle":1}` | `status` | 注销本地 MR |

**MR 注册请求字段说明：**

| 字段 | 类型 | 说明 |
|---|---|---|
| `device_kind` | String | `"memory"` 或 `"npu"` |
| `len` | u64 | MR 大小（字节） |
| `perms` | String | 权限：`"r"` / `"w"` / `"rw"` / `"rwa"`（含原子） |

### 6.3 Jetty

| Endpoint | Method | 请求体 | 响应字段 | 说明 |
|---|---|---|---|---|
| `/admin/jetty/create` | POST | — | `jetty_handle` | 创建 Jetty |
| `/admin/jetty/list` | GET | — | `jettys[]` | 列出本地 Jetty |
| `/admin/jetty/post-recv` | POST | `{"jetty_handle":1,"len":4096}` | `status` | 预投递接收缓冲区 |
| `/admin/jetty/poll-cqe` | POST | `{"jetty_handle":1}` | `wr_id`, `status`, `verb`, ... | 轮询完成事件 |

### 6.4 Verbs 操作

| Endpoint | Method | 请求体 | 响应字段 | 说明 |
|---|---|---|---|---|
| `/admin/verb/write` | POST | `{"ub_addr":"0x...","data":[...]}` | `status` | 远程写（fire-and-forget） |
| `/admin/verb/read` | POST | `{"ub_addr":"0x...","len":64}` | `data[]` | 远程读 |
| `/admin/verb/atomic-cas` | POST | `{"ub_addr":"0x...","expect":0,"new":1}` | `old_value` | 远程原子 CAS |
| `/admin/verb/atomic-faa` | POST | `{"ub_addr":"0x...","add":1}` | `old_value` | 远程原子 FAA |
| `/admin/verb/send` | POST | `{"dst_node_id":2,"dst_jetty_id":0,"data":[...]}` | `status` | 发送消息 |
| `/admin/verb/send-with-imm` | POST | `{"dst_node_id":2,"dst_jetty_id":0,"data":[...],"imm":42}` | `status` | 带立即数发送 |
| `/admin/verb/write-imm` | POST | `{"ub_addr":"0x...","data":[...],"imm":42,"dst_node_id":2,"dst_jetty_id":0}` | `status` | 带立即数写 |

**UB 地址格式：** 128-bit 十六进制字符串，如 `"0x00010001000000000000000000000000"`。
格式：`[PodID:4hex|NodeID:4hex|DeviceID:4hex|Offset:16hex|Reserved:4hex]`。

### 6.5 KV Demo

| Endpoint | Method | 请求体 | 响应字段 | 说明 |
|---|---|---|---|---|
| `/admin/kv/init` | POST | `{"slots":16,"device_kind":"memory"}` | `ub_addr`, `mr_handle`, `slot_size` | 初始化 KV |
| `/admin/kv/put` | POST | `{"ub_addr":"0x...","slot":0,"key":"k","value":"v"}` | `status`, `version` | KV 写入 |
| `/admin/kv/get` | POST | `{"ub_addr":"0x...","slot":0,"key":"k"}` | `key`, `value`, `version` | KV 读取 |
| `/admin/kv/cas` | POST | `{"ub_addr":"0x...","slot":0,"key":"k","expect_version":1,"value":"v2"}` | `status`, `new_version` | 原子 CAS |

**KV slot 布局：** key[32] + value[64] + version[8] = 104 字节/slot。

### 6.6 Managed 层（M7）

| Endpoint | Method | 请求体 | 响应字段 | 说明 |
|---|---|---|---|---|
| `/admin/device/list` | GET | — | `devices[]` | 列出设备 profile |
| `/admin/region/alloc` | POST | `{"size":4096,"latency_class":"normal","capacity_class":"small"}` | `va`, `region_id`, `home_node_id`, `len` | 分配 region |
| `/admin/region/free` | POST | `{"region_id":1}` | `status` | 释放 region |
| `/admin/region/list` | GET | — | `regions[]` | 列出 region 及一致性状态 |
| `/admin/region/register-remote` | POST | 见下方 | `status` | 注册远端 region |
| `/admin/region/invalidate` | POST | `{"region_id":1,"new_epoch":2}` | `invalidated` | 使缓存失效 |
| `/admin/verb/read-va` | POST | `{"va":"0x...","offset":0,"len":64}` | `data[]`, `source`, `len` | 按虚拟地址读 |
| `/admin/verb/write-va` | POST | `{"va":"0x...","offset":0,"data":[...]}` | `status` | 按虚拟地址写 |
| `/admin/verb/acquire-writer` | POST | `{"va":"0x..."}` | `status`, `epoch`, `region_id` | 获取写锁 |
| `/admin/verb/release-writer` | POST | `{"va":"0x..."}` | `status`, `epoch`, `region_id` | 释放写锁 |

**region/alloc 请求字段：**

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `size` | u64 | 必填 | 分配大小（字节） |
| `latency_class` | String | `"normal"` | 延迟类：`"critical"` / `"normal"` / `"bulk"` |
| `capacity_class` | String | `"small"` | 容量类：`"small"` / `"large"` / `"huge"` |
| `pin` | String | null | 绑定设备类型：`"memory"` / `"npu"` / 不填 |

**region/register-remote 请求字段：**

| 字段 | 类型 | 说明 |
|---|---|---|
| `region_id` | u64 | 远端 region ID |
| `home_node_id` | u16 | Home 节点 ID |
| `device_id` | u16 | Home 节点上的设备 ID |
| `mr_handle` | u32 | Home 节点上的 MR handle |
| `base_offset` | u64 | MR 内偏移 |
| `len` | u64 | Region 长度 |
| `epoch` | u64 | 当前 epoch |

**read-va 响应 `source` 字段：**

| 值 | 含义 |
|---|---|
| `"home"` | Region 在本节点，直接从本地 MR 读取 |
| `"cache"` | Cache 命中，从本地缓存池读取 |
| `"fetch"` | Cache 未命中，从 Home 节点 FETCH 获取 |

### 6.7 可观测

| Endpoint | Method | 说明 |
|---|---|---|
| `/metrics` | GET | Prometheus 文本格式指标 |

---

## 七、E2E 自动化测试

项目提供 6 套自动化 E2E 测试脚本，自动完成编译、启动、验证、清理：

```bash
# Verbs 层基础功能
bash tests/m1_e2e.sh    # 心跳 + 成员发现
bash tests/m2_e2e.sh    # verbs read/write/atomic
bash tests/m3_e2e.sh    # Jetty send/recv/write_with_imm
bash tests/m4_e2e.sh    # 可靠传输 + 丢包恢复

# Verbs 层全功能 + KV Demo
bash tests/m5_e2e.sh    # 3 节点, 16 步验证

# Managed 层 + SWMR 一致性
bash tests/m7_e2e.sh    # 3 节点, 12 步验证
```

脚本会自动清理残留进程，无需手动干预。退出码 0 表示全部通过。

---

## 八、端口分配参考

### Verbs 层集群（configs/node{0,1,2}.yaml）

| 节点 | node_id | 控制面 (TCP) | 数据面 (UDP) | Admin/Metrics (HTTP) |
|---|---|---|---|---|
| Node 0 | 1 | 7900 | 7910 | 9090 |
| Node 1 | 2 | 7901 | 7911 | 9091 |
| Node 2 | 3 | 7902 | 7912 | 9092 |

### Managed 层集群（configs/m7_node{0,1,2}.yaml）

| 节点 | node_id | 控制面 (TCP) | 数据面 (UDP) | Admin/Metrics (HTTP) |
|---|---|---|---|---|
| Node 0 | 1 | 7920 | 7930 | 9190 |
| Node 1 | 2 | 7921 | 7931 | 9191 |
| Node 2 | 3 | 7922 | 7932 | 9192 |

---

## 九、性能调优建议

| 场景 | 建议配置 |
|---|---|
| 低延迟网络（同机房） | `transport.rto_ms: 100`，`heartbeat.interval_ms: 500` |
| 高延迟网络（跨机房） | `transport.rto_ms: 500`，`transport.max_retries: 16` |
| 大吞吐 | `flow.initial_credits: 256`，`jetty.jfs_depth: 4096` |
| 小规模测试 | 默认配置即可 |
| Managed 层大缓存 | `managed.cache.max_bytes: 536870912`（512 MiB） |
| 长写操作 | `managed.writer_lease_ms: 30000`（30 秒） |

---

## 十、故障排查

| 症状 | 可能原因 | 解决方法 |
|---|---|---|
| 集群未形成（node/list 返回 <3 节点） | 1. 端口被占用 2. peers 配置错误 3. 启动顺序太快 | 检查端口冲突：`ss -tlnp \| grep 790`；确认 peers 地址正确；Hub 先启动，等 2 秒再启动其他节点 |
| 远程写/读失败 | 1. MR 未注册 2. UB 地址格式错误 3. 对端未上线 | 先 `mr-list` 确认 MR 已注册；UB 地址为 `0x` 开头的 32 位十六进制；`node-list` 确认对端 Alive |
| region/alloc 返回错误 | 1. managed.enabled 未设 true 2. placer_node_id 指向的节点未运行 | 确认配置 `managed.enabled: true`；确认 placer 节点已启动 |
| cache hit 未命中 | 1. region 未注册远端 2. invalidation 已清除缓存 | 首次读取前先 `region/register-remote`；检查 `region/invalidate` 是否被调用 |
| 心跳超时频繁 | 网络不稳定或负载过高 | 增大 `heartbeat.interval_ms` 和 `heartbeat.fail_after` |
| 传输层丢包重传过多 | UDP 缓冲区不足 | 增大系统 UDP 缓冲区：`sysctl -w net.core.rmem_max=8388608` |
| 日志查看 | 需要调试信息 | 查看日志文件：`cat /tmp/node0.log` 或 `cat /tmp/m7_node0.log` |