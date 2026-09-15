/** 全局 SSE 汇聚点。
 *
 * 浏览器对**同源** HTTP/1.1 连接有 6 条上限(Chrome/Firefox 都是这个数),
 * 而每个 `EventSource` 都是一条**常驻**连接。此前 `/api/events` 被 5 处
 * 各自订阅(App、RemoteBanner、SettingsModal、SessionsPage、以及设置面板),
 * 加上 `/api/browser/stream`,恰好把 6 条名额占满 —— 于是之后所有 `fetch`
 * (包括"关闭隧道""刷新链接")在浏览器侧**永久排队**,一个字节都发不出去,
 * 表现为"点了没反应、要响应半天"。服务端其实是毫秒级返回的。
 *
 * 因此全应用只保留**一条** `/api/events` 连接,按事件类型分发给订阅者。
 * 新增订阅方一律走 `subscribeServerEvents`,不要再自己 `new EventSource`。
 */

export type ServerEventType = string

type Listener = (type: ServerEventType, payload: unknown) => void

let source: EventSource | null = null
let refCount = 0
const listeners = new Set<Listener>()

/** 连接状态订阅者:用于把"断线重连中"提示收敛到一处。 */
type StatusListener = (connected: boolean) => void
const statusListeners = new Set<StatusListener>()
let connected = false
/** 是否曾经连上过 —— 首次连接与断线重连的语义不同(前者要拉列表,后者也要)。 */
let everOpened = false

function emitStatus(next: boolean) {
  if (connected === next) return
  connected = next
  for (const listener of statusListeners) listener(next)
}

function ensureSource() {
  if (source) return
  const es = new EventSource('/api/events')
  source = es
  es.onopen = () => {
    emitStatus(true)
    everOpened = true
  }
  es.onerror = () => {
    // EventSource 自带重连,这里只汇报状态,不关闭、不重建。
    emitStatus(false)
  }
  es.onmessage = (event) => {
    let type: string | undefined
    let payload: unknown = null
    try {
      const parsed = JSON.parse(event.data) as { type?: string }
      type = parsed?.type
      payload = parsed
    } catch {
      return /* 忽略坏帧 */
    }
    if (!type) return
    for (const listener of listeners) listener(type, payload)
  }
}

function releaseIfIdle() {
  if (refCount > 0 || !source) return
  source.close()
  source = null
  connected = false
  everOpened = false
}

/** 订阅 `/api/events`,返回退订函数。全应用共用底层那一条连接。 */
export function subscribeServerEvents(listener: Listener): () => void {
  listeners.add(listener)
  refCount += 1
  ensureSource()
  return () => {
    listeners.delete(listener)
    refCount -= 1
    releaseIfIdle()
  }
}

/** 订阅连接状态(供顶部"连接断开"提示使用)。 */
export function subscribeServerStatus(listener: StatusListener): () => void {
  statusListeners.add(listener)
  listener(connected)
  return () => {
    statusListeners.delete(listener)
  }
}

/** 是否已成功建立过连接。 */
export function hasServerEventsOpened(): boolean {
  return everOpened
}

/** 仅测试用:重置模块级状态。 */
export function __resetServerEventsForTest() {
  source?.close()
  source = null
  refCount = 0
  connected = false
  everOpened = false
  listeners.clear()
  statusListeners.clear()
}
