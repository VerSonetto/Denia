/**
 * 终端面板的 API 客户端:REST 管生命周期 + WebSocket 管交互数据。
 *
 * 分工理由见 `crates/server/src/api/terminals.rs` 顶部注释:键盘输入每敲一下
 * 都要往返,走 HTTP 延迟会叠加;WebSocket 一条连接双向复用。
 */

export interface TerminalInfo {
  id: string
  /** shell 展示标签(如 `PowerShell 7 (pwsh)`)。 */
  shell: string
  cwd: string
  cols: number
  rows: number
  createdAt: number
  exitCode?: number
}

export interface TerminalSnapshot {
  terminals: TerminalInfo[]
}

/** 服务端 → 客户端的帧。 */
export type TerminalFrame =
  | { type: 'data'; terminalId: string; data: string }
  | { type: 'replay'; data: string }
  | { type: 'exit'; terminalId: string; exitCode: number }
  | { type: 'closed'; terminalId: string; reason: string }
  | { type: 'terminals-changed' }
  | { type: 'error'; message: string }

async function http<T>(path: string, init?: RequestInit, timeoutMs = 15_000): Promise<T> {
  const response = await fetch(path, {
    ...init,
    headers: { 'content-type': 'application/json', ...(init?.headers ?? {}) },
    signal: AbortSignal.timeout(timeoutMs),
  })
  const text = await response.text()
  const value = text ? (JSON.parse(text) as unknown) : {}
  if (!response.ok) {
    const message =
      (value as { error?: { message?: string } }).error?.message ?? response.statusText
    throw new Error(message)
  }
  return value as T
}

export function fetchTerminals(): Promise<TerminalSnapshot> {
  return http<TerminalSnapshot>('/api/terminals')
}

export function createTerminal(body: {
  cwd?: string
  cols?: number
  rows?: number
}): Promise<{ terminal: TerminalInfo }> {
  return http<{ terminal: TerminalInfo }>('/api/terminals', {
    method: 'POST',
    body: JSON.stringify(body),
  })
}

export function closeTerminal(id: string): Promise<{ ok: boolean }> {
  return http<{ ok: boolean }>(`/api/terminals/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  })
}

export function resizeTerminal(id: string, cols: number, rows: number): Promise<{ ok: boolean }> {
  return http<{ ok: boolean }>(`/api/terminals/${encodeURIComponent(id)}/resize`, {
    method: 'POST',
    body: JSON.stringify({ cols, rows }),
  })
}

/* ---- base64:PTY 字节流不保证是合法 UTF-8,必须走二进制安全编码 ---- */

export function encodeInput(text: string): string {
  const bytes = new TextEncoder().encode(text)
  let binary = ''
  for (const byte of bytes) binary += String.fromCharCode(byte)
  return btoa(binary)
}

export function decodeOutput(base64: string): Uint8Array {
  const binary = atob(base64)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i)
  return bytes
}

/** 一个终端连接:自动重连 + 输入发送 + 输出回调。 */
export interface TerminalConnection {
  send(data: string): void
  resize(cols: number, rows: number): void
  dispose(): void
}

/**
 * 连接一个终端。
 *
 * `replay=1` 让服务端先补发回滚缓冲:重连(含服务端因 Lagged 主动断开)
 * 后画面能自愈,不会留下花屏。
 */
export function connectTerminal(
  id: string,
  handlers: {
    onData(bytes: Uint8Array): void
    onExit(exitCode: number): void
    onError(message: string): void
  },
): TerminalConnection {
  let socket: WebSocket | null = null
  let disposed = false
  let retryTimer: number | undefined
  let retryDelay = 300
  /** 重连期间累积的输入:连接没建立时按键不能丢。 */
  let pending: string[] = []
  let pendingResize: { cols: number; rows: number } | null = null

  const open = () => {
    if (disposed) return
    const scheme = window.location.protocol === 'https:' ? 'wss' : 'ws'
    const url = `${scheme}://${window.location.host}/api/terminals/${encodeURIComponent(id)}/ws?replay=1`
    let next: WebSocket
    try {
      next = new WebSocket(url)
    } catch (error) {
      handlers.onError(error instanceof Error ? error.message : String(error))
      scheduleRetry()
      return
    }
    socket = next

    next.addEventListener('open', () => {
      retryDelay = 300
      for (const item of pending) next.send(JSON.stringify({ type: 'input', data: item }))
      pending = []
      if (pendingResize) {
        next.send(JSON.stringify({ type: 'resize', ...pendingResize }))
        pendingResize = null
      }
    })

    next.addEventListener('message', (event) => {
      let frame: TerminalFrame
      try {
        frame = JSON.parse(String(event.data)) as TerminalFrame
      } catch {
        return
      }
      switch (frame.type) {
        case 'data':
          handlers.onData(decodeOutput(frame.data))
          break
        case 'replay':
          handlers.onData(decodeOutput(frame.data))
          break
        case 'exit':
          handlers.onExit(frame.exitCode)
          break
        case 'closed':
          handlers.onExit(0)
          break
        case 'error':
          handlers.onError(frame.message)
          break
        default:
          break
      }
    })

    next.addEventListener('close', () => {
      if (socket === next) socket = null
      scheduleRetry()
    })

    // onerror 之后一定跟着 close,重连交给 close 统一处理,避免双触发。
    next.addEventListener('error', () => {})
  }

  const scheduleRetry = () => {
    if (disposed || retryTimer !== undefined) return
    retryTimer = window.setTimeout(() => {
      retryTimer = undefined
      retryDelay = Math.min(retryDelay * 2, 5000)
      open()
    }, retryDelay)
  }

  open()

  return {
    send(data: string) {
      const encoded = encodeInput(data)
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: 'input', data: encoded }))
      } else {
        // 未连接:入队,连上后补发(上限防止长时间断线把内存撑爆)。
        pending.push(encoded)
        if (pending.length > 512) pending.shift()
      }
    },
    resize(cols: number, rows: number) {
      if (socket && socket.readyState === WebSocket.OPEN) {
        socket.send(JSON.stringify({ type: 'resize', cols, rows }))
      } else {
        pendingResize = { cols, rows }
      }
    },
    dispose() {
      disposed = true
      if (retryTimer !== undefined) window.clearTimeout(retryTimer)
      retryTimer = undefined
      pending = []
      const current = socket
      socket = null
      if (current) {
        // 先摘掉 close 监听再关,避免 dispose 触发一次重连。
        current.onclose = null
        try {
          current.close()
        } catch {
          /* 已关闭:忽略。 */
        }
      }
    },
  }
}
