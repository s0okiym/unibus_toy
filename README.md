# UniBus Toy

一个纯软件实现的、面向 Scale-Up 域的统一编址互联协议玩具实现。灵感来源：华为灵衢（UnifiedBus / UB）。

---

## 背景

AI 大模型时代，"Scale-Up 域"（一个紧耦合的计算单元集合，对外像一台超大服务器）成为新的设计中心。华为灵衢（UB）协议把 CPU / NPU / GPU / 内存 / SSD / 交换芯片统一在一套互联协议下，做到**全局统一编址 + 内存语义/消息语义融合 + 对等访问**，并在海思芯片里做了硬件实现。

本项目用**纯软件**方式，做一个"麻雀虽小五脏俱全"的 UB-like 协议 Toy，以**在软件范畴内尽量逼近性能上限**为目标，重点走通从地址翻译到端到端语义的全链路。

---

## 功能概览

| 能力 | 说明 |
|---|---|
| **统一编址** | 128 bit UB 地址，`[PodID:16 \| NodeID:16 \| DeviceID:16 \| Offset:64 \| Reserved:16]`，全局唯一 |
| **内存语义（单边）** | `ub_read` / `ub_write` / `ub_atomic_cas` / `ub_atomic_faa`，远端 CPU 不参与 |
| **消息语义（双边）** | `ub_send` / `ub_recv` / `ub_write_with_imm` |
| **Jetty 抽象** | 无连接的 JFS（发送队列）/ JFR（接收队列）/ JFC（完成队列）|
| **多类型 Device** | CPU 内存 + 模拟 NPU 显存共存，远端访问完全透明 |
| **可靠传输** | 在 UDP/TCP 之上自实现序号、ACK/SACK、重传、去重 |
| **流控** | 信用（credit）流控 + 速率反压 |
| **故障检测** | 心跳 + 秒级链路故障感知，JFC 交付明确错误码 |
| **CLI / SDK** | `unibusd` 守护进程 + `unibusctl` 命令行 + 最小客户端 SDK |
| **可观测** | 日志 / 计数器 / `tracing` span / HTTP `/metrics`（Prometheus 格式）|
| **Managed 层（M7）** | 在 verbs 之上叠"节点无感"全局虚拟地址层，`ub_alloc` 自动 placement + region 级缓存 + SWMR 失效 |

---

## 架构

```
┌─────────────────────────────────────────────────────┐
│ Application                                         │
│  ├─ 分布式 KV demo（verbs 层版，M5 交付）           │
│  └─ 3 节点 KV cache demo（managed 层版，M7 交付）   │
├─────────────────────────────────────────────────────┤
│ ub-managed  (M7 / exploratory)                      │
│   Global allocator · Placer · Region table          │
│   Coherence SM · Cache tier · LRU eviction          │
├─────────────────────────────────────────────────────┤
│ ub-core  verbs / jetty / mr / addr / device         │
├─────────────────────────────────────────────────────┤
│ ub-transport   序号 · ACK/SACK · 重传 · 分片重组    │
├─────────────────────────────────────────────────────┤
│ ub-fabric      UDP(默认) / TCP / UDS Fabric trait   │
└─────────────────────────────────────────────────────┘
          ▲
          │ ub-control   hub · 心跳 · MR 目录 · 成员关系
```

- **M1–M5 可独立交付演示**：verbs API（`ub_read/write/atomic/send` + NodeID 编址）不依赖 managed 层。
- **Managed 层是叠加上层**：`ub_alloc` 内部调用 verbs 层接口，verbs 层不感知 managed 层存在。
- **控制面横跨两层**：verbs 的 `MR_PUBLISH/HEARTBEAT` 与 managed 的 `ALLOC_REQ/INVALIDATE` 共用同一 `ub-control` 通道。

---

## 技术选型

| 项 | 选择 |
|---|---|
| 实现语言 | **Rust + tokio**（多线程 runtime） |
| 配置格式 | YAML |
| 默认 Fabric | UDP（可选 TCP / UDS） |
| 可观测端点 | HTTP `/metrics`（Prometheus）+ `/admin`（管理 API） |
| 序列化 | 手写 `byteorder` 二进制编解码，不依赖 protobuf |

---

## 文档

| 文档 | 说明 |
|---|---|
| [REQUIREMENTS.md](./REQUIREMENTS.md) | 功能需求、用户故事、验收标准、里程碑定义 |
| [DESIGN.md](./DESIGN.md) | 详细设计：Part I verbs 层（M1–M5）/ Part II managed 层（M7）/ Part III 测试与里程碑 |

---

## 里程碑

| 里程碑 | 主要内容 |
|---|---|
| **M1** | `ub-fabric` UDP/TCP/UDS；`ub-control` 心跳与节点状态机；`unibusd` 骨架 |
| **M2** | `ub-core` MR / addr / device（CPU + NPU 模拟后端）；verbs read/write/atomic；端到端测试 |
| **M3** | Jetty / send / recv / write_with_imm |
| **M4** | 完整可靠传输（FR-REL）+ 流控（FR-FLOW）+ 故障处理（FR-FAIL）|
| **M5** | 可观测（FR-OBS）+ benchmark + 分布式 KV demo |
| **M6**（可选）| 多跳 / 源路由 / 简化 nD-Mesh 拓扑 |
| **M7**（vision）| Managed 层：device capability registry → placer → region cache → SWMR 一致性协议 → 3 节点 KV cache demo |

---

## 快速上手（计划，实现后更新）

```bash
# 启动 3 节点 SuperPod（hub 模式）
unibusd --config node0.yaml &
unibusd --config node1.yaml &
unibusd --config node2.yaml &

# 查看拓扑
unibusctl node list

# 注册 MR 并读写
unibusctl mr register --node 1 --size 1048576
unibusctl write --addr <ub_addr> --data "hello"
unibusctl read  --addr <ub_addr> --len 5

# 查看指标
curl http://127.0.0.1:9090/metrics
```

---

## 许可证

MIT
