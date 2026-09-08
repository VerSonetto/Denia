# `ask` 工具:模型向用户提问

对照 dsh `ask_user_question`(`packages/interaction/tool-ask-user` + `user-questions`)
的设计评审与 denia 实现说明。

## 一、协议

模型调用:

```json
{
  "questions": [
    {
      "id": "cleanup",
      "question": "确认删除这 3 个文件吗?",
      "header": "确认",
      "detail": "可选补充说明(markdown)",
      "options": [
        { "label": "删除", "description": "移除 3 个过期文件", "recommended": true },
        { "label": "保留", "description": "中止清理" }
      ],
      "multiSelect": false,
      "allowCustom": true
    }
  ],
  "timeoutMs": 300000
}
```

工具结果(普通 `ToolResult`,回注 agent loop):

```
用户已回答。
answers: [{"id":"cleanup","selected":["删除"]}]
```

结局 `outcome` 四态:`answered` / `timed-out` / `cancelled` / `unavailable`。

## 二、dsh 设计的不足与 denia 的改进

| # | dsh 的做法 | 不足 | denia 的做法 |
|---|---|---|---|
| 1 | 无超时,工具挂起到用户作答或被轮次取消 | 无人值守/用户离开时工具**永久挂起**,轮次永不结束 | `timeoutMs`(缺省 5 分钟,上限 30 分钟),到点按 `timed-out` 结算并给出"据现有信息继续"的引导 |
| 2 | 失败一律 `Error: …`(NO_PROVIDER / ASK_ABORTED / DELEGATED_CALLER 都是硬错误) | 模型无法区分"用户没空答"与"答了空",容易把交互失败当成工具故障反复重试 | 四种结局都是**成功结果**(`is_error: false`),各自带可执行的下一步指引 |
| 3 | 推荐项靠标签后缀 `"(Recommended)"` 字符串约定 | 多语言/全半角括号/大小写都会失配,UI 只能正则猜 | `recommended: true` 结构化字段,UI 直接高亮,标签文本可自由本地化 |
| 4 | 自由填写是选项列表末尾的伪选项(单选时 `custom` 覆盖 `selected`) | "其它"混在选项里,与真实选项视觉同级;多选语义靠文档说明 | `allowCustom` 显式声明;自由填写独立一行输入框,单选填自定义即替换选择,多选则与勾选并存 |
| 5 | 无"跳过"的一等语义(UI 用 `{id, selected: []}` 表示) | 模型无法区分"用户跳过了"与"用户没答这题" | `skipped: true` 显式标记,工具结果原样回传 |
| 6 | 一次只显示一道题、底部翻页,答完最后一题才能提交 | 大屏浪费空间;"一共几题、答了几题"不可见;必须按顺序走完 | **整组同屏清单式**:顶部进度、任意顺序作答、每题独立跳过、底部一次提交 |
| 7 | `detail` 有词汇但无渲染约束;`intent`(`plan-review`)需要 UI 认识标签 | 能力标签只改变呈现,不认识标签的 UI 降级;意图校验靠 `BAD_INTENT` 运行时拒绝 | 只保留 `detail`(渲染在问题下方);不引入意图标签——需要"计划审批"这类形态时由调用方在 `options` 里表达,协议单一 |
| 8 | 无提问数量/选项数量上限 | 模型可以一次抛出几十个问题把 UI 压垮 | `questions` ≤ 8、每题 `options` ≤ 12,超限报可执行的参数错误 |
| 9 | `id` 由调用方给且无唯一性校验(重复 id 会让 UI 配对失败) | 重复 id 在 UI 侧静默降级为计数摘要 | 校验 id 非空且唯一,重复直接报错 |
| 10 | 请求与应答共用同一 id 空间(与审批同形) | 重试/多批次时旧应答可能误配新请求 | `request_id` 与 `call_id` 分离;`request_id` 唯一标识挂起,`call_id` 供 UI 把卡片挂到对应工具行 |
| 11 | 工具调用阻塞期间不落盘请求 | 前端刷新后无法恢复挂起卡片(状态只在内存) | 先落 `ask-requested` 事件再等待,刷新/重连按日志重建挂起态;结算落 `ask-resolved` |
| 12 | 子代理继承父代理完整工具面,提问靠 `DELEGATED_CALLER` 运行时拒绝 | 子代理拿得到交互/写类工具,却在调用时才被拒;模型先浪费一次调用,还可能重试 | **工具授予层收口**:子代理默认只拿只读集合(`SUBAGENT_READ_ONLY_TOOLS` = read_file/glob/grep/skill/browser),根本看不到 ask/写/命令/再委派;纪律段随工具同步过滤;服务层再留一道 `unavailable` 兜底 |

## 三、事件与 REST

日志事件(append-only,`crates/core/src/session.rs`):

- `ask-requested { request_id, call_id, questions, timeout_ms }`
- `ask-resolved { request_id, resolution }`

两者**不进入模型历史**(`derive_messages` 只投影 user/assistant/tool-result);
模型通过工具结果看到问答,与 dsh 一致。

REST:

- `POST /api/sessions/{id}/asks/{request_id}` body `{ answers: [...] }` 作答
- 同一端点 body `{ answers: [], cancel: true }` 放弃整组
- 重复应答返回 404 `ask/not-found`(单次原子结算,不覆盖已生效回答)

## 四、UI 差异(与 dsh 刻意区分)

| 维度 | dsh `QuestionComposer` | denia `AskCard` |
|---|---|---|
| 布局 | 单题卡片 + 底部翻页 | 整组清单,题间分隔线 |
| 进度 | `1 / 3` 页码 | `已答 2/3` + 每题状态标记 |
| 提交 | 答完最后一题自动提交 | 底部「提交回答」,全部处理完毕才可点 |
| 推荐 | `(Recommended)` 后缀解析 | 结构化 `recommended` 徽标 |
| 计时 | 无 | 头部倒计时,剩余 < 30s 转警示色 |
| 只读态 | 摘要 + 折叠正文 | 问题/答案逐条对照(选项 chip + 自定义文本) |
| 视觉 | dsh 卡片阴影/圆角语言 | 本仓 token(边框层级 + 语义别名),无新增设计变量 |

## 五、接入点

- 工具:`crates/tools/src/ask.rs`(`AskTool`)+ `AskBridge` 契约(`crates/tools/src/lib.rs`)
- 通道:server `ServerAskBridge`(`crates/server/src/state.rs`)
- 事件:`crates/core/src/session.rs`;REST:`crates/server/src/api/sessions.rs`
- 提示词:纪律段 `tool:ask`(`crates/tools/src/prompt.rs`),槽位
  `SectionOrder::ToolAsk`;schema 与纪律段同分支注册
- 前端:卡片 `web/src/components/AskCard.tsx`,事件接线 `web/src/fold.ts`
- 子代理收口:只读工具集 `SUBAGENT_READ_ONLY_TOOLS`(`crates/tools/src/lib.rs`),
  授予与校验在 `crates/server/src/agent_runtime.rs::delegate`

## 六、子代理为什么不能提问

子代理不与用户交互:它没有会话界面、也没有用户在等它,提问在语义上不成立。
因此 denia 不靠"调用时拒绝",而是在**工具授予层**直接不给:子代理默认只拿
只读工具(`read_file`/`glob`/`grep`/`skill`/`browser`),`allowed_tools` 只能
在这个集合内缩小,请求写文件/命令/提问/再委派一律报错(不静默忽略)。这样
模型压根看不到 ask,不会浪费一次调用去试。服务层仍保留 `unavailable` 兜底,
防御未来绕过授予的路径。

子代理的系统提示词同样按授予过滤:`tool:ask`(以及 `tool:bash`/`tool:write`/
`tool:edit`/`tool:agents`/`tool:jobs`/`tool:todo`)纪律段随工具一起消失,
避免模型读到不存在的工具纪律。
