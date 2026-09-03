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
  ModelSelection,
  WorkspaceRecord,
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

async function http<T>(path: string, init?: RequestInit): Promise<T> {
  const timeout = new AbortController()
  const timer = window.setTimeout(() => timeout.abort(), DEFAULT_TIMEOUT_MS)
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
): Promise<DiscoveredModel[]> {
  return http<{ models: DiscoveredModel[] }>('/api/llm/discover', {
    method: 'POST',
    body: JSON.stringify({ baseURL, apiKey, apiKeyEnv }),
  }).then((data) => data.models)
}

export function getDefaultModel(): Promise<ModelSelection> {
  return http('/api/llm/default-model')
}

export function saveDefaultModel(selection: ModelSelection): Promise<ModelSelection> {
  return http('/api/llm/default-model', {
    method: 'PUT',
    body: JSON.stringify(selection),
  })
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

export function getSession(id: string, signal?: AbortSignal): Promise<{
  header: SessionHeader
  events: SessionEnvelope[]
}> {
  return http(`/api/sessions/${encodeURIComponent(id)}`, { signal })
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

/** 上下文压力(provider 锚点 + 启发式)。 */
export interface ContextPressure {
  pressureTokens: number
  /** 是否使用了 provider 精确锚点。 */
  anchored: boolean
  anchorTokens: number
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
