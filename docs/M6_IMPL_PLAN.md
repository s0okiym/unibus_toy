# M6 实施计划：Verbs 层功能补全

**日期**: 2026-04-17
**前置里程碑**: M5（146 单元测试、M5 E2E 16 步 PASS）
**目标**: 补全 Verbs 层（M1–M5）设计文档要求但未实现的功能，将 FR 覆盖率从 ~70% 提升至 ~80%

---

## 1. 实施步骤

### Step 6.1: 远端 MR 权限检查

**需求**: FR-MR-5 — 远端 verb 操作需检查 MR 权限位，对只读 MR 执行 write 应返回 `PermDenied`。

**当前状态**: 本地操作已通过 `entry.check_perms(verb)` 检查权限，但数据面响应端（`handle_write`/`handle_read_req`/`handle_atomic_cas`/`handle_atomic_faa`）未检查 MR 权限。

**改动**:

1. `crates/ub-dataplane/src/lib.rs` — 在以下响应处理函数中，MR lookup 之后、执行操作之前，添加 `entry.check_perms(verb)` 检查：
   - `handle_write`: 检查 `MrPerms::WRITE`，失败则返回 ERR_RESP
   - `handle_read_req`: 检查 `MrPerms::READ`，失败则返回 ERR_RESP
   - `handle_atomic_cas`: 检查 `MrPerms::ATOMIC`，失败则返回 ERR_RESP
   - `handle_atomic_faa`: 检查 `MrPerms::ATOMIC`，失败则返回 ERR_RESP

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| `test_remote_write_perm_denied` | 单元 | 对只读 MR 发 write 请求，响应端返回 PermDenied |
| `test_remote_read_perm_denied` | 单元 | 对只写 MR 发 read 请求，响应端返回 PermDenied |
| `test_remote_atomic_perm_denied` | 单元 | 对无 ATOMIC 权限 MR 发 atomic 请求，响应端返回 PermDenied |

**验收**: 3 个单元测试通过；现有 E2E 回归不受影响。

---

### Step 6.2: MR Deregister 三态机

**需求**: FR-MR-3 — MR 注销时应进入 Revoking 状态，等待 inflight 操作归零后再释放；超时则返回错误。

**当前状态**: `MrState` 枚举和 `inflight_refs` 字段已定义，`try_inflight_inc()` 已实现，但 `deregister()` 直接移除条目，未走三态流程。

**改动**:

1. `crates/ub-core/src/mr.rs`:
   - `deregister()`: CAS `state` 从 Active → Revoking；若 state 非 Active 则返回错误
   - 添加 `deregister_async()`: CAS Active→Revoking 后，用 `tokio::sync::Notify` 异步等待 `inflight_refs` 降为 0，超时（`deregister_timeout_ms`）返回 `UbError::Timeout`
   - inflight 降为 0 时 Notify 唤醒等待者，state 设为 Released
   - `try_inflight_inc()`: 已有，当 state != Active 时返回 false（确认 Revoking/Released 拒绝新请求）

2. `crates/ub-dataplane/src/lib.rs`:
   - 所有 `try_inflight_inc()` 返回 false 的路径，返回 `UbError::AddrInvalid`（或新增 `UbError::Revoking` 语义，复用 AddrInvalid 即可）

3. `crates/unibusd/src/main.rs`:
   - `mr_deregister` handler 改为调用 `deregister_async()`，超时返回错误 JSON

4. `crates/ub-control/src/control.rs`:
   - MR_REVOKE 广播应在 CAS Active→Revoking 之后、而非条目删除之后发送

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| `test_mr_deregister_revoking_state` | 单元 | deregister 后 state 为 Revoking，新请求被拒绝 |
| `test_mr_deregister_waits_inflight` | 单元 | inflight_refs > 0 时 deregister 阻塞，归零后完成 |
| `test_mr_deregister_timeout` | 单元 | inflight_refs 未归零超时后返回 Timeout |
| `test_mr_deregister_already_revoking` | 单元 | 对 Revoking 状态 MR 再次 deregister 返回错误 |

**验收**: 4 个单元测试通过；E2E: mr register → write → deregister（正常路径）→ 写入返回错误。

---

### Step 6.3: Seed 模式 Bootstrap

**需求**: FR-CTRL-1 — 支持 seed 模式：新节点向 seed 报到，seed 回复 MemberSnapshot。

**当前状态**: `CtrlMsgType::Join`(0x10) 和 `MemberSnapshot`(0x05) 仅定义了 wire type 常量，无 payload、无 ControlMsg 变体、无逻辑。配置 schema 已有 `seed_addrs` 字段。

**改动**:

1. `crates/ub-control/src/message.rs`:
   - 添加 `JoinPayload` 结构体（node_id, epoch, data_addr, control_addr, initial_credits）
   - 添加 `MemberSnapshotPayload` 结构体（Vec of NodeInfo 精简版：node_id, state, data_addr, control_addr, epoch）
   - 添加 `ControlMsg::Join` 和 `ControlMsg::MemberSnapshot` 变体
   - 添加编解码逻辑

2. `crates/ub-control/src/control.rs`:
   - `start()`: 根据 `config.control.bootstrap` 分支：
     - `"static"`: 现有逻辑，连接配置中的 peers
     - `"seed"`: 连接 `config.control.seed_addrs` 中的地址，发送 Join 消息
   - `process_control_message()`: 处理 Join 和 MemberSnapshot：
     - 收到 Join：注册新节点信息，回复 MemberSnapshot
     - 收到 MemberSnapshot：批量注册所有节点信息，对未知节点发起连接

3. `configs/` — 添加 seed 模式配置示例（`seed_addrs` 字段）

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| `test_join_payload_encode_decode` | 单元 | Join payload 编解码 roundtrip |
| `test_member_snapshot_encode_decode` | 单元 | MemberSnapshot payload 编解码 roundtrip |
| seed 模式 E2E | 集成 | 3 节点用 seed 模式启动，集群正常形成 |

**验收**: 2 个单元测试 + seed 模式 E2E 通过；static 模式 E2E 回归不受影响。

---

### Step 6.4: 默认 Jetty + `unibusctl node start`

**需求**: FR-JETTY 默认 Jetty 概念 + FR-API-1 `unibusctl node start --config`。

**当前状态**: 无 `ub_default_jetty()` 函数；unibusctl 无 node start 子命令。

**改动**:

1. `crates/ub-core/src/jetty.rs`:
   - `JettyTable` 添加 `default_jetty: Option<JettyHandle>` 字段
   - `create()` 首次创建时自动设为默认 Jetty
   - 添加 `default_jetty() -> Option<JettyHandle>` 方法
   - 添加 `ub_default_jetty()` 便利函数

2. `crates/ub-dataplane/src/lib.rs`:
   - `ub_send_remote`/`ub_send_with_imm_remote` 中使用 `jetty_table.default_jetty()` 替代 "取列表第一个" workaround

3. `crates/unibusd/src/main.rs`:
   - `verb_send`/`verb_send_with_imm` 中使用 `jetty_table.default_jetty()` 替代 workaround

4. `crates/unibusctl/src/main.rs`:
   - 添加 `NodeStart { config: String }` 子命令
   - 实现方式：`std::process::Command::new("unibusd").args(["--config", &config]).spawn()`
   - 输出：启动的 PID 和 admin 地址

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| `test_default_jetty_first_create` | 单元 | 首次 create 后 default_jetty 返回该 handle |
| `test_default_jetty_not_changed_on_second_create` | 单元 | 第二次 create 不改变 default_jetty |
| `test_default_jetty_close_clears` | 单元 | 关闭 default jetty 后返回 None |
| CLI smoke test | 手动 | `unibusctl node start --config xxx.yaml` 成功启动 daemon |

**验收**: 3 个单元测试通过；现有 send/recv E2E 不受影响。

---

### Step 6.5: AIMD 拥塞控制 + EWMA RTT + SACK 快速重传

**需求**: FR-FLOW-3 AIMD 窗口调整 + DESIGN §5 EWMA RTT + SACK 快速重传。

**当前状态**: 静态信用窗口 + 固定 RTO + 纯定时器重传。SACK bitmap 已生成解析但未用于快速重传。

**改动**:

1. `crates/ub-transport/src/session.rs`:
   - 添加 `cwnd: u32`（拥塞窗口，初始 = initial_credits）、`ssthresh: u32`（慢启动阈值）
   - 添加 `srtt: f64`（平滑 RTT，ms）、`rttvar: f64`（RTT 方差，ms）
   - 添加 `rto: u64`（动态计算的 RTO，ms），替代固定 `rto_base_ms`
   - AIMD 逻辑：
     - 收到新 ACK（非 dup）：`cwnd += 1/cwnd`（AI）；若 `cwnd < ssthresh` 则慢启动 `cwnd *= 2`
     - 超时/丢包：`ssthresh = cwnd/2`，`cwnd = 1`（MD）
   - RTT 估算（Karn 算法）：
     - 每次 ACK 回来时，若非重传帧则采样 RTT：`srtt = 0.875 * srtt + 0.125 * sample`
     - `rttvar = 0.75 * rttvar + 0.25 * |sample - srtt|`
     - `RTO = srtt + 4 * rttvar`，clamp 到 [200ms, 60s]
     - 重传帧的 ACK 不参与采样（Karn 规则）
   - 添加 `send_available()` 方法：返回 `cwnd - outstanding`（未确认数）

2. `crates/ub-transport/src/manager.rs`:
   - `send()`: 检查 `session.send_available()` > 0 才发送，否则返回 `NoResources`
   - SACK 快速重传：
     - `handle_ack_frame()` 中统计重复 ACK：若 ACK 的 ack_seq 未推进但 sack_bitmap 有新位，计为 dup_ack
     - 连续 3 次 dup_ack：立即重传 ack_seq 指向的帧，重置 dup_ack 计数
   - ACK 处理时调用 `session.update_rtt(sample)` 更新 RTT 估算

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| `test_aimd_slow_start` | 单元 | 初始 cwnd 在 ACK 后指数增长 |
| `test_aimd_congestion_avoidance` | 单元 | cwnd > ssthresh 后线性增长 |
| `test_aimd_multiplicative_decrease` | 单元 | 超时后 cwnd 降为 1，ssthresh = cwnd/2 |
| `test_rtt_estimation` | 单元 | 多次 RTT 采样后 srtt 收敛，RTO 自适应 |
| `test_rtt_karn_skip_retransmit` | 单元 | 重传帧的 ACK 不更新 srtt |
| `test_sack_fast_retransmit` | 单元 | 3 次 dup ACK 触发立即重传 |
| 丢包注入集成测试 | 集成 | 5% 丢包下，快速重传比纯 RTO 更快恢复，操作仍正确完成 |

**验收**: 6 个单元测试 + 丢包集成测试通过；现有 E2E 回归不受影响（AIMD/RTT 参数在无丢包时与当前行为一致）。

---

### Step 6.6: KV Replica 通知 + NPU MR KV 变体

**需求**: DESIGN §19 KV demo — put/cas 成功后通过 write_with_imm 通知 replica；FR-DEV NPU MR KV 变体。

**改动**:

1. `crates/unibusd/src/main.rs`:
   - `kv_init`: 添加可选 `device_kind` 参数（"memory" 或 "npu"），NPU 时使用 `NpuDevice` 作为 backing
   - `kv_put`/`kv_cas`: 成功后，若配置了 replica 节点，调用 `dp.ub_write_imm_remote()` 通知 replica jetty，imm = slot index
   - `KvInitRequest` 添加 `device_kind` 和 `replica_node_id`/`replica_jetty_id` 可选字段

2. `crates/unibusctl/src/main.rs`:
   - `KvInit` 添加 `--device-kind` 和 `--replica` 可选参数

3. `tests/m5_e2e.sh` — 添加步骤：
   - KV init with replica
   - put 后验证 replica jetty 收到 CQE (imm = slot_index)
   - KV init with NPU device

**测试**:

| 测试 | 类型 | 验证内容 |
|------|------|---------|
| replica 通知 E2E | E2E | put 后 replica jetty poll-cqe 返回 imm=slot_index |
| NPU KV init + put/get | E2E | NPU-backed KV 存储的 put/get/cas 与 Memory 一致 |
| 现有 KV E2E | 回归 | 无 replica 参数时行为不变 |

**验收**: E2E 测试通过；NPU KV 操作数据正确。

---

## 2. 依赖关系

```
Step 6.1 (远端权限检查) ──── 无依赖
Step 6.2 (MR deregister) ──── 无依赖（可与 6.1 连续开发，同属 MR 层）
Step 6.3 (Seed bootstrap) ──── 无依赖
Step 6.4 (默认 Jetty) ──── 无依赖
Step 6.5 (AIMD+RTT+快速重传) ── 无依赖
Step 6.6 (KV replica+NPU) ──── 依赖 Step 6.4 的默认 Jetty

Step 6.1 → 6.2 可连续
Step 6.3、6.4、6.5 相互独立
Step 6.6 依赖 6.4
```

建议开发顺序：**6.1 → 6.2 → 6.3 → 6.4 → 6.5 → 6.6**

---

## 3. 文件变更预估

| 文件 | Step | 改动量 |
|------|------|-------|
| `crates/ub-dataplane/src/lib.rs` | 6.1, 6.2, 6.6 | 中 |
| `crates/ub-core/src/mr.rs` | 6.2 | 中 |
| `crates/ub-core/src/jetty.rs` | 6.4 | 小 |
| `crates/ub-control/src/message.rs` | 6.3 | 中 |
| `crates/ub-control/src/control.rs` | 6.2, 6.3 | 中 |
| `crates/ub-transport/src/session.rs` | 6.5 | 大 |
| `crates/ub-transport/src/manager.rs` | 6.5 | 中 |
| `crates/unibusd/src/main.rs` | 6.2, 6.4, 6.6 | 中 |
| `crates/unibusctl/src/main.rs` | 6.4, 6.6 | 小 |
| `tests/m5_e2e.sh` | 6.6 | 小 |

---

## 4. 验收总标准

- Step 6.1–6.5 每步完成后：该步新增单元测试全部通过
- 全部 6 步完成后：
  - 单元测试总数 >= 160
  - M5 E2E 回归通过
  - 新增 Step 6.3 seed 模式 E2E 通过
  - 新增 Step 6.5 丢包注入集成测试通过
  - 新增 Step 6.6 KV replica + NPU E2E 通过