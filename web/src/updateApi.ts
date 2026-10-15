import { getLocale } from './i18n'

export interface UpdateRelease {
  version: string
  tag: string
  name: string
  body: string
  htmlUrl: string
  publishedAt: string | null
  prerelease: boolean
  installer: { name: string; size: number } | null
}

export interface UpdateCheck {
  currentVersion: string
  latest: UpdateRelease | null
  updateAvailable: boolean
  canInstall: boolean
  checkedAt: number
  repositoryUrl: string
}

export interface UpdateStatus {
  phase: 'idle' | 'downloading' | 'verifying' | 'launching' | 'launched' | 'failed'
  version: string | null
  downloadedBytes: number
  totalBytes: number | null
  error: string | null
}

export class UpdateApiError extends Error {
  constructor(message: string, public code: string, public status: number) {
    super(message)
    this.name = 'UpdateApiError'
  }
}

async function request<T>(path: string, init?: RequestInit, timeoutMs = 15_000): Promise<T> {
  const controller = new AbortController()
  const timer = globalThis.setTimeout(() => controller.abort(), timeoutMs)
  const en = getLocale() === 'en'
  try {
    const response = await fetch(path, {
      ...init,
      headers: { 'content-type': 'application/json', ...init?.headers },
      signal: controller.signal,
      cache: 'no-store',
    })
    let body: unknown = null
    try {
      body = await response.json()
    } catch {
      // A proxy may return an HTML error page instead of the API envelope.
    }
    if (!response.ok) {
      const error = (body as { error?: { code?: string; message?: string } } | null)?.error
      throw new UpdateApiError(
        error?.message || (en ? `Update request failed (HTTP ${response.status}).` : `更新请求失败（HTTP ${response.status}）。`),
        error?.code || `http-${response.status}`,
        response.status,
      )
    }
    if (body === null) {
      throw new UpdateApiError(en ? 'The update service returned an invalid response.' : '更新服务返回了无效响应。', 'invalid-response', response.status)
    }
    return body as T
  } catch (error) {
    if (controller.signal.aborted) {
      throw new UpdateApiError(en ? 'The update request timed out. Please try again.' : '更新请求超时，请重试。', 'timeout', 0)
    }
    if (error instanceof UpdateApiError) throw error
    throw new UpdateApiError(en ? 'Cannot reach the update service. Check your connection and try again.' : '无法连接更新服务，请检查网络后重试。', 'network', 0)
  } finally {
    globalThis.clearTimeout(timer)
  }
}

export function fetchUpdateCheck(force = false): Promise<UpdateCheck> {
  return request(`/api/update/check${force ? '?force=true' : ''}`, undefined, 25_000)
}

export function fetchUpdateStatus(): Promise<UpdateStatus> {
  return request('/api/update/status')
}

export function requestUpdateInstall(version: string): Promise<UpdateStatus> {
  return request('/api/update/install', { method: 'POST', body: JSON.stringify({ version }) })
}
