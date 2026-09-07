import { lazy, Suspense, useCallback, useEffect, useRef, useState } from 'react'
import * as api from '../api'
import { applyEnvelopes, foldEvents } from '../fold'
import type { TranscriptNode } from '../fold'
import { t } from '../i18n'
import { attach, type SessionPageMeta } from '../sessionStreams'
import type { SessionEnvelope, TodoItem, UserMessageImage } from '../types'
import type { TrajectoryQuote } from '../trajectory'
import { Transcript } from './transcript'

const OLDER_PAGE_LIMIT = 500

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
  jumpRequest?: { seq: number; nonce: number } | null
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
}) {
  const [nodes, setNodes] = useState<TranscriptNode[]>([])
  // 原始事件流:轨迹视图的 fold 源(与 transcript 共用一次订阅)。
  const [events, setEvents] = useState<SessionEnvelope[]>([])
  const [pageMeta, setPageMeta] = useState<SessionPageMeta>({ total: 0, hasMoreBefore: false, anchors: [] })
  const [loadingOlder, setLoadingOlder] = useState(false)
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

  useEffect(() => {
    nodesChangeRef.current?.(nodes)
  }, [nodes])

  useEffect(() => {
    eventsRef.current = events
  }, [events])

  useEffect(() => {
    const flush = () => {
      rafRef.current = undefined
      const batch = queueRef.current
      queueRef.current = []
      if (batch.length === 0) return
      const previous = eventsRef.current
      const next = previous.length > 0 ? [...previous, ...batch] : batch
      eventsRef.current = next
      // events state 只服务于轨迹视图与 todo 台账;纯输出帧(全部是
      // assistant-chunk)不触发 setEvents,省掉一帧一次的全量数组拷贝。
      const needsEvents =
        viewRef.current === 'trajectory' ||
        batch.some((env) => env.type === 'todo-write' || env.type === 'user-message')
      if (needsEvents) setEvents(next)
      setNodes((previous) => applyEnvelopes(previous, batch))
    }
    const settlePending = (events: SessionEnvelope[]) => {
      for (const event of events) {
        if (event.type === 'user-message' && !event.injected) {
          settleRef.current?.({ text: event.text, images: event.images })
        }
      }
    }
    const unsubscribe = attach(id, {
      onSnapshot: (_header, snapshot, meta) => {
        queueRef.current = []
        const pageInfo = meta ?? { total: snapshot.length, hasMoreBefore: false, anchors: [] }
        eventsRef.current = snapshot
        setEvents(snapshot)
        setPageMeta(pageInfo)
        setNodes(foldEvents(snapshot))
        publishTodos(snapshot)
        settlePending(snapshot)
        setLoading(false)
      },
      onEnvelope: (envelope) => {
        settlePending([envelope])
        queueRef.current.push(envelope)
        if (envelope.type === 'todo-write' || envelope.type === 'user-message') {
          publishTodos([...eventsRef.current, ...queueRef.current])
        }
        if (rafRef.current === undefined) {
          rafRef.current = window.requestAnimationFrame(flush)
        }
      },
      onNotFound: () => {
        onNotFound?.()
      },
    })
    return () => {
      unsubscribe()
      if (rafRef.current !== undefined) window.cancelAnimationFrame(rafRef.current)
      rafRef.current = undefined
      queueRef.current = []
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
    const current = eventsRef.current
    const oldest = current[0]?.seq
    if (!oldest || loadingOlder || !pageMetaRef.current.hasMoreBefore) return false
    setLoadingOlder(true)
    try {
      const page = await api.getSessionPage(id, { before: oldest, limit: OLDER_PAGE_LIMIT })
      const merged = [...page.events, ...eventsRef.current]
      eventsRef.current = merged
      setEvents(merged)
      setNodes(foldEvents(merged))
      setPageMeta({ total: page.total, hasMoreBefore: page.hasMoreBefore, anchors: page.anchors ?? [] })
      publishTodos(merged)
      return page.events.length > 0
    } catch {
      /* 翻页失败不打断已有内容;按钮保持可重试 */
      return false
    } finally {
      setLoadingOlder(false)
    }
  }, [id, loadingOlder, publishTodos])

  // 轮次轴跳转:锚点在窗口内直接定位;不在则向上翻页直至覆盖(或到头放弃)。
  useEffect(() => {
    if (!jumpRequest) return
    let cancelled = false
    void (async () => {
      const { seq } = jumpRequest
      while (!cancelled && !eventsRef.current.some((envelope) => envelope.seq === seq)) {
        const oldest = eventsRef.current[0]?.seq
        const progressed = await loadOlder()
        if (cancelled) return
        // 到头仍未见目标(异常请求)或翻页无进展:放弃并恢复轴的可点击态。
        if (!progressed || eventsRef.current[0]?.seq === oldest) {
          jumpSettledRef.current?.()
          return
        }
      }
      if (cancelled) return
      // 等 React 提交新行后再定位,smooth 滚动到可视带中央。
      requestAnimationFrame(() => {
        const target = paneRef.current?.querySelector<HTMLElement>(
          `[data-user-anchor="${seq}"]`,
        )
        target?.scrollIntoView({ behavior: 'smooth', block: 'center' })
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
        pendingMessages={pendingMessages}
        onRewind={onRewind}
        onFork={onFork}
        onLoopContinue={onLoopContinue}
      />
    </div>
  )
}
