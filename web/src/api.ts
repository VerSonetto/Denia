import type {
  CredentialInfo,
  DiscoveredModel,
  ModelCatalog,
  ProviderInfo,
  SessionEnvelope,
  SessionHeader,
  SessionSummary,
  SettingsDescribe,
  ModelSelection,
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

async function http<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    headers: { 'content-type': 'application/json' },
    ...init,
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

/* ---- sessions ---- */

export function listSessions(): Promise<{ sessions: SessionSummary[] }> {
  return http('/api/sessions')
}

export function createSession(options: {
  sandbox: boolean
  cwd?: string
}): Promise<{ session: SessionSummary & { cwd: string } }> {
  return http('/api/sessions', {
    method: 'POST',
    body: JSON.stringify(options),
  })
}

export function getSession(id: string): Promise<{
  header: SessionHeader
  events: SessionEnvelope[]
}> {
  return http(`/api/sessions/${encodeURIComponent(id)}`)
}

export function deleteSession(id: string): Promise<unknown> {
  return http(`/api/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' })
}

export function postPrompt(
  id: string,
  body: {
    prompt: string
    provider?: string
    model?: string
    reasoningEffort?: string
  },
): Promise<unknown> {
  return http(`/api/sessions/${encodeURIComponent(id)}/prompt`, {
    method: 'POST',
    body: JSON.stringify(body),
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
