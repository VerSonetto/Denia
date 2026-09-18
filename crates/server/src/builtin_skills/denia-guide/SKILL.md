---
name: denia-guide
description: 用户请 AI 帮忙配置 denia 时加载：添加自定义提供商与模型、管理模型配置与密钥、创建 preset、接入 MCP 服务、改全局系统提示词。
metadata:
  short-description: denia 配置指南
---

# denia 配置指南

适用：用户说"帮我配 denia"——新增/排查自定义提供商与模型、配密钥、创建 preset、接入 MCP、改全局系统提示词。日常写代码别加载。

## 铁律（违反必翻车）

- 绝不用 read_file/write_file/edit 直写 `settings.yaml` / `.credentials.yaml`：绕过写入校验与 revision OCC，不触发热更新，密钥明文还绕过脱敏。读配置走 API（见下），写配置走 API 或控制台。
- 密钥只进 `PUT /api/credentials/{引用名}`，绝不写进 settings、不贴进聊天记录、不回显明文。`describe` 接口永远不回传明文，只说有无。
- 字段名照抄（含大小写，`baseURL` 不是 `baseUrl`）：provider/MCP 的未知键会被静默忽略，拼错等于没配。
- 删提供商/preset/MCP 服务器前先确认；写操作 409（revision 冲突）就重读再写一次。
- 当前只有 `llm-openai` 路由是活的：别配 `llm-deepseek`（代码里有、服务端没注册，写了也不生效）。

## 读写入口

- 服务地址用**运行时上下文快照里的「本实例 API」行**（每轮注入，就是当前进程的真实地址）。绝不凭默认端口猜：多实例共享数据目录，猜错会把配置写进另一个实例——你这边 HTTP 200 看似成功，用户的面板毫无变化。快照里找不到时才问用户。
- 读：`GET /api/settings`（全部命名空间快照，含各 `revision`）、`GET /api/llm/catalog`（ live 路由与模型）、`GET /api/mcp`（服务器与工具数）、`GET /api/agent-presets`（名册与健康状态）、`GET /api/system-prompt`。
- 写设置：`PATCH /api/settings/{ns}`（merge patch）或 `PUT`（整段替换，空对象=重置），body `{"value":{...},"expectedRevision":N}`，N 取自刚 GET 到的 revision。未知命名空间 404，revision 对不上 409。
- 数据目录默认 `~/.denia`（`settings.yaml`、`SYSTEM.md`、`agent-presets/` 等都在这），但走 API 就不用管路径。

## 自定义提供商与模型（命名空间 `llm-openai`）

结构：`{"providers": {"<路由id>": {baseURL, displayName?, apiKeyEnv?, protocol?, models[], defaultContextWindow?, defaultMaxTokens?, headers?}}}`。

- 路由 id：非空唯一即可，不限字符（常含点/下划线）；`baseURL` 必须是 `http(s)://` 开头。
- `protocol` 三选一，缺省 `openai-completions`：`openai-completions`（`POST {base}/chat/completions`）、`openai-responses`（`POST {base}/responses`）、`anthropic-messages`（`POST {root}/v1/messages`，base 以 `/v1` 结尾则保留否则补上）。未知值探测/对话直接 400。
- `apiKeyEnv`：凭据引用名（见下"密钥"），不填则无鉴权裸调。`headers` 附加请求头，值是整段 `${引用名}` 才走凭据解析，其余原样发送；空名/非法名请求前直接拒绝。
- `models` 是建议目录，空数组=每次走端点探测（`{base}/models`，anthropic 系走 `{root}/v1/models?limit=1000`）。目录只管展示与默认值：未知模型 id 照样能调（按路由默认值放行），所以"列表里没有"不是"调不通"的原因。
- 模型行字段：`id`（必填，同路由内唯一，允许 `/` `:` 等网关风格）、`name`、`description`、`contextWindow`、`inputModalities`（缺省 `["text"]`，识图加 `"image"`）、`thinkingSupported`、`reasoningEfforts`（仅思考开启时有效，取 `off/low/medium/high/xhigh/max` 子集）。回退链：行缺省→路由 `defaultContextWindow/defaultMaxTokens`→出厂默认（262144 / 32768）。
- 服务端无默认模型：每次 prompt 都带 provider/model，前端靠 localStorage（`denia.last-model`）记住上次选择。`POST /api/llm/discover`（`{baseURL, apiKey?（一次性，不落盘）|apiKeyEnv?, protocol?, headers?}`）先探测再导入；`POST /api/llm/chat`（`{provider, model, ...}`，两者为空直接 400）做连通冒烟。
- 新增标准动作：探测→定路由 id 与 protocol→（有密钥先 `PUT /api/credentials`）→整段读出、追加新路由后 `PUT` 写回→`GET /api/llm/catalog` 确认出现→chat 冒烟。

## 密钥

- 引用名：`^[A-Za-z_][A-Za-z0-9_]*$`；值：非空、可打印 ASCII、无空格。解析链：进程环境 > `.credentials.yaml` 文件 > 项目 `.env` > 用户目录 `.env`（每次实时重算）。
- 环境变量里已存在同名非空值时，`PUT/DELETE` 直接报 shadowed：去启动服务的 shell 里 unset，不要硬写文件。
- 新路由的引用名按控制台惯例派生：路由 id 大写、非 `[A-Z0-9]` 变下划线、末尾加 `_API_KEY`、数字开头补 `_` 前缀（如 `my-gateway`→`MY_GATEWAY_API_KEY`）。`.env` 行格式：`KEY=value`，支持 `#` 注释、`export` 前缀、单/双引号。
- 查状态：`GET /api/credentials?refs=A,B`（单次≤64，只回 `{configured, source, writable}`）。

## Preset（Agent 组装）

- 落盘：`<home>/agent-presets/<id>/preset.yml`，id 必须 `^[a-z0-9][a-z0-9-]*$`（同时是目录名）。随附三个：`standard`（全量）、`creator`（引导创作的多轮 ask 流程）、`minimal`（仅 shell、一句话 persona 独占、功能全关）。随附的不可删不可改。
- 文件字段：`name`、`description`、`tools`（省略=全量，空列表直接拒绝）、`persona`（省略=沿用部署 persona）、`personaComplete`（true=persona 独占系统提示，工具 schema 保留）、`features`（省略的键=开启，未知键直接拒绝；可用键：`agentsMd, memory, compaction, goal, skills, subagents, jobs, browser, ask, planMode`）。收窄只交不并：features 先摘、tools 白名单继续摘，`mcp__` 开头免校验。
- 损坏的 preset 会以 broken 行留在名册里并写明原因：先读原因再修，不要删了重建（id 占着）。
- 创建只走两条路：调 `create_preset` 工具（先用 ask 分轮收集 persona/工具面/功能开关→再用 ask 展示摘要拿到确认→一次会话只建一个），或 `POST /api/agent-presets` 从既有 preset 复制（`{from, id, name?}`，同名 id/已存在目录都拒绝，绝不覆盖）。改既有 preset 让用户亲手改 `preset.yml`（有 watcher，200ms 防抖热刷新，下一步即生效）；查单个文本走 `GET /api/agent-presets/{id}`，删走 `DELETE`。
- 默认 preset 在 `agent-presets` 命名空间（`{default, modeSelectionEnabled}`）：写不存在的 id 不报错但读取时回退 `standard`，所以改完默认要 `GET` 名册确认 id 真存在。

## MCP 服务（命名空间 `mcp`）

结构：`{"servers": [{id, transport, command, args, env, cwd, url, headers, enabled, disabledTools, scope, workspaceId, protocolVersion, callTimeoutMs}]}`，最多 32 个服务器、单个命令最多 64 个参数，整段校验不过整段拒绝并点名。

- `id`：`[a-z][a-z0-9-]*`、1–32 字符，同时是模型侧工具名前缀。`transport`：`stdio | http | sse`。
- `stdio` 必须给 `command`（`args` 无空串，`cwd` 给了就必须是绝对路径）；`http/sse` 必须给合法 `http(s)` 的 `url`，且不接受 `command/args/env`（填了就拒绝）。`env`/`headers` 的值是凭据，只存不回传。
- `protocolVersion` 目前固定 `2024-11-05`（空拒绝）；`callTimeoutMs` 给了就得在 1–600000 之间；`scope: project` 必须配 `workspaceId`（本期仍按全局生效，先保证字段合法）。
- 写路径：改 settings→重连→同步工具面→广播，控制台 `PUT /api/mcp/servers`（`{server, expectedRevision?}`，同名替换否则追加，顺序即展示顺序）一条龙；只开关某个工具走 `POST /api/mcp/tools`（不过滤重连，最便宜）。模型侧工具名形如 `mcp__<server>__<tool>`。
- 接入后模型默认只看到 MCP 服务器目录和 `mcp_list` 工具（工具清单与参数定义按需发现、首次调用装载）——这是目录化工具面的预期行为，不是没生效；生效的判定口径是 `GET /api/mcp` 里 status=connected 且 toolCount 增加。
- 常见拒绝：id 非法/重复、传输不支持、http 缺 url、stdio 缺 command、cwd 相对路径、参数超限、project 缺 workspace_id——报错里都已点名，按字面修。

## 全局系统提示词与控制台默认

- `GET/PUT/DELETE /api/system-prompt` 读写 `<home>/SYSTEM.md`：空或缺失=出厂 persona；PUT 空文本=删文件回出厂；有 watcher，改完即时生效。
- `console` 命名空间管主题（`system|light|dark`）、语言（`zh|en`，默认中文）、压缩与微压缩参数、工具并行上限、新建会话默认权限（`read-only|auto-edit|full`，没有 plan）。

## 验证与排错

- 配完提供商：catalog 里出现路由→chat 冒烟通→会话输入框能选到。`failures` 里有该路由=端点探测失败，看 message（多为 baseURL/protocol/密钥错）。
- 配完 MCP：`GET /api/mcp` 里状态正常且 toolCount 增加；连不上只标 error 不阻断启动，逐个修。
- 配完 preset：名册里健康行（无 `broken`）、新会话选择器可见。
- 高频坑：`baseURL` 大小写错键、protocol 拼错、模型 id 前后空格、headers 名非法、revision 拿旧了（409）、环境变量遮蔽密钥（shadowed）、preset id 大写或下划线、MCP 用 http 却填了 command、project 作用域漏 workspace_id。
