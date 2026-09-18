import type { BrowserState, BrowserEventFrame } from './types'

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    headers: { 'content-type': 'application/json' },
    ...init,
  })
  if (!response.ok) {
    let message = response.statusText
    try {
      const body = (await response.json()) as { error?: string }
      if (body?.error) message = body.error
    } catch {
      /* 非 JSON 错误体 */
    }
    throw new Error(message)
  }
  return (await response.json()) as T
}

/** 浏览器状态(tabs/活跃/dialog)。 */
export async function fetchBrowserState(): Promise<BrowserState> {
  return request<BrowserState>('/api/browser/state')
}

/** 执行一条浏览器命令;返回 CommandOutcome(JSON)。 */
export async function browserCommand(command: Record<string, unknown>): Promise<unknown> {
  return request<unknown>('/api/browser/command', {
    method: 'POST',
    body: JSON.stringify({ command }),
  })
}

/** 订阅浏览器事件流(tabs/frame/dialog/navigated/exited)。
 *
 * 断线时回调 onEnd 一次;EventSource 虽会自动重连,但断线窗口里可能错过
 * frame/tabs-changed 事件 —— 调用方在 onEnd 里自行补一次状态刷新即可对齐。
 */
export function subscribeBrowserEvents(
  onEvent: (event: BrowserEventFrame) => void,
  onEnd?: () => void,
): () => void {
  const source = new EventSource('/api/browser/stream')
  // 重连成功:把断线标记复位,保证 onEnd 语义为"每次掉线恰好一次"。
  let lost = false
  source.onopen = () => {
    lost = false
  }
  source.onerror = () => {
    if (!lost) {
      lost = true
      onEnd?.()
    }
    /* 重连由 EventSource 自动完成 */
  }
  source.onmessage = (event) => {
    try {
      onEvent(JSON.parse(event.data) as BrowserEventFrame)
    } catch {
      /* 忽略坏帧 */
    }
  }
  return () => source.close()
}
