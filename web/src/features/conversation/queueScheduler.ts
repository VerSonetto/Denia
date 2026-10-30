/**
 * 会话消息队列的跨会话调度器。
 *
 * # 为什么必须独立于页面
 *
 * 队列原先挂在 `SessionsPage` 的 `useEffect` 上：只有「当前选中的会话」
 * 的 `running` 变化才会触发自动发送。用户切到别的会话后，原会话本轮结束
 * 时那个组件已经卸载，队首消息就永远躺在 IndexedDB 里，直到用户切回去
 * 再触发一次运行状态变化——这正是"队列消息跨会话不自动发送"的根因。
 *
 * 调度器只做三件事：
 * 1. 订阅**全局** running 集合，对每个 `true → false` 的会话独立尝试发送；
 * 2. 当前页面有该会话时走页面注册的发送器（保留斜杠命令/乐观行等完整
 *    语义）；否则直接从 composer 存储读队列并调用 prompt API；
 * 3. 成功后才出队并广播刷新事件，失败保留队列并提示，不重复发送。
 *
 * # 持久化与并发
 *
 * 队列仍存 composer 快照（IndexedDB），切会话/刷新不丢。跨标签页用
 * Web Locks（可用时）串行化同一会话的发送；同一标签页内另有 in-flight
 * 去重。服务端「引导」送达事件会把对应项移出队列，避免轮次结束后重发。
 */

import * as api from '../../api'
import { loadComposer, saveComposer, type ComposerSnapshot } from '../../composerMemory'
import { notify, subscribeRunningStatus } from '../../appStore'
import { subscribeServerEvents } from '../../serverEvents'
import { peekModelSelection } from '../../modelSelectionStore'
import { t } from '../../i18n'
import { isPendingAttachment, type QueuedMessage } from '../../types'

/** 队列变化广播：当前页面收到后同步本地 state（跨会话时无页面监听）。 */
export const QUEUE_UPDATED_EVENT = 'denia:queue-updated'

export interface QueueUpdatedDetail {
  sessionId: string
  queue: QueuedMessage[]
}

/** 「引导」等待步骤边界的宽限：给送达事件一个先到的机会，避免重发。 */
const STEERING_GRACE_MS = 350

export interface QueueSessionHandler {
  /** 页面当前持有的队列（可能比 IndexedDB 更新）。 */
  getQueue(): QueuedMessage[]
  /** 走页面完整的 postMessage 路径；返回 false 表示没有发出（保留队列）。 */
  send(message: QueuedMessage): Promise<boolean>
  /** 发送成功后由页面移除（含持久化）。 */
  remove(id: string): void
}

const handlers = new Map<string, QueueSessionHandler>()
const inFlight = new Set<string>()
const suppressed = new Set<string>()
let prevRunning: Record<string, boolean> = {}
let stopScheduler: (() => void) | null = null

function scopeOf(sessionId: string): string {
  return `session:${sessionId}`
}

export function registerQueueHandler(sessionId: string, handler: QueueSessionHandler): () => void {
  handlers.set(sessionId, handler)
  return () => {
    if (handlers.get(sessionId) === handler) handlers.delete(sessionId)
  }
}

/** 抑制某个会话的自动发送（插话/回退期间用；不抑制用户手动操作）。 */
export function suppressQueueAuto(sessionId: string): void {
  suppressed.add(sessionId)
}

export function resumeQueueAuto(sessionId: string): void {
  suppressed.delete(sessionId)
}

function broadcastQueue(sessionId: string, queue: QueuedMessage[]): void {
  window.dispatchEvent(
    new CustomEvent<QueueUpdatedDetail>(QUEUE_UPDATED_EVENT, {
      detail: { sessionId, queue },
    }),
  )
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms))
}

/**
 * 同会话的跨标签页互斥。Web Locks 不支持的环境退化为仅本标签页去重。
 */
async function withSessionLock<T>(sessionId: string, action: () => Promise<T>): Promise<T | null> {
  const locks = navigator.locks
  if (!locks?.request) return action()
  return locks.request(`denia-queue:${sessionId}`, { ifAvailable: true }, async (lock) => {
    if (!lock) return null
    return action()
  })
}

/** 发送成功后把指定队列项从持久化快照里移除。 */
async function removeStored(sessionId: string, messageId: string): Promise<void> {
  const scope = scopeOf(sessionId)
  try {
    const snapshot = await loadComposer(scope)
    if (!snapshot.queue.some((message) => message.id === messageId)) return
    const queue = snapshot.queue.filter((message) => message.id !== messageId)
    await saveComposer(scope, { ...snapshot, queue })
    broadcastQueue(sessionId, queue)
  } catch (error) {
    notify('err', error instanceof Error ? error.message : String(error))
  }
}

/** 把一条队列消息发成普通 prompt（页面不在时的路径）。 */
async function sendStoredMessage(sessionId: string, message: QueuedMessage): Promise<void> {
  const selection = message.selection ?? peekModelSelection()
  const images = message.images ?? []
  const attachments = (message.attachments ?? []).filter(isPendingAttachment)
  const quotes = message.quotes ?? []
  const prompt =
    message.text.trim() ||
    (images.length > 0
      ? t('pastedImageLabel')
      : attachments.length > 0
        ? attachments[0]!.name
        : quotes.length > 0
          ? quotes[0]!.title
          : message.text)
  await api.postPrompt(sessionId, {
    prompt,
    provider: selection?.provider,
    model: selection?.model,
    reasoningEffort: selection?.reasoningEffort,
    images: images.map(({ name, mime, data }) => ({ name, mime, data })),
    files: attachments.map((attachment) => attachment.path),
    quoted: quotes.map(({ title, text }) => ({ title, text })),
    source: message.mode === 'interject' ? 'interject' : 'queue',
  })
}

/** 等待一小段宽限后重读队列：送达事件可能已把 steering 项移走。 */
async function settleSteering(sessionId: string, queue: QueuedMessage[]): Promise<QueuedMessage[]> {
  if (queue[0]?.state !== 'steering') return queue
  await delay(STEERING_GRACE_MS)
  try {
    return (await loadComposer(scopeOf(sessionId))).queue
  } catch {
    return queue
  }
}

/**
 * 尝试发送某会话队首。
 *
 * 返回 true 表示确实发出了一条（已出队）；false 表示没有可发/发送失败。
 * 失败一律保留队列，下一次运行状态变化或页面手动操作会重试。
 */
export async function flushQueueHead(sessionId: string): Promise<boolean> {
  if (inFlight.has(sessionId) || suppressed.has(sessionId)) return false
  const handler = handlers.get(sessionId)
  inFlight.add(sessionId)
  try {
    if (handler) {
      let queue = handler.getQueue()
      if (queue[0]?.state === 'steering') {
        await delay(STEERING_GRACE_MS)
        queue = handler.getQueue()
      }
      const head = queue[0]
      if (!head) return false
      const ok = await handler.send(head)
      if (ok) handler.remove(head.id)
      return ok
    }
    return await withSessionLock(sessionId, async () => {
      let snapshot = await loadComposer(scopeOf(sessionId))
      let queue = await settleSteering(sessionId, snapshot.queue)
      const head = queue[0]
      if (!head) return false
      try {
        await sendStoredMessage(sessionId, head)
      } catch (error) {
        // 409 = 会话已经开始跑别的轮次：保留队列，等下一次轮次结束。
        const status = (error as { status?: number }).status
        if (status !== 409) {
          notify('err', error instanceof Error ? error.message : String(error))
        }
        return false
      }
      // 发送后重新读取：期间用户可能编辑了队列，不能拿旧快照整体覆盖。
      snapshot = await loadComposer(scopeOf(sessionId))
      if (snapshot.queue.some((message) => message.id === head.id)) {
        const next = snapshot.queue.filter((message) => message.id !== head.id)
        await saveComposer(scopeOf(sessionId), { ...snapshot, queue: next })
        broadcastQueue(sessionId, next)
      }
      return true
    }) ?? false
  } finally {
    inFlight.delete(sessionId)
  }
}

/** 移除一条「已送达」的引导；当前页面与持久化快照同时对齐。 */
export async function acknowledgeSteering(sessionId: string, messageId: string): Promise<void> {
  handlers.get(sessionId)?.remove(messageId)
  await removeStored(sessionId, messageId)
}

/**
 * 启动全局调度（幂等）。返回清理函数；应用生命周期内只需要一次。
 */
export function startQueueScheduler(): () => void {
  if (stopScheduler) return stopScheduler
  const offRunning = subscribeRunningStatus((ids) => {
    const previous = prevRunning
    prevRunning = ids
    for (const id of Object.keys(previous)) {
      if (previous[id] && !ids[id]) void flushQueueHead(id)
    }
  })
  const offEvents = subscribeServerEvents((type, payload) => {
    if (type !== 'steering-delivered') return
    const event = payload as { id?: string; message_id?: string; messageId?: string }
    const sessionId = event.id
    const messageId = event.message_id ?? event.messageId
    if (sessionId && messageId) void acknowledgeSteering(sessionId, messageId)
  })
  stopScheduler = () => {
    offRunning()
    offEvents()
    stopScheduler = null
  }
  return stopScheduler
}

/** 页面重新挂载时用持久化快照校正本地队列（跨会话事件可能已改动）。 */
export async function refreshStoredQueue(sessionId: string): Promise<QueuedMessage[] | null> {
  try {
    return (await loadComposer(scopeOf(sessionId))).queue
  } catch {
    return null
  }
}

/** 读写快照的兼容出口（测试与调试用；不改变页面状态）。 */
export async function readStoredQueue(sessionId: string): Promise<ComposerSnapshot['queue']> {
  return (await loadComposer(scopeOf(sessionId))).queue
}
