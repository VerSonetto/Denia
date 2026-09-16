/**
 * 推送的**长轮询**传输层(SSE 判死时的替代通道)。
 *
 * ## 为什么不复用 api.ts 的 http()
 *
 * `api.ts` 顶层 import 了 `serverEvents.ts`(它的 `subscribeEvents` 走全局汇聚)。
 * 若这里再从 `api.ts` 取 poll 接口,就成了 serverEvents → api → serverEvents 的
 * 模块环:两边都只在函数里延迟调用,运行时未必炸,但打包顺序一变就可能拿到
 * undefined。这个模块**不 import 任何本项目模块**,环就断在这里。
 *
 * ## 为什么返回结果对象而不是抛异常
 *
 * 调用方要分三种失败处理:404(会话已删 → 停止重连)、主动 abort(代际失效 →
 * 静默丢弃)、网络错误(退避重试)。用异常就得跨模块判 `ApiError.status`,或再
 * 造一个错误类型;把 status 直接带回来最省事,也最不容易误判成"该重试"。
 */

/** 服务端 `/api/events/poll` 的一批全局事件。 */
export interface ServerEventBatch {
  events: { type?: string }[]
  /** 已交付到的游标:下一轮从这里接着要。 */
  seq: number
  /** 服务端环缓冲断档 —— 增量不完整,订阅方应各自重取权威状态。 */
  resync: boolean
  /** 运行中的会话 id。每轮都带:SSE 版靠连接首帧传这份快照,轮询没有"首帧"。 */
  running: string[]
}

/** 服务端 `/api/sessions/:id/follow/poll` 的一批会话事件。 */
export interface FollowBatch<T> {
  envelopes: T[]
  seq: number
}

/**
 * 结果用 `kind` 判别,而不是 `ok` + 可选字段。
 *
 * 后者在 `{ok:false}` 的几个分支上访问 `status` / `aborted` 会报"属性不存在",
 * 调用方只能到处写 `'x' in result` —— 一旦漏写就是拿 undefined 去比较,静默走错
 * 分支。判别式联合让编译器强制调用方把三种失败分开处理。
 */
export type PollResult<T> =
  | { kind: 'ok'; batch: T }
  /** 拿到了响应但状态码非 2xx:404 一类需要停止重试。 */
  | { kind: 'status'; status: number }
  /** 调用方主动 abort(切会话/卸载/代际失效):不是故障,静默丢弃。 */
  | { kind: 'aborted' }
  /** 网络层失败或本地超时:按退避重试。 */
  | { kind: 'transportError' }

/**
 * 服务端单次挂起上限(与 `api::events::POLL_WAIT_MAX` 一致)。
 *
 * 实测隧道能穿透挂 45 秒的普通响应,取 30 秒留一倍余量;客户端再砍 5 秒,
 * 避免卡在边缘超时边界上拿到半截响应。
 */
export const POLL_HOLD_SEC = 25

/** 本地超时 = 服务端挂起时长 + 公网往返余量。 */
const holdTimeoutMs = (holdSec: number) => (holdSec + 15) * 1000

async function pollOnce<T>(
  url: string,
  signal: AbortSignal,
  holdSec: number,
): Promise<PollResult<T>> {
  // 再包一层 controller:外层的 signal 可能来自 fetch 组合场景,超时得由这里
  // 自己断,且不能污染调用方的 signal。
  const local = new AbortController()
  const onOuterAbort = () => local.abort()
  if (signal.aborted) local.abort()
  else signal.addEventListener('abort', onOuterAbort, { once: true })
  const watchdog = setTimeout(() => local.abort(), holdTimeoutMs(holdSec))
  try {
    const response = await fetch(url, { signal: local.signal })
    if (!response.ok) {
      await response.body?.cancel().catch(() => {})
      return { kind: 'status', status: response.status }
    }
    return { kind: 'ok', batch: (await response.json()) as T }
  } catch {
    // 外层先断 → 调用方的意图(切会话/卸载);否则算网络失败或本地超时。
    if (signal.aborted) return { kind: 'aborted' }
    return { kind: 'transportError' }
  } finally {
    clearTimeout(watchdog)
    signal.removeEventListener('abort', onOuterAbort)
  }
}

/** 全局失效通知通道。SSE 判死后由 `serverEvents.ts` 改走这里。 */
export function pollServerEvents(
  after: number,
  holdSec: number,
  signal: AbortSignal,
): Promise<PollResult<ServerEventBatch>> {
  return pollOnce<ServerEventBatch>(`/api/events/poll?after=${after}&wait=${holdSec}`, signal, holdSec)
}

/** 会话事件增量。SSE 判死后由 `sessionStreams.ts` 改走这里。 */
export function pollSessionFollow<T>(
  id: string,
  after: number,
  holdSec: number,
  signal: AbortSignal,
): Promise<PollResult<FollowBatch<T>>> {
  return pollOnce<FollowBatch<T>>(
    `/api/sessions/${encodeURIComponent(id)}/follow/poll?after=${after}&wait=${holdSec}`,
    signal,
    holdSec,
  )
}
