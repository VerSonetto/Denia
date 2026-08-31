# AGENTS.md

dsh-rs 是 DeepSeek Harness(dsh)的 Rust 重写版:后端 Rust(axum + tokio),控制台 React + Vite + TS。本文件是仓库规范,在这个仓库干活的 agent 必须遵守。

## 项目定位与参考源

- **dsh(`D:\code_project\deepseek-harness`)是参考设计**。工作区管理、会话模型、目录选择器、i18n、交互形态等,dsh 有成熟设计就**先抄思路和交互形式**,不要自己造轮子。抄不了的部分(如 dsh UI 依赖 Cordis 浏览器插件运行时)说明边界后再简化实现。
- **设计哲学:极致性能,性能至上**。前端优先增量更新(流式 envelope 增量 fold,O(1)/条), ticking UI(计时器等)隔离成叶子组件;后端阻塞 fs/OS 调用一律 `spawn_blocking`,不挡异步运行时。
- 现阶段后端基本稳定,**优化重心在前端交互**;动后端前先问用户。

## 语言规范

- **一切用户可见文本用中文**:UI 文案、agent 系统提示、commit message、工具调用描述、报错信息。
- i18n **zh-source**:中文是键集源头,`en` 用 `typeof zh` 编译期对齐;语言进设置(`console.locale`),**默认中文,不靠浏览器 navigator 猜**。
- agent 系统提示用中文,并明确"始终使用简体中文回复,除非用户明确要求其他语言"。
- 代码标识符用英文;注释中文优先。

## 架构规范

- **事件源**:session 是 append-only JSONL 日志,模型历史由 `derive_messages()` 派生;model-visible == logged。会话头 cwd 不可变。
- **工作区是独立持久化域**(`workspaces.json`),不从会话派生:uuid id、规范化 path、sessionIds 账本;成员显示 = 账本 ∩(会话头 cwd == 工作区路径);创建幂等;首启 bootstrap 按会话头 cwd 自动分组;删除只删注册,会话落"未分组"。
- **目录选择器是 seam**:`/api/fs/capability` 启动时一次决策(远程/SSH → browse;win/mac → native;linux 看 DISPLAY+zenity),上层代码零分支。
- **配置驱动,无硬编码可调量**(dsh 式 validated config):`console` 命名空间管 sandbox/theme/locale/maxStepsPerTurn,校验失败在写入层拒绝。
- **fail loud**:死目录 prompt 400、坏配置拒绝、未知事件类型拒绝;不静默跳过。
- **报错不直接中断**:模型请求被提供方拒绝时,把错误以 injected 用户消息回注给模型让它自纠正,每轮最多 2 次防死循环;工具错误本来就以 isError 工具结果回给模型。
- **容错边界**:工具参数宽容解析(取第一个 JSON 值忽略尾部垃圾);wire 回传 tool_calls 参数清洗(畸形抢救第一个 JSON 值,否则 `{}`),不让一次坏输出毒化后续请求。
- crate 依赖方向:`core ← settings/credentials/llm/tools/session ← agent-loop ← server`。

## 交互形态规范(抄 dsh,改交互时对照)

- blank 会话(未发第一条消息)复用不新建——仅限启动自动落点与切换工作区;侧栏"新建会话"按钮**总是创建全新会话**,不复用。
- 新会话页即 blank 欢迎态:品牌问候 + 居中输入卡;发出首条消息立即切 transcript,不回欢迎页。
- **先选工作区才能发消息**:无工作区时输入栏 inert,点击弹选择器。
- 首条消息前可切工作区,发出后锁死。
- 侧栏工作区树:分组、默认 5 个会话 +"展开其余 n"、未分组桶、组内新建、删除带确认。
- 轮次关闭后,到最后一条工具调用为止(含最后一条)的过程内容折叠成一行"已工作 <时长> · N 次工具调用"概览(时长:<60秒直接秒数,≥60秒为分秒,≥1小时为时分秒),点击展开;其后的回答保持可见,真实用户消息不折。运行中不设"过程"组:工具调用逐行平铺显示,过程透明可见。

## 前端规范

- React + Vite + TS,**不引 UI 组件库**;设计 token 两层(静态色板 → 语义别名),暗/亮主题走别名重映射。
- 不引状态库;编排放 App,纯折叠逻辑放 `fold.ts` 纯函数。
- 新增文案先写 zh 再补 en,键名对齐 dsh 风格(如 `workspace.add`)。

## 构建与交付

- 一律 `scripts/build.sh`(杀运行中进程 → web 构建 → release 安装全局命令);带 `--run` 直接起。
- **提交必须用户明确要求才做**(commit-on-request);提交按逻辑拆分,消息用中文。
- 验证分工:agent 负责编译级 + curl 级验证;**浏览器验收由用户做**,不要反复自己开浏览器浪费轮次。

## 命令

```sh
scripts/build.sh            # 杀进程 + 构建 + 全局安装
scripts/build.sh --run      # 同上 + 起服务(3600)
cargo test                  # 后端单测
cd web && pnpm build        # 前端构建(tsc + vite)
node scripts/mock-gateway.mjs  # 无 key 全链路测试网关(:8788)
```

## 目录结构

```
crates/
  core/        领域类型(StreamChunk/SessionEvent/derive_messages/ToolSchema)
  settings/    命名空间 YAML 存储(分层解析/revision OCC/脱敏)
  credentials/ 凭据(env > file > .env)
  llm/         适配器注册表 + DeepSeek/OpenAI 兼容适配器 + SSE 翻译
  tools/       bash/read_file/write_file(confined 随沙箱)
  session/     JSONL 存储(torn-tail 修复/孤儿 turn 闭合)
  agent-loop/  turn/step 驱动器(中文系统提示 + 上下文注入)
  server/      axum API + 工作区注册表 + 内嵌控制台
web/           React 控制台(zh-source i18n)
scripts/       build.sh + mock-gateway.mjs
```
