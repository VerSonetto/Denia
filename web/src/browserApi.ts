import type {
  BrowserState,
  BrowserEventFrame,
  ReconBreakpoint,
  ReconConsoleEntry,
  ReconMatch,
  ReconPaused,
  ReconScript,
} from './types'

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

/** 订阅浏览器事件流(tabs/frame/dialog/navigated/exited/debugger)。
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

/* ---- JS 逆向工作台(原生 API,不依赖浏览器命令面) ---- */

interface ReconResp {
  tabId: string
}

async function recon<Body extends Record<string, unknown>, T>(
  endpoint: string,
  body: Body,
): Promise<T & ReconResp> {
  return request<T & ReconResp>(endpoint, {
    method: 'POST',
    body: JSON.stringify(body),
  })
}

/** 脚本表。 */
export async function reconScripts(tabId: string, urlFilter?: string): Promise<{ scripts: ReconScript[] } & ReconResp> {
  return recon('/api/browser/recon/scripts', { tabId, urlFilter: urlFilter || undefined })
}

/** 跨脚本检索。 */
export async function reconSearch(
  tabId: string,
  query: string,
  opts?: { isRegex?: boolean; caseSensitive?: boolean; urlFilter?: string; maxResults?: number },
): Promise<{ matches: ReconMatch[]; skippedLarge: number } & ReconResp> {
  return recon('/api/browser/recon/search', { tabId, query, ...opts })
}

/** 取脚本源码片段。 */
export async function reconSource(
  tabId: string,
  opts: { scriptId?: string; url?: string; startLine?: number; endLine?: number },
): Promise<{ source: string; totalLines: number; truncated: boolean; scriptId: string; url: string } & ReconResp> {
  return recon('/api/browser/recon/source', { tabId, ...opts })
}

/** 设文本断点。 */
export async function reconSetBreakpoint(
  tabId: string,
  query: string,
  opts?: { urlFilter?: string; occurrence?: number; condition?: string },
): Promise<{ breakpoint: ReconBreakpoint } & ReconResp> {
  return recon('/api/browser/recon/breakpoint', { tabId, query, ...opts })
}

/** 删断点。 */
export async function reconRemoveBreakpoint(
  tabId: string,
  breakpointId: string,
): Promise<{ removed: boolean } & ReconResp> {
  return recon('/api/browser/recon/breakpointRemove', { tabId, breakpointId })
}

/** 断点列表。 */
export async function reconBreakpoints(tabId: string): Promise<{ breakpoints: ReconBreakpoint[] } & ReconResp> {
  return recon('/api/browser/recon/breakpoints', { tabId })
}

/** 当前暂停详情(未暂停 = null)。 */
export async function reconPaused(tabId: string): Promise<{ paused: ReconPaused | null } & ReconResp> {
  return recon('/api/browser/recon/paused', { tabId })
}

/** 暂停帧内求值。 */
export async function reconEval(
  tabId: string,
  expression: string,
  frameIndex?: number,
): Promise<{ value: unknown } & ReconResp> {
  return recon('/api/browser/recon/eval', { tabId, expression, frameIndex: frameIndex ?? 0 })
}

/** 单步 over/into/out。 */
export async function reconStep(tabId: string, direction: 'over' | 'into' | 'out'): Promise<{ stepped: string } & ReconResp> {
  return recon('/api/browser/recon/step', { tabId, direction })
}

/** 恢复执行。 */
export async function reconResume(tabId: string): Promise<{ resumed: boolean } & ReconResp> {
  return recon('/api/browser/recon/resume', { tabId })
}

/** 控制台消息。 */
export async function reconConsole(
  tabId: string,
  max?: number,
): Promise<{ messages: ReconConsoleEntry[] } & ReconResp> {
  return recon('/api/browser/recon/console', { tabId, max: max ?? 100 })
}

/** 清空该 tab 侦察数据(脚本表/控制台/抓包)。 */
export async function reconClear(tabId: string): Promise<{ cleared: boolean } & ReconResp> {
  return recon('/api/browser/recon/clear', { tabId })
}
