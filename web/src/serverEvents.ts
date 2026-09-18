/** 全局服务端推送汇聚点:一条链路 + 按类型分发。
 *
 * ## 连接预算
 *
 * 浏览器对**同源** HTTP/1.1 连接有 6 条上限(Chrome/Firefox 都是这个数),而每个
 * `EventSource` 都是一条**常驻**连接。此前 `/api/events` 被 5 处各自订阅,加上
 * `/api/browser/stream` 恰好占满 6 条名额 —— 之后所有 `fetch` 在浏览器侧**永久
 * 排队**,表现为"点了没反应"。因此全应用只保留**一条**链路,按类型分发。新增
 * 订阅方一律走 `subscribeServerEvents`,不要再自己 `new EventSource`。
 *
 * ## 两种传输,自动择路
 *
 * 经 cloudflared 隧道时 SSE 的帧**完全透不过来**(实测:响应头 200、之后 0 帧;
 * 首帧垫 2KB / 16KB、心跳提到 1 秒都无效),而同一条隧道上挂 45 秒的普通响应
 * 完好穿透。所以这里有两种实现:SSE 优先,判死则整条链路切长轮询。判据与"为什
 * 么轮询通不能算 SSE 通"见 `pushChannel.ts`。
 *
 * 同一时刻只跑一种:`source` 与 `poll` 互斥,不会同时占两个连接名额。
 *
 * ## 依赖方向
 *
 * 本模块**不 import `api.ts`** —— `api.ts` 的 `subscribeEvents` 依赖这里,反过来
 * 引就是模块环。轮询接口因此单独放在零依赖的 `pushTransport.ts`。
 */

import {
  isPushDead,
  isPushStalled,
  markPushAttempt,
  noteSseFrame,
  reprobePush,
  setPushDead,
  subscribePushDead,
} from './pushChannel'
import { POLL_HOLD_SEC, pollServerEvents } from './pushTransport'

export type ServerEventType = string

type Listener = (type: ServerEventType, payload: unknown) => void

let refCount = 0
const listeners = new Set<Listener>()

/** 两种传输互斥;当前在跑的那个。 */
let source: EventSource | null = null
let poll: PollTransport | null = null

/** 连接状态订阅者:把"断线重连中"提示收敛到一处。 */
type StatusListener = (connected: boolean) => void
const statusListeners = new Set<StatusListener>()
let connected = false
let everOpened = false
/** 长轮询游标:与传输方式无关,切换时不能重置,否则事件会整批重放。 */
let pollCursor = 0
/** 上一批的 running 集合指纹:变化时才补发 running-snapshot。 */
let runningKey: string | null = null

function emitStatus(next: boolean) {
  if (connected === next) return
  connected = next
  for (const listener of statusListeners) listener(next)
}

function dispatch(type: string, payload: unknown) {
  for (const listener of listeners) listener(type, payload)
}

/** running 集合 → 与 SSE 首帧同形的事件,让上层逻辑无需区分传输方式。 */
function syncRunning(ids: string[] | undefined) {
  const next = (ids ?? []).join(',')
  if (runningKey === next) return
  runningKey = next
  dispatch('running-snapshot', { type: 'running-snapshot', ids: ids ?? [] })
}

/* ---------------------------------------------------------------- SSE -- */

function openSse() {
  if (source || poll) return
  const es = new EventSource('/api/events')
  source = es
  markPushAttempt()
  // 帧间静默是唯一可用的死信:半死链路不报错,只是不发数据。隧道上正是这种
  // 形态(握手 200、之后一帧不来),所以不能等 onerror —— 它根本不会触发。
  const stallTimer = window.setInterval(() => {
    if (isPushStalled()) setPushDead(true)
  }, 5_000)
  const stopStallWatch = () => window.clearInterval(stallTimer)
  es.onopen = () => {
    // 响应头到达不等于"通":这里**不**上报活性,否则会把握手成功当成链路活着、
    // 永远判不死。活性只认帧。
    emitStatus(true)
  }
  es.onerror = () => {
    // EventSource 自带重连;这里只汇报状态。真死了由 stall 判定并降级。
    emitStatus(false)
  }
  es.onmessage = (event) => {
    noteSseFrame()
    everOpened = true
    emitStatus(true)
    let type: string | undefined
    let payload: unknown = null
    try {
      const parsed = JSON.parse(event.data) as { type?: string; ids?: string[] }
      type = parsed?.type
      payload = parsed
      if (type === 'running-snapshot') syncRunning(parsed.ids)
    } catch {
      return /* 忽略坏帧 */
    }
    // 心跳帧只是活性凭据,已经上报过了;它没有业务语义,不要往下分发。
    if (!type || type === 'hb') return
    dispatch(type, payload)
  }
  const unsubscribe = subscribePushDead((dead) => {
    if (!dead) return
    stopStallWatch()
    unsubscribe()
    closeSse()
    startPoll()
  })
  sseCleanups.push(() => {
    stopStallWatch()
    unsubscribe()
  })
}

const sseCleanups: (() => void)[] = []

function closeSse() {
  if (!source) return
  source.close()
  source = null
  emitStatus(false)
}

/* ------------------------------------------------------------ 长轮询 -- */

interface PollTransport {
  controller: AbortController | null
  timer: number | null
  failures: number
  stopped: boolean
}

/** 前台挂起时长:服务端上限 30 秒,客户端砍到 25 留公网余量。 */
const HOLD_SEC = POLL_HOLD_SEC
/** 后台不挂请求(挂死的请求容易被浏览器/代理悄悄回收),退到这个间隔再问。 */
const IDLE_INTERVAL_MS = 20_000
const BACKOFF_STEPS = [500, 1_000, 2_000, 5_000, 10_000]

function startPoll() {
  if (poll || source) return
  const transport: PollTransport = { controller: null, timer: null, failures: 0, stopped: false }
  poll = transport
  const schedule = (delayMs: number) => {
    if (transport.stopped || transport.timer !== null) return
    transport.timer = window.setTimeout(() => {
      transport.timer = null
      void cycle()
    }, delayMs)
  }
  const cycle = async () => {
    if (transport.stopped) return
    const background = document.hidden
    const controller = new AbortController()
    transport.controller = controller
    const result = await pollServerEvents(
      pollCursor,
      background ? 0 : HOLD_SEC,
      controller.signal,
    )
    transport.controller = null
    if (transport.stopped) return
    if (result.kind === 'ok') {
      transport.failures = 0
      everOpened = true
      pollCursor = result.batch.seq
      emitStatus(true)
      syncRunning(result.batch.running)
      // resync = 服务端环缓冲断档,增量不完整:补一次列表失效,让订阅方各自
      // 重取权威状态 —— 对应 SSE 侧"lagged 就重连重快照"。
      if (result.batch.resync) dispatch('sessions-updated', { type: 'sessions-updated' })
      for (const event of result.batch.events) {
        const type = (event as { type?: string }).type
        if (!type || type === 'hb') continue
        dispatch(type, event)
      }
      schedule(background ? IDLE_INTERVAL_MS : 0)
      return
    }
    if (result.kind === 'aborted') return
    emitStatus(false)
    // 远程会话过期(401/403):再轮询也是同样结果,停在这里等用户重新扫码,
    // 别把请求打满。
    if (result.kind === 'status' && (result.status === 401 || result.status === 403)) {
      stopPoll()
      return
    }
    const step = BACKOFF_STEPS[Math.min(transport.failures, BACKOFF_STEPS.length - 1)]
    transport.failures += 1
    schedule(step)
  }
  void cycle()
}

function stopPoll() {
  if (!poll) return
  poll.stopped = true
  if (poll.timer !== null) window.clearTimeout(poll.timer)
  poll.controller?.abort()
  poll = null
  emitStatus(false)
}

/* ------------------------------------------------------------ 生命周期 -- */

function ensureTransport() {
  if (source || poll) return
  if (isPushDead()) startPoll()
  else openSse()
}

function releaseTransport() {
  closeSse()
  stopPoll()
  for (const cleanup of sseCleanups.splice(0)) cleanup()
}

/** 订阅服务端推送,返回退订函数。全应用共用底层那一条链路。 */
export function subscribeServerEvents(listener: Listener): () => void {
  listeners.add(listener)
  refCount += 1
  ensureTransport()
  return () => {
    listeners.delete(listener)
    refCount -= 1
    if (refCount <= 0) {
      refCount = 0
      releaseTransport()
      runningKey = null
    }
  }
}

/** 订阅连接状态(供顶部"连接断开"提示使用)。 */
export function subscribeServerStatus(listener: StatusListener): () => void {
  statusListeners.add(listener)
  listener(connected)
  return () => statusListeners.delete(listener)
}

/** 是否已成功建立过连接。 */
export function hasServerEventsOpened(): boolean {
  return everOpened
}

// 回到前台:手机锁屏 / 切后台再回来,链路状态可能已经变了。
// 立刻补一轮(不挂起)拿回落下的增量,而不是等下一个间隔;是否顺带重探 SSE
// 交给 reprobePush 的冷却 —— 刚判死就重探,只会让用户在"探测 → 白等一整个
// 判定周期"里来回挨卡。
document.addEventListener('visibilitychange', () => {
  if (document.hidden || refCount <= 0) return
  const transport = poll
  if (!transport) return
  // 先补一轮(wait=0,服务端立即返回):锁屏期间落下的增量立刻对齐,不必等
  // 下一个排定间隔 —— 手机上"回来看不到最新内容"就是这么攒出来的。
  if (transport.timer !== null) {
    window.clearTimeout(transport.timer)
    transport.timer = null
  }
  void pollOnceNow(transport)
  // 是否顺带重探 SSE 交给 reprobePush 的冷却:刚判死就重探,只会让用户在
  // "探测 → 白等一整个判定周期"里来回挨卡。
  if (!reprobePush()) return
  stopPoll()
  closeSse()
  openSse()
})

/** 立即跑一轮(回到前台时的"先补状态",不等已排定的定时器)。 */
async function pollOnceNow(transport: PollTransport) {
  transport.controller?.abort()
  const controller = new AbortController()
  transport.controller = controller
  const result = await pollServerEvents(pollCursor, 0, controller.signal)
  if (transport.controller === controller) transport.controller = null
  if (transport.stopped || result.kind !== 'ok') return
  pollCursor = result.batch.seq
  syncRunning(result.batch.running)
  for (const event of result.batch.events) {
    const type = (event as { type?: string }).type
    if (!type || type === 'hb') continue
    dispatch(type, event)
  }
}

/** 仅测试用:重置模块级状态。 */
export function __resetServerEventsForTest() {
  releaseTransport()
  refCount = 0
  connected = false
  everOpened = false
  pollCursor = 0
  runningKey = null
  listeners.clear()
}
