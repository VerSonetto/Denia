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
  // 崩溃孤儿轮次的合成闭合(服务端 close_orphaned_turn 生成)。
  | { kind: 'interrupted' }

/** One entry in the session's todo list (mirrors the Rust `TodoItem`). */
export interface TodoItem {
  content: string
  status: 'pending' | 'in_progress' | 'completed'
}

/** One inline image attached to a user message (mirrors Rust `ImageData`). */
export interface UserMessageImage {
  mime: string
  data: string
}

export type SessionEnvelope =
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
    }
  | {
      seq: number
      time: number
      type: 'todo-write'
      todos: TodoItem[]
    }
  | { seq: number; time: number; type: 'permission-mode'; mode: PermissionMode }
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
      outcome: 'allowed-once' | 'rejected' | 'cancelled' | 'unavailable'
    }

/** 当前会话权限模式(与 Rust `PermissionMode` 对齐)。 */
export type PermissionMode = 'read-only' | 'workspace-write' | 'danger-full-access'

export interface SessionHeader {
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
  id: string
  created_at: number
  excerpt: string | null
  cwd?: string
  sandbox?: boolean
  cwd_alive?: boolean
  /** 分支血缘:父会话 id;侧栏据此把子会话嵌套在源会话之下。 */
  parent_session?: string
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
