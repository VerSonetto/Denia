import { lazy, Suspense, useCallback, useEffect, useRef, useState } from 'react'
import * as api from '../api'
import { clearCompacting } from '../appStore'
import { applyEnvelopes, foldEvents } from '../fold'
import type { TranscriptNode } from '../fold'
import { t } from '../i18n'
import { attach, type SessionPageMeta } from '../sessionStreams'
import { emptyTotals } from '../stats'
import type { AskAnswer, SessionEnvelope, TodoItem, UserMessageImage } from '../types'
import type { TrajectoryQuote } from '../trajectory'
import { Transcript } from './transcript'
import {
  appendSessionEvents,
  isTransientEvent,
  liveWindowStart,
  SESSION_PAGE_LIMIT,
} from '../sessionMemory'

const OLDER_PAGE_LIMIT = SESSION_PAGE_LIMIT

// 轨迹台账体积不小(拖入大量历史事件),按需加载,不挤占对话首屏。
const LazyTrajectoryView = lazy(() =>
  import('./TrajectoryView').then((module) => ({ default: module.TrajectoryView })),
)

/** Latest todo snapshot wins; both snapshot and live frames feed it. */
function latestTodos(events: { type: string; todos?: TodoItem[] }[]): TodoItem[] {
  let todos: TodoItem[] = []
  for (const event of events) {
    if (event.type === 'todo-write' && Array.isArray(event.todos)) todos = event.todos
  }
  return todos
}

/**
 * 全部完成后本轮仍展示完成态;下一轮用户消息发出(乐观行或已入账)
 * 再收起,避免输入框上方一直占着一条「N 已完成」。
 */
function visibleTodos(
  events: SessionEnvelope[],
  pendingCount: number,
): TodoItem[] {
  const todos = latestTodos(events)
  if (todos.length === 0) return []
  if (todos.some((item) => item.status !== 'completed')) return todos
  if (pendingCount > 0) return []
  let lastTodoSeq = -1
  let lastUserSeq = -1
  for (const event of events) {
    if (event.type === 'todo-write') lastTodoSeq = event.seq
    else if (event.type === 'user-message' && !event.injected) lastUserSeq = event.seq
  }
  return lastUserSeq > lastTodoSeq ? [] : todos
}

/**
 * 会话内容视图:transcript + 乐观用户行 + todo 快照。
 *
 * ## 性能
 * 流式帧经 rAF 合并派发——浏览器一帧最多一次 setNodes,
 * 高频 chunk(数百/s)不会触发等量 React 渲染;事件不丢失,
 * 逐条 fold 后一次性应用。历史窗口由后端按展示粒度分页
 * (流式 chunk 不占窗口),向上翻页按需补更早的完整轮次组。
 *
 * ## 生命周期
 * - 卸载时取消 rAF 与流订阅,不残留定时器/帧回调。
 * - 会话被删除(404)→ `onNotFound` 通知父级清视图。
 */
export function SessionView({
  id,
  view = 'chat',
  pendingMessages,
  compacting = null,
  onTodosChange,
  onNotFound,
  onPendingSettled,
  onNodesChange,
  onAnchorsChange,
  jumpRequest,
  onJumpSettled,
  onRewind,
  onFork,
  onQuote,
  onLoopContinue,
  onAskAnswer,
  onAskCancel,
  onGoalTouch,
  onTaskTouch,
}: {
  id: string
  /** 内容视图:对话 transcript 或轨迹台账(共用同一事件流订阅)。 */
  view?: 'chat' | 'trajectory'
  /** 已发送未获服务端确认的用户消息(乐观行,渲染在 transcript 尾部)。 */
  pendingMessages: { text: string; images?: UserMessageImage[] }[]
  onTodosChange?: (todos: TodoItem[]) => void
  /** 会话已被删除:父级应清空 activeId。 */
  onNotFound?: () => void
  /** 某条乐观消息已被服务端事件确认(从 pending 列表移除)。 */
  onPendingSettled?: (message: { text: string; images?: UserMessageImage[] }) => void
  /** 内容变化时通知父级(对话轴/统计条/贴底跟随)。 */
  onNodesChange?: (nodes: TranscriptNode[]) => void
  /** 全会话用户消息锚点变化时通知父级(轮次轴刻度,不受分页窗口限制)。 */
  onAnchorsChange?: (anchors: api.SessionAnchor[]) => void
  /** 轮次轴跳转请求:锚点未加载时自动向上翻页直至覆盖后定位。 */
  jumpRequest?: { seq: number; nonce: number; behavior?: ScrollBehavior } | null
  /** 跳转请求已处理(定位完成或无法覆盖),父级据此清空请求。 */
  onJumpSettled?: () => void
  /** 用户消息回退按钮触发。 */
  onRewind?: (seq: number) => void
  /** 轮次收尾消息分支按钮触发(以该消息 seq 为锚点开新会话)。 */
  onFork?: (seq: number) => void
  /** 轨迹引用(单条/区间)提交到 composer。 */
  onQuote?: (quote: TrajectoryQuote) => void
  /** 死循环提示行点「继续」:自动发送继续消息。 */
  onLoopContinue?: () => void
  /** 回答一组模型提问(`ask` 工具)。 */
  onAskAnswer?: (requestId: string, answers: AskAnswer[]) => void
  /** 取消一组模型提问。 */
  onAskCancel?: (requestId: string) => void
  /** 目标状态可能变化(goal 事件 / 轮次闭合 / 快照重建):父级刷新 GoalBar。 */
  onGoalTouch?: () => void
  /**
   * 任务账本可能变化(task 事件 / 轮次闭合):父级重拉只读任务视图。
   * 任务状态不进对话流(见 fold.ts):它由服务端折叠裁决,重拉即可。
   */
  onTaskTouch?: () => void
  /**
   * 压缩进行中:传发起时刻(epoch ms)则在对话流尾部挂占位行,传 null 移除。
   * 由父级驱动 —— 压缩请求在父级发出,这里只负责把状态画出来。
   */
  compacting?: number | null
}) {
  const [nodes, setNodes] = useState<TranscriptNode[]>([])
  /** 会话头 cwd(不可变):产物 chip 的相对路径锚点。 */
  const [cwd, setCwd] = useState<string | null>(null)
  // 原始事件流:轨迹视图的 fold 源(与 transcript 共用一次订阅)。
  const [events, setEvents] = useState<SessionEnvelope[]>([])
  const [pageMeta, setPageMeta] = useState<SessionPageMeta>({ total: 0, hasMoreBefore: false, anchors: [], totals: emptyTotals() })
  const [loadingOlder, setLoadingOlder] = useState(false)
  const olderRequestRef = useRef<AbortController | null>(null)
  const [loading, setLoading] = useState(true)
  const eventsRef = useRef<SessionEnvelope[]>([])
  const pageMetaRef = useRef(pageMeta)
  const viewRef = useRef(view)
  viewRef.current = view
  const paneRef = useRef<HTMLDivElement | null>(null)
  const settleRef = useRef(onPendingSettled)
  settleRef.current = onPendingSettled
  const nodesChangeRef = useRef(onNodesChange)
  nodesChangeRef.current = onNodesChange
  const jumpSettledRef = useRef(onJumpSettled)
  jumpSettledRef.current = onJumpSettled
  const todosChangeRef = useRef(onTodosChange)
  todosChangeRef.current = onTodosChange
  const goalTouchRef = useRef(onGoalTouch)
  goalTouchRef.current = onGoalTouch
  const taskTouchRef = useRef(onTaskTouch)
  taskTouchRef.current = onTaskTouch
  const pendingCountRef = useRef(pendingMessages.length)
  pendingCountRef.current = pendingMessages.length

  const publishTodos = useCallback((source: SessionEnvelope[], pendingCount = pendingCountRef.current) => {
    todosChangeRef.current?.(visibleTodos(source, pendingCount))
  }, [])

  pageMetaRef.current = pageMeta

  useEffect(() => {
    onAnchorsChange?.(pageMeta.anchors)
  }, [pageMeta.anchors, onAnchorsChange])

  // rAF 批处理:流式帧入队,每帧最多一次渲染。
  const queueRef = useRef<SessionEnvelope[]>([])
  const rafRef = useRef<number | undefined>(undefined)
  const backgroundFlushRef = useRef<number | undefined>(undefined)
  /**
   * 事件累加器:当前历史窗口 + 已到达的增量。就地 push,不每帧重建数组 ——
   * 长会话里 `[...previous, ...batch]` 是 O(总事件数) 的复制,而流式帧
   * 每秒几十条,这些复制不产出任何 UI。只有真正需要 events state 时
   * (轨迹视图 / todo / 用户消息)才切一份快照交给 React。
   */
  const accRef = useRef<SessionEnvelope[]>([])

  /** 把累加器当前内容作为新引用交给 events state(仅在需要时调用)。 */
  const publishEvents = useCallback(() => {
    const next = accRef.current.slice()
    eventsRef.current = next
    setEvents(next)
  }, [])

  useEffect(() => {
    nodesChangeRef.current?.(nodes)
  }, [nodes])

  useEffect(() => {
    eventsRef.current = events
  }, [events])

  useEffect(() => {
    if (view === 'trajectory') publishEvents()
  }, [view, publishEvents])

  useEffect(() => {
    const flush = () => {
      rafRef.current = undefined
      if (backgroundFlushRef.current !== undefined) window.clearTimeout(backgroundFlushRef.current)
      backgroundFlushRef.current = undefined
      const batch = queueRef.current
      queueRef.current = []
      if (batch.length === 0) return
      // 就地累加,不复制整份历史(见 accRef 注释)。
      appendSessionEvents(accRef.current, batch)
      // 只数持久事件:高频流式帧(助手增量、工具实时输出)不占分页位次,
      // 把它们算进来会让翻页总数在命令运行期间自己往上飘。
      const durableCount = batch.filter((envelope) => !isTransientEvent(envelope)).length
      const scroller = paneRef.current?.closest('.conversation-scroll')
      const atBottom = scroller !== null && scroller !== undefined &&
        scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 64
      const windowStart = viewRef.current === 'chat' && atBottom && !olderRequestRef.current &&
        batch.some((envelope) => envelope.type === 'turn-end')
        ? liveWindowStart(accRef.current) : 0
      if (windowStart > 0) accRef.current = accRef.current.slice(windowStart)
      const addedAnchors = batch.flatMap((envelope) =>
        envelope.type === 'user-message' && !envelope.injected
          ? [{ seq: envelope.seq, text: envelope.text.slice(0, 160) }] : [],
      )
      if (durableCount > 0) {
        const pageInfo = {
          ...pageMetaRef.current,
          total: pageMetaRef.current.total + durableCount,
          hasMoreBefore: pageMetaRef.current.hasMoreBefore || windowStart > 0,
          anchors: addedAnchors.length > 0
            ? [...pageMetaRef.current.anchors, ...addedAnchors] : pageMetaRef.current.anchors,
        }
        pageMetaRef.current = pageInfo
        setPageMeta(pageInfo)
      }
      // 结算/持久事件更新快照,释放已完成的 chunk;纯流式帧不复制历史。
      const needsEvents =
        viewRef.current === 'trajectory' || windowStart > 0 ||
        batch.some((env) => !isTransientEvent(env))
      if (needsEvents) publishEvents()
      if (windowStart > 0) setNodes(foldEvents(accRef.current))
      else setNodes((previous) => applyEnvelopes(previous, batch))
    }
    const scheduleFlush = () => {
      if (document.hidden) {
        if (rafRef.current !== undefined) window.cancelAnimationFrame(rafRef.current)
        rafRef.current = undefined
        if (backgroundFlushRef.current === undefined) {
          backgroundFlushRef.current = window.setTimeout(flush, 100)
        }
      } else if (rafRef.current === undefined) {
        if (backgroundFlushRef.current !== undefined) window.clearTimeout(backgroundFlushRef.current)
        backgroundFlushRef.current = undefined
        rafRef.current = window.requestAnimationFrame(flush)
      }
    }
    const visibilityChanged = () => {
      if (queueRef.current.length > 0) scheduleFlush()
    }
    document.addEventListener('visibilitychange', visibilityChanged)
    const settlePending = (events: SessionEnvelope[]) => {
      for (const event of events) {
        if (event.type === 'user-message' && !event.injected) {
          settleRef.current?.({ text: event.text, images: event.images })
        }
      }
    }
    const unsubscribe = attach(id, {
      onSnapshot: (header, snapshot, meta) => {
        olderRequestRef.current?.abort()
        olderRequestRef.current = null
        setLoadingOlder(false)
        queueRef.current = []
        const pageInfo = meta ?? { total: snapshot.length, hasMoreBefore: false, anchors: [], totals: emptyTotals() }
        accRef.current = snapshot.slice()
        eventsRef.current = snapshot
        setEvents(snapshot)
        setPageMeta(pageInfo)
        pageMetaRef.current = pageInfo
        setNodes(foldEvents(snapshot))
        // 会话头 cwd 不可变,产物 chip 的相对路径锚点取它(不依赖侧栏会话摘要)。
        setCwd(header.cwd)
        publishTodos(snapshot)
        settlePending(snapshot)
        goalTouchRef.current?.()
        // 与 goal 同款:快照重建(重连 / 重载)也重拉一次,断流期间落账的
        // 任务状态不会停在旧结论上。
        taskTouchRef.current?.()
        setLoading(false)
      },
      onEnvelope: (envelope) => {
        settlePending([envelope])
        queueRef.current.push(envelope)
        if (envelope.type === 'todo-write' || envelope.type === 'user-message') {
          publishTodos([...accRef.current, ...queueRef.current])
        }
        // 压缩成功:摘要落盘事件到达 →"正在压缩"标记到此为止(摘要行会
        // 由 fold 就地渲染出来)。刷新后走的是冷启动 snapshot 分支。
        if (envelope.type === 'compaction-summary') clearCompacting(id)
        // 目标状态或轮次记账变化:父级重拉服务端权威的 goal 视图。
        if (envelope.type === 'goal' || envelope.type === 'turn-end') {
          goalTouchRef.current?.()
        }
        // 任务账本变化:同样的口径重拉只读视图(状态与裁决只在服务端折叠)。
        if (envelope.type === 'task' || envelope.type === 'turn-end') {
          taskTouchRef.current?.()
        }
        scheduleFlush()
      },
      onNotFound: () => {
        onNotFound?.()
      },
    })
    return () => {
      unsubscribe()
      olderRequestRef.current?.abort()
      olderRequestRef.current = null
      if (rafRef.current !== undefined) window.cancelAnimationFrame(rafRef.current)
      if (backgroundFlushRef.current !== undefined) window.clearTimeout(backgroundFlushRef.current)
      backgroundFlushRef.current = undefined
      document.removeEventListener('visibilitychange', visibilityChanged)
      rafRef.current = undefined
      queueRef.current = []
      accRef.current = []
      eventsRef.current = []
      setEvents([])
    }
    // 仅依赖 id:监听回调经 ref 透传,避免每次渲染重挂流。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id])

  useEffect(() => {
    return () => {
      todosChangeRef.current?.([])
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id])

  // 用户发出下一轮时立刻收起已完成清单,不等服务端 user-message 入账。
  useEffect(() => {
    publishTodos([...eventsRef.current, ...queueRef.current], pendingMessages.length)
  }, [pendingMessages.length, publishTodos])

  /** 向上翻页:把更早的完整轮次组 prepend 到当前事件流并重建 fold。 */
  const loadOlder = useCallback(async (): Promise<boolean> => {
    const current = accRef.current
    const oldest = current[0]?.seq
    if (!oldest || olderRequestRef.current || !pageMetaRef.current.hasMoreBefore) return false
    const controller = new AbortController()
    olderRequestRef.current = controller
    setLoadingOlder(true)
    try {
      const page = await api.getSessionPage(id, { before: oldest, limit: OLDER_PAGE_LIMIT }, controller.signal)
      if (controller.signal.aborted || olderRequestRef.current !== controller) return false
      const merged = [...page.events, ...accRef.current]
      accRef.current = merged
      publishEvents()
      setNodes(foldEvents(merged))
      const pageInfo = {
        total: Math.max(pageMetaRef.current.total, page.total),
        hasMoreBefore: page.hasMoreBefore,
        anchors: [...new Map([...(page.anchors ?? []), ...pageMetaRef.current.anchors]
          .map((anchor) => [anchor.seq, anchor])).values()].sort((left, right) => left.seq - right.seq),
        // 累计统计是全会话口径,向上翻页拿的还是同一份,不因翻页变化。
        totals: pageMetaRef.current.totals,
      }
      pageMetaRef.current = pageInfo
      setPageMeta(pageInfo)
      publishTodos(merged)
      return page.events.length > 0
    } catch {
      /* 翻页失败不打断已有内容;按钮保持可重试 */
      return false
    } finally {
      if (olderRequestRef.current === controller) {
        olderRequestRef.current = null
        setLoadingOlder(false)
      }
    }
  }, [id, publishTodos, publishEvents])

  // 轮次轴跳转:锚点在窗口内直接定位;不在则向上翻页直至覆盖(或到头放弃)。
  useEffect(() => {
    if (!jumpRequest) return
    let cancelled = false
    void (async () => {
      const { seq, behavior = 'smooth' } = jumpRequest
      while (!cancelled && !accRef.current.some((envelope) => envelope.seq === seq)) {
        const oldest = accRef.current[0]?.seq
        const progressed = await loadOlder()
        if (cancelled) return
        // 到头仍未见目标(异常请求)或翻页无进展:放弃并恢复轴的可点击态。
        if (!progressed || accRef.current[0]?.seq === oldest) {
          jumpSettledRef.current?.()
          return
        }
      }
      if (cancelled) return
      // 等 React 提交新行后再定位。对齐用户消息顶缘,与轴内直接跳转一致。
      requestAnimationFrame(() => {
        const target = paneRef.current?.querySelector<HTMLElement>(
          `[data-user-anchor="${seq}"]`,
        )
        target?.scrollIntoView({ behavior, block: 'start' })
        jumpSettledRef.current?.()
      })
    })()
    return () => {
      cancelled = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [jumpRequest])

  if (loading) {
    return <div className="empty-hint">{t('loading')}</div>
  }

  if (view === 'trajectory') {
    return (
      <Suspense fallback={<div className="empty-hint">{t('loading')}</div>}>
        <LazyTrajectoryView events={events} onQuote={onQuote} />
      </Suspense>
    )
  }

  return (
    <div className="transcript-pane" ref={paneRef}>
      {pageMeta.hasMoreBefore && (
        <div className="load-older-row">
          <button
            type="button"
            className="load-older-btn"
            disabled={loadingOlder}
            onClick={() => void loadOlder()}
          >
            {loadingOlder
              ? t('loadingOlder')
              : pageMeta.total > events.length
                ? t('loadOlderRemaining', { n: pageMeta.total - events.length })
                : t('loadOlder')}
          </button>
        </div>
      )}
      <Transcript
        nodes={nodes}
        cwd={cwd}
        pendingMessages={pendingMessages}
        compactingAt={compacting}
        onRewind={onRewind}
        onFork={onFork}
        onLoopContinue={onLoopContinue}
        onAskAnswer={onAskAnswer}
        onAskCancel={onAskCancel}
      />
    </div>
  )
}
