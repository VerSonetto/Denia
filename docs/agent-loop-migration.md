# agent-loop 迁移设计:denia → dsh 语义对齐(实施完成)

> 状态:第一轮迁移已完成并验证(全仓 cargo test 全绿,dev 实例 3601 构建启动,
> 旧会话加载兼容,孤儿轮自动闭合实测生效)。后续增量见文末"遗留清单"。

## 背景与动机

cat 网关(glm-5.3-flash)大请求间歇失败 + denia 容错设计放大,导致轮次频繁
error 终止。已确认的四条硬伤:

1. `connect_timeout = 30s`(openai.rs:151),无总超时 → 大请求被系统性错杀
   (实测 200K payload 正常 TTFB 21.8s)
2. `INVALID_REQUEST`/`STREAM_CLOSED` 不在 retryable_codes(retry.rs:27)
   → 间歇 404 零重试直接打注入通道,一次抖动杀死轮次
3. 重试过程只写 tracing(retry.rs:76),无事件落盘、无 UI → 体感"干等 100 秒"
4. 重试耗尽后一律注入"请纠正输出"(agent-loop lib.rs),对提供方故障无效,
   且注入计入 2 次配额(MAX_FEEDBACK),第 3 次失败直接 TurnEndReason::Error

同时,日志系统已对齐 dsh(request-header/request-context/sourceEventSeqs/
abort cause/interrupted)。

## dsh 核心语义(移植目标)

- **Turn/Step 状态机**:turn/start 开轮;step = 一次模型请求 + 其工具执行;
  step 内先落 request/header(按 initial/resume/change/series 按需),再 dispatch;
  turn 以 turn/end(completed/aborted{cause}/blocked/error/max-tokens/interrupted)闭合
- **请求重建**:GenerateOptions 由 logged header 重建;header 全等判定
  (callConfigEquals 语义)→ 相同不重复落盘
- **错误分流**(dsh llm-retry + command-feedback):
  - setup 失败(send/建流)→ 按 RetryPolicy 退避重试(可重试码 + 指数退避 + jitter + Retry-After)
  - mid-stream chunk 错误 → 不重试,注入 feedback(配额内)
  - 流在 [DONE] 前关闭(STREAM_CLOSED)→ 视为可重试?「等 dsh 报告确认」
  - 4xx 请求类错误 → 注入反馈让模型自纠(工具参数等问题)
- **取消**:AgentCancelCause(user/parent/hook/disposed)持久化进 turn/end
- **interrupted**:崩溃孤儿轮闭合专用标记(已实现)

## denia 侧实施计划

### 1. llm 层(llm crate)
- [ ] connect_timeout 30s → 60s;body > 100KB 时 120s(超时合理化)
- [ ] retryable_codes 增加 INVALID_REQUEST、STREAM_CLOSED、
      CONTEXT_WINDOW_EXCEEDED?(CONTEXT_WINDOW 重试无意义,改为提示,等确认)
- [ ] 重试可见性:with_retry 每次尝试失败 → 新增 log-only 事件
      (`retry-attempt`:attempt/delay_ms/code/message),经 emit 通道落盘
      (agent-loop 已能 append;llm 层需引入事件 sink 参数或返回重试轨迹)
- [ ] RetryPolicy 数值对齐 dsh(llm-retry 默认值,等报告)

### 2. agent-loop 核心
- [ ] 错误分流重构:setup/transport/间歇 4xx 类 → 内部退避重试(独立预算,
      不计 feedback 配额);重试耗尽 → 整轮重试一次(5s/15s/45s 退避)
      再失败才 error 终止;仅"模型输出问题"(malformed args 等)走注入反馈
- [ ] feedback 配额语义:提供方抖动类错误不计入 MAX_FEEDBACK(各占各的配额)
- [ ] 注入文案区分:提供方错误 = "[harness] 提供方暂时不可用(…),harness
      正在重试";模型问题 = 现有"请纠正上一条输出"
- [ ] 重试预算耗尽进行整轮重试时落盘说明性事件(可复用 injected user-message 或专用事件)

### 3. server 接线
- [ ] 若 llm 层需要 emit 通道:server 构造 registry 时传入;或由 agent-loop
      用独立 sink 订阅(不阻塞 SSE 流)

### 4. 兼容与验证
- [ ] 旧日志兼容:所有新事件 serde default + skip;SESSION_FORMAT_VERSION 不 bump
- [ ] cargo test 全绿;新增:重试事件落盘/错误分流/超时阈值单测
- [ ] dev 实例 3601 构建 + 真实网关 smoke(glm-5.3-flash 大 payload,
      观察 retry-attempt 事件)

## 风险与取舍

- 整轮重试可能重复执行工具(副作用)→ 只在"尚无工具副作用"的 step
  (首个模型请求)失败时整轮重试,或重试前落盘说明
- STREAM_CLOSED 重试:可能拿到部分输出,重放会重复 → 仅在
  "0 chunk 即断流"时重试(有输出就不重试,保留部分结果注入)
- INVALID_REQUEST 重试:4xx 语义分两类(参数错误 vs 服务端拒绝),只能
  启发式:同一 step 形态之前成功过 → 视为抖动可重试

## 第一轮实施记录(已落地)

| 项 | 文件 | 说明 |
|---|---|---|
| 重试集 dsh 对齐 + 优化 | `llm/src/retry.rs` | default: maxRetries=5、退避 500ms→10s ±10%、按 dsh 公式;retryable = dsh 5 码 + INVALID_REQUEST/STREAM_CLOSED(实测优化);Retry-After > maxDelay 放弃重试(provider hint 优先) |
| 重试可见性 | `llm/src/retry.rs` `lib.rs` `agent-loop/lib.rs` `core/session.rs` | `RetryAttempt` 轨迹 + `RetrySink` 通道 + `retry-attempt` 日志事件(attempt/code/message/delay_ms) |
| 超时合理化 | `llm/src/openai.rs` `deepseek.rs` | connect_timeout 30→60s;请求总超时按 body 规模 60/120s(实测 200K payload TTFB 21.8s) |
| 错误分流(对齐 dsh 三失败位点) | `agent-loop/lib.rs` | setup 失败:registry 内部重试耗尽后,feedback_eligible(INVALID_REQUEST/MALFORMED/UNSUPPORTED_*)注入自纠(denia 特性),其余 error 终止;流错误:无输出才 step 内重试,否则终止且不终稿;finish error:step 内退避重试环(可取消),耗尽终止 |
| 不吞错误 | `agent-loop/lib.rs` | EMPTY_RESPONSE 等 finish-error 不再误判 Completed(core 单测 + loop 测试覆盖) |
| 日志平衡 | `agent-loop/lib.rs` | assemble 失败补 step-end + turn-end;所有错误路径 step/turn 配对(测试断言) |
| checkpoint | `session/lib.rs` `agent-loop/lib.rs` | `Session::flush()` 请求前刷盘,fail-closed(dsh checkpoint-policy 三 flush 点之一) |
| 孤儿轮 closers | `session/lib.rs` | load 时补未答复工具结果(`ORPHAN_TOOL_RESULT` 文案)+ step-end + turn-end{interrupted},时间戳复用最后事件 |

验证:全仓 109 测试全绿(新增:重试轨迹落盘、AUTH 不烧配额、finish-error 重试、错误终止日志平衡);dev 实例 3601 构建启动;旧会话(138b2b67 孤儿轮实测被自动闭合补全)。

## 遗留清单(后续轮次)

- 工具并行执行滚动池(dsh maxParallelToolCalls=10、model-order 提交;denia 现为顺序)
- `session/end-seed` 持久化标记(fork 种子边界,替代 fork_cut_index 纯函数推断)
- 人类反馈 `feedback/record` 事件(只进日志不进模型)
- series 语义(compaction/pre-step decision 来源,denia 暂缺)
- Retry-After/请求体规模的运行时配置化(现为硬编码阈值)