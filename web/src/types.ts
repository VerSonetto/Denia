/** Wire types shared with the Rust backend. camelCase mirrors serde. */

export interface ProviderInfo {
  id: string
  name: string
}

export interface ReasoningEffortInfo {
  id: string
  name: string
  description?: string
}

export interface ReasoningInfo {
  efforts: ReasoningEffortInfo[]
  defaultEffort?: string
}

export interface CatalogModel {
  id: string
  name: string
  description?: string
  contextWindow?: number
  inputModalities?: string[]
  thinkingSupported?: boolean
  reasoning?: ReasoningInfo
}

export interface ModelProviderGroup {
  id: string
  name: string
  models: CatalogModel[]
}

export interface ModelCatalogFailure {
  id: string
  name: string
  message: string
}

export interface ModelSelection {
  provider: string
  model: string
  reasoningEffort?: string
}

/** 网关路由的线上协议(与 Rust `WireProtocol` 的 serde 标识对齐)。 */
export type WireProtocol = 'openai-completions' | 'openai-responses' | 'anthropic-messages'

export interface ModelCatalog {
  routableProviders: string[]
  groups: ModelProviderGroup[]
  failures: ModelCatalogFailure[]
}

export interface CredentialInfo {
  configured: boolean
  source?: string
  writable: boolean
}

export interface SecretInfo {
  path: string[]
  set: boolean
}

export interface NamespaceView {
  ns: string
  value: Record<string, unknown>
  base?: Record<string, unknown>
  user?: Record<string, unknown>
  revision: number
  applies: 'live' | 'restart'
  secrets: SecretInfo[]
}

export interface SettingsDescribe {
  writable: boolean
  documentPath: string
  namespaces: NamespaceView[]
}

export interface DiscoveredModel {
  id: string
  name?: string
}

export interface LlmFailure {
  message: string
  code: string
  status?: number
  providerRetryAfterMs?: number
  requestId?: string
}

export interface TokenUsage {
  inputTokens: number
  outputTokens: number
  cacheReadTokens?: number
  reasoningTokens?: number
}

export type StreamChunk =
  | { type: 'block-start'; index: number; block_type: 'text' | 'reasoning' | 'tool-call' }
  | { type: 'text-delta'; index: number; text: string }
  | { type: 'reasoning-delta'; index: number; text: string }
  | {
      type: 'tool-call-delta'
      index: number
      id: string
      name?: string
      arguments_delta: string
    }
  | { type: 'block-end'; index: number; block: ContentBlock }
  | { type: 'usage'; usage: TokenUsage }
  | { type: 'finish'; reason: FinishReason }

export type FinishReason =
  | { kind: 'stop' }
  | { kind: 'tool-calls' }
  | { kind: 'max-tokens' }
  | { kind: 'aborted'; failure: LlmFailure }
  | { kind: 'error'; failure: LlmFailure }

/** One configured gateway route in the `llm-openai` section. */
export interface OpenAiProfile {
  baseURL: string
  displayName?: string
  apiKeyEnv?: string
  /** 路由的线上协议;缺省即 chat/completions。 */
  protocol?: WireProtocol
  models: {
    id: string
    name?: string
    description?: string
    contextWindow?: number
    inputModalities?: string[]
    thinkingSupported?: boolean
    reasoningEfforts?: string[]
  }[]
  defaultContextWindow?: number
  defaultMaxTokens?: number
  /** 附加请求头;值支持 `${REF}` 引用凭据/环境变量(整段占位)。 */
  headers?: Record<string, string>
}

/* ---- session vocabulary (snake_case mirrors the Rust serde wire) ---- */

export type ContentBlock =
  | { type: 'text'; text: string }
  | { type: 'reasoning'; text: string }
  | { type: 'tool-call'; id: string; name: string; arguments: string }

export type TurnEndReason =
  | { kind: 'completed' }
  | { kind: 'aborted' }
  | { kind: 'max-tokens' }
  | { kind: 'error'; failure: LlmFailure }
  /** 死循环保护:模型连续输出完全相同的内容达到阈值,driver 强制中断。 */
  | { kind: 'loop-detected'; repeats?: number }
  // 崩溃孤儿轮次的合成闭合(服务端 close_orphaned_turn 生成)。
  | { kind: 'interrupted' }

/** One entry in the session's todo list (mirrors the Rust `TodoItem`). */
export interface TodoItem {
  content: string
  status: 'pending' | 'in_progress' | 'completed'
}

/** 输入框中的一张粘贴图片:预览 data URL 供缩略图,data 是发给模型的原始 base64。 */
export interface PastedImage {
  name: string
  mime: string
  data: string
  bytes: number
  preview: string
}

/** 输入框中的一份待上传附件:只持 File 句柄,真正上传发生在发送那一刻。 */
export interface PendingAttachment {
  name: string
  mime: string
  file: File
}

/**
 * 一条排队消息:AI 运行中输入的新消息,本轮结束后自动发送。
 *
 * 与输入框同一套载荷 —— 图片/附件/轨迹引用都随消息一起排队,不再只承载
 * 纯文本:排队时把输入区里的这些内容整体搬进队列(输入区随之下空),发送
 * 时原样交回 postMessage,与手动发送走同一条链路。
 */
export interface QueuedMessage {
  id: string
  text: string
  images?: PastedImage[]
  attachments?: PendingAttachment[]
  /** 轨迹引用(与 trajectory.ts 的 TrajectoryQuote 同形,避免循环依赖在此展开)。 */
  quotes?: { id: string; title: string; text: string }[]
}

/** One inline image attached to a user message (mirrors Rust `ImageData`). */
export interface UserMessageImage {
  mime: string
  data: string
}

export type SessionEnvelope =
  | { seq: number; time: number; type: 'agent-inbox' | 'agent-delivery'; id: string; text: string; source: string }
  | { seq: number; time: number; type: 'turn-start'; turn: number }
  | { seq: number; time: number; type: 'turn-end'; turn: number; reason: TurnEndReason }
  | { seq: number; time: number; type: 'step-start'; turn: number; step: number }
  | { seq: number; time: number; type: 'step-end'; turn: number; step: number }
  | {
      seq: number
      time: number
      type: 'user-message'
      text: string
      injected?: boolean
      images?: UserMessageImage[]
    }
  | { seq: number; time: number; type: 'system-prompt'; turn: number; step: number; text: string }
  | {
      seq: number
      time: number
      type: 'assistant-chunk'
      turn: number
      step: number
      chunk: StreamChunk
    }
  | {
      seq: number
      time: number
      type: 'assistant-message'
      turn: number
      step: number
      blocks: ContentBlock[]
      usage?: TokenUsage
      interrupted?: boolean
      /** 首个 token 帧落盘时刻(epoch ms);旧日志无此字段。 */
      first_token_time?: number
    }
  | {
      seq: number
      time: number
      type: 'tool-call'
      turn: number
      step: number
      call_id: string
      name: string
      arguments: string
    }
  | {
      seq: number
      time: number
      type: 'tool-result'
      turn: number
      step: number
      call_id: string
      content: string
      is_error: boolean
      error?: string
      /** 输出截断事实(统一输出预算产出);缺省 = 未截断。 */
      truncation?: { total_chars: number; shown_chars: number }
      /** 工具结果剪枝替换:本事件是对旧 tool-result 事件(seq)的 surface 替换。 */
      replaces?: number
    }
  | {
      seq: number
      time: number
      type: 'args-cleared'
      turn: number
      step: number
      call_id: string
      /** 派生历史里替换原参数的桩文本。 */
      placeholder: string
    }
  | {
      seq: number
      time: number
      type: 'compaction-summary'
      turn: number
      step: number
      summary: string
      /** 被压缩的事件 seq 区间(含);区间内的消息不再进入模型历史。 */
      replaces_from: number
      replaces_to: number
      /** 压缩后保留窗口的起始事件 seq。 */
      keep_from: number
      pre_tokens?: number
      post_tokens?: number
    }
  | {
      seq: number
      time: number
      type: 'todo-write'
      todos: TodoItem[]
    }
  | { seq: number; time: number; type: 'permission-mode'; mode: PermissionMode }
  /** 会话标题快照(服务端后台生成);latest-wins、仅日志持久。 */
  | { seq: number; time: number; type: 'session-title'; title: string }
  /** 会话运行用的 agent preset(决定工具面与 persona);仅空白会话可改选。 */
  | { seq: number; time: number; type: 'agent-preset'; preset: string }
  | {
      seq: number
      time: number
      type: 'goal'
      op: GoalOp
    }
  | {
      seq: number
      time: number
      type: 'command-run'
      /** 命令名(如 goal)。 */
      name: string
      /** 用户输入的完整原文。 */
      text: string
    }
  | { seq: number; time: number; type: 'approval-policy'; policy: 'ask' | 'never' }
  | {
      seq: number
      time: number
      type: 'approval-asked'
      request_id: string
      call_id: string
      tool: string
      args_preview: string
      reason?: string
    }
  | {
      seq: number
      time: number
      type: 'approval-decided'
      request_id: string
      outcome: 'allowed-once' | 'allowed-session' | 'rejected' | 'cancelled' | 'unavailable'
    }
  | {
      seq: number
      time: number
      type: 'ask-requested'
      request_id: string
      call_id: string
      questions: AskQuestion[]
      timeout_ms: number
    }
  | {
      seq: number
      time: number
      type: 'ask-resolved'
      request_id: string
      resolution: AskResolution
    }
  | {
      seq: number
      time: number
      type: 'request-header'
      turn: number
      step: number
      header: { config: { provider: string; model: string; reasoningEffort?: string } }
      reason: 'initial' | 'resume' | 'change' | 'series'
      starts_series?: boolean
    }
  | {
      seq: number
      time: number
      type: 'request-context'
      turn: number
      step: number
      provider: string
      model: string
      contextWindow?: number
    }
  | {
      seq: number
      time: number
      type: 'retry-attempt'
      turn: number
      step: number
      attempt: number
      code: string
      message: string
      /** 本次失败后的退避毫秒数(下一次尝试前等待)。 */
      delay_ms: number
    }

/* ---- ask 工具(模型向用户提问) ---- */

/** 提问的一个可选项(与 Rust `AskOption` 对齐)。 */
export interface AskOption {
  label: string
  description?: string
  /** 模型推荐项;UI 高亮显示,不靠标签文本约定。 */
  recommended?: boolean
}

/** 一个待回答的问题。 */
export interface AskQuestion {
  id: string
  question: string
  header?: string
  detail?: string
  options?: AskOption[]
  multiSelect?: boolean
  /** 是否允许自由填写;缺省 true。 */
  allowCustom?: boolean
}

/** 一个问题的回答。 */
export interface AskAnswer {
  id: string
  selected: string[]
  custom?: string
  /** 用户显式跳过本题。 */
  skipped?: boolean
}

/** 提问的结局。 */
export type AskOutcome = 'answered' | 'timed-out' | 'cancelled' | 'unavailable'

/** 一次提问的闭合结果。 */
export interface AskResolution {
  outcome: AskOutcome
  answers?: AskAnswer[]
  reason?: string
}

/** 当前会话权限模式(与 Rust `PermissionMode` 对齐,四档)。 */
export type PermissionMode = 'read-only' | 'auto-edit' | 'plan' | 'full'

/** 会话目标状态(与 Rust `GoalStatus` 对齐)。 */
export type GoalStatus = 'active' | 'paused' | 'blocked' | 'budget-limited' | 'complete'

/** 会话目标操作(与 Rust `GoalOp` 对齐,内部 tag `kind`)。 */
export type GoalOp =
  | { kind: 'set'; objective: string; token_budget?: number }
  | { kind: 'edit'; objective?: string; token_budget?: number }
  | { kind: 'pause' }
  | { kind: 'resume' }
  | { kind: 'round' }
  | { kind: 'complete' }
  | { kind: 'block'; reason?: string }
  | { kind: 'budget-limit' }
  | { kind: 'clear' }

/** 旧日志三档值 → 新四档的显示映射(serde alias 的前端对应)。 */
export function normalizePermissionMode(raw: string): PermissionMode {
  if (raw === 'workspace-write') return 'auto-edit'
  if (raw === 'danger-full-access') return 'full'
  if (raw === 'read-only' || raw === 'auto-edit' || raw === 'plan' || raw === 'full') {
    return raw
  }
  return 'auto-edit'
}

export interface SessionHeader {
  subagent?: { label: string; depth: number; mode: string; selection: ModelSelection }
  type: 'session'
  version: number
  id: string
  created_at: number
  cwd: string
  sandbox: boolean
  /** 分支血缘:父会话 id(旧日志/普通会话无此字段)。 */
  parent_session?: string
}

export interface SessionSummary {
  subagent?: { label: string; depth: number; mode: string; selection: ModelSelection }
  id: string
  created_at: number
  excerpt: string | null
  /** AI 生成的会话标题(第一轮用户消息后后台产出);展示优先于 excerpt。 */
  title: string | null
  cwd?: string
  sandbox?: boolean
  cwd_alive?: boolean
  /** 分支血缘:父会话 id;侧栏据此把子会话嵌套在源会话之下。 */
  parent_session?: string
  /** 会话运行的 agent preset(id);创建时写入,缺省表示后端未提供。 */
  agent_preset?: string
}

/**
 * agent preset 功能开关快照(与服务端 `PresetFeatures` 同名同义)。
 * 关闭的功能连带摘除对应工具、纪律段、注入通道与运行时行为。
 */
export interface PresetFeatures {
  agentsMd: boolean
  memory: boolean
  compaction: boolean
  goal: boolean
  skills: boolean
  subagents: boolean
  jobs: boolean
  browser: boolean
  ask: boolean
  planMode: boolean
}

/** agent preset 名册里的一行(随附或用户自定义)。 */
export interface AgentPresetRow {
  id: string
  trust: 'shipped' | 'user'
  name: string
  description: string
  /** 工具白名单;缺省表示出厂全量工具集。 */
  tools?: string[]
  hasPersona: boolean
  /** 用户根目录下的 preset 才可删除、可被对照。 */
  writable: boolean
  /** 功能开关快照(关闭项在卡片上列出)。 */
  features: PresetFeatures
  path?: string
  /** 损坏原因;有值时该行无法用于会话。 */
  broken?: string
}

export interface AgentPresetsView {
  /** 新建会话未显式指定时使用的默认 preset id。 */
  default: string
  /** 是否显示模式选择器;关闭时空白会话固定用部署默认组装。 */
  modeSelection: boolean
  /** 用户 preset 根目录(展示与"用编辑器打开"用)。 */
  root: string
  presets: AgentPresetRow[]
}

export interface WorkspaceEntry {
  path: string
  name?: string
}

/** 工作区注册表记录(抄 dsh workspace 实体)。 */
export interface WorkspaceRecord {
  id: string
  path: string
  title: string
  createdAt: number
  sessionIds: string[]
}

/* ---- 内嵌浏览器(与 browser 工具同款命令面) ---- */

export interface BrowserTabInfo {
  tabId: string
  url: string
  title: string
  active: boolean
}

export interface BrowserState {
  running: boolean
  activeTabId?: string | null
  tabs: BrowserTabInfo[]
  dialogs?: unknown[]
}

export interface BrowserOutcome {
  ok: boolean
  error?: { code: string; message: string }
  value?: unknown
  state?: Record<string, unknown>
  snapshot?: {
    url?: string
    title?: string
    elements?: {
      ref: string
      tag: string
      name?: string
      rect?: { x: number; y: number; width: number; height: number }
      inViewport?: boolean
      editable?: boolean
      href?: string
    }[]
    truncated?: boolean
  }
  image?: { base64: string; mimeType: string }
  dialog?: { type?: string; message?: string } | null
  tabs?: BrowserTabInfo[]
  elapsedMs: number
}

/** /api/browser/stream 的一帧事件。 */
export type BrowserEventFrame =
  | { type: 'tabs-changed' }
  | { type: 'frame'; tabId: string; data: string; viewport?: { width: number; height: number } }
  | { type: 'dialog-opened'; tabId: string; kind: string; message: string }
  | { type: 'dialog-closed'; tabId: string }
  | { type: 'navigated'; tabId: string; url: string; title: string }
  | { type: 'visual-mode-requested' }
  | { type: 'exited' }

/* ---- 远程连接 ---- */

/** 远程连接通道。 */
export type RemoteChannel = 'lan' | 'tunnel'

/** 一条可分享的访问链接及其二维码。 */
export interface RemoteLink {
  /** 不带票据的基址(直接访问会被远程门拦下)。 */
  url: string
  /** 带票据的完整链接 —— 扫码/分享用这一个。 */
  ticketUrl: string
  ticketExpiresAt: number
  ticketSingleUse: boolean
  requirePin: boolean
  /** 仅本机请求可见:远程客户端拿不到 PIN。 */
  pin: string | null
  qrSvg: string
  qrText: string
  qrWidth: number
  /** 行优先的模块矩阵(`true` = 深色),前端用它画 PNG。 */
  qrModules: boolean[]
}

export interface RemoteCandidate {
  interface: string
  address: string
  recommended: boolean
}

export interface RemoteLanStatus {
  bind: string
  port: number
  candidates: RemoteCandidate[]
  address: string
  link: RemoteLink
}

export interface RemoteTunnelStatus {
  url: string
  host: string
  pid: number
  startedAt: number
  link: RemoteLink
}

export interface RemoteSessionRecord {
  id: string
  via: RemoteChannel
  peer: string
  createdAt: number
  lastSeenAt: number
  expiresAt: number
  idleDeadline: number
}

export interface RemoteSessionView {
  total: number
  lan: number
  tunnel: number
  items: RemoteSessionRecord[]
}

export interface RemoteStatus {
  enabled: boolean
  /** 请求是否来自本机控制台(决定关闭按钮等主机侧操作是否可见)。 */
  local: boolean
  lan: RemoteLanStatus | null
  tunnel: RemoteTunnelStatus | null
  sessions: RemoteSessionView
  auditPath: string
  /** 当前处于退避中的来源 IP 数。 */
  blockedPeers: number
}

/** 票据兑换的结果:PIN 挑战或已建立会话。 */
export type RemoteExchangeResult =
  | { status: 'pin-required'; challenge: string; attempts: number }
  | { status: 'ok'; via: RemoteChannel; maxAge: number }
