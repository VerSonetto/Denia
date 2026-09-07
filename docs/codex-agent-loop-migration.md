# codex agent-loop 移植可行性分析与方案

> 状态:**实施完成**(待用户批准提交)。结论:整体 vendoring 不可行,
> 已按「选择性吸收算法」落地 P0/P1 全部项,审批编排判定为已充分映射。
> 本文件是决策依据 + 实施记录。

## 实施结果(已落地)

| 阶段 | codex 来源 | 落地 | 状态 |
|---|---|---|---|
| P0 工具并行执行 | `tools/parallel.rs` | `ParallelSettings` + `FuturesOrdered` 滚动池,`max_parallel_tool_calls` 走 settings 命名空间(`console.maxParallelToolCalls`,默认 10,范围 [1,64]) | ✅ |
| P0 连接重试独立退避 | `responses_retry.rs` | `RetryPolicy` 增加连接类错误独立退避曲线(5s→60s) | ✅ |
| P1 压缩硬上限触发 | `compact.rs` + `context_window` | `should_compact` 增加硬上限闸门(投影 ≥ 窗口强制压缩) | ✅ |
| P1 审批编排 | `tools/orchestrator.rs` | 判定已充分映射,无需重写(见下) | ✅ 说明 |

## 审批编排判定(无需重写)

codex 的「审批 → 沙箱 → 尝试 → 拒绝后升级重试 + 缓存审批」序列,在 denia
已由「三级权限模式 + 模型驱动升级 + `resolve_escalation` 的
`approval-asked/decided` 事件」完整覆盖:

- codex `ExecApprovalRequirement::{Skip,Forbidden,NeedsApproval}` → denia
  `is_strictly_wider` + `validate_escalation_args` 的 fail-closed 语义;
- codex 拒绝后升级重试 → denia 模型携带 `sandbox_permissions` 重试
  (escalation hint 驱动);
- codex `with_cached_approval`(会话级审批记忆)需前端 UI 配合,超出
  agent-loop crate 范围,不强行移植。

## 一、codex agent loop 核心的真实形态

codex 的 agent loop 核心位于 `codex-rs/core`(crate 名 `codex-core`),
不是一段自洽的"循环逻辑",而是一个深度耦合的巨型子系统:

| 模块 | 行数 | 角色 |
|---|---|---|
| `session/mod.rs` | 4499 | Session 生命周期、submission loop、审批/权限入口 |
| `session/turn.rs` | 2765 | turn 主循环、`run_sampling_request`、压缩触发 |
| `session/session.rs` | 1596 | Session 状态、历史、权限投影 |
| `tools/parallel.rs` | 631 | 工具并行执行(`ToolCallRuntime` + `FuturesOrdered`) |
| `tools/orchestrator.rs` | 531 | 审批 → 沙箱 → 尝试 → 升级重试序列 |
| `tools/approvals.rs` | 889 | 审批策略执行 + reviewer 路由 |
| `responses_retry.rs` | 126 | 流重试 + 传输回退 |
| `compact.rs` + `compact_*.rs` | ~3000 | 四种压缩策略(local/remote/remote-v2/token-budget) |

`codex-core` 的 `Cargo.toml` 声明了 **50+ 个 codex 内部 crate** 依赖,
其中 `protocol`(62 文件)、`exec-server`(146 文件)、`config`(76 文件)、
`rollout`(30 文件)、`sandboxing`(19 文件)、`mcp`、`client`、`history`、
`guardian`、`hooks`、`plugins`、`connectors`、`analytics`、`otel` 等,
每一个都是独立子系统。vendoring 意味着把**半个 codex 仓库**搬进 denia。

## 二、四个根本冲突(不可调和的架构差异)

### 1. 协议层冲突:Responses API vs Chat Completions

codex 的 agent loop 深度绑定 OpenAI **Responses API + WebSocket**
(`client.rs` 用 `ResponsesApiRequest`/`ResponsesWebsocketClient`,自带
guardian/attestation/rollout-trace 请求头)。denia 的 `llm` 层是
**Chat Completions / Responses / Anthropic 三种 wire 协议的通用适配器**,
通过 `LlmRegistry::stream()` 统一输出 `StreamChunk`。

任务约束「保持 denia 现有 llm 适配器不变」与「代码级移植 codex 核心」
直接冲突:codex 的 `run_sampling_request` 消费的是 `ResponseEvent`
(`ResponseItem::FunctionCall` 等),不是 `StreamChunk`。要么重写 codex 的
client 层(那已不是移植,是重写),要么放弃 denia 的 llm 层(违反约束)。

### 2. 历史模型冲突:rollout vs append-only JSONL

codex 的历史单元是 `ResponseItem`/`TurnItem`/`RolloutItem`,存于
**rollout 文件**,支持 truncate/rollback(非严格 append-only),投影函数是
`clone_history().for_prompt()`。denia 是**严格 append-only JSONL** +
`derive_messages()` 派生,事件词汇是 `SessionEvent`/`SessionEnvelope`。

codex 的 turn 循环每一步都在读写它自己的 `Session` 状态(含
`input_queue`、`active_turn`、`world_state`、`extension_data` 等可变
状态),无法直接映射到 denia 的 `Session`(内部 `Mutex<SessionInner>` +
`BufWriter` append-only 日志)。

### 3. 工具编排冲突:并行 vs 顺序

codex 用 `ToolCallRuntime` + `FuturesOrdered` + `parallel_execution`
RwLock 门做**并行工具执行**(`parallel_tool_calls: true`)。denia 当前是
**顺序 dispatch**(`for call in &calls`)。这是 denia 遗留清单里明确列的
待办项(dsh `maxParallelToolCalls=10`),但它是**增量能力**,不是替换
agent-loop 的理由。

### 4. 审批/权限冲突:guardian 三层 vs 三级权限模式

codex 的审批是 guardian + execpolicy + sandboxing 三层(含网络审批、
exec policy amendment、reviewer 路由、缓存审批)。denia 是三级权限模式
(`ReadOnly/WorkspaceWrite/DangerFullAccess`)+ `approval-asked/decided`
事件 + `resolve_escalation`。两者语义粒度完全不同,codex 的审批依赖
`codex-protocol` 的 `AskForApproval`/`ReviewDecision`/`ExecPolicyAmendment`
等类型,无法直接落到 denia 的事件词汇上。

## 三、结论:整体 vendoring 不可行

上述冲突属于任务「风险与回退」一节预判的「codex 核心与 denia 事件源
模型存在根本冲突」情形。强行 vendoring 会:

- 引入 50+ crate 的依赖爆炸,denia 的 `Cargo.lock` 与构建复杂度失控;
- 破坏 `derive_messages()` 派生语义与 append-only 日志不变量;
- 放弃 denia 的 `llm`/`tools`/`session` 三层,违背任务「其余架构不变」;
- 把 denia 绑死到 OpenAI 后端,失去通用 OpenAI 兼容适配能力。

## 四、替代方案:选择性吸收算法(推荐)

保留 denia 现有 `agent-loop` 骨架(事件源、turn/step 状态机、错误分流),
**吸收 codex 中与事件源模型解耦、可适配的算法/策略**,逐项落地:

| 优先级 | codex 来源 | 吸收内容 | 对应 denia 遗留项 |
|---|---|---|---|
| P0 | `tools/parallel.rs` | 工具并行执行:`FuturesOrdered` 滚动池 + 并行门(支持并行/串行工具区分) | 遗留「工具并行执行滚动池」 |
| P0 | `responses_retry.rs` | 连接重试语义:connection retry 独立预算 + 指数退避(5s→60s)+ 传输回退 | 现有 `retry.rs` 增强 |
| P1 | `compact.rs`(local 路径) | token-budget 触发压缩 + pre-sampling 压缩(预测 pending 输入越界提前压) | 现有 `compact.rs` 增强 |
| P1 | `tools/orchestrator.rs` | 审批编排序列:approval → sandbox → attempt → 拒绝后升级沙箱重试(缓存审批) | 现有 `resolve_escalation` 增强 |
| P2 | `util.rs` backoff | 抖动退避公式(200ms 起 ×2 ±10%) | 现有退避对齐 |

**明确不吸收**:codex 的 `Session`/rollout 历史、Responses API client、
guardian/execpolicy/sandboxing 三层、MCP/插件/connector 运行时、
analytics/otel 遥测 —— 这些要么与 denia 架构冲突,要么超出「agent loop
核心」范围。

## 五、事件词汇映射表(吸收算法时需保持的兼容)

| codex 概念 | denia 现有事件 | 吸收后动作 |
|---|---|---|
| `ResponseItem::FunctionCall` | `ToolCall` | 并行池按 call_id 一一落 `ToolCall`/`ToolResult` |
| `ResponseItem::Message`(assistant) | `AssistantMessage`/`AssistantChunk` | 不变 |
| `CompactionItem` | `CompactionSummary` | 不变(仅触发策略增强) |
| `AskForApproval`/`ReviewDecision` | `ApprovalAsked`/`ApprovalDecided` | 不变(仅编排序列增强) |
| `RetryOperation` | `RetryAttempt` | 不变(仅退避策略增强) |
| `needs_follow_up`(turn 续跑) | `TurnEnd` reason + 下一 `TurnStart` | 不变 |

## 六、分阶段实施计划(批准后执行)

1. **P0 工具并行执行**:在 `agent-loop` 内新增 `parallel.rs`,把顺序
   dispatch 改为 `FuturesOrdered` 滚动池(上限 `maxParallelToolCalls`,
   走 settings 命名空间),保持每个 call 恰好一条 `ToolResult` 的不变量。
2. **P0 连接重试**:在 `llm/retry.rs` 增加 connection-retry 独立预算与
   指数退避,`RetryAttempt` 事件不变。
3. **P1 压缩触发**:在 `agent-loop/compact.rs` 增加 token-budget 触发与
   pre-sampling 预测压缩。
4. **P1 审批编排**:在 `resolve_escalation` 增加「拒绝后升级沙箱重试 +
   缓存审批」序列。
5. 每阶段 `cargo test` + `pwsh scripts/dev.ps1 -Bg` + curl 探测。

## 七、回退

现有 `agent-loop` 全部保留在 git 历史;每阶段独立提交,提交前经用户批准。
若某阶段发现吸收成本过高,可单独回退该阶段,不影响其余。
