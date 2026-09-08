import type {
  CredentialInfo,
  DiscoveredModel,
  ModelCatalog,
  PermissionMode,
  ProviderInfo,
  SessionEnvelope,
  SessionHeader,
  SessionSummary,
  SettingsDescribe,
  StreamChunk,
  TokenUsage,
  WorkspaceRecord,
  WireProtocol,
} from './types'

export class ApiError extends Error {
  constructor(
    public code: string,
    message: string,
    public status: number,
  ) {
    super(message)
  }
}

/** 非 follow 型请求的默认超时:15s(挂死的请求不能卡住 UI)。 */
const DEFAULT_TIMEOUT_MS = 15_000

async function http<T>(path: string, init?: RequestInit, timeoutMs = DEFAULT_TIMEOUT_MS): Promise<T> {
  const timeout = new AbortController()
  const timer = window.setTimeout(() => timeout.abort(), timeoutMs)
  try {
    const response = await fetch(path, {
      headers: { 'content-type': 'application/json' },
      ...init,
      signal: init?.signal ?? timeout.signal,
    })
    if (!response.ok) {
      let code = `http-${response.status}`
      let message = response.statusText
      try {
        const body = await response.json()
        if (body?.error) {
          code = body.error.code ?? code
          message = body.error.message ?? message
        }
      } catch {
        /* body was not JSON */
      }
      throw new ApiError(code, message, response.status)
    }
    return response.json() as Promise<T>
  } finally {
    window.clearTimeout(timer)
  }
}

export function getSettings(): Promise<SettingsDescribe> {
  return http('/api/settings')
}

export function updateNamespace(
  ns: string,
  value: unknown,
  expectedRevision?: number,
): Promise<unknown> {
  return http(`/api/settings/${encodeURIComponent(ns)}`, {
    method: 'PATCH',
    body: JSON.stringify({ value, expectedRevision }),
  })
}

export function replaceNamespace(
  ns: string,
  value: unknown,
  expectedRevision?: number,
): Promise<unknown> {
  return http(`/api/settings/${encodeURIComponent(ns)}`, {
    method: 'PUT',
    body: JSON.stringify({ value, expectedRevision }),
  })
}

export async function describeCredentials(
  refs: string[],
): Promise<Record<string, CredentialInfo>> {
  if (refs.length === 0) return {}
  const data = await http<{ credentials: Record<string, CredentialInfo> }>(
    `/api/credentials?refs=${encodeURIComponent(refs.join(','))}`,
  )
  return data.credentials
}

export function setCredential(reference: string, value: string): Promise<unknown> {
  return http(`/api/credentials/${encodeURIComponent(reference)}`, {
    method: 'PUT',
    body: JSON.stringify({ value }),
  })
}

export function unsetCredential(reference: string): Promise<unknown> {
  return http(`/api/credentials/${encodeURIComponent(reference)}`, { method: 'DELETE' })
}

export async function getProviders(): Promise<{
  providers: ProviderInfo[]
  configurable: unknown[]
}> {
  return http('/api/llm/providers')
}

export function getCatalog(): Promise<ModelCatalog> {
  return http('/api/llm/catalog')
}

export function discoverModels(
  baseURL: string,
  apiKey?: string,
  apiKeyEnv?: string,
  protocol?: WireProtocol,
): Promise<DiscoveredModel[]> {
  return http<{ models: DiscoveredModel[] }>('/api/llm/discover', {
    method: 'POST',
    body: JSON.stringify({ baseURL, apiKey, apiKeyEnv, protocol }),
  }).then((data) => data.models)
}

/** Parses one SSE response body, invoking `onFrame` per data payload. */
async function parseSseBody(
  response: Response,
  onFrame: (payload: unknown, isError: boolean) => void,
  signal: AbortSignal,
): Promise<void> {
  if (!response.body) return
  const reader = response.body.getReader()
  const decoder = new TextDecoder()
  let buffer = ''
  let isError = false
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    buffer += decoder.decode(value, { stream: true })
    const lines = buffer.split('\n')
    buffer = lines.pop() ?? ''
    for (const line of lines) {
      const trimmed = line.trimEnd()
      if (trimmed.startsWith('event:')) {
        isError = trimmed.slice(6).trim() === 'error'
      } else if (trimmed.startsWith('data:')) {
        const payload = trimmed.slice(5).trim()
        if (!payload) continue
        try {
          onFrame(JSON.parse(payload), isError)
        } catch {
          /* skip malformed frame */
        }
        isError = false
      }
    }
    if (signal.aborted) break
  }
}

/* ---- llm chat (SSE, used by prompt optimization) ---- */

export interface ChatMessageInput {
  role: 'system' | 'user' | 'assistant'
  content: string
}

export interface ChatCompletionOptions {
  provider: string
  model: string
  reasoningEffort?: string
  system?: string
  messages: ChatMessageInput[]
}

/** 优化类长请求允许更长的等待时间(流式,非 15s 默认)。 */
const CHAT_TIMEOUT_MS = 120_000

interface ChatStreamPayload {
  type?: string
  text?: string
  block?: { type?: string; text?: string }
  reason?: { kind?: string; failure?: { code?: string; message?: string } }
  code?: string
  message?: string
}

/** Calls the SSE chat smoke-test endpoint and resolves with complete text. */
export async function chatCompletion(
  options: ChatCompletionOptions,
  signal?: AbortSignal,
): Promise<string> {
  const controller = new AbortController()
  let timedOut = false
  const timer = window.setTimeout(() => {
    timedOut = true
    controller.abort()
  }, CHAT_TIMEOUT_MS)
  const onOuterAbort = () => controller.abort()
  if (signal) {
    if (signal.aborted) controller.abort()
    else signal.addEventListener('abort', onOuterAbort, { once: true })
  }
  try {
    let response: Response
    try {
      response = await fetch('/api/llm/chat', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          provider: options.provider,
          model: options.model,
          reasoningEffort: options.reasoningEffort,
          system: options.system,
          messages: options.messages.map(({ role, content }) => ({ role, content })),
        }),
        signal: controller.signal,
      })
    } catch (error) {
      if (controller.signal.aborted) {
        throw new ApiError(
          timedOut ? 'chat/timeout' : 'chat/aborted',
          timedOut ? '模型请求超时' : '模型请求已取消',
          timedOut ? 504 : 499,
        )
      }
      throw error
    }
    if (!response.ok) {
      let code = `http-${response.status}`
      let message = response.statusText
      try {
        const body = await response.json()
        if (body?.error) {
          code = body.error.code ?? code
          message = body.error.message ?? message
        }
      } catch {
        /* body was not JSON */
      }
      throw new ApiError(code, message, response.status)
    }
    if (!response.body) {
      throw new ApiError('chat/empty-body', '模型没有返回内容', 500)
    }
    const reader = response.body.getReader()
    const decoder = new TextDecoder()
    let buffer = ''
    let isError = false
    let text = ''
    let failure: { code?: string; message?: string } | null = null
    for (;;) {
      const { done, value } = await reader.read()
      if (done) break
      buffer += decoder.decode(value, { stream: true })
      const lines = buffer.split('\n')
      buffer = lines.pop() ?? ''
      for (const line of lines) {
        const trimmed = line.trimEnd()
        if (trimmed.startsWith('event:')) {
          isError = trimmed.slice(6).trim() === 'error'
        } else if (trimmed.startsWith('data:')) {
          const payload = trimmed.slice(5).trim()
          if (!payload) continue
          let parsed: ChatStreamPayload
          try {
            parsed = JSON.parse(payload) as ChatStreamPayload
          } catch {
            continue
          }
          if (isError) {
            failure = parsed as { code?: string; message?: string }
            isError = false
            continue
          }
          if (parsed.type === 'text-delta' && typeof parsed.text === 'string') {
            text += parsed.text
          } else if (
            parsed.type === 'block-end' &&
            parsed.block?.type === 'text' &&
            typeof parsed.block.text === 'string'
          ) {
            text = parsed.block.text
          } else if (parsed.type === 'finish') {
            const kind = parsed.reason?.kind
            if (kind && kind !== 'stop') {
              failure = parsed.reason?.failure ?? { message: '模型输出失败' }
            }
          }
        }
      }
      if (controller.signal.aborted) break
    }
    if (controller.signal.aborted) {
      throw new ApiError(
        timedOut ? 'chat/timeout' : 'chat/aborted',
        timedOut ? '模型请求超时' : '模型请求已取消',
        timedOut ? 504 : 499,
      )
    }
    if (failure) {
      throw new ApiError(
        failure.code ?? 'chat/failed',
        failure.message ?? '模型调用失败',
        500,
      )
    }
    return text.trim()
  } finally {
    window.clearTimeout(timer)
    signal?.removeEventListener('abort', onOuterAbort)
  }
}

/* ---- llm chat probe (settings 连通性测试的流式回调版) ---- */

export interface ProbeStreamHandlers {
  /** 响应头已返回、开始读流(连接 OK,进入等待模型阶段)。 */
  onConnected: () => void
  /** 收到第一个输出增量(即 TTFT 锚点)。 */
  onFirstToken: () => void
  /** 每个输出增量(驱动流式进度)。 */
  onDelta: (text: string) => void
  /** 服务端上报的 token 用量。 */
  onUsage: (usage: TokenUsage) => void
  /** 正常结束。 */
  onSuccess: (reason: string) => void
  /** 流内/握手失败(错误码、状态与网关返回体由后端脱敏后随 failure 下发)。 */
  onFailure: (failure: { code: string; message: string; status?: number; requestId?: string; retryAfterMs?: number }) => void
}

/**
 * 调一次 /api/llm/chat 并把 StreamChunk 翻译成探测回调。
 * 与 chatCompletion 的区别:不聚合文本,而是边流边回报,供进度/TTFT/用量展示。
 */
export async function probeChatStream(
  options: { provider: string; model: string; reasoningEffort?: string; prompt: string; maxTokens?: number },
  handlers: ProbeStreamHandlers,
  signal?: AbortSignal,
  timeoutMs = 120_000,
): Promise<void> {
  const controller = new AbortController()
  let timedOut = false
  const timer = window.setTimeout(() => {
    timedOut = true
    controller.abort()
  }, timeoutMs)
  const onOuterAbort = () => controller.abort()
  if (signal) {
    if (signal.aborted) controller.abort()
    else signal.addEventListener('abort', onOuterAbort, { once: true })
  }
  const toFailure = (raw: {
    code?: string
    message?: string
    status?: number
    requestId?: string
    providerRetryAfterMs?: number
  }) => ({
    code: raw.code ?? 'probe/failed',
    message: raw.message ?? '模型调用失败',
    status: raw.status,
    requestId: raw.requestId,
    retryAfterMs: raw.providerRetryAfterMs,
  })
  try {
    let response: Response
    try {
      response = await fetch('/api/llm/chat', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          provider: options.provider,
          model: options.model,
          reasoningEffort: options.reasoningEffort,
          prompt: options.prompt,
          ...(options.maxTokens !== undefined ? { max_tokens: options.maxTokens } : {}),
        }),
        signal: controller.signal,
      })
    } catch (error) {
      if (controller.signal.aborted) {
        handlers.onFailure(
          timedOut
            ? { code: 'probe/timeout', message: '连接超时,网关长时间无响应', status: 504 }
            : { code: 'probe/aborted', message: '测试已取消' },
        )
        return
      }
      handlers.onFailure({ code: 'probe/network', message: error instanceof Error ? error.message : String(error) })
      return
    }
    if (!response.ok) {
      let code = `http-${response.status}`
      let message = response.statusText
      try {
        const body = await response.json()
        if (body?.error) {
          code = body.error.code ?? code
          message = body.error.message ?? message
        }
      } catch {
        /* body was not JSON */
      }
      handlers.onFailure({ code, message, status: response.status })
      return
    }
    if (!response.body) {
      handlers.onFailure({ code: 'probe/empty-body', message: '网关没有返回内容', status: response.status })
      return
    }
    handlers.onConnected()
    const reader = response.body.getReader()
    const decoder = new TextDecoder()
    let buffer = ''
    let isErrorFrame = false
    let settled = false
    for (;;) {
      const { done, value } = await reader.read()
      if (done) break
      buffer += decoder.decode(value, { stream: true })
      const lines = buffer.split('\n')
      buffer = lines.pop() ?? ''
      for (const line of lines) {
        const trimmed = line.trimEnd()
        if (trimmed.startsWith('event:')) {
          isErrorFrame = trimmed.slice(6).trim() === 'error'
        } else if (trimmed.startsWith('data:')) {
          const payload = trimmed.slice(5).trim()
          if (!payload) continue
          let parsed: StreamChunk | { code?: string; message?: string; status?: number; requestId?: string; providerRetryAfterMs?: number }
          try {
            parsed = JSON.parse(payload)
          } catch {
            continue
          }
          if (isErrorFrame) {
            settled = true
            handlers.onFailure(toFailure(parsed as { code?: string; message?: string }))
            isErrorFrame = false
            continue
          }
          const chunk = parsed as StreamChunk
          if (chunk.type === 'text-delta') {
            handlers.onFirstToken()
            handlers.onDelta(chunk.text)
          } else if (chunk.type === 'reasoning-delta') {
            handlers.onFirstToken()
          } else if (chunk.type === 'usage') {
            handlers.onUsage(chunk.usage)
          } else if (chunk.type === 'finish') {
            settled = true
            if (chunk.reason.kind === 'stop' || chunk.reason.kind === 'tool-calls') {
              handlers.onSuccess(chunk.reason.kind)
            } else {
              handlers.onFailure(
                toFailure(
                  chunk.reason.kind === 'error' || chunk.reason.kind === 'aborted'
                    ? (chunk.reason.failure as { code?: string; message?: string })
                    : { message: '输出提前截断' },
                ),
              )
            }
          }
        }
      }
      if (controller.signal.aborted) break
    }
    if (!settled) {
      if (controller.signal.aborted) {
        handlers.onFailure(
          timedOut
            ? { code: 'probe/timeout', message: '连接超时,网关长时间无响应', status: 504 }
            : { code: 'probe/aborted', message: '测试已取消' },
        )
      } else {
        handlers.onFailure({ code: 'probe/empty-body', message: '流在完成前提前关闭', status: response.status })
      }
    }
  } finally {
    window.clearTimeout(timer)
    signal?.removeEventListener('abort', onOuterAbort)
  }
}

/* ---- sessions ---- */

export function listSessions(): Promise<{ sessions: SessionSummary[] }> {
  return http('/api/sessions')
}

export function createSession(options: {
  workspaceId?: string
  cwd?: string
  sandbox?: boolean
}): Promise<{ session: SessionSummary & { cwd: string } }> {
  return http('/api/sessions', {
    method: 'POST',
    body: JSON.stringify(options),
  })
}

export function listWorkspaces(): Promise<{ workspaces: WorkspaceRecord[] }> {
  return http('/api/workspaces')
}

export function createWorkspace(
  path: string,
  title?: string,
): Promise<{ workspace: WorkspaceRecord }> {
  return http('/api/workspaces', {
    method: 'POST',
    body: JSON.stringify({ path, title }),
  })
}

export function deleteWorkspace(id: string): Promise<unknown> {
  return http(`/api/workspaces/${encodeURIComponent(id)}`, { method: 'DELETE' })
}

export function pickerCapability(): Promise<{ kind: 'native' | 'browse' }> {
  return http('/api/fs/capability')
}

export function mkdir(path: string, name: string): Promise<{ path: string }> {
  return http('/api/fs/mkdir', {
    method: 'POST',
    body: JSON.stringify({ path, name }),
  })
}

export function browseDirs(path?: string): Promise<{
  path: string
  parent: string | null
  dirs: string[]
}> {
  const query = path ? `?path=${encodeURIComponent(path)}` : ''
  return http(`/api/fs/dirs${query}`)
}

/** Opens the OS-native directory chooser on the host; null when cancelled. */
export function pickDirectory(): Promise<{ path: string | null }> {
  return http('/api/fs/pick', { method: 'POST' })
}

/** `@` 提及候选:相对会话 cwd 的路径条目(目录与文件)。 */
export interface MentionCandidate {
  path: string
  kind: 'file' | 'directory'
}

/** 搜索会话 cwd 树下的 `@` 提及候选;query 为空 = 列根目录。 */
export function searchMentions(
  cwd: string,
  query: string,
  signal?: AbortSignal,
): Promise<{ items: MentionCandidate[] }> {
  const params = new URLSearchParams({ path: cwd })
  if (query) params.set('query', query)
  return http(`/api/fs/mentions?${params.toString()}`, { signal })
}

export function getSession(id: string, signal?: AbortSignal): Promise<{
  header: SessionHeader
  events: SessionEnvelope[]
}> {
  return http(`/api/sessions/${encodeURIComponent(id)}`, { signal })
}

export interface SessionAnchor {
  seq: number
  text: string
}

export interface SessionPageResponse {
  header: SessionHeader
  events: SessionEnvelope[]
  total: number
  hasMoreBefore: boolean
  /** 全会话非注入 user-message 锚点(轮次轴刻度,不受分页窗口限制)。 */
  anchors: SessionAnchor[]
}

/** 分页读取会话事件窗口:不经过全量快照,长会话首屏只拉尾部有限窗口。 */
export function getSessionPage(
  id: string,
  options: { before?: number; limit?: number } = {},
  signal?: AbortSignal,
): Promise<SessionPageResponse> {
  const query = new URLSearchParams()
  if (options.before !== undefined) query.set('before', String(options.before))
  if (options.limit !== undefined) query.set('limit', String(options.limit))
  const qs = query.toString()
  return http(
    `/api/sessions/${encodeURIComponent(id)}/events${qs ? `?${qs}` : ''}`,
    { signal },
  )
}

export function deleteSession(id: string): Promise<unknown> {
  return http(`/api/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' })
}

/** 从某个已完成轮次边界分支出全新会话(dsh session.fork)。 */
export function forkSession(
  id: string,
  atSeq?: number,
): Promise<{ session: SessionSummary }> {
  return http(`/api/sessions/${encodeURIComponent(id)}/fork`, {
    method: 'POST',
    body: JSON.stringify(atSeq === undefined ? {} : { atSeq }),
  })
}

export function postPrompt(
  id: string,
  body: {
    prompt: string
    provider?: string
    model?: string
    reasoningEffort?: string
    /** 粘贴的内联图片(base64);模型必须支持识图。 */
    images?: { name?: string; mime: string; data: string }[]
    /** 已上传文件的绝对路径(作为注入上下文随消息发送)。 */
    files?: string[]
    /** 轨迹引用(标题, 正文);以 injected 上下文消息随本轮注入。 */
    quoted?: { title: string; text: string }[]
  },
): Promise<unknown> {
  return http(`/api/sessions/${encodeURIComponent(id)}/prompt`, {
    method: 'POST',
    body: JSON.stringify(body),
  })
}

/** 切换当前会话权限预设(抄 dsh /permission 写路径)。 */
export function setSessionPermission(
  id: string,
  mode: PermissionMode,
): Promise<{ mode: PermissionMode }> {
  return http(`/api/sessions/${encodeURIComponent(id)}/permission`, {
    method: 'PUT',
    body: JSON.stringify({ mode }),
  })
}

/** 应答一次挂起的审批(抄 dsh ui approval panel)。 */
export function answerApproval(
  id: string,
  requestId: string,
  decision: 'allow-once' | 'reject',
): Promise<{ ok: boolean }> {
  return http(
    `/api/sessions/${encodeURIComponent(id)}/approvals/${encodeURIComponent(requestId)}`,
    {
      method: 'POST',
      body: JSON.stringify({ decision }),
    },
  )
}

/** 上传一个附件(不限格式),返回持久化后的绝对路径与字节数。 */
export function uploadAttachment(params: {
  sessionId: string
  name: string
  mime: string
  data: string
}): Promise<{ path: string; name: string; bytes: number }> {
  return http('/api/attachments', {
    method: 'POST',
    body: JSON.stringify({
      sessionId: params.sessionId,
      name: params.name,
      mime: params.mime,
      data: params.data,
    }),
  })
}

/** 当前上下文 token 拆分(token-meter 服务端 fold)。 */
export interface ContextBreakdown {
  systemTokens: number
  toolsTokens: number
  messageTokens: number
}

/** 上下文压力投影(对齐 dsh `contextPressure`:字段各自 last-wins,可缺省)。 */
export interface ContextPressure {
  /** 路由容量(最新 request/context 记录的上下文窗口)。 */
  contextWindow?: number
  /** 最近一次 provider usage 的 prompt 侧总量(锚点);无样本则缺省。 */
  pressureTokens?: number
  /** 锚点 + 锚点后表面增量(下一次请求的期望 prompt 规模)。 */
  projectedTokens?: number
}

/** 精确 usage 累计(对齐 dsh `TurnTokenUsage`)。 */
export interface TurnTokenUsage {
  uncachedInputTokens: number
  outputTokens: number
  cacheReadTokens: number
  cacheWriteTokens: number
  reasoningTokens: number
}

export interface ContextBreakdownResponse {
  breakdown: ContextBreakdown
  pressure: ContextPressure
  usage: TurnTokenUsage
}

export function contextBreakdown(id: string): Promise<ContextBreakdownResponse> {
  return http(`/api/sessions/${encodeURIComponent(id)}/context-breakdown`)
}

/** 手动压缩结果(上下文面板按钮;摘要调用可能持续数十秒)。 */
export interface ManualCompactOutcome {
  replacesFrom: number
  replacesTo: number
  keepFrom: number
  preTokens: number
  postTokens: number
  savedTokens: number
}

export interface ManualCompactResponse {
  ok: boolean
  outcome?: ManualCompactOutcome
  /** `nothing-to-compact`:无可压缩区间(历史太短)。 */
  reason?: string
}

/** 手动压缩:恢复最近一次请求形态执行总结压缩;超时放宽到 5 分钟。 */
export function compactSession(id: string): Promise<ManualCompactResponse> {
  return http(`/api/sessions/${encodeURIComponent(id)}/compact`, { method: 'POST' }, 300_000)
}

export interface SystemPromptView {
  text: string
  source: 'file' | 'default'
  path: string
}

export function getSystemPrompt(): Promise<SystemPromptView> {
  return http('/api/system-prompt')
}

export function saveSystemPrompt(text: string): Promise<SystemPromptView> {
  return http('/api/system-prompt', {
    method: 'PUT',
    body: JSON.stringify({ text }),
  })
}

export function resetSystemPrompt(): Promise<SystemPromptView> {
  return http('/api/system-prompt', { method: 'DELETE' })
}

/** 一个可回退的用户消息 checkpoint。 */
export interface Checkpoint {
  seq: number
  time: number
  text: string
}

export function getCheckpoints(id: string): Promise<{ checkpoints: Checkpoint[] }> {
  return http(`/api/sessions/${encodeURIComponent(id)}/checkpoints`)
}

/** 回退确认框里的单条文件变化。 */
export interface FileDiffEntry {
  path: string
  action: 'restore' | 'delete'
}

export function getCheckpointDiff(
  id: string,
  seq: number,
): Promise<{ changes: FileDiffEntry[] }> {
  return http(`/api/sessions/${encodeURIComponent(id)}/checkpoints/${seq}/diff`)
}

export interface RewindResult {
  ok: boolean
  toSeq: number
  toMessage: string | null
  removedEvents: number
  changedFiles: string[]
}

export function rewindSession(
  id: string,
  toSeq: number,
): Promise<RewindResult> {
  return http(`/api/sessions/${encodeURIComponent(id)}/rewind`, {
    method: 'POST',
    body: JSON.stringify({ toSeq }),
  })
}

export function cancelSession(id: string): Promise<unknown> {
  return http(`/api/sessions/${encodeURIComponent(id)}/cancel`, { method: 'POST' })
}

/**
 * Follows one session's live event stream after `after`; returns the abort
 * handle. The stream ends on server-side lag or close — callers re-snapshot.
 */
export function followSession(
  id: string,
  after: number,
  onEnvelope: (envelope: SessionEnvelope) => void,
  onEnd: () => void,
): AbortController {
  const controller = new AbortController()
  const run = async () => {
    try {
      const response = await fetch(
        `/api/sessions/${encodeURIComponent(id)}/follow?after=${after}`,
        { signal: controller.signal },
      )
      if (!response.ok) {
        onEnd()
        return
      }
      await parseSseBody(
        response,
        (payload) => onEnvelope(payload as SessionEnvelope),
        controller.signal,
      )
    } catch {
      /* aborted or transport error: caller re-snapshots */
    }
    if (!controller.signal.aborted) onEnd()
  }
  void run()
  return controller
}

/** Subscribes to server push events; returns the close handle. */
export function subscribeEvents(onEvent: (type: string) => void): () => void {
  const source = new EventSource('/api/events')
  source.onmessage = (event) => {
    try {
      const parsed = JSON.parse(event.data) as { type?: string }
      if (parsed.type) onEvent(parsed.type)
    } catch {
      /* ignore malformed frame */
    }
  }
  return () => source.close()
}
