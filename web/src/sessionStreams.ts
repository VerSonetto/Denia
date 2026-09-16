import * as api from './api'
import type { SessionAnchor } from './api'
import type { SessionEnvelope, SessionHeader } from './types'
import { isPushDead, setPushDead } from './pushChannel'
import { POLL_HOLD_SEC, pollSessionFollow } from './pushTransport'

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
 * 后端日志被外部改写(回退物理截断 → seq 重新编号)时,任何与本地
 * cursor 不符的帧都会触发快照重对齐,不会静默吞帧。
 *
 * ## 自愈重连
 *
 * 流断(transport error / lag / 无帧超时)自动退避重连
 * (300ms → 1s → 3s → 10s 封顶);重连成功即以快照校正;
 * 会话被删(404)则停止重连并通知视图卸载。
 *
 * ## 经隧道时改用长轮询
 *
 * 实测(2026-09-16,scripts/probe-*.mjs):SSE 经 cloudflared 隧道时**一帧都不
 * 透传**(响应头 200 拿得到,之后什么都不来;首帧垫 2KB/16KB、心跳提到 1s 皆无效),
 * 而同一条隧道挂 45 秒的普通响应完好穿透。不处理的话,这里每条流都靠 32 秒
 * 无帧看门狗超时 → 重连 → 重拉快照,用户看到的就是"内容每 ~32 秒跳一次"。
 *
 * 所以链路上判定 SSE 不通(见 `pushChannel.ts`)后,本引擎改用
 * `/api/sessions/:id/follow/poll` 长轮询:服务端挂到有事件才返回,感知延迟
 * 从 ~32 秒降到一次请求往返。
 */

export interface SessionPageMeta {
  total: number
  hasMoreBefore: boolean
  /** 全会话非注入 user-message 锚点(轮次轴刻度)。 */
  anchors: SessionAnchor[]
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
      const meta = {
        total: data.total,
        hasMoreBefore: data.hasMoreBefore,
        anchors: data.anchors ?? [],
      }
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

/** 后台不挂长请求(挂死的会被浏览器/代理悄悄回收),按这个间隔快轮。 */
const FOLLOW_POLL_IDLE_MS = 15_000

/** follow 的两种传输按链路健康度择路:SSE 判死后改走长轮询。 */
function openFollow(id: string, state: StreamState, gen: number) {
  if (state.dead || gen !== state.generation) return
  if (isPushDead()) {
    openFollowPoll(id, state, gen)
    return
  }
  const controller = api.followSession(
    id,
    state.cursor,
    (envelope) => deliver(id, state, gen, envelope),
    () => {
      // 流结束(EOF/error/lagged):只要不是主动关,就退避重连。
      if (state.dead || gen !== state.generation) return
      scheduleRetry(id, state)
    },
  )
  state.controller = controller
  // watchdog:32 秒无帧(SSE 心跳 15 秒一次,说明通道死了)→ 主动断、重连。
  // 关键是同时把降级决定权交给 pushChannel:只重连不换传输,就还是每 32 秒
  // 白跑一趟 —— 那正是"AI 早回完了、手机上几十秒才跳出来"的成因。
  state.watchdogTimer = window.setTimeout(() => {
    state.watchdogTimer = null
    if (state.dead || gen !== state.generation) return
    if (now() - state.lastFrameAt <= 30_000) return
    setPushDead(true)
    abortAndNull(state)
    scheduleRetry(id, state)
  }, 32_000)
}

/** seq 连续性判定:SSE 与轮询共用同一套,两条路的行为必须一致。 */
function deliver(
  id: string,
  state: StreamState,
  gen: number,
  envelope: SessionEnvelope,
) {
  if (state.dead || gen !== state.generation) return
  state.lastFrameAt = now()
  if (envelope.seq === state.cursor + 1) {
    state.cursor = envelope.seq
    for (const listener of state.listeners) listener.onEnvelope(envelope)
    return
  }
  // 与本地认知不符的帧一律重拿快照自愈(resnapshot 内有 300ms 节流):
  // - seq > cursor+1:断档(follow lag 静默丢帧);
  // - seq <= cursor:正常只应是连接建立时 replay 与广播的重叠重复帧;但后端日志
  //   被外部改写(回退物理截断后 seq 重新编号、其他标签页/其他实例回退)时,
  //   新帧 seq 会整段落回 cursor 之下——旧 cursor 从此永久失真,若静默丢弃,
  //   新消息将一条都收不到,只能靠用户刷新。所以重复帧也不无脑吞掉。
  resnapshot(id, state)
}

/**
 * 长轮询版的 follow:SSE 判死时取代 SSE 那条连接。
 *
 * 每轮都从服务端读"当前 cursor 之后的已落盘事件",所以不必像 SSE 那样防 replay
 * 与广播重叠 —— cursor 就是唯一事实。一轮返回后立刻发下一轮、由服务端挂到有事件
 * 才回,稳态下手机上只有一次往返的延迟,而不是 32 秒一跳。
 *
 * 这里**不上报 pushChannel 活性**:长轮询通不等于 SSE 通(见 pushChannel 的
 * "一条铁律"),否则会立刻解除降级、回到 SSE、又 0 帧,来回挨卡。
 */
function openFollowPoll(id: string, state: StreamState, gen: number) {
  const controller = new AbortController()
  state.controller = controller

  const nextRound = (delayMs: number) => {
    if (state.dead || gen !== state.generation) return
    state.watchdogTimer = window.setTimeout(() => {
      state.watchdogTimer = null
      openFollow(id, state, gen)
    }, delayMs)
  }

  void (async () => {
    // 后台不挂长请求:服务端见 wait=0 立即返回,拿一次就按固定间隔再问。
    const background = typeof document !== 'undefined' && document.hidden
    const result = await pollSessionFollow<SessionEnvelope>(
      id,
      state.cursor,
      background ? 0 : POLL_HOLD_SEC,
      controller.signal,
    )
    if (state.dead || gen !== state.generation) return
    if (result.kind === 'ok') {
      // 轮到就把退避清零:能拿到响应说明链路是通的。
      state.retryDelayMs = 300
      for (const envelope of result.batch.envelopes ?? []) {
        if (state.dead || gen !== state.generation) return
        deliver(id, state, gen, envelope)
      }
      nextRound(background ? FOLLOW_POLL_IDLE_MS : 0)
      return
    }
    // 调用方主动 abort(切会话/代际失效/引擎卸载):不是故障,静默收尾。
    if (result.kind === 'aborted') return
    if (result.kind === 'status' && result.status === 404) {
      state.dead = true
      state.controller = null
      for (const listener of state.listeners) listener.onNotFound?.()
      return
    }
    // 轮询也失败 = 网络本身断了(或服务端重启)。放开降级判定,让下一次
    // openFollow 重新走 SSE 探路 —— 网络恢复后 SSE 可能又能用了。
    if (result.kind === 'transportError' || (result.kind === 'status' && result.status >= 500)) {
      setPushDead(false)
    }
    scheduleRetry(id, state)
  })()
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

/**
 * 会话状态失效(回退/外部截断):强制立即重连对齐。
 *
 * 后端 rewind 物理截断日志后事件 seq 重新编号,本地 cursor 与 follow
 * 连接全部失真。这里不销毁流(销毁会让仍在挂载的监听者——如会话级
 * 权限/审批监听——永久掉线),而是 abort 旧连接 + 立即重拉快照:
 * 快照成功后 cursor 校正为截断后的日志尾,onSnapshot 让所有监听者
 * 全量重建,既存的 follow 意愿(运行中)也会按新 cursor 重开。
 * 显式调用让"回退后对齐"不依赖视图重挂载这类间接效应。
 */
export function invalidateSession(id: string): void {
  const state = streams.get(id)
  if (!state || state.dead) return
  reconnect(id, state)
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
