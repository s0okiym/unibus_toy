# UniBus Toy 需求文档

> 一个纯软件实现的、面向 Scale-Up 域的统一编址互联协议玩具实现。
> 灵感来源：华为灵衢（UnifiedBus / UB）。

| 项 | 值 |
|---|---|
| **文档状态** | Draft（待评审） |
| **最后更新** | 2026-04-11 |

---

## 1. 项目背景

传统数据中心计算系统里，"总线" 与 "网络" 是两套割裂的技术栈：

- **机箱内**：PCIe / NVLink / CXL —— 追求极致带宽与延迟，主从架构，编程模型偏 load/store。
- **机箱外**：Ethernet / InfiniBand / RoCE —— 追求横向扩展，消息语义，编程模型偏 send/recv。

AI 大模型时代，单卡再快也撑不起一个万亿参数模型的训练；"Scale-Up 域"（一个紧耦合的计算单元集合，对外像一台超大服务器）成为新的设计中心。华为在 2025 年发布的灵衢（UB）协议，把 CPU / NPU / GPU / 内存 / SSD / 交换芯片统一在一套互联协议下，做到 **全局统一编址** + **内存语义/消息语义融合** + **对等访问**，并且在海思芯片里做了硬件实现。

本项目希望用 **纯软件** 的方式，做一个"麻雀虽小五脏俱全"的 UB-like 协议实现，作为学习与验证用的 Toy，但仍以 **在 N 卡 + Mellanox 等组合下尽量逼近软件可达的性能上限** 为目标；重点是走通从地址翻译到端到端语义，同时在设计上优先选择高效路径。

---

## 2. 项目目标

### 2.1 In-Scope（要做）

1. **Scale-Up 域内的统一编址**：在一个由若干"节点进程"组成的逻辑超节点（SuperPod）里，所有可被访问的资源（内存段、设备）拥有一个全局唯一的 UB 地址；任意节点可以根据 UB 地址直接访问对端资源。
2. **两套语义**：
   - 内存语义（单边）：Read / Write / Atomic（CAS / FAA）
   - 消息语义（双边）：Send / Recv，并支持 Write-with-Immediate
3. **无连接的 Jetty 抽象**：模仿 UB Jetty 的设计，提供 JFS（发送队列）/ JFR（接收队列）/ JFC（完成队列），不需要预先建立 QP 连接；"无连接"仅指 API 不做握手/绑定，不排斥在单条 TCP/UDS 上复用多 Jetty。
4. **拓扑发现与路由**：节点上线后能自动加入超节点、上报本地资源、与其他节点建立可达；首版默认 **全连接或星形** 拓扑，不要求多跳转发；源路由字段预留为扩展（M6）。
5. **可靠传输**：在 UDP / TCP 之上自实现轻量重传、序号、ACK，模拟 UB 的链路-传输层可靠性；底层只要求具备"发送/接收数据报"能力，UDP 可能丢包/乱序由上层去重，TCP 作为备用路径时也复用同一 Jetty 语义。
6. **基本的流控与拥塞处理**：信用（credit）流控 + 简单的速率反压。
7. **故障检测**：心跳 + 链路故障感知，秒级（toy 级别）通知上层。
8. **CLI / SDK**：提供命令行工具和一个最小的客户端 SDK，能跑通 demo（远端 alloc、写值、读回、原子加、双边发消息；**广播/多播**见 FR-CTRL-6，首版可为「向已知 Jetty 列表逐点发送」的简化实现）。
9. **可观测**：基本的日志、计数器、tracing 钩子。

### 2.2 Out-of-Scope（不做）

- 真实硬件协议帧、SerDes、PHY、物理层时序。
- 不承诺等同硬件 UB、RDMA、NVLink 的纳秒级延迟 / TB/s 带宽，但会优先利用 RoCE / NVLink / RDMA NIC 能提供的加速路径，在纯软件范畴内尽量贴近上限。
- 完整的多租户安全隔离、加密、权限模型（仅做最简版本）。
- 真实的 nD-FullMesh / Clos 拓扑，仅支持星形 + 全连接两种简化拓扑。
- 首版默认基于用户态 UDP/TCP socket；若性能瓶颈明显，可在后续里程碑按需切换或并存 DPDK / io_uring / RDMA verbs 等高性能路径（设计阶段预留接口）。
- 大规模（>64 节点）下的性能优化与一致性测试。

---

## 3. 术语表

| 术语 | 含义 |
|---|---|
| **SuperPod（超节点）** | 一个 Scale-Up 域，由若干 UB 节点组成的逻辑大机器。 |
| **Node（节点）** | 一个参与 SuperPod 的进程，对应真实世界中一颗芯片或一台服务器。 |
| **Device（设备）** | 节点内可被远端访问的资源单位，例如一段内存、一块"模拟 NPU"。 |
| **UB Address（UB 地址）** | 全局唯一的统一编址，128 bit，定位到 SuperPod 内任意字节。 |
| **MR（Memory Region）** | 一段被注册为可远程访问的本地内存。 |
| **Jetty** | 无连接的发送/接收抽象，由 JFS / JFR / JFC 组成。 |
| **JFS / JFR / JFC** | Jetty 的发送队列 / 接收队列 / 完成队列。 |
| **WR（Work Request）** | 应用提交给 JFS 的一次操作请求。 |
| **CQE（Completion Queue Entry）** | JFC 中的一条完成事件。 |
| **Verb** | 应用层调用的原子操作（Read / Write / Atomic / Send / Recv …）。 |
| **Fabric** | 节点间的传输底座，本项目用 UDP（默认）或 TCP 模拟。 |

---

## 4. 用户故事

> 站在一个想"在自己的笔记本上跑一个迷你超节点"的开发者视角。

- **US-1**：作为开发者，我可以用一行命令启动 N 个节点进程，它们自动组成一个 SuperPod，我能在 CLI 里看到拓扑。
- **US-2**：作为开发者，我可以在节点 A 上注册一段 1MB 的内存（得到一个 UB 地址），然后在节点 B 上用这个 UB 地址直接 `ub_write` / `ub_read`，读写结果应一致。
- **US-3**：作为开发者，我可以用 `ub_atomic_cas` 在跨节点的同一个 UB 地址上做原子操作，多个节点并发执行结果正确。
- **US-4**：作为开发者，我可以在节点 A 创建一个 Jetty，把消息 `send` 给节点 B 的 Jetty，B 在 JFC 上看到 CQE 并取出消息体。
- **US-5**：作为开发者，我可以用 `write_with_imm` 在写完远端内存后顺带通知对端"我写完了"，对端在 JFC 上获得通知。
- **US-6**：作为开发者，我可以杀掉一个节点进程，超节点能在若干秒内感知到，相关 UB 地址访问会返回明确的错误。
- **US-7**：作为开发者，我可以在 CLI 里查看每个节点的 MR 列表、Jetty 列表、收发计数、错误计数。
- **US-8**：作为学习者，我能从代码里清晰地看到 UB 协议各层（事务层 / 传输层 / 网络层 / 链路层 / 物理模拟层）的边界与职责。

---

## 5. 功能需求

### 5.1 统一编址（FR-ADDR）

- **FR-ADDR-1**：UB 地址固定 128 bit，结构化为 `[PodID:16 | NodeID:16 | DeviceID:16 | Offset:64 | Reserved:16]`（具体位宽在设计阶段可调，此处给一个起点）。
  - 示例：`0x0001:0042:0000:00000000DEADBEEF:0000` 表示 Pod 1、Node 66、Device 0、偏移 0xDEADBEEF
  - 字节级寻址：Offset 字段按字节粒度计算，支持任意对齐访问（但 atomic 操作有对齐要求，见 FR-MEM）
- **FR-ADDR-2**：SuperPod 内 UB 地址全局唯一，任意两次注册得到的 UB 地址不冲突。
- **FR-ADDR-3**：UB 地址的分配由各节点本地完成（本地 NodeID 已由控制面分配），不需要中心化分配每个 Offset。
- **FR-ADDR-4**：**VA 翻译**（UB Addr → 本地虚拟地址）仅由资源所在节点维护；其他节点**不得**缓存对端 VA。远端可缓存 **MR 元数据**（UB 地址区间、长度、权限、NodeID）用于路由与访问校验，但不得据此推导对端进程内指针。
- **FR-ADDR-5**：注销（dereg）后，针对该地址的访问应返回明确错误码 `UB_ERR_ADDR_INVALID`。

### 5.2 内存注册与生命周期（FR-MR）

- **FR-MR-1**：提供 `ub_mr_register(buf, len, perms)` → 返回 UB 地址 + MR Handle。
- **FR-MR-2**：权限位至少包含 `READ / WRITE / ATOMIC`。
- **FR-MR-3**：提供 `ub_mr_deregister(handle)`。
- **FR-MR-4**：MR 的元数据（地址、长度、权限）通过控制面广播给 SuperPod 内其他节点（或在按需查询时拉取，二选一，设计阶段决定）。

### 5.3 内存语义（FR-MEM）

- **FR-MEM-1**：`ub_read(local_buf, remote_ub_addr, len)` —— 单边读，远端 CPU 不参与。
- **FR-MEM-2**：`ub_write(remote_ub_addr, local_buf, len)` —— 单边写。
- **FR-MEM-3**：`ub_atomic_cas(remote_ub_addr, expect, new)` —— 8 字节原子比较交换。
  - 对齐要求：remote_ub_addr 必须 8 字节对齐，否则返回 `UB_ERR_ALIGNMENT`
  - 首版仅支持 8 字节；4/2/1 字节原子操作留作扩展
- **FR-MEM-4**：`ub_atomic_faa(remote_ub_addr, add)` —— 8 字节原子加。
  - 对齐要求：同 FR-MEM-3
- **FR-MEM-5**：每次操作都要异步提交，通过 JFC 拿完成事件；同时提供同步的便利包装。
- **FR-MEM-6**：单次操作 payload 上限至少 1 MiB；超过部分协议层应自动分片重组（IO 拆包），对应用透明。
- **FR-MEM-7（并发与顺序）**：对**同一 UB 地址**的并发内存 verb，除由 **FR-MEM-3/4** 定义的原子操作保证原子性外，**不保证**与提交顺序一致的可见顺序；需要顺序语义时由应用自行串行化或使用原子操作。

### 5.4 消息语义（FR-MSG）

- **FR-MSG-1**：`ub_send(jetty, dst_ub_jetty, buf, len)` —— 双边发送。
- **FR-MSG-2**：接收方需提前 `ub_post_recv(jetty, buf, len)`。
- **FR-MSG-3**：`ub_send_with_imm(..., imm: u64)` —— 在 CQE 中携带 8 字节立即数，避免再读一次内存。（与部分 RDMA 实现中 32 bit immediate 不同，本 toy 固定 64 bit，便于与 `ub_atomic_*` 等 API 对齐。）
- **FR-MSG-4**：`ub_write_with_imm(..., imm: u64)` —— 远端写完后在对方 JFC 上额外生成一条带立即数的完成事件。
- **FR-MSG-5**：消息有序性：
  - 同一对 (src_jetty, dst_jetty) 上的消息按提交顺序到达
  - **不同** jetty 对之间无顺序保证
  - 内存操作（read/write）与消息操作之间无跨语义顺序保证（应用需自行用 fence 或立即数同步）

### 5.5 Jetty 无连接抽象（FR-JETTY）

- **FR-JETTY-1**：`ub_jetty_create(attrs)` 返回一个 Jetty 句柄，包含 JFS / JFR / JFC。
- **FR-JETTY-2**：发送时只需指定目标 `(node_id, jetty_id)`，不需要事先 connect / handshake。
- **FR-JETTY-3**：**完成背压**：当 JFC 中未消费的 CQE 数量达到配置的上限（或与 JFS 队列深度成比例）时，发送路径应停止从 JFS 取新 WR，避免无限堆积；具体「槽位是否严格 1:1」在详细设计中定义，但语义上须保证不会因 CQE 积压导致死锁或静默丢完成事件。
- **FR-JETTY-4**：一个进程可创建多个 Jetty，用于隔离不同业务流（如训练梯度 vs. 控制消息），避免 HOL 阻塞。
- **FR-JETTY-5**：Jetty 销毁时所有未完成 WR 应被回收，并在 JFC 上生成 `flushed` 类型的 CQE。

### 5.6 拓扑与控制面（FR-CTRL）

- **FR-CTRL-1**：支持两种 bootstrap 模式：
  - **Static**：启动时通过配置文件指定所有节点列表（NodeID, IP, Port）。
  - **Seed**：指定一个或多个 seed 节点，新节点向 seed 报到后由 seed 广播全网。
- **FR-CTRL-2**：每个节点维护一份 SuperPod 视图（节点列表、节点状态、MR 元数据缓存）。
- **FR-CTRL-3**：控制面消息独立于数据面通道，避免相互阻塞。
- **FR-CTRL-4**：节点离开（主动 / 异常）时，控制面应在 ≤5s 内更新全网视图。
- **FR-CTRL-5**：拓扑首版只需支持 **全连接（任意两节点直连 UDP）**，源路由只是传输头里的可选字段，多跳留作扩展。
- **FR-CTRL-6（广播/多播）**：首版不要求硬件式多播树；若 demo 需要「一对多通知」，允许实现为 **向订阅表中的多个 `(node_id, jetty_id)` 顺序或并发发送** 的 fan-out，语义为 **best-effort**（不保证全员同时送达顺序），与 **FR-REL** 的单播可靠性独立计数。

### 5.7 可靠传输与重传（FR-REL）

- **FR-REL-1**：每个 (src_node, dst_node) 维护一个独立的可靠传输上下文（序号空间、重传队列、ACK 状态）。
- **FR-REL-2**：使用累积 ACK + 选择性重传（SACK 简化版）。
- **FR-REL-3**：超时重传，初始 RTO 可配置，做指数退避。
- **FR-REL-4**：超过最大重试次数后向上层报告 `UB_ERR_LINK_DOWN`。
- **FR-REL-5**：所有数据面操作的语义保证：**至多一次**（at-most-once），重复包必须被去重。
- **FR-REL-6**：对**写路径**（含 `ub_write`、带写副作用的协议控制消息），接收端去重与序号空间设计须避免 **重复执行** 同一逻辑写；对 **读路径**，重复包应丢弃或返回与首次一致的安全行为（详细设计中明确），且不得因去重导致接收端阻塞死锁。

### 5.8 流控与拥塞（FR-FLOW）

- **FR-FLOW-1**：发送端基于接收端通告的"剩余信用"决定是否发送（credit-based flow control）。
  - 信用单位：**WR 数量**（不按字节计）；接收端每消费一个 CQE，可向发送端返还 1 credit
- **FR-FLOW-2**：接收端在 JFR / 接收 buffer 紧张时可主动收回信用，形成背压。
- **FR-FLOW-3**：拥塞控制首版采用最简的 AIMD 调速即可，不要求实现 DCTCP / DCQCN。

### 5.9 故障检测（FR-FAIL）

- **FR-FAIL-1**：节点间维持周期性心跳（默认 1s）。
- **FR-FAIL-2**：连续 N 次心跳丢失判定为节点失联（N 默认 3）。
- **FR-FAIL-3**：失联事件应触发：相关重传任务终止 / 相关 JFC 投递错误 CQE / 控制面广播节点状态变化。

### 5.10 错误码定义（FR-ERR）

| 错误码 | 含义 | 触发场景 |
|---|---|---|
| `UB_OK` | 操作成功 | CQE 正常完成 |
| `UB_ERR_ADDR_INVALID` | 地址无效 | 访问未注册或已注销的 UB 地址 |
| `UB_ERR_PERM_DENIED` | 权限不足 | 访问权限不匹配（如对只读 MR 执行 write） |
| `UB_ERR_ALIGNMENT` | 对齐错误 | atomic 操作地址未 8 字节对齐 |
| `UB_ERR_LINK_DOWN` | 链路失效 | 超过最大重传次数或目标节点失联 |
| `UB_ERR_NO_RESOURCES` | 资源耗尽 | JFS/JFR/JFC 队列满 |
| `UB_ERR_TIMEOUT` | 操作超时 | 异步操作在用户指定时间内未完成 |
| `UB_ERR_PAYLOAD_TOO_LARGE` | 载荷过大 | 超过协议层或 MR 尺寸限制 |

### 5.11 CLI 与 SDK（FR-API）

- **FR-API-1**：提供 `unibusctl` 命令行：
  - `unibusctl node start --config xxx.yaml`
  - `unibusctl node list` / `node info <id>`
    - 示例输出：`NodeID=42 State=Active Addr=127.0.0.1:9000 MRs=3 Jettys=2 Uptime=1h23m`
  - `unibusctl mr list <node>`
    - 示例输出：`MR[0] UBAddr=0x0001:0042:0000:... Size=1MB Perms=RW`
  - `unibusctl jetty list <node>`
  - `unibusctl bench read|write|send` （内置 demo / 简单 micro-benchmark）
- **FR-API-2**：SDK 至少提供一种语言绑定（首版用项目的实现语言原生 API；选型在设计阶段定）。
- **FR-API-3**：SDK 接口需明确区分异步（返回 future / 通过 JFC 取完成）与同步包装。

### 5.12 可观测（FR-OBS）

- **FR-OBS-1**：每个节点暴露计数器：`tx_pkts / rx_pkts / retrans / drops / cqe_ok / cqe_err / mr_count / jetty_count`。
- **FR-OBS-2**：日志分级（DEBUG / INFO / WARN / ERROR），关键事件可追踪。
- **FR-OBS-3**：暴露一个 HTTP `/metrics` 端点（Prometheus 文本格式）；与 **Q5** 默认决策一致，首版即实现。
- **FR-OBS-4**：提供 tracing 钩子（例如 OpenTelemetry 兼容的 interface 或最小 `Span` 回调），默认实现可为 no-op，便于后续接入。

---

## 6. 非功能需求

| 类别 | 指标 | 说明 |
|---|---|---|
| **规模** | 单 SuperPod 支持 ≥ 16 节点，**目标 64 节点**（M5 达成），M6 可选验证 | 64 是 stretch goal；超过 64 未做优化（主要瓶颈在控制面全量广播 O(N²) 和 UDP 全连接拓扑） |
| **性能（参考，不强约束）** | 单节点对单节点 1 KiB write 吞吐 ≥ 50K ops/s（环回 / 本机多进程） | toy 级别 |
| **延迟（参考）** | 本机 loopback 下端到端 < 200 μs | 用户态 UDP 即可 |
| **可靠性** | 在 1% 丢包注入下，操作语义仍保持"至多一次"且无死锁 | |
| **可移植** | Linux / macOS 均能跑通 demo | |
| **可测试** | 单元测试覆盖率 ≥ 60%；提供端到端 demo 脚本 | |
| **代码质量** | 协议各层职责清晰，可独立替换底层 fabric | UDP / TCP / 共享内存可插拔 |

---

## 6.1 评审中已识别的风险与缺口（本版已部分吸收）

- **VA 与 MR 元数据**：历史上易混淆「不缓存翻译」与「不缓存 MR 目录」；已在 **FR-ADDR-4** 中拆开表述。
- **广播语义**：原目标与用户故事中的「广播」缺少对应 FR；已用 **FR-CTRL-6** 将首版收窄为可控的 fan-out，避免与单播可靠性混谈。
- **内存并发顺序**：补充 **FR-MEM-7**，避免读者误以为自动提供顺序一致性。

---

## 7. 系统边界与约束

- **纯软件**：不依赖任何特殊硬件或内核模块，跑在普通 Linux / macOS 用户态。
- **底层 fabric**：默认 UDP，保留 TCP / Unix Domain Socket / 共享内存的扩展点。
- **进程模型**：一个节点 = 一个进程（首版不考虑多节点共进程）。
- **语言**：在设计阶段定，候选 Rust / Go / C++（倾向 Rust 或 Go，理由设计阶段写）。
- **不做硬件仿真**：物理层 / 链路层只是抽象层，不模拟比特流。
- **安全假设**：本 toy **不面向** 不可信网络或恶意节点；控制面与数据面报文可做最小完整性校验（如 magic / version），但不承诺抗欺骗、抗重放、抗 DoS。生产级安全留扩展点即可。

---

## 8. 交付物与里程碑（粗粒度）

| 阶段 | 内容 | 验收标准 |
|---|---|---|
| **M0** | 需求 + 详细设计 review 通过（本阶段） | 至少 1 位 reviewer 完整审阅；关键设计决策达成一致 |
| **M1** | 控制面 + 拓扑发现 + 节点注册 | 能起 N 个节点组成 SuperPod，`unibusctl node list` 看到所有节点；节点主动离开或 kill 后能被其他节点感知 |
| **M2** | MR 注册 + 内存语义 | 跨节点 read/write 单测通过；跨节点 atomic_cas 并发正确性测试通过；`unibusctl mr list` 显示已注册 MR |
| **M3** | Jetty + 消息语义 | send/recv 单测通过；write_with_imm 能在接收端 JFC 上拿到立即数；消息有序性测试通过 |
| **M4** | 可靠传输 + 流控 + 故障检测 | 1% 丢包注入下端到端测试通过；链路失效后相关操作返回 `UB_ERR_LINK_DOWN`；信用耗尽时发送端正确背压 |
| **M5** | CLI 完善 + 可观测 + demo / bench 脚本 | `unibusctl bench` 能跑通简单压测；分布式 KV demo 可演示 get/put/cas；/metrics 端点输出符合 Prometheus 格式 |
| **M6**（可选） | 多跳 / 源路由 / 简化 nD-Mesh 拓扑 | 3 节点链式拓扑下消息能正确转发；源路由头解析与转发单测通过 |

---

## 9. 默认决策（请 review 时确认 / 修改）

> 这些问题原本想让你拍板，你说让我自己定，下面是我的默认选择和理由。
> Review 时如果某条不同意，告诉我改成什么即可。

| # | 问题 | 默认决策 | 理由 |
|---|---|---|---|
| Q1 | 目标语言 | **Go** | toy 项目优先开发速度；goroutine + channel 天然适合做 reactor / 队列 / 重传定时器；标准库 net 够用，不必上 Rust 的 async 复杂度。Rust 留作"如果哪天要追性能再重写"。 |
| Q2 | 跨语言 SDK | **首版只做 Go 原生 API** | 不浪费精力做 FFI，C SDK 留扩展点。 |
| Q3 | 拓扑发现 | **首版同时支持 static 和 seed**，默认走 static | static 调试方便，seed 实现成本不高且更像真实 SuperPod 上电流程。 |
| Q4 | 应用 demo | **做一个跨节点的"分布式 KV"** demo | 比 all-reduce 简单，能完整体现 read / write / atomic / 跨节点 send，亲和性强、用户感知明显。 |
| Q5 | 可观测 | **首版就上 HTTP `/metrics`**（Prometheus 文本格式） | Go 里加这个几乎零成本，调试和演示都用得上。 |
| Q6 | 线程模型 | **每节点 = 1 个 reactor goroutine + N 个 worker goroutine**，N 默认等于 CPU 数 | 单 reactor 处理 socket I/O + 定时器；worker 处理 verb / CQE 派发。语义上仍然是"逻辑单 reactor"，方便推理。 |
| Q7 | UB 地址位宽 | **128 bit**，结构 `[PodID:16 \| NodeID:16 \| DeviceID:16 \| Offset:64 \| Reserved:16]` | 既然是学习项目，128 bit 更能体现"全局编址"的味道；64 bit 偏 toy。Reserved 留给未来扩展（VLAN / Tenant ID / VC 等）。 |
| Q8 | 是否模拟 NPU | **首版不做**，但保留"Device 抽象"接口 | 把 MR 之外的资源抽象成 Device（带 type 字段），首版只实现 `Device.type = MEMORY`；将来加 `NPU` / `SSD` 不需要重构。 |

### 9.1 由上述决策衍生的几个具体约定

- **代码组织**（待详细设计阶段细化）：Go module，目录大致划分 `pkg/addr` / `pkg/mr` / `pkg/jetty` / `pkg/transport` / `pkg/control` / `pkg/fabric` / `cmd/unibusd` / `cmd/unibusctl`。
- **配置格式**：YAML（Go 生态习惯）。
- **底层 fabric 默认 UDP**，包大小 ≤ 1400 字节避免 IP 分片，超过部分协议层自己分片。
- **测试**：用 Go 自带 `testing`；端到端测试用一个脚本起多个 `unibusd` 进程跑 demo。

如果你对其中某条决策不同意，告诉我改哪一条；其余保持默认我就按这个方向写详细设计。

---

## 10. 参考资料

> **说明**：以下链接于 2026-04 月验证可访问。标 ⭐ 为核心参考，标 📖 为补充阅读。

- ⭐ Huawei Connect 2025 - SuperPoD 与 UnifiedBus 公告 — https://www.huawei.com/en/news/2025/9/hc-lingqu-ai-superpod
- ⭐ UB-Mesh: a Hierarchically Localized nD-FullMesh Datacenter Network Architecture (arXiv) — https://arxiv.org/html/2503.20377v1
- ⭐ UnifiedBus 官方文档仓库（codehubcloud/UnifiedBus） — https://github.com/codehubcloud/UnifiedBus
- ⭐ 灵衢系统高阶服务软件架构参考设计 v2.0 — https://www.openeuler.org/projects/ub-service-core/white-paper/UB-Service-Core-SW-Arch-RD-2.0-zh.pdf
- 📖 NVLink 的国产替代：华为 Unified Bus 背后的思考 — https://news.qq.com/rain/a/20251011A021OL00
- 📖 基于灵衢（UB）的超节点架构白皮书走读 — https://zhuanlan.zhihu.com/p/1955200725576561559
- 📖 华为灵衢 UB 总线介绍 — https://blog.csdn.net/DK_Allen/article/details/154440440
- 📖 Exploring Huawei's UnifiedBus Architecture — https://siliconflash.com/exploring-huaweis-unifiedbus-architecture-revolutionizing-cloud-ai-infrastructure/
- 📖 无硬件模拟灵衢架构：openFuyao 社区 UB 组件实践 — https://www.cnblogs.com/gccbuaa/p/19449423
