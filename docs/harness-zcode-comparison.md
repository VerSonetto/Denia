# Denia 与 ZCode：同模型表现差异的源码审计

日期：2026-10-02。

## 六项修复实施记录（2026-10-02）

下文保留修复前的审计证据；其中“当前源码”“未修改源码”描述的是审计时点，不是本次实施后的状态。

### 阶段一：输出执行与证据

- 生产使用的一次性 Bash 并发消费 stdout/stderr，等待退出与读管道并行；退出、超时和取消后的管道收尾最多两秒。预览有界，双路分别保留头尾。
- 会话 `tool-output` 目录保存产物，单调用 64 MiB、会话 256 MiB；并行调用与跨 turn 后台生产者共享容量锁，重启扫描已有占用，不删除旧产物。存储错误或达到容量上限继续消费并标注不完整。
- `read_tool_output` 使用产物 ID、stream、行 offset/limit；默认 400 行、最多 1,000 行。拒绝其他会话产物、路径和符号链接逃逸；遵循 preset 工具白名单。删除会话沿既有目录删除路径清理产物。
- 非 Bash 长结果保存全文，ToolResult.meta 携带产物信息；恢复截断元数据，并将截断提示本身纳入 32,000 字符预算。后台任务同样保存双路产物，结束预览分别保护 stderr。
- 验证：1 MiB stdout、stderr、双路；Unicode 头尾与分页；容量、并行、重启、存储失败、跨会话和路径拒绝；取消与超时；后台 Unicode 输出及回读。未启用的实验性持久 ShellHub 未改变其状态语义。

### 阶段二：思考回放与参数

- 历史投影与 token-meter 共用 assistant 重建；保留纯思考消息及兼容旧 JSONL 的可选载荷。
- Chat Completions 回传 reasoning_content；Anthropic 保留 thinking/signature/redacted_thinking；Responses 保存原生 reasoning item 并请求 encrypted_content。缺少必要签名或密文的旧记录不伪造回放。
- 明确拒绝历史推理的错误在普通重试前降级，原始日志不变。降级按 turn 和路由隔离，摘要与续写共享；鉴权、限流、网络、普通参数错误不移除思考。降级不重置普通网络重试预算。
- 三种协议使用显式 max_tokens → provider defaultMaxTokens → 32,768，再限制模型已配置上限；请求头记录有效值，主动压缩预留相同预算。保留显式 temperature，不添加模型名称名单。
- 验证：三协议请求、签名及密文流式往返；降级一次、切路由与新 turn 恢复、原始历史保留；输出预算优先级和上限。官方文档抓取被站点拒绝，因此没有把在线文档核对宣称为已通过。

### 阶段三：长任务恢复与无进展检测

- 输出触限无合法调用时保存部分消息并最多续写三次；标记不完整或非法 JSON 的调用不执行，完整调用只在所属 step 执行。
- 明确输入超窗时每步最多强制压缩一次；关闭、无可压缩区间、失败或断路时停止。收窄 HTTP 错误分类，普通 length/tokens 参数错误不触发压缩。
- 执行工具后比较规范化动作、结果及错误状态，三次提醒、十次停止；结果变化、其他动作或真实用户输入重置。文本重复不单独停止。全文摘要能识别预览中间被隐藏的输出变化，产物 ID 与展示时间不算进展。
- 新增本地 HTTP/SSE mock gateway，使用真实 OpenAI-compatible adapter 走请求 → 思考拒绝 → 降级 → 工具结果 → 截断续写 → 完成；断言工具仅执行一次、预算正确、下一用户 turn 恢复回放。另覆盖续写耗尽、不完整调用、强制压缩上限及跨用户轮次重置。

### 验收结果与已知失败

- 专项：agent-loop 100 项、llm 47 项通过；工具库 169 项通过、10 项原有忽略；新增共享存储后输出专项 4 项通过，后台任务专项 2 项通过。
- 已执行 `cargo test --workspace --offline --no-fail-fast`。唯一剩余失败为既有 `api::remote::tests::tickets_are_tracked_per_channel`：本机选中 172.29.128.1 网卡，而测试只接受特定局域网地址。未修改该网络模块。
- `npm run check` 在既有 check-tool-display.mjs 失败：脚本仍直接取 editDiff.lines/removed/added，而实现已返回 diffs 数组。其余检查继续单独执行通过；`npm run check:memory` 与 `npm run build` 通过。未重构前端内存功能。
- 未创建分支或提交，未访问真实模型凭据，未发送真实模型请求；功能测试不能证明真实模型任务成功率提升。桌面构建在验收后执行，产物为 target/release/denia-desktop.exe。

## 结论与边界

Denia 不是缺少基本能力的空壳。当前源码已经有事件源会话、并行工具池、微压缩、摘要压缩、项目指令、技能、记忆、子代理、后台任务和循环检测。优先问题不是继续堆功能，也不是先重写整套提示词，而是保证推理状态、诊断证据、模型参数和恢复路径在闭环中不被破坏。

本次发现一个实际复现的命令执行问题、一个实际复现的信息丢失机制，以及多处可直接从源码确认的策略差异。这些能够解释部分“同模型变笨”的体验，但没有两边同一个真实 bug 的请求轨迹，因此不能宣称已经证明某次失败的唯一根因，或者证明 ZCode 在所有任务上更优。

比较基线：

- Denia：`D:\Denia`，HEAD `514d61d1911df5183e7f307a95885d9c4e87fc6c`，同时审查当前工作区内容；已有未提交改动未覆盖。
- ZCode：`C:\Users\sonetto\.codex\reference-repos\ZCode`，浅克隆 HEAD `29628c9acdb81b703bbd4080c207a0e7ce5e276e`，提交时间 2026-09-24 14:49:06 +08:00，提交描述 `feat: update v3.14.3`。
- ZCode 仓库基线检查通过；未安装或运行 ZCode，未执行其真实模型请求。这是检出源码的比较，不是用户安装版本的运行时认证。
- 临时复现程序：`C:\Users\sonetto\AppData\Local\Temp\denia-harness-audit-20261002\src\main.rs`。直接调用当前 Denia 的工具库，没有修改项目源码。

## 优先级概览

| 优先级 | 差异 | 证据状态 | 主要影响 |
| --- | --- | --- | --- |
| P0 | 命令退出前不消费 stdout/stderr | 当前工具实际复现 | 假超时，阻断测试和诊断 |
| P0 | 历史推理内容一律不回传 | 源码、既有单测确认 | 支持推理保留的模型丢失工具链推理上下文 |
| P0 | 输出只保留前 32,000 字符，无通用完整输出产物 | 当前函数实际复现 | 尾部错误、stderr、测试结论丢失 |
| P1 | 通用参数映射、输出预算接线不足 | 源码确认；实际服务端行为未确认 | “同模型同档位”不一定同请求 |
| P1 | 输出截断、真实超窗后的恢复不同 | 源码确认 | 长任务提前终止，而非恢复后继续 |
| P1 | 重复指纹第四次强制停止且跨 turn 保留 | 源码确认 | 轮询和再次尝试可能被误判为无进展 |

P0/P1 是本次建议的排查顺序，不是已经通过修 bug 成功率评估得出的收益排名。

## 1. 命令执行：这是执行器问题，不是模型推理问题

Denia 的一次性 shell 路径先把 stdout/stderr 配置成管道，然后等待 `child.wait()`；只有子进程退出后才调用 `wait_with_output()` 读取输出。输出足够大时，子进程写满管道，等待读取者；父进程却在等子进程退出。增大超时不能解决这种相互等待。

证据：

- `D:\Denia\crates\tools\src\bash.rs:194`：stdout/stderr 使用管道。
- `D:\Denia\crates\tools\src\bash.rs:230`：先等待退出。
- `D:\Denia\crates\tools\src\bash.rs:254`：退出后才读取输出。
- `D:\Denia\crates\server\src\state.rs:980`：当前 server 注册的 BashTool 没有挂载 ShellHub，因此上述路径不是一个永远不会使用的备用实现。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\adapters\src\exec\node-execution-adapter-run.ts:284`：等待退出之前已挂载输出消费逻辑；另一条 Bash 路径使用文件输出。

在同一台机器上直接调用 `BashTool::new()`，timeout 设为 3,000 ms：

| 实验 | 结果 |
| --- | --- |
| Node 输出一小段文本 | 497 ms，成功 |
| Node 向 stdout 输出 1 MiB | 3,011 ms，工具报告超时 |
| Node 向 stderr 输出 1 MiB | 3,018 ms，工具报告超时 |
| 对照：同样 1 MiB stdout，用 Tokio `wait_with_output()` 并发读取 | 62 ms，成功，收到 1,048,576 bytes |

影响：原本快速结束的测试可能被错误地报告为超时。模型拿不到真实失败信息，还会受到“提高 timeout、拆小命令”的恢复建议影响，在错误方向上消耗更多时间。默认 timeout 是 120 秒，这尤其容易被用户感知为“想了很久却没有解决”。这没有证明每个长耗时都来自该问题。

建议：等待退出和消费两路输出必须并发；保留取消、超时和退出码语义；覆盖大 stdout、大 stderr、非零退出、取消及超时测试。不要仅增加 timeout。

## 2. 推理历史：Denia 在模型请求投影时一刀切删除

Denia 能接收、展示和记录 Reasoning，但生成下一次模型请求时，只从 assistant blocks 提取文本与工具调用。构造 `ChatMessage::assistant(text, None, calls)`，所有模型的 `reasoning_content` 都被清掉。

证据：

- `D:\Denia\crates\core\src\session.rs:979`：assistant 事件投影。
- `D:\Denia\crates\core\src\session.rs:1010`：无条件传入 None；注释直接假设不影响后续决策，这不是已经做过质量评估的证据。
- `D:\Denia\crates\agent-loop\src\request.rs:64`：真实模型请求采用这份派生历史。
- `D:\Denia\crates\core\src\session.rs:1439`：既有单测明确断言推理内容为空。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\adapters\src\model\reasoning-history-normalization.ts:30`：按模型、提供方兼容性和历史结构归一化，不是全部丢弃。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\adapters\src\model\transform.ts:300`：有效 reasoning blocks 进入请求投影。

Z.AI 的官方 Thinking Mode 文档明确说明：对于支持 Preserved Thinking 的模型，多步骤工具任务需要正确回传 `reasoning_content`；GLM-4.7 和 GLM-5 的说明要求工具调用消息中的该字段完整、未修改地返回。

参考：https://docs.z.ai/guides/capabilities/thinking-mode

影响必须分模型判断：这不表示所有模型都需要回放全部推理，也不表示 Denia 关闭了当前请求的思考；问题是它不允许 adapter 按模型协议保留需要的推理状态。对支持该机制的模型，这是高优先级质量风险。若实际使用的模型或网关不支持它，收益需要另行测试。

建议：保留原始推理块和需要的 provider 元数据，在 adapter 边界决定保留范围、跨模型清洗、签名处理及压缩策略。不能简单给所有模型无条件回放全部历史文本。

验证：`cargo test -p denia-core derive_projects_user_assistant_and_tool --offline -- --nocapture`，1 项通过。它证明当前投影确实采用删除策略，不证明这种策略正确。

## 3. 输出预算：删掉诊断证据与提供可回读的产物不是同一回事

Denia 的统一预算保留前 32,000 个字符。超出的内容在结果落盘前丢掉，只给出缩小范围或分页的提示。对于一次性命令，该提示并不能恢复已经执行过的输出。

更重要的是 Bash 结果先放 stdout，最后才追加 stderr。因此较长 stdout 可以挤掉全部 stderr，也可以挤掉位于日志尾部的测试结论。

证据：

- `D:\Denia\crates\tools\src\support.rs:18`：32,000 字符上限。
- `D:\Denia\crates\tools\src\support.rs:29`：只取输出头部。
- `D:\Denia\crates\agent-loop\src\exec.rs:147`：截断后的文本才进入 ToolResult 日志。
- `D:\Denia\crates\tools\src\bash.rs:264`：stdout 在前、stderr 在后。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\core\src\tool\handlers\bash.ts:480`：Bash 使用 artifact 策略、tail 预览、session 保留。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\core\src\tool\result-persistence-format.ts:28`：模型可以得到保存路径及预览。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\adapters\src\exec\output-collector.ts:51`：超限时启动输出持久化；产物仍有资源上限，不是无限保存。

独立复现：将 40,000 字符 stdout 与尾部 `ROOT_CAUSE_MARKER` 交给 Denia 当前 `apply_output_budget`，得到 `was_truncated=true`、`root_cause_preserved=false`。

影响：模型可能确实调用了测试，但只看到了启动过程而没看到最终失败原因。重新运行不一定能恢复证据，可能再次得到同样的截断。

建议：输出保存为有界产物，向模型提供路径、退出码、头尾摘要和分页读取方式。stdout/stderr 分开保护；不能只把上限从 32K 调得更大。

## 4. 参数映射与输出预算：相同显示名称不能保证相同有效调用

Denia 当前 agent request 固定 `temperature=None`、`max_tokens=None`。OpenAI-compatible adapter 虽然计算了 request/profile 的输出上限，但只在 Anthropic 分支使用这个计算结果；Chat Completions 和 Responses 构造器读取原始 request，因此配置的 defaultMaxTokens 没有沿这条路径进入请求 body。

另一方面，Chat Completions 的推理参数基本是直接发送 `reasoning_effort`；`off` 表示不发送该字段，不是发送明确关闭指令。不同模型、API 和网关是否接受这些参数，需要实际确认。

证据：

- `D:\Denia\crates\agent-loop\src\request.rs:61`：真实请求的可选参数。
- `D:\Denia\crates\llm\src\openai.rs:382`：计算输出上限后，只有 Anthropic 分支消费。
- `D:\Denia\crates\llm\src\protocols.rs:123`：Chat Completions body 参数映射。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\config\provider\zcode-builtin.json:2484`：按模型和 API 类型定义映射规则。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\packages\model-option-map\src\option-maps.ts:18`：编译并组合模型选项映射。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\adapters\src\model\model.ts:107`：验证输出 token 选项。

结论：接线差异已确认；实际两边推理强度、网关默认输出预算是否不同尚未确认。不应据此声称“Denia 肯定没开思考”。ZCode 的映射表也不能未经检查直接复制到所有网关，因为部分网关拒绝额外字段。

建议：定义显式的 provider/model/API capability contract，确保选项从配置到最终 HTTP body 一致。评估时比较脱敏后的实际请求，而不是 UI 档位。

## 5. 长任务恢复：触碰边界后是否能继续

Denia 在响应到达 MaxTokens 时直接结束 turn，标记 MaxTokens。对于被准确分类为 CONTEXT_WINDOW_EXCEEDED 的请求错误，也没有在该错误路径压缩后重试，反馈白名单不包含这个错误码。主动压缩存在，但不能覆盖所有估算失误或服务端超窗情况。

ZCode 对没有工具调用的输出上限响应有最多三次续写，并保留局部请求状态；对真实输入超窗错误有 reactive compact 恢复路径。它同样有恢复上限和失败状态，不是无限重试。

证据：

- `D:\Denia\crates\agent-loop\src\turn.rs:318`：MaxTokens 直接闭合。
- `D:\Denia\crates\agent-loop\src\request.rs:119`：建流失败分流。
- `D:\Denia\crates\agent-loop\src\errors.rs:16`：反馈白名单。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\core\src\runtime\methods\turn-output-token-continuation.ts:18`：续写上限。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\core\src\runtime\methods\turn-model-step.ts:660`：续写推进。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\core\src\runtime\methods\turn-model-step.ts:742`：真实超窗恢复。

影响：若任务常常触碰输出预算或上下文边界，一个系统会在中途停止，另一个可以保留状态继续；用户容易把这种差异理解成推理能力差异。未触碰这些边界的任务不受该机制直接影响。

## 6. 循环检测：相同动作并不等于没有新证据

Denia 对文本和工具参数指纹分别计数，第三次提醒、第四次结束。工具检测只看 name/arguments，不看工具结果是否改变，也不看工作区是否变化；检测状态跨 turn 保留。

ZCode 的重复工具检测也使用输入指纹，但这条路径主要注入 warning，不在第四次直接终止工作。它还有取消、权限、压缩快速回填等其他资源边界，不能简化成“不设限”。

证据：

- `D:\Denia\crates\agent-loop\src\loop_guard.rs:72`：只观察 assistant blocks。
- `D:\Denia\crates\agent-loop\src\loop_guard.rs:146`：工具签名只包含工具名和参数。
- `D:\Denia\crates\agent-loop\src\turn.rs:219`：第三次提醒、第四次强制闭合。
- `D:\Denia\crates\agent-loop\src\lib.rs:423`：检测状态按 session 跨 turn 借出并归还。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\core\src\runtime\helpers\model-anomaly.ts:23`：重复检测返回 warning。
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\core\src\runtime\methods\turn-tool-warnings.ts:52`：提醒进入后续模型请求。

影响例子：同参数查询后台任务，结果可能每次都在推进；用户重新要求尝试相同动作，历史指纹也未必应该继续累加。这些目前不能仅靠动作签名区分。

建议：区分重复动作与无进展，结合结果指纹、外部状态、用户新输入和预算；先引导重规划，明确的资源边界再决定停止。不是简单删除所有保护。

## 不应优先归因的东西

- “Denia 没有并行、压缩、技能、记忆或子代理”：与当前源码不符。
- “ZCode 有一句神奇提示词”：没有证据。两边的沟通、自主推进和上下文管理文字大量同义，摘要模板也高度相似。Denia 的工具规则更刚性，但未通过对照评估证明这是主因。
- “中文提示词天然更差”：本次没有这种证据。
- “多 agent 一定更聪明”：没有真实轨迹证明所比较任务使用了子代理，更没有证明收益来自 agent 数量。
- “循环检测、压缩或工具截断本身都错了”：这些机制有必要；问题在保留什么、允许什么恢复，以及是否能把真正无进展与有效探索区分开。

提示词与摘要比较入口：

- `D:\Denia\crates\tools\src\prompt.rs:424`
- `D:\Denia\crates\agent-loop\src\compact.rs:259`
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\core\src\context\dynamic-sections.ts:26`
- `C:\Users\sonetto\.codex\reference-repos\ZCode\apps\zcode-cli\packages\core\src\compact\prompt.ts:14`

## 下一步如何从机制推断升级成真实归因

1. 先修执行器和诊断证据链，避免用有问题的测量通道评估模型。
2. 从真实失败中选一组可以复现和验收的 bug；两个 harness 从独立、相同的 Git 状态开始。
3. 锁定实际模型版本、endpoint/API、推理参数、输出预算、工具权限、项目指令及资源预算。保存脱敏后的 wire request，不仅记录 UI 设置。
4. 每个 bug 在每个配置下独立重复运行，分别记录验收通过率、有效工具反馈、耗时、token、退出原因和关键证据获得顺序。
5. 在 Denia 内做消融：只改变推理保留、输出策略、恢复策略等一个因素。对比执行轨迹首次分叉的位置，而不是仅比较最后回答。

不能用“单位测试通过”代替 agent 成功率评估，也不能用某个任务的一次成功替代稳定性评估。

## 本次执行范围

- 克隆并只读检查 ZCode；基线新鲜度检查通过。
- 使用临时 Cargo 程序直接调用 Denia 工具库，复现大输出超时和尾部证据丢失；并发读取对照成功。
- 执行 Denia 的推理历史投影既有单测，1 项通过。
- 未修改任何项目源码、模型配置或凭据；未启动服务或发送真实模型请求。
- 仅新增本分析报告，原有工作区改动保留。编译期间已有 unused/dead-code 等 warning 未处理。
