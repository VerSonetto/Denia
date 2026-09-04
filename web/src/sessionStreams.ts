import * as api from './api'
import type { SessionEnvelope, SessionHeader } from './types'

/**
 * 会话事件流引擎(每会话一个状态机)。
 *
 * ## 连接预算(通信稳定性的关键)
 *
 * 只有**用户正在查看**的会话持有 SSE follow 长连接;切走会话即断开。
 * 浏览器对同源的 HTTP/1.1 连接数是有上限的,后台会话若各占一条长连接,
 * 新请求(发送消息等)会排队饿死——这是"点了没反应/消息发不出去"的
 * 典型成因。运行状态改为服务端 `/api/events` 推送(`running-changed`),
 * 侧栏圆点不依赖 follow。
 *
 * ## 竞态安全
 *
 * 每次(重)连接/快照递增 `generation`,一切回调先校验 generation,
 * 过期回调直接丢弃;不可能出现旧快照覆盖新状态的竞态。
 *
 * ## 自愈重连
 *
 * 流断(transport error / lag / 无帧超时)自动退避重连
 * (300ms → 1s → 3s → 10s 封顶);重连成功即以快照校正;
 * 会话被删(404)则停止重连并通知视图卸载。
 */

export interface SessionPageMeta {
  total: number
  hasMoreBefore: boolean
}

export interface SessionStreamListener {
  /** 有限窗口快照(fold 重建基础);重连自愈后也会再次送达。 */
  onSnapshot(header: SessionHeader, events: SessionEnvelope[], meta?: SessionPageMeta): void
  /** 增量帧,已按 seq 去重并保证连续。 */
  onEnvelope(envelope: SessionEnvelope): void
  /** 会话已不存在(被删除):视图应卸载,引擎停止重连。 */
  onNotFound?(): void
}

interface StreamState {
  sessionId: string
  generation: number
  controller: AbortController | null
  cursor: number
  listeners: Set<SessionStreamListener>
  resnapshotAt: number
  retryDelayMs: number
  retryTimer: number | null
  watchdogTimer: number | null
  /** 最近一次收到帧的时间(watchdog 依据)。 */
  lastFrameAt: number
  /** 主动关闭(会话删除/引擎卸载→不再重连)。 */
  dead: boolean
  /** 是否已请求 follow:运行中才开长连接,普通浏览不加载全量后端会话。 */
  followRequested: boolean
}

/** 初始快照只拉尾部这么多事件,避免长会话一次全量进前端。 */
const INITIAL_PAGE_LIMIT = 500

const streams = new Map<string, StreamState>()

function now() {
  return Date.now()
}

function stateOf(id: string): StreamState {
  let state = streams.get(id)
  if (!state) {
    state = {
      sessionId: id,
      generation: 0,
      controller: null,
      cursor: 0,
      listeners: new Set(),
      resnapshotAt: 0,
      retryDelayMs: 300,
      retryTimer: null,
      watchdogTimer: null,
      lastFrameAt: now(),
      dead: false,
      followRequested: false,
    }
    streams.set(id, state)
  }
  return state
}

function disposeState(state: StreamState) {
  if (streams.get(state.sessionId) === state) streams.delete(state.sessionId)
  state.dead = true
  clearTimers(state)
  state.controller?.abort()
  state.controller = null
}

function clearTimers(state: StreamState) {
  if (state.retryTimer !== null) {
    window.clearTimeout(state.retryTimer)
    state.retryTimer = null
  }
  if (state.watchdogTimer !== null) {
    window.clearTimeout(state.watchdogTimer)
    state.watchdogTimer = null
  }
}

function abortAndNull(state: StreamState) {
  state.controller?.abort()
  state.controller = null
  if (state.watchdogTimer !== null) {
    window.clearTimeout(state.watchdogTimer)
    state.watchdogTimer = null
  }
}

/** (重)连接:先有限窗口快照,按需再 follow。gen 守卫所有回调。 */
function reconnect(id: string, state: StreamState) {
  if (state.dead) return
  if (state.retryTimer !== null) {
    window.clearTimeout(state.retryTimer)
    state.retryTimer = null
  }
  const gen = ++state.generation
  abortAndNull(state)
  const controller = new AbortController()
  state.controller = controller
  state.resnapshotAt = now()

  void (async () => {
    try {
      const data = await api.getSessionPage(id, { limit: INITIAL_PAGE_LIMIT }, controller.signal)
      if (gen !== state.generation || state.dead) return
      // 快照成功:重置退避,以快照校正 cursor。
      state.retryDelayMs = 300
      state.cursor = data.events.length ? data.events[data.events.length - 1].seq : 0
      state.lastFrameAt = now()
      const meta = { total: data.total, hasMoreBefore: data.hasMoreBefore }
      for (const listener of state.listeners) {
        listener.onSnapshot(data.header, data.events, meta)
      }
      if (state.listeners.size === 0) {
        disposeState(state)
        return
      }
      if (state.followRequested) {
        openFollow(id, state, gen)
      }
    } catch (error) {
      if (gen !== state.generation || state.dead) return
      if (isNotFound(error)) {
        state.dead = true
        state.controller = null
        for (const listener of state.listeners) listener.onNotFound?.()
        return
      }
      // 网络类失败:退避重试;快照失败不向视图报错,保持旧状态。
      scheduleRetry(id, state)
    }
  })()
}

/** 请求打开 SSE follow(通常由“会话已运行”触发);不加载全量快照路径。 */
export function ensureFollowing(id: string): void {
  const state = streams.get(id)
  if (!state || state.dead) return
  if (state.followRequested) return
  state.followRequested = true
  // 若当前已有监听且未建立 follow,重连一次走“快照后再 follow”。
  if (state.listeners.size > 0) {
    reconnect(id, state)
  }
}

function isNotFound(error: unknown): boolean {
  return error instanceof api.ApiError && error.status === 404
}

function scheduleRetry(id: string, state: StreamState) {
  if (state.dead || state.retryTimer !== null) return
  const delay = state.retryDelayMs
  state.retryDelayMs = Math.min(state.retryDelayMs * 2, 10_000)
  state.retryTimer = window.setTimeout(() => {
    state.retryTimer = null
    if (state.dead) return
    reconnect(id, state)
  }, delay)
}

function openFollow(id: string, state: StreamState, gen: number) {
  if (state.dead || gen !== state.generation) return
  const controller = api.followSession(
    id,
    state.cursor,
    (envelope) => {
      if (state.dead || gen !== state.generation) return
      state.lastFrameAt = now()
      if (envelope.seq <= state.cursor) return
      if (envelope.seq === state.cursor + 1) {
        state.cursor = envelope.seq
        for (const listener of state.listeners) listener.onEnvelope(envelope)
      } else {
        // 帧断档(follow lag 会静默丢帧):重拿快照自愈。
        resnapshot(id, state)
      }
    },
    () => {
      // 流结束(EOF/error/lagged):只要不是主动关,就退避重连。
      if (state.dead || gen !== state.generation) return
      scheduleRetry(id, state)
    },
  )
  state.controller = controller
  // watchdog:32s 无帧 → 主动断、重连(SSE keepalive 15s,说明通道死了)。
  state.watchdogTimer = window.setTimeout(() => {
    state.watchdogTimer = null
    if (state.dead || gen !== state.generation) return
    if (now() - state.lastFrameAt > 30_000) {
      abortAndNull(state)
      scheduleRetry(id, state)
    }
  }, 32_000)
}

function resnapshot(id: string, state: StreamState) {
  if (state.dead) return
  const nowMs = now()
  if (nowMs - state.resnapshotAt < 300) return
  state.resnapshotAt = nowMs
  reconnect(id, state)
}

/** 注册监听并保证流存活;返回注销函数。 */
export function attach(id: string, listener: SessionStreamListener): () => void {
  const state = stateOf(id)
  state.listeners.add(listener)
  reconnect(id, state)
  return () => {
    const current = streams.get(id)
    if (!current) return
    current.listeners.delete(listener)
    // 无人查看即断开:no 后台保活,连接预算恒定。
    if (current.listeners.size === 0) disposeState(current)
  }
}

/** 会话被删除:停掉所有流并释放。 */
export function dropSession(id: string): void {
  const state = streams.get(id)
  if (state) disposeState(state)
}

/** 引擎全量清理(仅测试/应用卸载)。 */
export function resetEngine(): void {
  for (const state of streams.values()) {
    state.dead = true
    clearTimers(state)
    state.controller?.abort()
  }
  streams.clear()
}

/** 供测试使用:当前持流会话数。 */
export function streamCount(): number {
  return streams.size
}

/** 供测试使用:一个会话的已确认 seq(投喂 follow 起点)。 */
export function cursorOf(id: string): number | undefined {
  return streams.get(id)?.cursor
}
