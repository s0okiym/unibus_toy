# M5 Implementation Plan: Observability + Benchmark + Distributed KV Demo

**Milestone**: M5 — Observability (FR-OBS) + Benchmark + Distributed KV Demo
**Date**: 2026-04-16
**Preceding Milestone**: M4 (Reliable Transport + Flow Control + Failure Detection, 142 unit tests, 4 E2E tests)

---

## 1. Prerequisites Audit

### 1.1 Functional Prerequisites

| Prerequisite | Status | Evidence |
|-------------|--------|----------|
| M1–M4 全部完成 | OK | 142 单元测试，4 个 E2E 测试全部通过 |
| Data plane 读写原子操作 | OK | Write/Read/AtomicCas/AtomicFaa 跨节点正常 |
| Message semantics | OK | Send/Recv/Write_with_Imm 跨节点正常 |
| Reliable transport | OK | 1% 丢包下丢帧重传、至多一次语义验证通过 |
| Flow control | OK | Credit 窗口流控，信用耗尽返回 NoResources |
| Failure detection | OK | 节点 Down 检测、Suspect→Down 状态迁移 |
| Peer restart + epoch | OK | 重启后 epoch 变化、会话重建、操作恢复 |
| MR publish/resolve | OK | MR_PUBLISH 跨节点传播，mr_cache 可查询 |
| Jetty + CQE | OK | Send/Recv/WriteImm CQE 正确推送和轮询 |

### 1.2 Infrastructure Prerequisites

| Prerequisite | Status | Gap | Action |
|-------------|--------|-----|--------|
| `ub-obs` crate | 壳存在 | 无实现代码 | Step 5.1 实现 |
| `metrics` + `metrics-exporter-prometheus` 依赖 | 已声明 | 未调用 | Step 5.1 集成 |
| `tracing` + `tracing-subscriber` 依赖 | 已声明 | 仅做日志输出，无 `#[instrument]` | Step 5.2 添加 |
| `criterion` 依赖 | 缺失 | 未声明 | Step 5.3 添加 |
| `/metrics` HTTP 端点 | 不存在 | admin API 无此路由 | Step 5.1 添加 |
| `ObsConfig.metrics_listen` | 已有 | 配置字段已解析，端口 9090/9091 | Step 5.1 接入 |
| 3 节点配置文件 | 缺失 | 仅有 node0.yaml, node1.yaml | Step 5.5 创建 node2.yaml |
| KV demo 代码 | 不存在 | 无任何 KV 相关代码 | Step 5.4 实现 |
| `unibusctl bench` 子命令 | 不存在 | CLI 无 bench 相关代码 | Step 5.3 实现 |
| `unibusctl kv-*` 子命令 | 不存在 | CLI 无 KV 相关代码 | Step 5.4 实现 |
| NPU device 后端 | 已有 | ub-core device 支持模拟 NPU | KV demo 可选变体 |

### 1.3 结论

**所有功能先决条件已满足。** 基础设施方面需补充：ub-obs 实现代码、criterion 依赖、3 节点配置、`/metrics` 端点、bench/KV CLI 子命令。无阻塞项。

---

## 2. Scope and Feature List

### 2.1 FR-OBS 功能需求覆盖

| FR | 需求 | M5 交付 |
|----|------|---------|
| FR-OBS-1 | 每节点暴露计数器：tx_pkts/rx_pkts/retrans/drops/cqe_ok/cqe_err/mr_count/jetty_count；mr_count 按 device 类型细分 | Step 5.1 |
| FR-OBS-2 | 日志级别 DEBUG/INFO/WARN/ERROR，关键事件可追踪 | Step 5.2（已有基础，增强） |
| FR-OBS-3 | HTTP `/metrics` 端点，Prometheus text 格式 | Step 5.1 |
| FR-OBS-4 | tracing hooks（OpenTelemetry 兼容接口或最小 Span 回调）；默认实现可为 no-op | Step 5.2 |

### 2.2 FR-API 功能需求覆盖

| FR | 需求 | M5 交付 |
|----|------|---------|
| FR-API-1 | `unibusctl bench read\|write\|send` 内置简单微基准 | Step 5.3 |

### 2.3 Performance Requirements (FR-NFR)

| 需求 | 指标 | 验证方式 |
|------|------|----------|
| 单节点对 1KiB write 吞吐 | >= 50K ops/s | Step 5.3 criterion bench |
| 本地 loopback P50 延迟 | < 200µs | Step 5.3 criterion bench |

### 2.4 Feature List

| # | Feature | 依赖 | 优先级 |
|---|---------|------|--------|
| F1 | ub-obs 计数器定义 + increment 宏 | 无 | P0 |
| F2 | `/metrics` HTTP 端点 (Prometheus) | F1 | P0 |
| F3 | 各层计数器埋点（fabric/transport/dataplane/control） | F1 | P0 |
| F4 | `#[tracing::instrument]` 关键路径标注 | 无 | P1 |
| F5 | tracing span 结构化输出 | F4 | P1 |
| F6 | `criterion` benchmark 框架搭建 | F3 | P0 |
| F7 | `unibusctl bench write/read/send` 子命令 | F6 | P0 |
| F8 | 分布式 KV demo 服务端（put/get/cas） | 无 | P0 |
| F9 | 分布式 KV demo 客户端（unibusctl kv-* 子命令） | F8 | P0 |
| F10 | KV replica 通知（write_with_imm） | F8 | P1 |
| F11 | KV demo 故障处理（节点 DOWN → UB_ERR_LINK_DOWN） | F8 | P1 |
| F12 | 3 节点 E2E 测试脚本 | F1-F11 | P0 |
| F13 | NPU MR KV demo 变体（可选） | F8 | P2 |

---

## 3. Implementation Steps

### Step 5.1: ub-obs 计数器 + /metrics 端点

**目标**：实现 FR-OBS-1 和 FR-OBS-3。

#### 5.1.1 计数器定义 (`crates/ub-obs/src/metrics.rs`)

定义全局计数器/仪表常量和 increment 宏：

```rust
// Counter names (Prometheus naming convention)
pub const TX_PKTS: &str = "unibus_tx_pkts_total";
pub const RX_PKTS: &str = "unibus_rx_pkts_total";
pub const RETRANS: &str = "unibus_retrans_total";
pub const DROPS: &str = "unibus_drops_total";
pub const CQE_OK: &str = "unibus_cqe_ok_total";
pub const CQE_ERR: &str = "unibus_cqe_err_total";
pub const MR_COUNT: &str = "unibus_mr_count";
pub const JETTY_COUNT: &str = "unibus_jetty_count";
pub const PEER_RTT_MS: &str = "unibus_peer_rtt_ms";

// Helper macros for ergonomic counter updates
// increment_counter!(TX_PKTS);
// increment_counter!(MR_COUNT, "device" => "memory");
// record_histogram!(PEER_RTT_MS, rtt_ms);
```

- Counters: `tx_pkts`, `rx_pkts`, `retrans`, `drops`, `cqe_ok`, `cqe_err` — 单调递增
- Gauges: `mr_count{device="memory|npu"}`, `jetty_count` — 可增可减
- Histogram: `peer_rtt_ms` — 延迟分布（可选，为未来 RTT 估算预留）

#### 5.1.2 /metrics 端点 (`crates/ub-obs/src/server.rs`)

使用 `metrics-exporter-prometheus` 提供 Prometheus text format 输出。

集成方式：
- `metrics-exporter-prometheus` 的 `PrometheusBuilder::new()` 启动内部 HTTP 服务
- 或使用 `axum` 路由 `/metrics`，在 handler 中调用 `prometheus_handle.render()`
- 绑定到 `ObsConfig.metrics_listen`（默认 `127.0.0.1:9090`）

**决策**：与 admin API 共端口。admin API 已在 `metrics_listen` 地址上运行 axum，添加 `/metrics` 路由即可。避免额外端口。

#### 5.1.3 各层计数器埋点

| Layer | File | Counter | 位置 |
|-------|------|---------|------|
| fabric | `ub-fabric/src/udp.rs` | `tx_pkts`, `rx_pkts`, `drops` | `send_to()` / `recv` loop |
| transport | `ub-transport/src/manager.rs` | `retrans`, `tx_pkts`, `rx_pkts` | `send()` / `handle_incoming()` / RTO retransmit |
| transport | `ub-transport/src/manager.rs` | `peer_rtt_ms` | ACK 处理时计算（预留） |
| dataplane | `ub-dataplane/src/lib.rs` | `cqe_ok`, `cqe_err` | `push_cqe()` 后 / error 路径 |
| control | `ub-control/src/control.rs` | (无额外计数器，已有 tracing) | — |
| unibusd | `unibusd/src/main.rs` | `mr_count`, `jetty_count` | MR/Jetty 注册/注销时更新 |

**依赖变更**：
- `ub-fabric/Cargo.toml` 添加 `ub-obs`
- `ub-transport/Cargo.toml` 添加 `ub-obs`
- `ub-dataplane/Cargo.toml` 添加 `ub-obs`
- `ub-obs/Cargo.toml` 确保依赖完整

**测试**（3 个）：
- `test_metrics_increment_counter` — increment 后 counter 值正确
- `test_metrics_gauge_with_labels` — `mr_count{device="memory"}` 标签正确
- `test_prometheus_render` — render 输出包含 `unibus_` 前缀指标

---

### Step 5.2: tracing 钩子

**目标**：实现 FR-OBS-2 和 FR-OBS-4。

#### 5.2.1 `#[tracing::instrument]` 关键路径标注

在以下关键函数添加 `#[tracing::instrument(skip(...))]`：

| Layer | Function | Skip 参数 | 级别 |
|-------|----------|-----------|------|
| control | `ControlPlane::handle_hello` | raw bytes | info |
| control | `ControlPlane::handle_heartbeat` | — | debug |
| control | `ControlPlane::mark_suspect/mark_down/mark_active` | — | warn/info |
| transport | `TransportManager::send` | frame bytes | debug |
| transport | `TransportManager::handle_incoming` | raw bytes | debug |
| transport | `TransportManager::handle_data_frame` | payload | debug |
| transport | `TransportManager::process_ack` | — | debug |
| dataplane | `DataPlaneEngine::handle_verb_frame` | payload | debug |
| dataplane | `DataPlaneEngine::ub_write_remote` | data | info |
| dataplane | `DataPlaneEngine::ub_read_remote` | — | info |
| dataplane | `DataPlaneEngine::ub_atomic_cas_remote` | — | info |
| fabric | `UdpFabric::send_to` | data | trace |

#### 5.2.2 tracing subscriber 初始化

已有 `tracing_subscriber::fmt()` 初始化。增强：
- 确认 `RUST_LOG=debug` 时关键事件可追踪
- 验证 `RUST_LOG=unibusd=info,ub_transport=debug` 精确过滤
- 为 OpenTelemetry 预留扩展点（`tracing-opentelemetry` 可选依赖，M5 不实现）

**依赖变更**：无新增依赖。

**测试**（2 个）：
- `test_tracing_instrument_attribute` — 编译验证 `#[instrument]` 属性
- `test_structured_log_output` — RUST_LOG=debug 时 stderr 包含 span 信息

---

### Step 5.3: criterion benchmark

**目标**：实现 FR-API-1 和性能指标验证。

#### 5.3.1 criterion 依赖和框架

```toml
# Cargo.toml (workspace)
criterion = { version = "0.5", features = ["html_reports"] }

# crates/ub-dataplane/Cargo.toml
[dev-dependencies]
criterion = { workspace = true }

[[bench]]
name = "verb_bench"
harness = false
```

#### 5.3.2 benchmark 用例 (`crates/ub-dataplane/benches/verb_bench.rs`)

| Benchmark | 操作 | 目标 |
|-----------|------|------|
| `bench_write_1kib` | 1KiB write loopback | >= 50K ops/s |
| `bench_read_1kib` | 1KiB read loopback | — |
| `bench_atomic_cas` | CAS loopback | — |
| `bench_atomic_faa` | FAA loopback | — |
| `bench_send_recv` | Send+Recv loopback | — |

每个 benchmark：
1. 创建两个 DataPlaneEngine（loopback UDP）
2. 注册 MR、publish、建立连接和 session
3. 循环执行操作，测量吞吐和延迟

**注意**：criterion bench 需要 async runtime。使用 `tokio::runtime::Runtime` 在 bench 函数内创建。

#### 5.3.3 `unibusctl bench` 子命令

```bash
unibusctl bench write --size 1024 --count 10000
unibusctl bench read --size 1024 --count 10000
unibusctl bench send --size 64 --count 10000
```

实现方式：
- `unibusctl bench` 通过 admin API 循环调用 `verb/write`、`verb/read`、`verb/send`
- 测量总耗时，计算 ops/s 和平均延迟
- 输出：`Wrote 10000 x 1024B in 0.234s (42735 ops/s, avg 23.4µs)`

**依赖变更**：workspace `Cargo.toml` 添加 `criterion`。

**测试**（2 个）：
- `bench_write_1kib` — criterion bench 通过
- `bench_read_1kib` — criterion bench 通过

（criterion bench 本身即测试，不额外写 unit test）

---

### Step 5.4: 分布式 KV Demo

**目标**：实现需求文档 §19.1 分布式 KV demo。

#### 5.4.1 KV Demo 架构

```
┌─────────────┐    ub_write / ub_read / ub_atomic_cas    ┌─────────────┐
│  KV Client   │ ────────────────────────────────────────▶ │  KV Server   │
│ (unibusctl)  │                                         │  (owner)     │
└─────────────┘                                         └──────┬───────┘
      │                                                        │
      │           write_with_imm / send (replica notify)       │
      │                                                        ▼
      │                                                  ┌─────────────┐
      └──────────────────────────────────────────────────│  Replica     │
              ub_read (stale read)                        │  (observer) │
                                                          └─────────────┘
```

- **Owner 节点**：注册 MR 作为 KV 存储，处理 put/get/cas
- **Replica 节点**：通过 `write_with_imm` 接收更新通知
- **Client**：通过 admin API 发起 put/get/cas 操作

#### 5.4.2 KV 存储模型

简化版 KV：固定 slot 数量，每个 slot 存储 (key: [u8; 32], value: [u8; 64], version: u64)。

```
Slot layout in MR (each slot = 104 bytes):
  key: [u8; 32]     // offset 0
  value: [u8; 64]   // offset 32
  version: u64      // offset 96 (native endian)
```

操作映射：

| KV 操作 | UB Verb | 描述 |
|---------|---------|------|
| `put(key, value)` | `ub_write` | 写入 key + value + version 到 owner MR |
| `get(key)` | `ub_read` | 读取 slot，匹配 key，返回 value |
| `cas(key, expect_version, new_value)` | `ub_atomic_cas` | CAS version，成功则写入 new_value |

#### 5.4.3 Admin API 端点

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/admin/kv/init` | POST | 初始化 KV 存储：注册 MR，写入初始 slot |
| `/admin/kv/put` | POST | KV put：`{ub_addr, slot, key, value}` |
| `/admin/kv/get` | POST | KV get：`{ub_addr, slot, key}` |
| `/admin/kv/cas` | POST | KV cas：`{ub_addr, slot, key, expect_version, new_value}` |

#### 5.4.4 `unibusctl kv-*` 子命令

```bash
unibusctl kv-init [--slots 16]                           # 在目标节点初始化 KV 存储
unibusctl kv-put <addr> <slot> <key> <value>             # KV put
unibusctl kv-get <addr> <slot> <key>                     # KV get
unibusctl kv-cas <addr> <slot> <key> <expect> <new_value> # KV cas
```

#### 5.4.5 Replica 通知

Owner 节点执行 `put` 或 `cas` 成功后，通过 `write_with_imm` 通知 Replica 节点：
- `imm` = slot index
- Replica 收到 CQE 后，可通过 `ub_read` 拉取最新数据

这是可选增强（P1），先实现 put/get/cas 核心逻辑。

#### 5.4.6 故障处理

- Owner 节点 DOWN → client 的 `ub_write/ub_read/ub_atomic_cas` 返回 `UB_ERR_LINK_DOWN`
- Client 打印错误信息，不 panic
- E2E 测试验证此行为

**文件变更**：

| File | Change |
|------|--------|
| `crates/unibusd/src/main.rs` | 添加 4 个 KV admin 路由和 handler |
| `crates/unibusctl/src/main.rs` | 添加 `KvInit`, `KvPut`, `KvGet`, `KvCas` 子命令 |

**测试**（3 个）：
- `test_kv_init_and_put_get` — 初始化 + put + get 数据正确
- `test_kv_cas_correctness` — CAS 版本递增，旧版本 CAS 失败
- `test_kv_cas_at_most_once` — 并发 CAS 结果一致（至多一次语义）

---

### Step 5.5: 3 节点 E2E 测试脚本

**目标**：端到端验证 M5 所有功能 + M1–M4 回归。

#### 5.5.1 3 节点配置

新建 `configs/node2.yaml`：
- node_id: 3
- control listen: 0.0.0.0:7902, peer: 127.0.0.1:7900
- data listen: 0.0.0.0:7912
- obs metrics_listen: 127.0.0.1:9092

#### 5.5.2 E2E 测试步骤 (`tests/m5_e2e.sh`)

| Step | 测试 | 验证 |
|------|------|------|
| 1 | 启动 3 个 unibusd 节点 | 全部可见 |
| 2 | 等待 HELLO 交换 | 每节点看到 3 个节点 |
| 3 | M1-M4 回归 | node list / write+read / atomic / send+recv |
| 4 | /metrics 端点验证 | `curl :9090/metrics` 包含 `unibus_tx_pkts_total` 等 |
| 5 | 操作后计数器变化 | write 操作后 `unibus_cqe_ok_total` 递增 |
| 6 | KV init (node0) | MR 注册 + slot 初始化 |
| 7 | KV put (node1 → node0) | 写入 key-value |
| 8 | KV get (node1 → node0) | 读回数据正确 |
| 9 | KV cas (node1 → node0) | CAS 成功/失败正确 |
| 10 | KV replica 通知 (node0 → node2) | write_with_imm 通知到达 |
| 11 | 杀死 owner (node0) | node1 的 KV 操作返回 error |
| 12 | 重启 node0 | 3 节点恢复，KV 操作正常 |
| 13 | `unibusctl bench write --size 1024 --count 100` | 输出 ops/s |
| 14 | 清理 | 全部进程退出 |

**测试**（1 个 E2E 脚本）：
- `tests/m5_e2e.sh` — 14 步完整生命周期测试

---

## 4. Dependency Graph

```
Step 5.1 (ub-obs 计数器 + /metrics) ──────────────────┐
                                                        │
Step 5.2 (tracing 钩子) ──────────────────────────────┤
                                                        │
                     ┌──────────────────────────────────┘
                     ▼
Step 5.3 (criterion benchmark + unibusctl bench) ──────┐
                                                        │
Step 5.4 (KV demo + unibusctl kv-*) ──────────────────┤
                     │                                  │
                     ▼                                  ▼
Step 5.5 (3 节点 E2E 测试脚本) ◄────────────────────────┘
```

Step 5.1 和 5.2 可并行。5.3 依赖 5.1（metrics 埋点用于 bench 验证）。5.4 独立于 5.3 可并行。5.5 依赖全部。

---

## 5. File Summary

### 5.1 New Files

| File | Purpose | Est. Lines |
|------|---------|-----------|
| `crates/ub-obs/src/metrics.rs` | 计数器定义 + increment 宏 + Prometheus render | ~150 |
| `crates/ub-obs/src/server.rs` | /metrics axum handler | ~50 |
| `crates/ub-dataplane/benches/verb_bench.rs` | criterion benchmark | ~200 |
| `configs/node2.yaml` | 第 3 个节点配置 | ~36 |
| `tests/m5_e2e.sh` | M5 E2E 测试脚本 | ~350 |

### 5.2 Modified Files

| File | Changes |
|------|---------|
| `crates/ub-obs/src/lib.rs` | 添加 `pub mod server` |
| `crates/ub-obs/Cargo.toml` | 确认依赖完整 |
| `crates/ub-fabric/Cargo.toml` | 添加 `ub-obs` 依赖 |
| `crates/ub-fabric/src/udp.rs` | 添加 tx_pkts/rx_pkts/drops 计数器 |
| `crates/ub-transport/Cargo.toml` | 添加 `ub-obs` 依赖 |
| `crates/ub-transport/src/manager.rs` | 添加 retrans/tx_pkts/rx_pkts 计数器 + `#[instrument]` |
| `crates/ub-dataplane/Cargo.toml` | 添加 `ub-obs` 依赖 + `criterion` dev-dep + `[[bench]]` |
| `crates/ub-dataplane/src/lib.rs` | 添加 cqe_ok/cqe_err 计数器 + `#[instrument]` |
| `crates/ub-control/src/control.rs` | 添加 `#[instrument]` |
| `crates/unibusd/src/main.rs` | 添加 `/metrics` 路由 + KV admin 路由 + mr_count/jetty_count gauge + credit return |
| `crates/unibusctl/src/main.rs` | 添加 `Bench`, `KvInit`, `KvPut`, `KvGet`, `KvCas` 子命令 |
| `Cargo.toml` | workspace 添加 `criterion` |

---

## 6. Test Plan

### 6.1 Unit Tests

| Step | 新增测试 | 数量 | 验证内容 |
|------|---------|------|----------|
| 5.1 | ub-obs 计数器测试 | 3 | counter increment、gauge label、Prometheus render |
| 5.2 | tracing 属性编译测试 | 2 | `#[instrument]` 编译通过、structured log 输出 |
| 5.4 | KV demo 测试 | 3 | put+get 正确、CAS 版本语义、CAS 至多一次 |

**预计新增 8 个单元测试，M5 完成后总测试数 >= 150。**

### 6.2 Benchmark Tests

| Benchmark | 目标 | 验证 |
|-----------|------|------|
| `bench_write_1kib` | >= 50K ops/s | `cargo bench` 报告 |
| `bench_read_1kib` | 记录基线 | `cargo bench` 报告 |
| `bench_atomic_cas` | 记录基线 | `cargo bench` 报告 |
| `bench_atomic_faa` | 记录基线 | `cargo bench` 报告 |
| `bench_send_recv` | 记录基线 | `cargo bench` 报告 |

### 6.3 E2E Tests

| Test | Steps | 验证 |
|------|-------|------|
| `tests/m1_e2e.sh` | 回归 | M1 功能不受影响 |
| `tests/m2_e2e.sh` | 回归 | M2 功能不受影响 |
| `tests/m3_e2e.sh` | 回归 | M3 功能不受影响 |
| `tests/m4_e2e.sh` | 回归 | M4 功能不受影响 |
| `tests/m5_e2e.sh` | 14 步 | 全部 M5 功能 + 回归 |

### 6.4 Functional Completeness Checklist

到 M5 截止，以下能力必须全部可演示：

#### 数据面（M2–M4 交付）

- [x] MR register/deregister
- [x] Cross-node write + read
- [x] Cross-node atomic CAS + FAA
- [x] Jetty send/recv
- [x] Write_with_imm
- [x] Reliable transport (1% loss resilience)
- [x] Flow control (credit window)
- [x] Failure detection (peer Down)
- [x] Peer restart + session rebuild

#### 可观测性（M5 交付）

- [ ] `/metrics` 端点输出 Prometheus 格式
- [ ] `unibus_tx_pkts_total` / `unibus_rx_pkts_total` 递增
- [ ] `unibus_retrans_total` 在丢包时递增
- [ ] `unibus_cqe_ok_total` / `unibus_cqe_err_total` 递增
- [ ] `unibus_mr_count{device="memory|npu"}` 正确
- [ ] `unibus_jetty_count` 正确
- [ ] `RUST_LOG=debug` 关键事件可追踪
- [ ] `#[tracing::instrument]` 关键路径标注

#### 性能（M5 交付）

- [ ] 1KiB write 吞吐 >= 50K ops/s (loopback)
- [ ] 1KiB write P50 延迟 < 200µs (loopback)
- [ ] `unibusctl bench write --size 1024` 输出 ops/s

#### KV Demo（M5 交付）

- [ ] `unibusctl kv-init` 初始化 KV 存储
- [ ] `unibusctl kv-put` 写入 key-value
- [ ] `unibusctl kv-get` 读回正确
- [ ] `unibusctl kv-cas` CAS 版本语义正确
- [ ] Owner 节点 DOWN → 客户端收到错误
- [ ] Replica 通知（write_with_imm）到达

#### 完整性

- [ ] 3 节点 E2E 脚本通过
- [ ] M1–M4 全部 E2E 回归通过
- [ ] 单元测试覆盖率 >= 60%（cargo test 统计）
- [ ] 无内存泄漏（进程退出后无残留）

### 6.5 Performance Verification Method

1. **criterion bench**: `cargo bench` 自动生成报告（吞吐 + 延迟分位数）
2. **unibusctl bench**: 端到端压测（通过 admin API，包含网络开销）
3. **/metrics 验证**: 操作后 `curl /metrics` 确认计数器递增

性能目标注意：
- criterion bench 测量的是纯 Rust 函数开销（含 UDP loopback），不包含 admin API HTTP 开销
- `unibusctl bench` 测量的是端到端（HTTP + 事件循环 + UDP），ops/s 会低于 criterion
- 验收以 criterion bench 50K ops/s 为准；`unibusctl bench` 作为端到端参考数据

---

## 7. Risk Assessment

| 风险 | 影响 | 缓解措施 |
|------|------|---------|
| criterion async bench 复杂 | bench 框架搭建延迟 | 使用 `tokio::runtime::Runtime` 包装，参考 criterion tokio 示例 |
| admin API HTTP 开销拉低 bench 数据 | 50K ops/s 目标无法通过 `unibusctl bench` | 明确 criterion bench 和 CLI bench 的区别；以 criterion 为验收标准 |
| KV demo slot 冲突 | 并发 put 写同一 slot 数据混乱 | E2E 测试使用不同 slot；并发 CAS 测试使用 atomic_cas 保护 |
| 3 节点 E2E 测试时序 | 14 步脚本可能 flaky | 增加足够等待时间；关键步骤加 retry 逻辑 |
| ub-obs 依赖循环 | ub-obs 依赖 ub-core，ub-fabric 依赖 ub-obs，ub-fabric 依赖 ub-core | ub-obs 仅提供宏和常量，不依赖其他 UB crate；计数器调用方依赖 ub-obs 即可 |
| metrics 初始化时序 | /metrics 端点在计数器埋点之前启动 | 先注册所有计数器（describe），再启动 HTTP 端点 |

---

## 8. Verification Commands

```bash
# Step 5.1 完成后
curl http://127.0.0.1:9090/metrics | grep unibus_

# Step 5.2 完成后
RUST_LOG=debug unibusctl --config node0.yaml 2>&1 | grep "handle_hello"

# Step 5.3 完成后
cargo bench
unibusctl bench write --size 1024 --count 1000

# Step 5.4 完成后
unibusctl kv-init
unibusctl kv-put <addr> 0 mykey myvalue
unibusctl kv-get <addr> 0 mykey
unibusctl kv-cas <addr> 0 mykey 1 newvalue

# Step 5.5 完成后
cargo test                    # >= 150 tests
bash tests/m1_e2e.sh          # M1 regression
bash tests/m2_e2e.sh          # M2 regression
bash tests/m3_e2e.sh          # M3 regression
bash tests/m4_e2e.sh          # M4 regression
bash tests/m5_e2e.sh          # M5 E2E — PASS
```

---

## 9. Milestone Progress

| Milestone | Status | Unit Tests | E2E | Benchmark |
|-----------|--------|-----------|-----|-----------|
| M1 | DONE | — | PASS | — |
| M2 | DONE | 81 | PASS | — |
| M3 | DONE | 96 | PASS | — |
| M4 | DONE | 142 | PASS | — |
| **M5** | **PLAN** | **>= 150** | **PASS (5 scripts)** | **50K ops/s** |
| M6 | — | — | — | — |
| M7 | — | — | — | — |