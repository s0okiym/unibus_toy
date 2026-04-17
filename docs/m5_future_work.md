# M5 以后待完成工作盘点

**日期**: 2026-04-17
**当前状态**: M5 已完成（146 单元测试、M5 E2E 16 步 PASS、M4 回归 E2E PASS）

以 `docs/REQUIREMENTS.md` 功能需求和 `docs/DESIGN.md` 详细设计为基准，逐项盘点当前实现与设计文档的差距。

---

## 1. FR-REL 可靠传输（M4 延迟项）

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 1.1 | AIMD 拥塞控制 | DESIGN §6, FR-FLOW-3 | 未实现 | 当前仅有静态信用窗口，无动态窗口调整。需在 `ReliableSession` 中添加 `cwnd` 字段，成功 ACK 时加法增窗，丢包/超时时乘法减窗 |
| 1.2 | EWMA RTT 估算 + Karn 算法 | DESIGN §5 | 未实现 | RTO 固定 `rto_base_ms` + 指数退避，无自适应。需添加 `srtt`/`rttvar` 字段，在 ACK 回来时采样 RTT，Karn 规则排除重传样本 |
| 1.3 | SACK 快速重传（3 次 dup ACK 触发） | DESIGN §5 | 未实现 | SACK bitmap 已生成/解析，但发送端仅靠 RTO 超时重传。需在 `handle_ack_frame` 中统计重复 ACK，3 次 dup ACK 时立即重传丢失段 |

---

## 2. FR-MEM 内存语义

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 2.1 | 大 payload 自动分片重组 | FR-MEM-6, DESIGN §4 | 未实现 | Wire 格式已支持（`FRAG_FIRST/MIDDLE/LAST`、`frag_id/frag_index/frag_total`），但 `DataPlaneEngine` 硬限制 `MAX_PAYLOAD_SIZE=1320`，超过直接返回 `PayloadTooLarge`。需在 `ub-transport` 中实现发送端分片 + 接收端重组缓冲区（`TransportConfig.reassembly_budget_bytes` 已预留但未使用） |

---

## 3. FR-MR 内存注册与生命周期

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 3.1 | MR deregister 三态机 + inflight ref-count | DESIGN §8, REQUIREMENTS FR-MR-3 | 数据结构已有，流程未实现 | `MrState` 枚举（Active/Revoking/Released）和 `inflight_refs: AtomicI64` 已定义，但 `MrTable::deregister()` 直接移除条目，未做：① CAS state Active→Revoking ② 异步等待 inflight_refs 降为 0 ③ Notify 唤醒 ④ 超时后返回 `UB_ERR_TIMEOUT` ⑤ Revoking/Released 状态下拒绝新请求 |
| 3.2 | 远端 MR 权限检查 | FR-MR-5, DESIGN §9 | 未实现 | 本地操作已检查 `PermDenied`，但远端操作（`handle_write`/`handle_read_req`/`handle_atomic_*`）未检查 MR 权限位。需在数据面响应端添加 `entry.check_perms(verb)` 检查 |

---

## 4. FR-JETTY Jetty 无连接抽象

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 4.1 | 默认 Jetty | DESIGN §8 | 未实现 | 设计文档要求 `ub_default_jetty()` 返回 per-node 默认 Jetty，当前仅有 ad-hoc "取列表第一个" 的 workaround。需在 `JettyTable` 中添加 `default_jetty` 字段和 `ub_default_jetty()` 函数 |
| 4.2 | per-(src_jetty, dst_jetty) 排序 | FR-MSG-5 | 部分实现 | 当前传输层 `SessionKey` 为 `(local_node, remote_node, epoch)`，即同一对节点所有 Jetty 共享一个序号空间。设计要求同一 (src_jetty, dst_jetty) 有序，不同 jetty 对无序。需将 `SessionKey` 细化为 `(src_node, src_jetty, dst_node, dst_jetty, epoch)` 或使用子序号 |
| 4.3 | JFS MPSC lock-free 实现 | DESIGN §8 | 使用 Mutex\<VecDeque\> | 设计文档列出 `crossbeam::queue::ArrayQueue` 作为可选方案，当前选择简单实现。作为优化项，可在性能瓶颈明显时切换 |
| 4.4 | jetty_close 等待 in-flight WR | FR-JETTY-5 | 基本实现 | 当前 `close()` 会 flush JFS/JFR 中的条目，但不会等待已从 JFS 弹出、正在传输中的 WR 完成。完整实现需 inflight ref-count + Notify |

---

## 5. FR-CTRL 控制面

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 5.1 | Seed 模式 bootstrap | FR-CTRL-1 | 未实现 | `CtrlMsgType::Join`(0x10) 和 `MemberSnapshot`(0x05) 仅定义了 wire type 常量，无 payload 结构体、无 ControlMsg 变体、无处理逻辑。配置 schema 已有 `seed_addrs` 字段但未读取 |
| 5.2 | DEVICE_PROFILE_PUBLISH | DESIGN §7 | 未实现 | 无 opcode 定义、无 payload、无 ControlMsg 变体。此消息为 M7 Managed 层所需，用于传递设备能力画像 |
| 5.3 | Managed 层控制消息 | DESIGN §7, FR-GVA | 全部未实现 | ALLOC_REQ/RESP、REGION_CREATE/DELETE、FETCH/FETCH_RESP、WRITE_LOCK_REQ/GRANTED/UNLOCK、INVALIDATE/INVALIDATE_ACK（opcode 0x21-0x2B）均无任何代码。属 M7 范围 |
| 5.4 | Fan-out 广播（FR-CTRL-6） | FR-CTRL-6 | 部分实现 | 当前 `MemberTable::broadcast()` 可向所有 Active peer 发送，但缺少：① 订阅表 `(node_id, jetty_id)` ② fan-out 计数器 `unibus_fanout_ok_total`/`unibus_fanout_err_total` ③ per-recipient 追踪 |
| 5.5 | MemberUp 消息 | DESIGN §7 | 定义但未使用 | `ControlMsg::MemberUp` 变体存在但 `process_control_message` 中 fallthrough 到 `_ =>`，从未发送或处理 |

---

## 6. FR-API CLI 与 SDK

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 6.1 | `unibusctl node start --config` | FR-API-1 | 未实现 | 当前 `unibusctl` 仅查询/操作已运行的 daemon，不支持启动节点。需添加 `node start` 子命令（可简单包装 `std::process::Command` 启动 unibusd） |
| 6.2 | Rust SDK crate | FR-API-2 | 未实现 | 无 `ub-sdk` 或类似 crate。`ub-core` 暴露低层原语但不是应用 SDK。需新建 crate，封装 `ub_alloc`/`ub_free`/`ub_read`/`ub_write`/`ub_atomic_*`/`ub_send`/`ub_recv` 等高层 API |
| 6.3 | 同步/异步 API 区分 | FR-API-3 | 未实现 | `DataPlaneEngine` 的方法全是 async，无显式 sync 包装。SDK 需同时提供 async 原生接口和 sync 便利包装 |

---

## 7. FR-OBS 可观测性

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 7.1 | `#[tracing::instrument]` 全覆盖 | DESIGN §10 | 部分实现 | 关键路径已标注（write/read/atomic/send/recv/transport/control），但部分路径未覆盖：fabric `recv` loop、`UdpFabric::send_to`、`handle_hello`/`handle_heartbeat` 单独函数、MR 注册/注销 |
| 7.2 | Managed 层额外计数器 | DESIGN §10 | 未实现 | 设计文档列出 Managed 层计数器（`ub_alloc_total`、`ub_fetch_total`、`ub_cache_hit`、`ub_cache_miss`、`ub_invalidate_total` 等），属 M7 范围 |
| 7.3 | OpenTelemetry 兼容接口 | FR-OBS-4 | 未实现 | 当前仅有 `tracing_subscriber::fmt()` 输出，无 `tracing-opentelemetry` 集成。设计要求"默认可为 no-op，便于后续接入"，当前状态满足最低要求，但未预留扩展点 |

---

## 8. FR-DEV 设备抽象

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 8.1 | `unibusctl mr list` 区分 device type | FR-DEV-6 | 已实现 | CLI 已显示 `DeviceKind` 列 |
| 8.2 | NPU MR KV demo 变体 | DESIGN §19 | 未实现 | M5 实现计划中列为 P2 可选项。可在现有 KV demo 基础上切换 backing device 为 NPU |

---

## 9. KV Demo 扩展

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 9.1 | KV replica 通知（write_with_imm） | DESIGN §19 | 未实现 | Owner put/cas 成功后应通过 `write_with_imm` 通知 replica 节点，replica 收到 CQE 后可 `ub_read` 拉取最新数据 |
| 9.2 | KV 故障处理（owner DOWN → client error） | DESIGN §19 | 部分实现 | 数据面已有 `UB_ERR_LINK_DOWN` 返回，但 KV handler 层未对远端操作失败做用户友好的错误处理（如 "owner unreachable" 提示） |
| 9.3 | KV 跨节点 put/get/cas | DESIGN §19 | 已实现（远端路径） | KV handler 已支持远端 MR 的 verbs 路径，但 E2E 测试仅验证了本地路径 |

---

## 10. M6 多跳/源路由

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 10.1 | 3 节点链式拓扑转发 | REQUIREMENTS M6 | 未实现 | 需要路由表、源路由头解析、逐跳转发逻辑。当前仅支持全连接直连 |
| 10.2 | 源路由头解析与转发 | DESIGN §4 | 未实现 | Wire 格式中 `Opaque` 字段可承载路由信息，但未实现路由选择与转发 |

---

## 11. M7 Managed 层（全部未实现）

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 11.1 | GVA（全局虚拟地址） | FR-GVA-1 | 未实现 | `ub_alloc`/`ub_free`/`ub_read_va`/`ub_write_va` API 不存在 |
| 11.2 | 设备能力画像 + Placer | FR-GVA-2/3 | 未实现 | 无 Device Capability Registry、无 central Placer、无 cost function scoring |
| 11.3 | 无缓存直达 home | M7.2 | 未实现 | GVA region 概念、home node 映射不存在 |
| 11.4 | 本地 read cache + FETCH | M7.3 | 未实现 | 无 FETCH RPC、无 cache 命中/未命中路径 |
| 11.5 | SWMR + invalidate | M7.4 | 未实现 | 无写锁获取、无 invalidation 广播、无 coherence 状态机 |
| 11.6 | 3 节点 KV cache demo | M7.6 | 未实现 | 设计文档 §19 描述了 12 步完整 walkthrough |

---

## 12. 测试与质量

| # | 需求 | 设计文档位置 | 当前状态 | 差距说明 |
|---|------|------------|---------|---------|
| 12.1 | M1–M3 E2E 回归 | DESIGN §20 | 未验证 | 当前仅验证了 M4 回归 E2E，M1/M2/M3 的 E2E 脚本未在 M5 后重新运行 |
| 12.2 | 1% 丢包注入 E2E | DESIGN §20 | M4 验证过 | M4 有独立丢包测试，M5 未重复验证 |
| 12.3 | 多任务并发 atomic_cas 测试 | DESIGN §20 | 已有单元测试 | `test_concurrent_cas` 在 ub-core 中 |
| 12.4 | 多任务并发 ub_send 测试 | DESIGN §20 | 未实现 | 无多 Jetty 并发 send 集成测试 |
| 12.5 | MR deregister 期间 inflight 操作测试 | DESIGN §20 | 未实现 | 依赖 MR deregister 三态机实现（3.1） |
| 12.6 | 单元测试覆盖率 ≥ 60% | REQUIREMENTS §6 | 未度量 | 未配置 `cargo-tarpaulin` 或类似工具 |
| 12.7 | 1KiB write ≥ 50K ops/s (criterion) | REQUIREMENTS §6 | 未在 CI 验证 | criterion bench 存在但未在本机记录基线数据 |

---

## 13. 汇总：按优先级排列

### P0 — 设计文档明确要求但完全缺失的核心功能

| # | 功能 | 所属 FR | 里程碑 | 预估工作量 |
|---|------|---------|-------|-----------|
| 2.1 | 大 payload 分片重组 | FR-MEM-6 | M4 补充 | 中 |
| 3.1 | MR deregister 三态机 | FR-MR-3 | M4 补充 | 中 |
| 5.1 | Seed 模式 bootstrap | FR-CTRL-1 | M1 补充 | 小 |
| 6.2 | Rust SDK crate | FR-API-2 | M5 补充 | 大 |

### P1 — 设计文档要求但实现简化的功能

| # | 功能 | 所属 FR | 里程碑 | 预估工作量 |
|---|------|---------|-------|-----------|
| 1.1 | AIMD 拥塞控制 | FR-FLOW-3 | M4 补充 | 中 |
| 1.2 | EWMA RTT 估算 | FR-REL | M4 补充 | 小 |
| 1.3 | SACK 快速重传 | FR-REL | M4 补充 | 小 |
| 3.2 | 远端 MR 权限检查 | FR-MR-5 | M2 补充 | 小 |
| 4.1 | 默认 Jetty | FR-JETTY | M3 补充 | 小 |
| 4.2 | per-jetty-pair 排序 | FR-MSG-5 | M3 补充 | 中 |
| 6.1 | `unibusctl node start` | FR-API-1 | M5 补充 | 小 |
| 6.3 | 同步/异步 API 区分 | FR-API-3 | M5 补充 | 小 |
| 9.1 | KV replica 通知 | DEMO | M5 补充 | 小 |

### P2 — 优化项或设计文档提及的可选功能

| # | 功能 | 所属 FR | 里程碑 | 预估工作量 |
|---|------|---------|-------|-----------|
| 4.3 | JFS lock-free (ArrayQueue) | FR-JETTY | 优化 | 小 |
| 4.4 | jetty_close 等 inflight WR | FR-JETTY-5 | 优化 | 小 |
| 5.4 | Fan-out 订阅表 + 计数器 | FR-CTRL-6 | M5 补充 | 小 |
| 7.1 | tracing 全覆盖 | FR-OBS | M5 补充 | 小 |
| 8.2 | NPU MR KV demo | FR-DEV | M5 补充 | 小 |
| 9.2 | KV 故障处理增强 | DEMO | M5 补充 | 小 |
| 12.1 | M1–M3 E2E 回归 | TEST | 测试 | 小 |
| 12.4 | 并发 ub_send 集成测试 | TEST | 测试 | 小 |
| 12.6 | 测试覆盖率度量 | TEST | 测试 | 小 |

### M6 可选里程碑

| # | 功能 | 预估工作量 |
|---|------|-----------|
| 10.1 | 链式拓扑转发 | 大 |
| 10.2 | 源路由头解析 | 中 |

### M7 独立轨道（全部未开始）

| # | 功能 | 预估工作量 |
|---|------|-----------|
| 11.1 | GVA + ub_alloc/ub_free | 大 |
| 11.2 | 设备画像 + Placer | 大 |
| 11.3 | 无缓存直达 home (M7.2) | 中 |
| 11.4 | Read cache + FETCH (M7.3) | 大 |
| 11.5 | SWMR + invalidate (M7.4) | 大 |
| 11.6 | 3 节点 KV cache demo (M7.6) | 中 |
| 5.2 | DEVICE_PROFILE_PUBLISH | 中 |
| 5.3 | Managed 层控制消息 | 大 |
| 7.2 | Managed 层计数器 | 小 |

---

## 14. 与设计文档的覆盖率统计

### 功能需求（FR）覆盖率

| FR 编号 | 需求名称 | 完成度 | 说明 |
|---------|---------|-------|------|
| FR-ADDR | 统一编址 | **100%** | 128-bit 地址、全局唯一、本地分配、dereg 后报错均实现 |
| FR-MR | 内存注册 | **70%** | 注册/权限/广播已实现；deregister 三态机、远端权限检查缺失 |
| FR-MEM | 内存语义 | **85%** | read/write/atomic 已实现；分片重组未实现 |
| FR-MSG | 消息语义 | **90%** | send/recv/send_with_imm/write_with_imm 已实现；per-jetty-pair 排序简化 |
| FR-JETTY | Jetty 抽象 | **80%** | create/close/flushed 已实现；默认 Jetty、per-jetty 排序缺失 |
| FR-CTRL | 控制面 | **65%** | static bootstrap/heartbeat/MR publish/revote 已实现；seed 模式、fan-out 增强、managed 消息缺失 |
| FR-REL | 可靠传输 | **75%** | 序号/ACK/SACK/去重/幂等/重传已实现；AIMD/RTT 估算/快速重传缺失 |
| FR-FLOW | 流控 | **80%** | 信用流控已实现；AIMD 窗口调整缺失 |
| FR-FAIL | 故障检测 | **100%** | 心跳/Suspect→Down/CQE error/会话重建均实现 |
| FR-ERR | 错误码 | **90%** | 所有错误码已定义；MR deregister timeout 和远端权限检查缺失 |
| FR-API | CLI/SDK | **50%** | CLI 查询/bench/KV 已实现；SDK crate、node start、sync/async 区分缺失 |
| FR-OBS | 可观测 | **85%** | /metrics、计数器、tracing 已实现；OpenTelemetry 预留、全覆盖缺失 |
| FR-GVA | Managed 层 | **0%** | 全部未实现，属 M7 |
| FR-DEV | 设备抽象 | **95%** | CPU+NPU 模拟、统一编址、NPU API 已实现；NPU KV demo 变体缺失 |

**整体功能需求完成度: ~70%**（不含 M7）

### 里程碑完成度

| 里程碑 | 状态 | 补充工作 |
|-------|------|---------|
| M1 | 基本完成 | Seed 模式 bootstrap |
| M2 | 基本完成 | 远端 MR 权限检查 |
| M3 | 基本完成 | 默认 Jetty、per-jetty-pair 排序 |
| M4 | 基本完成 | 分片重组、AIMD、RTT 估算、SACK 快速重传、MR deregister 三态机 |
| M5 | 基本完成 | SDK crate、node start、KV replica 通知 |
| M6 | 未开始 | 可选 |
| M7 | 未开始 | 独立轨道 |