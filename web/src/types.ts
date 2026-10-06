// Shared wire types are generated from Rust/serde; UI-only types stay here.
import type { ModelSelection, LlmFailure, TokenUsage, StreamChunk, FinishReason, ContentBlock, TurnEndReason, TodoItem, AskOption, AskQuestion, AskAnswer, AskOutcome, AskResolution, PermissionMode, GoalStatus, GoalOp, SessionEnvelope, PresetFeatures, SessionHeader as WireSessionHeader, ImageData as UserMessageImage, SubagentProfile, SubagentProfileView, SubagentInlineSpec, ToolSelection, ModelChoice, PermissionCeiling, ProfileSource, ProfileWriteScope, ProfileColor, ProfileDiagnostic, SubagentCatalogEntry, SubagentDescriptor } from './generated/core'
export type { ModelSelection, LlmFailure, TokenUsage, StreamChunk, FinishReason, ContentBlock, TurnEndReason, TodoItem, AskOption, AskQuestion, AskAnswer, AskOutcome, AskResolution, PermissionMode, GoalStatus, GoalOp, SessionEnvelope, PresetFeatures, UserMessageImage, SubagentProfile, SubagentProfileView, SubagentInlineSpec, ToolSelection, ModelChoice, PermissionCeiling, ProfileSource, ProfileWriteScope, ProfileColor, ProfileDiagnostic, SubagentCatalogEntry, SubagentDescriptor }
export type SessionHeader = WireSessionHeader & { agent_preset?: string }

/** 定义目录响应：有效项 + 被覆盖项 + 诊断（来源元数据由服务端生成）。 */
export interface SubagentCatalogView {
  scope: 'user' | 'project'
  projectRoot?: string | null
  projectDir?: string | null
  userDir: string
  revision: number
  effective: SubagentProfileView[]
  shadowed: SubagentProfileView[]
  diagnostics: ProfileDiagnostic[]
  colors: ProfileColor[]
  sources: ProfileSource[]
}

/** 真实工具目录：来源、能力分类与可授予性。 */
export interface SubagentToolRow {
  name: string
  source: string
  category: string
  /** 副作用说明（写/shell/MCP 的影响必须看得见）。 */
  effect: string
  /** 只读上限下是否被拒（与执行面 `ceiling_denied_tool` 同源）。 */
  readOnlyDenied: boolean
  granted: boolean | null
  grantable: boolean
  /** 子代理硬禁用（禁止派遣 / 宿主配置 / 会话主控）：UI 显示为不可选。 */
  hardDenied: boolean
  reason: string
}

export interface SubagentToolsView {
  sessionId?: string | null
  registered: number
  tools: SubagentToolRow[]
}

/** 派遣预览：只有指定父会话时才宣称"最终可用工具"。 */
export interface SubagentPreview {
  sessionId?: string | null
  final: boolean
  note?: string
  requested?: string
  profile?: {
    qualifiedId: string
    name: string
    description: string
    inline: boolean
    revision: number
  }
  tools?: string[]
  toolCount?: number
  model?: ModelSelection
  modelOrigin?: 'call-override' | 'profile-explicit' | 'parent'
  permissionCeiling?: PermissionCeiling
  effectivePermissionMode?: PermissionMode
  delegationAllowed?: boolean
  instructionScope?: string
  fingerprint?: string
}
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

/** 旧日志三档值 → 新四档的显示映射(serde alias 的前端对应)。 */
export function normalizePermissionMode(raw: string): PermissionMode {
  if (raw === 'workspace-write') return 'auto-edit'
  if (raw === 'danger-full-access') return 'full'
  if (raw === 'read-only' || raw === 'auto-edit' || raw === 'plan' || raw === 'full') {
    return raw
  }
  return 'auto-edit'
}

export interface SessionSummary {
  /** 子代理运行快照（wire 类型由 Rust 生成；旧日志由 serde default 补齐）。 */
  subagent?: SubagentDescriptor
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
